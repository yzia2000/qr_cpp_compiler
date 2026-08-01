#include "qr/pcap_reader.hpp"

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cstring>
#include <stdexcept>

namespace qr {
namespace {

constexpr uint32_t PCAP_MAGIC_LE = 0xa1b2c3d4;
constexpr std::size_t GLOBAL_HDR = 24;
constexpr std::size_t REC_HDR = 16;
constexpr std::size_t L2L3L4 = 14 + 20 + 8;   // Ethernet + IPv4 + UDP
constexpr std::size_t QUOTE_SIZE = 32;

// Expiry ladder must match python/gen_pcap.py.
constexpr double EXPIRY_YEARS[8] = {
    7 / 365.0, 14 / 365.0, 30 / 365.0, 61 / 365.0,
    91 / 365.0, 182 / 365.0, 365 / 365.0, 730 / 365.0,
};
constexpr double RATE = 0.03;

template <typename T>
T load(const uint8_t* p) {
    T v;
    std::memcpy(&v, p, sizeof(T));
    return v;
}

struct Mmap {
    const uint8_t* data = nullptr;
    std::size_t len = 0;
    int fd = -1;
    explicit Mmap(const std::string& path) {
        fd = ::open(path.c_str(), O_RDONLY);
        if (fd < 0) throw std::runtime_error("cannot open " + path);
        struct stat st{};
        if (::fstat(fd, &st) != 0 || st.st_size < (off_t)GLOBAL_HDR) {
            ::close(fd);
            throw std::runtime_error("bad pcap file " + path);
        }
        len = (std::size_t)st.st_size;
        void* p = ::mmap(nullptr, len, PROT_READ, MAP_PRIVATE | MAP_POPULATE, fd, 0);
        if (p == MAP_FAILED) {
            ::close(fd);
            throw std::runtime_error("mmap failed for " + path);
        }
        data = (const uint8_t*)p;
    }
    ~Mmap() {
        if (data) ::munmap((void*)data, len);
        if (fd >= 0) ::close(fd);
    }
};

}  // namespace

QuoteBatch read_pcap(const std::string& path) {
    Mmap m(path);
    if (load<uint32_t>(m.data) != PCAP_MAGIC_LE)
        throw std::runtime_error("not a little-endian pcap: " + path);
    if (load<uint32_t>(m.data + 20) != 1)
        throw std::runtime_error("unexpected linktype (want Ethernet)");

    QuoteBatch q;
    q.rate = RATE;
    // Reserve from file size: each 32-byte message yields one quote.
    q.spot.reserve(m.len / QUOTE_SIZE);
    q.strike.reserve(m.len / QUOTE_SIZE);
    q.ttm.reserve(m.len / QUOTE_SIZE);
    q.mid.reserve(m.len / QUOTE_SIZE);
    q.is_call.reserve(m.len / QUOTE_SIZE);
    q.underlying.reserve(m.len / QUOTE_SIZE);
    q.expiry_idx.reserve(m.len / QUOTE_SIZE);

    std::size_t off = GLOBAL_HDR;
    while (off + REC_HDR <= m.len) {
        const uint32_t incl = load<uint32_t>(m.data + off + 8);
        const std::size_t pkt = off + REC_HDR;
        if (pkt + incl > m.len) break;                     // truncated capture tail
        if (incl >= L2L3L4 + 1) {
            const uint8_t* p = m.data + pkt;
            const uint16_t ethertype = (uint16_t)(p[12] << 8 | p[13]);
            if (ethertype == 0x0800 && p[14 + 9] == 17) {  // IPv4 + UDP
                const uint8_t count = p[L2L3L4];
                const uint8_t* msg = p + L2L3L4 + 1;
                const std::size_t avail = (incl - L2L3L4 - 1) / QUOTE_SIZE;
                const std::size_t n = count < avail ? count : avail;
                for (std::size_t i = 0; i < n; ++i, msg += QUOTE_SIZE) {
                    const uint16_t uid = load<uint16_t>(msg + 4);
                    const uint8_t eidx = msg[6];
                    if (uid >= 64 || eidx >= 8) continue;  // defensive vs corrupt data
                    // field order: strike, bid, ask, spot (see gen_pcap.py)
                    const double strike = load<uint32_t>(msg + 8) * 1e-4;
                    const double bidpx = load<uint32_t>(msg + 12) * 1e-4;
                    const double askpx = load<uint32_t>(msg + 16) * 1e-4;
                    const double spot = load<uint32_t>(msg + 20) * 1e-4;
                    // Sanity filtering happens HERE, before any fast-math
                    // compiled kernel sees the numbers.
                    if (!(strike > 0.0 && spot > 0.0 && bidpx > 0.0 && askpx >= bidpx))
                        continue;
                    q.spot.push_back(spot);
                    q.strike.push_back(strike);
                    q.ttm.push_back(EXPIRY_YEARS[eidx]);
                    q.mid.push_back(0.5 * (bidpx + askpx));
                    q.is_call.push_back(msg[7] & 1);
                    q.underlying.push_back(uid);
                    q.expiry_idx.push_back(eidx);
                }
            }
        }
        off = pkt + incl;
    }
    if (q.size() == 0) throw std::runtime_error("no quotes parsed from " + path);
    return q;
}

}  // namespace qr
