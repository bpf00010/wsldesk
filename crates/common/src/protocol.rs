//! WSLDesk wire protocol (Plan.Md §4.1).
//!
//! Two dedicated VSOCK ports:
//! - [`CONTROL_PORT`] (50001): bidirectional framed control plane, payloads are
//!   postcard-encoded [`ControlMessage`] values wrapped in `b"WSLD"` frames.
//! - [`VIDEO_PORT`] (50002): unidirectional guest→host frame stream; fixed
//!   [`VideoFrameHeader`] (`b"WFRM"`) followed by raw pixel bytes.
//!
//! Zero-copy rule: raw pixel buffers are NEVER serialized through
//! serde/postcard — only small control messages and fixed headers.

use serde::{Deserialize, Serialize};

use crate::ProbeReport;

/// Wire protocol version negotiated in `Hello`/`Welcome`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Control-plane VSOCK/Hyper-V service port.
pub const CONTROL_PORT: u32 = 50001;
/// Video-plane VSOCK/Hyper-V service port (guest → host only).
pub const VIDEO_PORT: u32 = 50002;

/// Control frame magic (`b"WSLD"`).
pub const CONTROL_MAGIC: [u8; 4] = *b"WSLD";
/// Video frame magic (`b"WFRM"`).
pub const VIDEO_MAGIC: [u8; 4] = *b"WFRM";

/// Maximum control-frame payload length in bytes (16 MiB).
pub const MAX_CONTROL_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Control frame header length: magic (4) + length (4).
pub const CONTROL_HEADER_LEN: usize = 8;

/// Video frame fixed header length in bytes.
pub const VIDEO_FRAME_HEADER_LEN: usize = 32;

/// Pixel formats carried on the video plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8888 = 0,
    Rgba8888 = 1,
}

/// Compression applied to a video frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameCompression {
    Raw = 0,
    Lz4 = 1,
}

/// Bidirectional control-plane messages. Extended across milestones; keep
/// variants append-only so old peers reject cleanly with `WireError`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Host → guest handshake opener.
    Hello {
        protocol_version: u32,
        host_version: String,
    },
    /// Guest → host handshake reply.
    Welcome {
        protocol_version: u32,
        guest_version: String,
        has_dxg: bool,
        systemd_active: bool,
    },
    /// Host → guest latency probe.
    Ping { seq: u64, timestamp_us: u64 },
    /// Guest → host latency probe echo.
    Pong { seq: u64, timestamp_us: u64 },
    /// Host → guest: enumerate installed Wayland desktop sessions.
    ListSessions,
    /// Guest → host: result of `ListSessions`.
    SessionList(Vec<WaylandSessionInfo>),
    /// Host → guest: launch a desktop session on the isolated runtime dir.
    StartSession {
        session_id: String,
        width: u32,
        height: u32,
        scale_milli: u32,
        /// When true, strip the WSLg `PULSE_SERVER` variable so the session
        /// has no Linux → Windows audio path (default: preserved).
        no_audio: bool,
    },
    /// Guest → host: session is live; the compositor accepted a Wayland
    /// client socket inside the isolated runtime dir.
    SessionStarted {
        /// `WAYLAND_DISPLAY` socket name, e.g. `wayland-0`.
        wayland_display: String,
        /// Isolated runtime dir backing `XDG_RUNTIME_DIR`.
        runtime_dir: String,
        /// Compositor process id (inside the guest).
        pid: u32,
        compositor_family: CompositorFamily,
        /// True when unshare(CLONE_NEWUSER|CLONE_NEWNS) + private tmpfs
        /// isolation succeeded; false = env-only degradation.
        namespaced: bool,
        /// Active wlroots renderer detected from compositor.log
        /// (`gles2`, `vulkan`, `pixman`, `d3d12`, `unknown`).
        renderer: String,
    },
    /// Host → guest: request clean teardown of a running session
    /// (`session_id` empty = stop whatever is active).
    StopSession { session_id: String },
    /// Guest → host: teardown result (compositor exit status + runtime-dir
    /// cleanup verification).
    SessionStopped {
        session_id: String,
        /// True when the compositor exited on request and the isolated
        /// runtime dir was fully removed (zero orphans, no leaked socket).
        clean_exit: bool,
        detail: String,
    },
    /// Host → guest: reconfigure the headless output mode.
    Resize {
        width: u32,
        height: u32,
        scale_milli: u32,
    },
    /// Host → guest: inject keyboard/pointer input (Milestone 4).
    InputEvent(InputPayload),
    /// Bidirectional clipboard sync (Milestone 4).
    ClipboardUpdate { mime_type: String, data: Vec<u8> },
    /// Either side: typed failure with a human-readable message.
    Error { code: u32, message: String },
}

