# custom iouring: XSUB → XPUB on io_uring, compared with libzmq

The application is simple. An **XSUB** socket receives messages for a topic,
and each received message is published, frames as-is, on an **XPUB**
socket. Subscriptions that arrive on the XPUB side are passed to the XSUB
side. Two implementations of that one application are compared:

* **custom iouring** (`src/main.rs`, Rust): speaks ZMTP 3.1 itself, using
  [`weida-zmtp`](https://crates.io/crates/weida-zmtp) for framing, on two
  io_uring instances driven by one thread:
  * the **xsub iouring** owns the XSUB listener and its connections. It
    accepts them, receives their messages, and sends them our
    subscriptions;
  * the **xpub iouring** owns the XPUB listener and its connections. It
    accepts them, receives their subscriptions, and sends them the forwarded
    messages.
* **libzmq** (`bench/libzmq_xsub_xpub.c`, C): a `ZMQ_XSUB` and a `ZMQ_XPUB`
  socket with the same loop: receive a message on XSUB, send its frames on
  XPUB (the same `zmq_msg_t`s, moved not copied).

## custom iouring: straight, synchronous forwarding, no queue

```
XSUB peer ──tcp──▶ xsub iouring: recv into the connection's buffer
                     │ until one whole message (all frames) is there
                     ▼
                   pick the XPUB peers whose subscriptions match frame 1
                     │
                     ▼
                   xpub iouring: SENDMSG of the message's bytes exactly as
                   received (headers included, straight from the receive
                   buffer, no copy), one per matching XPUB peer
                     │ wait until every one of those sends has completed
                     ▼
                   only then consume the next message / post the next recv
```

* **No application queue.** Nothing is ever queued to be sent later, and
  there is no send offload. Each message completes on every XPUB peer before
  the next one is read.
* **Flow control is TCP's.** A slow XPUB peer stalls the sends, which stalls
  the XSUB reads, which closes the XSUB peers' TCP windows. There is no HWM,
  and no drop policy.
* **Receive sizing.** When the frame being received is large, the recv asks
  for exactly the bytes it is missing. A message therefore usually ends at
  the end of the buffer, and the buffer never has to be compacted. The
  buffer grows to the largest message (`--max-msg-mb`, 64 by default).
* **One thread, two rings.** The thread sleeps on the xsub iouring, which
  holds a multishot `POLL_ADD` on the xpub iouring's fd, so XPUB-side events
  (new peers, subscriptions) wake it too. While a forward is waiting on its
  sends, it waits on the xpub iouring.
* **libzmq compatibility:** ZMTP 3.0 peers are accepted, and are sent
  subscriptions as `%x01`/`%x00` messages. Unknown commands are ignored. PING
  contexts are truncated to 16 bytes. Subscriptions are accepted both as
  commands and as messages.
* `--zc on` uses `SENDMSG_ZC` and also waits for its notification before
  the buffer is reused. It is off by default: on loopback the kernel copies
  anyway, so zero-copy only costs extra there.

Tested with libzmq 4.3.5 on both sides: the harness (PUB into XSUB, SUB out
of XPUB) loses no messages, and frames arrive intact. A second XPUB peer
received every message in order, with no gaps.

## Benchmark

`bench/harness.c` is **one process**, so send and receive timestamps come
from the same `CLOCK_MONOTONIC`:

* a sender thread publishes 2-frame messages `["bench"][payload]` to the
  application's XSUB endpoint (libzmq PUB; `ZMQ_XPUB_NODROP`, so it blocks
  instead of dropping);
* a receiver thread subscribes to `"bench"` on the application's XPUB
  endpoint (libzmq SUB);
* the payload carries a sequence number and the send timestamp (taken just
  before the first frame is handed to libzmq). Per-packet latency is the
  receive time minus that. Throughput counts the bytes received in the
  measurement window. Lost messages are detected from sequence gaps.

Two modes:

* **saturate**: send as fast as the path accepts. This gives end-to-end
  throughput. The latency it reports is latency *under full load*, so it is
  dominated by queueing in the harness's own libzmq sockets and the TCP
  buffers.
* **pingpong**: one message in flight. The next is sent only after the
  previous one arrived. This gives per-packet latency with no queueing.

