use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::symlink;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use flate2::read::GzDecoder;
use hermit_plugin_protocol::EX_CANTCREAT;
use hermit_plugin_protocol::EX_CONFIG;
use hermit_plugin_protocol::EnsureRequest;
use hermit_plugin_protocol::PayloadManifest;
use hermit_plugin_protocol::PluginIdentity;
use sha2::Digest as _;
use sha2::Sha256;

static PAYLOAD_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.tar.gz"));
static PAYLOAD_MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/payload/plugin.json"));

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            ExitCode::from(error.code as u8)
        }
    }
}

struct HelperError {
    code: i32,
    message: String,
}

impl HelperError {
    fn config(message: impl Into<String>) -> Self {
        Self {
            code: EX_CONFIG,
            message: message.into(),
        }
    }

    fn create(path: &Path, error: impl std::fmt::Display) -> Self {
        Self {
            code: EX_CANTCREAT,
            message: format!(
                "error: cannot materialize backend 'e9patch' payload under {}: {error}\n\
                 repair: set HERMIT_DIR to a writable directory or have an administrator materialize this exact hermit-e9patch version",
                path.display()
            ),
        }
    }
}

fn run() -> Result<(), HelperError> {
    let mut args = env::args_os();
    let _program = args.next();
    match (args.next().as_deref(), args.next()) {
        (Some(command), None) if command == OsStr::new("ensure") => {}
        _ => {
            return Err(HelperError::config(
                "error: hermit-e9patch is an internal Hermit backend helper; expected `hermit-e9patch ensure`",
            ));
        }
    }

    let request: EnsureRequest = serde_json::from_reader(io::stdin()).map_err(|error| {
        HelperError::config(format!(
            "error: invalid hermit-e9patch host request: {error}"
        ))
    })?;
    let embedded: PayloadManifest = serde_json::from_str(PAYLOAD_MANIFEST).map_err(|error| {
        HelperError::config(format!(
            "error: hermit-e9patch embedded manifest is invalid: {error}"
        ))
    })?;
    if let Some(field) = request.host.mismatch(&embedded.plugin) {
        return Err(incompatible_message(&request.host, &embedded.plugin, field));
    }

    let root = hermit_dir()?;
    let plugin_root = root.join("plugins/e9patch");
    fs::create_dir_all(plugin_root.join("releases"))
        .map_err(|error| HelperError::create(&root, error))?;
    let _lock =
        ExtractionLock::acquire(&plugin_root).map_err(|error| HelperError::create(&root, error))?;

    let release = plugin_root
        .join("releases")
        .join(embedded.plugin.release_key());
    if validate_release(&release, &embedded).is_err() {
        materialize_release(&plugin_root, &release, &embedded)
            .map_err(|error| HelperError::create(&root, error))?;
    }
    validate_release(&release, &embedded).map_err(HelperError::config)?;
    replace_current(&plugin_root, &release).map_err(|error| HelperError::create(&root, error))?;

    let response = resolved_manifest(&release, embedded);
    serde_json::to_writer(io::stdout(), &response).map_err(|error| {
        HelperError::config(format!("error: failed to encode payload response: {error}"))
    })?;
    Ok(())
}

fn incompatible_message(
    host: &PluginIdentity,
    plugin: &PluginIdentity,
    field: &str,
) -> HelperError {
    let selected = env::current_exe().unwrap_or_else(|_| PathBuf::from("hermit-e9patch"));
    HelperError::config(format!(
        "error: incompatible hermit-e9patch plugin; refusing backend 'e9patch' ({field} mismatch)\n\
         host:   hermit-run {}, Detcore ABI {}, build {}\n\
         plugin: hermit-e9patch {}, Detcore ABI {}, build {}\n\
         selected plugin: {}\n\
         repair with:\n  cargo install --force --locked hermit-e9patch@={}",
        host.package_version,
        host.detcore_abi,
        host.detcore_build_id,
        plugin.package_version,
        plugin.detcore_abi,
        plugin.detcore_build_id,
        selected.display(),
        host.package_version,
    ))
}

fn hermit_dir() -> Result<PathBuf, HelperError> {
    if let Some(root) = env::var_os("HERMIT_DIR") {
        if root.is_empty() {
            return Err(HelperError::config("error: HERMIT_DIR is empty"));
        }
        return Ok(PathBuf::from(root));
    }
    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| HelperError::config("error: HOME is unset; set HERMIT_DIR explicitly"))?;
    Ok(PathBuf::from(home).join(".hermit"))
}

struct ExtractionLock(fs::File);

