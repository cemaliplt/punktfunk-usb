//! usbmuxd client: finds the iPad and opens a TCP connection to a port the app listens on.
//!
//! Protocol (same as `~/Work/ipad-ekran/usbmux.py`): UNIX socket `/var/run/usbmuxd`; every
//! message is `len:u32 | version:u32 (=1) | request:u32 (=8, plist) | tag:u32` little endian,
//! then an XML plist. `ListDevices` answers with `DeviceList`; `Connect {DeviceID, PortNumber}`
//! answers `Result {Number}` (0 = ok) and the socket then IS the TCP stream to the device.
//! `PortNumber` is the port in NETWORK byte order stored in a host integer (`htons`).
//! `Connect` consumes the connection it is sent on, so listing and connecting use two sockets.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bridge::Connector;
use crate::link::Link;
use crate::plist::{self, Value};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

pub const DEFAULT_SOCKET: &str = "/var/run/usbmuxd";
pub const DEFAULT_DEVICE_PORT: u16 = 9779;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    pub serial: String,
    pub connection_type: String,
}

/// `htons` as usbmuxd wants it: the two bytes of `port` swapped.
pub fn port_field(port: u16) -> i64 {
    i64::from(port.swap_bytes())
}

fn frame(tag: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + body.len());
    v.extend_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(body);
    v
}

fn base(msg: &str) -> Vec<(&'static str, Value)> {
    vec![
        (
            "ClientVersionString",
            Value::Str("punktfunk-usb-bridge".into()),
        ),
        ("ProgName", Value::Str("punktfunk-usb-bridge".into())),
        ("MessageType", Value::Str(msg.into())),
    ]
}

fn read_reply<R: Read>(r: &mut R) -> io::Result<Value> {
    let mut h = [0u8; 16];
    r.read_exact(&mut h)?;
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    if !(16..=4 * 1024 * 1024).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("usbmuxd reply length {len}"),
        ));
    }
    let mut body = vec![0u8; len - 16];
    r.read_exact(&mut body)?;
    plist::parse(&body)
}

fn request<S: Read + Write>(s: &mut S, items: Vec<(&str, Value)>) -> io::Result<Value> {
    let body = plist::write_dict(&items);
    s.write_all(&frame(1, &body))?;
    read_reply(s)
}

/// Devices usbmuxd currently lists (USB and Wi-Fi sync entries both; filter on `connection_type`).
#[cfg(unix)]
pub fn list_devices(socket: &Path) -> io::Result<Vec<Device>> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let r = request(&mut s, base("ListDevices"))?;
    let list = r
        .get("DeviceList")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "usbmuxd: no DeviceList"))?;
    Ok(list
        .iter()
        .filter_map(|d| {
            let p = d.get("Properties")?;
            Some(Device {
                id: d.get("DeviceID").or_else(|| p.get("DeviceID"))?.as_int()?,
                serial: p
                    .get("SerialNumber")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                connection_type: p
                    .get("ConnectionType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// Opens a TCP connection to `port` on device `device_id` and returns the raw stream.
#[cfg(unix)]
pub fn connect_device(socket: &Path, device_id: i64, port: u16) -> io::Result<UnixStream> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut msg = base("Connect");
    msg.push(("DeviceID", Value::Int(device_id)));
    msg.push(("PortNumber", Value::Int(port_field(port))));
    let r = request(&mut s, msg)?;
    match r.get("Number").and_then(Value::as_int) {
        Some(0) => {
            s.set_read_timeout(None)?;
            s.set_write_timeout(None)?;
            Ok(s)
        }
        Some(n) => Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "usbmuxd Connect result {n} (3 = nothing listens on port {port} on the device)"
            ),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "usbmuxd: Connect reply without Number",
        )),
    }
}

/// The bridge's connection source for the real cable.
#[derive(Clone, Debug)]
pub struct UsbmuxdConnector {
    pub socket: PathBuf,
    /// Serial / UDID to pick; `None` takes the first USB device.
    pub udid: Option<String>,
    pub device_port: u16,
}

impl UsbmuxdConnector {
    pub fn new(udid: Option<String>, device_port: u16) -> Self {
        UsbmuxdConnector {
            socket: DEFAULT_SOCKET.into(),
            udid,
            device_port,
        }
    }
}

#[cfg(unix)]
impl Connector for UsbmuxdConnector {
    fn connect(&self) -> io::Result<Link> {
        let devices = list_devices(&self.socket)?;
        let dev = devices
            .iter()
            .filter(|d| d.connection_type == "USB")
            .find(|d| self.udid.as_deref().is_none_or(|u| d.serial == u))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching iPad on USB"))?;
        Ok(Link::Unix(connect_device(
            &self.socket,
            dev.id,
            self.device_port,
        )?))
    }

    fn describe(&self) -> String {
        format!(
            "usbmuxd {} device {} port {}",
            self.socket.display(),
            self.udid.as_deref().unwrap_or("(first USB)"),
            self.device_port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_is_byte_swapped_like_htons() {
        assert_eq!(port_field(9779), 0x3326); // 9779 = 0x2633
        assert_eq!(port_field(0x1234), 0x3412);
        assert_eq!(port_field(1), 256);
    }

    #[test]
    fn frame_header_layout() {
        let f = frame(7, b"abc");
        assert_eq!(&f[0..4], &19u32.to_le_bytes());
        assert_eq!(&f[4..8], &1u32.to_le_bytes());
        assert_eq!(&f[8..12], &8u32.to_le_bytes());
        assert_eq!(&f[12..16], &7u32.to_le_bytes());
        assert_eq!(&f[16..], b"abc");
    }
}
