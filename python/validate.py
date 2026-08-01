#!/usr/bin/env python3
"""Cross-compiler numeric validation on the dev pcap.

Every built variant runs in a fresh subprocess (dumping arrays via
bench_worker.py --dump-arrays); results are compared against the gcc-strict
reference and against the generator's ground-truth SVI surface. This is what
makes fast-math results trustworthy: identical convergence behavior and
bounded IV drift, or the run fails loudly."""

import json
import pathlib
import subprocess
import sys

import numpy as np

ROOT = pathlib.Path(__file__).resolve().parent.parent
VARIANTS = ["gcc-strict", "gcc-fast", "gcc-fast-zmm",
            "clang-strict", "clang-fast", "clang-fast-zmm",
            "icpx-strict", "icpx-fast", "icpx-fast-zmm"]
IV_TOL_STRICT = 1e-9      # vol points, IEEE builds must agree to fp noise
IV_TOL_FAST = 5e-5        # vol points, relaxed-FP drift budget
CONV_TOL = 1e-4           # fraction of quotes allowed to flip convergence
SVI_TOL = 2e-3            # param drift between builds


def main():
    pcap = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "data" / "quotes_dev.pcap")
    out_dir = ROOT / "results"
    out_dir.mkdir(exist_ok=True)

    dumps = {}
    for v in VARIANTS:
        build_dir = ROOT / f"build-{v}"
        if not (build_dir.exists() and list(build_dir.glob("qr_pipeline*.so"))):
            continue
        npz = out_dir / f"val-{v}.npz"
        subprocess.run(
            [sys.executable, str(ROOT / "python" / "bench_worker.py"),
             "--build-dir", str(build_dir), "--pcap", pcap, "--reps", "1",
             "--out", str(out_dir / f"val-{v}.json"), "--dump-arrays", str(npz)],
            check=True)
        dumps[v] = dict(np.load(npz))

    if "gcc-strict" not in dumps:
        sys.exit("gcc-strict reference build required")
    ref = dumps["gcc-strict"]
    n = len(ref["conv"])
    report = ["# Cross-compiler validation", "",
              f"- reference: gcc-strict, {n:,} quotes, `{pathlib.Path(pcap).name}`",
              f"- tolerances: strict IV <= {IV_TOL_STRICT}, fast IV <= {IV_TOL_FAST} vol pts",
              ""]
    failures = []
    for v, d in dumps.items():
        if v == "gcc-strict":
            continue
        fast = "fast" in v
        tol = IV_TOL_FAST if fast else IV_TOL_STRICT
        both = (d["conv"] > 0) & (ref["conv"] > 0)
        iv_diff = np.abs(d["iv_sample"] - ref["iv_sample"])[both[::100][:len(d["iv_sample"])]]
        max_iv = float(iv_diff.max()) if iv_diff.size else 0.0
        conv_flip = float(np.mean(d["conv"] != ref["conv"]))
        svi_diff = float(np.abs(d["svi"][:, :5] - ref["svi"][:, :5]).max())
        ok = max_iv <= tol and conv_flip <= CONV_TOL and svi_diff <= SVI_TOL
        status = "OK" if ok else "FAIL"
        if not ok:
            failures.append(v)
        report.append(
            f"- **{v}**: {status} — max|dIV| {max_iv:.2e}, "
            f"conv flips {conv_flip:.2e}, max|dSVI| {svi_diff:.2e}")

    # fit quality vs generator ground truth
    truth = np.load(pcap + ".truth.npz")
    sv = ref["svi"]
    fitted = sv[:, 6] >= 0
    rho_err = np.abs(sv[:, 2].reshape(64, 8) - truth["rho"])
    report += ["", "## Fit vs ground truth (gcc-strict)",
               f"- slices fitted: {int(fitted.sum())}/512",
               f"- mean |rho error|: {rho_err.mean():.4f}",
               f"- mean fit RMSE (total var): {sv[:, 5][fitted].mean():.2e}"]

    (out_dir / "validation.md").write_text("\n".join(report) + "\n")
    print("\n".join(report))
    if failures:
        sys.exit(f"VALIDATION FAILED: {failures}")


if __name__ == "__main__":
    main()
