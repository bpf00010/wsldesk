//! `wsl.exe` process wrappers (Plan.Md §4.3).
//!
//! Every invocation sets `WSL_UTF8=1` AND keeps a defensive UTF-16LE decoder:
//! older WSL builds ignore the env var and still emit UTF-16LE with a BOM or
//! interleaved NUL bytes.

use std::io::Write;
use std::process::{Child, Command, Stdio};

use thiserror::Error;

/// The WSL command line executable.
pub const WSL_EXE: &str = "wsl.exe";

/// Minimum supported WSL major version (Store WSL 2.0+).
pub const MIN_WSL_MAJOR: u64 = 2;

/// Decoded result of one `wsl.exe` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl WslOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Errors surfaced by the `wsl.exe` wrappers.
#[derive(Debug, Error)]
pub enum WslError {
    #[error("wsl.exe was not found on PATH — install WSL2 first (wsl.exe --install)")]
    NotFound(#[source] std::io::Error),
    #[error("wsl.exe invocation failed: `{args}` (exit {code:?})\nstderr: {stderr}")]
    Failed {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("could not parse wsl.exe output: {0}")]
    Parse(String),
    #[error("io error while running wsl.exe: {0}")]
    Io(#[from] std::io::Error),
}

/// Parsed WSL version triple plus the raw version string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub raw: String,
}

/// One row of `wsl.exe --list --verbose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistroInfo {
    pub name: String,
    pub state: String,
    pub version: u8,
    pub is_default: bool,
}

/// Build a `wsl.exe` command with `WSL_UTF8=1` set.
pub fn wsl_command(args: &[&str]) -> Command {
    let mut cmd = Command::new(WSL_EXE);
    cmd.env("WSL_UTF8", "1");
    cmd.args(args);
    cmd
}

/// Defensive decoder for `wsl.exe` output (Plan.Md §4.3).
///
/// 1. UTF-16LE BOM (`0xFF 0xFE`) → decode UTF-16LE after the BOM.
/// 2. ASCII text re-encoded as UTF-16LE (every second byte `0x00`) → UTF-16LE.
/// 3. Otherwise lossy UTF-8.
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return decode_utf16le(&bytes[2..]);
    }
    if looks_like_utf16le(bytes) {
        return decode_utf16le(bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let pairs = bytes.len() / 2;
    let nul_odd = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|pair| pair[0] != 0x00 && pair[1] == 0x00)
        .count();
    // ASCII-as-UTF-16LE has a NUL in nearly every odd slot; require a strong
    // majority to avoid misdetecting binary output.
    nul_odd * 10 > pairs * 9 && nul_odd > 2
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Run `wsl.exe <args>`, capture and decode output. Does NOT require exit 0.
pub fn run_wsl_capture(args: &[&str]) -> Result<WslOutput, WslError> {
    let output = match wsl_command(args).output() {
        Ok(out) => out,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(WslError::NotFound(err));
        }
        Err(err) => return Err(err.into()),
    };
    Ok(WslOutput {
        stdout: decode_wsl_output(&output.stdout),
        stderr: decode_wsl_output(&output.stderr),
        exit_code: output.status.code(),
    })
}

/// Like [`run_wsl_capture`], but maps non-zero exits to [`WslError::Failed`].
pub fn run_wsl_checked(args: &[&str]) -> Result<WslOutput, WslError> {
    let out = run_wsl_capture(args)?;
    if !out.success() {
        return Err(WslError::Failed {
            args: format!("{WSL_EXE} {}", args.join(" ")),
            code: out.exit_code,
            stderr: out.stderr.trim().to_string(),
        });
    }
    Ok(out)
}

/// Run `wsl.exe <args>`, streaming `stdin_bytes` into the child process before
/// collecting output. Used to ship the guest binary into the distro without
/// touching `/mnt/c` (Plan.Md §4.3, atomic `cat > tmp && mv` deployment).
pub fn run_wsl_with_stdin(
    args: &[&str],
    stdin_bytes: &[u8],
) -> Result<WslOutput, WslError> {
    let mut cmd = wsl_command(args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    write_stdin(&mut child, stdin_bytes)?;
    finish(child)
}

fn write_stdin(child: &mut Child, stdin_bytes: &[u8]) -> Result<(), WslError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| WslError::Parse("child stdin unavailable".into()))?;
    // The `cat`-style consumers used here do not emit output while reading, so
    // writing the whole payload before draining pipes cannot deadlock.
    stdin.write_all(stdin_bytes)?;
    stdin.flush()?;
    drop(stdin); // close the pipe so the reader sees EOF
    Ok(())
}

fn finish(child: Child) -> Result<WslOutput, WslError> {
    let output = child.wait_with_output()?;
    Ok(WslOutput {
        stdout: decode_wsl_output(&output.stdout),
        stderr: decode_wsl_output(&output.stderr),
        exit_code: output.status.code(),
    })
}

