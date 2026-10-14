//! An AF_XDP backed [`smoltcp::phy::Device`], with no libbpf/libxdp dependency.
//!
//! ```no_run
//! use smoltcp_afxdp::{Config, XdpDevice};
//! let dev = XdpDevice::new("eth0", Config::default())?;
//! # Ok::<(), std::io::Error>(())
//! ```

mod bpf;
mod device;
mod ring;

pub use device::{BindMode, Config, RxToken, TxToken, XdpDevice, XdpMode};
