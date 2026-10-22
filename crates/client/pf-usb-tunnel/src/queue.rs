//! The carrier's send queue, and the whole drop policy of the tunnel.
//!
//! Two queues feed one writer:
//!
//! * `prio`: control frames and QUIC datagrams. QUIC retransmits what it loses but a lost QUIC
//!   packet also halves its congestion window and stalls the control stream, so these are the
//!   last thing to drop: the cap is large (it exists only so memory stays bounded when the cable
//!   is dead) and the writer always serves this queue first.
//! * `media`: raw Punktfunk media datagrams (`is_media`). They are FEC-protected and the client
//!   asks for a keyframe when a frame is lost, and, more to the point, an old video packet is
//!   worthless. So the media queue is bounded in BYTES and in AGE:
//!     - tail drop: a packet that would push the queue past `media_cap_bytes` is dropped (the
//!       newest loses, like a full socket buffer would; the host's ABR reads the resulting
//!       loss/delay as a wall, which is exactly what a real bottleneck link would show it);
//!     - stale drop: at dequeue a packet older than `media_max_age` is dropped (after a stall
//!       the cable would otherwise first deliver 200 ms of video the decoder will discard).
//!
//! Nothing here ever blocks the producer: `push_*` returns at once. The only blocking call is
//! the writer's [`TxQueue::fill`]. The kernel send buffer of the carrier is kept small
//! ([`crate::link::Link::configure`]) so the queue that grows when the cable is slow is THIS
//! one, where we can drop, not the kernel's, where we cannot.
//!
//! To keep media from starving behind a long QUIC burst, one writer batch gives `prio` at most
//! half of the batch while media is waiting.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::frame::{self, KIND_DATA, MAX_PAYLOAD};
use crate::stats::{self, Counters};

#[derive(Clone, Copy, Debug)]
pub struct QueueConfig {
    /// Media queue bound, bytes of wire data (header included). Design start point:
    /// `max(512 KiB, ~25 ms x link rate)`: 512 KiB is 22 ms at 190 Mbit/s.
    pub media_cap_bytes: usize,
    /// Media older than this is dropped at dequeue.
    pub media_max_age: Duration,
    /// Priority (QUIC + control) queue bound, bytes. Memory guard only.
    pub prio_cap_bytes: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            media_cap_bytes: 512 * 1024,
            media_max_age: Duration::from_millis(100),
            prio_cap_bytes: 4 * 1024 * 1024,
        }
    }
}

struct Item {
    bytes: Vec<u8>,
    at: Instant,
}

#[derive(Default)]
struct Inner {
    prio: VecDeque<Item>,
    prio_bytes: usize,
    media: VecDeque<Item>,
    media_bytes: usize,
    closed: bool,
}

/// What `push_*` did with the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    Dropped,
}

pub struct TxQueue {
    inner: Mutex<Inner>,
    /// Writer wakeup (data or close).
    data_cv: Condvar,
    /// Watchdog wakeup (close only).
    closed_cv: Condvar,
    cfg: QueueConfig,
    counters: Arc<Counters>,
}

impl TxQueue {
    pub fn new(cfg: QueueConfig, counters: Arc<Counters>) -> Arc<TxQueue> {
        Arc::new(TxQueue {
            inner: Mutex::new(Inner::default()),
            data_cv: Condvar::new(),
            closed_cv: Condvar::new(),
            cfg,
            counters,
        })
    }

    /// A control frame (HELLO, OPEN, CLOSE, PING, PONG): always the priority queue.
    pub fn push_ctrl(&self, kind: u8, flow: u16, payload: &[u8]) -> Push {
        self.push_prio(frame::encode(kind, flow, payload))
    }

    /// One UDP datagram as a DATA frame, routed by its first byte.
    pub fn push_datagram(&self, flow: u16, data: &[u8]) -> Push {
        if data.len() > MAX_PAYLOAD {
            stats::inc(&self.counters.drop_oversize);
            return Push::Dropped;
        }
        let Some(&first) = data.first() else {
            return Push::Dropped;
        };
        let bytes = frame::encode(KIND_DATA, flow, data);
        if frame::is_media(first) {
            self.push_media(bytes)
        } else {
            self.push_prio(bytes)
        }
    }

    fn push_prio(&self, bytes: Vec<u8>) -> Push {
        let mut g = self.inner.lock().unwrap();
        if g.closed || g.prio_bytes + bytes.len() > self.cfg.prio_cap_bytes {
            drop(g);
            stats::inc(&self.counters.drop_quic_overflow);
            return Push::Dropped;
        }
        g.prio_bytes += bytes.len();
        stats::set(&self.counters.prio_q_bytes, g.prio_bytes as u64);
        stats::max(&self.counters.prio_q_peak, g.prio_bytes as u64);
        g.prio.push_back(Item {
            bytes,
            at: Instant::now(),
        });
        drop(g);
        self.data_cv.notify_one();
        Push::Queued
    }

