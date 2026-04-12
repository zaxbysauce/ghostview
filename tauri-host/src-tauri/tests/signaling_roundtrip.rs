//! Integration test: connect the Rust signaling client to the real Node
//! signaling server and verify the full protocol handshake (create-session,
//! join-session, offer/answer/ICE relay, end-session).
//!
//! Requires:
//!   * `node` on PATH
//!   * The project layout at `../../../server/server.js`
//!
//! The test starts the server on a random high port (no TLS) and tears it
//! down at the end. It is gated behind the `GHOSTVIEW_NODE_IT` env var so it
//! doesn't break CI on machines without Node.

use ghostview_pro_lib::signaling::{
    self, ClientMessage, IceCandidateInit, SdpPayload, ServerMessage,
};

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Recv events until one matches the predicate. Skips events that don't —
/// the pump pushes *every* server message (including SessionCreated /
/// SessionJoined bookkeeping) into the channel.
async fn recv_until<F>(
    rx: &mut mpsc::Receiver<ServerMessage>,
    mut pred: F,
    label: &str,
) -> ServerMessage
where
    F: FnMut(&ServerMessage) -> bool,
{
    let deadline = Duration::from_secs(5);
    loop {
        let msg = timeout(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timeout"))
            .unwrap_or_else(|| panic!("{label}: event channel closed"));
        if pred(&msg) {
            return msg;
        }
        // Otherwise drop it and keep waiting. Useful for skipping the
        // SessionCreated / SessionJoined bookkeeping events the pump always
        // forwards.
        eprintln!("[{label}] skipping {msg:?}");
    }
}

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

fn locate_server_js() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent() // src-tauri → tauri-host
        .unwrap()
        .parent() // tauri-host → repo root
        .unwrap()
        .join("server")
        .join("server.js")
}

async fn spawn_node_server(port: u16) -> Child {
    let server_js = locate_server_js();
    assert!(server_js.exists(), "server.js not found at {server_js:?}");

    let mut child = TokioCommand::new("node")
        .arg(server_js.as_os_str())
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn node");

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
    child
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_and_end() {
    if !node_it_enabled() {
        eprintln!("skipping: set GHOSTVIEW_NODE_IT=1 to run");
        return;
    }

    let port = pick_port();
    let mut child = spawn_node_server(port).await;

    let url = format!("ws://127.0.0.1:{port}/");
    let mut client = signaling::connect(&url).await.expect("connect");

    let pin = timeout(Duration::from_secs(5), client.create_session())
        .await
        .expect("create_session timeout")
        .expect("create_session error");
    assert_eq!(pin.len(), 6);
    assert!(pin.chars().all(|c| c.is_ascii_digit()));

    client
        .send(ClientMessage::EndSession)
        .await
        .expect("end-session");
    client.close().await;

    let _ = child.kill().await;
}

/// Full two-client round trip: host creates a session, viewer joins with the
/// PIN, host sends an offer, viewer sends an answer + ICE, host sees them
/// relayed back through the server.
///
/// This is the contract between the Pro host, the Node server, and any
/// browser viewer — if the server changes how it relays these, this test
/// catches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offer_answer_ice_roundtrip() {
    if !node_it_enabled() {
        eprintln!("skipping: set GHOSTVIEW_NODE_IT=1 to run");
        return;
    }

    let port = pick_port();
    let mut child = spawn_node_server(port).await;
    let url = format!("ws://127.0.0.1:{port}/");

    // --- Host: create session, get PIN ----------------------------------
    let mut host = signaling::connect(&url).await.expect("host connect");
    let mut host_events = host.take_events().expect("host events");
    let pin = timeout(Duration::from_secs(5), host.create_session())
        .await
        .expect("create_session timeout")
        .expect("create_session error");

    // --- Viewer: join with PIN ------------------------------------------
    let mut viewer = signaling::connect(&url).await.expect("viewer connect");
    let mut viewer_events = viewer.take_events().expect("viewer events");
    viewer
        .send(ClientMessage::JoinSession { pin: pin.clone() })
        .await
        .expect("viewer join send");

    // Viewer must receive `session-joined`. Host must receive `viewer-joined`.
    let _ = recv_until(
        &mut viewer_events,
        |m| matches!(m, ServerMessage::SessionJoined),
        "viewer session-joined",
    )
    .await;
    let _ = recv_until(
        &mut host_events,
        |m| matches!(m, ServerMessage::ViewerJoined),
        "host viewer-joined",
    )
    .await;

    // --- Host → Offer → Viewer ------------------------------------------
    let offer_sdp = "v=0\r\no=host 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
    host.send(ClientMessage::Offer {
        sdp: SdpPayload {
            kind: "offer".to_string(),
            sdp: offer_sdp.to_string(),
        },
    })
    .await
    .expect("host send offer");

    let viewer_offer = recv_until(
        &mut viewer_events,
        |m| matches!(m, ServerMessage::Offer { .. }),
        "viewer offer",
    )
    .await;
    match viewer_offer {
        ServerMessage::Offer { sdp } => {
            assert_eq!(sdp.kind, "offer");
            assert_eq!(sdp.sdp, offer_sdp);
        }
        _ => unreachable!(),
    }

    // --- Viewer → Answer → Host -----------------------------------------
    let answer_sdp = "v=0\r\no=viewer 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
    viewer
        .send(ClientMessage::Answer {
            sdp: SdpPayload {
                kind: "answer".to_string(),
                sdp: answer_sdp.to_string(),
            },
        })
        .await
        .expect("viewer send answer");

    let host_answer = recv_until(
        &mut host_events,
        |m| matches!(m, ServerMessage::Answer { .. }),
        "host answer",
    )
    .await;
    match host_answer {
        ServerMessage::Answer { sdp } => {
            assert_eq!(sdp.kind, "answer");
            assert_eq!(sdp.sdp, answer_sdp);
        }
        _ => unreachable!(),
    }

    // --- Viewer → ICE candidate → Host ----------------------------------
    viewer
        .send(ClientMessage::IceCandidate {
            candidate: Some(IceCandidateInit {
                candidate: "candidate:1 1 UDP 100 127.0.0.1 54321 typ host".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
                username_fragment: Some("frag".to_string()),
            }),
        })
        .await
        .expect("viewer send ice");

    let host_ice = recv_until(
        &mut host_events,
        |m| matches!(m, ServerMessage::IceCandidate { candidate: Some(_) }),
        "host ice",
    )
    .await;
    match host_ice {
        ServerMessage::IceCandidate { candidate: Some(c) } => {
            assert!(c.candidate.starts_with("candidate:"));
            assert_eq!(c.sdp_mid.as_deref(), Some("0"));
            assert_eq!(c.sdp_mline_index, Some(0));
        }
        _ => unreachable!(),
    }

    // --- Clean up -------------------------------------------------------
    let _ = host.send(ClientMessage::EndSession).await;
    host.close().await;
    viewer.close().await;
    let _ = child.kill().await;
}
