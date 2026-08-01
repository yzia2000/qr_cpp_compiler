#!/usr/bin/env python3
"""Generate a synthetic OPRA-like option-quote feed as a pcap file.

Quotes are sampled from ground-truth SVI surfaces so the C++ pipeline's
IV inversion and SVI fits can be validated against known parameters
(written to a `<out>.truth.npz` sidecar).

Wire format (all little-endian):
  pcap global header (24 B, magic 0xa1b2c3d4, linktype 1 = Ethernet)
  per packet: pcap record header (16 B)
              Ethernet (14 B) + IPv4 (20 B) + UDP (8 B)
              payload: u8 msg_count, then msg_count x 32-byte QuoteMsg:
                u32 seq | u16 underlying_id | u8 expiry_idx | u8 flags(bit0=is_call)
                u32 strike_1e4 | u32 bid_1e4 | u32 ask_1e4 | u32 spot_1e4 | u64 ts_ns

IP/UDP checksums are 0 (checksum generation/validation is out of scope for
a synthetic feed; the reader does not inspect them).
"""

import argparse
import struct

import numpy as np
from scipy.special import erf

MSGS_PER_PACKET = 40
QUOTE_SIZE = 32
PAYLOAD = 1 + MSGS_PER_PACKET * QUOTE_SIZE
WIRE_LEN = 14 + 20 + 8 + PAYLOAD          # 1323
RECORD = 16 + WIRE_LEN                    # + pcap record header

N_UNDERLYINGS = 64
EXPIRY_DAYS = np.array([7, 14, 30, 61, 91, 182, 365, 730])
N_EXPIRIES = len(EXPIRY_DAYS)
RATE = 0.03

QUOTE_DTYPE = np.dtype([
    ("seq", "<u4"), ("underlying_id", "<u2"), ("expiry_idx", "u1"), ("flags", "u1"),
    ("strike_1e4", "<u4"), ("bid_1e4", "<u4"), ("ask_1e4", "<u4"),
    ("spot_1e4", "<u4"), ("ts_ns", "<u8"),
])


def norm_cdf(x):
    return 0.5 * (1.0 + erf(x / np.sqrt(2.0)))


def bs_price(is_call, s, k, t, r, vol):
    sqt = np.sqrt(t)
    d1 = (np.log(s / k) + (r + 0.5 * vol * vol) * t) / (vol * sqt)
    d2 = d1 - vol * sqt
    call = s * norm_cdf(d1) - k * np.exp(-r * t) * norm_cdf(d2)
    put = call - s + k * np.exp(-r * t)
    return np.where(is_call, call, put)


def svi_total_var(k, a, b, rho, m, sigma):
    d = k - m
    return a + b * (rho * d + np.sqrt(d * d + sigma * sigma))


def make_surfaces(rng):
    """Ground-truth SVI params per (underlying, expiry) slice."""
    T = EXPIRY_DAYS / 365.0
    vol_atm = rng.uniform(0.15, 0.40, size=(N_UNDERLYINGS, 1)) \
        * (1.0 + rng.uniform(-0.05, 0.15, size=(N_UNDERLYINGS, N_EXPIRIES)))
    w_atm = vol_atm**2 * T                                   # ATM total variance
    beta = rng.uniform(0.5, 2.0, size=(N_UNDERLYINGS, N_EXPIRIES))
    b = beta * w_atm                                          # skew scales with level
    rho = rng.uniform(-0.8, -0.3, size=(N_UNDERLYINGS, N_EXPIRIES))
    m = rng.uniform(-0.05, 0.05, size=(N_UNDERLYINGS, N_EXPIRIES))
    sigma = rng.uniform(0.10, 0.30, size=(N_UNDERLYINGS, N_EXPIRIES))
    # a from the ATM condition w(0) = w_atm, floored for positivity everywhere:
    a = w_atm - b * (rho * (-m) + np.sqrt(m * m + sigma * sigma))
    min_w = a + b * sigma * np.sqrt(1.0 - rho**2)
    a += np.maximum(1e-4 - min_w, 0.0)
    spot = rng.uniform(50.0, 500.0, size=N_UNDERLYINGS)
    return dict(a=a, b=b, rho=rho, m=m, sigma=sigma, spot=spot, T=T, r=RATE)


def packet_template():
    eth = struct.pack("<6s6sH", b"\x02\x00\x00\x00\x00\x02",
                      b"\x02\x00\x00\x00\x00\x01", struct.unpack("<H", struct.pack(">H", 0x0800))[0])
    ip = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + 8 + PAYLOAD, 0, 0x4000,
                     64, 17, 0, bytes([10, 0, 0, 1]), bytes([239, 1, 1, 1]))
    udp = struct.pack(">HHHH", 31337, 12345, 8 + PAYLOAD, 0)
    return eth + ip + udp + bytes([MSGS_PER_PACKET])


