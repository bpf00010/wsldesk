//! `wsldesk.exe` — Windows host CLI (Plan.Md §5).
//!
//! Milestone 1 scope: `setup` (WSL checks, safe config patching, guest
//! deployment, optional service install) and `doctor` (pass/warn/fail
//! diagnostic table). `ping` arrives with Milestone 2, `sessions`/`launch`
//! with Milestone 3.
//!
//! Restart semantics (hardening pass):
//! - `/etc/wsl.conf` changes → per-distro `wsl --terminate <distro>` only;
//! - `.wslconfig` changes → full `wsl --shutdown` required, which stops ALL
//!   distros (docker-desktop included). Setup never runs it implicitly: it
//!   warns and requires the explicit `--shutdown` flag.

mod config;
mod deployer;
mod input;
mod renderer;
mod wsl_cli;
// The AF_HYPERV transport uses the Win32 `windows` crate — Windows-only.
#[cfg(windows)]
mod transport;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use common::ProbeReport;

/// Host crate version used for guest version-parity checks.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "wsldesk",
    version,
    about = "WSLDesk — distro-agnostic native Wayland full-desktop bridge for WSL2"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Provision Windows + WSL2 + the static guest binary.
    Setup {
        /// Target distro name (as shown by `wsl.exe --list --verbose`).
        #[arg(long)]
        distro: Option<String>,
        /// Run `wsl.exe --install -d <name> --no-launch` first when missing.
        #[arg(long)]
        install: bool,
        /// Explicitly set guiApplications=false in .wslconfig (disables WSLg).
        #[arg(long)]
        disable_wslg: bool,
        /// Path to a prebuilt wsldesk-guest binary.
        #[arg(long, value_name = "PATH")]
        guest_bin: Option<PathBuf>,
        /// Also install /etc/systemd/user/wsldesk-guest.service (enabled via
        /// `systemctl --global`, lingering for the default user).
        #[arg(long)]
        enable_service: bool,
        /// After a .wslconfig change, run `wsl --shutdown` (stops ALL distros,
        /// e.g. docker-desktop). Without this flag setup only warns.
        #[arg(long)]
        shutdown: bool,
        /// Install Sway and create the starter desktop if it has no config.
        #[arg(long)]
        install_desktop: bool,
    },
    /// Prepare the default WSL2 distro and open its Wayland desktop.
    Up {
        #[arg(long)]
        distro: Option<String>,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long)]
        fullscreen: bool,
        #[arg(long)]
        no_audio: bool,
        /// Requested guest output refresh in Hz (30–240).
        #[arg(long)]
        fps: Option<u32>,
    },
    /// Print a pass/warn/fail diagnostic table for a distro.
    Doctor {
        #[arg(long)]
        distro: Option<String>,
    },
    /// [M2] Measure AF_HYPERV ↔ AF_VSOCK round-trip latency.
    Ping {
        #[arg(long)]
        distro: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
        /// Launch `wsldesk-guest daemon --once` on demand when nothing is
        /// listening yet (no systemd needed).
        #[arg(long)]
        autostart: bool,
    },
    /// [M3] List detected Wayland desktop sessions over the control plane.
    Sessions {
        #[arg(long)]
        distro: String,
        /// Launch `wsldesk-guest daemon --once` on demand when nothing is
        /// listening yet.
        #[arg(long)]
        autostart: bool,
    },
    /// [M4B] Launch a headless Wayland session: without `--dry-run` this
    /// opens the native DX12 viewport with live pixels + full input.
    Launch {
        #[arg(long)]
        distro: String,
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 1.0)]
        scale: f32,
        /// Launch `wsldesk-guest daemon --once` on demand when nothing is
        /// listening yet.
        #[arg(long)]
        autostart: bool,
        /// Boot the session, verify the Wayland socket + isolation, hold
        /// briefly, then send StopSession and verify clean teardown.
        #[arg(long)]
        dry_run: bool,
        /// [4A diagnostics] Receive N real frames over the video plane,
        /// verify them (with an injected InputEvent + Resize test), then
        /// tear down — no window.
        #[arg(long, value_name = "N")]
        headless_frames: Option<u64>,
        /// Strip the WSLg audio passthrough (PULSE_SERVER). Audio is ON by
        /// default (M4B).
        #[arg(long)]
        no_audio: bool,
        /// Start the viewport borderless-fullscreen on the current monitor.
        #[arg(long)]
        fullscreen: bool,
        /// Trace keyboard forwarding while the viewport is focused.
        #[arg(long)]
        debug_input: bool,
        /// Requested guest output refresh in Hz (30–240, default 144).
        #[arg(long)]
        fps: Option<u32>,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "warn,wsldesk=info,wgpu=error,naga=error".into()
        }))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Setup {
            distro,
            install,
            disable_wslg,
            guest_bin,
            enable_service,
            shutdown,
            install_desktop,
        } => map_exit(resolve_distro(distro).and_then(|distro| run_setup(
            &distro,
            install,
            disable_wslg,
            guest_bin,
            enable_service,
            shutdown,
            install_desktop,
        ))),
        Command::Up { distro, width, height, fullscreen, no_audio, fps } =>
            map_exit(resolve_distro(distro).and_then(|distro| {
                run_setup(&distro, false, false, None, false, false, false)?;
                run_launch(&distro, "", width, height, 1.0, true, false, None, no_audio, fullscreen, false, fps)
            })),
        Command::Doctor { distro } => map_exit(resolve_distro(distro).and_then(|distro| run_doctor(&distro))),
        Command::Ping {
            distro,
            count,
            autostart,
        } => map_exit(run_ping(&distro, count, autostart)),
        Command::Sessions {
            distro,
            autostart,
        } => map_exit(run_sessions(&distro, autostart)),
        Command::Launch {
            distro,
            session,
            width,
            height,
            scale,
            autostart,
            dry_run,
            headless_frames,
            no_audio,
            fullscreen,
            debug_input,
            fps,
        } => map_exit(run_launch(
            &distro,
            &session,
            width,
            height,
            scale,
            autostart,
            dry_run,
            headless_frames,
            no_audio,
            fullscreen,
            debug_input,
            fps,
        )),
    }
}

