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
    [--direct-kb 64] [--policy backpressure|drop] [--inflight 2] \
    [--recv multishot|ring|chunk] [--recvs 2] [--ring-buf-kb 64] [--ring-entries 1024] \
    [--async-send on]

# benchmark (needs libzmq-dev, taskset, python3)
cd bench && for x in pub sub zproxy; do gcc -O2 -o $x $x.c -lzmq; done
PUB_NODROP=1 python3 run.py   # SIZES=, REPS=, WARM=, MEAS=, CONFIGS=, OUT= to override
python3 diag.py 1048576 zmq_proxy ring1 multishot   # per-CPU / per-socket diagnostics
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

### Two sends in flight per subscriber (`--inflight 2`)

Each round submits up to K `SENDMSG(_ZC)` ops per subscriber as one
`IOSQE_IO_LINK` chain, each with `MSG_WAITALL`, so the next batch is already
queued in the kernel while the current one is written, and TCP byte order
still holds. This was a separate run (`bench/results_inflight.json`), so the
baselines were re-measured alongside it; medians of 3 runs:

| Message | direct | `zmq_proxy` | copy, K=1 | **copy, K=2** | zc, K=1 | zc, K=2 |
|---|---|---|---|---|---|---|
| 10 KiB  | 941 MB/s  | 380 · 2.43 s/GB  | 910 · 0.39 | **917 · 0.38** | 879 · 0.79 | 865 · 0.80 |
| 100 KiB | 2776 MB/s | 1282 · 0.60 s/GB | 2306 · 0.25 | **2676 · 0.23** | 1137 · 0.60 | 1123 · 0.61 |
| 1 MiB   | 2630 MB/s | **2485 · 0.23 s/GB** | 2299 · 0.30 | 2200 · 0.30 | 1489 · 0.46 | 1521 · 0.46 |

* **100 KiB: +16%** (2306 → 2676 MB/s), which is 96% of the direct
  ceiling and 2.1× `zmq_proxy`. This is the size where the gap between one
  send finishing and the next being submitted was costing throughput.
* **10 KiB:** no change; it was already at about 97% of the ceiling.
* **1 MiB: no improvement (slightly worse, about −4%), and `zmq_proxy`
  still leads by about 10%.** The average send at 1 MiB was about one
  message, so the bridge is waiting on its single publisher recv path, not
  on a send gap. The remaining suspect is the receive side: each 1 MiB
  frame takes a chunk recv plus a direct recv, with one recv in flight per
  publisher. Untested.
* Zero-copy sends are unaffected; on loopback every one of them is still
  copied by the kernel.

### Provided buffer ring receive (`--recv ring|multishot`)

Every peer's recvs now pick buffers from one kernel-provided buffer ring
(`IORING_REGISTER_PBUF_RING`; 1024 × 64 KiB by default). Frames are assembled
from however many ring buffers they span and forwarded by reference; a
buffer returns to the ring when its last reference drops. An empty ring
(`-ENOBUFS`) parks a peer's recvs until buffers come back, so **the ring
size is the receive-side memory bound, the HWM replacement on the way in**.
`--recv ring --recvs K` keeps K plain recvs in flight per peer;
`--recv multishot` uses one multishot recv. Ordering across concurrent recvs
is checked via ring positions (the kernel consumes them in order): **0
reorders** in every run.

Copy-mode sends, `--inflight 2`; medians of 3 runs, MB/s · proxy CPU s/GB
(`bench/results_ring.json`, `bench/results_ring_ms256.json`):

| Message | direct | `zmq_proxy` | chunk recv (old) | ring, 1 recv | ring, 2 recvs | ring, 2 recvs, 256 KiB bufs | **multishot** | multishot, 256 KiB bufs |
|---|---|---|---|---|---|---|---|---|
| 10 KiB  | 945  | 386 · 2.39  | 908 · 0.38  | **952 · 0.37** | 928 · 0.38 | 914 · 0.39 | 905 · 0.40 | 914 · 0.39 |
| 100 KiB | 2696 | 1461 · 0.55 | 2426 · 0.24 | 1852 · 0.36 | 2175 · 0.32 | **2490 · 0.21** | 2182 · 0.24 | 2004 · 0.23 |
| 1 MiB   | 2576 | 2680 · 0.23 | 2248 · 0.30 | 2001 · 0.33 | 1878 · 0.34 | 2215 · 0.31 | **2748 · 0.25** | 2626 · 0.25 |

* **Multishot closes the 1 MiB gap**: 2748 MB/s against 2680 for
  `zmq_proxy`, so parity within run-to-run noise, up from 2248 for chunk
  recv. The receive side was the 1 MiB bottleneck, as suspected.
* **Two plain recvs per publisher did not help**: worse than one recv at
  100 KiB and 1 MiB with 64 KiB buffers. Each completion still costs a
  userspace round trip to re-arm, and with two outstanding, each tends to
  return a smaller slice of the socket. Multishot removes the re-arm
  entirely. This is a hypothesis; the per-recv sizes were not broken down.
