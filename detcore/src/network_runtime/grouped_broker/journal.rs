//! Durable exact write history. Successful serialization is not native custody.
use std::ffi::CString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::Failure;
use super::Intent;
use super::owner::FileIdentity;
use super::owner::stat;
use super::require;

pub(super) fn canonical(value: &Value) -> io::Result<Vec<u8>> {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut entries: Vec<_> = m.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            _ => v.clone(),
        }
    }
    let bytes = serde_json::to_vec(&sorted(value))?;
    require(bytes.is_ascii(), "grouped canonical protocol is ASCII")?;
    Ok(bytes)
}

#[derive(Debug)]
pub(super) struct Store {
    directory: OwnedFd,
    pub file: Option<OwnedFd>,
    name: CString,
    directory_identity: Option<FileIdentity>,
    identity: Option<FileIdentity>,
    pub content: Vec<u8>,
    pub writes: Vec<(usize, isize, Option<i32>)>,
    pub refused: Option<Failure>,
    pub file_syncs: usize,
    pub directory_synced: bool,
    frozen: bool,
}
impl Store {
    /// No I/O; the caller installs this in retained state before initialize.
    pub fn retain(directory: OwnedFd, intent: &Intent) -> Self {
        Self {
            directory,
            file: None,
            name: CString::new(format!("grouped-{}.jsonl", intent.nonce)).unwrap(),
            directory_identity: None,
            identity: None,
            content: Vec::new(),
            writes: Vec::new(),
            refused: None,
            file_syncs: 0,
            directory_synced: false,
            frozen: false,
        }
    }
    pub fn initialize(&mut self, header: Value) -> io::Result<()> {
        let result = self.initialize_inner(header);
        self.remember(&result);
        result
    }
    fn initialize_inner(&mut self, header: Value) -> io::Result<()> {
        require(
            self.refused.is_none() && self.file.is_none(),
            "journal cannot initialize twice",
        )?;
        let directory = stat(self.directory.as_raw_fd())?;
        require(
            directory.mode & libc::S_IFMT == libc::S_IFDIR
                && directory.mode & 0o7777 == 0o700
                && directory.uid == unsafe { libc::getuid() },
            "journal requires an owned 0700 directory",
        )?;
        self.directory_identity = Some(directory);
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.file = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        self.identity = Some(stat(fd)?);
        self.append_inner(json!({"kind":"intent", "value":header}))?;
        if unsafe { libc::fsync(self.directory.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory_synced = true;
        self.verify_inner()
    }
    fn remember<T>(&mut self, result: &io::Result<T>) {
        if let Err(error) = result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
    }
    fn verify_inner(&self) -> io::Result<()> {
        require(self.refused.is_none(), "journal refusal is sticky")?;
        let fd = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("journal file absent"))?
            .as_raw_fd();
        let directory = stat(self.directory.as_raw_fd())?;
        let old = self
            .directory_identity
            .as_ref()
            .ok_or_else(|| io::Error::other("journal directory identity absent"))?;
        require(
            directory.same_owner(old),
            "journal directory identity changed",
        )?;
        let held = stat(fd)?;
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::other("journal identity absent"))?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let named = FileIdentity::from(unsafe { named.assume_init() });
        require(
            held.mode & libc::S_IFMT == libc::S_IFREG
                && held.mode & 0o7777 == 0o600
                && held.uid == unsafe { libc::getuid() }
                && held.links == 1
                && held.same_object(identity)
                && named.same_owner(&held)
                && named.size == held.size
                && held.size >= 0
                && held.size as usize == self.content.len(),
            "journal held/name identity or extent changed",
        )?;
        let mut bytes = vec![0; self.content.len() + 1];
        let count = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        bytes.truncate(count as usize);
        require(bytes == self.content, "journal retained bytes changed")
    }
    pub fn verify(&mut self) -> io::Result<()> {
        let result = self.verify_inner();
        self.remember(&result);
        result
    }
    fn append_inner(&mut self, value: Value) -> io::Result<()> {
        require(self.refused.is_none(), "journal refusal is sticky")?;
        require(
            !self.frozen,
            "terminal-export journal is permanently frozen",
        )?;
        if !self.content.is_empty() {
            self.verify_inner()?;
        }
        let mut row = canonical(&value)?;
        row.push(b'\n');
        require(
            row.len() <= 4096 && self.content.len() + row.len() <= 1_048_576,
            "original journal row/aggregate bound exceeded",
        )?;
        let fd = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("journal file absent"))?
            .as_raw_fd();
        let mut done = 0;
        while done < row.len() {
            let offset = self.content.len() + done;
            let count = unsafe {
                libc::pwrite(
                    fd,
                    row[done..].as_ptr().cast(),
                    row.len() - done,
                    offset as libc::off_t,
                )
            };
            let error = (count < 0).then(io::Error::last_os_error);
            self.writes.push((
                offset,
                count,
                error.as_ref().and_then(io::Error::raw_os_error),
            ));
            if let Some(error) = error {
                return Err(error);
            }
            require(
                count > 0 && count as usize <= row.len() - done,
                "journal write made no progress",
            )?;
            done += count as usize;
        }
        if unsafe { libc::fsync(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.file_syncs += 1;
        self.content.extend_from_slice(&row);
        self.verify_inner()
    }
    pub fn append(&mut self, value: Value) -> io::Result<[u8; 32]> {
        let result = self
            .append_inner(value)
            .map(|()| Sha256::digest(&self.content).into());
        self.remember(&result);
        result
    }
    /// The added namespace observation shares only the original executable
    /// query's durability barrier. Neither row authorizes an effect separately.
    /// Original authentication and ACK-intent appends keep their own barriers.
    pub fn append_creator_queries(
        &mut self,
        namespace: Value,
        executable: Value,
    ) -> io::Result<[u8; 32]> {
        let result = (|| {
            require(self.refused.is_none(), "journal refusal is sticky")?;
            require(
                !self.frozen,
                "terminal-export journal is permanently frozen",
            )?;
            if !self.content.is_empty() {
                self.verify_inner()?;
            }
            require(
                namespace["kind"] == "actual-source-namespace-query"
                    && executable["kind"] == "actual-executable-query",
                "creator query pair changed fixed labels or order",
            )?;
            let mut rows = [canonical(&namespace)?, canonical(&executable)?];
            let mut extent = self.content.len();
            for row in &mut rows {
                row.push(b'\n');
                require(
                    row.len() <= 4096 && extent + row.len() <= 1_048_576,
                    "original journal row/aggregate bound exceeded",
                )?;
                extent += row.len();
            }
            let fd = self
                .file
                .as_ref()
                .ok_or_else(|| io::Error::other("journal file absent"))?
                .as_raw_fd();
            let mut written = 0;
            for row in &rows {
                let mut done = 0;
                while done < row.len() {
                    let offset = self.content.len() + written + done;
                    let count = unsafe {
                        libc::pwrite(
                            fd,
                            row[done..].as_ptr().cast(),
                            row.len() - done,
                            offset as libc::off_t,
                        )
                    };
                    let error = (count < 0).then(io::Error::last_os_error);
                    self.writes.push((
                        offset,
                        count,
                        error.as_ref().and_then(io::Error::raw_os_error),
                    ));
                    if let Some(error) = error {
                        return Err(error);
                    }
                    require(
                        count > 0 && count as usize <= row.len() - done,
                        "journal write made no progress",
                    )?;
                    done += count as usize;
                }
                written += row.len();
            }
            // A partial write or failed sync leaves the original committed
            // prefix intact in memory and latches refusal; no healthy prefix
            // or ACK can be obtained from the possibly written tail.
            if unsafe { libc::fsync(fd) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.file_syncs += 1;
            for row in &rows {
                self.content.extend_from_slice(row);
            }
            self.verify_inner()?;
            Ok(Sha256::digest(&self.content).into())
        })();
        self.remember(&result);
        result
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnerSnapshot {
    pub incarnation: u64,
    pub phase: u32,
    pub verified_sites: u32,
    pub attempted_sites: u32,
    pub event_id: u32,
    pub write_unknown: u32,
    pub pending_role: u32,
    pub pending_remove: u32,
    pub pending_bytes: u64,
    pub group: String,
    pub event: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Write {
    pub role: u32,
    pub remove: u32,
    pub submitted: u64,
    pub raw: i64,
    pub error: u32,
    pub started: u32,
    pub completed: u32,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Frame {
    pub schema: String,
    pub nonce: String,
    pub sequence: u64,
    pub owner: OwnerSnapshot,
    pub write: Write,
    pub line: String,
}
#[derive(Debug)]
pub(super) struct Journal {
    pub store: Store,
    intent: Intent,
    pub next: u64,
    pub pending: Option<Frame>,
    pub pairs: Vec<Value>,
    pub failed_pair: Option<Value>,
    pub create_mask: u32,
    delete_mask: u32,
    pub handoff_issued: bool,
    pub recovery_issued: bool,
    pub release_origin: Option<std::time::Instant>,
    pub refused: Option<Failure>,
    terminal_export_attempted: bool,
}
#[derive(Debug)]
pub(super) struct Acknowledgement {
    pub bytes: Vec<u8>,
    /// A durable failed outcome can be ACKed, but can never resume creation.
    pub native_failed: bool,
}
impl Journal {
    pub fn retain(directory: OwnedFd, intent: Intent) -> Self {
        Self {
            store: Store::retain(directory, &intent),
            intent,
            next: 1,
            pending: None,
            pairs: Vec::new(),
            failed_pair: None,
            create_mask: 0,
            delete_mask: 0,
            handoff_issued: false,
            recovery_issued: false,
            release_origin: None,
            refused: None,
            terminal_export_attempted: false,
        }
    }
    pub fn initialize(&mut self, header: Value) -> io::Result<()> {
        self.store.initialize(header)
    }
    pub fn receive(
        &mut self,
        bytes: &[u8],
        now: std::time::Instant,
    ) -> io::Result<Acknowledgement> {
        let result = self.receive_inner(bytes, now);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn receive_inner(
        &mut self,
        bytes: &[u8],
        now: std::time::Instant,
    ) -> io::Result<Acknowledgement> {
        require(
            self.refused.is_none() && self.store.directory_synced,
            "journal not admitted or refused",
        )?;
        require(
            !self.terminal_export_attempted,
            "callback after terminal export attempt",
        )?;
        require(bytes.len() <= 1536, "journal frame exceeds original bound")?;
        // Deserialize into a strict struct first: serde rejects duplicate keys,
        // unknown fields, floats, booleans in integers and narrowing overflow.
        let frame: Frame = serde_json::from_slice(bytes)?;
        require(
            canonical(&serde_json::to_value(&frame)?)? == bytes,
            "journal frame is not exact canonical bytes",
        )?;
        let o = &frame.owner;
        let w = &frame.write;
        require(
            frame.schema == "hermit-grouped-journal-v1"
                && frame.nonce == self.intent.nonce
                && frame.sequence == self.next
                && o.incarnation == self.intent.incarnation
                && o.group == self.intent.group()
                && o.event == self.intent.event(),
            "journal run/sequence/owner changed",
        )?;
        let line = self.intent.command(w.role, w.remove)?;
        require(
            frame.line == super::hex(line.as_bytes())
                && w.submitted == line.len() as u64
                && o.pending_role == w.role
                && o.pending_remove == w.remove
                && o.pending_bytes == w.submitted
                && o.verified_sites <= 0x1ffff
                && o.attempted_sites <= 0x1ffff
                && o.write_unknown <= 1
                && o.phase == if w.remove == 1 { 7 } else { 2 },
            "journal line/pending state differs",
        )?;
        if w.remove == 1 {
            let origin = self
                .release_origin
                .ok_or_else(|| io::Error::other("deletion lacks original release origin"))?;
            require(
                now >= origin && now.duration_since(origin) < std::time::Duration::from_secs(1),
                "original release deadline elapsed",
            )?;
        } else {
            require(
                self.release_origin.is_none() && o.write_unknown == 0,
                "creation after refusal or release",
            )?;
        }
        let mut native_failed = false;
        match (w.started, w.completed) {
            (0, 0) => {
                let seen = if w.remove == 1 {
                    self.delete_mask
                } else {
                    self.create_mask
                };
                require(
                    self.pending.is_none()
                        && w.raw == 0
                        && w.error == 0
                        && seen & (1 << (w.role - 1)) == 0,
                    "journal repeated or invented intent",
                )?;
                if w.remove == 0 {
                    require(
                        w.role == self.pairs.len() as u32 + 1
                            && o.verified_sites == (1 << (w.role - 1)) - 1
                            && o.attempted_sites == (1 << w.role) - 1
                            && o.event_id == 0,
                        "journal creation prefix changed",
                    )?;
                }
                self.store
                    .append(json!({"kind":"before-write", "value":frame}))?;
                self.pending = Some(frame.clone());
            }
            (1, 1) => {
                let pending = self
                    .pending
                    .as_ref()
                    .ok_or_else(|| io::Error::other("outcome without durable intent"))?;
                require(
                    o == &pending.owner
                        && frame.line == pending.line
                        && w.role == pending.write.role
                        && w.remove == pending.write.remove
                        && w.submitted == pending.write.submitted,
                    "outcome changed exact pending write",
                )?;
                require(
                    (w.raw < 0 && w.error > 0 && w.error <= 4095) || (w.raw >= 0 && w.error == 0),
                    "native write errno domain",
                )?;
                let pair = json!({"intent_owner":pending.owner,"outcome_owner":o,
                    "intent":pending.write,"outcome":w,"line":frame.line});
                self.store
                    .append(json!({"kind":"after-write", "value":frame}))?;
                self.pending = None;
                if w.raw != w.submitted as i64 {
                    if w.remove == 0 {
                        self.failed_pair = Some(pair);
                    }
                    native_failed = true;
                    self.refused = Some(Failure::capture(&io::Error::other(
                        "native short/error write retained UNKNOWN",
                    )));
                } else if w.remove == 0 {
                    self.create_mask |= 1 << (w.role - 1);
                    self.pairs.push(pair);
                } else {
                    self.delete_mask |= 1 << (w.role - 1);
                }
            }
            _ => return Err(io::Error::other("journal callback started/completed shape")),
        }
        let bytes = canonical(
            &json!({"schema":"hermit-grouped-journal-ack-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":frame.sequence}),
        )?;
        // Match the recovered failure behavior: a failed native outcome's ACK
        // retains its sequence and does not authorize another transition.
        if !native_failed {
            self.next += 1;
        }
        Ok(Acknowledgement {
            bytes,
            native_failed,
        })
    }
    pub fn complete(&mut self) -> io::Result<()> {
        require(
            self.refused.is_none()
                && self.pending.is_none()
                && self.release_origin.is_none()
                && !self.handoff_issued
                && !self.recovery_issued
                && self.create_mask == 0x1ffff
                && self.pairs.len() == 17,
            "journal lacks exact17 durable creation pairs",
        )?;
        self.store.verify()
    }

    /// Complete history is checked by the existing gate before any once-only
    /// export intent. Neither this projection nor an archive grants authority.
    pub fn canonical_created_pairs(&mut self) -> io::Result<Vec<u8>> {
        self.complete()?;
        require(
            self.next == 35 && self.failed_pair.is_none() && self.delete_mask == 0,
            "created history sequence or failed/deletion state differs",
        )?;
        canonical(&Value::Array(self.pairs.clone()))
    }

    /// Called only by the successful Keeper path after all native Holder/query
    /// checks. Attempt stays occupied after a failed append or final verify.
    pub fn begin_terminal_export(&mut self, record: Value) -> io::Result<()> {
        let result = (|| {
            require(
                !self.terminal_export_attempted,
                "terminal export cannot repeat",
            )?;
            self.complete()?;
            self.terminal_export_attempted = true;
            self.store.append(
                json!({"kind":"terminal-export-intent","value":export_commitment(&record)?}),
            )?;
            self.store.frozen = true;
            self.store.verify()
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }

    pub fn frozen_export(&mut self) -> io::Result<FrozenJournal<'_>> {
        require(
            self.terminal_export_attempted && self.store.frozen,
            "journal lacks its original completed export freeze",
        )?;
        let pairs = self.canonical_created_pairs()?;
        Ok(FrozenJournal {
            original: self,
            pairs,
        })
    }

    /// Installed LeafPlan owns the already joined original Guardian journal
    /// before invoking this operation. Occupy handoff before durable append;
    /// failure cannot create a retryable source token.
    pub fn begin_leaf_handoff(&mut self, request: Value) -> io::Result<()> {
        let result = (|| {
            self.complete()?;
            require(
                !self.terminal_export_attempted && !self.store.frozen,
                "leaf handoff requires the original mutable Guardian ledger",
            )?;
            self.handoff_issued = true;
            self.store
                .append(json!({"kind":"source-leaf-handoff-intent","value":request}))?;
            Ok(())
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
}

/// Original file descriptions are borrowed, never reopened or reconstructed.
#[derive(Debug)]
pub(super) struct FrozenJournal<'a> {
    original: &'a Journal,
    pairs: Vec<u8>,
}
impl FrozenJournal<'_> {
    pub fn rights(&self) -> [BorrowedFd<'_>; 2] {
        [
            self.original.store.directory.as_fd(),
            self.original.store.file.as_ref().unwrap().as_fd(),
        ]
    }
    pub fn bytes(&self) -> &[u8] {
        &self.original.store.content
    }
    pub fn pairs(&self) -> &[u8] {
        &self.pairs
    }
}

/// Received original descriptions, not a rehydrated Journal. Retain before any
/// fallible inspection. The serial join separately requires actual authenticated
/// producer custody and its natural exit; these bytes alone grant nothing.
#[derive(Debug)]
pub(super) struct ArchivedKeeperJournal {
    rights: Vec<OwnedFd>,
    directory_identity: Option<FileIdentity>,
    file_identity: Option<FileIdentity>,
    name: CString,
    bytes: Vec<u8>,
    checked: bool,
    handoff_issued: bool,
    refused: Option<Failure>,
}
impl ArchivedKeeperJournal {
    pub fn retain(rights: Vec<OwnedFd>, intent: &Intent) -> Self {
        Self {
            rights,
            directory_identity: None,
            file_identity: None,
            name: CString::new(format!("grouped-{}.jsonl", intent.nonce)).unwrap(),
            bytes: Vec::new(),
            checked: false,
            handoff_issued: false,
            refused: None,
        }
    }
    pub fn append_chunk(&mut self, offset: usize, bytes: &[u8]) -> io::Result<()> {
        require(
            !self.checked
                && self.refused.is_none()
                && self.bytes.len() == offset
                && bytes.len() <= 16_384
                && !bytes.is_empty()
                && self.bytes.len() + bytes.len() <= 1_048_576,
            "archive chunk reordered, repeated or beyond original1MiB",
        )?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            require(
                self.rights.len() == 2
                    && self.directory_identity.is_none()
                    && self.file_identity.is_none(),
                "archive original rights incomplete or reused",
            )?;
            self.directory_identity = Some(stat(self.rights[0].as_raw_fd())?);
            self.file_identity = Some(stat(self.rights[1].as_raw_fd())?);
            self.check_identity()
        })();
        self.remember(result)
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn check_identity(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(self.rights.len() == 2, "archive lost original descriptions")?;
        let directory = stat(self.rights[0].as_raw_fd())?;
        let file = stat(self.rights[1].as_raw_fd())?;
        require(
            directory.mode & libc::S_IFMT == libc::S_IFDIR
                && directory.mode & 0o7777 == 0o700
                && directory.uid == unsafe { libc::getuid() }
                && directory.same_owner(
                    self.directory_identity
                        .as_ref()
                        .ok_or_else(|| io::Error::other("archive directory identity absent"))?,
                )
                && file.mode & libc::S_IFMT == libc::S_IFREG
                && file.mode & 0o7777 == 0o600
                && file.uid == unsafe { libc::getuid() }
                && file.links == 1
                && file.same_owner(
                    self.file_identity
                        .as_ref()
                        .ok_or_else(|| io::Error::other("archive file identity absent"))?,
                ),
            "archive held original identity or permissions changed",
        )?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.rights[0].as_raw_fd(),
                self.name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let named = FileIdentity::from(unsafe { named.assume_init() });
        require(
            named.same_owner(&file) && named.links == 1 && named.size == file.size,
            "archive descriptor-relative original name changed",
        )
    }
    pub fn verify(&mut self, expected_header: &Value, expected_export: &Value) -> io::Result<()> {
        let result = (|| {
            self.check_identity()?;
            require(
                !self.handoff_issued && self.bytes.len() <= 1_048_576,
                "archive already consumed or beyond original extent",
            )?;
            let held = stat(self.rights[1].as_raw_fd())?;
            require(
                held.size >= 0 && held.size as usize == self.bytes.len(),
                "archive held extent differs",
            )?;
            let mut read = vec![0u8; self.bytes.len() + 1];
            let count = unsafe {
                libc::pread(
                    self.rights[1].as_raw_fd(),
                    read.as_mut_ptr().cast(),
                    read.len(),
                    0,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            read.truncate(count as usize);
            require(read == self.bytes, "archive actual retained bytes differ")?;
            validate_created_archive(&read, expected_header, expected_export)?;
            self.check_identity()?;
            self.checked = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn consume_handoff(&mut self) -> io::Result<()> {
        require(
            self.checked && !self.handoff_issued && self.refused.is_none(),
            "archive handoff is incomplete or already consumed",
        )?;
        self.handoff_issued = true;
        Ok(())
    }
    /// Check the same already consumed immutable archive for a later custody
    /// transfer. The existing verify() cannot be reused and no consumed bit is
    /// cleared. This lends descriptors/history only, not a new source token.
    pub(super) fn verify_consumed(
        &mut self,
        expected_header: &Value,
        expected_export: &Value,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                self.checked && self.handoff_issued && self.refused.is_none(),
                "archive lacks its original checked consumed handoff",
            )?;
            self.check_identity()?;
            require(
                self.bytes.len() <= 1_048_576,
                "consumed archive exceeds original bound",
            )?;
            let held = stat(self.rights[1].as_raw_fd())?;
            require(
                held.size >= 0 && held.size as usize == self.bytes.len(),
                "consumed archive held extent differs",
            )?;
            let mut read = vec![0u8; self.bytes.len() + 1];
            let count = unsafe {
                libc::pread(
                    self.rights[1].as_raw_fd(),
                    read.as_mut_ptr().cast(),
                    read.len(),
                    0,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            read.truncate(count as usize);
            require(read == self.bytes, "consumed archive actual bytes changed")?;
            validate_created_archive(&read, expected_header, expected_export)?;
            self.check_identity()
        })();
        self.remember(result)
    }
    pub fn rights(&self) -> &[OwnedFd] {
        &self.rights
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
fn export_commitment(record: &Value) -> io::Result<Value> {
    // The full17 transcript uses bounded transport chunks; never enlarge the
    // original4096-byte durable row limit to fit it into one intent record.
    Ok(json!({"schema":"hermit-grouped-terminal-export-intent-v1",
        "holder_sha256":super::hex(&Sha256::digest(canonical(record)?)),
        "pairs_sha256":super::hex(&Sha256::digest(canonical(&record["pairs"])?))}))
}
fn validate_created_archive(bytes: &[u8], header: &Value, export: &Value) -> io::Result<()> {
    require(
        !bytes.is_empty() && bytes.len() <= 1_048_576 && bytes.ends_with(b"\n"),
        "archive full framing differs",
    )?;
    let pairs = export["pairs"]
        .as_array()
        .ok_or_else(|| io::Error::other("archive expected pairs absent"))?;
    require(
        pairs.len() == 17 && export["next"] == 35 && export["create_mask"] == 0x1ffffu32,
        "archive expected exact17 history differs",
    )?;
    let mut rows = Vec::new();
    for line in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
        require(
            !line.is_empty() && line.len() + 1 <= 4096,
            "archive original row bound exceeded",
        )?;
        let row: Value = serde_json::from_slice(line)?;
        require(
            canonical(&row)? == line,
            "archive row is not exact canonical bytes",
        )?;
        rows.push(row);
    }
    require(
        rows.first() == Some(&json!({"kind":"intent","value":header}))
            && rows.last()
                == Some(
                    &json!({"kind":"terminal-export-intent","value":export_commitment(export)?}),
                ),
        "archive original header or terminal freeze differs",
    )?;
    // Compare actual persisted callback rows with the independently complete
    // Guardian history. Never call receive() to create a replacement history.
    let mut sequence = 0usize;
    for row in &rows {
        let kind = row["kind"]
            .as_str()
            .ok_or_else(|| io::Error::other("archive row kind absent"))?;
        if kind != "before-write" && kind != "after-write" {
            continue;
        }
        require(
            sequence < 34,
            "archive contains extra or duplicate callback",
        )?;
        let pair = &pairs[sequence / 2];
        let before = sequence % 2 == 0;
        require(
            kind == if before {
                "before-write"
            } else {
                "after-write"
            },
            "archive callback order differs",
        )?;
        let frame = json!({"schema":"hermit-grouped-journal-v1","nonce":export["nonce"],
            "sequence":sequence+1,"owner":pair[if before {"intent_owner"} else {"outcome_owner"}],
            "write":pair[if before {"intent"} else {"outcome"}],"line":pair["line"]});
        require(
            row == &json!({"kind":kind,"value":frame}),
            "archive callback differs from independent history",
        )?;
        sequence += 1;
    }
    require(sequence == 34, "archive lacks exact34 original callbacks")
}

/// A descriptive snapshot of an actual healthy source Store. Refusing a source
/// operation does not clear Journal::refused; a failed Store cannot export this.
#[derive(Debug)]
pub(super) struct SourceHistory {
    pub(super) frames: Vec<Frame>,
    pub(super) bytes: Vec<u8>,
    pub(super) digest: [u8; 32],
}
impl SourceHistory {
    fn read(intent: &Intent, bytes: Vec<u8>) -> io::Result<Self> {
        require(
            bytes.len() <= 1_048_576 && bytes.last() == Some(&b'\n'),
            "source history original extent or final newline differs",
        )?;
        let mut frames = Vec::new();
        for row in bytes.split_inclusive(|b| *b == b'\n') {
            require(
                row.len() <= 4096 && row.last() == Some(&b'\n'),
                "source history original row bound differs",
            )?;
            let value: Value = serde_json::from_slice(&row[..row.len() - 1])?;
            require(
                canonical(&value)? == row[..row.len() - 1],
                "source history row is not canonical",
            )?;
            match value["kind"].as_str() {
                Some("before-write") | Some("after-write") => {
                    let frame: Frame = serde_json::from_value(value["value"].clone())?;
                    require(
                        value["kind"]
                            == if frame.write.started == 0 {
                                "before-write"
                            } else {
                                "after-write"
                            },
                        "source history row kind changed",
                    )?;
                    require(
                        frames.len() < 34,
                        "source history exceeds original34 callbacks",
                    )?;
                    frames.push(frame);
                }
                _ => {}
            }
        }
        validate_source_frames(intent, &frames)?;
        Ok(Self {
            digest: Sha256::digest(&bytes).into(),
            bytes,
            frames,
        })
    }
    pub(super) fn commitment(&self) -> Value {
        json!({"sha256":super::hex(&self.digest),"bytes":self.bytes.len(),"frames":self.frames.len()})
    }
}
fn validate_source_frames(intent: &Intent, frames: &[Frame]) -> io::Result<()> {
    require(frames.len() <= 34, "source prefix exceeds original17 pairs")?;
    for (index, frame) in frames.iter().enumerate() {
        let role = (index / 2 + 1) as u32;
        let outcome = index % 2 == 1;
        let line = intent.command(role, 0)?;
        let w = &frame.write;
        let o = &frame.owner;
        require(
            frame.schema == "hermit-grouped-journal-v1"
                && frame.nonce == intent.nonce
                && frame.sequence == index as u64 + 1
                && o.incarnation == intent.incarnation
                && o.group == intent.group()
                && o.event == intent.event()
                && frame.line == super::hex(line.as_bytes())
                && w.role == role
                && w.remove == 0
                && w.submitted == line.len() as u64
                && o.phase == 2
                && o.verified_sites == (1 << (role - 1)) - 1
                && o.attempted_sites == (1 << role) - 1
                && o.event_id == 0
                && o.write_unknown == 0
                && o.pending_role == role
                && o.pending_remove == 0
                && o.pending_bytes == w.submitted,
            "source history exact creation prefix changed",
        )?;
        if !outcome {
            require(
                w.started == 0 && w.completed == 0 && w.raw == 0 && w.error == 0,
                "source history intent invented a native result",
            )?;
        } else {
            require(
                w.started == 1
                    && w.completed == 1
                    && ((w.raw < 0 && (1..=4095).contains(&w.error))
                        || (w.raw >= 0 && w.error == 0))
                    && o == &frames[index - 1].owner,
                "source history actual outcome or owner differs",
            )?;
            require(
                w.raw == w.submitted as i64 || index + 1 == frames.len(),
                "source history continued after failed native write",
            )?;
        }
    }
    Ok(())
}
impl Journal {
    pub(super) fn source_history(&mut self) -> io::Result<SourceHistory> {
        require(
            self.delete_mask == 0
                && !self.handoff_issued
                && !self.recovery_issued
                && !self.terminal_export_attempted
                && self.release_origin.is_none(),
            "source history crossed a different ownership transition",
        )?;
        self.store.verify()?;
        SourceHistory::read(&self.intent, self.store.content.clone())
    }
    /// Read the original successful Guardian Store after its actual one-use
    /// leaf transition. This is descriptive retained history, not a fresh
    /// source token. The old source_history/complete refusal remains intact.
    pub(super) fn completed_leaf_history(&mut self) -> io::Result<SourceHistory> {
        require(
            self.handoff_issued
                && !self.recovery_issued
                && !self.terminal_export_attempted
                && !self.store.frozen
                && self.refused.is_none()
                && self.release_origin.is_none()
                && self.pending.is_none()
                && self.failed_pair.is_none()
                && self.create_mask == 0x1ffff
                && self.delete_mask == 0
                && self.next == 35
                && self.pairs.len() == 17,
            "leaf history lacks actual complete handed-off Guardian custody",
        )?;
        self.store.verify()?;
        let history = SourceHistory::read(&self.intent, self.store.content.clone())?;
        require(
            history.frames.len() == 34,
            "leaf Store lost original34 callbacks",
        )?;
        for (index, pair) in self.pairs.iter().enumerate() {
            let before = &history.frames[index * 2];
            let after = &history.frames[index * 2 + 1];
            require(
                after.write.raw == after.write.submitted as i64
                    && *pair
                        == json!({"intent_owner":before.owner,"outcome_owner":after.owner,
                    "intent":before.write,"outcome":after.write,"line":before.line}),
                "leaf Store differs from the original actual completed pairs",
            )?;
        }
        Ok(history)
    }
    pub(super) fn source_store_rights(&self) -> io::Result<[BorrowedFd<'_>; 2]> {
        require(
            self.store.refused.is_none() && self.store.directory_synced,
            "source Store not healthy for retained descriptor transfer",
        )?;
        Ok([
            self.store.directory.as_fd(),
            self.store
                .file
                .as_ref()
                .ok_or_else(|| io::Error::other("source Store file absent"))?
                .as_fd(),
        ])
    }
    pub(super) fn refuse_creation(&mut self, cause: &io::Error) {
        self.refused.get_or_insert_with(|| Failure::capture(cause));
    }
}

/// Live peer's original Store descriptions. These may describe failed source
/// protocol custody, unlike the success-only archived Keeper journal. No value
/// here mints native source or cleanup authority.
#[derive(Debug)]
pub(super) struct SourceLedgerReader {
    rights: Vec<OwnedFd>,
    directory: Option<FileIdentity>,
    file: Option<FileIdentity>,
    intent: Intent,
    refused: Option<Failure>,
    acknowledged_prefix_attempt: Option<AcknowledgedPrefixAttempt>,
}
/// Exact readback of a previously acknowledged prefix. The trailing bytes have
/// no journal-health, native-outcome, write-eligibility or completion authority.
#[derive(Debug)]
pub(super) struct AcknowledgedSourceReadback {
    pub(super) history: SourceHistory,
    pub(super) unacknowledged_tail: Vec<u8>,
}
#[derive(Debug)]
pub(super) struct AcknowledgedPrefixAttempt {
    pub(super) prefix_bytes: usize,
    pub(super) observed_size: Option<usize>,
    pub(super) raw: Option<isize>,
    pub(super) errno: Option<i32>,
    pub(super) bytes: Vec<u8>,
}
impl SourceLedgerReader {
    /// Borrow the same initialized live Store descriptions for an additional
    /// retained cleanup owner. No archive/completion flag is issued or reset.
    pub(super) fn retained_rights(&self) -> io::Result<[BorrowedFd<'_>; 2]> {
        self.check()?;
        Ok([self.rights[0].as_fd(), self.rights[1].as_fd()])
    }
    pub(super) fn retain(rights: Vec<OwnedFd>, intent: Intent) -> Self {
        Self {
            rights,
            directory: None,
            file: None,
            intent,
            refused: None,
            acknowledged_prefix_attempt: None,
        }
    }
    pub(super) fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            require(
                self.refused.is_none()
                    && self.directory.is_none()
                    && self.file.is_none()
                    && self.rights.len() == 2,
                "live source Store descriptor population changed",
            )?;
            self.directory = Some(stat(self.rights[0].as_raw_fd())?);
            self.file = Some(stat(self.rights[1].as_raw_fd())?);
            self.check()
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn check(&self) -> io::Result<()> {
        require(
            self.refused.is_none(),
            "live source Store reader refusal is sticky",
        )?;
        let directory = stat(self.rights[0].as_raw_fd())?;
        let file = stat(self.rights[1].as_raw_fd())?;
        require(
            directory.same_owner(
                self.directory
                    .as_ref()
                    .ok_or_else(|| io::Error::other("Store directory absent"))?,
            ) && directory.mode & libc::S_IFMT == libc::S_IFDIR
                && directory.mode & 0o7777 == 0o700
                && directory.uid == unsafe { libc::getuid() }
                && file.same_object(
                    self.file
                        .as_ref()
                        .ok_or_else(|| io::Error::other("Store file absent"))?,
                )
                && file.mode & libc::S_IFMT == libc::S_IFREG
                && file.mode & 0o7777 == 0o600
                && file.uid == unsafe { libc::getuid() }
                && file.links == 1
                && file.size >= 0
                && file.size <= 1_048_576,
            "live source Store original identity or bounds changed",
        )?;
        let name = CString::new(format!("grouped-{}.jsonl", self.intent.nonce)).unwrap();
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.rights[0].as_raw_fd(),
                name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            FileIdentity::from(unsafe { named.assume_init() }).same_owner(&file),
            "live source Store name no longer identifies held file",
        )
    }
    pub(super) fn held_rights(&self) -> &[OwnedFd] {
        &self.rights
    }
    pub(super) fn acknowledged_prefix_attempt(&self) -> Option<&AcknowledgedPrefixAttempt> {
        self.acknowledged_prefix_attempt.as_ref()
    }
    /// The caller has already established actual terminal custody of both
    /// original writers. This observes only the exact prior acknowledged byte
    /// prefix; unlike read(), it deliberately does not parse the trailing bytes.
    /// It is descriptive and cannot issue a native terminal or recovery token.
    pub(super) fn read_acknowledged_prefix(
        &mut self,
        acknowledged: &SourceHistory,
    ) -> io::Result<AcknowledgedSourceReadback> {
        let result = (|| {
            self.check()?;
            require(
                self.acknowledged_prefix_attempt.is_none(),
                "acknowledged prefix read is one-use",
            )?;
            self.acknowledged_prefix_attempt = Some(AcknowledgedPrefixAttempt {
                prefix_bytes: acknowledged.bytes.len(),
                observed_size: None,
                raw: None,
                errno: None,
                bytes: Vec::new(),
            });
            require(
                acknowledged.bytes.len() <= 1_048_576
                    && <[u8; 32]>::from(Sha256::digest(&acknowledged.bytes)) == acknowledged.digest,
                "retained acknowledged prefix digest or extent differs",
            )?;
            let size = usize::try_from(stat(self.rights[1].as_raw_fd())?.size)
                .map_err(io::Error::other)?;
            require(
                size <= 1_048_576 && size >= acknowledged.bytes.len(),
                "held source lost acknowledged prefix or exceeded original extent",
            )?;
            let attempt = self.acknowledged_prefix_attempt.as_mut().unwrap();
            attempt.observed_size = Some(size);
            attempt.bytes.resize(size + 1, 0);
            let raw = unsafe {
                libc::pread(
                    self.rights[1].as_raw_fd(),
                    attempt.bytes.as_mut_ptr().cast(),
                    attempt.bytes.len(),
                    0,
                )
            };
            let error = (raw == -1).then(io::Error::last_os_error);
            attempt.raw = Some(raw);
            attempt.errno = error.as_ref().and_then(io::Error::raw_os_error);
            if raw >= 0 {
                attempt.bytes.truncate(raw as usize);
            } else {
                attempt.bytes.clear();
            }
            if let Some(error) = error {
                return Err(error);
            }
            require(
                raw as usize == size,
                "terminal source Store changed during acknowledged-prefix read",
            )?;
            self.check()?;
            require(
                stat(self.rights[1].as_raw_fd())?.size == size as i64,
                "terminal source Store extent changed after acknowledged-prefix read",
            )?;
            let bytes = &self.acknowledged_prefix_attempt.as_ref().unwrap().bytes;
            let prefix = &bytes[..acknowledged.bytes.len()];
            require(
                prefix == acknowledged.bytes
                    && <[u8; 32]>::from(Sha256::digest(prefix)) == acknowledged.digest,
                "actual held Store no longer contains exact acknowledged bytes",
            )?;
            let history = SourceHistory::read(&self.intent, prefix.to_vec())?;
            require(
                history.frames == acknowledged.frames
                    && history.commitment() == acknowledged.commitment(),
                "acknowledged prefix parser changed original callbacks",
            )?;
            Ok(AcknowledgedSourceReadback {
                history,
                unacknowledged_tail: bytes[prefix.len()..].to_vec(),
            })
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub(super) fn read(&mut self, commitment: &Value) -> io::Result<SourceHistory> {
        let result = (|| {
            self.check()?;
            let size = stat(self.rights[1].as_raw_fd())?.size as usize;
            let mut bytes = vec![0; size + 1];
            let raw = unsafe {
                libc::pread(
                    self.rights[1].as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(
                raw as usize == size,
                "live source Store changed during read",
            )?;
            bytes.truncate(size);
            self.check()?;
            let history = SourceHistory::read(&self.intent, bytes)?;
            require(
                history.commitment() == *commitment,
                "live source Store differs from healthy peer commitment",
            )?;
            Ok(history)
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
}

/// Creation source history is immutable after failure; all subsequent removal
/// rows live in this new Store. It accepts only paired reverse deletion frames,
/// after the envelope has installed actual dual-history/native authority.
#[derive(Debug)]
pub(super) struct RemovalJournal {
    pub(super) store: Store,
    intent: Intent,
    pending: Option<(OwnerSnapshot, Write, Vec<u8>)>,
    last_role: u32,
    count: usize,
    refused: Option<Failure>,
}
impl RemovalJournal {
    pub(super) fn retain(directory: OwnedFd, intent: Intent) -> Self {
        Self {
            store: Store::retain(directory, &intent),
            intent,
            pending: None,
            last_role: 18,
            count: 0,
            refused: None,
        }
    }
    pub(super) fn initialize(&mut self, role: &str) -> io::Result<()> {
        self.store
            .initialize(json!({"schema":"hermit-creation-cleanup-store-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"role":role}))
    }
    pub(super) fn append(
        &mut self,
        owner: OwnerSnapshot,
        write: Write,
        line: &[u8],
    ) -> io::Result<[u8; 32]> {
        let result = (|| {
            require(
                self.refused.is_none() && self.count < 34,
                "removal journal refused or original34 exhausted",
            )?;
            require(
                write.remove == 1
                    && (1..=17).contains(&write.role)
                    && self.intent.command(write.role, 1)?.as_bytes() == line
                    && write.submitted == line.len() as u64
                    && owner.incarnation == self.intent.incarnation
                    && owner.group == self.intent.group()
                    && owner.event == self.intent.event()
                    && owner.phase == 7
                    && owner.pending_role == write.role
                    && owner.pending_remove == 1
                    && owner.pending_bytes == write.submitted
                    && owner.verified_sites <= 0x1ffff
                    && owner.attempted_sites <= 0x1ffff
                    && owner.write_unknown <= 1,
                "removal callback changed exact run/role/line",
            )?;
            match (write.started, write.completed) {
                (0, 0) => {
                    require(
                        self.pending.is_none()
                            && write.role < self.last_role
                            && write.raw == 0
                            && write.error == 0,
                        "removal intent repeated or invented result",
                    )?;
                    let digest = self.store.append(json!({"kind":"before-remove","owner":owner,"write":write,"line":super::hex(line)}))?;
                    self.pending = Some((owner, write, line.to_vec()));
                    self.count += 1;
                    Ok(digest)
                }
                (1, 1) => {
                    let (pending_owner, pending_write, pending_line) = self
                        .pending
                        .as_ref()
                        .ok_or_else(|| io::Error::other("removal outcome lacks intent"))?;
                    require(
                        &owner == pending_owner
                            && write.role == pending_write.role
                            && write.submitted == pending_write.submitted
                            && line == pending_line
                            && ((write.raw < 0 && (1..=4095).contains(&write.error))
                                || (write.raw >= 0 && write.error == 0)),
                        "removal outcome changed pending native operation",
                    )?;
                    let digest=self.store.append(json!({"kind":"after-remove","owner":owner,"write":write,"line":super::hex(line)}))?;
                    self.pending = None;
                    self.last_role = write.role;
                    self.count += 1;
                    if write.raw != write.submitted as i64 {
                        self.refused = Some(Failure::capture(&io::Error::other(
                            "native removal failure retained",
                        )));
                    }
                    Ok(digest)
                }
                _ => Err(io::Error::other("removal callback shape differs")),
            }
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub(super) fn complete(&mut self) -> io::Result<()> {
        require(
            self.refused.is_none() && self.pending.is_none(),
            "removal journal is incomplete or refused",
        )?;
        self.store.verify()
    }
}

#[cfg(test)]
mod creator_query_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let fd = std::fs::File::open(directory.path()).unwrap().into();
        let intent = Intent::new("1a".repeat(16), 31).unwrap();
        let mut store = Store::retain(fd, &intent);
        store
            .initialize(json!({"controlled":"Store component only"}))
            .unwrap();
        (directory, store)
    }
    fn row(kind: &str, bytes: usize) -> Value {
        let mut value = json!({"kind":kind,"padding":""});
        let overhead = canonical(&value).unwrap().len() + 1;
        value["padding"] = json!("x".repeat(bytes.checked_sub(overhead).unwrap()));
        assert_eq!(canonical(&value).unwrap().len() + 1, bytes);
        value
    }
    fn disk(store: &Store) -> Vec<u8> {
        let mut bytes = vec![0; 1_048_577];
        let raw = unsafe {
            libc::pread(
                store.file.as_ref().unwrap().as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        assert!(raw >= 0);
        bytes.truncate(raw as usize);
        bytes
    }
    fn pair() -> [Value; 2] {
        [
            row("actual-source-namespace-query", 4096),
            row("actual-executable-query", 4096),
        ]
    }

    #[test]
    fn creator_query_pair_preserves_two_maximum_rows_with_one_barrier() {
        let (_directory, mut store) = store();
        let mut expected = store.content.clone();
        let syncs = store.file_syncs;
        let [namespace, executable] = pair();
        for value in [&namespace, &executable] {
            expected.extend(canonical(value).unwrap());
            expected.push(b'\n');
        }
        let digest = store.append_creator_queries(namespace, executable).unwrap();
        assert_eq!(store.file_syncs, syncs + 1);
        assert_eq!(store.content, expected);
        assert_eq!(disk(&store), expected);
        assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&expected)));
        store.verify().unwrap();
    }

    #[test]
    fn creator_query_pair_bounds_refuse_before_writing_either_row_and_stay_refused() {
        for oversized in 0..3 {
            let (_directory, mut store) = store();
            let [mut namespace, mut executable] = pair();
            match oversized {
                0 => namespace = row("actual-source-namespace-query", 4097),
                1 => executable = row("actual-executable-query", 4097),
                2 => {
                    // Controlled existing canonical rows fill the actual same
                    // file to two bytes beyond the pair's remaining allowance.
                    let target = 1_048_576 - 8192 + 2;
                    let mut padding = target - store.content.len();
                    let mut tail = Vec::new();
                    if padding % 2 != 0 {
                        tail.extend_from_slice(b"{}\n");
                        padding -= 3;
                    }
                    for _ in 0..padding / 2 {
                        tail.extend_from_slice(b"0\n");
                    }
                    assert_eq!(
                        unsafe {
                            libc::pwrite(
                                store.file.as_ref().unwrap().as_raw_fd(),
                                tail.as_ptr().cast(),
                                tail.len(),
                                store.content.len() as libc::off_t,
                            )
                        },
                        tail.len() as isize
                    );
                    store.content.extend_from_slice(&tail);
                    store.verify().unwrap();
                }
                _ => unreachable!(),
            }
            let before = store.content.clone();
            let writes = store.writes.len();
            let syncs = store.file_syncs;
            let error = store
                .append_creator_queries(namespace, executable)
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "original journal row/aggregate bound exceeded"
            );
            assert_eq!(store.writes.len(), writes);
            assert_eq!(store.file_syncs, syncs);
            assert_eq!(store.content, before);
            assert_eq!(disk(&store), before);
            let [namespace, executable] = pair();
            assert!(store.append_creator_queries(namespace, executable).is_err());
            assert!(store.append(json!({"attempted":"repair"})).is_err());
            assert!(store.verify().is_err());
            assert_eq!(
                store.refused.as_ref().unwrap().error().to_string(),
                error.to_string()
            );
            assert_eq!(store.writes.len(), writes);
            assert_eq!(disk(&store), before);
        }
    }

    #[test]
    fn creator_query_pair_native_write_failure_keeps_original_prefix_and_first_cause() {
        let (directory, mut store) = store();
        let before = store.content.clone();
        let writes = store.writes.len();
        let syncs = store.file_syncs;
        let readonly =
            std::fs::File::open(directory.path().join(store.name.to_str().unwrap())).unwrap();
        let original = store.file.replace(readonly.into()).unwrap();
        let [namespace, executable] = pair();
        let error = store
            .append_creator_queries(namespace, executable)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        assert_eq!(
            store.writes[writes..],
            [(before.len(), -1, Some(libc::EBADF))]
        );
        assert_eq!(store.file_syncs, syncs);
        assert_eq!(store.content, before);
        assert_eq!(disk(&store), before);
        drop(store.file.replace(original));
        let [namespace, executable] = pair();
        assert!(store.append_creator_queries(namespace, executable).is_err());
        assert!(store.append(json!({"attempted":"repair"})).is_err());
        assert!(store.verify().is_err());
        assert_eq!(
            store.refused.as_ref().unwrap().error().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(store.writes.len(), writes + 1);
        assert_eq!(store.file_syncs, syncs);
        assert_eq!(disk(&store), before);
    }
}
