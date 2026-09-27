//! S1 terminal custody transfer and the parent's exclusive post-join cursor.
//! S2 import/offer/echo/ACK requires a separate retained owner and is not present.
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::time::Instant;

use super::Failure;
use super::journal;
use super::owner;
use super::require;
use super::wire;

#[derive(Debug)]
pub(super) struct CursorCustody {
    descriptors: [RawFd; 3],
    deadline: Instant,
    native_deadline: u64,
    parent: wire::Credentials,
}
/// No constructor from FD, phase number, JSON, or a peer's diagnostic record.
/// The borrow spans every rewind/read and prevents a same-process second lease.
/// Its captured actual parent identity also rejects use of a forked copy.
#[derive(Debug)]
pub(super) struct LocalReadLease<'a> {
    custody: &'a mut CursorCustody,
    controls: &'a owner::Controls,
}
impl CursorCustody {
    // Only an actually joined source bundle can create the initial host epoch.
    // No SourceTerminal is issued until its fresh snapshots/census also pass.
    pub fn after_source_join(joined: &super::serial::JoinedSource) -> io::Result<Self> {
        let controls = joined.controls()?;
        controls.check()?;
        require(
            controls.fds.len() == 3,
            "joined cursor control population differs",
        )?;
        Ok(Self {
            descriptors: std::array::from_fn(|i| controls.fds[i].as_raw_fd()),
            deadline: joined.deadline(),
            native_deadline: joined.native_deadline(),
            parent: wire::Credentials {
                pid: unsafe { libc::getpid() },
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            },
        })
    }
    fn check(&self) -> io::Result<()> {
        require(
            Instant::now() < self.deadline
                && super::guardian::monotonic_ns()? < self.native_deadline,
            "shared cursor original stage expired",
        )?;
        require(
            self.parent
                == (wire::Credentials {
                    pid: unsafe { libc::getpid() },
                    uid: unsafe { libc::getuid() },
                    gid: unsafe { libc::getgid() },
                }),
            "shared cursor belongs to another actual actor",
        )
    }
    pub fn local_read<'a>(
        &'a mut self,
        controls: &'a owner::Controls,
    ) -> io::Result<LocalReadLease<'a>> {
        self.check()?;
        require(
            controls.fds.len() == 3
                && controls
                    .fds
                    .iter()
                    .zip(self.descriptors)
                    .all(|(fd, original)| fd.as_raw_fd() == original),
            "shared cursor original descriptions differ",
        )?;
        controls.check()?;
        Ok(LocalReadLease {
            custody: self,
            controls,
        })
    }
}
impl LocalReadLease<'_> {
    pub fn check(&self, controls: &owner::Controls, deadline: Instant) -> io::Result<()> {
        self.custody.check()?;
        require(
            std::ptr::eq(self.controls, controls) && deadline == self.custody.deadline,
            "snapshot lacks exact original cursor lease or stage",
        )
    }
    pub fn snapshot(&mut self, index: usize) -> io::Result<Vec<u8>> {
        let controls = self.controls;
        let deadline = self.custody.deadline;
        controls.snapshot_with_lease(self, index, deadline)
    }
}

