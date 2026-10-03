//! Single-threaded XSUB -> XPUB ZMTP bridge on one io_uring ring.
//!
//! Publishers connect to the front (we are XSUB), subscribers connect to the
//! back (we are XPUB). Subscriptions flow back-to-front, messages front-to-back.
//!
//! What replaces libzmq's HWM / SNDBUF / RCVBUF here:
//! * received bytes land in refcounted chunks and are forwarded by reference
//!   (header iovec + body iovecs) - no userspace copy on the data path;
//! * outbound frames are sent with IORING_OP_SENDMSG_ZC, so a buffer stays
//!   pinned until the kernel's notification CQE arrives;
//! * the only bound is bytes: a global budget for all live buffers, and a
//!   per-subscriber cap on queued + not-yet-notified bytes. Over budget, we
//!   stop posting recvs on publishers (TCP backpressure); with `--policy drop`
//!   a subscriber over its cap misses whole messages instead, like PUB at HWM.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::os::fd::{IntoRawFd, RawFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use io_uring::{cqueue, opcode, squeue, types, IoUring};
use weida_zmtp::{frame, Command, FrameKind, Greeting, Mechanism, Metadata, SocketType, GREETING_LEN};

// ---------------------------------------------------------------- config

#[derive(Clone)]
struct Config {
    front: String,
    back: String,
    zc: bool,
    budget: usize,
    sub_cap: usize,
    chunk: usize,
    direct: usize,
    drop_policy: bool,
    inflight: usize,
    max_frame: u64,
}

fn parse_args() -> Config {
    let mut c = Config {
        front: "127.0.0.1:5555".into(),
        back: "127.0.0.1:5556".into(),
        zc: true,
        budget: 256 << 20,
        sub_cap: 64 << 20,
        chunk: 256 << 10,
        direct: 64 << 10,
        drop_policy: false,
        inflight: 2,
        max_frame: 64 << 20,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--front" => c.front = v,
            "--back" => c.back = v,
            "--zc" => c.zc = v != "off",
            "--budget-mb" => c.budget = v.parse::<usize>().expect("MB") << 20,
            "--sub-cap-mb" => c.sub_cap = v.parse::<usize>().expect("MB") << 20,
            "--chunk-kb" => c.chunk = v.parse::<usize>().expect("KB") << 10,
            "--direct-kb" => c.direct = v.parse::<usize>().expect("KB") << 10,
            "--policy" => c.drop_policy = v == "drop",
            "--inflight" => c.inflight = v.parse::<usize>().expect("count").max(1),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    c
}

// ---------------------------------------------------------------- buffers

thread_local! {
    static POOL: RefCell<HashMap<usize, Vec<Box<[u8]>>>> = RefCell::new(HashMap::new());
    static LIVE: Cell<usize> = const { Cell::new(0) };
}

/// A pooled, fixed-size buffer. Written through a raw pointer while other
/// `Rc`s (and the kernel) read disjoint, already-filled ranges of it.
struct Buf {
    ptr: *mut u8,
    cap: usize,
}

impl Buf {
    fn alloc(min: usize) -> Rc<Buf> {
        let class = min.max(4096).next_power_of_two();
        let mem = POOL
            .with(|p| p.borrow_mut().get_mut(&class).and_then(Vec::pop))
            .unwrap_or_else(|| vec![0u8; class].into_boxed_slice());
        LIVE.with(|l| l.set(l.get() + class));
        let ptr = Box::into_raw(mem) as *mut u8;
        Rc::new(Buf { ptr, cap: class })
    }
    fn cap(&self) -> usize {
        self.cap
    }
    fn ptr(&self) -> *mut u8 {
        self.ptr
    }
    fn slice(&self, off: usize, len: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        let class = self.cap;
        let mem = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.ptr, class)) };
        LIVE.with(|l| l.set(l.get() - class));
        POOL.with(|p| {
            let mut p = p.borrow_mut();
            let free = p.entry(class).or_default();
            // Keep a bounded cache per class; the rest goes back to the allocator.
            if free.len() * class < (64 << 20) {
                free.push(mem);
            }
        });
    }
}

fn live_bytes() -> usize {
    LIVE.with(Cell::get)
}

