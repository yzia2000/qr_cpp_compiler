#!/usr/bin/env python3
"""Mid-frame disconnect churn: connections that die while a large frame is half received.

Targets receive paths that hand a buffer to the kernel for the rest of a frame (the
io_uring direct receive in rzmq_zc/patches): a connection that closes while such a
receive is in flight must neither leak the buffer nor let the kernel write into freed
memory, and its fd number, reused right away by the next connection, must not inherit it.

- The independent xbench client (libzmq) drives steady verified traffic through the proxy
  (default 1 MiB messages, window 8) for the whole run.
- In parallel, raw ZMTP 3.1 publishers (no libzmq) connect to the proxy's publisher side,
  handshake, send a long-frame header announcing a large body plus only part of that body,
  then disconnect: half close normally (FIN), half abort (SO_LINGER 0 -> RST). A few
  announce 2**44 bytes (the pre-allocation attack) and then disconnect.
- Server RSS is sampled throughout.

Pass: server alive, xbench saw no loss/duplication/reordering/corruption, and RSS at the end
is back near where it was after warm-up (no per-disconnect leak).

Usage: midframe_churn.py SERVER [--duration 20] [--size 1048576] [--conns 400]
"""
import argparse
import json
import os
import random
import socket
import struct
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(__file__))
from conformance import Server  # noqa: E402
from zmtp_raw import greeting, ready, recv_exact  # noqa: E402

XBIN = os.environ.get("XBIN", "/home/user/xbin")


def rss_mb(pid):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    except OSError:
        return None


def raw_publisher(port, declared, send_body, abort):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    try:
        s.sendall(greeting())
        recv_exact(s, 64)
        s.sendall(ready(b"PUB"))
        flags = recv_exact(s, 1)[0]
        size = recv_exact(s, 8 if flags & 0x02 else 1)
        size = struct.unpack(">Q", size)[0] if flags & 0x02 else size[0]
        recv_exact(s, size)
        s.sendall(b"\x02" + struct.pack(">Q", declared))
        chunk = b"\xab" * 65536
        left = send_body
        while left > 0:
            n = min(left, len(chunk))
            s.sendall(chunk[:n])
            left -= n
        time.sleep(random.uniform(0, 0.02))  # let the server arm its receive
        if abort:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("server")
    ap.add_argument("--duration", type=float, default=20)
    ap.add_argument("--size", type=int, default=1 << 20)
    ap.add_argument("--window", type=int, default=8)
    ap.add_argument("--conns", type=int, default=400)
    ap.add_argument("--json")
    args = ap.parse_args()

    srv = Server(args.server)
    fe_port = int(srv.fe.rsplit(":", 1)[1])
    try:
        bench_out = f"/tmp/midframe-{os.getpid()}.json"
        bench = subprocess.Popen(
            [f"{XBIN}/xbench", "--pub", srv.fe, "--sub", srv.be, "--size", str(args.size),
             "--mode", "window", "--window", str(args.window), "--duration", str(args.duration),
             "--warmup", "1", "--label", args.server, "--json", bench_out],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        time.sleep(1.5)
        rss_start = rss_mb(srv.proc.pid)
        rss_peak = rss_start
        stop = threading.Event()
        stats = {"conns": 0, "errors": 0, "aborted": 0, "huge": 0}

        def churn():
            deadline = time.time() + args.duration - 3
            interval = max(0.001, (args.duration - 4) / args.conns)
            while not stop.is_set() and time.time() < deadline and stats["conns"] < args.conns:
                huge = random.random() < 0.05
                declared = 1 << 44 if huge else random.randint(64 << 10, 4 << 20)
                send_body = random.randint(0, min(declared - 1, 3 << 20))
                abort = random.random() < 0.5
                try:
                    raw_publisher(fe_port, declared, send_body, abort)
                except OSError:
                    stats["errors"] += 1
                stats["conns"] += 1
                stats["aborted"] += abort
                stats["huge"] += huge
                time.sleep(interval)

        t = threading.Thread(target=churn)
        t.start()
        while bench.poll() is None:
            r = rss_mb(srv.proc.pid)
            if r:
                rss_peak = max(rss_peak, r)
            time.sleep(0.25)
        stop.set()
        t.join()
        time.sleep(1.0)
        rss_end = rss_mb(srv.proc.pid)
        alive = srv.alive()
        try:
            rec = json.load(open(bench_out))
            os.unlink(bench_out)
        except (OSError, ValueError):
            rec = {"error": "no xbench json", "stderr": bench.stderr.read()[-800:]}
    finally:
        srv.stop()

    sub = (rec.get("subs_detail") or [{}])[0]
    bad = sum(sub.get(k, 0) for k in ("lost", "dup", "reorder", "corrupt")) if sub else None
    result = {
        "server": args.server, "alive": alive, "churn": stats,
        "xbench": {"sent": rec.get("sent"), "received": sub.get("received"), "bad": bad,
                   "msgs_s": sub.get("msgs_s"), "first_error": sub.get("first_error")},
        "rss_mb": {"start": rss_start, "peak": rss_peak, "end": rss_end},
    }
    result["pass"] = bool(alive and bad == 0 and rec.get("sent")
                          and rss_end is not None and rss_end < rss_start + 64)
    print(json.dumps(result, indent=1))
    if args.json:
        with open(args.json, "w") as f:
            json.dump(result, f, indent=1)
    sys.exit(0 if result["pass"] else 1)


if __name__ == "__main__":
    main()
