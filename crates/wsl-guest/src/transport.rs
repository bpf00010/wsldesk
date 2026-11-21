//! Linux AF_VSOCK listener transport (Plan.Md §4.2, Milestones 2–4B).
//!
//! The guest is the AF_VSOCK **listener** bound to `VMADDR_CID_ANY` on
//! [`common::protocol::CONTROL_PORT`]; the Windows host connects as the
//! AF_HYPERV client (reversing the direction causes WSAEACCES on non-elevated
//! shells and is forbidden — Plan.Md §1, docs/references/hyprsock.md).
//!
//! Wire format: `b"WSLD"`-framed postcard `ControlMessage`s.
//! Handshake: `Hello` → `Welcome`, then `Ping` → `Pong`, and since 4B:
//! `InputEvent` / `Resize` / `ClipboardUpdate` are routed into the active
//! session's Wayland bridge while guest clipboard selections flow back over
//! the same control connection (bidirectional, non-blocking).
//!
//! Connection architecture (4B): the VSOCK stream is split into a read half
//! (owned by the message loop) and a write half (shared via `Arc<tokio Mutex>`
//! so handlers and the clipboard-forwarding task can all emit messages).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use common::protocol::{
    version_compatible, welcome_from_probe, ControlMessage, FrameDecoder, CONTROL_PORT,
    PROTOCOL_VERSION,
};
use common::ProbeReport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::{VsockAddr, VsockListener, VsockStream};
use vsock::VMADDR_CID_ANY;

use crate::capture::VideoChannel;
use crate::env_isolate::{self, SessionSpec};

/// Guest crate version reported in `Welcome`.
pub const GUEST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Frame read buffer per connection.
const READ_BUF_LEN: usize = 16 * 1024;

/// `--once` mode gives up after this long without a client connecting, so an
/// abandoned `--autostart` daemon cannot hold AF_VSOCK port 50001 forever.
const ONCE_ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Linux `EADDRINUSE` (98): another daemon already owns the port.
const EADDRINUSE: i32 = 98;

/// Shared write half of the control connection: every emitter (message
/// handlers, clipboard forwarder) writes through this.
type SharedWriter = Arc<tokio::sync::Mutex<tokio_vsock::OwnedWriteHalf>>;

/// Synchronous entry point for the `daemon` subcommand: builds the tokio
/// runtime and runs the accept loop.
pub fn daemon_main(once: bool) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to start tokio runtime")?;
    runtime.block_on(serve(once))
}

/// Bind `AF_VSOCK` port 50001 and serve framed control connections.
///
/// With `once = true` exactly one connection is served and the daemon exits —
/// this is what `wsldesk.exe ping --autostart` launches on demand so ping
/// works without systemd lingering. In once-mode an idle 30s accept timeout
/// applies; the persistent daemon serves forever.
pub async fn serve(once: bool) -> Result<()> {
    let addr = VsockAddr::new(VMADDR_CID_ANY, CONTROL_PORT);
    let listener = match VsockListener::bind(addr) {
        Ok(l) => l,
        Err(err) if err.raw_os_error() == Some(EADDRINUSE) => {
            anyhow::bail!(
                "AF_VSOCK port {CONTROL_PORT} is already in use (EADDRINUSE): all WSL2 \
                 distros share one utility VM and one vsock namespace, so another \
                 wsldesk-guest daemon instance is already listening — reuse it or stop \
                 it first (e.g. `wsl.exe -d <distro> -- pkill wsldesk-guest`)"
            );
        }
        Err(err) => return Err(err).context(format!("failed to bind AF_VSOCK port {CONTROL_PORT}")),
    };
    eprintln!(
        "wsldesk-guest daemon listening on AF_VSOCK cid=VMADDR_CID_ANY port={CONTROL_PORT} (once={once})"
    );
    loop {
        let accepted = if once {
            // Bound the idle wait so an abandoned autostart daemon exits.
            match tokio::time::timeout(ONCE_ACCEPT_TIMEOUT, listener.accept()).await {
                Ok(result) => Some(result),
                Err(_elapsed) => {
                    eprintln!(
                        "--once: no client connected within {}s; exiting cleanly",
                        ONCE_ACCEPT_TIMEOUT.as_secs()
                    );
                    return Ok(());
                }
            }
        } else {
            Some(listener.accept().await)
        };
        let (stream, peer) = match accepted {
            Some(Ok(pair)) => pair,
            Some(Err(err)) => return Err(err).context("AF_VSOCK accept failed"),
            None => unreachable!("accepted is always Some here"),
        };
        eprintln!("accepted connection from cid={} port={}", peer.cid(), peer.port());
        if once {
            handle_conn(stream).await;
            eprintln!("once-mode connection handled; daemon exiting");
            return Ok(());
        }
        tokio::spawn(handle_conn(stream));
    }
}

