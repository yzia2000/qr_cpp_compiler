#!/usr/bin/env python3
"""Raw SP (nanomsg / NNG) TCP peer for adversarial frames — the NNG counterpart of zmtp_raw.py.

Completes the 8-byte SP handshake as the given protocol, then sends one message header
declaring DECLARED_LEN bytes followed by SEND_BYTES of body, and reports whether the server
kept the connection. Used to check what a server does with a message that *declares* a huge
body: NNG allocates the declared size up front unless NNG_OPT_RECVMAXSZ is set.

SP over TCP: each side sends 00 'S' 'P' 00 <protocol id, 16-bit BE> 00 00; every message is
then an 8-byte big-endian length followed by the body.

Usage: sp_raw.py HOST PORT PROTO DECLARED_LEN [SEND_BYTES] [HOLD_S]
       PROTO: pub | sub   (a publisher talks to the broker's --frontend)
"""
import socket
import struct
import sys
import time

PROTOCOLS = {"pub": 0x20, "sub": 0x21}


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    proto, declared = PROTOCOLS[sys.argv[3]], int(sys.argv[4])
    send_bytes = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    hold = float(sys.argv[6]) if len(sys.argv) > 6 else 1.0
    s = socket.create_connection((host, port))
    s.sendall(b"\x00SP\x00" + struct.pack(">H", proto) + b"\x00\x00")
    s.settimeout(5)
    peer = b""
    while len(peer) < 8:
        chunk = s.recv(8 - len(peer))
        if not chunk:
            print("peer closed during handshake")
            return
        peer += chunk
    s.sendall(struct.pack(">Q", declared))
    chunk = b"\xab" * 65536
    left = send_bytes
    while left > 0:
        n = min(left, len(chunk))
        s.sendall(chunk[:n])
        left -= n
    time.sleep(hold)
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
