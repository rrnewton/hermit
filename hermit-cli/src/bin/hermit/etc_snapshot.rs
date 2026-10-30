//! Shared read-only `/etc` snapshots keyed by content hash.
//!
//! In snapshot mode the guest never sees host `/etc` dentries: a private
//! tmpfs is mounted at guest `/etc` and the snapshot directory is bound
//! read-only onto it (see `container::identity_hardening_mounts_with_mode`).
//! Snapshots live under the hermit cache (`$XDG_CACHE_HOME/hermit/etc-snapshots`
//! or `~/.cache/hermit/etc-snapshots`, following `image::rootfs_cache_dir`) and
//! are reused across runs while the host tree is unchanged.

use std::fmt;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use detcore::Digest;
use hermit::Context;
use hermit::Error;

/// How the guest `/etc` is presented; mirrors `--base-env` parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EtcMode {
    /// Today's behaviour: live host `/etc` plus per-file frozen binds.
    Host,
    /// Shared hash-keyed read-only snapshot (the default).
    #[default]
    Snapshot,
    /// Future minimal `/etc`; parsing succeeds but use refuses by name.
    Minimal,
}

impl fmt::Display for EtcMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EtcMode::Host => write!(f, "host"),
            EtcMode::Snapshot => write!(f, "snapshot"),
            EtcMode::Minimal => write!(f, "minimal"),
        }
    }
}

impl FromStr for EtcMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "host" => Ok(EtcMode::Host),
            "snapshot" => Ok(EtcMode::Snapshot),
            "minimal" => Ok(EtcMode::Minimal),
            _ => Err(format!(
                "Expected host | snapshot | minimal, could not parse: {s:?}"
            )),
        }
    }
}

/// Refuse `minimal` by name wherever a mode is consumed.
pub fn refuse_minimal(mode: EtcMode) -> Result<(), Error> {
    if mode == EtcMode::Minimal {
        anyhow::bail!("etc-mode=minimal: not yet implemented");
    }
    Ok(())
}

/// Cache root holding one directory per snapshot hash.
pub fn snapshot_cache_root() -> PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        std::env::temp_dir()
    };
    base.join("hermit").join("etc-snapshots")
}

const READY_MARKER: &str = ".hermit-etc-ready";
const SEVEN_DAYS_SECS: u64 = 7 * 24 * 3600;

fn hash_bytes(data: &[u8]) -> String {
    Digest::new(data).to_string()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SecretClass {
    Copied,
    EmptyPlaceholder,
    Omitted,
    UnreadableDir,
}

/// Classify one relative path. The world-readable bit is checked on the
/// opened descriptor by the caller; this decides the class from the path.
fn secret_class(rel: &Path, world_readable: bool, is_dir: bool) -> SecretClass {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_dir {
        return if world_readable {
            SecretClass::Copied
        } else {
            SecretClass::UnreadableDir
        };
    }
    if name == "shadow" || name == "gshadow" {
        return SecretClass::EmptyPlaceholder;
    }
    let in_ssh = rel.starts_with("ssh");
    if in_ssh && name.starts_with("ssh_host_") && name.ends_with("_key") {
        return SecretClass::Omitted;
    }
    let lower = rel.to_string_lossy().to_lowercase();
    let in_tls = lower.contains("ssl") || lower.contains("pki") || lower.contains("tls");
    if in_tls && (name.ends_with(".key") || name == "key.pem") && !world_readable {
        return SecretClass::Omitted;
    }
    if world_readable {
        SecretClass::Copied
    } else {
        SecretClass::EmptyPlaceholder
    }
}

struct Entry {
    rel: PathBuf,
    kind: &'static str,
    mode: u32,
    detail: String,
    class: SecretClass,
}

/// Collect the manifest for `root` (sorted). `group_bytes`/`cache_bytes`
/// override the hashed content of `group`/`ld.so.cache` when present.
fn collect_manifest(
    root: &Path,
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let abs_dir = if rel_dir.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel_dir)
        };
        let read = match fs::read_dir(&abs_dir) {
            Ok(r) => r,
            Err(e) if rel_dir.as_os_str().is_empty() => {
                return Err(e).with_context(|| {
                    format!("Failed to list the host /etc tree at {}", root.display())
                });
            }
            Err(_) => continue, // unreadable subdir: its own entry already records it
        };
        let mut children: Vec<PathBuf> = read
            .filter_map(|e| e.ok())
            .map(|e| rel_dir.join(e.file_name()))
            .collect();
        children.sort();
        for rel in children {
            let abs = root.join(&rel);
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Uninspectable entries (locked-down config trees) become
                    // empty placeholders; their bytes are never read.
                    entries.push(Entry {
                        rel,
                        kind: "file",
                        mode: 0,
                        detail: "placeholder".to_string(),
                        class: SecretClass::EmptyPlaceholder,
                    });
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to inspect {} for the /etc snapshot", abs.display())
                    });
                }
            };
            let mode = meta.permissions().mode() & 0o7777;
            let ft = meta.file_type();
            if ft.is_symlink() {
                let target = fs::read_link(&abs)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push(Entry {
                    rel: rel.clone(),
                    kind: "symlink",
                    mode,
                    detail: target,
                    class: SecretClass::Copied,
                });
            } else if ft.is_dir() {
                let class = secret_class(&rel, mode & 0o004 != 0, true);
                entries.push(Entry {
                    rel: rel.clone(),
                    kind: "dir",
                    mode,
                    detail: String::new(),
                    class,
                });
                if class != SecretClass::UnreadableDir {
                    stack.push(rel);
                }
            } else if ft.is_file() {
                // World-readable check on the opened fd, never following links.
                let (world, content_hash) = read_file_hash(&abs, &rel, group_bytes, cache_bytes)?;
                let class = secret_class(&rel, world, false);
                let detail = match class {
                    SecretClass::Copied => content_hash,
                    SecretClass::EmptyPlaceholder => "placeholder".to_string(),
                    SecretClass::Omitted => "omitted".to_string(),
                    SecretClass::UnreadableDir => "unreadable-dir".to_string(),
                };
                entries.push(Entry {
                    rel,
                    kind: "file",
                    mode,
                    detail,
                    class,
                });
            } else {
                entries.push(Entry {
                    rel,
                    kind: "other",
                    mode,
                    detail: "omitted".to_string(),
                    class: SecretClass::Omitted,
                });
            }
        }
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(entries)
}

