//! omq-tokio 0.24.0 XSUB/XPUB forwarder with every user-side workaround for
//! the review's findings applied. It is NOT a drop-in zmq_proxy: it gives up
//! upstream subscription forwarding (publishers send everything to the
//! broker) in exchange for not depending on omq's subscription bookkeeping.
//!
//! Workarounds:
//!  * io_threads >= 2           -> avoids the 64-entry fan-out control-ring
//!                                 livelock (lane worker and socket actor on
//!                                 different runtimes).
//!  * transmit_slot_cap 64 MiB  -> XPUB no longer drops 100 KB-1 MB messages
//!                                 once ~512 KiB is queued per subscriber.
//!  * XSUB subscribes to "" once and never forwards SUBSCRIBE/CANCEL
//!                              -> no refcount / leak / amplification bugs;
//!                                 XPUB still filters per subscriber.
//!  * max_message_size 16 MiB -> a crafted oversized frame length closes that
//!                                 peer instead of aborting the process (C1).
//!  * XPUB notifications and upstream data are drained and discarded
//!                              -> a peer cannot terminate the forwarder, and
//!                                 the XPUB actor never blocks on a full
//!                                 receive queue.
//!
//! Flags: --frontend EP --backend EP [--io-threads N] [--slot-cap BYTES] [--hwm N]
use std::time::Duration;

use omq_tokio::{Context, ContextConfig, Endpoint, Options, SocketType};

fn main() {
    let mut frontend = "tcp://127.0.0.1:5555".to_string();
    let mut backend = "tcp://127.0.0.1:5556".to_string();
    let mut io_threads = 2usize;
    let mut slot_cap = 64usize << 20;
    let mut max_msg = 16usize << 20;  // finite cap closes the oversized-frame abort (finding C1)
    let mut hwm: Option<u32> = None;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--frontend" => frontend = it.next().expect("value"),
            "--backend" => backend = it.next().expect("value"),
            "--io-threads" => io_threads = it.next().expect("value").parse().expect("int"),
            "--slot-cap" => slot_cap = it.next().expect("value").parse().expect("int"),
            "--max-msg-size" => max_msg = it.next().expect("value").parse().expect("int"),
            "--hwm" => hwm = Some(it.next().expect("value").parse().expect("int")),
            "--run-on" => {
                let _ = it.next();
            }
            "--xpub-nodrop" => {}
            other => panic!("unknown arg {other}"),
        }
    }
    let ctx = Context::with_config(ContextConfig { io_threads });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let code = rt.block_on(async move {
        let mut opts = Options::default();
        opts.transmit_slot_cap = Some(slot_cap);
        opts.max_message_size = Some(max_msg);
        if let Some(h) = hwm {
            opts.send_hwm = h;
            opts.recv_hwm = h;
        }
        let xsub = ctx.socket(SocketType::XSub, opts.clone());
        let xpub = ctx.socket(SocketType::XPub, opts);
        let fe: Endpoint = frontend.parse().expect("frontend endpoint");
        let be: Endpoint = backend.parse().expect("backend endpoint");
        xsub.bind(fe).await.expect("bind xsub");
        xpub.bind(be).await.expect("bind xpub");
        // Subscribe to everything upstream once; replayed to every publisher
        // that connects later.
        xsub.subscribe(bytes::Bytes::new()).await.expect("subscribe all");
        println!("READY omq-tokio 0.24.0 hardened io_threads={io_threads} slot_cap={slot_cap} max_msg={max_msg}");

        // Drain XPUB-side traffic (subscription notifications, upstream data)
        // so the XPUB actor never blocks on its receive queue.
        let drain = xpub.clone();
        tokio::spawn(async move {
            while drain.recv().await.is_ok() {}
        });
        loop {
            match xsub.recv().await {
                Ok(msg) => {
                    if let Err(e) = xpub.send(msg).await {
                        eprintln!("PROXY EXIT: xpub send {e:?}");
                        return 3;
                    }
                }
                Err(e) => {
                    eprintln!("PROXY EXIT: xsub recv {e:?}");
                    return 3;
                }
            }
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    std::process::exit(code);
}