* **Buffer size matters at 100 KiB**: 256 KiB ring buffers with 2 recvs reach
  2490 MB/s (best, at the lowest CPU per GB of any config), against 2175
  with 64 KiB buffers. Multishot prefers 64 KiB buffers at 1 MiB.
* No single configuration is best at every size. **Multishot with 64 KiB
  buffers is now the default**: best at 1 MiB, at the ceiling at 10 KiB,
  1.5× `zmq_proxy` at 100 KiB, and ordered by design. For 100 KiB-heavy
  traffic, use `--recv ring --recvs 2 --ring-buf-kb 256 --ring-entries 256`.
* With zero-copy sends, ring receive behaves like before: the kernel copies on
  loopback anyway, so it is slower.

## Root cause analysis

Tooling: `bench/diag.py` (per-CPU, per-process and per-socket queue
sampling), `BR_DIAG=1` (bridge wait-state and size histograms),
`--sink`/`--source` (receive-only and send-only bridge), `ss -ti` TCP
limit counters, and `perf record -g`.

**1. Every earlier table was publisher-bound.** cpu0 (the libzmq PUB) was
0% idle in every configuration, *including direct PUB→SUB*. libzmq PUB never
blocks at HWM, so the benchmark's send loop spun at user level on the same
core as libzmq's I/O thread. With `PUB_NODROP=1` (`ZMQ_XPUB_NODROP`: block
instead of drop), direct PUB→SUB at 1 MiB rises from **2.8 to 4.6 GB/s**.
The earlier tables compared proxies under identical conditions, but their
absolute numbers were understated.

**2. With that fixed, the limit is the bridge's single thread doing
receive and send kernel work serially on one core.**

* Under load the bridge thread is 100% runnable, and about 97% of that is
  inside `io_uring_enter`: copies, TCP and loopback processing, and ACKs. Its
  user-space work is 1-3%.
* Each path is fast on its own: **7.5-8 GB/s** receive-only (`--sink`,
  ring) and **3.8 GB/s** send-only (`--source`, independent of send size
  from 64 KiB to 8 MiB).
* On one core they serialize and slow each other down. ring1's receive path
  costs about 260 µs/MB with sends sharing the core, against 132 µs/MB
  alone. Most of the extra is the receive copy (`_copy_to_iter`) and
  window-update ACKs (`tcp_cleanup_rbuf` → `tcp_send_ack`, about 8.9 µs per
  ACK against 2.8 µs under multishot). The exact cause of that per-op
  inflation is **unconfirmed**: this KVM/Firecracker guest has no hardware
  performance counters. The suspects are cache effects from a 4 MB unread
  backlog against a 2 MiB L2, and timer reprogramming cost in the VM.
* ruled out: undersized receive buffer (a fixed 16 MiB `SO_RCVBUF` changed
  nothing, because the window was full, not small); the publisher's transmit
  running on the bridge's core (0% of samples); send batch size.

**Causal test:** `--async-send` sets `IOSQE_ASYNC` on the linked send chain,
so io_uring runs sends on an io-wq worker on the second core. Fixed
publisher, medians of 3, MB/s (`bench/results_rootcause.json`):

| Message | direct | `zmq_proxy` | ring 1 recv | ring 2 recvs | multishot | **ring 1 + async send** | **ring 2 + async send** | **multishot + async send** |
|---|---|---|---|---|---|---|---|---|
| 10 KiB  | 1425 | 376  | 1531 | 1551 | 1561 | 1587 | 1546 | 1546 |
| 100 KiB | 4785 | 1298 | 1545 | 1657 | 3000 | **4729** | **4777** | 4485 |
| 1 MiB   | 4592 | 3365 | 2080 | 2122 | 2457 | 3792 | 3826 | **3906** |

* With sends on their own core, **100 KiB reaches the direct ceiling (about
  4.8 GB/s, 3.7× `zmq_proxy`)** and **1 MiB reaches 3.9 GB/s (+16% over
  `zmq_proxy`)**, close to the 3.8 GB/s send-path limit. The bridge uses 1.0-1.2
  cores in total.
* **10 KiB:** every bridge mode beats direct PUB→SUB (about 1.55 vs 1.43 GB/s)
  and is 4× `zmq_proxy`; there the endpoints are the limit, not the
  bridge.
* The differences between receive modes (ring, 1 or 2 recvs, multishot)
  mostly disappear once the cores are split. That is why two recvs never
  helped: the constraint was one core's worth of kernel work, not recv
  concurrency. Both 64 KiB recvs completed in the same kernel pass (2.1
  CQEs per wake), so it was the same work on the same core.
* The proper fix is two rings on two threads, one for receiving and one
  for sending. `--async-send` approximates that by using io-wq as the second
  thread, with ordering preserved by the linked chain.

Caveats: these are loopback numbers on a small VM with everything on 4
cores. The PUB and SUB are libzmq in both cases, so the comparison between
proxies is fair, but the absolute numbers will differ on real hardware and
NICs.
