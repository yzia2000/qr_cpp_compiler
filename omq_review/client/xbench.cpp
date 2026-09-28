// xbench: independent load generator + verifier for XSUB/XPUB forwarding servers.
//
// Runs as its own process and uses only the reference libzmq (4.3.5 via the
// C API), so nothing in the measurement path is shared with the server under
// test. One PUB thread connects to the server's XSUB side and pushes
// messages; N SUB threads connect to the server's XPUB side and receive them
// back. Every received message is verified byte-for-byte:
//
//   frame   = ["BNCH"][header 60B][body]          (single-frame mode)
//           = ["BNCH"] + [header 60B][body]       (--multipart: 2 frames)
//   header  = magic, flags, seq, send_ts(CLOCK_MONOTONIC ns), size, base idx
//   body    = base_buffer[seq % 61] with `seq` stamped into the first 8 bytes
//             of every 4 KiB page and into the last 8 bytes of the frame.
//
// So a receiver can rebuild the exact expected bytes for any seq and detect
// corruption, truncation, cross-message mixing (e.g. a recycled buffer), loss
// (seq gaps), duplicates and reordering. Latency = recv_ts - send_ts, both on
// the same host's monotonic clock.
//
// Modes:
//   window  closed loop: at most W messages in flight (sent but not yet seen
//           by every fast subscriber). Lossless expected when W < HWM.
//   rate    open loop at --rate msgs/s.
//   flood   open loop, as fast as zmq_msg_send accepts.
#include <zmq.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cinttypes>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

