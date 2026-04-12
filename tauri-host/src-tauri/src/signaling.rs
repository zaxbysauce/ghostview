#![allow(dead_code)]
//! Minimal signaling WebSocket client.
//!
//! Connects to the GhostView signaling server, registers a session (receiving a
//! numeric PIN), and relays WebRTC SDP + ICE messages between the host and the
//! viewer through the server. The wire protocol is JSON; see [`ClientMessage`]
//! / [`ServerMessage`] for the exact shape.
//!
//! # Wire protocol notes
//! * Message `type` values are kebab-case (`create-session`, `ice-candidate`,
//!   etc.) — see each variant.
//! * Outgoing messages intentionally do **not** carry a `pin` field: the server
//!   already knows which session a WebSocket belongs to (it pairs host/viewer
//!   internally). The browser clients likewise send no `pin`.
//! * SDP payloads use [`SdpPayload`], which mirrors the browser's
//!   `RTCSessionDescriptionInit` shape (`{ "type": "offer"|"answer", "sdp": "…" }`).
//! * ICE candidate payloads use [`IceCandidateInit`], which mirrors
//!   `RTCIceCandidateInit` (`{ "candidate": "…", "sdpMid": "…", "sdpMLineIndex": 0 }`).
//!   `candidate` is optional: browsers send `null` at end-of-candidates.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("json error: {0}")]
    Json(String),
    #[error("channel closed")]
    ChannelClosed,
}

impl From<tokio_tungstenite::tungstenite::Error> for SignalingError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        SignalingError::WebSocket(e.to_string())
    }
}

impl From<serde_json::Error> for SignalingError {
    fn from(e: serde_json::Error) -> Self {
        SignalingError::Json(e.to_string())
    }
}

/// Matches the browser's `RTCSessionDescriptionInit` JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdpPayload {
    /// `"offer"` | `"answer"` | `"pranswer"` | `"rollback"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub sdp: String,
}

/// Matches the browser's `RTCIceCandidateInit` JSON shape (camelCase on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IceCandidateInit {
    pub candidate: String,
    #[serde(
        rename = "sdpMid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sdp_mid: Option<String>,
    #[serde(
        rename = "sdpMLineIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sdp_mline_index: Option<u16>,
    #[serde(
        rename = "usernameFragment",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub username_fragment: Option<String>,
}

/// Outbound messages the host sends to the signaling server.
///
/// Variant names serialize to the `type` field in kebab-case.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientMessage {
    /// Ask the server for a new session + PIN.
    CreateSession,
    /// Join an existing session by PIN (viewer direction; unused by the Pro host
    /// but included for completeness / future multi-viewer support).
    JoinSession { pin: String },
    Offer { sdp: SdpPayload },
    Answer { sdp: SdpPayload },
    /// `candidate: null` is a valid end-of-candidates sentinel.
    IceCandidate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate: Option<IceCandidateInit>,
    },
    EndSession,
}

/// Inbound messages from the signaling server.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMessage {
    SessionCreated { pin: String },
    SessionJoined,
    ViewerJoined,
    Offer { sdp: SdpPayload },
    Answer { sdp: SdpPayload },
    IceCandidate {
        #[serde(default)]
        candidate: Option<IceCandidateInit>,
    },
    PeerDisconnected,
    SessionExpired,
    /// The server uses `error` (not `message`) as the string field name.
    Error { error: String },
}

/// Handle to a live signaling connection.
///
/// Call [`SignalingClient::close`] to cleanly tear down the socket; otherwise
/// the socket survives until the struct is dropped.
pub struct SignalingClient {
    url: String,
    /// `None` after `close()` has been called.
    tx: Option<mpsc::Sender<ClientMessage>>,
    events: Option<mpsc::Receiver<ServerMessage>>,
    session_created_rx: Option<oneshot::Receiver<Result<String, SignalingError>>>,
}

impl SignalingClient {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Take the server-event stream. Can only be called once.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<ServerMessage>> {
        self.events.take()
    }

    /// Send a raw client message to the server.
    pub async fn send(&self, msg: ClientMessage) -> Result<(), SignalingError> {
        let tx = self.tx.as_ref().ok_or(SignalingError::ChannelClosed)?;
        tx.send(msg)
            .await
            .map_err(|_| SignalingError::ChannelClosed)
    }

    /// Request a new session and wait for the server's `session-created`
    /// response (or the first error). Returns the PIN on success.
    pub async fn create_session(&mut self) -> Result<String, SignalingError> {
        self.send(ClientMessage::CreateSession).await?;
        let rx = self
            .session_created_rx
            .take()
            .ok_or_else(|| SignalingError::Protocol("session already created".into()))?;
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SignalingError::ChannelClosed),
            Err(_) => Err(SignalingError::Timeout("session-created")),
        }
    }

    /// Close the client. Drops the outbound sender, which lets the pump task
    /// drain and close the WebSocket.
    pub async fn close(&mut self) {
        self.tx = None;
    }
}

/// Connect to the signaling server and spawn the background pump.
pub async fn connect(url: &str) -> Result<SignalingClient, SignalingError> {
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let (client_tx, mut client_rx) = mpsc::channel::<ClientMessage>(32);
    let (event_tx, event_rx) = mpsc::channel::<ServerMessage>(32);
    let (created_tx, created_rx) = oneshot::channel::<Result<String, SignalingError>>();
    let mut created_tx = Some(created_tx);

    // Outbound pump: serialize ClientMessage -> WsMessage::Text. Exits when the
    // last Sender (held by SignalingClient) is dropped.
    let outbound = async move {
        while let Some(msg) = client_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "signaling: serialize failed");
                    continue;
                }
            };
            if let Err(e) = ws_tx.send(WsMessage::Text(text.into())).await {
                tracing::warn!(error = %e, "signaling: ws send failed, exiting pump");
                break;
            }
        }
        let _ = ws_tx.close().await;
    };

    // Inbound pump: parse text frames into ServerMessage, deliver on event_tx,
    // and fulfil the session-created oneshot the first time we see one.
    let inbound_event_tx = event_tx.clone();
    let inbound = async move {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(WsMessage::Text(txt)) => {
                    let parsed: Result<ServerMessage, _> = serde_json::from_str(&txt);
                    match parsed {
                        Ok(sm) => {
                            if let Some(tx) = created_tx.take() {
                                match &sm {
                                    ServerMessage::SessionCreated { pin } => {
                                        let _ = tx.send(Ok(pin.clone()));
                                    }
                                    ServerMessage::Error { error } => {
                                        let _ = tx.send(Err(SignalingError::Server(
                                            error.clone(),
                                        )));
                                    }
                                    _ => {
                                        // Not what we were waiting for yet; put it back.
                                        created_tx = Some(tx);
                                    }
                                }
                            }
                            if inbound_event_tx.send(sm).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, payload = %txt, "signaling: bad json");
                        }
                    }
                }
                Ok(WsMessage::Binary(_)) => {
                    tracing::debug!("signaling: ignoring binary frame");
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {}
                Ok(WsMessage::Close(_)) => break,
                Ok(WsMessage::Frame(_)) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "signaling: ws recv error");
                    break;
                }
            }
        }
    };

    tokio::spawn(async move {
        tokio::join!(outbound, inbound);
    });

    Ok(SignalingClient {
        url: url.to_string(),
        tx: Some(client_tx),
        events: Some(event_rx),
        session_created_rx: Some(created_rx),
    })
}