/// Verify Store WSL 2.0+ is installed (Plan.Md §5 M1).
pub fn check_wsl_version() -> Result<WslVersion, WslError> {
    let out = run_wsl_capture(&["--version"])?;
    if !out.success() {
        return Err(WslError::Failed {
            args: format!("{WSL_EXE} --version"),
            code: out.exit_code,
            stderr: out.stderr.trim().to_string(),
        });
    }
    parse_wsl_version(&out.stdout)
        .ok_or_else(|| WslError::Parse(format!("no version line in {:?}", out.stdout)))
}

fn parse_wsl_version(text: &str) -> Option<WslVersion> {
    for line in text.lines() {
        let line = line.trim();
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        if !label.to_ascii_lowercase().contains("version") {
            continue;
        }
        let raw = value.trim().to_string();
        let mut parts = raw.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
        return Some(WslVersion {
            major,
            minor,
            patch,
            raw,
        });
    }
    None
}

/// Parse `wsl.exe --list --verbose` into distro rows (Plan.Md §5 M1).
pub fn list_distros() -> Result<Vec<DistroInfo>, WslError> {
    let out = run_wsl_checked(&["--list", "--verbose"])?;
    parse_list_verbose(&out.stdout)
        .ok_or_else(|| WslError::Parse("failed to parse --list --verbose output".into()))
}

/// Parse the table produced by `wsl.exe --list --verbose` (UTF-8 or UTF-16LE).
pub fn parse_list_verbose(text: &str) -> Option<Vec<DistroInfo>> {
    let mut distros = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("NAME") {
            continue; // header row
        }
        let is_default = trimmed.starts_with('*');
        let body = trimmed.trim_start_matches('*').trim();
        let mut tokens: Vec<&str> = body.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        let version = tokens.pop()?;
        let state = tokens.pop()?.to_string();
        let name = tokens.join(" ");
        if name.is_empty() {
            continue;
        }
        let version = match version.parse::<u8>() {
            Ok(v @ 1..=2) => v,
            _ => continue, // not a distro row (e.g. localized header)
        };
        distros.push(DistroInfo {
            name,
            state,
            version,
            is_default,
        });
    }
    Some(distros)
}

/// Trigger `wsl.exe --install -d <name> --no-launch` (Plan.Md §5 M1).
pub fn install_distro(name: &str) -> Result<WslOutput, WslError> {
    run_wsl_checked(&["--install", "-d", name, "--no-launch"])
}

/// Fetch the utility-VM GUID via `wslinfo --vm-id`, trimming whitespace and
/// `{}` braces (Plan.Md §4.2). This GUID identifies the AF_HYPERV peer.
pub fn get_vm_id(distro: &str) -> Result<String, WslError> {
    let out = run_wsl_checked(&["-d", distro, "--", "wslinfo", "--vm-id"])?;
    Ok(clean_guid(&out.stdout))
}

/// Trim whitespace plus `{}` braces from a GUID string.
pub fn clean_guid(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim()
        .to_string()
}

/// Run a `sh -c` command as root inside the distro, optionally piping stdin.
pub fn exec_root(
    distro: &str,
    sh_cmd: &str,
    stdin_bytes: Option<&[u8]>,
) -> Result<WslOutput, WslError> {
    match stdin_bytes {
        Some(bytes) => run_wsl_with_stdin(
            &["-d", distro, "-u", "root", "--", "sh", "-c", sh_cmd],
            bytes,
        ),
        None => run_wsl_capture(&["-d", distro, "-u", "root", "--", "sh", "-c", sh_cmd]),
    }
}

/// Run an executable as the default user inside the distro.
pub fn exec_user(distro: &str, args: &[&str]) -> Result<WslOutput, WslError> {
    let mut full = Vec::with_capacity(args.len() + 3);
    full.extend_from_slice(&["-d", distro, "--"]);
    full.extend_from_slice(args);
    run_wsl_capture(&full)
}

/// `wsl.exe --terminate <distro>` (used after writing systemd=true).
pub fn terminate_distro(distro: &str) -> Result<WslOutput, WslError> {
    run_wsl_checked(&["--terminate", distro])
}

/// `wsl.exe --shutdown` — stops ALL distros and the utility VM.
///
/// REQUIRED for `.wslconfig` changes to take effect (a per-distro
/// `--terminate` is not enough). DANGER: this stops every running distro
/// (e.g. `docker-desktop`). Only call after the user explicitly opted in via
/// the `--shutdown` flag — never implicitly (Plan.Md guardrail: no silent
/// destructive edits).
pub fn shutdown() -> Result<WslOutput, WslError> {
    run_wsl_checked(&["--shutdown"])
}

