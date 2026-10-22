#![allow(dead_code)]
//! Shared test rig: a UDP echo "host", a relay, a bridge, a severable TCP proxy and a fake usbmuxd.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pf_usb_tunnel::bridge::{self, BridgeConfig, BridgeHandle, TcpConnector};
use pf_usb_tunnel::carrier::Timing;
use pf_usb_tunnel::relay::{self, RelayConfig, RelayHandle};

pub const LO: Ipv4Addr = Ipv4Addr::LOCALHOST;

pub fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for: {what}");
}

/// Fast timers so the tests do not take seconds per scenario.
pub fn fast_timing() -> Timing {
    Timing {
        ping_interval: Duration::from_millis(150),
        dead_after: Duration::from_millis(1200),
        ..Timing::default()
    }
}

/// A UDP "host": echoes every datagram back to its source and remembers the source ports it saw.
pub struct Echo {
    pub addr: SocketAddr,
    pub sources: Arc<Mutex<HashSet<SocketAddr>>>,
    pub received: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    t: Option<JoinHandle<()>>,
}

impl Echo {
    pub fn start() -> Echo {
        let sock = UdpSocket::bind((LO, 0)).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let addr = sock.local_addr().unwrap();
        let sources = Arc::new(Mutex::new(HashSet::new()));
        let received = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, r2, st2) = (sources.clone(), received.clone(), stop.clone());
        let t = thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while !st2.load(Ordering::SeqCst) {
                if let Ok((n, src)) = sock.recv_from(&mut buf) {
                    r2.fetch_add(1, Ordering::Relaxed);
                    s2.lock().unwrap().insert(src);
                    let _ = sock.send_to(&buf[..n], src);
                }
            }
        });
        Echo {
            addr,
            sources,
            received,
            stop,
            t: Some(t),
        }
    }
    pub fn ports(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self
            .sources
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.port())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

pub fn relay_cfg() -> RelayConfig {
    RelayConfig {
        tcp_bind: (LO, 0).into(),
        udp_bind: (LO, 0).into(),
        timing: fast_timing(),
        ..RelayConfig::default()
    }
}

pub fn bridge_cfg(host: SocketAddr) -> BridgeConfig {
    BridgeConfig {
        host,
        timing: fast_timing(),
        reconnect_min: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(100),
        ..BridgeConfig::default()
    }
}

pub struct Rig {
    pub echo: Echo,
    pub relay: RelayHandle,
    pub bridge: BridgeHandle,
}

impl Rig {
    pub fn start() -> Rig {
        Rig::with(relay_cfg(), |_| {})
    }

    pub fn with(rcfg: RelayConfig, tweak: impl FnOnce(&mut BridgeConfig)) -> Rig {
        let echo = Echo::start();
        let relay = relay::start_with(rcfg).unwrap();
        let mut bcfg = bridge_cfg(echo.addr);
        tweak(&mut bcfg);
        let bridge = bridge::start(Box::new(TcpConnector(relay.tcp_addr())), bcfg).unwrap();
        let rig = Rig {
            echo,
            relay,
            bridge,
        };
        wait_until("carrier up", 5, || {
            rig.relay.carrier_up() && rig.bridge.carrier_up()
        });
        rig
    }

    pub fn client(&self) -> UdpSocket {
        let s = UdpSocket::bind((LO, 0)).unwrap();
        s.connect(self.relay.udp_addr()).unwrap(); // like the Apple client: only accepts the relay's address
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s
    }
}

