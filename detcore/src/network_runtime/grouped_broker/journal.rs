//! Durable exact write history. Successful serialization is not native custody.
use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
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
}
