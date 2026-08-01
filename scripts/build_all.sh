#!/usr/bin/env bash
# Builds every compiler x FP-mode variant that has its compiler available,
# then asserts all resulting Python modules link the same libstdc++.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -f /opt/intel/oneapi/setvars.sh ] && ! command -v icpx >/dev/null 2>&1; then
    set +u; source /opt/intel/oneapi/setvars.sh --force >/dev/null 2>&1; set -u
fi

PRESETS=(gcc-strict gcc-fast clang-strict clang-fast)
if command -v icpx >/dev/null 2>&1; then
    PRESETS+=(icpx-strict icpx-fast)
    # icpx must link the same libstdc++ as the nix gcc builds.
    if [ -z "${QR_GCC_TOOLCHAIN:-}" ]; then
        QR_GCC_TOOLCHAIN="$(dirname "$(dirname "$(command -v g++)")")"
        export QR_GCC_TOOLCHAIN
    fi
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

echo "==> libstdc++ consistency check"
paths=$(for p in "${PRESETS[@]}"; do
    ldd "build-$p"/qr_pipeline.*.so | awk '/libstdc\+\+/ {print $3}'
done | sort -u)
echo "$paths"
if [ "$(echo "$paths" | wc -l)" -ne 1 ]; then
    echo "ERROR: variants link different libstdc++ libraries" >&2
    exit 1
fi
echo "OK: all variants share one libstdc++"
