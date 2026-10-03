// Copyright (c) Meta Platforms, Inc. and affiliates.
// Licensed under the BSD-style license in the LICENSE file.

//! Admission-only consumption of separately certified failed-resource cleanup.
//! Original execution receipts remain failed. This module cannot construct a
//! normal terminal receipt, runtime capability, trace result, or syscall proof.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

mod original;
#[cfg(test)]
mod tests;
mod wire;

const PRODUCER: &[u8] = include_bytes!("../../scripts/network_recovery.py");
const ACCEPTED_LIMIT: usize = 1_048_576;
const UNIX_LIMIT: usize = 65_536;

fn require(condition: bool, reason: &'static str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(reason))
    }
}
fn digest(bytes: &[u8]) -> String {
    detcore::Digest::new(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn array(value: &Value, length: usize) -> io::Result<&[Value]> {
    value
        .as_array()
        .filter(|v| v.len() == length)
        .map(Vec::as_slice)
        .ok_or_else(|| io::Error::other("resource proof array arity differs"))
}
fn list(value: &Value) -> io::Result<&[Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| io::Error::other("resource proof is not an array"))
}
fn number(value: &Value) -> io::Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| io::Error::other("resource proof integer differs"))
}
fn text(value: &Value) -> io::Result<&str> {
    value
        .as_str()
        .filter(|s| s.is_ascii() && s.len() <= 8192 && !s.contains('\0'))
        .ok_or_else(|| io::Error::other("resource proof string differs"))
}
fn hex(value: &Value, length: usize) -> io::Result<&str> {
    let s = text(value)?;
    require(
        s.len() == length
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "resource proof hex identity differs",
    )?;
    Ok(s)
}
fn label(value: &Value) -> io::Result<&str> {
    let s = hex(value, 32)?;
    require(
        s.bytes().any(|b| b != b'0'),
        "resource proof launch label is zero",
    )?;
    Ok(s)
}
fn metadata_identity(m: &std::fs::Metadata) -> Value {
    serde_json::json!([m.dev(), m.ino(), m.mode(), m.uid()])
}
fn descriptor_identity(fd: BorrowedFd<'_>) -> io::Result<Value> {
    let file = File::from(fd.try_clone_to_owned()?);
    Ok(metadata_identity(&file.metadata()?))
}