fn read_file_hash(
    abs: &Path,
    rel: &Path,
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<(bool, String), Error> {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let (true, Some(b)) = (name == "group", group_bytes) {
        return Ok((true, hash_bytes(b)));
    }
    if let (true, Some(b)) = (name == "ld.so.cache", cache_bytes) {
        return Ok((true, hash_bytes(b)));
    }
    // The mode decides readability before any open: a non-world-readable
    // file's bytes are never read (they may also be unopenable here).
    let meta = fs::symlink_metadata(abs)
        .with_context(|| format!("Failed to inspect {} for the /etc snapshot", abs.display()))?;
    if meta.permissions().mode() & 0o004 == 0 {
        return Ok((false, String::new()));
    }
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(abs)
    {
        Ok(f) => f,
        // World-readable by mode but denied by ACLs: treat as unreadable.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok((false, String::new()));
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to open {} for the /etc snapshot", abs.display())
            });
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {} for the /etc snapshot", abs.display()))?;
    Ok((true, hash_bytes(&bytes)))
}

/// Content hash naming a snapshot: sorted paths, kinds, modes and content
/// hashes (placeholder classes contribute their class, never secret bytes).
pub fn snapshot_hash(
    root: &Path,
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<String, Error> {
    let entries = collect_manifest(root, group_bytes, cache_bytes)?;
    let mut manifest = Vec::new();
    for e in &entries {
        manifest.extend_from_slice(e.rel.to_string_lossy().as_bytes());
        manifest.extend_from_slice(b"|");
        manifest.extend_from_slice(e.kind.as_bytes());
        manifest.extend_from_slice(b"|");
        manifest.extend_from_slice(format!("{:o}", e.mode).as_bytes());
        manifest.extend_from_slice(b"|");
        manifest.extend_from_slice(e.detail.as_bytes());
        manifest.extend_from_slice(b"\n");
    }
    Ok(hash_bytes(&manifest))
}

fn reflink_or_copy(src: &Path, dst: &Path) -> Result<(), Error> {
    let src_file = File::open(src)
        .with_context(|| format!("Failed to open {} for the /etc snapshot", src.display()))?;
    let dst_file = File::create(dst)
        .with_context(|| format!("Failed to create {} for the /etc snapshot", dst.display()))?;
    // FICLONE = _IOW(0x94, 9, int) = 0x40049409 on Linux.
    let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), 0x4004_9409, src_file.as_raw_fd()) };
    if rc == 0 {
        return Ok(());
    }
    drop(dst_file);
    fs::copy(src, dst)
        .with_context(|| format!("Failed to copy {} for the /etc snapshot", src.display()))?;
    Ok(())
}