fn map_exit(result: Result<bool>) -> ExitCode {
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

/// Order matters: distro-side changes (`/etc/wsl.conf`, guest binary, unit)
/// happen first; the `.wslconfig` patch runs last because it may end the
/// session with `wsl --shutdown`.
fn resolve_distro(explicit: Option<String>) -> Result<String> {
    if let Some(name) = explicit {
        return Ok(name);
    }
    wsl_cli::list_distros()?.into_iter().find(|d| d.is_default && d.version == 2)
        .map(|d| d.name)
        .ok_or_else(|| anyhow::anyhow!("No default WSL2 distro. Select one with --distro or use wsl --set-default."))
}

fn run_setup(
    distro: &str,
    install: bool,
    disable_wslg: bool,
    guest_bin: Option<PathBuf>,
    enable_service: bool,
    shutdown_flag: bool,
    install_desktop: bool,
) -> Result<bool> {
    println!("== wsldesk setup: distro '{distro}' ==");

    // 1. WSL 2.0+ present?
    match wsl_cli::check_wsl_version() {
        Ok(v) => println!("[1/6] WSL version {} (>= 2.0 required) — OK", v.raw),
        Err(err) => {
            if install {
                eprintln!(
                    "warning: wsl.exe --version failed ({err}); \
                     attempting `wsl.exe --install` is not automated — install WSL manually first"
                );
            }
            bail!("WSL 2.0+ is required: {err}");
        }
    }

    // 2. Distro present (optionally install) and WSL2 (never WSL1).
    let distros = wsl_cli::list_distros().map_err(|err| {
        anyhow::anyhow!("failed to list distros: {err} — is WSL initialized?")
    })?;
    let existing = distros.iter().find(|d| d.name == distro).cloned();
    let mut info = existing;
    if info.is_none() {
        if !install {
            bail!(
                "distro '{distro}' is not installed (pass --install to run \
                 `wsl.exe --install -d {distro} --no-launch`)"
            );
        }
        println!("[2/6] installing distro '{distro}' (--no-launch)…");
        let out = wsl_cli::install_distro(distro)?;
        if !out.success() {
            bail!("`wsl.exe --install -d {distro} --no-launch` failed: {}", out.stderr.trim());
        }
        info = wsl_cli::list_distros()?
            .into_iter()
            .find(|d| d.name == distro);
    }
    match &info {
        Some(d) if d.version == 2 => {
            println!("[2/6] distro '{}' state={} WSL{} — OK", d.name, d.state, d.version)
        }
        Some(d) => bail!(
            "distro '{distro}' runs WSL{} — WSLDesk REQUIRES WSL2 (AF_VSOCK and /dev/dxg \
             are WSL2-only); convert it with `wsl.exe --set-version {distro} 2`",
            d.version
        ),
        None => bail!("distro '{distro}' still missing after install"),
    }

    deployer::ensure_graphics_groups(distro)?;
    if install_desktop {
        deployer::install_desktop(distro)?;
    }

    // 3. /etc/wsl.conf — LF-only, systemd=true, per-distro terminate only.
    let wslconf = config::patch_distro_wsl_conf(distro)?;
    print_config_patch("[3/6]", "/etc/wsl.conf", &wslconf.diff, wslconf.backup_path.clone(), wslconf.changed);
    if wslconf.needs_terminate {
        println!(
            "      systemd was newly enabled — terminating '{distro}' so it boots \
             with systemd on next launch (per-distro restart is sufficient for \
             /etc/wsl.conf)"
        );
        let out = wsl_cli::terminate_distro(distro)?;
        if !out.success() {
            eprintln!(
                "warning: `wsl.exe --terminate {distro}` failed: {} (the change \
                 still applies on next launch)",
                out.stderr.trim()
            );
        }
    }

    // 4. Guest binary deploy (SHA-256 verified, atomic via stdin).
    let report = deployer::deploy_guest(distro, guest_bin.as_deref())?;
    println!(
        "[4/6] guest {} ({}; sha256 {}…) — {}",
        report.bin_path.display(),
        report.source.label(),
        &report.local_sha256[..12.min(report.local_sha256.len())],
        if report.deployed {
            "deployed".to_string()
        } else {
            format!("already up to date (remote sha {}… matches)", {
                let r = report.remote_sha256_before.as_deref().unwrap_or("");
                r[..12.min(r.len())].to_string()
            })
        }
    );
    match &report.guest_version {
        Some(v) => println!("      verified: {v}"),
        None => eprintln!("      warning: could not verify `wsldesk-guest --version`"),
    }

    // 5. Optional systemd user unit (global enable + linger).
    if enable_service {
        let svc = deployer::install_guest_service(distro)?;
        println!("[5/6] installed {}", svc.unit_path);
        if svc.global_enabled {
            println!("      systemctl --global enable wsldesk-guest.service — OK");
        } else {
            eprintln!(
                "      warning: `systemctl --global enable wsldesk-guest.service` failed \
                 (systemd not fully booted or minimal distro); enable manually inside the distro"
            );
        }
        match &svc.linger_user {
            Some(user) if svc.linger_enabled => {
                println!("      lingering enabled for default user '{user}' (user manager starts at VM boot)");
            }
            Some(user) => eprintln!(
                "      warning: `loginctl enable-linger {user}` failed — run it inside the distro \
                 so the user manager starts without login"
            ),
            None => eprintln!(
                "      warning: could not resolve the default user for lingering \
                 (initialize the distro and set [user] default= in /etc/wsl.conf)"
            ),
        }
    } else {
        println!("[5/6] systemd user unit skipped (pass --enable-service to install)");
    }

    // 6. .wslconfig — CRLF; changes require a FULL wsl --shutdown.
    let wslconfig = config::patch_user_wslconfig(disable_wslg)?;
    print_config_patch(
        "[6/6]",
        &wslconfig.path.display().to_string(),
        &wslconfig.diff,
        wslconfig.backup_path.as_ref().map(|p| p.display().to_string()),
        wslconfig.changed,
    );
    if wslconfig.changed {
        handle_wslconfig_shutdown(distro, shutdown_flag)?;
    }

    println!("setup complete for '{distro}'");
    Ok(report.guest_version.is_some())
}

/// `.wslconfig` is only read when the utility VM boots, so any change needs a
/// full `wsl --shutdown` (all distros stop). Never run implicitly: warn about
/// currently running distros and require the explicit `--shutdown` flag.
fn handle_wslconfig_shutdown(target_distro: &str, shutdown_flag: bool) -> Result<()> {
    let running: Vec<String> = wsl_cli::list_distros()?
        .into_iter()
        .filter(|d| d.state.eq_ignore_ascii_case("running"))
        .map(|d| d.name)
        .collect();
    if !running.is_empty() {
        eprintln!(
            "warning: .wslconfig changes require a FULL `wsl.exe --shutdown`; \
             these distros are currently RUNNING and would be stopped: {}",
            running.join(", ")
        );
    } else {
        println!(
            "      .wslconfig changes take effect only after a full `wsl.exe --shutdown`"
        );
    }
    if shutdown_flag {
        println!(
            "      --shutdown flag set — running `wsl.exe --shutdown` now \
             (stops ALL distros incl. '{target_distro}')"
        );
        let out = wsl_cli::shutdown()?;
        if !out.success() {
            eprintln!(
                "warning: `wsl.exe --shutdown` failed: {} (run it manually)",
                out.stderr.trim()
            );
        }
    } else {
        println!(
            "      NOT shut down automatically — run `wsl.exe --shutdown` manually \
             (or rerun setup with --shutdown)"
        );
    }
    Ok(())
}

fn print_config_patch(step: &str, path: &str, diff: &[String], backup: Option<String>, changed: bool) {
    if changed {
        println!("{step} patched {path}");
    } else {
        println!("{step} {path} already up to date (untouched)");
    }
    if let Some(backup) = backup {
        println!("      backup: {backup}");
    }
    for line in diff {
        println!("      {line}");
    }
}

// ---------------------------------------------------------------------------
// control plane (Milestone 2 transport + Milestone 3 orchestration)
// ---------------------------------------------------------------------------

/// Connect to the guest control plane, optionally autostarting the daemon.
///
/// WSL2 hvsocket reports an unregistered service as WSAETIMEDOUT rather than
/// WSAECONNREFUSED, so autostart retries over a short window instead of
/// trusting a single refused/timeout classification.
#[cfg(windows)]
fn connect_control_plane(distro: &str, autostart: bool) -> Result<transport::HvConnection> {
    use common::protocol::CONTROL_PORT;
    use transport::{HvConnection, TransportError};

    // Resolve the VM GUID live (boots the distro if stopped, proving the
    // utility VM is reachable before we touch sockets).
    let vm_id = wsl_cli::get_vm_id(distro)
        .map_err(|err| anyhow::anyhow!("cannot resolve VM GUID for '{distro}': {err}"))?;
    println!("distro '{distro}' VM id: {vm_id}");

    let connect = || HvConnection::connect(distro, CONTROL_PORT);
    match connect() {
        Ok(conn) => Ok(conn),
        Err(TransportError::DaemonNotRunning { .. }) if autostart => {
            println!("daemon not listening — autostarting `wsldesk-guest daemon --once`…");
            if input::DEBUG_INPUT.load(std::sync::atomic::Ordering::Relaxed) {
                // Inherit stderr so the guest trace appears alongside host events.
                std::process::Command::new("wsl.exe")
                    .env("WSL_UTF8", "1")
                    .args(["-d", distro, "--", "env", "WSLDESK_DEBUG_INPUT=1",
                        "wsldesk-guest", "daemon", "--once"])
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .map_err(|err| anyhow::anyhow!("debug daemon autostart failed: {err}"))?;
            } else {
                wsl_cli::spawn_user(distro, &["wsldesk-guest", "daemon", "--once"])
                    .map_err(|err| anyhow::anyhow!("autostart failed: {err}"))?;
            }
            // Poll for the AF_VSOCK listener to come up (bind can take a
            // moment after wsl.exe launches the distro).
            let mut last_err = None;
            let mut conn_opt = None;
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                match connect() {
                    Ok(conn) => {
                        conn_opt = Some(conn);
                        break;
                    }
                    Err(err @ TransportError::DaemonNotRunning { .. }) => last_err = Some(err),
                    Err(other) => bail!("{other}"),
                }
            }
            match conn_opt {
                Some(conn) => Ok(conn),
                None => bail!(
                    "daemon still not listening after autostart ({}) — is wsldesk-guest \
                     deployed? run `wsldesk setup --distro {distro}` first",
                    last_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unknown error".into())
                ),
            }
        }
        Err(err) => bail!("{err}"),
    }
}

