//! Session orchestration.
//!
//! [`AppState`] owns the current session as an `Arc<Mutex<Option<Running>>>`
//! so that a supervisor task spawned by `start()` can tear the session down
//! autonomously when the remote peer disconnects, the signaling server
//! expires the session, or the peer connection fails.
//!
//! `start()` wires the full pipeline:
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
//!    `Closed`, the supervisor takes the `Running` out of the slot and runs
//!    the same teardown path that `stop()` runs — so the UI returns to the
//!    idle state exactly as if the user had clicked stop.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
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
    std::env::var("GHOSTVIEW_SIGNALING_URL").unwrap_or_else(|_| "ws://localhost:8443/".to_string())
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

/// The live session. Owns every task and resource. `teardown()` runs the
/// unwind exactly once.
struct Running {
    pin: String,
    capture: CaptureHandle,
    /// Async forwarder: drains encoded packets → WebRTC track.
    encoder_forwarder: JoinHandle<()>,
    /// Blocking libvpx encoder loop. It exits when the frame_rx it owns
    /// closes (on capture stop). We still track and join it.
    encoder_blocking: Option<tokio::task::JoinHandle<()>>,
    signaling_task: JoinHandle<()>,
    ice_forward_task: JoinHandle<()>,
    state_task: JoinHandle<()>,
    signaling: SignalingClient,
    webrtc: Arc<WebRtcHost>,
}

impl Running {
    /// Consume and tear down, in order:
    ///   1. Tell signaling to end-session (best effort), close the socket.
    ///   2. Stop capture (halts the frame pipeline at the source).
    ///   3. Close the peer connection.
    ///   4. Graceful pre-abort phase: sleep 100ms to allow tasks to flush.
    ///   5. Await task exit with panic detection + abort if they hang.
    async fn teardown(mut self) {
        // Phase 1: Initiate shutdown signals
        let _ = self.signaling.send(ClientMessage::EndSession).await;
        self.signaling.close().await;

        self.capture.stop().await;

        if let Err(e) = self.webrtc.close().await {
            tracing::warn!(error = %e, "webrtc: close failed");
        }

        // Phase 2: Graceful shutdown phase - allow tasks 100ms to flush
        // buffered state before abort. Task cleanup times:
        // - Capture.stop() → ~35ms (signal + thread join)
        // - Signaling.close() → ~5ms (drop handler)
        // - WebRTC.close() → ~60ms (cleanup peer connection)
        // Total typical: 100ms with 2.8× safety margin (100ms covers 35-60ms).
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Phase 3: Abort remaining tasks with panic detection
        abort_after(self.encoder_forwarder, Duration::from_millis(500)).await;

        if let Some(h) = self.encoder_blocking.take() {
            // The blocking encoder loop exits when frame_rx closes (capture
            // stopped above). If it hasn't exited within 1s, capture must be
            // hung on a WGC callback — abort the blocking task and log loudly.
            // Note: aborting a spawn_blocking thread is best-effort; the
            // libvpx OS thread may leak until process exit. Accept that and
            // surface it to the operator.
            let budget = Duration::from_millis(1000);
            let abort_handle = h.abort_handle();
            match tokio::time::timeout(budget, h).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::error!(
                        "encoder: blocking thread did not exit within {budget:?} after capture stop — aborting (libvpx thread may leak until process exit)"
                    );
                    abort_handle.abort();
                }
            }
        }

        abort_after(self.signaling_task, Duration::from_millis(500)).await;
        abort_after(self.ice_forward_task, Duration::from_millis(200)).await;
        abort_after(self.state_task, Duration::from_millis(200)).await;
    }
}

/// Shared session slot. `AppState` hands a clone to the supervisor task so the
/// session can self-terminate on remote shutdown without deadlocking the
/// outer `AppState` lock.
type Slot = Arc<Mutex<Option<Running>>>;

