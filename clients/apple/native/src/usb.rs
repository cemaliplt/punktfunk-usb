//! USB tunnel relay C ABI for the Apple client.
//!
//! Exposes the in-process UDP-over-TCP tunnel relay (`pf-usb-tunnel`) to Swift.
//! When running on iOS, usbmuxd forwards host PC connections to the device's
//! loopback TCP port. The relay accepts the carrier and forwards datagrams to
//! and from a local UDP port that the Punktfunk client treats as the host.

use std::net::{Ipv4Addr, SocketAddr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use pf_usb_tunnel::relay::{self, RelayHandle};

/// Safe unwinding wrapper: unwinding across an extern "C" boundary is undefined.
fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

/// Point-in-time snapshot of the USB tunnel relay counters.
///
/// Every field is an unsigned 64-bit integer, matching `pf_usb_tunnel::Stats`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PfUsbStats {
    pub carrier_up: u64,
    pub carriers_total: u64,
    pub carrier_errors: u64,
    pub flows_opened: u64,
    pub flows_closed: u64,
    pub flows_active: u64,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub udp_in: u64,
    pub udp_out: u64,
    pub drop_media_overflow: u64,
    pub drop_media_stale: u64,
    pub drop_quic_overflow: u64,
    pub drop_no_carrier: u64,
    pub drop_unknown_flow: u64,
    pub drop_no_flow: u64,
    pub drop_oversize: u64,
    pub udp_send_errors: u64,
    pub media_q_bytes: u64,
    pub media_q_peak: u64,
    pub prio_q_bytes: u64,
    pub prio_q_peak: u64,
    pub rtt_us: u64,
    pub udp_rcvbuf: u64,
}

impl From<pf_usb_tunnel::stats::Stats> for PfUsbStats {
    fn from(s: pf_usb_tunnel::stats::Stats) -> Self {
        Self {
            carrier_up: s.carrier_up,
            carriers_total: s.carriers_total,
            carrier_errors: s.carrier_errors,
            flows_opened: s.flows_opened,
            flows_closed: s.flows_closed,
            flows_active: s.flows_active,
            tx_frames: s.tx_frames,
            tx_bytes: s.tx_bytes,
            rx_frames: s.rx_frames,
            rx_bytes: s.rx_bytes,
            udp_in: s.udp_in,
            udp_out: s.udp_out,
            drop_media_overflow: s.drop_media_overflow,
            drop_media_stale: s.drop_media_stale,
            drop_quic_overflow: s.drop_quic_overflow,
            drop_no_carrier: s.drop_no_carrier,
            drop_unknown_flow: s.drop_unknown_flow,
            drop_no_flow: s.drop_no_flow,
            drop_oversize: s.drop_oversize,
            udp_send_errors: s.udp_send_errors,
            media_q_bytes: s.media_q_bytes,
            media_q_peak: s.media_q_peak,
            prio_q_bytes: s.prio_q_bytes,
            prio_q_peak: s.prio_q_peak,
            rtt_us: s.rtt_us,
            udp_rcvbuf: s.udp_rcvbuf,
        }
    }
}

/// An opaque handle to a running USB relay instance.
pub struct PfUsbRelay {
    inner: Mutex<Option<RelayHandle>>,
}

/// Start a USB tunnel relay listening on `tcp_port` and binding `udp_port`.
///
/// On success, stores an allocated handle into `*out` and returns 0.
/// Returns -1 on null pointer arguments, -2 on bind/socket error, -3 on panic.
///
/// # Safety
/// `out` must be a valid, writable pointer to a pointer slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_usb_start(
    tcp_port: u16,
    udp_port: u16,
    out: *mut *mut PfUsbRelay,
) -> i32 {
    guard(-3, || {
        if out.is_null() {
            return -1;
        }
        let udp_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, udp_port));
        match relay::start(tcp_port, udp_addr) {
            Ok(handle) => {
                let relay = Box::new(PfUsbRelay {
                    inner: Mutex::new(Some(handle)),
                });
                unsafe {
                    *out = Box::into_raw(relay);
                }
                0
            }
            Err(e) => {
                tracing::error!("usb: start failed on tcp {tcp_port}, udp {udp_port}: {e}");
                -2
            }
        }
    })
}

/// Read counter statistics from the relay into `out`.
///
/// Returns 0 on success, -1 on null pointers, -2 if stopped, -3 on panic.
///
/// # Safety
/// `r` must be a valid pointer from `punktfunk_usb_start`. `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_usb_stats(
    r: *const PfUsbRelay,
    out: *mut PfUsbStats,
) -> i32 {
    guard(-3, || {
        if r.is_null() || out.is_null() {
            return -1;
        }
        let relay = unsafe { &*r };
        let guard = match relay.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        match guard.as_ref() {
            Some(handle) => {
                let s = handle.stats();
                unsafe {
                    *out = PfUsbStats::from(s);
                }
                0
            }
            None => -2,
        }
    })
}

/// Stop the USB relay and free its resources.
///
/// Idempotent. Safe to call with NULL. After this call, `r` is invalid.
///
/// # Safety
/// `r` is NULL or was returned by `punktfunk_usb_start` and not freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_usb_stop(r: *mut PfUsbRelay) {
    guard((), || {
        if r.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(r) };
        if let Ok(mut guard) = boxed.inner.lock() {
            if let Some(mut handle) = guard.take() {
                handle.stop();
            }
        }
    });
}
