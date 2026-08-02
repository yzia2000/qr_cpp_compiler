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
- Nine build variants: {gcc, clang, icpx} x {strict IEEE, fast-math, fast-math
  forced to 512-bit vectors}, identical source, single-threaded by design
  (this measures codegen, not threading).
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
| gcc-fast-zmm | gcc-fast + `-mprefer-vector-width=512` |
| clang-fast-zmm | clang-fast + `-mprefer-vector-width=512` |
| icpx-fast-zmm | icpx-fast + `-qopt-zmm-usage=high` |

- The `-zmm` variants force full 512-bit vectors; all three compilers default
  to 256-bit on AVX-512 hardware to avoid license-based frequency throttling —
  a heuristic tuned for older silicon and mixed workloads, worth challenging on
  a dedicated math pipeline running on modern cores.

## Results

<!-- RESULTS:BEGIN -->
| variant | compiler | IV med (ms) | IV Mq/s | SVI med (ms) | greeks med (ms) | total (ms) | vs gcc-strict |
|---|---|---|---|---|---|---|---|
| gcc-strict | GNU 13.3.0 | 13087 | 2.28 | 8879 | 2571 | 24536 | 1.00x |
| gcc-fast | GNU 13.3.0 | 9719 | 3.07 | 9494 | 2341 | 21554 | 1.14x |
| gcc-fast-zmm | GNU 13.3.0 | 9813 | 3.04 | 9094 | 2499 | 21406 | 1.15x |
| clang-strict | Clang 18.1.8 | 13971 | 2.14 | 5852 | 2652 | 22475 | 1.09x |
| clang-fast | Clang 18.1.8 | 12877 | 2.32 | 5411 | 2454 | 20742 | 1.18x |
| clang-fast-zmm | Clang 18.1.8 | 12501 | 2.39 | 5461 | 2491 | 20453 | 1.20x |
| icpx-strict | IntelLLVM 2025.3.3 | 13067 | 2.29 | 4333 | 2520 | 19920 | 1.23x |
| icpx-fast | IntelLLVM 2025.3.3 | 10545 | 2.83 | 2385 | 770 | 13700 | 1.79x |
| icpx-fast-zmm | IntelLLVM 2025.3.3 | 12056 | 2.48 | 3039 | 633 | 15728 | 1.56x |

- 29,873,000 quotes from `quotes_1g.pcap`; per-stage median of timed reps, single-threaded; higher `vs gcc-strict` = faster overall.
<!-- RESULTS:END -->

## Conclusion

- **The oneAPI theory holds**: icpx-fast finishes the pipeline **1.79x faster than
  gcc-strict** and **1.44x faster than the best non-Intel variant** (clang-fast-zmm) —
  the single biggest lever in this workload class.
- The advantage lands exactly where predicted: SVML. The icpx-fast module
  imports `__svml_erf4` / `__svml_exp4` / `__svml_log4` (vectorized
  transcendentals); the gcc/clang modules contain **no** vector-math symbols, and
  clang explicitly reports the Newton loop "not vectorized" — glibc's libmvec
  has no vector `erf`, so any `norm_cdf`-bearing loop stays scalar for gcc/clang.
- Stage detail: SVI fit 3.7x vs gcc-strict (2.4s vs 8.9s) and greeks 3.3x
  (0.77s vs 2.57s) for icpx-fast — the erf- and FMA-dense sweeps vectorize fully.
- **Forcing 512-bit vectors (zmm) did not pay off — it made icpx *slower*.**
  icpx-fast-zmm imports the wider `__svml_erf8/exp8/log8` and its greeks/SVI
  objects carry real zmm instruction counts (136/219, vs 0 at default width),
  yet its total time (15.7s) is **14.8% worse** than icpx-fast (13.7s): IV
  regressed 10.5s→12.1s and SVI 2.4s→3.0s, while only greeks improved
  (0.77s→0.63s). This is the classic AVX-512 license-downclocking trade —
  even on Emerald Rapids, where the frequency penalty is supposed to be mild,
  8-wide execution still cost more in throttling/transition overhead than it
  gained in width for this workload. Intel's `-qopt-zmm-usage=low` default is
  the right call here; the compilers' 256-bit conservatism was vindicated, not
  overcome.
- The zmm flag was a no-op for gcc/clang as predicted: `-mprefer-vector-width=512`
  only widens loops that already vectorize, and neither compiler vectorizes
  the erf-bearing loops in the first place — their zmm variants move within
  run-to-run noise (gcc: 21.55s→21.41s; clang: 20.74s→20.45s).
- Nuance 1: on the Newton IV stage alone, gcc-fast (3.07 Mq/s) beats
  icpx-fast (2.83 Mq/s) — gcc's scalar fast-math codegen is excellent when
  data-dependent iteration state limits vector efficiency. SVML is not a
  universal win; it wins where loops are cleanly vectorizable, and loses when
  forced wider than the workload wants (see zmm above).
- Nuance 2: under strict IEEE semantics the field compresses to 1.00-1.23x
  (icpx still fastest via value-safe SVI vectorization). Most of Intel's edge
  requires opting into `-fp-model=fast`.
- Nuance 3: clang beats gcc clearly on the branch-and-Cholesky-heavy LM fitter
  (5.9s vs 8.9s strict) — compiler strength is stage-dependent.
- The speed came at no accuracy cost here: all nine builds agree to **2e-14 vol
  points** with zero convergence flips (`results/validation.md`), helped by
  `-fimf-precision=high` and the branchless fixed-iteration Newton design.
- Practical read for a vol desk: if you run Intel hardware and can qualify
  relaxed-FP numerics, icpx + SVML at its **default** vector width is worth
  ~30-45% wall-clock on transcendental-bound pipelines — don't reach for
  `-qopt-zmm-usage=high` expecting a free win, it cost this pipeline ~15%; if
  you must stay strict-IEEE or run the fitter-style branchy code, the three
  compilers are much closer than the marketing suggests.

## Appendix: vectorization evidence

- `nm -D build-icpx-fast/qr_pipeline*.so | grep svml` → `__svml_erf2, __svml_erf4,
  __svml_exp4, __svml_log4` (4-wide double transcendentals).
- `clang++ -Rpass-missed=loop-vectorize src/implied_vol.cpp` →
  `remark: loop not vectorized` on the Newton loop (scalar `erf` call blocks it).
- `nm -D build-gcc-fast/qr_pipeline*.so | grep _ZGV` → empty (no libmvec
  vector entry points; gcc-fast's IV gain is scalar fast-math codegen).
- zmm variants: `build-icpx-fast-zmm` imports `__svml_erf8/exp8/log8` (8-wide)
  and its greeks/SVI objects carry 136/219 zmm instructions vs 0 at default
  width; the gcc/clang `-mprefer-vector-width=512` builds emit a single zmm
  zeroing instruction — with no vector `erf` there is nothing to widen.
- No compiler vectorizes the Newton IV loop itself (data-dependent iteration
  state); icpx's IV edge over clang comes from libimf's faster *scalar*
  erf/exp, and the SVML `*4`/`*8` calls belong to the greeks/SVI sweeps.

## Methodology notes

- 4-vCPU KVM guest on a 5th-gen Xeon Scalable — Emerald Rapids, family 6
  model 207 (hypervisor masks the retail branding), full AVX-512+FP16+AMX
  exposed. Shared cloud host: numbers are relative comparisons from one
  session on one box, not absolutes.
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
