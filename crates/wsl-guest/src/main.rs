//! `wsldesk-guest` — static Linux guest CLI (Plan.Md §5).
//!
//! Milestone 1 scope: `--version` and `doctor-probe --json`.
//! Milestone 2 adds `daemon` (AF_VSOCK listener on port 50001), Milestone 3
//! adds `list-sessions`/`exec-session`, Milestone 4 adds frame capture.
//!
//! Built as a 100% statically linked `x86_64-unknown-linux-musl` binary; the
//! non-Linux stub below keeps `cargo check --workspace` green on Windows.

#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod env_isolate;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod sessions;
#[cfg(target_os = "linux")]
mod transport;
#[cfg(target_os = "linux")]
mod wayland_bridge;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use clap::Parser;

    #[derive(Parser)]
    #[command(
        name = "wsldesk-guest",
        version,
        about = "WSLDesk guest-side bridge daemon (static musl binary)"
    )]
    struct Cli {
        #[command(subcommand)]
        command: Option<Command>,
    }

    #[derive(clap::Subcommand)]
    enum Command {
        /// Diagnose systemd PID 1, /dev/dxg, WSLg, Mesa D3D12 and Wayland sessions.
        DoctorProbe {
            /// Print machine-readable JSON (parsed by `wsldesk.exe doctor`).
            #[arg(long)]
            json: bool,
        },
        /// Listen on AF_VSOCK port 50001 for the Windows host control plane.
        Daemon {
            /// Serve exactly one connection, then exit. Used by
            /// `wsldesk.exe ping --autostart` so ping works without systemd.
            #[arg(long)]
            once: bool,
        },
        /// List installable Wayland sessions (local, no host connection).
        ListSessions,
        /// [internal] Enter the isolated namespace and exec a compositor.
        /// Spawned by the daemon's StartSession handler — not for
        /// interactive use.
        #[command(hide = true)]
        ExecSession {
            /// Session id (as listed by list-sessions).
            session_id: String,
            #[arg(long, default_value_t = 1920)]
            width: u32,
            #[arg(long, default_value_t = 1080)]
            height: u32,
            #[arg(long, default_value_t = 1000)]
            scale_milli: u32,
            /// Strip the WSLg audio passthrough (PULSE_SERVER).
            #[arg(long, default_value_t = false)]
            no_audio: bool,
        },
    }

    match Cli::parse().command {
        Some(Command::DoctorProbe { json }) => {
            let report = probe::collect();
            if json {
                println!("{}", serde_json::to_string(&report).expect("probe JSON serializes"));
            } else {
                print_human(&report);
            }
            std::process::ExitCode::SUCCESS
        }
        Some(Command::Daemon { once }) => match transport::daemon_main(once) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("wsldesk-guest daemon: {err:#}");
                std::process::ExitCode::from(1)
            }
        },
        Some(Command::ListSessions) => {
            let sessions = sessions::available_sessions();
            if sessions.is_empty() {
                println!("no Wayland sessions found (no .desktop entries, no known compositor binaries)");
            } else {
                println!("{:<20} {:<10} NAME", "ID", "FAMILY");
                for s in &sessions {
                    println!("{:<20} {:<10} {}", s.id, s.family.label(), s.name);
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Some(Command::ExecSession {
            session_id,
            width,
            height,
            scale_milli,
            no_audio,
        }) => {
            let Some(info) = sessions::find_session(&session_id) else {
                eprintln!("wsldesk-guest exec-session: session '{session_id}' not found");
                return std::process::ExitCode::from(66);
            };
            let spec = env_isolate::SessionSpec {
                id: session_id,
                family: info.family,
                exec: info.exec,
                width,
                height,
                scale_milli,
                no_audio,
            };
            let code = env_isolate::run_isolated_session(&spec);
            std::process::ExitCode::from(code.min(u8::MAX as i32) as u8)
        }
        None => {
            eprintln!("wsldesk-guest: pass a subcommand — try --help, `doctor-probe --json`, or `daemon`.");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(target_os = "linux")]
fn print_human(report: &common::ProbeReport) {
    println!(
        "pid1_comm:            {}",
        report.pid1_comm.as_deref().unwrap_or("<unreadable>")
    );
    println!("systemd_active:       {}", report.systemd_active());
    println!("has_dxg:              {}", report.has_dxg);
    println!("wslg_detected:        {}", report.wslg_detected);
    println!("mesa_d3d12_available: {}", report.mesa_d3d12_available);
    println!("wayland_sessions ({}):", report.wayland_sessions.len());
    for s in &report.wayland_sessions {
        println!(
            "  - {:<24} name={:?} exec={:?} family={} desktop_names={:?}",
            s.id, s.name, s.exec, s.family.label(), s.desktop_names
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("wsldesk-guest must be built for x86_64-unknown-linux-musl (see Plan.Md §3)");
}