#[derive(Clone, Copy, Debug)]
enum ExportPart {
    Header,
    Creator,
    Controls,
    Journal,
    Query(usize),
    Chunk,
    End,
}
#[derive(Debug)]
struct ExportFrame {
    bytes: Vec<u8>,
    part: ExportPart,
}
#[derive(Debug)]
pub(super) struct TerminalExport {
    frames: Vec<ExportFrame>,
    next: usize,
    prepared: bool,
    prepare_attempted: bool,
    acknowledged: bool,
    ack: Option<Vec<u8>>,
    refused: Option<Failure>,
}
impl TerminalExport {
    pub fn retain() -> Self {
        Self {
            frames: Vec::new(),
            next: 0,
            prepared: false,
            prepare_attempted: false,
            acknowledged: false,
            ack: None,
            refused: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        Ok(())
    }
    fn push(&mut self, part: ExportPart, value: serde_json::Value) -> io::Result<()> {
        let bytes = journal::canonical(&value)?;
        require(
            bytes.len() <= wire::MAX_PACKET
                && self.frames.len() < 128
                && self.frames.iter().map(|f| f.bytes.len()).sum::<usize>() + bytes.len()
                    <= 1_048_576,
            "terminal export original packet/128 receipt/1MiB aggregate bound exceeded",
        )?;
        self.frames.push(ExportFrame { bytes, part });
        Ok(())
    }
    pub fn prepare(&mut self, holder: &mut super::guardian::Holder) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                !self.prepare_attempted,
                "terminal export preparation cannot repeat",
            )?;
            self.prepare_attempted = true;
            holder.begin_success_export()?;
            let completed = holder.check_completed_source()?;
            require(
                completed.role() == super::guardian::Role::Keeper,
                "terminal export producer is not original Keeper",
            )?;
            let record = completed.record()?;
            let nonce = completed.intent().nonce.clone();
            let incarnation = completed.intent().incarnation;
            let native_deadline = completed.native_deadline();
            let query_records = completed
                .queries()?
                .iter()
                .map(owner::CompletedQuery::record)
                .collect::<io::Result<Vec<_>>>()?;
            self.push(
                ExportPart::Header,
                serde_json::json!({"schema":"hermit-grouped-terminal-header-v1","record":record}),
            )?;
            self.push(ExportPart::Creator, serde_json::json!({"schema":"hermit-grouped-terminal-rights-v1","role":"creator","sequence":1}))?;
            self.push(ExportPart::Controls, serde_json::json!({"schema":"hermit-grouped-terminal-rights-v1","role":"controls","sequence":2}))?;
            self.push(ExportPart::Journal, serde_json::json!({"schema":"hermit-grouped-terminal-rights-v1","role":"journal","sequence":3}))?;
            for (index, record) in query_records.into_iter().enumerate() {
                self.push(
                    ExportPart::Query(index),
                    serde_json::json!({"schema":"hermit-grouped-terminal-query-v1",
                    "sequence":index+4,"query":index,"record":record}),
                )?;
            }
            let frozen = holder.frozen_journal()?;
            let bytes = frozen.bytes();
            for (index, chunk) in bytes.chunks(16_384).enumerate() {
                self.push(
                    ExportPart::Chunk,
                    serde_json::json!({"schema":"hermit-grouped-terminal-journal-chunk-v1",
                    "offset":index*16_384,"bytes":super::hex(chunk)}),
                )?;
            }
            use sha2::Digest;
            let hash = super::hex(&sha2::Sha256::digest(bytes));
            let end = serde_json::json!({"schema":"hermit-grouped-terminal-end-v1","nonce":nonce,
                "incarnation":incarnation,"stage_deadline":native_deadline,
                "bytes":bytes.len(),"sha256":hash,"frames":self.frames.len()+1});
            self.push(ExportPart::End, end.clone())?;
            let mut ack = end;
            ack["schema"] = serde_json::json!("hermit-grouped-terminal-custody-ack-v1");
            self.ack = Some(journal::canonical(&ack)?);
            self.prepared = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn send_next(
        &mut self,
        holder: &mut super::guardian::Holder,
        parent: &mut wire::Channel,
    ) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.prepared && !self.acknowledged,
                "terminal export not prepared or already acknowledged",
            )?;
            if self.next == self.frames.len() {
                return Ok(false);
            }
            let index = self.next;
            self.next += 1; // occupy before any native proof/send can fail
            let frame = &self.frames[index];
            let completed = holder.check_completed_source()?;
            let deadline = completed.deadline();
            let native_deadline = completed.native_deadline();
            match frame.part {
                ExportPart::Creator => {
                    parent.send_once(&frame.bytes, &completed.creator_rights())?
                }
                ExportPart::Controls => {
                    use std::os::fd::AsFd;
                    let rights: Vec<_> = completed
                        .controls()
                        .fds
                        .iter()
                        .map(|fd| fd.as_fd())
                        .collect();
                    parent.send_once(&frame.bytes, &rights)?;
                }
                ExportPart::Query(index) => {
                    parent.send_once(&frame.bytes, &completed.queries()?[index].rights())?
                }
                ExportPart::Journal => {
                    let frozen = holder.frozen_journal()?;
                    parent.send_once(&frame.bytes, &frozen.rights())?;
                }
                _ => parent.send_once(&frame.bytes, &[])?,
            }
            require(
                Instant::now() < deadline && super::guardian::monotonic_ns()? < native_deadline,
                "terminal export send completed after original stage",
            )?;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn receive_ack(
        &mut self,
        holder: &mut super::guardian::Holder,
        parent: &mut wire::Channel,
        peer: wire::Credentials,
    ) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.prepared && self.next == self.frames.len() && !self.acknowledged,
                "terminal custody ACK out of phase or repeated",
            )?;
            let completed = holder.check_completed_source()?;
            let deadline = completed.deadline();
            let native_deadline = completed.native_deadline();
            let Some(index) = parent.receive(2048)? else {
                return Ok(false);
            };
            let packet = &parent.packets[index];
            packet.exact(0, peer)?;
            require(
                Some(&packet.bytes) == self.ack.as_ref(),
                "terminal custody ACK differs",
            )?;
            require(
                Instant::now() < deadline && super::guardian::monotonic_ns()? < native_deadline,
                "terminal custody ACK received after original stage",
            )?;
            self.acknowledged = true;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn sent(&self) -> bool {
        self.prepared && self.next == self.frames.len() && self.refused.is_none()
    }
    pub fn acknowledged(&self) -> bool {
        self.acknowledged && self.refused.is_none()
    }
}

