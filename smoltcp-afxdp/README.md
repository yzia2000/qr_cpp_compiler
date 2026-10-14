# smoltcp-afxdp

A feasibility prototype: an AF_XDP socket used as a zero-copy
[`smoltcp::phy::Device`](https://docs.rs/smoltcp/0.14/smoltcp/phy/trait.Device.html).
The only dependencies are `smoltcp` 0.14 and `libc`. It does not use libbpf, libxdp, or a
BPF C toolchain.

## Verdict: viable, and small

The device fits smoltcp's token API with no adaptation layer and no copies:

| smoltcp concept | AF_XDP mapping |
|---|---|
| `Device::receive` | pop one descriptor from the RX ring |
| `RxToken::consume(f)` | `f(&umem[desc.addr..][..len])`, the frame read in place. On drop, the frame goes back to the fill ring |
| `TxToken::consume(len, f)` | take a frame from the free list, `f(&mut umem[addr..][..len])`, push it on the TX ring |
| `Device::transmit` returning `None` | backpressure when the free list and completion ring are empty or the TX ring is full |
| `DeviceCapabilities` | Ethernet medium, MTU and burst size come from `Config` |

The rx and tx tokens returned together by `receive()` borrow two separate halves of the
device (`RxSide`: rx + fill rings, `TxSide`: tx + completion rings + free list), and they
always touch different UMEM frames. That means no `RefCell` and no runtime borrow checks.

### Lines of code (excluding blanks and comments, after `cargo fmt`)

| file | what | code lines |
|---|---|---:|
| `src/ring.rs` | SPSC producer/consumer rings over the mmap'd kernel rings | 96 |
| `src/bpf.rs` | raw `bpf(2)`: XSKMAP, a 6-instruction redirect program, attach via BPF link, map update | 141 |
| `src/device.rs` | UMEM, socket setup and bind, `Config`, the `Device`/`RxToken`/`TxToken` impls, stats, flush | 331 |
| `src/lib.rs` | re-exports | 4 |
| **library total** | | **~570** |
| `examples/tcp_echo.rs` | TCP echo server + ping responder, poll(2)-driven | 59 |
| `scripts/veth_test.sh` | end-to-end test using a netns and a veth pair | 35 |

The smoltcp-specific part (`Device` and the tokens) is only about **90 lines**. The rest is
AF_XDP plumbing that any Rust user would need. With a crate such as `xsk-rs` or `aya`, the
library would likely shrink to about 150–250 lines, at the cost of a libbpf or aya dependency.

## Tested

`scripts/veth_test.sh [skb|drv]` creates `veth0` (smoltcp, `10.77.0.2`) and `veth1` inside
a network namespace (Linux stack, `10.77.0.1`). It then runs:

* `ping` → answered by smoltcp's ICMP handling (about 0.1–0.2 ms RTT)
* a short TCP echo
* an 8 MiB random-payload TCP echo, compared byte for byte with `cmp`

The test passes in both generic (`XDP_FLAGS_SKB_MODE`) and native (`XDP_FLAGS_DRV_MODE`) XDP
on kernel 6.18. One rough sanity figure: 128 MiB was echoed in ~1.2 s (≈110 MB/s each way)
over veth with one core, a 64 KiB TCP window and no tuning. Treat it as "it isn't
pathologically slow", not as a benchmark. veth only supports copy mode, so zero-copy on a
real NIC (ice, i40e, mlx5) is still untested.

```sh
sudo ./scripts/veth_test.sh skb
# or by hand:
sudo cargo run --release --example tcp_echo -- eth1 192.168.1.50/24 drv
```

## Design notes / gotchas found

* **No libbpf.** The redirect program
  `bpf_redirect_map(&xsks_map, ctx->rx_queue_index, XDP_PASS)` is hand-encoded as 6 eBPF
  instructions and loaded with the `bpf(2)` syscall. It is attached with `BPF_LINK_CREATE`,
  so it detaches automatically when the process exits or crashes. Frames on queues that
  have no socket fall through to the kernel (`XDP_PASS`).
* **Checksums on veth.** A kernel-side sender with TX offload emits `CHECKSUM_PARTIAL`
  frames, which arrive in the UMEM with unfinished L4 checksums, and smoltcp rejects them.
  Fix it either with `ethtool -K <peer> tx off` (done in the test) or with
  `Config::checksum` (the example's `nocsum` flag). Real NICs deliver complete frames.
* **TX wakeups.** In copy mode, every TX batch needs a `sendto()` kick. `auto_flush = false`
  plus `dev.flush()` after `iface.poll()` turns that into one syscall per poll instead of one
  per frame. The fill ring uses `XDP_USE_NEED_WAKEUP` and only calls `recvfrom()` when the
  kernel asks.
* **Exhausted TX frames.** smoltcp's `TxToken::consume` can't fail, so if the TX token paired
  with an RX token finds no free frame, the frame is built in scratch space and dropped. The
  drop is counted in `tx_dropped()`, much like a full NIC queue. `transmit()` itself applies
  proper backpressure.
* **The kernel steals the queue.** Everything arriving on the bound queue goes to smoltcp,
  including traffic the host stack wanted, such as SSH. On a real NIC, use a dedicated
  queue plus ethtool n-tuple steering (`ethtool -N eth1 flow-type udp4 dst-port 9000 action 3`),
  or a dedicated interface.

## What a production version would still need (rough size)

| item | est. lines |
|---|---:|
| multi-queue: one socket per queue with shared UMEM (`XDP_SHARED_UMEM`), one smoltcp `Interface` per thread | 80–150 |
| a smarter XDP program that redirects only matching flows (needs a real BPF toolchain or aya, or more hand-written asm) | 50–200 |
| hugepage UMEM, busy-poll (`SO_PREFER_BUSY_POLL`), TX checksum offload via `XDP_TX_METADATA` | 50–100 |
| tests: ring wraparound unit tests, CI in a privileged container | 100–200 |
| netlink attach fallback for kernels older than 5.9 (no XDP BPF links) | ~100 |

In total, a hardened, multi-queue version is roughly **1,000–1,300 lines**.