/// Per-connection loop: decode frames from the stream, dispatch, and exit
/// cleanly on EOF/IO/framing errors — never crashes the daemon.
async fn handle_conn(stream: VsockStream) {
    let (mut read_half, write_half) = stream.into_split();
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; READ_BUF_LEN];
    loop {
        // Dispatch every complete frame already buffered.
        loop {
            match dec.next_message() {
                Ok(Some(msg)) => {
                    if !handle_message(&writer, msg).await {
                        return;
                    }
                }
                Ok(None) => break, // need more bytes
                Err(err) => {
                    // Stream is desynced: report and drop the connection.
                    let _ = send_msg(
                        &writer,
                        &ControlMessage::Error {
                            code: 1,
                            message: format!("framing error: {err}"),
                        },
                    )
                    .await;
                    eprintln!("framing error ({err}); closed connection");
                    return;
                }
            }
        }
        match read_half.read(&mut buf).await {
            Ok(0) => {
                eprintln!("client disconnected cleanly");
                teardown_active_session_if_any().await;
                return;
            }
            Ok(n) => dec.feed(&buf[..n]),
            Err(err) => {
                eprintln!("read error ({err}); closed connection");
                teardown_active_session_if_any().await;
                return;
            }
        }
    }
}

/// Tear down the active session (video thread + compositor + runtime dir)
/// when the owning control connection goes away without an explicit
/// StopSession — host crash or window-kill must never leak a headless
/// compositor (Plan.Md §4A/§4B orphan prevention; PR_SET_PDEATHSIG only
/// covers the --once daemon's own death).
async fn teardown_active_session_if_any() {
    let Some(active) = active_slot().lock().expect("active lock").take() else {
        return;
    };
    let ActiveSession {
        mut child,
        runtime_dir,
        spec,
        video,
        input_tx,
        clipboard_task,
    } = active;
    eprintln!(
        "control connection lost with active session '{}' — tearing down (SIGTERM, \
         runtime-dir cleanup)",
        spec.id
    );
    if let Some(video) = &video {
        video.stop();
    }
    drop(input_tx); // closes the bridge input channel
    if let Some(task) = clipboard_task {
        task.abort();
    }
    let _ = tokio::task::spawn_blocking(move || {
        let _ = env_isolate::stop_child(&mut child, env_isolate::STOP_GRACE);
        if let Some(video) = video {
            video.join();
        }
        env_isolate::cleanup_runtime_dir(Path::new(&runtime_dir));
    })
    .await;
}

