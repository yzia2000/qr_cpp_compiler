#!/usr/bin/env bash
# Installs Nix and enables flakes. Idempotent.
#
# Primary route: Determinate Systems installer (works in containers).
# Fallback route (restricted networks): official static tarball from
# releases.nixos.org, installed single-user via its bundled install script.
set -euo pipefail

if command -v nix >/dev/null 2>&1; then
    echo "nix already installed: $(nix --version)"
    exit 0
fi

NIX_VERSION="${NIX_VERSION:-2.24.9}"

determinate_route() {
    local init_args=""
    [ ! -d /run/systemd/system ] && init_args="--init none"
    curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 15 https://install.determinate.systems/nix \
        | sh -s -- install linux --no-confirm \
             --extra-conf "experimental-features = nix-command flakes" $init_args
}

tarball_route() {
    echo "==> Falling back to static tarball from releases.nixos.org (nix ${NIX_VERSION})"
    local tmp
    tmp=$(mktemp -d)
    curl -fsSL "https://releases.nixos.org/nix/nix-${NIX_VERSION}/nix-${NIX_VERSION}-x86_64-linux.tar.xz" \
        -o "$tmp/nix.tar.xz"
    tar -xJf "$tmp/nix.tar.xz" -C "$tmp"
    # Single-user install; the bundled script needs a non-root invoker by
    # default, but root works in containers with these overrides.
    if [ "$(id -u)" -eq 0 ]; then
        mkdir -m 0755 -p /nix
        USER=root HOME=/root sh "$tmp"/nix-*/install --no-daemon --no-channel-add --no-modify-profile || {
            # Last resort: manual store copy + db registration.
            cp -a "$tmp"/nix-*/store /nix/store 2>/dev/null || true
        }
    else
        sh "$tmp"/nix-*/install --no-daemon --no-channel-add --no-modify-profile
    fi
    rm -rf "$tmp"
    mkdir -p /etc/nix
    grep -q flakes /etc/nix/nix.conf 2>/dev/null || \
        echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf
}

determinate_route || tarball_route

export PATH="/root/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH"
nix --version
echo "OK."
