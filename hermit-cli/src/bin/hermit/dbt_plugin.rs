/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Dependency-light host adapter for the separately installed DBT payload.

use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::process::Stdio;

use hermit_plugin_protocol::EX_CONFIG;
use hermit_plugin_protocol::EX_UNAVAILABLE;
use hermit_plugin_protocol::EnsureRequest;
use hermit_plugin_protocol::PayloadManifest;
use hermit_plugin_protocol::PluginIdentity;

const DIAGNOSTIC_FD: libc::c_int = 198;

#[derive(Debug)]
pub(super) struct PluginFailure {
    pub(super) code: i32,
    pub(super) message: String,
}

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn roots() -> (PathBuf, PathBuf) {
    let home = env::var_os("HOME").map(PathBuf::from);
    let hermit = env::var_os("HERMIT_DIR")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".hermit")))
        .unwrap_or_else(|| PathBuf::from("$HOME/.hermit"));
    let cargo = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".cargo")))
        .unwrap_or_else(|| PathBuf::from("$HOME/.cargo"));
    (hermit, cargo)
}

fn select_helper() -> Result<PathBuf, PluginFailure> {
    let (hermit, cargo) = roots();
    let direct = [
        hermit.join("bin/hermit-dynamorio"),
        cargo.join("bin/hermit-dynamorio"),
    ];
    select_helper_from(direct, env::var_os("PATH").as_deref())
}

fn select_helper_from(
    direct: [PathBuf; 2],
    path: Option<&OsStr>,
) -> Result<PathBuf, PluginFailure> {
    for path in &direct {
        if executable(path) {
            return Ok(path.clone());
        }
        if fs::symlink_metadata(path).is_ok() {
            return Err(PluginFailure {
                code: EX_CONFIG,
                message: format!(
                    "error: selected hermit-dynamorio helper {} exists but is not executable; refusing to fall back to a different plugin\nrepair: reinstall it with `cargo install --force hermit-dynamorio` or fix its execute permission",
                    path.display()
                ),
            });
        }
    }
    if let Some(path) = path.and_then(|path| {
        env::split_paths(path)
            .map(|directory| directory.join("hermit-dynamorio"))
            .find(|candidate| executable(candidate))
    }) {
        return Ok(path);
    }
    Err(PluginFailure {
        code: EX_UNAVAILABLE,
        message: format!(
            "error: backend 'dbt' is unavailable: hermit-dynamorio was not found\n\
             install it with:\n  cargo install hermit-dynamorio\n\
             searched:\n  {}\n  {}\n  PATH",
            direct[0].display(),
            direct[1].display()
        ),
    })
}

pub(super) fn ensure_payload() -> Result<PayloadManifest, PluginFailure> {
    let helper = select_helper()?;
    let host = PluginIdentity::current(env!("CARGO_PKG_VERSION"), detcore::DETCORE_BUILD_ID);
    let request = EnsureRequest { host: host.clone() };
    let mut child = Command::new(&helper)
        .arg("ensure")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: selected hermit-dynamorio helper {} could not be started: {error}",
                helper.display()
            ),
        })?;
    serde_json::to_writer(
        child
            .stdin
            .as_mut()
            .expect("piped helper stdin disappeared"),
        &request,
    )
    .map_err(|error| PluginFailure {
        code: EX_CONFIG,
        message: format!("error: failed to encode hermit-dynamorio request: {error}"),
    })?;
    drop(child.stdin.take());
    let output = child.wait_with_output().map_err(|error| PluginFailure {
        code: EX_CONFIG,
        message: format!("error: failed waiting for {}: {error}", helper.display()),
    })?;
    if !output.status.success() {
        return Err(PluginFailure {
            code: output.status.code().unwrap_or(EX_CONFIG),
            message: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_owned(),
        });
    }
    let payload: PayloadManifest =
        serde_json::from_slice(&output.stdout).map_err(|error| PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: hermit-dynamorio returned an invalid payload manifest: {error}"
            ),
        })?;
    if let Some(field) = host.mismatch(&payload.plugin) {
        return Err(PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: hermit-dynamorio returned an incompatible payload ({field} mismatch); refusing backend 'dbt'"
            ),
        });
    }
    let release = fs::canonicalize(&payload.release_dir).map_err(|error| PluginFailure {
        code: EX_CONFIG,
        message: format!(
            "error: hermit-dynamorio payload directory {} is unavailable: {error}",
            payload.release_dir.display()
        ),
    })?;
    for (description, path) in [
        ("DynamoRIO launcher", &payload.drrun),
        ("DynamoRIO client", &payload.client),
        ("Detcore runtime", &payload.detcore_runtime),
    ] {
        let resolved = fs::canonicalize(path).map_err(|error| PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: {description} {} is unavailable: {error}",
                path.display()
            ),
        })?;
        if !resolved.starts_with(&release) || !resolved.is_file() {
            return Err(PluginFailure {
                code: EX_CONFIG,
                message: format!(
                    "error: hermit-dynamorio returned {description} outside its validated release: {}",
                    path.display()
                ),
            });
        }
    }
    Ok(payload)
}