/// Input event payload carried by [`ControlMessage::InputEvent`].
///
/// Keycodes are Linux evdev codes (see `common::keymap`, Milestone 4B);
/// buttons are the Linux `BTN_*` range. Pointer position uses normalized
/// absolute coordinates so `zwlr_virtual_pointer_v1::motion_absolute` aligns
/// the cursor 1:1 with the captured output (no relative drift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputPayload {
    /// Evdev keycode transition.
    Key { code: u32, pressed: bool },
    /// Relative pointer motion.
    PointerMotion { dx: i32, dy: i32 },
    /// Absolute pointer position in a `width` × `height` coordinate space
    /// (the captured output size at send time).
    PointerMotionAbsolute {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// Pointer button (`button` is a Linux input button code, e.g. BTN_LEFT =
    /// 0x110; evdev button codes are u16).
    PointerButton { button: u16, pressed: bool },
    /// Wheel/scroll axis; `vertical` selects the axis, `delta` in wheel steps.
    PointerAxis { vertical: bool, delta: i32 },
}

/// Compositor family classification for a Wayland session desktop entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompositorFamily {
    Wlroots,
    Gnome,
    Kde,
    Cosmic,
    Weston,
    Other,
}

impl CompositorFamily {
    /// Short lowercase tag for doctor tables and logs.
    pub fn label(&self) -> &'static str {
        match self {
            CompositorFamily::Wlroots => "wlroots",
            CompositorFamily::Gnome => "gnome",
            CompositorFamily::Kde => "kde",
            CompositorFamily::Cosmic => "cosmic",
            CompositorFamily::Weston => "weston",
            CompositorFamily::Other => "other",
        }
    }
}

/// One entry from `/usr/share/wayland-sessions/*.desktop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaylandSessionInfo {
    /// Desktop-file stem; stable identifier used by `StartSession`.
    pub id: String,
    /// Human-readable session name (`Name=`).
    pub name: String,
    /// Command line (`Exec=`), executed inside the isolated runtime dir.
    pub exec: String,
    /// `DesktopNames=` entries split on `;` (may be empty).
    pub desktop_names: Vec<String>,
    /// Heuristic compositor family (refined in Milestone 3).
    pub family: CompositorFamily,
}

/// Either side of the control plane: typed framing errors.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Need more bytes to complete the current frame.
    #[error("incomplete frame: {0} of {1} header/payload bytes available")]
    Incomplete(usize, usize),
    /// First 4 bytes were not the expected magic.
    #[error("bad frame magic: expected {expected:?}, got {got:?}")]
    BadMagic {
        expected: [u8; 4],
        got: [u8; 4],
    },
    /// Payload exceeds [`MAX_CONTROL_FRAME_LEN`].
    #[error("frame payload too large: {len} bytes (max {max})")]
    PayloadTooLarge { len: usize, max: usize },
    /// postcard (de)serialization failure inside the payload.
    #[error("payload encode/decode failed: {0}")]
    Postcard(#[from] postcard::Error),
    /// Caller passed a buffer shorter than the fixed video header.
    #[error("video header buffer too small: {0} bytes (need {1})")]
    VideoHeaderTooSmall(usize, usize),
}

