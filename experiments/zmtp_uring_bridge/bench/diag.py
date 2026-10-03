#!/usr/bin/env python3
"""Root-cause diagnostics for one (config, size) run.

Same topology and pinning as run.py (PUB cpu0, SUB cpu1, proxy cpus 2-3).
During the measurement window it records:
  * per-CPU utilisation split into user / sys / softirq / idle (/proc/stat);
  * per-process CPU (pub, sub, proxy), user and sys separately;
  * socket queues every 100 ms via `ss -tin`: Recv-Q (bytes the reader has
    not read yet) and Send-Q (bytes the writer has queued but the peer has
    not acknowledged), per hop.
Prints one JSON object.
"""
import json, os, re, signal, subprocess, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
BRIDGE = os.path.join(HERE, "..", "target", "release", "zmtp-uring-bridge")
FRONT, BACK = "tcp://127.0.0.1:5555", "tcp://127.0.0.1:5556"
TICK = os.sysconf("SC_CLK_TCK")
WARM, MEAS = float(os.environ.get("WARM", "2")), float(os.environ.get("MEAS", "4"))

CONFIGS = {
    "direct": None,
    "zmq_proxy": [os.path.join(HERE, "zproxy"), FRONT, BACK],
    "chunk": [BRIDGE, "--zc", "off", "--recv", "chunk"],
    "ring1": [BRIDGE, "--zc", "off", "--recv", "ring", "--recvs", "1"],
    "ring2": [BRIDGE, "--zc", "off", "--recv", "ring", "--recvs", "2"],
    "ring2-256k": [BRIDGE, "--zc", "off", "--recv", "ring", "--recvs", "2", "--ring-buf-kb", "256", "--ring-entries", "256"],
    "multishot": [BRIDGE, "--zc", "off", "--recv", "multishot"],
    "multishot-256k": [BRIDGE, "--zc", "off", "--recv", "multishot", "--ring-buf-kb", "256", "--ring-entries", "256"],
}


def cpu_times():
    out = {}
    for line in open("/proc/stat"):
        if re.match(r"cpu\d", line):
            f = line.split()
            v = list(map(int, f[1:9]))
            # user nice system idle iowait irq softirq steal
            out[f[0]] = {"user": v[0] + v[1], "sys": v[2], "idle": v[3] + v[4], "irq": v[5], "softirq": v[6], "steal": v[7]}
    return out


