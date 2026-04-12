#![allow(dead_code)]
//! VP9 encoder wrapper.
//!
//! With the `vpx` feature enabled, this wraps the `vpx-encode` crate for real
//! software VP9 encoding. Without the feature (the default for the scaffold),
//! this is a pass-through encoder that simply returns the raw BGRA bytes
//! wrapped with a short scaffold header, so downstream pipeline code can be
//! wired and exercised without pulling in libvpx.

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("codec init failed: {0}")]
    Init(String),
    #[error("encode failed: {0}")]
    Encode(String),
}

/// VP9 encoder wrapper.
///
/// The scaffold implementation (feature `vpx` OFF) does not actually encode:
/// it just prefixes the raw frame bytes with a small magic header that
/// includes width/height/pts so consumers can distinguish scaffold payloads
/// from real VP9 keyframes during development.
pub struct Vp9Encoder {
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    #[cfg(feature = "vpx")]
    inner: vpx_encode::Encoder,
}

impl Vp9Encoder {
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, CodecError> {
        #[cfg(feature = "vpx")]
        {
            let config = vpx_encode::Config {
                width,
                height,
                timebase: [1, 1000],
                bitrate: bitrate_kbps,
                codec: vpx_encode::VideoCodecId::VP9,
            };
            let inner = vpx_encode::Encoder::new(config)
                .map_err(|e| CodecError::Init(format!("{e:?}")))?;
            return Ok(Self {
                width,
                height,
                bitrate_kbps,
                inner,
            });
        }

        #[cfg(not(feature = "vpx"))]
        {
            tracing::warn!(
                "Vp9Encoder: `vpx` feature disabled — returning pass-through scaffold encoder"
            );
            Ok(Self {
                width,
                height,
                bitrate_kbps,
            })
        }
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

    /// Encode a single BGRA frame. The scaffold wraps raw bytes with a tiny
    /// header `[b"GVSCAF", width:u32, height:u32, pts:u64]` and returns them
    /// as "encoded" output. The real encoder (feature `vpx`) converts BGRA to
    /// I420 and feeds it into libvpx.
    pub fn encode(&mut self, bgra_frame: &[u8], pts_ms: u64) -> Result<Vec<u8>, CodecError> {
        #[cfg(feature = "vpx")]
        {
            // Convert BGRA to I420 for VP9.
            let yuv = bgra_to_i420(bgra_frame, self.width, self.height)
                .ok_or_else(|| CodecError::Encode("BGRA->I420 conversion failed".into()))?;
            let packets = self
                .inner
                .encode(pts_ms as i64, &yuv)
                .map_err(|e| CodecError::Encode(format!("{e:?}")))?;
            let mut out = Vec::new();
            for pkt in packets {
                out.extend_from_slice(&pkt.data);
            }
            return Ok(out);
        }

        #[cfg(not(feature = "vpx"))]
        {
            let mut out = Vec::with_capacity(bgra_frame.len() + 24);
            out.extend_from_slice(b"GVSCAF");
            out.extend_from_slice(&self.width.to_le_bytes());
            out.extend_from_slice(&self.height.to_le_bytes());
            out.extend_from_slice(&pts_ms.to_le_bytes());
            out.extend_from_slice(bgra_frame);
            Ok(out)
        }
    }
}

#[cfg(feature = "vpx")]
fn bgra_to_i420(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if bgra.len() < w * h * 4 {
        return None;
    }
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];
    let (y_plane, uv) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);

    for j in 0..h {
        for i in 0..w {
            let idx = (j * w + i) * 4;
            let b = bgra[idx] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[j * w + i] = y.clamp(0, 255) as u8;
            if j % 2 == 0 && i % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (j / 2) * (w / 2) + (i / 2);
                u_plane[uv_idx] = u.clamp(0, 255) as u8;
                v_plane[uv_idx] = v.clamp(0, 255) as u8;
            }
        }
    }
    Some(out)
}