/// Hello → Welcome handshake with protocol version check; returns
/// `(guest_version, has_dxg, systemd_active)`.
#[cfg(windows)]
fn handshake(
    conn: &transport::HvConnection,
    distro: &str,
) -> Result<(String, bool, bool)> {
    use common::protocol::{version_compatible, ControlMessage, PROTOCOL_VERSION};

    conn.send_msg(&ControlMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_version: HOST_VERSION.into(),
    })?;
    let welcome = conn.recv_msg()?;
    let ControlMessage::Welcome {
        protocol_version,
        guest_version,
        has_dxg,
        systemd_active,
    } = welcome
    else {
        bail!("expected Welcome, got {}", transport::message_kind_host(&welcome));
    };
    if !version_compatible(protocol_version) {
        bail!(
            "protocol version mismatch: guest {protocol_version}, host {PROTOCOL_VERSION} — \
             redeploy the guest with `wsldesk setup --distro {distro}`"
        );
    }
    Ok((guest_version, has_dxg, systemd_active))
}

/// `wsldesk.exe ping` — Hello/Welcome handshake then N sequential Ping/Pong
/// round-trips over the VMBus, reporting per-packet and min/avg/max RTT in
/// microseconds.
#[cfg(windows)]
fn run_ping(distro: &str, count: u32, autostart: bool) -> Result<bool> {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use common::protocol::ControlMessage;

    let conn = connect_control_plane(distro, autostart)?;
    let (guest_version, has_dxg, systemd_active) = handshake(&conn, distro)?;
    println!(
        "handshake OK: guest {guest_version}, has_dxg={has_dxg}, systemd_active={systemd_active}"
    );

    println!("ping {count} packets over VMBus (AF_HYPERV → AF_VSOCK):");
    let mut rtts_us: Vec<u64> = Vec::with_capacity(count as usize);
    for seq in 0..u64::from(count) {
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let t0 = Instant::now();
        conn.send_msg(&ControlMessage::Ping { seq, timestamp_us })?;
        let reply = conn.recv_msg()?;
        let rtt = t0.elapsed();
        let ControlMessage::Pong {
            seq: pong_seq, ..
        } = reply
        else {
            bail!(
                "expected Pong for seq {seq}, got {}",
                transport::message_kind_host(&reply)
            );
        };
        if pong_seq != seq {
            bail!("Pong seq mismatch: sent {seq}, got {pong_seq}");
        }
        let rtt_us = u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX);
        rtts_us.push(rtt_us);
        println!("  seq={seq:<3} rtt={rtt_us:>6} µs");
    }

    let min = rtts_us.iter().min().copied().unwrap_or(0);
    let max = rtts_us.iter().max().copied().unwrap_or(0);
    let avg = if rtts_us.is_empty() {
        0
    } else {
        rtts_us.iter().sum::<u64>() / rtts_us.len() as u64
    };
    println!(
        "rtt min/avg/max = {min}/{avg}/{max} µs over {} packets (zero network config, VMBus only)",
        rtts_us.len()
    );
    Ok(true)
}