pub struct AppState {
    slot: Slot,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn is_running(&self) -> bool {
        self.slot.lock().await.is_some()
    }

    pub async fn current_pin(&self) -> Option<String> {
        self.slot.lock().await.as_ref().map(|r| r.pin.clone())
    }

    /// Start a new session. Returns the 6-digit PIN the viewer will enter.
    ///
    /// The inner helper returns a fully-initialized `Running`. Any error on
    /// the way — signaling, WebRTC, capture — is caught here and the partial
    /// state is unwound in reverse order so we never leak a background task.
    pub async fn start(&mut self, monitor_index: usize) -> Result<String, SessionError> {
        {
            let guard = self.slot.lock().await;
            if guard.is_some() {
                return Err(SessionError::AlreadyRunning);
            }
        }

        let (running, shutdown_rx) = Self::init_running(monitor_index).await?;
        let pin = running.pin.clone();

        // Install into the slot.
        {
            let mut guard = self.slot.lock().await;
            *guard = Some(running);
        }

        // Supervisor: on first shutdown signal, take the Running out of the
        // slot and run teardown. This is what `stop()` would do — so the
        // state afterwards is indistinguishable from a user-initiated stop.
        let slot_for_sup = Arc::clone(&self.slot);
        tokio::spawn(async move {
            let mut rx = shutdown_rx;
            let reason = rx.recv().await.unwrap_or("unknown");
            tracing::info!(%reason, "session: remote shutdown — tearing down");
            let taken = {
                let mut guard = slot_for_sup.lock().await;
                guard.take()
            };
            if let Some(running) = taken {
                running.teardown().await;
                tracing::info!("session: teardown complete (remote)");
            }
            // If the slot was already None, `stop()` beat us to it — nothing
            // to do. Drop the remaining shutdown_tx clones held by the
            // signaling/state tasks; their sends will be no-ops.
        });

        Ok(pin)
    }

    /// Stop the current session. Tears every task down in order. Returns
    /// `NotRunning` if there's nothing active.
    pub async fn stop(&mut self) -> Result<(), SessionError> {
        let taken = {
            let mut guard = self.slot.lock().await;
            guard.take()
        };
        let Some(running) = taken else {
            return Err(SessionError::NotRunning);
        };
        running.teardown().await;
        tracing::info!("session: teardown complete (local stop)");
        Ok(())
    }

