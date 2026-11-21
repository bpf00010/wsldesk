//! Wayland session discovery & classification (Plan.Md §5 M3).
//!
//! Primary source: `/usr/share/wayland-sessions/*.desktop` (parsing logic
//! shared with `probe.rs`). Fallback: known compositor binaries on `PATH`
//! (`weston`, `sway`, …) for distros that install a compositor without a
//! desktop entry (e.g. a bare `apt install weston`).

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use common::protocol::{CompositorFamily, WaylandSessionInfo};

use crate::probe;

/// Directory holding `.desktop` session entries.
pub const WAYLAND_SESSIONS_DIR: &str = "/usr/share/wayland-sessions";

/// Known Wayland compositor binaries for PATH-fallback detection, with their
/// compositor family. Order defines the synthesized session order.
const FALLBACK_BINARIES: &[(&str, CompositorFamily)] = &[
    ("Hyprland", CompositorFamily::Wlroots),
    ("sway", CompositorFamily::Wlroots),
    ("wayfire", CompositorFamily::Wlroots),
    ("river", CompositorFamily::Wlroots),
    ("labwc", CompositorFamily::Wlroots),
    ("cosmic-session", CompositorFamily::Cosmic),
    ("weston", CompositorFamily::Weston),
    ("gnome-session", CompositorFamily::Gnome),
    ("startplasma-wayland", CompositorFamily::Kde),
];

/// All launchable Wayland sessions: `.desktop` entries first (stable,
/// admin-curated), then PATH-fallback binaries for IDs not already covered.
pub fn available_sessions() -> Vec<WaylandSessionInfo> {
    let mut sessions = probe::parse_wayland_sessions(WAYLAND_SESSIONS_DIR);
    let fallback = detect_binary_fallback(&path_dirs());
    for entry in fallback {
        if !sessions.iter().any(|s| s.id == entry.id) {
            sessions.push(entry);
        }
    }
    sessions
}

/// Resolve a session spec by id (`None` when unknown).
pub fn find_session(id: &str) -> Option<WaylandSessionInfo> {
    available_sessions().into_iter().find(|s| s.id == id)
}

/// `$PATH` entries of this process (order-preserving, empty entries dropped).
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).filter(|d| !d.as_os_str().is_empty()).collect())
        .unwrap_or_default()
}

/// Synthesize session entries for known compositor binaries found on the
/// given directories. Pure function over the dir list → unit-testable.
pub fn detect_binary_fallback(dirs: &[PathBuf]) -> Vec<WaylandSessionInfo> {
    let mut out = Vec::new();
    for (bin, family) in FALLBACK_BINARIES {
        if dirs.iter().any(|dir| dir.join(bin).is_file()) {
            let id = bin.to_ascii_lowercase();
            out.push(WaylandSessionInfo {
                id: id.clone(),
                name: format!("{bin} (binary fallback)"),
                exec: (*bin).to_string(),
                desktop_names: Vec::new(),
                family: *family,
            });
        }
    }
    out
}

/// True when a filesystem entry resolves to a UNIX domain socket.
///
/// Uses `fs::metadata` (follows symlinks) so compositor socket symlinks also
/// count; a broken link or regular file does not.
pub fn is_wayland_socket(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.file_type().is_socket(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wsldesk-sessions-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn fallback_detects_only_present_binaries() {
        let dir = tempdir();
        fs::write(dir.join("sway"), b"#!/bin/sh\n").unwrap();
        fs::write(dir.join("weston"), b"#!/bin/sh\n").unwrap();
        // Non-file entries must be ignored.
        fs::create_dir_all(dir.join("Hyprland")).unwrap();

        let found = detect_binary_fallback(std::slice::from_ref(&dir));
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["sway", "weston"]);
        assert_eq!(found[0].family, CompositorFamily::Wlroots);
        assert_eq!(found[1].family, CompositorFamily::Weston);
        assert!(found[0].exec == "sway" && found[0].name.contains("fallback"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fallback_on_empty_dirs_is_empty() {
        let dir = tempdir();
        assert!(detect_binary_fallback(std::slice::from_ref(&dir)).is_empty());
        assert!(detect_binary_fallback(&[]).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fallback_order_matches_declaration_order() {
        let dir = tempdir();
        // Create in reverse order; output must keep FALLBACK_BINARIES order.
        fs::write(dir.join("weston"), b"x").unwrap();
        fs::write(dir.join("sway"), b"x").unwrap();
        let found = detect_binary_fallback(std::slice::from_ref(&dir));
        assert_eq!(
            found.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["sway", "weston"]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wayland_socket_detection() {
        use std::os::unix::net::UnixListener;
        let dir = tempdir();
        // A real UNIX socket.
        let sock_path = dir.join("wayland-0");
        let listener = UnixListener::bind(&sock_path).expect("bind test socket");
        assert!(is_wayland_socket(&sock_path));
        // A regular file is NOT a socket.
        let file_path = dir.join("regular.txt");
        fs::write(&file_path, b"data").unwrap();
        assert!(!is_wayland_socket(&file_path));
        // A missing path is NOT a socket.
        assert!(!is_wayland_socket(&dir.join("wayland-missing")));
        // Symlink pointing to a socket IS a socket (compositors symlink).
        let link = dir.join("wayland-link");
        std::os::unix::fs::symlink(&sock_path, &link).unwrap();
        assert!(is_wayland_socket(&link));
        drop(listener);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn socket_permissions_lint_guard() {
        // Compile-time reminder that runtime dirs are created 0700 in
        // env_isolate; PermissionsExt must stay in scope there.
        let mode = fs::metadata("/tmp").map(|m| m.permissions().mode()).unwrap_or(0);
        let _ = mode & 0o777;
    }
}