/// One ZMTP frame to forward: our own header plus one or two body segments
/// that point into received buffers. Never copied, only refcounted.
struct Frame {
    hdr: [u8; 9],
    hlen: usize,
    segs: [(Option<Rc<Buf>>, usize, usize); 2],
}

impl Frame {
    fn new(more: bool, len: usize) -> Frame {
        let mut v = Vec::with_capacity(9);
        frame::encode_header(FrameKind::Message { more }, len as u64, &mut v);
        let mut hdr = [0u8; 9];
        hdr[..v.len()].copy_from_slice(&v);
        Frame { hdr, hlen: v.len(), segs: [(None, 0, 0), (None, 0, 0)] }
    }
    fn body_len(&self) -> usize {
        self.segs[0].2 + self.segs[1].2
    }
    fn wire_len(&self) -> usize {
        self.hlen + self.body_len()
    }
    fn more(&self) -> bool {
        self.hdr[0] & frame::MORE != 0
    }
    /// Whether the body starts with `prefix` (for subscription matching).
    fn starts_with(&self, prefix: &[u8]) -> bool {
        if prefix.len() > self.body_len() {
            return false;
        }
        let mut rest = prefix;
        for (buf, off, len) in &self.segs {
            if rest.is_empty() {
                break;
            }
            let Some(buf) = buf else { continue };
            let n = rest.len().min(*len);
            if buf.slice(*off, n) != &rest[..n] {
                return false;
            }
            rest = &rest[n..];
        }
        rest.is_empty()
    }
}

#[derive(Clone)]
enum Out {
    Frame(Rc<Frame>),
    Ctrl(Rc<Vec<u8>>),
}

