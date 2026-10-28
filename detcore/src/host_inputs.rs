/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Writes a run's host-input records to its `host_input_log` when the run
//! ends (see `detcore_model::host_input`), hashes the files they name once the
//! guest's processes have exited ([`hash_at_end`]), sums them up as the run's
//! [`HostInputSet`] for its summary and its preemption record, and compares a
//! schedule replay's files against the recorded run's set
//! ([`changes_since`]).
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
use std::path::PathBuf;

use detcore_model::host_input::HOST_INPUT_CONTENT_LIMIT;
use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInput;
use detcore_model::host_input::HostInputChange;
use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use detcore_model::host_input::HostInputSet;
use detcore_model::host_input::HostMutationRecord;
use detcore_model::host_input::HostReopen;
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

/// Whether the first open of `record`'s path counts as a host input file:
/// not a kernel object by either path, a regular file, and not a path the
/// guest rebound itself (`mutated`, or a relative path).
fn host_input_file(record: &HostInputRecord, mutated: &BTreeSet<String>) -> bool {
    !record.kernel_object
        && !record.non_regular
        && !is_kernel_object(&record.path)
        && !guest_rebound(mutated, &record.path)
}

/// The run's [`HostInputSet`]: for each path the guest opened, in path order,
/// the identity and bytes its first open found, leaving out what is not a
/// host input file ([`host_input_file`]), and the set's digest
/// (`HostInputSet`).
pub(crate) fn input_set(records: &[HostInputRecord], mutated: &BTreeSet<String>) -> HostInputSet {
    let mut first: BTreeMap<&str, &HostInputRecord> = BTreeMap::new();
    for record in records {
        first.entry(record.path.as_str()).or_insert(record);
    }
    let files: Vec<HostInput> = first
        .into_iter()
        .filter(|(_, record)| host_input_file(record, mutated))
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

/// Fill in the bytes' digest of each record's file ([`HostInputRecord::content`]),
/// for a run whose guest's processes have all exited, so that the reads can
/// fire nothing the guest observes (an inotify watch on the file's
/// directory, its access time). A record is hashed when the tool can find
/// its file again ([`HostReopen`]), the file is at most
/// [`HOST_INPUT_CONTENT_LIMIT`] bytes, and the file still has the identity and
/// change time its open found, before the read and after it. Otherwise its
/// digest stays unknown and the record is compared by its identity alone: the
/// bytes the guest read are no longer there to hash. Each file is read once,
/// however many paths or opens name it.
pub(crate) fn hash_at_end(records: &mut [HostInputRecord]) {
    let mut digests: HashMap<(PathBuf, HostFileIdentity), Option<String>> = HashMap::new();
    for record in records {
        let Some(reopen) = &record.reopen else {
            continue;
        };
        record.content = digests
            .entry((reopen.path.clone(), record.identity))
            .or_insert_with(|| file_digest(reopen, &record.identity))
            .clone();
    }
}

/// The digest of the file `reopen` finds, when it is the file whose identity
/// its open found was `identity` and is no larger than
/// [`HOST_INPUT_CONTENT_LIMIT`] ([`hash_at_end`]).
fn file_digest(reopen: &HostReopen, identity: &HostFileIdentity) -> Option<String> {
    if u64::try_from(identity.size).map_or(true, |size| size > HOST_INPUT_CONTENT_LIMIT) {
        return None;
    }
    let unchanged =
        || crate::util::raw_stat(&reopen.path).is_ok_and(|stat| reopen.unchanged(identity, &stat));
    if !unchanged() {
        return None;
    }
    let digest = read_digest(&reopen.path)?;
    unchanged().then_some(digest)
}

/// The SHA-256 of the file at `path`, read to its end but no further than
/// [`HOST_INPUT_CONTENT_LIMIT`] bytes, or `None` when it cannot be read or is
/// longer than that. Raw syscalls only, as everywhere in Detcore
/// (`crate::util::raw_syscall`).
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
    /// the image, as the kernel's own `exe` link names it, and its program
    /// interpreter, the file mapped where the auxiliary vector's `AT_BASE`
    /// says the kernel loaded it, under the name `/proc/<tid>/maps` shows. For
    /// a `#!` script the image is the interpreter the kernel finally ran, not
    /// the script. Names are made absolute in the process's root.
    ///
    /// Nothing is opened but the thread's own procfs files: a `stat` and a
    /// `readlink` of the `exe` link, the `auxv` and `maps` files, and a
    /// `stat` of the interpreter. The guest could observe a read of the image
    /// or the interpreter (an inotify watch on their directories). Their bytes
    /// are hashed when the run ends ([`hash_at_end`]). Nothing where the tool
    /// cannot see the thread's procfs directory. Raw syscalls only, as for
    /// [`read_digest`].
    pub(crate) async fn record_exec_host_inputs<G: reverie::Guest<Self>>(&self, guest: &mut G) {
        let Some(task) = self.guest_task_directory(guest) else {
            return;
        };
        let Ok(root) = crate::util::raw_read_link(&task.join("root")) else {
            return;
        };
        let mut files = Vec::new();
        // The image: the `exe` link, which `stat` follows to the file.
        let exe = task.join("exe");
        if let Ok(image) = crate::util::raw_read_link(&exe)
            && let Ok(stat) = crate::util::raw_stat(&exe)
        {
            files.push((image, stat));
        }
        // The program interpreter, if the image has one.
        if let Some(interpreter) = read_procfs(&task.join("auxv"))
            .and_then(|auxv| crate::exec_record::auxv_entry(&auxv, libc::AT_BASE))
            .filter(|base| *base != 0)
            .and_then(|base| mapped_file(&read_procfs(&task.join("maps"))?, base))
            && let Ok(stat) = crate::util::raw_stat(&interpreter)
        {
            files.push((interpreter, stat));
        }
        for (host, stat) in files {
            // The tool's view of the file (`root` is the process's root as
            // the tool sees it), made absolute in the guest's root.
            let Some(path) = host
                .strip_prefix(&root)
                .ok()
                .map(|relative| Path::new("/").join(relative))
            else {
                continue;
            };
            let regular = stat.st_mode & libc::S_IFMT == libc::S_IFREG;
            crate::tool_global::record_host_input(
                guest,
                path.to_string_lossy().into_owned(),
                HostFileIdentity::from_stat(&stat),
                regular.then(|| HostReopen::new(host.clone(), &stat)),
                true,
                false,
                !regular,
            )
            .await;
        }
    }
}

