#!/usr/bin/env python3
"""Benchmark ONE build variant in this (fresh) process and emit JSON.

Run via bench.py; one variant per process so different compilers' .so files
never coexist in an interpreter.
"""

import argparse
import json
import statistics
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build-dir", required=True)
    ap.add_argument("--pcap", required=True)
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dump-arrays", help="npz path for validation arrays")
    args = ap.parse_args()

    sys.path.insert(0, args.build_dir)
    import numpy as np
    import qr_pipeline as qr

    info = qr.build_info()
    p = qr.Pipeline(args.pcap)

    stages = {"iv": p.run_iv, "svi": p.fit_svi, "greeks": p.run_greeks}
    for fn in stages.values():   # warmup rep, discarded
        fn()
    times = {name: [] for name in stages}
    for _ in range(args.reps):
        for name, fn in stages.items():
            times[name].append(fn())

    iv = np.asarray(p.iv())
    conv = np.asarray(p.converged())
    sv = np.asarray(p.svi_params())
    g = {k: np.asarray(v) for k, v in p.greeks().items()}

    def stats(ns_list):
        ms = sorted(x / 1e6 for x in ns_list)
        return {
            "ns": ns_list,
            "median_ms": statistics.median(ms),
            "iqr_ms": ms[len(ms) * 3 // 4] - ms[len(ms) // 4],
            "mquotes_per_s": p.num_quotes / statistics.median(ns_list) * 1e3,
        }

    result = {
        "build_dir": args.build_dir,
        "compiler": info["compiler"],
        "flags": info["flags"],
        "fast_math": info["fast_math"],
        "python": sys.executable,
        "num_quotes": p.num_quotes,
        "parse_ms": p.parse_ns / 1e6,
        "stages": {name: stats(ns) for name, ns in times.items()},
        "checksums": {
            "iv_mean": float(iv.mean()),
            "iv_std": float(iv.std()),
            "converged": int(conv.sum()),
            "svi_rmse_mean": float(sv[:, 5][sv[:, 6] >= 0].mean()),
            "delta_mean": float(g["delta"].mean()),
            "gamma_mean": float(g["gamma"].mean()),
        },
    }
    with open(args.out, "w") as f:
        json.dump(result, f, indent=1)

    if args.dump_arrays:
        np.savez_compressed(
            args.dump_arrays,
            iv_sample=iv[::100].astype(np.float64),
            conv=conv, svi=sv,
            delta_sample=g["delta"][::100], vega_sample=g["vega"][::100])
    print(f"{args.build_dir}: " + ", ".join(
        f"{k} {v['median_ms']:.1f}ms" for k, v in result["stages"].items()))


if __name__ == "__main__":
    main()
