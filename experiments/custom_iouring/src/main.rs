//! custom iouring (2 threads): an XSUB -> XPUB forwarder speaking ZMTP 3.1
//! (libzmq compatible), split across two threads that each own one io_uring:
//!
//! * the **receive thread** owns the **xsub iouring**: the XSUB listener, every
//!   XSUB connection, and one provided buffer ring shared by all of them. It
//!   receives, parses, and once a whole message sits in ring buffers it hands
//!   the publish thread a *descriptor*: the (buffer id, offset, length) list
//!   covering the message's bytes exactly as they arrived. No message bytes
//!   are copied or moved between threads.
//! * the **publish thread** owns the **xpub iouring**: the XPUB listener, every
//!   XPUB connection, and their subscriptions. It takes a descriptor, matches
//!   the topic (read in place from the ring buffer), and sends those same
//!   buffers to every matching XPUB peer. When all of those sends have
//!   completed it hands the descriptor back, and the receive thread (the
//!   buffer ring's only owner) returns the buffers to the ring.
//!
//! The two threads talk through two lock-free single-producer/single-consumer
//! queues (forward: receive -> publish, done: publish -> receive) that carry
//! only descriptors. Each thread sleeps in its own ring. After a push, the
//! pusher posts an IORING_OP_MSG_RING "doorbell" completion into the other
//! thread's ring, so nobody spins or polls.
//!
//! `--depth N` (default 1) is the forward queue's capacity: how many complete
//! messages the receive thread may hold ready while the publish thread is
//! still sending an earlier one. While the queue is full the receive thread
//! posts no recv, so TCP pushes back on the XSUB peers. Memory is bounded by
//! the buffer ring (`--ring-entries` x `--ring-buf-kb`).

use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::mem::MaybeUninit;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::sync::atomic::{fence, AtomicU16, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use io_uring::{cqueue, opcode, squeue, types, IoUring};
use weida_zmtp::{frame, Command, FrameKind, Greeting, Mechanism, Metadata, SocketType, GREETING_LEN};

// ---------------------------------------------------------------- config

#[derive(Clone)]
struct Config {
    xsub: String,
    xpub: String,
    depth: usize,
    ring_buf: usize,
    ring_entries: u16,
    zc: bool,
    cpus: Option<(usize, usize)>,
}

fn parse_args() -> Config {
    let mut c = Config {
        xsub: "127.0.0.1:5555".into(),
        xpub: "127.0.0.1:5556".into(),
        depth: 1,
        ring_buf: 256 << 10,
        ring_entries: 512,
        zc: false,
        cpus: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--xsub" => c.xsub = v,
            "--xpub" => c.xpub = v,
            "--depth" => c.depth = v.parse::<usize>().expect("depth").max(1),
            "--ring-buf-kb" => c.ring_buf = v.parse::<usize>().expect("KB") << 10,
            "--ring-entries" => c.ring_entries = v.parse().expect("power of two"),
            "--zc" => c.zc = v == "on",
            "--cpus" => {
                let (a, b) = v.split_once(',').expect("--cpus RECV,PUBLISH");
                c.cpus = Some((a.parse().expect("cpu"), b.parse().expect("cpu")));
            }
            other => panic!(
                "unknown argument {other} (--xsub A --xpub A [--depth N] [--ring-buf-kb K] [--ring-entries N] [--zc on] [--cpus R,P])"
            ),
        }
        i += 2;
    }
    assert!(c.ring_entries.is_power_of_two(), "--ring-entries must be a power of two");
    c
}

fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

// ---------------------------------------------------------------- SPSC queue

#[repr(align(64))]
struct CachePadded<T>(T);

/// Lock-free bounded single-producer/single-consumer queue.
struct Spsc<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    head: CachePadded<AtomicUsize>, // next slot to pop (consumer)
    tail: CachePadded<AtomicUsize>, // next slot to push (producer)
}

unsafe impl<T: Send> Send for Spsc<T> {}
unsafe impl<T: Send> Sync for Spsc<T> {}

impl<T> Spsc<T> {
    fn new(cap: usize) -> Spsc<T> {
        Spsc {
            slots: (0..cap).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect(),
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
        }
    }
    /// Producer side only.
    fn is_full(&self) -> bool {
        self.tail.0.load(Ordering::Relaxed) - self.head.0.load(Ordering::Acquire) == self.slots.len()
    }
    /// Producer side only.
    fn push(&self, v: T) -> Result<(), T> {
        let t = self.tail.0.load(Ordering::Relaxed);
        if t - self.head.0.load(Ordering::Acquire) == self.slots.len() {
            return Err(v);
        }
        unsafe { (*self.slots[t % self.slots.len()].get()).write(v) };
        self.tail.0.store(t + 1, Ordering::Release);
        Ok(())
    }
    /// Either side: number of queued items (exact for the caller's own view).
    fn len(&self) -> usize {
        let h = self.head.0.load(Ordering::Acquire);
        self.tail.0.load(Ordering::Acquire) - h
    }
    /// Consumer side only.
    fn pop(&self) -> Option<T> {
        let h = self.head.0.load(Ordering::Relaxed);
        if h == self.tail.0.load(Ordering::Acquire) {
            return None;
        }
        let v = unsafe { (*self.slots[h % self.slots.len()].get()).assume_init_read() };
        self.head.0.store(h + 1, Ordering::Release);
        Some(v)
    }
}

// ---------------------------------------------------------------- descriptors

const MAX_SEGS: usize = 64;

