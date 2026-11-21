//! Video-plane plumbing + frame encoding (Plan.Md §5 M4A/M4B).
//!
//! The Wayland capture loop itself (screencopy + input bridge + output
//! management + clipboard) lives in [`crate::wayland_bridge`]; this module
//! owns the AF_VSOCK video server (port 50002), the `b"WFRM"` frame encoder,
//! and the renderer-detection helper.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::protocol::{FrameCompression, PixelFormat, VideoFrameHeader, VIDEO_FRAME_HEADER_LEN};
use lz4_flex::compress_prepend_size;
use wayland_client::protocol::wl_shm::Format as WlShmFormat;

use crate::wayland_bridge::{self, SessionChannels};

/// Video-plane VSOCK port (guest → host only).
pub const VIDEO_PORT: u32 = 50002;

/// `--once` mode gives up after this long without a client connecting, so an
/// abandoned `--autostart` daemon cannot hold AF_VSOCK port 50001 forever.
pub const ONCE_ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Linux `EADDRINUSE` (98): another daemon already owns the port.
pub const EADDRINUSE: i32 = 98;

// ---------------------------------------------------------------------------
// Renderer detection
// ---------------------------------------------------------------------------

/// Detect the active wlroots renderer from the compositor log (wlroots logs
/// renderer creation lines tagged `render/<backend>`).
///
/// When EGL/GBM cannot serve buffer allocation on WSL2's dxg render node,
/// wlroots silently falls back to software pixman — we surface that
/// explicitly instead of reporting `unknown`, since it is THE
/// perf-critical fact for Milestone 4 streaming.
pub fn detect_renderer(compositor_log: &PathBuf) -> String {
    let Ok(text) = std::fs::read_to_string(compositor_log) else {
        return "unknown".into();
    };
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("render/gles2") {
            return "gles2".into();
        }
        if lower.contains("render/vulkan") {
            return "vulkan".into();
        }
        if lower.contains("render/pixman") {
            return "pixman".into();
        }
        // wlroots logs the EGL failure as an ERROR before the pixman
        // fallback; the fallback line itself may only appear at debug level.
        if lower.contains("failed to open") && lower.contains("renderd128") {
            return "pixman (renderD128 permission denied — add user to 'render' group)".into();
        }
        if lower.contains("d3d12") && lower.contains("renderer") {
            return "d3d12".into();
        }
    }
    "unknown".into()
}

// ---------------------------------------------------------------------------
// Video server (VMBus 50002 → capture loop)
// ---------------------------------------------------------------------------

/// Handle for the per-session video channel: cancellable thread running the
/// unified session loop (capture + input + output + clipboard).
pub struct VideoChannel {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoChannel {
    /// Bind AF_VSOCK 50002, wait for the host video connection, then run the
    /// unified session loop until [`VideoChannel::stop`] or a stream error.
    pub fn start(socket_path: PathBuf, channels: SessionChannels, width: u32, height: u32, scale_milli: u32) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let handle = std::thread::Builder::new()
            .name("wsldesk-video".into())
            .spawn(move || {
                video_server_main(socket_path, channels, width, height, scale_milli, flag)
            })
            .ok();
        Self { running, handle }
    }

    /// Signal the session loop to exit (takes effect within one frame).
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Join the video thread (blocks ≤ one frame interval + write timeout).
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn video_server_main(
    socket_path: PathBuf,
    channels: SessionChannels,
    width: u32,
    height: u32,
    scale_milli: u32,
    running: Arc<AtomicBool>,
) {
    let addr = vsock::VsockAddr::new(vsock::VMADDR_CID_ANY, VIDEO_PORT);
    let listener = match vsock::VsockListener::bind(&addr) {
        Ok(l) => l,
        Err(err) if err.raw_os_error() == Some(EADDRINUSE) => {
            eprintln!(
                "wsldesk-guest video: port {VIDEO_PORT} already in use — another video \
                 channel owns it (EADDRINUSE)"
            );
            return;
        }
        Err(err) => {
            eprintln!("wsldesk-guest video: bind AF_VSOCK {VIDEO_PORT} failed: {err}");
            return;
        }
    };
    eprintln!("wsldesk-guest video: listening on AF_VSOCK port {VIDEO_PORT}");

    // Non-blocking accept so StopSession can cancel the wait.
    use std::os::fd::AsRawFd as _;
    use std::os::fd::BorrowedFd;
    let raw = listener.as_raw_fd();
    // SAFETY: `raw` is the listener fd owned by `listener`, alive for the
    // whole nonblocking-accept loop below.
    if let Err(err) = nix::fcntl::fcntl(
        unsafe { BorrowedFd::borrow_raw(raw) },
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    ) {
        eprintln!("wsldesk-guest video: cannot set O_NONBLOCK on listener: {err}");
        return;
    }

    let deadline = std::time::Instant::now() + ONCE_ACCEPT_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                eprintln!("wsldesk-guest video: host connected cid={}", peer.cid());
                break stream;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if !running.load(Ordering::SeqCst) {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!(
                        "wsldesk-guest video: no host connection within {}s; exiting",
                        ONCE_ACCEPT_TIMEOUT.as_secs()
                    );
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(err) => {
                eprintln!("wsldesk-guest video: accept failed: {err}");
                return;
            }
        }
    };

