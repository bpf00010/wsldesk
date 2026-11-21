//! Win32 AF_HYPERV client transport (Plan.Md §4.2, Milestone 2).
//!
//! Direction rule (Plan.Md §1, docs/references/hyprsock.md): the Linux guest
//! listens on AF_VSOCK (`VMADDR_CID_ANY`); THIS side connects as the AF_HYPERV
//! client using the VM GUID fetched live via `wslinfo --vm-id` (GUIDs rotate
//! on every WSL2 boot, so they are never cached).
//!
//! Win32 async safety: raw AF_HYPERV sockets are NOT assumed to work with
//! Tokio's IOCP reactor. The blocking SOCKET is wrapped in an RAII owner
//! ([`HvSocket`], `closesocket` on `Drop`) with SO_RCVTIMEO/SO_SNDTIMEO
//! timeouts so a stalled VM can never hang the process. Milestone 4 will
//! bridge this to async via `spawn_blocking` pumps over bounded channels.

use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use thiserror::Error;
use windows::core::GUID;
use windows::Win32::Networking::WinSock::{
    closesocket, connect, recv, send, setsockopt, socket, ADDRESS_FAMILY, AF_HYPERV, SOCKADDR,
    SOCK_STREAM, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_RCVTIMEO, SO_SNDTIMEO, SEND_RECV_FLAGS,
    WSAGetLastError, WSAStartup, WSADATA,
};
use windows::Win32::System::Hypervisor::{HV_PROTOCOL_RAW, SOCKADDR_HV};

use crate::wsl_cli::{self, WslError};
use common::protocol::{
    decode_dirty_rects, decode_frame, encode_frame, ControlMessage, DirtyRectHeader,
    FrameCompression, VideoFrameHeader,
    WireError, CONTROL_MAGIC, MAX_CONTROL_FRAME_LEN, VIDEO_FRAME_HEADER_LEN, VIDEO_PORT,
};

/// `HV_GUID_VSOCK_TEMPLATE` = 00000000-facb-11e6-bd58-64006a7986d3.
///
/// The service GUID for a VSOCK port is this template with `Data1` (the
/// leading u32) replaced by the port number (Plan.Md §4.2).
const VSOCK_TEMPLATE_FIELDS: (u32, u16, u16, [u8; 8]) = (
    0x0000_0000,
    0xfacb,
    0x11e6,
    [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
);

/// Default socket read/write timeout: a stalled peer must surface as an
/// error, never an infinite block.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the AF_HYPERV service GUID for a VSOCK port.
pub fn vsock_service_id(port: u32) -> GUID {
    let (_, data2, data3, data4) = VSOCK_TEMPLATE_FIELDS;
    GUID {
        data1: port,
        data2,
        data3,
        data4,
    }
}

/// Parse a GUID string from `wslinfo --vm-id` (whitespace/braces tolerated).
pub fn parse_vm_guid(raw: &str) -> Option<GUID> {
    let uuid = uuid::Uuid::parse_str(raw.trim()).ok()?;
    let (data1, data2, data3, data4) = uuid.as_fields();
    Some(GUID {
        data1,
        data2,
        data3,
        data4: *data4,
    })
}

/// Transport-level errors with actionable messages.
///
/// Distinction between failure classes: `get_vm_id` failures mean the
/// distro/VM is not available ([`TransportError::Wsl`]); once the GUID is
/// fetched live, the VM is provably booted, so ANY connect failure means the
/// guest daemon is not (yet) listening ([`TransportError::DaemonNotRunning`]).
/// Note WSL2 hvsocket reports an unregistered service as WSAETIMEDOUT (10060)
/// rather than WSAECONNREFUSED (10061) — the code is carried for detail.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("wsl.exe failure: {0}")]
    Wsl(#[from] WslError),
    #[error("invalid VM GUID '{0}' from wslinfo --vm-id")]
    BadGuid(String),
    #[error(
        "guest daemon is not listening on AF_HYPERV port {port} (WSAE{wsa_code}) — \
         start it inside the distro with `wsl.exe -d {distro} -- wsldesk-guest daemon`, \
         or rerun with --autostart"
    )]
    DaemonNotRunning {
        distro: String,
        port: u32,
        wsa_code: i32,
    },
    #[error("AF_HYPERV socket error (WSAE{0}): {1}")]
    Wsa(i32, String),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// WSA error code of the last failed WinSock call on this thread.
