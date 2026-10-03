# zmtp-uring-bridge

An experimental, single-threaded **XSUB → XPUB bridge** that speaks ZMTP 3.1
(libzmq-compatible) on a single io_uring ring. It is a drop-in replacement for
`zmq_proxy(xsub, xpub)` in a PUB/SUB fan-out, built to test one idea:

> Replace libzmq's message-count HWM and SNDBUF/RCVBUF tuning with
> refcounted receive buffers forwarded by reference, `IORING_OP_SENDMSG_ZC`
> on the way out, and a single **byte** budget as the only flow-control knob.

The framing comes from [`weida-zmtp`](https://crates.io/crates/weida-zmtp) (a
dependency-free, no-I/O codec). The session, routing, buffer management and
io_uring driver are about 900 lines in `src/main.rs`.

## Design

```
 libzmq PUB ──tcp──▶ [front: XSUB]  ──Rc<Frame>──▶  [back: XPUB] ──tcp──▶ libzmq SUB
                      recv into 256 KiB chunks       SENDMSG_ZC  hdr + body iovecs
                      large frames: recv the tail    batched per peer
                      straight into its own buffer   buffers held until NOTIF CQE
        ◀── SUBSCRIBE/CANCEL (union, refcounted) ◀── SUBSCRIBE/CANCEL or %x01/%x00
```

* **No userspace copy on the data path.** A frame that fits in the receive
  chunk is forwarded as a slice of that chunk. A large frame that crosses the
  chunk end keeps its first part in the chunk and receives the rest directly
  into a dedicated buffer, so it goes out as two segments, not a copy. The
  header is re-encoded (2 or 9 bytes). Fan-out to N subscribers shares one
  `Rc<Frame>`.
* **HWM is replaced with bytes.** Every buffer comes from a size-classed pool,
  and its bytes count against `--budget-mb` until the last reference drops,
  which for zero-copy sends means *after the kernel's notification CQE*. Each
  subscriber also has `--sub-cap-mb` (queued plus awaiting-notification bytes).
  * `--policy backpressure` (default): over budget or over a subscriber's
    cap, the bridge stops posting recvs on publishers, so TCP pushes back
    upstream. Memory is bounded and the bridge itself loses nothing.
  * `--policy drop`: a subscriber over its cap misses whole messages, as a
    PUB does at HWM.
* **SNDBUF/RCVBUF** are left at kernel defaults. With zero-copy sends the
  socket buffer stops being the place data waits; the pinned user buffers
  are, and those are what the budget counts.
* **Protocol behaviour, matching libzmq:** 3.0 peers are accepted (the 3.0
  form of subscriptions is sent to them), unknown commands are ignored, PING
  contexts longer than 16 bytes are truncated rather than rejected, and both
  subscription forms are accepted from subscribers.
* **Not implemented:** CURVE/PLAIN, IPC, reconnect (the bridge only binds),
  multiple threads, and the XPUB options (verbose, manual, welcome message).

## Running

```sh
cargo build --release
./target/release/zmtp-uring-bridge --front 127.0.0.1:5555 --back 127.0.0.1:5556 \
    [--zc on|off] [--budget-mb 256] [--sub-cap-mb 64] [--chunk-kb 256] \
    [--direct-kb 64] [--policy backpressure|drop]

# benchmark (needs libzmq-dev, taskset, python3)
cd bench && for x in pub sub zproxy; do gcc -O2 -o $x $x.c -lzmq; done
python3 run.py            # SIZES=..., REPS=..., WARM=..., MEAS=... to override
```

## Results

See the "Results" section appended below by the benchmark run.
