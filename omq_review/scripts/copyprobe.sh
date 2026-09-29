#!/usr/bin/env bash
# copyprobe.sh "<server cmd>" "<xbench args>"
# Userspace bytes copied by the proxy per payload byte forwarded: runs the server under the
# memcount.c LD_PRELOAD shim (memcpy/memmove >= 4 KiB and realloc moves), drives it with
# xbench, and diffs the shim's counters from just before to just after the run.
#   MEMCOUNT_SO: the built shim (gcc -O2 -shared -fPIC -o libmemcount.so memcount.c -ldl -lpthread)
set -u
srv=$1; cargs=$2
MEMCOUNT_SO=${MEMCOUNT_SO:-/tmp/libmemcount.so}
XBIN=${XBIN:-/home/user/xbin}
port=$(( 20000 + RANDOM % 20000 ))
fe="tcp://127.0.0.1:$port"; be="tcp://127.0.0.1:$((port+1))"
log=$(mktemp); js=$(mktemp); mc=$(mktemp -u)
MEMCOUNT_OUT=$mc LD_PRELOAD=$MEMCOUNT_SO $srv --frontend "$fe" --backend "$be" > "$log" 2>&1 &
spid=$!
for _ in $(seq 100); do grep -q READY "$log" 2>/dev/null && break; sleep 0.05; done
sleep 0.2; cat $mc > $log.a
$XBIN/xbench --pub "$fe" --sub "$be" --json "$js" --label t --warmup 0 $cargs 2> "$log.client"
sleep 0.3; cat $mc > $log.b
kill $spid 2>/dev/null; wait $spid 2>/dev/null
python3 - "$log" "$js" "$log.a" "$log.b" <<'PY'
import json,re,sys
lines=[open(sys.argv[3]).read(), open(sys.argv[4]).read()]
a,b=[dict((k,int(v)) for k,v in re.findall(r"(\w+)=(\d+)",l)) for l in lines[:2]]
d={k:b[k]-a[k] for k in a}
j=json.load(open(sys.argv[2]))
recv=sum(s.get("received",0) for s in j["subs_detail"])
bad=sum(s.get("lost",0)+s.get("corrupt",0)+s.get("dup",0)+s.get("reorder",0) for s in j["subs_detail"])
size=j.get("size")
sent=j.get("sent")
payload_in=sent*size; payload_out=recv*size
print(f"sent={sent} recv_total={recv} size={size} lost+corrupt+dup+reorder={bad} msg/s={[s['msgs_s'] for s in j['subs_detail']]}")
for k in ("memcpy_bytes","memmove_bytes","realloc_moved_bytes"):
    print(f"  {k:22s} {d[k]/1e6:12.1f} MB  = {d[k]/payload_in:5.2f} x payload received by proxy")
tot=d["memcpy_bytes"]+d["memmove_bytes"]+d["realloc_moved_bytes"]
print(f"  TOTAL userspace copy   {tot/1e6:12.1f} MB  = {tot/payload_in:5.2f} x payload in   ({tot/max(payload_out,1):.2f} x payload out)")
PY
rm -f "$log" "$log.client" "$js" "$log.a" "$log.b" "$mc"
