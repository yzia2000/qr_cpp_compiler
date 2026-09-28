# omq-libzmq 0.5.20: zmq_setsockopt option coverage (from source audit)

Reference: `omq-libzmq/src/opts.rs`. libzmq 4.3.5 for comparison. [tags]: security / data-path
impact when the option is silently ignored.

## Accepted with `return 0` and NO effect (opts.rs:907-970, 812)
4 AFFINITY, 8 RATE, 9 RECOVERY_IVL, 25 MULTICAST_HOPS,
38 TCP_ACCEPT_FILTER [security], 41 ROUTER_RAW, 56 ROUTER_HANDOVER [security: forced on],
57 TOS, 58-60 IPC_FILTER_* [security], 61 CONNECT_ROUTING_ID [data path],
62-65 & 90-91 GSSAPI_* [security: silently runs NULL/plaintext; libzmq returns EINVAL without GSSAPI],
68/99/100 SOCKS_* [security: proxy bypassed], 70 BLOCKY,
71 XPUB_MANUAL [security: subscriptions auto-applied, topic ACL bypass],
72 XPUB_WELCOME_MSG [data path], 73 STREAM_NOTIFY,
74 INVERT_MATCHING [data path: subscriber gets complement],
78 XPUB_VERBOSER, 80 TCP_MAXRT, 84 MULTICAST_MAXTPDU, 85-88 VMCI_*, 89 USE_FD,
92 BINDTODEVICE [security: binds all interfaces], 94 LOOPBACK_FASTPATH, 95 METADATA,
96 MULTICAST_LOOP, 97 ROUTER_NOTIFY [data path], 98 MANUAL_LAST_VALUE,
101-102 IN/OUT_BATCH_SIZE, 108 ONLY_FIRST_SUBSCRIBE,
110/111/114 HELLO/DISCONNECT/HICCUP_MSG [data path], 112 PRIORITY, 113 BUSY_POLL,
115 XSUB_VERBOSE_UNSUBSCRIBE, 117-124 NORM_*.
Read-only 13,14,15,16,32,43,81,116 also return 0 (libzmq: EINVAL).

## Stored but never applied
19 BACKLOG, 40 XPUB_VERBOSE, 51 PROBE_ROUTER, 52 REQ_CORRELATE, 53 REQ_RELAXED.

## Partially applied
79 CONNECT_TIMEOUT, 109 RECONNECT_STOP (conn-refused bit only), 55 ZAP_DOMAIN (fail-open, SH2),
54 CONFLATE.

## Work only if set BEFORE first bind/connect (SH3 freeze)
ZAP_ENFORCE_DOMAIN, IMMEDIATE, ROUTER_MANDATORY, SNDTIMEO/RCVTIMEO (last two stay dynamic).

## Rejected with EINVAL
unknown numbers; HWM<=0 (libzmq: 0=unlimited, SH4); IDENTITY>255; non-ASCII/space PLAIN creds;
ZAP_DOMAIN non-ASCII or >255; bad CURVE key length; optvallen < 4/8.

Verified in source during this review: HWM<=0 -> EINVAL (opts.rs:444-461); CURVE ZAP skipped when
domain empty (socket.rs:418-429); to_options() only called from ensure_materialized (socket.rs:492).