/// A slice of one provided-ring buffer.
#[derive(Clone, Copy, Default)]
struct Seg {
    bid: u16,
    off: u32,
    len: u32,
}

/// One whole message, as the ring-buffer slices holding its original bytes.
#[derive(Clone, Copy)]
struct Desc {
    n: usize,
    len: usize,
    segs: [Seg; MAX_SEGS],
}

struct Shared {
    fwd: Spsc<Desc>,
    done: Spsc<Desc>,
    /// Base address and buffer size of the provided buffer ring (set by the
    /// receive thread before it publishes any descriptor).
    base: AtomicUsize,
    buf_size: usize,
    /// Doorbell suppression: a thread advertises that it is about to sleep
    /// (and why) before its final re-check; the other thread only rings when
    /// the flag says a wake-up is needed. Both sides use SeqCst fences, so a
    /// push racing with going to sleep is never missed.
    recv_sleep: AtomicU8,
    publish_sleep: AtomicU8,
    depth: usize,
}

const AWAKE: u8 = 0;
/// Receive thread sleeps; needs waking when the forward queue has room.
const WAIT_ROOM: u8 = 1;
/// Receive thread sleeps with no free ring buffers; needs waking on done.
const WAIT_BUFFERS: u8 = 2;
/// Receive thread sleeps waiting only for its sockets.
const WAIT_IO: u8 = 3;
/// Publish thread sleeps; needs waking when the forward queue is non-empty.
const WAIT_WORK: u8 = 1;

impl Shared {
    fn seg_ptr(&self, s: &Seg) -> *mut u8 {
        (self.base.load(Ordering::Relaxed) + usize::from(s.bid) * self.buf_size + s.off as usize) as *mut u8
    }
    /// Copies up to `n` bytes of the message starting at `at`.
    fn copy_out(&self, d: &Desc, mut at: usize, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        for s in &d.segs[..d.n] {
            let len = s.len as usize;
            if at >= len {
                at -= len;
                continue;
            }
            let k = (len - at).min(n - out.len());
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(self.seg_ptr(s).add(at), k) });
            at = 0;
            if out.len() == n {
                break;
            }
        }
        out
    }
}

// ---------------------------------------------------------------- rings

const DOORBELL: u64 = u64::MAX - 1;
const DOORBELL_SRC: u64 = u64::MAX - 2;
const IORING_SEND_ZC_REPORT_USAGE: u16 = 1 << 3;

enum OpKind {
    Accept { listener: RawFd },
    Recv,
    Send { _ctl: Option<Vec<u8>>, _iov: Vec<libc::iovec>, _msg: Box<libc::msghdr>, len: usize },
}

struct Op {
    kind: OpKind,
    conn: usize,
    done: Option<i32>,
    flags: u32,
    notif_pending: bool,
}

enum Event {
    Accepted(i32),
    Received(usize, i32, u32),
    Doorbell,
}

/// One io_uring plus its operation table. Completions of sends are recorded
/// for their (synchronous) waiter; everything else becomes an event.
struct Ring {
    ring: IoUring,
    ops: Vec<Option<Op>>,
    free: Vec<usize>,
    events: VecDeque<Event>,
    peer_fd: RawFd,
    accept_addr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
}

