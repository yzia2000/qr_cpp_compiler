#!/usr/bin/env python3
"""Performance matrix for XSUB/XPUB proxy servers with 100 KB - 1 MB payloads.

For every (test, server, size, rep) it starts a fresh server process, runs the
independent libzmq client (xbench) against it as a separate process, samples
the server's RSS and CPU from /proc, and appends one JSON record to
results/bench.jsonl.

Tests:
  tput     closed loop, window 8, 1 subscriber          -> throughput + loss
  tput32   closed loop, window 32, 1 subscriber
  pingpong closed loop, window 1                         -> round-trip latency
  fanout4  closed loop, window 8, 4 subscribers          -> fan-out throughput
  slowsub  window 8, 1 fast + 1 slow (5 ms/msg) sub      -> isolation + memory
  rate     open loop, fixed 20% of libzmq's tput         -> latency under load
  flood    open loop, publisher blocks on HWM (NODROP)   -> saturation + memory
"""
import argparse
import json
import os
import random
import subprocess
import sys
import time

XBIN = os.environ.get("XBIN", "/home/user/xbin")
XTGT = os.environ.get("XTGT", "/home/user/xproxy-target/release")
SERVERS = {
    "libzmq-c": [f"{XBIN}/xproxy_libzmq"],
    "rust-zmq": [f"{XTGT}/libzmq_proxy"],
    "omq-tokio": [f"{XTGT}/omq_proxy"],
    "omq-tokio+slotcap64M": [f"{XTGT}/omq_proxy", "--slot-cap", str(64 << 20)],
    "omq-hardened": [f"{XTGT}/omq_proxy_hardened"],
    "omq-c": [f"{XBIN}/xproxy_omqc"],
    "zeromq": [f"{XTGT}/zeromq_proxy"],
}
SIZES = {"100KB": 102400, "256KB": 262144, "512KB": 524288, "1MB": 1048576}
TESTS = {
    "tput": ["--mode", "window", "--window", "8"],
    "tput32": ["--mode", "window", "--window", "32"],
    "pingpong": ["--mode", "window", "--window", "1"],
    "fanout4": ["--mode", "window", "--window", "8", "--subs", "4"],
    "slowsub": ["--mode", "window", "--window", "8", "--subs", "1", "--slow-subs", "1",
                "--slow-delay-us", "5000"],
    "rate": ["--mode", "rate"],  # --rate filled per size
    "flood": ["--mode", "flood", "--pub-nodrop"],
}
# Fixed open-loop rates (msg/s): ~25-30% of the slowest healthy server's
# closed-loop throughput at each size, so every server should keep up.
RATES = {"100KB": 4000, "256KB": 2000, "512KB": 1000, "1MB": 500}
CLK = os.sysconf("SC_CLK_TCK")


def proc_cpu(pid):
    try:
        f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / CLK
    except OSError:
        return None


def proc_rss_kb(pid):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def run_one(server, test, size_name, rep, duration, out_dir):
    port = random.randint(20000, 60000)
    fe, be = f"tcp://127.0.0.1:{port}", f"tcp://127.0.0.1:{port + 1}"
    log = os.path.join(out_dir, f"srv-{port}.log")
    srv = subprocess.Popen(SERVERS[server] + ["--frontend", fe, "--backend", be],
                           stdout=open(log, "w"), stderr=subprocess.STDOUT)
    for _ in range(200):
        if "READY" in open(log).read():
            break
        time.sleep(0.01)
    cargs = [f"{XBIN}/xbench", "--pub", fe, "--sub", be, "--size", str(SIZES[size_name]),
             "--duration", str(duration), "--warmup", "1", "--label", server] + TESTS[test]
    if test == "rate":
        cargs += ["--rate", str(RATES[size_name])]
    jpath = os.path.join(out_dir, f"c-{port}.json")
    cargs += ["--json", jpath]
    c0, t0 = proc_cpu(srv.pid), time.time()
    cli = subprocess.Popen(cargs, stderr=subprocess.PIPE, text=True)
    peak, samples = 0, []
    while cli.poll() is None:
        r = proc_rss_kb(srv.pid)
        peak = max(peak, r)
        samples.append(r)
        time.sleep(0.1)
    c1, t1 = proc_cpu(srv.pid), time.time()
    cerr = cli.stderr.read()
    alive = srv.poll() is None
    srv.kill()
    srv.wait()
    slog = open(log).read()
    os.unlink(log)
    try:
        rec = json.load(open(jpath))
        os.unlink(jpath)
    except (OSError, ValueError):
        rec = {"error": "no client json", "client_stderr": cerr[-1500:]}
    rec.update(server=server, test=test, size_name=size_name, rep=rep,
               server_cpu_pct=round(100 * (c1 - c0) / (t1 - t0), 1) if c0 is not None and c1 is not None else None,
               server_peak_rss_mb=round(peak / 1024, 1),
               server_alive=alive and "PROXY EXIT" not in slog,
               server_log_tail=slog[-300:], client_rc=cli.returncode, ts=time.time())
    return rec, cerr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tests", default="tput,pingpong")
    ap.add_argument("--servers", default=",".join(SERVERS))
    ap.add_argument("--sizes", default=",".join(SIZES))
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--duration", type=float, default=5)
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", "results", "bench.jsonl"))
    a = ap.parse_args()
    out_dir = "/tmp/xbench-runs"
    os.makedirs(out_dir, exist_ok=True)
    with open(a.out, "a") as out:
        for rep in range(a.reps):  # reps outermost: spreads host noise across servers
            for test in a.tests.split(","):
                for size_name in a.sizes.split(","):
                    for server in a.servers.split(","):
                        rec, cerr = run_one(server, test, size_name, rep, a.duration, out_dir)
                        out.write(json.dumps(rec) + "\n")
                        out.flush()
                        subs = rec.get("subs_detail", [])
                        fast = [s for s in subs if not s["slow"]]
                        line = " ".join(
                            f"[{s['msgs_s']:.0f}/s loss={s['loss_pct']:.2f}% p50={s['lat_us']['p50']:.0f}us"
                            f" p99={s['lat_us']['p99']:.0f}us{' SLOW' if s['slow'] else ''}]" for s in subs)
                        print(f"rep{rep} {test:8s} {size_name:5s} {server:22s} cpu={rec['server_cpu_pct']}% "
                              f"rss={rec['server_peak_rss_mb']}MB alive={rec['server_alive']} {line}", flush=True)
                        if not fast:
                            print("   client stderr:", cerr[-400:].replace("\n", " | "), flush=True)


if __name__ == "__main__":
    main()
