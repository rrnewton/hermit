//! Best-effort append-only operational audit records and spool hygiene checks.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Number, Value};

use crate::client::bounded_output;
use crate::output;
use crate::textfmt::{display, json_line, quote};
use crate::timefmt;

/// Return the audit-log path for one project and spool setting.
#[must_use]
pub fn audit_path(project_root: &Path, spool_dir: &Path) -> PathBuf {
    let root = if spool_dir.is_absolute() {
        spool_dir.to_path_buf()
    } else {
        project_root.join(spool_dir)
    };
    root.join("audit.jsonl")
}

/// Ask Git whether the spool is ignored, or return `None` when the question is unavailable.
#[must_use]
pub fn spool_is_ignored(project_root: &Path, spool_dir: &Path) -> Option<bool> {
    let root = if spool_dir.is_absolute() {
        spool_dir.to_path_buf()
    } else {
        project_root.join(spool_dir)
    };
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project_root)
        .args(["check-ignore", "-q", "--"])
        .arg(root.join("probe"));
    let output = bounded_output(&mut command, Duration::from_secs(10)).ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Warn to stderr when command output would land in a tracked part of a Git work tree.
#[must_use]
pub fn warn_if_spool_is_tracked(project_root: &Path, spool_dir: &Path) -> bool {
    if spool_is_ignored(project_root, spool_dir) != Some(false) {
        return false;
    }
    let spool_text = spool_dir.to_string_lossy();
    output::diagnostic(&format!(
        "herdr-run: WARNING: spool directory {} is NOT git-ignored in {}. \
         Command output and the audit log will be written into a tracked \
         tree. Add '{}/' to .gitignore.",
        quote(&spool_text),
        quote(&project_root.to_string_lossy()),
        display(spool_text.trim_end_matches('/'))
    ));
    true
}

/// Append one best-effort audit record with optional additional fields.
///
/// Returns `false` when storage failed so the caller can warn without masking the command result.
pub fn record(
    path: &Path,
    agent: &str,
    command: &str,
    verdict: &str,
    detail: &str,
    fields: Map<String, Value>,
) -> bool {
    record_at(
        path,
        agent,
        command,
        verdict,
        detail,
        fields,
        SystemTime::now(),
        std::process::id(),
    )
}

#[allow(clippy::too_many_arguments)]
fn record_at(
    path: &Path,
    agent: &str,
    command: &str,
    verdict: &str,
    detail: &str,
    fields: Map<String, Value>,
    now: SystemTime,
    pid: u32,
) -> bool {
    let mut document = Map::new();
    document.insert("time".to_owned(), Value::String(timefmt::rfc3339(now)));
    document.insert("agent".to_owned(), Value::String(agent.to_owned()));
    document.insert("pid".to_owned(), Value::Number(Number::from(pid)));
    document.insert("command".to_owned(), Value::String(command.to_owned()));
    document.insert("verdict".to_owned(), Value::String(verdict.to_owned()));
    document.insert("detail".to_owned(), Value::String(detail.to_owned()));
    document.extend(fields);
    let mut encoded = json_line(&Value::Object(document)).into_bytes();
    encoded.push(b'\n');
    let Some(parent) = path.parent() else {
        return false;
    };
    let mut parent_builder = fs::DirBuilder::new();
    if parent_builder
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .is_err()
    {
        return false;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        if file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .is_err()
        {
            return false;
        }
        // Match the other distribution's one-syscall append boundary. A partial write is still
        // best-effort failure, but concurrent complete lines cannot be split across write calls.
        return file
            .write(&encoded)
            .is_ok_and(|written| written == encoded.len());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_appends_sorted_valid_json_lines() {
        let root = std::env::temp_dir().join(format!("herdr-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("audit.jsonl");
        assert!(record_at(
            &path,
            "agent",
            "git status",
            "RAN",
            "exit 0",
            Map::new(),
            std::time::UNIX_EPOCH,
            7,
        ));
        assert!(record_at(
            &path,
            "agent",
            "curl x",
            "REFUSED",
            "no",
            Map::new(),
            std::time::UNIX_EPOCH,
            7,
        ));
        let text = fs::read_to_string(&path).expect("audit should be readable");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\", \""));
        assert!(lines[0].contains("\": \""));
        let first: Value = serde_json::from_str(lines[0]).expect("valid JSON");
        assert_eq!(first["time"], "1970-01-01T00:00:00Z");
        assert_eq!(first["pid"], 7);
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unwritable_audit_is_best_effort() {
        assert!(!record(
            Path::new("/proc/definitely/not/writable/audit.jsonl"),
            "a",
            "c",
            "RAN",
            "d",
            Map::new(),
        ));
    }

    #[test]
    fn audit_does_not_chmod_an_existing_parent_directory() {
        let root = std::env::temp_dir().join(format!(
            "herdr-audit-existing-parent-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(record(
            &root.join("audit.jsonl"),
            "agent",
            "git status",
            "DRY-RUN",
            "fixture",
            Map::new(),
        ));
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_refuses_a_symlink_instead_of_modifying_its_target() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("herdr-audit-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, b"must survive\n").unwrap();
        let link = root.join("audit.jsonl");
        symlink(&victim, &link).unwrap();

        assert!(!record(
            &link,
            "agent",
            "git status",
            "DRY-RUN",
            "fixture",
            Map::new(),
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"must survive\n");
        let _ = fs::remove_dir_all(root);
    }
}
