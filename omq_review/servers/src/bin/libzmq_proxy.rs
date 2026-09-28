//! XSUB/XPUB forwarding server on libzmq through the `zmq` crate (rust-zmq
//! 0.10). Note: zmq-sys 0.12 always builds its own bundled libzmq 4.3.4 via
//! zeromq-src, so this is "what a Rust user of the zmq crate runs today".
//! The libzmq 4.3.5 reference server is `c_proxy/xproxy.c`.
//!
//! Same flags as `omq_proxy` (minus --run-on): --frontend, --backend,
//! --io-threads, --hwm, --xpub-nodrop.
fn main() {
    let mut frontend = "tcp://127.0.0.1:5555".to_string();
    let mut backend = "tcp://127.0.0.1:5556".to_string();
    let mut io_threads = 1;
    let mut hwm: Option<i32> = None;
    let mut xpub_nodrop = false;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--frontend" => frontend = it.next().expect("value"),
            "--backend" => backend = it.next().expect("value"),
            "--io-threads" => io_threads = it.next().expect("value").parse().expect("int"),
            "--hwm" => hwm = Some(it.next().expect("value").parse().expect("int")),
            "--xpub-nodrop" => xpub_nodrop = true,
            "--run-on" => {
                let _ = it.next();
            }
            other => panic!("unknown arg {other}"),
        }
    }
    let ctx = zmq::Context::new();
    ctx.set_io_threads(io_threads).expect("io threads");
    let xsub = ctx.socket(zmq::XSUB).expect("xsub");
    let mut xpub = ctx.socket(zmq::XPUB).expect("xpub");
    if let Some(h) = hwm {
        for s in [&xsub, &xpub] {
            s.set_sndhwm(h).expect("sndhwm");
            s.set_rcvhwm(h).expect("rcvhwm");
        }
    }
    if xpub_nodrop {
        // rust-zmq 0.10 has no setter for ZMQ_XPUB_NODROP (69); go through zmq-sys.
        let one: i32 = 1;
        let rc = unsafe {
            zmq_sys::zmq_setsockopt(
                xpub.as_mut_ptr(),
                69,
                (&raw const one).cast(),
                std::mem::size_of::<i32>(),
            )
        };
        assert_eq!(rc, 0, "ZMQ_XPUB_NODROP");
    }
    xsub.bind(&frontend).expect("bind xsub");
    xpub.bind(&backend).expect("bind xpub");
    let (a, b, c) = zmq::version();
    println!("READY libzmq {a}.{b}.{c} io_threads={io_threads} hwm={hwm:?}");
    let rc = zmq::proxy(&xsub, &xpub);
    eprintln!("PROXY EXIT: {rc:?}");
    std::process::exit(if rc.is_ok() { 0 } else { 3 });
}
