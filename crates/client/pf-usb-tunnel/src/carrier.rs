//! One carrier session: HELLO exchange, the writer, the watchdog and the read loop. Shared by
//! the relay and the bridge; they differ only in the [`Handler`] they plug in.
//!
//! Threads of a session: the CALLER runs the reader (blocking, returns when the carrier ends),
//! one writer thread drains the [`TxQueue`], one watchdog thread sends PINGs and kills a carrier
//! that has been silent for `dead_after`. Any of the three ending closes the queue and shuts the
//! socket down, which unblocks the other two.

use std::io::{self, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::frame::{
    self, Frame, FrameReader, Hello, Role, KIND_HELLO, KIND_PING, KIND_PONG, VERSION,
};
use crate::link::Link;
use crate::queue::TxQueue;
use crate::stats::{self, Counters};

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// PING cadence while the carrier is up.
    pub ping_interval: Duration,
    /// No frame received for this long: the carrier is dead (cable pulled without an RST).
    pub dead_after: Duration,
    /// Kernel send buffer of the carrier socket. Small on purpose, see `link::Link::configure`.
    pub link_send_buf: usize,
    /// Most bytes the writer coalesces into one `write`.
    pub max_batch: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ping_interval: Duration::from_secs(1),
            dead_after: Duration::from_secs(4),
            link_send_buf: 64 * 1024,
            max_batch: 48 * 1024,
        }
    }
}

/// What the relay / bridge does with the carrier's traffic.
pub trait Handler {
    /// The peer's HELLO arrived (version and role already checked). Publish the queue to the
    /// datagram producers here and send any resync frames; until this returns, nothing but
    /// HELLO/PING/PONG has been queued.
    fn on_hello(&mut self, peer: Hello) -> io::Result<()>;
    /// OPEN / DATA / CLOSE and unknown kinds.
    fn on_frame(&mut self, f: Frame<'_>);
}

/// Runs one carrier to its end. `expect_peer` is the role the other side must announce.
pub fn run(
    link: Link,
    q: Arc<TxQueue>,
    me: Role,
    generation: u32,
    timing: Timing,
    counters: &Arc<Counters>,
    handler: &mut dyn Handler,
) -> io::Result<()> {
    link.configure(timing.link_send_buf, timing.dead_after);
    let writer_link = link.try_clone()?;
    let kill_link = link.try_clone()?;
    let base = Instant::now();
    let last_rx = Arc::new(AtomicU64::new(0)); // micros since `base`

    q.push_ctrl(
        KIND_HELLO,
        0,
        &Hello {
            version: VERSION,
            role: me as u8,
            generation,
        }
        .encode(),
    );

    let writer = {
        let q = q.clone();
        let c = counters.clone();
        let mut w = writer_link;
        let kill = kill_link.try_clone()?;
        thread::Builder::new()
            .name("pf-usb-tx".into())
            .spawn(move || {
                let mut batch = Vec::with_capacity(timing.max_batch + frame::MAX_PAYLOAD + 8);
                while {
                    batch.clear();
                    q.fill(&mut batch, timing.max_batch)
                } {
                    if let Err(e) = w.write_all(&batch) {
                        tracing::debug!("carrier write failed: {e}");
                        break;
                    }
                    stats::add(&c.tx_bytes, batch.len() as u64);
                }
                q.close();
                kill.shutdown();
            })?
    };

    let watchdog = {
        let q = q.clone();
        let last_rx = last_rx.clone();
        let kill = kill_link.try_clone()?;
        thread::Builder::new()
            .name("pf-usb-wd".into())
            .spawn(move || loop {
                if q.sleep_unless_closed(timing.ping_interval) {
                    break;
                }
                let age = base
                    .elapsed()
                    .saturating_sub(Duration::from_micros(last_rx.load(Ordering::Relaxed)));
                if age > timing.dead_after {
                    tracing::info!("carrier silent for {age:?}, closing it");
                    q.close();
                    kill.shutdown();
                    break;
                }
                q.push_ctrl(
                    KIND_PING,
                    0,
                    &(base.elapsed().as_micros() as u64).to_be_bytes(),
                );
            })?
    };

    let result = read_loop(link, &q, me, base, &last_rx, counters, handler);

    q.close();
    kill_link.shutdown();
    let _ = writer.join();
    let _ = watchdog.join();
    result
}

fn read_loop(
    link: Link,
    q: &Arc<TxQueue>,
    me: Role,
    base: Instant,
    last_rx: &AtomicU64,
    counters: &Arc<Counters>,
    handler: &mut dyn Handler,
) -> io::Result<()> {
    let mut rd = FrameReader::new(BufReader::with_capacity(128 * 1024, link));
    let mut got_hello = false;
    loop {
        let Some(f) = rd.read_frame()? else {
            return Ok(());
        };
        last_rx.store(base.elapsed().as_micros() as u64, Ordering::Relaxed);
        stats::inc(&counters.rx_frames);
        stats::add(
            &counters.rx_bytes,
            (frame::HEADER_LEN + f.payload.len()) as u64,
        );
        if !got_hello {
            if f.kind != KIND_HELLO {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "first frame is not HELLO",
                ));
            }
            let h = Hello::decode(f.payload)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short HELLO"))?;
            if h.version != VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("peer speaks tunnel v{}, we speak v{VERSION}", h.version),
                ));
            }
            let peer_role = Role::from_u8(h.role);
            let want = match me {
                Role::Relay => Role::Bridge,
                Role::Bridge => Role::Relay,
            };
            if peer_role != Some(want) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("peer role {} is wrong", h.role),
                ));
            }
            got_hello = true;
            handler.on_hello(h)?;
            stats::inc(&counters.carriers_total);
            continue;
        }
        match f.kind {
            KIND_PING => {
                q.push_ctrl(KIND_PONG, 0, f.payload);
            }
            KIND_PONG => {
                if let Ok(b) = <[u8; 8]>::try_from(f.payload) {
                    let sent = u64::from_be_bytes(b);
                    let now = base.elapsed().as_micros() as u64;
                    stats::set(&counters.rtt_us, now.saturating_sub(sent).max(1));
                }
            }
            KIND_HELLO => {}
            _ => handler.on_frame(f),
        }
    }
}
