//! Upper bound on smoltcp's own TCP cost: two sockets talking over the in-process
//! `phy::Loopback` device (no syscalls, no NIC). Whatever AF_XDP achieves will be below this.
//!
//!     cargo run --release --example smoltcp_ceiling -- [MiB] [mtu]

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let total: usize = args.get(1).map_or(1024, |s| s.parse().unwrap()) << 20;
    let mtu: usize = args.get(2).map_or(1514, |s| s.parse().unwrap());

    let mut dev = Loopback::new(Medium::Ethernet);
    // Loopback has no MTU knob; emulate a real link's MTU through the capabilities.
    let mut dev = MtuLimited { inner: &mut dev, mtu };
    let cfg = Config::new(EthernetAddress([2, 0, 0, 0, 0, 1]).into());
    let mut iface = Interface::new(cfg, &mut dev, Instant::now());
    iface.update_ip_addrs(|a| a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap());

    let buf = || tcp::SocketBuffer::new(vec![0; 1 << 20]);
    let mut sockets = SocketSet::new(vec![]);
    let srv = sockets.add(tcp::Socket::new(buf(), buf()));
    let cli = sockets.add(tcp::Socket::new(buf(), buf()));
    sockets.get_mut::<tcp::Socket>(srv).listen(1234).unwrap();
    sockets.get_mut::<tcp::Socket>(cli).connect(iface.context(), (IpAddress::v4(127, 0, 0, 1), 1234), 65000).unwrap();

    let chunk = vec![0xabu8; 1 << 16];
    let (mut sent, mut recvd) = (0usize, 0usize);
    let start = std::time::Instant::now();
    while recvd < total {
        iface.poll(Instant::now(), &mut dev, &mut sockets);
        let c = sockets.get_mut::<tcp::Socket>(cli);
        if c.can_send() && sent < total {
            sent += c.send_slice(&chunk[..chunk.len().min(total - sent)]).unwrap();
        }
        let s = sockets.get_mut::<tcp::Socket>(srv);
        while s.can_recv() {
            recvd += s.recv(|b| (b.len(), b.len())).unwrap();
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let segs = total as f64 / (mtu - 54) as f64;
    println!(
        "mtu {mtu}: {} MiB in {secs:.2}s = {:.2} Gbit/s, ~{:.2} Mpps data segments (one core, both ends)",
        total >> 20,
        total as f64 * 8.0 / secs / 1e9,
        segs / secs / 1e6
    );
}

struct MtuLimited<'a> {
    inner: &'a mut Loopback,
    mtu: usize,
}

impl smoltcp::phy::Device for MtuLimited<'_> {
    type RxToken<'a>
        = <Loopback as smoltcp::phy::Device>::RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = <Loopback as smoltcp::phy::Device>::TxToken<'a>
    where
        Self: 'a;
    fn receive(&mut self, t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.inner.receive(t)
    }
    fn transmit(&mut self, t: Instant) -> Option<Self::TxToken<'_>> {
        self.inner.transmit(t)
    }
    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut c = self.inner.capabilities();
        c.max_transmission_unit = self.mtu;
        c
    }
}
