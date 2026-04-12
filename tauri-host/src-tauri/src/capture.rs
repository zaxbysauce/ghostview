#![allow(dead_code)]
//! Screen capture.
//!
//! Phase 1 target: Windows 10/11 via the `windows-capture` crate (Windows
//! Graphics Capture API). On non-Windows platforms `start()` / `list_monitors()`
//! report [`CaptureError::UnsupportedPlatform`] — that is by design; GhostView
//! Pro ships Windows-only for Phase 1 (macOS/Linux planned).
//!
//! Frames are delivered via an mpsc channel as [`RawFrame`] with BGRA bytes.
//! Encoder and WebRTC layers consume the channel.

use crate::MonitorInfo;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture is not supported on this platform (Windows-only)")]
    UnsupportedPlatform,
    #[error("capture failed: {0}")]
    Failed(String),
    #[error("monitor {0} not found")]
    MonitorNotFound(usize),
}

/// Raw captured frame. `bgra` layout is top-down, stride = width*4.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub pts_ms: u64,
}

/// Handle returned by [`start`]. Dropping stops capture; `stop().await` is the
/// polite tear-down that waits for the producer thread to exit.
pub struct CaptureHandle {
    #[cfg(target_os = "windows")]
    inner: Option<windows_impl::Handle>,
    #[cfg(not(target_os = "windows"))]
    _phantom: std::marker::PhantomData<()>,
}

impl CaptureHandle {
    pub async fn stop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if let Some(handle) = self.inner.take() {
                handle.stop().await;
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // nothing to stop — start() couldn't succeed on this platform
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if let Some(handle) = self.inner.take() {
                handle.abort();
            }
        }
    }
}

/// Enumerate capturable monitors.
pub fn list_monitors() -> Vec<MonitorInfo> {
    #[cfg(target_os = "windows")]
    {
        match windows_impl::list_monitors() {
            Ok(list) => return list,
            Err(e) => {
                tracing::warn!(error = %e, "capture: monitor enumeration failed");
                return vec![];
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![]
    }
}

/// Start capturing `monitor_index`. Frames are delivered on `tx` until the
/// returned [`CaptureHandle`] is dropped or `stop().await` is called.
pub fn start(
    monitor_index: usize,
    tx: mpsc::Sender<RawFrame>,
) -> Result<CaptureHandle, CaptureError> {
    #[cfg(target_os = "windows")]
    {
        let inner = windows_impl::start(monitor_index, tx)?;
        Ok(CaptureHandle { inner: Some(inner) })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (monitor_index, tx);
        Err(CaptureError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Real Windows screen capture via the `windows-capture` crate.
    //!
    //! The crate exposes a `GraphicsCaptureApiHandler` trait. Frames arrive on
    //! a dedicated capture thread; we copy the BGRA buffer (the underlying
    //! frame memory is owned by the Graphics Capture API and must not outlive
    //! the callback) and forward it over an mpsc channel.

    use super::{CaptureError, RawFrame};
    use crate::MonitorInfo;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::mpsc;
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings,
    };

    /// State passed into the capture thread.
    struct Flags {
        tx: mpsc::Sender<RawFrame>,
        start: Instant,
        stopping: Arc<AtomicBool>,
    }

    /// Handler implementing the capture callbacks.
    struct Handler {
        tx: mpsc::Sender<RawFrame>,
        start: Instant,
        stopping: Arc<AtomicBool>,
    }

    impl GraphicsCaptureApiHandler for Handler {
        type Flags = Flags;
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            let Flags { tx, start, stopping } = ctx.flags;
            Ok(Self {
                tx,
                start,
                stopping,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame<'_>,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            if self.stopping.load(Ordering::Relaxed) {
                capture_control.stop();
                return Ok(());
            }

            let width = frame.width();
            let height = frame.height();
            // Grab a BGRA buffer. `buffer()` returns a wrapper whose bytes
            // live only for the frame; copy immediately.
            let mut buf = frame.buffer()?;
            let bgra = buf.as_raw_buffer().to_vec();
            let pts_ms = self.start.elapsed().as_millis() as u64;

            // Non-blocking send: if the consumer is slow, drop rather than
            // blocking the capture thread (WGC is fragile about reentrancy).
            let raw = RawFrame {
                width,
                height,
                bgra,
                pts_ms,
            };
            match self.tx.try_send(raw) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("capture: frame dropped (consumer backpressure)");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    capture_control.stop();
                }
            }
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            tracing::info!("capture: session closed by OS");
            Ok(())
        }
    }

    pub struct Handle {
        stopping: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Handle {
        pub async fn stop(mut self) {
            self.stopping.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                // Join on a blocking thread so we don't stall the tokio runtime.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = t.join();
                })
                .await;
            }
        }

        pub fn abort(mut self) {
            self.stopping.store(true, Ordering::Relaxed);
            // Let the capture thread notice the flag on its next frame and exit.
            drop(self.thread.take());
        }
    }

    pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
        let monitors = Monitor::enumerate()
            .map_err(|e| CaptureError::Failed(format!("monitor enumerate: {e:?}")))?;
        let primary = Monitor::primary().ok();

        let mut out = Vec::with_capacity(monitors.len());
        for (idx, m) in monitors.into_iter().enumerate() {
            let width = m.width().unwrap_or(0);
            let height = m.height().unwrap_or(0);
            let name = m
                .device_name()
                .unwrap_or_else(|_| format!("Monitor {idx}"));
            let is_primary = primary
                .as_ref()
                .map(|p| p.index().ok() == m.index().ok())
                .unwrap_or(false);
            out.push(MonitorInfo {
                index: idx,
                name,
                width,
                height,
                is_primary,
            });
        }
        Ok(out)
    }

    pub fn start(
        monitor_index: usize,
        tx: mpsc::Sender<RawFrame>,
    ) -> Result<Handle, CaptureError> {
        let monitors = Monitor::enumerate()
            .map_err(|e| CaptureError::Failed(format!("monitor enumerate: {e:?}")))?;
        let monitor = monitors
            .into_iter()
            .nth(monitor_index)
            .ok_or(CaptureError::MonitorNotFound(monitor_index))?;

        let stopping = Arc::new(AtomicBool::new(false));
        let flags = Flags {
            tx,
            start: Instant::now(),
            stopping: Arc::clone(&stopping),
        };

        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            ColorFormat::Bgra8,
            flags,
        );

        // `windows-capture` runs the capture loop on the calling thread when
        // `Handler::start` is invoked. Move it to a dedicated OS thread so the
        // tokio runtime isn't blocked.
        let thread = std::thread::Builder::new()
            .name("ghostview-capture".into())
            .spawn(move || {
                if let Err(e) = Handler::start(settings) {
                    tracing::error!(error = ?e, "capture: session ended with error");
                } else {
                    tracing::info!("capture: session ended cleanly");
                }
            })
            .map_err(|e| CaptureError::Failed(format!("spawn: {e}")))?;

        Ok(Handle {
            stopping,
            thread: Some(thread),
        })
    }
}
