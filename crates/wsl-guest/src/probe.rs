//! Guest system diagnostics (Plan.Md §5 M1).
//!
//! Produces a `common::ProbeReport`: systemd PID 1 check, `/dev/dxg` vGPU
//! readability, WSLg mount detection, Mesa D3D12 driver availability, and
//! parsed `/usr/share/wayland-sessions/*.desktop` entries.

use std::fs::{self, OpenOptions};
use std::path::Path;

use common::protocol::{CompositorFamily, WaylandSessionInfo};
use common::ProbeReport;

/// Directory holding `.desktop` session entries.
pub const WAYLAND_SESSIONS_DIR: &str = "/usr/share/wayland-sessions";

/// Collect the full probe report.
pub fn collect() -> ProbeReport {
    ProbeReport {
        pid1_comm: read_pid1_comm(),
        has_dxg: has_dxg(),
        wslg_detected: wslg_detected(),
        mesa_d3d12_available: mesa_d3d12_available(),
        wayland_sessions: parse_wayland_sessions(WAYLAND_SESSIONS_DIR),
    }
}

/// PID 1 name from `/proc/1/comm`; `Some("systemd")` means systemd booted.
pub fn read_pid1_comm() -> Option<String> {
    fs::read_to_string("/proc/1/comm")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `/dev/dxg` exists AND is readable (Plan.Md: "exists and is readable").
pub fn has_dxg() -> bool {
    let path = Path::new("/dev/dxg");
    path.exists() && OpenOptions::new().read(true).open(path).is_ok()
}

/// True when the WSLg system distro is mounted at `/mnt/wslg`.
pub fn wslg_detected() -> bool {
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        if mounts
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some("/mnt/wslg"))
        {
            return true;
        }
    }
    Path::new("/mnt/wslg").exists()
}

/// Mesa D3D12 Gallium driver present (`d3d12_dri.so`) or a GL/Vulkan probe
/// tool (`glxinfo`/`vulkaninfo`) on PATH.
///
/// Distro layout coverage:
/// - Debian/Ubuntu/Mint (`multiarch`): `/usr/lib/x86_64-linux-gnu/dri`
/// - Arch/Manjaro: `/usr/lib/dri`
/// - Fedora/Rocky/openSUSE: `/usr/lib64/dri`
/// - anything else: generic `/usr/lib/*/dri` scan plus the `glxinfo`/
///   `vulkaninfo` fallback (those report D3D12 via GL_RENDERER/Vulkan ICDs).
pub fn mesa_d3d12_available() -> bool {
    for dir in [
        "/usr/lib/x86_64-linux-gnu/dri",
        "/usr/lib64/dri",
        "/usr/lib/dri",
    ] {
        if Path::new(dir).join("d3d12_dri.so").is_file() {
            return true;
        }
    }
    if let Ok(entries) = fs::read_dir("/usr/lib") {
        for entry in entries.flatten() {
            if entry.path().join("dri").join("d3d12_dri.so").is_file() {
                return true;
            }
        }
    }
    tool_on_path("glxinfo") || tool_on_path("vulkaninfo")
}

fn tool_on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file())
        })
        .unwrap_or(false)
}

/// Parse every `*.desktop` under `dir` into session entries (sorted by file
/// name for stable output).
pub fn parse_wayland_sessions(dir: &str) -> Vec<WaylandSessionInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    let mut desktops: Vec<std::fs::DirEntry> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "desktop")
                .unwrap_or(false)
        })
        .collect();
    desktops.sort_by_key(|e| e.file_name());
    for entry in desktops {
        let path = entry.path();
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(info) = parse_desktop_file(id, &text) {
            out.push(info);
        }
    }
    out
}