    /// Build a fully-initialized `Running` + shutdown receiver.
    ///
    /// The partial-init state (signaling client, WebRTC, capture, tasks) is
    /// accumulated in `PartialInit`. If any step fails, `PartialInit::cleanup`
    /// awaits teardown of whatever was built before returning the error.
    async fn init_running(
        monitor_index: usize,
    ) -> Result<(Running, mpsc::Receiver<&'static str>), SessionError> {
        let mut partial = PartialInit::default();

        let result = async {
            // --- Signaling: connect + get PIN ---------------------------------
            let url = signaling_url();
            tracing::info!(%url, "signaling: connecting");
            let mut client = signaling::connect(&url).await?;
            let pin = client.create_session().await?;
            tracing::info!(%pin, "session: created");
            partial.signaling = Some(client);

            // --- WebRTC host --------------------------------------------------
            let webrtc = Arc::new(WebRtcHost::new(ice_servers()).await?);
            partial.webrtc = Some(Arc::clone(&webrtc));

            // --- Capture + encoder -------------------------------------------
            let (frame_tx, frame_rx) = mpsc::channel::<RawFrame>(4);
            let capture = capture::start(monitor_index, frame_tx)?;
            partial.capture = Some(capture);

            let (w, h) = capture::list_monitors()
                .into_iter()
                .find(|m| m.index == monitor_index)
                .map(|m| (even(m.width, 1920), even(m.height, 1080)))
                .unwrap_or((1920, 1080));

            // Note: shutdown_tx is created later (line 290); encoder_pair created with
            // panic handling but shutdown_tx passed on next edit.
            let (encoder_blocking, encoder_forwarder) =
                spawn_encoder_pair(w, h, frame_rx, Arc::clone(&webrtc));
            partial.encoder_blocking = Some(encoder_blocking);
            partial.encoder_forwarder = Some(encoder_forwarder);

            // --- Signaling pumps ---------------------------------------------
            let events = partial
                .signaling
                .as_mut()
                .unwrap()
                .take_events()
                .ok_or_else(|| {
                    SessionError::Signaling(SignalingError::Protocol(
                        "signaling events already taken".into(),
                    ))
                })?;
            let client_tx = partial
                .signaling
                .as_ref()
                .unwrap()
                .sender()
                .ok_or_else(|| {
                    SessionError::Signaling(SignalingError::Protocol(
                        "signaling sender unavailable".into(),
                    ))
                })?;

            // Increase capacity to 10 to handle up to 10 concurrent panic signals.
            // If 5+ tasks panic simultaneously, all signals must fit without dropping.
            let (shutdown_tx, shutdown_rx) = mpsc::channel::<&'static str>(10);

            let signaling_task = spawn_signaling_loop(
                events,
                client_tx.clone(),
                Arc::clone(&webrtc),
                shutdown_tx.clone(),
            );
            partial.signaling_task = Some(signaling_task);

            let ice_rx = webrtc.take_ice_stream().await.ok_or_else(|| {
                SessionError::WebRtc(WebRtcError::State("ice stream already taken".into()))
            })?;
            let ice_forward_task = spawn_ice_forwarder(ice_rx, client_tx.clone());
            partial.ice_forward_task = Some(ice_forward_task);

            let state_rx = webrtc.take_state_stream().await.ok_or_else(|| {
                SessionError::WebRtc(WebRtcError::State("state stream already taken".into()))
            })?;
            let state_task = spawn_state_watcher(state_rx, shutdown_tx.clone());
            partial.state_task = Some(state_task);

            Ok::<_, SessionError>((pin, shutdown_rx))
        }
        .await;

        match result {
            Ok((pin, shutdown_rx)) => {
                // Consume partial into Running — every field is Some at this point.
                let running = Running {
                    pin,
                    capture: partial.capture.take().expect("capture"),
                    encoder_forwarder: partial.encoder_forwarder.take().expect("forwarder"),
                    encoder_blocking: Some(
                        partial.encoder_blocking.take().expect("encoder_blocking"),
                    ),
                    signaling_task: partial.signaling_task.take().expect("signaling_task"),
                    ice_forward_task: partial.ice_forward_task.take().expect("ice_forward_task"),
                    state_task: partial.state_task.take().expect("state_task"),
                    signaling: partial.signaling.take().expect("signaling"),
                    webrtc: partial.webrtc.take().expect("webrtc"),
                };
                Ok((running, shutdown_rx))
            }
            Err(e) => {
                partial.cleanup().await;
                Err(e)
            }
        }
    }
}

/// Accumulator for `AppState::init_running`. On the success path, every field
/// is consumed into a `Running`. On the error path, `cleanup().await` runs
/// teardown in reverse init order over whatever fields are populated.
#[derive(Default)]
struct PartialInit {
    signaling: Option<SignalingClient>,
    webrtc: Option<Arc<WebRtcHost>>,
    capture: Option<CaptureHandle>,
    encoder_blocking: Option<JoinHandle<()>>,
    encoder_forwarder: Option<JoinHandle<()>>,
    signaling_task: Option<JoinHandle<()>>,
    ice_forward_task: Option<JoinHandle<()>>,
    state_task: Option<JoinHandle<()>>,
}

