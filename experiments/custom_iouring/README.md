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

RESULTS_PLACEHOLDER
