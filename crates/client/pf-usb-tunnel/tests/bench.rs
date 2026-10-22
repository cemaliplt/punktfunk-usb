//! Loopback measurement of the whole tunnel (relay + TCP carrier + bridge), run by hand:
//!
//! ```text
//! cargo test -p pf-usb-tunnel --release --test bench -- --ignored --nocapture --test-threads=1
//! PF_BENCH_MBIT=300 PF_BENCH_SECS=20 ... (defaults 150 and 10)
//! ```
//! Datagrams are 1454 bytes (what the host seals in default mode), `0x00` first byte (media), paced
//! by a spin loop at the requested bit rate. Each carries a send timestamp from one monotonic
//! clock, so the latency is exact (same machine, same clock); loopback has no real link, so these
//! numbers are the tunnel's own overhead and queueing, not the USB cable.

mod common;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use pf_usb_tunnel::bridge::{self, TcpConnector};
use pf_usb_tunnel::relay;

const PKT: usize = 1454;

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn stamp(buf: &mut [u8], seq: u32, base: Instant) {
    buf[0] = 0x00;
    buf[1..5].copy_from_slice(&seq.to_be_bytes());
    buf[5..13].copy_from_slice(&(base.elapsed().as_nanos() as u64).to_be_bytes());
}

fn pace_send(
    sock: &UdpSocket,
    to: Option<std::net::SocketAddr>,
    mbit: u64,
    secs: u64,
    base: Instant,
) -> u32 {
    let interval = Duration::from_nanos(PKT as u64 * 8 * 1000 / mbit); // ns per packet at `mbit` Mbit/s
    let mut buf = vec![0u8; PKT];
    let start = Instant::now();
    let end = start + Duration::from_secs(secs);
    let mut seq = 0u32;
    let mut next = start;
    while Instant::now() < end {
        while Instant::now() < next {
            std::hint::spin_loop();
        }
        stamp(&mut buf, seq, base);
        let _ = match to {
            Some(a) => sock.send_to(&buf, a),
            None => sock.send(&buf),
        };
        seq += 1;
        next += interval;
    }
    seq
}

struct Lat(Vec<u32>);
impl Lat {
    fn pct(&mut self, p: f64) -> u32 {
        if self.0.is_empty() {
            return 0;
        }
        self.0.sort_unstable();
        self.0[((self.0.len() as f64 - 1.0) * p) as usize]
    }
}

