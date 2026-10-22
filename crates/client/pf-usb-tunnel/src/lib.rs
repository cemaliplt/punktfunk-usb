//! UDP over the USB cable: the shared tunnel code for the iPad-side relay and the PC-side
//! bridge. See `punktfunk-usb-notlar/01-tasarim.md` (design (b)) and `03-tunel.md`.
//!
//! ```text
//!  iPad                                                        PC
//!  Punktfunk client --UDP--> relay --TCP (usbmuxd cable)--> bridge --UDP--> host :9777
//!  (host = 127.0.0.1:9777)   [relay::start]              [bridge::start]
//! ```

pub mod bridge;
pub mod carrier;
pub mod frame;
pub mod link;
pub mod net;
pub mod plist;
pub mod queue;
pub mod relay;
pub mod stats;
pub mod usbmux;

pub use bridge::{BridgeConfig, BridgeHandle, Connector, TcpConnector};
pub use relay::{RelayConfig, RelayHandle};
pub use stats::Stats;
