//! rzmq 0.5.26 forwarding server.
//!
//! rzmq has no XPUB/XSUB socket types and no zmq_proxy equivalent, so this is
//! the closest broker rzmq can build: a SUB bound on the frontend that
//! subscribes to everything, and a PUB bound on the backend that filters per
//! subscriber (rzmq PUB filters publisher-side). Publishers connect to the SUB
//! side, subscribers to the PUB side. Upstream subscription propagation is
//! impossible without XPUB/XSUB, exactly like `omq_proxy_hardened`.
//!
//! Flags:
//!   --frontend EP --backend EP
//!   --mode tokio|uring|uring-zc
//!         tokio    : standard Tokio sessions (rzmq defaults)
//!         uring    : io_uring sessions + multishot receive on both sockets
//!         uring-zc : uring + zero-copy send (SEND_ZC), as rzmq's own benchmarks
//!   --cork              TCP_CORK on both sockets
//!   --throttle on|off   rzmq adaptive I/O throttle (rzmq default on; its benchmarks turn it off)
//!   --workers N         io_uring worker threads (rzmq default: ceil(ncpu/2)-2, min 1)
//!   --strategy performance|balanced|low_power   io_uring worker polling strategy
//!   --hwm N             SNDHWM/RCVHWM on both sockets (rzmq default 1000)
//!   --max-msg-size N    MAXMSGSIZE on both sockets (rzmq default -1 = unlimited)
//!   --sndtimeo MS       PUB send timeout (rzmq default -1 = wait forever on a full subscriber)
//!   --tokio-workers N   Tokio worker threads (default 2)
//!   --sqpoll            IORING_SETUP_SQPOLL (kernel SQ polling thread per io_uring worker)
use rzmq::socket::{
    ADAPTIVE_THROTTLE, IO_URING_RCVMULTISHOT, IO_URING_SESSION_ENABLED, IO_URING_SNDZEROCOPY,
    MAXMSGSIZE, RCVHWM, SNDHWM, SNDTIMEO, SUBSCRIBE, TCP_CORK,
};
use rzmq::uring::{UringConfig, UringPollingStrategy, initialize_uring_backend};
use rzmq::{Context, Socket, SocketType};

#[derive(Clone)]
struct Args {
    frontend: String,
    backend: String,
    mode: String,
    cork: bool,
    throttle: Option<bool>,
    workers: Option<usize>,
    strategy: String,
    hwm: Option<i32>,
    max_msg: Option<i64>,
    sndtimeo: Option<i32>,
    tokio_workers: usize,
    sqpoll: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        frontend: "tcp://127.0.0.1:5555".into(),
        backend: "tcp://127.0.0.1:5556".into(),
        mode: "tokio".into(),
        cork: false,
        throttle: None,
        workers: None,
        strategy: "balanced".into(),
        hwm: None,
        max_msg: None,
        sndtimeo: None,
        tokio_workers: 2,
        sqpoll: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("value");
        match k.as_str() {
            "--frontend" => a.frontend = v(),
            "--backend" => a.backend = v(),
            "--mode" => a.mode = v(),
            "--cork" => a.cork = true,
            "--sqpoll" => a.sqpoll = true,
            "--throttle" => a.throttle = Some(v() == "on"),
            "--workers" => a.workers = Some(v().parse().expect("int")),
            "--strategy" => a.strategy = v(),
            "--hwm" => a.hwm = Some(v().parse().expect("int")),
            "--max-msg-size" => a.max_msg = Some(v().parse().expect("int")),
            "--sndtimeo" => a.sndtimeo = Some(v().parse().expect("int")),
            "--tokio-workers" => a.tokio_workers = v().parse().expect("int"),
            // accepted for CLI parity with the other servers
            "--io-threads" | "--run-on" | "--slot-cap" => {
                let _ = v();
            }
            "--xpub-nodrop" => {}
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

async fn configure(s: &Socket, a: &Args, uring: bool, zc: bool) -> Result<(), rzmq::ZmqError> {
    if let Some(h) = a.hwm {
        s.set_option(SNDHWM, h).await?;
        s.set_option(RCVHWM, h).await?;
    }
    if let Some(m) = a.max_msg {
        s.set_option_raw(MAXMSGSIZE, &m.to_ne_bytes()).await?;
    }
    if let Some(t) = a.throttle {
        s.set_option(ADAPTIVE_THROTTLE, i32::from(t)).await?;
    }
    if a.cork {
        s.set_option(TCP_CORK, true).await?;
    }
    if uring {
        s.set_option(IO_URING_SESSION_ENABLED, true).await?;
        s.set_option(IO_URING_RCVMULTISHOT, true).await?;
        if zc {
            s.set_option(IO_URING_SNDZEROCOPY, true).await?;
        }
    }
    Ok(())
}

fn main() {
    let a = parse_args();
    let uring = a.mode.starts_with("uring");
    let zc = a.mode == "uring-zc";
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(a.tokio_workers)
        .enable_all()
        .build()
        .expect("runtime");
    let code = rt.block_on(async move {
        if uring {
            let polling_strategy = match a.strategy.as_str() {
                "performance" => UringPollingStrategy::ultra_low_latency(),
                "low_power" => UringPollingStrategy::low_power(),
                _ => UringPollingStrategy::balanced(),
            };
            let mut cfg = UringConfig {
                polling_strategy,
                default_send_zerocopy: zc,
                default_recv_multishot: true,
                ..UringConfig::default()
            };
            if let Some(w) = a.workers {
                cfg.num_workers = w;
            }
            cfg.sqpoll_enabled = a.sqpoll;
            initialize_uring_backend(cfg).expect("initialize io_uring backend");
        }
        let ctx = Context::new().expect("context");
        let sub = ctx.socket(SocketType::Sub).expect("sub");
        let publ = ctx.socket(SocketType::Pub).expect("pub");
        configure(&sub, &a, uring, zc).await.expect("configure sub");
        configure(&publ, &a, uring, zc).await.expect("configure pub");
        if let Some(t) = a.sndtimeo {
            publ.set_option(SNDTIMEO, t).await.expect("sndtimeo");
        }
        sub.set_option(SUBSCRIBE, b"".as_slice()).await.expect("subscribe all");
        sub.bind(&a.frontend).await.expect("bind sub");
        publ.bind(&a.backend).await.expect("bind pub");
        println!(
            "READY rzmq 0.5.26 mode={} cork={} throttle={:?} workers={:?} strategy={} sqpoll={} hwm={:?}",
            a.mode, a.cork, a.throttle, a.workers, a.strategy, a.sqpoll, a.hwm
        );
        loop {
            let frames = match sub.recv_multipart().await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("PROXY EXIT: sub recv {e:?}");
                    return 3;
                }
            };
            if let Err(e) = publ.send_multipart(frames).await {
                eprintln!("PROXY EXIT: pub send {e:?}");
                return 3;
            }
        }
    });
    std::process::exit(code);
}