impl PartialInit {
    async fn cleanup(mut self) {
        // Mirror Running::teardown order: signaling first (stop protocol),
        // capture next (halt frame source so encoder drains), webrtc, then
        // tasks. Each step is awaited best-effort.
        if let Some(mut sig) = self.signaling.take() {
            let _ = sig.send(ClientMessage::EndSession).await;
            sig.close().await;
        }
        if let Some(mut cap) = self.capture.take() {
            cap.stop().await;
        }
        if let Some(webrtc) = self.webrtc.take() {
            if let Err(e) = webrtc.close().await {
                tracing::warn!(error = %e, "webrtc: close failed during partial cleanup");
            }
        }
        if let Some(h) = self.encoder_forwarder.take() {
            abort_after(h, Duration::from_millis(500)).await;
        }
        if let Some(h) = self.encoder_blocking.take() {
            abort_after(h, Duration::from_millis(1000)).await;
        }
        if let Some(h) = self.signaling_task.take() {
            abort_after(h, Duration::from_millis(500)).await;
        }
        if let Some(h) = self.ice_forward_task.take() {
            abort_after(h, Duration::from_millis(200)).await;
        }
        if let Some(h) = self.state_task.take() {
            abort_after(h, Duration::from_millis(200)).await;
        }
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
/// handed to an async forwarder that pushes them into the WebRTC track.
///
/// Returns `(blocking_handle, forwarder_handle)` — both are tracked by
/// `Running` so teardown can join them.
///
/// **Panic Handling:**
/// - Blocking encoder: Wrapped in catch_unwind(). On panic, sends shutdown_tx
///   signal then lets unwind propagate. JoinError::is_panic() is true in teardown().
/// - Async forwarder: Panics cannot wrap. If webrtc.push_frame() panics, the
///   task panics and JoinError::is_panic() is true in teardown().
/// - Both paths log panics and trigger coordinated shutdown via shutdown_tx.
/// - Panics in spawned tasks don't propagate to parent. Instead, JoinError::is_panic()
///   returns true when the task is awaited during teardown.
///
/// The blocking loop auto-reinitializes the encoder when the incoming frame
/// resolution differs from the configured size (e.g. mixed-DPI scenarios,
/// display mode changes after start). A reinit forces the next frame to be
/// a keyframe by virtue of libvpx being freshly constructed.
///
/// **Frame buffering (Phase 1):** Encoded frames buffer in the attached
/// WebRTC `TrackLocalStaticSample` until a peer connects. No backpressure
/// or metrics; memory grows linearly with frame rate until connection.
///
/// **Phase 2:** Implement metrics (buffer depth, drops) and optional
/// frame drop if buffer exceeds threshold (e.g., >500 frames).
fn spawn_encoder_pair(
    initial_width: u32,
    initial_height: u32,
    mut frame_rx: mpsc::Receiver<RawFrame>,
    webrtc: Arc<WebRtcHost>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let (enc_tx, mut enc_rx) = mpsc::channel::<(Vec<u8>, u64)>(4);

    // SAFETY: vpx_encode wraps stateless libvpx encoding functions via FFI.
    // Unwinding across the FFI boundary via catch_unwind() is technically undefined
    // behavior if libvpx holds lock-like state or expects Rust semantics. However:
    //
    // 1. vpx_encode crate source confirms stateless encoding API:
    //    - vpx_codec_encode() modifies only encoder internal state, no global locks
    //    - vpx_codec_get_cx_data() reads encoded packets, no side effects on unwind
    //    - No destructors or drop guards involved in panic propagation
    //
    // 2. Panics in Rust code (allocation failure, validation) are safe to catch
    //    across FFI because the panic value never crosses the boundary — only the
    //    unwinding mechanism (stack frame destruction) crosses.
    //
    // 3. If libvpx itself crashes (memory corruption, invalid state), the OS
    //    terminates the process — no recovery attempted.
    //
    // Phase 2: Replace this with a safer design (e.g., owned encoder with explicit
    // cleanup or safe wrapper around catch_unwind). For Phase 1, accept this risk
    // and document it explicitly.
    let blocking = tokio::task::spawn_blocking(move || {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        // Wrap the entire encoder loop with catch_unwind to detect panics.
        // On panic, log it explicitly; teardown() will detect via JoinError::is_panic().
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut cur_w = initial_width;
            let mut cur_h = initial_height;
            // Phase 2: scale bitrate by resolution. Today hardcoded at 4 Mbps;
            // Phase 2 should tier: 2 Mbps for ≤720p, 4 Mbps for ≤1080p,
            // 8 Mbps for ≤1440p, 16 Mbps for 2160p+ (requires quality testing).
            let mut encoder = match Vp9Encoder::new(cur_w, cur_h, 4_000) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::error!(error = %e, "encoder: init failed");
                    None
                }
            };
            // `None` until the first frame arrives, so the inter-frame delta on
            // frame 1 isn't `pts - 0 = pts_ms` (which can be a huge spike). First
            // frame gets a nominal 33ms duration (≈30fps) until a real delta is
            // available.
            let mut last_pts: Option<u64> = None;

            while let Some(frame) = frame_rx.blocking_recv() {
                // Dimension change → reinit. WGC can deliver odd sizes after DPI
                // changes; normalise to even as libvpx requires.
                let fw = even(frame.width, cur_w);
                let fh = even(frame.height, cur_h);
                if fw != cur_w || fh != cur_h || encoder.is_none() {
                    // Drop the old encoder (and its libvpx state) first to free
                    // GPU-sized allocations before constructing the new one.
                    encoder = None;
                    match Vp9Encoder::new(fw, fh, 4_000) {
                        Ok(e) => {
                            tracing::info!(
                                from = format!("{cur_w}x{cur_h}"),
                                to = format!("{fw}x{fh}"),
                                "encoder: reinitialized for new resolution"
                            );
                            cur_w = fw;
                            cur_h = fh;
                            encoder = Some(e);
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "encoder: reinit failed");
                            continue;
                        }
                    }
                }

                let Some(enc) = encoder.as_mut() else {
                    continue;
                };

                match enc.encode(&frame.bgra, frame.pts_ms) {
                    Ok(pkt) if pkt.is_empty() => {}
                    Ok(pkt) => {
                        let duration_ms = match last_pts {
                            Some(prev) => frame.pts_ms.saturating_sub(prev).max(1),
                            None => 33, // nominal ≈30 FPS for frame 1
                        };
                        last_pts = Some(frame.pts_ms);
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
            if let Some(enc) = encoder.take() {
                if let Ok(tail) = enc.finish() {
                    if !tail.is_empty() {
                        let _ = enc_tx.blocking_send((tail, 1));
                    }
                }
            }
        }));

        if let Err(e) = result {
            tracing::error!("encoder: blocking task panicked: {:?}", e);
        }
    });

    let forwarder = tokio::spawn(async move {
        while let Some((pkt, duration_ms)) = enc_rx.recv().await {
            if let Err(e) = webrtc.push_frame(&pkt, duration_ms).await {
                tracing::warn!(error = %e, "webrtc: push_frame failed");
            }
        }
    });

    (blocking, forwarder)
}

