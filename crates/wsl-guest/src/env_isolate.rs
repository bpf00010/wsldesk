//! WSLg isolation & session environment orchestration (Plan.Md §5 M3).
//!
//! Two-layer isolation, applied inside a dedicated child process
//! (`wsldesk-guest exec-session`, spawned by the daemon's `StartSession`
//! handler):
//!
//! 1. **Kernel layer (preferred):** `unshare(CLONE_NEWUSER | CLONE_NEWNS)`
//!    with a 1:1 uid/gid map (`setgroups=deny`, `uid_map`, `gid_map`) and a
//!    private `tmpfs` mounted over `/tmp/.X11-unix`, so XWayland inside the
//!    desktop session gets a clean `:0` without touching WSLg. This works
//!    only for privileged launches. Ordinary-user sessions keep the distro
//!    user namespace so root-owned setuid programs such as sudo work.
//! 2. **Environment layer (always):** strip stale WSLg variables
//!    (`DISPLAY`, `WAYLAND_DISPLAY`, `WAYLAND_DISPLAY_ORIG`, `XAUTHORITY`,
//!    `PULSE_SERVER` when it points into `/mnt/wslg`), point
//!    `XDG_RUNTIME_DIR` at the isolated per-session dir (never WSLg's
//!    `/mnt/wslg/runtime-dir`), set `XDG_SESSION_TYPE=wayland`, keep
//!    `DBUS_*` and `/dev/dxg` access, and inject headless backend variables
//!    per compositor family.
//!
//! If user namespaces are disabled by the kernel/distro, layer 1 degrades
//! with a loud warning (written to the session log + isolation marker file)
//! and layer 2 still prevents WSLg collisions.

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use common::protocol::CompositorFamily;
use nix::mount::MsFlags;
use nix::sched::CloneFlags;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// `wayland-*` prefix used when polling the runtime dir for a live socket.
pub const WAYLAND_SOCKET_PREFIX: &str = "wayland-";

/// Grace period before a non-exiting compositor gets SIGKILLed.
pub const STOP_GRACE: Duration = Duration::from_secs(3);

/// One launch request resolved from a `StartSession` control message.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub id: String,
    pub family: CompositorFamily,
    pub exec: String,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
    /// Strip the WSLg audio passthrough (`PULSE_SERVER`) — default false.
    pub no_audio: bool,
}

/// Result of the kernel-layer isolation attempt, persisted into the runtime
/// dir for the daemon/dry-run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationStatus {
    /// True when `unshare(CLONE_NEWUSER|CLONE_NEWNS)` + tmpfs succeeded.
    pub namespaced: bool,
    /// True when `/dev/dxg` is readable inside the (possibly new) namespace.
    pub has_dxg: bool,
}

/// Current real uid (via `/proc/self` ownership; no libc needed).
pub fn current_uid() -> u32 {
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000)
}

/// Current real gid.
pub fn current_gid() -> u32 {
    std::fs::metadata("/proc/self").map(|m| m.gid()).unwrap_or(1000)
}

/// Isolated runtime dir: `/run/user/<uid>/wsldesk-<session_id>` (mode 0700).
///
/// Deliberately NOT WSLg's `/mnt/wslg/runtime-dir`.
pub fn runtime_dir_for(session_id: &str) -> PathBuf {
    PathBuf::from(format!(
        "/run/user/{}/wsldesk-{}",
        current_uid(),
        sanitize_session_id(session_id)
    ))
}

/// Session ids come from desktop-file stems; keep the path component safe.
///
/// Only `[A-Za-z0-9_-]` survive — dots become `-` too, so a hostile id can
/// never form a `..` path segment inside `/run/user/<uid>/wsldesk-<id>`.
pub fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Create the runtime dir (and parents) with mode 0700.
pub fn ensure_runtime_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create runtime dir {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", dir.display()))?;
    Ok(())
}

