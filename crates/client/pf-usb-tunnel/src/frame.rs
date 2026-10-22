//! Wire format of the carrier (the one TCP / usbmuxd byte stream between relay and bridge).
//!
//! ```text
//! kind:u8 | flow:u16 BE | len:u16 BE | payload[len]
//! ```
//!
//! Five bytes of header. `len` is 16 bit so a whole UDP datagram (max 65507 bytes, jumbo 8972
//! included) fits in one frame: nothing in the tunnel ever shortens or splits a datagram. The
//! host learns a smaller wire MTU for an IP that lets a short datagram through (design
//! 01-tasarim.md section 3.3 "MTU"), so that property is load-bearing.
//!
//! `flow` 0 is the control plane (HELLO, PING, PONG). Flow ids 1.. are UDP flows; the relay
//! allocates them (the iPad opens flows, the PC cannot), the bridge only echoes them back.
//!
//! | kind  | value | direction        | payload                                              |
//! |-------|-------|------------------|------------------------------------------------------|
//! | HELLO | 1     | both, first      | `version:u8 | role:u8 | generation:u32 BE`            |
//! | OPEN  | 2     | relay -> bridge  | empty (idempotent: re-sent for every live flow after a reconnect) |
//! | DATA  | 3     | both             | one UDP datagram, verbatim                            |
//! | CLOSE | 4     | both             | empty                                                 |
//! | PING  | 5     | both             | 8 opaque bytes (sender's microsecond clock)           |
//! | PONG  | 6     | both             | the PING payload, echoed                              |
//!
//! Unknown kinds are skipped (their payload is consumed), so a newer peer may add frames; a
//! different HELLO `version` is refused.

use std::io::{self, Read};

/// Protocol version carried in HELLO.
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 5;
pub const MAX_PAYLOAD: usize = u16::MAX as usize;

pub const KIND_HELLO: u8 = 1;
pub const KIND_OPEN: u8 = 2;
pub const KIND_DATA: u8 = 3;
pub const KIND_CLOSE: u8 = 4;
pub const KIND_PING: u8 = 5;
pub const KIND_PONG: u8 = 6;

/// Who is speaking, in HELLO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The iPad side: listens, owns the flow ids.
    Relay = 0,
    /// The PC side: dials out (usbmuxd), owns the UDP sockets towards the host.
    Bridge = 1,
}

impl Role {
    pub fn from_u8(v: u8) -> Option<Role> {
        match v {
            0 => Some(Role::Relay),
            1 => Some(Role::Bridge),
            _ => None,
        }
    }
}

/// A decoded HELLO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u8,
    pub role: u8,
    /// The relay's flow-table generation. It changes whenever the relay forgot its flows, so a
    /// bridge that still holds sockets for older ids knows to drop them. Bridge sends 0.
    pub generation: u32,
}

impl Hello {
    pub const LEN: usize = 6;

    pub fn encode(self) -> [u8; Self::LEN] {
        let g = self.generation.to_be_bytes();
        [self.version, self.role, g[0], g[1], g[2], g[3]]
    }

    pub fn decode(p: &[u8]) -> Option<Hello> {
        if p.len() < Self::LEN {
            return None;
        }
        Some(Hello {
            version: p[0],
            role: p[1],
            generation: u32::from_be_bytes([p[2], p[3], p[4], p[5]]),
        })
    }
}

/// Appends one frame (header and payload) to `out`. `payload` must fit `MAX_PAYLOAD`.
pub fn put_frame(out: &mut Vec<u8>, kind: u8, flow: u16, payload: &[u8]) {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    out.push(kind);
    out.extend_from_slice(&flow.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
}

/// One frame as a fresh `Vec`.
pub fn encode(kind: u8, flow: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    put_frame(&mut v, kind, flow, payload);
    v
}

/// A frame borrowed from the reader's buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u8,
    pub flow: u16,
    pub payload: &'a [u8],
}

