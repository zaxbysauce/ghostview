#![allow(dead_code)]
//! VP9 software encoder.
//!
//! Thin wrapper around [`vpx_encode::Encoder`]. Input is BGRA (as produced by
//! the Windows Graphics Capture API); we convert to I420 and feed it to libvpx
//! one frame at a time. Output is the raw VP9 bitstream for the frame —
//! suitable for [`webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample::write_sample`],
//! which handles RTP packetization.

use vpx_encode::{Config, Encoder, VideoCodecId};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("codec init failed: {0}")]
    Init(String),
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("invalid frame: expected {expected} bytes, got {actual}")]
    InvalidFrame { expected: usize, actual: usize },
}

pub struct Vp9Encoder {
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    inner: Encoder,
    /// Reusable I420 scratch buffer, sized to the configured frame dimensions.
    i420_buf: Vec<u8>,
}

impl Vp9Encoder {
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, CodecError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(CodecError::Init(format!(
                "width/height must be non-zero and even: got {width}x{height}"
            )));
        }

        let config = Config {
            width,
            height,
            timebase: [1, 1000], // milliseconds
            bitrate: bitrate_kbps,
            codec: VideoCodecId::VP9,
        };
        let inner = Encoder::new(config).map_err(|e| CodecError::Init(format!("{e:?}")))?;

        let i420_len = i420_buffer_len(width as usize, height as usize);

        Ok(Self {
            width,
            height,
            bitrate_kbps,
            inner,
            i420_buf: vec![0u8; i420_len],
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Encode one BGRA frame. Returns the concatenated VP9 bitstream bytes
    /// emitted for this input (usually one packet, occasionally zero while
    /// libvpx buffers, occasionally more than one at resolution changes).
    pub fn encode(&mut self, bgra_frame: &[u8], pts_ms: u64) -> Result<Vec<u8>, CodecError> {
        let w = self.width as usize;
        let h = self.height as usize;
        let expected = w * h * 4;
        if bgra_frame.len() < expected {
            return Err(CodecError::InvalidFrame {
                expected,
                actual: bgra_frame.len(),
            });
        }

        bgra_to_i420_into(bgra_frame, w, h, &mut self.i420_buf);

        let packets = self
            .inner
            .encode(pts_ms as i64, &self.i420_buf)
            .map_err(|e| CodecError::Encode(format!("{e:?}")))?;

        // Concatenate packet payloads into one buffer. For VP9 each packet is
        // already a complete encoded frame; concatenating is a no-op when only
        // one packet is emitted (the common case).
        let mut out = Vec::new();
        for pkt in packets {
            out.extend_from_slice(&pkt.data);
        }
        Ok(out)
    }

    /// Flush any buffered frames at end-of-stream.
    pub fn finish(self) -> Result<Vec<u8>, CodecError> {
        let mut fin = self
            .inner
            .finish()
            .map_err(|e| CodecError::Encode(format!("{e:?}")))?;
        let mut out = Vec::new();
        // `Finish::next()` -> Result<Option<Frame>>; drain until None.
        loop {
            match fin.next() {
                Ok(Some(frame)) => out.extend_from_slice(&frame.data),
                Ok(None) => break,
                Err(e) => return Err(CodecError::Encode(format!("{e:?}"))),
            }
        }
        Ok(out)
    }
}

/// I420 (a.k.a. YUV 4:2:0 planar) buffer length for `w x h`.
fn i420_buffer_len(w: usize, h: usize) -> usize {
    // Y plane: w * h, then U and V each at half resolution in both dimensions.
    w * h + 2 * ((w / 2) * (h / 2))
}

/// BGRA → I420 (BT.601 studio-range). Writes into `out`, which must be at least
/// `i420_buffer_len(w, h)` bytes long.
fn bgra_to_i420_into(bgra: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    debug_assert!(out.len() >= y_size + 2 * uv_size);

    let (y_plane, uv) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);

    for j in 0..h {
        for i in 0..w {
            let idx = (j * w + i) * 4;
            let b = bgra[idx] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;
            // BT.601 studio-range coefficients, integer-math form used by libyuv.
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[j * w + i] = y.clamp(0, 255) as u8;

            // 4:2:0 subsampling — write one U/V per 2x2 block.
            if j % 2 == 0 && i % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (j / 2) * (w / 2) + (i / 2);
                u_plane[uv_idx] = u.clamp(0, 255) as u8;
                v_plane[uv_idx] = v.clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_len_matches_formula() {
        assert_eq!(i420_buffer_len(1920, 1080), 1920 * 1080 * 3 / 2);
        assert_eq!(i420_buffer_len(320, 240), 320 * 240 * 3 / 2);
    }

    #[test]
    fn rejects_odd_dimensions() {
        assert!(Vp9Encoder::new(1921, 1080, 2000).is_err());
        assert!(Vp9Encoder::new(1920, 1081, 2000).is_err());
        assert!(Vp9Encoder::new(0, 1080, 2000).is_err());
    }

    #[test]
    fn rejects_short_frame() {
        let mut enc = Vp9Encoder::new(320, 240, 500).expect("encoder");
        let result = enc.encode(&vec![0u8; 10], 0);
        assert!(matches!(result, Err(CodecError::InvalidFrame { .. })));
    }
}