/// `wsldesk.exe sessions` — ListSessions over the control plane, printed as a
/// clean table (ID / Name / Family / Exec).
#[cfg(windows)]
fn run_sessions(distro: &str, autostart: bool) -> Result<bool> {
    use common::protocol::ControlMessage;

    let conn = connect_control_plane(distro, autostart)?;
    let (guest_version, has_dxg, systemd_active) = handshake(&conn, distro)?;
    println!(
        "connected: guest {guest_version}, has_dxg={has_dxg}, systemd_active={systemd_active}"
    );

    conn.send_msg(&ControlMessage::ListSessions)?;
    let reply = conn.recv_msg()?;
    let ControlMessage::SessionList(sessions) = reply else {
        bail!(
            "expected SessionList, got {}",
            transport::message_kind_host(&reply)
        );
    };
    if sessions.is_empty() {
        println!(
            "no Wayland sessions detected in '{distro}' (no .desktop entries and no known \
             compositor binaries on PATH) — install one, e.g. `sudo apt install weston` \
             or `sudo pacman -S sway`"
        );
        return Ok(false);
    }
    print_sessions_table(&sessions);
    Ok(true)
}

/// Pure table renderer (unit-testable).
fn print_sessions_table(sessions: &[common::protocol::WaylandSessionInfo]) {
    let id_w = sessions.iter().map(|s| s.id.len()).max().unwrap_or(2).max(2);
    let name_w = sessions
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let fam_w = sessions
        .iter()
        .map(|s| s.family.label().len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<id_w$}  {:<name_w$}  {:<fam_w$}  EXEC",
        "ID",
        "NAME",
        "FAMILY",
        id_w = id_w,
        name_w = name_w,
        fam_w = fam_w
    );
    println!("{}", "-".repeat(id_w + name_w + fam_w + 12));
    for s in sessions {
        println!(
            "{:<id_w$}  {:<name_w$}  {:<fam_w$}  {}",
            s.id,
            s.name,
            s.family.label(),
            s.exec,
            id_w = id_w,
            name_w = name_w,
            fam_w = fam_w
        );
    }
}

