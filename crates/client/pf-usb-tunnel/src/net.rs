//! Small socket helpers.

use std::net::UdpSocket;

use socket2::SockRef;

/// Asks for a big kernel receive buffer on a UDP socket and returns what the kernel granted
/// (Linux reports the doubled value, and caps the request at `net.core.rmem_max` unless the
/// process may use SO_RCVBUFFORCE; the host logs the same warning, design 01 section 3.3
/// "Pacing"). The host itself asks for 32 MiB.
pub fn grow_udp_recv_buffer(sock: &UdpSocket, want: usize) -> usize {
    let r = SockRef::from(sock);
    let _ = r.set_recv_buffer_size(want);
    r.recv_buffer_size().unwrap_or(0)
}

pub fn grow_udp_send_buffer(sock: &UdpSocket, want: usize) {
    let _ = SockRef::from(sock).set_send_buffer_size(want);
}