/// Handle one inbound message; returns `false` when the connection must close.
async fn handle_message(writer: &SharedWriter, msg: ControlMessage) -> bool {
    match msg {
        ControlMessage::Hello {
            protocol_version,
            host_version,
        } => {
            eprintln!("Hello from host {host_version} (protocol {protocol_version})");
            if !version_compatible(protocol_version) {
                let _ = send_msg(
                    writer,
                    &ControlMessage::Error {
                        code: 2,
                        message: format!(
                            "unsupported protocol version {protocol_version}; expected {PROTOCOL_VERSION}"
                        ),
                    },
                )
                .await;
                return false;
            }
            // Real diagnostics, not canned values.
            let probe = ProbeReport {
                pid1_comm: crate::probe::read_pid1_comm(),
                has_dxg: crate::probe::has_dxg(),
                ..ProbeReport::default()
            };
            match send_msg(writer, &welcome_from_probe(GUEST_VERSION.into(), &probe)).await {
                Ok(()) => true,
                Err(err) => {
                    eprintln!("failed to send Welcome: {err}");
                    false
                }
            }
        }
        ControlMessage::Ping { seq, timestamp_us } => {
            let pong = ControlMessage::Pong { seq, timestamp_us };
            match send_msg(writer, &pong).await {
                Ok(()) => true,
                Err(err) => {
                    eprintln!("failed to send Pong (seq {seq}): {err}");
                    false
                }
            }
        }
        ControlMessage::ListSessions => {
            let list = crate::sessions::available_sessions();
            eprintln!("ListSessions → {} entries", list.len());
            send_msg(writer, &ControlMessage::SessionList(list))
                .await
                .is_ok()
        }
        ControlMessage::StartSession {
            session_id,
            width,
            height,
            scale_milli,
            no_audio,
            refresh_mhz,
        } => start_session(writer, session_id, width, height, scale_milli, no_audio, refresh_mhz).await,
        ControlMessage::StopSession { session_id } => stop_session(writer, session_id).await,
        // 4B: interactive traffic — route into the active session's bridge.
        ControlMessage::InputEvent(_)
        | ControlMessage::Resize { .. }
        | ControlMessage::ClipboardUpdate { .. } => {
            route_to_session(msg).await
        }
        other => {
            let _ = send_msg(
                writer,
                &ControlMessage::Error {
                    code: 3,
                    message: format!(
                        "{} is not supported until a later milestone",
                        message_kind(&other)
                    ),
                },
            )
            .await;
            true // unsupported message does not desync the stream
        }
    }
}

