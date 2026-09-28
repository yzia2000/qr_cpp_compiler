#!/usr/bin/env bash
# runone.sh SERVER_NAME "SERVER_ARGS" "CLIENT_ARGS" OUT_JSON
# Starts one proxy server, samples its RSS/CPU from /proc while the independent
# xbench client runs against it, then stops it. Server stderr (incl. any
# "PROXY EXIT") is appended to OUT_JSON.server.log.
set -u
BIN=${XBIN:-/home/user/xbin}
TGT=${XTGT:-/home/user/xproxy-target/release}
name=$1; sargs=$2; cargs=$3; out=$4
case $name in
  libzmq-c)  cmd="$BIN/xproxy_libzmq" ;;
  omq-c)     cmd="$BIN/xproxy_omqc" ;;
  rust-zmq)  cmd="$TGT/libzmq_proxy" ;;
  omq-tokio) cmd="$TGT/omq_proxy" ;;
  zeromq)    cmd="$TGT/zeromq_proxy" ;;
  *) echo "unknown server $name" >&2; exit 2 ;;
esac
port=$(( 20000 + RANDOM % 20000 ))
fe="tcp://127.0.0.1:$port"; be="tcp://127.0.0.1:$((port+1))"
$cmd --frontend "$fe" --backend "$be" $sargs > "$out.server.log" 2>&1 &
spid=$!
for _ in $(seq 100); do grep -q READY "$out.server.log" 2>/dev/null && break; sleep 0.05; done
clk=$(getconf CLK_TCK)
read -r u0 s0 < <(awk '{print $14, $15}' /proc/$spid/stat)
t0=$(date +%s.%N)
( peak=0; while kill -0 $spid 2>/dev/null; do
    r=$(awk '/VmRSS/{print $2}' /proc/$spid/status 2>/dev/null || echo 0)
    [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r
    echo $peak > "$out.rss"; sleep 0.1; done ) &
mpid=$!
$BIN/xbench --pub "$fe" --sub "$be" --json "$out" --label "$name" $cargs 2> "$out.client.log"
crc=$?
t1=$(date +%s.%N)
if kill -0 $spid 2>/dev/null; then
  read -r u1 s1 < <(awk '{print $14, $15}' /proc/$spid/stat)
  alive=true
else
  u1=$u0; s1=$s0; alive=false
fi
kill $spid 2>/dev/null; wait $spid 2>/dev/null; kill $mpid 2>/dev/null; wait $mpid 2>/dev/null
peak=$(cat "$out.rss" 2>/dev/null || echo 0); rm -f "$out.rss"
cpu=$(python3 -c "print(round((($u1-$u0)+($s1-$s0))/$clk/($t1-$t0)*100,1))")
python3 - "$out" "$peak" "$cpu" "$alive" "$crc" <<'EOF'
import json, sys
out, peak, cpu, alive, crc = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), sys.argv[4] == "true", int(sys.argv[5])
try:
    d = json.load(open(out))
except Exception:
    d = {"label": "?", "error": "client produced no json"}
d["server_peak_rss_mb"] = round(peak / 1024, 1)
d["server_cpu_pct"] = cpu
d["server_alive_at_end"] = alive
d["client_rc"] = crc
d["server_log"] = open(out + ".server.log").read()[-2000:]
json.dump(d, open(out, "w"))
EOF
cat "$out.client.log" >&2