/// Parse one freedesktop `.desktop` body into a session entry. Returns `None`
/// when there is no usable `Exec=` line.
pub fn parse_desktop_file(id: &str, text: &str) -> Option<WaylandSessionInfo> {
    let mut section = String::new();
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut desktop_names_raw: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed[1..trimmed.len() - 1].trim().to_string();
            continue;
        }
        if section != "Desktop Entry" {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            // Exact `Name` only: localized variants (`Name[de]=…`) are ignored.
            "Name" => {
                name.get_or_insert(value.to_string());
            }
            "Exec" => {
                exec.get_or_insert(value.to_string());
            }
            "DesktopNames" => {
                desktop_names_raw.get_or_insert(value.to_string());
            }
            _ => {}
        }
    }

    let exec = exec?;
    let name = name.unwrap_or_else(|| id.to_string());
    let desktop_names = desktop_names_raw
        .map(|raw| {
            raw.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    let family = classify_compositor(&exec, &desktop_names);
    Some(WaylandSessionInfo {
        id: id.to_string(),
        name,
        exec,
        desktop_names,
        family,
    })
}

/// Heuristic compositor family from `Exec=` + `DesktopNames=` (refined in
/// Milestone 3's session launcher).
pub fn classify_compositor(exec: &str, desktop_names: &[String]) -> CompositorFamily {
    let exec_l = exec.to_ascii_lowercase();
    let names_l: Vec<String> = desktop_names.iter().map(|s| s.to_ascii_lowercase()).collect();
    let hay = |needle: &str| {
        exec_l.contains(needle)
            || names_l.iter().any(|n| n == needle || n.contains(needle))
    };
    if hay("cosmic") {
        return CompositorFamily::Cosmic;
    }
    if hay("gnome") || hay("mutter") {
        return CompositorFamily::Gnome;
    }
    if hay("kde") || hay("plasma") || hay("kwin") {
        return CompositorFamily::Kde;
    }
    if hay("hyprland")
        || hay("sway")
        || hay("river")
        || hay("wayfire")
        || hay("labwc")
        || hay("wlroots")
    {
        return CompositorFamily::Wlroots;
    }
    if hay("weston") {
        return CompositorFamily::Weston;
    }
    CompositorFamily::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hyprland_desktop() {
        let text = "[Desktop Entry]\n\
                    Type=Application\n\
                    Name=Hyprland\n\
                    GenericName=Wayland Compositor\n\
                    Exec=Hyprland\n\
                    DesktopNames=Hyprland;wlroots;\n";
        let info = parse_desktop_file("hyprland", text).expect("parsed");
        assert_eq!(info.id, "hyprland");
        assert_eq!(info.name, "Hyprland");
        assert_eq!(info.exec, "Hyprland");
        assert_eq!(info.desktop_names, vec!["Hyprland", "wlroots"]);
        assert_eq!(info.family, CompositorFamily::Wlroots);
    }

    #[test]
    fn parse_ignores_localized_names_and_other_sections() {
        let text = "[Desktop Entry]\n\
                    Name=KDE Plasma\n\
                    Name[de]=KDE Plasma\n\
                    Exec=/usr/bin/startplasma-wayland\n\
                    DesktopNames=KDE\n\n\
                    [Desktop Action foo]\n\
                    Name=Wrong\n\
                    Exec=/bin/false\n";
        let info = parse_desktop_file("plasma", text).expect("parsed");
        assert_eq!(info.name, "KDE Plasma");
        assert_eq!(info.exec, "/usr/bin/startplasma-wayland");
        assert_eq!(info.family, CompositorFamily::Kde);
    }

    #[test]
    fn parse_requires_exec() {
        let text = "[Desktop Entry]\nName=Broken\n";
        assert!(parse_desktop_file("broken", text).is_none());
    }

    #[test]
    fn parse_defaults_name_to_id() {
        let text = "[Desktop Entry]\nExec=sway\n";
        let info = parse_desktop_file("sway", text).expect("parsed");
        assert_eq!(info.name, "sway");
        assert!(info.desktop_names.is_empty());
    }

    #[test]
    fn parse_handles_crlf_desktop_files() {
        let text = "[Desktop Entry]\r\nName=Sway\r\nExec=sway\r\nDesktopNames=wlroots\r\n";
        let info = parse_desktop_file("sway", text).expect("parsed");
        assert_eq!(info.name, "Sway");
        assert_eq!(info.exec, "sway");
        // CR must never leak into parsed values
        assert!(!info.exec.contains('\r'));
        assert_eq!(info.desktop_names, vec!["wlroots"]);
    }

    #[test]
    fn parse_desktop_names_with_empty_entries() {
        // Trailing/duplicated semicolons produce empty entries; they must be
        // dropped, not kept as empty strings.
        let text = "[Desktop Entry]\nName=X\nExec=x\nDesktopNames=;;GNOME;;\n";
        let info = parse_desktop_file("x", text).expect("parsed");
        assert_eq!(info.desktop_names, vec!["GNOME"]);
    }

    #[test]
    fn parse_first_duplicate_key_wins() {
        let text = "[Desktop Entry]\nName=First\nName=Second\nExec=exec-a\nExec=exec-b\n";
        let info = parse_desktop_file("dup", text).expect("parsed");
        assert_eq!(info.name, "First");
        assert_eq!(info.exec, "exec-a");
    }

    #[test]
    fn parse_tolerates_section_header_whitespace() {
        // "[ Desktop Entry ]" (extra spaces) must still be recognized as the
        // Desktop Entry section.
        let text = "[ Desktop Entry ]\nName=T\nExec=t\n";
        let info = parse_desktop_file("t", text).expect("parsed");
        assert_eq!(info.name, "T");
        assert_eq!(info.exec, "t");
    }

    #[test]
    fn parse_ignores_unknown_and_metadata_keys() {
        let text = "[Desktop Entry]\n\
                    Type=Application\n\
                    Version=1.5\n\
                    Name=Hyprland\n\
                    Comment=Dynamic tiling Wayland compositor\n\
                    Keywords=tiling;wm;windowmanager\n\
                    TryExec=Hyprland\n\
                    Exec=Hyprland\n\
                    Icon=hyprland\n\
                    Categories=System;\n";
        let info = parse_desktop_file("hyprland", text).expect("parsed");
        assert_eq!(info.name, "Hyprland");
        assert_eq!(info.exec, "Hyprland");
        assert!(info.desktop_names.is_empty());
        assert_eq!(info.family, CompositorFamily::Wlroots);
    }

    #[test]
    fn parse_falls_back_to_desktop_names_when_exec_is_ambiguous() {
        // A generic launcher name must still classify via DesktopNames.
        let text = "[Desktop Entry]\nName=Plasma (wayland)\nExec=/usr/lib/plasma-libwayland/startplasma-wayland\nDesktopNames=KDE\n";
        let info = parse_desktop_file("plasma", text).expect("parsed");
        assert_eq!(info.family, CompositorFamily::Kde);
    }

    #[test]
    fn classify_families() {
        assert_eq!(
            classify_compositor("gnome-session", &["GNOME".into()]),
            CompositorFamily::Gnome
        );
        assert_eq!(
            classify_compositor("Hyprland", &[]),
            CompositorFamily::Wlroots
        );
        assert_eq!(
            classify_compositor("cosmic-session", &["COSMIC".into()]),
            CompositorFamily::Cosmic
        );
        assert_eq!(
            classify_compositor("weston --backend=headless-backend.so", &[]),
            CompositorFamily::Weston
        );
        assert_eq!(
            classify_compositor("/usr/bin/unknown-wm", &[]),
            CompositorFamily::Other
        );
    }
}