/// Acquire a shared lock on the actual retained open directory description.
/// There is deliberately no unlock-on-return guard: the owner keeps this lock
/// until its final description closes, including across dup/SCM_RIGHTS/clone.
/// Recovery's exclusive nonblocking flock therefore cannot race native load.
pub(crate) fn lock_launch_directory(fd: BorrowedFd<'_>) -> io::Result<()> {
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Root {
    file: File,
    path: PathBuf,
    identity: Value,
}
impl Root {
    fn open(path: &Path) -> io::Result<Self> {
        require(
            path.is_absolute() && path.canonicalize()? == path,
            "resource root is not canonical",
        )?;
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let m = file.metadata()?;
        require(
            m.is_dir() && m.uid() == unsafe { libc::getuid() } && m.mode() & 0o7777 == 0o700,
            "resource root owner or mode differs",
        )?;
        lock_launch_directory(file.as_fd())?;
        let root = Self {
            file,
            path: path.to_owned(),
            identity: metadata_identity(&m),
        };
        root.recheck()?;
        Ok(root)
    }
    fn recheck(&self) -> io::Result<()> {
        require(
            metadata_identity(&self.file.metadata()?) == self.identity
                && metadata_identity(&self.path.symlink_metadata()?) == self.identity,
            "resource root name or held identity changed",
        )
    }
    fn read(&self, name: &str, bound: usize) -> io::Result<Receipt> {
        require(
            !name.contains('/') && !matches!(name, "" | "." | ".."),
            "resource receipt name differs",
        )?;
        let c = std::ffi::CString::new(name).map_err(io::Error::other)?;
        let raw = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(raw) };
        let m = file.metadata()?;
        require(
            m.is_file()
                && m.uid() == unsafe { libc::getuid() }
                && m.mode() & 0o7777 == 0o600
                && m.nlink() == 1
                && m.len() <= bound as u64,
            "resource receipt custody differs",
        )?;
        let mut bytes = Vec::new();
        (&mut file).take(bound as u64 + 1).read_to_end(&mut bytes)?;
        require(
            bytes.len() == m.len() as usize && bytes.len() <= bound,
            "resource receipt read changed",
        )?;
        let receipt = Receipt {
            file,
            name: name.to_owned(),
            bytes,
            stat: file_stat(&m),
        };
        receipt.recheck(self)?;
        Ok(receipt)
    }
}
fn file_stat(m: &std::fs::Metadata) -> Value {
    serde_json::json!([
        m.dev(),
        m.ino(),
        m.mode(),
        m.uid(),
        m.nlink(),
        m.len(),
        i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
        i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec())
    ])
}
struct Receipt {
    file: File,
    name: String,
    bytes: Vec<u8>,
    stat: Value,
}
impl Receipt {
    fn recheck(&self, root: &Root) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        let mut raw = vec![0; self.bytes.len() + 1];
        let got = self.file.read_at(&mut raw, 0)?;
        require(
            got == self.bytes.len()
                && raw[..got] == self.bytes
                && file_stat(&self.file.metadata()?) == self.stat
                && file_stat(&root.path.join(&self.name).symlink_metadata()?) == self.stat,
            "resource receipt changed during admission",
        )
    }
    fn matches_proof(&self, proof: &Value, prefix: Option<&[u8]>) -> io::Result<()> {
        let p = array(proof, 10)?;
        let s = array(&self.stat, 8)?;
        require(
            text(&p[0])? == self.name && p[1..6] == s[..5],
            "resource file identity differs",
        )?;
        let bytes = prefix.unwrap_or(&self.bytes);
        require(
            number(&p[6])? == bytes.len() as u64 && hex(&p[9], 64)? == digest(bytes),
            "resource original file length or digest differs",
        )?;
        if prefix.is_none() {
            require(
                p[7..9] == s[6..8],
                "resource unchanged file timestamps differ",
            )?;
        } else {
            number(&p[7])?;
            number(&p[8])?;
        }
        Ok(())
    }
}

