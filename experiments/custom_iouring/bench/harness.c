// Benchmark harness: ONE process, so both ends share CLOCK_MONOTONIC.
//
// A sender thread publishes 2-frame messages ["bench"][payload] to the app's
// XSUB endpoint (libzmq PUB, ZMQ_XPUB_NODROP so it blocks instead of
// dropping); a receiver thread subscribes to "bench" on the app's XPUB
// endpoint (libzmq SUB). The payload's first 16 bytes carry a sequence
// number and the send timestamp (CLOCK_MONOTONIC, ns, taken just before the
// first frame is handed to libzmq); latency is receive time minus that.
//
// Modes:
//   saturate  send as fast as the path accepts (end-to-end throughput, and
//             latency under full load)
//   pingpong  one message in flight: the next is sent only after the
//             previous one was received (latency with no queueing)
//
// usage: harness XSUB_ENDPOINT XPUB_ENDPOINT SIZE MODE WARM_S MEAS_S
//        (XSUB_ENDPOINT may be bind:ENDPOINT for the direct baseline)
// prints one JSON line.
#define _GNU_SOURCE
#include <zmq.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sched.h>
#include <unistd.h>

#define NBUF 256   // payload buffers; must exceed messages libzmq can hold
#define HWM 64     // harness PUB/SUB high-water marks (messages)