impl Ring {
    fn new() -> Ring {
        Ring {
            ring: IoUring::builder().setup_single_issuer().build(1024).expect("io_uring"),
            ops: Vec::new(),
            free: Vec::new(),
            events: VecDeque::new(),
            peer_fd: -1,
            accept_addr: Box::new((unsafe { std::mem::zeroed() }, 0)),
        }
    }
    fn add(&mut self, op: Op) -> u64 {
        match self.free.pop() {
            Some(i) => {
                self.ops[i] = Some(op);
                i as u64
            }
            None => {
                self.ops.push(Some(op));
                (self.ops.len() - 1) as u64
            }
        }
    }
    fn push(&mut self, sqe: squeue::Entry) {
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
    fn drain(&mut self) {
        let cqes: Vec<(u64, i32, u32)> = self.ring.completion().map(|e| (e.user_data(), e.result(), e.flags())).collect();
        for (ud, res, flags) in cqes {
            if ud == DOORBELL {
                self.events.push_back(Event::Doorbell);
                continue;
            }
            if ud == DOORBELL_SRC {
                continue; // only posted if the MSG_RING itself failed
            }
            if cqueue::notif(flags) {
                self.ops[ud as usize].as_mut().expect("send").notif_pending = false;
                continue;
            }
            let op = self.ops[ud as usize].as_mut().expect("op");
            match op.kind {
                OpKind::Send { .. } => {
                    op.done = Some(res);
                    op.notif_pending = cqueue::more(flags);
                }
                OpKind::Accept { listener } => {
                    self.release(ud);
                    self.events.push_back(Event::Accepted(res));
                    self.post_accept(listener);
                }
                OpKind::Recv => {
                    let (conn, f) = (op.conn, flags);
                    self.release(ud);
                    self.events.push_back(Event::Received(conn, res, f));
                }
            }
        }
    }
    fn post_accept(&mut self, listener: RawFd) {
        let a = &mut *self.accept_addr;
        a.1 = std::mem::size_of::<libc::sockaddr_storage>() as _;
        let (addr, len) = (&mut a.0 as *mut _ as *mut libc::sockaddr, &mut a.1 as *mut libc::socklen_t);
        let ud = self.add(Op { kind: OpKind::Accept { listener }, conn: usize::MAX, done: None, flags: 0, notif_pending: false });
        self.push(opcode::Accept::new(types::Fd(listener), addr, len).build().user_data(ud));
    }
    /// Posts a completion into the other thread's ring to wake it.
    fn doorbell(&mut self) {
        // SKIP_SUCCESS drops the CQE on *this* ring; the target ring still
        // gets its DOORBELL completion.
        let sqe = opcode::MsgRingData::new(types::Fd(self.peer_fd), 0, DOORBELL, None)
            .build()
            .flags(squeue::Flags::SKIP_SUCCESS)
            .user_data(DOORBELL_SRC);
        self.push(sqe);
        self.ring.submit().expect("submit doorbell");
    }
    /// Sends `iov` to each fd and waits until every send has completed (and,
    /// for zero-copy, been released). Returns (conn, ok) per destination.
    fn send_sync(&mut self, to: &[(usize, RawFd)], iov: &[libc::iovec], len: usize, zc: bool, ctl: Option<Vec<u8>>) -> Vec<(usize, bool)> {
        let mut uds = Vec::with_capacity(to.len());
        let mut ctl = ctl;
        for (i, &(conn, fd)) in to.iter().enumerate() {
            let mut iov = iov.to_vec();
            let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = iov.len();
            let flags = (libc::MSG_NOSIGNAL | libc::MSG_WAITALL) as u32;
            let sqe = if zc {
                opcode::SendMsgZc::new(types::Fd(fd), &*msg).ioprio(IORING_SEND_ZC_REPORT_USAGE).flags(flags).build()
            } else {
                opcode::SendMsg::new(types::Fd(fd), &*msg).flags(flags).build()
            };
            let keep = if i + 1 == to.len() { ctl.take() } else { None };
            let ud = self.add(Op {
                kind: OpKind::Send { _ctl: keep, _iov: iov, _msg: msg, len },
                conn,
                done: None,
                flags: 0,
                notif_pending: false,
            });
            self.push(sqe.user_data(ud));
            uds.push(ud);
        }
        while uds.iter().any(|&ud| {
            let op = self.ops[ud as usize].as_ref().expect("op");
            op.done.is_none() || op.notif_pending
        }) {
            self.ring.submit_and_wait(1).expect("submit_and_wait");
            self.drain();
        }
        uds.into_iter()
            .map(|ud| {
                let op = self.release(ud);
                let OpKind::Send { len, .. } = op.kind else { unreachable!() };
                let res = op.done.expect("done");
                let _ = op.flags;
                (op.conn, res >= 0 && res as usize == len)
            })
            .collect()
    }
    fn send_ctl(&mut self, conn: usize, fd: RawFd, bytes: Vec<u8>) -> bool {
        let iov = [libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() }];
        let len = bytes.len();
        self.send_sync(&[(conn, fd)], &iov, len, false, Some(bytes))[0].1
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Greeting,
    Handshake,
    Traffic,
}

fn pong(data: &[u8]) -> Vec<u8> {
    // libzmq truncates an over-long PING context rather than failing.
    let ctx = data.get(2..).unwrap_or(&[]);
    let ctx = &ctx[..ctx.len().min(16)];
    let mut v = Vec::new();
    frame::encode_header(FrameKind::Command, (5 + ctx.len()) as u64, &mut v);
    v.extend_from_slice(b"\x04PONG");
    v.extend_from_slice(ctx);
    v
}

fn ready(ours: SocketType) -> Vec<u8> {
    Command::Ready(Metadata::new().with_socket_type(ours)).encode().expect("READY")
}

fn command_name(body: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let n = *body.first().ok_or("empty command")? as usize;
    let name = body.get(1..1 + n).ok_or("truncated command")?;
    Ok((name, &body[1 + n..]))
}

// ---------------------------------------------------------------- receive thread

struct XsubConn {
    fd: RawFd,
    phase: Phase,
    legacy: bool,
    dead: bool,
    ops: usize,
    recv_armed: bool,
    starved: bool,
    /// Received bytes not yet consumed, as ring-buffer slices.
    rx: VecDeque<Seg>,
    rx_avail: usize,
}

struct ReceiveThread {
    cfg: Config,
    sh: Arc<Shared>,
    ring: Ring,
    conns: Vec<Option<XsubConn>>,
    /// Provided buffer ring: entries, tail, and a reference count per buffer
    /// (1 while it is in some conn's rx, +1 per descriptor that uses it).
    entries: *mut types::BufRingEntry,
    tail: u16,
    refs: Vec<u32>,
    free_bufs: usize,
    /// Subscriptions from the publish thread, as (on, prefix).
    ctl: mpsc::Receiver<(bool, Vec<u8>)>,
    union: HashMap<Vec<u8>, usize>,
    msgs: u64,
    bytes: u64,
    max_msg: usize,
    /// Parsing stopped because the forward queue was full.
    blocked: bool,
    doorbells: u64,
}

impl ReceiveThread {
    fn setup_buf_ring(&mut self) {
        let n = usize::from(self.cfg.ring_entries);
        let bytes = (n * std::mem::size_of::<types::BufRingEntry>()).next_multiple_of(4096);
        let entries = unsafe {
            libc::mmap(std::ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANONYMOUS | libc::MAP_PRIVATE, -1, 0)
        };
        assert!(entries != libc::MAP_FAILED, "mmap");
        let base = Box::leak(vec![0u8; n * self.cfg.ring_buf].into_boxed_slice()).as_mut_ptr();
        self.sh.base.store(base as usize, Ordering::Release);
        unsafe { self.ring.ring.submitter().register_buf_ring_with_flags(entries as u64, n as u16, 0, 0) }.expect("register_buf_ring");
        self.entries = entries as *mut types::BufRingEntry;
        self.refs = vec![0; n];
        for bid in 0..n {
            self.give(bid as u16);
        }
    }