#[derive(Debug)]
struct ExportedCompletedQuery {
    rights: Vec<std::os::fd::OwnedFd>,
    record: serde_json::Value,
}
#[derive(Debug)]
pub(super) struct KeeperExportReceiver {
    record: Option<serde_json::Value>,
    creator: Vec<std::os::fd::OwnedFd>,
    controls: Vec<std::os::fd::OwnedFd>,
    journal: Option<journal::ArchivedKeeperJournal>,
    queries: Vec<ExportedCompletedQuery>,
    frames: usize,
    bytes: usize,
    end: Option<serde_json::Value>,
    ack_attempted: bool,
    acknowledged: bool,
    refused: Option<Failure>,
}
impl KeeperExportReceiver {
    pub fn retain() -> Self {
        Self {
            record: None,
            creator: Vec::new(),
            controls: Vec::new(),
            journal: None,
            queries: Vec::new(),
            frames: 0,
            bytes: 0,
            end: None,
            ack_attempted: false,
            acknowledged: false,
            refused: None,
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        Ok(())
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn receive(
        &mut self,
        parent: &mut wire::Channel,
        peer: wire::Credentials,
        original: &super::guardian::CompletedHolder<'_>,
    ) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.end.is_none() && Instant::now() < original.deadline(),
                "terminal export repeated or original deadline expired",
            )?;
            let Some(index) = parent.receive(wire::MAX_PACKET)? else {
                return Ok(false);
            };
            let packet = &mut parent.packets[index];
            require(
                self.frames < 128 && self.bytes + packet.bytes.len() <= 1_048_576,
                "terminal export original receipt/aggregate bound exceeded",
            )?;
            self.bytes += packet.bytes.len();
            let sequence = self.frames;
            self.frames += 1;
            let value: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "terminal export packet is not canonical",
            )?;
            let rights = match sequence {
                1 | 3 => 2,
                2 | 4..=7 => 3,
                _ => 0,
            };
            packet.exact(rights, peer)?;
            match sequence {
                0 => {
                    let mut expected = original.record()?;
                    expected["role"] = serde_json::json!("keeper");
                    require(
                        value
                            == serde_json::json!({"schema":"hermit-grouped-terminal-header-v1","record":expected}),
                        "independent completed Keeper history or identity differs",
                    )?;
                    self.record = Some(value["record"].clone());
                }
                1 | 2 | 3 => {
                    let role = ["", "creator", "controls", "journal"][sequence];
                    require(
                        value
                            == serde_json::json!({"schema":"hermit-grouped-terminal-rights-v1","role":role,"sequence":sequence}),
                        "terminal export rights order differs",
                    )?;
                    match sequence {
                        1 => self.creator = std::mem::take(&mut packet.rights),
                        2 => self.controls = std::mem::take(&mut packet.rights),
                        3 => {
                            self.journal = Some(journal::ArchivedKeeperJournal::retain(
                                std::mem::take(&mut packet.rights),
                                original.intent(),
                            ));
                            self.journal.as_mut().unwrap().initialize()?;
                        }
                        _ => unreachable!(),
                    }
                }
                4..=7 => {
                    require(
                        value
                            == serde_json::json!({"schema":"hermit-grouped-terminal-query-v1","sequence":sequence,
                        "query":sequence-4,"record":value["record"]}),
                        "terminal export query envelope differs",
                    )?;
                    self.queries.push(ExportedCompletedQuery {
                        rights: std::mem::take(&mut packet.rights),
                        record: value["record"].clone(),
                    });
                }
                _ => {
                    if value["schema"] == "hermit-grouped-terminal-journal-chunk-v1" {
                        let text = value["bytes"]
                            .as_str()
                            .ok_or_else(|| io::Error::other("archive chunk bytes absent"))?;
                        let chunk = decode_hex(text, 16_384)?;
                        let offset = value["offset"]
                            .as_u64()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| io::Error::other("archive chunk offset absent"))?;
                        require(
                            value
                                == serde_json::json!({"schema":"hermit-grouped-terminal-journal-chunk-v1","offset":offset,"bytes":text}),
                            "archive chunk envelope differs",
                        )?;
                        self.journal
                            .as_mut()
                            .unwrap()
                            .append_chunk(offset, &chunk)?;
                    } else {
                        use sha2::Digest;
                        let bytes = self.journal.as_ref().unwrap().bytes();
                        let expected = serde_json::json!({"schema":"hermit-grouped-terminal-end-v1","nonce":original.intent().nonce,
                            "incarnation":original.intent().incarnation,"stage_deadline":original.native_deadline(),
                            "bytes":bytes.len(),"sha256":super::hex(&sha2::Sha256::digest(bytes)),"frames":self.frames});
                        require(
                            value == expected,
                            "terminal export final commitment differs",
                        )?;
                        self.end = Some(value);
                    }
                }
            }
            require(
                Instant::now() < original.deadline()
                    && super::guardian::monotonic_ns()? < original.native_deadline(),
                "terminal export receive exceeded original stage",
            )?;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn complete(&self) -> bool {
        self.end.is_some() && self.refused.is_none()
    }
    pub fn verify(&mut self, original: &super::guardian::CompletedHolder<'_>) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.complete()
                    && self.creator.len() == 2
                    && self.controls.len() == 3
                    && self.queries.len() == 4,
                "terminal export original custody incomplete",
            )?;
            let mut expected = original.record()?;
            expected["role"] = serde_json::json!("keeper");
            require(
                self.record.as_ref() == Some(&expected),
                "terminal export independent history changed",
            )?;
            let original_creator = original.creator_rights();
            // The immutable source helper opens its Keeper pidfd separately
            // from the Guardian bootstrap. Both refer to the same held pidfs
            // object, not necessarily the same open-file description.
            verify_source_pidfd(
                self.creator[0].as_raw_fd(),
                original_creator[0].as_raw_fd(),
                &expected["source_pidfd"],
            )?;
            let cgroup = owner::stat(original_creator[1].as_raw_fd())?;
            use std::os::fd::AsFd;
            match owner::read_retained_cgroup(
                self.creator[0].as_fd(),
                self.creator[1].as_fd(),
                &cgroup,
            )? {
                owner::CgroupReadbackProgress::Observed(actual) => require(
                    actual.creator_terminal && actual.unlinked,
                    "exported Creator is not actually terminal and unlinked",
                )?,
                owner::CgroupReadbackProgress::Pending(_) => {
                    return Err(io::Error::other("exported Creator cgroup still pending"));
                }
            }
            for (received, original) in self.controls.iter().zip(&original.controls().fds) {
                require(
                    unsafe {
                        libc::syscall(
                            libc::SYS_kcmp,
                            libc::getpid(),
                            libc::getpid(),
                            0,
                            received.as_raw_fd(),
                            original.as_raw_fd(),
                        )
                    } == 0,
                    "exported control is not the original shared open-file description",
                )?;
            }
            let originals = original.queries()?;
            for (query, original) in self.queries.iter().zip(&originals) {
                let expected = original.record()?;
                require(
                    query.record.as_object().is_some_and(|record| {
                        record.keys().eq(expected.as_object().unwrap().keys())
                    }) && query.record["argv"] == expected["argv"],
                    "exported original query kind or complete field population differs",
                )?;
                verify_exported_query(query)?;
            }
            let header = serde_json::json!({"kind":"grouped-independent-holder","role":"keeper",
                "nonce":original.intent().nonce,"incarnation":original.intent().incarnation,
                "unit":original.unit(),"stage_deadline":original.native_deadline()});
            self.journal.as_mut().unwrap().verify(&header, &expected)?;
            require(
                Instant::now() < original.deadline(),
                "terminal export verify exceeded original stage",
            )
        })();
        self.remember(result)
    }
    pub fn acknowledge(
        &mut self,
        parent: &mut wire::Channel,
        original: &super::guardian::CompletedHolder<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(!self.ack_attempted, "terminal custody ACK cannot repeat")?;
            self.ack_attempted = true;
            self.verify(original)?;
            let mut ack = self.end.as_ref().unwrap().clone();
            ack["schema"] = serde_json::json!("hermit-grouped-terminal-custody-ack-v1");
            require(
                Instant::now() < original.deadline(),
                "terminal custody ACK exceeded original stage",
            )?;
            parent.send_once(&journal::canonical(&ack)?, &[])?;
            require(
                Instant::now() < original.deadline()
                    && super::guardian::monotonic_ns()? < original.native_deadline(),
                "terminal custody ACK send completed after original stage",
            )?;
            self.acknowledged = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn acknowledged(&self) -> bool {
        self.acknowledged && self.refused.is_none()
    }
    pub fn consume_handoff(&mut self) -> io::Result<()> {
        self.check()?;
        require(
            self.acknowledged(),
            "archive handoff precedes actual custody ACK",
        )?;
        self.journal.as_mut().unwrap().consume_handoff()
    }
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        self.creator
            .iter()
            .chain(self.controls.iter())
            .chain(self.journal.iter().flat_map(|journal| journal.rights()))
            .chain(self.queries.iter().flat_map(|query| &query.rights))
            .map(AsRawFd::as_raw_fd)
            .collect()
    }
}
fn decode_hex(text: &str, cap: usize) -> io::Result<Vec<u8>> {
    require(
        text.len() % 2 == 0
            && text.len() / 2 <= cap
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "bounded archive hex differs",
    )?;
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).map_err(io::Error::other)
        })
        .collect()
}
fn verify_exported_query(query: &ExportedCompletedQuery) -> io::Result<()> {
    require(
        query.rights.len() == 3 && owner::terminal(query.rights[0].as_raw_fd())?,
        "exported original query pidfd is absent or live",
    )?;
    let record = &query.record;
    let descriptions = record["original_descriptions"]
        .as_array()
        .ok_or_else(|| io::Error::other("exported original query descriptions absent"))?;
    require(
        descriptions.len() == 3,
        "exported original query description population differs",
    )?;
    for (fd, description) in query.rights.iter().zip(descriptions) {
        owner::verify_description(fd.as_raw_fd(), description)?;
    }
    require(
        record["eof"] == serde_json::json!([true, true])
            && record["wait_code"] == 0
            && record["raw_wait_status"] == 0
            && record["stderr"] == ""
            && record["original_query_origin_present"] == true
            && record["initialized"] == true
            && record["first_failure"].is_null()
            && record["retirement_failure"].is_null()
            && record["retirement_started"] == false
            && record["custody_retired"] == false
            && record["completed_with_original_cutoff"] == true
            && record["original_query_bound_seconds"] == 2
            && record["pidfd_held"] == true
            && record["pid"]
                .as_u64()
                .is_some_and(|pid| (2..=i32::MAX as u64).contains(&pid)),
        "exported original query record was not successful native custody",
    )?;
    decode_hex(
        record["stdout"]
            .as_str()
            .ok_or_else(|| io::Error::other("exported query stdout absent"))?,
        1_048_576,
    )?;
    // This is still ExportedCompletedQuery, never ManagerQuery/Snapshot: the
    // producer's actual wait relationship is not transferable through SCM.
    for pipe in &query.rights[1..] {
        require(
            owner::stat(pipe.as_raw_fd())?.mode & libc::S_IFMT == libc::S_IFIFO,
            "exported original query output is not a pipe",
        )?;
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        require(
            flags >= 0 && flags & libc::O_NONBLOCK != 0,
            "exported query output is blocking",
        )?;
        let mut byte = 0u8;
        let count = unsafe { libc::read(pipe.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        require(
            count == 0,
            "exported original query output is not actual EOF",
        )?;
    }
    Ok(())
}
fn verify_source_pidfd(
    received: RawFd,
    original: RawFd,
    description: &serde_json::Value,
) -> io::Result<()> {
    require(
        owner::terminal(original)? && owner::terminal(received)?,
        "exported source pidfd is not actually terminal",
    )?;
    owner::verify_description(received, description)?;
    require(
        owner::stat(received)?.same_object(&owner::stat(original)?),
        "exported source pidfd is a different held pidfs object",
    )
}

impl KeeperExportReceiver {
    pub(super) fn verify_after_leaf(
        &mut self,
        original: &super::guardian::LeafArchive<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.acknowledged(),
                "leaf Keeper export lacks original custody ACK",
            )?;
            require(
                self.complete()
                    && self.creator.len() == 2
                    && self.controls.len() == 3
                    && self.queries.len() == 4,
                "terminal export original custody incomplete",
            )?;
            let mut expected = original.completed_record()?;
            expected["role"] = serde_json::json!("keeper");
            require(
                self.record.as_ref() == Some(&expected),
                "terminal export independent history changed",
            )?;
            let original_creator = original.creator_rights();
            // The immutable source helper opens its Keeper pidfd separately
            // from the Guardian bootstrap. Both refer to the same held pidfs
            // object, not necessarily the same open-file description.
            verify_source_pidfd(
                self.creator[0].as_raw_fd(),
                original_creator[0].as_raw_fd(),
                &expected["source_pidfd"],
            )?;
            let cgroup = owner::stat(original_creator[1].as_raw_fd())?;
            use std::os::fd::AsFd;
            match owner::read_retained_cgroup(
                self.creator[0].as_fd(),
                self.creator[1].as_fd(),
                &cgroup,
            )? {
                owner::CgroupReadbackProgress::Observed(actual) => require(
                    actual.creator_terminal && actual.unlinked,
                    "exported Creator is not actually terminal and unlinked",
                )?,
                owner::CgroupReadbackProgress::Pending(_) => {
                    return Err(io::Error::other("exported Creator cgroup still pending"));
                }
            }
            for (received, original) in self.controls.iter().zip(&original.controls().fds) {
                require(
                    unsafe {
                        libc::syscall(
                            libc::SYS_kcmp,
                            libc::getpid(),
                            libc::getpid(),
                            0,
                            received.as_raw_fd(),
                            original.as_raw_fd(),
                        )
                    } == 0,
                    "exported control is not the original shared open-file description",
                )?;
            }
            let originals = original.queries()?;
            for (query, original) in self.queries.iter().zip(&originals) {
                let expected = original.record()?;
                require(
                    query.record.as_object().is_some_and(|record| {
                        record.keys().eq(expected.as_object().unwrap().keys())
                    }) && query.record["argv"] == expected["argv"],
                    "exported original query kind or complete field population differs",
                )?;
                verify_exported_query(query)?;
            }
            let header = serde_json::json!({"kind":"grouped-independent-holder","role":"keeper",
                "nonce":original.intent().nonce,"incarnation":original.intent().incarnation,
                "unit":original.unit(),"stage_deadline":original.native_deadline()});
            self.journal
                .as_mut()
                .unwrap()
                .verify_consumed(&header, &expected)?;
            require(
                Instant::now() < original.deadline(),
                "terminal export verify exceeded original stage",
            )
        })();
        self.remember(result)
    }
}

