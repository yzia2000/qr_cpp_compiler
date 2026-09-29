#!/usr/bin/env python3
"""Black-box XSUB/XPUB proxy conformance suite.

Every scenario runs against a *fresh* server process (see SERVERS) and drives
it only from the outside with pyzmq (libzmq 4.3.5): publishers connect to the
proxy's XSUB side, subscribers to its XPUB side. The expected behaviour is
what libzmq's zmq_proxy(XSUB, XPUB) does; each scenario states the libzmq
rule it checks. After each scenario we also check the server is still alive
and has not left its proxy loop.

Usage: conformance.py [--servers a,b,...] [--only name,...] [--json out.json]
"""
import argparse
import json
import os
import random
import subprocess
import sys
import time
import traceback

import zmq

XBIN = os.environ.get("XBIN", "/home/user/xbin")
XTGT = os.environ.get("XTGT", "/home/user/xproxy-target/release")
# rzmq_proxy built against rzmq 0.5.26 + rzmq_zc/patches (rzmq_zc/server)
XZC = os.environ.get("XZC", "/home/user/rzmq-zc-target/release")
SERVERS = {
    "libzmq-c": [f"{XBIN}/xproxy_libzmq"],
    "omq-c": [f"{XBIN}/xproxy_omqc"],
    "rust-zmq": [f"{XTGT}/libzmq_proxy"],
    "omq-tokio": [f"{XTGT}/omq_proxy"],
    # same server with 2 IO threads: sidesteps the 64-entry control-ring livelock
    "omq-tokio-io2": [f"{XTGT}/omq_proxy", "--io-threads", "2"],
    # user-side workarounds applied (see servers/src/bin/omq_proxy_hardened.rs)
    "omq-hardened": [f"{XTGT}/omq_proxy_hardened"],
    "zeromq": [f"{XTGT}/zeromq_proxy"],
    # rzmq 0.5.26 has no XPUB/XSUB: subscribe-all SUB->PUB forwarder (servers/src/bin/rzmq_proxy.rs)
    "rzmq-tokio": [f"{XTGT}/rzmq_proxy", "--mode", "tokio"],
    "rzmq-uring": [f"{XTGT}/rzmq_proxy", "--mode", "uring", "--throttle", "off"],
    # NNG (SP protocol, not ZMTP): only for the NNG-aware scenarios (topic_filter_cost.py);
    # the pyzmq scenarios in this file cannot talk to them.
    "nng-device": [f"{XBIN}/nng_proxy"],
    "nng-device-tuned": [f"{XBIN}/nng_proxy", "--recvbuf", "1000", "--sendbuf", "1000",
                         "--recvmaxsz", str(16 << 20)],
    "nng-loop": [f"{XBIN}/nng_proxy", "--mode", "loop"],
    # patched rzmq (rzmq_zc/patches): direct receive + SENDMSG_ZC + non-blocking PUB
    "rzmqzc-tokio": [f"{XZC}/rzmq_proxy_zc", "--mode", "tokio"],
    "rzmqzc-uring": [f"{XZC}/rzmq_proxy_zc", "--mode", "uring", "--throttle", "off"],
    "rzmqzc-uring-zc": [f"{XZC}/rzmq_proxy_zc", "--mode", "uring-zc", "--throttle", "off"],
    # same patched binary with direct receive off (stock receive path) for A/B
    "rzmqzc-uring-nodirect": [f"{XZC}/rzmq_proxy_zc", "--mode", "uring", "--throttle", "off",
                              "--rcv-direct-threshold", "0"],
}


class Server:
    def __init__(self, name, extra=()):
        self.name = name
        self.extra = tuple(extra)
        port = random.randint(20000, 60000)
        self.fe = f"tcp://127.0.0.1:{port}"
        self.be = f"tcp://127.0.0.1:{port + 1}"
        self.log = f"/tmp/conf-{name}-{port}.log"
        self._start()

    def _start(self):
        self.proc = subprocess.Popen(
            SERVERS[self.name] + ["--frontend", self.fe, "--backend", self.be, *self.extra],
            stdout=open(self.log, "w"), stderr=subprocess.STDOUT)
        for _ in range(300):
            if "READY" in open(self.log).read():
                break
            time.sleep(0.01)

    def restart(self):
        self.proc.kill()
        self.proc.wait()
        self._start()

    def alive(self):
        return self.proc.poll() is None and "PROXY EXIT" not in open(self.log).read()

    def tail(self):
        return open(self.log).read()[-600:]

    def stop(self):
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait()
        try:
            os.unlink(self.log)
        except OSError:
            pass