fn build_tree(
    root: &Path,
    dest: &Path,
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<(), Error> {
    let entries = collect_manifest(root, group_bytes, cache_bytes)?;
    for e in &entries {
        let target = dest.join(&e.rel);
        match e.kind {
            "dir" => {
                fs::create_dir_all(&target).with_context(|| {
                    format!(
                        "Failed to create {} for the /etc snapshot",
                        target.display()
                    )
                })?;
            }
            "symlink" => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).ok();
                }
                std::os::unix::fs::symlink(&e.detail, &target).with_context(|| {
                    format!("Failed to link {} for the /etc snapshot", target.display())
                })?;
            }
            "file" => {
                if e.class == SecretClass::Omitted {
                    continue;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).ok();
                }
                let name = e
                    .rel
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if e.class == SecretClass::EmptyPlaceholder {
                    fs::write(&target, b"").with_context(|| {
                        format!("Failed to write {} for the /etc snapshot", target.display())
                    })?;
                } else if let (true, Some(bytes)) = (name == "group", group_bytes) {
                    fs::write(&target, bytes).with_context(|| {
                        format!("Failed to write {} for the /etc snapshot", target.display())
                    })?;
                } else if let (true, Some(bytes)) = (name == "ld.so.cache", cache_bytes) {
                    fs::write(&target, bytes).with_context(|| {
                        format!("Failed to write {} for the /etc snapshot", target.display())
                    })?;
                } else {
                    reflink_or_copy(&root.join(&e.rel), &target)?;
                }
                fs::set_permissions(&target, fs::Permissions::from_mode(e.mode)).ok();
            }
            _ => {}
        }
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn touch_last_used(cache_root: &Path, hash: &str, now: u64) {
    let _ = fs::write(
        cache_root.join(format!("{hash}.last-used")),
        now.to_string(),
    );
}

