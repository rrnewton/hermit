//! Held cgroup-v2 ownership shared by bounded Hermit test supervisors.
//!
//! The owner retains the parent, child, and every control descriptor. Reads,
//! signals, and removal authenticate the held identity against its parent name.
//! This type supplies ownership operations, not a completion verdict: callers
//! must reap their children, prove emptiness, retain final CPU, and explicitly
//! remove the child. Dropping this value does not claim cleanup succeeded.

use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

fn checked_child_name(name: &str) -> Result<CString, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err("owned cgroup name must be one normal nonempty component without NUL".into());
    }
    CString::new(name).map_err(|_| "owned cgroup name contains a NUL byte".into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn file_identity(file: &File, label: &str) -> Result<FileIdentity, String> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(format!(
            "cannot inspect {label} identity: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn cgroup_text(file: &File, label: &str) -> Result<String, String> {
    const LIMIT: usize = 8192;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let count = file
            .read_at(&mut chunk, bytes.len() as u64)
            .map_err(|error| format!("cannot read {label}: {error}"))?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > LIMIT {
            return Err(format!("{label} exceeds the {LIMIT}-byte accounting bound"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|error| format!("{label} is not valid UTF-8: {error}"))
}

fn cgroup_field(file: &File, file_label: &str, field: &str) -> Result<u64, String> {
    let text = cgroup_text(file, file_label)?;
    let mut found = None;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let value = words
            .next()
            .ok_or_else(|| format!("{file_label} has no value for {name:?}"))?;
        if words.next().is_some() {
            return Err(format!("{file_label} has extra fields on line {line:?}"));
        }
        if name == field {
            if found.is_some() {
                return Err(format!("{file_label} repeats field {field:?}"));
            }
            found = Some(
                value
                    .parse::<u64>()
                    .map_err(|error| format!("{file_label} has invalid {field}: {error}"))?,
            );
        }
    }
    found.ok_or_else(|| format!("{file_label} is missing field {field:?}"))
}

fn openat_file(directory: &File, name: &str, flags: i32, label: &str) -> Result<File, String> {
    let name = CString::new(name).expect("owned cgroup control names contain no NUL");
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(format!(
            "cannot open {label}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn current_cgroup_directory() -> Result<PathBuf, String> {
    let raw = fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("cannot read /proc/self/cgroup: {error}"))?;
    let mut unified = raw.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = unified
        .next()
        .ok_or_else(|| "/proc/self/cgroup has no unified cgroup v2 entry".to_string())?;
    if unified.next().is_some() {
        return Err("/proc/self/cgroup has multiple unified cgroup v2 entries".into());
    }
    let relative = Path::new(path.trim_start_matches('/'));
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(format!(
            "unified cgroup path {path:?} is not a relative kernel path"
        ));
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

pub struct OwnedCpuCgroup {
    parent: File,
    child: File,
    cpu_stat: File,
    events: File,
    procs_read: File,
    procs_write: File,
    kill: File,
    name: CString,
    identity: FileIdentity,
    path: PathBuf,
}

impl OwnedCpuCgroup {
    /// Create a fresh child under the caller's current delegated cgroup.
    pub fn create(name: &str) -> Result<Self, String> {
        checked_child_name(name)?;
        let parent_path = current_cgroup_directory()?;
        Self::create_in(&parent_path, name)
    }

    /// Create a fresh named child beneath a held cgroup-v2 directory.
    /// Existing names are refused; the caller owns explicit cleanup.
    pub fn create_in(parent_path: &Path, name_text: &str) -> Result<Self, String> {
        let name = checked_child_name(name_text)?;
        let parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent_path)
            .map_err(|error| {
                format!(
                    "cannot open delegated cgroup {}: {error}",
                    parent_path.display()
                )
            })?;
        let mut filesystem = unsafe { std::mem::zeroed::<libc::statfs>() };
        if unsafe { libc::fstatfs(parent.as_raw_fd(), &mut filesystem) } != 0 {
            return Err(format!(
                "cannot inspect delegated cgroup filesystem: {}",
                io::Error::last_os_error()
            ));
        }
        const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
        if filesystem.f_type != CGROUP2_SUPER_MAGIC {
            return Err(format!(
                "delegated cgroup {} is not on cgroup v2",
                parent_path.display()
            ));
        }
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
            return Err(format!(
                "cannot create fresh attempt cgroup {}: {}",
                parent_path.join(name_text).display(),
                io::Error::last_os_error()
            ));
        }
        let result = (|| {
            let child = openat_file(
                &parent,
                name_text,
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                "owned attempt cgroup",
            )?;
            let identity = file_identity(&child, "owned attempt cgroup")?;
            let open_control = |control: &str, flags: i32| -> Result<File, String> {
                let file = openat_file(
                    &child,
                    control,
                    flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    &format!("owned attempt cgroup {control}"),
                )?;
                let control_identity = file_identity(&file, control)?;
                if control_identity.device != identity.device {
                    return Err(format!(
                        "owned attempt cgroup {control} is on device {}, expected {}",
                        control_identity.device, identity.device
                    ));
                }
                Ok(file)
            };
            let cpu_stat = open_control("cpu.stat", libc::O_RDONLY)?;
            let events = open_control("cgroup.events", libc::O_RDONLY)?;
            let procs_read = open_control("cgroup.procs", libc::O_RDONLY)?;
            let procs_write = open_control("cgroup.procs", libc::O_WRONLY)?;
            let kill = open_control("cgroup.kill", libc::O_WRONLY)?;
            let owned = Self {
                parent: parent.try_clone().map_err(|error| {
                    format!("cannot retain delegated cgroup descriptor: {error}")
                })?,
                child,
                cpu_stat,
                events,
                procs_read,
                procs_write,
                kill,
                name: name.clone(),
                identity,
                path: parent_path.join(name_text),
            };
            owned.verify_identity()?;
            if owned.cpu_usage_usec()? != 0 {
                return Err("fresh attempt cgroup has nonzero cpu.stat usage_usec".into());
            }
            if owned.populated()? || !owned.procs_empty()? {
                return Err("fresh attempt cgroup is not empty before enrollment".into());
            }
            Ok(owned)
        })();
        match result {
            Ok(owned) => Ok(owned),
            Err(error) => {
                if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                    == 0
                {
                    Err(error)
                } else {
                    Err(format!(
                        "{error}; partial owned-cgroup initialization cleanup also failed: {}",
                        io::Error::last_os_error()
                    ))
                }
            }
        }
    }

    fn verify_identity(&self) -> Result<(), String> {
        let held = file_identity(&self.child, "held attempt cgroup")?;
        if held != self.identity {
            return Err("held attempt cgroup identity changed".into());
        }
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(format!(
                "cannot authenticate owned attempt cgroup path: {}",
                io::Error::last_os_error()
            ));
        }
        let named = FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        };
        if named != self.identity || stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err("owned attempt cgroup path was replaced".into());
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Descriptor for enrollment before exec. Keep this owner alive through
    /// the complete spawn; the child writes `0\n` using only async-signal-safe
    /// operations before running any guest code.
    pub fn enrollment_fd(&self) -> BorrowedFd<'_> {
        self.procs_write.as_fd()
    }

    pub fn cpu_usage_usec(&self) -> Result<u64, String> {
        self.verify_identity()?;
        cgroup_field(&self.cpu_stat, "owned attempt cpu.stat", "usage_usec")
    }

    pub fn populated(&self) -> Result<bool, String> {
        self.verify_identity()?;
        match cgroup_field(&self.events, "owned attempt cgroup.events", "populated")? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "owned attempt cgroup.events has invalid populated value {value}"
            )),
        }
    }

    pub fn procs_empty(&self) -> Result<bool, String> {
        self.verify_identity()?;
        Ok(cgroup_text(&self.procs_read, "owned attempt cgroup.procs")?
            .trim()
            .is_empty())
    }

    pub fn kill(&self) -> Result<(), String> {
        self.verify_identity()?;
        loop {
            let written = unsafe { libc::write(self.kill.as_raw_fd(), b"1\n".as_ptr().cast(), 2) };
            if written == 2 {
                return Ok(());
            }
            if written < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(if written < 0 {
                format!(
                    "cannot kill owned attempt cgroup: {}",
                    io::Error::last_os_error()
                )
            } else {
                format!("short write to owned attempt cgroup.kill: {written} bytes")
            });
        }
    }

    /// Remove only the verified, empty child. The caller must first reap its
    /// descendants and retain any final CPU reading it needs after removal.
    pub fn remove_empty(&mut self) -> Result<(), String> {
        self.verify_identity()?;
        if self.populated()? || !self.procs_empty()? {
            return Err("cannot remove populated owned attempt cgroup".into());
        }
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(format!(
                "cannot remove owned empty attempt cgroup: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn invalid_names_are_refused_before_delegation_or_creation() {
        let directory = tempfile::tempdir().unwrap();
        let absent_parent = directory.path().join("absent-parent");
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "/absolute",
            "trailing/",
            "embedded\0nul",
        ] {
            for parent in [directory.path(), absent_parent.as_path()] {
                let error = OwnedCpuCgroup::create_in(parent, name)
                    .err()
                    .expect("invalid child names must be refused");
                assert!(
                    error.contains("one normal nonempty component"),
                    "{name:?}: {error}"
                );
                assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn valid_linux_component_names_preserve_the_exact_name() {
        // Dots within a name and whitespace are normal Linux filename bytes;
        // callers provide their own namespace, not an allowlist imposed here.
        for name in [
            "hermit-nextest-attempt-123-0123456789abcdef",
            "verify.run-2",
            "with space",
        ] {
            assert_eq!(
                checked_child_name(name).unwrap().as_bytes(),
                name.as_bytes()
            );
        }
    }

    #[test]
    fn non_cgroup_and_symlinked_parents_are_refused_without_creating_a_child() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("ordinary-directory");
        fs::create_dir(&parent).unwrap();
        let link = directory.path().join("alias");
        symlink(&parent, &link).unwrap();
        let ordinary_error = OwnedCpuCgroup::create_in(&parent, "child")
            .err()
            .expect("ordinary filesystems cannot provide cgroup controls");
        assert!(
            ordinary_error.contains("not on cgroup v2"),
            "{ordinary_error}"
        );
        let link_error = OwnedCpuCgroup::create_in(&link, "child")
            .err()
            .expect("symlinked delegation must not be followed");
        assert!(
            link_error.contains("cannot open delegated cgroup"),
            "{link_error}"
        );
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    }
}