    // Bounded blocking writes so a dead host connection surfaces as an error.
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));

    if let Err(err) = wayland_bridge::run_session_loop(
        socket_path.clone(),
        &stream,
        &running,
        channels,
        width,
        height,
        scale_milli,
    ) {
        if running.load(Ordering::SeqCst) {
            eprintln!("wsldesk-guest video: capture ended: {err:#}");
        }
        // NEVER debug blind: dump the compositor log tail (wlroots/libxkbcommon
        // print the exact failure — keymap syntax, allocator errors, protocol
        // rejections — into this log before the compositor is torn down).
        let log_path = socket_path.with_file_name("compositor.log");
        if let Ok(text) = std::fs::read_to_string(&log_path) {
            let tail: Vec<&str> = text.lines().rev().take(25).collect();
            eprintln!("=== compositor.log tail (last 25 lines) ===");
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
            eprintln!("=== end compositor.log tail ===");
        }
    }
    eprintln!("wsldesk-guest video: capture loop finished");
}

// ---------------------------------------------------------------------------
// Frame encoding
// ---------------------------------------------------------------------------

/// Map a wl_shm format to the wire `PixelFormat`; bails on anything else.
pub fn map_pixel_format(format: WlShmFormat) -> anyhow::Result<PixelFormat> {
    match format {
        WlShmFormat::Xrgb8888 | WlShmFormat::Argb8888 => Ok(PixelFormat::Bgra8888),
        WlShmFormat::Xbgr8888 | WlShmFormat::Abgr8888 => Ok(PixelFormat::Rgba8888),
        other => anyhow::bail!(
            "compositor advertised unsupported shm format {other:?}; expected \
             XRGB8888/ARGB8888 (BGRA) or XBGR8888/ABGR8888 (RGBA)"
        ),
    }
}


/// Encode one frame: fixed 32-byte `b"WFRM"` header + LZ4 payload.
pub fn encode_video_frame(
    frame_id: u64,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    packed: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let compressed = compress_prepend_size(packed);
    let header = VideoFrameHeader {
        frame_id,
        width,
        height,
        stride: width * 4,
        pixel_format,
        compression: FrameCompression::Lz4,
        rect_count: 0, // full frame (dirty-tile diffs are a later optimization)
        payload_len: compressed.len() as u32,
    };
    let mut out = Vec::with_capacity(VIDEO_FRAME_HEADER_LEN + compressed.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_detection_from_log_text() {
        let dir = std::env::temp_dir().join(format!("wsldesk-renderer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("compositor.log");

        std::fs::write(&log, "00:00:00.010 [render/gles2] Creating EGL renderer\n").unwrap();
        assert_eq!(detect_renderer(&log), "gles2");

        std::fs::write(&log, "00:00:00.010 [render/pixman] Using pixman\n").unwrap();
        assert_eq!(detect_renderer(&log), "pixman");

        std::fs::write(
            &log,
            "00:00:00.010 [ERROR] [wlr] Failed to open '/dev/dri/renderD128': Permission denied\n",
        )
        .unwrap();
        let detected = detect_renderer(&log);
        assert!(detected.starts_with("pixman"), "got {detected}");

        std::fs::write(&log, "nothing here\n").unwrap();
        assert_eq!(detect_renderer(&log), "unknown");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encoded_video_frame_header_layout() {
        let packed = vec![0x42u8; 64];
        let bytes = encode_video_frame(7, 4, 4, PixelFormat::Bgra8888, &packed).unwrap();
        let header = VideoFrameHeader::from_bytes(&bytes[..32]).unwrap();
        assert_eq!(header.frame_id, 7);
        assert_eq!(header.width, 4);
        assert_eq!(header.height, 4);
        assert_eq!(header.stride, 16);
        assert_eq!(header.pixel_format, PixelFormat::Bgra8888);
        assert_eq!(header.compression, FrameCompression::Lz4);
        assert_eq!(header.rect_count, 0);
        assert_eq!(header.payload_len as usize, bytes.len() - 32);
    }

    #[test]
    fn format_mapping_covers_wlroots_defaults() {
        assert_eq!(map_pixel_format(WlShmFormat::Xrgb8888).unwrap(), PixelFormat::Bgra8888);
        assert_eq!(map_pixel_format(WlShmFormat::Argb8888).unwrap(), PixelFormat::Bgra8888);
        assert_eq!(map_pixel_format(WlShmFormat::Xbgr8888).unwrap(), PixelFormat::Rgba8888);
        assert_eq!(map_pixel_format(WlShmFormat::Abgr8888).unwrap(), PixelFormat::Rgba8888);
        assert!(map_pixel_format(WlShmFormat::Rgb565).is_err());
    }
}