/// `wsldesk.exe launch` — StartSession on the guest, then:
/// - `--headless-frames N`: verify N real frames over the video plane (no
///   window) and tear down — non-interactive pipeline proof (M4A);
/// - `--dry-run`: hold 2s, StopSession, verify clean teardown;
/// - otherwise: open the native DX12 viewport with live pixels.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_launch(
    distro: &str,
    session: &str,
    width: u32,
    height: u32,
    scale: f32,
    autostart: bool,
    dry_run: bool,
    headless_frames: Option<u64>,
    no_audio: bool,
    fullscreen: bool,
    debug_input: bool,
    fps: Option<u32>,
) -> Result<bool> {
    input::DEBUG_INPUT.store(debug_input, std::sync::atomic::Ordering::Relaxed);
    use std::sync::Arc;
    use std::time::Duration;

    use common::protocol::ControlMessage;

    let scale_milli = (scale * 1000.0).round().clamp(500.0, 8000.0) as u32;
    let refresh_mhz = fps.map_or_else(common::protocol::default_refresh_mhz, |fps| {
        fps.clamp(30, 240) * 1000
    });

    let conn = connect_control_plane(distro, autostart)?;
    let (guest_version, has_dxg, systemd_active) = handshake(&conn, distro)?;
    println!(
        "connected: guest {guest_version}, has_dxg={has_dxg}, systemd_active={systemd_active}"
    );
    // Long-lived control stream: relaxed now; the viewport path sets it to
    // infinite (None) — an idle user must never lose the control channel.
    conn.set_timeouts(Some(Duration::from_secs(120)));

    let selected_session;
    let session = if session.is_empty() {
        conn.send_msg(&ControlMessage::ListSessions)?;
        let ControlMessage::SessionList(sessions) = conn.recv_msg()? else {
            bail!("guest did not return its Wayland session list");
        };
        selected_session = sessions.iter().find(|s| s.id == "sway")
            .or_else(|| sessions.iter().find(|s| s.family == common::protocol::CompositorFamily::Wlroots))
            .ok_or_else(|| anyhow::anyhow!("No supported wlroots session. Run wsldesk setup --install-desktop."))?
            .id.clone();
        println!("selected session '{selected_session}'");
        selected_session.as_str()
    } else {
        session
    };

    conn.send_msg(&ControlMessage::StartSession {
        session_id: session.to_string(),
        width,
        height,
        scale_milli,
        no_audio,
        refresh_mhz,
    })?;
    let reply = conn.recv_msg()?;
    let ControlMessage::SessionStarted {
        wayland_display,
        runtime_dir,
        pid,
        compositor_family,
        namespaced,
        renderer,
    } = reply
    else {
        bail!(
            "expected SessionStarted, got {}",
            transport::message_kind_host(&reply)
        );
    };
    println!(
        "session '{session}' started: family={} pid={pid} namespaced={namespaced} renderer={renderer}",
        compositor_family.label()
    );
    println!("  wayland socket : {runtime_dir}/{wayland_display}");
    println!("  runtime dir    : {runtime_dir} (XDG_RUNTIME_DIR, mode 0700)");
    println!("  /dev/dxg       : {}", if has_dxg { "accessible (vGPU)" } else { "NOT accessible" });
    if renderer == "pixman" {
        eprintln!(
            "  warning: compositor is on software pixman rendering — check GPU/EGL \
             (mesa d3d12 + /dev/dxg) if the stream feels sluggish"
        );
    }

    // Receive the requested sample before injecting input and a live resize.
    // Always tear down — even when receiving fails — so a failed run never
    // leaks /run/user/1000/wsldesk-* or an orphaned compositor.
    if let Some(frame_sample) = headless_frames {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let live_resize_tested = (width, height) != (800, 600);
        let received = renderer::receive_headless_frames(
            distro, frame_sample.max(1), (width, height), scale_milli, refresh_mhz, &conn, &stop,
        );
        let mut last_dims = (0u32, 0u32);
        let mut resize_flipped = false;
        let (stats, checksum_ok) = match received {
            Ok((stats, checksum_ok, flipped)) => {
                last_dims = (stats.width, stats.height);
                resize_flipped = flipped;
                println!(
                    "headless verification: {} frames, {} verified non-empty/correct-size, {}x{}, {:?}",
                    stats.frames, checksum_ok, stats.width, stats.height, stats.pixel_format
                );
                println!(
                    "  {:.1} fps, wire {:.2} MB, raw {:.2} MB, LZ4 compression {:.1}x",
                    stats.frames as f64 / stats.seconds.max(1e-6),
                    stats.wire_bytes as f64 / 1_048_576.0,
                    stats.raw_bytes as f64 / 1_048_576.0,
                    stats.raw_bytes as f64 / stats.wire_bytes.max(1) as f64
                );
                println!(
                    "  {} full frames, {} dirty frames; mean dirty frame: {:.0} wire bytes",
                    stats.full_frames,
                    stats.dirty_frames,
                    stats.dirty_wire_bytes as f64 / stats.dirty_frames.max(1) as f64,
                );
                (Some(stats), Some(checksum_ok))
            }
            Err(err) => {
                eprintln!("headless capture failed: {err:#}");
                (None, None)
            }
        };
        let live_resize_ok = if live_resize_tested {
            if resize_flipped {
                println!(
                    "live resize verification: OK — captured geometry switched to {last_dims:?} \
                     via zwlr_output_management"
                );
                true
            } else {
                eprintln!(
                    "live resize verification FAILED: no frame arrived at 800x600 \
                     (last frame {last_dims:?})"
                );
                false
            }
        } else {
            true
        };
        // Teardown in every path.
        match conn.send_msg(&ControlMessage::StopSession {
            session_id: session.to_string(),
        }) {
            Ok(()) => {
                let clean = verify_stopped(&conn, session)?;
                let frame_count = stats.as_ref().map(|s| s.frames);
                return Ok(clean
                    && live_resize_ok
                    && frame_count > Some(0)
                    && checksum_ok == frame_count);
            }
            Err(err) => {
                eprintln!("StopSession send failed after capture end: {err}");
                return Ok(false);
            }
        }
    }

    if dry_run {
        println!("dry-run: holding 2s before StopSession …");
        std::thread::sleep(Duration::from_secs(2));
        conn.send_msg(&ControlMessage::StopSession {
            session_id: session.to_string(),
        })?;
        return verify_stopped(&conn, session);
    }

    // Interactive viewport: full interactivity via the single-owner control
    // pump and teardown ownership (window close + Ctrl+C → StopSession).
    //
    // NOTE: do NOT set_timeouts(None) here — the pump owns the socket from
    // this point and gates inbound reads with select(); a leftover infinite
    // timeout would be fine for select-gated reads but a generous safety
    // bound keeps a mid-frame stall recoverable. The pump re-applies its own
    // 2ms select gate regardless.
    conn.set_timeouts(Some(Duration::from_secs(120)));
    let publisher = Arc::new(renderer::FramePublisher::new());
    let distro_static: &'static str = Box::leak(distro.to_string().into_boxed_str());
    renderer::run_viewport(
        distro_static,
        &format!(
            "WSLDesk — {session} ({}/{renderer}, DX12)",
            compositor_family.label()
        ),
        conn,
        publisher,
        fullscreen,
        (width, height, scale_milli),
        refresh_mhz,
        fps.is_none(),
    )?;
    Ok(true)
}