class Ctx:
    """Owns client sockets for one scenario."""

    def __init__(self, srv):
        self.srv = srv
        self.ctx = zmq.Context()
        self.socks = []

    def sock(self, kind, side, **opts):
        s = self.ctx.socket(kind)
        s.setsockopt(zmq.LINGER, 0)
        for k, v in opts.items():
            s.setsockopt(getattr(zmq, k), v)
        s.connect(self.srv.fe if side == "fe" else self.srv.be)
        self.socks.append(s)
        return s

    def close(self):
        for s in self.socks:
            s.close(0)
        self.ctx.term()


def drain(sock, timeout=0.3, multipart=False):
    out = []
    poller = zmq.Poller()
    poller.register(sock, zmq.POLLIN)
    end = time.time() + timeout
    while True:
        left = end - time.time()
        if left <= 0:
            break
        if poller.poll(left * 1000):
            out.append(sock.recv_multipart() if multipart else sock.recv())
        else:
            break
    return out


def pump_until(pub, sub, payload, timeout=5.0, match=None):
    """Publish `payload` every 20 ms until `sub` receives a matching message."""
    match = match or payload
    end = time.time() + timeout
    while time.time() < end:
        pub.send(payload)
        for m in drain(sub, 0.02):
            if m == match:
                return True
    return False


def wait_upstream(xpub_client, want, timeout=3.0):
    """Collect subscription traffic arriving at an upstream XPUB client until
    `want(events)` is true or timeout. Returns the collected events."""
    events = []
    end = time.time() + timeout
    while time.time() < end:
        events += drain(xpub_client, 0.05)
        if want(events):
            break
    return events


SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


