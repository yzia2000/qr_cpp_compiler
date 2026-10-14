//! Minimal, dependency-free loader for the XDP program that steers frames into
//! an AF_XDP socket. This replaces what libbpf/libxdp would normally do:
//!
//! 1. create an `XSKMAP`
//! 2. load a 6-instruction XDP program: `return bpf_redirect_map(&xsks, rx_queue_index, XDP_PASS)`
//! 3. attach it to the interface with a BPF link (detached when the link fd is closed)
//!
//! Frames on queues without a registered socket fall through to the kernel stack (`XDP_PASS`).

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const BPF_MAP_CREATE: libc::c_long = 0;
const BPF_MAP_UPDATE_ELEM: libc::c_long = 2;
const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_LINK_CREATE: libc::c_long = 28;

const BPF_MAP_TYPE_XSKMAP: u32 = 17;
const BPF_PROG_TYPE_XDP: u32 = 6;
const BPF_XDP: u32 = 37;
const BPF_PSEUDO_MAP_FD: u8 = 1;
const BPF_FUNC_REDIRECT_MAP: i32 = 51;
const XDP_PASS: i32 = 2;

pub const XDP_FLAGS_SKB_MODE: u32 = 1 << 1;
pub const XDP_FLAGS_DRV_MODE: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct Insn {
    code: u8,
    regs: u8, // dst:4 | src:4 (little endian)
    off: i16,
    imm: i32,
}

const fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn { code, regs: (src << 4) | dst, off, imm }
}

#[repr(C)]
#[derive(Default)]
struct MapCreateAttr {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct MapUpdateAttr {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Default)]
struct ProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

#[repr(C)]
#[derive(Default)]
struct LinkCreateAttr {
    prog_fd: u32,
    target_ifindex: u32,
    attach_type: u32,
    flags: u32,
}

fn bpf<T>(cmd: libc::c_long, attr: &mut T) -> io::Result<libc::c_long> {
    let ret = unsafe { libc::syscall(libc::SYS_bpf, cmd, attr as *mut T, std::mem::size_of::<T>() as u32) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn bpf_fd<T>(cmd: libc::c_long, attr: &mut T) -> io::Result<OwnedFd> {
    bpf(cmd, attr).map(|fd| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// An XDP redirect program attached to an interface. Dropping it detaches the program.
pub struct XdpProgram {
    xsk_map: OwnedFd,
    _prog: OwnedFd,
    _link: OwnedFd,
}

impl XdpProgram {
    pub fn attach(ifindex: u32, max_queues: u32, xdp_flags: u32) -> io::Result<Self> {
        let mut map_attr = MapCreateAttr {
            map_type: BPF_MAP_TYPE_XSKMAP,
            key_size: 4,
            value_size: 4,
            max_entries: max_queues,
            ..Default::default()
        };
        let xsk_map = bpf_fd(BPF_MAP_CREATE, &mut map_attr)?;

        let map_fd = xsk_map.as_raw_fd();
        let prog = [
            insn(0x61, 2, 1, 16, 0),                     // r2 = *(u32 *)(r1 + 16)  ; ctx->rx_queue_index
            insn(0x18, 1, BPF_PSEUDO_MAP_FD, 0, map_fd), // r1 = &xsk_map (ld_imm64, 1/2)
            insn(0x00, 0, 0, 0, 0),                      //                         (ld_imm64, 2/2)
            insn(0xb7, 3, 0, 0, XDP_PASS),               // r3 = XDP_PASS           ; action on map miss
            insn(0x85, 0, 0, 0, BPF_FUNC_REDIRECT_MAP),  // r0 = bpf_redirect_map(r1, r2, r3)
            insn(0x95, 0, 0, 0, 0),                      // exit
        ];
        let license = c"GPL";
        let mut log = vec![0u8; 64 * 1024];
        let mut name = [0u8; 16];
        name[..8].copy_from_slice(b"xsk_redr");
        let mut prog_attr = ProgLoadAttr {
            prog_type: BPF_PROG_TYPE_XDP,
            insn_cnt: prog.len() as u32,
            insns: prog.as_ptr() as u64,
            license: license.as_ptr() as u64,
            log_level: 1,
            log_size: log.len() as u32,
            log_buf: log.as_mut_ptr() as u64,
            prog_name: name,
            expected_attach_type: BPF_XDP,
            ..Default::default()
        };
        let prog_fd = bpf_fd(BPF_PROG_LOAD, &mut prog_attr).map_err(|e| {
            let msg = CStr::from_bytes_until_nul(&log).map(|s| s.to_string_lossy()).unwrap_or_default();
            io::Error::new(e.kind(), format!("BPF_PROG_LOAD: {e}\nverifier log:\n{msg}"))
        })?;

        let mut link_attr = LinkCreateAttr {
            prog_fd: prog_fd.as_raw_fd() as u32,
            target_ifindex: ifindex,
            attach_type: BPF_XDP,
            flags: xdp_flags,
        };
        let link = bpf_fd(BPF_LINK_CREATE, &mut link_attr)
            .map_err(|e| io::Error::new(e.kind(), format!("BPF_LINK_CREATE (attach XDP): {e}")))?;

        Ok(Self { xsk_map, _prog: prog_fd, _link: link })
    }

    /// Steer frames received on `queue_id` into the AF_XDP socket `xsk_fd`.
    pub fn register(&self, queue_id: u32, xsk_fd: RawFd) -> io::Result<()> {
        let key = queue_id;
        let value = xsk_fd as u32;
        let mut attr = MapUpdateAttr {
            map_fd: self.xsk_map.as_raw_fd() as u32,
            key: &key as *const u32 as u64,
            value: &value as *const u32 as u64,
            ..Default::default()
        };
        bpf(BPF_MAP_UPDATE_ELEM, &mut attr).map(drop)
    }
}