    fn give(&mut self, bid: u16) {
        let mask = self.cfg.ring_entries - 1;
        let e = unsafe { &mut *self.entries.add(usize::from(self.tail & mask)) };
        e.set_addr(self.sh.base.load(Ordering::Relaxed) as u64 + u64::from(bid) * self.cfg.ring_buf as u64);
        e.set_len(self.cfg.ring_buf as u32);
        e.set_bid(bid);
        self.tail = self.tail.wrapping_add(1);
        let tail = unsafe { types::BufRingEntry::tail(self.entries) } as *const AtomicU16;
        unsafe { (*tail).store(self.tail, Ordering::Release) };
        self.free_bufs += 1;
    }

    fn unref(&mut self, bid: u16) {
        let r = &mut self.refs[usize::from(bid)];
        *r -= 1;
        if *r == 0 {
            self.give(bid);
        }
    }

    fn conn(&mut self, id: usize) -> &mut XsubConn {
        self.conns[id].as_mut().expect("conn")
    }

    fn run(mut self, listener: RawFd) {
        self.setup_buf_ring();
        self.ring.post_accept(listener);
        let mut last = Instant::now();
        let (mut lm, mut lb) = (0, 0);
        loop {
            while let Some(ev) = self.ring.events.pop_front() {
                match ev {
                    Event::Accepted(fd) => self.on_accept(fd),
                    Event::Received(id, res, flags) => self.on_recv(id, res, flags),
                    Event::Doorbell => self.on_doorbell(),
                }
            }
            self.drain_done();
            // About to sleep: say why, then re-check so a racing push or pop
            // by the publish thread cannot be missed.
            let starved = self.conns.iter().flatten().any(|c| c.starved && !c.dead);
            let why = if starved {
                WAIT_BUFFERS
            } else if self.blocked {
                WAIT_ROOM
            } else {
                WAIT_IO
            };
            self.sh.recv_sleep.store(why, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let room = self.blocked && self.sh.fwd.len() <= self.sh.depth / 2;
            let returned = self.sh.done.len() > 0;
            if room || (starved && returned) {
                self.sh.recv_sleep.store(AWAKE, Ordering::SeqCst);
                self.on_doorbell();
                continue;
            }
            self.ring.ring.submit_and_wait(1).expect("submit_and_wait");
            self.sh.recv_sleep.store(AWAKE, Ordering::SeqCst);
            self.ring.drain();
            if last.elapsed() >= Duration::from_secs(1) {
                let dt = last.elapsed().as_secs_f64();
                eprintln!(
                    "[custom iouring] {:.0} msg/s {:.0} MB/s | free ring buffers {} | doorbells sent by receive thread {}",
                    (self.msgs - lm) as f64 / dt,
                    (self.bytes - lb) as f64 / dt / 1e6,
                    self.free_bufs,
                    self.doorbells
                );
                (lm, lb, last) = (self.msgs, self.bytes, Instant::now());
            }
        }
    }

    fn on_accept(&mut self, fd: i32) {
        if fd < 0 {
            return;
        }
        let one: libc::c_int = 1;
        unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &one as *const _ as *const _, 4) };
        let c = XsubConn { fd, phase: Phase::Greeting, legacy: false, dead: false, ops: 0, recv_armed: false, starved: false, rx: VecDeque::new(), rx_avail: 0 };
        let id = match self.conns.iter().position(Option::is_none) {
            Some(i) => {
                self.conns[i] = Some(c);
                i
            }
            None => {
                self.conns.push(Some(c));
                self.conns.len() - 1
            }
        };
        eprintln!("[custom iouring] XSUB peer connected (conn {id})");
        self.send_ctl(id, Greeting::null().encode().to_vec());
        self.arm(id);
    }

    fn send_ctl(&mut self, id: usize, bytes: Vec<u8>) {
        let fd = self.conn(id).fd;
        if !self.ring.send_ctl(id, fd, bytes) {
            self.kill(id, "send failed");
        }
    }

    /// Posts a recv unless one is armed, or the forward queue is full (then
    /// TCP pushes back until the publish thread catches up).
    fn arm(&mut self, id: usize) {
        let full = self.sh.fwd.is_full();
        let size = self.cfg.ring_buf;
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead || c.recv_armed || c.starved || (c.phase == Phase::Traffic && full) {
            return;
        }
        c.recv_armed = true;
        c.ops += 1;
        let fd = c.fd;
        let ud = self.ring.add(Op { kind: OpKind::Recv, conn: id, done: None, flags: 0, notif_pending: false });
        let sqe = opcode::Recv::new(types::Fd(fd), std::ptr::null_mut(), size as u32)
            .buf_group(0)
            .build()
            .flags(squeue::Flags::BUFFER_SELECT)
            .user_data(ud);
        self.ring.push(sqe);
    }