/// Is a WSLg-owned variable value pointing into WSLg's mount? (Documented
/// for future value-based strip decisions; audio is handled by `strip_audio`
/// since M4B.)
#[allow(dead_code)]
pub fn wslg_owned_value(value: &str) -> bool {
    value.contains("/mnt/wslg")
}

/// Strip stale WSLg variables from an env map (pure, unit-testable).
///
/// - Always stripped: `DISPLAY`, `WAYLAND_DISPLAY`, `WAYLAND_DISPLAY_ORIG`,
///   `XAUTHORITY`, `XDG_RUNTIME_DIR` (replaced with the isolated dir).
/// - `PULSE_SERVER`: PRESERVED by default (Milestone 4B — gives working
///   Linux → Windows audio through WSLg's PulseAudio server). Stripped only
///   when `strip_audio` is set (the `launch --no-audio` flag).
/// - Preserved: `DBUS_SESSION_BUS_ADDRESS`/`DBUS_SESSION_BUS_PID` (D-Bus
///   session bus) and everything else, including `/dev/dxg`-related defaults
///   (the device node itself is untouched — namespaces never mask /dev).
pub fn strip_wslg_vars(vars: &mut HashMap<String, String>, strip_audio: bool) {
    for name in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "WAYLAND_DISPLAY_ORIG",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
    ] {
        vars.remove(name);
    }
    if strip_audio {
        vars.remove("PULSE_SERVER");
    }
}

/// Headless backend variables / launch behavior per compositor family.
///
/// - `wlroots` (sway/hyprland/wayfire/river/labwc): `WLR_BACKENDS=headless`
///   with `WLR_LIBINPUT_NO_DEVICES=1`. `WLR_RENDERER_ALLOW_SOFTWARE=1` only
///   PERMITS the software fallback — wlroots still prefers the D3D12 GBM/EGL
///   path via `/dev/dxg` + Mesa when it initializes.
/// - `weston`: CLI flags are injected by [`compositor_command`]
///   (`--backend=headless-backend.so --width --height --socket=wayland-wsldesk`).
/// - `gnome` / `kde` / `cosmic`: the launch hook is structured here, but full
///   headless support requires the portal/EIS/PipeWire paths landing in
///   Milestone 4; the session will boot or report a captured-compositor
///   error through the normal socket-poll timeout path.
pub fn headless_vars(family: CompositorFamily, width: u32, height: u32) -> Vec<(&'static str, String)> {
    let mut vars: Vec<(&'static str, String)> = vec![
        ("XDG_SESSION_TYPE", "wayland".to_string()),
        (
            "WSLDESK_HEADLESS_SIZE",
            format!("{width}x{height}"),
        ),
    ];
    match family {
        CompositorFamily::Wlroots => {
            vars.push(("WLR_BACKENDS", "headless".into()));
            vars.push(("WLR_LIBINPUT_NO_DEVICES", "1".into()));
            vars.push(("WLR_RENDERER_ALLOW_SOFTWARE", "1".into()));
            // Enable Mesa D3D12 EGL (verified live: without this Mesa's
            // default dri2 path fails on the dxg render node; with it
            // wlroots reports "GL renderer: D3D12 (Intel Iris Xe)").
            vars.push(("GALLIUM_DRIVER", "d3d12".into()));
            // Verified live on WSL2 dxg DRM: gles2 initializes (D3D12 EGL)
            // but OUTPUT buffer allocation fails — gbm_bo_create fails
            // because the dxg DRM does not support DRM_IOCTL_MODE_CREATE_DUMB,
            // so zwlr_screencopy frames fail before a buffer is ever
            // advertised ("screencopy failed before advertising buffer
            // geometry"). pixman uses SHM allocation and captures reliably
            // (proven live: 10-frame pipeline verified end to end). Revisit
            // when a WSL2 kernel/mesa combination adds GBM dumb buffers on
            // dxg — the switch is this one variable.
            vars.push(("WLR_RENDERER", "pixman".into()));
        }
        CompositorFamily::Weston => {
            // Backend selection is via CLI args (see compositor_command);
            // weston reads XDG_RUNTIME_DIR for its socket dir.
        }
        CompositorFamily::Gnome => {
            vars.push(("XDG_CURRENT_DESKTOP", "GNOME".into()));
            // Full GNOME headless (mutter on a headless backend + portals)
            // is structured for Milestone 4 capture work.
        }
        CompositorFamily::Kde => {
            vars.push(("XDG_CURRENT_DESKTOP", "KDE".into()));
            vars.push(("KWIN_WAYLAND_NO_MODESET", "1".into()));
        }
        CompositorFamily::Cosmic => {
            vars.push(("XDG_CURRENT_DESKTOP", "COSMIC".into()));
        }
        CompositorFamily::Other => {}
    }
    vars
}

