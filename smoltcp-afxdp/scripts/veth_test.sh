#!/usr/bin/env bash
# End-to-end test: smoltcp on veth0 (AF_XDP) <-> Linux kernel stack in netns "xdpns" on veth1.
# Requires root (CAP_NET_ADMIN + CAP_BPF), iproute2, ethtool, ping, nc.
set -euo pipefail
cd "$(dirname "$0")/.."
MODE=${1:-skb}

cleanup() {
  [[ -n "${PID:-}" ]] && kill "$PID" 2>/dev/null || true
  ip netns del xdpns 2>/dev/null || true
  ip link del veth0 2>/dev/null || true
}
trap cleanup EXIT
cleanup

ip netns add xdpns
ip link add veth0 type veth peer name veth1
ip link set veth1 netns xdpns
ip link set veth0 up
ip -n xdpns addr add 10.77.0.1/24 dev veth1
ip -n xdpns link set veth1 up
ip -n xdpns link set lo up
# Without this the kernel side sends CHECKSUM_PARTIAL frames with unfinished L4 checksums.
ip netns exec xdpns ethtool -K veth1 tx off >/dev/null

cargo build --release --example tcp_echo -q
./target/release/examples/tcp_echo veth0 10.77.0.2/24 "$MODE" & PID=$!
sleep 1

echo "--- ping"
ip netns exec xdpns ping -c 3 -i 0.2 10.77.0.2

echo "--- tcp echo"
reply=$(echo "hello over af_xdp" | ip netns exec xdpns nc -q 1 10.77.0.2 7)
echo "reply: $reply"
[[ "$reply" == "hello over af_xdp" ]]

echo "--- bulk tcp echo (8 MiB)"
head -c 8M /dev/urandom > /tmp/xdp_in.bin
start=$(date +%s.%N)
ip netns exec xdpns nc -q 2 10.77.0.2 7 < /tmp/xdp_in.bin > /tmp/xdp_out.bin
end=$(date +%s.%N)
cmp /tmp/xdp_in.bin /tmp/xdp_out.bin
echo "8 MiB echoed intact in $(echo "$end - $start" | bc) s (includes nc's 2 s linger)"
echo "PASS ($MODE mode)"