    fn on_recv(&mut self, id: usize, res: i32, flags: u32) {
        let bid = cqueue::buffer_select(flags);
        if let Some(b) = bid {
            self.free_bufs -= 1;
            self.refs[usize::from(b)] = 1;
        }
        let c = self.conn(id);
        c.ops -= 1;
        c.recv_armed = false;
        if c.dead || res <= 0 {
            if let Some(b) = bid {
                self.unref(b);
            }
            if self.conn(id).dead {
                return self.reap(id);
            }
            if res == -libc::ENOBUFS {
                // Every buffer is in use; re-armed when descriptors come back.
                self.conn(id).starved = true;
                return;
            }
            return self.kill(id, if res == 0 { "peer closed" } else { "recv error" });
        }
        let b = bid.expect("recv with buffer select carries a buffer id");
        let c = self.conn(id);
        c.rx.push_back(Seg { bid: b, off: 0, len: res as u32 });
        c.rx_avail += res as usize;
        if let Err(e) = self.parse(id) {
            return self.kill(id, &e);
        }
        self.arm(id);
    }

    /// Descriptors whose sends have all completed: release their buffers.
    /// Called on every loop iteration and before every push, so at most
    /// depth + 1 descriptors are ever waiting in the done queue (its capacity
    /// is depth + 2) and the publish thread's push never has to wait.
    fn drain_done(&mut self) {
        while let Some(d) = self.sh.done.pop() {
            for s in &d.segs[..d.n] {
                self.unref(s.bid);
            }
        }
    }

    fn on_doorbell(&mut self) {
        self.blocked = false;
        self.drain_done();
        // Subscription changes from XPUB peers.
        while let Ok((on, prefix)) = self.ctl.try_recv() {
            self.union_change(on, &prefix);
        }
        // Room in the forward queue / buffers back: resume parsing and recvs.
        for id in 0..self.conns.len() {
            if self.conns[id].as_ref().is_none_or(|c| c.dead) {
                continue;
            }
            if self.free_bufs > 0 {
                self.conn(id).starved = false;
            }
            if let Err(e) = self.parse(id) {
                self.kill(id, &e);
                continue;
            }
            self.arm(id);
        }
    }

