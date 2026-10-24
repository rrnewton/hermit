/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Writes a run's host-input records to its `host_input_log` when the run
//! ends (see `detcore_model::host_input`), sums them up as the run's
//! [`HostInputSet`] for its summary and its preemption record, and compares
//! the opens of a schedule replay against the recorded run's set
//! ([`ReplayHostInputs`]).
//!
//! The records are an observation for the verification layer, not Detcore
//! state: Detcore's openat handler reports each one to the global state
//! (`tool_global::record_host_input`), which keeps them for the run, and
//! nothing reads them back, no scheduling decision consults them, and nothing
//! the guest sees depends on them. They are written once, after the run, so
//! the log stays empty while any guest runs; a run that does not end normally
//! writes nothing, which leaves its divergence unexplained rather than
//! explained by partial evidence.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use detcore_model::host_input::HOST_INPUT_CONTENT_LIMIT;
use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInput;
use detcore_model::host_input::HostInputChange;
use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use detcore_model::host_input::HostInputSet;
use detcore_model::host_input::HostMutationRecord;
use detcore_model::host_input::guest_rebound;
use detcore_model::host_input::host_input_changed;
use detcore_model::host_input::is_kernel_object;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

/// Write `records` to `log`, one JSON line each, then one
/// [`HostMutationRecord`] line for each path in `mutated`, followed by a
/// [`HostInputLogEnd`] line naming how many lines precede it, so that a
/// reader can tell a complete log from a cut one.
pub(crate) fn write(log: &Path, records: &[HostInputRecord], mutated: &BTreeSet<String>) {
    let mut text = Vec::new();
    let end = HostInputLogEnd {
        records: (records.len() + mutated.len()) as u64,
    };
    let encoded = records
        .iter()
        .map(serde_json::to_vec)
        .chain(mutated.iter().map(|path| {
            serde_json::to_vec(&HostMutationRecord {
                mutated: path.clone(),
            })
        }))
        .chain(std::iter::once(serde_json::to_vec(&end)))
        .try_for_each(|line| {
            text.extend(line?);
            text.push(b'\n');
            Ok::<(), serde_json::Error>(())
        });
    let written = encoded
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::write(log, &text));
    if let Err(error) = written {
        warn!(
            "[detcore] cannot write the host-input log {}: {}",
            log.display(),
            error
        );
    }
}

/// The run's [`HostInputSet`]: for each path the guest opened, in path order,
/// the identity and bytes its first open found, leaving out kernel objects
/// and paths the guest rebound itself (`mutated`, or a relative path), and
/// the set's digest (`HostInputSet`).
pub(crate) fn input_set(records: &[HostInputRecord], mutated: &BTreeSet<String>) -> HostInputSet {
    let mut first: BTreeMap<&str, &HostInputRecord> = BTreeMap::new();
    for record in records {
        first.entry(record.path.as_str()).or_insert(record);
    }
    let files: Vec<HostInput> = first
        .into_iter()
        .filter(|(path, record)| {
            !record.kernel_object && !is_kernel_object(path) && !guest_rebound(mutated, path)
        })
        .map(|(path, record)| HostInput {
            path: path.to_owned(),
            identity: record.identity,
            content: record.content.clone(),
        })
        .collect();
    // One line per file: its path (which holds no NUL), then its bytes'
    // digest where hashed, else its identity.
    let mut hasher = Sha256::new();
    for file in &files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        match &file.content {
            Some(content) => {
                hasher.update(b"content ");
                hasher.update(content.as_bytes());
            }
            None => {
                hasher.update(b"identity ");
                hasher.update(serde_json::to_vec(&file.identity).expect("an identity serializes"));
            }
        }
        hasher.update(b"\n");
    }
    HostInputSet {
        digest: hex(&hasher.finalize()),
        files,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The SHA-256 of the bytes of the regular file whose host identity is
/// `identity`, read through `path` (a guest descriptor's `/proc/<tid>/fd/<n>`
/// link, which opens the same file with an offset of its own), or `None` for
/// a file larger than [`HOST_INPUT_CONTENT_LIMIT`] or one the tool cannot
/// read. Kept per identity for the process, so a file opened again unchanged
/// is not read again.
pub(crate) fn content_digest(path: &Path, identity: HostFileIdentity) -> Option<String> {
    static DIGESTS: Mutex<Option<HashMap<HostFileIdentity, Option<String>>>> = Mutex::new(None);
    if u64::try_from(identity.size).map_or(true, |size| size > HOST_INPUT_CONTENT_LIMIT) {
        return None;
    }
    if let Some(known) = DIGESTS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|digests| digests.get(&identity))
    {
        return known.clone();
    }
    let digest = read_digest(path);
    DIGESTS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(identity, digest.clone());
    digest
}