namespace {

constexpr uint32_t kMagic = 0x584D5142;  // "BQMX"
constexpr uint32_t kFlagProbe = 1;
constexpr uint32_t kFlagEnd = 2;
constexpr size_t kTopicLen = 4;
char kTopic[kTopicLen + 1] = "BNCH";  // overridable with --topic (exactly 4 bytes)
constexpr size_t kHdrLen = 60;
constexpr size_t kPage = 4096;
constexpr uint32_t kNBase = 61;  // prime, so buffer reuse periods don't alias

#pragma pack(push, 1)
struct Header {
    uint32_t magic;
    uint32_t flags;
    uint64_t seq;
    uint64_t send_ns;
    uint64_t size;  // total bytes of the payload region (header + body)
    uint32_t base;
    uint8_t reserved[kHdrLen - 36];
};
#pragma pack(pop)
static_assert(sizeof(Header) == kHdrLen, "header size");

uint64_t now_ns() {
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return uint64_t(ts.tv_sec) * 1000000000ull + uint64_t(ts.tv_nsec);
}

struct Config {
    std::string pub_ep = "tcp://127.0.0.1:5555";
    std::string sub_ep = "tcp://127.0.0.1:5556";
    size_t size = 1 << 20;  // payload bytes (header + body), excludes topic
    int subs = 1;
    int slow_subs = 0;
    int slow_delay_us = 2000;
    std::string mode = "window";
    int window = 16;
    double rate = 100;
    double duration = 10;
    double warmup = 1;
    uint64_t count = 0;  // 0 = duration-bound
    bool multipart = false;
    bool full_verify = false;
    int hwm = -1;  // -1: libzmq default (1000)
    bool pub_nodrop = false;
    std::string json;
    std::string label;
    double stall_timeout = 3.0;
    double probe_timeout = 15.0;
};

// Deterministic base buffers shared by sender and receivers.
std::vector<std::vector<uint8_t>> make_bases(size_t size) {
    std::vector<std::vector<uint8_t>> bases(kNBase);
    uint64_t x = 0x9E3779B97F4A7C15ull;
    for (uint32_t b = 0; b < kNBase; b++) {
        bases[b].resize(size);
        for (size_t i = 0; i < size; i += 8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            size_t n = std::min<size_t>(8, size - i);
            memcpy(&bases[b][i], &x, n);
        }
    }
    return bases;
}

// Offsets (relative to the start of the payload region) that carry the seq.
inline bool is_stamp_offset(size_t off, size_t size) {
    return (off >= kPage && off % kPage == 0 && off + 8 <= size) || off == size - 8;
}

void fill_payload(uint8_t *p, size_t size, uint64_t seq, uint32_t flags,
                  const std::vector<std::vector<uint8_t>> &bases) {
    uint32_t b = uint32_t(seq % kNBase);
    if (size > kHdrLen) memcpy(p + kHdrLen, bases[b].data() + kHdrLen, size - kHdrLen);
    for (size_t off = kPage; off + 8 <= size; off += kPage) memcpy(p + off, &seq, 8);
    if (size >= kHdrLen + 8) memcpy(p + size - 8, &seq, 8);
    Header h{};
    h.magic = kMagic;
    h.flags = flags;
    h.seq = seq;
    h.size = size;
    h.base = b;
    h.send_ns = now_ns();  // stamped last, right before handing to libzmq
    memcpy(p, &h, sizeof h);
}

enum class Verdict { Ok, BadFrames, BadTopic, BadMagic, BadSize, BadStamp, BadBody };

const char *verdict_name(Verdict v) {
    switch (v) {
        case Verdict::Ok: return "ok";
        case Verdict::BadFrames: return "bad_frame_count";
        case Verdict::BadTopic: return "bad_topic";
        case Verdict::BadMagic: return "bad_magic";
        case Verdict::BadSize: return "bad_size";
        case Verdict::BadStamp: return "bad_seq_stamp";
        case Verdict::BadBody: return "bad_body_bytes";
    }
    return "?";
}

Verdict verify_payload(const uint8_t *p, size_t len, size_t expect_size, Header &h,
                       bool full, const std::vector<std::vector<uint8_t>> &bases,
                       uint64_t rnd) {
    if (len < kHdrLen) return Verdict::BadSize;
    memcpy(&h, p, sizeof h);
    if (h.magic != kMagic) return Verdict::BadMagic;
    if (h.flags & (kFlagProbe | kFlagEnd)) return Verdict::Ok;  // control messages
    if (h.size != len || len != expect_size) return Verdict::BadSize;
    uint64_t seq = h.seq;
    if (h.base != seq % kNBase) return Verdict::BadStamp;
    for (size_t off = kPage; off + 8 <= len; off += kPage)
        if (memcmp(p + off, &seq, 8) != 0) return Verdict::BadStamp;
    if (len >= kHdrLen + 8 && memcmp(p + len - 8, &seq, 8) != 0) return Verdict::BadStamp;
    const uint8_t *base = bases[h.base].data();
    auto check_range = [&](size_t a, size_t b) -> bool {  // [a,b) excluding stamps
        size_t off = a;
        while (off < b) {
            size_t next = std::min(b, (off / kPage + 1) * kPage);
            size_t lo = off, hi = next;
            if (lo % kPage == 0 && lo >= kPage) lo += 8;  // skip page stamp
            if (hi > len - 8) hi = len - 8;                  // skip tail stamp
            if (lo < hi && memcmp(p + lo, base + lo, hi - lo) != 0) return false;
            off = next;
        }
        return true;
    };
    if (full) {
        if (!check_range(kHdrLen, len)) return Verdict::BadBody;
    } else {
        // light: first page, last page, one random page
        size_t pages = (len + kPage - 1) / kPage;
        size_t rp = pages > 2 ? 1 + rnd % (pages - 2) : 0;
        if (!check_range(kHdrLen, std::min(len, kPage))) return Verdict::BadBody;
        if (len > kPage && !check_range((pages - 1) * kPage, len)) return Verdict::BadBody;
        if (rp && !check_range(rp * kPage, std::min(len, (rp + 1) * kPage)))
            return Verdict::BadBody;
    }
    return Verdict::Ok;
}

struct SubStats {
    // shared with publisher
    std::atomic<uint64_t> highest{0};
    std::atomic<uint64_t> received{0};
    std::atomic<bool> got_probe{false};
    std::atomic<bool> got_end{false};
    // private until join
    std::vector<uint8_t> seen;  // seen[seq] = count (saturating)
    std::vector<uint32_t> lat_us;
    uint64_t dup = 0, reorder = 0, corrupt = 0, bytes = 0;
    uint64_t first_recv_ns = 0, last_recv_ns = 0;
    uint64_t measured_msgs = 0, measured_bytes = 0;
    uint64_t measure_first_ns = 0, measure_last_ns = 0;
    std::string first_error;
    bool slow = false;
    int id = 0;
};

struct Shared {
    std::atomic<uint64_t> measure_first_seq{UINT64_MAX};
    std::atomic<bool> stop_subs{false};
};

void set_int(void *s, int opt, int v) {
    if (zmq_setsockopt(s, opt, &v, sizeof v) != 0) {
        fprintf(stderr, "setsockopt %d: %s\n", opt, zmq_strerror(zmq_errno()));
        exit(2);
    }
}

void subscriber(const Config &cfg, SubStats &st, Shared &sh,
                const std::vector<std::vector<uint8_t>> &bases, std::atomic<int> &ready) {
    void *ctx = zmq_ctx_new();
    void *s = zmq_socket(ctx, ZMQ_SUB);
    if (cfg.hwm >= 0) set_int(s, ZMQ_RCVHWM, cfg.hwm);
    set_int(s, ZMQ_RCVTIMEO, 100);
    set_int(s, ZMQ_LINGER, 0);
    zmq_setsockopt(s, ZMQ_SUBSCRIBE, kTopic, kTopicLen);
    if (zmq_connect(s, cfg.sub_ep.c_str()) != 0) {
        fprintf(stderr, "sub connect: %s\n", zmq_strerror(zmq_errno()));
        exit(2);
    }
    ready.fetch_add(1);
    st.seen.reserve(1 << 20);
    uint64_t rnd = 0x1234567ull + st.id;
    zmq_msg_t m1, m2;
    zmq_msg_init(&m1);
    zmq_msg_init(&m2);
    while (!sh.stop_subs.load(std::memory_order_relaxed)) {
        int rc = zmq_msg_recv(&m1, s, 0);
        if (rc < 0) {
            if (zmq_errno() == EAGAIN) continue;
            break;
        }
        uint64_t t = now_ns();
        bool more = zmq_msg_more(&m1);
        const uint8_t *payload;
        size_t plen;
        Verdict v = Verdict::Ok;
        size_t total = zmq_msg_size(&m1);
        if (cfg.multipart) {
            if (!more) { v = Verdict::BadFrames; payload = nullptr; plen = 0; }
            else {
                if (zmq_msg_size(&m1) != kTopicLen || memcmp(zmq_msg_data(&m1), kTopic, kTopicLen))
                    v = Verdict::BadTopic;
                zmq_msg_recv(&m2, s, 0);
                total += zmq_msg_size(&m2);
                if (zmq_msg_more(&m2)) {
                    v = Verdict::BadFrames;
                    while (zmq_msg_more(&m2)) zmq_msg_recv(&m2, s, 0);  // drain
                }
                payload = static_cast<const uint8_t *>(zmq_msg_data(&m2));
                plen = zmq_msg_size(&m2);
            }
        } else {
            if (more) {
                v = Verdict::BadFrames;
                while (zmq_msg_more(&m1)) zmq_msg_recv(&m1, s, 0);
            }
            const uint8_t *d = static_cast<const uint8_t *>(zmq_msg_data(&m1));
            if (zmq_msg_size(&m1) < kTopicLen || memcmp(d, kTopic, kTopicLen)) v = Verdict::BadTopic;
            payload = d + kTopicLen;
            plen = zmq_msg_size(&m1) >= kTopicLen ? zmq_msg_size(&m1) - kTopicLen : 0;
        }
        Header h{};
        if (v == Verdict::Ok) {
            rnd = rnd * 6364136223846793005ull + 1442695040888963407ull;
            v = verify_payload(payload, plen, cfg.size, h, cfg.full_verify, bases, rnd >> 33);
        }
        if (v != Verdict::Ok) {
            st.corrupt++;
            if (st.first_error.empty()) {
                char buf[160];
                snprintf(buf, sizeof buf, "%s (len=%zu seq_field=%" PRIu64 ")", verdict_name(v),
                         plen, h.seq);
                st.first_error = buf;
            }
            continue;
        }
        if (h.flags & kFlagProbe) { st.got_probe.store(true); continue; }
        if (h.flags & kFlagEnd) { st.got_end.store(true); break; }
        uint64_t seq = h.seq;
        if (seq >= st.seen.size()) st.seen.resize(std::max<size_t>(seq + 1, st.seen.size() * 2), 0);
        if (st.seen[seq]) { st.dup++; continue; }
        st.seen[seq] = 1;
        uint64_t hi = st.highest.load(std::memory_order_relaxed);  // max seq seen so far
        if (seq < hi) st.reorder++;  // arrived after a later seq
        if (seq > hi) st.highest.store(seq, std::memory_order_release);
        st.received.fetch_add(1, std::memory_order_release);
        st.bytes += total;
        if (!st.first_recv_ns) st.first_recv_ns = t;
        st.last_recv_ns = t;
        if (seq >= sh.measure_first_seq.load(std::memory_order_acquire)) {
            if (!st.measure_first_ns) st.measure_first_ns = t;
            st.measure_last_ns = t;
            st.measured_msgs++;
            st.measured_bytes += total;
            uint64_t lat = t > h.send_ns ? (t - h.send_ns) / 1000 : 0;
            st.lat_us.push_back(uint32_t(std::min<uint64_t>(lat, UINT32_MAX)));
        }
        if (st.slow && cfg.slow_delay_us > 0) std::this_thread::sleep_for(std::chrono::microseconds(cfg.slow_delay_us));
    }
    zmq_msg_close(&m1);
    zmq_msg_close(&m2);
    zmq_close(s);
    zmq_ctx_term(ctx);
}

double pct(std::vector<uint32_t> &v, double p) {
    if (v.empty()) return -1;
    size_t i = size_t(std::min<double>(v.size() - 1, std::floor(p / 100.0 * (v.size() - 1) + 0.5)));
    return v[i];
}

}  // namespace