def generate(out_path, target_bytes, seed):
    rng = np.random.default_rng(seed)
    surf = make_surfaces(rng)
    n_packets = max(1, (target_bytes - 24) // RECORD)
    n_quotes = n_packets * MSGS_PER_PACKET

    tmpl = packet_template()
    hdr_len = len(tmpl) - 1 - 0  # eth+ip+udp+count byte precedes quotes
    assert len(tmpl) == 14 + 20 + 8 + 1

    with open(out_path, "wb") as f:
        f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
        seq0 = 0
        t0_ns = 1_722_500_000 * 10**9  # fixed epoch base for determinism
        block_packets = 25_000  # 1M quotes per block
        for start in range(0, n_packets, block_packets):
            npk = min(block_packets, n_packets - start)
            nq = npk * MSGS_PER_PACKET
            uid = rng.integers(0, N_UNDERLYINGS, nq)
            eidx = rng.integers(0, N_EXPIRIES, nq).astype(np.uint8)
            T = surf["T"][eidx]
            spot = surf["spot"][uid]
            kmag = rng.normal(0.0, 0.25 * np.sqrt(T))
            k = np.clip(kmag, -0.6, 0.6)                     # log-moneyness ln(K/F)
            fwd = spot * np.exp(RATE * T)
            strike = fwd * np.exp(k)
            a_, b_, rho_, m_, sg_ = (surf[key][uid, eidx] for key in ("a", "b", "rho", "m", "sigma"))
            w = svi_total_var(k, a_, b_, rho_, m_, sg_)
            vol_true = np.sqrt(w / T)
            # quote noise: ~30 bp relative vol perturbation per quote
            vol_q = vol_true * np.exp(rng.normal(0.0, 0.003, nq))
            is_call = k >= 0                                  # OTM convention
            price = bs_price(is_call, spot, strike, T, RATE, vol_q)
            half_spread = np.maximum(price * rng.uniform(0.001, 0.005, nq), 2e-4)
            bid = np.maximum(price - half_spread, 1e-4)
            ask = price + half_spread

            q = np.zeros(nq, dtype=QUOTE_DTYPE)
            q["seq"] = (seq0 + np.arange(nq)) & 0xFFFFFFFF
            q["underlying_id"] = uid
            q["expiry_idx"] = eidx
            q["flags"] = is_call.astype(np.uint8)
            q["strike_1e4"] = np.round(strike * 1e4).astype(np.uint64)
            q["bid_1e4"] = np.round(bid * 1e4).astype(np.uint64)
            q["ask_1e4"] = np.round(ask * 1e4).astype(np.uint64)
            q["spot_1e4"] = np.round(spot * 1e4).astype(np.uint64)
            q["ts_ns"] = t0_ns + (seq0 + np.arange(nq)) * 1000
            seq0 += nq

            recs = np.zeros((npk, RECORD), dtype=np.uint8)
            ts_us = (start + np.arange(npk)) * 100           # 100 us between packets
            pcap_hdr = np.zeros(npk, dtype=np.dtype(
                [("sec", "<u4"), ("usec", "<u4"), ("incl", "<u4"), ("orig", "<u4")]))
            pcap_hdr["sec"] = 1_722_500_000 + ts_us // 1_000_000
            pcap_hdr["usec"] = ts_us % 1_000_000
            pcap_hdr["incl"] = WIRE_LEN
            pcap_hdr["orig"] = WIRE_LEN
            recs[:, :16] = pcap_hdr.view(np.uint8).reshape(npk, 16)
            recs[:, 16:16 + len(tmpl)] = np.frombuffer(tmpl, dtype=np.uint8)
            recs[:, 16 + len(tmpl):] = q.view(np.uint8).reshape(npk, MSGS_PER_PACKET * QUOTE_SIZE)
            f.write(recs.tobytes())

    np.savez(str(out_path) + ".truth.npz", **surf,
             expiry_days=EXPIRY_DAYS, n_quotes=n_quotes, seed=seed)
    return n_packets, n_quotes


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("out")
    ap.add_argument("--size-gb", type=float, default=1.0)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()
    n_packets, n_quotes = generate(args.out, int(args.size_gb * 1e9), args.seed)
    print(f"wrote {args.out}: {n_packets} packets, {n_quotes} quotes "
          f"({(24 + n_packets * RECORD) / 1e6:.1f} MB)")


if __name__ == "__main__":
    main()
