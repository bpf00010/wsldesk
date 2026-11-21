//! Static guest binary deployment (Plan.Md §4.3).
//!
//! Never relies on `/mnt/c` automount or `\\wsl$` UNC paths: the guest binary
//! is streamed through stdin of `wsl.exe -u root -- sh -c 'cat > tmp && mv'`
//! for an atomic replacement (safe against ETXTBSY with a running instance).
//! SHA-256 is used to skip redeployment when the remote copy matches.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::wsl_cli;

/// Install path of the guest binary inside the distro.
pub const GUEST_INSTALL_PATH: &str = "/usr/local/bin/wsldesk-guest";
/// Name of the guest binary on the build host.
pub const GUEST_BIN_NAME: &str = "wsldesk-guest";

fn checked_output(out: wsl_cli::WslOutput, operation: &str) -> Result<()> {
    if !out.success() {
        bail!("{operation} failed: {} {}", out.stdout.trim(), out.stderr.trim());
    }
    Ok(())
}

pub fn ensure_graphics_groups(distro: &str) -> Result<()> {
    let out = wsl_cli::exec_user(distro, &["id", "-un"])?;
    let user = out.stdout.trim();
    if !out.success() || user.is_empty() || user == "root"
        || !user.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        bail!("Set a default non-root user in '{distro}' before running WSLDesk.");
    }
    let cmd = format!(
        "set -eu\nfor group in render video; do getent group \"$group\" >/dev/null || groupadd -f \"$group\"; done\nusermod -aG render,video -- '{user}'\n"
    );
    checked_output(wsl_cli::run_wsl_with_stdin(
        &["-d", distro, "-u", "root", "--", "sh", "-s"],
        cmd.as_bytes(),
    )?, "graphics group setup")
}

pub fn install_desktop(distro: &str) -> Result<()> {
    let packages = include_str!("../../../scripts/desktop-packages.sh").replace("\r\n", "\n");
    checked_output(wsl_cli::run_wsl_with_stdin(
        &["-d", distro, "-u", "root", "--", "sh", "-s"],
        packages.as_bytes(),
    )?, "desktop package installation")?;
    let config = include_str!("../../../scripts/desktop-config.sh").replace("\r\n", "\n");
    checked_output(wsl_cli::run_wsl_with_stdin(
        &["-d", distro, "--", "sh", "-s", "starter"],
        config.as_bytes(),
    )?, "starter desktop configuration")
}
/// Temporary file used for the atomic move.
const GUEST_TMP_PATH: &str = "/usr/local/bin/wsldesk-guest.tmp";
/// systemd user unit installed with `--enable-service`.
const GUEST_SERVICE_UNIT: &str = "/etc/systemd/user/wsldesk-guest.service";

/// Where the guest binary was resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestBinSource {
    #[cfg(feature = "embed-guest")]
    Embedded,
    ExplicitArg,
    Sidecar,
    CargoTarget,
}

impl GuestBinSource {
    /// Short human label for setup output.
    pub fn label(&self) -> &'static str {
        match self {
            #[cfg(feature = "embed-guest")]
            GuestBinSource::Embedded => "embedded in wsldesk.exe",
            GuestBinSource::ExplicitArg => "--guest-bin",
            GuestBinSource::Sidecar => "sidecar (next to wsldesk.exe)",
            GuestBinSource::CargoTarget => "cargo musl target dir",
        }
    }
}

/// Outcome of a `deploy_guest` run.
#[derive(Debug, Clone)]
pub struct DeployReport {
    pub bin_path: PathBuf,
    pub source: GuestBinSource,
    pub local_sha256: String,
    pub remote_sha256_before: Option<String>,
    pub deployed: bool,
    /// Output of `wsldesk-guest --version` after deployment.
    pub guest_version: Option<String>,
}