/// Live regression proof; requires the deployed Ubuntu guest and Sway/foot.
#[cfg(all(test, windows))]
#[test]
#[ignore = "requires Ubuntu, AF_HYPERV, Sway and foot"]
fn live_sway_input() -> Result<()> {
    use common::protocol::{ControlMessage, InputPayload};
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
    use std::time::{Duration, Instant};

    fn query(kind: &str) -> Result<String> {
        let output = std::process::Command::new("wsl.exe")
            .env("WSL_UTF8", "1")
            .args(["-d", "Ubuntu", "--", "sh", "-c", &format!(
                "swaymsg -s /run/user/1000/wsldesk-sway/sway-ipc.*.sock -r -t {kind}"
            )])
            .output()?;
        anyhow::ensure!(output.status.success(), "swaymsg {kind} failed");
        Ok(wsl_cli::decode_wsl_output(&output.stdout))
    }

    let conn = connect_control_plane("Ubuntu", true)?;
    handshake(&conn, "Ubuntu")?;
    conn.send_msg(&ControlMessage::StartSession {
        session_id: "sway".into(), width: 1280, height: 720,
        scale_milli: 1000, no_audio: true, refresh_mhz: common::protocol::default_refresh_mhz(),
    })?;
    if !matches!(conn.recv_msg()?, ControlMessage::SessionStarted { .. }) {
        bail!("Sway did not start");
    }
    let pump = input::ControlPump::start(Arc::new(conn));
    let publisher = Arc::new(renderer::FramePublisher::new());
    let stop = Arc::new(AtomicBool::new(false));
    let video = renderer::spawn_video_receiver(
        "Ubuntu", publisher.clone(), stop.clone(),
        Arc::new(Mutex::new(renderer::VideoStats::default())),
    );
    let proof = (|| -> Result<()> {
        let started = Instant::now();
        while !publisher.peek() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::ensure!(publisher.peek(), "no live video frame");
        let inputs = query("get_inputs")?;
        // Exercise a clipboard offer larger than a normal pipe before input
        // and StopSession. A blocking self-offer used to strand the daemon.
        pump.send(ControlMessage::ClipboardUpdate {
            mime_type: "text/plain;charset=utf-8".into(),
            data: vec![b'x'; 128 * 1024],
        });
        let seats = query("get_seats")?;
        println!("get_inputs: {inputs}\nget_seats: {seats}");
        anyhow::ensure!(seats.contains("seat0")
            && seats.contains("wlr_virtual_keyboard_v1")
            && seats.contains("wlr_virtual_pointer_v1"), "virtual devices missing from seat0");
        anyhow::ensure!(!query("get_tree")?.contains("\"app_id\": \"foot\""),
            "foot already present before input");
        pump.send(ControlMessage::InputEvent(InputPayload::PointerMotionAbsolute {
            x: 320, y: 180, width: 1280, height: 720,
        }));
        for (code, pressed) in [
            (common::keymap::KEY_LEFTMETA, true),
            (common::keymap::KEY_ENTER, true),
            (common::keymap::KEY_ENTER, false),
            (common::keymap::KEY_LEFTMETA, false),
        ] {
            pump.send(ControlMessage::InputEvent(InputPayload::Key { code, pressed }));
        }
        let started = Instant::now();
        loop {
            let tree = query("get_tree")?;
            if tree.contains("\"app_id\": \"foot\"") {
                println!("get_tree after VMBus Super+Enter: {tree}");
                return Ok(());
            }
            anyhow::ensure!(started.elapsed() < Duration::from_secs(5),
                "Super+Enter did not open foot; inspect compositor.log");
            std::thread::sleep(Duration::from_millis(100));
        }
    })();
    let clean = pump.stop_session_and_wait(Duration::from_secs(10));
    stop.store(true, Ordering::SeqCst);
    publisher.close();
    let _ = video.join();
    pump.shutdown();
    proof?;
    anyhow::ensure!(clean == Some(true), "unclean Sway teardown: {clean:?}");
    Ok(())
}

/// Expect + verify the `SessionStopped` reply after a StopSession.
#[cfg(windows)]
fn verify_stopped(conn: &transport::HvConnection, session: &str) -> Result<bool> {
    let stopped = conn.recv_msg()?;
    let common::protocol::ControlMessage::SessionStopped {
        session_id,
        clean_exit,
        detail,
    } = stopped
    else {
        bail!(
            "expected SessionStopped, got {}",
            transport::message_kind_host(&stopped)
        );
    };
    println!(
        "teardown of '{session_id}': {} — {detail}",
        if clean_exit { "CLEAN (zero orphans, runtime dir removed)" } else { "UNCLEAN" }
    );
    let _ = session;
    Ok(clean_exit)
}

