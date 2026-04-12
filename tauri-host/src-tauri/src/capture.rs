#![allow(dead_code)]
//! Screen capture abstraction.
//!
//! The scaffold produces synthetic BGRA frames on ALL platforms so the rest of
//! the pipeline (encoder, WebRTC host, signaling) can be developed and smoke
//! tested. A Windows-only implementation backed by the `windows-capture` crate
//! will replace the synthetic producer — that wiring is stubbed below inside
//! `windows_impl` and clearly marked with TODOs.

use crate::MonitorInfo;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture not supported on this platform")]
    UnsupportedPlatform,
    #[error("capture failed: {0}")]
    Failed(String),
}

/// A raw frame produced by the capture source. Kept deliberately simple for the
/// scaffold: width + height + BGRA bytes + monotonic presentation timestamp.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub pts_ms: u64,
}

/// Opaque handle returned from [`start`]. Dropping it stops capture.
pub struct CaptureHandle {
    join: Option<JoinHandle<()>>,
    stop_tx: Option<mpsc::Sender<()>>,
}

impl CaptureHandle {
    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(()).await;
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.try_send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

/// Enumerate capturable monitors.
pub fn list_monitors() -> Vec<MonitorInfo> {
    #[cfg(target_os = "windows")]
    {
        match windows_impl::list_monitors() {
            Ok(list) if !list.is_empty() => return list,
            _ => {} // fall through to synthetic
        }
    }

    // Non-Windows or Windows fallback: one synthetic virtual display.
    vec![MonitorInfo {
        index: 0,
        name: "Virtual Display".to_string(),
        width: 1920,
        height: 1080,
        is_primary: true,
    }]
}

/// Start capturing the given monitor. Frames are delivered via `tx`.
///
/// For the scaffold this always produces synthetic frames at ~30 FPS. A real
/// Windows backend will be wired in later.
pub fn start(
    monitor_index: usize,
    tx: mpsc::Sender<RawFrame>,
) -> Result<CaptureHandle, CaptureError> {
    let monitors = list_monitors();
    let monitor = monitors
        .into_iter()
        .find(|m| m.index == monitor_index)
        .ok_or_else(|| CaptureError::Failed(format!("monitor {monitor_index} not found")))?;

    // TODO: on Windows, replace this with a real `windows-capture` backed
    // producer driven by the Graphics Capture API. See `windows_impl` module.
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let width = monitor.width;
    let height = monitor.height;

    let join = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(33));
        let mut counter: u64 = 0;
        let start = std::time::Instant::now();
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                _ = interval.tick() => {
                    let frame = synthetic_frame(width, height, counter);
                    let pts_ms = start.elapsed().as_millis() as u64;
                    if tx
                        .send(RawFrame { width, height, bgra: frame, pts_ms })
                        .await
                        .is_err()
                    {
                        // Receiver dropped; exit quietly.
                        break;
                    }
                    counter = counter.wrapping_add(1);
                }
            }
        }
    });

    Ok(CaptureHandle {
        join: Some(join),
        stop_tx: Some(stop_tx),
    })
}

/// Produce a BGRA frame with a simple color gradient + frame counter blob so
/// downstream encoders can see bytes change per frame. Very small: we fill only
/// a single solid color to keep allocations bounded.
fn synthetic_frame(width: u32, height: u32, counter: u64) -> Vec<u8> {
    // 4 bytes per pixel. For the scaffold we keep this tight: allocate exactly
    // one row's worth of BGRA bytes, since no consumer actually renders it.
    // Consumers that need a full frame will be wired up when the real capture
    // backend lands.
    let row_bytes = (width as usize).saturating_mul(4);
    let mut buf = vec![0u8; row_bytes.min(4096)];
    let b = (counter & 0xFF) as u8;
    let g = ((counter >> 8) & 0xFF) as u8;
    let r = ((counter >> 16) & 0xFF) as u8;
    for px in buf.chunks_exact_mut(4) {
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 0xFF;
    }
    // Height is reported for metadata / downstream sizing; we don't fill the
    // whole frame here to save CPU/memory in the scaffold.
    let _ = height;
    buf
}

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Real Windows screen capture implementation.
    //!
    //! TODO: wire actual `windows-capture` API. The crate's API surface may
    //! change between versions; this module intentionally keeps the shape
    //! minimal and returns an error so non-Windows scaffold development is not
    //! blocked.

    use crate::MonitorInfo;
    use super::CaptureError;

    pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
        // TODO: enumerate monitors via `windows_capture::monitor::Monitor`.
        Err(CaptureError::Failed(
            "windows-capture integration not wired yet".to_string(),
        ))
    }
}