/// Resolve the guest binary path (Plan.Md §5 M1), in priority order:
/// 1. explicit `--guest-bin <path>`;
/// 2. sidecar next to `wsldesk.exe`;
/// 3. cargo target dir `target/x86_64-unknown-linux-musl/release/wsldesk-guest`.
pub fn resolve_guest_bin(explicit: Option<&Path>) -> Result<(PathBuf, GuestBinSource)> {
    let mut tried = Vec::new();
    if let Some(explicit) = explicit {
        if explicit.exists() {
            return Ok((explicit.to_path_buf(), GuestBinSource::ExplicitArg));
        }
        bail!("explicit guest binary does not exist: {}", explicit.display());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sidecar = dir.join(GUEST_BIN_NAME);
            if sidecar.exists() {
                return Ok((sidecar, GuestBinSource::Sidecar));
            }
            tried.push(sidecar);
            // cargo puts us in <workspace>/target/{debug,release}; the musl
            // guest lives in <workspace>/target/x86_64-unknown-linux-musl/release.
            let mut dir = dir.to_path_buf();
            for _ in 0..3 {
                if let Some(parent) = dir.parent() {
                    dir = parent.to_path_buf();
                    let candidate = dir
                        .join("target")
                        .join("x86_64-unknown-linux-musl")
                        .join("release")
                        .join(GUEST_BIN_NAME);
                    if candidate.exists() {
                        return Ok((candidate, GuestBinSource::CargoTarget));
                    }
                    tried.push(candidate);
                }
            }
        }
    }
    bail!(
        "wsldesk-guest binary not found (tried {:?}); build it first with \
         `cargo build -p wsl-guest --release --target x86_64-unknown-linux-musl`",
        tried
    )
}

/// SHA-256 of in-memory bytes as lowercase hex.
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// Query the remote guest binary SHA-256 (first token of `sha256sum`), or
/// `None` when the file is missing.
pub fn remote_sha256(distro: &str) -> Result<Option<String>> {
    let cmd = format!("sha256sum {GUEST_INSTALL_PATH} 2>/dev/null || true");
    let out = wsl_cli::exec_root(distro, &cmd, None)?;
    let line = out.stdout.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        return Ok(None);
    }
    let sha = line
        .split_whitespace()
        .next()
        .context("empty sha256sum output")?
        .to_ascii_lowercase();
    Ok(Some(sha))
}

/// Fetch the guest's reported version via `wsldesk-guest --version`.
pub fn query_guest_version(distro: &str) -> Result<Option<String>> {
    let out = wsl_cli::exec_user(distro, &[GUEST_BIN_NAME, "--version"])?;
    if !out.success() {
        return Ok(None);
    }
    Ok(Some(out.stdout.trim().to_string()))
}

/// Verify local/remote SHA-256 and atomically deploy when mismatched
/// (Plan.Md §4.3). Returns a full report; `verified_version` confirms the
/// deployed binary actually executes.
pub fn deploy_guest(distro: &str, explicit: Option<&Path>) -> Result<DeployReport> {
    #[cfg(feature = "embed-guest")]
    if explicit.is_none() {
        return deploy_bytes(
            distro, Path::new(GUEST_BIN_NAME), GuestBinSource::Embedded,
            include_bytes!("../../../target/x86_64-unknown-linux-musl/release/wsldesk-guest"),
        );
    }
    let (bin_path, source) = resolve_guest_bin(explicit)?;
    let local_bytes = fs::read(&bin_path)
        .with_context(|| format!("failed to read guest binary {}", bin_path.display()))?;
    deploy_bytes(distro, &bin_path, source, &local_bytes)
}

fn deploy_bytes(distro: &str, bin_path: &Path, source: GuestBinSource, local_bytes: &[u8]) -> Result<DeployReport> {
    let local_sha = hex_sha256(local_bytes);
    let remote_before = remote_sha256(distro)?;

    let deployed = if remote_before.as_deref() == Some(local_sha.as_str()) {
        false
    } else {
        let cmd = format!(
            "cat > {GUEST_TMP_PATH} && chmod 0755 {GUEST_TMP_PATH} && mv -f {GUEST_TMP_PATH} {GUEST_INSTALL_PATH}"
        );
        let out = wsl_cli::exec_root(distro, &cmd, Some(local_bytes))?;
        if !out.success() {
            bail!(
                "guest deployment failed inside {distro}: {}",
                out.stderr.trim()
            );
        }
        // Verify the hash of what actually landed.
        let after = remote_sha256(distro)?
            .context("remote sha256 missing after deployment")?;
        if after != local_sha {
            bail!("deployed binary hash mismatch: remote {after}, local {local_sha}");
        }
        true
    };

    let guest_version = query_guest_version(distro)?;
    Ok(DeployReport {
        bin_path: bin_path.to_path_buf(),
        source,
        local_sha256: local_sha,
        remote_sha256_before: remote_before,
        deployed,
        guest_version,
    })
}

/// systemd user unit content for the guest daemon (Milestone 2 activates it).
pub fn service_unit() -> String {
    format!(
        "# Installed by wsldesk setup — managed manually, edits preserved.\n\
         [Unit]\n\
         Description=WSLDesk guest bridge (VSOCK control plane on port {port})\n\
         After=default.target\n\n\
         [Service]\n\
         ExecStart={install} daemon\n\
         Restart=on-failure\n\
         RestartSec=2\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        port = common::protocol::CONTROL_PORT,
        install = GUEST_INSTALL_PATH,
    )
}