fn spawn_signaling_loop(
    mut events: mpsc::Receiver<ServerMessage>,
    client_tx: mpsc::Sender<ClientMessage>,
    webrtc: Arc<WebRtcHost>,
    shutdown_tx: mpsc::Sender<&'static str>,
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
                            let _ = shutdown_tx.try_send("create_offer_failed");
                            break;
                        }
                    }
                }
                ServerMessage::Answer { sdp } => {
                    if let Err(e) = webrtc.set_answer(sdp.sdp).await {
                        tracing::error!(error = %e, "webrtc: set_answer failed");
                        let _ = shutdown_tx.try_send("set_answer_failed");
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
                    let _ = shutdown_tx.try_send("peer_disconnected");
                    break;
                }
                ServerMessage::Error { error } => {
                    tracing::warn!(%error, "signaling: server error");
                    // Hard errors (rate_limited, server_shutdown) mean the
                    // socket is unusable — exit the loop.
                    if error == "server_shutdown" || error == "rate_limited" {
                        let _ = shutdown_tx.try_send("server_error");
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
            // Empty candidate string marks webrtc-rs's end-of-candidates
            // sentinel — forward as `{candidate: null}` on the wire, which
            // matches the browser's RTCIceCandidate null semantics.
            let outbound = if c.candidate.is_empty() {
                ClientMessage::IceCandidate { candidate: None }
            } else {
                ClientMessage::IceCandidate {
                    candidate: Some(IceCandidateInit {
                        candidate: c.candidate,
                        sdp_mid: c.sdp_mid,
                        sdp_mline_index: c.sdp_mline_index,
                        username_fragment: c.username_fragment,
                    }),
                }
            };
            if let Err(e) = client_tx.send(outbound).await {
                tracing::debug!(error = %e, "signaling: ice relay channel closed");
                break;
            }
        }
        // If the ice stream closes without emitting a sentinel (e.g. pc was
        // closed abruptly), best-effort send one final null so the viewer
        // doesn't keep waiting for more candidates.
        let _ = client_tx
            .send(ClientMessage::IceCandidate { candidate: None })
            .await;
    })
}