/// Borrowed descriptor roles from the actual verified, consumed Keeper export.
/// The old receiver remains the owner; this is not a new SourceTerminal.
#[derive(Debug)]
pub(super) struct LeafKeeperArchive<'a> {
    original: &'a KeeperExportReceiver,
}
impl KeeperExportReceiver {
    pub(super) fn borrow_after_leaf(
        &mut self,
        original: &super::guardian::LeafArchive<'_>,
    ) -> io::Result<LeafKeeperArchive<'_>> {
        self.verify_after_leaf(original)?;
        Ok(LeafKeeperArchive { original: self })
    }
}
impl LeafKeeperArchive<'_> {
    pub fn record(&self) -> &serde_json::Value {
        self.original.record.as_ref().unwrap()
    }
    pub fn creator_rights(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.original.creator.iter().map(AsFd::as_fd).collect()
    }
    pub fn control_rights(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.original.controls.iter().map(AsFd::as_fd).collect()
    }
    pub fn store_rights(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.original
            .journal
            .as_ref()
            .unwrap()
            .rights()
            .iter()
            .map(AsFd::as_fd)
            .collect()
    }
    pub fn history_bytes(&self) -> &[u8] {
        self.original.journal.as_ref().unwrap().bytes()
    }
    pub fn query(
        &self,
        index: usize,
    ) -> io::Result<(&serde_json::Value, Vec<std::os::fd::BorrowedFd<'_>>)> {
        use std::os::fd::AsFd;
        let query = self
            .original
            .queries
            .get(index)
            .ok_or_else(|| io::Error::other("leaf Keeper query index outside original4"))?;
        Ok((
            &query.record,
            query.rights.iter().map(AsFd::as_fd).collect(),
        ))
    }
}
