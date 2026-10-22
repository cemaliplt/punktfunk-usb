//! The iPad side. Listens for the carrier (TCP, normally 127.0.0.1:9779: usbmuxd delivers its
//! connections to the device's loopback) and for the Punktfunk client's datagrams on a local UDP
//! socket (normally 127.0.0.1:9777, where the client believes the host is).
//!
//! * Every distinct source address that talks to the UDP socket is a FLOW with its own id. The
//!   first datagram of a flow sends OPEN, then DATA. The bridge's answers come back as DATA for
//!   that id and leave the UDP socket from its bound address, `127.0.0.1:9777`: the Apple
//!   client's socket is `connect()`ed to that address and drops anything else.
//! * One carrier at a time. A new connection replaces the old one (the PC reconnects before the
//!   iPad notices the old cable is dead). After a carrier dies the flow table is kept for
//!   `grace`; if the same bridge reconnects within it the client's QUIC session (idle timeout
//!   8 s) carries on over the same flow ids, and the bridge keeps the same host-side UDP source
//!   ports. Later than that the table is cleared and `generation` bumps, which tells a bridge that
//!   still holds sockets to drop them.
//! * Flows idle for `flow_idle` (default 30 s) are closed (the 10 s reachability probes and the
//!   pairing handshakes use short-lived client sockets).

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::carrier::{self, Handler, Timing};
use crate::frame::{Frame, Hello, Role, KIND_CLOSE, KIND_DATA, KIND_OPEN};
use crate::link::Link;
use crate::net;
use crate::queue::{QueueConfig, TxQueue};
use crate::stats::{self, Counters, Stats};

#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Where the bridge's carrier connects. Loopback by default: nothing else on the Wi-Fi
    /// should be able to inject datagrams.
    pub tcp_bind: SocketAddr,
    /// Where the client sends. This is the "host address" the client is configured with.
    pub udp_bind: SocketAddr,
    pub queue: QueueConfig,
    pub timing: Timing,
    pub flow_idle: Duration,
    pub grace: Duration,
    pub max_flows: usize,
    /// Wanted kernel receive buffer of the UDP socket (the client may burst at the relay).
    pub udp_rcvbuf: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig {
            tcp_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 9779)),
            udp_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 9777)),
            queue: QueueConfig::default(),
            timing: Timing::default(),
            flow_idle: Duration::from_secs(30),
            grace: Duration::from_secs(10),
            max_flows: 1024,
            udp_rcvbuf: 32 * 1024 * 1024,
        }
    }
}

struct FlowEntry {
    addr: SocketAddr,
    last: Instant,
}

#[derive(Default)]
struct Tables {
    by_addr: HashMap<SocketAddr, u16>,
    by_id: HashMap<u16, FlowEntry>,
    next_id: u16,
    /// The queue of the carrier that finished HELLO, with that carrier's id.
    tx: Option<(u64, Arc<TxQueue>)>,
    down_since: Option<Instant>,
}

impl Tables {
    fn alloc_id(&mut self) -> Option<u16> {
        for _ in 0..u16::MAX {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id == 0 {
                self.next_id = 1;
            }
            if !self.by_id.contains_key(&self.next_id) {
                return Some(self.next_id);
            }
        }
        None
    }

    fn remove(&mut self, id: u16) -> bool {
        match self.by_id.remove(&id) {
            Some(e) => {
                self.by_addr.remove(&e.addr);
                true
            }
            None => false,
        }
    }
}

struct Shared {
    cfg: RelayConfig,
    udp: UdpSocket,
    tables: Mutex<Tables>,
    counters: Arc<Counters>,
    stop: AtomicBool,
    generation: AtomicU32,
    active: Mutex<Option<(u64, Link)>>,
    next_carrier: AtomicU64,
}

pub struct RelayHandle {
    shared: Arc<Shared>,
    tcp_addr: SocketAddr,
    udp_addr: SocketAddr,
    threads: Vec<JoinHandle<()>>,
    maint: Option<thread::Thread>,
}

/// Starts a relay with defaults: carrier on `127.0.0.1:tcp_port`, client datagrams on `udp_bind`.
pub fn start(tcp_port: u16, udp_bind: SocketAddr) -> io::Result<RelayHandle> {
    start_with(RelayConfig {
        tcp_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        udp_bind,
        ..RelayConfig::default()
    })
}