impl Out {
    fn len(&self) -> usize {
        match self {
            Out::Frame(f) => f.wire_len(),
            Out::Ctrl(v) => v.len(),
        }
    }
    /// Appends iovecs for this item, skipping its first `skip` bytes.
    fn iovecs(&self, mut skip: usize, iov: &mut Vec<libc::iovec>) {
        let mut push = |ptr: *const u8, len: usize, skip: &mut usize| {
            if *skip >= len {
                *skip -= len;
                return;
            }
            iov.push(libc::iovec {
                iov_base: unsafe { ptr.add(*skip) } as *mut _,
                iov_len: len - *skip,
            });
            *skip = 0;
        };
        match self {
            Out::Ctrl(v) => push(v.as_ptr(), v.len(), &mut skip),
            Out::Frame(f) => {
                push(f.hdr.as_ptr(), f.hlen, &mut skip);
                for (b, off, len) in &f.segs {
                    if let Some(b) = b {
                        if *len > 0 {
                            push(unsafe { b.ptr().add(*off) }, *len, &mut skip);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- connections

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// An upstream publisher connected to our XSUB front.
    Pub,
    /// A downstream subscriber connected to our XPUB back.
    Sub,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Greeting,
    Handshake,
    Traffic,
}

/// A large frame being received straight into its own buffer.
struct Direct {
    frame: Frame,
    buf: Rc<Buf>,
    filled: usize,
    want: usize,
}

struct Conn {
    fd: RawFd,
    role: Role,
    phase: Phase,
    dead: bool,
    ops: usize,
    legacy_subs: bool,
    // receive side
    chunk: Rc<Buf>,
    start: usize,
    end: usize,
    direct: Option<Direct>,
    recv_inflight: bool,
    // multipart routing for publishers
    in_multipart: bool,
    targets: Vec<usize>,
    // send side
    outq: VecDeque<Out>,
    front_off: usize,
    send_inflight: usize,
    queued_bytes: usize,
    notif_bytes: usize,
    // subscriber state
    prefixes: Vec<Vec<u8>>,
}

enum Op {
    Accept { role: Role, listener: RawFd },
    Recv { conn: usize },
    Send { conn: usize, items: Vec<Out>, _iov: Vec<libc::iovec>, _msg: Box<libc::msghdr>, total: usize },
    Notif { conn: usize, _items: Vec<Out>, bytes: usize },
}

#[derive(Default)]
struct Stats {
    msgs_in: u64,
    bytes_in: u64,
    sends: u64,
    bytes_out: u64,
    dropped: u64,
    notifs: u64,
    zc_copied: u64,
    paused: u64,
}

struct Bridge {
    cfg: Config,
    ring: IoUring,
    ops: Vec<Option<Op>>,
    free_ops: Vec<usize>,
    conns: Vec<Option<Conn>>,
    subs_union: HashMap<Vec<u8>, usize>,
    stats: Stats,
    accept_addr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
}

const MAX_IOV: usize = 96;
const MAX_SEND_BYTES: usize = 8 << 20;
const IORING_SEND_ZC_REPORT_USAGE: u16 = 1 << 3;
const IORING_NOTIF_USAGE_ZC_COPIED: i32 = 1 << 31;

impl Bridge {
    fn new(cfg: Config) -> Bridge {
        let ring = IoUring::builder()
            .setup_single_issuer()
            .setup_coop_taskrun()
            .build(4096)
            .expect("io_uring");
        Bridge {
            cfg,
            ring,
            ops: Vec::new(),
            free_ops: Vec::new(),
            conns: Vec::new(),
            subs_union: HashMap::new(),
            stats: Stats::default(),
            accept_addr: Box::new((unsafe { std::mem::zeroed() }, 0)),
        }
    }

    fn op(&mut self, op: Op) -> u64 {
        if let Some(i) = self.free_ops.pop() {
            self.ops[i] = Some(op);
            i as u64
        } else {
            self.ops.push(Some(op));
            (self.ops.len() - 1) as u64
        }
    }

    fn push(&mut self, sqe: squeue::Entry) {
        unsafe {
            while self.ring.submission().push(&sqe).is_err() {
                self.ring.submit().expect("submit");
            }
        }
    }

    fn post_accept(&mut self, role: Role, listener: RawFd) {
        let ud = self.op(Op::Accept { role, listener });
        let a = &mut *self.accept_addr;
        a.1 = std::mem::size_of::<libc::sockaddr_storage>() as _;
        let sqe = opcode::Accept::new(types::Fd(listener), &mut a.0 as *mut _ as *mut _, &mut a.1)
            .build()
            .user_data(ud);
        self.push(sqe);
    }

    fn conn(&mut self, id: usize) -> &mut Conn {
        self.conns[id].as_mut().expect("live conn")
    }

    fn on_accept(&mut self, role: Role, fd: RawFd) {
        let one: libc::c_int = 1;
        unsafe {
            libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &one as *const _ as *const _, 4);
        }
        let conn = Conn {
            fd,
            role,
            phase: Phase::Greeting,
            dead: false,
            ops: 0,
            legacy_subs: false,
            chunk: Buf::alloc(self.cfg.chunk),
            start: 0,
            end: 0,
            direct: None,
            recv_inflight: false,
            in_multipart: false,
            targets: Vec::new(),
            outq: VecDeque::new(),
            front_off: 0,
            send_inflight: 0,
            queued_bytes: 0,
            notif_bytes: 0,
            prefixes: Vec::new(),
        };
        let id = match self.conns.iter().position(Option::is_none) {
            Some(i) => {
                self.conns[i] = Some(conn);
                i
            }
            None => {
                self.conns.push(Some(conn));
                self.conns.len() - 1
            }
        };
        eprintln!("[bridge] {role:?} connected (conn {id})");
        let g = Greeting::null().encode();
        self.enqueue(id, Out::Ctrl(Rc::new(g.to_vec())));
        self.post_recv(id);
    }

    // ------------------------------------------------------------ receive

    fn paused(&self) -> bool {
        if live_bytes() > self.cfg.budget {
            return true;
        }
        if !self.cfg.drop_policy {
            for c in self.conns.iter().flatten() {
                if c.role == Role::Sub && c.queued_bytes + c.notif_bytes > self.cfg.sub_cap {
                    return true;
                }
            }
        }
        false
    }

    fn post_recv(&mut self, id: usize) {
        let throttle = self.conns[id].as_ref().is_some_and(|c| c.role == Role::Pub && c.phase == Phase::Traffic)
            && self.paused();
        let chunk_size = self.cfg.chunk;
        let c = self.conn(id);
        if c.dead || c.recv_inflight {
            return;
        }
        if throttle {
            return;
        }
        let (ptr, len) = if let Some(d) = &c.direct {
            (unsafe { d.buf.ptr().add(d.filled) }, d.want - d.filled)
        } else {
            if c.end == c.chunk.cap() || c.chunk.cap() - c.end < 512 {
                // Out of room: compact what is left into a fresh (or the same,
                // if nothing else references it) chunk.
                let tail = c.end - c.start;
                let fresh = if Rc::strong_count(&c.chunk) == 1 {
                    unsafe { std::ptr::copy(c.chunk.ptr().add(c.start), c.chunk.ptr(), tail) };
                    None
                } else {
                    let n = Buf::alloc(chunk_size.max(tail * 2));
                    unsafe { std::ptr::copy_nonoverlapping(c.chunk.ptr().add(c.start), n.ptr(), tail) };
                    Some(n)
                };
                if let Some(n) = fresh {
                    c.chunk = n;
                }
                c.start = 0;
                c.end = tail;
            }
            (unsafe { c.chunk.ptr().add(c.end) }, c.chunk.cap() - c.end)
        };
        let fd = c.fd;
        c.recv_inflight = true;
        c.ops += 1;
        let ud = self.op(Op::Recv { conn: id });
        let sqe = opcode::Recv::new(types::Fd(fd), ptr, len.min(u32::MAX as usize) as u32).build().user_data(ud);
        self.push(sqe);
    }

    fn on_recv(&mut self, id: usize, res: i32) {
        {
            let c = self.conn(id);
            c.recv_inflight = false;
            c.ops -= 1;
            if c.dead {
                return self.reap(id);
            }
        }
        if res <= 0 {
            if res == -libc::EAGAIN || res == -libc::EINTR {
                return self.post_recv(id);
            }
            return self.kill(id, if res == 0 { "peer closed" } else { "recv error" });
        }
        let n = res as usize;
        let done = {
            let c = self.conn(id);
            if let Some(d) = &mut c.direct {
                d.filled += n;
                d.filled == d.want
            } else {
                c.end += n;
                false
            }
        };
        if done {
            let d = self.conn(id).direct.take().expect("direct");
            let mut f = d.frame;
            f.segs[1] = (Some(d.buf), 0, d.want);
            self.route(id, f);
        }
        if let Err(e) = self.parse(id) {
            return self.kill(id, &e);
        }
        if !self.conn(id).dead {
            self.post_recv(id);
        }
    }

    fn parse(&mut self, id: usize) -> Result<(), String> {
        loop {
            let (phase, avail) = {
                let c = self.conn(id);
                if c.dead || c.direct.is_some() {
                    return Ok(());
                }
                (c.phase, c.end - c.start)
            };
            match phase {
                Phase::Greeting => {
                    if avail < GREETING_LEN {
                        return Ok(());
                    }
                    let (g, role) = {
                        let c = self.conn(id);
                        (Greeting::decode(c.chunk.slice(c.start, GREETING_LEN)), c.role)
                    };
                    let g = g.map_err(|e| format!("greeting: {e}"))?;
                    let v = g.accept_downgrading(Mechanism::NULL).map_err(|e| format!("greeting: {e}"))?;
                    let ours = if role == Role::Pub { SocketType::XSub } else { SocketType::XPub };
                    let ready = Command::Ready(Metadata::new().with_socket_type(ours)).encode().map_err(|e| e.to_string())?;
                    let c = self.conn(id);
                    c.start += GREETING_LEN;
                    c.phase = Phase::Handshake;
                    c.legacy_subs = v.minor < 1;
                    self.enqueue(id, Out::Ctrl(Rc::new(ready)));
                }
                Phase::Handshake | Phase::Traffic => {
                    let max = self.cfg.max_frame;
                    let direct_min = self.cfg.direct;
                    let chunk_size = self.cfg.chunk;
                    let c = self.conn(id);
                    let bytes = c.chunk.slice(c.start, avail);
                    let (h, hl) = match frame::decode_header(bytes, max) {
                        Ok(x) => x,
                        Err(e) if !e.is_violation() => return Ok(()),
                        Err(e) => return Err(format!("frame: {e}")),
                    };
                    let len = h.len as usize;
                    let whole = hl + len <= avail;
                    match h.kind {
                        FrameKind::Command => {
                            if !whole {
                                if hl + len > chunk_size {
                                    return Err("command larger than a chunk".into());
                                }
                                return Ok(());
                            }
                            let body = c.chunk.slice(c.start + hl, len).to_vec();
                            c.start += hl + len;
                            self.on_command(id, &body)?;
                        }
                        FrameKind::Message { more } => {
                            if phase == Phase::Handshake {
                                return Err("message before READY".into());
                            }
                            let mut f = Frame::new(more, len);
                            if whole {
                                f.segs[0] = (Some(c.chunk.clone()), c.start + hl, len);
                                c.start += hl + len;
                                self.route(id, f);
                            } else if len >= direct_min {
                                // Large and incomplete: keep what we have in the
                                // chunk as segment 0, recv the rest straight into
                                // its own buffer as segment 1. No copy.
                                let have = avail - hl;
                                f.segs[0] = (Some(c.chunk.clone()), c.start + hl, have);
                                let want = len - have;
                                c.direct = Some(Direct { frame: f, buf: Buf::alloc(want), filled: 0, want });
                                c.start = c.end;
                                return Ok(());
                            } else {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }

    fn on_command(&mut self, id: usize, body: &[u8]) -> Result<(), String> {
        let nlen = *body.first().ok_or("empty command")? as usize;
        let name = body.get(1..1 + nlen).ok_or("truncated command")?;
        let data = &body[1 + nlen..];
        let (phase, role) = {
            let c = self.conn(id);
            (c.phase, c.role)
        };
        if phase == Phase::Handshake {
            if name != b"READY" {
                return Err(format!("expected READY, got {}", String::from_utf8_lossy(name)));
            }
            let md = match Command::decode(body).map_err(|e| e.to_string())? {
                Command::Ready(md) => md,
                _ => unreachable!(),
            };
            let ours = if role == Role::Pub { SocketType::XSub } else { SocketType::XPub };
            match md.socket_type() {
                Some(t) if ours.accepts(t) => {}
                other => return Err(format!("incompatible peer socket type {other:?}")),
            }
            self.conn(id).phase = Phase::Traffic;
            eprintln!("[bridge] conn {id} ready ({role:?})");
            if role == Role::Pub {
                let all: Vec<Vec<u8>> = self.subs_union.keys().cloned().collect();
                for p in all {
                    self.send_sub(id, true, &p);
                }
            }
            return Ok(());
        }
        match name {
            b"PING" => {
                let ctx = data.get(2..).unwrap_or(&[]);
                let ctx = &ctx[..ctx.len().min(16)];
                let mut v = Vec::new();
                frame::encode_header(FrameKind::Command, (5 + ctx.len()) as u64, &mut v);
                v.extend_from_slice(b"\x04PONG");
                v.extend_from_slice(ctx);
                self.enqueue(id, Out::Ctrl(Rc::new(v)));
            }
            b"SUBSCRIBE" if role == Role::Sub => self.subscription(id, true, data),
            b"CANCEL" if role == Role::Sub => self.subscription(id, false, data),
            b"ERROR" => return Err("peer sent ERROR".into()),
            _ => {} // unknown or irrelevant commands are ignored, like libzmq
        }
        Ok(())
    }

    // ------------------------------------------------------------ routing

    fn subscription(&mut self, id: usize, on: bool, prefix: &[u8]) {
        let c = self.conn(id);
        if on {
            c.prefixes.push(prefix.to_vec());
        } else if let Some(i) = c.prefixes.iter().position(|p| p == prefix) {
            c.prefixes.swap_remove(i);
        } else {
            return;
        }
        let count = self.subs_union.entry(prefix.to_vec()).or_insert(0);
        let changed = if on {
            *count += 1;
            *count == 1
        } else {
            *count -= 1;
            *count == 0
        };
        if !on && *count == 0 {
            self.subs_union.remove(prefix);
        }
        if changed {
            let pubs: Vec<usize> = self.live(Role::Pub);
            for p in pubs {
                self.send_sub(p, on, prefix);
            }
        }
    }

    fn send_sub(&mut self, id: usize, on: bool, prefix: &[u8]) {
        let mut v = Vec::new();
        if self.conn(id).legacy_subs {
            frame::encode_header(FrameKind::Message { more: false }, (1 + prefix.len()) as u64, &mut v);
            v.push(u8::from(on));
        } else {
            let name: &[u8] = if on { b"\x09SUBSCRIBE" } else { b"\x06CANCEL" };
            frame::encode_header(FrameKind::Command, (name.len() + prefix.len()) as u64, &mut v);
            v.extend_from_slice(name);
        }
        v.extend_from_slice(prefix);
        self.enqueue(id, Out::Ctrl(Rc::new(v)));
    }

    fn live(&self, role: Role) -> Vec<usize> {
        self.conns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().filter(|c| c.role == role && c.phase == Phase::Traffic && !c.dead).map(|_| i))
            .collect()
    }

    fn route(&mut self, id: usize, f: Frame) {
        let role = self.conn(id).role;
        if role == Role::Sub {
            // A subscriber's message is a subscription in the %x01/%x00 form.
            if !f.more() && f.body_len() >= 1 {
                let mut b = Vec::with_capacity(f.body_len());
                for (buf, off, len) in &f.segs {
                    if let Some(buf) = buf {
                        b.extend_from_slice(buf.slice(*off, *len));
                    }
                }
                if b[0] <= 1 {
                    self.subscription(id, b[0] == 1, &b[1..]);
                }
            }
            return;
        }
        self.stats.bytes_in += f.body_len() as u64;
        let first = !self.conn(id).in_multipart;
        if first {
            self.stats.msgs_in += 1;
            let cap = self.cfg.sub_cap;
            let drop_policy = self.cfg.drop_policy;
            let mut targets = std::mem::take(&mut self.conn(id).targets);
            targets.clear();
            for (i, c) in self.conns.iter().enumerate() {
                let Some(c) = c else { continue };
                if c.role != Role::Sub || c.phase != Phase::Traffic || c.dead {
                    continue;
                }
                if !c.prefixes.iter().any(|p| f.starts_with(p)) {
                    continue;
                }
                if drop_policy && c.queued_bytes + c.notif_bytes > cap {
                    self.stats.dropped += 1;
                    continue;
                }
                targets.push(i);
            }
            self.conn(id).targets = targets;
        }
        let more = f.more();
        self.conn(id).in_multipart = more;
        let targets = std::mem::take(&mut self.conn(id).targets);
        if !targets.is_empty() {
            let f = Rc::new(f);
            for &t in &targets {
                if self.conns[t].as_ref().is_some_and(|c| !c.dead) {
                    self.enqueue(t, Out::Frame(f.clone()));
                }
            }
        }
        self.conn(id).targets = targets;
    }

    // ------------------------------------------------------------ send

    fn enqueue(&mut self, id: usize, item: Out) {
        let c = self.conn(id);
        c.queued_bytes += item.len();
        c.outq.push_back(item);
        if c.send_inflight == 0 {
            self.post_send(id);
        }
    }

    /// Submits up to `--inflight` sends for this peer as one linked chain.
    ///
    /// Two independent sends on one TCP socket may interleave if the first is
    /// only partly written, so the chain is ordered with IOSQE_IO_LINK and
    /// each send carries MSG_WAITALL: it completes whole or fails, and a
    /// failure cancels the rest of the chain. A new chain is only started
    /// once the previous one has fully completed.
    fn post_send(&mut self, id: usize) {
        let zc = self.cfg.zc;
        let k = self.cfg.inflight;
        let c = self.conn(id);
        if c.dead || c.send_inflight > 0 || c.outq.is_empty() {
            return;
        }
        let fd = c.fd;
        let mut chain = Vec::with_capacity(k);
        let mut idx = 0;
        let mut skip = c.front_off;
        while chain.len() < k && idx < c.outq.len() {
            let mut iov = Vec::with_capacity(MAX_IOV + 3);
            let mut items = Vec::new();
            let mut total = 0;
            while idx < c.outq.len() && iov.len() + 3 <= MAX_IOV && total < MAX_SEND_BYTES {
                let item = &c.outq[idx];
                let before = iov.len();
                item.iovecs(skip, &mut iov);
                total += iov[before..].iter().map(|v| v.iov_len).sum::<usize>();
                skip = 0;
                items.push(item.clone());
                idx += 1;
            }
            let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = iov.len();
            let flags = (libc::MSG_NOSIGNAL | libc::MSG_WAITALL) as u32;
            let sqe = if zc {
                opcode::SendMsgZc::new(types::Fd(fd), &*msg).ioprio(IORING_SEND_ZC_REPORT_USAGE).flags(flags).build()
            } else {
                opcode::SendMsg::new(types::Fd(fd), &*msg).flags(flags).build()
            };
            chain.push((sqe, Op::Send { conn: id, items, _iov: iov, _msg: msg, total }));
        }
        let n = chain.len();
        c.send_inflight = n;
        c.ops += n;
        // A chain must not straddle two submits, or the kernel ends it early.
        let free = {
            let sq = self.ring.submission();
            sq.capacity() - sq.len()
        };
        if free < n {
            self.ring.submit().expect("submit");
        }
        for (i, (sqe, op)) in chain.into_iter().enumerate() {
            let ud = self.op(op);
            let sqe = if i + 1 < n { sqe.flags(squeue::Flags::IO_LINK) } else { sqe };
            self.push(sqe.user_data(ud));
        }
        self.stats.sends += n as u64;
    }

    fn on_send(&mut self, ud: u64, res: i32, flags: u32) {
        let Some(Op::Send { conn: id, items, _iov, _msg, total }) = self.ops[ud as usize].take() else {
            unreachable!()
        };
        let notif_pending = self.cfg.zc && cqueue::more(flags);
        let sent = res.max(0) as usize;
        let chain_done = {
            let c = self.conn(id);
            c.send_inflight -= 1;
            c.ops -= 1;
            // Chain members complete in order, so the queue advances in order.
            // A dead connection's queue was already cleared by `kill`.
            let mut left = if c.dead { 0 } else { sent };
            while left > 0 {
                let front_len = c.outq.front().expect("queued").len() - c.front_off;
                if left >= front_len {
                    left -= front_len;
                    c.outq.pop_front();
                    c.front_off = 0;
                } else {
                    c.front_off += left;
                    left = 0;
                }
            }
            if !c.dead {
                c.queued_bytes -= sent;
            }
            if notif_pending {
                c.notif_bytes += sent;
                c.ops += 1;
            }
            c.send_inflight == 0
        };
        if notif_pending {
            // Keep every buffer this send referenced alive until the kernel says so.
            self.ops[ud as usize] = Some(Op::Notif { conn: id, _items: items, bytes: sent });
        } else {
            self.free_ops.push(ud as usize);
        }
        self.stats.bytes_out += sent as u64;
        if self.conn(id).dead {
            return self.reap(id);
        }
        // -ECANCELED is a later chain member after an earlier one stopped
        // short; it sent nothing, and the queue resumes from where it stands.
        let transient = [libc::EAGAIN, libc::ENOBUFS, libc::EINTR, libc::ECANCELED];
        if res < 0 && !transient.contains(&-res) {
            return self.kill(id, &format!("send error {}", -res));
        }
        let _ = total;
        if chain_done {
            self.post_send(id);
        }
        self.resume_pubs();
    }

    fn on_notif(&mut self, ud: u64, res: i32) {
        let Some(Op::Notif { conn: id, _items, bytes }) = self.ops[ud as usize].take() else {
            unreachable!()
        };
        self.free_ops.push(ud as usize);
        self.stats.notifs += 1;
        if res & IORING_NOTIF_USAGE_ZC_COPIED != 0 {
            self.stats.zc_copied += 1;
        }
        drop(_items);
        {
            let c = self.conn(id);
            c.notif_bytes -= bytes;
            c.ops -= 1;
            if c.dead {
                return self.reap(id);
            }
        }
        self.resume_pubs();
    }

    fn resume_pubs(&mut self) {
        if self.paused() {
            self.stats.paused += 1;
            return;
        }
        for i in 0..self.conns.len() {
            if self.conns[i].as_ref().is_some_and(|c| c.role == Role::Pub && !c.recv_inflight && !c.dead) {
                self.post_recv(i);
            }
        }
    }

    // ------------------------------------------------------------ teardown

    fn kill(&mut self, id: usize, why: &str) {
        let (role, prefixes) = {
            let c = self.conn(id);
            if c.dead {
                return;
            }
            c.dead = true;
            unsafe { libc::shutdown(c.fd, libc::SHUT_RDWR) };
            (c.role, std::mem::take(&mut c.prefixes))
        };
        eprintln!("[bridge] conn {id} ({role:?}) closed: {why}");
        // Restore the prefixes so `subscription` can cancel them one by one.
        self.conn(id).prefixes = prefixes.clone();
        for p in prefixes {
            self.subscription(id, false, &p);
        }
        let c = self.conn(id);
        c.outq.clear();
        c.queued_bytes = 0;
        c.direct = None;
        self.reap(id);
        self.resume_pubs();
    }

    fn reap(&mut self, id: usize) {
        if self.conns[id].as_ref().is_some_and(|c| c.dead && c.ops == 0) {
            let c = self.conns[id].take().expect("conn");
            unsafe { libc::close(c.fd) };
        }
    }

    // ------------------------------------------------------------ loop

    fn run(&mut self) {
        let front = TcpListener::bind(&self.cfg.front).expect("bind front").into_raw_fd();
        let back = TcpListener::bind(&self.cfg.back).expect("bind back").into_raw_fd();
        eprintln!(
            "[bridge] XSUB front {} / XPUB back {} | zc={} inflight={} budget={}MB sub_cap={}MB chunk={}KB policy={}",
            self.cfg.front,
            self.cfg.back,
            self.cfg.zc,
            self.cfg.inflight,
            self.cfg.budget >> 20,
            self.cfg.sub_cap >> 20,
            self.cfg.chunk >> 10,
            if self.cfg.drop_policy { "drop" } else { "backpressure" }
        );
        self.post_accept(Role::Pub, front);
        self.post_accept(Role::Sub, back);
        let mut last = Instant::now();
        let mut last_bytes = 0u64;
        let mut cqes = Vec::with_capacity(4096);
        loop {
            self.ring.submit_and_wait(1).expect("submit_and_wait");
            cqes.extend(self.ring.completion().map(|e| (e.user_data(), e.result(), e.flags())));
            for (ud, res, flags) in cqes.drain(..) {
                if cqueue::notif(flags) {
                    self.on_notif(ud, res);
                    continue;
                }
                match &self.ops[ud as usize] {
                    Some(Op::Accept { role, listener }) => {
                        let (role, listener) = (*role, *listener);
                        self.ops[ud as usize] = None;
                        self.free_ops.push(ud as usize);
                        if res >= 0 {
                            self.on_accept(role, res);
                        }
                        self.post_accept(role, listener);
                    }
                    Some(Op::Recv { conn }) => {
                        let id = *conn;
                        self.ops[ud as usize] = None;
                        self.free_ops.push(ud as usize);
                        self.on_recv(id, res);
                    }
                    Some(Op::Send { .. }) => self.on_send(ud, res, flags),
                    other => panic!("unexpected completion for op {} ({})", ud, other.is_some()),
                }
            }
            if last.elapsed() >= Duration::from_secs(1) {
                let s = &self.stats;
                let dt = last.elapsed().as_secs_f64();
                eprintln!(
                    "[bridge] in {:.0} MB/s | msgs_in {} out_bytes {} | avg send {} KB | live {} MB | dropped {} | zc notifs {} copied {} | pauses {}",
                    (s.bytes_in - last_bytes) as f64 / dt / 1e6,
                    s.msgs_in,
                    s.bytes_out,
                    s.bytes_out / s.sends.max(1) / 1024,
                    live_bytes() >> 20,
                    s.dropped,
                    s.notifs,
                    s.zc_copied,
                    s.paused
                );
                last_bytes = s.bytes_in;
                last = Instant::now();
            }
        }
    }
}

fn main() {
    let cfg = parse_args();
    Bridge::new(cfg).run();
}