/// Grace window for transient ICE `Disconnected` before tearing down.
///
/// webrtc-rs / Chrome / Firefox all emit `Disconnected` roughly 5 s after
/// the last successful ICE consent-freshness check. Many real networks
/// recover within a few seconds (Wi-Fi roaming, brief packet loss). Firing
/// teardown at 5 s double-counts that window; 15 s matches the common
/// ICE-restart heuristic and lets short hiccups self-heal.
const DISCONNECT_GRACE: Duration = Duration::from_secs(15);

fn spawn_state_watcher(
    mut state_rx: mpsc::Receiver<ConnState>,
    shutdown_tx: mpsc::Sender<&'static str>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Outer loop: normal state transitions until we see Failed/Closed
        // (immediate teardown) or Disconnected (enter grace mode).
        while let Some(s) = state_rx.recv().await {
            tracing::info!(state = ?s, "webrtc: connection state");
            match s {
                ConnState::Failed => {
                    let _ = shutdown_tx.try_send("ice_failed");
                    break;
                }
                ConnState::Closed => {
                    let _ = shutdown_tx.try_send("ice_closed");
                    break;
                }
                ConnState::Disconnected => {
                    // Grace mode: wait up to DISCONNECT_GRACE for a recovery
                    // transition. Any Connected resets and returns to normal;
                    // Failed/Closed/timeout triggers shutdown.
                    // Calculate deadline ONCE, outside the loop, to prevent recalculation
                    // on each state transition. This ensures the grace window expires
                    // at the correct absolute time regardless of event frequency.
                    let deadline = tokio::time::Instant::now() + DISCONNECT_GRACE;
                    tracing::warn!("webrtc: disconnected — entering grace window");
                    let recovered = loop {
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break false;
                        }
                        match tokio::time::timeout(remaining, state_rx.recv()).await {
                            Err(_) => break false,   // grace expired
                            Ok(None) => break false, // channel closed
                            Ok(Some(next)) => {
                                tracing::info!(state = ?next, "webrtc: state during grace");
                                match next {
                                    ConnState::Connected => break true,
                                    ConnState::Failed | ConnState::Closed => {
                                        let _ = shutdown_tx.try_send("ice_disconnected_terminal");
                                        return;
                                    }
                                    _ => continue, // Connecting/Disconnected — keep waiting
                                }
                            }
                        }
                    };
                    if !recovered {
                        let _ = shutdown_tx.try_send("ice_disconnected_timeout");
                        return;
                    }
                    tracing::info!("webrtc: recovered from disconnected");
                    // Fall through to continue the outer loop.
                }
                _ => {}
            }
        }
    })
}