/// Non-Windows stubs so `cargo check --workspace` stays green on Linux.
#[cfg(not(windows))]
fn run_ping(distro: &str, _count: u32, _autostart: bool) -> Result<bool> {
    bail!("AF_HYPERV transport is Windows-only (distro '{distro}'); build wsldesk.exe on x86_64-pc-windows-msvc")
}

#[cfg(not(windows))]
fn run_sessions(distro: &str, _autostart: bool) -> Result<bool> {
    bail!("control plane is Windows-only (distro '{distro}'); build wsldesk.exe on x86_64-pc-windows-msvc")
}

#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
fn run_launch(
    distro: &str,
    _session: &str,
    _width: u32,
    _height: u32,
    _scale: f32,
    _autostart: bool,
    _dry_run: bool,
    _headless_frames: Option<u64>,
    _no_audio: bool,
    _fullscreen: bool,
    _debug_input: bool,
    _fps: Option<u32>,
) -> Result<bool> {
    bail!("control plane is Windows-only (distro '{distro}'); build wsldesk.exe on x86_64-pc-windows-msvc")
}

/// Non-Windows stub so `cargo check --workspace` stays green on Linux.
#[cfg(not(windows))]
fn run_ping(distro: &str, _count: u32, _autostart: bool) -> Result<bool> {
    bail!("AF_HYPERV transport is Windows-only (distro '{distro}'); build wsldesk.exe on x86_64-pc-windows-msvc")
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// One diagnostic row of the doctor table.
struct Row {
    check: String,
    status: &'static str,
    detail: String,
}

impl Row {
    fn pass(check: &str, detail: impl Into<String>) -> Self {
        Self { check: check.into(), status: "PASS", detail: detail.into() }
    }
    fn warn(check: &str, detail: impl Into<String>) -> Self {
        Self { check: check.into(), status: "WARN", detail: detail.into() }
    }
    fn fail(check: &str, detail: impl Into<String>) -> Self {
        Self { check: check.into(), status: "FAIL", detail: detail.into() }
    }
}

/// Run diagnostics; returns `Ok(true)` when no row is FAIL.
fn run_doctor(distro: &str) -> Result<bool> {
    let mut rows: Vec<Row> = Vec::new();

    // WSL2 version.
    rows.push(match wsl_cli::check_wsl_version() {
        Ok(v) if v.major >= wsl_cli::MIN_WSL_MAJOR => {
            Row::pass("WSL2 Version", format!("WSL version {}", v.raw))
        }
        Ok(v) => Row::fail(
            "WSL2 Version",
            format!("WSL version {} is below the required 2.0", v.raw),
        ),
        Err(err) => Row::fail("WSL2 Version", err.to_string()),
    });

    // Distro state — WSL1 is a hard fail with a clear remedy.
    let distro_state: Option<Result<String, String>> = match wsl_cli::list_distros() {
        Ok(distros) => match distros.iter().find(|d| d.name == distro) {
            Some(d) if d.version == 2 => Some(Ok(d.state.clone())),
            Some(d) => Some(Err(format!(
                "WSL{version} distro — WSLDesk REQUIRES WSL2 (AF_VSOCK and /dev/dxg are \
                 WSL2-only); convert with `wsl.exe --set-version {distro} 2`",
                version = d.version
            ))),
            None => Some(Err(format!(
                "not installed — create it with `wsldesk setup --distro {distro} --install`"
            ))),
        },
        Err(err) => Some(Err(format!("cannot list distros: {err}"))),
    };
    match distro_state {
        Some(Ok(state)) => rows.push(Row::pass("Target Distro State", format!("state {state}, WSL2"))),
        Some(Err(reason)) => rows.push(Row::fail("Target Distro State", reason)),
        None => unreachable!("distro_state is always Some"),
    }

    // VM GUID (this also boots the distro if stopped).
    rows.push(match wsl_cli::get_vm_id(distro) {
        Ok(guid) => Row::pass("VM GUID (wslinfo --vm-id)", guid),
        Err(err) => Row::warn(
            "VM GUID (wslinfo --vm-id)",
            format!("unavailable: {err} (required for Milestone 2 transport)"),
        ),
    });

    // Default user — an uninitialized distro runs commands as UID 0, which
    // would invalidate the /dev/dxg permission test below.
    rows.push(match wsl_cli::exec_user(distro, &["id", "-u"]) {
        Ok(out) if out.success() => match out.stdout.trim().parse::<u32>() {
            Ok(0) => Row::warn(
                "Default User (non-root)",
                "commands run as UID 0 — distro not initialized; launch it once to create \
                 a user account, otherwise /dev/dxg permissions are not genuinely tested",
            ),
            Ok(uid) => Row::pass("Default User (non-root)", format!("UID {uid}")),
            Err(_) => Row::warn(
                "Default User (non-root)",
                format!("unexpected `id -u` output: {:?}", out.stdout.trim()),
            ),
        },
        Ok(out) => Row::warn(
            "Default User (non-root)",
            format!("`id -u` failed (exit {:?}): {}", out.exit_code, out.stderr.trim()),
        ),
        Err(err) => Row::warn("Default User (non-root)", format!("cannot execute: {err}")),
    });

    // DRM render node — the GPU path wlroots gles2/vulkan actually needs
    // (mesa d3d12 EGL binds /dev/dxg to it). Missing or unreadable = silent
    // pixman software fallback in headless compositors.
    //
    // `test -r` (access(2)) — deliberately NOT `cat`: reading a DRM char
    // device can block indefinitely and hang the whole doctor run.
    rows.push(match wsl_cli::exec_user(
        distro,
        &["sh", "-c", "test -c /dev/dri/renderD128 && test -r /dev/dri/renderD128 && echo OPEN || echo DENIED"],
    ) {
        Ok(out) if out.success() => match out.stdout.trim() {
            "OPEN" => Row::pass(
                "DRM Render Node",
                "/dev/dri/renderD128 accessible — EGL/GBM GPU path available",
            ),
            _ => Row::warn(
                "DRM Render Node",
                "/dev/dri/renderD128 missing or unreadable — compositors silently fall \
                 back to pixman; fix with `sudo usermod -aG render,video <user>` then \
                 `wsl --terminate` the distro",
            ),
        },
        Ok(out) => Row::warn(
            "DRM Render Node",
            format!(
                "probe failed (exit {:?}): {} (renderD128 may be absent — WSLg GPU stack)",
                out.exit_code,
                out.stderr.trim()
            ),
        ),
        Err(err) => Row::warn("DRM Render Node", format!("cannot probe: {err}")),
    });

    // Guest probe: version parity + systemd + dxg + wslg + mesa + sessions.
    let probe_rows = guest_probe_rows(distro);
    rows.extend(probe_rows);

    print_doctor_table(&rows);
    Ok(!rows.iter().any(|r| r.status == "FAIL"))
}

/// Run `wsldesk-guest doctor-probe --json` and turn the report into rows.
///
/// Everything here runs via `exec_user` (the distro's default, non-root
/// user) on purpose: `/dev/dxg` readability must be tested under real user
/// permissions — probing as root would always succeed.
fn guest_probe_rows(distro: &str) -> Vec<Row> {
    // Guest binary presence + version parity first.
    let guest_version = match deployer::query_guest_version(distro) {
        Ok(Some(v)) => Some(v),
        Ok(None) => None,
        Err(err) => {
            return vec![Row::warn(
                "Guest Binary Version Parity",
                format!("cannot run wsldesk-guest --version: {err}"),
            )];
        }
    };
    let parity = guest_version.as_deref().and_then(|v| v.split_whitespace().next_back());
    let parity_row = match parity {
        Some(v) if v == HOST_VERSION => {
            Row::pass("Guest Binary Version Parity", format!("{v} == host {HOST_VERSION}"))
        }
        Some(v) => Row::warn(
            "Guest Binary Version Parity",
            format!("guest {v} != host {HOST_VERSION} — rerun `wsldesk setup`"),
        ),
        None => Row::fail("Guest Binary Version Parity", "no version output from guest"),
    };

    match wsl_cli::exec_user(distro, &["wsldesk-guest", "doctor-probe", "--json"]) {
        Ok(out) if out.success() => match serde_json::from_str::<ProbeReport>(out.stdout.trim())
        {
            Ok(probe) => {
                let mut rows = vec![parity_row];
                rows.push(match probe.pid1_comm.as_deref() {
                    Some("systemd") => Row::pass("Systemd PID 1", "/proc/1/comm = systemd"),
                    Some(other) => Row::warn(
                        "Systemd PID 1",
                        format!(
                            "PID 1 is '{other}' — add [boot] systemd=true to /etc/wsl.conf via `wsldesk setup`"
                        ),
                    ),
                    None => Row::warn("Systemd PID 1", "/proc/1/comm unreadable"),
                });
                rows.push(if probe.has_dxg {
                    Row::pass("/dev/dxg vGPU", "/dev/dxg present and readable (as default user)")
                } else {
                    Row::fail(
                        "/dev/dxg vGPU",
                        "/dev/dxg missing or unreadable by the default user — GPU-backed \
                         Wayland rendering unavailable",
                    )
                });
                rows.push(if probe.wslg_detected {
                    Row::warn(
                        "WSLg Detection",
                        "/mnt/wslg mounted — desktop sessions must stay isolated (Milestone 3)",
                    )
                } else {
                    Row::pass("WSLg Detection", "/mnt/wslg not mounted")
                });
                rows.push(if probe.mesa_d3d12_available {
                    Row::pass("Mesa D3D12", "d3d12_dri.so or GL/Vulkan probe tool found")
                } else {
                    Row::warn(
                        "Mesa D3D12",
                        "no d3d12_dri.so and no glxinfo/vulkaninfo — software rendering likely",
                    )
                });
                if probe.wayland_sessions.is_empty() {
                    rows.push(Row::warn(
                        "Detected Wayland Sessions",
                        "none — install a Wayland DE/WM (e.g. hyprland, sway, plasma, gnome)",
                    ));
                } else {
                    let names: Vec<String> = probe
                        .wayland_sessions
                        .iter()
                        .map(|s| format!("{} ({})", s.id, s.family.label()))
                        .collect();
                    rows.push(Row::pass(
                        "Detected Wayland Sessions",
                        names.join(", "),
                    ));
                }
                rows
            }
            Err(err) => vec![
                parity_row,
                Row::fail("Guest doctor-probe", format!("bad JSON: {err}")),
            ],
        },
        Ok(out) => vec![
            parity_row,
            Row::fail(
                "Guest doctor-probe",
                format!(
                    "wsldesk-guest doctor-probe failed (exit {:?}): {}",
                    out.exit_code,
                    out.stderr.trim()
                ),
            ),
        ],
        Err(err) => vec![
            parity_row,
            Row::warn("Guest doctor-probe", format!("cannot execute: {err}")),
        ],
    }
}

fn print_doctor_table(rows: &[Row]) {
    let check_w = rows.iter().map(|r| r.check.len()).max().unwrap_or(5).max(5);
    let status_w = 5;
    println!(
        "{:<check_w$}  {:<status_w$}  DETAIL",
        "CHECK",
        "STATUS",
        check_w = check_w,
        status_w = status_w
    );
    println!("{}", "-".repeat(check_w + status_w + 12));
    for row in rows {
        println!(
            "{:<check_w$}  {:<status_w$}  {}",
            row.check,
            row.status,
            row.detail,
            check_w = check_w,
            status_w = status_w
        );
    }
}