/// Spawn `wsl.exe -d <distro> -- <args>` fire-and-forget with stdio silenced.
///
/// Used by `ping --autostart` to launch `wsldesk-guest daemon --once` on
/// demand without waiting for it (the child outlives this process handle and
/// exits by itself once the one connection is served).
pub fn spawn_user(distro: &str, args: &[&str]) -> Result<(), WslError> {
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 3);
    full.extend_from_slice(&["-d", distro, "--"]);
    full.extend_from_slice(args);
    wsl_command(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => WslError::NotFound(err),
            _ => WslError::Io(err),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_plain_utf8() {
        assert_eq!(decode_wsl_output(b"hello\r\n"), "hello\r\n");
    }

    #[test]
    fn decode_utf16le_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "Ubuntu\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_wsl_output(&bytes), "Ubuntu\r\n");
    }

    #[test]
    fn decode_utf16le_without_bom() {
        // Older wsl.exe emits UTF-16LE without BOM for ASCII-heavy output.
        let mut bytes = Vec::new();
        for unit in "NAME STATE\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_wsl_output(&bytes), "NAME STATE\r\n");
    }

    #[test]
    fn decode_binary_falls_back_to_lossy_utf8() {
        // Random-ish bytes without a NUL majority must stay UTF-8.
        let bytes = [0x61, 0x62, 0x63, 0x64, 0xFF, 0x65];
        assert_eq!(decode_wsl_output(&bytes), String::from_utf8_lossy(&bytes));
    }

    #[test]
    fn parse_wsl_version_line() {
        let v = parse_wsl_version("WSL version: 2.4.13.0\r\nKernel version: 5.15\r\n")
            .expect("parsed");
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 4);
        assert_eq!(v.patch, 13);
        assert_eq!(v.raw, "2.4.13.0");
        assert!(parse_wsl_version("garbage").is_none());
    }

    #[test]
    fn parse_list_verbose_standard_layout() {
        let text = "  NAME            STATE           VERSION\r\n\
                    * Ubuntu-24.04    Running         2\r\n\
                      Debian-12       Stopped         1\r\n\
                      Fedora-40       Installing      2\r\n";
        let distros = parse_list_verbose(text).unwrap();
        assert_eq!(distros.len(), 3);
        assert_eq!(
            distros[0],
            DistroInfo {
                name: "Ubuntu-24.04".into(),
                state: "Running".into(),
                version: 2,
                is_default: true,
            }
        );
        assert_eq!(distros[1].version, 1);
        assert!(!distros[1].is_default);
        assert_eq!(distros[2].state, "Installing");
    }

    #[test]
    fn parse_list_verbose_tolerates_distro_names_with_spaces() {
        let text = "  NAME            STATE           VERSION\r\n\
                    * Arch Linux      Running         2\r\n";
        let distros = parse_list_verbose(text).unwrap();
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "Arch Linux");
    }

    #[test]
    fn parse_list_verbose_distinguishes_wsl1_and_wsl2() {
        let text = "  NAME            STATE           VERSION\r\n\
                    * Ubuntu-24.04    Running         2\r\n\
                      Debian-12       Stopped         1\r\n";
        let distros = parse_list_verbose(text).unwrap();
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0].version, 2, "WSL2 distro");
        assert_eq!(distros[1].version, 1, "WSL1 distro must be flagged as 1");
        assert_ne!(distros[0].version, distros[1].version);
    }

    #[test]
    fn parse_list_verbose_preserves_default_marker_only_for_starred_row() {
        let text = "  NAME            STATE           VERSION\r\n\
                      A               Stopped         2\r\n\
                    * B               Stopped         2\r\n";
        let distros = parse_list_verbose(text).unwrap();
        assert!(!distros[0].is_default, "no leading '*' means not default");
        assert!(distros[1].is_default, "'*' marks the default distro");
        // the '*' must never leak into the name
        assert_eq!(distros[1].name, "B");
    }

    #[test]
    fn parse_list_verbose_handles_utf16_output_end_to_end() {
        // Full pipeline: wsl.exe UTF-16LE output → decode_wsl_output → parse.
        let text = "  NAME            STATE           VERSION\r\n\
                    * Ubuntu          Stopped         2\r\n\
                      docker-desktop  Stopped         2\r\n";
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_wsl_output(&bytes);
        let distros = parse_list_verbose(&decoded).expect("parsed");
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0].name, "Ubuntu");
        assert!(distros[0].is_default);
        assert_eq!(distros[1].name, "docker-desktop");
        assert!(!distros[1].is_default);
    }

    #[test]
    fn parse_list_verbose_keeps_unknown_states() {
        let text = "  NAME            STATE           VERSION\r\n\
                      Ubuntu          Installing      2\r\n\
                      Fedora          Converting      2\r\n";
        let distros = parse_list_verbose(text).unwrap();
        assert_eq!(distros[0].state, "Installing");
        assert_eq!(distros[1].state, "Converting");
    }

    #[test]
    fn parse_list_verbose_empty_input() {
        assert!(parse_list_verbose("").unwrap().is_empty());
    }

    #[test]
    fn clean_guid_strips_braces_and_whitespace() {
        assert_eq!(clean_guid("  {1a2b3c4d-1111-2222-3333-444455556666} \n"),
                   "1a2b3c4d-1111-2222-3333-444455556666");
        assert_eq!(clean_guid("1a2b3c4d-1111-2222-3333-444455556666\r\n"),
                   "1a2b3c4d-1111-2222-3333-444455556666");
    }
}