/// The SHA-256 of the file at `path`, read to its end but no further than
/// [`HOST_INPUT_CONTENT_LIMIT`] bytes, or `None` when it cannot be read or is
/// longer than that. Raw syscalls only: in-guest Detcore must not call
/// libc's wrappers, which the guest may define (`crate::util::raw_syscall`).
fn read_digest(path: &Path) -> Option<String> {
    let file = crate::util::RawFd::open(path, libc::O_RDONLY | libc::O_NONBLOCK).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    let mut read = 0_u64;
    loop {
        let count = file.read_at(&mut buffer, read).ok()?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > HOST_INPUT_CONTENT_LIMIT {
            return None;
        }
        hasher.update(&buffer[..count]);
    }
    Some(hex(&hasher.finalize()))
}

impl<T: crate::record_or_replay::RecordOrReplay> crate::Detcore<T> {
    /// Report the files the kernel opened for the exec this image came from
    /// to the global state's host-input records ([`HostInputRecord::exec`]):
    /// the image, read through the kernel's own `exe` link, and its program
    /// interpreter, looked up in the process's root. For a `#!` script the
    /// image is the interpreter the kernel finally ran, not the script. Under
    /// a chroot an absolute symbolic link on the interpreter's path resolves
    /// from the tool's root, not the guest's (as for `fifo_peer::fifo_at`).
    /// Nothing where the tool cannot see the thread's procfs directory. Raw
    /// syscalls only, as for [`read_digest`].
    pub(crate) async fn record_exec_host_inputs<G: reverie::Guest<Self>>(&self, guest: &mut G) {
        let Some(task) = self.guest_task_directory(guest) else {
            return;
        };
        let Ok(root) = crate::util::raw_read_link(&task.join("root")) else {
            return;
        };
        let exe = task.join("exe");
        let mut files = Vec::new();
        if let Some(image) = crate::util::raw_read_link(&exe)
            .ok()
            .and_then(|image| Some(Path::new("/").join(image.strip_prefix(&root).ok()?)))
        {
            files.push((image, exe.clone()));
        }
        if let Some(interpreter) = crate::exec_record::program_interpreter(&exe)
            && let Ok(relative) = interpreter.strip_prefix("/")
        {
            let host = root.join(relative);
            files.push((interpreter, host));
        }
        for (path, host) in files {
            let Ok(stat) = crate::util::raw_stat(&host) else {
                continue;
            };
            let identity = HostFileIdentity::from_stat(&stat);
            let content = (stat.st_mode & libc::S_IFMT == libc::S_IFREG)
                .then(|| content_digest(&host, identity))
                .flatten();
            crate::tool_global::record_host_input(
                guest,
                path.to_string_lossy().into_owned(),
                identity,
                content,
                true,
                false,
            )
            .await;
        }
    }
}

/// A schedule replay's comparison of the host files its guest opens against
/// the recorded run's [`HostInputSet`]. Each path is compared once, at its
/// first open in this run; a path the recorded set does not hold, a kernel
/// object, or one the guest has rebound by then is not compared.
#[derive(Debug)]
pub(crate) struct ReplayHostInputs {
    recorded: HostInputSet,
    opened: HashSet<String>,
    changes: Vec<HostInputChange>,
}

