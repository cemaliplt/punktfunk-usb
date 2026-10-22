//! The PC side of the USB tunnel.
//!
//! ```text
//! punktfunk-usb-bridge --usbmuxd [--udid UDID] [--device-port 9779] [--usbmuxd-socket PATH]
//! punktfunk-usb-bridge --tcp HOST:PORT                 (test mode: a relay reachable over TCP)
//! punktfunk-usb-bridge --list                          (show the devices usbmuxd knows, exit)
//!   common: [--host 127.0.0.1:9777] [--stats SECONDS] [--media-queue-kib 512] [--max-age-ms 100]
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use pf_usb_tunnel::bridge::{self, BridgeConfig, Connector, TcpConnector};
use pf_usb_tunnel::usbmux::{self, UsbmuxdConnector};

fn usage() -> ! {
    eprintln!(
        "usage: punktfunk-usb-bridge (--usbmuxd [--udid X] [--device-port 9779] [--usbmuxd-socket PATH] | --tcp HOST:PORT | --list)\n\
         \x20      [--host 127.0.0.1:9777] [--stats SECONDS] [--media-queue-kib N] [--max-age-ms N]"
    );
    std::process::exit(2)
}

fn main() {
    init_logging();
    let mut cfg = BridgeConfig::default();
    let mut use_usbmuxd = false;
    let mut tcp: Option<SocketAddr> = None;
    let mut udid = None;
    let mut device_port = usbmux::DEFAULT_DEVICE_PORT;
    let mut socket = PathBuf::from(usbmux::DEFAULT_SOCKET);
    let mut list = false;
    let mut stats_every = 0u64;
    let mut a = std::env::args().skip(1);
    while let Some(arg) = a.next() {
        let mut val = || a.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--usbmuxd" => use_usbmuxd = true,
            "--tcp" => tcp = Some(val().parse().unwrap_or_else(|_| usage())),
            "--udid" => udid = Some(val()),
            "--device-port" => device_port = val().parse().unwrap_or_else(|_| usage()),
            "--usbmuxd-socket" => socket = PathBuf::from(val()),
            "--list" => list = true,
            "--host" => cfg.host = val().parse().unwrap_or_else(|_| usage()),
            "--stats" => stats_every = val().parse().unwrap_or_else(|_| usage()),
            "--media-queue-kib" => {
                cfg.queue.media_cap_bytes =
                    val().parse::<usize>().unwrap_or_else(|_| usage()) * 1024
            }
            "--max-age-ms" => {
                cfg.queue.media_max_age =
                    Duration::from_millis(val().parse().unwrap_or_else(|_| usage()))
            }
            _ => usage(),
        }
    }
    if list {
        match usbmux::list_devices(&socket) {
            Ok(d) if d.is_empty() => println!("(usbmuxd lists no devices)"),
            Ok(d) => {
                for x in d {
                    println!("id {}  {}  serial {}", x.id, x.connection_type, x.serial);
                }
            }
            Err(e) => {
                eprintln!("cannot ask usbmuxd at {}: {e}", socket.display());
                std::process::exit(1)
            }
        }
        return;
    }
    let connector: Box<dyn Connector> = match (use_usbmuxd, tcp) {
        (true, None) => Box::new(UsbmuxdConnector {
            socket,
            udid,
            device_port,
        }),
        (false, Some(addr)) => Box::new(TcpConnector(addr)),
        _ => usage(),
    };
    let h = match bridge::start(connector, cfg.clone()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("punktfunk-usb-bridge: {e}");
            std::process::exit(1)
        }
    };
    eprintln!("punktfunk-usb-bridge: forwarding to host {}", cfg.host);
    loop {
        std::thread::sleep(Duration::from_secs(if stats_every > 0 {
            stats_every
        } else {
            3600
        }));
        if stats_every > 0 {
            eprintln!("up={} flows={} {:?}", h.carrier_up(), h.flows(), h.stats());
        }
    }
}

fn init_logging() {
    let level = match std::env::var("PF_USB_LOG").as_deref() {
        Ok("debug") => tracing::Level::DEBUG,
        Ok("trace") => tracing::Level::TRACE,
        Ok("warn") => tracing::Level::WARN,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
}
