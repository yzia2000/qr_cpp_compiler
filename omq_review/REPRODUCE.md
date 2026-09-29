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

## Patched rzmq (zero-copy receive, SENDMSG_ZC, non-blocking PUB)

```sh
# 1. upstream rzmq at the 0.5.26 release commit + rzmq_zc/patches (clones github.com/excsn/rzmq;
#    pass a local clone path to skip the fetch). Creates rzmq_zc/src-rzmq (gitignored).
rzmq_zc/prepare.sh

# 2. rzmq_proxy_zc = servers/src/bin/rzmq_proxy.rs built against the patched rzmq
(cd rzmq_zc/server && CARGO_TARGET_DIR=/home/user/rzmq-zc-target cargo build --release)
#    extra flag: --rcv-direct-threshold N   (patch default 32768; 0 = stock receive path)

# 3. rzmq's own tests on the patched tree, including the two new test files
(cd rzmq_zc/src-rzmq/core && cargo test --release --features io-uring --lib \
   --test io_uring_direct_recv --test pub_stalled_subscriber --test io_uring_resource_exhaustion \
   --test stress --test pub_sub --test push_pull --test maxmsgsize --test subscription_trie_contention \
   -- --test-threads=1)

# 4. proxy-level checks and the same-session comparison (XZC points at the patched binary)
export XZC=/home/user/rzmq-zc-target/release
python3 scenarios/conformance.py --servers rzmqzc-uring,rzmqzc-tokio
python3 scenarios/midframe_churn.py rzmqzc-uring --duration 45 --conns 1200   # disconnects mid-frame
python3 scenarios/zmtp_raw.py 127.0.0.1 <frontend-port> PUB $((2**44)) 200000   # C1 against rzmqzc-*
S=libzmq-c,omq-hardened,rzmq-uring,rzmq-uring-zc-w2,rzmqzc-uring-nodirect,rzmqzc-uring,rzmqzc-tokio
python3 scripts/bench_matrix.py --tests tput,pingpong --reps 3 --duration 4 --servers $S \
  --out results/bench_rzmq_zc.jsonl
python3 scripts/bench_matrix.py --tests fanout4,slowsub,flood --sizes 100KB,1MB --reps 2 --duration 4 \
  --servers $S --out results/bench_rzmq_zc.jsonl
python3 scripts/summarize.py results/bench_rzmq_zc.jsonl
```

Copy accounting (userspace bytes copied per forwarded byte) uses an `LD_PRELOAD` shim counting
`memcpy`/`memmove` calls of at least 4 KiB and `realloc` moves; `scripts/copyprobe.sh` wraps it:

```sh
gcc -O2 -shared -fPIC -o /tmp/libmemcount.so scripts/memcount.c -ldl -lpthread
MEMCOUNT_SO=/tmp/libmemcount.so scripts/copyprobe.sh "$XZC/rzmq_proxy_zc --mode uring --throttle off" \
  "--size 1048576 --mode window --window 8 --duration 3"
```

Which io_uring opcodes a server really submits (e.g. whether `SENDMSG_ZC` is used, and whether the
kernel reports it copied anyway) comes from the `io_uring:io_uring_submit_req` /
`io_uring_complete` tracepoints (`scripts/uring_trace.sh`, needs tracefs and root).

## NNG 1.12.4 (nanomsg-next-generation)

NNG speaks the SP protocol, not ZMTP, so it gets its own broker and a build of the same client
with the SP transport compiled in (`-DXBENCH_NNG`: identical payload, verification and output).

```sh
git clone https://github.com/nanomsg/nng && cd nng && git checkout v1.12.4
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
  -DNNG_TESTS=OFF -DNNG_TOOLS=OFF -DNNG_ENABLE_TLS=OFF -DCMAKE_INSTALL_PREFIX=$HOME/nng-install-1.12.4
ninja -C build install
N=$HOME/nng-install-1.12.4
gcc -O2 -I$N/include -o nng_proxy omq_review/c_proxy/nng_proxy.c -L$N/lib -lnng -Wl,-rpath,$N/lib -lpthread
g++ -O2 -march=native -std=c++17 -DXBENCH_NNG -I$N/include -o xbench_nng omq_review/client/xbench.cpp \
  -L$N/lib -lnng -Wl,-rpath,$N/lib -lpthread
# both binaries go in $XBIN next to xbench; bench_matrix.py uses xbench_nng for nng-* servers

S=libzmq-c,omq-hardened,rzmq-uring,nng-device,nng-device-tuned,nng-loop
python3 scripts/bench_matrix.py --tests tput,pingpong --reps 3 --duration 4 --servers $S --out results/bench_nng.jsonl
python3 scripts/bench_matrix.py --tests fanout4,slowsub,flood --sizes 100KB,1MB --reps 2 --duration 4 \
  --servers $S --out results/bench_nng.jsonl
python3 scripts/pivot.py results/bench_nng.jsonl --servers $S

# traffic a subscriber did not subscribe to (SP filters in the subscriber)
for s in libzmq-c omq-hardened rzmq-uring nng-device-tuned; do
  python3 scenarios/topic_filter_cost.py $s --json results/topic_filter_cost.jsonl; done
# a message header declaring a huge body (NNG allocates the declared size unless RECVMAXSZ is set)
python3 scenarios/sp_raw.py 127.0.0.1 <frontend-port> pub $((2**44)) 0
# userspace copies / zero-filled bytes per forwarded byte
XBENCH=xbench_nng MEMCOUNT_SO=/tmp/libmemcount.so scripts/copyprobe.sh "$XBIN/nng_proxy --recvbuf 1000" \
  "--size 1048576 --mode window --window 8 --duration 3"
```

## Files

- `servers/` — the four Rust proxy servers (one CLI). `omq_proxy` exposes `--io-threads`, `--hwm`,
  `--slot-cap`, `--max-msg-size`, `--xpub-nodrop`, `--run-on main|ctx`. `omq_proxy_hardened` bakes
  in the recommended workarounds.
- `c_proxy/xproxy.c` — libzmq C-API proxy, built against system libzmq and against omq's shim.
  `c_proxy/nng_proxy.c` — NNG pub/sub forwarder (`nng_device` or a recv/send loop).
- `client/xbench.cpp` — the independent load generator + verifier (`-DXBENCH_NNG` builds the
  SP/NNG variant, `xbench_nng`).
- `scenarios/` — `conformance.py` (black-box libzmq-semantics suite), `churn_soak.py`,
  `zmtp_raw.py` (raw ZMTP peer for the crash/oversized-frame tests), `zmtp30_publisher.py`.
- `scripts/` — `bench_matrix.py`, `runone.sh` (RSS/CPU sampling wrapper), `summarize.py`.
- `rzmq_zc/` — `patches/` (three patches against rzmq 0.5.26), `prepare.sh`, `server/` (builds
  `rzmq_proxy_zc`).
- `results/` — `bench.jsonl` (raw records), `conformance.json`, `agent_notes/`; rzmq:
  `bench_rzmq.jsonl`, `rzmq_sweep.jsonl`, `conformance_rzmq.json`; patched rzmq:
  `bench_rzmq_zc.jsonl`, `conformance_rzmq_zc.json`, `midframe_churn_rzmq_zc.json`, `copies_1mb.txt`.
