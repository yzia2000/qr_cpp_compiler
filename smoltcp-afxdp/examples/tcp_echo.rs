//! TCP echo server (port 7) on an AF_XDP-backed smoltcp interface. Also answers ping.
//!
//!     cargo run --release --example tcp_echo -- <ifname> <ip/prefix> [skb|drv] [nocsum]

use std::os::fd::AsRawFd;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
use smoltcp::phy::{Checksum, ChecksumCapabilities, Device};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr};
use smoltcp_afxdp::{Config, XdpDevice, XdpMode};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let ifname = args.get(1).expect("usage: tcp_echo <ifname> <ip/prefix> [skb|drv] [nocsum]");
    let cidr: IpCidr = args.get(2).expect("missing ip/prefix").parse().expect("bad ip/prefix");
    let mut cfg = Config::default();
    if args.iter().any(|a| a == "drv") {
        cfg.xdp_mode = XdpMode::Driver;
    }
    if args.iter().any(|a| a == "nocsum") {
        // Don't verify RX checksums (for peers using TX checksum offload on veth).
        let mut c = ChecksumCapabilities::default();
        c.ipv4 = Checksum::Tx;
        c.tcp = Checksum::Tx;
        c.udp = Checksum::Tx;
        c.icmpv4 = Checksum::Tx;
        cfg.checksum = c;
    }
    cfg.auto_flush = false;

    let mut dev = XdpDevice::new(ifname, cfg)?;
    let mac = EthernetAddress(dev.mac_address());
    let mut iface = Interface::new(IfaceConfig::new(mac.into()), &mut dev, Instant::now());
    iface.update_ip_addrs(|a| a.push(cidr).unwrap());
    eprintln!(
        "listening on {cidr} port 7 via AF_XDP on {ifname} ({mac}), mtu {}",
        dev.capabilities().max_transmission_unit
    );

    let mut sockets = SocketSet::new(vec![]);
    let tcp = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536]));
    let handle = sockets.add(tcp);

    loop {
        let now = Instant::now();
        iface.poll(now, &mut dev, &mut sockets);

        let sock = sockets.get_mut::<tcp::Socket>(handle);
        if !sock.is_open() {
            sock.listen(7).unwrap();
        }
        if sock.may_recv() && sock.can_send() {
            let mut buf = [0u8; 16384];
            let room = (sock.send_capacity() - sock.send_queue()).min(buf.len());
            let n = sock.recv_slice(&mut buf[..room]).unwrap_or(0);
            if n > 0 {
                sock.send_slice(&buf[..n]).unwrap();
            }
        } else if sock.may_send() && !sock.may_recv() {
            sock.close();
        }

        // Push echoed data out now, then flush all queued TX frames with one syscall.
        iface.poll(Instant::now(), &mut dev, &mut sockets);
        dev.flush();

        let timeout = iface.poll_delay(Instant::now(), &sockets).map(|d| d.total_millis() as i32).unwrap_or(-1);
        let mut pfd = libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, timeout) };
    }
}
