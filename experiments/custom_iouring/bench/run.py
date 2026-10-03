#!/usr/bin/env python3
"""Runs the benchmark matrix: {direct, custom iouring, libzmq} x sizes x modes.

The harness process (sender + receiver threads, libzmq I/O threads) is pinned
to cpus 0-1, the application under test to cpus 2-3. Each run prints the
harness JSON plus the application's CPU use (cores) over the measured window.
"""
import json, os, signal, socket, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
XSUB, XPUB = "tcp://127.0.0.1:5555", "tcp://127.0.0.1:5556"
SIZES = [int(s) for s in os.environ.get("SIZES", "10240,102400,1048576").split(",")]
MODES = os.environ.get("MODES", "saturate,pingpong").split(",")
REPS = int(os.environ.get("REPS", "3"))
WARM, MEAS = float(os.environ.get("WARM", "2")), float(os.environ.get("MEAS", "5"))
TICK = os.sysconf("SC_CLK_TCK")

APPS = {
    "direct": None,
    "custom iouring 1 thread": [os.path.join(HERE, "..", "target", "release", "custom-iouring-1thread"), "--xsub", "127.0.0.1:5555", "--xpub", "127.0.0.1:5556"],
    "custom iouring 2 threads, depth 1": [os.path.join(HERE, "..", "target", "release", "custom-iouring"), "--cpus", "2,3", "--depth", "1"],
    "custom iouring 2 threads, depth 8": [os.path.join(HERE, "..", "target", "release", "custom-iouring"), "--cpus", "2,3", "--depth", "8"],
    "custom iouring 2 threads, depth 64": [os.path.join(HERE, "..", "target", "release", "custom-iouring"), "--cpus", "2,3", "--depth", "64"],
    "libzmq": [os.path.join(HERE, "libzmq_xsub_xpub"), XSUB, XPUB],
}
only = os.environ.get("APPS")
if only:
    APPS = {k: v for k, v in APPS.items() if k in only.split(";")}


def ports_free():
    for _ in range(150):
        try:
            for p in (5555, 5556):
                s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1", p)); s.close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("ports busy")


def cpu(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(f[11]) + int(f[12])) / TICK


def run(app, cmd, size, mode):
    ports_free()
    proc = None
    if cmd:
        proc = subprocess.Popen(["taskset", "-c", "2,3"] + cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.4)
    xsub = XSUB if cmd else "bind:" + XSUB
    xpub = XPUB if cmd else XSUB
    h = subprocess.Popen(["taskset", "-c", "0,1", os.path.join(HERE, "harness"), xsub, xpub, str(size), mode, str(WARM), str(MEAS)],
                         stdout=subprocess.PIPE, text=True)
    cores = None
    if proc:
        time.sleep(0.5 + WARM)
        c0, t0 = cpu(proc.pid), time.time()
        time.sleep(MEAS * 0.8)
        cores = round((cpu(proc.pid) - c0) / (time.time() - t0), 2)
    out, _ = h.communicate(timeout=120)
    if proc:
        proc.send_signal(signal.SIGKILL); proc.wait()
    try:
        r = json.loads(out.strip().splitlines()[-1])
    except Exception:
        r = {"error": out.strip()}
    r.update(app=app, app_cores=cores)
    return r


out = os.path.join(HERE, os.environ.get("OUT", "results.json"))
results = []
for size in SIZES:
    for mode in MODES:
        for app, cmd in APPS.items():
            for rep in range(REPS):
                r = run(app, cmd, size, mode)
                r["rep"] = rep
                print(json.dumps(r), flush=True)
                results.append(r)
json.dump(results, open(out, "w"), indent=1)