class Fail(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise Fail(msg)


# --------------------------------------------------------------------------
@scenario
def basic_filtering(c):
    """End-to-end prefix filtering: SUB('A') gets 'A*', never 'B*'."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, sub, b"A-probe"), "subscription never propagated")
    for i in range(20):
        pub.send(b"B%d" % i)
        pub.send(b"A%d" % i)
    got = drain(sub, 0.5)
    check(all(m.startswith(b"A") for m in got), f"received non-matching: {[m for m in got if not m.startswith(b'A')][:3]}")
    check(len([m for m in got if m != b"A-probe"]) == 20, f"expected 20 A-messages, got {len(got)}")


@scenario
def late_joining_publisher(c):
    """libzmq XSUB replays its subscription set to every newly attached
    publisher (xsub_t::xattach_pipe), so a publisher that connects after the
    subscriber must still reach it."""
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    time.sleep(0.5)  # subscription reaches the proxy before any publisher exists
    pub = c.sock(zmq.PUB, "fe")
    check(pump_until(pub, sub, b"A-late", timeout=5), "late publisher never reached subscriber")


@scenario
def shared_topic_one_unsubscribes(c):
    """Two subscribers on topic 'A'; one UNSUBSCRIBEs. libzmq XPUB only
    reports an unsubscribe when the *last* subscriber of a topic leaves
    (mtrie rm == last_value_removed) and XSUB refcounts, so the other
    subscriber must keep receiving 'A'."""
    pub = c.sock(zmq.PUB, "fe")
    s1 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    s2 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, s1, b"A-p1"), "s1 never subscribed")
    check(pump_until(pub, s2, b"A-p2"), "s2 never subscribed")
    drain(s1, 0.1)
    s1.setsockopt(zmq.UNSUBSCRIBE, b"A")
    time.sleep(0.5)  # let the cancel propagate through the proxy
    drain(s2, 0.1)
    for i in range(10):
        pub.send(b"A-after-%d" % i)
        time.sleep(0.01)
    got = drain(s2, 1.0)
    check(len(got) == 10, f"remaining subscriber received {len(got)}/10 messages after the other unsubscribed")


@scenario
def shared_topic_one_disconnects(c):
    """Two subscribers on 'A'; one disconnects. The other keeps receiving."""
    pub = c.sock(zmq.PUB, "fe")
    s1 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    s2 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, s1, b"A-p1"), "s1 never subscribed")
    check(pump_until(pub, s2, b"A-p2"), "s2 never subscribed")
    s1.close(0)
    c.socks.remove(s1)
    time.sleep(0.5)
    drain(s2, 0.1)
    for i in range(10):
        pub.send(b"A-after-%d" % i)
        time.sleep(0.01)
    got = drain(s2, 1.0)
    check(len(got) == 10, f"remaining subscriber received {len(got)}/10")


@scenario
def upstream_cancel_on_unsubscribe(c):
    """SUB unsubscribes -> the proxy must cancel upstream (publisher-side
    XPUB sees '\\x00A')."""
    up = c.sock(zmq.XPUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    ev = wait_upstream(up, lambda e: b"\x01A" in e)
    check(b"\x01A" in ev, f"subscribe never reached publisher: {ev}")
    sub.setsockopt(zmq.UNSUBSCRIBE, b"A")
    ev = wait_upstream(up, lambda e: b"\x00A" in e)
    check(b"\x00A" in ev, f"unsubscribe never reached publisher: {ev}")


@scenario
def upstream_cancel_on_subscriber_disconnect(c):
    """libzmq XPUB turns a departing subscriber's subscriptions into
    unsubscribe notifications (xpub_t::xpipe_terminated), so the proxy
    cancels upstream and publishers stop sending data nobody wants."""
    up = c.sock(zmq.XPUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    ev = wait_upstream(up, lambda e: b"\x01A" in e)
    check(b"\x01A" in ev, f"subscribe never reached publisher: {ev}")
    sub.close(0)
    c.socks.remove(sub)
    ev = wait_upstream(up, lambda e: b"\x00A" in e, timeout=3)
    check(b"\x00A" in ev, "subscriber disconnected but its subscription was never cancelled upstream (leak)")


@scenario
def subscription_leak_under_churn(c):
    """50 short-lived subscribers each subscribe to a unique topic and then
    disconnect. Count upstream subscriptions still active afterwards."""
    up = c.sock(zmq.XPUB, "fe", XPUB_VERBOSER=1)
    active = {}
    for i in range(50):
        s = c.ctx.socket(zmq.SUB)
        s.setsockopt(zmq.LINGER, 0)
        s.setsockopt(zmq.SUBSCRIBE, b"T%03d" % i)
        s.connect(c.srv.be)
        ev = wait_upstream(up, lambda e, i=i: (b"\x01T%03d" % i) in e, timeout=2)
        for e in ev:
            if e[:1] == b"\x01":
                active[e[1:]] = active.get(e[1:], 0) + 1
            elif e[:1] == b"\x00":
                active[e[1:]] = active.get(e[1:], 0) - 1
        s.close(0)
    for e in wait_upstream(up, lambda e: False, timeout=2):
        if e[:1] == b"\x01":
            active[e[1:]] = active.get(e[1:], 0) + 1
        elif e[:1] == b"\x00":
            active[e[1:]] = active.get(e[1:], 0) - 1
    leaked = sum(1 for v in active.values() if v > 0)
    check(leaked == 0, f"{leaked}/50 subscriptions of disconnected subscribers still active upstream")


@scenario
def upstream_subscribe_dedup(c):
    """Several subscribers of one topic: libzmq forwards ONE subscribe
    upstream (non-verbose XPUB reports only the first). Publisher-side XPUB
    is VERBOSER so it shows every subscribe/cancel it receives."""
    up = c.sock(zmq.XPUB, "fe", XPUB_VERBOSER=1)
    subs = [c.sock(zmq.SUB, "be", SUBSCRIBE=b"A") for _ in range(5)]
    ev = wait_upstream(up, lambda e: False, timeout=1.5)
    n = ev.count(b"\x01A")
    check(n == 1, f"{n} upstream SUBSCRIBE messages for one topic with 5 subscribers (libzmq: 1)")


@scenario
def multipart_integrity(c):
    """Multipart messages stay atomic and ordered through the proxy."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"M")
    check(pump_until(pub, sub, b"M-probe"), "never subscribed")
    big = os.urandom(1 << 20)
    for i in range(20):
        pub.send_multipart([b"M", b"%d" % i, big, b"tail%d" % i])
    got = drain(sub, 2.0, multipart=True)
    ok = [m for m in got if len(m) == 4 and m[2] == big and m[1] == b"%d" % int(m[1]) and m[3] == b"tail" + m[1]]
    order = [int(m[1]) for m in ok]
    check(len(ok) == 20 and order == sorted(order), f"got {len(got)} msgs, {len(ok)} intact, order={order[:5]}...")


@scenario
def xsub_client_sends_data_upstream(c):
    """An XSUB peer on the subscriber side may send non-subscription messages
    upstream; libzmq XPUB passes them to the proxy which forwards them to
    publishers. The proxy must at minimum keep running."""
    up = c.sock(zmq.XPUB, "fe")
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, sub, b"A-probe"), "never subscribed")
    x = c.sock(zmq.XSUB, "be")
    time.sleep(0.3)
    x.send(b"hello-upstream")
    time.sleep(0.5)
    alive = c.srv.alive()
    still_forwards = pump_until(pub, sub, b"A-after", timeout=2)
    got_up = b"hello-upstream" in drain(up, 0.3)
    check(alive and still_forwards,
          f"proxy died or stopped forwarding after one upstream data message (alive={alive}, "
          f"forwarding={still_forwards}); server log: {c.srv.tail()!r}")
    check(got_up, "upstream data message was not forwarded to publishers (libzmq forwards it)")