/// Actual CLI-configured peer roots, opened and locked before either census.
/// Certificate strings never select a path to open. The caller retains this
/// object with the accepted launch owner through its publication or failure.
pub struct ResourceRecoveryContext {
    accepted: Root,
    unix: Root,
    pins: Root,
}
impl ResourceRecoveryContext {
    /// Open only the three roots selected by the original invocation.
    pub fn open(accepted: &Path, unix: &Path, pins: &Path) -> io::Result<Self> {
        require(
            unsafe { libc::getuid() == libc::geteuid() },
            "resource admission uid differs",
        )?;
        let mut paths = [(accepted, 0), (unix, 1), (pins, 2)];
        paths.sort_by_key(|(p, _)| *p);
        let mut roots: [Option<Root>; 3] = [None, None, None];
        for (path, index) in paths {
            roots[index] = Some(Root::open(path)?);
        }
        let [Some(accepted), Some(unix), Some(pins)] = roots else {
            unreachable!()
        };
        let identities: BTreeSet<_> = [&accepted, &unix, &pins]
            .into_iter()
            .map(|r| (r.identity[0].as_u64(), r.identity[1].as_u64()))
            .collect();
        require(identities.len() == 3, "resource recovery roots alias")?;
        Ok(Self {
            accepted,
            unix,
            pins,
        })
    }
    pub(crate) fn accepted_resolved(&self, root: BorrowedFd<'_>, label: &str) -> bool {
        self.resolve(0, root, label).is_ok()
    }
    pub(crate) fn unix_resolved(
        &self,
        recovery: BorrowedFd<'_>,
        pins: BorrowedFd<'_>,
        label: &str,
    ) -> bool {
        descriptor_identity(pins).is_ok_and(|v| v == self.pins.identity)
            && self.resolve(1, recovery, label).is_ok()
    }
    fn resolve(&self, domain: usize, actual: BorrowedFd<'_>, selected: &str) -> io::Result<()> {
        let roots = [&self.accepted, &self.unix];
        let prefixes = ["accepted-b1-", "guard-b1-"];
        require(
            domain < 2 && descriptor_identity(actual)? == roots[domain].identity,
            "resource admission owner differs",
        )?;
        label(&Value::String(selected.to_owned()))?;
        let selected_file = roots[domain].read(
            &format!("{}{selected}.terminal.jsonl", prefixes[domain]),
            [ACCEPTED_LIMIT, UNIX_LIMIT][domain],
        )?;
        let selected_rows = raw_rows(&selected_file.bytes, [5, 4][domain])?;
        let intent_raw = selected_rows[selected_rows.len() - 2];
        let result_raw = selected_rows[selected_rows.len() - 1];
        let intent: Value = serde_json::from_slice(intent_raw)?;
        let result: Value = serde_json::from_slice(result_raw)?;
        let i = array(&intent, 11)?;
        let accepted_input = array(&i[6], 7)?;
        let unix_input = array(&i[7], 8)?;
        let labels = [label(&accepted_input[2])?, label(&unix_input[2])?];
        require(
            labels[domain] == selected,
            "resource selected identity differs",
        )?;
        let files = [
            ["terminal.jsonl", "stdout.log", "stderr.log"].map(|role| {
                self.accepted.read(
                    &format!("accepted-b1-{}.{}", labels[0], role),
                    ACCEPTED_LIMIT,
                )
            }),
            ["terminal.jsonl", "stdout.log", "stderr.log"].map(|role| {
                self.unix
                    .read(&format!("guard-b1-{}.{}", labels[1], role), UNIX_LIMIT)
            }),
        ];
        let mut held = Vec::new();
        for row in files {
            for file in row {
                held.push(file?);
            }
        }
        let accepted = &held[..3];
        let unix = &held[3..];
        let a_rows = raw_rows(&accepted[0].bytes, 5)?;
        let u_rows = raw_rows(&unix[0].bytes, 4)?;
        require(
            a_rows[3] == intent_raw
                && u_rows[2] == intent_raw
                && a_rows[4] == result_raw
                && u_rows[3] == result_raw,
            "resource paired raw rows differ",
        )?;
        let a_prefix =
            &accepted[0].bytes[..accepted[0].bytes.len() - intent_raw.len() - result_raw.len()];
        let u_prefix = &unix[0].bytes[..unix[0].bytes.len() - intent_raw.len() - result_raw.len()];
        let journal = self
            .unix
            .read(&format!("ugb1-{}", &labels[1][..16]), UNIX_LIMIT)?;
        match text(&array(&result, 12)?[0])? {
            "hermit-failed-resource-result-v1" => {
                wire::validate(self, &intent, intent_raw, &result)?;
            }
            "hermit-failed-resource-resume-result-v1" => {
                wire::validate_resume(self, &intent, intent_raw, &result)?;
            }
            _ => return Err(io::Error::other("resource result protocol differs")),
        }
        original::validate(self, &intent, accepted, unix, &journal, a_prefix, u_prefix)?;
        for root in [&self.accepted, &self.unix, &self.pins] {
            root.recheck()?;
        }
        for (root, receipts) in [(roots[0], accepted), (roots[1], unix)] {
            for receipt in receipts {
                receipt.recheck(root)?;
            }
        }
        journal.recheck(&self.unix)?;
        selected_file.recheck(roots[domain])?;
        Ok(())
    }
}
fn raw_rows(bytes: &[u8], count: usize) -> io::Result<Vec<&[u8]>> {
    require(bytes.ends_with(b"\n"), "resource rows are incomplete")?;
    let rows: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
    require(
        rows.len() == count && rows.iter().all(|r| r.len() > 1),
        "resource row population differs",
    )?;
    Ok(rows)
}