/// Compute the final program + args for a headless launch (pure fn).
///
/// `weston` gets explicit headless flags appended per Plan.Md §5 M3; `sway`
/// gets `-V` so wlroots' INFO-level renderer-selection line
/// (`[render/gles2] … GL renderer: D3D12 (…)` vs `[render/pixman]`) lands in
/// compositor.log — sway suppresses INFO logs by default, which made the
/// active renderer unknowable otherwise.
pub fn compositor_command(spec: &SessionSpec) -> (String, Vec<String>) {
    let mut tokens = spec.exec.split_whitespace().map(str::to_string);
    let Some(program) = tokens.next() else {
        return (String::new(), Vec::new());
    };
    let mut args: Vec<String> = tokens.collect();
    match spec.family {
        CompositorFamily::Weston => {
            args.extend([
                "--backend=headless-backend.so".to_string(),
                format!("--width={}", spec.width),
                format!("--height={}", spec.height),
                "--socket=wayland-wsldesk".to_string(),
            ]);
        }
        CompositorFamily::Wlroots if program == "sway" => {
            args.push("-V".to_string());
        }
        _ => {}
    }
    (program, args)
}

/// 1:1 uid map content, e.g. `1000 1000 1` (pure fn, testable).
pub fn uid_map_line(id: u32) -> String {
    format!("{id} {id} 1\n")
}

/// Attempt kernel-layer isolation in the CURRENT process (the
/// `exec-session` child — single-threaded, so `unshare` is safe here).
///
/// Steps (in order, each required by the kernel):
/// 1. remember uid/gid (after `CLONE_NEWUSER` we are 65534 until mapped),
/// 2. `unshare(CLONE_NEWUSER | CLONE_NEWNS)`,
/// 3. `setgroups=deny`, then 1:1 `uid_map`/`gid_map`,
/// 4. private `tmpfs` (`nosuid,nodev`, mode 1777) over `/tmp/.X11-unix`.
///
/// Returns `Ok(IsolationStatus)` with `namespaced = true`, or `Err(reason)`
/// when the kernel/distro refuses — callers degrade to env-only isolation.
pub fn userns_prepare() -> Result<IsolationStatus, String> {
    let uid = current_uid();
    // A single-UID user namespace leaves distro UID 0 unmapped. Sudo then
    // sees an overflow owner instead of root and cannot elevate. Choose
    // the existing environment fallback before changing any namespaces.
    if uid != 0 {
        return Err(
            "keeping distro UID mappings for sudo/setuid support; unprivileged sessions use environment isolation".into()
        );
    }
    let gid = current_gid();

    nix::sched::unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS).map_err(|e| {
        format!(
            "unshare(CLONE_NEWUSER|CLONE_NEWNS) failed: {e} — unprivileged user \
             namespaces may be disabled (kernel.unprivileged_userns_clone / apparmor)"
        )
    })?;

    // The kernel refuses gid_map unless setgroups is denied first.
    std::fs::write("/proc/self/setgroups", "deny")
        .map_err(|e| format!("write setgroups deny failed: {e}"))?;
    std::fs::write("/proc/self/uid_map", uid_map_line(uid))
        .map_err(|e| format!("write uid_map failed: {e}"))?;
    std::fs::write("/proc/self/gid_map", uid_map_line(gid))
        .map_err(|e| format!("write gid_map failed: {e}"))?;

    // Private tmpfs over /tmp/.X11-unix: XWayland inside the session gets a
    // clean :0; WSLg's X sockets are shadowed beneath the mount.
    std::fs::create_dir_all("/tmp/.X11-unix")
        .map_err(|e| format!("mkdir /tmp/.X11-unix failed: {e}"))?;
    nix::mount::mount(
        Some("tmpfs"),
        "/tmp/.X11-unix",
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777,size=4096k"),
    )
    .map_err(|e| format!("tmpfs mount on /tmp/.X11-unix failed: {e}"))?;

    let has_dxg = super::probe::has_dxg();
    Ok(IsolationStatus {
        namespaced: true,
        has_dxg,
    })
}