fn last_wsa_error() -> i32 {
    unsafe { WSAGetLastError() }.0
}

/// One-shot, process-lifetime `WSAStartup` (never cleaned up — standard for
/// short-lived tools; `OnceLock` makes double-init impossible).
static WSA_INIT: OnceLock<Result<(), i32>> = OnceLock::new();

fn ensure_wsa() -> Result<(), TransportError> {
    match WSA_INIT.get_or_init(|| {
        let mut data = unsafe { std::mem::zeroed::<WSADATA>() };
        // 0x0202 = MAKEWORD(2, 2): WinSock 2.2.
        let err = unsafe { WSAStartup(0x0202, &mut data) };
        if err == 0 {
            Ok(())
        } else {
            Err(err)
        }
    }) {
        Ok(()) => Ok(()),
        Err(code) => Err(TransportError::Wsa(*code, "WSAStartup failed".into())),
    }
}

/// RAII owner of a raw Win32 `SOCKET`: `closesocket` runs exactly once on
/// drop, so no leak and no double-close panic — even on error paths.
struct HvSocket {
    raw: SOCKET,
}

impl HvSocket {
    /// Create an AF_HYPERV (34) stream socket with HV_PROTOCOL_RAW (1).
    fn new() -> Result<Self, TransportError> {
        ensure_wsa()?;
        let raw = unsafe { socket(AF_HYPERV as i32, SOCK_STREAM, HV_PROTOCOL_RAW as i32) }
            .map_err(|_| TransportError::Wsa(last_wsa_error(), "socket(AF_HYPERV) failed".into()))?;
        Ok(Self { raw })
    }

    /// Connect to `<vm_id>:<port>` using the VSOCK service template.
    ///
    /// `SOCKADDR_HV` is completely zero-initialized first (`Reserved = 0`,
    /// Plan.Md §4.2 memory-safety rule).
    fn connect_vm(vm_id: GUID, port: u32) -> Result<Self, TransportError> {
        let sock = Self::new()?;
        let mut addr = unsafe { std::mem::zeroed::<SOCKADDR_HV>() };
        addr.Family = ADDRESS_FAMILY(AF_HYPERV);
        addr.VmId = vm_id;
        addr.ServiceId = vsock_service_id(port);
        let result = unsafe {
            connect(
                sock.raw,
                &addr as *const SOCKADDR_HV as *const SOCKADDR,
                std::mem::size_of::<SOCKADDR_HV>() as i32,
            )
        };
        if result == SOCKET_ERROR {
            let code = last_wsa_error();
            return Err(TransportError::Wsa(
                code,
                format!("connect to VM {{{:?}}} port {port} failed", vm_id),
            ));
        }
        sock.apply_timeouts(Some(IO_TIMEOUT));
        Ok(sock)
    }

    /// Apply read/write timeouts. `Some(d)` enforces a guard; `None` disables
    /// it (needed for long-lived control streams like `launch` where the
    /// connection idles while the compositor runs — a 10s guard would kill it
    /// mid-session). Windows semantics: timeout value 0 = infinite.
    fn apply_timeouts(&self, timeout: Option<Duration>) {
        let ms: u32 = match timeout {
            Some(d) => d.as_millis().min(u32::MAX as u128) as u32,
            None => 0,
        };
        unsafe {
            let _ = setsockopt(self.raw, SOL_SOCKET, SO_RCVTIMEO, Some(ms.to_ne_bytes().as_ref()));
            let _ = setsockopt(self.raw, SOL_SOCKET, SO_SNDTIMEO, Some(ms.to_ne_bytes().as_ref()));
        }
    }

    /// Blocking send loop; returns only when `buf` is fully written.
    fn send_all(&self, mut buf: &[u8]) -> Result<(), TransportError> {
        while !buf.is_empty() {
            let n = unsafe { send(self.raw, buf, SEND_RECV_FLAGS(0)) };
            if n == SOCKET_ERROR {
                return Err(TransportError::Wsa(last_wsa_error(), "send failed".into()));
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }

    /// Blocking recv loop; fills `out` completely or errors. `0` bytes from
    /// recv before the buffer is full means the peer closed → UnexpectedEof.
    fn recv_exact(&self, out: &mut [u8]) -> Result<(), TransportError> {
        let mut filled = 0usize;
        while filled < out.len() {
            let n = unsafe { recv(self.raw, &mut out[filled..], SEND_RECV_FLAGS(0)) };
            if n == SOCKET_ERROR {
                return Err(TransportError::Wsa(last_wsa_error(), "recv failed".into()));
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed connection mid-frame",
                )
                .into());
            }
            filled += n as usize;
        }
        Ok(())
    }
}

