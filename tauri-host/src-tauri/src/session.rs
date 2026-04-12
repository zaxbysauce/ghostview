#![allow(dead_code)]
//! Session orchestration.
//!
//! [`AppState`] holds the currently-running session (if any), wires up the
//! capture -> encoder -> WebRTC host pipeline, and talks to the signaling
//! server to obtain a PIN. With the `real-webrtc` feature disabled, the
//! pipeline still runs end-to-end but drops encoded frames instead of
//! publishing them, so the UI can be developed in isolation.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::capture::{self, CaptureError, CaptureHandle, RawFrame};
use crate::codec::{CodecError, Vp9Encoder};
use crate::signaling::{self, SignalingClient, SignalingError};
use crate::webrtc_host::{WebRtcError, WebRtcHost};

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
    WebRtc(String),
}

impl From<WebRtcError> for SessionError {
    fn from(e: WebRtcError) -> Self {
        SessionError::WebRtc(e.to_string())
    }
}

/// Default signaling URL, overridable via `GHOSTVIEW_SIGNALING_URL`.
pub fn signaling_url() -> String {
    std::env::var("GHOSTVIEW_SIGNALING_URL")
        .unwrap_or_else(|_| "ws://localhost:8443/".to_string())
}

/// Handles for a running session so it can be torn down cleanly.
struct Running {
    pin: String,
    capture: CaptureHandle,
    encoder_task: JoinHandle<()>,
    signaling: SignalingClient,
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

    /// Start a new session. Orchestrates signaling connect, PIN request, and
    /// the capture -> encoder pipeline. Returns the PIN on success.
    ///
    /// Does NOT negotiate a real WebRTC connection unless the `real-webrtc`
    /// feature is enabled.
    pub async fn start(&mut self, monitor_index: usize) -> Result<String, SessionError> {
        if self.running.is_some() {
            return Err(SessionError::AlreadyRunning);
        }

        let url = signaling_url();
        tracing::info!(%url, "signaling: connecting");
        let mut client = signaling::connect(&url).await?;
        let pin = client.create_session().await?;
        tracing::info!(%pin, "session: created");

        // Wire capture -> encoder.
        let (frame_tx, mut frame_rx) = mpsc::channel::<RawFrame>(8);
        let capture = capture::start(monitor_index, frame_tx)?;

        // Pick encoder dimensions from the selected monitor, falling back.
        let (w, h) = capture::list_monitors()
            .into_iter()
            .find(|m| m.index == monitor_index)
            .map(|m| (m.width, m.height))
            .unwrap_or((1920, 1080));

        let encoder_task = tokio::spawn(async move {
            let mut encoder = match Vp9Encoder::new(w, h, 4_000) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "encoder: init failed");
                    return;
                }
            };
            #[cfg(feature = "real-webrtc")]
            let webrtc = match WebRtcHost::new().await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::error!(error = %e, "webrtc: init failed");
                    None
                }
            };
            #[cfg(not(feature = "real-webrtc"))]
            let _webrtc: Option<WebRtcHost> = None;

            let mut last_pts: u64 = 0;
            while let Some(frame) = frame_rx.recv().await {
                match encoder.encode(&frame.bgra, frame.pts_ms) {
                    Ok(pkt) => {
                        let duration_ms = frame.pts_ms.saturating_sub(last_pts).max(1);
                        last_pts = frame.pts_ms;
                        #[cfg(feature = "real-webrtc")]
                        {
                            if let Some(ref h) = webrtc {
                                if let Err(e) = h.push_frame(&pkt, duration_ms).await {
                                    tracing::warn!(error = %e, "webrtc: push_frame failed");
                                }
                            }
                        }
                        #[cfg(not(feature = "real-webrtc"))]
                        {
                            // Drop the encoded packet on the floor in scaffold mode.
                            let _ = pkt;
                            let _ = duration_ms;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "encoder: encode failed");
                    }
                }
            }
        });

        self.running = Some(Running {
            pin: pin.clone(),
            capture,
            encoder_task,
            signaling: client,
        });

        Ok(pin)
    }

    /// Stop the current session. No-op if nothing is running.
    pub async fn stop(&mut self) -> Result<(), SessionError> {
        let Some(mut running) = self.running.take() else {
            return Err(SessionError::NotRunning);
        };

        // Tell the server.
        let _ = running
            .signaling
            .send(crate::signaling::ClientMessage::EndSession)
            .await;
        running.signaling.close().await;

        running.capture.stop().await;

        // Give the encoder task a brief moment to drain, then abort.
        let abort_handle = running.encoder_task.abort_handle();
        match tokio::time::timeout(Duration::from_millis(500), running.encoder_task).await {
            Ok(_) => {}
            Err(_) => abort_handle.abort(),
        }

        Ok(())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