/// Forward interactive traffic to the active session's Wayland bridge. With
/// no active session the message is dropped silently (the viewport only
/// sends input while a session exists — races at teardown are normal).
async fn route_to_session(msg: ControlMessage) -> bool {
    let input_tx = active_slot().lock().expect("active lock").as_ref().map(|a| a.input_tx.clone());
    if let Some(tx) = input_tx {
        // Never block the control loop on a slow session: drop when full.
        if matches!(msg, ControlMessage::InputEvent(common::protocol::InputPayload::Key { .. })) {
            if tx.send(msg).await.is_err() { eprintln!("keyboard bridge disconnected"); }
        } else {
            let _ = tx.try_send(msg);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Session lifecycle (Milestones 3–4B)
// ---------------------------------------------------------------------------

/// State of the single running session (M3 supports one at a time; the
/// daemon itself stays alive across sessions).
struct ActiveSession {
    spec: SessionSpec,
    child: std::process::Child,
    runtime_dir: String,
    /// Screencopy + input/output/clipboard bridge channel on VIDEO_PORT (4A).
    video: Option<VideoChannel>,
    /// Inbound interactive traffic (InputEvent/Resize/ClipboardUpdate).
    input_tx: tokio::sync::mpsc::Sender<ControlMessage>,
    /// Guest clipboard selections forwarded over the control connection.
    clipboard_task: Option<tokio::task::JoinHandle<()>>,
}

static ACTIVE_SESSION: OnceLock<Mutex<Option<ActiveSession>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Option<ActiveSession>> {
    ACTIVE_SESSION.get_or_init(|| Mutex::new(None))
}

/// StartSession handler: spawn the isolated compositor, poll the isolated
/// runtime dir (up to 5s) for a CONNECTABLE `wayland-*` socket, verify the
/// child is still alive, then reply `SessionStarted` and open the video +
/// interactivity channels. On failure: reap the child, capture its stderr,
/// clean the runtime dir, and reply a diagnostic `Error`.
async fn start_session(
    writer: &SharedWriter,
    session_id: String,
    width: u32,
    height: u32,
    scale_milli: u32,
    no_audio: bool,
    refresh_mhz: u32,
) -> bool {
    if active_slot().lock().expect("active lock").is_some() {
        let _ = send_msg(
            writer,
            &ControlMessage::Error {
                code: 10,
                message: "a session is already running — send StopSession first".into(),
            },
        )
        .await;
        return true;
    }

    let Some(info) = crate::sessions::find_session(&session_id) else {
        let _ = send_msg(
            writer,
            &ControlMessage::Error {
                code: 11,
                message: format!("session '{session_id}' not found — send ListSessions first"),
            },
        )
        .await;
        return true;
    };

    let spec = SessionSpec {
        id: session_id.clone(),
        family: info.family,
        exec: info.exec.clone(),
        width,
        height,
        scale_milli,
        no_audio,
    };
    let runtime_dir = env_isolate::runtime_dir_for(&session_id);

    // A previous crashed/aborted run may have left its runtime dir (with a
    // dead wayland socket inode) behind — wipe it so the socket poll can
    // never latch onto a stale socket instead of this session's own.
    let _ = std::fs::remove_dir_all(&runtime_dir);

    eprintln!(
        "StartSession '{}' ({}): exec='{}' size={width}x{height} scale_milli={scale_milli} \
         no_audio={no_audio} runtime_dir={}",
        spec.id,
        info.family.label(),
        info.exec,
        runtime_dir.display()
    );

    let child = match env_isolate::spawn_isolated(&spec) {
        Ok(child) => child,
        Err(err) => {
            let _ = send_msg(
                writer,
                &ControlMessage::Error {
                    code: 12,
                    message: format!("failed to spawn exec-session: {err:#}"),
                },
            )
            .await;
            return true;
        }
    };

    // Poll startup in a blocking worker: try_wait + fs polling with sleeps
    // must not stall the tokio runtime (spawn_isolated child is Send).
    let rd_path = runtime_dir.clone();
    let startup = tokio::task::spawn_blocking(move || {
        let mut child = child; // mutable inside this worker only
        let socket = env_isolate::poll_session_startup(&rd_path, &mut child, STARTUP_TIMEOUT);
        (socket, child)
    })
    .await;
    let (startup_result, child) = match startup {
        Ok(pair) => pair,
        Err(err) => {
            let _ = send_msg(
                writer,
                &ControlMessage::Error {
                    code: 12,
                    message: format!("startup poll failed: {err}"),
                },
            )
            .await;
            return true;
        }
    };

    match startup_result {
        Ok(Some(wayland_display)) => {
            // Read the isolation marker written by the exec-session child.
            let (namespaced, has_dxg_in_ns) =
                std::fs::read_to_string(runtime_dir.join("wsldesk-isolation.txt"))
                    .map(|text| env_isolate::parse_isolation_marker(&text))
                    .unwrap_or((false, false));
            // Renderer reality check (gles2/vulkan vs pixman) from the
            // compositor log — reported to the host and shown in the title.
            let renderer = crate::capture::detect_renderer(&runtime_dir.join("compositor.log"));
            eprintln!(
                "session '{}' live: socket={wayland_display} pid={} namespaced={namespaced} \
                 has_dxg_in_ns={has_dxg_in_ns} renderer={renderer}",
                spec.id,
                child.id()
            );
            let pid = child.id();

            // Interactivity channels (4B): daemon → bridge and bridge → daemon.
            let (input_tx, input_rx) = tokio::sync::mpsc::channel::<ControlMessage>(256);
            let (clip_tx, mut clip_rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(16);

            // Video + session bridge on VIDEO_PORT.
            let video = VideoChannel::start(
                runtime_dir.join(&wayland_display),
                crate::wayland_bridge::SessionChannels {
                    input_rx,
                    clipboard_tx: clip_tx,
                    refresh_mhz,
                },
                width,
                height,
                scale_milli,
            );

            // Forward guest clipboard selections over the control connection.
            let clip_writer = writer.clone();
            let clipboard_task = tokio::spawn(async move {
                while let Some((mime, data)) = clip_rx.recv().await {
                    let msg = ControlMessage::ClipboardUpdate {
                        mime_type: mime,
                        data,
                    };
                    if send_msg(&clip_writer, &msg).await.is_err() {
                        break;
                    }
                }
            });

            *active_slot().lock().expect("active lock") = Some(ActiveSession {
                spec,
                child,
                runtime_dir: runtime_dir.display().to_string(),
                video: Some(video),
                input_tx,
                clipboard_task: Some(clipboard_task),
            });
            let reply = ControlMessage::SessionStarted {
                wayland_display,
                runtime_dir: runtime_dir.display().to_string(),
                pid,
                compositor_family: info.family,
                namespaced,
                renderer,
            };
            send_msg(writer, &reply).await.is_ok()
        }
        Ok(None) => {
            // Timeout: reap the child, capture stderr, clean up.
            let tail = stop_and_collect(child, runtime_dir.clone()).await;
            let _ = send_msg(
                writer,
                &ControlMessage::Error {
                    code: 13,
                    message: format!(
                        "no Wayland socket appeared in {} within {}s for '{}' ({}); {}",
                        runtime_dir.display(),
                        STARTUP_TIMEOUT.as_secs(),
                        info.exec,
                        info.family.label(),
                        tail
                    ),
                },
            )
            .await;
            true
        }
        Err(startup_error) => {
            // Child died during startup: collect stderr and clean up.
            let tail = stop_and_collect(child, runtime_dir.clone()).await;
            let _ = send_msg(
                writer,
                &ControlMessage::Error {
                    code: 13,
                    message: format!("{startup_error}; {tail}"),
                },
            )
            .await;
            true
        }
    }
}

/// StopSession handler: cancel the video/bridge thread, SIGTERM → grace
/// poll → SIGKILL, verify exit, remove the runtime dir, reply
/// `SessionStopped`.
async fn stop_session(writer: &SharedWriter, session_id: String) -> bool {
    let taken = active_slot().lock().expect("active lock").take();
    let Some(active) = taken else {
        let _ = send_msg(
            writer,
            &ControlMessage::Error {
                code: 14,
                message: "no active session".into(),
            },
        )
        .await;
        return true;
    };
    if !session_id.is_empty() && session_id != active.spec.id {
        let active_id = active.spec.id.clone();
        *active_slot().lock().expect("active lock") = Some(active);
        let _ = send_msg(
            writer,
            &ControlMessage::Error {
                code: 14,
                message: format!(
                    "session id mismatch: active '{active_id}', requested '{session_id}'"
                ),
            },
        )
        .await;
        return true;
    }

    let ActiveSession {
        mut child,
        runtime_dir,
        spec,
        video,
        input_tx,
        clipboard_task,
    } = active;
    // Cancel the video/bridge thread first so its wayland connection to the
    // compositor being stopped goes away (its stop flag makes the thread
    // exit within one frame).
    if let Some(video) = &video {
        video.stop();
    }
    drop(input_tx);
    if let Some(task) = clipboard_task {
        task.abort();
    }
    eprintln!("StopSession '{}': terminating pid {}", spec.id, child.id());
    let (exited_cleanly, detail) = tokio::task::spawn_blocking(move || {
        // The video thread exits within one frame after its stop flag; join
        // it after the compositor is gone so no wayland fd outlives cleanup.
        let (result, detail) = env_isolate::stop_child(&mut child, env_isolate::STOP_GRACE)
            .unwrap_or((false, "stop failed".into()));
        if let Some(video) = video {
            video.join();
        }
        (result, detail)
    })
    .await
    .unwrap_or((false, "stop task failed".into()));
    let dir_removed = env_isolate::cleanup_runtime_dir(Path::new(&runtime_dir));
    let clean = exited_cleanly && dir_removed;
    eprintln!(
        "session '{}' stopped: clean={clean} detail={detail} dir_removed={dir_removed}",
        spec.id
    );
    send_msg(
        writer,
        &ControlMessage::SessionStopped {
            session_id: spec.id,
            clean_exit: clean,
            detail: format!("{detail} (runtime dir removed: {dir_removed})"),
        },
    )
    .await
    .is_ok()
}

/// Reap a failing child (blocking work in `spawn_blocking`), collect its
/// captured stderr and clean up the runtime dir; returns a diagnostic string.
async fn stop_and_collect(mut child: std::process::Child, runtime_dir: PathBuf) -> String {
    let tail = tokio::task::spawn_blocking(move || {
        let _ = env_isolate::stop_child(&mut child, Duration::from_secs(2));
        env_isolate::stderr_tail(&mut child, 512)
    })
    .await
    .unwrap_or_else(|_| String::from("stop task failed"));
    let dir_removed = env_isolate::cleanup_runtime_dir(&runtime_dir);
    let mut message = if tail.is_empty() {
        String::from("no stderr captured")
    } else {
        format!("stderr: {tail}")
    };
    if !dir_removed {
        message.push_str("; runtime dir cleanup incomplete");
    }
    message
}

/// How long to wait for the Wayland socket to appear (Plan.Md: up to 5s).
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Stable short name for a control message (logs + Error replies).
pub fn message_kind(msg: &ControlMessage) -> &'static str {
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

/// Encode and write one control frame through the shared writer.
async fn send_msg(writer: &SharedWriter, msg: &ControlMessage) -> io::Result<()> {
    let bytes =
        common::protocol::encode_frame(msg).map_err(|err| io::Error::other(err.to_string()))?;
    let mut guard = writer.lock().await;
    guard.write_all(&bytes).await?;
    guard.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_kind_covers_all_variants() {
        let msgs = [
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                host_version: "t".into(),
            },
            ControlMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                guest_version: "t".into(),
                has_dxg: false,
                systemd_active: false,
            },
            ControlMessage::Ping { seq: 1, timestamp_us: 2 },
            ControlMessage::Pong { seq: 1, timestamp_us: 2 },
            ControlMessage::ListSessions,
            ControlMessage::SessionList(vec![]),
            ControlMessage::StartSession {
                session_id: "s".into(),
                width: 1,
                height: 1,
                scale_milli: 1000,
                no_audio: false,
                refresh_mhz: common::protocol::default_refresh_mhz(),
            },
            ControlMessage::SessionStarted {
                wayland_display: "wayland-0".into(),
                runtime_dir: "/tmp".into(),
                pid: 1,
                compositor_family: common::protocol::CompositorFamily::Wlroots,
                namespaced: true,
                renderer: "pixman".into(),
            },
            ControlMessage::StopSession { session_id: "s".into() },
            ControlMessage::SessionStopped {
                session_id: "s".into(),
                clean_exit: true,
                detail: "ok".into(),
            },
            ControlMessage::Resize {
                width: 1,
                height: 1,
                scale_milli: 1000,
                refresh_mhz: common::protocol::default_refresh_mhz(),
            },
            ControlMessage::InputEvent(common::protocol::InputPayload::Key {
                code: 1,
                pressed: true,
            }),
            ControlMessage::ClipboardUpdate {
                mime_type: "t".into(),
                data: vec![],
            },
            ControlMessage::Error {
                code: 0,
                message: String::new(),
            },
        ];
        for msg in &msgs {
            assert!(!message_kind(msg).is_empty());
        }
    }

    #[test]
    fn ping_pong_frames_roundtrip() {
        // The daemon echoes seq and timestamp_us verbatim; pin that contract.
        let ping = ControlMessage::Ping {
            seq: 0xABCD_EF01,
            timestamp_us: 1_732_000_000_123_456,
        };
        let pong = match ping {
            ControlMessage::Ping { seq, timestamp_us } => ControlMessage::Pong { seq, timestamp_us },
            _ => unreachable!(),
        };
        let bytes = common::protocol::encode_frame(&pong).expect("encode");
        let (decoded, consumed) = common::protocol::decode_frame(&bytes).expect("decode");
        assert_eq!(consumed, bytes.len());
        match decoded {
            ControlMessage::Pong { seq, timestamp_us } => {
                assert_eq!(seq, 0xABCD_EF01);
                assert_eq!(timestamp_us, 1_732_000_000_123_456);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn guest_version_constant_is_set() {
        assert!(!GUEST_VERSION.is_empty());
    }
}
