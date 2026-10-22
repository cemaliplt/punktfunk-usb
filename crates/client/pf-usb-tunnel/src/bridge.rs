//! The PC side. Dials the carrier (through usbmuxd, or plain TCP in tests) and, for every flow the
//! relay opens, owns one UDP socket `connect()`ed to the real host (`127.0.0.1:9777`).
//!
//! * The host-side source port of a flow is the port of that socket and never changes while the
//!   flow lives. That is what keeps the host's Retry token and QUIC connection valid, and it is
//!   why the sockets outlive a carrier: after a cable glitch the bridge reconnects and the
//!   relay re-announces its flows with OPEN (idempotent), and traffic resumes on the same
//!   ports. Without a carrier for `grace` (or when the relay's HELLO `generation` changed) the
//!   sockets are dropped.
//! * Each flow has a reader thread: host datagram -> the carrier's queue (QUIC to the priority
//!   queue, media to the tail-drop queue, see `queue.rs`). The UDP receive buffer is asked to be
//!   large (the host paces a keyframe out at line rate; the host asks for 32 MiB itself).
//! * The reconnect loop never gives up; it backs off 100 ms -> 2 s.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::carrier::{self, Handler, Timing};
use crate::frame::{Frame, Hello, Role, KIND_CLOSE, KIND_DATA, KIND_OPEN};
use crate::link::Link;
use crate::net;
use crate::queue::{QueueConfig, TxQueue};
use crate::stats::{self, Counters, Stats};

/// Where carriers come from. The bridge calls `connect` again after every carrier ends.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self) -> io::Result<Link>;
    fn describe(&self) -> String;
}

/// A plain TCP address (test mode: relay and bridge on one machine, or over `adb forward`-like
/// plumbing).
#[derive(Clone, Debug)]
pub struct TcpConnector(pub SocketAddr);

impl Connector for TcpConnector {
    fn connect(&self) -> io::Result<Link> {
        Ok(Link::Tcp(TcpStream::connect_timeout(
            &self.0,
            Duration::from_secs(2),
        )?))
    }
    fn describe(&self) -> String {
        format!("tcp {}", self.0)
    }
}

#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// The real host's native port.
    pub host: SocketAddr,
    pub queue: QueueConfig,
    pub timing: Timing,
    pub flow_idle: Duration,
    pub grace: Duration,
    pub max_flows: usize,
    pub udp_rcvbuf: usize,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        BridgeConfig {
            host: SocketAddr::from((Ipv4Addr::LOCALHOST, 9777)),
            queue: QueueConfig::default(),
            timing: Timing::default(),
            flow_idle: Duration::from_secs(30),
            grace: Duration::from_secs(10),
            max_flows: 1024,
            udp_rcvbuf: 32 * 1024 * 1024,
            reconnect_min: Duration::from_millis(100),
            reconnect_max: Duration::from_secs(2),
        }
    }
}

struct Flow {
    sock: UdpSocket,
    closed: AtomicBool,
    /// Milliseconds since `Shared::base` of the last datagram in either direction.
    last_ms: AtomicU64,
}

struct Shared {
    cfg: BridgeConfig,
    base: Instant,
    flows: Mutex<HashMap<u16, Arc<Flow>>>,
    /// The queue of the carrier that finished HELLO.
    tx: Mutex<Option<Arc<TxQueue>>>,
    counters: Arc<Counters>,
    stop: AtomicBool,
    last_generation: Mutex<Option<u32>>,
    down_since: Mutex<Option<Instant>>,
    active: Mutex<Option<Link>>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

pub struct BridgeHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    maint: Option<thread::Thread>,
}

pub fn start(connector: Box<dyn Connector>, cfg: BridgeConfig) -> io::Result<BridgeHandle> {
    let counters = Arc::new(Counters::default());
    let shared = Arc::new(Shared {
        cfg,
        base: Instant::now(),
        flows: Mutex::new(HashMap::new()),
        tx: Mutex::new(None),
        counters,
        stop: AtomicBool::new(false),
        last_generation: Mutex::new(None),
        down_since: Mutex::new(None),
        active: Mutex::new(None),
    });
    let mut threads = Vec::new();
    {
        let s = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("pf-usb-dial".into())
                .spawn(move || dial_loop(s, connector))?,
        );
    }
    let maint = {
        let s = shared.clone();
        let t = thread::Builder::new()
            .name("pf-usb-maint".into())
            .spawn(move || maint_loop(s))?;
        let th = t.thread().clone();
        threads.push(t);
        Some(th)
    };
    Ok(BridgeHandle {
        shared,
        threads,
        maint,
    })
}