#[derive(Clone, Debug)]
pub(super) struct DbiRunner {
    drrun: PathBuf,
    client: PathBuf,
    client_arguments: Vec<OsString>,
    summary: bool,
    isolated_process_group: bool,
}

impl DbiRunner {
    pub(super) fn new(drrun: &Path, client: &Path) -> io::Result<Self> {
        if !executable(drrun) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("DynamoRIO launcher is not executable: {}", drrun.display()),
            ));
        }
        if !client.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("DynamoRIO client is missing: {}", client.display()),
            ));
        }
        Ok(Self {
            drrun: drrun.to_owned(),
            client: client.to_owned(),
            client_arguments: Vec::new(),
            summary: false,
            isolated_process_group: false,
        })
    }

    pub(super) fn summary(mut self, enabled: bool) -> Self {
        self.summary = enabled;
        self
    }

    pub(super) fn isolated_process_group(mut self, enabled: bool) -> Self {
        self.isolated_process_group = enabled;
        self
    }

    pub(super) fn client_argument(mut self, argument: impl Into<OsString>) -> Self {
        self.client_arguments.push(argument.into());
        self
    }

    pub(super) fn status(&self, guest: &Command) -> io::Result<ExitStatus> {
        self.command(guest)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?
            .wait()
    }

    pub(super) fn output_with_inherited_stdin(&self, guest: &Command) -> io::Result<Output> {
        self.command(guest)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
            .wait_with_output()
    }

    pub(super) fn output_with_detached_reader<R>(
        &self,
        guest: &Command,
        mut input: R,
    ) -> io::Result<Output>
    where
        R: Read + Send + 'static,
    {
        let mut child = self
            .command(guest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "failed to open DBT guest stdin")
        })?;
        std::thread::spawn(move || {
            let _ = io::copy(&mut input, &mut stdin);
        });
        child.wait_with_output()
    }

    fn command(&self, guest: &Command) -> Command {
        let mut command = Command::new(&self.drrun);
        command
            .arg("-quiet")
            .arg("-disable_rseq")
            .args(["-stack_size", "2M"])
            .arg("-c")
            .arg(&self.client)
            .arg("-diagnostic_fd")
            .arg(DIAGNOSTIC_FD.to_string())
            .args(&self.client_arguments);
        if self.isolated_process_group {
            command.arg("-isolated-process-group");
        }
        if self.summary {
            command.arg("-summary");
        }
        command
            .arg("--")
            .arg(guest.get_program())
            .args(guest.get_args());
        if let Some(directory) = guest.get_current_dir() {
            command.current_dir(directory);
        }
        for (key, value) in guest.get_envs() {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        if self.isolated_process_group {
            command.process_group(0);
        }
        unsafe {
            command.pre_exec(|| {
                if libc::dup2(libc::STDERR_FILENO, DIAGNOSTIC_FD) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let current = libc::personality(0xffff_ffff);
                if current == -1 {
                    return Err(io::Error::last_os_error());
                }
                let deterministic =
                    current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong;
                if libc::personality(deterministic) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_helper_message_is_actionable() {
        let error = select_helper_from(
            [
                PathBuf::from("/nonexistent/hermit/bin/hermit-dynamorio"),
                PathBuf::from("/nonexistent/cargo/bin/hermit-dynamorio"),
            ],
            Some(OsStr::new("")),
        )
        .unwrap_err();
        assert_eq!(error.code, EX_UNAVAILABLE);
        assert!(error.message.contains("cargo install hermit-dynamorio"));
    }

    #[test]
    fn selected_incompatible_candidate_cannot_fall_through() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("root/bin/hermit-dynamorio");
        let fallback_directory = directory.path().join("path");
        let fallback = fallback_directory.join("hermit-dynamorio");
        fs::create_dir_all(selected.parent().unwrap()).unwrap();
        fs::create_dir_all(&fallback_directory).unwrap();
        for path in [&selected, &fallback] {
            fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert_eq!(
            select_helper_from(
                [selected.clone(), PathBuf::from("/nonexistent/cargo-helper")],
                Some(fallback_directory.as_os_str()),
            )
            .unwrap(),
            selected
        );
    }

    #[test]
    fn non_executable_explicit_candidate_blocks_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("root/bin/hermit-dynamorio");
        let fallback_directory = directory.path().join("path");
        let fallback = fallback_directory.join("hermit-dynamorio");
        fs::create_dir_all(selected.parent().unwrap()).unwrap();
        fs::create_dir_all(&fallback_directory).unwrap();
        fs::write(&selected, b"not executable").unwrap();
        fs::write(&fallback, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&fallback, fs::Permissions::from_mode(0o755)).unwrap();

        let error = select_helper_from(
            [selected, PathBuf::from("/nonexistent/cargo-helper")],
            Some(fallback_directory.as_os_str()),
        )
        .unwrap_err();
        assert_eq!(error.code, EX_CONFIG);
        assert!(error.message.contains("refusing to fall back"));
    }
}
