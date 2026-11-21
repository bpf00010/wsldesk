//! Shared, OS-agnostic building blocks for WSLDesk.
//!
//! - [`protocol`]: control-plane framing (`b"WSLD"`) and the video frame header
//!   (`b"WFRM"`), both defined in Plan.Md §4.1.
//! - [`keymap`]: cross-platform evdev keycode definitions (Milestone 4).
//!
//! [`ProbeReport`] (in this module) is the DTO produced by the guest's
//! `doctor-probe` subcommand and consumed by the host `doctor` command, so the
//! JSON schema has exactly one definition.
//!
//! This crate is wire-format and DTO code only: it is forbidden from using
//! `unsafe` (no transmutes, no raw pointer casts — endianness handling is done
//! exclusively via explicit `to_le_bytes`/`from_le_bytes`).

#![forbid(unsafe_code)]

pub mod keymap;
pub mod protocol;

use serde::{Deserialize, Serialize};

/// Reports whether PID 1 inside the distro is systemd (raw `/proc/1/comm`).
pub type Pid1Comm = Option<String>;

/// Structured result of `wsldesk-guest doctor-probe`.
///
/// Serialized to JSON by the guest (musl binary) and deserialized by the
/// Windows host `doctor` command. Keep field names stable — they are the wire
/// JSON contract between the two sides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// Raw `/proc/1/comm`; `Some("systemd")` means systemd is PID 1.
    #[serde(default)]
    pub pid1_comm: Pid1Comm,
    /// `/dev/dxg` exists and is readable (vGPU paravirtualization device).
    #[serde(default)]
    pub has_dxg: bool,
    /// `/mnt/wslg` is mounted (WSLg stack detected).
    #[serde(default)]
    pub wslg_detected: bool,
    /// Mesa D3D12 Gallium driver (or a GL/Vulkan probe tool) is available.
    #[serde(default)]
    pub mesa_d3d12_available: bool,
    /// Desktops parsed from `/usr/share/wayland-sessions/*.desktop`.
    #[serde(default)]
    pub wayland_sessions: Vec<protocol::WaylandSessionInfo>,
}

impl ProbeReport {
    /// True when PID 1 reported `systemd`.
    pub fn systemd_active(&self) -> bool {
        self.pid1_comm.as_deref() == Some("systemd")
    }
}