/// SIGTERM as the variadic `c_ulong` prctl argument.
fn libc_or_nix_sigterm() -> nix::libc::c_ulong {
    Signal::SIGTERM as nix::libc::c_ulong
}

/// Write the isolation marker file into the runtime dir (best effort).
pub fn write_isolation_marker(runtime_dir: &Path, status: IsolationStatus, reason: &str) {
    let path = runtime_dir.join("wsldesk-isolation.txt");
    let content = format!(
        "namespaced={}\nhas_dxg={}\nreason={}\n",
        status.namespaced, status.has_dxg, reason
    );
    let _ = std::fs::write(path, content);
}

/// Parse the isolation marker (pure fn, testable): `(namespaced, has_dxg)`.
pub fn parse_isolation_marker(text: &str) -> (bool, bool) {
    let mut namespaced = false;
    let mut has_dxg = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("namespaced=") {
            namespaced = v.trim() == "true";
        }
        if let Some(v) = line.strip_prefix("has_dxg=") {
            has_dxg = v.trim() == "true";
        }
    }
    (namespaced, has_dxg)
}

/// `exec-session` child path: isolate, then exec the compositor (never
/// returns on success). Returns a process exit code on pre-exec failure.
///
/// NOTE: this runs in a freshly spawned single-threaded process — `unshare`
/// here cannot destabilize the daemon's tokio threads.
pub fn run_isolated_session(spec: &SessionSpec) -> i32 {
    // Orphan prevention (Plan.Md §4A): if the daemon dies (crash, kill,
    // host disconnect closing a --once daemon), the kernel delivers SIGTERM
    // to this compositor process. The getppid check closes the classic race
    // where the parent died before prctl landed.
    unsafe {
        // PR_SET_PDEATHSIG = 1; SIGTERM.
        nix::libc::prctl(1, libc_or_nix_sigterm(), 0, 0, 0);
    }
    if nix::unistd::getppid().as_raw() == 1 {
        eprintln!("wsldesk-guest exec-session: parent daemon already died; exiting");
        return 143; // 128 + SIGTERM
    }

    let runtime_dir = runtime_dir_for(&spec.id);

    if let Err(err) = ensure_runtime_dir(&runtime_dir) {
        eprintln!("wsldesk-guest exec-session: {err:#}");
        return 126;
    }

    let (status, reason) = match userns_prepare() {
        Ok(status) => (
            status,
            String::from("unshare(CLONE_NEWUSER|CLONE_NEWNS) + tmpfs /tmp/.X11-unix"),
        ),
        Err(reason) => {
            eprintln!(
                "wsldesk-guest exec-session: WARNING: namespace isolation unavailable: \
                 {reason}; degrading to env-only isolation (stale DISPLAY/XAUTHORITY \
                 stripped, XDG_RUNTIME_DIR isolated, /dev/dxg access preserved)"
            );
            (
                IsolationStatus {
                    namespaced: false,
                    has_dxg: super::probe::has_dxg(),
                },
                reason,
            )
        }
    };
    write_isolation_marker(&runtime_dir, status, &reason);

    // Environment: inherit, strip WSLg, isolate runtime dir, headless vars.
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    strip_wslg_vars(&mut vars, spec.no_audio);
    vars.insert(
        "XDG_RUNTIME_DIR".to_string(),
        runtime_dir.display().to_string(),
    );
    for (key, value) in headless_vars(spec.family, spec.width, spec.height) {
        vars.insert(key.to_string(), value);
    }

    // Route the compositor's output into the runtime dir log (kept for crash
    // diagnostics, removed on clean teardown).
    let log_path = runtime_dir.join("compositor.log");
    match std::fs::File::create(&log_path) {
        Ok(log) => {
            // dup2 onto fd 1/2 (inherited by exec), then drop the original fd.
            let _ = nix::unistd::dup2_stdout(&log);
            let _ = nix::unistd::dup2_stderr(&log);
        }
        Err(err) => eprintln!(
            "wsldesk-guest exec-session: cannot create compositor log {}: {err}",
            log_path.display()
        ),
    }

    let (program, args) = compositor_command(spec);
    if program.is_empty() {
        eprintln!("wsldesk-guest exec-session: empty Exec= line for '{}'", spec.id);
        return 126;
    }
    eprintln!(
        "wsldesk-guest exec-session: launching '{}' {:?} (XDG_RUNTIME_DIR={}, namespaced={})",
        program,
        args,
        runtime_dir.display(),
        status.namespaced
    );

    let err = Command::new(&program)
        .args(&args)
        .env_clear()
        .envs(vars)
        .exec(); // never returns on success
    eprintln!("wsldesk-guest exec-session: failed to exec {program}: {err}");
    127
}

