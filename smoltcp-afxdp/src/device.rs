//! AF_XDP socket + UMEM, exposed as a zero-copy `smoltcp::phy::Device`.
//!
//! Frame ownership:
//! * RX: kernel -> RX ring -> `RxToken` (smoltcp reads the frame in place) -> fill ring -> kernel
//! * TX: free list -> `TxToken` (smoltcp writes the frame in place) -> TX ring -> kernel
//!   -> completion ring -> free list

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use smoltcp::phy::{self, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::bpf::{XDP_FLAGS_DRV_MODE, XDP_FLAGS_SKB_MODE, XdpProgram};
use crate::ring::Ring;

/// Bytes the kernel reserves in front of every received frame (`XDP_PACKET_HEADROOM`).
const XDP_PACKET_HEADROOM: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XdpMode {
    /// Generic XDP (`XDP_FLAGS_SKB_MODE`): works on any netdev, always copies.
    Skb,
    /// Native driver XDP (`XDP_FLAGS_DRV_MODE`): needs driver support.
    Driver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindMode {
    /// Let the kernel pick (zero-copy if the driver supports it, else copy).
    Auto,
    Copy,
    ZeroCopy,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub queue_id: u32,
    /// UMEM chunk size; must be a power of two (2048 or 4096).
    pub frame_size: u32,
    /// Total UMEM frames, split between RX (fill ring) and TX (free list).
    pub frame_count: u32,
    /// Size of each of the four rings; must be a power of two.
    pub ring_size: u32,
    pub xdp_mode: XdpMode,
    pub bind_mode: BindMode,
    /// Max Ethernet frame length (incl. 14 byte header) reported to smoltcp.
    pub mtu: usize,
    /// Kick the kernel after every transmitted frame. If false, call `XdpDevice::flush`
    /// after `Interface::poll` to batch TX wakeups into one syscall.
    pub auto_flush: bool,
    /// Checksum handling reported to smoltcp (e.g. disable RX verification for
    /// peers that leave `CHECKSUM_PARTIAL` packets unfinished).
    pub checksum: ChecksumCapabilities,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            queue_id: 0,
            frame_size: 4096,
            frame_count: 4096,
            ring_size: 2048,
            xdp_mode: XdpMode::Skb,
            bind_mode: BindMode::Auto,
            mtu: 1514,
            auto_flush: true,
            checksum: ChecksumCapabilities::default(),
        }
    }
}

/// Raw view of the UMEM area; shared by both token kinds, which always touch disjoint frames.
#[derive(Clone, Copy)]
struct Umem {
    base: *mut u8,
    len: usize,
}

impl Umem {
    /// # Safety
    /// `addr..addr+len` must lie in the UMEM and the frame must be owned by the caller.
    unsafe fn frame<'a>(self, addr: u64, len: usize) -> &'a mut [u8] {
        assert!(addr as usize + len <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.base.add(addr as usize), len) }
    }
}

struct RxSide {
    fd: RawFd,
    rx: Ring<libc::xdp_desc>,
    fill: Ring<u64>,
    frame_mask: u64,
}

struct TxSide {
    fd: RawFd,
    tx: Ring<libc::xdp_desc>,
    comp: Ring<u64>,
    free: Vec<u64>,
    auto_flush: bool,
    scratch: Vec<u8>,
    tx_dropped: u64,
}

/// An AF_XDP socket bound to one interface queue, implementing `smoltcp::phy::Device`.
pub struct XdpDevice {
    // Field order is drop order: unmap rings, close socket, free UMEM, detach XDP.
    rxs: RxSide,
    txs: TxSide,
    sock: OwnedFd,
    umem: Umem,
    _umem_map: UmemGuard,
    prog: Option<XdpProgram>,
    caps: DeviceCapabilities,
    mac: [u8; 6],
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn setsockopt<T>(fd: RawFd, opt: libc::c_int, val: &T) -> io::Result<()> {
    let len = std::mem::size_of::<T>() as libc::socklen_t;
    cvt(unsafe { libc::setsockopt(fd, libc::SOL_XDP, opt, val as *const T as *const _, len) }).map(drop)
}

fn read_mac(ifname: &str) -> io::Result<[u8; 6]> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{ifname}/address"))?;
    let mut mac = [0u8; 6];
    for (b, part) in mac.iter_mut().zip(s.trim().split(':')) {
        *b = u8::from_str_radix(part, 16).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }
    Ok(mac)
}