int main(int argc, char **argv) {
    Config cfg;
    for (int i = 1; i < argc; i++) {
        std::string k = argv[i];
        auto val = [&]() -> std::string {
            if (i + 1 >= argc) { fprintf(stderr, "missing value for %s\n", k.c_str()); exit(2); }
            return argv[++i];
        };
        if (k == "--pub") cfg.pub_ep = val();
        else if (k == "--sub") cfg.sub_ep = val();
        else if (k == "--size") cfg.size = std::stoull(val());
        else if (k == "--subs") cfg.subs = std::stoi(val());
        else if (k == "--slow-subs") cfg.slow_subs = std::stoi(val());
        else if (k == "--slow-delay-us") cfg.slow_delay_us = std::stoi(val());
        else if (k == "--mode") cfg.mode = val();
        else if (k == "--window") cfg.window = std::stoi(val());
        else if (k == "--rate") cfg.rate = std::stod(val());
        else if (k == "--duration") cfg.duration = std::stod(val());
        else if (k == "--warmup") cfg.warmup = std::stod(val());
        else if (k == "--count") cfg.count = std::stoull(val());
        else if (k == "--multipart") cfg.multipart = true;
        else if (k == "--full-verify") cfg.full_verify = true;
        else if (k == "--hwm") cfg.hwm = std::stoi(val());
        else if (k == "--pub-nodrop") cfg.pub_nodrop = true;
        else if (k == "--json") cfg.json = val();
        else if (k == "--label") cfg.label = val();
        else if (k == "--stall-timeout") cfg.stall_timeout = std::stod(val());
        else if (k == "--topic") {
            std::string t = val();
            if (t.size() != kTopicLen) { fprintf(stderr, "--topic must be 4 bytes\n"); return 2; }
            memcpy(kTopic, t.data(), kTopicLen);
        }
        else { fprintf(stderr, "unknown arg %s\n", k.c_str()); return 2; }
    }
    if (cfg.size < kHdrLen + 8) cfg.size = kHdrLen + 8;
    const int nsub = cfg.subs + cfg.slow_subs;
    auto bases = make_bases(cfg.size);

    Shared sh;
    std::vector<SubStats> stats(nsub);
    std::vector<std::thread> threads;
    std::atomic<int> ready{0};
    for (int i = 0; i < nsub; i++) {
        stats[i].id = i;
        stats[i].slow = i >= cfg.subs;
        threads.emplace_back(subscriber, std::cref(cfg), std::ref(stats[i]), std::ref(sh),
                             std::cref(bases), std::ref(ready));
    }
    while (ready.load() < nsub) std::this_thread::sleep_for(std::chrono::milliseconds(1));

    void *pctx = zmq_ctx_new();
    void *pub = zmq_socket(pctx, ZMQ_PUB);
    if (cfg.hwm >= 0) set_int(pub, ZMQ_SNDHWM, cfg.hwm);
    if (cfg.pub_nodrop) set_int(pub, ZMQ_XPUB_NODROP, 1);
    set_int(pub, ZMQ_LINGER, 0);
    if (zmq_connect(pub, cfg.pub_ep.c_str()) != 0) {
        fprintf(stderr, "pub connect: %s\n", zmq_strerror(zmq_errno()));
        return 2;
    }

    const size_t frame_len = cfg.multipart ? cfg.size : cfg.size + kTopicLen;
    auto send_one = [&](uint64_t seq, uint32_t flags, size_t size) -> bool {
        zmq_msg_t m;
        if (cfg.multipart) {
            zmq_msg_t t;
            zmq_msg_init_size(&t, kTopicLen);
            memcpy(zmq_msg_data(&t), kTopic, kTopicLen);
            if (zmq_msg_send(&t, pub, ZMQ_SNDMORE) < 0) { zmq_msg_close(&t); return false; }
            zmq_msg_init_size(&m, size);
            fill_payload(static_cast<uint8_t *>(zmq_msg_data(&m)), size, seq, flags, bases);
        } else {
            zmq_msg_init_size(&m, size + kTopicLen);
            uint8_t *d = static_cast<uint8_t *>(zmq_msg_data(&m));
            memcpy(d, kTopic, kTopicLen);
            fill_payload(d + kTopicLen, size, seq, flags, bases);
        }
        if (zmq_msg_send(&m, pub, 0) < 0) { zmq_msg_close(&m); return false; }
        return true;
    };
    (void)frame_len;

    // Phase 1: wait for subscriptions to propagate SUB -> XPUB -> proxy -> XSUB -> PUB.
    uint64_t t_probe0 = now_ns();
    bool all_probed = false;
    while (!all_probed) {
        send_one(0, kFlagProbe, kHdrLen + 8);
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
        all_probed = true;
        for (auto &s : stats) all_probed &= s.got_probe.load();
        if ((now_ns() - t_probe0) / 1e9 > cfg.probe_timeout) break;
    }
    double probe_s = (now_ns() - t_probe0) / 1e9;
    if (!all_probed) {
        fprintf(stderr, "FATAL: subscriptions did not propagate within %.1fs\n", cfg.probe_timeout);
    }

    // Phase 2: warmup + measurement.
    uint64_t seq = 1, stalls = 0;
    uint64_t t0 = now_ns();
    uint64_t measure_t0 = 0, measure_first = 0;
    uint64_t last_progress_ns = t0, last_done = 0;
    double interval_ns = cfg.mode == "rate" ? 1e9 / cfg.rate : 0;
    uint64_t next_send = t0;
    std::vector<uint64_t> done_floor(nsub, 0);
    rusage ru0;
    getrusage(RUSAGE_SELF, &ru0);
    while (all_probed) {
        uint64_t t = now_ns();
        double el = (t - t0) / 1e9;
        if (!measure_t0 && el >= cfg.warmup) {
            measure_t0 = t;
            measure_first = seq;
            sh.measure_first_seq.store(seq, std::memory_order_release);
        }
        if (measure_t0 && ((cfg.count && seq - measure_first >= cfg.count) ||
                           (!cfg.count && (t - measure_t0) / 1e9 >= cfg.duration)))
            break;
        if (cfg.mode == "window") {
            // min over fast subscribers of highest seq seen (+ floor for declared-lost)
            uint64_t done = UINT64_MAX;
            for (int i = 0; i < cfg.subs; i++)
                done = std::min(done, std::max(stats[i].highest.load(std::memory_order_acquire),
                                               done_floor[i]));
            if (cfg.subs == 0) done = seq;
            if (done != last_done) { last_done = done; last_progress_ns = t; }
            // in flight = (seq-1) - done; send only while in flight < window
            if (seq > done + uint64_t(cfg.window)) {
                if ((t - last_progress_ns) / 1e9 > cfg.stall_timeout) {
                    // Nothing came back for stall_timeout: treat in-flight as lost and move on.
                    stalls++;
                    for (int i = 0; i < cfg.subs; i++) done_floor[i] = seq - 1;
                    last_progress_ns = t;
                } else {
                    std::this_thread::yield();
                    continue;
                }
            }
        } else if (cfg.mode == "rate") {
            while (now_ns() < next_send) {}
            next_send += uint64_t(interval_ns);
        }
        if (!send_one(seq, 0, cfg.size)) {
            fprintf(stderr, "send failed: %s\n", zmq_strerror(zmq_errno()));
            break;
        }
        seq++;
    }
    uint64_t measure_t1 = now_ns();
    uint64_t measure_last = seq - 1;  // inclusive
    rusage ru1;
    getrusage(RUSAGE_SELF, &ru1);

    // Phase 3: drain. Wait until fast subs caught up (or 5s), then END markers.
    uint64_t td = now_ns();
    while ((now_ns() - td) / 1e9 < 5.0) {
        bool caught = true;
        for (int i = 0; i < cfg.subs; i++) caught &= stats[i].highest.load() >= measure_last;
        if (caught) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    uint64_t te = now_ns();
    while ((now_ns() - te) / 1e9 < 3.0) {
        send_one(seq, kFlagEnd, kHdrLen + 8);
        bool all_end = true;
        for (auto &s : stats) all_end &= s.got_end.load();
        if (all_end) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    sh.stop_subs.store(true);
    for (auto &th : threads) th.join();
    zmq_close(pub);
    zmq_ctx_term(pctx);

    // Report.
    double mdur = (measure_t1 - measure_t0) / 1e9;
    uint64_t sent_measured = measure_t0 ? measure_last - measure_first + 1 : 0;
    double client_cpu = (ru1.ru_utime.tv_sec - ru0.ru_utime.tv_sec) + (ru1.ru_stime.tv_sec - ru0.ru_stime.tv_sec) +
                        ((ru1.ru_utime.tv_usec - ru0.ru_utime.tv_usec) + (ru1.ru_stime.tv_usec - ru0.ru_stime.tv_usec)) / 1e6;
    std::string js = "{";
    char b[512];
    snprintf(b, sizeof b,
             "\"label\":\"%s\",\"mode\":\"%s\",\"size\":%zu,\"multipart\":%s,\"window\":%d,\"rate\":%.1f,"
             "\"subs\":%d,\"slow_subs\":%d,\"probe_s\":%.3f,\"subscribed\":%s,\"measure_s\":%.3f,"
             "\"sent\":%" PRIu64 ",\"stalls\":%" PRIu64 ",\"send_rate\":%.1f,\"client_cpu_s\":%.2f,\"subs_detail\":[",
             cfg.label.c_str(), cfg.mode.c_str(), cfg.size, cfg.multipart ? "true" : "false", cfg.window,
             cfg.rate, cfg.subs, cfg.slow_subs, probe_s, all_probed ? "true" : "false", mdur, sent_measured,
             stalls, mdur > 0 ? sent_measured / mdur : 0.0, client_cpu);
    js += b;
    fprintf(stderr, "[%s] mode=%s size=%zu sent=%" PRIu64 " in %.2fs (%.1f msg/s) stalls=%" PRIu64 " probe=%.2fs\n",
            cfg.label.c_str(), cfg.mode.c_str(), cfg.size, sent_measured, mdur,
            mdur > 0 ? sent_measured / mdur : 0.0, stalls, probe_s);
    for (int i = 0; i < nsub; i++) {
        auto &s = stats[i];
        uint64_t got = 0;
        for (uint64_t q = measure_first; q <= measure_last && q < s.seen.size(); q++) got += s.seen[q];
        uint64_t lost = sent_measured > got ? sent_measured - got : 0;
        std::sort(s.lat_us.begin(), s.lat_us.end());
        double rdur = s.measure_last_ns > s.measure_first_ns ? (s.measure_last_ns - s.measure_first_ns) / 1e9 : 0;
        double msgs_s = rdur > 0 ? (s.measured_msgs - 1) / rdur : 0;
        double mb_s = rdur > 0 ? (s.measured_bytes) / rdur / 1e6 : 0;
        snprintf(b, sizeof b,
                 "%s{\"id\":%d,\"slow\":%s,\"received\":%" PRIu64 ",\"lost\":%" PRIu64 ",\"loss_pct\":%.4f,"
                 "\"dup\":%" PRIu64 ",\"reorder\":%" PRIu64 ",\"corrupt\":%" PRIu64 ",\"msgs_s\":%.1f,\"MB_s\":%.1f,"
                 "\"lat_us\":{\"p50\":%.0f,\"p90\":%.0f,\"p99\":%.0f,\"p999\":%.0f,\"max\":%.0f},\"first_error\":\"%s\"}",
                 i ? "," : "", i, s.slow ? "true" : "false", got, lost,
                 sent_measured ? 100.0 * lost / sent_measured : 0.0, s.dup, s.reorder, s.corrupt, msgs_s, mb_s,
                 pct(s.lat_us, 50), pct(s.lat_us, 90), pct(s.lat_us, 99), pct(s.lat_us, 99.9),
                 s.lat_us.empty() ? -1.0 : double(s.lat_us.back()), s.first_error.c_str());
        js += b;
        fprintf(stderr,
                "  sub%d%s recv=%" PRIu64 " lost=%" PRIu64 " (%.2f%%) dup=%" PRIu64 " reorder=%" PRIu64
                " corrupt=%" PRIu64 " %.1f msg/s %.1f MB/s lat p50=%.0fus p99=%.0fus p99.9=%.0fus max=%.0fus %s\n",
                i, s.slow ? "(slow)" : "", got, lost, sent_measured ? 100.0 * lost / sent_measured : 0.0, s.dup,
                s.reorder, s.corrupt, msgs_s, mb_s, pct(s.lat_us, 50), pct(s.lat_us, 99), pct(s.lat_us, 99.9),
                s.lat_us.empty() ? -1.0 : double(s.lat_us.back()), s.first_error.c_str());
    }
    js += "]}";
    if (!cfg.json.empty()) {
        FILE *f = fopen(cfg.json.c_str(), "w");
        if (f) { fputs(js.c_str(), f); fputc('\n', f); fclose(f); }
    } else {
        puts(js.c_str());
    }
    return all_probed ? 0 : 4;
}
