//! Idempotent installation of the bundled agentctl skill.
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent::AgentError;

fn fail(message: impl Into<String>) -> AgentError {
    AgentError::Delivery(message.into())
}

#[derive(Debug, Serialize)]
pub(crate) struct InstallResult {
    installed: Vec<String>,
    unchanged: Vec<String>,
}

fn direct_root(harness: &str) -> Result<PathBuf, AgentError> {
    if let Some(value) = std::env::var_os(format!(
        "AGENTCTL_{}_SKILLS_DIR",
        harness.to_ascii_uppercase()
    )) {
        let root = PathBuf::from(value);
        if !root.is_absolute() {
            return Err(fail(format!(
                "{harness} skill directory override must be absolute"
            )));
        }
        return Ok(root);
    }
    let directory = match harness {
        "codex" => ".codex",
        "claude" => ".claude",
        _ => {
            return Err(fail(format!(
                "unsupported direct skill harness {harness:?}"
            )))
        }
    };
    Ok(crate::client::account_home()?
        .join(directory)
        .join("skills"))
}

fn ensure_real_directory(path: &Path) -> Result<(), AgentError> {
    if !path.is_absolute() {
        return Err(fail(format!(
            "skill destination must be absolute: {}",
            path.display()
        )));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(fail(format!(
                    "refusing non-directory skill destination: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|error| {
                        fail(format!(
                            "cannot create skill destination {}: {error}",
                            current.display()
                        ))
                    })?;
            }
            Err(error) => {
                return Err(fail(format!(
                    "cannot inspect skill destination {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn existing(path: &Path, force: bool) -> Result<Option<&'static str>, AgentError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(fail(format!(
                "cannot inspect installed skill {}: {error}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(fail(format!(
            "refusing non-regular skill file: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        fail(format!(
            "cannot inspect installed skill {}: {error}",
            path.display()
        ))
    })?;
    if bytes == crate::AGENTCTL_SKILL.as_bytes() {
        return Ok(Some("unchanged"));
    }
    if !force {
        return Err(fail(format!(
            "refusing to overwrite divergent skill {}; inspect it or pass --force",
            path.display()
        )));
    }
    Ok(None)
}

fn write_direct(path: &Path, force: bool) -> Result<&'static str, AgentError> {
    let directory = path.parent().expect("skill parent");
    ensure_real_directory(directory)?;
    if let Some(outcome) = existing(path, force)? {
        return Ok(outcome);
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| fail(error.to_string()))?
        .as_nanos();
    let temporary = directory.join(format!(".SKILL.md.{}.{nonce}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| fail(error.to_string()))?;
    let result = (|| {
        file.write_all(crate::AGENTCTL_SKILL.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(directory)?.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(fail(error.to_string()));
    }
    Ok("installed")
}

fn muse_config_home() -> Result<PathBuf, AgentError> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        let root = PathBuf::from(value);
        if !root.is_absolute() {
            return Err(fail(
                "XDG_CONFIG_HOME must be absolute for Muse skill installation",
            ));
        }
        return Ok(root.join("muse"));
    }
    Ok(crate::client::account_home()?.join(".config/muse"))
}

fn muse_executable() -> Result<PathBuf, AgentError> {
    let Some(configured) = std::env::var_os("AGENTCTL_MUSE_BIN") else {
        return Ok(crate::client::resolve_harness_executable("muse")?);
    };
    let path = PathBuf::from(configured);
    if !path.is_absolute() {
        return Err(fail("AGENTCTL_MUSE_BIN must be an absolute path"));
    }
    let resolved = fs::canonicalize(&path).map_err(|error| {
        fail(format!(
            "cannot inspect Muse executable {}: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::metadata(&resolved).map_err(|error| {
        fail(format!(
            "cannot inspect Muse executable {}: {error}",
            resolved.display()
        ))
    })?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(fail(format!(
            "refusing unsafe Muse executable: {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn output_detail(output: &[u8]) -> String {
    let start = output.len().saturating_sub(4000);
    String::from_utf8_lossy(&output[start..]).trim().to_owned()
}

/// How long the native Muse skill manager may run before its process group is killed.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30);

fn install_muse(force: bool) -> Result<&'static str, AgentError> {
    // Resolve and validate code before making any configuration directories.
    let executable = muse_executable()?;
    let config_home = muse_config_home()?;
    install_muse_with(&executable, &config_home, force, INSTALL_TIMEOUT)
}

/// Install through the validated Muse `executable` into `config_home` (`.../muse`), giving the
/// manager `timeout` before its process group is killed.
fn install_muse_with(
    executable: &Path,
    config_home: &Path,
    force: bool,
    timeout: Duration,
) -> Result<&'static str, AgentError> {
    let skills = config_home.join("skills");
    ensure_real_directory(&skills)?;
    let destination_directory = skills.join("agentctl");
    if let Ok(metadata) = fs::symlink_metadata(&destination_directory) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(fail(format!(
                "refusing non-directory skill destination: {}",
                destination_directory.display()
            )));
        }
    }
    let destination = destination_directory.join("SKILL.md");
    if let Some(outcome) = existing(&destination, force)? {
        return Ok(outcome);
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| fail(error.to_string()))?
        .as_nanos();
    let staging = std::env::temp_dir().join(format!(
        "agentctl-muse-skill.{}.{nonce}",
        std::process::id()
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|error| fail(format!("cannot stage Muse skill: {error}")))?;
    let source = staging.join("agentctl");
    DirBuilder::new()
        .mode(0o700)
        .create(&source)
        .map_err(|error| fail(format!("cannot stage Muse skill: {error}")))?;
    let source_file = source.join("SKILL.md");
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&source_file)?;
        file.write_all(crate::AGENTCTL_SKILL.as_bytes())?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(fail(format!("cannot stage Muse skill: {error}")));
    }

    let mut command = Command::new(executable);
    command
        .args(["skills", "install"])
        .arg(&source)
        .args(["--scope", "user", "--name", "agentctl", "--json"])
        .env(
            "XDG_CONFIG_HOME",
            config_home.parent().expect("Muse config parent"),
        )
        .stdin(Stdio::null());
    if force {
        command.arg("--force");
    }
    let output = crate::client::bounded_output(command, timeout);
    let _ = fs::remove_dir_all(&staging);
    let output =
        output.map_err(|error| fail(format!("Muse skill installation failed: {error}")))?;
    if !output.status.success() {
        let detail = if output.stderr.is_empty() {
            output_detail(&output.stdout)
        } else {
            output_detail(&output.stderr)
        };
        return Err(fail(format!(
            "Muse skill installation failed: {}",
            if detail.is_empty() {
                output.status.to_string()
            } else {
                detail
            }
        )));
    }
    let receipt: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        fail(format!(
            "Muse skill installation returned invalid JSON: {error}"
        ))
    })?;
    if receipt
        .get("installed")
        .and_then(Value::as_object)
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        != Some("agentctl")
    {
        return Err(fail("Muse skill installation returned no agentctl receipt"));
    }
    if existing(&destination, false)? != Some("unchanged") {
        return Err(fail(
            "Muse reported installation without the expected managed skill",
        ));
    }
    Ok("installed")
}

