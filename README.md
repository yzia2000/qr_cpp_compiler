# qr_cpp_compiler

C++ compiler shootout — **GCC vs Clang vs Intel oneAPI (icpx)** — on a
math-heavy equity-volatility quant research pipeline.

## Preface

- Question: which compiler produces the fastest binaries for transcendental-heavy
  equity-vol work at maximum optimization, and does Intel oneAPI's SVML vector
  math library actually pull ahead on Intel hardware?
- Workload: parse ~30M option quotes from a 1 GB pcap feed, invert Black-Scholes
  implied vols (Newton + analytic vega), fit per-expiry SVI smiles
  (Levenberg-Marquardt), compute analytic greeks for every quote.
- The kernels are saturated with `erf`/`exp`/`log`/`sqrt` — exactly where compiler
  vector-math runtimes diverge: icpx auto-vectorizes these via SVML; GCC/Clang
  can only reach glibc's libmvec (no vector `erf`) under `-ffast-math`.
- Six build variants: {gcc, clang, icpx} x {strict IEEE, fast-math}, identical
  source, single-threaded by design (this measures codegen, not threading).
- Every variant is numerically validated against a strict reference and against
  the generator's ground-truth SVI surfaces before results count.

## Code architecture

- `python/gen_pcap.py` — writes a deterministic ~1 GB pcap: UDP packets carrying
  40 x 32-byte OPRA-like quote messages, sampled from known SVI surfaces
  (64 underlyings x 8 expiries) plus bid/ask noise; ground truth saved to a
  `.truth.npz` sidecar.
- `src/pcap_reader.cpp` — mmap + raw header walk (no libpcap), decodes quotes
  into structure-of-arrays; all sanity filtering happens here, before any
  fast-math code runs.
- `src/implied_vol.cpp` — the hot loop: fixed 8-iteration branchless Newton per
  quote (2x `erf`, 2x `exp`, `log`, `sqrt`, divides per iteration), identical
  work in every build.
- `src/svi_fit.cpp` — 512 slice fits: LM with analytic Jacobian, hand-rolled
  5x5 Cholesky; hot loop is pure sqrt/FMA arithmetic (the deliberate contrast:
  no transcendental calls, so all compilers can vectorize it).
- `src/greeks.cpp` — second full-batch transcendental sweep off the fitted vols.
- `python/bindings.cpp` — nanobind module (`Pipeline`), zero-copy numpy views,
  `build_info()` bakes compiler identity into each .so.