@scenario
def xsub_client_sends_empty_message(c):
    """Empty message from an XSUB peer: libzmq forwards it; must not kill the proxy."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, sub, b"A-probe"), "never subscribed")
    x = c.sock(zmq.XSUB, "be")
    time.sleep(0.3)
    x.send(b"")
    time.sleep(0.5)
    check(c.srv.alive() and pump_until(pub, sub, b"A-after", timeout=2),
          f"proxy died/stalled after an empty upstream message; log: {c.srv.tail()!r}")


@scenario
def xsub_client_sends_multipart_subscribe(c):
    """Two-frame message whose first frame is a subscription: libzmq XPUB
    applies it; the proxy must not die."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, sub, b"A-probe"), "never subscribed")
    x = c.sock(zmq.XSUB, "be")
    time.sleep(0.3)
    x.send_multipart([b"\x01B", b"extra"])
    time.sleep(0.5)
    check(c.srv.alive() and pump_until(pub, sub, b"A-after", timeout=2),
          f"proxy died/stalled after a multipart upstream message; log: {c.srv.tail()!r}")


@scenario
def overlapping_prefixes(c):
    """SUB1('AB') and SUB2('A'); after SUB2 unsubscribes 'A', SUB1 must
    still get 'ABx' (upstream 'AB' subscription must survive)."""
    pub = c.sock(zmq.PUB, "fe")
    s1 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"AB")
    s2 = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, s1, b"AB-p"), "s1 never subscribed")
    check(pump_until(pub, s2, b"A-p", match=b"A-p"), "s2 never subscribed")
    s2.setsockopt(zmq.UNSUBSCRIBE, b"A")
    time.sleep(0.5)
    drain(s1, 0.1)
    for i in range(10):
        pub.send(b"ABx%d" % i)
    got = drain(s1, 1.0)
    check(len(got) == 10, f"s1 got {len(got)}/10 'AB*' messages after s2 dropped 'A'")


@scenario
def same_socket_double_subscribe(c):
    """One SUB subscribes 'A' twice and unsubscribes once: libzmq SUB
    refcounts, it must still receive 'A' (end-to-end through the proxy)."""
    pub = c.sock(zmq.PUB, "fe")
    s = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    s.setsockopt(zmq.SUBSCRIBE, b"A")
    check(pump_until(pub, s, b"A-p"), "never subscribed")
    s.setsockopt(zmq.UNSUBSCRIBE, b"A")
    time.sleep(0.5)
    drain(s, 0.1)
    for i in range(10):
        pub.send(b"A%d" % i)
    got = drain(s, 1.0)
    check(len(got) == 10, f"got {len(got)}/10 after subscribe x2 / unsubscribe x1")


