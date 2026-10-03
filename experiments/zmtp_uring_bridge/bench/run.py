#!/usr/bin/env python3
"""Throughput benchmark: libzmq PUB -> {direct | zmq_proxy | io_uring bridge} -> libzmq SUB.

Pinning on a 4-core box: PUB on cpu0, SUB on cpu1, the proxy under test on
cpus 2-3 (libzmq uses its main + I/O thread; the bridge is single-threaded).
Reports SUB-side delivered throughput over a steady window and the proxy's
CPU time over that same window.
"""
import json, os, re, signal, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
BRIDGE = os.path.join(HERE, "..", "target", "release", "zmtp-uring-bridge")
FRONT, BACK = "tcp://127.0.0.1:5555", "tcp://127.0.0.1:5556"
SIZES = [int(s) for s in os.environ.get("SIZES", "10240,102400,1048576").split(",")]
REPS = int(os.environ.get("REPS", "3"))
WARM, MEAS = float(os.environ.get("WARM", "2")), float(os.environ.get("MEAS", "5"))
TICK = os.sysconf("SC_CLK_TCK")

CONFIGS = {
    "direct (no proxy)": None,
    "libzmq zmq_proxy": [os.path.join(HERE, "zproxy"), FRONT, BACK],
    "libzmq zmq_proxy hwm=0": [os.path.join(HERE, "zproxy"), FRONT, BACK, "0"],
    "libzmq zmq_proxy io_threads=2": [os.path.join(HERE, "zproxy"), FRONT, BACK, "1000", "2"],
    "uring bridge (copy)": [BRIDGE, "--zc", "off"],
    "uring bridge (zc)": [BRIDGE, "--zc", "on"],
}

def cpu_secs(pid):
    # Process-wide utime + stime: covers every thread, including exited io-wq workers.
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(f[11]) + int(f[12])) / TICK

def run(name, cmd, size):
    proxy = None
    if cmd:
        proxy = subprocess.Popen(["taskset", "-c", "2,3"] + cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.4)
    sub_ep = BACK if cmd else FRONT
    pub_ep = FRONT if cmd else "bind:" + FRONT
    sub = subprocess.Popen(["taskset", "-c", "1", os.path.join(HERE, "sub"), sub_ep, str(size), str(WARM), str(MEAS)],
                           stdout=subprocess.PIPE, text=True)
    time.sleep(0.3)
    pub = subprocess.Popen(["taskset", "-c", "0", os.path.join(HERE, "pub"), pub_ep, str(size), str(WARM + MEAS + 3)],
                           stdout=subprocess.DEVNULL)
    cpu = None
    if proxy:
        # pub warms 1.5 s before the timed loop; sub's window starts WARM s after its first message.
        time.sleep(1.5 + WARM)
        c0, t0 = cpu_secs(proxy.pid), time.time()
        time.sleep(MEAS)
        cpu = (cpu_secs(proxy.pid) - c0) / (time.time() - t0)
    out, _ = sub.communicate(timeout=60)
    pub.kill(); pub.wait()
    if proxy:
        proxy.send_signal(signal.SIGKILL); proxy.wait()
    time.sleep(0.5)
    m = re.search(r"rate=(\d+) msg/s throughput=([\d.]+) MB/s", out)
    if not m:
        return {"error": out.strip()}
    mbps = float(m.group(2))
    r = {"msgs_per_s": int(m.group(1)), "MB_per_s": mbps}
    if cpu is not None:
        r["proxy_cpu_cores"] = round(cpu, 2)
        r["proxy_cpu_s_per_GB"] = round(cpu / (mbps / 1000), 3) if mbps else None
    return r

results = []
for size in SIZES:
    for name, cmd in CONFIGS.items():
        for rep in range(REPS):
            r = run(name, cmd, size)
            r.update(config=name, size=size, rep=rep)
            print(json.dumps(r), flush=True)
            results.append(r)
json.dump(results, open(os.path.join(HERE, "results.json"), "w"), indent=1)
