#!/usr/bin/env python3
"""Cost of traffic a subscriber did not subscribe to.

Two independent flows through one broker, each driven by its own verifying client process:
  heavy: topic "HVYx", 1 MiB messages, 8 in flight (the other tenant)
  light: topic "LITx", 1 KiB messages at 200 msg/s (the subscriber we look at)
While both run, the light client's TCP sockets are sampled with `ss -tinp` and the bytes the
kernel delivered to them are summed. An XPUB/XSUB broker (and a PUB that filters on the
publisher side) sends the light subscriber only its own topic; SP pub/sub (NNG) filters in the
subscriber, so the broker sends it every message and the subscriber discards the heavy ones.

Usage: topic_filter_cost.py SERVER [--duration 6] [--json out.json]
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from conformance import SERVERS, XBIN, Server  # noqa: E402


def client_for(server):
    return f"{XBIN}/xbench_nng" if server.startswith("nng") else f"{XBIN}/xbench"


def socket_rx_bytes(pid):
    """Sum of bytes_received over the TCP sockets of process `pid` (ss -tinp)."""
    out = subprocess.run(["ss", "-tinpH"], capture_output=True, text=True).stdout
    total, lines = 0, out.splitlines()
    for i, line in enumerate(lines):
        if f"pid={pid}," not in line:
            continue
        info = lines[i + 1] if i + 1 < len(lines) else ""
        m = re.search(r"bytes_received:(\d+)", line + " " + info)
        if m:
            total += int(m.group(1))
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("server")
    ap.add_argument("--duration", type=float, default=6)
    ap.add_argument("--json")
    args = ap.parse_args()
    if args.server not in SERVERS:
        sys.exit(f"unknown server {args.server}")
    client = client_for(args.server)
    srv = Server(args.server)
    try:
        heavy = subprocess.Popen(
            [client, "--pub", srv.fe, "--sub", srv.be, "--topic", "HVYx", "--size", str(1 << 20),
             "--mode", "window", "--window", "8", "--duration", str(args.duration + 2),
             "--warmup", "0.5", "--label", "heavy", "--json", "/tmp/tfc-heavy.json"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(1.0)
        light = subprocess.Popen(
            [client, "--pub", srv.fe, "--sub", srv.be, "--topic", "LITx", "--size", "1024",
             "--mode", "rate", "--rate", "200", "--duration", str(args.duration),
             "--warmup", "0.5", "--label", "light", "--json", "/tmp/tfc-light.json"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        last = 0
        while light.poll() is None:
            last = max(last, socket_rx_bytes(light.pid))
            time.sleep(0.1)
        heavy.wait()
        alive = srv.alive()
    finally:
        srv.stop()
    lj = json.load(open("/tmp/tfc-light.json"))
    hj = json.load(open("/tmp/tfc-heavy.json"))
    l_sub, h_sub = lj["subs_detail"][0], hj["subs_detail"][0]
    wanted = l_sub["received"] * (1024 + 4)
    res = {
        "server": args.server, "alive": alive,
        "light_msgs_received": l_sub["received"], "light_loss_pct": l_sub["loss_pct"],
        "light_p99_us": l_sub["lat_us"]["p99"],
        "heavy_msgs_s": h_sub["msgs_s"],
        "light_socket_bytes_received": last,
        "light_payload_bytes_wanted": wanted,
        "overhead_x": round(last / wanted, 1) if wanted else None,
    }
    print(json.dumps(res))
    if args.json:
        with open(args.json, "a") as f:
            f.write(json.dumps(res) + "\n")


if __name__ == "__main__":
    main()
