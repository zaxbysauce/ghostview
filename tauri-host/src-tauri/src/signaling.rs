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
use std::sync::{Arc, Mutex as StdMutex};
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
    #[serde(rename = "sdpMid", default, skip_serializing_if = "Option::is_none")]
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
    JoinSession {
        pin: String,
    },
    Offer {
        sdp: SdpPayload,
    },
    Answer {
        sdp: SdpPayload,
    },
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
    SessionCreated {
        pin: String,
    },
    SessionJoined,
    ViewerJoined,
    Offer {
        sdp: SdpPayload,
    },
    Answer {
        sdp: SdpPayload,
    },
    IceCandidate {
        #[serde(default)]
        candidate: Option<IceCandidateInit>,
    },
    PeerDisconnected,
    SessionExpired,
    /// The server uses `error` (not `message`) as the string field name.
    Error {
        error: String,
    },
}

/// Slot the inbound pump uses to deliver the `session-created` (or server
/// error while waiting for it) to a pending `create_session()` call. The slot
/// is armed *only* inside `create_session()` so that unsolicited pre-
/// create-session errors never consume a oneshot that no one is waiting on.
type CreatedSlot = Arc<StdMutex<Option<oneshot::Sender<Result<String, SignalingError>>>>>;

/// Handle to a live signaling connection.
///
/// Call [`SignalingClient::close`] to cleanly tear down the socket; otherwise
/// the socket survives until the struct is dropped.
pub struct SignalingClient {
    url: String,
    /// `None` after `close()` has been called.
    tx: Option<mpsc::Sender<ClientMessage>>,
    events: Option<mpsc::Receiver<ServerMessage>>,
    created_slot: CreatedSlot,
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

    /// Clone the outbound sender so background tasks can push messages
    /// independently of the [`SignalingClient`] lifetime. Returns `None`
    /// after [`close`] has been called.
    pub fn sender(&self) -> Option<mpsc::Sender<ClientMessage>> {
        self.tx.clone()
    }

    /// Request a new session and wait for the server's `session-created`
    /// response (or the first error while waiting). Returns the PIN on success.
    ///
    /// The `session-created` slot is armed *here* — not at connect time — so
    /// an unsolicited server error before `create_session()` is called cannot
    /// consume the oneshot that a later `create_session()` would block on.
    pub async fn create_session(&mut self) -> Result<String, SignalingError> {
        let (tx, rx) = oneshot::channel::<Result<String, SignalingError>>();
        {
            let mut slot = self.created_slot.lock().unwrap();
            if slot.is_some() {
                return Err(SignalingError::Protocol(
                    "create_session already in flight".into(),
                ));
            }
            *slot = Some(tx);
        }

        if let Err(e) = self.send(ClientMessage::CreateSession).await {
            // Disarm the slot on send failure.
            let _ = self.created_slot.lock().unwrap().take();
            return Err(e);
        }

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SignalingError::ChannelClosed),
            Err(_) => {
                // Disarm on timeout so a late response doesn't stall a later call.
                let _ = self.created_slot.lock().unwrap().take();
                Err(SignalingError::Timeout("session-created"))
            }
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
    let created_slot: CreatedSlot = Arc::new(StdMutex::new(None));
    let pump_slot = Arc::clone(&created_slot);

