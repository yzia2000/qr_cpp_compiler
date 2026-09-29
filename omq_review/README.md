# omq.rs XPUB/XSUB production review

An independent review of **omq.rs** (omq-tokio 0.24.0 / omq-proto 0.28.1 / omq-libzmq 0.5.20) as an
XPUB/XSUB message broker for 100 KB – 1 MB payloads, benchmarked and semantics-checked against
**libzmq 4.3.5** with a separate load-generator process.

- **[REPORT.md](REPORT.md)** — findings (bugs, correctness gaps, DoS vectors), performance tables,
  and the production-readiness verdict. Start here.
- **[REPRODUCE.md](REPRODUCE.md)** — exact build and run commands, and the host used.

Layout:

| dir | what |
|---|---|
| `servers/` | XSUB/XPUB proxy servers (omq native, omq hardened, rust-zmq, zmq.rs) plus an rzmq SUB→PUB forwarder (Tokio or io_uring), one CLI |
| `c_proxy/` | libzmq C-API proxy, built against system libzmq and against omq's `libomq_zmq.so` |
| `client/` | `xbench.cpp` — independent libzmq load generator + byte-for-byte verifier |
| `scenarios/` | pyzmq/raw-socket conformance, churn soak, and ZMTP edge-case peers |
| `scripts/` | benchmark matrix runner, per-run RSS/CPU sampler, results summarizer |
| `results/` | raw `bench.jsonl`, `conformance.json`, audit notes |

Headline: **not production-ready as a drop-in XPUB/XSUB broker in the default configuration** — a
single crafted frame crashes it, it drops 4–60% of large messages under trivial load, and 65 topics
from one subscriber wedge it. Three option changes remove those three issues and it then runs within
~1.4–2× of libzmq (and uses far less memory under slow consumers), but proxy-semantics divergence and
several liveness/integrity gaps remain, and two of the fixes aren't reachable from the C API. Full
detail and the nuances in [REPORT.md](REPORT.md).

**Addendum — rzmq and io_uring:** rzmq 0.5.26 (the pure-Rust implementation with an io_uring
backend) was benchmarked in the same harness. io_uring makes it fast at ~100 KB (at or above libzmq)
but not at 256 KB–1 MB, where libzmq on plain epoll stays ~1.6× ahead; rzmq has no XPUB/XSUB, and its
PUB blocks every subscriber behind a slow one unless `SNDTIMEO=0`. omq doesn't use io_uring at all.
Details in the REPORT addendum.

*This review was produced with LLM assistance; every "reproduced" finding was demonstrated with a
running binary (commands in REPRODUCE.md), and code-only findings carry file:line references.*