pub(crate) fn install(harnesses: &[String], force: bool) -> Result<InstallResult, AgentError> {
    let selected = if harnesses.is_empty() {
        vec!["codex".to_owned(), "claude".to_owned(), "muse".to_owned()]
    } else {
        let mut values = Vec::new();
        for harness in harnesses {
            if !values.contains(harness) {
                values.push(harness.clone());
            }
        }
        values
    };
    let invalid: std::collections::BTreeSet<&str> = selected
        .iter()
        .map(String::as_str)
        .filter(|harness| !matches!(*harness, "codex" | "claude" | "muse"))
        .collect();
    if !invalid.is_empty() {
        return Err(fail(format!(
            "unsupported skill harnesses: {}",
            invalid.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let mut result = InstallResult {
        installed: Vec::new(),
        unchanged: Vec::new(),
    };
    for harness in selected {
        let outcome = if harness == "muse" {
            install_muse(force)?
        } else {
            let destination = direct_root(&harness)?.join("agentctl/SKILL.md");
            write_direct(&destination, force)?
        };
        match outcome {
            "installed" => result.installed.push(harness),
            "unchanged" => result.unchanged.push(harness),
            _ => unreachable!(),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    //! Security and resource bounds, symlink refusal, and atomic replacement.

    // The hostile-PATH, output-bound and deadline tests come from the other edition's
    // test_agentctl_skill_install.py.
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    /// Stdout bound that `client::bounded_output` applies to every control command.
    const CONTROL_STDOUT_BYTES: usize = 8 * 1024 * 1024;
    const HOSTILE_PATH_CHILD: &str = "AGENTCTL_SKILL_INSTALL_HOSTILE_PATH_CHILD";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            // Canonical, so no symlinked ancestor of the temporary directory is mistaken for a
            // hostile destination component.
            let base = fs::canonicalize(std::env::temp_dir()).unwrap();
            let path = base.join(format!(
                "agentctl-skill-install-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn executable(path: &Path, script: &str) -> PathBuf {
        fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        path.to_path_buf()
    }

    /// Whether the process whose ID is in `pid_file` has gone (or is an unreaped zombie).
    fn process_gone(pid_file: &Path) -> bool {
        let pid = fs::read_to_string(pid_file).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                Err(_) => return true,
                Ok(stat)
                    if stat
                        .rsplit_once(") ")
                        .is_some_and(|(_, tail)| tail.starts_with('Z')) =>
                {
                    return true
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        false
    }

    #[test]
    fn muse_executable_does_not_search_hostile_path() {
        if std::env::var_os(HOSTILE_PATH_CHILD).is_some() {
            match muse_executable() {
                Ok(path) => println!("<<resolved {}>>", path.display()),
                Err(error) => println!("<<refused {error}>>"),
            }
            return;
        }
        let scratch = Scratch::new();
        let executed = scratch.0.join("executed");
        let planted = executable(
            &scratch.0.join("muse"),
            &format!("touch '{}'", executed.display()),
        );
        // The search path is process state, so the resolution runs in a child of this test.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "skill_install::tests::muse_executable_does_not_search_hostile_path",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HOSTILE_PATH_CHILD, "1")
            .env("PATH", &scratch.0)
            .env_remove("AGENTCTL_MUSE_BIN")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result = stdout
            .split_once("<<")
            .and_then(|(_, rest)| rest.split_once(">>"))
            .map(|(result, _)| result)
            .unwrap_or_else(|| panic!("child printed no resolution: {stdout}"));
        assert!(!result.contains(&planted.display().to_string()), "{result}");
        assert!(planted.is_file());
        assert!(!executed.exists());
    }

    #[test]
    fn muse_installer_kills_a_process_group_that_exceeds_output_bound() {
        let scratch = Scratch::new();
        let helper = scratch.0.join("helper.pid");
        let muse = executable(
            &scratch.0.join("muse-output"),
            &format!(
                "sleep 60 & echo $! > '{}'\nhead -c {} /dev/zero\nwait",
                helper.display(),
                CONTROL_STDOUT_BYTES + 1
            ),
        );
        let error = install_muse_with(
            &muse,
            &scratch.0.join("config/muse"),
            false,
            INSTALL_TIMEOUT,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("Muse skill installation failed:") && error.contains("exceeds"),
            "{error}"
        );
        assert!(process_gone(&helper), "the installer's helper survived");
        assert!(!scratch.0.join("config/muse/skills/agentctl").exists());
    }

    #[test]
    fn muse_installer_kills_a_process_group_at_deadline() {
        // The deadline starts before the script runs; a loaded host can expire it before the
        // helper is published, so such an attempt is retried with a longer deadline.
        for timeout in [200, 1_000, 5_000].map(Duration::from_millis) {
            let scratch = Scratch::new();
            let helper = scratch.0.join("helper.pid");
            let muse = executable(
                &scratch.0.join("muse-sleep"),
                &format!("sleep 60 & echo $! > '{}'\nwait", helper.display()),
            );
            let started = Instant::now();
            let error = install_muse_with(&muse, &scratch.0.join("config/muse"), false, timeout)
                .unwrap_err()
                .to_string();
            assert!(error.contains("timed out"), "{error}");
            assert!(started.elapsed() < timeout + Duration::from_secs(5));
            if !helper.exists() {
                continue;
            }
            assert!(process_gone(&helper), "the installer's helper survived");
            return;
        }
        panic!("the installer never published its helper before any deadline");
    }

    #[test]
    fn muse_install_uses_the_native_manager_and_proves_its_result() {
        let scratch = Scratch::new();
        let config = scratch.0.join("config/muse");
        let argv = scratch.0.join("argv");
        let receipt = r#"printf '%s\n' '{"installed": {"id": "agentctl"}}'"#;
        let muse = executable(
            &scratch.0.join("muse"),
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\n\
                 mkdir -p \"$XDG_CONFIG_HOME/muse/skills/agentctl\"\n\
                 cp \"$3/SKILL.md\" \"$XDG_CONFIG_HOME/muse/skills/agentctl/SKILL.md\"\n\
                 {receipt}",
                argv.display()
            ),
        );
        assert_eq!(
            install_muse_with(&muse, &config, false, INSTALL_TIMEOUT).unwrap(),
            "installed"
        );
        let arguments = fs::read_to_string(&argv).unwrap();
        let arguments: Vec<&str> = arguments.lines().collect();
        assert_eq!(arguments[..2], ["skills", "install"]);
        assert_eq!(
            arguments[3..],
            ["--scope", "user", "--name", "agentctl", "--json"]
        );
        assert_eq!(
            fs::read(config.join("skills/agentctl/SKILL.md")).unwrap(),
            crate::AGENTCTL_SKILL.as_bytes()
        );
        // An identical installed skill is reported without running the manager again.
        fs::remove_file(&argv).unwrap();
        assert_eq!(
            install_muse_with(&muse, &config, false, INSTALL_TIMEOUT).unwrap(),
            "unchanged"
        );
        assert!(!argv.exists());
        // A manager that claims success without writing the managed skill is not believed.
        fs::write(config.join("skills/agentctl/SKILL.md"), "owner edits").unwrap();
        let liar = executable(
            &scratch.0.join("muse-liar"),
            &format!("printf '%s\\n' \"$@\" > '{}'\n{receipt}", argv.display()),
        );
        let error = install_muse_with(&liar, &config, true, INSTALL_TIMEOUT).unwrap_err();
        assert!(fs::read_to_string(&argv)
            .unwrap()
            .lines()
            .any(|line| line == "--force"));
        assert!(
            error
                .to_string()
                .contains("refusing to overwrite divergent skill"),
            "{error}"
        );
        fs::remove_file(config.join("skills/agentctl/SKILL.md")).unwrap();
        assert_eq!(
            install_muse_with(&liar, &config, false, INSTALL_TIMEOUT)
                .unwrap_err()
                .to_string(),
            "Muse reported installation without the expected managed skill"
        );
    }

    #[test]
    fn symlinked_or_non_directory_destinations_are_refused_before_any_write() {
        let scratch = Scratch::new();
        let real = scratch.0.join("real");
        fs::create_dir(&real).unwrap();
        let link = scratch.0.join("link");
        symlink(&real, &link).unwrap();
        for force in [false, true] {
            assert_eq!(
                write_direct(&link.join("agentctl/SKILL.md"), force)
                    .unwrap_err()
                    .to_string(),
                format!(
                    "refusing non-directory skill destination: {}",
                    link.display()
                )
            );
        }
        assert!(!real.join("agentctl").exists());

        let directory = scratch.0.join("skills/agentctl");
        ensure_real_directory(&directory).unwrap();
        let target = scratch.0.join("elsewhere.md");
        fs::write(&target, "owner content").unwrap();
        let skill = directory.join("SKILL.md");
        symlink(&target, &skill).unwrap();
        for force in [false, true] {
            assert_eq!(
                write_direct(&skill, force).unwrap_err().to_string(),
                format!("refusing non-regular skill file: {}", skill.display())
            );
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "owner content");

        let file = scratch.0.join("file");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            write_direct(&file.join("agentctl/SKILL.md"), false)
                .unwrap_err()
                .to_string(),
            format!(
                "refusing non-directory skill destination: {}",
                file.display()
            )
        );
        assert_eq!(
            ensure_real_directory(Path::new("relative/skills"))
                .unwrap_err()
                .to_string(),
            "skill destination must be absolute: relative/skills"
        );

        // Muse: a symlinked skill directory is refused before the native manager runs.
        let config = scratch.0.join("muse-config/muse");
        ensure_real_directory(&config.join("skills")).unwrap();
        symlink(&real, config.join("skills/agentctl")).unwrap();
        let ran = scratch.0.join("ran");
        let muse = executable(
            &scratch.0.join("muse"),
            &format!("touch '{}'", ran.display()),
        );
        assert_eq!(
            install_muse_with(&muse, &config, true, INSTALL_TIMEOUT)
                .unwrap_err()
                .to_string(),
            format!(
                "refusing non-directory skill destination: {}",
                config.join("skills/agentctl").display()
            )
        );
        assert!(!ran.exists());
    }

    #[test]
    fn direct_install_is_idempotent_and_replaces_divergent_content_atomically() {
        let scratch = Scratch::new();
        let path = scratch.0.join("skills/agentctl/SKILL.md");
        assert_eq!(write_direct(&path, false).unwrap(), "installed");
        assert_eq!(fs::read(&path).unwrap(), crate::AGENTCTL_SKILL.as_bytes());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        for created in [scratch.0.join("skills"), scratch.0.join("skills/agentctl")] {
            assert_eq!(fs::metadata(&created).unwrap().mode() & 0o777, 0o700);
        }
        assert_eq!(write_direct(&path, false).unwrap(), "unchanged");

        fs::write(&path, "owner edits").unwrap();
        assert_eq!(
            write_direct(&path, false).unwrap_err().to_string(),
            format!(
                "refusing to overwrite divergent skill {}; inspect it or pass --force",
                path.display()
            )
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "owner edits");

        // --force installs a complete new file by rename: a hard link to the replaced inode
        // still holds the old bytes, and no temporary file is left behind.
        let kept = scratch.0.join("kept");
        fs::hard_link(&path, &kept).unwrap();
        let replaced_inode = fs::metadata(&path).unwrap().ino();
        assert_eq!(write_direct(&path, true).unwrap(), "installed");
        assert_eq!(fs::read(&path).unwrap(), crate::AGENTCTL_SKILL.as_bytes());
        assert_ne!(fs::metadata(&path).unwrap().ino(), replaced_inode);
        assert_eq!(fs::read_to_string(&kept).unwrap(), "owner edits");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["SKILL.md"]);
    }

    #[test]
    fn unsupported_harnesses_are_refused_together_before_any_install() {
        let error = install(
            &[
                "zeta".to_owned(),
                "codex".to_owned(),
                "alpha".to_owned(),
                "zeta".to_owned(),
            ],
            false,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unsupported skill harnesses: alpha, zeta"
        );
    }
}
