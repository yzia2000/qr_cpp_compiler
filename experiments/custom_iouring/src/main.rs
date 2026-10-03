//! custom iouring: an XSUB -> XPUB forwarder speaking ZMTP 3.1 (libzmq
//! compatible), built on two io_uring instances in one thread:
//!
//! * the **xsub iouring** owns the XSUB listener and its connections (the
//!   peers that send messages, e.g. libzmq PUB/XPUB sockets): accept, recv,
//!   and the subscription commands we send to them;
//! * the **xpub iouring** owns the XPUB listener and its connections (the
//!   peers that receive messages, e.g. libzmq SUB/XSUB sockets): accept, the
//!   subscription commands they send us, and the forwarded messages.
//!
//! Forwarding is straight and synchronous, with no queue: a message is
//! received completely on the XSUB side, then its frames are sent as-is (the
//! very bytes that arrived, header included, straight from the receive
//! buffer) to every XPUB peer whose subscriptions match, and the next message
//! is not read until all of those sends have completed. Flow control is
//! therefore TCP's own: a slow XPUB peer stalls the sends, which stalls the
//! XSUB reads, which closes the XSUB peers' windows.

use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::time::{Duration, Instant};

use io_uring::{cqueue, opcode, types, IoUring};
use weida_zmtp::{frame, Command, FrameKind, Greeting, Mechanism, Metadata, SocketType, GREETING_LEN};

// ---------------------------------------------------------------- config

struct Config {
    xsub: String,
    xpub: String,
    zc: bool,
    max_msg: usize,
}

fn parse_args() -> Config {
    let mut c = Config { xsub: "127.0.0.1:5555".into(), xpub: "127.0.0.1:5556".into(), zc: false, max_msg: 64 << 20 };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--xsub" => c.xsub = v,
            "--xpub" => c.xpub = v,
            "--zc" => c.zc = v == "on",
            "--max-msg-mb" => c.max_msg = v.parse::<usize>().expect("MB") << 20,
            other => panic!("unknown argument {other} (use --xsub ADDR --xpub ADDR [--zc on] [--max-msg-mb N])"),
        }
        i += 2;
    }
    c
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    /// A connection accepted on the XSUB listener; served by the xsub iouring.
    Xsub,
    /// A connection accepted on the XPUB listener; served by the xpub iouring.
    Xpub,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Greeting,
    Handshake,
    Traffic,
}

struct Conn {
    fd: RawFd,
    side: Side,
    phase: Phase,
    dead: bool,
    /// Operations of ours still in the kernel for this fd.
    ops: usize,
    /// Peer greeted with ZMTP 3.0: it wants subscriptions as %x01/%x00 messages.
    legacy: bool,
    /// Received bytes not yet consumed: buf[start..end].
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// Bytes still missing from the frame being received, when known.
    need: usize,
    /// XPUB peers: the prefixes this peer subscribed to.
    prefixes: Vec<Vec<u8>>,
}

enum OpKind {
    Accept { listener: RawFd },
    Recv,
    Send { _ctl: Option<Vec<u8>>, _iov: Vec<libc::iovec>, _msg: Box<libc::msghdr>, len: usize },
    /// On the xsub iouring: readiness of the xpub iouring's completion queue.
    WatchXpubRing,
}

struct Op {
    kind: OpKind,
    conn: usize,
    done: Option<i32>,
    notif_pending: bool,
}

struct Ring {
    ring: IoUring,
    ops: Vec<Option<Op>>,
    free: Vec<usize>,
}

impl Ring {
    fn new() -> Ring {
        // No COOP_TASKRUN: a completion on one ring must be able to wake this
        // thread while it sleeps in the other ring's io_uring_enter.
        let ring = IoUring::builder().setup_single_issuer().build(1024).expect("io_uring");
        Ring { ring, ops: Vec::new(), free: Vec::new() }
    }
    fn add(&mut self, op: Op) -> u64 {
        let i = match self.free.pop() {
            Some(i) => {
                self.ops[i] = Some(op);
                i
            }
            None => {
                self.ops.push(Some(op));
                self.ops.len() - 1
            }
        };
        i as u64
    }
    fn push(&mut self, sqe: io_uring::squeue::Entry) {
        unsafe {
            while self.ring.submission().push(&sqe).is_err() {
                self.ring.submit().expect("submit");
            }
        }
    }
    fn release(&mut self, ud: u64) -> Op {
        let op = self.ops[ud as usize].take().expect("op");
        self.free.push(ud as usize);
        op
    }
}