/// Spawn the isolated `exec-session` child (daemon side).
///
/// stdout/stderr are piped so pre-exec failures are capturable; the
/// compositor's own output is redirected to the runtime-dir log by the child.
pub fn spawn_isolated(spec: &SessionSpec) -> Result<std::process::Child> {
    let exe = std::env::current_exe().context("cannot resolve current_exe for exec-session")?;
    let mut cmd = Command::new(exe);
    cmd.args([
        "exec-session",
        "--width",
        &spec.width.to_string(),
        "--height",
        &spec.height.to_string(),
        "--scale-milli",
        &spec.scale_milli.to_string(),
    ]);
    if spec.no_audio {
        cmd.arg("--no-audio");
    }
    cmd.arg(&spec.id);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn exec-session child")
}

/// Ask a child to exit: SIGTERM, poll up to `grace`, then SIGKILL and reap.
///
/// Blocking; call from a blocking context (the daemon wraps it in
/// `spawn_blocking`).
///
/// Returns `(exited_within_grace, detail)`. Compositors without SIGTERM
/// handlers die BY the signal — that still counts as a graceful teardown
/// (no orphans, runtime dir removable), so "clean" is about the grace-period
/// exit, not the process exit status.
pub fn stop_child(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<(bool, String)> {
    let pid = child.id() as i32;
    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait().context("try_wait failed")? {
            Some(status) => {
                let how = if status.success() {
                    "exited cleanly".to_string()
                } else {
                    format!("exited on SIGTERM ({status})")
                };
                return Ok((true, how));
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            None => break,
        }
    }
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    let status = child.wait().context("wait after SIGKILL failed")?;
    Ok((
        false,
        format!("did not exit within {grace:?}; SIGKILLed ({status})"),
    ))
}

/// Drain a spawned child's captured stderr (post-exit) as a short tail.
pub fn stderr_tail(child: &mut std::process::Child, max_bytes: usize) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
    let text = String::from_utf8_lossy(&buf);
    let tail: String = text
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ");
    truncate_utf8(&tail, max_bytes)
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("…{}", &text[..end])
}

/// Best-effort runtime-dir cleanup; `Ok(true)` when fully removed.
pub fn cleanup_runtime_dir(runtime_dir: &Path) -> bool {
    match std::fs::remove_dir_all(runtime_dir) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            eprintln!(
                "wsldesk-guest: runtime dir cleanup failed for {}: {err}",
                runtime_dir.display()
            );
            false
        }
    }
}

