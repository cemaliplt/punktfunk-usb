//! Counters. Everything the relay and the bridge count lives in one flat block of atomics so a
//! C ABI (or a `--stats` line) can read it without locks, and a [`Stats`] snapshot is a plain
//! `Copy` struct of `u64`s that maps one-to-one onto a C struct.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($( $(#[$m:meta])* $name:ident ),* $(,)?) => {
        /// Live counters (shared, atomic).
        #[derive(Default)]
        pub struct Counters { $( $(#[$m])* pub $name: AtomicU64, )* }

        /// A point-in-time copy of [`Counters`].
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct Stats { $( $(#[$m])* pub $name: u64, )* }

        impl Counters {
            pub fn snapshot(&self) -> Stats {
                Stats { $( $name: self.$name.load(Ordering::Relaxed), )* }
            }
        }
    };
}

counters! {
    /// 1 while a carrier is up and past the HELLO exchange.
    carrier_up,
    /// Carriers that completed HELLO (relay: accepted; bridge: connected).
    carriers_total,
    /// Failed connection attempts (bridge) / carriers that died on the wire (both).
    carrier_errors,
    /// Flows opened since start.
    flows_opened,
    flows_closed,
    /// Flows alive now (gauge).
    flows_active,
    /// Frames and bytes written to / read from the carrier (headers included).
    tx_frames,
    tx_bytes,
    rx_frames,
    rx_bytes,
    /// Datagrams read from the UDP side (relay: from the client; bridge: from the host).
    udp_in,
    /// Datagrams written to the UDP side.
    udp_out,
    /// Media datagrams dropped because the media queue was at its byte cap (tail drop).
    drop_media_overflow,
    /// Media datagrams dropped because they sat in the queue longer than `media_max_age`.
    drop_media_stale,
    /// QUIC / control frames dropped because even the priority queue hit its (large) cap.
    /// Should stay 0.
    drop_quic_overflow,
    /// Datagrams dropped because no carrier was up (or not yet past HELLO).
    drop_no_carrier,
    /// DATA frames for a flow this side does not have.
    drop_unknown_flow,
    /// Datagrams dropped because the flow table was full.
    drop_no_flow,
    /// Datagrams that did not fit a frame (never happens for real UDP, kept as a tripwire).
    drop_oversize,
    /// Failed `send`/`send_to` on a UDP socket.
    udp_send_errors,
    /// Media / priority queue depth now (bytes, gauge) and the highest it reached.
    media_q_bytes,
    media_q_peak,
    prio_q_bytes,
    prio_q_peak,
    /// Last PING round trip over the carrier, microseconds (0 = none yet).
    rtt_us,
    /// Kernel receive buffer actually granted to the UDP socket(s), bytes.
    udp_rcvbuf,
}

#[inline]
pub(crate) fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub(crate) fn set(c: &AtomicU64, v: u64) {
    c.store(v, Ordering::Relaxed);
}

#[inline]
pub(crate) fn max(c: &AtomicU64, v: u64) {
    c.fetch_max(v, Ordering::Relaxed);
}