    fn push_media(&self, bytes: Vec<u8>) -> Push {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return Push::Dropped;
        }
        if g.media_bytes + bytes.len() > self.cfg.media_cap_bytes {
            drop(g);
            stats::inc(&self.counters.drop_media_overflow);
            return Push::Dropped;
        }
        g.media_bytes += bytes.len();
        stats::set(&self.counters.media_q_bytes, g.media_bytes as u64);
        stats::max(&self.counters.media_q_peak, g.media_bytes as u64);
        g.media.push_back(Item {
            bytes,
            at: Instant::now(),
        });
        drop(g);
        self.data_cv.notify_one();
        Push::Queued
    }

    /// Blocks until there is something to write or the queue is closed. Appends up to about
    /// `max_batch` bytes of whole frames to `out` (priority first, see the module comment) and
    /// returns `true`; returns `false` once closed. A batch always holds at least one frame.
    pub fn fill(&self, out: &mut Vec<u8>, max_batch: usize) -> bool {
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.closed {
                return false;
            }
            if !g.prio.is_empty() || !g.media.is_empty() {
                break;
            }
            g = self.data_cv.wait(g).unwrap();
        }
        let start = out.len();
        let now = Instant::now();
        let prio_budget = if g.media.is_empty() {
            max_batch
        } else {
            max_batch / 2
        };
        let mut stale = 0u64;
        let mut frames = 0u64;
        while out.len() - start < prio_budget {
            let Some(it) = g.prio.pop_front() else { break };
            g.prio_bytes -= it.bytes.len();
            out.extend_from_slice(&it.bytes);
            frames += 1;
        }
        // Rest of the batch: media first (the priority share is spent), then whatever priority
        // is left, so a batch is only short when both queues are empty.
        while out.len() - start < max_batch {
            if let Some(it) = g.media.pop_front() {
                g.media_bytes -= it.bytes.len();
                if now.duration_since(it.at) > self.cfg.media_max_age {
                    stale += 1;
                    continue;
                }
                out.extend_from_slice(&it.bytes);
                frames += 1;
            } else if let Some(it) = g.prio.pop_front() {
                g.prio_bytes -= it.bytes.len();
                out.extend_from_slice(&it.bytes);
                frames += 1;
            } else {
                break;
            }
        }
        stats::set(&self.counters.media_q_bytes, g.media_bytes as u64);
        stats::set(&self.counters.prio_q_bytes, g.prio_bytes as u64);
        drop(g);
        stats::add(&self.counters.tx_frames, frames);
        if stale > 0 {
            stats::add(&self.counters.drop_media_stale, stale);
        }
        true
    }

    /// Closes the queue: producers are refused, the writer's `fill` returns `false`, sleepers
    /// wake. Queued frames are discarded.
    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        g.prio.clear();
        g.media.clear();
        g.prio_bytes = 0;
        g.media_bytes = 0;
        drop(g);
        stats::set(&self.counters.media_q_bytes, 0);
        stats::set(&self.counters.prio_q_bytes, 0);
        self.data_cv.notify_all();
        self.closed_cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// Sleeps up to `d`; returns `true` as soon as the queue is closed.
    pub fn sleep_unless_closed(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut g = self.inner.lock().unwrap();
        while !g.closed {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            g = self.closed_cv.wait_timeout(g, left).unwrap().0;
        }
        true
    }

    /// (priority bytes, media bytes) queued right now.
    pub fn depths(&self) -> (usize, usize) {
        let g = self.inner.lock().unwrap();
        (g.prio_bytes, g.media_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameReader, HEADER_LEN, KIND_PING};
    use std::io::Cursor;

    fn q(cfg: QueueConfig) -> (Arc<TxQueue>, Arc<Counters>) {
        let c = Arc::new(Counters::default());
        (TxQueue::new(cfg, c.clone()), c)
    }

    fn media(n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        v[0] = 0x00;
        v
    }
    fn quic(n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        v[0] = 0x4A;
        v
    }

    fn drain(q: &TxQueue) -> Vec<(u8, u16, usize)> {
        let mut out = Vec::new();
        q.fill(&mut out, usize::MAX / 2);
        let mut r = FrameReader::new(Cursor::new(out));
        let mut v = Vec::new();
        while let Some(f) = r.read_frame().unwrap() {
            v.push((f.kind, f.flow, f.payload.len()));
        }
        v
    }

    #[test]
    fn media_tail_drops_at_byte_cap_and_quic_is_never_dropped() {
        let cfg = QueueConfig {
            media_cap_bytes: 10_000,
            ..QueueConfig::default()
        };
        let (q, c) = q(cfg);
        let mut queued = 0;
        let mut dropped = 0;
        for _ in 0..100 {
            match q.push_datagram(1, &media(1454)) {
                Push::Queued => queued += 1,
                Push::Dropped => dropped += 1,
            }
        }
        for _ in 0..50 {
            assert_eq!(q.push_datagram(1, &quic(1200)), Push::Queued);
        }
        let s = c.snapshot();
        assert_eq!(queued, 10_000 / (1454 + HEADER_LEN));
        assert_eq!(s.drop_media_overflow, dropped);
        assert_eq!(s.drop_quic_overflow, 0);
        assert!(s.media_q_bytes <= 10_000);
        assert_eq!(s.prio_q_bytes as usize, 50 * (1200 + HEADER_LEN));
    }

    #[test]
    fn tail_drop_keeps_the_oldest_packets() {
        let cfg = QueueConfig {
            media_cap_bytes: 3 * (100 + HEADER_LEN),
            ..QueueConfig::default()
        };
        let (q, _) = q(cfg);
        for i in 0..6u8 {
            let mut d = media(100);
            d[1] = i;
            q.push_datagram(1, &d);
        }
        let mut out = Vec::new();
        q.fill(&mut out, usize::MAX / 2);
        let mut r = FrameReader::new(Cursor::new(out));
        let mut seen = Vec::new();
        while let Some(f) = r.read_frame().unwrap() {
            seen.push(f.payload[1]);
        }
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn priority_goes_first_even_if_it_arrived_later() {
        let (q, _) = q(QueueConfig::default());
        q.push_datagram(1, &media(100));
        q.push_datagram(1, &media(100));
        q.push_ctrl(KIND_PING, 0, &[0; 8]);
        q.push_datagram(2, &quic(100));
        let kinds: Vec<_> = drain(&q).iter().map(|f| (f.0, f.1)).collect();
        assert_eq!(
            kinds,
            vec![
                (KIND_PING, 0),
                (KIND_DATA, 2),
                (KIND_DATA, 1),
                (KIND_DATA, 1)
            ]
        );
    }

    #[test]
    fn stale_media_is_dropped_at_dequeue_but_quic_is_not() {
        let cfg = QueueConfig {
            media_max_age: Duration::from_millis(20),
            ..QueueConfig::default()
        };
        let (q, c) = q(cfg);
        q.push_datagram(1, &media(100));
        q.push_datagram(1, &quic(100));
        std::thread::sleep(Duration::from_millis(40));
        q.push_datagram(1, &media(100)); // fresh
        let f = drain(&q);
        assert_eq!(
            f.len(),
            2,
            "one stale media dropped, QUIC and fresh media kept: {f:?}"
        );
        assert_eq!(c.snapshot().drop_media_stale, 1);
    }

    #[test]
    fn media_cannot_be_starved_by_a_long_quic_burst() {
        let (q, _) = q(QueueConfig::default());
        for _ in 0..200 {
            q.push_datagram(1, &quic(1200));
        }
        q.push_datagram(2, &media(1000));
        let mut out = Vec::new();
        q.fill(&mut out, 16 * 1024);
        let mut r = FrameReader::new(Cursor::new(out));
        let mut got_media = false;
        while let Some(f) = r.read_frame().unwrap() {
            got_media |= f.flow == 2;
        }
        assert!(got_media, "the media packet must ride in the first batch");
    }

    #[test]
    fn empty_and_oversize_datagrams_are_refused() {
        let (q, c) = q(QueueConfig::default());
        assert_eq!(q.push_datagram(1, &[]), Push::Dropped);
        assert_eq!(
            q.push_datagram(1, &vec![0u8; MAX_PAYLOAD + 1]),
            Push::Dropped
        );
        assert_eq!(c.snapshot().drop_oversize, 1);
    }

    #[test]
    fn close_wakes_the_writer_and_refuses_producers() {
        let (q, _) = q(QueueConfig::default());
        let q2 = q.clone();
        let t = std::thread::spawn(move || {
            let mut out = Vec::new();
            q2.fill(&mut out, 1024)
        });
        std::thread::sleep(Duration::from_millis(30));
        q.close();
        assert!(!t.join().unwrap());
        assert_eq!(q.push_datagram(1, &quic(10)), Push::Dropped);
        assert!(q.sleep_unless_closed(Duration::from_secs(5)));
    }

    #[test]
    fn batch_is_bounded() {
        let (q, _) = q(QueueConfig::default());
        for _ in 0..100 {
            q.push_datagram(1, &quic(1000));
        }
        let mut out = Vec::new();
        q.fill(&mut out, 8 * 1024);
        assert!(out.len() <= 8 * 1024 + 1005, "{}", out.len());
        assert!(out.len() >= 8 * 1024);
    }
}