impl ExtractionLock {
    fn acquire(plugin_root: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(plugin_root.join(".extract.lock"))?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for ExtractionLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn materialize_release(
    plugin_root: &Path,
    release: &Path,
    embedded: &PayloadManifest,
) -> io::Result<()> {
    let releases = plugin_root.join("releases");
    let temporary = releases.join(format!(
        ".extract-{}-{}",
        std::process::id(),
        embedded.plugin.detcore_build_id
    ));
    if temporary.exists() {
        fs::remove_dir_all(&temporary)?;
    }
    fs::create_dir_all(&temporary)?;

    let result = unpack_payload(&temporary)
        .and_then(|_| validate_release(&temporary, embedded).map_err(io::Error::other));
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    if release.exists() {
        if validate_release(release, embedded).is_ok() {
            fs::remove_dir_all(&temporary)?;
            return Ok(());
        }
        let corrupt = releases.join(format!(
            ".corrupt-{}-{}",
            std::process::id(),
            embedded.plugin.detcore_build_id
        ));
        if corrupt.exists() {
            fs::remove_dir_all(&corrupt)?;
        }
        fs::rename(release, &corrupt)?;
    }
    if let Some(parent) = release.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&temporary, release)?;
    Ok(())
}

fn unpack_payload(destination: &Path) -> io::Result<()> {
    let gzip = GzDecoder::new(PAYLOAD_ARCHIVE);
    let mut archive = tar::Archive::new(gzip);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let relative = entry.path()?.into_owned();
        if !safe_relative(&relative) || !entry.header().entry_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "embedded payload contains unsafe path {}",
                    relative.display()
                ),
            ));
        }
        let path = destination.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(entry.header().mode().unwrap_or(0o644))
            .open(path)?;
        io::copy(&mut entry, &mut output)?;
    }
    Ok(())
}

fn validate_release(release: &Path, embedded: &PayloadManifest) -> Result<(), String> {
    let manifest_path = release.join("plugin.json");
    let installed: PayloadManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?,
    )
    .map_err(|error| format!("invalid {}: {error}", manifest_path.display()))?;
    if installed.plugin != embedded.plugin || installed.files != embedded.files {
        return Err("installed plugin manifest does not match the embedded payload".to_owned());
    }
    for (relative, expected) in &embedded.files {
        let relative = Path::new(relative);
        if !safe_relative(relative) {
            return Err(format!(
                "payload manifest contains unsafe path {relative:?}"
            ));
        }
        let path = release.join(relative);
        let bytes = fs::read(&path)
            .map_err(|error| format!("cannot read payload file {}: {error}", path.display()))?;
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != *expected {
            return Err(format!("payload hash mismatch for {}", path.display()));
        }
        if (relative == Path::new("bin/e9tool") || relative == Path::new("bin/e9patch"))
            && fs::metadata(&path)
                .map_err(|error| error.to_string())?
                .permissions()
                .mode()
                & 0o111
                == 0
        {
            return Err(format!(
                "payload executable bit is missing: {}",
                path.display()
            ));
        }
    }
    let resolved = resolved_manifest(release, installed);
    for required in [&resolved.e9tool, &resolved.e9patch] {
        if !required.is_file() {
            return Err(format!(
                "required payload file is missing: {}",
                required.display()
            ));
        }
    }
    for executable in [&resolved.e9tool, &resolved.e9patch] {
        if fs::metadata(executable)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(format!(
                "e9patch payload is not executable: {}",
                executable.display()
            ));
        }
    }
    Ok(())
}

fn resolved_manifest(release: &Path, mut manifest: PayloadManifest) -> PayloadManifest {
    manifest.release_dir = release.to_path_buf();
    manifest.e9tool = release.join(&manifest.e9tool);
    manifest.e9patch = release.join(&manifest.e9patch);
    manifest
}

fn replace_current(plugin_root: &Path, release: &Path) -> io::Result<()> {
    let current = plugin_root.join("current");
    if let Ok(metadata) = fs::symlink_metadata(&current)
        && !metadata.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a symlink", current.display()),
        ));
    }
    let temporary = plugin_root.join(format!(".current-{}.tmp", std::process::id()));
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let relative = release
        .strip_prefix(plugin_root)
        .map_err(io::Error::other)?;
    symlink(relative, &temporary)?;
    fs::rename(temporary, current)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(!safe_relative(Path::new("../escape")));
        assert!(!safe_relative(Path::new("/absolute")));
        assert!(safe_relative(Path::new("lib/runtime.so")));
    }

    #[test]
    fn embedded_manifest_matches_helper_identity() {
        let manifest: PayloadManifest = serde_json::from_str(PAYLOAD_MANIFEST).unwrap();
        assert_eq!(
            manifest.plugin,
            PluginIdentity::with_abi(
                "e9patch",
                env!("CARGO_PKG_VERSION"),
                hermit_plugin_protocol::E9PATCH_ABI_TAG,
                detcore::DETCORE_BUILD_ID,
            )
        );
        assert!(!manifest.files.is_empty());
    }
}