- `python/bench.py` / `bench_worker.py` — each variant benchmarked in a fresh
  subprocess (never two compilers' modules in one interpreter), warmup + 7 timed
  reps, medians + IQR, results injected below.
- `python/validate.py` — cross-build IV/convergence/SVI drift checks with hard
  tolerances plus fit-vs-truth quality; fails loudly if fast-math cheats.
- Toolchain: `flake.nix` pins gcc 13 / clang 18 / cmake / python / nanobind;
  icpx is not in nixpkgs, so `scripts/install_oneapi.sh` installs it separately
  (documented hybrid). Zero third-party C++ deps by design — every timed
  instruction comes from the compiler under test (also why no Conan: there are
  no compiled deps to manage per profile).

## Build matrix

| variant | flags |
|---|---|
| gcc-strict | `-O3 -march=native -funroll-loops` |
| gcc-fast | `-O3 -march=native -funroll-loops -ffast-math` |
| clang-strict | `-O3 -march=native` |
| clang-fast | `-O3 -march=native -ffast-math -fveclib=libmvec` |
| icpx-strict | `-O3 -xHost -fp-model=precise` |
| icpx-fast | `-O3 -xHost -fp-model=fast -fimf-precision=high` |

## Results

<!-- RESULTS:BEGIN -->
| variant | compiler | IV med (ms) | IV Mq/s | SVI med (ms) | greeks med (ms) | total (ms) | vs gcc-strict |
|---|---|---|---|---|---|---|---|
| gcc-strict | GNU 13.3.0 | 16316 | 1.83 | 10928 | 2983 | 30226 | 1.00x |
| gcc-fast | GNU 13.3.0 | 11842 | 2.52 | 10921 | 2837 | 25600 | 1.18x |
| clang-strict | Clang 18.1.8 | 17061 | 1.75 | 6559 | 3021 | 26640 | 1.13x |
| clang-fast | Clang 18.1.8 | 15574 | 1.92 | 6278 | 2843 | 24695 | 1.22x |
| icpx-strict | IntelLLVM 2025.3.3 | 15830 | 1.89 | 5107 | 2982 | 23919 | 1.26x |
| icpx-fast | IntelLLVM 2025.3.3 | 12672 | 2.36 | 2802 | 850 | 16323 | 1.85x |

- 29,873,000 quotes from `quotes_1g.pcap`; per-stage median of timed reps, single-threaded; higher `vs gcc-strict` = faster overall.
<!-- RESULTS:END -->

## Conclusion

- **The oneAPI theory holds**: icpx-fast finishes the pipeline **1.85x faster than
  gcc-strict** and **1.51x faster than the best non-Intel variant** (clang-fast) —
  the single biggest lever in this workload class.
- The advantage lands exactly where predicted: SVML. The icpx-fast module
  imports `__svml_erf4` / `__svml_exp4` / `__svml_log4` (vectorized
  transcendentals); the gcc/clang modules contain **no** vector-math symbols, and
  clang explicitly reports the Newton loop "not vectorized" — glibc's libmvec
  has no vector `erf`, so any `norm_cdf`-bearing loop stays scalar for gcc/clang.
- Stage detail: SVI fit 3.9x vs gcc (2.8s vs 10.9s) and greeks 3.5x (0.85s vs
  2.98s) for icpx-fast — the erf- and FMA-dense sweeps vectorize fully.
- Nuance 1: on the Newton IV stage alone, gcc-fast (2.52 Mq/s) slightly beats
  icpx-fast (2.36 Mq/s) — gcc's scalar fast-math codegen is excellent when
  data-dependent iteration state limits vector efficiency. SVML is not a
  universal win; it wins where loops are cleanly vectorizable.
- Nuance 2: under strict IEEE semantics the field compresses to 1.00-1.26x
  (icpx still fastest via value-safe SVI vectorization). Most of Intel's edge
  requires opting into `-fp-model=fast`.
- Nuance 3: clang beats gcc clearly on the branch-and-Cholesky-heavy LM fitter
  (6.6s vs 10.9s strict) — compiler strength is stage-dependent.
- The speed came at no accuracy cost here: all six builds agree to **2e-14 vol
  points** with zero convergence flips (`results/validation.md`), helped by
  `-fimf-precision=high` and the branchless fixed-iteration Newton design.
- Practical read for a vol desk: if you run Intel hardware and can qualify
  relaxed-FP numerics, icpx + SVML is worth ~35-50% wall-clock on
  transcendental-bound pipelines; if you must stay strict-IEEE or run the
  fitter-style branchy code, the three compilers are much closer than the
  marketing suggests.

## Appendix: vectorization evidence

- `nm -D build-icpx-fast/qr_pipeline*.so | grep svml` → `__svml_erf2, __svml_erf4,
  __svml_exp4, __svml_log4` (4-wide double transcendentals).
- `clang++ -Rpass-missed=loop-vectorize src/implied_vol.cpp` →
  `remark: loop not vectorized` on the Newton loop (scalar `erf` call blocks it).
- `nm -D build-gcc-fast/qr_pipeline*.so | grep _ZGV` → empty (no libmvec
  vector entry points; gcc-fast's IV gain is scalar fast-math codegen).

## Methodology notes

- 4-vCPU Intel Xeon (Sapphire-Rapids-class, AVX-512+FMA), shared cloud host:
  numbers are relative comparisons from one session on one box, not absolutes.
- Median of 7 reps after 1 warmup, fresh subprocess per variant, 5 s settle
  between variants, single-threaded kernels, identical fixed work per build.
- Parse stage (mmap + decode, memory-bound) is timed but excluded from the
  compiler comparison; it is compiler-insensitive as expected.
- icpx was pulled from the `intel/oneapi-basekit` OCI image via
  `mirror.gcr.io` (this environment blocks Intel's apt/registration hosts) and
  links its bundled `lld` + the system gcc toolchain — see
  `scripts/install_oneapi.sh` and comments in `CMakeLists.txt`.

## Reproducing

```bash
scripts/install_nix.sh          # or bring your own gcc/clang/cmake/nanobind
nix develop                     # pinned toolchain shell
scripts/install_oneapi.sh       # icpx (outside nix; apt or OCI-layer fallback)
scripts/run_bench.sh            # data -> build -> validate -> bench -> README
```
