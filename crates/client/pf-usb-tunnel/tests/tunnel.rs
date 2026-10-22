//! End to end: UDP client -> relay -> (TCP carrier) -> bridge -> UDP echo host, and back.

mod common;

use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

use common::*;
use pf_usb_tunnel::bridge::{self, TcpConnector};
use pf_usb_tunnel::frame::{self, FrameReader, Hello, Role, KIND_DATA, KIND_HELLO, VERSION};
use pf_usb_tunnel::relay;

#[test]
fn echo_roundtrip_and_replies_come_from_the_relay_address() {
    let rig = Rig::start();
    let c = rig.client();
    for i in 0..100u32 {
        c.send(&tagged(1, i, 1454)).unwrap();
        let mut b = vec![0u8; 2048];
        let n = c.recv(&mut b).expect("echo");
        assert_eq!(n, 1454);
        assert_eq!(untag(&b[..n]), (1, i));
    }
    // `c` is connect()ed to the relay's address: had the relay answered from any other address
    // the kernel would have discarded the reply and recv would have timed out.
    let s = rig.relay.stats();
    assert_eq!(s.flows_opened, 1);
    assert_eq!(s.drop_media_overflow + s.drop_quic_overflow, 0);
}

#[test]
fn quic_and_media_both_pass_untouched_with_all_sizes() {
    let rig = Rig::start();
    let c = rig.client();
    for (first, n) in [
        (0x00u8, 1454usize),
        (0x4A, 1200),
        (0xC1, 1252),
        (0x00, 8972),
        (0x40, 65000),
        (0x00, 1),
    ] {
        let mut d: Vec<u8> = (0..n).map(|i| (i % 253) as u8).collect();
        d[0] = first;
        c.send(&d).unwrap();
        let mut b = vec![0u8; 65536];
        let got = c.recv(&mut b).unwrap();
        assert_eq!(&b[..got], &d[..], "first byte {first:#x} len {n}");
    }
}

#[test]
fn many_flows_at_once_do_not_mix() {
    let rig = Rig::start();
    const FLOWS: u16 = 8;
    const PER: u32 = 300;
    let mut ts = Vec::new();
    for f in 0..FLOWS {
        let c = rig.client();
        ts.push(thread::spawn(move || {
            let mut got = Vec::new();
            for i in 0..PER {
                c.send(&tagged(f, i, 200 + f as usize)).unwrap();
                let mut b = vec![0u8; 2048];
                let n = c.recv(&mut b).expect("echo");
                assert_eq!(n, 200 + f as usize);
                let (id, seq) = untag(&b[..n]);
                assert_eq!(id, f, "datagram of another flow arrived here");
                got.push(seq);
            }
            got
        }));
    }
    for t in ts {
        let got = t.join().unwrap();
        assert_eq!(got, (0..PER).collect::<Vec<_>>());
    }
    assert_eq!(rig.relay.flows(), FLOWS as usize);
    wait_until("bridge flows", 2, || rig.bridge.flows() == FLOWS as usize);
    assert_eq!(
        rig.echo.ports().len(),
        FLOWS as usize,
        "one host-side source port per flow"
    );
}

#[test]
fn host_side_source_port_is_fixed_for_the_life_of_a_flow() {
    let rig = Rig::start();
    let c = rig.client();
    let mut b = vec![0u8; 2048];
    for i in 0..200u32 {
        c.send(&tagged(1, i, 500)).unwrap();
        c.recv(&mut b).unwrap();
    }
    let ports = rig.echo.ports();
    assert_eq!(ports.len(), 1, "the host saw {ports:?}");
    assert_eq!(rig.bridge.flow_ports().len(), 1);
    assert_eq!(rig.bridge.flow_ports()[0].1, ports[0]);
    // The source is loopback, as the host's LAN/WAN rules expect.
    assert!(rig
        .echo
        .sources
        .lock()
        .unwrap()
        .iter()
        .all(|a| a.ip().is_loopback()));
}

