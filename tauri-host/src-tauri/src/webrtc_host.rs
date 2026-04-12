#![allow(dead_code)]
//! WebRTC host wrapper.
//!
//! Named `webrtc_host` to avoid colliding with the external `webrtc` crate.
//! The real WebRTC stack is heavy and is gated behind the `real-webrtc`
//! feature. With the feature off (scaffold default), a stub implementation
//! satisfies the rest of the codebase and reports that the feature must be
//! enabled at build time for real sessions.

#[cfg(not(feature = "real-webrtc"))]
mod stub {
    #[derive(Debug, thiserror::Error)]
    pub enum WebRtcError {
        #[error("webrtc feature not enabled — build with --features real-webrtc")]
        FeatureDisabled,
        #[error("{0}")]
        Other(String),
    }

    /// Stub host. All methods return `FeatureDisabled` so callers can still
    /// compile and be exercised by tests without pulling in the full WebRTC
    /// stack.
    pub struct WebRtcHost;

    impl WebRtcHost {
        pub async fn new() -> Result<Self, WebRtcError> {
            Err(WebRtcError::FeatureDisabled)
        }

        pub async fn create_offer(&self) -> Result<String, WebRtcError> {
            Err(WebRtcError::FeatureDisabled)
        }

        pub async fn set_answer(&self, _sdp: String) -> Result<(), WebRtcError> {
            Err(WebRtcError::FeatureDisabled)
        }

        pub async fn add_ice_candidate(
            &self,
            _candidate: String,
            _sdp_mid: Option<String>,
            _sdp_mline_index: Option<u16>,
        ) -> Result<(), WebRtcError> {
            Err(WebRtcError::FeatureDisabled)
        }

        pub async fn push_frame(
            &self,
            _data: &[u8],
            _duration_ms: u64,
        ) -> Result<(), WebRtcError> {
            Err(WebRtcError::FeatureDisabled)
        }

        pub async fn close(&self) -> Result<(), WebRtcError> {
            Ok(())
        }
    }
}

#[cfg(feature = "real-webrtc")]
mod real {
    //! Realistic webrtc-rs scaffold. This module intentionally keeps the
    //! surface shape identical to `stub` so consumers compile across both
    //! feature configurations. The internals construct a `RTCPeerConnection`
    //! with a VP9 `TrackLocalStaticSample` and expose SDP offer/answer plus a
    //! `push_frame` entrypoint for the encoder.
    //!
    //! Exact API details of `webrtc` 0.11 may need minor refinement when this
    //! feature is first built — any drift should be localised to this file.

    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use webrtc::api::interceptor_registry::register_default_interceptors;
    use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_VP9};
    use webrtc::api::APIBuilder;
    use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
    use webrtc::ice_transport::ice_server::RTCIceServer;
    use webrtc::interceptor::registry::Registry;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
    use webrtc::peer_connection::RTCPeerConnection;
    use webrtc::rtp_transceiver::rtp_codec::{
        RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
    };
    use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
    use webrtc::track::track_local::TrackLocal;
    use webrtc::media::Sample;

    #[derive(Debug, thiserror::Error)]
    pub enum WebRtcError {
        #[error("webrtc error: {0}")]
        Webrtc(String),
        #[error("invalid state: {0}")]
        State(String),
        #[error("{0}")]
        Other(String),
    }

    impl From<webrtc::Error> for WebRtcError {
        fn from(e: webrtc::Error) -> Self {
            WebRtcError::Webrtc(e.to_string())
        }
    }

    pub struct WebRtcHost {
        pc: Arc<RTCPeerConnection>,
        video_track: Arc<TrackLocalStaticSample>,
    }

    impl WebRtcHost {
        pub async fn new() -> Result<Self, WebRtcError> {
            let mut media = MediaEngine::default();
            media.register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_VP9.to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: String::new(),
                        rtcp_feedback: vec![],
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
                ice_servers: vec![RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            };

            let pc = Arc::new(api.new_peer_connection(config).await?);

            let video_track = Arc::new(TrackLocalStaticSample::new(
                RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP9.to_owned(),
                    clock_rate: 90000,
                    ..Default::default()
                },
                "video".to_owned(),
                "ghostview".to_owned(),
            ));

            let _rtp_sender = pc
                .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
                .await?;

            Ok(Self { pc, video_track })
        }

        pub async fn create_offer(&self) -> Result<String, WebRtcError> {
            let offer = self.pc.create_offer(None).await?;
            self.pc.set_local_description(offer.clone()).await?;
            Ok(offer.sdp)
        }

        pub async fn set_answer(&self, sdp: String) -> Result<(), WebRtcError> {
            let answer = RTCSessionDescription::answer(sdp)?;
            self.pc.set_remote_description(answer).await?;
            Ok(())
        }

        pub async fn add_ice_candidate(
            &self,
            candidate: String,
            sdp_mid: Option<String>,
            sdp_mline_index: Option<u16>,
        ) -> Result<(), WebRtcError> {
            let init = RTCIceCandidateInit {
                candidate,
                sdp_mid,
                sdp_mline_index,
                username_fragment: None,
            };
            self.pc.add_ice_candidate(init).await?;
            Ok(())
        }

        pub async fn push_frame(
            &self,
            data: &[u8],
            duration_ms: u64,
        ) -> Result<(), WebRtcError> {
            let sample = Sample {
                data: Bytes::copy_from_slice(data),
                duration: Duration::from_millis(duration_ms),
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
}

#[cfg(not(feature = "real-webrtc"))]
pub use stub::*;

#[cfg(feature = "real-webrtc")]
pub use real::*;
