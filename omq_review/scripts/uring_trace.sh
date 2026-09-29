#!/usr/bin/env bash
# uring_trace.sh "<server cmd...>" "<xbench args>" [trace_secs]
# Starts a proxy, drives it with xbench, and histograms io_uring opcodes submitted
# (kernel tracepoint io_uring:io_uring_submit_req) during the middle of the run.
# Needs root and tracefs (mount -t tracefs nodev /sys/kernel/tracing).
set -u
srv=$1; cargs=$2; secs=${3:-2}
XBIN=${XBIN:-/home/user/xbin}
T=/sys/kernel/tracing
port=$(( 20000 + RANDOM % 20000 ))
fe="tcp://127.0.0.1:$port"; be="tcp://127.0.0.1:$((port+1))"
log=$(mktemp)
$srv --frontend "$fe" --backend "$be" > "$log" 2>&1 &
spid=$!
for _ in $(seq 100); do grep -q READY "$log" 2>/dev/null && break; sleep 0.05; done
$XBIN/xbench --pub "$fe" --sub "$be" --json /dev/null --label t $cargs 2> "$log.client" &
cpid=$!
sleep 1.5
echo 0 > $T/tracing_on; echo > $T/trace; echo 16384 > $T/buffer_size_kb
echo 1 > $T/events/io_uring/io_uring_submit_req/enable
echo 1 > $T/events/io_uring/io_uring_complete/enable
echo 1 > $T/tracing_on; sleep "$secs"; echo 0 > $T/tracing_on
echo 0 > $T/events/io_uring/io_uring_submit_req/enable
echo 0 > $T/events/io_uring/io_uring_complete/enable
wait $cpid
tail -3 "$log.client"
kill $spid 2>/dev/null; wait $spid 2>/dev/null
echo "== opcodes submitted in ${secs}s =="
grep -o "opcode [A-Z_0-9]*" $T/trace | sort | uniq -c | sort -rn
echo "== completions: result histogram for req that were WRITEV/SEND*/RECV* (bytes) =="
awk '/io_uring_submit_req/ { match($0, /req 0x[0-9a-f]+/); r=substr($0,RSTART+4,RLENGTH-4); match($0,/opcode [A-Z_0-9]+/); op[r]=substr($0,RSTART+7,RLENGTH-7) }
     /io_uring_complete/ { match($0, /req 0x[0-9a-f]+/); r=substr($0,RSTART+4,RLENGTH-4); match($0,/result -?[0-9]+/); res=substr($0,RSTART+7,RLENGTH-7)+0; o=(r in op)?op[r]:"?";
        n[o]++; if (res>0) b[o]+=res; }
     END { for (o in n) printf "%-16s completions=%-8d bytes=%-12d avg=%d\n", o, n[o], b[o], (n[o]?b[o]/n[o]:0) }' $T/trace | sort -k2 -t= -rn
grep -c "lost" $T/trace
rm -f "$log" "$log.client"
