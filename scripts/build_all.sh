#!/usr/bin/env bash
# Builds every compiler x FP-mode variant that has its compiler available,
# then asserts all resulting Python modules link the same libstdc++.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -f /opt/intel/oneapi/setvars.sh ] && ! command -v icpx >/dev/null 2>&1; then
    set +u; source /opt/intel/oneapi/setvars.sh --force >/dev/null 2>&1; set -u
fi

PRESETS=(gcc-strict gcc-fast gcc-fast-zmm clang-strict clang-fast clang-fast-zmm)
if command -v icpx >/dev/null 2>&1; then
    PRESETS+=(icpx-strict icpx-fast icpx-fast-zmm)
else
    echo "WARNING: icpx not found - skipping icpx presets (run scripts/install_oneapi.sh)" >&2
fi

avail_kb=$(df --output=avail . | tail -1)
[ "$avail_kb" -lt 2000000 ] && { echo "need >=2 GB free disk" >&2; exit 1; }

for p in "${PRESETS[@]}"; do
    echo "==> $p"
    cmake --preset "$p"
    cmake --build --preset "$p"
done

# Every module must depend on the same libstdc++ SONAME (libstdc++.so.6).
# Exact file paths may differ (nix vs system gcc 13 runtimes); that is safe
# because each variant runs in its own process and no C++ runtime code is on
# the timed path. The resolved version of each is printed for the record.
echo "==> libstdc++ consistency check"
sonames=$(for p in "${PRESETS[@]}"; do
    so="$(ls "build-$p"/qr_pipeline.*.so)"
    lib=$(ldd "$so" | awk '/libstdc\+\+/ {print $3}')
    echo "$p -> $(basename "$(readlink -f "$lib")")" >&2
    ldd "$so" | awk '/libstdc\+\+/ {print $1}'
done | sort -u)
if [ "$(echo "$sonames" | wc -l)" -ne 1 ]; then
    echo "ERROR: variants depend on different libstdc++ SONAMEs: $sonames" >&2
    exit 1
fi
echo "OK: all variants depend on $sonames"
