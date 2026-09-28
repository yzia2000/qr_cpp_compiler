//! XSUB/XPUB forwarding server built on omq-tokio 0.24.0.
//!
//! Publishers connect to `--frontend` (XSUB, bound), subscribers connect to
//! `--backend` (XPUB, bound). Messages flow XSUB -> XPUB, subscriptions flow
//! XPUB -> XSUB, via `omq_tokio::proxy::proxy` (the crate's own zmq_proxy
//! equivalent).
//!
//! Flags:
//!   --frontend EP        XSUB bind endpoint   (default tcp://127.0.0.1:5555)
//!   --backend EP         XPUB bind endpoint   (default tcp://127.0.0.1:5556)
//!   --io-threads N       omq IO threads       (default 1, like libzmq)
//!   --hwm N              send/recv HWM on both sockets (default: omq default 1000)
//!   --run-on main|ctx    run the proxy loop on the main tokio runtime (default)
//!                        or inside the omq context's primary IO runtime
//!   --xpub-nodrop        set xpub_nodrop on the XPUB socket
//!   --slot-cap BYTES     Options::transmit_slot_cap (per-peer encoded-bytes cap;
//!                        omq default 512 KiB)
use std::time::Duration;

use omq_tokio::{Context, ContextConfig, Endpoint, Options, SocketType};

struct Args {
    frontend: String,
    backend: String,
    io_threads: usize,
    hwm: Option<u32>,
    run_on_ctx: bool,
    xpub_nodrop: bool,
    slot_cap: Option<usize>,
    max_msg: Option<usize>,
}

fn parse_args() -> Args {
    let mut a = Args {
        frontend: "tcp://127.0.0.1:5555".into(),
        backend: "tcp://127.0.0.1:5556".into(),
        io_threads: 1,
        hwm: None,
        run_on_ctx: false,
        xpub_nodrop: false,
        slot_cap: None,
        max_msg: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--frontend" => a.frontend = it.next().expect("value"),
            "--backend" => a.backend = it.next().expect("value"),
            "--io-threads" => a.io_threads = it.next().expect("value").parse().expect("int"),
            "--hwm" => a.hwm = Some(it.next().expect("value").parse().expect("int")),
            "--run-on" => a.run_on_ctx = it.next().expect("value") == "ctx",
            "--xpub-nodrop" => a.xpub_nodrop = true,
            "--slot-cap" => a.slot_cap = Some(it.next().expect("value").parse().expect("int")),
            "--max-msg-size" => a.max_msg = Some(it.next().expect("value").parse().expect("int")),
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

async fn serve(ctx: Context, args: Args) -> i32 {
    let mut opts = Options::default();
    if let Some(h) = args.hwm {
        opts.send_hwm = h;
        opts.recv_hwm = h;
    }
    if let Some(c) = args.slot_cap {
        opts.transmit_slot_cap = Some(c);
    }
    if let Some(m) = args.max_msg {
        opts.max_message_size = Some(m);
    }
    let xsub = ctx.socket(SocketType::XSub, opts.clone());
    let mut xpub_opts = opts.clone();
    xpub_opts.xpub_nodrop = args.xpub_nodrop;
    let xpub = ctx.socket(SocketType::XPub, xpub_opts);
    let fe: Endpoint = args.frontend.parse().expect("frontend endpoint");
    let be: Endpoint = args.backend.parse().expect("backend endpoint");
    xsub.bind(fe).await.expect("bind xsub");
    xpub.bind(be).await.expect("bind xpub");
    println!(
        "READY omq-tokio 0.24.0 io_threads={} run_on={} hwm={:?} slot_cap={:?}",
        args.io_threads,
        if args.run_on_ctx { "ctx" } else { "main" },
        args.hwm,
        args.slot_cap
    );
    let exit = omq_tokio::proxy::proxy(xsub, xpub, None).await;
    // Surface the exit reason: a production broker must never leave this loop.
    eprintln!("PROXY EXIT: {exit:?}");
    match exit {
        Ok(_) => 0,
        Err(_) => 3,
    }
}

fn main() {
    let args = parse_args();
    let ctx = Context::with_config(ContextConfig {
        io_threads: args.io_threads,
    });
    let code = if args.run_on_ctx {
        let c2 = ctx.clone();
        ctx.block_on(async move { serve(c2, args).await })
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(serve(ctx.clone(), args))
    };
    // Give stderr a moment to flush before exiting.
    std::thread::sleep(Duration::from_millis(50));
    std::process::exit(code);
}