`direct` is the harness talking to itself (PUB binds, SUB connects, no
application in between): the ceiling the harness and libzmq endpoints can
reach.

The machine is a 4-vCPU KVM guest (Firecracker), kernel 6.18, libzmq 4.3.5,
over loopback. The harness is pinned to cpus 0-1 and the application to cpus
2-3. Each cell is the median of 3 runs: a 2 s warm-up, then a 5 s
measurement.

```sh
cargo build --release
cd bench && gcc -O2 -o harness harness.c -lzmq -lpthread && gcc -O2 -o libzmq_xsub_xpub libzmq_xsub_xpub.c -lzmq
python3 run.py      # SIZES=, MODES=, APPS=, REPS=, WARM=, MEAS=, OUT= to override
```

## Results

Medians of 3 runs. Throughput is the payload and topic bytes delivered to the
XPUB-side receiver per second. Latency is per packet, measured with the
same-process monotonic clock. "cores" is the application's CPU use. No run
lost a message. Raw data: `bench/results.json`.

### Saturate: end-to-end throughput, and latency under full load

| Size | | direct (ceiling) | **custom iouring** | **libzmq** |
|---|---|---|---|---|
| 10 KiB  | throughput | 1193 MB/s · 116k msg/s | **1118 MB/s** · 109k msg/s | 667 MB/s · 65k msg/s |
|         | p50 / p99  | 2.8 / 4.5 ms | 7.6 / 15.5 ms | 19.8 / 23.1 ms |
|         | app cores  | – | 0.64 | 1.05 |
| 100 KiB | throughput | 2192 MB/s · 21k msg/s | **1677 MB/s** · 16k msg/s | 1294 MB/s · 13k msg/s |
|         | p50 / p99  | 4.0 / 7.0 ms | 4.3 / 6.8 ms | 14.2 / 24.0 ms |
|         | app cores  | – | 0.65 | 1.11 |
| 1 MiB   | throughput | 3918 MB/s · 3.7k msg/s | 2287 MB/s · 2.2k msg/s | **2390 MB/s** · 2.3k msg/s |
|         | p50 / p99  | 13.7 / 27.0 ms | 25.7 / 41.6 ms | 25.4 / 38.2 ms |
|         | app cores  | – | 0.97 | 1.02 |

### Pingpong: per-packet latency with one message in flight

| Size | direct p50 / p99 | **custom iouring** p50 / p99 | **libzmq** p50 / p99 |
|---|---|---|---|
| 10 KiB  | 33 / 58 µs   | **58 / 95 µs**   | 95 / 154 µs  |
| 100 KiB | 43 / 72 µs   | **100 / 143 µs** | 140 / 198 µs |
| 1 MiB   | 248 / 352 µs | 602 / 786 µs     | **498 / 707 µs** |

### Reading the numbers

* **10 KiB and 100 KiB: custom iouring wins on every axis.** Throughput is
  1.7× and 1.3× libzmq's. Pingpong latency is about 40 µs lower, at p50 and
  at p99. It uses about 0.65 of a core against libzmq's 1.05-1.1. At 10 KiB
  it reaches 94% of the direct ceiling. One of its three 10 KiB saturate
  runs came in at 505 MB/s; the median and the other run were about
  1.1-1.2 GB/s.
* **1 MiB: libzmq is slightly ahead**: 4% more throughput, and about 100 µs
  lower pingpong p50. custom iouring's single thread does the whole
  receive copy, then the whole send copy, of each 1 MiB message. libzmq
  overlaps them across its I/O threads and the forwarding thread, which only
  moves `zmq_msg_t`s. At 1 MiB custom iouring is at 0.97 core, so it is
  CPU-bound. This is the cost of the "straight synchronous, no queue"
  design at large sizes.
* **Saturate-mode latency is queueing, not processing.** The direct
  ceiling alone shows ms-scale p50, because the harness's own libzmq queues
  and the TCP buffers are full. Compare the applications' saturate
  latencies with each other, not with pingpong.

Caveats: loopback on a 4-vCPU VM. The harness and libzmq endpoints use 2
cores, and the application gets the other 2. Absolute numbers will differ on
real hardware and NICs. Zero-copy send (`--zc on`) cannot show a benefit on
loopback.
