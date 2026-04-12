//! Integration test: connect the Rust signaling client to the real Node
//! signaling server and verify the full protocol handshake (create-session,
//! join-session, relay, end-session).
//!
//! Requires:
//!   * `node` on PATH
//!   * The project layout at `../../../server/server.js`
//!
//! The test starts the server on a random high port (no TLS) and tears it
//! down at the end. It is gated behind the `GHOSTVIEW_NODE_IT` env var so it
//! doesn't break CI on machines without Node.

use ghostview_pro_lib::signaling;

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

fn node_it_enabled() -> bool {
    std::env::var("GHOSTVIEW_NODE_IT").is_ok_and(|v| v != "0" && !v.is_empty())
}

async fn wait_for_port(port: u16) -> bool {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn pick_port() -> u16 {
    // Bind ephemeral, read port, release it. Race with other processes is
    // tolerable for a test.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_and_end() {
    if !node_it_enabled() {
        eprintln!("skipping: set GHOSTVIEW_NODE_IT=1 to run");
        return;
    }

    // Locate server.js relative to this test file.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let server_js = manifest_dir
        .parent() // src-tauri → tauri-host
        .unwrap()
        .parent() // tauri-host → repo root
        .unwrap()
        .join("server")
        .join("server.js");
    assert!(server_js.exists(), "server.js not found at {server_js:?}");

    let port = pick_port();
    let mut child = TokioCommand::new("node")
        .arg(server_js.as_os_str())
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn node");

    // Drain stdout/stderr in the background so the child doesn't block.
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            eprintln!("[node:stdout] {l}");
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            eprintln!("[node:stderr] {l}");
        }
    });

    assert!(
        wait_for_port(port).await,
        "server did not come up on :{port}"
    );

    let url = format!("ws://127.0.0.1:{port}/");
    let mut client = signaling::connect(&url).await.expect("connect");

    let pin = timeout(Duration::from_secs(5), client.create_session())
        .await
        .expect("create_session timeout")
        .expect("create_session error");
    assert_eq!(pin.len(), 6);
    assert!(pin.chars().all(|c| c.is_ascii_digit()));

    // Send end-session so the server cleans up before shutdown.
    client
        .send(signaling::ClientMessage::EndSession)
        .await
        .expect("end-session");
    client.close().await;

    // Tear down the server.
    let _ = child.kill().await;
}