/// Encode one control frame: `b"WSLD"` magic, u32 LE length, postcard payload.
pub fn encode_frame(msg: &ControlMessage) -> Result<Vec<u8>, WireError> {
    let payload = postcard::to_allocvec(msg)?;
    if payload.len() > MAX_CONTROL_FRAME_LEN as usize {
        return Err(WireError::PayloadTooLarge {
            len: payload.len(),
            max: MAX_CONTROL_FRAME_LEN as usize,
        });
    }
    let mut buf = Vec::with_capacity(CONTROL_HEADER_LEN + payload.len());
    buf.extend_from_slice(&CONTROL_MAGIC);
    let len = payload.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode one control frame from the front of `buf`.
///
/// Returns the message and the exact number of bytes consumed (header +
/// payload). Trailing bytes belong to the next frame and are left alone.
pub fn decode_frame(buf: &[u8]) -> Result<(ControlMessage, usize), WireError> {
    if buf.len() < CONTROL_HEADER_LEN {
        return Err(WireError::Incomplete(buf.len(), CONTROL_HEADER_LEN));
    }
    let got = [buf[0], buf[1], buf[2], buf[3]];
    if got != CONTROL_MAGIC {
        return Err(WireError::BadMagic {
            expected: CONTROL_MAGIC,
            got,
        });
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if len > MAX_CONTROL_FRAME_LEN as usize {
        return Err(WireError::PayloadTooLarge {
            len,
            max: MAX_CONTROL_FRAME_LEN as usize,
        });
    }
    let total = CONTROL_HEADER_LEN + len;
    if buf.len() < total {
        return Err(WireError::Incomplete(buf.len(), total));
    }
    let msg = postcard::from_bytes::<ControlMessage>(&buf[CONTROL_HEADER_LEN..total])?;
    Ok((msg, total))
}

/// Streaming frame reassembler for socket pumps: feed raw bytes, pull out
/// complete messages. On a fatal (non-`Incomplete`) error the internal buffer
/// is dropped so the caller can treat the stream as desynced.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw socket bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to extract the next complete message; `Ok(None)` while more bytes
    /// are needed.
    pub fn next_message(&mut self) -> Result<Option<ControlMessage>, WireError> {
        match decode_frame(&self.buf) {
            Ok((msg, consumed)) => {
                self.buf.drain(..consumed);
                Ok(Some(msg))
            }
            Err(WireError::Incomplete(..)) => Ok(None),
            Err(err) => {
                self.buf.clear();
                Err(err)
            }
        }
    }

    /// Bytes currently buffered but not yet decoded.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Drop all buffered bytes (e.g. after a desync the caller chose to
    /// recover from).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Fixed 32-byte little-endian video frame header (Plan.Md §4.1).
///
/// Followed immediately by `payload_len` raw bytes (LZ4-compressed dirty tiles
/// or a full raw frame). Pixel bytes are written/read directly on the socket —
/// never passed through serde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameHeader {
    pub frame_id: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per row in the pixel payload.
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub compression: FrameCompression,
    /// Number of dirty tile records following the header; 0 = full frame.
    pub rect_count: u16,
    /// Exact byte length of the raw/compressed payload that follows.
    pub payload_len: u32,
}

impl VideoFrameHeader {
    pub const MAGIC: [u8; 4] = VIDEO_MAGIC;

    /// Serialize to exactly [`VIDEO_FRAME_HEADER_LEN`] little-endian bytes.
    pub fn to_bytes(&self) -> [u8; VIDEO_FRAME_HEADER_LEN] {
        let mut out = [0u8; VIDEO_FRAME_HEADER_LEN];
        out[0..4].copy_from_slice(&Self::MAGIC);
        out[4..12].copy_from_slice(&self.frame_id.to_le_bytes());
        out[12..16].copy_from_slice(&self.width.to_le_bytes());
        out[16..20].copy_from_slice(&self.height.to_le_bytes());
        out[20..24].copy_from_slice(&self.stride.to_le_bytes());
        out[24] = match self.pixel_format {
            PixelFormat::Bgra8888 => 0,
            PixelFormat::Rgba8888 => 1,
        };
        out[25] = match self.compression {
            FrameCompression::Raw => 0,
            FrameCompression::Lz4 => 1,
        };
        out[26..28].copy_from_slice(&self.rect_count.to_le_bytes());
        out[28..32].copy_from_slice(&self.payload_len.to_le_bytes());
        out
    }

    /// Parse a header written by [`VideoFrameHeader::to_bytes`].
    pub fn from_bytes(buf: &[u8]) -> Result<Self, WireError> {
        if buf.len() < VIDEO_FRAME_HEADER_LEN {
            return Err(WireError::VideoHeaderTooSmall(
                buf.len(),
                VIDEO_FRAME_HEADER_LEN,
            ));
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[0..4]);
        if magic != Self::MAGIC {
            return Err(WireError::BadMagic {
                expected: Self::MAGIC,
                got: magic,
            });
        }
        let frame_id = u64::from_le_bytes(buf[4..12].try_into().expect("8 bytes"));
        let width = u32::from_le_bytes(buf[12..16].try_into().expect("4 bytes"));
        let height = u32::from_le_bytes(buf[16..20].try_into().expect("4 bytes"));
        let stride = u32::from_le_bytes(buf[20..24].try_into().expect("4 bytes"));
        let pixel_format = match buf[24] {
            0 => PixelFormat::Bgra8888,
            1 => PixelFormat::Rgba8888,
            other => {
                return Err(WireError::PayloadTooLarge {
                    // Reuse: invalid discriminated byte is a desync, not size.
                    len: other as usize,
                    max: 1,
                })
            }
        };
        let compression = match buf[25] {
            0 => FrameCompression::Raw,
            1 => FrameCompression::Lz4,
            other => {
                return Err(WireError::PayloadTooLarge {
                    len: other as usize,
                    max: 1,
                })
            }
        };
        let rect_count = u16::from_le_bytes(buf[26..28].try_into().expect("2 bytes"));
        let payload_len = u32::from_le_bytes(buf[28..32].try_into().expect("4 bytes"));
        Ok(Self {
            frame_id,
            width,
            height,
            stride,
            pixel_format,
            compression,
            rect_count,
            payload_len,
        })
    }
}

/// Convenience: check whether the control handshake versions match.
pub fn version_compatible(theirs: u32) -> bool {
    theirs == PROTOCOL_VERSION
}

/// Convenience: build a [`ProbeReport`]-backed `Welcome` reply payload summary.
pub fn welcome_from_probe(guest_version: String, probe: &ProbeReport) -> ControlMessage {
    ControlMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        guest_version,
        has_dxg: probe.has_dxg,
        systemd_active: probe.systemd_active(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sessions() -> Vec<WaylandSessionInfo> {
        vec![WaylandSessionInfo {
            id: "hyprland".into(),
            name: "Hyprland".into(),
            exec: "Hyprland".into(),
            desktop_names: vec!["Hyprland".into(), "wlroots".into()],
            family: CompositorFamily::Wlroots,
        }]
    }

    fn sample_messages() -> Vec<ControlMessage> {
        vec![
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                host_version: "0.1.0".into(),
            },
            ControlMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                guest_version: "0.1.0".into(),
                has_dxg: true,
                systemd_active: true,
            },
            ControlMessage::Ping {
                seq: 42,
                timestamp_us: 1_732_000_000_123,
            },
            ControlMessage::Pong {
                seq: 42,
                timestamp_us: 1_732_000_000_456,
            },
            ControlMessage::ListSessions,
            ControlMessage::SessionList(sample_sessions()),
            ControlMessage::StartSession {
                session_id: "hyprland".into(),
                width: 1920,
                height: 1080,
                scale_milli: 1000,
                no_audio: false,
            },
            ControlMessage::SessionStarted {
                wayland_display: "wayland-0".into(),
                runtime_dir: "/run/user/1000/wsldesk-hyprland".into(),
                pid: 4242,
                compositor_family: CompositorFamily::Wlroots,
                namespaced: true,
                renderer: "gles2".into(),
            },
            ControlMessage::StopSession { session_id: "hyprland".into() },
            ControlMessage::SessionStopped {
                session_id: "hyprland".into(),
                clean_exit: true,
                detail: "exited on SIGTERM, runtime dir removed".into(),
            },
            ControlMessage::Resize {
                width: 2560,
                height: 1440,
                scale_milli: 1500,
            },
            ControlMessage::InputEvent(InputPayload::Key {
                code: 125,
                pressed: true,
            }),
            ControlMessage::InputEvent(InputPayload::PointerMotion { dx: -3, dy: 7 }),
            ControlMessage::InputEvent(InputPayload::PointerMotionAbsolute {
                x: 640,
                y: 360,
                width: 1280,
                height: 720,
            }),
            ControlMessage::InputEvent(InputPayload::PointerButton {
                button: 0x110,
                pressed: false,
            }),
            ControlMessage::InputEvent(InputPayload::PointerAxis {
                vertical: true,
                delta: -1,
            }),
            ControlMessage::ClipboardUpdate {
                mime_type: "text/plain;charset=utf-8".into(),
                data: b"hello wsl".to_vec(),
            },
            ControlMessage::Error {
                code: 7,
                message: "session failed to start".into(),
            },
        ]
    }

    #[test]
    fn control_frame_roundtrip_all_variants() {
        for msg in sample_messages() {
            let bytes = encode_frame(&msg).expect("encode");
            assert_eq!(&bytes[0..4], &CONTROL_MAGIC, "magic mismatch");
            let (decoded, consumed) = decode_frame(&bytes).expect("decode");
            assert_eq!(consumed, bytes.len(), "consumed must cover whole frame");
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn decode_leaves_trailing_bytes_unconsumed() {
        let first = encode_frame(&ControlMessage::Ping {
            seq: 1,
            timestamp_us: 2,
        })
        .unwrap();
        let second = encode_frame(&ControlMessage::Pong {
            seq: 3,
            timestamp_us: 4,
        })
        .unwrap();
        let mut joined = first.clone();
        joined.extend_from_slice(&second[..4]); // only part of the next header

        let (msg, consumed) = decode_frame(&joined).unwrap();
        assert_eq!(
            msg,
            ControlMessage::Ping {
                seq: 1,
                timestamp_us: 2
            }
        );
        assert_eq!(consumed, first.len());
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode_frame(&ControlMessage::ListSessions).unwrap();
        bytes[0] = b'X';
        match decode_frame(&bytes) {
            Err(WireError::BadMagic { got, .. }) => assert_eq!(got, *b"XSLD"),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn decode_reports_incomplete_frames() {
        let bytes = encode_frame(&ControlMessage::ListSessions).unwrap();
        for cut in 0..bytes.len() {
            match decode_frame(&bytes[..cut]) {
                Err(WireError::Incomplete(..)) => {}
                other => panic!("cut {cut}: expected Incomplete, got {other:?}"),
            }
        }
        decode_frame(&bytes).expect("full frame decodes");
    }

    #[test]
    fn decode_rejects_oversized_length() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CONTROL_MAGIC);
        bytes.extend_from_slice(&(MAX_CONTROL_FRAME_LEN + 1).to_le_bytes());
        match decode_frame(&bytes) {
            Err(WireError::PayloadTooLarge { len, max }) => {
                assert_eq!(len, MAX_CONTROL_FRAME_LEN as usize + 1);
                assert_eq!(max, MAX_CONTROL_FRAME_LEN as usize);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn encoder_rejects_oversized_payload() {
        let big = ControlMessage::ClipboardUpdate {
            mime_type: "application/octet-stream".into(),
            data: vec![0u8; MAX_CONTROL_FRAME_LEN as usize + 1],
        };
        match encode_frame(&big) {
            Err(WireError::PayloadTooLarge { .. }) => {}
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn streaming_decoder_reassembles_across_feeds() {
        let mut dec = FrameDecoder::new();
        let a = encode_frame(&ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            host_version: "test".into(),
        })
        .unwrap();
        let b = encode_frame(&ControlMessage::ListSessions).unwrap();
        let mut joined = a;
        joined.extend_from_slice(&b);

        // Feed 1 byte at a time to simulate fragmented socket reads.
        let mut out = Vec::new();
        for chunk in joined.chunks(1) {
            dec.feed(chunk);
            while let Some(msg) = dec.next_message().unwrap() {
                out.push(msg);
            }
        }
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                host_version: "test".into(),
            }
        );
        assert_eq!(out[1], ControlMessage::ListSessions);
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn streaming_decoder_clears_on_fatal_error() {
        let mut dec = FrameDecoder::new();
        dec.feed(b"garbage!");
        match dec.next_message() {
            Err(WireError::BadMagic { .. }) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
        assert_eq!(dec.buffered(), 0, "buffer must be cleared after fatal error");
    }

    #[test]
    fn decoder_rejects_bad_magic_before_reading_length() {
        let mut dec = FrameDecoder::new();
        // Bad magic AND an absurd length claim: the magic check must win so no
        // length-driven allocation logic ever runs.
        let mut header = [0u8; CONTROL_HEADER_LEN];
        header[0..4].copy_from_slice(b"NOPE");
        header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        dec.feed(&header);
        match dec.next_message() {
            Err(WireError::BadMagic { got, .. }) => assert_eq!(got, *b"NOPE"),
            other => panic!("expected BadMagic, got {other:?}"),
        }
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn decoder_rejects_oversized_length_before_waiting_for_payload() {
        let mut dec = FrameDecoder::new();
        let mut header = [0u8; CONTROL_HEADER_LEN];
        header[0..4].copy_from_slice(&CONTROL_MAGIC);
        header[4..8].copy_from_slice(&(MAX_CONTROL_FRAME_LEN + 1).to_le_bytes());
        dec.feed(&header);
        // Only the 8 header bytes were fed; the cap check must fire
        // immediately instead of waiting for (or pre-allocating) the claimed
        // 16 MiB+ payload.
        match dec.next_message() {
            Err(WireError::PayloadTooLarge { len, max }) => {
                assert_eq!(len, (MAX_CONTROL_FRAME_LEN as usize) + 1);
                assert_eq!(max, MAX_CONTROL_FRAME_LEN as usize);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
        assert_eq!(dec.buffered(), 0, "fatal error must clear the buffer");
    }

    #[test]
    fn decoder_never_preallocates_for_oversized_claims() {
        // A flood of forged headers claiming u32::MAX payloads must never grow
        // the internal buffer beyond what was actually fed.
        let mut dec = FrameDecoder::new();
        for _ in 0..8 {
            let mut header = [0u8; CONTROL_HEADER_LEN];
            header[0..4].copy_from_slice(&CONTROL_MAGIC);
            header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
            dec.feed(&header);
            assert_eq!(
                dec.buffered(),
                CONTROL_HEADER_LEN,
                "no payload bytes were fed"
            );
            assert!(matches!(
                dec.next_message(),
                Err(WireError::PayloadTooLarge { .. })
            ));
            assert_eq!(dec.buffered(), 0);
        }
    }

    #[test]
    fn video_header_wire_layout_is_explicit_little_endian() {
        // Distinctive field values; the expected byte order below pins the
        // wire layout byte-for-byte, proving to_le_bytes encoding (a
        // native-endian transmute would fail on big-endian hosts and any
        // offset change fails everywhere).
        let hdr = VideoFrameHeader {
            frame_id: 0x0102_0304_0506_0708,
            width: 0x0A0B_0C0D,
            height: 2,
            stride: 8,
            pixel_format: PixelFormat::Rgba8888,
            compression: FrameCompression::Lz4,
            rect_count: 0x0102,
            payload_len: 0x0102_0304,
        };
        let b = hdr.to_bytes();
        assert_eq!(b.len(), 32);
        assert_eq!(&b[0..4], b"WFRM");
        assert_eq!(&b[4..12], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&b[12..16], &[0x0D, 0x0C, 0x0B, 0x0A]);
        assert_eq!(&b[16..20], &[0x02, 0x00, 0x00, 0x00]);
        assert_eq!(&b[20..24], &[0x08, 0x00, 0x00, 0x00]);
        assert_eq!(b[24], 1); // PixelFormat::Rgba8888
        assert_eq!(b[25], 1); // FrameCompression::Lz4
        assert_eq!(&b[26..28], &[0x02, 0x01]);
        assert_eq!(&b[28..32], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn video_header_roundtrip_and_size() {
        let hdr = VideoFrameHeader {
            frame_id: 0x0102_0304_0506_0708,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            pixel_format: PixelFormat::Bgra8888,
            compression: FrameCompression::Lz4,
            rect_count: 5,
            payload_len: 123_456,
        };
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[0..4], &VIDEO_MAGIC);
        assert_eq!(VideoFrameHeader::from_bytes(&bytes).unwrap(), hdr);
    }

    #[test]
    fn video_header_rejects_bad_magic_and_short_buf() {
        let bytes = [0u8; 32];
        match VideoFrameHeader::from_bytes(&bytes) {
            Err(WireError::BadMagic { got, .. }) => assert_eq!(got, [0, 0, 0, 0]),
            other => panic!("expected BadMagic, got {other:?}"),
        }
        match VideoFrameHeader::from_bytes(&bytes[..31]) {
            Err(WireError::VideoHeaderTooSmall(31, 32)) => {}
            other => panic!("expected VideoHeaderTooSmall, got {other:?}"),
        }
    }

    #[test]
    fn version_gate() {
        assert!(version_compatible(PROTOCOL_VERSION));
        assert!(!version_compatible(PROTOCOL_VERSION + 1));
    }
}
