#![allow(dead_code)]
//! Session orchestration.
//!
//! [`AppState`] owns the current session. `start()` wires the full pipeline:
//!
//!   capture → encoder → WebRTC video track
//!             signaling ⇄ WebRTC SDP/ICE
//!
//! Phase 1 flow:
//! 1. Open WebSocket to the signaling server, request a PIN.
//! 2. Start the capture + encoder pipeline (encoded frames buffer into the
//!    `TrackLocalStaticSample` immediately; nobody consumes them until a
//!    peer connects).
//! 3. Wait for `viewer-joined`. When it arrives, create an SDP offer and send
//!    it. Local ICE candidates are relayed to signaling as they trickle.
//! 4. Apply the viewer's SDP answer; apply incoming ICE candidates.
//! 5. On `peer-disconnected` / `session-expired` / connection `Failed` or
//!    `Closed`, tear the session down.
//!
//! `stop()` performs the reverse: send `end-session`, close signaling, close
//! the peer connection, stop capture, and wait briefly for the encoder task
//! to exit.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc::ice_transport::ice_server::RTCIceServer;

use crate::capture::{self, CaptureError, CaptureHandle, RawFrame};
use crate::codec::{CodecError, Vp9Encoder};
use crate::signaling::{
    self, ClientMessage, IceCandidateInit, SdpPayload, ServerMessage, SignalingClient,
    SignalingError,
};
use crate::webrtc_host::{ConnState, LocalIceCandidate, WebRtcError, WebRtcHost};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("already running")]
    AlreadyRunning,
    #[error("not running")]
    NotRunning,
    #[error("capture: {0}")]
    Capture(#[from] CaptureError),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("signaling: {0}")]
    Signaling(#[from] SignalingError),
    #[error("webrtc: {0}")]
    WebRtc(#[from] WebRtcError),
}

/// Default signaling URL, overridable via `GHOSTVIEW_SIGNALING_URL`.
pub fn signaling_url() -> String {
    std::env::var("GHOSTVIEW_SIGNALING_URL")
        .unwrap_or_else(|_| "ws://localhost:8443/".to_string())
}

/// ICE server list, overridable via `GHOSTVIEW_ICE_SERVERS` (comma-separated
/// URLs). Defaults to the public Google STUN server. TURN credentials are not
/// configurable via env yet — set them here if deploying behind NAT.
pub fn ice_servers() -> Vec<RTCIceServer> {
    let urls = std::env::var("GHOSTVIEW_ICE_SERVERS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["stun:stun.l.google.com:19302".to_string()]);
    vec![RTCIceServer {
        urls,
        ..Default::default()
    }]
}

struct Running {
    pin: String,
    capture: CaptureHandle,
    encoder_task: JoinHandle<()>,
    signaling_task: JoinHandle<()>,
    ice_forward_task: JoinHandle<()>,
    state_task: JoinHandle<()>,
    signaling: SignalingClient,
    webrtc: Arc<WebRtcHost>,
    /// Triggered when the session ends from the *remote* side (server,
    /// viewer, or peer connection). `stop()` is expected to tolerate being
    /// called either way.
    shutdown_tx: mpsc::Sender<()>,
}

pub struct AppState {
    running: Option<Running>,
}

impl AppState {
    pub fn new() -> Self {
        Self { running: None }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn current_pin(&self) -> Option<&str> {
        self.running.as_ref().map(|r| r.pin.as_str())
    }

    /// Start a new session. Returns the 6-digit PIN the viewer will enter.
    pub async fn start(&mut self, monitor_index: usize) -> Result<String, SessionError> {
        if self.running.is_some() {
            return Err(SessionError::AlreadyRunning);
        }

        // --- Signaling: connect + get PIN --------------------------------------
        let url = signaling_url();
        tracing::info!(%url, "signaling: connecting");
        let mut client = signaling::connect(&url).await?;
        let pin = client.create_session().await?;
        tracing::info!(%pin, "session: created");

        // --- WebRTC host -------------------------------------------------------
        let webrtc = Arc::new(WebRtcHost::new(ice_servers()).await?);

        // --- Capture + encoder -------------------------------------------------
        let (frame_tx, frame_rx) = mpsc::channel::<RawFrame>(4);
        let capture = capture::start(monitor_index, frame_tx)?;

        // Pick encoder dimensions from the chosen monitor, defaulting to 1080p.
        // (On non-Windows this is academic — capture::start already returned
        // UnsupportedPlatform.)
        let (w, h) = capture::list_monitors()
            .into_iter()
            .find(|m| m.index == monitor_index)
            .map(|m| (even(m.width, 1920), even(m.height, 1080)))
            .unwrap_or((1920, 1080));

        let encoder_task = spawn_encoder_task(w, h, frame_rx, Arc::clone(&webrtc));

        // --- Signaling pumps ---------------------------------------------------
        let events = client
            .take_events()
            .ok_or_else(|| SessionError::Signaling(SignalingError::Protocol(
                "signaling events already taken".into(),
            )))?;
        let client_tx = client.sender().ok_or_else(|| {
            SessionError::Signaling(SignalingError::Protocol(
                "signaling sender unavailable".into(),
            ))
        })?;

        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(2);

        let signaling_task = spawn_signaling_loop(
            events,
            client_tx.clone(),
            Arc::clone(&webrtc),
            shutdown_tx.clone(),
        );

        // Forward local ICE candidates → signaling.
        let ice_rx = webrtc.take_ice_stream().await.ok_or_else(|| {
            SessionError::WebRtc(WebRtcError::State("ice stream already taken".into()))
        })?;
        let ice_forward_task = spawn_ice_forwarder(ice_rx, client_tx.clone());

        // Watch peer-connection state for Failed/Closed → trigger shutdown.
        let state_rx = webrtc.take_state_stream().await.ok_or_else(|| {
            SessionError::WebRtc(WebRtcError::State("state stream already taken".into()))
        })?;
        let state_task = spawn_state_watcher(state_rx, shutdown_tx.clone());

        // Tie remote-shutdown signal into a watcher that drops us cleanly.
        // Hold the receiver by moving it into a task that just logs on wake;
        // the actual teardown happens when `stop()` is called by the UI or
        // when AppState is dropped. For automatic unwind-on-remote-close we
        // spawn a supervisor: on the first shutdown tick, tear the session
        // down by sending a synthetic stop via a channel back to AppState.
        // Phase 1 keeps this simple: we just log, because the signaling/ICE
        // tasks exit cleanly on their own and the UI polls state.
        tokio::spawn(async move {
            let mut rx = shutdown_rx;
            if rx.recv().await.is_some() {
                tracing::info!("session: shutdown signaled (remote peer / state change)");
            }
        });

        self.running = Some(Running {
            pin: pin.clone(),
            capture,
            encoder_task,
            signaling_task,
            ice_forward_task,
            state_task,
            signaling: client,
            webrtc,
            shutdown_tx,
        });

        Ok(pin)
    }

    /// Stop the current session. Tears every task down in order. Returns
    /// `NotRunning` if there's nothing active.
    pub async fn stop(&mut self) -> Result<(), SessionError> {
        let Some(mut running) = self.running.take() else {
            return Err(SessionError::NotRunning);
        };

        // Best-effort: tell the server. If the socket is already gone it's fine.
        let _ = running.signaling.send(ClientMessage::EndSession).await;
        running.signaling.close().await;

        // Stop capture first so no more frames queue up.
        running.capture.stop().await;

        // Close peer connection.
        if let Err(e) = running.webrtc.close().await {
            tracing::warn!(error = %e, "webrtc: close failed");
        }

        // Signal any waiters (no-op if already drained).
        let _ = running.shutdown_tx.try_send(());

        // Wait briefly for tasks to exit; abort if they don't.
        abort_after(running.encoder_task, Duration::from_millis(500)).await;
        abort_after(running.signaling_task, Duration::from_millis(500)).await;
        abort_after(running.ice_forward_task, Duration::from_millis(200)).await;
        abort_after(running.state_task, Duration::from_millis(200)).await;

        Ok(())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// --------------------------------------------------------------------------
// Task spawners
// --------------------------------------------------------------------------

/// Encoder pipeline.
///
/// `vpx_encode::Encoder` is `!Send` (holds raw libvpx pointers), so the
/// encode loop lives on a dedicated blocking thread. Encoded packets are
/// handed to an async forwarder task that pushes them into the WebRTC track.
/// The returned `JoinHandle` is for the async forwarder; the blocking thread
/// is orphaned — it exits when `frame_rx` closes (i.e. capture stops).
fn spawn_encoder_task(
    width: u32,
    height: u32,
    mut frame_rx: mpsc::Receiver<RawFrame>,
    webrtc: Arc<WebRtcHost>,
) -> JoinHandle<()> {
    let (enc_tx, mut enc_rx) = mpsc::channel::<(Vec<u8>, u64)>(4);

    tokio::task::spawn_blocking(move || {
        let mut encoder = match Vp9Encoder::new(width, height, 4_000) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "encoder: init failed");
                return;
            }
        };
        let mut last_pts: u64 = 0;
        while let Some(frame) = frame_rx.blocking_recv() {
            if frame.width != width || frame.height != height {
                tracing::warn!(
                    "encoder: frame {}x{} ignored (configured {}x{})",
                    frame.width,
                    frame.height,
                    width,
                    height
                );
                continue;
            }
            match encoder.encode(&frame.bgra, frame.pts_ms) {
                Ok(pkt) if pkt.is_empty() => {}
                Ok(pkt) => {
                    let duration_ms = frame.pts_ms.saturating_sub(last_pts).max(1);
                    last_pts = frame.pts_ms;
                    if enc_tx.blocking_send((pkt, duration_ms)).is_err() {
                        break; // forwarder gone
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "encoder: encode failed");
                }
            }
        }
        // Drain any libvpx-buffered frames on clean shutdown.
        if let Ok(tail) = encoder.finish() {
            if !tail.is_empty() {
                let _ = enc_tx.blocking_send((tail, 1));
            }
        }
    });

    tokio::spawn(async move {
        while let Some((pkt, duration_ms)) = enc_rx.recv().await {
            if let Err(e) = webrtc.push_frame(&pkt, duration_ms).await {
                tracing::warn!(error = %e, "webrtc: push_frame failed");
            }
        }
    })
}

fn spawn_signaling_loop(
    mut events: mpsc::Receiver<ServerMessage>,
    client_tx: mpsc::Sender<ClientMessage>,
    webrtc: Arc<WebRtcHost>,
    shutdown_tx: mpsc::Sender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = events.recv().await {
            match msg {
                ServerMessage::ViewerJoined => {
                    tracing::info!("signaling: viewer joined — creating offer");
                    match webrtc.create_offer().await {
                        Ok(sdp) => {
                            let payload = SdpPayload {
                                kind: "offer".to_string(),
                                sdp,
                            };
                            if let Err(e) =
                                client_tx.send(ClientMessage::Offer { sdp: payload }).await
                            {
                                tracing::warn!(error = %e, "signaling: send offer failed");
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "webrtc: create_offer failed");
                            let _ = shutdown_tx.try_send(());
                            break;
                        }
                    }
                }
                ServerMessage::Answer { sdp } => {
                    if let Err(e) = webrtc.set_answer(sdp.sdp).await {
                        tracing::error!(error = %e, "webrtc: set_answer failed");
                        let _ = shutdown_tx.try_send(());
                        break;
                    }
                }
                ServerMessage::IceCandidate { candidate } => {
                    let Some(c) = candidate else {
                        // End-of-candidates sentinel; webrtc-rs handles this implicitly.
                        continue;
                    };
                    if let Err(e) = webrtc
                        .add_ice_candidate(
                            c.candidate,
                            c.sdp_mid,
                            c.sdp_mline_index,
                            c.username_fragment,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "webrtc: add_ice_candidate failed");
                    }
                }
                ServerMessage::Offer { .. } => {
                    // The host is always the offerer in Phase 1. Receiving an
                    // offer means the server is confused or the protocol drifted.
                    tracing::warn!("signaling: unexpected offer received; ignoring");
                }
                ServerMessage::SessionCreated { .. } | ServerMessage::SessionJoined => {
                    // Already handled by create_session() / not used by host.
                }
                ServerMessage::PeerDisconnected | ServerMessage::SessionExpired => {
                    tracing::info!("signaling: session ended by server/peer");
                    let _ = shutdown_tx.try_send(());
                    break;
                }
                ServerMessage::Error { error } => {
                    tracing::warn!(%error, "signaling: server error");
                    // Hard errors (rate_limited, server_shutdown) mean the
                    // socket is unusable — exit the loop.
                    if error == "server_shutdown" || error == "rate_limited" {
                        let _ = shutdown_tx.try_send(());
                        break;
                    }
                }
            }
        }
    })
}

