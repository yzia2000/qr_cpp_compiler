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
_Results pending._
<!-- RESULTS:END -->

## Conclusion

_Pending final benchmark run._

## Reproducing

```bash
scripts/install_nix.sh          # or bring your own gcc/clang/cmake/nanobind
nix develop                     # pinned toolchain shell
scripts/install_oneapi.sh       # icpx (outside nix; apt or OCI-layer fallback)
scripts/run_bench.sh            # data -> build -> validate -> bench -> README
```