impl Drop for HvSocket {
    fn drop(&mut self) {
        unsafe {
            let _ = closesocket(self.raw);
        }
    }
}

/// A connected AF_HYPERV control-plane connection speaking framed
/// `ControlMessage`s.
pub struct HvConnection {
    sock: HvSocket,
}

impl HvConnection {
    /// Resolve the VM GUID live via `wslinfo --vm-id` and connect to
    /// `port` (the guest must already be listening).
    pub fn connect(distro: &str, port: u32) -> Result<Self, TransportError> {
        let vm_id_raw = wsl_cli::get_vm_id(distro)?;
        let vm_id = parse_vm_guid(&vm_id_raw)
            .ok_or_else(|| TransportError::BadGuid(vm_id_raw.clone()))?;
        match HvSocket::connect_vm(vm_id, port) {
            Ok(sock) => Ok(Self { sock }),
            Err(TransportError::Wsa(code, _)) => Err(classify_connect(code, distro, port)),
            Err(other) => Err(other),
        }
    }

    /// Re-configure read/write timeouts on the live connection.
    ///
    /// Short RPCs (`ping`, `sessions`) keep the 10s guard; long-lived control
    /// streams (`launch`) must relax it — the connection idles while the
    /// compositor runs and a 10s read timeout would kill it mid-session.
    pub fn set_timeouts(&self, timeout: Option<Duration>) {
        self.sock.apply_timeouts(timeout);
    }

    /// Raw Win32 SOCKET handle (for `select()`-gated non-blocking reads in
    /// the control pump). The socket stays owned by this connection.
    pub fn raw_socket(&self) -> windows::Win32::Networking::WinSock::SOCKET {
        self.sock.raw
    }

    /// Encode + send one control frame (little-endian, length-prefixed).
    pub fn send_msg(&self, msg: &ControlMessage) -> Result<(), TransportError> {
        let bytes = encode_frame(msg)?;
        self.sock.send_all(&bytes)
    }

    /// Read one complete control frame and decode it.
    ///
    /// Header magic/length are validated before the payload is allocated;
    /// the payload size is capped at `MAX_CONTROL_FRAME_LEN` (16 MiB).
    pub fn recv_msg(&self) -> Result<ControlMessage, TransportError> {
        let mut header = [0u8; 8];
        self.sock.recv_exact(&mut header)?;
        let mut got = [0u8; 4];
        got.copy_from_slice(&header[0..4]);
        if got != CONTROL_MAGIC {
            return Err(WireError::BadMagic {
                expected: CONTROL_MAGIC,
                got,
            }
            .into());
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len > MAX_CONTROL_FRAME_LEN as usize {
            return Err(WireError::PayloadTooLarge {
                len,
                max: MAX_CONTROL_FRAME_LEN as usize,
            }
            .into());
        }
        let mut payload = vec![0u8; len];
        self.sock.recv_exact(&mut payload)?;
        Ok(decode_frame_payload(&header, &payload)?)
    }
}

/// Winsock `select()` readability gate: `true` when at least one byte is
/// buffered and a subsequent `recv` will not block. Used by the single-owner
/// control pump so inbound checking NEVER stalls outbound input (a blocking
/// recv with an infinite timeout on the pump thread would deadlock input
/// forwarding).
pub fn select_readable(sock: windows::Win32::Networking::WinSock::SOCKET, timeout: Duration) -> bool {
    use windows::Win32::Networking::WinSock::{select, FD_SET, TIMEVAL};
    unsafe {
        let mut read_set = FD_SET {
            fd_count: 1,
            fd_array: [sock; 64],
        };
        let timeout = TIMEVAL {
            tv_sec: timeout.as_secs() as i32,
            tv_usec: timeout.subsec_micros() as i32,
        };
        let result = select(
            0,
            Some(&mut read_set),
            None,
            None,
            Some(&timeout),
        );
        result > 0
    }
}

fn decode_frame_payload(header: &[u8; 8], payload: &[u8]) -> Result<ControlMessage, WireError> {
    let mut full = Vec::with_capacity(8 + payload.len());
    full.extend_from_slice(header);
    full.extend_from_slice(payload);
    Ok(decode_frame(&full)?.0)
}

/// Short stable name for a control message (host-side errors/logs).
pub fn message_kind_host(msg: &ControlMessage) -> &'static str {
    match msg {
        ControlMessage::Hello { .. } => "Hello",
        ControlMessage::Welcome { .. } => "Welcome",
        ControlMessage::Ping { .. } => "Ping",
        ControlMessage::Pong { .. } => "Pong",
        ControlMessage::ListSessions => "ListSessions",
        ControlMessage::SessionList(_) => "SessionList",
        ControlMessage::StartSession { .. } => "StartSession",
        ControlMessage::SessionStarted { .. } => "SessionStarted",
        ControlMessage::StopSession { .. } => "StopSession",
        ControlMessage::SessionStopped { .. } => "SessionStopped",
        ControlMessage::Resize { .. } => "Resize",
        ControlMessage::InputEvent(_) => "InputEvent",
        ControlMessage::ClipboardUpdate { .. } => "ClipboardUpdate",
        ControlMessage::Error { .. } => "Error",
    }
}
/// Classify a connect failure. The caller fetched the VM GUID live just
/// before connecting, so the VM is booted — any WinSock connect failure means
/// the guest daemon is not listening (or vsock registration raced); the raw
/// WSAE code is carried for diagnosis.
fn classify_connect(code: i32, distro: &str, port: u32) -> TransportError {
    TransportError::DaemonNotRunning {
        distro: distro.to_string(),
        port,
        wsa_code: code,
    }
}