    // Outbound pump: serialize ClientMessage -> WsMessage::Text. Exits when the
    // last Sender (held by SignalingClient) is dropped. Also drives a 20 s
    // keepalive ping — under common NAT UDP idle timeouts (~30 s) and well
    // under idle TCP timeouts — so the signaling path stays warm even when
    // the WebRTC data path is carrying all the traffic.
    let outbound = async move {
        let mut keepalive = tokio::time::interval(Duration::from_secs(20));
        keepalive.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                maybe = client_rx.recv() => {
                    let Some(msg) = maybe else { break };
                    let text = match serde_json::to_string(&msg) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::error!(error = %e, "signaling: serialize failed");
                            continue;
                        }
                    };
                    if let Err(e) = ws_tx.send(WsMessage::Text(text)).await {
                        tracing::warn!(error = %e, "signaling: ws send failed, exiting pump");
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = ws_tx.send(WsMessage::Ping(Vec::new())).await {
                        tracing::warn!(error = %e, "signaling: keepalive ping failed, exiting pump");
                        break;
                    }
                }
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
                            // Only fulfil the slot if it's armed (i.e. a
                            // create_session() call is in flight). Unsolicited
                            // errors delivered before create_session() is
                            // called go to the events channel only.
                            match &sm {
                                ServerMessage::SessionCreated { pin } => {
                                    if let Some(tx) = pump_slot.lock().unwrap().take() {
                                        let _ = tx.send(Ok(pin.clone()));
                                    }
                                }
                                ServerMessage::Error { error } => {
                                    if let Some(tx) = pump_slot.lock().unwrap().take() {
                                        let _ = tx.send(Err(SignalingError::Server(error.clone())));
                                    }
                                }
                                _ => {}
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
        created_slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outbound shapes must match the browser's wire protocol exactly. These
    /// assertions are the contract between the Pro host and the Node signaling
    /// server in server/server.js.
    #[test]
    fn create_session_is_bare_type() {
        let json = serde_json::to_string(&ClientMessage::CreateSession).unwrap();
        assert_eq!(json, r#"{"type":"create-session"}"#);
    }

    #[test]
    fn end_session_carries_no_pin() {
        let json = serde_json::to_string(&ClientMessage::EndSession).unwrap();
        assert_eq!(json, r#"{"type":"end-session"}"#);
    }

    #[test]
    fn offer_wraps_sdp_payload() {
        let msg = ClientMessage::Offer {
            sdp: SdpPayload {
                kind: "offer".to_string(),
                sdp: "v=0\r\n".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"offer","sdp":{"type":"offer","sdp":"v=0\r\n"}}"#
        );
    }

    #[test]
    fn ice_candidate_camelcase_fields() {
        let msg = ClientMessage::IceCandidate {
            candidate: Some(IceCandidateInit {
                candidate: "candidate:1".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
                username_fragment: Some("frag".to_string()),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        // camelCase on the wire — matches browser's RTCIceCandidateInit.
        assert!(json.contains(r#""sdpMid":"0""#), "{json}");
        assert!(json.contains(r#""sdpMLineIndex":0"#), "{json}");
        assert!(json.contains(r#""usernameFragment":"frag""#), "{json}");
    }

    #[test]
    fn ice_candidate_null_is_end_of_candidates() {
        let msg = ClientMessage::IceCandidate { candidate: None };
        let json = serde_json::to_string(&msg).unwrap();
        // When no candidate, the field is elided — matches our server's
        // relay semantics (server just forwards the JSON).
        assert_eq!(json, r#"{"type":"ice-candidate"}"#);
    }

    // Inbound parsing — matches server/server.js wire format.

    #[test]
    fn parses_session_created() {
        let sm: ServerMessage =
            serde_json::from_str(r#"{"type":"session-created","pin":"123456"}"#).unwrap();
        match sm {
            ServerMessage::SessionCreated { pin } => assert_eq!(pin, "123456"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_server_error_with_error_field() {
        // The Node server sends `{type:"error", error:"..."}`.
        let sm: ServerMessage =
            serde_json::from_str(r#"{"type":"error","error":"invalid_pin"}"#).unwrap();
        match sm {
            ServerMessage::Error { error } => assert_eq!(error, "invalid_pin"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_viewer_joined_and_peer_disconnected_bare() {
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"viewer-joined"}"#).unwrap(),
            ServerMessage::ViewerJoined
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"peer-disconnected"}"#).unwrap(),
            ServerMessage::PeerDisconnected
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"session-expired"}"#).unwrap(),
            ServerMessage::SessionExpired
        ));
    }

    #[test]
    fn parses_answer_with_nested_sdp() {
        let sm: ServerMessage =
            serde_json::from_str(r#"{"type":"answer","sdp":{"type":"answer","sdp":"v=0\r\n"}}"#)
                .unwrap();
        match sm {
            ServerMessage::Answer { sdp } => {
                assert_eq!(sdp.kind, "answer");
                assert!(sdp.sdp.starts_with("v=0"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_ice_candidate_with_browser_camelcase() {
        let sm: ServerMessage = serde_json::from_str(
            r#"{"type":"ice-candidate","candidate":{"candidate":"candidate:foo","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"u"}}"#,
        )
        .unwrap();
        match sm {
            ServerMessage::IceCandidate { candidate: Some(c) } => {
                assert_eq!(c.candidate, "candidate:foo");
                assert_eq!(c.sdp_mid.as_deref(), Some("0"));
                assert_eq!(c.sdp_mline_index, Some(0));
                assert_eq!(c.username_fragment.as_deref(), Some("u"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_ice_candidate_null() {
        let sm: ServerMessage =
            serde_json::from_str(r#"{"type":"ice-candidate","candidate":null}"#).unwrap();
        assert!(matches!(
            sm,
            ServerMessage::IceCandidate { candidate: None }
        ));
    }
}
