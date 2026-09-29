#!/usr/bin/env python3
"""Summarize results/bench.jsonl into markdown tables (medians over reps)."""
import json
import statistics
import sys
from collections import defaultdict

SERVER_ORDER = ["libzmq-c", "rust-zmq", "omq-tokio", "omq-tokio+slotcap64M", "omq-hardened", "omq-c", "zeromq", "rzmq-tokio", "rzmq-tokio-nothrottle", "rzmq-uring", "rzmq-uring-w2", "rzmq-uring-w2-perf", "rzmq-uring-zc", "rzmq-uring-zc-w2", "rzmq-uring-zc-w2-drop", "rzmq-uring-w2-cork", "rzmq-uring-sqpoll", "rzmq-uring-max", "rzmqzc-tokio", "rzmqzc-uring-nodirect", "rzmqzc-uring", "rzmqzc-uring-zc", "nng-device", "nng-device-tuned", "nng-loop"]
SIZE_ORDER = ["100KB", "256KB", "512KB", "1MB"]


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def fmt(x, nd=0, suffix=""):
    if x is None:
        return "–"
    return f"{x:,.{nd}f}{suffix}"


def load(path):
    recs = [json.loads(l) for l in open(path) if l.strip()]
    groups = defaultdict(list)
    for r in recs:
        groups[(r["test"], r["size_name"], r["server"])].append(r)
    return groups


def agg(runs, slow=False):
    """Aggregate fast (or slow) subscribers over reps."""
    rows = []
    for r in runs:
        subs = [s for s in r.get("subs_detail", []) if s["slow"] == slow]
        if not subs:
            rows.append(None)
            continue
        rows.append(dict(
            msgs_s=sum(s["msgs_s"] for s in subs) / len(subs),
            agg_MB_s=sum(s["MB_s"] for s in subs),
            loss=max(s["loss_pct"] for s in subs),
            p50=statistics.mean(s["lat_us"]["p50"] for s in subs),
            p99=max(s["lat_us"]["p99"] for s in subs),
            p999=max(s["lat_us"]["p999"] for s in subs),
            corrupt=sum(s["corrupt"] + s["dup"] + s["reorder"] for s in subs),
            send_rate=r.get("send_rate"),
            cpu=r.get("server_cpu_pct"), rss=r.get("server_peak_rss_mb"), alive=r.get("server_alive"),
        ))
    ok = [x for x in rows if x]
    if not ok:
        return None
    out = {k: med([x[k] for x in ok]) for k in ok[0] if k not in ("alive", "corrupt")}
    out["loss_max"] = max(x["loss"] for x in ok)
    out["corrupt"] = sum(x["corrupt"] for x in ok)
    out["alive"] = all(x["alive"] for x in ok)
    out["reps"] = len(ok)
    return out


def table(groups, test, cols, title, slow=False):
    print(f"\n### {title}\n")
    hdr = "| size | server | " + " | ".join(c[0] for c in cols) + " |"
    print(hdr)
    print("|" + "---|" * (2 + len(cols)))
    for size in SIZE_ORDER:
        for srv in SERVER_ORDER:
            runs = groups.get((test, size, srv))
            if not runs:
                continue
            a = agg(runs, slow)
            if a is None:
                print(f"| {size} | {srv} | " + " | ".join("–" for _ in cols) + " |")
                continue
            print(f"| {size} | {srv} | " + " | ".join(c[1](a) for c in cols) + " |")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "results/bench.jsonl"
    g = load(path)
    tests = sorted({k[0] for k in g})
    base = [
        ("delivered msg/s", lambda a: fmt(a["msgs_s"])),
        ("MB/s", lambda a: fmt(a["agg_MB_s"])),
        ("loss % (median / worst)", lambda a: f"{fmt(a['loss'], 2)} / {fmt(a['loss_max'], 2)}"),
        ("p50 µs", lambda a: fmt(a["p50"])),
        ("p99 µs", lambda a: fmt(a["p99"])),
        ("server CPU %", lambda a: fmt(a["cpu"])),
        ("server peak RSS MB", lambda a: fmt(a["rss"], 1)),
        ("corrupt/dup/reorder", lambda a: str(a["corrupt"])),
        ("server alive", lambda a: "yes" if a["alive"] else "**NO**"),
    ]
    titles = {
        "tput": "Closed-loop throughput, 8 messages in flight, 1 subscriber",
        "tput32": "Closed-loop throughput, 32 messages in flight, 1 subscriber",
        "pingpong": "Round-trip latency, 1 message in flight (ping-pong)",
        "fanout4": "Fan-out to 4 subscribers, 8 in flight (MB/s is aggregate delivered)",
        "slowsub": "1 fast + 1 slow subscriber (5 ms/msg), 8 in flight — FAST subscriber",
        "rate": "Open loop at fixed rate (100KB 4000/s, 256KB 2000/s, 512KB 1000/s, 1MB 500/s)",
        "flood": "Open-loop flood, publisher blocks at its HWM (no client-side drops)",
    }
    for t in tests:
        table(g, t, base, titles.get(t, t))
        if t == "slowsub":
            table(g, t, base, "1 fast + 1 slow subscriber — SLOW subscriber", slow=True)


if __name__ == "__main__":
    main()