// ---------------------------------------------------------------------------
// Video plane (Milestone 4A): AF_HYPERV client → guest AF_VSOCK 50002
// ---------------------------------------------------------------------------

/// Max video payload accepted from the wire (4K BGRA LZ4 + generous slack).
const MAX_VIDEO_PAYLOAD: usize = 64 * 1024 * 1024;

/// One fully decoded video frame, ready for GPU upload.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub header: VideoFrameHeader,
    pub payload: FramePayload,
}

#[derive(Debug, Clone)]
pub enum FramePayload {
    Full(Vec<u8>),
    DirtyRects(Vec<DirtyRect>),
}

#[derive(Debug, Clone)]
pub struct DirtyRect {
    pub region: DirtyRectHeader,
    pub pixels: Vec<u8>,
}

impl DecodedFrame {
    /// Approximate compressed size of this frame on the wire (header + LZ4).
    pub fn wire_len(&self) -> usize {
        VIDEO_FRAME_HEADER_LEN + self.header.payload_len as usize
    }

    pub fn pixel_len(&self) -> usize {
        match &self.payload {
            FramePayload::Full(pixels) => pixels.len(),
            FramePayload::DirtyRects(rects) => rects.iter().map(|rect| rect.pixels.len()).sum(),
        }
    }
}

/// Blocking AF_HYPERV video-stream connection (guest → host, one direction).
///
/// Owns a dedicated [`HvSocket`] to `VIDEO_PORT`; `recv_frame` reads a fixed
/// 32-byte `b"WFRM"` header, then the payload, then LZ4-decodes it. Raw
/// payloads are passed through.
pub struct VideoStream {
    sock: HvSocket,
}

impl VideoStream {
    /// Resolve the VM GUID and connect to `VIDEO_PORT`.
    pub fn connect(distro: &str) -> Result<Self, TransportError> {
        let vm_id_raw = wsl_cli::get_vm_id(distro)?;
        let vm_id =
            parse_vm_guid(&vm_id_raw).ok_or_else(|| TransportError::BadGuid(vm_id_raw.clone()))?;
        match HvSocket::connect_vm(vm_id, VIDEO_PORT) {
            Ok(sock) => Ok(Self { sock }),
            Err(TransportError::Wsa(code, _)) => Err(classify_connect(code, distro, VIDEO_PORT)),
            Err(other) => Err(other),
        }
    }

