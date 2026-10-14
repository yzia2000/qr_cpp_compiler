#!/usr/bin/env bash
# Rough TCP echo throughput over a veth pair: kernel<->kernel vs kernel<->smoltcp(AF_XDP).
# Not a NIC benchmark: veth is copy-mode only and both ends share this machine's CPUs.
set -uo pipefail
cd "$(dirname "$0")/.."
MIB=${MIB:-512}
cargo build --release --example tcp_echo -q
ECHO_PY=$(mktemp); trap 'rm -f "$ECHO_PY" /tmp/xdp_bench.bin; teardown' EXIT
cat > "$ECHO_PY" <<'PY'
import socket
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("0.0.0.0", 7)); s.listen()
while True:
    c, _ = s.accept()
    while (d := c.recv(1 << 20)): c.sendall(d)
    c.close()
PY
head -c ${MIB}M /dev/zero > /tmp/xdp_bench.bin

setup() {
  ip netns add xdpns; ip link add veth0 type veth peer name veth1; ip link set veth1 netns xdpns
  ip link set veth0 up; ip -n xdpns addr add 10.77.0.1/24 dev veth1; ip -n xdpns link set veth1 up
  ip netns exec xdpns ethtool -K veth1 tx off >/dev/null
}
teardown() {
  [[ -n "${PID:-}" ]] && kill "$PID" 2>/dev/null; wait 2>/dev/null; PID=
  ip netns del xdpns 2>/dev/null; ip link del veth0 2>/dev/null; ip netns del kns 2>/dev/null; true
}
run() {
  local s e n; s=$(date +%s.%N)
  n=$(ip netns exec xdpns timeout 60 nc -q 0 10.77.0.2 7 < /tmp/xdp_bench.bin | wc -c); e=$(date +%s.%N)
  printf "%-36s %4d/%d MiB echoed  %5.2f Gbit/s (sum of both directions)\n" "$1" $((n >> 20)) "$MIB" \
    "$(echo "$MIB * 8 * 2 / 1024 / ($e - $s)" | bc -l)"
}

teardown
setup; ip netns add kns; ip link set veth0 netns kns
ip -n kns addr add 10.77.0.2/24 dev veth0; ip -n kns link set veth0 up; ip netns exec kns ethtool -K veth0 tx off >/dev/null
ip netns exec kns python3 "$ECHO_PY" & PID=$!; sleep 0.5
run "kernel TCP <-> kernel TCP"; teardown

for buf in 65536 1048576; do for mode in skb drv; do
  setup; ECHO_BUF=$buf ./target/release/examples/tcp_echo veth0 10.77.0.2/24 $mode 2>/dev/null & PID=$!; sleep 1
  run "kernel <-> smoltcp/AF_XDP $mode buf=$((buf >> 10))K"; teardown
done; done