#[test]
fn carrier_cut_and_reconnect_keeps_flow_and_host_port() {
    let echo = Echo::start();
    let relay = relay::start_with(relay_cfg()).unwrap();
    let proxy = Proxy::start(relay.tcp_addr());
    let bridge = bridge::start(Box::new(TcpConnector(proxy.addr)), bridge_cfg(echo.addr)).unwrap();
    wait_until("carrier up", 5, || {
        relay.carrier_up() && bridge.carrier_up()
    });

    let c = UdpSocket::bind((LO, 0)).unwrap();
    c.connect(relay.udp_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut b = vec![0u8; 2048];
    c.send(&tagged(1, 0, 300)).unwrap();
    c.recv(&mut b).unwrap();
    let before = bridge.flow_ports();
    assert_eq!(before.len(), 1);

    for round in 1..=3u32 {
        proxy.sever();
        wait_until("carrier down on both sides", 5, || {
            !relay.carrier_up() && !bridge.carrier_up()
        });
        wait_until("carrier back", 5, || {
            relay.carrier_up() && bridge.carrier_up()
        });
        c.send(&tagged(1, round, 300)).unwrap();
        let n = c.recv(&mut b).expect("echo after reconnect");
        assert_eq!(untag(&b[..n]), (1, round));
        assert_eq!(
            bridge.flow_ports(),
            before,
            "same flow id and same host source port after reconnect {round}"
        );
    }
    assert_eq!(echo.ports().len(), 1);
    assert!(relay.stats().carriers_total >= 4);
    assert_eq!(
        relay.stats().flows_opened,
        1,
        "the relay never re-opened the flow"
    );
}

#[test]
fn silent_carrier_is_detected_and_dropped() {
    // A peer that completes HELLO and then goes silent (cable pulled with no RST).
    let relay = relay::start_with(relay_cfg()).unwrap();
    let mut s = std::net::TcpStream::connect(relay.tcp_addr()).unwrap();
    use std::io::Write;
    s.write_all(&frame::encode(
        KIND_HELLO,
        0,
        &Hello {
            version: VERSION,
            role: Role::Bridge as u8,
            generation: 0,
        }
        .encode(),
    ))
    .unwrap();
    wait_until("carrier up", 3, || relay.carrier_up());
    wait_until("silent carrier dropped by the watchdog", 5, || {
        !relay.carrier_up()
    });
    assert!(relay.stats().carrier_errors + relay.stats().carriers_total >= 1);
}

#[test]
fn rtt_is_measured_by_ping_pong() {
    let rig = Rig::start();
    wait_until("rtt on both", 3, || {
        rig.relay.stats().rtt_us > 0 && rig.bridge.stats().rtt_us > 0
    });
    assert!(
        rig.relay.stats().rtt_us < 50_000,
        "loopback rtt {} us",
        rig.relay.stats().rtt_us
    );
}

#[test]
fn idle_flows_are_closed_on_both_sides() {
    let mut cfg = relay_cfg();
    cfg.flow_idle = Duration::from_millis(1200);
    let rig = Rig::with(cfg, |b| b.flow_idle = Duration::from_millis(1200));
    let c = rig.client();
    let mut b = vec![0u8; 2048];
    c.send(&tagged(1, 0, 100)).unwrap();
    c.recv(&mut b).unwrap();
    assert_eq!((rig.relay.flows(), rig.bridge.flows()), (1, 1));
    wait_until("flow swept on both sides", 6, || {
        rig.relay.flows() == 0 && rig.bridge.flows() == 0
    });
    // And a new datagram simply opens a fresh flow.
    c.send(&tagged(1, 1, 100)).unwrap();
    c.recv(&mut b).unwrap();
    assert_eq!(rig.relay.stats().flows_opened, 2);
}

#[test]
fn flows_survive_while_active_even_past_the_idle_time() {
    let mut cfg = relay_cfg();
    cfg.flow_idle = Duration::from_millis(1200);
    let rig = Rig::with(cfg, |b| b.flow_idle = Duration::from_millis(1200));
    let c = rig.client();
    let mut b = vec![0u8; 2048];
    for i in 0..30u32 {
        c.send(&tagged(1, i, 100)).unwrap();
        c.recv(&mut b).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        rig.relay.stats().flows_opened,
        1,
        "an active flow was swept"
    );
}

#[test]
fn bridge_forgets_flows_when_the_relay_forgot_them() {
    let echo = Echo::start();
    let mut rcfg = relay_cfg();
    rcfg.grace = Duration::from_millis(500);
    let relay = relay::start_with(rcfg).unwrap();
    let proxy = Proxy::start(relay.tcp_addr());
    let mut bcfg = bridge_cfg(echo.addr);
    bcfg.grace = Duration::from_secs(60); // the bridge alone would keep its sockets
    let bridge = bridge::start(Box::new(TcpConnector(proxy.addr)), bcfg).unwrap();
    wait_until("carrier up", 5, || {
        relay.carrier_up() && bridge.carrier_up()
    });
    let c = UdpSocket::bind((LO, 0)).unwrap();
    c.connect(relay.udp_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut b = vec![0u8; 2048];
    c.send(&tagged(1, 0, 100)).unwrap();
    c.recv(&mut b).unwrap();
    let old = bridge.flow_ports();

    proxy.block(true);
    proxy.sever();
    wait_until("relay forgot its flows after the grace time", 6, || {
        relay.flows() == 0
    });
    proxy.block(false);
    wait_until("carrier back", 5, || {
        relay.carrier_up() && bridge.carrier_up()
    });
    wait_until("bridge dropped the stale sockets", 3, || {
        bridge.flows() == 0
    });

    c.send(&tagged(1, 1, 100)).unwrap();
    c.recv(&mut b).unwrap();
    let new = bridge.flow_ports();
    assert_eq!(new.len(), 1);
    assert_ne!(
        new[0].1, old[0].1,
        "a forgotten flow gets a new host-side port"
    );
}

#[test]
fn a_new_carrier_replaces_the_old_one() {
    let relay = relay::start_with(relay_cfg()).unwrap();
    let mut p1 = raw_bridge(relay.tcp_addr());
    wait_until("first carrier", 3, || {
        relay.carrier_up() && relay.stats().carriers_total == 1
    });
    // A restarted bridge connects while the old connection still looks alive to the relay.
    let mut p2 = raw_bridge(relay.tcp_addr());
    wait_until("second carrier took over", 3, || {
        relay.stats().carriers_total == 2
    });
    // The first connection is shut down by the relay: its reader sees the end of the stream.
    p1.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut rd = FrameReader::new(std::io::BufReader::new(&mut p1));
    let mut ended = false;
    for _ in 0..8 {
        match rd.read_frame() {
            Ok(Some(_)) => {}
            _ => {
                ended = true;
                break;
            }
        }
    }
    assert!(ended, "the replaced carrier was left open");
    assert!(relay.carrier_up(), "the new carrier must be the live one");
    // and the new one carries traffic
    let c = UdpSocket::bind((LO, 0)).unwrap();
    c.send_to(&tagged(1, 1, 100), relay.udp_addr()).unwrap();
    p2.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut rd2 = FrameReader::new(std::io::BufReader::new(&mut p2));
    let mut got_data = false;
    for _ in 0..8 {
        match rd2.read_frame() {
            Ok(Some(f)) if f.kind == KIND_DATA => {
                got_data = true;
                break;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(got_data);
}

#[test]
fn wrong_version_is_refused() {
    use std::io::{Read, Write};
    let relay = relay::start_with(relay_cfg()).unwrap();
    let mut s = std::net::TcpStream::connect(relay.tcp_addr()).unwrap();
    s.write_all(&frame::encode(
        KIND_HELLO,
        0,
        &Hello {
            version: VERSION + 1,
            role: Role::Bridge as u8,
            generation: 0,
        }
        .encode(),
    ))
    .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    // the relay sends its own HELLO, then hangs up
    let mut rd = FrameReader::new(&mut s);
    let mut saw_eof = false;
    for _ in 0..4 {
        match rd.read_frame() {
            Ok(None) | Err(_) => {
                saw_eof = true;
                break;
            }
            Ok(Some(_)) => {}
        }
    }
    assert!(
        saw_eof,
        "relay kept a carrier with a wrong protocol version"
    );
    assert!(!relay.carrier_up());
    let mut x = [0u8; 1];
    let _ = s.read(&mut x);
}

#[test]
fn datagrams_without_a_carrier_are_dropped_and_counted_not_buffered() {
    let relay = relay::start_with(relay_cfg()).unwrap();
    let c = UdpSocket::bind((LO, 0)).unwrap();
    for i in 0..10u32 {
        c.send_to(&tagged(1, i, 100), relay.udp_addr()).unwrap();
    }
    wait_until("drops counted", 2, || relay.stats().drop_no_carrier == 10);
    assert_eq!(
        relay.flows(),
        0,
        "no flow is created while the cable is out"
    );
}

#[cfg(unix)]
#[test]
fn bridge_through_a_fake_usbmuxd_with_device_selection() {
    use pf_usb_tunnel::usbmux::UsbmuxdConnector;
    let dir = std::env::temp_dir().join(format!("pf-usb-tunnel-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo = Echo::start();
    let relay = relay::start_with(relay_cfg()).unwrap();
    let other = relay::start_with(relay_cfg()).unwrap(); // a second iPad's relay that must NOT be picked
    let mux = FakeUsbmuxd::start(
        &dir,
        vec![
            FakeDevice {
                id: 2,
                serial: "WIFI-ONLY",
                conn_type: "Network",
                target: other.tcp_addr(),
            },
            FakeDevice {
                id: 5,
                serial: "OTHER-IPAD",
                conn_type: "USB",
                target: other.tcp_addr(),
            },
            FakeDevice {
                id: 9,
                serial: "MY-IPAD",
                conn_type: "USB",
                target: relay.tcp_addr(),
            },
        ],
    );
    let conn = UsbmuxdConnector {
        socket: mux.path.clone(),
        udid: Some("MY-IPAD".into()),
        device_port: 9779,
    };
    let bridge = bridge::start(Box::new(conn), bridge_cfg(echo.addr)).unwrap();
    wait_until("carrier via usbmuxd", 5, || {
        relay.carrier_up() && bridge.carrier_up()
    });
    let c = UdpSocket::bind((LO, 0)).unwrap();
    c.connect(relay.udp_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut b = vec![0u8; 2048];
    c.send(&tagged(1, 7, 400)).unwrap();
    let n = c.recv(&mut b).unwrap();
    assert_eq!(untag(&b[..n]), (1, 7));
    assert_eq!(
        mux.connects.lock().unwrap()[0],
        (9, 9779),
        "Connect asked for device 9, port 9779 (htons round-trips)"
    );
    assert!(!other.carrier_up());
    drop(bridge);
    drop(mux); // its Drop wakes the accept loop through the socket path: remove the directory after
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn usbmuxd_first_usb_device_and_refusal_are_reported() {
    use pf_usb_tunnel::bridge::Connector;
    use pf_usb_tunnel::usbmux::{self, UsbmuxdConnector};
    let dir = std::env::temp_dir().join(format!("pf-usb-tunnel-test2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The only USB device points at a port nobody listens on.
    let dead = std::net::TcpListener::bind((LO, 0)).unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let mux = FakeUsbmuxd::start(
        &dir,
        vec![FakeDevice {
            id: 1,
            serial: "X",
            conn_type: "USB",
            target: dead_addr,
        }],
    );
    let devs = usbmux::list_devices(&mux.path).unwrap();
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].serial, "X");
    let c = UsbmuxdConnector {
        socket: mux.path.clone(),
        udid: None,
        device_port: 9779,
    };
    let e = c.connect().err().expect("refused");
    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}");
    let c = UsbmuxdConnector {
        socket: mux.path.clone(),
        udid: Some("nope".into()),
        device_port: 9779,
    };
    assert_eq!(
        c.connect().err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    drop(mux);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Queue policy through the real relay, with a hand-driven peer that can stop reading.

/// A raw bridge: speaks HELLO, can stop reading (to jam the carrier), then drains.
fn raw_bridge(relay_tcp: std::net::SocketAddr) -> std::net::TcpStream {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::io::Write;
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    sock.set_recv_buffer_size(4096).unwrap(); // tiny: the jam must show up in OUR queue
    sock.connect(&relay_tcp.into()).unwrap();
    let mut s: std::net::TcpStream = sock.into();
    s.set_nodelay(true).unwrap();
    s.write_all(&frame::encode(
        KIND_HELLO,
        0,
        &Hello {
            version: VERSION,
            role: Role::Bridge as u8,
            generation: 0,
        }
        .encode(),
    ))
    .unwrap();
    s
}

#[test]
fn jammed_carrier_drops_media_never_quic_and_nothing_waits() {
    let mut cfg = relay_cfg();
    cfg.queue.media_cap_bytes = 256 * 1024;
    cfg.queue.media_max_age = Duration::from_secs(120); // isolate the tail-drop rule
    cfg.timing.dead_after = Duration::from_secs(60);
    cfg.timing.ping_interval = Duration::from_secs(30);
    cfg.timing.link_send_buf = 16 * 1024;
    let relay = relay::start_with(cfg).unwrap();
    let mut peer = raw_bridge(relay.tcp_addr());
    wait_until("carrier up", 3, || relay.carrier_up());

    // The client blasts 30k media datagrams with a QUIC packet (numbered) every 20th, while the
    // peer reads NOTHING.
    let c = UdpSocket::bind((LO, 0)).unwrap();
    let mut quic_sent = 0u32;
    let mut media_sent = 0u32;
    let t0 = std::time::Instant::now();
    for i in 0..30_000u32 {
        if i % 20 == 0 {
            let mut d = vec![0u8; 1200];
            d[0] = 0x4A;
            d[1..5].copy_from_slice(&quic_sent.to_be_bytes());
            c.send_to(&d, relay.udp_addr()).unwrap();
            quic_sent += 1;
        } else {
            let mut d = vec![0u8; 1400];
            d[0] = 0x00;
            d[1..5].copy_from_slice(&media_sent.to_be_bytes());
            c.send_to(&d, relay.udp_addr()).unwrap();
            media_sent += 1;
        }
    }
    let flood = t0.elapsed();
    assert!(
        flood < Duration::from_secs(10),
        "producer waited on a jammed carrier ({flood:?})"
    );
    wait_until("relay consumed the flood", 5, || {
        let s = relay.stats();
        s.udp_in + s.drop_no_carrier >= 30_000 || {
            thread::sleep(Duration::from_millis(200));
            relay.stats().udp_in == s.udp_in
        }
    });
    let s = relay.stats();
    assert!(
        s.drop_media_overflow > 0,
        "no media was dropped on a jammed carrier: {s:?}"
    );
    assert_eq!(s.drop_quic_overflow, 0, "QUIC must never be dropped: {s:?}");
    assert!(
        s.media_q_peak <= 256 * 1024,
        "media queue exceeded its cap: {}",
        s.media_q_peak
    );

    // Now let the peer read everything that was queued.
    peer.set_read_timeout(Some(Duration::from_millis(700)))
        .unwrap();
    let mut rd = FrameReader::new(std::io::BufReader::new(&mut peer));
    let (mut media_got, mut quic_got) = (0u32, Vec::new());
    loop {
        match rd.read_frame() {
            Ok(Some(f)) if f.kind == KIND_DATA => {
                if f.payload[0] == 0x4A {
                    quic_got.push(u32::from_be_bytes([
                        f.payload[1],
                        f.payload[2],
                        f.payload[3],
                        f.payload[4],
                    ]));
                } else {
                    media_got += 1;
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    let s = relay.stats();
    let udp_in_quic = quic_got.len() as u32;
    // Every QUIC datagram that reached the relay arrived, in order, exactly once.
    assert_eq!(
        quic_got,
        (0..udp_in_quic).collect::<Vec<_>>(),
        "QUIC packets lost or reordered"
    );
    assert!(
        udp_in_quic >= quic_sent * 9 / 10,
        "client->relay kernel loss too high: {udp_in_quic}/{quic_sent}"
    );
    let media_in = s.udp_in - u64::from(udp_in_quic);
    assert_eq!(
        u64::from(media_got) + s.drop_media_overflow + s.drop_media_stale,
        media_in,
        "media accounting: got {media_got}, overflow {}, stale {}, in {media_in}",
        s.drop_media_overflow,
        s.drop_media_stale
    );
    assert!(media_got > 0);
    eprintln!(
        "jam test: flood of {} media + {} quic took {flood:?}; media delivered {media_got}, tail-dropped {}, quic delivered {}/{}",
        media_sent, quic_sent, s.drop_media_overflow, udp_in_quic, quic_sent
    );
}

#[test]
fn stale_media_is_not_delivered_after_a_stall() {
    let mut cfg = relay_cfg();
    cfg.queue.media_cap_bytes = 4 * 1024 * 1024;
    cfg.queue.media_max_age = Duration::from_millis(150);
    cfg.timing.dead_after = Duration::from_secs(60);
    cfg.timing.ping_interval = Duration::from_secs(30);
    cfg.timing.link_send_buf = 16 * 1024;
    let relay = relay::start_with(cfg).unwrap();
    let mut peer = raw_bridge(relay.tcp_addr());
    wait_until("carrier up", 3, || relay.carrier_up());
    let c = UdpSocket::bind((LO, 0)).unwrap();
    for i in 0..2000u32 {
        let mut d = vec![0u8; 1400];
        d[1..5].copy_from_slice(&i.to_be_bytes());
        c.send_to(&d, relay.udp_addr()).unwrap();
    }
    thread::sleep(Duration::from_millis(500)); // the stall: everything queued is now older than 150 ms
    let mut fresh = vec![0u8; 1400];
    fresh[1..5].copy_from_slice(&9999u32.to_be_bytes());
    c.send_to(&fresh, relay.udp_addr()).unwrap();
    wait_until("fresh packet queued", 2, || relay.stats().udp_in >= 2001);
    peer.set_read_timeout(Some(Duration::from_millis(700)))
        .unwrap();
    let mut rd = FrameReader::new(std::io::BufReader::new(&mut peer));
    let mut ids = Vec::new();
    while let Ok(Some(f)) = rd.read_frame() {
        if f.kind == KIND_DATA {
            ids.push(u32::from_be_bytes([
                f.payload[1],
                f.payload[2],
                f.payload[3],
                f.payload[4],
            ]));
        }
    }
    assert!(ids.contains(&9999), "the fresh packet must be delivered");
    // The kernel/TCP buffers may hold the first few KB in flight; everything else that waited
    // past max_age is gone.
    assert!(
        ids.len() < 200,
        "{} stale packets were delivered after the stall",
        ids.len()
    );
    assert!(relay.stats().drop_media_stale > 1500, "{:?}", relay.stats());
}
