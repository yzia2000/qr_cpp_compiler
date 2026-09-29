#!/usr/bin/env bash
# cpuprobe.sh "<server cmd>" "<xbench args>"
# Server CPU (user/sys) and minor page faults per forwarded message, from /proc/<pid>/stat
# before and after one xbench run (XBENCH=xbench_nng for NNG servers).
set -u
srv=$1; cargs=$2
XBIN=${XBIN:-/home/user/xbin}
port=$(( 20000 + RANDOM % 20000 ))
fe="tcp://127.0.0.1:$port"; be="tcp://127.0.0.1:$((port+1))"
log=$(mktemp); js=$(mktemp)
$srv --frontend "$fe" --backend "$be" > "$log" 2>&1 &
spid=$!
for _ in $(seq 100); do grep -q READY "$log" 2>/dev/null && break; sleep 0.05; done
read -r f0 u0 s0 < <(awk '{print $10, $14, $15}' /proc/$spid/stat)
$XBIN/${XBENCH:-xbench} --pub "$fe" --sub "$be" --json "$js" --label t --warmup 0 $cargs 2>/dev/null
read -r f1 u1 s1 < <(awk '{print $10, $14, $15}' /proc/$spid/stat)
kill $spid; wait $spid 2>/dev/null
python3 - "$js" $f0 $f1 $u0 $u1 $s0 $s1 <<'PY'
import json,sys
j=json.load(open(sys.argv[1])); f0,f1,u0,u1,s0,s1=map(int,sys.argv[2:])
n=j["sent"]; hz=100  # CLK_TCK
print(f"msgs={n} msg/s={j['subs_detail'][0]['msgs_s']:.0f} minflt/msg={(f1-f0)/n:.1f} user_ms/msg={(u1-u0)*1000/hz/n:.3f} sys_ms/msg={(s1-s0)*1000/hz/n:.3f}")
PY
rm -f "$log" "$js"