fn spawn_ice_forwarder(
    mut ice_rx: mpsc::Receiver<LocalIceCandidate>,
    client_tx: mpsc::Sender<ClientMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(c) = ice_rx.recv().await {
            let init = IceCandidateInit {
                candidate: c.candidate,
                sdp_mid: c.sdp_mid,
                sdp_mline_index: c.sdp_mline_index,
                username_fragment: c.username_fragment,
            };
            if let Err(e) = client_tx
                .send(ClientMessage::IceCandidate {
                    candidate: Some(init),
                })
                .await
            {
                tracing::debug!(error = %e, "signaling: ice relay channel closed");
                break;
            }
        }
        // End-of-candidates sentinel.
        let _ = client_tx
            .send(ClientMessage::IceCandidate { candidate: None })
            .await;
    })
}

fn spawn_state_watcher(
    mut state_rx: mpsc::Receiver<ConnState>,
    shutdown_tx: mpsc::Sender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(s) = state_rx.recv().await {
            tracing::info!(state = ?s, "webrtc: connection state");
            match s {
                ConnState::Failed | ConnState::Closed => {
                    let _ = shutdown_tx.try_send(());
                    break;
                }
                _ => {}
            }
        }
    })
}

async fn abort_after(task: JoinHandle<()>, timeout: Duration) {
    let abort = task.abort_handle();
    match tokio::time::timeout(timeout, task).await {
        Ok(_) => {}
        Err(_) => abort.abort(),
    }
}

fn even(v: u32, fallback: u32) -> u32 {
    if v == 0 {
        fallback
    } else if v % 2 == 0 {
        v
    } else {
        v - 1
    }
}
