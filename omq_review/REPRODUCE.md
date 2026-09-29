# Reproducing the omq.rs XPUB/XSUB review

Host used: 4 vCPU Intel Xeon (2.10 GHz, Emerald Rapids-class) KVM guest, 16 GB RAM,
Ubuntu 24.04, Linux 6.18, all traffic over loopback TCP. Numbers are relative comparisons on
one shared box; medians of 2–3 runs.

## Versions under review (crates.io, 2026-09-27)

- `omq-tokio` 0.24.0, `omq-proto` 0.28.1, `omq-libzmq` 0.5.20
  (identical source to `paddor/omq.rs` `main`/`79cf9e0` for the XPUB/XSUB/proxy code;
  built from release commit `5e29747`).

## Prerequisites

```sh
# system libzmq 4.3.5 (the reference) + pyzmq + C/C++ toolchain
sudo apt-get install -y libzmq3-dev cppzmq-dev build-essential
pip3 install pyzmq
# Rust toolchain (any recent stable that supports edition 2024, >= 1.93)
```

## 1. Build the servers

```sh
# Rust servers: omq_proxy, omq_proxy_hardened, libzmq_proxy (rust-zmq), zeromq_proxy (zmq.rs)
cd omq_review/servers
cargo build --release        # binaries in target/release/

# C servers from one source, linked two ways:
cd ../c_proxy
gcc -O2 -o xproxy_libzmq xproxy.c -lzmq                      # system libzmq 4.3.5
# omq's libzmq drop-in: build libomq_zmq.so from the omq-libzmq crate first, then:
gcc -O2 -Ipath/to/omq-libzmq -o xproxy_omqc xproxy.c \
    -Lpath/to/libomq_zmq_dir -lomq_zmq -Wl,-rpath,path/to/libomq_zmq_dir
```

The five server labels (`libzmq-c`, `omq-c`, `rust-zmq`, `omq-tokio`, `zeromq`) plus
`omq-tokio+slotcap64M` and `omq-hardened` are what the scripts reference.

## 2. Build the independent client

```sh
cd omq_review/client
g++ -O2 -march=native -std=c++17 -o xbench xbench.cpp -lzmq -lpthread
```

`xbench` uses only the reference libzmq 4.3.5. It PUBs to the server's XSUB port and SUBs from the
server's XPUB port, verifying every received byte (sequence number stamped per 4 KiB page +
deterministic body), so it detects loss, duplication, reordering, and corruption independently of
the server under test.

## 3. Reproduce the headline findings

```sh
# Point the scripts at the binaries:
export XBIN=$PWD/omq_review/client:$PWD/omq_review/c_proxy   # (or copy all binaries into one dir)
export XTGT=$PWD/omq_review/servers/target/release

# C1 — one 9-byte frame crashes the broker (omq-tokio / omq-c / zeromq abort; libzmq survives):
python3 omq_review/scenarios/zmtp_raw.py 127.0.0.1 <xsub-port> PUB $((2**44)) 0

# C3 — large-message drops vs libzmq (compare loss %):
omq_review/scripts/runone.sh omq-tokio "" "--size 262144 --mode window --window 8 --duration 3" /tmp/o.json
omq_review/scripts/runone.sh libzmq-c  "" "--size 262144 --mode window --window 8 --duration 3" /tmp/l.json

# C2 — 65-topic subscriber livelocks the default context:
python3 omq_review/scenarios/conformance.py --servers omq-tokio --only subscriber_with_65_topics

# Full semantics matrix (5 implementations):
python3 omq_review/scenarios/conformance.py --json omq_review/results/conformance.json

# Performance matrix (medians -> markdown):
python3 omq_review/scripts/bench_matrix.py --tests tput,pingpong,fanout4,slowsub,flood --reps 3
python3 omq_review/scripts/summarize.py omq_review/results/bench.jsonl
```

## rzmq / io_uring addendum

```sh
# rzmq_proxy is built with the servers crate (rzmq 0.5.26, feature "io-uring"). Modes:
#   --mode tokio | uring | uring-zc   [--workers N] [--sqpoll] [--strategy performance|balanced|low_power]
#   [--cork] [--throttle on|off] [--sndtimeo MS]   (SNDTIMEO=0 => drop on a full subscriber)
# Needs a kernel with io_uring enabled (cat /proc/sys/kernel/io_uring_disabled -> 0).

# tuning sweep, then the same-session comparison vs libzmq and omq-hardened:
python3 scripts/bench_matrix.py --tests tput --sizes 100KB,1MB --reps 1 --duration 3 \
  --servers rzmq-tokio,rzmq-uring,rzmq-uring-w2,rzmq-uring-zc-w2,rzmq-uring-sqpoll,rzmq-uring-max \
  --out results/rzmq_sweep.jsonl
python3 scripts/bench_matrix.py --tests tput,pingpong --reps 3 --duration 4 \
  --servers libzmq-c,omq-hardened,rzmq-tokio,rzmq-uring,rzmq-uring-zc-w2 --out results/bench_rzmq.jsonl
python3 scripts/bench_matrix.py --tests fanout4,slowsub,flood --sizes 100KB,1MB --reps 2 --duration 4 \
  --servers libzmq-c,omq-hardened,rzmq-tokio,rzmq-uring,rzmq-uring-zc-w2 --out results/bench_rzmq.jsonl
python3 scripts/summarize.py results/bench_rzmq.jsonl
python3 scenarios/conformance.py --servers rzmq-tokio,rzmq-uring --json results/conformance_rzmq.json

# confirm io_uring is carrying the data (io_uring_enter on rzmq-io-uring-w, no recvfrom/writev):
strace -f -c -p <rzmq_proxy pid>   # while xbench runs
```

## Files

- `servers/` — the four Rust proxy servers (one CLI). `omq_proxy` exposes `--io-threads`, `--hwm`,
  `--slot-cap`, `--max-msg-size`, `--xpub-nodrop`, `--run-on main|ctx`. `omq_proxy_hardened` bakes
  in the recommended workarounds.
- `c_proxy/xproxy.c` — libzmq C-API proxy, built against system libzmq and against omq's shim.
- `client/xbench.cpp` — the independent load generator + verifier.
- `scenarios/` — `conformance.py` (black-box libzmq-semantics suite), `churn_soak.py`,
  `zmtp_raw.py` (raw ZMTP peer for the crash/oversized-frame tests), `zmtp30_publisher.py`.
- `scripts/` — `bench_matrix.py`, `runone.sh` (RSS/CPU sampling wrapper), `summarize.py`.
- `results/` — `bench.jsonl` (raw records), `conformance.json`, `agent_notes/`.
