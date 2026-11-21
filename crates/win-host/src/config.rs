//! Non-destructive INI patcher for `.wslconfig` and `/etc/wsl.conf`
//! (Plan.Md §5 M1).
//!
//! Hand-rolled line-preserving editor: comments, blank lines, unknown keys and
//! user formatting are untouched. Every write is preceded by a timestamped
//! backup and every modification is logged as an exact line diff.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::wsl_cli;

/// One edit performed by [`IniEdit`], rendered as an exact line diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniChange {
    pub line: String,
    pub note: String,
}

/// Line-preserving INI editor. Lines keep their original endings (CRLF or LF).
#[derive(Debug, Clone, Default)]
pub struct IniEdit {
    lines: Vec<String>,
    changes: Vec<IniChange>,
}

impl IniEdit {
    /// Parse INI text while preserving every line (including its terminator).
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.split_inclusive('\n').map(str::to_string).collect(),
            changes: Vec::new(),
        }
    }

    /// True once any modification has been recorded.
    pub fn is_modified(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Recorded changes (exact lines).
    pub fn changes(&self) -> &[IniChange] {
        &self.changes
    }

    /// Render back to text. Lines carry their original terminators; a missing
    /// final newline is added when lines were appended.
    pub fn render(&self) -> String {
        self.lines.concat()
    }

    /// Ensure a `[section]` header exists (case-insensitive match), appending
    /// it at the end when missing.
    pub fn ensure_section(&mut self, section: &str) {
        if self.find_section(section).is_some() {
            return;
        }
        let header = format!("[{section}]\n");
        if let Some(last) = self.lines.last_mut() {
            if !last.ends_with('\n') {
                last.push('\n');
            }
        }
        self.changes.push(IniChange {
            line: header.trim_end().to_string(),
            note: format!("added section [{section}]"),
        });
        self.lines.push(header);
    }

    /// Ensure `key = value` exists inside `section` (case-insensitive section
    /// and key match, mirroring WSL's own tolerance). Existing lines are
    /// updated in place; missing keys are appended at the end of the section.
    pub fn ensure_key(&mut self, section: &str, key: &str, value: &str) {
        let Some(sec_start) = self.find_section(section) else {
            self.ensure_section(section);
            // ensure_section either found or appended the section.
            let idx = self
                .find_section(section)
                .expect("section must exist after ensure_section");
            self.insert_key_after_section(idx, section, key, value);
            return;
        };
        let sec_end = self.section_end(sec_start);
        // Prefer the last occurrence of the key inside the section.
        let mut key_line: Option<usize> = None;
        for (i, line) in self.lines[sec_start + 1..sec_end].iter().enumerate() {
            if let Some((k, _)) = split_key_value(line) {
                if k.eq_ignore_ascii_case(key) {
                    key_line = Some(sec_start + 1 + i);
                }
            }
        }
        let new_line = format!("{key} = {value}\n");
        match key_line {
            Some(idx) => {
                let current = split_key_value(&self.lines[idx])
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default();
                if current.eq_ignore_ascii_case(value) {
                    return; // already correct — never touch the file
                }
                let old = self.lines[idx].trim_end().to_string();
                let new_trimmed = new_line.trim_end().to_string();
                self.lines[idx] = new_line;
                self.changes.push(IniChange {
                    line: format!("- {old}\n+ {new_trimmed}"),
                    note: format!("updated [{section}] {key}"),
                });
            }
            None => self.insert_key_after_section(sec_start, section, key, value),
        }
    }

    fn insert_key_after_section(&mut self, sec_start: usize, section: &str, key: &str, value: &str) {
        let insert_at = self.section_end(sec_start);
        let new_line = format!("{key} = {value}\n");
        self.lines.insert(insert_at, new_line.clone());
        self.changes.push(IniChange {
            line: format!("+ {}", new_line.trim_end()),
            note: format!("added [{section}] {key}"),
        });
    }

    /// Normalize every line ending to LF (`\n`) and guarantee a final newline.
    /// Mandatory for `/etc/wsl.conf`: it is read by systemd inside the Linux
    /// distro, where stray CR characters would corrupt config values.
    pub fn enforce_lf(&mut self) {
        let mut changed = 0usize;
        for line in &mut self.lines {
            if line.ends_with("\r\n") {
                let base = line[..line.len() - 2].to_string();
                *line = base + "\n";
                changed += 1;
            }
        }
        if let Some(last) = self.lines.last_mut() {
            if !last.ends_with('\n') {
                last.push('\n');
                changed += 1;
            }
        }
        if changed > 0 {
            self.changes.push(IniChange {
                line: format!("normalized {changed} line ending(s) to LF"),
                note: "enforced LF line endings".into(),
            });
        }
    }

    /// Normalize every line ending to CRLF (`\r\n`) and guarantee a final
    /// newline. Used for `%USERPROFILE%\.wslconfig`, which is read by
    /// Windows-native tooling that expects CRLF.
    pub fn enforce_crlf(&mut self) {
        let mut changed = 0usize;
        for line in &mut self.lines {
            if line.ends_with('\n') && !line.ends_with("\r\n") {
                let base = line[..line.len() - 1].to_string();
                *line = base + "\r\n";
                changed += 1;
            }
        }
        if let Some(last) = self.lines.last_mut() {
            if !last.ends_with('\n') {
                last.push_str("\r\n");
                changed += 1;
            }
        }
        if changed > 0 {
            self.changes.push(IniChange {
                line: format!("normalized {changed} line ending(s) to CRLF"),
                note: "enforced CRLF line endings".into(),
            });
        }
    }

    /// Index of the `[section]` header line, case-insensitive.
    fn find_section(&self, section: &str) -> Option<usize> {
        self.lines.iter().position(|line| {
            let t = line.trim();
            t.starts_with('[')
                && t.ends_with(']')
                && t[1..t.len() - 1].trim().eq_ignore_ascii_case(section)
        })
    }

    /// Exclusive end index of the section starting at `start` (next section
    /// header or end of file).
    fn section_end(&self, start: usize) -> usize {
        let mut end = self.lines.len();
        for (i, line) in self.lines.iter().enumerate().skip(start + 1) {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') {
                end = i;
                break;
            }
        }
        end
    }
}