static inline uint64_t now_ns(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static const char *xsub_ep, *xpub_ep;
static size_t size;
static int pingpong;
static double warm_s, meas_s;
static void *ctx;
static atomic_int stop;
static atomic_uint_fast64_t received_seq;   // last seq received + 1
static atomic_int in_use[NBUF];
static char *bufs[NBUF];

static void release(void *data, void *hint) { (void)data; atomic_store(&in_use[(intptr_t)hint], 0); }

static void *sender(void *arg) {
    (void)arg;
    void *pub = zmq_socket(ctx, ZMQ_PUB);
    int one = 1, hwm = HWM;
    zmq_setsockopt(pub, ZMQ_XPUB_NODROP, &one, sizeof one);
    zmq_setsockopt(pub, ZMQ_SNDHWM, &hwm, sizeof hwm);
    // "bind:ENDPOINT" makes the harness PUB bind, for the direct baseline
    // (harness PUB -> harness SUB, no application in between).
    if (!strncmp(xsub_ep, "bind:", 5)) zmq_bind(pub, xsub_ep + 5); else zmq_connect(pub, xsub_ep);
    uint64_t seq = 0;
    while (!atomic_load(&stop)) {
        if (pingpong && seq > 0) {
            // Wait for the previous message; resend if it was lost before the
            // subscription reached the publisher.
            uint64_t t0 = now_ns();
            while (atomic_load(&received_seq) < seq && !atomic_load(&stop) && now_ns() - t0 < 100000000ull) {}
        }
        int slot = (int)(seq % NBUF);
        while (atomic_load(&in_use[slot]) && !atomic_load(&stop)) sched_yield();
        if (atomic_load(&stop)) break;
        atomic_store(&in_use[slot], 1);
        uint64_t ts = now_ns();
        memcpy(bufs[slot], &seq, 8);
        memcpy(bufs[slot] + 8, &ts, 8);
        zmq_send(pub, "bench", 5, ZMQ_SNDMORE);
        zmq_msg_t m;
        zmq_msg_init_data(&m, bufs[slot], size, release, (void *)(intptr_t)slot);
        if (zmq_msg_send(&m, pub, 0) < 0) { zmq_msg_close(&m); break; }
        seq++;
    }
    int linger = 0;
    zmq_setsockopt(pub, ZMQ_LINGER, &linger, sizeof linger);
    zmq_close(pub);
    return NULL;
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    if (argc < 7) { fprintf(stderr, "usage: %s XSUB_EP XPUB_EP SIZE saturate|pingpong WARM_S MEAS_S\n", argv[0]); return 1; }
    xsub_ep = argv[1]; xpub_ep = argv[2]; size = strtoull(argv[3], 0, 10);
    pingpong = !strcmp(argv[4], "pingpong"); warm_s = atof(argv[5]); meas_s = atof(argv[6]);
    if (size < 16) size = 16;
    for (int i = 0; i < NBUF; i++) { bufs[i] = malloc(size); memset(bufs[i], 'x', size); }
    ctx = zmq_ctx_new();
    zmq_ctx_set(ctx, ZMQ_IO_THREADS, 2);

    void *sub = zmq_socket(ctx, ZMQ_SUB);
    int hwm = HWM, to = 200;
    zmq_setsockopt(sub, ZMQ_RCVHWM, &hwm, sizeof hwm);
    zmq_setsockopt(sub, ZMQ_RCVTIMEO, &to, sizeof to);
    zmq_setsockopt(sub, ZMQ_SUBSCRIBE, "bench", 5);
    zmq_connect(sub, xpub_ep);

    pthread_t th;
    pthread_create(&th, NULL, sender, NULL);

    size_t cap = 1 << 20, n = 0;
    uint64_t *lat = malloc(cap * sizeof *lat);
    uint64_t bytes = 0, lost = 0, last = UINT64_MAX, win_start = 0, win_end = 0, first_t = 0, give_up = now_ns() + 10000000000ull;
    zmq_msg_t topic, payload;
    zmq_msg_init(&topic); zmq_msg_init(&payload);
    for (;;) {
        if (zmq_msg_recv(&topic, sub, 0) < 0) {
            if (!first_t && now_ns() > give_up) { printf("{\"error\": \"no messages\"}\n"); return 2; }
            if (win_end && now_ns() >= win_end) break;
            continue;
        }
        if (!zmq_msg_more(&topic) || zmq_msg_recv(&payload, sub, 0) < 0) { printf("{\"error\": \"bad message\"}\n"); return 2; }
        uint64_t t = now_ns(), seq, ts;
        if (zmq_msg_size(&payload) != size) { printf("{\"error\": \"bad size %zu\"}\n", zmq_msg_size(&payload)); return 2; }
        memcpy(&seq, zmq_msg_data(&payload), 8);
        memcpy(&ts, (char *)zmq_msg_data(&payload) + 8, 8);
        atomic_store(&received_seq, seq + 1);
        if (!first_t) { first_t = t; win_start = t + (uint64_t)(warm_s * 1e9); win_end = win_start + (uint64_t)(meas_s * 1e9); }
        if (t >= win_end) break;
        if (t >= win_start) {
            if (n == cap) { cap *= 2; lat = realloc(lat, cap * sizeof *lat); }
            lat[n++] = t - ts;
            bytes += size + 5;  // payload + topic frame
            if (last != UINT64_MAX && seq > last + 1) lost += seq - last - 1;
        }
        last = seq;
    }
    atomic_store(&stop, 1);
    pthread_join(th, NULL);
    double dt = meas_s;
    qsort(lat, n, sizeof *lat, cmp);
    #define PCT(p) (n ? lat[(size_t)((p) * (n - 1))] / 1000.0 : 0)
    printf("{\"size\": %zu, \"mode\": \"%s\", \"msgs\": %zu, \"msgs_per_s\": %.0f, \"MB_per_s\": %.1f, "
           "\"p50_us\": %.1f, \"p99_us\": %.1f, \"p999_us\": %.1f, \"max_us\": %.1f, \"lost\": %llu}\n",
           size, pingpong ? "pingpong" : "saturate", n, n / dt, bytes / dt / 1e6,
           PCT(0.50), PCT(0.99), PCT(0.999), n ? lat[n - 1] / 1000.0 : 0, (unsigned long long)lost);
    int linger = 0;
    zmq_setsockopt(sub, ZMQ_LINGER, &linger, sizeof linger);
    zmq_close(sub);
    fflush(stdout);
    _exit(0);  // do not wait on libzmq teardown
}