/// Scan `dir` once for a `wayland-*` socket (pure-ish, testable on tmpfs).
pub fn find_wayland_socket(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(WAYLAND_SOCKET_PREFIX)
            && super::sessions::is_wayland_socket(&entry.path())
        {
            return Some(name.into_owned());
        }
    }
    None
}

/// A socket file is only "live" when something is listening: stale sockets
/// from crashed runs accept `connect()` with ECONNREFUSED. This probe makes
/// startup verification REAL — a false positive here sends the capture
/// thread against a dead inode (seen live: ECONNREFUSED after a previous
/// failed run left its runtime dir behind).
pub fn wayland_socket_connectable(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Wait for the compositor child to either exit (failure) or produce a
/// CONNECTABLE Wayland socket; async-friendly 100ms cadence for the daemon.
/// Returns the socket name, or `Err(exit_info)` when the child died first.
pub fn poll_session_startup(
    runtime_dir: &Path,
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::result::Result<Option<String>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "compositor exited during startup ({status}); log tail: {}",
                    stderr_tail(child, 512)
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("try_wait failed: {err}")),
        }
        if let Some(name) = find_wayland_socket(runtime_dir) {
            if wayland_socket_connectable(&runtime_dir.join(&name)) {
                return Ok(Some(name));
            }
            // Socket file exists but no listener (stale inode) — keep
            // polling until the compositor binds its own or the deadline.
        }
        if Instant::now() >= deadline {
            return Ok(None); // timeout — caller decides (captures stderr)
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn strip_removes_stale_wslg_vars() {
        let mut vars = vars_from(&[
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("WAYLAND_DISPLAY_ORIG", "wayland-0"),
            ("XAUTHORITY", "/mnt/wslg/.Xauthority"),
            ("XDG_RUNTIME_DIR", "/mnt/wslg/runtime-dir"),
            ("PULSE_SERVER", "unix:/mnt/wslg/pulseaudio.native"),
            ("PATH", "/usr/bin"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ]);
        strip_wslg_vars(&mut vars, true);
        for gone in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_DISPLAY_ORIG",
            "XAUTHORITY",
            "XDG_RUNTIME_DIR",
        ] {
            assert!(!vars.contains_key(gone), "{gone} must be stripped");
        }
        assert!(!vars.contains_key("PULSE_SERVER"), "WSLg pulse is stripped with --no-audio");
        // Preserved.
        assert_eq!(vars.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            vars.get("DBUS_SESSION_BUS_ADDRESS").map(String::as_str),
            Some("unix:path=/run/user/1000/bus")
        );
    }

    #[test]
    fn audio_is_preserved_by_default() {
        // M4B default: WSLg audio passthrough stays live (Linux → Windows).
        let mut vars = vars_from(&[("PULSE_SERVER", "unix:/mnt/wslg/pulseaudio.native")]);
        strip_wslg_vars(&mut vars, false);
        assert!(
            vars.contains_key("PULSE_SERVER"),
            "PULSE_SERVER must be preserved by default (only --no-audio strips it)"
        );
    }

    #[test]
    fn headless_vars_per_family() {
        let wlroots = headless_vars(CompositorFamily::Wlroots, 1920, 1080);
        let get = |vars: &[( &str, String)], key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get(&wlroots, "WLR_BACKENDS").as_deref(), Some("headless"));
        assert_eq!(get(&wlroots, "WLR_LIBINPUT_NO_DEVICES").as_deref(), Some("1"));
        assert_eq!(
            get(&wlroots, "WLR_RENDERER_ALLOW_SOFTWARE").as_deref(),
            Some("1")
        );
        assert_eq!(get(&wlroots, "XDG_SESSION_TYPE").as_deref(), Some("wayland"));
        assert_eq!(get(&wlroots, "WSLDESK_HEADLESS_SIZE").as_deref(), Some("1920x1080"));

        // gnome/kde structure their hooks even though capture lands in M4.
        assert_eq!(
            get(&headless_vars(CompositorFamily::Gnome, 1, 1), "XDG_CURRENT_DESKTOP").as_deref(),
            Some("GNOME")
        );
        assert_eq!(
            get(&headless_vars(CompositorFamily::Kde, 1, 1), "XDG_CURRENT_DESKTOP").as_deref(),
            Some("KDE")
        );
    }

    #[test]
    fn weston_command_gets_headless_flags() {
        let spec = SessionSpec {
            id: "weston".into(),
            family: CompositorFamily::Weston,
            exec: "weston".into(),
            width: 1920,
            height: 1080,
            scale_milli: 1000,
            no_audio: false,
        };
        let (program, args) = compositor_command(&spec);
        assert_eq!(program, "weston");
        assert!(args.contains(&"--backend=headless-backend.so".to_string()));
        assert!(args.contains(&"--width=1920".to_string()));
        assert!(args.contains(&"--height=1080".to_string()));
        assert!(args.contains(&"--socket=wayland-wsldesk".to_string()));
    }

    #[test]
    fn sway_command_gets_verbose_flag_for_renderer_visibility() {
        let spec = SessionSpec {
            id: "sway".into(),
            family: CompositorFamily::Wlroots,
            exec: "sway".into(),
            width: 1920,
            height: 1080,
            scale_milli: 1000,
            no_audio: false,
        };
        let (program, args) = compositor_command(&spec);
        assert_eq!(program, "sway");
        assert!(args.contains(&"-V".to_string()), "sway needs -V so the wlroots renderer-selection line reaches compositor.log");
    }

    #[test]
    fn wlroots_env_carries_d3d12_egl_enablement() {
        let vars = headless_vars(CompositorFamily::Wlroots, 1920, 1080);
        let d3d12 = vars
            .iter()
            .find(|(k, _)| *k == "GALLIUM_DRIVER")
            .map(|(_, v)| v.clone());
        assert_eq!(d3d12.as_deref(), Some("d3d12"));
    }

    #[test]
    fn wlroots_command_passes_exec_through() {
        let spec = SessionSpec {
            id: "hyprland".into(),
            family: CompositorFamily::Wlroots,
            exec: "Hyprland".into(),
            width: 1920,
            height: 1080,
            scale_milli: 1000,
            no_audio: false,
        };
        let (program, args) = compositor_command(&spec);
        assert_eq!(program, "Hyprland");
        assert!(args.is_empty(), "non-sway wlroots launch is env-driven, got {args:?}");
    }

    #[test]
    fn uid_map_is_1_to_1() {
        assert_eq!(uid_map_line(1000), "1000 1000 1\n");
        assert_eq!(uid_map_line(0), "0 0 1\n");
    }

    #[test]
    fn runtime_dir_is_isolated_per_session_and_never_wslg() {
        let dir = runtime_dir_for("hyprland");
        let text = dir.display().to_string();
        assert!(text.starts_with("/run/user/"), "got {text}");
        assert!(text.contains("wsldesk-hyprland"));
        assert!(!text.contains("/mnt/wslg"), "must never be WSLg's dir");
    }

    #[test]
    fn sanitize_session_id_neutralizes_path_tricks() {
        assert_eq!(sanitize_session_id("hyprland"), "hyprland");
        // "../../etc" → 6 dashes + "etc": no '/', no '..', no traversal.
        assert_eq!(sanitize_session_id("../../etc"), "------etc");
        assert_eq!(sanitize_session_id("a b/c"), "a-b-c");
        let sanitized = sanitize_session_id("../../etc");
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains(".."));
    }

    #[test]
    fn isolation_marker_roundtrip() {
        assert_eq!(parse_isolation_marker("namespaced=true\nhas_dxg=true\nreason=x\n"), (true, true));
        assert_eq!(parse_isolation_marker("namespaced=false\nhas_dxg=true\n"), (false, true));
        assert_eq!(parse_isolation_marker(""), (false, false));
    }
}