impl BridgeHandle {
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }
    pub fn carrier_up(&self) -> bool {
        self.shared.tx.lock().unwrap().is_some()
    }
    pub fn flows(&self) -> usize {
        self.shared.flows.lock().unwrap().len()
    }
    /// Host-side UDP source ports of the live flows (sorted by flow id), for tests and logs.
    pub fn flow_ports(&self) -> Vec<(u16, u16)> {
        let f = self.shared.flows.lock().unwrap();
        let mut v: Vec<_> = f
            .iter()
            .filter_map(|(id, fl)| fl.sock.local_addr().ok().map(|a| (*id, a.port())))
            .collect();
        v.sort();
        v
    }

    pub fn stop(&mut self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(l) = self.shared.active.lock().unwrap().take() {
            l.shutdown();
        }
        if let Some(t) = self.maint.take() {
            t.unpark();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        close_all(&self.shared);
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn close_all(s: &Shared) {
    let mut f = s.flows.lock().unwrap();
    for fl in f.values() {
        fl.closed.store(true, Ordering::SeqCst);
    }
    f.clear();
    stats::set(&s.counters.flows_active, 0);
}

fn dial_loop(s: Arc<Shared>, connector: Box<dyn Connector>) {
    let what = connector.describe();
    let mut backoff = s.cfg.reconnect_min;
    let mut last_err_log = None::<Instant>;
    while !s.stop.load(Ordering::SeqCst) {
        match connector.connect() {
            Ok(link) => {
                backoff = s.cfg.reconnect_min;
                tracing::info!("bridge: carrier up via {what}");
                let Ok(mine) = link.try_clone() else { continue };
                *s.active.lock().unwrap() = Some(mine);
                let q = TxQueue::new(s.cfg.queue, s.counters.clone());
                let mut h = BridgeHandler {
                    s: s.clone(),
                    q: q.clone(),
                };
                let res = carrier::run(link, q, Role::Bridge, 0, s.cfg.timing, &s.counters, &mut h);
                *s.active.lock().unwrap() = None;
                *s.tx.lock().unwrap() = None;
                *s.down_since.lock().unwrap() = Some(Instant::now());
                stats::set(&s.counters.carrier_up, 0);
                match res {
                    Ok(()) => tracing::info!("bridge: carrier closed"),
                    Err(e) => {
                        stats::inc(&s.counters.carrier_errors);
                        tracing::info!("bridge: carrier ended: {e}");
                    }
                }
            }
            Err(e) => {
                stats::inc(&s.counters.carrier_errors);
                let quiet = last_err_log.is_some_and(|t| t.elapsed() < Duration::from_secs(10));
                if !quiet {
                    tracing::warn!("bridge: cannot reach the iPad via {what}: {e} (retrying)");
                    last_err_log = Some(Instant::now());
                }
            }
        }
        // sleep in small steps so stop() is prompt
        let until = Instant::now() + backoff;
        while Instant::now() < until && !s.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }
        backoff = (backoff * 2).min(s.cfg.reconnect_max);
    }
}

struct BridgeHandler {
    s: Arc<Shared>,
    q: Arc<TxQueue>,
}

impl Handler for BridgeHandler {
    fn on_hello(&mut self, peer: Hello) -> io::Result<()> {
        {
            let mut g = self.s.last_generation.lock().unwrap();
            if g.is_some_and(|old| old != peer.generation) {
                tracing::info!(
                    "bridge: relay forgot its flows (generation changed), dropping ours"
                );
                close_all(&self.s);
            }
            *g = Some(peer.generation);
        }
        *self.s.tx.lock().unwrap() = Some(self.q.clone());
        *self.s.down_since.lock().unwrap() = None;
        stats::set(&self.s.counters.carrier_up, 1);
        Ok(())
    }

    fn on_frame(&mut self, f: Frame<'_>) {
        let c = &self.s.counters;
        match f.kind {
            KIND_OPEN => {
                let exists = self.s.flows.lock().unwrap().contains_key(&f.flow);
                if !exists {
                    if let Err(e) = open_flow(&self.s, f.flow) {
                        tracing::warn!("bridge: cannot open flow {}: {e}", f.flow);
                        self.q.push_ctrl(KIND_CLOSE, f.flow, &[]);
                    }
                }
            }
            KIND_DATA => {
                let fl = self.s.flows.lock().unwrap().get(&f.flow).cloned();
                match fl {
                    Some(fl) => {
                        fl.last_ms.store(self.s.now_ms(), Ordering::Relaxed);
                        match fl.sock.send(f.payload) {
                            Ok(_) => stats::inc(&c.udp_out),
                            Err(_) => stats::inc(&c.udp_send_errors),
                        }
                    }
                    None => {
                        stats::inc(&c.drop_unknown_flow);
                        self.q.push_ctrl(KIND_CLOSE, f.flow, &[]);
                    }
                }
            }
            KIND_CLOSE => {
                let mut flows = self.s.flows.lock().unwrap();
                if let Some(fl) = flows.remove(&f.flow) {
                    fl.closed.store(true, Ordering::SeqCst);
                    stats::inc(&c.flows_closed);
                    stats::set(&c.flows_active, flows.len() as u64);
                }
            }
            _ => {}
        }
    }
}

fn open_flow(s: &Arc<Shared>, id: u16) -> io::Result<()> {
    if s.flows.lock().unwrap().len() >= s.cfg.max_flows {
        return Err(io::Error::other("flow table full"));
    }
    let bind: SocketAddr = if s.cfg.host.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    // A loopback host gets a loopback-bound socket so the source address is 127.0.0.1, which is
    // what the host's decisions assume (01-tasarim.md section 2).
    let bind = if s.cfg.host.ip().is_loopback() {
        match s.cfg.host {
            SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            SocketAddr::V6(_) => SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
        }
    } else {
        bind
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(s.cfg.host)?;
    let granted = net::grow_udp_recv_buffer(&sock, s.cfg.udp_rcvbuf);
    stats::set(&s.counters.udp_rcvbuf, granted as u64);
    net::grow_udp_send_buffer(&sock, 4 * 1024 * 1024);
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let flow = Arc::new(Flow {
        sock,
        closed: AtomicBool::new(false),
        last_ms: AtomicU64::new(s.now_ms()),
    });
    tracing::debug!(
        "bridge: flow {id} -> host from local port {}",
        flow.sock.local_addr().map(|a| a.port()).unwrap_or(0)
    );
    s.flows.lock().unwrap().insert(id, flow.clone());
    stats::inc(&s.counters.flows_opened);
    stats::set(
        &s.counters.flows_active,
        s.flows.lock().unwrap().len() as u64,
    );
    let s2 = s.clone();
    thread::Builder::new()
        .name(format!("pf-usb-flow{id}"))
        .spawn(move || flow_reader(s2, id, flow))?;
    Ok(())
}

fn flow_reader(s: Arc<Shared>, id: u16, flow: Arc<Flow>) {
    let mut buf = vec![0u8; 65536];
    let c = &s.counters;
    loop {
        if flow.closed.load(Ordering::SeqCst) || s.stop.load(Ordering::SeqCst) {
            return;
        }
        match flow.sock.recv(&mut buf) {
            Ok(n) => {
                stats::inc(&c.udp_in);
                flow.last_ms.store(s.now_ms(), Ordering::Relaxed);
                let q = s.tx.lock().unwrap().clone();
                match q {
                    Some(q) => {
                        q.push_datagram(id, &buf[..n]);
                    }
                    None => stats::inc(&c.drop_no_carrier),
                }
            }
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::Interrupted => {}
                // ICMP "port unreachable" from a host that is not (yet) listening.
                io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset => {
                    thread::sleep(Duration::from_millis(5));
                }
                _ => {
                    tracing::warn!("bridge: flow {id} socket failed: {e}");
                    if let Some(q) = s.tx.lock().unwrap().clone() {
                        q.push_ctrl(KIND_CLOSE, id, &[]);
                    }
                    let mut f = s.flows.lock().unwrap();
                    if f.get(&id).is_some_and(|x| Arc::ptr_eq(x, &flow)) {
                        f.remove(&id);
                    }
                    return;
                }
            },
        }
    }
}

fn maint_loop(s: Arc<Shared>) {
    while !s.stop.load(Ordering::SeqCst) {
        thread::park_timeout(Duration::from_secs(1));
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        let now = s.now_ms();
        let idle_ms = s.cfg.flow_idle.as_millis() as u64;
        let q = s.tx.lock().unwrap().clone();
        let mut f = s.flows.lock().unwrap();
        let idle: Vec<u16> = f
            .iter()
            .filter(|(_, fl)| now.saturating_sub(fl.last_ms.load(Ordering::Relaxed)) > idle_ms)
            .map(|(i, _)| *i)
            .collect();
        for id in idle {
            if let Some(fl) = f.remove(&id) {
                fl.closed.store(true, Ordering::SeqCst);
                if let Some(q) = &q {
                    q.push_ctrl(KIND_CLOSE, id, &[]);
                }
                stats::inc(&s.counters.flows_closed);
                tracing::debug!("bridge: flow {id} idle, closed");
            }
        }
        if q.is_none() && !f.is_empty() {
            if let Some(since) = *s.down_since.lock().unwrap() {
                if since.elapsed() > s.cfg.grace {
                    tracing::info!(
                        "bridge: no carrier for {:?}, dropping {} flows",
                        s.cfg.grace,
                        f.len()
                    );
                    for fl in f.values() {
                        fl.closed.store(true, Ordering::SeqCst);
                    }
                    f.clear();
                }
            }
        }
        stats::set(&s.counters.flows_active, f.len() as u64);
    }
}
