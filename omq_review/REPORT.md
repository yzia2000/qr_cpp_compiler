# omq.rs as an XPUB/XSUB broker for 100 KB – 1 MB payloads: production review

Reviewed: **omq-tokio 0.24.0, omq-proto 0.28.1, omq-libzmq 0.5.20** (crates.io releases of
2026-09-27, built from omq.rs commit `5e29747`; the XPUB/XSUB/proxy code is byte-identical at
`main` = `79cf9e0`, which only adds latency-path commits).

Compared against:

| label in this report | implementation | notes |
|---|---|---|
| `libzmq-c` | **libzmq 4.3.5** (Ubuntu 24.04 package), `zmq_proxy()` from C | the reference |
| `rust-zmq` | `zmq` crate 0.10 (rust-zmq) | zmq-sys 0.12 always builds its **bundled libzmq 4.3.4** |
| `omq-tokio` | omq native Rust API, `omq_tokio::proxy::proxy()` | default `Context::new()` (1 IO thread) |
| `omq-tokio+slotcap64M` | same, `Options::transmit_slot_cap = 64 MiB` | mitigation for finding C3 |
| `omq-hardened` | native, `io_threads=2` + `slot_cap=64 MiB` + `max_message_size=16 MiB`, subscribe-all + drain | C1+C2+C3 workarounds |
| `omq-c` | the **same C server source** linked against `libomq_zmq.so` (omq's libzmq drop-in) | reports itself as libzmq 4.3.6 |
| `zeromq` | zmq.rs (`zeromq` crate 0.6.0) | the other pure-Rust implementation |
| `rzmq-*` | rzmq 0.5.26, Tokio or io_uring backend | no XPUB/XSUB: subscribe-all SUB→PUB forwarder ([addendum](#addendum-rzmq-0526-and-does-io_uring-make-it-faster)) |

## Verdict

**Not production-ready as a drop-in XPUB/XSUB broker for 100 KB–1 MB payloads today, in either the
native or the C-shim form.** The gaps are not subtle tuning issues; several are reachable from a
normal (or hostile) peer and were reproduced here:

* **It crashes.** One 9-byte frame from any peer that completes the (trivial, unauthenticated) NULL
  handshake aborts the whole broker process (C1). libzmq survives the identical input. This alone
  disqualifies the default configuration for any untrusted-facing deployment.
* **It loses data under trivial load.** With 8 messages in flight and one healthy subscriber, omq
  dropped 4–60% of 100 KB–1 MB messages where libzmq dropped none (C3). This is the direct answer to
  "is it good for large payloads": in the default config, no.
* **It wedges.** A subscriber with 65+ topics pins the default single-IO-thread context at 100% CPU
  and stops serving everyone (C2).
* **It is not a faithful `zmq_proxy`.** Shared-topic unsubscribe, subscriber-disconnect cleanup, and
  subscribe de-duplication all diverge from libzmq, and one non-subscription message from the
  subscriber side terminates the proxy (C4).

**With three configuration changes** — `max_message_size` finite (C1), `transmit_slot_cap` raised to
several MiB (C3), and `io_threads >= 2` (C2) — the native API becomes *much* more usable: in my
`omq-hardened` build those three (plus not relying on omq's subscription forwarding) removed the
crash, the drops, and the livelock, and it then ran within ~1.1–1.4× of libzmq's single-subscriber
throughput (and *faster* than libzmq on 4-way fan-out) at 0% loss. But two of those knobs are **not reachable from the C `libzmq` API** (C3, and SH3 freezes the
rest), the proxy-semantics divergence (C4) has no config workaround, and open data-integrity/liveness
risks remain (H1, H2, H7). So: promising for a *controlled* native-Rust deployment you fully own and
can put behind trusted publishers, with those options set and heartbeats off; **not** a safe libzmq
substitute for an untrusted-facing or semantics-sensitive XPUB/XSUB broker.

For comparison, the other pure-Rust implementation (zmq.rs / `zeromq` 0.6.0) was **worse**: it could
not carry ≥100 KB through this proxy pattern at all, and its process aborted on 512 KB–1 MB frames.
omq is well ahead of zmq.rs; it is not yet at libzmq's level for this use case.

**rzmq (io_uring) was added later** — see the [addendum](#addendum-rzmq-0526-and-does-io_uring-make-it-faster).
In short: io_uring makes rzmq fast at ~100 KB (at or above libzmq) but not at 256 KB–1 MB, where
libzmq on plain epoll stays ~1.6× ahead; rzmq has no XPUB/XSUB at all, and its PUB blocks on a slow
subscriber by default. (omq itself does not use io_uring; its io_uring backend was removed in July
2026, so io_uring plays no part in the omq-vs-libzmq gap.)


## Findings at a glance

| # | severity | area | gap vs libzmq | how found |
|---|---|---|---|---|
| C1 | Critical | codec / DoS | one 9-byte oversized-frame header aborts the whole process | reproduced |
| C2 | Critical | fan-out | 65+ topics from one SUB livelock the default context at 100% CPU | reproduced |
| C3 | Critical | XPUB send | drops 4–60% of 100 KB–1 MB messages with 8 in flight (512 KiB slot cap) | reproduced |
| C4 | Critical | proxy/XSUB | not a faithful `zmq_proxy`: shared-topic unsub, no disconnect cleanup, no dedup, dies on non-sub message | reproduced |
| H1 | High | codec | large-frame read blocks the driver loop (no heartbeat/cancel mid-message) | code |
| H2 | High | actor | one slow/stalled publisher can freeze subscription propagation broker-wide | code |
| H3 | High | actor | XPUB actor stall halts admission/cleanup, false heartbeat timeouts | code |
| H4 | High | actor | biased `select!` starves peer events; actor↔driver deadlock reachable remotely | code |
| H5 | High | subs | O(N) subscription bookkeeping + per-handshake `Vec` clone; storms are quadratic | code |
| H6 | High | XPUB | `xpub_nodrop` silently drops for CURVE/WS/inproc/mixed-compression peers | code |
| H7 | High | wire | heartbeat/PONG can be written inside a data frame → corrupt payload + desync | code |
| H8 | High | fan-out | WS/mixed-compression subscriber muted forever after one inbox overflow | code |
| M1 | Medium | close | native `linger` defaults to 0 → drops queued messages on close | code |
| M2 | Medium | context | `Context::block_on(proxy)` serializes the whole context | code |
| M3 | Medium | accept | 128 stalled handshakes lock out all new peers for 30 s | code |
| L1 | Low | subs | one detached task spawned per SUBSCRIBE | code |
| SH1–7 | High/Med | C shim | ignored security options, CURVE ZAP fail-open, options frozen, HWM=0 rejected, `ZMQ_FD` spins, UAF on cross-thread close | code + source |

"reproduced" = demonstrated with a running binary here; "code" = read in the released source with
file:line (details and libzmq references in the Findings section).

## How it was tested

Everything here was measured with an **independent client process** that uses only the reference
libzmq (4.3.5) — nothing in the measurement path shares code with the server under test.

* **Servers** (`servers/`, `c_proxy/`): each binds XSUB (publishers connect) and XPUB (subscribers
  connect) on TCP and runs the implementation's own proxy (`zmq_proxy`, `omq_tokio::proxy::proxy`,
  `zeromq::proxy`). Same CLI for all.
* **Load generator / verifier** (`client/xbench.cpp`, C++ on libzmq 4.3.5): one PUB thread pushes to
  the server, N SUB threads receive the messages back. Every payload is rebuilt byte-for-byte by the
  receiver (a sequence number is stamped into every 4 KiB page, the body comes from one of 61
  deterministic buffers), so corruption, truncation, cross-message mixing, loss (sequence gaps),
  duplicates and reordering are all detected; latency is `recv − send` on the shared monotonic
  clock. Modes: closed loop with *W* messages in flight (lossless is expected whenever *W* ≪ HWM),
  open loop at a fixed rate, and flood.
* **Semantics** (`scenarios/conformance.py`, pyzmq on libzmq 4.3.5): black-box scenarios that each
  start a fresh server and check one libzmq rule (the rule and the libzmq source it comes from are
  in each scenario's docstring). Plus `zmtp_raw.py` / `zmtp30_publisher.py`, raw-socket ZMTP peers
  for protocol edge cases, and `churn_soak.py` (traffic + peer churn + RSS sampling).
* **Code review** of `omq-proto`, `omq-tokio`, `omq-libzmq` against libzmq's `xpub.cpp`, `xsub.cpp`,
  `dist.cpp`, `pipe.cpp`, `proxy.cpp`, `v2_decoder.cpp`.
* Host: 4 vCPU Xeon KVM guest, 16 GB RAM, Linux 6.18, loopback TCP. Numbers are relative
  comparisons on one box, medians of 3 runs.

The 21-scenario black-box conformance suite (`results/conformance.json`) scored:

| server | PASS | what still fails |
|---|---|---|
| libzmq-c (reference) | **21/21** | — |
| omq-tokio (default) | **9/21** | C4 semantics (8), C2 livelock (65-topics / 20k-storm / 10×10), C3 drops (burst, multipart) |
| omq-tokio, `io_threads=2` | **12/21** | C4 semantics (8), C3 drops (burst, multipart) — livelock fixed |
| omq-hardened (all knobs) | **15/21** | the 6 upstream-subscription-propagation tests — **by design** (it subscribes-all), not crashes/drops |
| zeromq / zmq.rs | **15/21** | shared-unsub, disconnect-cleanup, dedup, multipart integrity, large-message burst |

The progression 9 → 12 → 15 is the story in one line: `io_threads=2` buys back the livelock
scenarios, the slot cap buys back the drop scenarios, and what remains for the fully-hardened build
is the subscription-semantics divergence (C4), which has no configuration fix.

## Findings

Severity: **C**ritical (crash / data loss / DoS reachable from a normal or hostile peer) ·
**H**igh · **M**edium · **L**ow. "Confirmed" = reproduced here with a running binary or a decisive
test; "code-confirmed" = read directly in the released source with file:line, not separately
reproduced. Line numbers are in the crates.io releases (they match `main` for this code).

### C1 — A single 9-byte frame header crashes the whole broker (remote, unauthenticated-equivalent). **Critical. Confirmed.**

Any peer that finishes the trivial NULL handshake and then sends one long-frame header declaring a
huge body — **and no body at all** — aborts the entire process. The tokio driver's "direct read"
path allocates the *declared* length up front:

* `omq-tokio/src/engine/driver.rs:2122` → `BytesMut::with_capacity(plen)` where `plen` is the wire
  length, bounded only by `isize::MAX` (`omq-proto/src/proto/frame.rs:244-250`);
* the size is only checked when `Options::max_message_size` is `Some`, and it **defaults to `None`**
  (`omq-proto/src/options.rs:359`), matching libzmq's `ZMQ_MAXMSGSIZE = -1` default;
* `with_capacity` on a failed allocation calls `handle_alloc_error` → `process::abort()`.

Reproduced (`scenarios/zmtp_raw.py`): one TCP connection, NULL greeting + READY, then the 9 bytes
`02 00 00 10 00 00 00 00 00` (LONG data frame, length 2⁴⁴). The broker dies with
`memory allocation of 17592186044416 bytes failed` and a backtrace through `driver.rs:2122`. Every
other publisher and subscriber on that broker drops instantly. **libzmq 4.3.5 and rust-zmq survive
the identical attack** (they turn the failed `init_size` into a per-connection error —
`libzmq/src/v2_decoder.cpp:95-117`, `msg.cpp:83-86`). The omq-libzmq C shim (`omq-c`) and zmq.rs
(`zeromq`) **also abort** — so omq is at zmq.rs's maturity here, not libzmq's.

The sans-I/O codec is *not* at fault (it grows with bytes actually received); only the tokio
driver's direct-read optimisation pre-allocates. libzmq's own LZ4 path in this same tree has the
correct guard and an explicit comment about this exact hazard (`omq-proto/.../transform/lz4.rs`),
which was not applied to the plain read path.

**Mitigation (confirmed):** set `Options::max_message_size` to a finite value (e.g. 16 MiB). With it
set, the same attack just closes the offending connection and legitimate 1 MB traffic keeps flowing.
For a production XPUB/XSUB broker this is effectively **mandatory**, and it cannot be set at all from
the `omq-libzmq` C API in a way that protects this path (see C6). A real fix must cap `plen` at
`min(max_message_size, pool_max)` and grow incrementally, never `with_capacity(declared)`.

### C2 — 65 subscriptions from one subscriber wedge the whole broker at 100% CPU (default config). **Critical. Confirmed.**

With the default one IO thread, a subscriber that registers **more than 64 topic subscriptions**
livelocks the entire context. The fan-out lane's control ring is fixed at 64
(`omq-tokio/src/routing/fan_out/lane.rs:25`, `LANE_CTRL_RING_CAP = 64`) and the socket actor submits
subscribe/cancel/peer commands to it with a **busy spin** — `push_control_spinning` loops on
`std::thread::yield_now()` (`lane.rs:355-369`) until the ring drains. With one IO thread the lane
worker that drains the ring shares that very runtime thread, so the actor spins forever waiting for a
consumer that can never run. Measured: a single SUB with 65 topics takes the server to **100% CPU**,
after which existing subscribers stop receiving, new subscribers can't join, and the process is
effectively dead (but still "alive" to a health check). At 64 topics everything is fine — a hard
cliff. 65+ topics on one SUB is completely ordinary for market-data fan-out.

The real trigger is **more than 64 lane-control events in one actor poll**, not "65 topics per
subscriber" — so it also fires from many subscribers arriving together: the
`ten_subscribers_ten_topics_connect_together` conformance test (10 SUBs × 10 topics = 100 aggregate
subscribe events, none individually over 64) wedged omq-tokio just the same, and per the fan-out
audit so do 65+ simultaneous connects/disconnects (33+ with compression), a reconnecting SUB that
replays a large subscription set, and CANCEL/JOIN storms — i.e. an ordinary broker restart with
enough subscribers reconnecting at once.

**Mitigation (confirmed):** run the context with `io_threads >= 2` so the lane worker and the actor
are on different runtimes. This removes the livelock in tests, but the busy-spin submission remains
and other churn (see H4) can still stall it. libzmq has no such limit (its pipe commands are
unbounded/among threads).

### C3 — XPUB silently drops 100 KB–1 MB messages under trivial load (default config). **Critical for this workload. Confirmed.**

Each subscriber's per-peer transmit slot is capped at **512 KiB** of encoded bytes
(`omq-tokio/src/engine/transmit_slot.rs:23`, `TRANSMIT_SLOT_CAP_DEFAULT = 512*1024`). A slot with one
message already queued rejects the next once the total would reach the cap
(`transmit_slot.rs:182`), and for PUB/XPUB the lane worker treats that as **drop the message and
deactivate the peer** (`routing/fan_out/lane.rs:1245`), because fan-out is lossy by default. So a
subscriber can hold **at most one** 256 KiB–1 MiB message in its slot; anything produced while that
one is draining is dropped — even though the socket's HWM (1000 messages) is nowhere near reached and
the subscriber is perfectly healthy.

Measured with the independent client, closed loop, only **8 messages in flight**, one fast
subscriber, medians of 3 runs:

| payload | omq-tokio loss | libzmq loss |
|---|---|---|
| 100 KB | **14%** (worst 16%) | 0% |
| 256 KB | **60%** | 0% |
| 512 KB | **49%** | 0% |
| 1 MB | **4–6%** | 0% |

This is data loss with the client barely pushing — no HWM reached, no slow consumer, no memory
pressure. A libzmq SUB in the same test loses nothing. The public `Options::transmit_slot_cap`
**doc comment says the default is 2 MiB** (`omq-proto/src/options.rs:286`) but the constant is
512 KiB — a documentation bug that hides the problem.

**Mitigation (confirmed):** set `Options::transmit_slot_cap` to ≥ a few MiB (I used 64 MiB). Loss
goes to 0% at every size with no throughput penalty. **Not settable from the `omq-libzmq` C API**
(see C6), so the C/`libzmq`-drop-in path cannot be fixed without patching omq.

### C4 — omq's XSUB/XPUB is not a faithful `zmq_proxy` broker: subscription semantics diverge and one peer can kill the proxy. **Critical for broker use. Confirmed (black-box).**

Running `omq_tokio::proxy::proxy(XSUB, XPUB)` — the documented broker pattern — fails a series of
libzmq semantics that real pub/sub deployments depend on. Each was reproduced against a fresh server
with pyzmq (`scenarios/conformance.py`); libzmq-c and rust-zmq pass every one.

1. **Shared-topic unsubscribe cancels for everyone.** Two subscribers on topic `A`; one
   unsubscribes → the *other* stops receiving `A` entirely. omq's SUB/XSUB keeps subscriptions in a
   plain `Vec` with no refcount and re-broadcasts every subscribe/cancel
   (`socket/actor/endpoints.rs:266-302`), so through the proxy the cancel propagates upstream and
   kills the topic for all. libzmq refcounts and only forwards the *last* unsubscribe
   (`libzmq/src/xsub.cpp`, `xpub.cpp` mtrie).
2. **No unsubscribe when a subscriber disconnects** → upstream subscription leak. libzmq synthesises
   unsubscribes on pipe termination (`xpub.cpp::xpipe_terminated`); omq never does. Confirmed: after
   a subscriber drops, its topics stay active upstream forever (50/50 leaked in the churn test).
3. **No subscribe de-duplication.** Five subscribers on topic `A` send **five** SUBSCRIBEs upstream;
   libzmq sends one. Amplifies upstream traffic and breaks last-value-cache patterns.
4. **Any non-subscription message from the XPUB side terminates the proxy.** omq's XSUB `send`
   rejects anything that isn't a single `\x00`/`\x01` frame with `Error::Protocol`
   (`socket/handle.rs:1343-1365`), and `Proxy::try_forward` propagates the error and exits
   (`proxy.rs:332`). A subscriber-side peer that sends a data frame, an **empty** frame, or a
   multipart message brings the whole broker's forwarding loop down. libzmq forwards such messages
   upstream (XPUB→XSUB is a normal data path). Confirmed: the omq proxy process exits with
   `PROXY EXIT: Err(Protocol("XSUB..."))`.

Items 1–3 also mean **XPUB delivers every SUBSCRIBE/CANCEL to the application** with no first/last
dedup, so hand-rolled brokers built on omq XPUB inherit the same divergence.

### H1 — Large-frame read blocks the driver's event loop: no heartbeats, timeouts, or cancellation mid-message. **High. Code-confirmed (`driver.rs:1515-1535, 2113-2149`).**

For any frame ≥ 128 KiB the body is read by an unbounded `while buf.len() < target { read_buf().await }`
loop *inside* the reader's `select!` arm. A `select!` arm body is not cancellable once entered, so
while a large frame is incomplete the sibling arms — cancellation, the heartbeat tick, and all
writes — cannot fire. A peer that sends a large-frame header and then dribbles or stops the body
stalls that whole driver task: no PING is sent, the receive-timeout check can't run, and TCP
keepalive is off by default. libzmq's decoder is fully non-blocking and its heartbeat timer still
fires mid-message (`libzmq/src/stream_engine_base.cpp:220-310, 735-754`). Combined with C1's
allocation, each such connection also pins its buffer for the stall. (I confirmed the code path;
the wedged-heartbeat timing was not separately reproduced.)

### H2 — A single slow or stalled publisher can freeze subscription propagation for the whole broker. **High. Code-confirmed; end-to-end effect likely.**

The XSUB actor awaits each ready publisher's bounded 64-slot inbox when broadcasting a subscription
(`socket/actor/endpoints.rs:291-300`) and when replaying subscriptions to a new publisher
(`socket/actor/peer.rs:584-591`). If one publisher's inbox backs up (slow link, or stalled mid-frame
per H1), the actor blocks, the XSUB `cmd_tx` fills, the proxy stops reading the XPUB side
(`proxy.rs:210-211`), the XPUB receive queue fills, and the XPUB actor blocks in `recv_tx.send`
(`peer.rs:543-552`). Net effect: subscription handling for *all* peers proceeds at the pace of the
slowest publisher — an estimated ceiling of ~64 subscriptions per slow-publisher round-trip — and a
publisher stopped mid-frame can block it indefinitely (heartbeats never evaluated, `close()` hangs).
libzmq writes subscriptions to pipes without blocking and drops at HWM (`xsub.cpp:262-280`).

### H3 — XPUB actor stall stops admission/cleanup and can trigger false heartbeat timeouts. **High. Code-confirmed; timing likely.**

XPUB is not in `can_bypass_actor_recv` (`socket/actor/peer_materialize.rs:739-753`), so every inbound
subscription and message goes through the actor's blocking `recv_tx.send`. While blocked it services
none of `peer_out`, `internal_rx`, or `cmd_rx`: new subscribers never get `ActivateDataPlane`,
`PeerClosed` is not processed, and the accept loop stalls. Subscriber drivers that must emit
(a SUBSCRIBE, a PONG) block outside their `select!`, so they write no data and send no PINGs;
when released their `last_input` is stale and the next heartbeat tick can return `Error::Timeout`
for healthy peers, feeding a reconnect/resubscribe storm. With `xpub_nodrop` this composes with H2
into a permanent deadlock.

### H4 — Actor `select!` is biased toward `cmd_rx`; a subscription storm starves peer events and can deadlock. **High. Starvation code-confirmed (`socket/actor/mod.rs:354-388`).**

The actor polls `cmd_rx` before peer/internal events. A proxy-fed subscription storm keeps `cmd_tx`
non-empty, so publisher Accepted/HandshakeSucceeded/PeerClosed events wait until it drains. If
`peer_out` (capacity 256, shared by all connections) fills while the actor is mid-broadcast, a ready
driver blocks in `emit_connection_events` while the actor blocks pushing to that driver's inbox —
an actor↔driver deadlock an attacker can drive by flooding SUBSCRIBEs on one side and subscriptions
on the other.

### H5 — Subscription bookkeeping is O(N) per operation and O(N) per publisher handshake. **High. Code-confirmed.**

`apply_subscription` does a linear `Vec` scan per subscribe and a scan + `Vec::remove` per cancel
(`endpoints.rs:276-282`); every publisher handshake clones the entire subscription `Vec`
(`peer.rs:405`) and then does N awaited sends. For 10k–100k topics this is seconds to tens of
seconds of actor time on the single IO thread, blocking all other work, and every duplicate
subscribe is re-broadcast to every publisher. libzmq uses a refcounted radix trie
(`xsub.cpp`, `xpub.cpp`) with cost proportional to topic length. In the storm test, omq forwarded
0/20k subscriptions within 60 s in the default config (the C2 livelock dominates first); libzmq
forwarded all 20k in well under a second.

### H6 — `xpub_nodrop` is not actually no-drop for CURVE / WebSocket / inproc / mixed-compression subscribers. **High. Code-confirmed.**

`xpub_nodrop` is the intended way to get libzmq's `ZMQ_XPUB_NODROP` backpressure (return "would
block" instead of dropping). It only works for the fast TCP slot path. Subscribers that go through
the **driver inbox fallback** instead of a transmit slot — CURVE (no slot,
`peer_materialize.rs:523`), WebSocket, inproc, and any peer whose compression differs from the first
lane peer (`routing/fan_out.rs:475-488`) — silently drop under Block mode: a full inbox send is
discarded (`routing/fan_out/fallback.rs:102-104`) and a Full fallback returns `Ok`
(`fallback.rs:25-43`). Also, `try_send` returns `Full` only when **lane 0's ring** is full
(`lane.rs:474-509`), not when a subscriber is full — the opposite of what the option's doc claims
(`options.rs:290-293`). libzmq returns `EAGAIN` if *any* matching pipe is at HWM and sends nothing
(`xpub.cpp:314-325`). So a CURVE or WSS pub/sub deployment that relies on `xpub_nodrop` for
losslessness still loses messages to any subscriber that lags by 64.

### H7 — A heartbeat/PONG can be written *inside* a data frame, corrupting the subscriber's stream. **High. Code-confirmed by audit; not reproduced here.**

A transmit-slot drain moves at most 1024 chunk entries per turn (`engine/driver.rs:836`,
`omq-proto/src/frame_buffer.rs:384`), and a multipart message is several entries (header + payload
per part). Queued commands (an auto-PONG, `inbound.rs:321-325`, or an outgoing PING,
`driver.rs:1622`) are written as soon as the current `pending_write` empties (`driver.rs:1985-1986`),
which can fall **between a frame header and its payload** when a message straddles the 1024-entry
drain boundary. libzmq always finishes the current frame before its ping/pong hooks run
(`libzmq/src/stream_engine_base.cpp:329-347`). Effect: with heartbeats enabled on either side and a
slot holding 1024+ chunks, a subscriber can receive a corrupted payload and then desync.
**This interacts badly with the C3 fix:** raising `transmit_slot_cap` (needed to stop the drops) makes
a slot hold many more large messages, making the 1024-chunk boundary — and this corruption — *more*
likely. Heartbeats are off by default in my test servers, so my byte-for-byte verifier did not
exercise this path (all runs showed `corrupt=0`); it should be treated as an open data-integrity risk
for any deployment that enables `heartbeat_interval`.

### H8 — WebSocket / mixed-compression subscribers can be muted permanently after one inbox overflow. **High (only if `ws` or mixed lz4/zstd endpoints are used). Code-confirmed.**

Fallback (non-slot) peers are deactivated on a full inbox (`fan_out.rs:113-128`) and filtered out of
future fan-out, but **reactivation only fires from a transmit-slot drain**
(`engine/transmit_slot.rs:320-333`) — which never happens for a peer that has no slot. So a WS or
mixed-compression subscriber that stalls for 64 messages once receives nothing again until it
reconnects. libzmq re-enables the pipe from the reader side (`libzmq/src/pipe.cpp:201-202`). There is
also a non-atomic window between the slot flag and inner flag (`fan_out.rs:122-127` vs the
reactivation callback `:524-533`) that can leave even a slot peer permanently `slot=active,
inner=inactive`.

### M1 — Native default `linger = 0` drops queued messages on close; libzmq default is infinite. **Medium. Code-confirmed (`options.rs:351`).**

The native API defaults `linger` to `Some(0)` — closing or dropping a socket discards all queued
outbound data immediately. libzmq defaults to `-1` (wait forever). The `omq-libzmq` C shim restores
`-1` by default, but native-API users (and anyone porting from libzmq) will silently lose in-flight
1 MB messages on shutdown unless they set linger explicitly. With linger > 0 the `is_drained` check
doesn't account for bytes already staged in the driver, so the last per-subscriber batch (up to the
slot cap) can still be lost.

### M2 — `Context::block_on(proxy(...))` serializes the whole context. **Medium. Code-confirmed (`context.rs:115-130`).**

The owned-runtime job loop runs one job to completion before the next, so running the proxy via
`ctx.block_on(...)` (the `--run-on ctx` mode, and the obvious way to host a proxy) makes every other
blocking call on that Context — bind, connect, subscribe, close — hang for the proxy's lifetime.
Use a separate runtime to drive the proxy.

### M3 — Pending-handshake cap (128) + 30 s handshake timeout enables an easy lockout. **Medium. Code-confirmed (`options.rs:26`, `peer.rs:113-124`).**

128 idle TCP connections that complete TCP but stall the ZMTP handshake occupy all
pending-handshake slots for 30 s, locking out every legitimate publisher and subscriber. libzmq has
no equivalent global cap. Any accept error also sleeps 50 ms (`endpoints.rs:351-354`).

### L1 — One detached tokio task spawned per XPUB SUBSCRIBE. **Low. Code-confirmed (`peer.rs:514-525`).**

`count_subscription_after` spawns a task per subscribe just to bump the `wait_subscribed` counter; a
100k-subscription storm creates 100k tasks that pile up while the lane control ring is stuck.

### The `omq-libzmq` C shim (drop-in `libzmq`) adds its own gaps

If you deploy omq as a libzmq replacement (`libomq_zmq.so`) rather than through the Rust API, these
are on top of C1–C3 and H1–H8 (which all apply — the shim is a thin wrapper over omq-tokio):

* **SH1 — silently ignored options, several security-relevant. High. Code/grep-confirmed
  (`omq-libzmq/src/opts.rs:907-970`).** `zmq_setsockopt` returns success (0) but does nothing for a
  long list of options. The dangerous ones for a broker: `ZMQ_XPUB_MANUAL` and `ZMQ_XPUB_WELCOME_MSG`
  (a broker using manual subscription approval to enforce topic ACLs gets **no** enforcement — every
  subscription is auto-applied), `ZMQ_TCP_ACCEPT_FILTER` / `ZMQ_IPC_FILTER_*` (address ACLs ignored),
  the `ZMQ_GSSAPI_*` family (a socket meant to use GSSAPI silently runs **NULL** = plaintext,
  unauthenticated), `ZMQ_SOCKS_*` (proxy bypassed), `ZMQ_BINDTODEVICE`, and `ZMQ_INVERT_MATCHING`
  (subscribers silently get non-matching data). `ZMQ_XPUB_VERBOSE`/`VERBOSER` are ignored, and
  `ZMQ_ROUTER_HANDOVER` is forced on. A program that sets any of these and checks the return value
  gets no error.
* **SH2 — CURVE ZAP is fail-open unless `ZMQ_ZAP_DOMAIN` is set. High (security). Confirmed in
  source (`socket.rs:418-429`).** A `ZMQ_CURVE_SERVER` with a bound ZAP key-allowlist but no ZAP
  domain skips the ZAP handler entirely (`needs_zap` is false), so **every** client keypair is
  admitted. libzmq consults a bound handler regardless of domain. This silently disables CURVE
  authentication for the common "set curve_server, install ZAP allowlist" pattern.
* **SH3 — options are frozen when the socket first materialises. High. Confirmed
  (`socket.rs:492`, only caller of `to_options()`).** Bind, connect, `ZMQ_SUBSCRIBE`, a monitor, or
  `zmq_proxy` freezes the option set; later `zmq_setsockopt` returns 0 and `zmq_getsockopt` echoes the
  new value but nothing changes (except LINGER, SND/RCVTIMEO, SUBSCRIBE). So
  `zmq_socket_monitor()` before `ZMQ_SNDHWM`/`ZMQ_XPUB_NODROP` silently drops both settings — a very
  easy mistake that turns C3's mitigation into a no-op.
* **SH4 — `ZMQ_SNDHWM`/`ZMQ_RCVHWM = 0` is rejected with EINVAL. High. Confirmed
  (`opts.rs:444-461`).** In libzmq `0` means *unlimited*; brokers set it to avoid drops. Here the call
  fails, and code that ignores the return keeps the default 1000 and the C3 drops.
* **SH5 — `ZMQ_FD` never clears / `ZMQ_EVENTS` always reports writable. High. Code-confirmed
  (`opts.rs:1202-1212`, `poll.rs:68-70`).** Reading `ZMQ_EVENTS` or a `DONTWAIT` recv does not drain
  the readiness eventfd, and POLLOUT is always set. Level-triggered integrations (pyzmq asyncio,
  libuv `uv_poll`) therefore **spin at 100% CPU**. Edge-triggered epoll is fine. `zmq_poll` also
  swallows EINTR and can starve raw fds at timeout 0.
* **SH6 — `ZMQ_MAXMSGSIZE` semantics differ. Medium. Confirmed
  (`omq-proto/src/proto/connection/inbound.rs:198-214`, shim `send_recv.rs:376,484`).** omq enforces
  the limit as the **sum of all frames + 64 B/part** and also on the **send** side (returns EMSGSIZE);
  libzmq checks each frame individually and does not enforce it on send. An exactly-1 MiB message can
  be rejected when `MAXMSGSIZE=1 MiB`.
* **SH7 — thread-ownership and shutdown hazards. Medium/High. Code-confirmed.** Release builds of the
  per-socket `LocalCell` have **no** ownership check (`local_cell.rs:43-57`), so two threads touching
  one socket race on the yring consumer (heap corruption); `zmq_close` from another thread while a
  recv is blocked is a use-after-free (`socket.rs:599,637,658` vs `send_recv.rs:344-356`). Any panic
  crossing the C ABI aborts the process. eventfds/pipes are created without `CLOEXEC`
  (`notify.rs`), so fork+exec leaks them and a forked child hangs. `zmq_proxy` only checks for
  context termination when both directions are idle, so it may never exit after `zmq_ctx_term` under
  sustained traffic, and its capture socket drops instead of blocking (libzmq blocks).

(The full ~90-entry option table is in `results/agent_notes/` for reference; the above are the ones
that change security or data-path behaviour for an XPUB/XSUB broker.)

### Non-findings verified (so they can be closed out)

* **No memory-unsafety.** `omq-proto` and `omq-tokio` are `#![forbid(unsafe_code)]`; every issue
  above is logical, not UB. Pooled-buffer aliasing is sound (buffers return to the pool only after
  the last `Bytes` clone drops).
* **u64→usize frame lengths are checked** (`frame.rs:243-250`), safe on 32-bit.
* **Frame flags validation is correct and slightly stricter than libzmq** (reserved bits and
  COMMAND+MORE rejected); pre-handshake command frames are capped at 256 KiB regardless of
  `max_message_size`, so C1 requires completing the (trivial NULL) handshake first.
* **Multipart-accumulation DoS not demonstrated.** The code lacks a byte cap on `pending_parts` when
  `max_message_size` is `None` (`inbound.rs:281-312`), but an 8 s flood of MORE-flagged frames on one
  connection left server RSS flat at ~5 MiB — per-connection TCP flow control bounded it in practice.
  Reported as a latent gap, not a live DoS.
* **Exclusive (`&mut self`) sockets are not affected by C1/H1** — they use only the safe incremental
  codec path. The crash/stall is specific to the actor-driven sockets that back an XPUB/XSUB broker.
* **Large inbound transfers do not trip heartbeat/handshake timeouts** on a healthy slow link (any
  received byte refreshes `last_input`) — more lenient than libzmq, not a bug.

## Performance (100 KB – 1 MB)

All numbers from the independent `xbench` client. `tput`/`pingpong` are medians of 3 runs;
`fanout4`/`slowsub`/`flood` are 1–2 runs (the effects are large and stable). `MB/s` is payload
throughput as seen by the subscriber; "loss" is verified message loss; "corrupt/dup/reorder" was
**0 everywhere** — when omq delivers a message, the bytes are correct and in order (with heartbeats
off; see H7). Full records in `results/bench.jsonl`.

**Closed-loop throughput, 8 messages in flight, 1 subscriber** — the core result:

| size | server | msg/s | MB/s | loss (med/worst) | p50 µs | p99 µs | RSS MB |
|---|---|---|---|---|---|---|---|
| 100KB | libzmq-c | 16,693 | 1,709 | 0% / 0% | 432 | 1,299 | 7.1 |
| 100KB | omq-tokio | 15,159 | 1,552 | **14% / 16%** | 378 | 1,108 | 6.7 |
| 100KB | omq-tokio+slotcap | 16,493 | 1,689 | 0% / 0% | 391 | 1,034 | 6.7 |
| 256KB | libzmq-c | 8,729 | 2,288 | 0% / 0% | 819 | 2,176 | 8.6 |
| 256KB | omq-tokio | 3,962 | 1,039 | **60% / 65%** | 471 | 1,258 | 7.6 |
| 256KB | omq-tokio+slotcap | 8,071 | 2,116 | 0% / 0% | 796 | 2,135 | 7.6 |
| 512KB | libzmq-c | 4,883 | 2,560 | 0% / 0% | 1,383 | 4,255 | 10.3 |
| 512KB | omq-tokio | 2,877 | 1,509 | **49% / 54%** | 865 | 2,335 | 9.6 |
| 512KB | omq-tokio+slotcap | 4,531 | 2,376 | 0% / 0% | 1,446 | 4,041 | 9.6 |
| 1MB | libzmq-c | 2,875 | 3,015 | 0% / 0% | 2,155 | 6,589 | 14.3 |
| 1MB | omq-tokio | 2,035 | 2,134 | **4% / 6%** | 3,512 | 6,885 | 13.7 |
| 1MB | omq-tokio+slotcap | 2,106 | 2,209 | 0% / 0% | 3,584 | 6,795 | 13.6 |

(rust-zmq tracks libzmq-c within noise and is omitted; `zeromq`/zmq.rs delivered **0** at every size
≥ 100 KB and its process died at 512 KB.) Takeaway: **default omq drops 4–60%; with the slot cap
raised it matches libzmq's throughput at 0% loss**, trailing by only 3–10% except a wider gap
appearing at 1 MB latency.

**Round-trip latency, 1 in flight (never hits the slot cap, so no drops):**

| size | libzmq-c p50/p99 | omq-tokio p50/p99 | zeromq p50/p99 |
|---|---|---|---|
| 100KB | 150 / 262 µs | 154 / 289 µs | 116 / 196 µs |
| 256KB | 199 / 390 µs | 195 / 408 µs | 191 / 387 µs |
| 1MB | 492 / 910 µs | 728 / 1,264 µs | dead |

omq is within noise up to 256 KB and ~1.5× libzmq's latency at 1 MB. zmq.rs is fastest at small
sizes but cannot do 1 MB.

**Fan-out to 4 subscribers, 8 in flight** (aggregate MB/s delivered):

| size | libzmq-c | omq-tokio | omq-tokio+slotcap | omq-hardened |
|---|---|---|---|---|
| 100KB | 2,680 MB/s, 0% | 3,039, **14%** | 3,236, 0% | 2,796, 0% |
| 256KB | 2,784, 0% | 2,379, **64%** | 3,093, 0% | 3,641, 0% |
| 1MB | 4,058, 0% | 3,111, **1.8%** | 3,415, 0% | **5,172, 0%** |

With the fixes applied, omq's fan-out is lossless and `omq-hardened` (io_threads=2) actually
**beats libzmq's aggregate fan-out throughput at 256 KB–1 MB** — the multi-lane design pays off once
it isn't dropping.

**One fast + one slow (5 ms/msg) subscriber — the FAST subscriber (isolation + memory):**

| size | server | fast msg/s | fast loss | server RSS |
|---|---|---|---|---|
| 256KB | libzmq-c | 10,299 | 0% | 252 MB |
| 256KB | omq-tokio | 3,996 | **60%** | 8 MB |
| 256KB | omq-tokio+slotcap | 7,632 | 0% | 136 MB |
| 1MB | libzmq-c | 353 | 0% | **913 MB** |
| 1MB | omq-tokio | 1,910 | **5%** | 16 MB |
| 1MB | omq-tokio+slotcap | 1,651 | 0% | **140 MB** |

Two things: default omq makes the *fast* subscriber pay for the slow one (C3 drops); but once the
slot cap is set, omq isolates the slow subscriber at **0% fast-subscriber loss using ~6× less broker
memory than libzmq** (140 MB vs 913 MB at 1 MB), because it bounds per-subscriber buffering instead
of queueing a full HWM of 1 MB messages. That bounded-memory behaviour is a genuine advantage of the
omq design — it just needs the cap set high enough to stop dropping.

**Open-loop flood (publisher outruns the broker):**

| size | server | MB/s | loss | RSS |
|---|---|---|---|---|
| 100KB | libzmq-c | 1,329 | 0% | 39 MB |
| 100KB | omq-tokio | 283 | **92%** | 127 MB |
| 100KB | omq-hardened | 1,962 | **24%** | 161 MB |
| 1MB | libzmq-c | 1,720 | 2.5% | 583 MB |
| 1MB | omq-tokio | 1,776 | 18% | 33 MB |
| 1MB | omq-tokio+slotcap | 1,927 | **0%** | 37 MB |

Size-dependent: libzmq propagates backpressure to the publisher (via the client's `XPUB_NODROP`) and
loses little; omq's fan-out is lossy by default and does not backpressure the publisher without
server-side `xpub_nodrop` (which has the H6 caveats), so at 100 KB it drops heavily. At 1 MB, though,
omq+slotcap is **lossless with 16× less memory than libzmq** — the lower message rate lets its drain
keep up. For a flood-prone 100 KB workload, omq needs `xpub_nodrop` and the H3/H6 caveats apply.

**Sustained churn** (`churn_soak.py`: 20 s of steady 256 KB traffic, 4 in flight, while ~100
subscribers and ~30 publishers connect/subscribe-random-topics/disconnect): `omq-hardened` held
**0% loss, 0 corruption, stable RSS (~94→136 MB), and slightly higher throughput than libzmq**
(5,490 vs 5,077 msg/s), i.e. the livelock/leak issues don't bite under *moderate* churn once the
knobs are set. Default `omq-tokio` dropped 31% (C3). None of the servers crashed or leaked
unboundedly over this window — the H2–H5 liveness failures need the specific triggers (a stalled
peer, >64 events per poll, 10k+ topics), not ordinary churn.


## Addendum: rzmq 0.5.26, and does io_uring make it faster?

### Short answer

* **io_uring is not why libzmq beat omq — neither uses it.** omq-tokio runs on Tokio's epoll
  reactor; omq's io_uring backend (`omq-compio`) was *removed* in the 2026-07-10 release
  (`omq.rs/CHANGELOG.md:363-366`). libzmq is epoll too.
* **rzmq really does use io_uring** (verified below), and it helps **only at the small end of this
  range**: at 100 KB it lifts rzmq's throughput +36–50% (to libzmq parity), gives the best 100 KB
  latency of anything tested (121 µs p50), and the best 4-way fan-out (+45% over libzmq). **From
  256 KB up it buys nothing**: libzmq on plain epoll is ~1.6× faster than rzmq's best configuration
  at 256 KB–1 MB, and rzmq's *default* io_uring setup is slower than its own Tokio mode at 1 MB.
* **rzmq cannot be an XPUB/XSUB broker.** It has no XPUB/XSUB socket types and no proxy function
  (its README: "No built-in high-level proxy function"). The closest it can do is a subscribe-all
  SUB→PUB forwarder — the same shape as `omq-hardened`, with the same loss of upstream subscription
  propagation.

### How it was tested

* rzmq 0.5.26 (crates.io, 2026-09-27) with the `io-uring` feature; server
  `servers/src/bin/rzmq_proxy.rs`: SUB bound on the publisher side subscribing to everything, PUB
  bound on the subscriber side (rzmq filters publisher-side), whole-message
  `recv_multipart`/`send_multipart` loop. Same independent `xbench` client and conformance suite.
* Variants: `rzmq-tokio` (library defaults); `rzmq-uring` (io_uring sessions + multishot receive,
  adaptive throttle off — how rzmq's own benchmark configures it); `rzmq-uring-zc-w2` (+ zero-copy
  send, 2 io_uring workers — the best all-rounder from the sweep); `rzmq-uring-zc-w2-drop` (same, PUB
  `SNDTIMEO=0`).
* **io_uring was verified to be carrying the data.** With strace and per-thread CPU during 1 MB
  traffic: in io_uring modes the sockets are driven by `io_uring_enter` on the `rzmq-io-uring-w`
  thread with no `recvfrom`/`writev` at all; in Tokio mode it is `recvfrom`/`writev`/`epoll_wait` on
  the Tokio workers. io_uring is enabled on this kernel (`io_uring_setup` succeeds, no seccomp).
* libzmq and omq-hardened were **re-baselined in the same session** (the container was recycled
  between runs and the host came back 5–15% faster depending on size, so the tables below are not
  mixed with the earlier ones). Medians of 3 runs for throughput/latency, 2 for the rest.

### Tuning sweep (8 in flight, 1 subscriber, 1 run each)

| rzmq config | 100 KB msg/s | 1 MB msg/s | server CPU |
|---|---|---|---|
| Tokio (defaults) | 10,653 | 2,084 | ~95–120% |
| io_uring, 1 worker (rzmq default count) | **17,317** | 1,490 | ~100% (worker pegged) |
| io_uring, 2 workers | 15,149 | 1,868 | ~80–130% |
| io_uring + zero-copy, 2 workers | 16,094 | 1,989 | ~100–130% |
| io_uring + SQPOLL | 17,205 | 1,993 | 160–190% |
| io_uring, 2 workers, busy-poll strategy | 11,474 | 1,956 | ~190% |
| "everything on" (ZC + SQPOLL + busy-poll) | **1,008** | 804 | 324–346% |

On a 4-vCPU box the spinning options oversubscribe the CPUs the client also needs; "everything on"
collapsed.

### Results vs libzmq and omq (same session)

**Throughput, 8 in flight, 1 subscriber (msg/s):**

| size | libzmq | omq-hardened | rzmq Tokio | rzmq io_uring | rzmq io_uring+ZC, 2w |
|---|---|---|---|---|---|
| 100 KB | 17,572 | 15,874 | 11,598 | 15,735 | **17,413** |
| 256 KB | **10,698** | 7,470 | 6,697 | 6,648 | 6,825 |
| 512 KB | **6,260** | 4,296 | 3,167 | 3,175 | 3,789 |
| 1 MB | **3,274** | 2,300 | 1,991 | 1,516 | 1,993 |

**Round-trip latency, 1 in flight (p50 / p99 µs):**

| size | libzmq | omq-hardened | rzmq Tokio | rzmq io_uring |
|---|---|---|---|---|
| 100 KB | 146 / 222 | 151 / 228 | 142 / 226 | **121 / 194** |
| 256 KB | 183 / 277 | 184 / 282 | 242 / 522 | 181 / 308 |
| 512 KB | **262 / 479** | 267 / 441 | 336 / 961 | 384 / 576 |
| 1 MB | **435 / 700** | 681 / 966 | 784 / 1,686 | 728 / 1,052 |

io_uring clearly tightens rzmq's tails (1 MB p99 1,686 → 1,052 µs) but doesn't close the gap to
libzmq above 256 KB.

**Fan-out to 4 subscribers, 8 in flight (msg/s per subscriber, 0% loss for all):**

| size | libzmq | omq-hardened | rzmq Tokio | rzmq io_uring | rzmq io_uring+ZC, 2w |
|---|---|---|---|---|---|
| 100 KB | ~6,830 | ~8,200 | ~6,780 | ~8,400 | **~9,900** |
| 1 MB | ~1,200 | **~1,310** | ~1,050 | ~690 | ~1,070 |

**Open-loop flood (msg/s delivered; loss):** at 100 KB rzmq Tokio delivered 24,209 msg/s with 0%
loss vs libzmq 13,016 (≈0%) and omq-hardened 18,356 (35% loss) — rzmq's write batching shines when
there is a deep queue to batch. At 1 MB: libzmq 3,006 (0%), rzmq io_uring+ZC 2,529 (0%), rzmq
Tokio 2,395 (0%), omq-hardened 2,474 (22–34% loss), rzmq io_uring 1-worker 1,604.

### Why io_uring doesn't help 100 KB–1 MB messages much here

1. **Large messages are copy-bound, not syscall-bound.** A 1 MB message costs a handful of
   syscalls but ~1 MB of memory copying at several points (kernel loopback copy, userspace
   framing). io_uring removes syscall and context-switch overhead; it does not remove copies. At
   100 KB the fixed per-message overhead is a bigger share, which is exactly where it helped.
2. **rzmq's "zero-copy" send isn't zero-copy end to end.** It first copies each message into a
   pre-registered send buffer (`io_uring_backend/worker/cqe_processor.rs:160-180`,
   `acquire_and_prep_buffer` → `len_copied`) and then issues `SEND_ZC`; on loopback the kernel copies
   again when it delivers to the local receiver.
3. **One io_uring worker by default.** rzmq uses `ceil(ncpu/2) − 2`, minimum 1 → **1 worker** on
   4 vCPUs. All connections funnel through it; it sat at 100% CPU at 1 MB while Tokio mode spread the
   same work across two threads. A second worker recovers Tokio-level throughput, not more.
4. **Cross-thread handoffs.** Socket logic runs on Tokio, I/O on the io_uring worker: ~2,600 futex
   wake-ups/s at 1 MB.

(Caveat: this is 4 vCPUs over loopback. With more cores and a real NIC, io_uring's syscall savings
matter more for small messages; for 100 KB–1 MB they stay secondary to copying.)

### rzmq production concerns for this use case

* **No XPUB/XSUB, no proxy** → no upstream subscription propagation; publishers send everything to
  the broker. Same trade-off as `omq-hardened`.
* **Head-of-line blocking by default — the most important one.** rzmq's PUB, on a full subscriber
  pipe, *awaits* instead of dropping (`SNDTIMEO` defaults to infinite: `sessionx/iface.rs:104-112`).
  With one slow subscriber (5 ms/message) the **fast** subscriber fell from ~16–18k to **~195 msg/s
  at 100 KB**, with multi-second p99 — every subscriber runs at the slowest one's pace. libzmq and
  omq-hardened kept the fast subscriber at full speed.
  **Fix: set `SNDTIMEO=0` on the PUB** (libzmq-style drop). Confirmed: 100 KB fast subscriber back
  to 16,190 msg/s; at 1 MB it improves to 448 msg/s (vs 208 default) but stays far below
  omq-hardened's 2,113, and ~575 MB is queued for the slow subscriber (libzmq: ~850 MB).
* **Robustness is better than omq out of the box:** survives the C1 oversized-frame attack in both
  modes; no livelock with 65 topics or 10 subscribers × 10 topics; **0% loss in every closed-loop
  and flood run** (it blocks rather than drops); a stray data/empty/multipart frame from a subscriber
  is ignored rather than killing the forwarder.
* **Conformance: 15/21 in both modes** — the same 6 upstream-propagation scenarios `omq-hardened`
  fails, all by design of a subscribe-all forwarder. Every data-delivery scenario passes, including
  the 50 × 512 KiB burst (all delivered).
* From its own docs: "Beta"; announces ZMTP 3.0 (interoperates with libzmq via legacy
  `\x01topic` subscriptions); no ZAP; limited libzmq option parity; no `zmq_poll`/`zmq_proxy`.

### Bottom line on rzmq

rzmq + io_uring is genuinely fast for ~100 KB messages with healthy consumers (at or above libzmq,
best-in-class latency and fan-out) and is more robust out of the box than omq. But for 256 KB–1 MB
it is ~1.6× slower than libzmq whether or not io_uring is on, it cannot be a faithful XPUB/XSUB
broker, and its default blocking PUB lets one slow subscriber stall everyone unless you set
`SNDTIMEO=0`. It is not a drop-in replacement for a libzmq XPUB/XSUB broker either.

## If you must run omq for this today (native API)

In priority order, all confirmed to help here:

1. **`Options::max_message_size = Some(N)`** (e.g. 16–64 MiB). Closes the C1 process-abort and bounds
   H1/multipart memory. Effectively mandatory. Pick N ≥ your largest legitimate message.
2. **`Options::transmit_slot_cap = Some(N)`** with N a few MiB above your largest message (I used
   64 MiB). Removes the C3 drops (0% loss at every size in my runs). Costs a few MiB of buffering per
   subscriber.
3. **`ContextConfig { io_threads: 2 }`** (or more). Removes the C2 livelock in tests.
4. **Do not use `omq_tokio::proxy::proxy(XSUB, XPUB)` as a real broker.** Its subscription semantics
   diverge (C4). If you control the publishers, subscribe the XSUB to everything once and filter on
   the XPUB (as `servers/src/bin/omq_proxy_hardened.rs` does), accepting that you lose upstream
   subscription propagation. If you need faithful last-value/refcount semantics, omq cannot provide
   them yet.
5. **Set `linger` explicitly** if you care about draining on shutdown (M1); the native default is 0.
6. **Leave heartbeats off** until H7 (mid-frame PONG corruption) is resolved, or keep slots small —
   but small slots reintroduce C3. This is an unresolved tension.
7. **Do not rely on `xpub_nodrop` for CURVE/WSS/inproc subscribers** (H6) — it does not backpressure
   them.

`omq-hardened` in this repo applies 1–4 and is the configuration the "hardened" rows below were
measured with. Note it still cannot fix the C-shim-only issues, and H1/H2/H7 remain.

Memory note (answering the large-payload memory question directly): under a **slow** subscriber omq
keeps broker RSS *low* (it drops rather than queues — e.g. 9 MB where libzmq grew to ~250 MB), which
is good for memory but means healthy subscribers also lose messages (C3). Under **backpressure/Block**
mode, however, each fan-out lane ring can hold up to `send_hwm` *raw* messages with no byte bound
(`routing/fan_out/lane.rs:227-246`) — ≈ `send_hwm × message_size` per socket, i.e. ~1 GiB at
`send_hwm=1000` and 1 MiB messages. Size `send_hwm` accordingly for large payloads.

## Reproducing

Everything is in `omq_review/`. Prereqs: a Rust toolchain, `libzmq-dev` 4.3.5, `pyzmq`, a C++
compiler. The omq crates are pulled from crates.io at the exact versions under review.

```
omq_review/
  servers/       omq_proxy, omq_proxy_hardened, libzmq_proxy (rust-zmq), zeromq_proxy (cargo build --release)
  c_proxy/       xproxy.c -> xproxy_libzmq (system libzmq), xproxy_omqc (omq's libomq_zmq.so)
  client/        xbench.cpp -> xbench   (independent libzmq load generator + byte-for-byte verifier)
  scenarios/     conformance.py, churn_soak.py, zmtp_raw.py, zmtp30_publisher.py  (pyzmq / raw sockets)
  scripts/       bench_matrix.py, runone.sh, summarize.py
  results/       bench.jsonl (raw), conformance.json, agent_notes/
  REPRODUCE.md   exact build + run commands
```

Key one-liners:

```sh
# C1 crash (any server; watch it die):
./scenarios/zmtp_raw.py 127.0.0.1 <xsub-port> PUB $((2**44)) 0

# C3 drops vs libzmq (compare loss %):
./scripts/runone.sh omq-tokio "" "--size 262144 --mode window --window 8 --duration 3" out.json
./scripts/runone.sh libzmq-c  "" "--size 262144 --mode window --window 8 --duration 3" out.json

# C2 livelock (server pins at 100% CPU, stops serving):
./scenarios/conformance.py --servers omq-tokio --only subscriber_with_65_topics

# Full semantics matrix across all 5 implementations:
./scenarios/conformance.py --json results/conformance.json
```

See `REPRODUCE.md` for the full build steps and the exact host used.

## Appendix: environment configured for this review

The review ran in a cloud container whose network policy allowed crates.io, GitHub, and the Ubuntu
archive (for `libzmq-dev` 4.3.5). The three reference repos (`paddor/omq.rs`, `zeromq/libzmq`,
`zeromq/zmq.rs`) were cloned read-only. No changes were pushed to any of them; all review artefacts
live on the working branch of `yzia2000/qr_cpp_compiler` under `omq_review/`.