pub fn start_with(cfg: RelayConfig) -> io::Result<RelayHandle> {
    let listener = TcpListener::bind(cfg.tcp_bind)?;
    let udp = UdpSocket::bind(cfg.udp_bind)?;
    let counters = Arc::new(Counters::default());
    let granted = net::grow_udp_recv_buffer(&udp, cfg.udp_rcvbuf);
    stats::set(&counters.udp_rcvbuf, granted as u64);
    let tcp_addr = listener.local_addr()?;
    let udp_addr = udp.local_addr()?;
    let generation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(1);
    let shared = Arc::new(Shared {
        cfg,
        udp,
        tables: Mutex::new(Tables::default()),
        counters,
        stop: AtomicBool::new(false),
        generation: AtomicU32::new(generation),
        active: Mutex::new(None),
        next_carrier: AtomicU64::new(1),
    });
    tracing::info!("relay: carrier on {tcp_addr}, datagrams on {udp_addr}, udp rcvbuf {granted} B");

    let mut threads = Vec::new();
    {
        let s = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("pf-usb-accept".into())
                .spawn(move || accept_loop(s, listener))?,
        );
    }
    {
        let s = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("pf-usb-udp".into())
                .spawn(move || udp_loop(s))?,
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
    Ok(RelayHandle {
        shared,
        tcp_addr,
        udp_addr,
        threads,
        maint,
    })
}