fn receiver(
    sock: UdpSocket,
    base: Instant,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<(u32, Lat, u32)> {
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    thread::spawn(move || {
        let mut buf = vec![0u8; 2048];
        let mut lat = Vec::with_capacity(2_000_000);
        let mut got = 0u32;
        let mut reordered = 0u32;
        let mut max_seq = 0u32;
        loop {
            match sock.recv(&mut buf) {
                Ok(n) if n == PKT => {
                    let now = base.elapsed().as_nanos() as u64;
                    let sent = u64::from_be_bytes(buf[5..13].try_into().unwrap());
                    lat.push(((now.saturating_sub(sent)) / 1000) as u32);
                    let seq = u32::from_be_bytes(buf[1..5].try_into().unwrap());
                    if seq < max_seq {
                        reordered += 1;
                    }
                    max_seq = max_seq.max(seq);
                    got += 1;
                }
                Ok(_) => {}
                Err(_) => {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        (got, Lat(lat), reordered)
    })
}

fn report(title: &str, sent: u32, got: u32, lat: &mut Lat, reordered: u32, mbit: u64, secs: u64) {
    let loss = 100.0 * (sent as f64 - got as f64) / sent as f64;
    let achieved = sent as f64 * PKT as f64 * 8.0 / secs as f64 / 1e6;
    eprintln!(
        "BENCH {title}: target {mbit} Mbit/s, achieved send {achieved:.1} Mbit/s, sent {sent}, received {got}, loss {loss:.3}%, reordered {reordered}, latency us p50 {} p90 {} p99 {} p99.9 {} max {}",
        lat.pct(0.50),
        lat.pct(0.90),
        lat.pct(0.99),
        lat.pct(0.999),
        lat.pct(1.0)
    );
}

/// Host -> client at the target rate (this is the video direction). The host-side sender waits
/// for the client's first datagram (the "hello" that opens the flow), then streams to the
/// bridge's flow socket; the client records one-way latency. A trickle of QUIC-shaped packets
/// goes the other way.
#[test]
#[ignore]
fn video_direction_one_way() {
    let mbit = env_u64("PF_BENCH_MBIT", 150);
    let secs = env_u64("PF_BENCH_SECS", 10);
    let host = UdpSocket::bind((LO, 0)).unwrap();
    host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let host_addr = host.local_addr().unwrap();
    let relay = relay::start_with({
        let mut c = relay_cfg();
        c.timing = Default::default();
        c
    })
    .unwrap();
    let mut bc = bridge_cfg(host_addr);
    bc.timing = Default::default();
    let bridge = bridge::start(Box::new(TcpConnector(relay.tcp_addr())), bc).unwrap();
    wait_until("carrier", 5, || relay.carrier_up() && bridge.carrier_up());

    let client = UdpSocket::bind((LO, 0)).unwrap();
    client.connect(relay.udp_addr()).unwrap();
    let base = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let rx = receiver(client.try_clone().unwrap(), base, stop.clone());

    // open the flow with a QUIC-shaped packet; learn the bridge's source address
    client.send(&[0x4A; 100]).unwrap();
    let mut b = [0u8; 2048];
    let (_, flow_src) = host.recv_from(&mut b).unwrap();

    // client -> host trickle (QUIC acks/feedback: 1 pkt/ms)
    let trickle_stop = Arc::new(AtomicBool::new(false));
    let ts = {
        let (c, s) = (client.try_clone().unwrap(), trickle_stop.clone());
        thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let _ = c.send(&[0x4A; 120]);
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let sent = pace_send(&host, Some(flow_src), mbit, secs, base);
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::SeqCst);
    trickle_stop.store(true, Ordering::SeqCst);
    ts.join().unwrap();
    let (got, mut lat, reordered) = rx.join().unwrap();
    report(
        "host->client (video direction, one-way)",
        sent,
        got,
        &mut lat,
        reordered,
        mbit,
        secs,
    );
    let s = bridge.stats();
    eprintln!(
        "BENCH bridge counters: media tail drops {}, stale drops {}, quic drops {}, media queue peak {} B, rtt (ping) {} us, udp rcvbuf {} B",
        s.drop_media_overflow, s.drop_media_stale, s.drop_quic_overflow, s.media_q_peak, s.rtt_us, s.udp_rcvbuf
    );
}

/// Both directions at once: client sends at the rate, an echo host returns every packet; the
/// carrier moves the rate in each direction and the latency is the full round trip.
#[test]
#[ignore]
fn echo_round_trip_both_directions() {
    let mbit = env_u64("PF_BENCH_MBIT", 150);
    let secs = env_u64("PF_BENCH_SECS", 10);
    let echo = Echo::start();
    let relay = relay::start_with({
        let mut c = relay_cfg();
        c.timing = Default::default();
        c
    })
    .unwrap();
    let mut bc = bridge_cfg(echo.addr);
    bc.timing = Default::default();
    let bridge = bridge::start(Box::new(TcpConnector(relay.tcp_addr())), bc).unwrap();
    wait_until("carrier", 5, || relay.carrier_up() && bridge.carrier_up());
    let client = UdpSocket::bind((LO, 0)).unwrap();
    client.connect(relay.udp_addr()).unwrap();
    let base = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let rx = receiver(client.try_clone().unwrap(), base, stop.clone());
    let sent = pace_send(&client, None, mbit, secs, base);
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::SeqCst);
    let (got, mut lat, reordered) = rx.join().unwrap();
    report(
        "client->host->client (round trip)",
        sent,
        got,
        &mut lat,
        reordered,
        mbit,
        secs,
    );
    let (r, b) = (relay.stats(), bridge.stats());
    eprintln!(
        "BENCH relay: tail drops {} queue peak {} B udp rcvbuf {} B; bridge: tail drops {} queue peak {} B",
        r.drop_media_overflow, r.media_q_peak, r.udp_rcvbuf, b.drop_media_overflow, b.media_q_peak
    );
}

/// A small-packet latency probe at low rate: the floor of the tunnel (no queueing).
#[test]
#[ignore]
fn small_packet_floor() {
    let echo = Echo::start();
    let relay = relay::start_with({
        let mut c = relay_cfg();
        c.timing = Default::default();
        c
    })
    .unwrap();
    let mut bc = bridge_cfg(echo.addr);
    bc.timing = Default::default();
    let bridge = bridge::start(Box::new(TcpConnector(relay.tcp_addr())), bc).unwrap();
    wait_until("carrier", 5, || relay.carrier_up() && bridge.carrier_up());
    let client = UdpSocket::bind((LO, 0)).unwrap();
    client.connect(relay.udp_addr()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut lat = Vec::new();
    let mut b = [0u8; 2048];
    for _ in 0..5000 {
        let t = Instant::now();
        client.send(&[0x4A; 100]).unwrap();
        if client.recv(&mut b).is_ok() {
            lat.push(t.elapsed().as_micros() as u32);
        }
        thread::sleep(Duration::from_micros(200));
    }
    let mut l = Lat(lat);
    eprintln!(
        "BENCH small packets (100 B, one at a time) round trip us: p50 {} p99 {} max {}",
        l.pct(0.5),
        l.pct(0.99),
        l.pct(1.0)
    );
}