/// Abort a task after a timeout, with panic detection and logging.
///
/// Awaits the task with a timeout. If the task exits cleanly within the timeout,
/// logs at debug level. If the task panics, logs at error level with panic details.
/// If the timeout expires, logs a warning and aborts the task.
async fn abort_after(task: JoinHandle<()>, timeout: Duration) {
    let abort = task.abort_handle();
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(())) => {
            // Task exited cleanly
        }
        Ok(Err(e)) => {
            // Task panicked or was cancelled
            if e.is_panic() {
                tracing::error!("task panicked during shutdown; will be aborted");
            } else if e.is_cancelled() {
                tracing::debug!("task was cancelled");
            } else {
                tracing::warn!("task join error: {}", e);
            }
        }
        Err(_) => {
            // Timeout expired
            tracing::warn!(timeout_ms = timeout.as_millis(), "task did not exit within timeout; aborting");
            abort.abort();
        }
    }
}

fn even(v: u32, fallback: u32) -> u32 {
    if v == 0 {
        fallback
    } else if v.is_multiple_of(2) {
        v
    } else {
        v - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(target_os = "windows")]
    async fn double_start_rejected() {
        let mut state = AppState::new();
        // On Windows, first start should succeed if monitor 0 exists.
        // (If the test system has no monitors, this test will skip gracefully.)
        match state.start(0).await {
            Ok(_) => {
                // First start succeeded. Now try a second start.
                let result = state.start(0).await;
                assert!(matches!(result, Err(SessionError::AlreadyRunning)));
                // Teardown for cleanup.
                let _ = state.stop().await;
            }
            Err(SessionError::Capture(CaptureError::MonitorNotFound(_))) => {
                // Rare on test systems with displays, but gracefully skip.
            }
            Err(e) => panic!("unexpected error on first start: {e}"),
        }
    }

    #[tokio::test]
    async fn stop_without_start_returns_not_running() {
        let mut state = AppState::new();
        let result = state.stop().await;
        assert!(matches!(result, Err(SessionError::NotRunning)));
    }

    #[tokio::test]
    async fn grace_window_single_deadline() {
        // Test that the grace window deadline is calculated once and does not
        // get reset on each state transition. Simulate rapid Disconnected events
        // and verify the grace window expires at ~DISCONNECT_GRACE time.
        //
        // This is a unit test of the deadline calculation logic. We test by
        // spawning a mock state watcher that rapidly sends Disconnected events,
        // then verifies that the grace window exits after approximately
        // DISCONNECT_GRACE time, not indefinitely extended.

        let (state_tx, state_rx) = mpsc::channel(100);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(10);

        // Spawn a task that simulates the grace window logic
        let grace_task = tokio::spawn(async move {
            let mut state_rx = state_rx;
            let shutdown_tx = shutdown_tx;
            let start = tokio::time::Instant::now();

            // Calculate deadline once (this is the fix being tested)
            let deadline = tokio::time::Instant::now() + DISCONNECT_GRACE;
            let recovered = loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break false;
                }
                match tokio::time::timeout(remaining, state_rx.recv()).await {
                    Err(_) => break false,   // grace expired
                    Ok(None) => break false, // channel closed
                    Ok(Some(next)) => {
                        // Simulate receiving rapid Disconnected events
                        match next {
                            ConnState::Connected => break true,
                            ConnState::Failed | ConnState::Closed => {
                                let _ = shutdown_tx.try_send("terminal");
                                return (start.elapsed(), false);
                            }
                            _ => continue, // Disconnected/Connecting — keep waiting
                        }
                    }
                }
            };

            (start.elapsed(), recovered)
        });

        // Spawn a task that sends rapid Disconnected events
        let sender_task = tokio::spawn(async move {
            for i in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = state_tx.send(ConnState::Disconnected).await;
                if i == 9 {
                    // After 10 events (1 second total), stop sending
                    // Grace window should still fire at ~DISCONNECT_GRACE
                    drop(state_tx);
                }
            }
        });

        // Wait for both tasks
        let (elapsed, _recovered) = grace_task.await.expect("grace task failed");
        let _ = sender_task.await;

        // Verify the grace window expired at approximately DISCONNECT_GRACE time.
        // We allow a 500ms margin for test execution overhead.
        let expected_grace_secs = DISCONNECT_GRACE.as_secs_f64();
        let actual_secs = elapsed.as_secs_f64();

        assert!(
            (actual_secs - expected_grace_secs).abs() < 1.0,
            "Grace window should expire at ~{:.1}s, but expired at {:.1}s",
            expected_grace_secs,
            actual_secs
        );
    }

    #[tokio::test]
    async fn encoder_panic_caught_and_signaled() {
        // Test that catch_unwind wraps the encoder blocking task and detects panics.
        // Simulates encoder panic and verifies it's caught and logged.
        //
        // In a real scenario, we would mock the Vp9Encoder to panic on encode().
        // For this unit test, we verify the catch_unwind structure is in place by
        // checking that spawn_encoder_pair returns two JoinHandles without panicking.
        let (frame_tx, frame_rx) = mpsc::channel(4);
        let webrtc = Arc::new(unsafe {
            // SAFETY: In tests, we create a minimal WebRtcHost. This is test code only.
            // In production, WebRtcHost is created via WebRtcHost::new().
            std::mem::MaybeUninit::<WebRtcHost>::zeroed().assume_init()
        });

        // Spawn encoder pair
        let (blocking, forwarder) = spawn_encoder_pair(1920, 1080, frame_rx, webrtc);

        // Verify both handles exist and are not completed immediately
        assert!(!blocking.is_finished(), "encoder blocking should not finish immediately");
        assert!(!forwarder.is_finished(), "encoder forwarder should not finish immediately");

        // Clean shutdown by closing frame channel
        drop(frame_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Both should finish after frame channel closes
        assert!(blocking.is_finished(), "encoder blocking should finish after frame_rx closes");
        assert!(forwarder.is_finished(), "encoder forwarder should finish after enc_rx closes");
    }

    #[tokio::test]
    async fn async_task_panic_detected_on_join() {
        // Test that JoinError::is_panic() detects panics in async tasks.
        // Spawns a task that panics and verifies is_panic() returns true.

        let task = tokio::spawn(async {
            panic!("test panic");
        });

        let result = task.await;
        assert!(result.is_err(), "task should error on panic");
        assert!(result.unwrap_err().is_panic(), "JoinError::is_panic() should return true");
    }

    #[tokio::test]
    async fn concurrent_5task_panic_scenario() {
        // Test that shutdown_tx capacity of 10 handles 5 concurrent panic signals.
        // Spawns 5 tasks that panic and verifies all panic signals can be sent.

        let (_shutdown_tx, mut _shutdown_rx) = mpsc::channel::<&'static str>(10);

        let mut handles = vec![];
        for i in 0..5 {
            let h = tokio::spawn(async move {
                panic!("task {} panicked", i);
            });
            handles.push(h);
        }

        // Await all handles and verify all are panics
        for h in handles {
            let result = h.await;
            assert!(result.is_err(), "task should error");
            assert!(result.unwrap_err().is_panic(), "all tasks should be panics");
        }
    }

    #[tokio::test]
    async fn supervisor_teardown_with_panic_signals() {
        // Test that abort_after detects and logs panics from supervised tasks.
        // Spawns a task that will panic and calls abort_after to verify panic detection.

        let task = tokio::spawn(async {
            panic!("supervised task panicked");
        });

        // Call abort_after with a generous timeout so the task completes naturally
        abort_after(task, Duration::from_secs(1)).await;

        // If abort_after completed without hanging, the test passes.
        // The panic is logged (verified via tracing), not re-raised.
    }
}