/// Enable command used for the unit. `systemctl --global enable` runs as root
/// and creates the enable symlink for EVERY user's future systemd instance —
/// `systemctl --user enable` run as root would target root's own (usually
/// bus-less) instance and is wrong here.
pub const GLOBAL_ENABLE_CMD: &str = "systemctl --global enable wsldesk-guest.service";

/// Outcome of `install_guest_service`.
///
/// `Ok(_)` implies the unit file was written (the function bails otherwise).
#[derive(Debug, Clone)]
pub struct ServiceInstallReport {
    /// Absolute unit path inside the distro.
    pub unit_path: &'static str,
    /// `systemctl --global enable` succeeded.
    pub global_enabled: bool,
    /// Default user resolved for lingering (None when undeterminable).
    pub linger_user: Option<String>,
    /// `loginctl enable-linger <user>` succeeded.
    pub linger_enabled: bool,
}

/// Resolve the distro's default (non-root) username.
///
/// Preference order: `/etc/wsl.conf` `[user] default=<name>`, else the account
/// owning UID 1000. Returns `None` when neither resolves (uninitialized
/// distro).
pub fn default_username(distro: &str) -> Option<String> {
    // [user] default= line (any section-tolerant grep; WSL itself is
    // case-insensitive about keys).
    let grep_cmd = "grep -i '^[[:space:]]*default[[:space:]]*=' /etc/wsl.conf 2>/dev/null \
                    | head -n1 | cut -d'=' -f2 | tr -d '[:space:]'";
    if let Ok(out) = wsl_cli::exec_root(distro, grep_cmd, None) {
        let name = out.stdout.trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    if let Ok(out) = wsl_cli::exec_root(distro, "id -un 1000 2>/dev/null || true", None) {
        let name = out.stdout.trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Install the user systemd unit and enable it for real users.
///
/// 1. Atomic write of `/etc/systemd/user/wsldesk-guest.service` (root).
/// 2. `systemctl --global enable wsldesk-guest.service` — as root this is the
///    correct way to enable a user unit; `--user` as root would only affect
///    root's own instance (and usually fails: no user bus).
/// 3. `loginctl enable-linger <default-user>` so the user systemd manager
///    starts at VM boot without a login session.
///
/// Steps 2 and 3 are best-effort (reported, never fatal): minimal or
/// not-yet-booted systemd distros may lack `systemctl`/`loginctl`, and the
/// user can enable manually later.
pub fn install_guest_service(distro: &str) -> Result<ServiceInstallReport> {
    // 1. Unit file (atomic: tmp + chmod + mv).
    let unit = service_unit();
    let write_cmd = format!(
        "cat > {GUEST_SERVICE_UNIT} && chmod 0644 {GUEST_SERVICE_UNIT}"
    );
    let out = wsl_cli::exec_root(distro, &write_cmd, Some(unit.as_bytes()))?;
    if !out.success() {
        bail!(
            "failed to install {GUEST_SERVICE_UNIT} inside {distro}: {}",
            out.stderr.trim()
        );
    }

    // 2. Global enable (root, affects all users).
    let global_enabled = wsl_cli::exec_root(distro, GLOBAL_ENABLE_CMD, None)
        .map(|o| o.success())
        .unwrap_or(false);

    // 3. Linger for the default user.
    let linger_user = default_username(distro);
    let linger_enabled = match &linger_user {
        Some(user) => wsl_cli::exec_root(
            distro,
            &format!("loginctl enable-linger '{user}'"),
            None,
        )
        .map(|o| o.success())
        .unwrap_or(false),
        None => false,
    };

    Ok(ServiceInstallReport {
        unit_path: GUEST_SERVICE_UNIT,
        global_enabled,
        linger_user,
        linger_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex_sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn service_unit_references_installed_path_and_control_port() {
        let unit = service_unit();
        assert!(unit.contains("ExecStart=/usr/local/bin/wsldesk-guest daemon"));
        assert!(unit.contains("port 50001"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn global_enable_uses_global_scope_not_user_scope() {
        // Running as root, `systemctl --user` would target root's instance;
        // the deployment must use `--global`.
        assert!(GLOBAL_ENABLE_CMD.contains("--global"));
        assert!(!GLOBAL_ENABLE_CMD.contains("--user"));
        assert!(GLOBAL_ENABLE_CMD.ends_with("wsldesk-guest.service"));
    }
}
