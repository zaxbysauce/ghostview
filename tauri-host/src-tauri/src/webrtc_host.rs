//! WebRTC peer-connection host.
//!
//! Wraps a single [`RTCPeerConnection`] with a pre-attached VP9 video track.
//! The host only sends video — no data channel or audio on Phase 1. The
//! encoder pumps samples via [`WebRtcHost::push_frame`]; SDP offer/answer and
//! ICE candidates flow through the signaling layer.
//!
//! The peer connection emits local ICE candidates through an mpsc stream that
//! callers can drain and forward to the signaling server. Likewise it emits
//! connection-state changes so the session supervisor can tear down on
//! disconnect / failure.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, Mutex};

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_VP9};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

#[derive(Debug, thiserror::Error)]
pub enum WebRtcError {
    #[error("webrtc error: {0}")]
    Webrtc(String),
    #[error("invalid state: {0}")]
    State(String),
}

impl From<webrtc::Error> for WebRtcError {
    fn from(e: webrtc::Error) -> Self {
        WebRtcError::Webrtc(e.to_string())
    }
}

/// Our own serializable-friendly snapshot of an ICE candidate to forward to
/// signaling. Mirrors `RTCIceCandidateInit` but owns its strings.
#[derive(Debug, Clone)]
pub struct LocalIceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
    pub username_fragment: Option<String>,
}

/// Connection-state snapshot emitted by [`WebRtcHost::take_state_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

impl From<RTCPeerConnectionState> for ConnState {
    fn from(s: RTCPeerConnectionState) -> Self {
        match s {
            RTCPeerConnectionState::New | RTCPeerConnectionState::Unspecified => ConnState::New,
            RTCPeerConnectionState::Connecting => ConnState::Connecting,
            RTCPeerConnectionState::Connected => ConnState::Connected,
            RTCPeerConnectionState::Disconnected => ConnState::Disconnected,
            RTCPeerConnectionState::Failed => ConnState::Failed,
            RTCPeerConnectionState::Closed => ConnState::Closed,
        }
    }
}

pub struct WebRtcHost {
    pc: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    _rtp_sender: Arc<RTCRtpSender>,
    ice_rx: Mutex<Option<mpsc::Receiver<LocalIceCandidate>>>,
    state_rx: Mutex<Option<mpsc::Receiver<ConnState>>>,
}

impl WebRtcHost {
    /// Create a peer connection with a VP9 video sender, a STUN server, and
    /// channels that relay local ICE candidates and connection-state changes.
    pub async fn new(ice_servers: Vec<RTCIceServer>) -> Result<Self, WebRtcError> {
        let mut media = MediaEngine::default();

        // Mirror webrtc-rs's default video RTCP feedback set. Required for
        // reasonable behavior in Chrome / the in-repo viewer: REMB for
        // bandwidth estimation, NACK/PLI for loss recovery, CCM FIR for
        // forced intra refresh.
        let video_rtcp_feedback = vec![
            RTCPFeedback {
                typ: "goog-remb".to_owned(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
        ];

        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP9.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    // `profile-id=0` matches our 8-bit 4:2:0 VP9 encoder
                    // output and matches webrtc-rs's default VP9 PT 98 entry.
                    sdp_fmtp_line: "profile-id=0".to_owned(),
                    rtcp_feedback: video_rtcp_feedback,
                },
                payload_type: 98,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media)?;

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(config).await?);

        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP9.to_owned(),
                clock_rate: 90_000,
                ..Default::default()
            },
            "video".to_owned(),
            "ghostview".to_owned(),
        ));

        let rtp_sender = pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // ICE candidate channel. Bounded so a flaky network can't balloon
        // memory; signaling consumes promptly.
        let (ice_tx, ice_rx) = mpsc::channel::<LocalIceCandidate>(64);
        pc.on_ice_candidate(Box::new(move |cand| {
            let tx = ice_tx.clone();
            Box::pin(async move {
                let local = match cand {
                    Some(c) => {
                        let init = match c.to_json() {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!(error = %e, "webrtc: ice candidate to_json failed");
                                return;
                            }
                        };
                        LocalIceCandidate {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_mline_index: init.sdp_mline_index,
                            username_fragment: init.username_fragment,
                        }
                    }
                    None => {
                        // End-of-candidates sentinel. Forward it so signaling
                        // can send `{candidate: null}` to the viewer. The
                        // forwarder distinguishes the sentinel by empty
                        // `candidate` string.
                        LocalIceCandidate {
                            candidate: String::new(),
                            sdp_mid: None,
                            sdp_mline_index: None,
                            username_fragment: None,
                        }
                    }
                };
                if tx.send(local).await.is_err() {
                    tracing::debug!("webrtc: ice candidate channel closed");
                }
            })
        }));

        // Connection-state channel.
        let (state_tx, state_rx) = mpsc::channel::<ConnState>(16);
        pc.on_peer_connection_state_change(Box::new(move |s| {
            let tx = state_tx.clone();
            Box::pin(async move {
                let _ = tx.send(s.into()).await;
            })
        }));

        Ok(Self {
            pc,
            video_track,
            _rtp_sender: rtp_sender,
            ice_rx: Mutex::new(Some(ice_rx)),
            state_rx: Mutex::new(Some(state_rx)),
        })
    }

    /// Take the local ICE candidate stream. Single consumer.
    pub async fn take_ice_stream(&self) -> Option<mpsc::Receiver<LocalIceCandidate>> {
        self.ice_rx.lock().await.take()
    }

    /// Take the connection-state stream. Single consumer.
    pub async fn take_state_stream(&self) -> Option<mpsc::Receiver<ConnState>> {
        self.state_rx.lock().await.take()
    }

    /// Create and set our local SDP offer. Returns the SDP string.
    pub async fn create_offer(&self) -> Result<String, WebRtcError> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer.sdp)
    }

    /// Apply the remote SDP answer.
    pub async fn set_answer(&self, sdp: String) -> Result<(), WebRtcError> {
        let answer = RTCSessionDescription::answer(sdp)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Add a remote ICE candidate. `candidate` may be empty (browsers send
    /// empty strings for end-of-candidates instead of `null` when they send
    /// anything at all).
    pub async fn add_ice_candidate(
        &self,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
        username_fragment: Option<String>,
    ) -> Result<(), WebRtcError> {
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        };
        self.pc.add_ice_candidate(init).await?;
        Ok(())
    }

    /// Push an encoded VP9 frame into the outbound RTP track.
    pub async fn push_frame(&self, data: &[u8], duration_ms: u64) -> Result<(), WebRtcError> {
        if data.is_empty() {
            return Ok(()); // libvpx sometimes emits zero-packet results; skip.
        }
        let sample = Sample {
            data: Bytes::copy_from_slice(data),
            duration: Duration::from_millis(duration_ms.max(1)),
            ..Default::default()
        };
        self.video_track.write_sample(&sample).await?;
        Ok(())
    }

    pub async fn close(&self) -> Result<(), WebRtcError> {
        self.pc.close().await?;
        Ok(())
    }
}