@scenario
def subscriber_with_65_topics(c):
    """A subscriber that arrives with 65 topic subscriptions (common for
    market-data style clients). The proxy must stay responsive: an existing
    subscriber keeps receiving and new subscribers can join."""
    pub = c.sock(zmq.PUB, "fe")
    good = c.sock(zmq.SUB, "be", SUBSCRIBE=b"G")
    check(pump_until(pub, good, b"G-1"), "healthy subscriber never subscribed")
    bad = c.ctx.socket(zmq.SUB)
    bad.setsockopt(zmq.LINGER, 0)
    for i in range(65):
        bad.setsockopt(zmq.SUBSCRIBE, b"t%05d" % i)
    bad.connect(c.srv.be)
    c.socks.append(bad)
    time.sleep(1.0)
    pid = c.srv.proc.pid
    clk = os.sysconf("SC_CLK_TCK")

    def cpu():
        f = open(f"/proc/{pid}/stat").read().split()
        return (int(f[13]) + int(f[14])) / clk

    c0 = cpu()
    time.sleep(1.0)
    c.result["idle_server_cpu_pct"] = round(100 * (cpu() - c0))
    still = pump_until(pub, good, b"G-2", timeout=3)
    late = c.sock(zmq.SUB, "be", SUBSCRIBE=b"L")
    joins = pump_until(pub, late, b"L-1", timeout=3)
    topic_ok = pump_until(pub, bad, b"t00064-x", timeout=3)
    check(still and joins and topic_ok,
          f"server wedged: existing subscriber receives={still}, new subscriber can join={joins}, "
          f"65th topic delivered={topic_ok}, idle server CPU={c.result['idle_server_cpu_pct']}%")


@scenario
def subscription_storm_20k(c):
    """One subscriber subscribes to 20,000 topics. Time until the proxy has
    forwarded all of them upstream and the last topic is deliverable."""
    up = c.sock(zmq.XPUB, "fe", XPUB_VERBOSER=1, RCVHWM=0)
    sub = c.sock(zmq.SUB, "be", SNDHWM=0)
    time.sleep(0.3)
    t0 = time.time()
    n = 20000
    for i in range(n):
        sub.setsockopt(zmq.SUBSCRIBE, b"topic-%06d" % i)
    seen = 0
    end = time.time() + 60
    while seen < n and time.time() < end:
        seen += sum(1 for e in drain(up, 0.2) if e[:1] == b"\x01")
    dt = time.time() - t0
    c.result["storm_upstream_s"] = round(dt, 2)
    c.result["storm_forwarded"] = seen
    check(seen >= n, f"only {seen}/{n} subscriptions reached the publisher within 60 s")
    check(dt < 10, f"20k subscriptions took {dt:.1f}s to propagate")


@scenario
def publisher_restart(c):
    """Publisher goes away and a new one connects: it must receive the
    existing subscription set from the proxy's XSUB (replay)."""
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    pub = c.sock(zmq.PUB, "fe")
    check(pump_until(pub, sub, b"A-1"), "never subscribed")
    pub.close(0)
    c.socks.remove(pub)
    time.sleep(0.3)
    pub2 = c.sock(zmq.PUB, "fe")
    check(pump_until(pub2, sub, b"A-2", timeout=5), "restarted publisher never reached subscriber")