impl XdpDevice {
    /// Create the UMEM + socket, attach the redirect program to `ifname` and bind to `cfg.queue_id`.
    pub fn new(ifname: &str, cfg: Config) -> io::Result<Self> {
        let inval = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
        if !cfg.frame_size.is_power_of_two() || !cfg.ring_size.is_power_of_two() {
            return Err(inval("frame_size and ring_size must be powers of two"));
        }
        if cfg.mtu as u32 > cfg.frame_size - XDP_PACKET_HEADROOM {
            return Err(inval("mtu does not fit in a frame"));
        }
        let fill_frames = cfg.ring_size.min(cfg.frame_count / 2);

        let c_ifname = CString::new(ifname).map_err(|_| inval("bad interface name"))?;
        let ifindex = unsafe { libc::if_nametoindex(c_ifname.as_ptr()) };
        if ifindex == 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = cvt(unsafe { libc::socket(libc::AF_XDP, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) })?;
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };

        // UMEM: one anonymous mapping, registered with the socket.
        let len = cfg.frame_size as usize * cfg.frame_count as usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let umem = Umem { base: base.cast(), len };
        // Owns the mapping: unmapped on early return, or after the socket closes.
        let guard = UmemGuard(umem);

        let reg = libc::xdp_umem_reg_v1 { addr: base as u64, len: len as u64, chunk_size: cfg.frame_size, headroom: 0 };
        setsockopt(fd, libc::XDP_UMEM_REG, &reg)?;
        for opt in [libc::XDP_UMEM_FILL_RING, libc::XDP_UMEM_COMPLETION_RING, libc::XDP_RX_RING, libc::XDP_TX_RING] {
            setsockopt(fd, opt, &cfg.ring_size)?;
        }

        let mut off: libc::xdp_mmap_offsets = unsafe { std::mem::zeroed() };
        let mut optlen = std::mem::size_of_val(&off) as libc::socklen_t;
        cvt(unsafe {
            libc::getsockopt(fd, libc::SOL_XDP, libc::XDP_MMAP_OFFSETS, &mut off as *mut _ as *mut _, &mut optlen)
        })?;
        let rx = Ring::map(fd, &off.rx, cfg.ring_size, libc::XDP_PGOFF_RX_RING as u64)?;
        let tx = Ring::map(fd, &off.tx, cfg.ring_size, libc::XDP_PGOFF_TX_RING as u64)?;
        let mut fill = Ring::map(fd, &off.fr, cfg.ring_size, libc::XDP_UMEM_PGOFF_FILL_RING)?;
        let comp = Ring::map(fd, &off.cr, cfg.ring_size, libc::XDP_UMEM_PGOFF_COMPLETION_RING)?;

        // Frames [0, fill_frames) go to the kernel for RX; the rest are TX buffers.
        for i in 0..fill_frames {
            fill.push(i as u64 * cfg.frame_size as u64);
        }
        fill.submit();
        let free = (fill_frames..cfg.frame_count).map(|i| i as u64 * cfg.frame_size as u64).collect();

        let prog_flags = match cfg.xdp_mode {
            XdpMode::Skb => XDP_FLAGS_SKB_MODE,
            XdpMode::Driver => XDP_FLAGS_DRV_MODE,
        };
        let prog = XdpProgram::attach(ifindex, cfg.queue_id + 1, prog_flags)?;

        let bind_flags = libc::XDP_USE_NEED_WAKEUP
            | match cfg.bind_mode {
                BindMode::Auto => 0,
                BindMode::Copy => libc::XDP_COPY,
                BindMode::ZeroCopy => libc::XDP_ZEROCOPY,
            };
        let sa = libc::sockaddr_xdp {
            sxdp_family: libc::AF_XDP as u16,
            sxdp_flags: bind_flags,
            sxdp_ifindex: ifindex,
            sxdp_queue_id: cfg.queue_id,
            sxdp_shared_umem_fd: 0,
        };
        cvt(unsafe { libc::bind(fd, &sa as *const _ as *const libc::sockaddr, std::mem::size_of_val(&sa) as u32) })?;
        prog.register(cfg.queue_id, fd)?;

        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = cfg.mtu;
        caps.max_burst_size = Some(cfg.ring_size as usize);
        caps.checksum = cfg.checksum.clone();