/// Pulls frames out of a byte stream. Wrap the stream in a `BufReader` first: the header is
/// read with its own small `read_exact`.
pub struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        FrameReader {
            inner,
            buf: vec![0; MAX_PAYLOAD],
        }
    }

    /// The next frame, or `Ok(None)` on a clean end of stream at a frame boundary. An end of
    /// stream in the middle of a frame is an `UnexpectedEof` error.
    pub fn read_frame(&mut self) -> io::Result<Option<Frame<'_>>> {
        let mut h = [0u8; HEADER_LEN];
        let mut got = 0;
        while got < HEADER_LEN {
            match self.inner.read(&mut h[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let kind = h[0];
        let flow = u16::from_be_bytes([h[1], h[2]]);
        let len = u16::from_be_bytes([h[3], h[4]]) as usize;
        self.inner.read_exact(&mut self.buf[..len])?;
        Ok(Some(Frame {
            kind,
            flow,
            payload: &self.buf[..len],
        }))
    }
}

/// Whether a datagram is Punktfunk media rather than QUIC. Same rule as the core's
/// `transport::shared::is_media` (pinned to it by `tests/classify.rs`, which reads the core's source): QUIC packets carry the
/// fixed bit (0x40) or the long-header bit (0x80); raw media packets have both clear.
pub fn is_media(first_byte: u8) -> bool {
    first_byte & 0xC0 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_all_kinds() {
        let mut wire = Vec::new();
        put_frame(
            &mut wire,
            KIND_HELLO,
            0,
            &Hello {
                version: 1,
                role: 0,
                generation: 0xA1B2C3D4,
            }
            .encode(),
        );
        put_frame(&mut wire, KIND_OPEN, 7, &[]);
        put_frame(&mut wire, KIND_DATA, 7, b"hello datagram");
        put_frame(&mut wire, KIND_CLOSE, 7, &[]);
        put_frame(&mut wire, KIND_PING, 0, &42u64.to_be_bytes());
        put_frame(&mut wire, KIND_PONG, 0, &42u64.to_be_bytes());
        let mut r = FrameReader::new(Cursor::new(wire));
        let f = r.read_frame().unwrap().unwrap();
        assert_eq!((f.kind, f.flow), (KIND_HELLO, 0));
        let h = Hello::decode(f.payload).unwrap();
        assert_eq!(
            h,
            Hello {
                version: 1,
                role: 0,
                generation: 0xA1B2C3D4
            }
        );
        let f = r.read_frame().unwrap().unwrap();
        assert_eq!((f.kind, f.flow, f.payload.len()), (KIND_OPEN, 7, 0));
        let f = r.read_frame().unwrap().unwrap();
        assert_eq!(
            (f.kind, f.flow, f.payload),
            (KIND_DATA, 7, &b"hello datagram"[..])
        );
        assert_eq!(r.read_frame().unwrap().unwrap().kind, KIND_CLOSE);
        assert_eq!(
            r.read_frame().unwrap().unwrap().payload,
            42u64.to_be_bytes()
        );
        assert_eq!(r.read_frame().unwrap().unwrap().kind, KIND_PONG);
        assert!(r.read_frame().unwrap().is_none());
    }

    #[test]
    fn header_is_big_endian_five_bytes() {
        let f = encode(KIND_DATA, 0x0102, &[9, 9, 9]);
        assert_eq!(f, vec![KIND_DATA, 0x01, 0x02, 0x00, 0x03, 9, 9, 9]);
    }

    #[test]
    fn max_payload_fits_without_truncation() {
        let big = vec![0xAB; MAX_PAYLOAD];
        let wire = encode(KIND_DATA, 65535, &big);
        assert_eq!(wire.len(), HEADER_LEN + MAX_PAYLOAD);
        let mut r = FrameReader::new(Cursor::new(wire));
        let f = r.read_frame().unwrap().unwrap();
        assert_eq!((f.flow, f.payload.len()), (65535, MAX_PAYLOAD));
        assert_eq!(f.payload, &big[..]);
    }

    #[test]
    fn jumbo_and_standard_datagrams_survive_untouched() {
        for n in [1usize, 1454, 1472, 8972, 65507] {
            let d: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let mut r = FrameReader::new(Cursor::new(encode(KIND_DATA, 3, &d)));
            assert_eq!(r.read_frame().unwrap().unwrap().payload, &d[..], "len {n}");
        }
    }

    #[test]
    fn truncated_frame_is_unexpected_eof_not_clean_end() {
        let wire = encode(KIND_DATA, 1, b"abcdef");
        // cut inside the payload
        let mut r = FrameReader::new(Cursor::new(wire[..wire.len() - 2].to_vec()));
        assert_eq!(
            r.read_frame().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        // cut inside the header
        let mut r = FrameReader::new(Cursor::new(wire[..3].to_vec()));
        assert_eq!(
            r.read_frame().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        // empty stream is a clean end
        let mut r = FrameReader::new(Cursor::new(Vec::new()));
        assert!(r.read_frame().unwrap().is_none());
    }

    #[test]
    fn reader_survives_one_byte_at_a_time_delivery() {
        struct Dribble(Vec<u8>, usize);
        impl Read for Dribble {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.1 >= self.0.len() || b.is_empty() {
                    return Ok(0);
                }
                b[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut wire = encode(KIND_DATA, 5, b"xyz");
        wire.extend(encode(KIND_CLOSE, 5, &[]));
        let mut r = FrameReader::new(Dribble(wire, 0));
        assert_eq!(r.read_frame().unwrap().unwrap().payload, b"xyz");
        assert_eq!(r.read_frame().unwrap().unwrap().kind, KIND_CLOSE);
        assert!(r.read_frame().unwrap().is_none());
    }

    #[test]
    fn unknown_kind_is_delivered_with_its_payload_so_callers_can_skip_it() {
        let mut wire = encode(0xEE, 9, b"future");
        wire.extend(encode(KIND_DATA, 1, b"ok"));
        let mut r = FrameReader::new(Cursor::new(wire));
        let f = r.read_frame().unwrap().unwrap();
        assert_eq!((f.kind, f.payload), (0xEE, &b"future"[..]));
        assert_eq!(r.read_frame().unwrap().unwrap().payload, b"ok");
    }

    #[test]
    fn hello_rejects_short_payload() {
        assert!(Hello::decode(&[1, 0, 0]).is_none());
        assert_eq!(Role::from_u8(1), Some(Role::Bridge));
        assert_eq!(Role::from_u8(9), None);
    }

    #[test]
    fn classify_matches_quic_header_bits() {
        assert!(is_media(0x00));
        assert!(is_media(0x3F));
        assert!(!is_media(0x40)); // QUIC short header (fixed bit)
        assert!(!is_media(0xC3)); // QUIC long header (Initial)
        assert!(!is_media(0x80));
    }
}
