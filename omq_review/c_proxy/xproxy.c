/*
 * XSUB/XPUB forwarding server on the libzmq C API.
 *
 * The same source is linked twice:
 *   xproxy_libzmq  -> system libzmq 4.3.5 (reference)
 *   xproxy_omqc    -> omq-libzmq 0.5.20 (libomq_zmq.so, omq's drop-in C ABI)
 *
 * Flags: --frontend EP --backend EP --io-threads N --hwm N --xpub-nodrop
 *        --xpub-verbose --xpub-verboser --welcome MSG
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zmq.h>

#ifndef ZMQ_XPUB_VERBOSER
#define ZMQ_XPUB_VERBOSER 78
#endif

static void check(int rc, const char *what) {
    if (rc != 0) {
        fprintf(stderr, "FATAL %s: %s\n", what, zmq_strerror(zmq_errno()));
        exit(2);
    }
}

int main(int argc, char **argv) {
    const char *fe = "tcp://127.0.0.1:5555";
    const char *be = "tcp://127.0.0.1:5556";
    const char *welcome = NULL;
    int io_threads = 1, hwm = -1, nodrop = 0, verbose = 0, verboser = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frontend") && i + 1 < argc) fe = argv[++i];
        else if (!strcmp(argv[i], "--backend") && i + 1 < argc) be = argv[++i];
        else if (!strcmp(argv[i], "--io-threads") && i + 1 < argc) io_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hwm") && i + 1 < argc) hwm = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--run-on") && i + 1 < argc) ++i;
        else if (!strcmp(argv[i], "--xpub-nodrop")) nodrop = 1;
        else if (!strcmp(argv[i], "--xpub-verbose")) verbose = 1;
        else if (!strcmp(argv[i], "--xpub-verboser")) verboser = 1;
        else if (!strcmp(argv[i], "--welcome") && i + 1 < argc) welcome = argv[++i];
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); return 2; }
    }
    void *ctx = zmq_ctx_new();
    check(zmq_ctx_set(ctx, ZMQ_IO_THREADS, io_threads), "io_threads");
    void *xsub = zmq_socket(ctx, ZMQ_XSUB);
    void *xpub = zmq_socket(ctx, ZMQ_XPUB);
    if (hwm >= 0) {
        check(zmq_setsockopt(xsub, ZMQ_SNDHWM, &hwm, sizeof hwm), "xsub sndhwm");
        check(zmq_setsockopt(xsub, ZMQ_RCVHWM, &hwm, sizeof hwm), "xsub rcvhwm");
        check(zmq_setsockopt(xpub, ZMQ_SNDHWM, &hwm, sizeof hwm), "xpub sndhwm");
        check(zmq_setsockopt(xpub, ZMQ_RCVHWM, &hwm, sizeof hwm), "xpub rcvhwm");
    }
    if (nodrop) check(zmq_setsockopt(xpub, ZMQ_XPUB_NODROP, &nodrop, sizeof nodrop), "nodrop");
    if (verbose) check(zmq_setsockopt(xpub, ZMQ_XPUB_VERBOSE, &verbose, sizeof verbose), "verbose");
    if (verboser) check(zmq_setsockopt(xpub, ZMQ_XPUB_VERBOSER, &verboser, sizeof verboser), "verboser");
    if (welcome) check(zmq_setsockopt(xpub, ZMQ_XPUB_WELCOME_MSG, welcome, strlen(welcome)), "welcome");
    check(zmq_bind(xsub, fe), "bind xsub");
    check(zmq_bind(xpub, be), "bind xpub");
    int major, minor, patch;
    zmq_version(&major, &minor, &patch);
    printf("READY c-api zmq_version=%d.%d.%d io_threads=%d hwm=%d\n", major, minor, patch,
           io_threads, hwm);
    fflush(stdout);
    int rc = zmq_proxy(xsub, xpub, NULL);
    /* zmq_proxy only returns on context termination; anything else is a failure. */
    fprintf(stderr, "PROXY EXIT: rc=%d errno=%d (%s)\n", rc, zmq_errno(),
            zmq_strerror(zmq_errno()));
    return rc == 0 ? 0 : 3;
}