    /// Copies `n` unparsed bytes starting at offset `at`.
    fn rx_copy(&self, id: usize, mut at: usize, n: usize) -> Vec<u8> {
        let c = self.conns[id].as_ref().expect("conn");
        let mut out = Vec::with_capacity(n);
        for s in &c.rx {
            let len = s.len as usize;
            if at >= len {
                at -= len;
                continue;
            }
            let k = (len - at).min(n - out.len());
            let p = self.sh.seg_ptr(&Seg { bid: s.bid, off: s.off + at as u32, len: k as u32 });
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, k) });
            at = 0;
            if out.len() == n {
                break;
            }
        }
        out
    }

    /// Removes the first `n` unparsed bytes; with `desc`, records them as a
    /// descriptor (taking a reference on each buffer).
    fn rx_take(&mut self, id: usize, mut n: usize, mut desc: Option<&mut Desc>) -> Result<(), String> {
        let mut released = Vec::new();
        let mut taken = Vec::new();
        {
            let c = self.conns[id].as_mut().expect("conn");
            c.rx_avail -= n;
            while n > 0 {
                let s = c.rx.front_mut().expect("rx_avail covers n");
                let k = (s.len as usize).min(n);
                taken.push(Seg { bid: s.bid, off: s.off, len: k as u32 });
                if k == s.len as usize {
                    released.push(s.bid);
                    c.rx.pop_front();
                } else {
                    s.off += k as u32;
                    s.len -= k as u32;
                }
                n -= k;
            }
        }
        if let Some(d) = desc.as_deref_mut() {
            if taken.len() > MAX_SEGS {
                return Err(format!("message spans {} ring buffers (max {MAX_SEGS})", taken.len()));
            }
            for s in &taken {
                self.refs[usize::from(s.bid)] += 1;
                d.segs[d.n] = *s;
                d.n += 1;
                d.len += s.len as usize;
            }
        }
        for b in released {
            self.unref(b);
        }
        Ok(())
    }

    fn parse(&mut self, id: usize) -> Result<(), String> {
        loop {
            let Some(c) = self.conns[id].as_ref() else { return Ok(()) };
            if c.dead {
                return Ok(());
            }
            let (phase, avail) = (c.phase, c.rx_avail);
            match phase {
                Phase::Greeting => {
                    if avail < GREETING_LEN {
                        return Ok(());
                    }
                    let raw = self.rx_copy(id, 0, GREETING_LEN);
                    self.rx_take(id, GREETING_LEN, None)?;
                    let g = Greeting::decode(&raw).map_err(|e| format!("greeting: {e}"))?;
                    let v = g.accept_downgrading(Mechanism::NULL).map_err(|e| format!("greeting: {e}"))?;
                    let c = self.conn(id);
                    c.legacy = v.minor < 1;
                    c.phase = Phase::Handshake;
                    self.send_ctl(id, ready(SocketType::XSub));
                }
                Phase::Handshake | Phase::Traffic => {
                    if phase == Phase::Traffic && self.sh.fwd.is_full() {
                        self.blocked = true;
                        return Ok(()); // resumed by the next doorbell
                    }
                    // Walk frames until one whole message (or a command) is here.
                    let mut at = 0;
                    let mut is_command = false;
                    loop {
                        let head = self.rx_copy(id, at, (avail - at).min(9));
                        let (h, hl) = match frame::decode_header(&head, self.max_msg as u64) {
                            Ok(x) => x,
                            Err(e) if !e.is_violation() => return Ok(()),
                            Err(e) => return Err(format!("frame: {e}")),
                        };
                        let end = at + hl + h.len as usize;
                        if end > avail {
                            if end > self.max_msg {
                                return Err("message larger than the buffer ring allows".into());
                            }
                            return Ok(());
                        }
                        match h.kind {
                            FrameKind::Command if at == 0 => {
                                is_command = true;
                                at = end;
                                break;
                            }
                            FrameKind::Command => return Err("command inside a multipart message".into()),
                            FrameKind::Message { .. } if phase == Phase::Handshake => return Err("message before READY".into()),
                            FrameKind::Message { more } => {
                                at = end;
                                if !more {
                                    break;
                                }
                            }
                        }
                    }
                    if is_command {
                        let whole = self.rx_copy(id, 0, at);
                        self.rx_take(id, at, None)?;
                        let (_, hl) = frame::decode_header(&whole, self.max_msg as u64).map_err(|e| e.to_string())?;
                        self.command(id, &whole[hl..])?;
                        continue;
                    }
                    self.drain_done();
                    let mut d = Desc { n: 0, len: 0, segs: [Seg::default(); MAX_SEGS] };
                    self.rx_take(id, at, Some(&mut d))?;
                    self.msgs += 1;
                    self.bytes += d.len as u64;
                    if self.sh.fwd.push(d).is_err() {
                        unreachable!("checked not full; single producer");
                    }
                    fence(Ordering::SeqCst);
                    if self.sh.publish_sleep.load(Ordering::SeqCst) == WAIT_WORK {
                        self.sh.publish_sleep.store(AWAKE, Ordering::SeqCst);
                        self.ring.doorbell();
                        self.doorbells += 1;
                    }
                }
            }
        }
    }

    fn command(&mut self, id: usize, body: &[u8]) -> Result<(), String> {
        let (name, data) = command_name(body)?;
        if self.conn(id).phase == Phase::Handshake {
            let t = match Command::decode(body).map_err(|e| e.to_string())? {
                Command::Ready(md) => md.socket_type(),
                other => return Err(format!("expected READY, got {}", other.name())),
            };
            match t {
                Some(t) if SocketType::XSub.accepts(t) => {}
                other => return Err(format!("incompatible peer socket type {other:?}")),
            }
            self.conn(id).phase = Phase::Traffic;
            eprintln!("[custom iouring] XSUB conn {id} ready");
            let subs: Vec<Vec<u8>> = self.union.keys().cloned().collect();
            for p in subs {
                self.send_subscription(id, true, &p);
            }
            return Ok(());
        }
        match name {
            b"PING" => self.send_ctl(id, pong(data)),
            b"ERROR" => return Err("peer sent ERROR".into()),
            _ => {} // unknown commands are ignored, as libzmq does
        }
        Ok(())
    }

    fn union_change(&mut self, on: bool, prefix: &[u8]) {
        let n = self.union.entry(prefix.to_vec()).or_insert(0);
        let edge = if on {
            *n += 1;
            *n == 1
        } else {
            *n = n.saturating_sub(1);
            *n == 0
        };
        if !on && *n == 0 {
            self.union.remove(prefix);
        }
        if edge {
            let ids: Vec<usize> = (0..self.conns.len())
                .filter(|&i| self.conns[i].as_ref().is_some_and(|c| c.phase == Phase::Traffic && !c.dead))
                .collect();
            for id in ids {
                self.send_subscription(id, on, prefix);
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
        self.send_ctl(id, v);
    }

    fn kill(&mut self, id: usize, why: &str) {
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead {
            return;
        }
        c.dead = true;
        unsafe { libc::shutdown(c.fd, libc::SHUT_RDWR) };
        eprintln!("[custom iouring] XSUB conn {id} closed: {why}");
        let rx: Vec<Seg> = c.rx.drain(..).collect();
        c.rx_avail = 0;
        for s in rx {
            self.unref(s.bid);
        }
        self.reap(id);
    }

    fn reap(&mut self, id: usize) {
        if self.conns[id].as_ref().is_some_and(|c| c.dead && c.ops == 0) {
            let c = self.conns[id].take().expect("conn");
            unsafe { libc::close(c.fd) };
        }
    }
}

// ---------------------------------------------------------------- publish thread

struct XpubConn {
    fd: RawFd,
    phase: Phase,
    dead: bool,
    ops: usize,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    prefixes: Vec<Vec<u8>>,
}

struct PublishThread {
    cfg: Config,
    sh: Arc<Shared>,
    ring: Ring,
    conns: Vec<Option<XpubConn>>,
    ctl: mpsc::Sender<(bool, Vec<u8>)>,
}

impl PublishThread {
    fn conn(&mut self, id: usize) -> &mut XpubConn {
        self.conns[id].as_mut().expect("conn")
    }

    fn run(mut self, listener: RawFd) {
        self.ring.post_accept(listener);
        loop {
            while let Some(ev) = self.ring.events.pop_front() {
                match ev {
                    Event::Accepted(fd) => self.on_accept(fd),
                    Event::Received(id, res, _) => self.on_recv(id, res),
                    Event::Doorbell => {}
                }
            }
            // Publish everything the receive thread has handed over.
            let mut handed_back = false;
            while let Some(d) = self.sh.fwd.pop() {
                // A slot just freed up: if the receive thread is waiting for
                // room (and the queue has drained to half), wake it so it
                // fetches the next message while this one is being sent.
                fence(Ordering::SeqCst);
                if self.sh.recv_sleep.load(Ordering::SeqCst) == WAIT_ROOM && self.sh.fwd.len() <= self.sh.depth / 2 {
                    self.sh.recv_sleep.store(AWAKE, Ordering::SeqCst);
                    self.ring.doorbell();
                }
                self.publish(&d);
                let mut d = d;
                while let Err(back) = self.sh.done.push(d) {
                    d = back;
                    std::hint::spin_loop();
                }
                handed_back = true;
                fence(Ordering::SeqCst);
                if self.sh.recv_sleep.load(Ordering::SeqCst) == WAIT_BUFFERS {
                    self.sh.recv_sleep.store(AWAKE, Ordering::SeqCst);
                    self.ring.doorbell();
                }
            }
            if handed_back || !self.ring.events.is_empty() {
                continue;
            }
            self.sh.publish_sleep.store(WAIT_WORK, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if self.sh.fwd.len() > 0 {
                self.sh.publish_sleep.store(AWAKE, Ordering::SeqCst);
                continue;
            }
            self.ring.ring.submit_and_wait(1).expect("submit_and_wait");
            self.sh.publish_sleep.store(AWAKE, Ordering::SeqCst);
            self.ring.drain();
        }
    }

    /// Sends one message's original bytes, straight from the ring buffers,
    /// to every XPUB peer whose subscriptions match its first frame, and
    /// returns once all of those sends have completed.
    fn publish(&mut self, d: &Desc) {
        let head = self.sh.copy_out(d, 0, d.len.min(9));
        let Ok((h, hl)) = frame::decode_header(&head, u64::MAX) else { return };
        let topic = self.sh.copy_out(d, hl, (h.len as usize).min(256));
        let to: Vec<(usize, RawFd)> = self
            .conns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.as_ref()
                    .filter(|c| c.phase == Phase::Traffic && !c.dead && c.prefixes.iter().any(|p| topic.starts_with(p)))
                    .map(|c| (i, c.fd))
            })
            .collect();
        if to.is_empty() {
            return;
        }
        let iov: Vec<libc::iovec> =
            d.segs[..d.n].iter().map(|s| libc::iovec { iov_base: self.sh.seg_ptr(s) as *mut _, iov_len: s.len as usize }).collect();
        for &(id, _) in &to {
            self.conn(id).ops += 1;
        }
        let zc = self.cfg.zc;
        for (id, ok) in self.ring.send_sync(&to, &iov, d.len, zc, None) {
            self.conn(id).ops -= 1;
            if !ok {
                self.kill(id, "send failed");
            }
            self.reap(id);
        }
    }

    fn on_accept(&mut self, fd: i32) {
        if fd < 0 {
            return;
        }
        let one: libc::c_int = 1;
        unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &one as *const _ as *const _, 4) };
        let c = XpubConn { fd, phase: Phase::Greeting, dead: false, ops: 0, buf: vec![0; 64 << 10], start: 0, end: 0, prefixes: Vec::new() };
        let id = match self.conns.iter().position(Option::is_none) {
            Some(i) => {
                self.conns[i] = Some(c);
                i
            }
            None => {
                self.conns.push(Some(c));
                self.conns.len() - 1
            }
        };
        eprintln!("[custom iouring] XPUB peer connected (conn {id})");
        self.send_ctl(id, Greeting::null().encode().to_vec());
        self.arm(id);
    }

    fn send_ctl(&mut self, id: usize, bytes: Vec<u8>) {
        let fd = self.conn(id).fd;
        self.conn(id).ops += 1;
        let ok = self.ring.send_ctl(id, fd, bytes);
        self.conn(id).ops -= 1;
        if !ok {
            self.kill(id, "send failed");
        }
    }

    fn arm(&mut self, id: usize) {
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead {
            return;
        }
        if c.start == c.end {
            (c.start, c.end) = (0, 0);
        } else if c.start > 0 {
            c.buf.copy_within(c.start..c.end, 0);
            c.end -= c.start;
            c.start = 0;
        }
        if c.end == c.buf.len() {
            let n = c.buf.len() * 2;
            c.buf.resize(n, 0);
        }
        let (fd, ptr, len) = (c.fd, unsafe { c.buf.as_mut_ptr().add(c.end) }, c.buf.len() - c.end);
        c.ops += 1;
        let ud = self.ring.add(Op { kind: OpKind::Recv, conn: id, done: None, flags: 0, notif_pending: false });
        self.ring.push(opcode::Recv::new(types::Fd(fd), ptr, len as u32).build().user_data(ud));
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
        self.arm(id);
        self.reap(id);
    }

    fn parse(&mut self, id: usize) -> Result<(), String> {
        loop {
            let Some(c) = self.conns[id].as_mut() else { return Ok(()) };
            if c.dead {
                return Ok(());
            }
            if c.phase == Phase::Greeting {
                if c.end - c.start < GREETING_LEN {
                    return Ok(());
                }
                let g = Greeting::decode(&c.buf[c.start..c.start + GREETING_LEN]).map_err(|e| format!("greeting: {e}"))?;
                g.accept_downgrading(Mechanism::NULL).map_err(|e| format!("greeting: {e}"))?;
                c.start += GREETING_LEN;
                c.phase = Phase::Handshake;
                self.send_ctl(id, ready(SocketType::XPub));
                continue;
            }
            let (h, hl) = match frame::decode_header(&c.buf[c.start..c.end], 1 << 20) {
                Ok(x) => x,
                Err(e) if !e.is_violation() => return Ok(()),
                Err(e) => return Err(format!("frame: {e}")),
            };
            let total = hl + h.len as usize;
            if c.end - c.start < total {
                return Ok(());
            }
            let body = c.buf[c.start + hl..c.start + total].to_vec();
            c.start += total;
            let phase = c.phase;
            match (phase, h.kind) {
                (Phase::Handshake, FrameKind::Command) => {
                    let t = match Command::decode(&body).map_err(|e| e.to_string())? {
                        Command::Ready(md) => md.socket_type(),
                        other => return Err(format!("expected READY, got {}", other.name())),
                    };
                    match t {
                        Some(t) if SocketType::XPub.accepts(t) => {}
                        other => return Err(format!("incompatible peer socket type {other:?}")),
                    }
                    self.conn(id).phase = Phase::Traffic;
                    eprintln!("[custom iouring] XPUB conn {id} ready");
                }
                (Phase::Handshake, _) => return Err("message before READY".into()),
                (_, FrameKind::Command) => {
                    let (name, data) = command_name(&body)?;
                    match name {
                        b"SUBSCRIBE" => self.subscription(id, true, data),
                        b"CANCEL" => self.subscription(id, false, data),
                        b"PING" => self.send_ctl(id, pong(data)),
                        b"ERROR" => return Err("peer sent ERROR".into()),
                        _ => {}
                    }
                }
                (_, FrameKind::Message { more: false }) if !body.is_empty() && body[0] <= 1 => {
                    self.subscription(id, body[0] == 1, &body[1..]);
                }
                _ => {} // other messages from XPUB peers are dropped
            }
        }
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
        let _ = self.ctl.send((on, prefix.to_vec()));
        self.ring.doorbell();
    }

    fn kill(&mut self, id: usize, why: &str) {
        let Some(c) = self.conns[id].as_mut() else { return };
        if c.dead {
            return;
        }
        c.dead = true;
        unsafe { libc::shutdown(c.fd, libc::SHUT_RDWR) };
        eprintln!("[custom iouring] XPUB conn {id} closed: {why}");
        for p in std::mem::take(&mut c.prefixes) {
            let _ = self.ctl.send((false, p));
        }
        self.ring.doorbell();
        self.reap(id);
    }

    fn reap(&mut self, id: usize) {
        if self.conns[id].as_ref().is_some_and(|c| c.dead && c.ops == 0) {
            let c = self.conns[id].take().expect("conn");
            unsafe { libc::close(c.fd) };
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let cfg = parse_args();
    let xsub_l = TcpListener::bind(&cfg.xsub).expect("bind XSUB").into_raw_fd();
    let xpub_l = TcpListener::bind(&cfg.xpub).expect("bind XPUB").into_raw_fd();
    eprintln!(
        "[custom iouring] XSUB {} (receive thread, xsub iouring) -> XPUB {} (publish thread, xpub iouring) | SPSC depth {} | ring {} x {} KiB",
        cfg.xsub,
        cfg.xpub,
        cfg.depth,
        cfg.ring_entries,
        cfg.ring_buf >> 10
    );
    let sh = Arc::new(Shared {
        fwd: Spsc::new(cfg.depth),
        done: Spsc::new(cfg.depth + 2),
        base: AtomicUsize::new(0),
        buf_size: cfg.ring_buf,
        recv_sleep: AtomicU8::new(AWAKE),
        publish_sleep: AtomicU8::new(AWAKE),
        depth: cfg.depth,
    });
    let (ctl_tx, ctl_rx) = mpsc::channel();
    // Each ring must be created on the thread that submits to it
    // (SINGLE_ISSUER); the threads swap ring fds for the doorbells.
    let (r_fd_tx, r_fd_rx) = mpsc::channel::<RawFd>();
    let (p_fd_tx, p_fd_rx) = mpsc::channel::<RawFd>();
    let max_msg = (MAX_SEGS - 1) * cfg.ring_buf;

    let (cfg_r, sh_r) = (cfg.clone(), sh.clone());
    let recv = std::thread::Builder::new()
        .name("xsub-iouring".into())
        .spawn(move || {
            if let Some((cpu, _)) = cfg_r.cpus {
                pin(cpu);
            }
            let mut ring = Ring::new();
            r_fd_tx.send(ring.ring.as_raw_fd()).expect("fd");
            ring.peer_fd = p_fd_rx.recv().expect("fd");
            let t = ReceiveThread {
                cfg: cfg_r,
                sh: sh_r,
                ring,
                conns: Vec::new(),
                entries: std::ptr::null_mut(),
                tail: 0,
                refs: Vec::new(),
                free_bufs: 0,
                ctl: ctl_rx,
                union: HashMap::new(),
                msgs: 0,
                bytes: 0,
                max_msg,
                blocked: false,
                doorbells: 0,
            };
            t.run(xsub_l);
        })
        .expect("spawn");
    let (cfg_p, sh_p) = (cfg.clone(), sh.clone());
    let publish = std::thread::Builder::new()
        .name("xpub-iouring".into())
        .spawn(move || {
            if let Some((_, cpu)) = cfg_p.cpus {
                pin(cpu);
            }
            let mut ring = Ring::new();
            p_fd_tx.send(ring.ring.as_raw_fd()).expect("fd");
            ring.peer_fd = r_fd_rx.recv().expect("fd");
            let t = PublishThread { cfg: cfg_p, sh: sh_p, ring, conns: Vec::new(), ctl: ctl_tx };
            t.run(xpub_l);
        })
        .expect("spawn");
    recv.join().expect("receive thread");
    publish.join().expect("publish thread");
}