enum Event {
    Accepted(Side, i32),
    Received(usize, i32),
}

#[derive(Default)]
struct Stats {
    msgs: u64,
    bytes: u64,
}

struct App {
    cfg: Config,
    xsub: Ring,
    xpub: Ring,
    conns: Vec<Option<Conn>>,
    /// Every prefix any XPUB peer subscribed to, with a count, so that XSUB
    /// peers see one SUBSCRIBE per distinct prefix and one CANCEL at zero.
    union: HashMap<Vec<u8>, usize>,
    events: VecDeque<Event>,
    watch_armed: bool,
    accept_addr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
    stats: Stats,
}

const RECV_MIN: usize = 64 << 10;
const IORING_SEND_ZC_REPORT_USAGE: u16 = 1 << 3;

impl App {
    fn ring(&mut self, side: Side) -> &mut Ring {
        match side {
            Side::Xsub => &mut self.xsub,
            Side::Xpub => &mut self.xpub,
        }
    }

    fn conn(&mut self, id: usize) -> &mut Conn {
        self.conns[id].as_mut().expect("conn")
    }

    fn ours(side: Side) -> SocketType {
        match side {
            Side::Xsub => SocketType::XSub,
            Side::Xpub => SocketType::XPub,
        }
    }

    // ------------------------------------------------------------ completions

    /// Moves completions off one ring. Sends are only marked done (their
    /// waiter collects them); accepts and recvs become events handled by the
    /// main loop, so nothing here re-enters a handler.
    fn drain(&mut self, side: Side) {
        let cqes: Vec<(u64, i32, u32)> =
            self.ring(side).ring.completion().map(|e| (e.user_data(), e.result(), e.flags())).collect();
        for (ud, res, flags) in cqes {
            let ring = self.ring(side);
            if cqueue::notif(flags) {
                ring.ops[ud as usize].as_mut().expect("send op").notif_pending = false;
                continue;
            }
            let more = cqueue::more(flags);
            let op = ring.ops[ud as usize].as_mut().expect("op");
            match op.kind {
                OpKind::Send { .. } => {
                    op.done = Some(res);
                    op.notif_pending = more;
                }
                OpKind::Accept { listener } => {
                    ring.release(ud);
                    self.events.push_back(Event::Accepted(side, res));
                    self.post_accept(side, listener);
                }
                OpKind::Recv => {
                    let conn = op.conn;
                    ring.release(ud);
                    self.events.push_back(Event::Received(conn, res));
                }
                OpKind::WatchXpubRing => {
                    if !more {
                        ring.release(ud);
                        self.watch_armed = false;
                    }
                }
            }
        }
    }

    fn post_accept(&mut self, side: Side, listener: RawFd) {
        let a = &mut *self.accept_addr;
        a.1 = std::mem::size_of::<libc::sockaddr_storage>() as _;
        let (addr, len) = (&mut a.0 as *mut _ as *mut libc::sockaddr, &mut a.1 as *mut libc::socklen_t);
        let ring = self.ring(side);
        let ud = ring.add(Op { kind: OpKind::Accept { listener }, conn: usize::MAX, done: None, notif_pending: false });
        ring.push(opcode::Accept::new(types::Fd(listener), addr, len).build().user_data(ud));
    }

