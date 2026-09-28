#!/usr/bin/env python3
"""Churn soak: steady large-message traffic + subscriber/publisher churn.

- The independent xbench client (libzmq) drives steady traffic through the
  proxy in closed loop (window 1 by default, so no server-side queue ever
  holds more than one message) and measures loss/corruption/latency.
- In parallel this process churns peers on both sides of the proxy:
  every ~100 ms a SUB connects with a few random topics (+ the bench topic
  for half of them, so it also receives 256 KiB messages), reads briefly and
  disconnects; every ~500 ms an extra PUB connects, publishes a few small
  messages on a random topic and disconnects.
- Server RSS and CPU are sampled every second.

Usage: churn_soak.py SERVER [--duration 60] [--size 262144] [--window 1]
"""
import argparse
import json
import os
import random
import subprocess
import sys
import threading
import time

import zmq

sys.path.insert(0, os.path.dirname(__file__))
from conformance import Server  # noqa: E402

XBIN = os.environ.get("XBIN", "/home/user/xbin")
CLK = os.sysconf("SC_CLK_TCK")


def rss_mb(pid):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    except OSError:
        return None


def cpu_s(pid):
    try:
        f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / CLK
    except OSError:
        return None


def churn(srv, stop, stats):
    ctx = zmq.Context()
    last_pub = 0.0
    while not stop.is_set():
        s = ctx.socket(zmq.SUB)
        s.setsockopt(zmq.LINGER, 0)
        s.setsockopt(zmq.RCVHWM, 10)
        for _ in range(random.randint(1, 20)):
            s.setsockopt(zmq.SUBSCRIBE, b"churn-%d" % random.randint(0, 5000))
        if random.random() < 0.5:
            s.setsockopt(zmq.SUBSCRIBE, b"BNCH")
        s.connect(srv.be)
        t_end = time.time() + random.uniform(0.05, 0.3)
        while time.time() < t_end:
            try:
                s.recv(zmq.NOBLOCK)
                stats["churn_msgs"] += 1
            except zmq.Again:
                time.sleep(0.005)
        s.close(0)
        stats["subs_churned"] += 1
        if time.time() - last_pub > 0.5:
            p = ctx.socket(zmq.PUB)
            p.setsockopt(zmq.LINGER, 0)
            p.connect(srv.fe)
            time.sleep(0.05)
            for _ in range(5):
                p.send(b"churn-%d-x" % random.randint(0, 5000))
            p.close(0)
            stats["pubs_churned"] += 1
            last_pub = time.time()
    ctx.term()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("server")
    ap.add_argument("--duration", type=float, default=60)
    ap.add_argument("--size", type=int, default=262144)
    ap.add_argument("--window", type=int, default=1)
    ap.add_argument("--json", default="")
    a = ap.parse_args()
    srv = Server(a.server)
    stop = threading.Event()
    stats = {"subs_churned": 0, "pubs_churned": 0, "churn_msgs": 0}
    jpath = f"/tmp/churn-{os.getpid()}.json"
    bench = subprocess.Popen(
        [f"{XBIN}/xbench", "--pub", srv.fe, "--sub", srv.be, "--size", str(a.size), "--mode", "window",
         "--window", str(a.window), "--duration", str(a.duration), "--warmup", "1", "--label", a.server,
         "--json", jpath], stderr=subprocess.PIPE, text=True)
    time.sleep(2.0)  # let the steady flow establish before churning
    th = threading.Thread(target=churn, args=(srv, stop, stats), daemon=True)
    th.start()
    samples = []
    t0 = time.time()
    c0 = cpu_s(srv.proc.pid)
    while bench.poll() is None:
        samples.append((round(time.time() - t0, 1), rss_mb(srv.proc.pid)))
        time.sleep(1.0)
    c1 = cpu_s(srv.proc.pid)
    stop.set()
    th.join(timeout=5)
    err = bench.stderr.read()
    try:
        res = json.load(open(jpath))
        os.unlink(jpath)
    except (OSError, ValueError):
        res = {"error": "no client json", "client_stderr": err[-800:]}
    rss = [r for _, r in samples if r is not None]
    third = max(1, len(rss) // 3)
    out = dict(server=a.server, size=a.size, window=a.window, duration=a.duration, churn=stats,
               server_alive=srv.alive(), server_log=srv.tail(),
               rss_first_third_mb=round(sum(rss[:third]) / third, 1) if rss else None,
               rss_last_third_mb=round(sum(rss[-third:]) / third, 1) if rss else None,
               rss_peak_mb=round(max(rss), 1) if rss else None,
               server_cpu_pct=round(100 * (c1 - c0) / (time.time() - t0), 1) if c0 and c1 else None,
               bench=res)
    srv.stop()
    sub0 = (res.get("subs_detail") or [{}])[0]
    print(f"{a.server:22s} steady: {sub0.get('msgs_s', 0):.0f} msg/s loss={sub0.get('loss_pct')}% "
          f"corrupt={sub0.get('corrupt')} p99={sub0.get('lat_us', {}).get('p99')}us | churned subs="
          f"{stats['subs_churned']} pubs={stats['pubs_churned']} | RSS first/last third="
          f"{out['rss_first_third_mb']}/{out['rss_last_third_mb']} MB peak={out['rss_peak_mb']} | "
          f"alive={out['server_alive']}")
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
