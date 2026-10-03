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

The setup is a 4-vCPU cloud VM, kernel 6.18, libzmq 4.3.5, over **loopback**.
The libzmq PUB is pinned to cpu0 and the libzmq SUB to cpu1; the proxy under
test gets cpus 2-3. Each cell is the median of 3 runs: a 2 s warm-up, then 5 s
of throughput measured at the SUB. "CPU s/GB" is the proxy process's
user + system time per GB delivered. Raw data is in `bench/results.json`.

| Message | direct PUB→SUB (ceiling) | `zmq_proxy` | `zmq_proxy` hwm=0 | `zmq_proxy` 2 I/O threads | **bridge, copy send** | bridge, `SEND_ZC` |
|---|---|---|---|---|---|---|
| 10 KiB  | 1004 MB/s | 423 MB/s · 2.18 s/GB | 391 · 2.38 | 403 · 2.32 | **901 MB/s · 0.39 s/GB** | 909 · 0.76 |
| 100 KiB | 2808 MB/s | 1319 MB/s · 0.58 s/GB | 1383 · 0.56 | 1302 · 0.58 | **2460 MB/s · 0.24 s/GB** | 1151 · 0.61 |
| 1 MiB   | 2826 MB/s | **2686 MB/s · 0.22 s/GB** | 2705 · 0.22 | 2575 · 0.22 | 2310 MB/s · 0.30 s/GB | 1436 · 0.47 |

What this shows:

* **Small and medium messages:** the bridge delivers **2.1× (10 KiB) and
  1.9× (100 KiB)** the throughput of `zmq_proxy`, at **5.6× and 2.4× less
  CPU per GB**. It reaches 90% and 88% of the direct PUB→SUB ceiling, which
  is set by the single libzmq SUB, not by the bridge.
* **1 MiB:** `zmq_proxy` wins by about 14% and uses less CPU per GB. The
  bridge keeps only one send in flight per subscriber; overlapping sends
  per peer is the next thing to try, but that is untested. Raising
  `--chunk-kb`/`--direct-kb` did not help copy mode (about 2.3 GB/s at 1 MiB
  and 2 MiB chunks) and made 100 KiB slower.
* **Zero-copy cannot be evaluated on loopback.** With
  `IORING_SEND_ZC_REPORT_USAGE`, the kernel flagged **100%** of
  notifications as `ZC_COPIED`: loopback (and veth) delivery forces a
  deferred copy, so `SEND_ZC` pays for page pinning and notifications on top
  of a copy. Any benefit only shows up on a real NIC (typically for frames
  of about 10 KB and up). Copy mode is the fair loopback number. With 1 MiB
  chunks, zero-copy at 1 MiB improves to about 2.07 GB/s.
* **libzmq's HWM is not the bottleneck here.** `hwm=0` (unlimited) and a
  second I/O thread change nothing, so the gap comes from per-message
  pipe/queue overhead in the proxy. The bridge avoids that by forwarding
  refcounted buffers in batches of up to 96 iovecs per `sendmsg`.
* **Fan-out** (3 SUBs, 100 KiB, backpressure policy): every subscriber
  received exactly the same 15,309 messages at 522 MB/s each, with no
  loss. Live buffer memory stayed around 1 MB throughout. The
  `--policy drop` path ran but never triggered, because no subscriber fell
  behind; it has not been exercised against a slow consumer.

Caveats: these are loopback numbers on a small VM with everything on 4
cores. The PUB and SUB are libzmq in both cases, so the comparison between
proxies is fair, but the absolute numbers will differ on real hardware and
NICs.