impl ReplayHostInputs {
    pub(crate) fn new(recorded: HostInputSet) -> Self {
        let recorded = recorded.normalized();
        Self {
            recorded,
            opened: HashSet::new(),
            changes: Vec::new(),
        }
    }

    /// Compare an open of `path` that found `identity` and, where it was
    /// hashed, `content` (`kernel_object` when the descriptor's own path is a
    /// kernel object), with `mutated` the paths the guest has rebound so
    /// far. Returns the change when this first open of the path found a
    /// different file than the recording ([`host_input_changed`]).
    pub(crate) fn observe(
        &mut self,
        path: &str,
        identity: HostFileIdentity,
        content: Option<&str>,
        kernel_object: bool,
        mutated: &BTreeSet<String>,
    ) -> Option<&HostInputChange> {
        if !self.opened.insert(path.to_owned())
            || kernel_object
            || is_kernel_object(path)
            || guest_rebound(mutated, path)
        {
            return None;
        }
        let recorded = self.recorded.file(path)?;
        let now = HostInput {
            path: path.to_owned(),
            identity,
            content: content.map(str::to_owned),
        };
        if !host_input_changed(recorded, &now) {
            return None;
        }
        self.changes.push(HostInputChange {
            path: now.path,
            recorded: recorded.identity,
            now: identity,
        });
        self.changes.last()
    }