/// A TCP proxy between bridge and relay that can be cut (and refuse reconnects).
pub struct Proxy {
    pub addr: SocketAddr,
    conns: Arc<Mutex<Vec<TcpStream>>>,
    blocked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    t: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn start(target: SocketAddr) -> Proxy {
        let l = TcpListener::bind((LO, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let blocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, b2, s2) = (conns.clone(), blocked.clone(), stop.clone());
        let t = thread::spawn(move || {
            for inc in l.incoming() {
                if s2.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(a) = inc else { continue };
                if b2.load(Ordering::SeqCst) {
                    let _ = a.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(b) = TcpStream::connect(target) else {
                    continue;
                };
                let _ = a.set_nodelay(true);
                let _ = b.set_nodelay(true);
                c2.lock().unwrap().push(a.try_clone().unwrap());
                c2.lock().unwrap().push(b.try_clone().unwrap());
                pipe(a.try_clone().unwrap(), b.try_clone().unwrap());
                pipe(b, a);
            }
        });
        Proxy {
            addr,
            conns,
            blocked,
            stop,
            t: Some(t),
        }
    }
    /// Cuts every live connection.
    pub fn sever(&self) {
        for c in self.conns.lock().unwrap().drain(..) {
            let _ = c.shutdown(Shutdown::Both);
        }
    }
    pub fn block(&self, on: bool) {
        self.blocked.store(on, Ordering::SeqCst);
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.sever();
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

/// A fake usbmuxd on a UNIX socket: answers ListDevices / Connect and, on Connect, splices the
/// socket to `devices[id].port_target` (a TCP address standing in for the iPad's listener).
#[cfg(unix)]
pub struct FakeUsbmuxd {
    pub path: std::path::PathBuf,
    pub connects: Arc<Mutex<Vec<(i64, u16)>>>,
    stop: Arc<AtomicBool>,
    t: Option<JoinHandle<()>>,
}

#[cfg(unix)]
pub struct FakeDevice {
    pub id: i64,
    pub serial: &'static str,
    pub conn_type: &'static str,
    pub target: SocketAddr,
}

#[cfg(unix)]
impl FakeUsbmuxd {
    pub fn start(dir: &std::path::Path, devices: Vec<FakeDevice>) -> FakeUsbmuxd {
        use std::os::unix::net::UnixListener;
        let path = dir.join("usbmuxd");
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        let connects: Arc<Mutex<Vec<(i64, u16)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, s2) = (connects.clone(), stop.clone());
        let devices = Arc::new(devices);
        let t = thread::spawn(move || {
            for inc in l.incoming() {
                if s2.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut s) = inc else { continue };
                let (devices, c3) = (devices.clone(), c2.clone());
                thread::spawn(move || {
                    let Some(req) = read_msg(&mut s) else { return };
                    let mt = req
                        .get("MessageType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if mt == "ListDevices" {
                        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>DeviceList</key><array>");
                        for d in devices.iter() {
                            x.push_str(&format!(
                                "<dict><key>DeviceID</key><integer>{}</integer><key>MessageType</key><string>Attached</string><key>Properties</key><dict><key>ConnectionType</key><string>{}</string><key>DeviceID</key><integer>{}</integer><key>SerialNumber</key><string>{}</string></dict></dict>",
                                d.id, d.conn_type, d.id, d.serial
                            ));
                        }
                        x.push_str("</array></dict></plist>");
                        write_msg(&mut s, x.as_bytes());
                    } else if mt == "Connect" {
                        let id = req.get("DeviceID").and_then(|v| v.as_int()).unwrap_or(-1);
                        let port = (req.get("PortNumber").and_then(|v| v.as_int()).unwrap_or(0)
                            as u16)
                            .swap_bytes();
                        c3.lock().unwrap().push((id, port));
                        let dev = devices.iter().find(|d| d.id == id);
                        let (num, up) = match dev {
                            Some(d) => match TcpStream::connect(d.target) {
                                Ok(t) => (0, Some(t)),
                                Err(_) => (3, None),
                            },
                            None => (2, None),
                        };
                        let r = format!(
                            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>MessageType</key><string>Result</string><key>Number</key><integer>{num}</integer></dict></plist>"
                        );
                        write_msg(&mut s, r.as_bytes());
                        if let Some(t) = up {
                            splice(s, t);
                        }
                    }
                });
            }
        });
        FakeUsbmuxd {
            path,
            connects,
            stop,
            t: Some(t),
        }
    }
}

#[cfg(unix)]
fn read_msg(s: &mut std::os::unix::net::UnixStream) -> Option<pf_usb_tunnel::plist::Value> {
    let mut h = [0u8; 16];
    s.read_exact(&mut h).ok()?;
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 1, "version");
    assert_eq!(
        u32::from_le_bytes([h[8], h[9], h[10], h[11]]),
        8,
        "plist request"
    );
    let mut b = vec![0u8; len - 16];
    s.read_exact(&mut b).ok()?;
    pf_usb_tunnel::plist::parse(&b).ok()
}

#[cfg(unix)]
fn write_msg(s: &mut std::os::unix::net::UnixStream, body: &[u8]) {
    let mut v = Vec::new();
    v.extend_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(body);
    s.write_all(&v).unwrap();
}

#[cfg(unix)]
fn splice(u: std::os::unix::net::UnixStream, t: TcpStream) {
    let (mut u2, mut t2) = (u.try_clone().unwrap(), t.try_clone().unwrap());
    let (mut u1, mut t1) = (u, t);
    thread::spawn(move || {
        let _ = std::io::copy(&mut u1, &mut t1);
        let _ = t1.shutdown(Shutdown::Both);
    });
    thread::spawn(move || {
        let _ = std::io::copy(&mut t2, &mut u2);
        let _ = u2.shutdown(Shutdown::Both);
    });
}

#[cfg(unix)]
impl Drop for FakeUsbmuxd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A 8+ byte datagram tagged with (flow label, sequence). First byte 0x00 = media-shaped.
pub fn tagged(id: u16, seq: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.max(8)];
    v[0] = 0x00; // media
    v[1..3].copy_from_slice(&id.to_be_bytes());
    v[3..7].copy_from_slice(&seq.to_be_bytes());
    v
}

pub fn untag(d: &[u8]) -> (u16, u32) {
    (
        u16::from_be_bytes([d[1], d[2]]),
        u32::from_be_bytes([d[3], d[4], d[5], d[6]]),
    )
}
