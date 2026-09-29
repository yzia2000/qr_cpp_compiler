#!/usr/bin/env python3
"""Compact report tables from bench_matrix JSONL: one row per size, one column per server.

  pivot.py results/bench_rzmq_zc.jsonl [--servers a,b,c]

Per test: tput -> fast-subscriber msg/s; pingpong -> p50 / p99 µs; fanout4 -> msg/s per
subscriber; slowsub -> fast-subscriber msg/s (slow subscriber msg/s); flood -> delivered
msg/s (worst loss %). Medians over reps, as summarize.py.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from summarize import SIZE_ORDER, agg, load  # noqa: E402


def cell(test, fast, slow):
    if not fast:
        return "–"
    if test == "pingpong":
        return f"{fast['p50']:,.0f} / {fast['p99']:,.0f}"
    if test == "slowsub":
        s = f" ({slow['msgs_s']:,.0f})" if slow else ""
        return f"{fast['msgs_s']:,.0f}{s}"
    if test == "flood":
        return f"{fast['msgs_s']:,.0f} ({fast['loss_max']:.1f}%)"
    txt = f"{fast['msgs_s']:,.0f}"
    if fast["loss_max"] > 0:
        txt += f" ({fast['loss_max']:.1f}% loss)"
    return txt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl")
    ap.add_argument("--servers")
    args = ap.parse_args()
    groups = load(args.jsonl)
    servers = args.servers.split(",") if args.servers else sorted({k[2] for k in groups})
    for test in ("tput", "pingpong", "fanout4", "slowsub", "flood"):
        sizes = [s for s in SIZE_ORDER if any((test, s, srv) in groups for srv in servers)]
        if not sizes:
            continue
        print(f"\n{test}\n")
        print("| size | " + " | ".join(servers) + " |")
        print("|---" * (len(servers) + 1) + "|")
        for size in sizes:
            row = []
            for srv in servers:
                runs = groups.get((test, size, srv))
                row.append(cell(test, agg(runs) if runs else None, agg(runs, slow=True) if runs else None))
            print(f"| {size} | " + " | ".join(row) + " |")


if __name__ == "__main__":
    main()
