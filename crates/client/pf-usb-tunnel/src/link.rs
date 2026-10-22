//! The carrier byte stream: a TCP socket (tests, `--tcp`, the iPad's accepted connection) or a
//! UNIX socket (what usbmuxd hands back after `Connect`).

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use socket2::SockRef;

pub enum Link {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Link {
    pub fn try_clone(&self) -> io::Result<Link> {
        Ok(match self {
            Link::Tcp(s) => Link::Tcp(s.try_clone()?),
            #[cfg(unix)]
            Link::Unix(s) => Link::Unix(s.try_clone()?),
        })
    }

    /// Wakes any thread blocked in `read`/`write` on this socket (or a clone) with an error/EOF.
    pub fn shutdown(&self) {
        let _ = match self {
            Link::Tcp(s) => s.shutdown(Shutdown::Both),
            #[cfg(unix)]
            Link::Unix(s) => s.shutdown(Shutdown::Both),
        };
    }

    /// TCP_NODELAY (TCP only), a small kernel send buffer (so queueing happens in OUR queue,
    /// where we can drop, not in the kernel where we cannot) and a write timeout (a stalled
    /// peer must not block the writer past the liveness deadline).
    pub fn configure(&self, send_buf: usize, write_timeout: Duration) {
        match self {
            Link::Tcp(s) => {
                let _ = s.set_nodelay(true);
                let _ = SockRef::from(s).set_send_buffer_size(send_buf);
                let _ = s.set_write_timeout(Some(write_timeout));
            }
            #[cfg(unix)]
            Link::Unix(s) => {
                let _ = SockRef::from(s).set_send_buffer_size(send_buf);
                let _ = s.set_write_timeout(Some(write_timeout));
            }
        }
    }

    /// Kernel send buffer in effect, bytes (for logs and tests).
    pub fn send_buffer(&self) -> usize {
        match self {
            Link::Tcp(s) => SockRef::from(s).send_buffer_size().unwrap_or(0),
            #[cfg(unix)]
            Link::Unix(s) => SockRef::from(s).send_buffer_size().unwrap_or(0),
        }
    }
}

impl Read for Link {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Tcp(s) => s.read(b),
            #[cfg(unix)]
            Link::Unix(s) => s.read(b),
        }
    }
}

impl Write for Link {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Link::Tcp(s) => s.write(b),
            #[cfg(unix)]
            Link::Unix(s) => s.write(b),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Link::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Link::Unix(s) => s.flush(),
        }
    }
}
