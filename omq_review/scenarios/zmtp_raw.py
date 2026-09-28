#!/usr/bin/env python3
"""Raw ZMTP 3.1 peer for adversarial / edge-case frames (no libzmq involved).

Connects to a server port, performs a NULL-mechanism handshake as the given
socket type, then sends one crafted frame header. Used to check how a server
handles a frame that *declares* a huge body (the classic pre-allocation DoS):
libzmq rejects/handles allocation failure per connection; a Rust process that
pre-allocates the declared length aborts on allocation failure.

Usage: zmtp_raw.py HOST PORT SOCKTYPE DECLARED_LEN [SEND_BYTES]
"""
import socket
import struct
import sys
import time


def greeting():
    # signature(10) + version 3.1 + mechanism "NULL" padded to 20 + as-server 0 + filler 31
    return (b"\xff" + b"\x00" * 8 + b"\x7f" + b"\x03\x01" + b"NULL".ljust(20, b"\x00")
            + b"\x00" + b"\x00" * 31)


def command(name, body=b""):
    payload = bytes([len(name)]) + name + body
    if len(payload) < 256:
        return b"\x04" + bytes([len(payload)]) + payload
    return b"\x06" + struct.pack(">Q", len(payload)) + payload


def ready(socktype):
    prop = b"\x0bSocket-Type" + struct.pack(">I", len(socktype)) + socktype
    return command(b"READY", prop)


def recv_exact(s, n, timeout=5):
    s.settimeout(timeout)
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("closed")
        buf += chunk
    return buf


def main():
    host, port, socktype, declared = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), int(sys.argv[4])
    send_bytes = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    s = socket.create_connection((host, port))
    s.sendall(greeting())
    recv_exact(s, 64)
    s.sendall(ready(socktype))
    # read peer READY command (short or long command frame)
    flags = recv_exact(s, 1)[0]
    size = recv_exact(s, 8 if flags & 0x02 else 1)
    size = struct.unpack(">Q", size)[0] if flags & 0x02 else size[0]
    recv_exact(s, size)
    # long data frame header declaring `declared` bytes, then some body bytes
    s.sendall(b"\x02" + struct.pack(">Q", declared))
    if send_bytes:
        s.sendall(b"\xab" * send_bytes)
    time.sleep(1.0)
    try:
        s.settimeout(0.5)
        data = s.recv(1)
        state = "peer closed connection" if data == b"" else "connection open"
    except socket.timeout:
        state = "connection open (no data)"
    except ConnectionError as e:
        state = f"connection error: {e}"
    print(state)


if __name__ == "__main__":
    main()
