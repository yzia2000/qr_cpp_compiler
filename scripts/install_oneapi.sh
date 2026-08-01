#!/usr/bin/env bash
# Installs the Intel oneAPI DPC++/C++ compiler (icpx).
#
# Primary route: Intel's apt repository (standard, recommended).
# Fallback route (restricted networks where apt.repos.intel.com is blocked):
# pull the `intel/oneapi-basekit` Docker Hub image layers anonymously with
# curl and stream-extract only the compiler component into /opt/intel/oneapi.
#
# Note: oneAPI is intentionally OUTSIDE the nix flake — icpx is not packaged
# in nixpkgs (https://github.com/NixOS/nixpkgs/issues/367722). The exact
# compiler version is recorded into benchmark results JSON instead.
set -euo pipefail

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

if command -v icpx >/dev/null 2>&1; then
    echo "icpx already on PATH: $(icpx --version | head -1)"
    exit 0
fi
if [ -f /opt/intel/oneapi/setvars.sh ]; then
    echo "oneAPI already at /opt/intel/oneapi — run: source /opt/intel/oneapi/setvars.sh"
    exit 0
fi

apt_route() {
    echo "==> Trying Intel apt repository"
    wget -qO- --timeout=15 https://apt.repos.intel.com/intel-gpg-keys/GPG-PUB-KEY-INTEL-SW-PRODUCTS.PUB \
        | gpg --dearmor | $SUDO tee /usr/share/keyrings/oneapi-archive-keyring.gpg >/dev/null
    echo "deb [signed-by=/usr/share/keyrings/oneapi-archive-keyring.gpg] https://apt.repos.intel.com/oneapi all main" \
        | $SUDO tee /etc/apt/sources.list.d/oneAPI.list >/dev/null
    $SUDO apt-get update -qq
    $SUDO apt-get install -y intel-oneapi-compiler-dpcpp-cpp
}

docker_layer_route() {
    # Pulls the compiler out of the intel/oneapi-basekit image, layer by layer,
    # with plain curl (no docker daemon needed). Uses Google's Docker Hub
    # mirror by default: unlike registry-1.docker.io (whose blob CDN
    # production.cloudfront.docker.com is blocked on some networks),
    # mirror.gcr.io serves blobs from the same host.
    echo "==> Falling back to registry layer extraction (intel/oneapi-basekit)"
    local repo="intel/oneapi-basekit" tag="latest"
    local registry="${QR_OCI_REGISTRY:-mirror.gcr.io}"
    local tok manifest
    get_token() {
        if [ "$registry" = "mirror.gcr.io" ]; then
            curl -fsS "https://mirror.gcr.io/v2/token?scope=repository:${repo}:pull&service=mirror.gcr.io" \
                | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])"
        else
            curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:${repo}:pull" \
                | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])"
        fi
    }
    tok=$(get_token)
    manifest=$(curl -fsS -H "Authorization: Bearer $tok" \
        -H "Accept: application/vnd.docker.distribution.manifest.v2+json" \
        "https://${registry}/v2/${repo}/manifests/${tag}")
    # The oneAPI install is in the largest layer; extract only what icpx needs.
    local digest
    digest=$(printf '%s' "$manifest" | python3 -c "
import json,sys
m=json.load(sys.stdin)
print(max(m['layers'], key=lambda l: l['size'])['digest'])")
    echo "==> Downloading layer $digest (~4.2 GB compressed, resumable)"
    local blob="${TMPDIR:-/tmp}/oneapi_layer.tar.gz"
    for attempt in 1 2 3 4 5 6 7 8; do
        if curl -fSL -C - --retry 5 --retry-delay 5 \
            -H "Authorization: Bearer $tok" \
            -o "$blob" \
            "https://${registry}/v2/${repo}/blobs/${digest}"; then
            break
        fi
        echo "download attempt $attempt interrupted; resuming in 10s" >&2
        sleep 10
        tok=$(get_token)
        [ "$attempt" = 8 ] && { echo "download failed" >&2; return 1; }
    done
    echo "==> Verifying digest"
    local got
    got=$(sha256sum "$blob" | cut -d' ' -f1)
    [ "sha256:$got" = "$digest" ] || { echo "digest mismatch: $got" >&2; return 1; }
    echo "==> Extracting compiler component into /opt/intel"
    $SUDO mkdir -p /opt
    $SUDO rm -rf /opt/intel   # drop any partial prior extraction
    gunzip -c "$blob" | $SUDO tar -x -C / --wildcards \
        opt/intel/oneapi/setvars.sh \
        opt/intel/oneapi/common \
        opt/intel/oneapi/compiler \
        opt/intel/oneapi/tbb \
        opt/intel/oneapi/umf \
        'opt/intel/oneapi/2[0-9]*'   # unified-layout dir (hard-link targets)
    rm -f "$blob"
}

if ! apt_route; then
    $SUDO rm -f /etc/apt/sources.list.d/oneAPI.list
    docker_layer_route
fi

echo "==> Verifying"
set +u
# shellcheck disable=SC1091
source /opt/intel/oneapi/setvars.sh --force >/dev/null
set -u
icpx --version | head -1
echo "OK. Add to your shell: source /opt/intel/oneapi/setvars.sh"
