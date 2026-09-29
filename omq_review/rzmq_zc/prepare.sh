#!/usr/bin/env bash
# Creates rzmq_zc/src-rzmq: upstream rzmq (github.com/excsn/rzmq) at the 0.5.26 release
# commit with the patches in rzmq_zc/patches applied, for rzmq_zc/server to build against.
#   rzmq_zc/prepare.sh [existing-rzmq-checkout]   # optional local clone instead of fetching
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dst=$here/src-rzmq
rev=4a66cf7e220731c07aefb98286d43c10d434d017   # "rzmq 0.5.26" (same source as the crates.io release)
rm -rf "$dst"
if [ $# -ge 1 ]; then
  git clone -q "$1" "$dst"
else
  git clone -q https://github.com/excsn/rzmq "$dst"
fi
git -C "$dst" checkout -q "$rev"
for p in "$here"/patches/*.patch; do
  git -C "$dst" apply --index "$p"
  echo "applied $(basename "$p")"
done
echo "ready: $dst  (build: cd $here/server && cargo build --release)"