impl RelayHandle {
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }
    /// The carrier listener's actual address (useful when started on port 0).
    pub fn tcp_addr(&self) -> SocketAddr {
        self.tcp_addr
    }
    /// The client-facing UDP socket's actual address.
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp_addr
    }
    pub fn carrier_up(&self) -> bool {
        self.shared.tables.lock().unwrap().tx.is_some()
    }
    pub fn flows(&self) -> usize {
        self.shared.tables.lock().unwrap().by_id.len()
    }

    /// Stops all threads and closes the carrier. Idempotent.
    pub fn stop(&mut self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some((_, l)) = self.shared.active.lock().unwrap().take() {
            l.shutdown();
        }
        // Wake the two blocking threads: a connection for the acceptor, a datagram for the
        // UDP reader. Both check `stop` first thing.
        let _ = TcpStream::connect_timeout(&loopback_of(self.tcp_addr), Duration::from_millis(500));
        if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = s.send_to(&[0], loopback_of(self.udp_addr));
        }
        if let Some(t) = self.maint.take() {
            t.unpark();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn loopback_of(a: SocketAddr) -> SocketAddr {
    if a.ip().is_unspecified() {
        SocketAddr::from((Ipv4Addr::LOCALHOST, a.port()))
    } else {
        a
    }
}

fn accept_loop(s: Arc<Shared>, listener: TcpListener) {
    loop {
        let (conn, peer) = match listener.accept() {
            Ok(x) => x,
            Err(e) => {
                if s.stop.load(Ordering::SeqCst) {
                    return;
                }
                tracing::warn!("relay: accept failed: {e}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        let id = s.next_carrier.fetch_add(1, Ordering::SeqCst);
        let link = Link::Tcp(conn);
        let Ok(mine) = link.try_clone() else { continue };
        // Replace the previous carrier: shutting it down ends its thread.
        if let Some((_, old)) = s.active.lock().unwrap().replace((id, mine)) {
            tracing::info!("relay: new carrier from {peer} replaces the old one");
            old.shutdown();
        } else {
            tracing::info!("relay: carrier from {peer}");
        }
        let s2 = s.clone();
        let _ = thread::Builder::new()
            .name("pf-usb-rx".into())
            .spawn(move || carrier_thread(s2, id, link));
    }
}

fn carrier_thread(s: Arc<Shared>, id: u64, link: Link) {
    let q = TxQueue::new(s.cfg.queue, s.counters.clone());
    let mut h = RelayHandler {
        s: s.clone(),
        id,
        q: q.clone(),
    };
    let gen_ = s.generation.load(Ordering::SeqCst);
    let res = carrier::run(
        link,
        q.clone(),
        Role::Relay,
        gen_,
        s.cfg.timing,
        &s.counters,
        &mut h,
    );
    match res {
        Ok(()) => tracing::info!("relay: carrier closed"),
        Err(e) => {
            stats::inc(&s.counters.carrier_errors);
            tracing::info!("relay: carrier ended: {e}");
        }
    }
    let mut t = s.tables.lock().unwrap();
    if matches!(&t.tx, Some((cid, _)) if *cid == id) {
        t.tx = None;
        t.down_since = Some(Instant::now());
        stats::set(&s.counters.carrier_up, 0);
    }
    drop(t);
    let mut a = s.active.lock().unwrap();
    if matches!(&*a, Some((cid, _)) if *cid == id) {
        *a = None;
    }
}

struct RelayHandler {
    s: Arc<Shared>,
    id: u64,
    q: Arc<TxQueue>,
}

impl Handler for RelayHandler {
    fn on_hello(&mut self, _peer: Hello) -> io::Result<()> {
        let mut t = self.s.tables.lock().unwrap();
        // The bridge may be a new process (no sockets) or the old one coming back: OPEN is
        // idempotent, so tell it about every flow we still hold before any DATA flows.
        for id in t.by_id.keys() {
            self.q.push_ctrl(KIND_OPEN, *id, &[]);
        }
        t.tx = Some((self.id, self.q.clone()));
        t.down_since = None;
        stats::set(&self.s.counters.carrier_up, 1);
        Ok(())
    }

    fn on_frame(&mut self, f: Frame<'_>) {
        let c = &self.s.counters;
        match f.kind {
            KIND_DATA => {
                let addr = {
                    let mut t = self.s.tables.lock().unwrap();
                    match t.by_id.get_mut(&f.flow) {
                        Some(e) => {
                            e.last = Instant::now();
                            Some(e.addr)
                        }
                        None => None,
                    }
                };
                match addr {
                    Some(a) => match self.s.udp.send_to(f.payload, a) {
                        Ok(_) => stats::inc(&c.udp_out),
                        Err(_) => stats::inc(&c.udp_send_errors),
                    },
                    None => stats::inc(&c.drop_unknown_flow),
                }
            }
            KIND_CLOSE => {
                let mut t = self.s.tables.lock().unwrap();
                if t.remove(f.flow) {
                    stats::inc(&c.flows_closed);
                    stats::set(&c.flows_active, t.by_id.len() as u64);
                }
            }
            _ => {}
        }
    }
}

fn udp_loop(s: Arc<Shared>) {
    let mut buf = vec![0u8; 65536];
    let c = &s.counters;
    loop {
        let (n, src) = match s.udp.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) => {
                if s.stop.load(Ordering::SeqCst) {
                    return;
                }
                // ICMP port unreachable echoes from earlier sends surface as ConnectionReset
                // on some platforms; they are not fatal.
                if e.kind() != io::ErrorKind::ConnectionReset
                    && e.kind() != io::ErrorKind::ConnectionRefused
                {
                    tracing::warn!("relay: udp recv: {e}");
                    thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
        };
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        stats::inc(&c.udp_in);
        let mut guard = s.tables.lock().unwrap();
        let t = &mut *guard;
        if t.tx.is_none() {
            stats::inc(&c.drop_no_carrier);
            continue;
        }
        let now = Instant::now();
        let id = match t.by_addr.get(&src) {
            Some(&id) => id,
            None => {
                if t.by_id.len() >= s.cfg.max_flows {
                    stats::inc(&c.drop_no_flow);
                    continue;
                }
                let Some(id) = t.alloc_id() else {
                    stats::inc(&c.drop_no_flow);
                    continue;
                };
                t.by_addr.insert(src, id);
                t.by_id.insert(
                    id,
                    FlowEntry {
                        addr: src,
                        last: now,
                    },
                );
                if let Some((_, q)) = &t.tx {
                    q.push_ctrl(KIND_OPEN, id, &[]);
                }
                stats::inc(&c.flows_opened);
                stats::set(&c.flows_active, t.by_id.len() as u64);
                id
            }
        };
        if let Some(e) = t.by_id.get_mut(&id) {
            e.last = now;
        }
        if let Some((_, q)) = &t.tx {
            q.push_datagram(id, &buf[..n]);
        }
    }
}

fn maint_loop(s: Arc<Shared>) {
    while !s.stop.load(Ordering::SeqCst) {
        thread::park_timeout(Duration::from_secs(1));
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let mut t = s.tables.lock().unwrap();
        let idle: Vec<u16> = t
            .by_id
            .iter()
            .filter(|(_, e)| now.duration_since(e.last) > s.cfg.flow_idle)
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            t.remove(id);
            if let Some((_, q)) = &t.tx {
                q.push_ctrl(KIND_CLOSE, id, &[]);
            }
            stats::inc(&s.counters.flows_closed);
            tracing::debug!("relay: flow {id} idle, closed");
        }
        if t.tx.is_none() && !t.by_id.is_empty() {
            if let Some(since) = t.down_since {
                if now.duration_since(since) > s.cfg.grace {
                    tracing::info!(
                        "relay: carrier gone for {:?}, forgetting {} flows",
                        s.cfg.grace,
                        t.by_id.len()
                    );
                    t.by_id.clear();
                    t.by_addr.clear();
                    s.generation.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        stats::set(&s.counters.flows_active, t.by_id.len() as u64);
    }
}
