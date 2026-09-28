#!/usr/bin/env python3
"""Does the proxy's XSUB talk to a ZMTP 3.0 publisher (libzmq 4.1/4.2 era)?

A raw socket plays a ZMTP 3.0 PUB (greeting minor = 0) and connects to the
proxy's XSUB side. A libzmq SUB subscribes to b"A" on the XPUB side. We then
record how the proxy forwards that subscription to the 3.0 publisher:

  * as a data frame  b"\\x01A"      -> what ZMTP 3.0 / RFC 23 requires
                                       (libzmq uses its v2 encoder for 3.0 peers)
  * as a COMMAND frame "SUBSCRIBE"  -> ZMTP 3.1-only; a real libzmq 4.1/4.2
                                       publisher ignores it and never publishes.

Usage: zmtp30_publisher.py SERVER_NAME
"""
import os
import socket
import struct
import sys
import time

import zmq

sys.path.insert(0, os.path.dirname(__file__))
from conformance import Server  # noqa: E402
from zmtp_raw import command, recv_exact  # noqa: E402


def greeting_30():
    return (b"\xff" + b"\x00" * 8 + b"\x7f" + b"\x03\x00" + b"NULL".ljust(20, b"\x00")
            + b"\x00" + b"\x00" * 31)


def read_frame(s, timeout):
    flags = recv_exact(s, 1, timeout)[0]
    size = recv_exact(s, 8 if flags & 0x02 else 1, timeout)
    size = struct.unpack(">Q", size)[0] if flags & 0x02 else size[0]
    return flags, recv_exact(s, size, timeout)


def main():
    name = sys.argv[1]
    srv = Server(name)
    host, port = srv.fe.replace("tcp://", "").split(":")
    s = socket.create_connection((host, int(port)))
    s.sendall(greeting_30())
    peer_greeting = recv_exact(s, 64)
    prop = b"\x0bSocket-Type" + struct.pack(">I", 3) + b"PUB"
    s.sendall(command(b"READY", prop))
    read_frame(s, 5)  # peer READY
    ctx = zmq.Context()
    sub = ctx.socket(zmq.SUB)
    sub.setsockopt(zmq.SUBSCRIBE, b"A")
    sub.connect(srv.be)
    seen = []
    end = time.time() + 3
    while time.time() < end:
        try:
            flags, body = read_frame(s, 0.5)
            seen.append((flags, body))
        except (socket.timeout, TimeoutError):
            continue
        except ConnectionError:
            seen.append(("closed", b""))
            break
    verdict = "no subscription received"
    for flags, body in seen:
        if flags == 0 and body == b"\x01A":
            verdict = "OK: legacy data-frame subscription b'\\x01A' (ZMTP 3.0 compatible)"
            break
        if isinstance(flags, int) and flags & 0x04 and body[1:10] == b"SUBSCRIBE":
            verdict = "INCOMPATIBLE: sent ZMTP 3.1 SUBSCRIBE command to a ZMTP 3.0 peer"
            break
    print(f"{name:10s} peer greeting minor={peer_greeting[11]} frames={[(f, b[:12]) for f, b in seen]} -> {verdict}")
    sub.close(0)
    ctx.term()
    s.close()
    srv.stop()


if __name__ == "__main__":
    main()
