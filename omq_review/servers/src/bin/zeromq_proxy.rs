//! XSUB/XPUB forwarding server on zmq.rs (`zeromq` crate 0.6.0), the other
//! pure-Rust ZeroMQ implementation. zeromq exposes no HWM or io-thread knobs,
//! so those flags are accepted and ignored.
use zeromq::{Socket, XPubSocket, XSubSocket};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut frontend = "tcp://127.0.0.1:5555".to_string();
    let mut backend = "tcp://127.0.0.1:5556".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--frontend" => frontend = it.next().expect("value"),
            "--backend" => backend = it.next().expect("value"),
            "--io-threads" | "--hwm" | "--run-on" => {
                let _ = it.next();
            }
            "--xpub-nodrop" => {}
            other => panic!("unknown arg {other}"),
        }
    }
    let mut xsub = XSubSocket::new();
    xsub.bind(&frontend).await.expect("bind xsub");
    let mut xpub = XPubSocket::new();
    xpub.bind(&backend).await.expect("bind xpub");
    println!("READY zeromq 0.6.0");
    let rc = zeromq::proxy(xsub, xpub, None).await;
    eprintln!("PROXY EXIT: {rc:?}");
    std::process::exit(if rc.is_ok() { 0 } else { 3 });
}
