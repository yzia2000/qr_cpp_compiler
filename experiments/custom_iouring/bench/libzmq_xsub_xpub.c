// libzmq: the same application as custom iouring, on libzmq sockets.
// An XSUB socket receives messages; each one is published as-is (the same
// zmq_msg_t frames, moved not copied) on the XPUB socket. Subscriptions
// arriving on the XPUB socket are passed to the XSUB socket.
//
// ZMQ_XPUB_NODROP makes the XPUB block at its HWM instead of dropping, so
// both applications are lossless and push back on the XSUB side.
//
// usage: libzmq_xsub_xpub XSUB_ENDPOINT XPUB_ENDPOINT
#include <zmq.h>
#include <stdio.h>

static int forward(void *from, void *to) {
    // One whole message, frame by frame, as-is.
    int more;
    do {
        zmq_msg_t m;
        zmq_msg_init(&m);
        if (zmq_msg_recv(&m, from, ZMQ_DONTWAIT) < 0) { zmq_msg_close(&m); return -1; }
        more = zmq_msg_more(&m);
        if (zmq_msg_send(&m, to, more ? ZMQ_SNDMORE : 0) < 0) { zmq_msg_close(&m); return -1; }
    } while (more);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: %s XSUB_ENDPOINT XPUB_ENDPOINT\n", argv[0]); return 1; }
    void *ctx = zmq_ctx_new();
    void *xsub = zmq_socket(ctx, ZMQ_XSUB), *xpub = zmq_socket(ctx, ZMQ_XPUB);
    int one = 1;
    zmq_setsockopt(xpub, ZMQ_XPUB_NODROP, &one, sizeof one);
    if (zmq_bind(xsub, argv[1]) || zmq_bind(xpub, argv[2])) { perror("bind"); return 1; }
    fprintf(stderr, "[libzmq] XSUB %s -> XPUB %s\n", argv[1], argv[2]);
    zmq_pollitem_t items[] = {{xsub, 0, ZMQ_POLLIN, 0}, {xpub, 0, ZMQ_POLLIN, 0}};
    for (;;) {
        if (zmq_poll(items, 2, -1) < 0) return 1;
        if (items[0].revents & ZMQ_POLLIN) while (forward(xsub, xpub) == 0) {}
        if (items[1].revents & ZMQ_POLLIN) while (forward(xpub, xsub) == 0) {}
    }
}