def proc_times(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return int(f[11]) / TICK, int(f[12]) / TICK


def ss_sampler(stop, samples):
    while not stop.is_set():
        out = subprocess.run(["ss", "-tinH", "state", "established", "( sport = :5555 or sport = :5556 or dport = :5555 or dport = :5556 )"],
                             capture_output=True, text=True).stdout
        lines = out.splitlines()
        snap = []
        i = 0
        while i < len(lines):
            f = lines[i].split()
            if len(f) >= 4 and f[0].isdigit():
                rq, sq, local, peer = int(f[0]), int(f[1]), f[2], f[3]
                info = lines[i + 1] if i + 1 < len(lines) and not lines[i + 1].split()[0].isdigit() else ""
                m = re.search(r"rcv_space:(\d+)", info)
                snap.append((local, peer, rq, sq, int(m.group(1)) if m else 0))
            i += 1
        samples.append(snap)
        time.sleep(0.1)


def label(local, peer):
    lp, pp = local.rsplit(":", 1)[1], peer.rsplit(":", 1)[1]
    if lp == "5555":
        return "front: proxy end (reads from PUB)"
    if pp == "5555":
        return "front: PUB end (writes to proxy)"
    if lp == "5556":
        return "back: proxy end (writes to SUB)"
    if pp == "5556":
        return "back: SUB end (reads from proxy)"
    return f"{local}->{peer}"


def run(name, size):
    import socket
    for _ in range(150):
        try:
            for port in (5555, 5556):
                sk = socket.socket(); sk.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); sk.bind(("127.0.0.1", port)); sk.close()
            break
        except OSError:
            time.sleep(0.1)
    cmd = CONFIGS[name]
    proxy = None
    if cmd:
        log = open(f"/tmp/diag_proxy_{name}.log", "w")
        proxy = subprocess.Popen(["taskset", "-c", "2,3"] + cmd, stdout=subprocess.DEVNULL, stderr=log,
                                 env={**os.environ, "BR_DIAG": "1"})
        time.sleep(0.4)
    sub_ep = BACK if cmd else FRONT
    pub_ep = FRONT if cmd else "bind:" + FRONT
    sub = subprocess.Popen(["taskset", "-c", "1", os.path.join(HERE, "sub"), sub_ep, str(size), str(WARM), str(MEAS)],
                           stdout=subprocess.PIPE, text=True)
    time.sleep(0.3)
    pub = subprocess.Popen(["taskset", "-c", "0", os.path.join(HERE, "pub"), pub_ep, str(size), str(WARM + MEAS + 3)],
                           stdout=subprocess.DEVNULL)
    time.sleep(1.5 + WARM)
    pids = {"pub": pub.pid, "sub": sub.pid}
    if proxy:
        pids["proxy"] = proxy.pid
    c0, p0, t0 = cpu_times(), {k: proc_times(v) for k, v in pids.items()}, time.time()
    stop, samples = threading.Event(), []
    th = threading.Thread(target=ss_sampler, args=(stop, samples))
    th.start()
    time.sleep(MEAS * 0.9)
    stop.set()
    th.join()
    c1, p1, dt = cpu_times(), {k: proc_times(v) for k, v in pids.items()}, time.time() - t0
    out, _ = sub.communicate(timeout=60)
    pub.kill(); pub.wait()
    if proxy:
        proxy.send_signal(signal.SIGKILL); proxy.wait()
    time.sleep(0.5)

    m = re.search(r"throughput=([\d.]+) MB/s", out)
    res = {"config": name, "size": size, "MB_per_s": float(m.group(1)) if m else None}
    cpus = {}
    for cpu in ("cpu0", "cpu1", "cpu2", "cpu3"):
        d = {k: c1[cpu][k] - c0[cpu][k] for k in c0[cpu]}
        tot = sum(d.values()) or 1
        cpus[cpu] = {k: round(100 * v / tot) for k, v in d.items() if k in ("user", "sys", "softirq", "idle")}
    res["cpu_pct"] = cpus
    res["proc_cores"] = {k: {"user": round((p1[k][0] - p0[k][0]) / dt, 2), "sys": round((p1[k][1] - p0[k][1]) / dt, 2)} for k in pids}
    hops = {}
    for snap in samples:
        for local, peer, rq, sq, rspace in snap:
            h = hops.setdefault(label(local, peer), {"recvq": [], "sendq": [], "rcv_space": []})
            h["recvq"].append(rq); h["sendq"].append(sq); h["rcv_space"].append(rspace)
    res["queues_KB"] = {k: {"recvq_avg": round(sum(v["recvq"]) / len(v["recvq"]) / 1024), "sendq_avg": round(sum(v["sendq"]) / len(v["sendq"]) / 1024),
                            "recvq_zero_pct": round(100 * sum(1 for x in v["recvq"] if x == 0) / len(v["recvq"])),
                            "rcv_space_KB": round(max(v["rcv_space"]) / 1024)} for k, v in sorted(hops.items())}
    if proxy and cmd[0] == BRIDGE:
        diags = [json.loads(l[5:]) for l in open(f"/tmp/diag_proxy_{name}.log") if l.startswith("DIAG ")]
        # Keep the steady-state seconds: drop warm-up and the teardown second.
        diags = [d for d in diags if d["bytes_in"] > 0][-int(MEAS) - 1:-1] or diags[-2:-1]
        if diags:
            n = len(diags)
            keys = {k for d in diags for k in d["wait_pct"]}
            res["bridge"] = {
                "busy_pct": round(sum(d["busy_pct"] for d in diags) / n, 1),
                "wait_pct": {k: round(sum(d["wait_pct"].get(k, 0) for d in diags) / n, 1) for k in sorted(keys)},
                "cqes_per_wake": round(sum(d["cqes_per_wake"] for d in diags) / n, 2),
                "wakes_per_s": round(sum(d["wakes"] for d in diags) / n),
                "recv_hist": [sum(d["recv_hist"][i] for d in diags) for i in range(8)],
                "send_hist": [sum(d["send_hist"][i] for d in diags) for i in range(8)],
            }
    return res


if __name__ == "__main__":
    size = int(sys.argv[1])
    for name in sys.argv[2:]:
        print(json.dumps(run(name, size)), flush=True)