    fn watch_xpub_ring(&mut self) {
        if self.watch_armed {
            return;
        }
        let fd = self.xpub.ring.as_raw_fd();
        let ud = self.xsub.add(Op { kind: OpKind::WatchXpubRing, conn: usize::MAX, done: None, notif_pending: false });
        self.xsub.push(opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as u32).multi(true).build().user_data(ud));
        self.watch_armed = true;
    }

    // ------------------------------------------------------------ synchronous send

    /// Sends `bytes` (one contiguous slice) to every conn in `to`, all on the
    /// same ring, and returns only when every send has completed (and, with
    /// zero-copy, been released by the kernel). Failed peers are killed.
    fn send_sync(&mut self, side: Side, to: &[usize], ptr: *const u8, len: usize, ctl: Option<Vec<u8>>) {
        if to.is_empty() || len == 0 {
            return;
        }
        let zc = self.cfg.zc && ctl.is_none();
        let mut uds = Vec::with_capacity(to.len());
        let mut ctl = ctl;
        for (i, &id) in to.iter().enumerate() {
            let fd = self.conn(id).fd;
            self.conn(id).ops += 1;
            let mut iov = vec![libc::iovec { iov_base: ptr as *mut _, iov_len: len }];
            let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = 1;
            let flags = (libc::MSG_NOSIGNAL | libc::MSG_WAITALL) as u32;
            let sqe = if zc {
                opcode::SendMsgZc::new(types::Fd(fd), &*msg).ioprio(IORING_SEND_ZC_REPORT_USAGE).flags(flags).build()
            } else {
                opcode::SendMsg::new(types::Fd(fd), &*msg).flags(flags).build()
            };
            // The control buffer (if any) is owned by the last op; all of
            // them complete before this function returns.
            let keep = if i + 1 == to.len() { ctl.take() } else { None };
            let ring = self.ring(side);
            let ud = ring.add(Op {
                kind: OpKind::Send { _ctl: keep, _iov: iov, _msg: msg, len },
                conn: id,
                done: None,
                notif_pending: false,
            });
            ring.push(sqe.user_data(ud));
            uds.push(ud);
        }
        loop {
            let pending = uds.iter().any(|&ud| {
                let op = self.ring(side).ops[ud as usize].as_ref().expect("op");
                op.done.is_none() || op.notif_pending
            });
            if !pending {
                break;
            }
            self.ring(side).ring.submit_and_wait(1).expect("submit_and_wait");
            self.drain(side);
        }
        for ud in uds {
            let op = self.ring(side).release(ud);
            let id = op.conn;
            self.conn(id).ops -= 1;
            let res = op.done.expect("done");
            let OpKind::Send { len, .. } = op.kind else { unreachable!() };
            if res < 0 || res as usize != len {
                self.kill(id, &format!("send failed ({res})"));
            }
            self.reap(id);
        }
    }

    // ------------------------------------------------------------ connections

    fn on_accept(&mut self, side: Side, fd: i32) {
        if fd < 0 {
            return;
        }
        let one: libc::c_int = 1;
        unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &one as *const _ as *const _, 4) };
        let conn = Conn {
            fd,
            side,
            phase: Phase::Greeting,
            dead: false,
            ops: 0,
            legacy: false,
            buf: vec![0; 256 << 10],
            start: 0,
            end: 0,
            need: 0,
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
        eprintln!("[custom iouring] {side:?} peer connected (conn {id})");
        let g = Greeting::null().encode().to_vec();
        self.send_ctl(side, id, g);
        self.arm_recv(id);
    }

    fn send_ctl(&mut self, side: Side, id: usize, bytes: Vec<u8>) {
        if self.conns[id].as_ref().is_none_or(|c| c.dead) {
            return;
        }
        let (ptr, len) = (bytes.as_ptr(), bytes.len());
        self.send_sync(side, &[id], ptr, len, Some(bytes));
    }

    fn arm_recv(&mut self, id: usize) {
        let max = self.cfg.max_msg;
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead {
            return;
        }
        if c.start == c.end {
            c.start = 0;
            c.end = 0;
        }
        // A large frame is received exactly to its end, so the next message
        // starts at the front of the buffer and nothing ever needs moving.
        let want = if c.need >= RECV_MIN { c.need } else { RECV_MIN };
        if c.buf.len() - c.end < want {
            if c.start > 0 {
                c.buf.copy_within(c.start..c.end, 0);
                c.end -= c.start;
                c.start = 0;
            }
            if c.buf.len() - c.end < want {
                let size = (c.end + want).next_power_of_two().min(max + (1 << 20));
                c.buf.resize(size.max(c.end + want), 0);
            }
        }
        let len = if c.need >= RECV_MIN { c.need } else { c.buf.len() - c.end };
        let ptr = unsafe { c.buf.as_mut_ptr().add(c.end) };
        let (fd, side) = (c.fd, c.side);
        c.ops += 1;
        let ring = self.ring(side);
        let ud = ring.add(Op { kind: OpKind::Recv, conn: id, done: None, notif_pending: false });
        ring.push(opcode::Recv::new(types::Fd(fd), ptr, len as u32).build().user_data(ud));
    }

    fn on_recv(&mut self, id: usize, res: i32) {
        self.conn(id).ops -= 1;
        if self.conn(id).dead {
            return self.reap(id);
        }
        if res <= 0 {
            return self.kill(id, if res == 0 { "peer closed" } else { "recv error" });
        }
        self.conn(id).end += res as usize;
        if let Err(e) = self.parse(id) {
            return self.kill(id, &e);
        }
        self.arm_recv(id);
        self.reap(id);
    }

    fn kill(&mut self, id: usize, why: &str) {
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead {
            return;
        }
        c.dead = true;
        unsafe { libc::shutdown(c.fd, libc::SHUT_RDWR) };
        let (side, prefixes) = (c.side, std::mem::take(&mut c.prefixes));
        eprintln!("[custom iouring] {side:?} conn {id} closed: {why}");
        for p in prefixes {
            self.union_change(false, &p);
        }
        self.reap(id);
    }

    fn reap(&mut self, id: usize) {
        if self.conns[id].as_ref().is_some_and(|c| c.dead && c.ops == 0) {
            let c = self.conns[id].take().expect("conn");
            unsafe { libc::close(c.fd) };
        }
    }

    fn peers(&self, side: Side) -> Vec<usize> {
        self.conns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().filter(|c| c.side == side && c.phase == Phase::Traffic && !c.dead).map(|_| i))
            .collect()
    }

    // ------------------------------------------------------------ protocol

    fn parse(&mut self, id: usize) -> Result<(), String> {
        loop {
            // A failed send to another peer can cascade into closing this one.
            let Some(c) = self.conns[id].as_mut() else { return Ok(()) };
            if c.dead {
                return Ok(());
            }
            let side = c.side;
            match c.phase {
                Phase::Greeting => {
                    if c.end - c.start < GREETING_LEN {
                        c.need = 0;
                        return Ok(());
                    }
                    let g = Greeting::decode(&c.buf[c.start..c.start + GREETING_LEN]).map_err(|e| format!("greeting: {e}"))?;
                    let v = g.accept_downgrading(Mechanism::NULL).map_err(|e| format!("greeting: {e}"))?;
                    c.start += GREETING_LEN;
                    c.legacy = v.minor < 1;
                    c.phase = Phase::Handshake;
                    let ready = Command::Ready(Metadata::new().with_socket_type(Self::ours(side)))
                        .encode()
                        .map_err(|e| e.to_string())?;
                    self.send_ctl(side, id, ready);
                }
                Phase::Handshake => {
                    let Some((kind, body)) = self.next_frame(id)? else { return Ok(()) };
                    if kind != FrameKind::Command {
                        return Err("message before READY".into());
                    }
                    let md = match Command::decode(&body).map_err(|e| e.to_string())? {
                        Command::Ready(md) => md.socket_type(),
                        other => return Err(format!("expected READY, got {}", other.name())),
                    };
                    match md {
                        Some(t) if Self::ours(side).accepts(t) => {}
                        other => return Err(format!("incompatible peer socket type {other:?}")),
                    }
                    self.conn(id).phase = Phase::Traffic;
                    eprintln!("[custom iouring] {side:?} conn {id} ready");
                    if side == Side::Xsub {
                        let subs: Vec<Vec<u8>> = self.union.keys().cloned().collect();
                        for p in subs {
                            self.send_subscription(id, true, &p);
                        }
                    }
                }
                Phase::Traffic => {
                    let done = match side {
                        Side::Xsub => self.forward_one(id)?,
                        Side::Xpub => self.xpub_input(id)?,
                    };
                    if !done {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Takes one whole frame (handshake and XPUB peers: small) off the buffer.
    fn next_frame(&mut self, id: usize) -> Result<Option<(FrameKind, Vec<u8>)>, String> {
        let max = self.cfg.max_msg as u64;
        let c = self.conn(id);
        let avail = &c.buf[c.start..c.end];
        let (h, hl) = match frame::decode_header(avail, max) {
            Ok(x) => x,
            Err(e) if !e.is_violation() => return Ok(None),
            Err(e) => return Err(format!("frame: {e}")),
        };
        let total = hl + h.len as usize;
        if avail.len() < total {
            c.need = total - avail.len();
            return Ok(None);
        }
        let body = avail[hl..total].to_vec();
        c.start += total;
        c.need = 0;
        Ok(Some((h.kind, body)))
    }

    /// XSUB side: if one whole message (or a command) is in the buffer,
    /// forward it as-is to every matching XPUB peer and wait for the sends.
    fn forward_one(&mut self, id: usize) -> Result<bool, String> {
        let max = self.cfg.max_msg as u64;
        let c = self.conn(id);
        let base = c.start;
        let mut p = base;
        let mut first: Option<(usize, usize)> = None;
        loop {
            let (h, hl) = match frame::decode_header(&c.buf[p..c.end], max) {
                Ok(x) => x,
                Err(e) if !e.is_violation() => {
                    c.need = 0;
                    return Ok(false);
                }
                Err(e) => return Err(format!("frame: {e}")),
            };
            let len = h.len as usize;
            if p + hl + len > c.end {
                c.need = p + hl + len - c.end;
                if p + hl + len - base > self.cfg.max_msg {
                    return Err("message over --max-msg-mb".into());
                }
                return Ok(false);
            }
            match h.kind {
                FrameKind::Command if p == base => {
                    let body = c.buf[p + hl..p + hl + len].to_vec();
                    c.start = p + hl + len;
                    c.need = 0;
                    self.command(id, &body)?;
                    return Ok(true);
                }
                FrameKind::Command => return Err("command inside a multipart message".into()),
                FrameKind::Message { more } => {
                    if first.is_none() {
                        first = Some((p + hl, len));
                    }
                    p += hl + len;
                    if !more {
                        break;
                    }
                }
            }
        }
        // [base, p) is one complete message, exactly as it arrived.
        let (fo, fl) = first.expect("a message has a first frame");
        let ptr = unsafe { c.buf.as_ptr().add(base) };
        let topic: &[u8] = unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(fo), fl) };
        let targets: Vec<usize> = self
            .conns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.as_ref()
                    .filter(|c| c.side == Side::Xpub && c.phase == Phase::Traffic && !c.dead)
                    .filter(|c| c.prefixes.iter().any(|pre| topic.starts_with(pre)))
                    .map(|_| i)
            })
            .collect();
        self.stats.msgs += 1;
        self.stats.bytes += (p - base) as u64;
        self.send_sync(Side::Xpub, &targets, ptr, p - base, None);
        let Some(c) = self.conns[id].as_mut() else { return Ok(false) };
        c.start = p;
        c.need = 0;
        Ok(true)
    }

    /// XPUB side: subscriptions (commands or %x01/%x00 messages) and PINGs.
    fn xpub_input(&mut self, id: usize) -> Result<bool, String> {
        let Some((kind, body)) = self.next_frame(id)? else { return Ok(false) };
        match kind {
            FrameKind::Command => self.command(id, &body)?,
            FrameKind::Message { more: false } if !body.is_empty() && body[0] <= 1 => {
                self.subscription(id, body[0] == 1, &body[1..]);
            }
            FrameKind::Message { .. } => {} // XPUB peers' other messages are dropped
        }
        Ok(true)
    }

    fn command(&mut self, id: usize, body: &[u8]) -> Result<(), String> {
        let nlen = *body.first().ok_or("empty command")? as usize;
        let name = body.get(1..1 + nlen).ok_or("truncated command")?;
        let data = &body[1 + nlen..];
        let side = self.conn(id).side;
        match name {
            b"PING" => {
                // libzmq truncates an over-long context rather than failing.
                let ctx = data.get(2..).unwrap_or(&[]);
                let ctx = &ctx[..ctx.len().min(16)];
                let mut v = Vec::new();
                frame::encode_header(FrameKind::Command, (5 + ctx.len()) as u64, &mut v);
                v.extend_from_slice(b"\x04PONG");
                v.extend_from_slice(ctx);
                self.send_ctl(side, id, v);
            }
            b"SUBSCRIBE" if side == Side::Xpub => self.subscription(id, true, data),
            b"CANCEL" if side == Side::Xpub => self.subscription(id, false, data),
            b"ERROR" => return Err("peer sent ERROR".into()),
            _ => {} // unknown commands are ignored, as libzmq does
        }
        Ok(())
    }

    fn subscription(&mut self, id: usize, on: bool, prefix: &[u8]) {
        let c = self.conn(id);
        if on {
            c.prefixes.push(prefix.to_vec());
        } else if let Some(i) = c.prefixes.iter().position(|p| p == prefix) {
            c.prefixes.swap_remove(i);
        } else {
            return;
        }
        self.union_change(on, prefix);
    }

    fn union_change(&mut self, on: bool, prefix: &[u8]) {
        let n = self.union.entry(prefix.to_vec()).or_insert(0);
        let edge = if on {
            *n += 1;
            *n == 1
        } else {
            *n -= 1;
            *n == 0
        };
        if !on && *n == 0 {
            self.union.remove(prefix);
        }
        if edge {
            for x in self.peers(Side::Xsub) {
                self.send_subscription(x, on, prefix);
            }
        }
    }

    fn send_subscription(&mut self, id: usize, on: bool, prefix: &[u8]) {
        let mut v = Vec::new();
        if self.conn(id).legacy {
            frame::encode_header(FrameKind::Message { more: false }, (1 + prefix.len()) as u64, &mut v);
            v.push(u8::from(on));
        } else {
            let name: &[u8] = if on { b"\x09SUBSCRIBE" } else { b"\x06CANCEL" };
            frame::encode_header(FrameKind::Command, (name.len() + prefix.len()) as u64, &mut v);
            v.extend_from_slice(name);
        }
        v.extend_from_slice(prefix);
        self.send_ctl(Side::Xsub, id, v);
    }

    // ------------------------------------------------------------ loop

    fn run(&mut self) {
        let xsub_l = TcpListener::bind(&self.cfg.xsub).expect("bind XSUB").into_raw_fd();
        let xpub_l = TcpListener::bind(&self.cfg.xpub).expect("bind XPUB").into_raw_fd();
        eprintln!(
            "[custom iouring] XSUB {} (xsub iouring) -> XPUB {} (xpub iouring) | synchronous forwarding, zc={}",
            self.cfg.xsub, self.cfg.xpub, self.cfg.zc
        );
        self.post_accept(Side::Xsub, xsub_l);
        self.post_accept(Side::Xpub, xpub_l);
        let mut last = Instant::now();
        let (mut last_msgs, mut last_bytes) = (0, 0);
        loop {
            while let Some(ev) = self.events.pop_front() {
                match ev {
                    Event::Accepted(side, fd) => self.on_accept(side, fd),
                    Event::Received(id, res) => self.on_recv(id, res),
                }
            }
            self.watch_xpub_ring();
            self.xpub.ring.submit().expect("submit xpub");
            // Sleep on the xsub iouring; the xpub iouring wakes it through the
            // poll on its fd.
            self.xsub.ring.submit_and_wait(1).expect("submit_and_wait xsub");
            self.drain(Side::Xsub);
            self.drain(Side::Xpub);
            if last.elapsed() >= Duration::from_secs(1) {
                let dt = last.elapsed().as_secs_f64();
                eprintln!(
                    "[custom iouring] {:.0} msg/s {:.0} MB/s",
                    (self.stats.msgs - last_msgs) as f64 / dt,
                    (self.stats.bytes - last_bytes) as f64 / dt / 1e6
                );
                (last_msgs, last_bytes) = (self.stats.msgs, self.stats.bytes);
                last = Instant::now();
            }
        }
    }
}

fn main() {
    let cfg = parse_args();
    let mut app = App {
        cfg,
        xsub: Ring::new(),
        xpub: Ring::new(),
        conns: Vec::new(),
        union: HashMap::new(),
        events: VecDeque::new(),
        watch_armed: false,
        accept_addr: Box::new((unsafe { std::mem::zeroed() }, 0)),
        stats: Stats::default(),
    };
    app.run();
}