/// The bytes of the procfs file at `path`, or `None` where it cannot be read.
/// Raw syscalls only, as for [`read_digest`].
fn read_procfs(path: &Path) -> Option<Vec<u8>> {
    let file = crate::util::RawFd::open(path, libc::O_RDONLY).ok()?;
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        let count = file.read_at(&mut buffer, bytes.len() as u64).ok()?;
        if count == 0 {
            return Some(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

/// The file mapped at `address` in a process whose `/proc/<pid>/maps` is
/// `maps`, by the name the file shows there: the mapping's pathname field,
/// when it names a file (absolute, and not deleted since it was mapped).
fn mapped_file(maps: &[u8], address: u64) -> Option<PathBuf> {
    maps.split(|byte| *byte == b'\n').find_map(|line| {
        let line = std::str::from_utf8(line).ok()?;
        // address perms offset dev inode pathname: the pathname is the rest of
        // the line after the fifth field, and may hold spaces.
        let mut fields = line.splitn(6, ' ');
        let (start, end) = fields.next()?.split_once('-')?;
        let (start, end) = (
            u64::from_str_radix(start, 16).ok()?,
            u64::from_str_radix(end, 16).ok()?,
        );
        if !(start..end).contains(&address) {
            return None;
        }
        let path = fields.nth(4)?.trim_start();
        (path.starts_with('/') && !path.ends_with(" (deleted)")).then(|| PathBuf::from(path))
    })
}

/// The files a schedule replay's guest opened (`records`) that changed since
/// the recorded run's set (`recorded`, [`host_input_changed`]), in the order
/// of their first opens. Only the first open of each path is compared, and
/// only for a host input file ([`host_input_file`], with `mutated` the paths
/// the guest rebound in this run) that the recorded set holds. Made when the
/// run ends, once the files were hashed ([`hash_at_end`]).
pub(crate) fn changes_since(
    recorded: &HostInputSet,
    records: &[HostInputRecord],
    mutated: &BTreeSet<String>,
) -> Vec<HostInputChange> {
    let recorded = recorded.clone().normalized();
    let mut compared = HashSet::new();
    records
        .iter()
        .filter(|record| compared.insert(record.path.as_str()))
        .filter(|record| host_input_file(record, mutated))
        .filter_map(|record| {
            let then = recorded.file(&record.path)?;
            let now = HostInput {
                path: record.path.clone(),
                identity: record.identity,
                content: record.content.clone(),
            };
            host_input_changed(then, &now).then_some(HostInputChange {
                path: now.path,
                recorded: then.identity,
                now: now.identity,
            })
        })
        .collect()
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
            non_regular: false,
            reopen: None,
        }
    }

    fn hashed(path: &str, ino: u64, content: &str) -> HostInputRecord {
        HostInputRecord {
            content: Some(content.to_owned()),
            ..open(path, ino, 2)
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
            // A directory, such as the private `/tmp` Hermit makes for each
            // run, which has a new inode in every run.
            HostInputRecord {
                non_regular: true,
                ..open("/tmp", 15, 13)
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
        let before = input_set(&[open("/a", 1, 1), hashed("/b", 2, "aa")], &none);
        let rewritten = input_set(&[open("/a", 1, 1), hashed("/b", 3, "aa")], &none);
        let edited = input_set(&[open("/a", 1, 1), hashed("/b", 2, "bb")], &none);
        assert_eq!(before.digest, rewritten.digest);
        assert_ne!(before.digest, edited.digest);
        // A directory made again for each run leaves the digest as it was.
        let with_tmp = input_set(
            &[
                open("/a", 1, 1),
                open("/b", 2, 2),
                HostInputRecord {
                    non_regular: true,
                    ..open("/tmp", 99, 3)
                },
            ],
            &none,
        );
        assert_eq!(with_tmp.digest, first.digest);
    }

    #[test]
    fn a_replay_reports_each_changed_file_once_in_first_open_order() {
        let none = BTreeSet::new();
        let recorded = input_set(
            &[
                open("/etc/ld.so.cache", 9, 6),
                open("/lib", 10, 7),
                open("/z", 20, 8),
                HostInputRecord {
                    non_regular: true,
                    ..open("/tmp", 30, 9)
                },
            ],
            &none,
        );
        let replay = [
            open("/z", 21, 1),
            open("/etc/ld.so.cache", 11, 2),
            // A later open of the same path is not compared again.
            open("/etc/ld.so.cache", 12, 3),
            // An unchanged file, one the recording did not open, a kernel
            // object and a directory are not reported.
            open("/lib", 10, 4),
            open("/new", 13, 5),
            open("/proc/self/maps", 14, 6),
            HostInputRecord {
                non_regular: true,
                ..open("/tmp", 31, 7)
            },
        ];
        let changes = changes_since(&recorded, &replay, &none);
        assert_eq!(
            changes,
            [
                HostInputChange {
                    path: "/z".to_owned(),
                    recorded: identity(20),
                    now: identity(21),
                },
                HostInputChange {
                    path: "/etc/ld.so.cache".to_owned(),
                    recorded: identity(9),
                    now: identity(11),
                },
            ]
        );
    }

    #[test]
    fn a_replay_compares_hashed_content_before_identity() {
        let none = BTreeSet::new();
        let recorded = input_set(&[hashed("/etc/ld.so.cache", 9, "aa")], &none);
        // Rewritten with the same bytes under a new inode: not reported.
        assert!(
            changes_since(&recorded, &[hashed("/etc/ld.so.cache", 11, "aa")], &none).is_empty()
        );
        // Other bytes under the same inode: reported.
        assert_eq!(
            changes_since(&recorded, &[hashed("/etc/ld.so.cache", 9, "bb")], &none).len(),
            1
        );
        // A recorded set out of path order (a hand-edited file) is searched
        // sorted, and an open whose descriptor is a kernel object is not
        // compared.
        let mut unsorted = input_set(&[open("/a", 1, 1), open("/b", 2, 2)], &none);
        unsorted.files.reverse();
        let replay = [
            open("/a", 5, 1),
            HostInputRecord {
                kernel_object: true,
                ..open("/b", 6, 2)
            },
        ];
        let changes = changes_since(&unsorted, &replay, &none);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/a");
    }

    #[test]
    fn a_replay_does_not_report_a_file_the_guest_rebound() {
        let recorded = input_set(&[open("/work/in", 9, 6)], &BTreeSet::new());
        let mutated = BTreeSet::from(["/work".to_owned()]);
        assert!(changes_since(&recorded, &[open("/work/in", 11, 1)], &mutated).is_empty());
    }

    /// The end-of-run hash reads a file whose identity and change time are
    /// those its open found, each file once, and leaves the digest unknown
    /// for a file rewritten since the open (whose bytes then are gone), one
    /// too large, and one that is gone.
    #[test]
    fn the_end_of_run_hash_reads_only_the_file_the_open_found() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("F");
        std::fs::write(&path, "input\n").unwrap();
        let at_open = |path: &Path| {
            let stat = crate::util::raw_stat(path).unwrap();
            (
                HostFileIdentity::from_stat(&stat),
                HostReopen::new(path.to_path_buf(), &stat),
            )
        };
        let record = |(identity, reopen): (HostFileIdentity, HostReopen)| HostInputRecord {
            identity,
            reopen: Some(reopen),
            ..open("/F", 0, 1)
        };
        // `printf 'input\n' | sha256sum`
        let expected = "7d3f9b6284c6f36e77b425cac882e8fbbcc97a4727ec20790853076d0f463453";
        let mut records = [record(at_open(&path)), record(at_open(&path))];
        hash_at_end(&mut records);
        assert_eq!(records[0].content.as_deref(), Some(expected));
        assert_eq!(records[1].content.as_deref(), Some(expected));

        // Rewritten in place after the open, with the same size and with its
        // modification time set back: the change time tells.
        let mut rewritten = [record(at_open(&path))];
        let times = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "other\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(times)
            .unwrap();
        hash_at_end(&mut rewritten);
        assert_eq!(rewritten[0].content, None);

        let (mut identity, reopen) = at_open(&path);
        identity.size = HOST_INPUT_CONTENT_LIMIT as i64 + 1;
        let mut large = [record((identity, reopen))];
        hash_at_end(&mut large);
        assert_eq!(large[0].content, None);

        let mut gone = [record(at_open(&path))];
        std::fs::remove_file(&path).unwrap();
        hash_at_end(&mut gone);
        assert_eq!(gone[0].content, None);
        // A record the tool cannot find again is not hashed.
        let mut unreachable = [open("/F", 0, 1)];
        hash_at_end(&mut unreachable);
        assert_eq!(unreachable[0].content, None);
    }

    /// The program interpreter is the file mapped at the auxiliary vector's
    /// `AT_BASE`: for this test binary, which is dynamically linked, the
    /// dynamic loader, under the name the mapping shows.
    #[test]
    fn the_interpreter_is_the_file_mapped_at_the_base_the_auxv_names() {
        let auxv = read_procfs(Path::new("/proc/self/auxv")).unwrap();
        let base = crate::exec_record::auxv_entry(&auxv, libc::AT_BASE).unwrap();
        assert_ne!(base, 0, "the test binary is dynamically linked");
        let maps = read_procfs(Path::new("/proc/self/maps")).unwrap();
        let interpreter = mapped_file(&maps, base).expect("a file is mapped at AT_BASE");
        assert!(interpreter.is_absolute(), "{}", interpreter.display());
        // The dynamic loader: `ld-linux-<arch>.so.<n>` or `ld-musl-<arch>.so.1`.
        assert!(
            interpreter
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("ld-")),
            "{} is not a dynamic loader",
            interpreter.display()
        );
        assert!(crate::util::raw_stat(&interpreter).is_ok());

        let maps = b"00400000-00401000 r-xp 00000000 08:01 12 /usr/bin/a b\n\
            7f0000000000-7f0000001000 r--p 00000000 08:01 13 /lib/ld (deleted)\n\
            7f0000002000-7f0000003000 r--p 00000000 00:00 0 \n";
        assert_eq!(
            mapped_file(maps, 0x400800),
            Some(PathBuf::from("/usr/bin/a b"))
        );
        assert_eq!(mapped_file(maps, 0x7f0000000000), None);
        assert_eq!(mapped_file(maps, 0x7f0000002000), None);
        assert_eq!(mapped_file(maps, 0x500000), None);
    }
}
