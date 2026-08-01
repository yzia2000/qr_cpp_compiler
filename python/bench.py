#!/usr/bin/env python3
"""Benchmark orchestrator: runs bench_worker.py per build variant in fresh
subprocesses, aggregates results, writes results/results.md, and injects the
table into README.md between the RESULTS markers."""

import argparse
import json
import pathlib
import re
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
VARIANTS = ["gcc-strict", "gcc-fast", "clang-strict", "clang-fast",
            "icpx-strict", "icpx-fast"]


def run_variant(variant, pcap, reps):
    build_dir = ROOT / f"build-{variant}"
    if not (build_dir.exists() and list(build_dir.glob("qr_pipeline*.so"))):
        print(f"-- skipping {variant} (not built)")
        return None
    out = ROOT / "results" / f"{variant}.json"
    cmd = [sys.executable, str(ROOT / "python" / "bench_worker.py"),
           "--build-dir", str(build_dir), "--pcap", pcap,
           "--reps", str(reps), "--out", str(out)]
    print(f"== {variant}")
    subprocess.run(cmd, check=True)
    return json.loads(out.read_text())


def to_markdown(results):
    baseline = next((r for r in results if "gcc" in r["build_dir"]
                     and not r["fast_math"]), results[0])
    base_total = sum(s["median_ms"] for s in baseline["stages"].values())
    lines = [
        "| variant | compiler | IV med (ms) | IV Mq/s | SVI med (ms) | greeks med (ms) | total (ms) | vs gcc-strict |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for r in results:
        name = r["build_dir"].split("build-")[-1]
        st = r["stages"]
        total = sum(s["median_ms"] for s in st.values())
        lines.append(
            f"| {name} | {r['compiler']} | {st['iv']['median_ms']:.0f} "
            f"| {st['iv']['mquotes_per_s']:.2f} | {st['svi']['median_ms']:.0f} "
            f"| {st['greeks']['median_ms']:.0f} | {total:.0f} "
            f"| {base_total / total:.2f}x |")
    return "\n".join(lines)


def inject_readme(table_md, n_quotes, pcap):
    readme = ROOT / "README.md"
    text = readme.read_text()
    block = (f"<!-- RESULTS:BEGIN -->\n{table_md}\n\n"
             f"- {n_quotes:,} quotes from `{pcap}`; per-stage median of timed reps, "
             f"single-threaded; higher `vs gcc-strict` = faster overall.\n"
             f"<!-- RESULTS:END -->")
    text = re.sub(r"<!-- RESULTS:BEGIN -->.*?<!-- RESULTS:END -->", block,
                  text, flags=re.S)
    readme.write_text(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pcap", default=str(ROOT / "data" / "quotes_1g.pcap"))
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--settle", type=float, default=5.0,
                    help="seconds between variants")
    args = ap.parse_args()

    (ROOT / "results").mkdir(exist_ok=True)
    results = []
    for v in VARIANTS:
        r = run_variant(v, args.pcap, args.reps)
        if r:
            results.append(r)
            time.sleep(args.settle)
    if not results:
        sys.exit("no built variants found")

    table = to_markdown(results)
    (ROOT / "results" / "results.md").write_text(table + "\n")
    inject_readme(table, results[0]["num_quotes"], pathlib.Path(args.pcap).name)
    print("\n" + table)


if __name__ == "__main__":
    main()