        Ok(Self {
            rxs: RxSide { fd, rx, fill, frame_mask: !(cfg.frame_size as u64 - 1) },
            txs: TxSide { fd, tx, comp, free, auto_flush: cfg.auto_flush, scratch: vec![0; cfg.mtu], tx_dropped: 0 },
            sock,
            umem,
            _umem_map: guard,
            prog: Some(prog),
            caps,
            mac: read_mac(ifname)?,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Frames smoltcp tried to send while the TX ring/free list was exhausted.
    pub fn tx_dropped(&self) -> u64 {
        self.txs.tx_dropped
    }

    /// Kernel-side counters (`XDP_STATISTICS`).
    pub fn statistics(&self) -> io::Result<libc::xdp_statistics> {
        let mut st: libc::xdp_statistics = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&st) as libc::socklen_t;
        cvt(unsafe {
            libc::getsockopt(
                self.as_raw_fd(),
                libc::SOL_XDP,
                libc::XDP_STATISTICS,
                &mut st as *mut _ as *mut _,
                &mut len,
            )
        })?;
        Ok(st)
    }

    /// TX frames queued but not yet consumed by the kernel. If non-zero after `flush`,
    /// don't sleep indefinitely: poll again soon.
    pub fn tx_pending(&self) -> u32 {
        self.txs.tx.unconsumed()
    }

    /// Kick the kernel to transmit anything queued on the TX ring (only needed with
    /// `auto_flush = false`).
    pub fn flush(&mut self) {
        self.txs.kick();
    }

    /// Detach the XDP program before the socket is closed (also happens on drop).
    pub fn detach(&mut self) {
        self.prog = None;
    }
}

impl AsRawFd for XdpDevice {
    /// Pollable for `POLLIN` (RX ring non-empty) / `POLLOUT`.
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

struct UmemGuard(Umem);
impl Drop for UmemGuard {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.0.base.cast(), self.0.len) };
    }
}

impl TxSide {
    fn reclaim(&mut self) {
        let mut n = 0;
        while let Some(addr) = self.comp.pop() {
            self.free.push(addr);
            n += 1;
        }
        if n > 0 {
            self.comp.release();
        }
    }

    fn has_room(&mut self) -> bool {
        if self.free.is_empty() {
            self.reclaim();
        }
        !self.free.is_empty() && self.tx.free() > 0
    }

    fn kick(&mut self) {
        // Copy mode transmits at most 32 frames per sendto() (and returns EAGAIN if more are
        // queued), so keep kicking until the kernel has consumed the whole TX ring. Bounded so
        // that zero-copy mode, where the driver drains the ring asynchronously, cannot spin.
        for _ in 0..=self.tx.size() / 32 {
            if self.tx.unconsumed() == 0 || !self.tx.needs_wakeup() {
                break;
            }
            let r = unsafe { libc::sendto(self.fd, std::ptr::null(), 0, libc::MSG_DONTWAIT, std::ptr::null(), 0) };
            if r < 0 && !matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN | libc::EBUSY)) {
                break;
            }
        }
        self.reclaim();
    }
}

pub struct RxToken<'a> {
    side: &'a mut RxSide,
    umem: Umem,
    desc: libc::xdp_desc,
}

pub struct TxToken<'a> {
    side: &'a mut TxSide,
    umem: Umem,
}

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        // Safety: the frame was handed to us via the RX ring and is not on any other ring.
        f(unsafe { self.umem.frame(self.desc.addr, self.desc.len as usize) })
        // Drop returns the frame to the fill ring.
    }
}

impl Drop for RxToken<'_> {
    fn drop(&mut self) {
        let s = &mut *self.side;
        s.fill.push(self.desc.addr & s.frame_mask);
        s.fill.submit();
        if s.fill.needs_wakeup() {
            unsafe {
                libc::recvfrom(
                    s.fd,
                    std::ptr::null_mut(),
                    0,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
        }
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let s = self.side;
        if !s.has_room() {
            // Out of buffers (only reachable via the TX token paired with an RX token):
            // build the frame into scratch space and drop it, like a full NIC queue would.
            s.tx_dropped += 1;
            s.scratch.resize(len.max(s.scratch.len()), 0);
            return f(&mut s.scratch[..len]);
        }
        let addr = s.free.pop().unwrap();
        // Safety: `addr` came off the free list, so neither the kernel nor an RxToken owns it.
        let r = f(unsafe { self.umem.frame(addr, len) });
        s.tx.push(libc::xdp_desc { addr, len: len as u32, options: 0 });
        s.tx.submit();
        if s.auto_flush {
            s.kick();
        }
        r
    }
}

impl phy::Device for XdpDevice {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(RxToken<'_>, TxToken<'_>)> {
        let desc = self.rxs.rx.pop()?;
        self.rxs.rx.release();
        Some((RxToken { side: &mut self.rxs, umem: self.umem, desc }, TxToken { side: &mut self.txs, umem: self.umem }))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<TxToken<'_>> {
        if !self.txs.has_room() {
            self.txs.kick();
            return None;
        }
        Some(TxToken { side: &mut self.txs, umem: self.umem })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.caps.clone()
    }
}