@scenario
def burst_of_large_messages(c):
    """50 x 512 KiB messages published back-to-back (far below the default
    HWM of 1000) to one ready subscriber: libzmq queues and delivers all."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"L")
    check(pump_until(pub, sub, b"L-probe"), "never subscribed")
    body = os.urandom(512 * 1024)
    for i in range(50):
        pub.send(b"L%03d" % i + body)
    got = drain(sub, 3.0)
    ok = [m for m in got if m[4:] == body]
    c.result["delivered"] = len(ok)
    check(len(ok) == 50, f"delivered {len(ok)}/50 intact 512 KiB messages from a single burst")


@scenario
def server_restart_recovery(c):
    """The proxy process is restarted on the same ports while libzmq clients
    stay up; clients reconnect and re-send subscriptions, data must resume."""
    pub = c.sock(zmq.PUB, "fe")
    sub = c.sock(zmq.SUB, "be", SUBSCRIBE=b"A")
    check(pump_until(pub, sub, b"A-1"), "never subscribed")
    c.srv.restart()
    t0 = time.time()
    ok = pump_until(pub, sub, b"A-2", timeout=10)
    c.result["recovery_s"] = round(time.time() - t0, 2)
    check(ok, "no data within 10 s after the proxy restarted")


@scenario
def ten_subscribers_ten_topics_connect_together(c):
    """10 subscribers with 10 topics each (100 subscriptions in total, none
    of them 'heavy') connect at the same moment. All must get their data."""
    pub = c.sock(zmq.PUB, "fe")
    subs = []
    for k in range(10):
        s = c.ctx.socket(zmq.SUB)
        s.setsockopt(zmq.LINGER, 0)
        for i in range(10):
            s.setsockopt(zmq.SUBSCRIBE, b"s%02d-t%02d" % (k, i))
        subs.append(s)
        c.socks.append(s)
    for s in subs:
        s.connect(c.srv.be)
    time.sleep(0.5)
    ok = [pump_until(pub, s, b"s%02d-t09-x" % k, timeout=3) for k, s in enumerate(subs)]
    check(all(ok), f"{ok.count(False)}/10 subscribers never received data")


@scenario
def restart_with_ten_subscribers_ten_topics(c):
    """Broker restart: 10 connected subscribers x 10 topics reconnect and
    re-send 100 subscriptions at once. Data must resume for all of them."""
    pub = c.sock(zmq.PUB, "fe")
    subs = []
    for k in range(10):
        s = c.ctx.socket(zmq.SUB)
        s.setsockopt(zmq.LINGER, 0)
        for i in range(10):
            s.setsockopt(zmq.SUBSCRIBE, b"s%02d-t%02d" % (k, i))
        s.connect(c.srv.be)
        subs.append(s)
        c.socks.append(s)
        check(pump_until(pub, s, b"s%02d-t00-a" % k, timeout=3), f"sub {k} never subscribed")
    c.srv.restart()
    t0 = time.time()
    ok = [pump_until(pub, s, b"s%02d-t09-b" % k, timeout=6) for k, s in enumerate(subs)]
    c.result["recovery_s"] = round(time.time() - t0, 2)
    check(all(ok), f"after restart {ok.count(False)}/10 subscribers never received data again")


def run(servers, only, extra_args):
    results = {}
    for name in servers:
        results[name] = {}
        for fn in SCENARIOS:
            if only and fn.__name__ not in only:
                continue
            srv = Server(name, extra_args.get(name, ()))
            c = Ctx(srv)
            c.result = {}
            t0 = time.time()
            try:
                fn(c)
                status, detail = "PASS", ""
            except Fail as e:
                status, detail = "FAIL", str(e)
            except Exception as e:  # noqa: BLE001
                status, detail = "ERROR", f"{type(e).__name__}: {e}\n{traceback.format_exc()[-500:]}"
            alive = srv.alive()
            if status == "PASS" and not alive:
                status, detail = "FAIL", f"server exited: {srv.tail()!r}"
            c.result.update(status=status, detail=detail, server_alive=alive,
                            seconds=round(time.time() - t0, 2))
            results[name][fn.__name__] = c.result
            print(f"{name:10s} {fn.__name__:40s} {status:5s} {detail[:160]}", flush=True)
            c.close()
            srv.stop()
    return results


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--servers", default=",".join(k for k in SERVERS if not k.startswith("nng")))
    ap.add_argument("--only", default="")
    ap.add_argument("--json", default="")
    a = ap.parse_args()
    res = run(a.servers.split(","), set(filter(None, a.only.split(","))), {})
    if a.json:
        json.dump(res, open(a.json, "w"), indent=1)