fn last_used(cache_root: &Path, hash: &str) -> Option<u64> {
    fs::read_to_string(cache_root.join(format!("{hash}.last-used")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Delete snapshots unused for longer than `retention_secs`, never `current`.
pub fn gc_snapshots(cache_root: &Path, current: &str, now: u64, retention_secs: u64) {
    let Ok(read) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in read.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == current || name.starts_with('.') || name.ends_with(".last-used") {
            continue;
        }
        if !entry.path().join(READY_MARKER).exists() {
            continue;
        }
        let used = last_used(cache_root, &name).unwrap_or(0);
        if now.saturating_sub(used) > retention_secs {
            let _ = fs::remove_dir_all(entry.path());
            let _ = fs::remove_file(cache_root.join(format!("{name}.last-used")));
        }
    }
}

/// Ensure a snapshot of `host_etc` exists; returns `(dir, hash)`.
/// `cache_root` is injectable for tests.
pub fn ensure_snapshot_in(
    cache_root: &Path,
    host_etc: &Path,
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<(PathBuf, String), Error> {
    let hash = snapshot_hash(host_etc, group_bytes, cache_bytes)?;
    fs::create_dir_all(cache_root).with_context(|| {
        format!(
            "Failed to create the /etc snapshot cache at {}",
            cache_root.display()
        )
    })?;
    let dest = cache_root.join(&hash);
    if dest.join(READY_MARKER).exists() {
        touch_last_used(cache_root, &hash, now_secs());
        gc_snapshots(cache_root, &hash, now_secs(), SEVEN_DAYS_SECS);
        return Ok((dest, hash));
    }
    let tmp = cache_root.join(format!(".build-{}-{hash}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)
        .with_context(|| format!("Failed to create {} for the /etc snapshot", tmp.display()))?;
    build_tree(host_etc, &tmp, group_bytes, cache_bytes)?;
    // Freeze contents read-only; the guest bind is read-only regardless.
    for e in collect_manifest(host_etc, group_bytes, cache_bytes)? {
        if e.kind == "file" && e.class != SecretClass::Omitted {
            let _ = fs::set_permissions(tmp.join(&e.rel), fs::Permissions::from_mode(0o444));
        }
    }
    fs::write(tmp.join(READY_MARKER), hash.as_bytes())
        .with_context(|| format!("Failed to mark the /etc snapshot at {}", tmp.display()))?;
    match fs::rename(&tmp, &dest) {
        Ok(()) => {}
        Err(_) if dest.join(READY_MARKER).exists() => {
            let _ = fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to publish the /etc snapshot at {}", dest.display())
            });
        }
    }
    let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o555));
    touch_last_used(cache_root, &hash, now_secs());
    gc_snapshots(cache_root, &hash, now_secs(), SEVEN_DAYS_SECS);
    Ok((dest, hash))
}

/// Ensure a snapshot of the host `/etc` in the default cache.
pub fn ensure_snapshot(
    group_bytes: Option<&[u8]>,
    cache_bytes: Option<&[u8]>,
) -> Result<(PathBuf, String), Error> {
    ensure_snapshot_in(
        &snapshot_cache_root(),
        Path::new("/etc"),
        group_bytes,
        cache_bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_etc() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("passwd"), b"root:x:0:0::/root:/bin/sh\n").unwrap();
        fs::write(root.join("group"), b"root:x:0:\n").unwrap();
        fs::write(root.join("shadow"), b"root:SECRET:1:::\n").unwrap();
        fs::set_permissions(root.join("shadow"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::create_dir(root.join("ssh")).unwrap();
        fs::write(root.join("ssh/ssh_host_rsa_key"), b"PRIVATEKEY").unwrap();
        fs::set_permissions(
            root.join("ssh/ssh_host_rsa_key"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(root.join("ssh/ssh_host_rsa_key.pub"), b"ssh-rsa AAAA").unwrap();
        std::os::unix::fs::symlink("passwd", root.join("passwd-link")).unwrap();
        dir
    }

    #[test]
    fn mode_parsing() {
        assert_eq!("host".parse::<EtcMode>().unwrap(), EtcMode::Host);
        assert_eq!("SNAPSHOT".parse::<EtcMode>().unwrap(), EtcMode::Snapshot);
        assert_eq!("minimal".parse::<EtcMode>().unwrap(), EtcMode::Minimal);
        assert!("bogus".parse::<EtcMode>().is_err());
        assert!(
            refuse_minimal(EtcMode::Minimal)
                .unwrap_err()
                .to_string()
                .contains("not yet implemented")
        );
        assert!(refuse_minimal(EtcMode::Snapshot).is_ok());
    }

    #[test]
    fn hash_keying_stable_and_sensitive() {
        let etc = fake_etc();
        let cache = tempfile::TempDir::new().unwrap();
        let (_, h1) = ensure_snapshot_in(cache.path(), etc.path(), None, None).unwrap();
        let (_, h2) = ensure_snapshot_in(cache.path(), etc.path(), None, None).unwrap();
        assert_eq!(h1, h2, "same content must give the same hash");
        fs::write(etc.path().join("passwd"), b"root:x:0:0::/root:/bin/bash\n").unwrap();
        let (_, h3) = ensure_snapshot_in(cache.path(), etc.path(), None, None).unwrap();
        assert_ne!(h1, h3, "changed content must give a new hash");
    }

    #[test]
    fn secrets_become_placeholders_or_omissions() {
        let etc = fake_etc();
        let cache = tempfile::TempDir::new().unwrap();
        let (dir, _) = ensure_snapshot_in(cache.path(), etc.path(), None, None).unwrap();
        assert_eq!(
            fs::read(dir.join("shadow")).unwrap(),
            b"",
            "shadow must be an empty placeholder"
        );
        assert!(
            !dir.join("ssh/ssh_host_rsa_key").exists(),
            "private keys must be omitted"
        );
        assert_eq!(
            fs::read(dir.join("ssh/ssh_host_rsa_key.pub")).unwrap(),
            b"ssh-rsa AAAA"
        );
        assert_eq!(
            fs::read_link(dir.join("passwd-link")).unwrap(),
            Path::new("passwd")
        );
    }

    #[test]
    fn gc_keeps_recent_and_current_deletes_old() {
        let etc = fake_etc();
        let cache = tempfile::TempDir::new().unwrap();
        let (_, current) = ensure_snapshot_in(cache.path(), etc.path(), None, None).unwrap();
        // Forge an old snapshot.
        let old = cache.path().join("oldhash");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(READY_MARKER), b"oldhash").unwrap();
        fs::write(cache.path().join("oldhash.last-used"), b"1000").unwrap();
        gc_snapshots(
            cache.path(),
            &current,
            1000 + SEVEN_DAYS_SECS + 10,
            SEVEN_DAYS_SECS,
        );
        assert!(!old.exists(), "stale snapshot must be collected");
        assert!(
            cache.path().join(&current).exists(),
            "current snapshot must survive GC"
        );
    }
}