    /// The changes found so far, in the order of their first opens.
    pub(crate) fn changes(&self) -> &[HostInputChange] {
        &self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(ino: u64) -> HostFileIdentity {
        HostFileIdentity {
            dev: 33,
            ino,
            size: 100 + ino as i64,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
        }
    }

    fn open(path: &str, ino: u64, syscall: u64) -> HostInputRecord {
        HostInputRecord {
            path: path.to_owned(),
            dtid: 3,
            syscall,
            identity: identity(ino),
            content: None,
            exec: false,
            kernel_object: false,
        }
    }

    #[test]
    fn the_set_keeps_first_opens_of_host_files_in_path_order() {
        let records = [
            open("/etc/ld.so.cache", 9, 6),
            open("/proc/self/maps", 1, 7),
            open("/lib64/libc.so.6", 10, 8),
            open("/etc/ld.so.cache", 11, 9),
            open("/work/out", 12, 10),
            open("relative", 13, 11),
            // `/etc/mtab` names `/proc/self/mounts`.
            HostInputRecord {
                kernel_object: true,
                ..open("/etc/mtab", 14, 12)
            },
        ];
        let mutated = BTreeSet::from(["/work".to_owned()]);
        let set = input_set(&records, &mutated);
        let files: Vec<(&str, u64)> = set
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.identity.ino))
            .collect();
        assert_eq!(files, [("/etc/ld.so.cache", 9), ("/lib64/libc.so.6", 10)]);
        assert_eq!(set.digest.len(), 64);
        assert!(set.digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn the_digest_follows_the_files_alone() {
        let none = BTreeSet::new();
        let first = input_set(&[open("/a", 1, 1), open("/b", 2, 2)], &none);
        let reordered = input_set(&[open("/b", 2, 5), open("/a", 1, 6)], &none);
        let changed = input_set(&[open("/a", 1, 1), open("/b", 3, 2)], &none);
        assert_eq!(first, reordered);
        assert_ne!(first.digest, changed.digest);
        assert_ne!(first.digest, input_set(&[], &none).digest);
        // Where the bytes were hashed they decide, as `host_input_changed`
        // decides: a new inode with the same bytes keeps the digest, other
        // bytes under the same inode change it.
        let hashed = |ino: u64, content: &str| HostInputRecord {
            content: Some(content.to_owned()),
            ..open("/b", ino, 2)
        };
        let before = input_set(&[open("/a", 1, 1), hashed(2, "aa")], &none);
        let rewritten = input_set(&[open("/a", 1, 1), hashed(3, "aa")], &none);
        let edited = input_set(&[open("/a", 1, 1), hashed(2, "bb")], &none);
        assert_eq!(before.digest, rewritten.digest);
        assert_ne!(before.digest, edited.digest);
    }

    #[test]
    fn a_replay_reports_each_changed_file_once_at_its_first_open() {
        let none = BTreeSet::new();
        let recorded = input_set(
            &[open("/etc/ld.so.cache", 9, 6), open("/lib", 10, 7)],
            &none,
        );
        let mut replay = ReplayHostInputs::new(recorded);
        assert_eq!(
            replay.observe("/etc/ld.so.cache", identity(11), None, false, &none),
            Some(&HostInputChange {
                path: "/etc/ld.so.cache".to_owned(),
                recorded: identity(9),
                now: identity(11),
            })
        );
        // A later open of the same path is not compared again.
        assert_eq!(
            replay.observe("/etc/ld.so.cache", identity(12), None, false, &none),
            None
        );
        // An unchanged file, one the recording did not open, and a kernel
        // object are not reported.
        assert_eq!(
            replay.observe("/lib", identity(10), None, false, &none),
            None
        );
        assert_eq!(
            replay.observe("/new", identity(13), None, false, &none),
            None
        );
        assert_eq!(
            replay.observe("/proc/self/maps", identity(14), None, false, &none),
            None
        );
        assert_eq!(replay.changes().len(), 1);
    }

    #[test]
    fn a_replay_compares_hashed_content_before_identity() {
        let none = BTreeSet::new();
        let mut recorded = open("/etc/ld.so.cache", 9, 6);
        recorded.content = Some("aa".to_owned());
        let mut replay = ReplayHostInputs::new(input_set(&[recorded], &none));
        // Rewritten with the same bytes under a new inode: not reported.
        assert_eq!(
            replay.observe("/etc/ld.so.cache", identity(11), Some("aa"), false, &none),
            None
        );
        let mut replay = ReplayHostInputs::new(replay.recorded.clone());
        assert!(
            replay
                .observe("/etc/ld.so.cache", identity(9), Some("bb"), false, &none)
                .is_some()
        );
        // A recorded set out of path order (a hand-edited file) is searched
        // sorted, and an open whose descriptor is a kernel object is not
        // compared.
        let mut unsorted = input_set(&[open("/a", 1, 1), open("/b", 2, 2)], &none);
        unsorted.files.reverse();
        let mut replay = ReplayHostInputs::new(unsorted);
        assert!(
            replay
                .observe("/a", identity(5), None, false, &none)
                .is_some()
        );
        assert_eq!(replay.observe("/b", identity(6), None, true, &none), None);
    }

    #[test]
    fn a_digest_reads_the_whole_file_and_only_a_bounded_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("F");
        std::fs::write(&path, "input\n").unwrap();
        let identity = |size: i64| HostFileIdentity {
            dev: 1,
            ino: 2,
            size,
            mtime_sec: 3,
            mtime_nsec: 4,
        };
        // `printf 'input\n' | sha256sum`
        let expected = "7d3f9b6284c6f36e77b425cac882e8fbbcc97a4727ec20790853076d0f463453";
        assert_eq!(
            content_digest(&path, identity(6)).as_deref(),
            Some(expected)
        );
        // Kept per identity: a rewrite under the same identity is not read again.
        std::fs::write(&path, "other\n").unwrap();
        assert_eq!(
            content_digest(&path, identity(6)).as_deref(),
            Some(expected)
        );
        assert_eq!(
            content_digest(&path, identity(HOST_INPUT_CONTENT_LIMIT as i64 + 1)),
            None
        );
        assert_eq!(read_digest(&directory.path().join("missing")), None);
    }

    #[test]
    fn a_replay_does_not_report_a_file_the_guest_rebound_first() {
        let recorded = input_set(&[open("/work/in", 9, 6)], &BTreeSet::new());
        let mut replay = ReplayHostInputs::new(recorded);
        let mutated = BTreeSet::from(["/work".to_owned()]);
        assert_eq!(
            replay.observe("/work/in", identity(11), None, false, &mutated),
            None
        );
        assert!(replay.changes().is_empty());
    }
}
