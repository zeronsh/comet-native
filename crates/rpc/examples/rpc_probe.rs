//! Ad-hoc RPC probe: call or subscribe against a running engine's IPC socket.
//!
//! Usage:
//!   cargo run -p comet-rpc --example rpc_probe -- ws://127.0.0.1:27801 LocalDevice '{}'
//!   cargo run -p comet-rpc --example rpc_probe -- ws://127.0.0.1:27801 WatchSessions '{}' --stream 3

use comet_rpc::connect_ws;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, method, params, rest @ ..] = args.as_slice() else {
        eprintln!("usage: rpc_probe <ws-url> <method> <params-json> [--stream [n]]");
        std::process::exit(2);
    };
    let params: serde_json::Value = serde_json::from_str(params).expect("params json");
    let client = connect_ws(url).await.expect("connect");
    if rest.first().map(String::as_str) == Some("--stream") {
        let count: usize = rest.get(1).and_then(|n| n.parse().ok()).unwrap_or(1);
        let mut rx = client.subscribe(method, params).await.expect("subscribe");
        for _ in 0..count {
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await {
                Ok(Some(item)) => println!("{item}"),
                Ok(None) => {
                    eprintln!("stream ended");
                    break;
                }
                Err(_) => {
                    eprintln!("timed out");
                    break;
                }
            }
        }
    } else {
        match client.call(method, params).await {
            Ok(value) => println!("{value}"),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        }
    }
}
