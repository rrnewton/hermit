//! Fail-closed client for separately installed backend payload helpers.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use hermit_plugin_protocol::EX_CONFIG;
use hermit_plugin_protocol::EX_UNAVAILABLE;
use hermit_plugin_protocol::EnsureRequest;
use hermit_plugin_protocol::PayloadManifest;
use hermit_plugin_protocol::PluginIdentity;
use hermit_plugin_protocol::cargo_install_repair;

#[derive(Clone, Copy)]
pub(crate) struct PluginSpec {
    pub(crate) backend: &'static str,
    pub(crate) helper: &'static str,
    pub(crate) abi: &'static str,
}

#[derive(Debug)]
pub(crate) struct PluginFailure {
    pub(crate) code: i32,
    pub(crate) message: String,
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

fn select_helper(spec: PluginSpec) -> Result<PathBuf, PluginFailure> {
    let (hermit, cargo) = roots();
    select_helper_from(
        spec,
        [
            hermit.join("bin").join(spec.helper),
            cargo.join("bin").join(spec.helper),
        ],
        env::var_os("PATH").as_deref(),
    )
}

fn select_helper_from(
    spec: PluginSpec,
    direct: [PathBuf; 2],
    path: Option<&OsStr>,
) -> Result<PathBuf, PluginFailure> {
    for path in &direct {
        if executable(path) {
            return Ok(path.clone());
        }
        if fs::symlink_metadata(path).is_ok() {
            let repair = cargo_install_repair(path, spec.helper, env!("CARGO_PKG_VERSION"))
                .unwrap_or_else(|error| error);
            return Err(PluginFailure {
                code: EX_CONFIG,
                message: format!(
                    "error: selected {} helper {} exists but is not executable; refusing to fall back to a different plugin\nrepair selected plugin in place with:\n  {}\nor fix its execute permission",
                    spec.helper,
                    path.display(),
                    repair,
                ),
            });
        }
    }
    if let Some(path) = path.and_then(|path| {
        env::split_paths(path)
            .map(|directory| directory.join(spec.helper))
            .find(|candidate| executable(candidate))
    }) {
        return Ok(path);
    }
    Err(PluginFailure {
        code: EX_UNAVAILABLE,
        message: format!(
            "error: backend '{}' is unavailable: {} was not found\ninstall it with:\n  cargo install {}\nsearched:\n  {}\n  {}\n  PATH",
            spec.backend,
            spec.helper,
            spec.helper,
            direct[0].display(),
            direct[1].display(),
        ),
    })
}

pub(crate) fn ensure_payload(spec: PluginSpec) -> Result<PayloadManifest, PluginFailure> {
    let helper = select_helper(spec)?;
    let host = PluginIdentity::with_abi(
        spec.backend,
        env!("CARGO_PKG_VERSION"),
        spec.abi,
        detcore::DETCORE_BUILD_ID,
    );
    let request = EnsureRequest { host: host.clone() };
    let mut child = Command::new(&helper)
        .arg("ensure")
        .env("HERMIT_SELECTED_HELPER", &helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: selected {} helper {} could not be started: {error}",
                spec.helper,
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
        message: format!("error: failed to encode {} request: {error}", spec.helper),
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
                "error: {} returned an invalid payload manifest: {error}",
                spec.helper
            ),
        })?;
    if let Some(field) = host.mismatch(&payload.plugin) {
        return Err(PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: {} returned an incompatible payload ({field} mismatch); refusing backend '{}'",
                spec.helper, spec.backend
            ),
        });
    }
    validate_paths(spec, payload)
}

fn validate_paths(
    spec: PluginSpec,
    payload: PayloadManifest,
) -> Result<PayloadManifest, PluginFailure> {
    let release = fs::canonicalize(&payload.release_dir).map_err(|error| PluginFailure {
        code: EX_CONFIG,
        message: format!(
            "error: {} payload directory {} is unavailable: {error}",
            spec.helper,
            payload.release_dir.display()
        ),
    })?;
    for path in [
        &payload.drrun,
        &payload.client,
        &payload.detcore_runtime,
        &payload.sabre,
        &payload.e9tool,
        &payload.e9patch,
    ] {
        if path.as_os_str().is_empty() {
            continue;
        }
        let resolved = fs::canonicalize(path).map_err(|error| PluginFailure {
            code: EX_CONFIG,
            message: format!(
                "error: {} payload resource {} is unavailable: {error}",
                spec.helper,
                path.display()
            ),
        })?;
        if !resolved.starts_with(&release) || !resolved.is_file() {
            return Err(PluginFailure {
                code: EX_CONFIG,
                message: format!(
                    "error: {} returned a resource outside its validated release: {}",
                    spec.helper,
                    path.display()
                ),
            });
        }
    }
    Ok(payload)
}

pub(crate) fn as_io_error(error: PluginFailure) -> io::Error {
    io::Error::other(format!("{} (exit {})", error.message, error.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SABRE: PluginSpec = PluginSpec {
        backend: "sabre",
        helper: "hermit-sabre",
        abi: hermit_plugin_protocol::SABRE_DETCORE_ABI_TAG,
    };

    #[test]
    fn missing_helper_message_is_actionable() {
        let error = select_helper_from(
            SABRE,
            [
                PathBuf::from("/nonexistent/hermit/bin/hermit-sabre"),
                PathBuf::from("/nonexistent/cargo/bin/hermit-sabre"),
            ],
            Some(OsStr::new("")),
        )
        .unwrap_err();
        assert_eq!(error.code, EX_UNAVAILABLE);
        assert!(error.message.contains("cargo install hermit-sabre"));
    }

    #[test]
    fn non_executable_selected_candidate_blocks_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("root/bin/hermit-sabre");
        let fallback_directory = directory.path().join("path");
        let fallback = fallback_directory.join("hermit-sabre");
        fs::create_dir_all(selected.parent().unwrap()).unwrap();
        fs::create_dir_all(&fallback_directory).unwrap();
        fs::write(&selected, b"not executable").unwrap();
        fs::write(&fallback, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&fallback, fs::Permissions::from_mode(0o755)).unwrap();

        let error = select_helper_from(
            SABRE,
            [selected, PathBuf::from("/nonexistent/cargo-helper")],
            Some(fallback_directory.as_os_str()),
        )
        .unwrap_err();
        assert_eq!(error.code, EX_CONFIG);
        assert!(error.message.contains("refusing to fall back"));
    }
}