    /// Receive, validate, and decode one frame.
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        select_readable(self.sock.raw, timeout)
    }

    /// Receive, validate, and decode one frame after it becomes readable.
    pub fn recv_frame(&self) -> Result<DecodedFrame, TransportError> {
        let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_LEN];
        self.sock.recv_exact(&mut header_bytes)?;
        let header = VideoFrameHeader::from_bytes(&header_bytes)?;
        let full_len = u64::from(header.width) * u64::from(header.height);
        let full_len = full_len.checked_mul(4)
            .filter(|len| (1..=MAX_VIDEO_PAYLOAD as u64).contains(len))
            .ok_or_else(|| TransportError::Wsa(0, "invalid video dimensions".into()))? as usize;
        if header.width.checked_mul(4) != Some(header.stride)
            || header.rect_count > common::protocol::MAX_DIRTY_RECTS
        {
            return Err(TransportError::Wsa(0, "invalid video stride or rectangle count".into()));
        }
        if header.payload_len as usize > MAX_VIDEO_PAYLOAD {
            return Err(TransportError::Wsa(
                0,
                format!("video payload too large: {}", header.payload_len),
            ));
        }
        let mut payload = vec![0u8; header.payload_len as usize];
        self.sock.recv_exact(&mut payload)?;
        let max_decoded = if header.rect_count == 0 {
            full_len
        } else {
            MAX_VIDEO_PAYLOAD
        };
        let pixels = match header.compression {
            FrameCompression::Lz4 => {
                let len = payload.first_chunk::<4>().map(|bytes| u32::from_le_bytes(*bytes) as usize);
                if len.is_none_or(|len| len > max_decoded || len == 0) {
                    return Err(TransportError::Wsa(0, "invalid LZ4 decoded size".into()));
                }
                lz4_flex::decompress_size_prepended(&payload)
                    .map_err(|err| TransportError::Wsa(0, format!("lz4 decompress failed: {err}")))?
            }
            FrameCompression::Raw => payload,
        };
        if header.rect_count == 0 && pixels.len() != full_len {
            return Err(TransportError::Wsa(
                0,
                format!(
                    "frame size mismatch: decoded {} bytes, expected {} ({}x{} BGRA/RGBA)",
                    pixels.len(),
                    full_len,
                    header.width,
                    header.height
                ),
            ));
        }
        let payload = if header.rect_count == 0 {
            FramePayload::Full(pixels)
        } else {
            FramePayload::DirtyRects(
                decode_dirty_rects(&pixels, header.rect_count, header.width, header.height)?
                    .into_iter().map(|(region, data)| DirtyRect { region, pixels: data.to_vec() })
                    .collect(),
            )
        };
        Ok(DecodedFrame { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_id_matches_vsock_template_with_port_in_data1() {
        let guid = vsock_service_id(50001);
        assert_eq!(guid.data1, 50001, "Data1 carries the port");
        // The remaining bytes must equal the template's:
        // 00000000-facb-11e6-bd58-64006a7986d3
        let template = uuid::Uuid::parse_str("00000000-facb-11e6-bd58-64006a7986d3")
            .expect("template parses");
        let (_, data2, data3, data4) = template.as_fields();
        assert_eq!(guid.data2, data2);
        assert_eq!(guid.data3, data3);
        assert_eq!(guid.data4, *data4);
    }

    #[test]
    fn parse_vm_guid_accepts_braces_and_whitespace() {
        let guid = parse_vm_guid("  {7E6EB0B2-E007-434E-8D1F-B767946E63BF} \n")
            .expect("braced GUID parses");
        let expected = uuid::Uuid::parse_str("7e6eb0b2-e007-434e-8d1f-b767946e63bf").unwrap();
        let (data1, data2, data3, data4) = expected.as_fields();
        assert_eq!((guid.data1, guid.data2, guid.data3), (data1, data2, data3));
        assert_eq!(guid.data4, *data4);
    }

    #[test]
    fn parse_vm_guid_rejects_garbage() {
        assert!(parse_vm_guid("not-a-guid").is_none());
        assert!(parse_vm_guid("").is_none());
    }

    #[test]
    fn classify_connect_flags_daemon_not_running_with_code() {
        // The VM GUID is always fetched live before connecting, so any WinSock
        // connect failure (including WSAETIMEDOUT for an unregistered vsock
        // service) classifies as "daemon not listening".
        for code in [10061, 10060, 10065, 10051, 10049, 10064, 12345] {
            match classify_connect(code, "Ubuntu", 50001) {
                TransportError::DaemonNotRunning {
                    distro,
                    port,
                    wsa_code,
                } => {
                    assert_eq!(distro, "Ubuntu");
                    assert_eq!(port, 50001);
                    assert_eq!(wsa_code, code);
                }
                other => panic!("expected DaemonNotRunning, got {other:?}"),
            }
        }
    }
}