/// Split `key = value`; returns `(key, value)` only for non-comment lines.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with(';') || t.starts_with('[') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    Some((k.trim(), v))
}

/// UTC-seconds timestamp used in backup file names.
fn unix_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u128)
        .unwrap_or(0)
}

/// Result of patching the user's `%USERPROFILE%\.wslconfig`.
#[derive(Debug, Clone)]
pub struct WslConfigPatch {
    pub path: PathBuf,
    pub changed: bool,
    pub backup_path: Option<PathBuf>,
    pub diff: Vec<String>,
}

/// Patch `%USERPROFILE%\.wslconfig` (Plan.Md §5 M1):
/// - timestamped backup `.wslconfig.wsldesk.<unix_ts>.bak` before any write;
/// - ensure `[wsl2]` exists;
/// - only when `disable_wslg` is explicitly passed, set `guiApplications=false`;
/// - output is normalized to CRLF (Windows-native INI).
///
/// NOTE: any change here only takes effect after a FULL `wsl --shutdown` —
/// per-distro `--terminate` is not enough. The caller (`setup`) must warn
/// about running distros and never shut down without an explicit `--shutdown`
/// flag.
pub fn patch_user_wslconfig(disable_wslg: bool) -> Result<WslConfigPatch> {
    let profile = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .context("USERPROFILE is not set; cannot locate .wslconfig")?;
    let path = profile.join(".wslconfig");

    let original = fs::read_to_string(&path).unwrap_or_default();
    let backup_path = if path.exists() {
        let backup = backup_file(&path, |ts| {
            format!(".wslconfig.wsldesk.{ts}.bak")
        })?;
        Some(backup)
    } else {
        None
    };

    let mut edit = IniEdit::parse(&original);
    edit.ensure_section("wsl2");
    if disable_wslg {
        edit.ensure_key("wsl2", "guiApplications", "false");
    }
    edit.enforce_crlf();

    let changed = edit.is_modified();
    if changed {
        fs::write(&path, edit.render())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    let diff = edit
        .changes()
        .iter()
        .map(|c| format!("{}  # {}", c.line.replace('\n', "\\n").replace('\r', "\\r"), c.note))
        .collect();
    Ok(WslConfigPatch {
        path,
        changed,
        backup_path,
        diff,
    })
}

/// Result of patching `/etc/wsl.conf` inside a distro.
#[derive(Debug, Clone)]
pub struct WslConfPatch {
    pub changed: bool,
    pub backup_path: Option<String>,
    pub diff: Vec<String>,
    /// True when systemd was newly enabled — the distro needs a restart.
    pub needs_terminate: bool,
}

/// Patch `/etc/wsl.conf` inside the distro over `wsl.exe` (root):
/// - timestamped backup `/etc/wsl.conf.wsldesk.<ts>.bak` inside the distro;
/// - ensure `[boot] systemd=true`;
/// - output is strictly LF (systemd reads this file inside Linux);
/// - if modified, the caller must `wsl.exe --terminate <distro>` (per-distro
///   restart only — `.wslconfig` changes are the ones that need a full
///   `wsl --shutdown`).
pub fn patch_distro_wsl_conf(distro: &str) -> Result<WslConfPatch> {
    let read = wsl_cli::exec_root(distro, "cat /etc/wsl.conf 2>/dev/null || true", None)?;
    let original = read.stdout;

    let mut edit = IniEdit::parse(&original);
    edit.ensure_section("boot");
    edit.ensure_key("boot", "systemd", "true");
    edit.enforce_lf();
    let changed = edit.is_modified();
    if !changed {
        return Ok(WslConfPatch {
            changed: false,
            backup_path: None,
            diff: Vec::new(),
            needs_terminate: false,
        });
    }

    let backup_path = if original.trim().is_empty() {
        None
    } else {
        let ts = unix_timestamp();
        let target = format!("/etc/wsl.conf.wsldesk.{ts}.bak");
        let cmd = format!(
            "cp -p /etc/wsl.conf '{target}' && echo '{target}'",
            target = target
        );
        let out = wsl_cli::exec_root(distro, &cmd, None)?;
        if !out.success() {
            anyhow::bail!(
                "failed to back up /etc/wsl.conf inside {distro}: {}",
                out.stderr.trim()
            );
        }
        Some(target)
    };

    let rendered = edit.render();
    let write_cmd = "cat > /etc/wsl.conf.wsldesk-new \
                     && chmod 0644 /etc/wsl.conf.wsldesk-new \
                     && mv -f /etc/wsl.conf.wsldesk-new /etc/wsl.conf";
    let out = wsl_cli::exec_root(distro, write_cmd, Some(rendered.as_bytes()))?;
    if !out.success() {
        anyhow::bail!(
            "failed to write /etc/wsl.conf inside {distro}: {}",
            out.stderr.trim()
        );
    }

    let diff = edit
        .changes()
        .iter()
        .map(|c| format!("{}  # {}", c.line.replace('\n', "\\n").replace('\r', "\\r"), c.note))
        .collect();
    let needs_terminate = edit
        .changes()
        .iter()
        .any(|c| c.note.contains("systemd"));
    Ok(WslConfPatch {
        changed: true,
        backup_path,
        diff,
        needs_terminate,
    })
}

/// Copy `src` to `<stem>.<suffix-with-timestamp>` next to it, returning the
/// backup path. `name_for_ts` receives the timestamp and returns the file
/// suffix (starting with `.`).
fn backup_file(src: &Path, name_for_ts: impl Fn(u128) -> String) -> Result<PathBuf> {
    let ts = unix_timestamp();
    let file_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .context("config path has no file name")?
        .to_string();
    let backup_name = format!("{file_name}{}", name_for_ts(ts));
    let backup = src.with_file_name(backup_name);
    fs::copy(src, &backup)
        .with_context(|| format!("failed to create backup {}", backup.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# my wslconfig\n\
                          [wsl2]\n\
                          memory=8GB\n\
                          # keep this comment\n\
                          processors=4\n";

    #[test]
    fn ensure_section_is_idempotent_and_preserves_content() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_section("wsl2");
        assert!(!edit.is_modified());
        assert_eq!(edit.render(), SAMPLE);
    }

    #[test]
    fn ensure_section_appends_when_missing() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_section("experimental");
        assert!(edit.is_modified());
        let rendered = edit.render();
        assert!(rendered.starts_with(SAMPLE)); // everything preserved, appended
        assert!(rendered.contains("[experimental]\n"));
        assert_eq!(
            edit.changes()[0].line,
            "[experimental]"
        );
    }

    #[test]
    fn ensure_key_updates_existing_line_in_place() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_key("wsl2", "memory", "16GB");
        assert!(edit.is_modified());
        let rendered = edit.render();
        assert!(rendered.contains("memory = 16GB\n"));
        assert!(!rendered.contains("memory=8GB"));
        // comment lines untouched, key order preserved
        assert!(rendered.contains("# keep this comment\n"));
        assert!(rendered.find("memory").unwrap() < rendered.find("processors").unwrap());
        assert!(edit.changes()[0].line.contains("memory=8GB"));
        assert!(edit.changes()[0].line.contains("memory = 16GB"));
    }

    #[test]
    fn ensure_key_noop_when_value_matches() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_key("wsl2", "memory", "8GB");
        assert!(!edit.is_modified());
        assert_eq!(edit.render(), SAMPLE);
    }

    #[test]
    fn ensure_key_appends_inside_correct_section() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_key("wsl2", "guiApplications", "false");
        let rendered = edit.render();
        let wsl2_start = rendered.find("[wsl2]").unwrap();
        let next_section = rendered.find("[other]");
        let key_at = rendered.find("guiApplications = false").unwrap();
        assert!(key_at > wsl2_start);
        if let Some(next) = next_section {
            assert!(key_at < next);
        }
        assert!(rendered.ends_with("guiApplications = false\n"));
    }

    #[test]
    fn ensure_key_creates_missing_section_first() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_key("boot", "systemd", "true");
        let rendered = edit.render();
        assert!(rendered.contains("[boot]\n"));
        assert!(rendered.contains("systemd = true\n"));
        assert_eq!(edit.changes().len(), 2);
    }

    #[test]
    fn crlf_files_keep_their_endings() {
        let crlf = "[wsl2]\r\nmemory=8GB\r\n";
        let mut edit = IniEdit::parse(crlf);
        edit.ensure_key("wsl2", "swap", "4GB");
        let rendered = edit.render();
        assert!(rendered.starts_with("[wsl2]\r\nmemory=8GB\r\n"));
        assert!(rendered.contains("swap = 4GB\n")); // new lines use LF; fine for INI
    }

    #[test]
    fn keys_in_other_sections_are_ignored() {
        let text = "[boot]\nsystemd=true\n[wsl2]\nmemory=4GB\n";
        let mut edit = IniEdit::parse(text);
        edit.ensure_key("wsl2", "systemd", "false");
        // "systemd" exists only under [boot]; [wsl2] must gain its own key.
        let rendered = edit.render();
        assert!(rendered.contains("[boot]\nsystemd=true"));
        assert!(rendered.ends_with("systemd = false\n"));
    }

    #[test]
    fn empty_file_gains_section_and_key() {
        let mut edit = IniEdit::parse("");
        edit.ensure_key("boot", "systemd", "true");
        assert_eq!(edit.render(), "[boot]\nsystemd = true\n");
    }

    #[test]
    fn empty_file_section_only() {
        let mut edit = IniEdit::parse("");
        edit.ensure_section("wsl2");
        assert_eq!(edit.render(), "[wsl2]\n");
    }

    #[test]
    fn missing_sections_are_created_in_order() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_section("boot");
        edit.ensure_key("boot", "systemd", "true");
        edit.ensure_section("automount");
        edit.ensure_key("automount", "enabled", "false");
        let rendered = edit.render();
        assert!(rendered.starts_with(SAMPLE));
        assert!(rendered.contains("[boot]\nsystemd = true\n"));
        assert!(rendered.contains("[automount]\nenabled = false\n"));
    }

    #[test]
    fn existing_systemd_false_is_flipped_to_true() {
        let text = "# distro config\n[boot]\nsystemd=false\n[network]\nhostname=wsl\n";
        let mut edit = IniEdit::parse(text);
        edit.ensure_key("boot", "systemd", "true");
        assert!(edit.is_modified());
        let rendered = edit.render();
        assert!(rendered.contains("systemd = true\n"));
        assert!(!rendered.contains("systemd=false"));
        // user content untouched
        assert!(rendered.contains("# distro config\n"));
        assert!(rendered.contains("[network]\nhostname=wsl\n"));
        // the change note flags systemd so the caller knows to --terminate
        assert!(edit.changes().iter().any(|c| c.note.contains("systemd")));
    }

    #[test]
    fn comments_surrounding_systemd_are_preserved() {
        let text = "[boot]\n# keep me\nsystemd=false\n# keep me too\n";
        let mut edit = IniEdit::parse(text);
        edit.ensure_key("boot", "systemd", "true");
        let rendered = edit.render();
        assert!(rendered.contains("# keep me\n"));
        assert!(rendered.contains("# keep me too\n"));
        assert_eq!(
            rendered,
            "[boot]\n# keep me\nsystemd = true\n# keep me too\n"
        );
    }

    #[test]
    fn double_apply_is_idempotent() {
        let mut edit = IniEdit::parse(SAMPLE);
        edit.ensure_key("wsl2", "memory", "16GB");
        edit.ensure_section("boot");
        edit.ensure_key("boot", "systemd", "true");
        let first = edit.render();
        assert!(edit.is_modified());

        // Simulate a second `setup` run against the already-patched file.
        let mut second = IniEdit::parse(&first);
        second.ensure_key("wsl2", "memory", "16GB");
        second.ensure_section("boot");
        second.ensure_key("boot", "systemd", "true");
        assert!(
            !second.is_modified(),
            "second apply must be a no-op, got changes: {:?}",
            second.changes()
        );
        assert_eq!(second.render(), first);
    }

    #[test]
    fn wsl_conf_output_is_strict_lf() {
        // CRLF input must be fully normalized to LF for /etc/wsl.conf.
        let crlf = "[boot]\r\nsystemd=false\r\n# note\r\n";
        let mut edit = IniEdit::parse(crlf);
        edit.ensure_key("boot", "systemd", "true");
        edit.enforce_lf();
        assert_eq!(edit.render(), "[boot]\nsystemd = true\n# note\n");
        assert!(!edit.render().contains('\r'));
        // the normalization itself is recorded
        assert!(edit
            .changes()
            .iter()
            .any(|c| c.note.contains("LF line endings")));
    }

    #[test]
    fn wsl_conf_lf_enforcement_appends_missing_final_newline() {
        let mut edit = IniEdit::parse("[boot]\nsystemd=true"); // no final newline
        edit.enforce_lf();
        assert_eq!(edit.render(), "[boot]\nsystemd=true\n");
    }

    #[test]
    fn wsl_conf_lf_enforcement_is_noop_for_lf_files() {
        let mut edit = IniEdit::parse("[boot]\nsystemd=true\n");
        edit.enforce_lf();
        assert!(!edit.is_modified());
    }

    #[test]
    fn wslconfig_output_is_strict_crlf() {
        // LF-only input must be normalized to CRLF for the Windows-side INI.
        let lf = "[wsl2]\nmemory=8GB\n# note\n";
        let mut edit = IniEdit::parse(lf);
        edit.ensure_key("wsl2", "swap", "4GB");
        edit.enforce_crlf();
        let rendered = edit.render();
        assert_eq!(
            rendered,
            "[wsl2]\r\nmemory=8GB\r\n# note\r\nswap = 4GB\r\n"
        );
        assert!(edit
            .changes()
            .iter()
            .any(|c| c.note.contains("CRLF line endings")));
    }

    #[test]
    fn wslconfig_crlf_enforcement_is_noop_for_crlf_files() {
        let mut edit = IniEdit::parse("[wsl2]\r\nmemory=8GB\r\n");
        edit.enforce_crlf();
        assert!(!edit.is_modified());
    }

    #[test]
    fn wslconfig_crlf_preserves_content_when_normalizing() {
        let mixed = "[wsl2]\r\nmemory=8GB\nprocessors=4\r\n";
        let mut edit = IniEdit::parse(mixed);
        edit.enforce_crlf();
        let rendered = edit.render();
        // every line now ends with \r\n, content characters unchanged
        assert_eq!(rendered.matches("\r\n").count(), 3);
        assert!(rendered.contains("memory=8GB"));
        assert!(rendered.contains("processors=4"));
    }
}
