/*
 * NNG pub/sub forwarding server (libnng, SP protocol) — NNG's counterpart of
 * zmq_proxy(XSUB, XPUB). Publishers dial --frontend, subscribers dial --backend.
 *
 * SP pub/sub has no subscription messages on the wire: subscribers filter locally, so
 * nothing can propagate upstream and the broker sends every message to every
 * subscriber, whatever it subscribed to.
 *
 *   --mode device (default)  nng_device() between a raw SUB and a raw PUB — the
 *                            documented way to build an NNG pub/sub forwarder.
 *   --mode loop              cooked SUB subscribed to everything + cooked PUB, one
 *                            thread: nng_recvmsg -> nng_sendmsg (like rzmq_proxy.rs).
 *
 * Tuning (all default to NNG's own defaults):
 *   --recvbuf N    NNG_OPT_RECVBUF on the ingress SUB, in messages (raw SUB: the socket
 *                  receive queue, default 1; cooked SUB: default 128)
 *   --sendbuf N    NNG_OPT_SENDBUF on the PUB: per-subscriber queue, default 16
 *   --recvmaxsz N  NNG_OPT_RECVMAXSZ on the ingress SUB, bytes (default 0 = unlimited)
 *   --task-threads N / --poller-threads N   nng_init_set_parameter() before first use
 * --io-threads/--hwm/--run-on/--xpub-nodrop are accepted and ignored (CLI parity).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nng/nng.h>
#include <nng/protocol/pubsub0/pub.h>
#include <nng/protocol/pubsub0/sub.h>

static void check(int rv, const char *what) {
    if (rv != 0) {
        fprintf(stderr, "FATAL %s: %s\n", what, nng_strerror(rv));
        exit(2);
    }
}

int main(int argc, char **argv) {
    const char *fe = "tcp://127.0.0.1:5555";
    const char *be = "tcp://127.0.0.1:5556";
    const char *mode = "device";
    long recvbuf = -1, sendbuf = -1, task_threads = -1, poller_threads = -1;
    long long recvmaxsz = -1;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frontend") && i + 1 < argc) fe = argv[++i];
        else if (!strcmp(argv[i], "--backend") && i + 1 < argc) be = argv[++i];
        else if (!strcmp(argv[i], "--mode") && i + 1 < argc) mode = argv[++i];
        else if (!strcmp(argv[i], "--recvbuf") && i + 1 < argc) recvbuf = atol(argv[++i]);
        else if (!strcmp(argv[i], "--sendbuf") && i + 1 < argc) sendbuf = atol(argv[++i]);
        else if (!strcmp(argv[i], "--recvmaxsz") && i + 1 < argc) recvmaxsz = atoll(argv[++i]);
        else if (!strcmp(argv[i], "--task-threads") && i + 1 < argc) task_threads = atol(argv[++i]);
        else if (!strcmp(argv[i], "--poller-threads") && i + 1 < argc) poller_threads = atol(argv[++i]);
        else if ((!strcmp(argv[i], "--io-threads") || !strcmp(argv[i], "--hwm") ||
                  !strcmp(argv[i], "--run-on")) && i + 1 < argc) ++i;
        else if (!strcmp(argv[i], "--xpub-nodrop")) {}
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); return 2; }
    }
    int loop = !strcmp(mode, "loop");
    if (!loop && strcmp(mode, "device")) { fprintf(stderr, "unknown mode %s\n", mode); return 2; }
    if (task_threads > 0) nng_init_set_parameter(NNG_INIT_NUM_TASK_THREADS, (uint64_t) task_threads);
    if (poller_threads > 0) nng_init_set_parameter(NNG_INIT_NUM_POLLER_THREADS, (uint64_t) poller_threads);

    nng_socket sub, pub;
    if (loop) {
        check(nng_sub0_open(&sub), "sub0 open");
        check(nng_sub0_socket_subscribe(sub, "", 0), "subscribe all");
        check(nng_pub0_open(&pub), "pub0 open");
    } else {
        check(nng_sub0_open_raw(&sub), "sub0 open raw");
        check(nng_pub0_open_raw(&pub), "pub0 open raw");
    }
    if (recvbuf >= 0) check(nng_socket_set_int(sub, NNG_OPT_RECVBUF, (int) recvbuf), "recvbuf");
    if (sendbuf >= 0) check(nng_socket_set_int(pub, NNG_OPT_SENDBUF, (int) sendbuf), "sendbuf");
    if (recvmaxsz >= 0) check(nng_socket_set_size(sub, NNG_OPT_RECVMAXSZ, (size_t) recvmaxsz), "recvmaxsz");
    check(nng_listen(sub, fe, NULL, 0), "listen frontend");
    check(nng_listen(pub, be, NULL, 0), "listen backend");
    printf("READY nng %s mode=%s recvbuf=%ld sendbuf=%ld recvmaxsz=%lld\n", nng_version(), mode, recvbuf,
           sendbuf, recvmaxsz);
    fflush(stdout);

    if (!loop) {
        int rv = nng_device(sub, pub);
        fprintf(stderr, "PROXY EXIT: nng_device: %s\n", nng_strerror(rv));
        return 3;
    }
    for (;;) {
        nng_msg *m;
        int rv = nng_recvmsg(sub, &m, 0);
        if (rv != 0) {
            fprintf(stderr, "PROXY EXIT: recv: %s\n", nng_strerror(rv));
            return 3;
        }
        if ((rv = nng_sendmsg(pub, m, 0)) != 0) {
            nng_msg_free(m);
            fprintf(stderr, "PROXY EXIT: send: %s\n", nng_strerror(rv));
            return 3;
        }
    }
}
