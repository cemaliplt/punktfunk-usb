//! Runs the iPad-side relay on Linux, for tests and for trying the tunnel without an iPad.
//!
//! ```text
//! pf-usb-relay [--tcp 127.0.0.1:9779] [--udp 127.0.0.1:9777] [--stats SECONDS]
//!              [--media-queue-kib 512] [--max-age-ms 100]
//! ```
//! Point a Punktfunk client at the `--udp` address and a bridge (`punktfunk-usb-bridge --tcp`)
//! at the `--tcp` address.

use std::net::SocketAddr;
use std::time::Duration;

use pf_usb_tunnel::relay::{self, RelayConfig};

fn usage() -> ! {
    eprintln!(
        "usage: pf-usb-relay [--tcp ADDR:PORT] [--udp ADDR:PORT] [--stats SECONDS] [--media-queue-kib N] [--max-age-ms N]\n\
         defaults: --tcp 127.0.0.1:9779 --udp 127.0.0.1:9777"
    );
    std::process::exit(2)
}

fn main() {
    init_logging();
    let mut cfg = RelayConfig::default();
    let mut stats_every = 0u64;
    let mut a = std::env::args().skip(1);
    while let Some(arg) = a.next() {
        let mut val = || a.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--tcp" => cfg.tcp_bind = val().parse::<SocketAddr>().unwrap_or_else(|_| usage()),
            "--udp" => cfg.udp_bind = val().parse::<SocketAddr>().unwrap_or_else(|_| usage()),
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
    let h = match relay::start_with(cfg) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("pf-usb-relay: {e}");
            std::process::exit(1)
        }
    };
    eprintln!(
        "pf-usb-relay: carrier on {}, client datagrams on {}",
        h.tcp_addr(),
        h.udp_addr()
    );
    loop {
        std::thread::sleep(Duration::from_secs(if stats_every > 0 {
            stats_every
        } else {
            3600
        }));
        if stats_every > 0 {
            eprintln!("{:?}", h.stats());
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
