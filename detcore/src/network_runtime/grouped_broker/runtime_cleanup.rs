//! Retained runtime custody for the actual grouped provider. Transport records
//! commit held native descriptions; they never reconstruct a Creator, Child,
//! successful query, SourceTerminal, or an exclusive shared-OFD read lease.
use std::ffi::CString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::Failure;
use super::Intent;
use super::cleanup_native as ffi;
use super::guardian;
use super::hex;
use super::journal;
use super::native::Bridge;
use super::owner;
use super::require;
use super::wire;

pub(super) fn duplicate(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
pub(super) fn before(cutoff: u64) -> io::Result<()> {
    require(
        cutoff != 0 && guardian::monotonic_ns()? < cutoff,
        "runtime original cutoff expired",
    )
}
pub(super) fn credentials(pid: i32) -> io::Result<wire::Credentials> {
    require(
        pid > 1 && pid != unsafe { libc::getpid() },
        "runtime peer is not independent",
    )?;
    Ok(wire::Credentials {
        pid,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    })
}
pub(super) fn same_ofd(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> io::Result<()> {
    let raw = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            libc::getpid(),
            libc::getpid(),
            0,
            a.as_raw_fd() as libc::c_ulong,
            b.as_raw_fd() as libc::c_ulong,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    require(raw == 0, "runtime transfer changed original OFD")
}
pub(super) fn receive(
    channel: &mut wire::Channel,
    peer: wire::Credentials,
    rights: usize,
    cap: usize,
    cutoff: u64,
) -> io::Result<usize> {
    loop {
        before(cutoff)?;
        if let Some(index) = channel.receive(cap)? {
            let packet = &channel.packets[index];
            packet.exact(rights, peer)?;
            let value: Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "runtime frame is not canonical",
            )?;
            before(cutoff)?;
            return Ok(index);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn receive_progress(
    channel: &mut wire::Channel,
    peer: wire::Credentials,
    rights: usize,
    cap: usize,
    cutoff: u64,
    progress: &mut impl FnMut() -> io::Result<()>,
    inspect: &mut impl FnMut(&wire::Packet) -> io::Result<()>,
) -> io::Result<usize> {
    loop {
        before(cutoff)?;
        progress()?;
        if let Some(index) = channel.receive(cap)? {
            let packet = &channel.packets[index];
            inspect(packet)?;
            packet.exact(rights, peer)?;
            let value: Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "runtime frame is not canonical",
            )?;
            before(cutoff)?;
            return Ok(index);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
pub(super) fn receive_value(
    channel: &mut wire::Channel,
    peer: wire::Credentials,
    cutoff: u64,
) -> io::Result<Value> {
    let index = receive(channel, peer, 0, 4096, cutoff)?;
    serde_json::from_slice(&channel.packets[index].bytes).map_err(io::Error::other)
}
pub(super) fn send(
    channel: &mut wire::Channel,
    value: &Value,
    rights: &[BorrowedFd<'_>],
    cutoff: u64,
) -> io::Result<()> {
    before(cutoff)?;
    let bytes = journal::canonical(value)?;
    require(
        bytes.len() <= wire::MAX_PACKET,
        "runtime frame exceeds original packet bound",
    )?;
    channel.send_once(&bytes, rights)?;
    before(cutoff)
}

/// Continuously held pre-admission custody. Healthy Store bytes are observed
/// only while the original source callback is withheld or both writers have
/// become terminal. This never constructs Holder/Creator/SourceTerminal.
#[derive(Debug)]
struct PrefixReceipt {
    history: usize,
    sequence: u64,
    validated: bool,
    durable: bool,
    ack_attempted: bool,
    ack_completed: bool,
}
#[derive(Debug)]
struct EarlyCreation {
    frames: Vec<(Value, Vec<u8>, Vec<OwnedFd>)>,
    readers: Vec<journal::SourceLedgerReader>,
    histories: Vec<[journal::SourceHistory; 2]>,
    prefixes: Vec<PrefixReceipt>,
    terminal_readbacks: Vec<journal::AcknowledgedSourceReadback>,
    ready: bool,
    acknowledged: u64,
    agreement: Option<Value>,
    agreement_sha: Option<String>,
    eligible: Option<u32>,
    observed_mask: Option<u32>,
    read_granted: bool,
    read_relinquished: bool,
    refused: bool,
    retired: bool,
    closes: Vec<Value>,
    snapshots: Vec<AbsenceObservation>,
}
impl EarlyCreation {
    fn retain() -> Self {
        Self {
            frames: Vec::new(),
            readers: Vec::new(),
            histories: Vec::new(),
            prefixes: Vec::new(),
            terminal_readbacks: Vec::new(),
            ready: false,
            acknowledged: 0,
            agreement: None,
            agreement_sha: None,
            eligible: None,
            observed_mask: None,
            read_granted: false,
            read_relinquished: false,
            refused: false,
            retired: false,
            closes: Vec::new(),
            snapshots: Vec::new(),
        }
    }
    fn receive(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
    ) -> io::Result<()> {
        require(
            self.frames.is_empty() && !self.ready,
            "early service custody is one-use",
        )?;
        for sequence in 0..5 {
            let index = receive(channel, peer, [3, 3, 2, 2, 0][sequence], 4096, cutoff)?;
            let packet = &mut channel.packets[index];
            self.frames.push((
                Value::Null,
                packet.bytes.clone(),
                std::mem::take(&mut packet.rights),
            ));
            let frame = self.frames.last_mut().unwrap();
            frame.0 = serde_json::from_slice(&frame.1)?;
            if sequence == 0 {
                let keeper = frame.0["keeper_pid"]
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| io::Error::other("original source Keeper PID absent"))?;
                require(
                    frame.0
                        == json!({"schema":"hermit-grouped-runtime-creation-custody-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":cutoff,
                    "creator":frame.0["creator"],"keeper_pid":keeper,
                    "initial_guardian_store":frame.0["initial_guardian_store"],
                    "initial_keeper_store":frame.0["initial_keeper_store"]}),
                    "early service source identity differs",
                )?;
                check_creator(&frame.2[..2], &frame.0["creator"], false)?;
                owner::pidfd_matches(frame.2[2].as_raw_fd(), keeper)?;
                require(
                    !owner::terminal(frame.2[2].as_raw_fd())?,
                    "original source Keeper already terminal before custody",
                )?;
            } else if sequence < 4 {
                let role = ["", "controls", "guardian_store", "keeper_store"][sequence];
                require(
                    frame.0
                        == json!({"schema":"hermit-grouped-runtime-creation-rights-v1","sequence":sequence,
                    "role":role,"nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":cutoff}),
                    "early service descriptor role differs",
                )?;
                if sequence == 1 {
                    check_controls(&frame.2)?;
                } else {
                    self.readers.push(journal::SourceLedgerReader::retain(
                        std::mem::take(&mut frame.2),
                        intent.clone(),
                    ));
                    self.readers.last_mut().unwrap().initialize()?;
                }
            }
        }
        let mut hash = Sha256::new();
        let mut count = 0usize;
        for (_, bytes, _) in &self.frames[..4] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            count += bytes.len();
        }
        require(
            self.frames[4].0
                == json!({"schema":"hermit-grouped-runtime-creation-end-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":cutoff,
            "preceding_frames":4,"preceding_bytes":count,"framed_sha256":hex(&hash.finalize())}),
            "early service custody transcript differs",
        )?;
        let guardian = self.readers[0].read(&self.frames[0].0["initial_guardian_store"])?;
        let keeper = self.readers[1].read(&self.frames[0].0["initial_keeper_store"])?;
        self.histories.push([guardian, keeper]);
        let initial = self.histories.last().unwrap();
        self.prefixes.push(PrefixReceipt {
            history: 0,
            sequence: 0,
            validated: false,
            durable: false,
            ack_attempted: false,
            ack_completed: false,
        });
        require(
            initial[0].frames.len() == 1
                && initial[1].frames.is_empty()
                && initial[0].frames[0].write.role == 1
                && initial[0].frames[0].write.started == 0,
            "early service initial histories are not original held first intent/no Keeper ACK",
        )?;
        self.prefixes[0].validated = true;
        before(cutoff)?;
        self.ready = true;
        Ok(())
    }
    fn controls(&self) -> io::Result<[BorrowedFd<'_>; 3]> {
        require(
            self.ready && !self.retired,
            "early controls lack retained preparation or were retired",
        )?;
        check_controls(&self.frames[1].2)?;
        Ok([
            self.frames[1].2[0].as_fd(),
            self.frames[1].2[1].as_fd(),
            self.frames[1].2[2].as_fd(),
        ])
    }
    fn prefix(&mut self, value: &Value, intent: &Intent, cutoff: u64) -> io::Result<usize> {
        require(
            self.ready && !self.refused && self.agreement.is_none() && self.acknowledged < 34,
            "early prefix follows refusal or final agreement",
        )?;
        let sequence = self.acknowledged + 1;
        require(
            value
                == &json!({"schema":"hermit-grouped-runtime-creation-prefix-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"stage_deadline":cutoff,"sequence":sequence,
            "guardian_store":value["guardian_store"],"keeper_store":value["keeper_store"]}),
            "early prefix changed original sequence or fields",
        )?;
        check_creator(&self.frames[0].2[..2], &self.frames[0].0["creator"], false)?;
        require(
            !owner::terminal(self.frames[0].2[2].as_raw_fd())?,
            "source Keeper terminal during held callback",
        )?;
        let guardian = self.readers[0].read(&value["guardian_store"])?;
        let keeper = self.readers[1].read(&value["keeper_store"])?;
        // Retain both observed byte strings before evaluating correspondence.
        let history = self.histories.len();
        self.histories.push([guardian, keeper]);
        let receipt = self.prefixes.len();
        self.prefixes.push(PrefixReceipt {
            history,
            sequence,
            validated: false,
            durable: false,
            ack_attempted: false,
            ack_completed: false,
        });
        let current = &self.histories[history];
        require(
            current[0].frames.len() == sequence as usize && current[0].frames == current[1].frames,
            "held callback does not have equal original dual histories",
        )?;
        if sequence > 0 {
            let prior = self
                .prefixes
                .iter()
                .find(|p| {
                    p.sequence == self.acknowledged && p.validated && p.durable && p.ack_completed
                })
                .ok_or_else(|| {
                    io::Error::other("prior prefix has no completed durable service ACK")
                })?;
            let old = &self.histories[prior.history];
            require(
                current[0].frames[..old[0].frames.len()] == old[0].frames
                    && current[1].frames[..old[1].frames.len()] == old[1].frames,
                "held callback rewrote earlier acknowledged source history",
            )?;
        }
        before(cutoff)?;
        self.prefixes[receipt].validated = true;
        Ok(receipt)
    }
    fn terminal(&self, controller: BorrowedFd<'_>) -> io::Result<()> {
        require(
            self.ready && owner::terminal(controller.as_raw_fd())?,
            "creation controller is not actually terminal",
        )?;
        check_creator(&self.frames[0].2[..2], &self.frames[0].0["creator"], true)?;
        require(
            owner::terminal(self.frames[0].2[2].as_raw_fd())?,
            "original source Keeper writer remains live",
        )
    }
    fn bind_archive(&mut self, archive: &SourceArchive, cutoff: u64) -> io::Result<()> {
        require(
            self.ready && !self.refused && self.acknowledged == 34 && archive.verified,
            "full source archive lacks actual pre-admission callback custody",
        )?;
        require(
            self.frames[0].0["creator"] == archive.frames[0].0["guardian"]["creator"],
            "completed source replaced early Creator",
        )?;
        check_creator(&self.frames[0].2[..2], &self.frames[0].0["creator"], true)?;
        require(
            owner::terminal(self.frames[0].2[2].as_raw_fd())?,
            "original Keeper is not terminal at full handoff",
        )?;
        for index in 0..2 {
            same_ofd(
                self.frames[0].2[index].as_fd(),
                archive.frames[1].2[index].as_fd(),
            )?;
        }
        for index in 0..3 {
            same_ofd(self.controls()?[index], archive.controls()?[index])?;
        }
        for (index, name) in [(0, "guardian_store"), (1, "keeper_store")] {
            for n in 0..2 {
                same_ofd(
                    self.readers[index].held_rights()[n].as_fd(),
                    archive.readers[index].held_rights()[n].as_fd(),
                )?;
            }
            let actual = self.readers[index].read(&archive.frames[0].0[name])?;
            let last = self
                .prefixes
                .iter()
                .find(|p| p.sequence == 34 && p.validated && p.durable && p.ack_completed)
                .ok_or_else(|| {
                    io::Error::other("full source lacks exact durable final service ACK")
                })?;
            require(
                actual.frames == self.histories[last.history][index].frames,
                "full archive changed the actual last acknowledged callback prefix",
            )?;
        }
        before(cutoff)
    }
}

/// Received actual descriptions remain owned even when one later shape or
/// native check refuses. The caller installs the archive before receive().
#[derive(Debug)]
pub(super) struct SourceArchive {
    pub(super) frames: Vec<(Value, Vec<u8>, Vec<OwnedFd>)>,
    pub(super) readers: Vec<journal::SourceLedgerReader>,
    histories: Vec<journal::SourceHistory>,
    verified: bool,
    frame_alias_retirement_attempted: bool,
    frame_alias_closes: Vec<Value>,
    refused: Option<Failure>,
}
impl SourceArchive {
    pub(super) fn retain() -> Self {
        Self {
            frames: Vec::new(),
            readers: Vec::new(),
            histories: Vec::new(),
            verified: false,
            frame_alias_retirement_attempted: false,
            frame_alias_closes: Vec::new(),
            refused: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    #[expect(dead_code, reason = "Typed archive receive variants are retained; active startup uses progress or inspecting variants")]
    pub(super) fn receive(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
    ) -> io::Result<()> {
        self.receive_inner(channel, peer, intent, cutoff, None)
    }
    /// Continue with an actual already-retained first packet, after the caller
    /// dispatched its schema. Every original header check still runs here.
    #[expect(dead_code, reason = "Typed archive receive variants are retained; active startup uses progress or inspecting variants")]
    pub(super) fn receive_started(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
        first_index: usize,
    ) -> io::Result<()> {
        self.receive_inner(channel, peer, intent, cutoff, Some(first_index))
    }
    #[expect(dead_code, reason = "Typed archive receive variants are retained; active startup uses progress or inspecting variants")]
    fn receive_inner(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
        first_index: Option<usize>,
    ) -> io::Result<()> {
        self.receive_progress(channel, peer, intent, cutoff, first_index, || Ok(()))
    }
    pub(super) fn receive_progress(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
        first_index: Option<usize>,
        progress: impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        self.receive_observed(channel, peer, intent, (cutoff, first_index), progress, |_| {
            Ok(())
        })
    }
    /// Inspect only the actual retained packet before archive grammar applies.
    /// A separately authenticated refusal stays in the original Channel inbox.
    pub(super) fn receive_started_inspecting(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        cutoff: u64,
        first_index: usize,
        inspect: impl FnMut(&wire::Packet) -> io::Result<()>,
    ) -> io::Result<()> {
        self.receive_observed(
            channel,
            peer,
            intent,
            (cutoff, Some(first_index)),
            || Ok(()),
            inspect,
        )
    }
    fn receive_observed(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
        intent: &Intent,
        boundary: (u64, Option<usize>),
        mut progress: impl FnMut() -> io::Result<()>,
        mut inspect: impl FnMut(&wire::Packet) -> io::Result<()>,
    ) -> io::Result<()> {
        let (cutoff, first_index) = boundary;
        let result = (|| {
            require(
                self.frames.is_empty() && self.refused.is_none(),
                "runtime archive receive is one-use",
            )?;
            for sequence in 0..16 {
                progress()?;
                let rights = match sequence {
                    0 | 15 => 0,
                    1 | 3 | 4 | 5 => 2,
                    _ => 3,
                };
                let index = if sequence == 0 {
                    if let Some(index) = first_index {
                        before(cutoff)?;
                        let packet = channel.packets.get(index).ok_or_else(|| {
                            io::Error::other("retained archive header index absent")
                        })?;
                        inspect(packet)?;
                        packet.exact(rights, peer)?;
                        let value: Value = serde_json::from_slice(&packet.bytes)?;
                        require(
                            packet.bytes.len() <= wire::MAX_PACKET
                                && journal::canonical(&value)? == packet.bytes,
                            "retained archive header is noncanonical or exceeds original bound",
                        )?;
                        before(cutoff)?;
                        index
                    } else {
                        receive_progress(
                            channel,
                            peer,
                            rights,
                            wire::MAX_PACKET,
                            cutoff,
                            &mut progress,
                            &mut inspect,
                        )?
                    }
                } else {
                    receive_progress(
                        channel,
                        peer,
                        rights,
                        wire::MAX_PACKET,
                        cutoff,
                        &mut progress,
                        &mut inspect,
                    )?
                };
                let packet = &mut channel.packets[index];
                // Move first, then parse; a malformed frame remains in this owner.
                let bytes = packet.bytes.clone();
                let rights = std::mem::take(&mut packet.rights);
                self.frames.push((Value::Null, bytes, rights));
                let frame = self.frames.last_mut().unwrap();
                frame.0 = serde_json::from_slice(&frame.1)?;
            }
            self.verify(intent, cutoff)
        })();
        self.remember(result)
    }
    pub(super) fn verify(&mut self, intent: &Intent, cutoff: u64) -> io::Result<()> {
        before(cutoff)?;
        require(
            self.refused.is_none()
                && !self.frame_alias_retirement_attempted
                && self.frames.len() == 16,
            "runtime archive incomplete, retired or refused",
        )?;
        let header = self.frames[0].0.clone();
        require(
            header["schema"] == "hermit-grouped-runtime-source-header-v1"
                && header["nonce"] == intent.nonce
                && header["incarnation"] == intent.incarnation
                && header["stage_deadline"] == cutoff,
            "runtime source header changed original identity",
        )?;
        let guardian = &header["guardian"];
        let keeper = &header["keeper"];
        for record in [guardian, keeper] {
            require(
                record["schema"] == "hermit-grouped-completed-holder-v1"
                    && record["nonce"] == intent.nonce
                    && record["incarnation"] == intent.incarnation
                    && record["stage_deadline"] == cutoff
                    && record["next"] == 35
                    && record["create_mask"] == 0x1ffffu32
                    && record["pairs"]
                        .as_array()
                        .is_some_and(|pairs| pairs.len() == 17),
                "runtime archive lacks original complete seventeen pairs",
            )?;
        }
        require(
            guardian["role"] == "guardian"
                && keeper["role"] == "keeper"
                && guardian["creator"] == keeper["creator"]
                && guardian["pairs"] == keeper["pairs"],
            "runtime original holders disagree",
        )?;
        for (sequence, role) in [
            (1, "source_creator"),
            (2, "controls"),
            (3, "guardian_store"),
            (4, "keeper_store"),
            (5, "keeper_creator"),
            (6, "keeper_controls"),
        ] {
            require(
                self.frames[sequence].0
                    == json!({"schema":"hermit-grouped-runtime-source-rights-v1",
                "sequence":sequence,"role":role}),
                "runtime archive role changed",
            )?;
        }
        check_creator(&self.frames[1].2, &guardian["creator"], true)?;
        check_creator(&self.frames[5].2, &keeper["creator"], true)?;
        owner::verify_description(self.frames[1].2[0].as_raw_fd(), &guardian["source_pidfd"])?;
        owner::verify_description(self.frames[5].2[0].as_raw_fd(), &keeper["source_pidfd"])?;
        require(
            owner::stat(self.frames[1].2[0].as_raw_fd())?
                .same_object(&owner::stat(self.frames[5].2[0].as_raw_fd())?),
            "runtime archives hold different source pidfs objects",
        )?;
        check_controls(&self.frames[2].2)?;
        check_controls(&self.frames[6].2)?;
        for i in 0..3 {
            same_ofd(self.frames[2].2[i].as_fd(), self.frames[6].2[i].as_fd())?;
        }
        for i in 7..15 {
            let frame = &self.frames[i];
            let holder = if i < 11 { "guardian" } else { "keeper" };
            let query = if i < 11 { i - 7 } else { i - 11 };
            require(
                frame.0["schema"] == "hermit-grouped-runtime-source-query-v1"
                    && frame.0["sequence"] == i
                    && frame.0["holder"] == holder
                    && frame.0["query"] == query,
                "runtime query role changed",
            )?;
            verify_query(&frame.0["record"], &frame.2)?;
        }
        let mut hash = Sha256::new();
        let mut count = 0;
        for (_, bytes, _) in &self.frames[..15] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            count += bytes.len();
        }
        require(
            count <= 1_048_576
                && self.frames[15].0
                    == json!({
            "schema":"hermit-grouped-runtime-source-end-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"stage_deadline":cutoff,"offer":header["offer"],
            "transcript_sha256":header["transcript_sha256"],"preceding_frames":15,
            "preceding_bytes":count,"framed_sha256":hex(&hash.finalize())}),
            "runtime archive transcript differs",
        )?;
        require(
            self.readers.is_empty(),
            "runtime Store validation cannot repeat",
        )?;
        for (index, commitment) in [
            (3, header["guardian_store"].clone()),
            (4, header["keeper_store"].clone()),
        ] {
            let rights = std::mem::take(&mut self.frames[index].2);
            self.readers
                .push(journal::SourceLedgerReader::retain(rights, intent.clone()));
            let reader = self.readers.last_mut().unwrap();
            reader.initialize()?;
            self.histories.push(reader.read(&commitment)?);
        }
        require(
            self.histories[0].frames.len() == 34
                && self.histories[1].frames.len() == 34
                && self.histories[0].frames == self.histories[1].frames,
            "runtime Store callback histories differ or are incomplete",
        )?;
        for (i, pair) in self.histories[0]
            .frames
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
        {
            require(
                pair[1].write.raw == pair[1].write.submitted as i64
                    && pair[1].write.error == 0
                    && guardian["pairs"][i]
                        == json!({"intent_owner":pair[0].owner,
                    "outcome_owner":pair[1].owner,"intent":pair[0].write,
                    "outcome":pair[1].write,"line":pair[0].line}),
                "runtime native source outcome differs from original complete pair",
            )?;
        }
        before(cutoff)?;
        self.verified = true;
        Ok(())
    }
    pub(super) fn controls(&self) -> io::Result<[BorrowedFd<'_>; 3]> {
        require(
            self.verified && !self.frame_alias_retirement_attempted && self.refused.is_none(),
            "runtime controls precede complete source custody or follow retirement",
        )?;
        check_controls(&self.frames[2].2)?;
        Ok([
            self.frames[2].2[0].as_fd(),
            self.frames[2].2[1].as_fd(),
            self.frames[2].2[2].as_fd(),
        ])
    }
    pub(super) fn forward(&self, peer: &mut wire::Channel, cutoff: u64) -> io::Result<()> {
        require(
            self.verified && !self.frame_alias_retirement_attempted && self.refused.is_none(),
            "unverified or retired source cannot transfer",
        )?;
        for (index, (value, _, frame_rights)) in self.frames.iter().enumerate() {
            let rights = match index {
                3 => self.readers[0].held_rights(),
                4 => self.readers[1].held_rights(),
                _ => frame_rights,
            };
            let fds: Vec<_> = rights.iter().map(AsFd::as_fd).collect();
            send(peer, value, &fds, cutoff)?;
        }
        Ok(())
    }
    /// Retire extra SCM aliases only after the other actual holder acknowledged
    /// this complete archive. The original early controls/Stores stay held by
    /// RuntimeCleanup; these closes are local alias receipts, never deletion.
    fn retire_frame_aliases(&mut self, cutoff: u64) -> io::Result<()> {
        require(
            self.verified && !self.frame_alias_retirement_attempted && self.refused.is_none(),
            "archive frame alias retirement is one-use after verification",
        )?;
        self.frame_alias_retirement_attempted = true;
        let result = (|| {
            for frame in &mut self.frames {
                while !frame.2.is_empty() {
                    before(cutoff)?;
                    let fd = frame.2.pop().unwrap().into_raw_fd();
                    self.frame_alias_closes
                        .push(json!({"fd":fd,"attempted":true,"raw":null,"errno":null}));
                    let raw = unsafe { libc::close(fd) };
                    let error = (raw == -1).then(io::Error::last_os_error);
                    *self.frame_alias_closes.last_mut().unwrap() = json!({"fd":fd,"attempted":true,"raw":raw,
                        "errno":error.as_ref().and_then(io::Error::raw_os_error)});
                    if let Some(error) = error {
                        return Err(error);
                    }
                    require(raw == 0, "archive alias close returned unexpected value")?;
                }
            }
            before(cutoff)
        })();
        self.remember(result)
    }
    /// Descriptive borrowed FD census for explicit local retirement. No
    /// wait, release or native close is performed by this accessor.
    pub(super) fn held_descriptors(&self) -> Vec<i32> {
        self.frames
            .iter()
            .flat_map(|frame| frame.2.iter().map(AsRawFd::as_raw_fd))
            .chain(
                self.readers
                    .iter()
                    .flat_map(|reader| reader.held_rights().iter().map(AsRawFd::as_raw_fd)),
            )
            .collect()
    }
    pub(super) fn acknowledgement(&self, schema: &str) -> io::Result<Value> {
        require(
            self.verified && self.refused.is_none(),
            "unverified archive cannot acknowledge",
        )?;
        let mut value = self.frames[15].0.clone();
        value["schema"] = json!(schema);
        Ok(value)
    }
}

pub(super) fn check_creator(fds: &[OwnedFd], record: &Value, terminal: bool) -> io::Result<()> {
    require(
        fds.len() == 2
            && record["creator_pidfd_held"] == true
            && record["cgroup_directory_held"] == true,
        "runtime source native custody absent",
    )?;
    let pid = record["pid"]
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| io::Error::other("runtime source PID malformed"))?;
    let held = owner::stat(fds[1].as_raw_fd())?;
    require(
        held.mode & libc::S_IFMT == libc::S_IFDIR
            && owner::filesystem(fds[1].as_raw_fd())? == 0x6367_7270
            && record["device"] == held.device
            && record["inode"] == held.inode,
        "runtime source held cgroup identity changed",
    )?;
    if terminal {
        let observed = owner::read_retained_cgroup(fds[0].as_fd(), fds[1].as_fd(), &held)?;
        require(
            matches!(observed,owner::CgroupReadbackProgress::Observed(ref p)
            if p.creator_terminal&&p.unlinked),
            "runtime source is not terminal and unlinked",
        )
    } else {
        owner::pidfd_matches(fds[0].as_raw_fd(), pid)?;
        require(
            !owner::terminal(fds[0].as_raw_fd())?,
            "runtime source already terminal before custody",
        )?;
        let group = record["cgroup"]
            .as_str()
            .ok_or_else(|| io::Error::other("runtime source cgroup absent"))?;
        require(
            owner::read_file(&format!("/proc/{pid}/cgroup"), 4096)? == format!("0::{group}\n"),
            "runtime source membership changed",
        )
    }
}
pub(super) fn check_controls(fds: &[OwnedFd]) -> io::Result<()> {
    owner::protected_holder()?;
    require(fds.len() == 3, "runtime requires three original controls")?;
    for (index, fd) in fds.iter().enumerate() {
        let identity = owner::stat(fd.as_raw_fd())?;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        require(
            owner::filesystem(fd.as_raw_fd())? == 0x7472_6163
                && identity.uid == 0
                && identity.gid == 0
                && identity.mode & libc::S_IFMT
                    == (if index == 2 {
                        libc::S_IFDIR
                    } else {
                        libc::S_IFREG
                    })
                && flags >= 0
                && flags & libc::O_ACCMODE
                    == (if index == 0 {
                        libc::O_RDWR
                    } else {
                        libc::O_RDONLY
                    })
                && flags & !(libc::O_ACCMODE | 0o100000 | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                    == 0
                && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
            "runtime controls differ from original bounded root-owned tracefs roles",
        )?;
    }
    for i in 0..3 {
        for j in i..3 {
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_kcmp,
                    libc::getpid(),
                    libc::getpid(),
                    0,
                    fds[i].as_raw_fd(),
                    fds[j].as_raw_fd(),
                )
            };
            require(
                if i == j {
                    raw == 0
                } else {
                    (1..=3).contains(&raw)
                },
                "runtime control roles alias or compare failed",
            )?;
        }
    }
    Ok(())
}
fn decode_hex(text: &str, cap: usize) -> io::Result<Vec<u8>> {
    require(
        text.len().is_multiple_of(2)
            && text.len() / 2 <= cap
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "runtime bounded hex differs",
    )?;
    text.as_bytes()
        .as_chunks::<2>().0.iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).map_err(io::Error::other)
        })
        .collect()
}
fn verify_query(record: &Value, rights: &[OwnedFd]) -> io::Result<()> {
    require(
        rights.len() == 3 && owner::terminal(rights[0].as_raw_fd())?,
        "runtime query pidfd absent or live",
    )?;
    let descriptions = record["original_descriptions"]
        .as_array()
        .ok_or_else(|| io::Error::other("runtime query descriptions absent"))?;
    require(
        descriptions.len() == 3,
        "runtime query description count differs",
    )?;
    for (fd, description) in rights.iter().zip(descriptions) {
        owner::verify_description(fd.as_raw_fd(), description)?;
    }
    require(
        record["eof"] == json!([true, true])
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
                .is_some_and(|n| (2..=i32::MAX as u64).contains(&n)),
        "runtime query archive was not successful native custody",
    )?;
    decode_hex(
        record["stdout"]
            .as_str()
            .ok_or_else(|| io::Error::other("runtime query stdout absent"))?,
        1_048_576,
    )?;
    for fd in &rights[1..] {
        require(
            owner::stat(fd.as_raw_fd())?.mode & libc::S_IFMT == libc::S_IFIFO,
            "runtime query EOF description is not pipe",
        )?;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        require(
            flags >= 0 && flags & libc::O_NONBLOCK != 0,
            "runtime query pipe is blocking",
        )?;
        let mut byte = 0u8;
        require(
            unsafe { libc::read(fd.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) } == 0,
            "runtime query output is not actual EOF",
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cursor {
    Uninstalled,
    Parent,
    GrantSubmitted,
    Peer,
    Unknown,
    Closed,
}
#[derive(Debug)]
pub(super) struct RuntimeCallbacks {
    pub(super) intent: Intent,
    pub(super) ledger: Option<journal::RemovalJournal>,
    peer: Option<wire::Channel>,
    peer_pin: Option<OwnedFd>,
    peer_identity: Option<wire::Credentials>,
    cutoff: Option<u64>,
    origin: Option<u64>,
    cursor: Cursor,
    sequence: u64,
    in_callback: bool,
    first_failure: Option<Failure>,
    first_failure_origin: Option<u64>,
}
impl RuntimeCallbacks {
    pub(super) fn retain(intent: Intent) -> Self {
        Self {
            intent,
            ledger: None,
            peer: None,
            peer_pin: None,
            peer_identity: None,
            cutoff: None,
            origin: None,
            cursor: Cursor::Uninstalled,
            sequence: 0,
            in_callback: false,
            first_failure: None,
            first_failure_origin: None,
        }
    }
    fn latch(&mut self, error: &io::Error) {
        if self.first_failure.is_none() {
            self.first_failure = Some(Failure::capture(error));
            self.first_failure_origin = guardian::monotonic_ns().ok();
        }
    }
    fn peer_live(&self) -> io::Result<()> {
        let pin = self
            .peer_pin
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime peer pidfd absent"))?;
        let identity = self
            .peer_identity
            .ok_or_else(|| io::Error::other("runtime peer credentials absent"))?;
        owner::pidfd_matches(pin.as_raw_fd(), identity.pid)?;
        require(
            !owner::terminal(pin.as_raw_fd())?,
            "runtime peer is terminal",
        )
    }
    fn check(&self) -> io::Result<u64> {
        if let Some(error) = &self.first_failure {
            return Err(error.error());
        }
        let cutoff = self
            .cutoff
            .ok_or_else(|| io::Error::other("runtime original terminal cut absent"))?;
        before(cutoff)?;
        self.peer_live()?;
        Ok(cutoff)
    }
    fn remove(
        &mut self,
        native: &ffi::Owner,
        write: &ffi::Write,
        line: *const libc::c_char,
    ) -> io::Result<()> {
        let cutoff = self.check()?;
        require(
            self.cursor == Cursor::Parent && !line.is_null() && write.submitted < 256,
            "runtime removal lacks its original exclusive cursor or line",
        )?;
        let bytes = unsafe { std::slice::from_raw_parts(line.cast::<u8>(), write.submitted) };
        let owner = rust_owner(native)?;
        let write = rust_write(write)?;
        let ledger = self
            .ledger
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime local removal Store absent"))?;
        let digest = ledger.append(owner.clone(), write.clone(), bytes)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("runtime removal sequence overflow"))?;
        let sequence = self.sequence;
        let request = json!({"schema":"hermit-cleanup-remove-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":sequence,"owner":owner,
            "write":write,"line":hex(bytes),"cutoff":cutoff});
        let peer = self
            .peer
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime peer channel absent"))?;
        send(peer, &request, &[], cutoff)?;
        let response = receive_value(peer, self.peer_identity.unwrap(), cutoff)?;
        require(
            response
                == json!({"schema":"hermit-cleanup-remove-ack-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":sequence,"cutoff":cutoff}),
            "runtime removal ACK changed original request",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"dual-removal-ack",
            "sequence":sequence,"digest":hex(&digest),"value":response}))?;
        self.check()?;
        Ok(())
    }
}
pub(super) unsafe extern "C" fn runtime_journal(
    context: *mut libc::c_void,
    native: *const ffi::Owner,
    write: *const ffi::Write,
    line: *const libc::c_char,
) -> libc::c_int {
    if context.is_null() || native.is_null() || write.is_null() {
        unsafe {
            *libc::__errno_location() = libc::EINVAL;
        }
        return -1;
    }
    // The sole caller is the synchronous authenticated C Bridge. Context is a
    // stable Box retained by RuntimeCleanup even on refusal; no concurrent call
    // or Rust reference into it remains active during the FFI operation.
    let state = unsafe { &mut *context.cast::<RuntimeCallbacks>() };
    if state.in_callback {
        unsafe {
            *libc::__errno_location() = libc::EDEADLK;
        }
        return -1;
    }
    state.in_callback = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.remove(unsafe { &*native }, unsafe { &*write }, line)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("runtime removal callback panicked")));
    state.in_callback = false;
    match result {
        Ok(()) => 0,
        Err(error) => {
            state.latch(&error);
            unsafe {
                *libc::__errno_location() = error.raw_os_error().unwrap_or(libc::EPROTO);
            }
            -1
        }
    }
}
pub(super) fn rust_owner(value: &ffi::Owner) -> io::Result<journal::OwnerSnapshot> {
    fn name(bytes: &[libc::c_char]) -> io::Result<String> {
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("native name lacks NUL"))?;
        require(
            bytes[end..].iter().all(|b| *b == 0),
            "native name has trailing bytes",
        )?;
        String::from_utf8(bytes[..end].iter().map(|b| *b as u8).collect()).map_err(io::Error::other)
    }
    Ok(journal::OwnerSnapshot {
        incarnation: value.incarnation,
        phase: value.phase,
        verified_sites: value.verified_sites,
        attempted_sites: value.attempted_sites,
        event_id: value.event_id,
        write_unknown: value.write_unknown,
        pending_role: value.pending_role,
        pending_remove: value.pending_remove,
        pending_bytes: value.pending_bytes as u64,
        group: name(&value.group)?,
        event: name(&value.event)?,
    })
}
pub(super) fn rust_write(value: &ffi::Write) -> io::Result<journal::Write> {
    require(
        value.error >= 0 && value.started >= 0 && value.completed >= 0,
        "native write unsigned conversion differs",
    )?;
    Ok(journal::Write {
        role: value.role,
        remove: value.remove,
        submitted: value.submitted as u64,
        raw: value.raw as i64,
        error: value.error as u32,
        started: value.started as u32,
        completed: value.completed as u32,
    })
}

/// Actual outcome of the failed pre-open branch. Retained runtime custody is
/// intentionally distinct from completed independent creation cleanup.
#[derive(Debug)]
pub(in crate::network_runtime) enum PreOpenRecovery {
    CreationPeerCompleted,
    RuntimeTerminalRetained,
}
/// The service's initial S2 announcement to the Keeper. The later native
/// Guardian announcement is a separate, unchanged authentication step.
#[derive(Debug, Default)]
struct SuccessorCreator {
    attempted: bool,
    pidfd: Option<OwnedFd>,
    directory: Option<OwnedFd>,
    packet: Option<Vec<u8>>,
    send_attempted: bool,
    exec_packet: Option<usize>,
    accepted: bool,
}
impl SuccessorCreator {
    fn held_descriptors(&self) -> impl Iterator<Item = i32> + '_ {
        self.pidfd
            .iter()
            .chain(self.directory.iter())
            .map(AsRawFd::as_raw_fd)
    }
}
/// The service installs this owner before any bootstrap import or native open.
/// Its constructor grants nothing. No Drop performs close, native deletion, or
/// terminal success; the dedicated accepted process retains it until _exit.
#[derive(Debug)]
#[must_use = "runtime owner must remain retained through all partial failures"]
pub(in crate::network_runtime) struct RuntimeCleanup {
    bootstrap: wire::Channel,
    run: [u8; 16],
    startup_cutoff: Option<u64>,
    bootstrap_peer: Option<wire::Credentials>,
    controller: Option<OwnedFd>,
    source: Option<wire::Channel>,
    source_peer: Option<wire::Credentials>,
    source_pin: Option<OwnedFd>,
    coordinator: Option<wire::Channel>,
    coordinator_peer: Option<wire::Credentials>,
    coordinator_pin: Option<OwnedFd>,
    callbacks: Box<RuntimeCallbacks>,
    early: EarlyCreation,
    archive: SourceArchive,
    leaves: Vec<OwnedFd>,
    unit: Option<CString>,
    creator_cutoff: Option<u64>,
    successor_creator: SuccessorCreator,
    install_attempted: bool,
    runtime_prepared: bool,
    commit_attempted: bool,
    commit_acknowledged: bool,
    pre_open_recovery_attempted: bool,
    pre_open_aliases: Vec<OwnedFd>,
    pending_creation_agreement: Option<Value>,
    keeper_archive_ack_seen: bool,
    pre_open_shutdown: Option<Value>,
    s2_guardian: Option<OwnedFd>,
    s2_guardian_record: Option<Value>,
    installed: bool,
    terminal_attempted: bool,
    finish_attempted: bool,
    terminal_complete: bool,
    observations: Vec<AbsenceObservation>,
    retained_join: Option<Value>,
    source_failure_notice_attempted: bool,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
}
impl RuntimeCleanup {
    pub(in crate::network_runtime) fn retain_bootstrap(endpoint: OwnedFd, run: [u8; 16]) -> Self {
        let intent = Intent {
            nonce: hex(&run),
            incarnation: u64::from_le_bytes(run[..8].try_into().unwrap()),
        };
        Self {
            bootstrap: wire::Channel::retain(endpoint),
            run,
            startup_cutoff: None,
            bootstrap_peer: None,
            controller: None,
            source: None,
            source_peer: None,
            source_pin: None,
            coordinator: None,
            coordinator_peer: None,
            coordinator_pin: None,
            callbacks: Box::new(RuntimeCallbacks::retain(intent)),
            early: EarlyCreation::retain(),
            archive: SourceArchive::retain(),
            leaves: Vec::new(),
            unit: None,
            creator_cutoff: None,
            successor_creator: SuccessorCreator::default(),
            install_attempted: false,
            runtime_prepared: false,
            commit_attempted: false,
            commit_acknowledged: false,
            pre_open_recovery_attempted: false,
            pre_open_aliases: Vec::new(),
            pending_creation_agreement: None,
            keeper_archive_ack_seen: false,
            pre_open_shutdown: None,
            s2_guardian: None,
            s2_guardian_record: None,
            installed: false,
            terminal_attempted: false,
            finish_attempted: false,
            terminal_complete: false,
            observations: Vec::new(),
            retained_join: None,
            source_failure_notice_attempted: false,
            refusal: None,
            failure_origin: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && self.refusal.is_none()
        {
                self.refusal = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        result
    }
    /// Receive the actual CLI-created channels/pins and local Store. Every
    /// original request right remains owned by the AcceptedSession too.
    pub(in crate::network_runtime) fn initialize_bootstrap(
        &mut self,
        controller: BorrowedFd<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                self.startup_cutoff.is_none() && self.controller.is_none(),
                "runtime bootstrap cannot repeat",
            )?;
            self.controller = Some(duplicate(controller)?);
            self.bootstrap.validate()?;
            let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    self.bootstrap.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let expected = credentials(peer.pid)?;
            require(
                size as usize == std::mem::size_of_val(&peer)
                    && peer.uid == expected.uid
                    && peer.gid == expected.gid,
                "runtime bootstrap original CLI credentials differ",
            )?;
            self.bootstrap_peer = Some(expected);
            // Only the first config receive has this finite ceiling. Its
            // original earlier stage deadline then replaces it, never extends it.
            let ceiling = guardian::monotonic_ns()?
                .checked_add(20_000_000_000)
                .ok_or_else(|| io::Error::other("runtime ceiling overflow"))?;
            let index = receive(&mut self.bootstrap, expected, 1, 4096, ceiling)?;
            let header: Value = serde_json::from_slice(&self.bootstrap.packets[index].bytes)?;
            let cutoff = header["stage_deadline"]
                .as_u64()
                .ok_or_else(|| io::Error::other("runtime startup cutoff absent"))?;
            self.startup_cutoff = Some(cutoff);
            require(
                header["schema"] == "hermit-grouped-runtime-service-bootstrap-v1"
                    && header["run"] == hex(&self.run)
                    && header["incarnation"] == self.callbacks.intent.incarnation
                    && cutoff <= ceiling,
                "runtime bootstrap identity or original stage differs",
            )?;
            before(cutoff)?;
            let directory = self.bootstrap.packets[index].rights.pop().unwrap();
            self.callbacks.ledger = Some(journal::RemovalJournal::retain(
                directory,
                self.callbacks.intent.clone(),
            ));
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .initialize("runtime-guardian")?;
            self.unit = Some(
                CString::new(
                    header["unit"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("runtime provider unit absent"))?,
                )
                .map_err(io::Error::other)?,
            );
            for (sequence, role, count) in [(1, "source", 2), (2, "keeper", 2), (3, "parent", 2)] {
                let index = receive(&mut self.bootstrap, expected, count, 4096, cutoff)?;
                let value: Value = serde_json::from_slice(&self.bootstrap.packets[index].bytes)?;
                let pid = value["pid"]
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| io::Error::other("runtime link PID absent"))?;
                require(
                    value
                        == json!({"schema":"hermit-grouped-runtime-service-link-v1","run":hex(&self.run),
                    "stage_deadline":cutoff,"sequence":sequence,"role":role,"pid":pid}),
                    "runtime link fields differ",
                )?;
                let rights = &mut self.bootstrap.packets[index].rights;
                let endpoint = rights.pop().unwrap();
                let pin = rights.pop().unwrap();
                match role {
                    "source" => {
                        self.source = Some(wire::Channel::retain(endpoint));
                        self.source_pin = Some(pin);
                        self.source_peer = Some(credentials(pid)?);
                    }
                    "keeper" => {
                        self.callbacks.peer = Some(wire::Channel::retain(endpoint));
                        self.callbacks.peer_pin = Some(pin);
                        self.callbacks.peer_identity = Some(credentials(pid)?);
                    }
                    _ => {
                        self.coordinator = Some(wire::Channel::retain(endpoint));
                        self.coordinator_pin = Some(pin);
                        self.coordinator_peer = Some(credentials(pid)?);
                    }
                }
            }
            for (pin, peer) in [
                (self.source_pin.as_ref().unwrap(), self.source_peer.unwrap()),
                (
                    self.callbacks.peer_pin.as_ref().unwrap(),
                    self.callbacks.peer_identity.unwrap(),
                ),
                (
                    self.coordinator_pin.as_ref().unwrap(),
                    self.coordinator_peer.unwrap(),
                ),
            ] {
                owner::pidfd_matches(pin.as_raw_fd(), peer.pid)?;
                require(
                    !owner::terminal(pin.as_raw_fd())?,
                    "runtime bootstrap helper is already terminal",
                )?;
            }
            self.source.as_ref().unwrap().validate()?;
            self.callbacks.peer.as_ref().unwrap().validate()?;
            self.coordinator.as_ref().unwrap().validate()?;
            before(cutoff)
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn prepare_creation_peer(&mut self) -> io::Result<()> {
        let result = (|| {
            let cutoff = self
                .startup_cutoff
                .ok_or_else(|| io::Error::other("runtime not bootstrapped"))?;
            require(
                self.refusal.is_none() && !self.install_attempted && self.leaves.is_empty(),
                "early cleanup peer cannot be installed after adoption",
            )?;
            self.callbacks.peer_live()?;
            self.early.receive(
                self.callbacks.peer.as_mut().unwrap(),
                self.callbacks.peer_identity.unwrap(),
                &self.callbacks.intent,
                cutoff,
            )?;
            let mut ack = self.early.frames[4].0.clone();
            ack["schema"] = json!("hermit-grouped-runtime-service-creation-custody-v1");
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"actual-early-creation-custody",
                "creator":self.early.frames[0].0,"end":self.early.frames[4].0}))?;
            self.early.prefixes[0].durable = true;
            self.callbacks.peer_live()?;
            self.early.prefixes[0].ack_attempted = true;
            send(
                self.callbacks.peer.as_mut().unwrap(),
                &ack,
                &self.early.controls()?,
                cutoff,
            )?;
            self.early.prefixes[0].ack_completed = true;
            Ok(())
        })();
        self.remember(result)
    }
    fn retain_s2_guardian(&mut self, index: usize, cutoff: u64) -> io::Result<()> {
        require(
            self.s2_guardian.is_none() && !self.install_attempted,
            "S2 Guardian registration repeated or late",
        )?;
        let packet = &mut self.callbacks.peer.as_mut().unwrap().packets[index];
        packet.exact(1, self.callbacks.peer_identity.unwrap())?;
        self.s2_guardian = packet.rights.pop();
        let value: Value = serde_json::from_slice(&packet.bytes)?;
        self.s2_guardian_record = Some(value.clone());
        let pid = value["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("S2 Guardian PID absent"))?;
        require(
            value
                == json!({"schema":"hermit-grouped-runtime-s2-guardian-v1",
            "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
            "stage_deadline":cutoff,"pid":pid}),
            "S2 Guardian changed original registration",
        )?;
        owner::pidfd_matches(self.s2_guardian.as_ref().unwrap().as_raw_fd(), pid)?;
        require(
            !owner::terminal(self.s2_guardian.as_ref().unwrap().as_raw_fd())?,
            "S2 Guardian already terminal before controls",
        )?;
        self.callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-S2-Guardian-before-controls","value":value}))?;
        let mut ack = value;
        ack["schema"] = json!("hermit-grouped-runtime-s2-guardian-ack-v1");
        self.callbacks.peer_live()?;
        send(self.callbacks.peer.as_mut().unwrap(), &ack, &[], cutoff)
    }
    fn progress_creation_prefix(&mut self, value: Value, cutoff: u64) -> io::Result<()> {
        self.callbacks.peer_live()?;
        let receipt = self.early.prefix(&value, &self.callbacks.intent, cutoff)?;
        self.callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-held-dual-source-prefix","value":value}))?;
        self.early.prefixes[receipt].durable = true;
        let mut ack = value;
        ack["schema"] = json!("hermit-grouped-runtime-creation-prefix-ack-v1");
        self.callbacks.peer_live()?;
        self.early.prefixes[receipt].ack_attempted = true;
        send(self.callbacks.peer.as_mut().unwrap(), &ack, &[], cutoff)?;
        self.early.prefixes[receipt].ack_completed = true;
        self.early.acknowledged = self.early.prefixes[receipt].sequence;
        Ok(())
    }
    fn retain_source_failure(&mut self, error: &io::Error) -> io::Result<u64> {
        if self.refusal.is_none() {
            self.refusal = Some(Failure::capture(error));
            self.failure_origin = guardian::monotonic_ns().ok();
        }
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("original pre-adoption failure origin unknown"))?;
        let cutoff = self.startup_cutoff.unwrap().min(
            origin
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("original pre-adoption failure overflow"))?,
        );
        before(cutoff)?;
        if !self.source_failure_notice_attempted {
            self.source_failure_notice_attempted = true;
            self.callbacks.peer_live()?;
            send(
                self.callbacks.peer.as_mut().unwrap(),
                &json!({"schema":"hermit-grouped-runtime-creation-peer-refused-v1",
                "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                "stage_deadline":self.startup_cutoff.unwrap(),"first_failure_origin":origin,
                "cause":self.refusal.as_ref().unwrap().message}),
                &[],
                cutoff,
            )?;
        }
        Ok(cutoff)
    }
    fn run_creation_cleanup_peer(&mut self, value: Value) -> io::Result<()> {
        require(
            self.early.ready
                && !self.early.refused
                && self.early.agreement.is_none()
                && !self.commit_acknowledged
                && !self.installed
                && (!self.install_attempted || self.pre_open_recovery_attempted),
            "creation cleanup cannot follow provider commit or unretired pre-open custody",
        )?;
        // Close admission irreversibly before parsing a fallible cleanup
        // agreement. All inputs and the original failure remain owned here.
        self.early.refused = true;
        let origin = value["original_start"]
            .as_u64()
            .ok_or_else(|| io::Error::other("creation original failure origin absent"))?;
        let cutoff = value["cutoff"]
            .as_u64()
            .ok_or_else(|| io::Error::other("creation original failure cutoff absent"))?;
        let cause = value["cause"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| io::Error::other("creation original failure absent"))?
            .to_owned();
        let now = guardian::monotonic_ns()?;
        let stage = self.startup_cutoff.unwrap();
        require(
            origin <= now
                && now < cutoff
                && cutoff <= stage
                && cutoff
                    <= origin
                        .checked_add(1_000_000_000)
                        .ok_or_else(|| io::Error::other("creation original origin overflow"))?,
            "creation cleanup extends original stage or failure1s",
        )?;
        if let Some(earlier) = self.failure_origin {
            require(
                cutoff
                    <= earlier
                        .checked_add(1_000_000_000)
                        .ok_or_else(|| io::Error::other("earlier service failure overflow"))?,
                "creation cleanup extends earlier service failure",
            )?;
        }
        self.refusal
            .get_or_insert_with(|| Failure::capture(&io::Error::other(cause.clone())));
        self.failure_origin = Some(self.failure_origin.map_or(origin, |old| old.min(origin)));
        self.callbacks.origin = Some(origin);
        self.callbacks.cutoff = Some(cutoff);
        self.early
            .terminal(self.source_pin.as_ref().unwrap().as_fd())?;
        if let Some(pin) = &self.s2_guardian {
            require(
                owner::terminal(pin.as_raw_fd())?,
                "original S2 Guardian remains live during creation recovery",
            )?;
        }
        let sequence = value["sequence"]
            .as_u64()
            .ok_or_else(|| io::Error::other("creation agreement sequence absent"))?;
        let selected = self
            .early
            .prefixes
            .iter()
            .find(|p| p.sequence == sequence && p.validated && p.durable && p.ack_attempted)
            .ok_or_else(|| {
                io::Error::other("creation agreement has no validated durable service ACK attempt")
            })?;
        let expected_histories = &self.early.histories[selected.history];
        require(
            value["guardian_store"] == expected_histories[0].commitment()
                && value["keeper_store"] == expected_histories[1].commitment(),
            "creation agreement changed last durable original Store commitments",
        )?;
        for (index, expected) in expected_histories.iter().enumerate() {
            let actual =
                self.early.readers[index].read_acknowledged_prefix(expected)?;
            self.early.terminal_readbacks.push(actual);
        }
        let histories = &self.early.terminal_readbacks;
        let (eligible, relation) = if sequence == 0 {
            require(
                histories[0].history.frames.len() == 1
                    && histories[1].history.frames.is_empty()
                    && histories[0].history.frames[0].write.started == 0
                    && histories[0].history.frames[0].write.role == 1,
                "creation zero-write cleanup lacks actual withheld first intent",
            )?;
            (0, "guardian-first-intent-not-acknowledged-no-writes")
        } else {
            let (_, eligible, relation) = super::cleanup::select_runtime_creation_recovery(
                &self.callbacks.intent,
                &histories[0].history,
                &histories[1].history,
            )?;
            (eligible, relation)
        };
        require(
            value
                == json!({"schema":"hermit-grouped-runtime-creation-agreement-v1",
            "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,"stage_deadline":stage,
            "original_start":origin,"cutoff":cutoff,"cause":cause,"source":self.early.frames[0].0["creator"],
            "keeper_pid":self.early.frames[0].0["keeper_pid"],"guardian_store":histories[0].history.commitment(),
            "keeper_store":histories[1].history.commitment(),"sequence":sequence,"eligible":eligible,"relation":relation}),
            "creation cleanup agreement differs from actual local handles and histories",
        )?;
        let digest = hex(&Sha256::digest(journal::canonical(&value)?));
        self.early.agreement = Some(value.clone());
        self.early.agreement_sha = Some(digest.clone());
        self.early.eligible = Some(eligible);
        if eligible == 0 {
            self.early.observed_mask = Some(0);
        }
        self.callbacks.cursor = Cursor::Peer;
        for (index, readback) in self.early.terminal_readbacks.iter().enumerate() {
            for (offset, chunk) in readback.unacknowledged_tail.chunks(1024).enumerate() {
                before(cutoff)?;
                self.callbacks.ledger.as_mut().unwrap().store.append(json!({"kind":"unacknowledged-source-tail-bytes",
                    "source":index,"offset":offset*1024,"bytes":hex(chunk),"grants_write_or_history_authority":false}))?;
            }
            self.callbacks.ledger.as_mut().unwrap().store.append(json!({"kind":"actual-acknowledged-source-prefix-readback",
                "source":index,"acknowledged":readback.history.commitment(),
                "unacknowledged_tail":{"bytes":readback.unacknowledged_tail.len(),"sha256":hex(&Sha256::digest(&readback.unacknowledged_tail))},
                "tail_parsed_as_healthy":false}))?;
        }
        self.callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"original-pre-adoption-failure-agreement",
            "value":value,"agreement":digest,"local_source_and_both_writers_terminal":true}))?;
        let mut ack = value;
        ack["schema"] = json!("hermit-grouped-runtime-creation-agreement-ack-v1");
        self.callbacks.check()?;
        send(self.callbacks.peer.as_mut().unwrap(), &ack, &[], cutoff)?;
        loop {
            self.callbacks.check()?;
            let request = receive_value(
                self.callbacks.peer.as_mut().unwrap(),
                self.callbacks.peer_identity.unwrap(),
                cutoff,
            )?;
            match request["schema"].as_str() {
                Some("hermit-cleanup-read-grant-v1") => {
                    require(
                        eligible != 0
                            && !self.early.read_granted
                            && self.callbacks.cursor == Cursor::Peer
                            && request
                                == json!({"schema":"hermit-cleanup-read-grant-v1","nonce":self.callbacks.intent.nonce,
                            "incarnation":self.callbacks.intent.incarnation,"epoch":1,"agreement":digest,
                            "eligible":eligible,"cutoff":cutoff}),
                        "creation cleanup read grant differs or repeats",
                    )?;
                    self.early.read_granted = true;
                    self.callbacks.cursor = Cursor::Parent;
                    self.early
                        .snapshots
                        .push(AbsenceObservation::retain(cutoff));
                    self.early.controls()?;
                    let controls = [
                        self.early.frames[1].2[0].as_fd(),
                        self.early.frames[1].2[1].as_fd(),
                        self.early.frames[1].2[2].as_fd(),
                    ];
                    let callback = &*self.callbacks;
                    read_control_contents(
                        self.early.snapshots.last_mut().unwrap(),
                        controls,
                        cutoff,
                        || {
                            require(
                                callback.cursor == Cursor::Parent,
                                "creation cleanup read lost exclusive cursor",
                            )?;
                            callback.check().map(|_| ())
                        },
                    )?;
                    let actual = self.early.snapshots.last().unwrap();
                    let observed = owner::runtime_creation_observed_mask(
                        &self.callbacks.intent,
                        &actual.definitions,
                        &actual.profile,
                        eligible,
                    )?;
                    self.early.observed_mask = Some(observed);
                    actual.persist_contents(
                        &mut self.callbacks.ledger.as_mut().unwrap().store,
                        0,
                        "creation-recovery-read-bytes",
                    )?;
                    self.callbacks.check()?;
                    self.callbacks.cursor = Cursor::Peer;
                    self.early.read_relinquished = true;
                    send(
                        self.callbacks.peer.as_mut().unwrap(),
                        &json!({"schema":"hermit-cleanup-read-done-v1",
                        "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                        "epoch":1,"agreement":digest,"cutoff":cutoff,"observed_mask":observed}),
                        &[],
                        cutoff,
                    )?;
                }
                Some("hermit-cleanup-remove-v1") => {
                    let observed = self.early.observed_mask.ok_or_else(|| {
                        io::Error::other("creation present mask was never observed")
                    })?;
                    require(
                        self.early.read_relinquished
                            && self.callbacks.cursor == Cursor::Peer
                            && self.callbacks.sequence < (observed.count_ones() as u64) * 2,
                        "creation removal lacks acknowledged prefix or writer cursor",
                    )?;
                    let sequence = self.callbacks.sequence + 1;
                    let owner: journal::OwnerSnapshot =
                        serde_json::from_value(request["owner"].clone())?;
                    let write: journal::Write = serde_json::from_value(request["write"].clone())?;
                    let line = decode_hex(
                        request["line"]
                            .as_str()
                            .ok_or_else(|| io::Error::other("creation removal line absent"))?,
                        255,
                    )?;
                    let roles = (1u32..=17)
                        .rev()
                        .filter(|role| observed & (1 << (*role - 1)) != 0)
                        .collect::<Vec<_>>();
                    require(
                        request
                            == json!({"schema":"hermit-cleanup-remove-v1","nonce":self.callbacks.intent.nonce,
                        "incarnation":self.callbacks.intent.incarnation,"sequence":sequence,"owner":owner,"write":write,
                        "line":hex(&line),"cutoff":cutoff})
                            && Some(&write.role)
                                == roles.get((self.callbacks.sequence / 2) as usize),
                        "creation removal changed original reverse prefix or fields",
                    )?;
                    self.callbacks
                        .ledger
                        .as_mut()
                        .unwrap()
                        .append(owner, write, &line)?;
                    self.callbacks.sequence = sequence;
                    self.callbacks.check()?;
                    send(
                        self.callbacks.peer.as_mut().unwrap(),
                        &json!({
                        "schema":"hermit-cleanup-remove-ack-v1","nonce":self.callbacks.intent.nonce,
                        "incarnation":self.callbacks.intent.incarnation,"sequence":sequence,"cutoff":cutoff}),
                        &[],
                        cutoff,
                    )?;
                }
                Some("hermit-grouped-runtime-creation-absence-grant-v1") => {
                    let observed = self.early.observed_mask.ok_or_else(|| {
                        io::Error::other("creation present mask was never observed")
                    })?;
                    require(
                        !self.finish_attempted
                            && self.callbacks.cursor == Cursor::Peer
                            && self.callbacks.sequence == (observed.count_ones() as u64) * 2
                            && request
                                == json!({"schema":"hermit-grouped-runtime-creation-absence-grant-v1",
                            "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                            "original_start":origin,"cutoff":cutoff,"agreement":digest}),
                        "creation absence grant precedes complete removal or changes cutoff",
                    )?;
                    self.finish_attempted = true;
                    self.callbacks.ledger.as_mut().unwrap().complete()?;
                    self.callbacks.cursor = Cursor::Parent;
                    for index in 0..2 {
                        self.observations.push(AbsenceObservation::retain(cutoff));
                        let controls = self.early.controls()?;
                        let callbacks = &*self.callbacks;
                        observe_absent(
                            self.observations.last_mut().unwrap(),
                            controls,
                            &callbacks.intent,
                            cutoff,
                            || {
                                require(
                                    callbacks.cursor == Cursor::Parent,
                                    "creation absence lost exclusive cursor",
                                )?;
                                callbacks.check().map(|_| ())
                            },
                        )?;
                        self.observations
                            .last()
                            .unwrap()
                            .persist(&mut self.callbacks.ledger.as_mut().unwrap().store, index)?;
                    }
                    self.callbacks.check()?;
                    self.callbacks.cursor = Cursor::Peer;
                    send(
                        self.callbacks.peer.as_mut().unwrap(),
                        &json!({"schema":"hermit-grouped-runtime-creation-absence-done-v1",
                        "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                        "original_start":origin,"cutoff":cutoff,"agreement":digest,
                        "observations":self.observations.iter().map(AbsenceObservation::receipt).collect::<Vec<_>>()}),
                        &[],
                        cutoff,
                    )?;
                }
                Some("hermit-grouped-runtime-creation-retire-v1") => {
                    require(
                        self.finish_attempted
                            && self.observations.len() == 2
                            && !self.early.retired
                            && self.callbacks.cursor == Cursor::Peer
                            && request
                                == json!({"schema":"hermit-grouped-runtime-creation-retire-v1",
                            "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                            "original_start":origin,"cutoff":cutoff,"agreement":digest}),
                        "creation alias retirement differs or repeats",
                    )?;
                    self.callbacks.ledger.as_mut().unwrap().complete()?;
                    self.early.retired = true;
                    let mut fds = self
                        .early
                        .frames
                        .iter()
                        .flat_map(|frame| frame.2.iter().map(AsRawFd::as_raw_fd))
                        .chain(
                            self.early
                                .readers
                                .iter()
                                .flat_map(|r| r.held_rights().iter().map(AsRawFd::as_raw_fd)),
                        )
                        .chain(self.archive.held_descriptors())
                        .chain(self.leaves.iter().map(AsRawFd::as_raw_fd))
                        .chain(self.pre_open_aliases.iter().map(AsRawFd::as_raw_fd))
                        .chain(self.successor_creator.held_descriptors())
                        .chain(
                            self.source
                                .iter()
                                .flat_map(|channel| channel.packets.iter())
                                .flat_map(|packet| packet.rights.iter().map(AsRawFd::as_raw_fd)),
                        )
                        .chain(
                            self.callbacks
                                .peer
                                .iter()
                                .flat_map(|channel| channel.packets.iter())
                                .flat_map(|packet| packet.rights.iter().map(AsRawFd::as_raw_fd)),
                        )
                        .chain(self.s2_guardian.iter().map(AsRawFd::as_raw_fd))
                        .collect::<Vec<_>>();
                    fds.sort_unstable();
                    require(
                        fds.windows(2).all(|p| p[0] != p[1]),
                        "creation alias census duplicated descriptor",
                    )?;
                    // This dedicated service is retained in ManuallyDrop until
                    // _exit. After this irreversible phase no closed owner is
                    // read, reused, implicitly dropped or offered to Provider.
                    for fd in fds {
                        self.callbacks.check()?;
                        self.early
                            .closes
                            .push(json!({"fd":fd,"attempted":true,"raw":null,"errno":null}));
                        let raw = unsafe { libc::close(fd) };
                        let error = (raw == -1).then(io::Error::last_os_error);
                        *self.early.closes.last_mut().unwrap() = json!({"fd":fd,"attempted":true,"raw":raw,
                            "errno":error.as_ref().and_then(io::Error::raw_os_error)});
                        if let Some(error) = error {
                            return Err(error);
                        }
                        require(raw == 0, "creation alias close returned unexpected value")?;
                    }
                    self.callbacks
                        .ledger
                        .as_mut()
                        .unwrap()
                        .store
                        .append(json!({"kind":"creation-service-aliases-retired",
                        "closes":self.early.closes,"original_cause":cause,"cutoff":cutoff}))?;
                    self.callbacks.check()?;
                    self.callbacks.cursor = Cursor::Closed;
                    send(
                        self.callbacks.peer.as_mut().unwrap(),
                        &json!({"schema":"hermit-grouped-runtime-creation-retired-v1",
                        "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
                        "original_start":origin,"cutoff":cutoff,"agreement":digest,"aliases_retired":true}),
                        &[],
                        cutoff,
                    )?;
                    self.terminal_complete = true;
                    // Successful cleanup never changes the failed bootstrap.
                    return Err(self.refusal.as_ref().unwrap().error());
                }
                _ => {
                    return Err(io::Error::other(
                        "unexpected creation cleanup phase message",
                    ));
                }
            }
        }
    }
    pub(in crate::network_runtime) fn receive_leaves(&mut self) -> io::Result<()> {
        let result = (|| {
            let cutoff = self
                .startup_cutoff
                .ok_or_else(|| io::Error::other("runtime not bootstrapped"))?;
            require(
                self.early.ready && self.leaves.is_empty() && self.refusal.is_none(),
                "runtime leaves cannot repeat after refusal or precede cleanup custody",
            )?;
            // Never block solely on S2: the independent writer needs this real
            // live peer if creation fails before any provider adoption.
            let mut wait_cutoff = cutoff;
            let index = loop {
                before(wait_cutoff)?;
                self.callbacks.peer_live()?;
                if let Some(index) = self.callbacks.peer.as_mut().unwrap().receive(4096)? {
                    let packet = &self.callbacks.peer.as_ref().unwrap().packets[index];
                    let value: Value = serde_json::from_slice(&packet.bytes)?;
                    require(
                        journal::canonical(&value)? == packet.bytes,
                        "creation peer message is not canonical",
                    )?;
                    packet.exact(
                        if value["schema"] == "hermit-grouped-runtime-s2-guardian-v1" {
                            1
                        } else {
                            0
                        },
                        self.callbacks.peer_identity.unwrap(),
                    )?;
                    match value["schema"].as_str() {
                        Some("hermit-grouped-runtime-s2-guardian-v1") => {
                            if let Err(error) = self.retain_s2_guardian(index, cutoff) {
                                wait_cutoff = self.retain_source_failure(&error)?;
                            }
                        }
                        Some("hermit-grouped-runtime-creation-prefix-v1") => {
                            if self.refusal.is_none()
                                && let Err(error) = self.progress_creation_prefix(value, cutoff) {
                                    wait_cutoff = self.retain_source_failure(&error)?;
                                }
                        }
                        Some("hermit-grouped-runtime-creation-agreement-v1") => {
                            return self.run_creation_cleanup_peer(value);
                        }
                        _ => {
                            return Err(io::Error::other(
                                "unexpected pre-adoption cleanup message",
                            ));
                        }
                    }
                }
                let source = if self.refusal.is_none() {
                    match self.source.as_mut().unwrap().receive(wire::MAX_PACKET) {
                        Ok(value) => value,
                        Err(error) => {
                            wait_cutoff = self.retain_source_failure(&error)?;
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some(index) = source {
                    let packet = &self.source.as_ref().unwrap().packets[index];
                    packet.exact(3, self.source_peer.unwrap())?;
                    let value: Value = serde_json::from_slice(&packet.bytes)?;
                    require(
                        journal::canonical(&value)? == packet.bytes,
                        "source leaf message is not canonical",
                    )?;
                    require(
                        self.early.acknowledged == 34,
                        "S2 leaves preceded all original held callback ACKs",
                    )?;
                    break index;
                }
                if self.refusal.is_none()
                    && owner::terminal(self.source_pin.as_ref().unwrap().as_raw_fd())?
                {
                    wait_cutoff = self.retain_source_failure(&io::Error::other(
                        "original startup controller became terminal before S2 leaves",
                    ))?;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            let packet = &mut self.source.as_mut().unwrap().packets[index];
            self.leaves = std::mem::take(&mut packet.rights);
            let value: Value = serde_json::from_slice(&packet.bytes)?;
            require(
                value["schema"] == "hermit-grouped-successor-leaves-v1"
                    && value["nonce"] == self.callbacks.intent.nonce
                    && value["incarnation"] == self.callbacks.intent.incarnation
                    && value["stage_deadline"] == cutoff
                    && value["roles"] == json!(["ID", "FORMAT", "ENABLE"]),
                "runtime leaf delivery fields differ",
            )?;
            let descriptions = value["descriptions"]
                .as_array()
                .ok_or_else(|| io::Error::other("runtime leaf descriptions absent"))?;
            require(
                descriptions.len() == 3,
                "runtime leaf description count differs",
            )?;
            for (fd, description) in self.leaves.iter().zip(descriptions) {
                owner::verify_description(fd.as_raw_fd(), description)?;
            }
            let creator_cutoff = value["creator_cutoff"]
                .as_u64()
                .ok_or_else(|| io::Error::other("original S2 creator cutoff absent"))?;
            let now = guardian::monotonic_ns()?;
            require(
                now < creator_cutoff
                    && creator_cutoff <= cutoff
                    && creator_cutoff - now <= 1_000_000_000,
                "runtime S2 cut differs from original bounded controller transition",
            )?;
            self.creator_cutoff = Some(creator_cutoff);
            let ack = json!({"schema":"hermit-grouped-successor-leaves-ack-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"stage_deadline":cutoff,
                "creator_cutoff":creator_cutoff,"descriptions":descriptions});
            send(self.source.as_mut().unwrap(), &ack, &[], creator_cutoff)?;
            Ok(())
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn source_endpoint(&self) -> io::Result<BorrowedFd<'_>> {
        Ok(self
            .source
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime source endpoint absent"))?
            .fd
            .as_fd())
    }
    fn check_successor_creator(&self) -> io::Result<u64> {
        let cutoff = self
            .creator_cutoff
            .ok_or_else(|| io::Error::other("runtime original S2 creator cutoff absent"))?;
        require(
            self.early.ready
                && self.early.acknowledged == 34
                && self.leaves.len() == 3
                && self.startup_cutoff.is_some_and(|stage| cutoff <= stage),
            "runtime initial Creator lacks original S2 admission",
        )?;
        before(cutoff)?;
        let peer = self
            .source_peer
            .ok_or_else(|| io::Error::other("runtime original S2 Keeper credentials absent"))?;
        let pin = self
            .source_pin
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime original S2 Keeper pidfd absent"))?;
        owner::pidfd_matches(pin.as_raw_fd(), peer.pid)?;
        self.source
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime original S2 channel absent"))?
            .validate()?;
        before(cutoff)?;
        Ok(cutoff)
    }
    /// Complete the Keeper's initial Creator/EXEC exchange before native
    /// adoption waits for the independently authenticated Guardian endpoint.
    /// The cutoff was supplied by the original controller; never resample it.
    pub(in crate::network_runtime) fn announce_successor_creator(&mut self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.successor_creator.attempted,
                "runtime initial Creator announcement cannot repeat",
            )?;
            self.successor_creator.attempted = true;
            self.check_successor_creator()?;
            let unit = self
                .unit
                .as_ref()
                .ok_or_else(|| io::Error::other("runtime original service unit absent"))?
                .to_str()
                .map_err(io::Error::other)?;
            require(
                unit.strip_prefix("hermit-accepted-")
                    .and_then(|s| s.strip_suffix(".service"))
                    .is_some_and(super::valid_nonce)
                    && super::valid_nonce(&self.callbacks.intent.nonce),
                "runtime initial Creator unit or nonce differs",
            )?;
            let invocation = std::env::var("INVOCATION_ID").map_err(io::Error::other)?;
            require(
                super::valid_nonce(&invocation),
                "manager invocation is absent or malformed",
            )?;
            let pid = unsafe { libc::getpid() };
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.successor_creator.pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
            let membership = owner::read_file("/proc/self/cgroup", 4096)?;
            let group = membership
                .strip_prefix("0::")
                .and_then(|s| s.strip_suffix('\n'))
                .ok_or_else(|| io::Error::other("runtime Creator cgroup framing differs"))?;
            require(
                group.starts_with('/')
                    && group != "/"
                    && !group.contains('\n')
                    && !group.split('/').any(|s| matches!(s, "." | "..")),
                "runtime Creator cgroup is not an exact nonroot path",
            )?;
            let path = CString::new(format!("/sys/fs/cgroup{group}")).map_err(io::Error::other)?;
            let raw = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.successor_creator.directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
            require(
                owner::filesystem(raw)? == 0x6367_7270,
                "runtime Creator directory is not actual cgroup2",
            )?;
            owner::pidfd_matches(
                self.successor_creator.pidfd.as_ref().unwrap().as_raw_fd(),
                pid,
            )?;
            self.successor_creator.packet = Some(
                format!(
                    "UNIT_CREATED unit={unit} invocation={invocation} pid={pid} nonce={}\n",
                    self.callbacks.intent.nonce
                )
                .into_bytes(),
            );
            self.exchange_successor_creator()
        })();
        self.remember(result)
    }
    fn exchange_successor_creator(&mut self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        let result = (|| {
            require(
                self.successor_creator.attempted
                    && !self.successor_creator.send_attempted
                    && !self.successor_creator.accepted
                    && self.successor_creator.pidfd.is_some()
                    && self.successor_creator.directory.is_some()
                    && self.successor_creator.packet.is_some(),
                "runtime initial Creator send repeated or lacks retained identity",
            )?;
            self.successor_creator.send_attempted = true;
            self.check_successor_creator()?;
            // Channel retains the exact attempted packet/raw send; these two
            // actual descriptions remain owned even after a failed send.
            self.source.as_mut().unwrap().send_once(
                self.successor_creator.packet.as_ref().unwrap(),
                &[
                    self.successor_creator.pidfd.as_ref().unwrap().as_fd(),
                    self.successor_creator.directory.as_ref().unwrap().as_fd(),
                ],
            )?;
            loop {
                let cutoff = self.check_successor_creator()?;
                if let Some(index) = self.source.as_mut().unwrap().receive(2048)? {
                    self.successor_creator.exec_packet = Some(index);
                    let packet = &self.source.as_ref().unwrap().packets[index];
                    packet.exact(0, self.source_peer.unwrap())?;
                    require(
                        packet.bytes
                            == format!("EXEC {}\n", self.callbacks.intent.nonce).as_bytes(),
                        "runtime initial Creator did not receive exact Keeper EXEC",
                    )?;
                    self.check_successor_creator()?;
                    self.successor_creator.accepted = true;
                    return Ok(());
                }
                let now = guardian::monotonic_ns()?;
                require(now < cutoff, "runtime original cutoff expired")?;
                let mut fd = libc::pollfd {
                    fd: self.source.as_ref().unwrap().fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds = (cutoff - now).div_ceil(1_000_000) as i32;
                let raw = unsafe { libc::poll(&mut fd, 1, milliseconds) };
                if raw < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                require(
                    raw == 1 && fd.revents & libc::POLLIN != 0,
                    "runtime initial Creator received no bounded Keeper response",
                )?;
            }
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn adoption(
        &self,
    ) -> io::Result<(u64, CString, u64, u64, CString)> {
        require(
            self.refusal.is_none() && self.leaves.len() == 3,
            "runtime adoption lacks retained leaves",
        )?;
        require(
            self.successor_creator.accepted,
            "runtime adoption preceded initial Keeper EXEC",
        )?;
        Ok((
            self.callbacks.intent.incarnation,
            CString::new(self.callbacks.intent.nonce.as_str()).unwrap(),
            self.startup_cutoff.unwrap(),
            self.creator_cutoff.unwrap(),
            self.unit.as_ref().unwrap().clone(),
        ))
    }
    pub(in crate::network_runtime) fn duplicate_leaves_into(
        &self,
        slot: &mut Vec<OwnedFd>,
    ) -> io::Result<()> {
        require(
            slot.is_empty() && self.leaves.len() == 3,
            "runtime leaf aliases repeated or absent",
        )?;
        for fd in &self.leaves {
            slot.push(duplicate(fd.as_fd())?);
        }
        Ok(())
    }
    fn commit_packet(&self, schema: &str) -> Value {
        json!({"schema":schema,"nonce":self.callbacks.intent.nonce,
            "incarnation":self.callbacks.intent.incarnation,"stage_deadline":self.startup_cutoff.unwrap(),"sequence":1})
    }
    fn receive_commit(&mut self, cutoff: u64) -> io::Result<()> {
        let index = receive(
            self.callbacks.peer.as_mut().unwrap(),
            self.callbacks.peer_identity.unwrap(),
            0,
            4096,
            cutoff,
        )?;
        let value: Value =
            serde_json::from_slice(&self.callbacks.peer.as_ref().unwrap().packets[index].bytes)?;
        if value["schema"] == "hermit-grouped-runtime-creation-agreement-v1" {
            self.pending_creation_agreement = Some(value);
            return Err(io::Error::other(
                "Keeper retained creation cleanup instead of provider commit",
            ));
        }
        require(
            value == self.commit_packet("hermit-grouped-runtime-provider-commit-ack-v1"),
            "runtime Keeper changed exact pre-open commit",
        )?;
        require(
            self.runtime_prepared && self.commit_attempted,
            "provider commit preceded actual runtime custody",
        )?;
        self.commit_acknowledged = true;
        self.callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-runtime-pre-open-commit","value":value}))?;
        before(cutoff)?;
        self.installed = true;
        Ok(())
    }
    pub(in crate::network_runtime) fn recover_pending_pre_open(
        &mut self,
        bridge: &mut Bridge,
        error: &io::Error,
        leaf_aliases: &mut Vec<OwnedFd>,
    ) -> io::Result<PreOpenRecovery> {
        if self.refusal.is_none() {
            self.refusal = Some(Failure::capture(error));
            self.failure_origin = guardian::monotonic_ns().ok();
        }
        self.recover_pre_open(bridge, error, self.failure_origin, leaf_aliases)
    }
    /// Called only by the retained service/FFI owner before any actual provider
    /// open or pointer lease. All native owner facts remain in that caller.
    pub(in crate::network_runtime) fn recover_pre_open(
        &mut self,
        bridge: &mut Bridge,
        error: &io::Error,
        original_origin: Option<u64>,
        leaf_aliases: &mut Vec<OwnedFd>,
    ) -> io::Result<PreOpenRecovery> {
        if self.terminal_complete {
            require(
                self.early.retired
                    && !self.commit_acknowledged
                    && leaf_aliases.is_empty()
                    && bridge.lease_attempt().is_none(),
                "completed creation cleanup changed native custody",
            )?;
            return Ok(PreOpenRecovery::CreationPeerCompleted);
        }
        require(
            !self.pre_open_recovery_attempted
                && !self.terminal_attempted
                && bridge.lease_attempt().is_none(),
            "pre-open recovery repeated or followed a native provider lease",
        )?;
        self.pre_open_recovery_attempted = true;
        self.pre_open_aliases.append(leaf_aliases);
        if self.refusal.is_none() {
            self.refusal = Some(Failure::capture(error));
            self.failure_origin = original_origin;
        } else if let (Some(old), Some(origin)) = (self.failure_origin, original_origin) {
            self.failure_origin = Some(old.min(origin));
        }
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("original pre-open refusal clock unknown"))?;
        let cutoff = self
            .startup_cutoff
            .ok_or_else(|| io::Error::other("pre-open startup cutoff absent"))?
            .min(
                origin
                    .checked_add(1_000_000_000)
                    .ok_or_else(|| io::Error::other("pre-open original cutoff overflow"))?,
            );
        before(cutoff)?;
        if self.commit_attempted
            && !self.commit_acknowledged
            && self.pending_creation_agreement.is_none()
        {
            // A submitted commit is never turned back into AdmissionHeld. Read
            // the actual retained peer response; a refused channel stays UNKNOWN.
            if let Err(error) = self.receive_commit(cutoff)
                && self.pending_creation_agreement.is_none() {
                    return Err(error);
                }
        }
        if self.commit_acknowledged {
            require(
                self.runtime_prepared,
                "committed pre-open failure lacks actual native runtime custody",
            )?;
            self.installed = true;
            // No early aliases/deletion: after bootstrap failure notification,
            // the existing original-controller terminal path retires unopened C.
            return Ok(PreOpenRecovery::RuntimeTerminalRetained);
        }
        require(
            self.early.ready && !self.early.refused,
            "pre-open failure lacks original early cleanup peer",
        )?;
        // The original live S2 channel is shut down once, without dropping its
        // actual owner. This wakes the retained controller and any aliases.
        if self.pre_open_shutdown.is_none() {
            let raw = unsafe {
                libc::shutdown(
                    self.source.as_ref().unwrap().fd.as_raw_fd(),
                    libc::SHUT_RDWR,
                )
            };
            let failed = (raw == -1).then(io::Error::last_os_error);
            self.pre_open_shutdown =
                Some(json!({"raw":raw,"errno":failed.as_ref().and_then(io::Error::raw_os_error)}));
            if let Some(error) = failed {
                return Err(error);
            }
            require(raw == 0, "pre-open S2 shutdown returned unexpected value")?;
        }
        before(cutoff)?;
        let retired = bridge.retire_pre_open_aliases();
        let observation = format!("{:?}", bridge.pre_open_alias_retirement());
        self.callbacks.ledger.as_mut().unwrap().store.append(json!({"kind":"actual-never-leased-local-bridge-retirement",
            "observation":observation,"shutdown":self.pre_open_shutdown,"original_start":origin,"cutoff":cutoff,
            "performs_global_deletion":false}))?;
        retired?;
        before(cutoff)?;
        if self.pending_creation_agreement.is_none() {
            self.retain_source_failure(error)?;
        }
        let agreement = loop {
            before(cutoff)?;
            self.callbacks.peer_live()?;
            if let Some(value) = self.pending_creation_agreement.take() {
                break value;
            }
            let Some(index) = self.callbacks.peer.as_mut().unwrap().receive(4096)? else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            let packet = &self.callbacks.peer.as_ref().unwrap().packets[index];
            let value: Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "pre-open recovery peer reply is noncanonical",
            )?;
            if value["schema"] == "hermit-grouped-runtime-keeper-custody-v1" {
                packet.exact(3, self.callbacks.peer_identity.unwrap())?;
                require(
                    !self.keeper_archive_ack_seen
                        && value
                            == self
                                .archive
                                .acknowledgement("hermit-grouped-runtime-keeper-custody-v1")?,
                    "late custody ACK changed original archive or repeated",
                )?;
                let controls = self.archive.controls()?;
                for (i, control) in controls.into_iter().enumerate() {
                    same_ofd(control, packet.rights[i].as_fd())?;
                }
                self.keeper_archive_ack_seen = true;
                self.callbacks.ledger.as_mut().unwrap().store.append(
                    json!({"kind":"late-archive-custody-without-provider-commit","value":value}),
                )?;
                continue;
            }
            packet.exact(0, self.callbacks.peer_identity.unwrap())?;
            require(
                value["schema"] == "hermit-grouped-runtime-creation-agreement-v1",
                "pre-open refusal did not receive original creation agreement",
            )?;
            break value;
        };
        let cleanup = self.run_creation_cleanup_peer(agreement);
        if self.terminal_complete {
            Ok(PreOpenRecovery::CreationPeerCompleted)
        } else {
            cleanup?;
            Err(io::Error::other(
                "creation peer returned without actual terminal completion",
            ))
        }
    }
    pub(in crate::network_runtime) fn install_provider(
        &mut self,
        bridge: &mut Bridge,
        controller: BorrowedFd<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                !self.install_attempted && self.refusal.is_none(),
                "runtime install is one-use",
            )?;
            self.install_attempted = true;
            same_ofd(controller, self.controller.as_ref().unwrap().as_fd())?;
            let cutoff = self.startup_cutoff.unwrap();
            before(cutoff)?;
            {
                let peer = self.callbacks.peer.as_mut().unwrap();
                let identity = self.callbacks.peer_identity.unwrap();
                let pending = &mut self.pending_creation_agreement;
                let pin = self.source_pin.as_ref().unwrap().as_raw_fd();
                self.archive.receive_progress(
                    self.source.as_mut().unwrap(),
                    self.source_peer.unwrap(),
                    &self.callbacks.intent,
                    cutoff,
                    None,
                    || {
                        if let Some(index) = peer.receive(4096)? {
                            let packet = &peer.packets[index];
                            packet.exact(0, identity)?;
                            let value: Value = serde_json::from_slice(&packet.bytes)?;
                            require(
                                journal::canonical(&value)? == packet.bytes
                                    && value["schema"]
                                        == "hermit-grouped-runtime-creation-agreement-v1",
                                "unexpected Keeper message during original source archive",
                            )?;
                            *pending = Some(value);
                            return Err(io::Error::other(
                                "Keeper requested creation cleanup during source archive",
                            ));
                        }
                        require(
                            !owner::terminal(pin)?,
                            "startup controller became terminal during source archive",
                        )
                    },
                )?;
            }
            self.early.bind_archive(&self.archive, cutoff)?;
            {
                let actual = bridge.runtime_control_descriptors()?;
                let imported = self.archive.controls()?;
                for i in 0..3 {
                    same_ofd(actual[i], imported[i])?;
                }
            }
            self.archive
                .forward(self.callbacks.peer.as_mut().unwrap(), cutoff)?;
            let index = loop {
                before(cutoff)?;
                self.callbacks.peer_live()?;
                if let Some(index) = self.callbacks.peer.as_mut().unwrap().receive(4096)? {
                    let packet = &self.callbacks.peer.as_ref().unwrap().packets[index];
                    let value: Value = serde_json::from_slice(&packet.bytes)?;
                    require(
                        journal::canonical(&value)? == packet.bytes,
                        "Keeper archive reply is not canonical",
                    )?;
                    if value["schema"] == "hermit-grouped-runtime-creation-agreement-v1" {
                        packet.exact(0, self.callbacks.peer_identity.unwrap())?;
                        self.pending_creation_agreement = Some(value);
                        return Err(io::Error::other(
                            "Keeper requested creation cleanup during archive acknowledgement",
                        ));
                    }
                    packet.exact(3, self.callbacks.peer_identity.unwrap())?;
                    break index;
                }
                require(
                    !owner::terminal(self.source_pin.as_ref().unwrap().as_raw_fd())?,
                    "startup controller became terminal before archive acknowledgement",
                )?;
                std::thread::sleep(Duration::from_millis(1));
            };
            let packet = &self.callbacks.peer.as_ref().unwrap().packets[index];
            require(
                packet.bytes
                    == journal::canonical(
                        &self
                            .archive
                            .acknowledgement("hermit-grouped-runtime-keeper-custody-v1")?,
                    )?,
                "runtime Keeper did not acknowledge exact original custody",
            )?;
            let controls = self.archive.controls()?;
            for (i, control) in controls.into_iter().enumerate() {
                same_ofd(control, packet.rights[i].as_fd())?;
            }
            self.keeper_archive_ack_seen = true;
            self.callbacks.peer_live()?;
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"actual-runtime-keeper-custody",
                "packet":hex(&packet.bytes)}))?;
            let ack = self
                .archive
                .acknowledgement("hermit-grouped-runtime-source-dual-custody-ack-v1")?;
            send(self.source.as_mut().unwrap(), &ack, &[], cutoff)?;
            self.callbacks.cursor = Cursor::Parent;
            let context = (&mut *self.callbacks as *mut RuntimeCallbacks).cast();
            unsafe {
                bridge.install_runtime_journal(runtime_journal, context)?;
            }
            // The actual spawning CLI owner performs the Child/pipe/group join.
            // This process additionally observes the original helper pidfd;
            // the receipt never becomes a reconstructed Child or query owner.
            let request = json!({"schema":"hermit-grouped-parent-join-startup-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"stage_deadline":cutoff});
            send(self.coordinator.as_mut().unwrap(), &request, &[], cutoff)?;
            let response = receive_value(
                self.coordinator.as_mut().unwrap(),
                self.coordinator_peer.unwrap(),
                cutoff,
            )?;
            require(
                response["schema"] == "hermit-grouped-parent-startup-joined-v1"
                    && response["nonce"] == self.callbacks.intent.nonce
                    && response["incarnation"] == self.callbacks.intent.incarnation
                    && response["stage_deadline"] == cutoff
                    && response["raw_wait_status"] == 0
                    && response["eof"] == json!([true, true])
                    && response["group_absent"] == true,
                "runtime original CLI startup join refused",
            )?;
            require(
                owner::terminal(self.source_pin.as_ref().unwrap().as_raw_fd())?,
                "startup helper remains live after original join",
            )?;
            self.retained_join = Some(response);
            self.callbacks.ledger.as_mut().unwrap().store.append(
                json!({"kind":"original-startup-child-joined","value":self.retained_join}),
            )?;
            self.archive.retire_frame_aliases(cutoff)?;
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"actual-exported-frame-aliases-retired",
                "closes":self.archive.frame_alias_closes}))?;
            before(cutoff)?;
            self.runtime_prepared = true;
            let commit = self.commit_packet("hermit-grouped-runtime-provider-commit-v1");
            self.commit_attempted = true;
            send(self.callbacks.peer.as_mut().unwrap(), &commit, &[], cutoff)?;
            self.receive_commit(cutoff)
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn provider_ready(&self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        require(
            self.installed
                && self.commit_acknowledged
                && !self.terminal_attempted
                && self.callbacks.cursor == Cursor::Parent,
            "runtime transfer is not installed and joined",
        )?;
        self.callbacks.peer_live()
    }
    pub(in crate::network_runtime) fn begin_terminal(
        &mut self,
        controller: BorrowedFd<'_>,
        start: u64,
        cutoff: u64,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                !self.terminal_attempted && self.installed,
                "runtime terminal is one-use after real installation",
            )?;
            self.terminal_attempted = true;
            let now = guardian::monotonic_ns()?;
            require(
                start <= now
                    && cutoff
                        <= start
                            .checked_add(1_000_000_000)
                            .ok_or_else(|| io::Error::other("runtime release overflow"))?,
                "runtime terminal original bound differs",
            )?;
            if let Some(failure) = self.failure_origin {
                require(
                    cutoff
                        <= failure
                            .checked_add(1_000_000_000)
                            .ok_or_else(|| io::Error::other("runtime failure bound overflow"))?,
                    "runtime terminal extends original failure",
                )?;
            }
            self.callbacks.origin = Some(start);
            self.callbacks.cutoff = Some(cutoff);
            same_ofd(controller, self.controller.as_ref().unwrap().as_fd())?;
            require(
                owner::terminal(controller.as_raw_fd())?,
                "runtime original controller is live",
            )?;
            self.callbacks.check()?;
            let request = json!({"schema":"hermit-grouped-runtime-terminal-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"original_start":start,"cutoff":cutoff,"sequence":1});
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"original-runtime-release","value":request}))?;
            send(self.callbacks.peer.as_mut().unwrap(), &request, &[], cutoff)?;
            let mut expected = request;
            expected["schema"] = json!("hermit-grouped-runtime-terminal-ack-v1");
            let response = receive_value(
                self.callbacks.peer.as_mut().unwrap(),
                self.callbacks.peer_identity.unwrap(),
                cutoff,
            )?;
            require(
                response == expected,
                "runtime terminal peer changed original boundary",
            )?;
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"dual-runtime-release","value":response}))?;
            self.callbacks.check()?;
            Ok(())
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn finish_terminal(
        &mut self,
        bridge: &mut Bridge,
        start: u64,
        cutoff: u64,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                !self.finish_attempted
                    && self.terminal_attempted
                    && self.callbacks.origin == Some(start)
                    && self.callbacks.cutoff == Some(cutoff),
                "runtime finish changed original one-use boundary",
            )?;
            self.finish_attempted = true;
            self.callbacks.check()?;
            self.callbacks.ledger.as_mut().unwrap().complete()?;
            require(
                self.callbacks.sequence == 34,
                "runtime complete deletion lacks exact34 original removal callbacks",
            )?;
            for _ in 0..2 {
                self.observations.push(AbsenceObservation::retain(cutoff));
                let controls = self.early.controls()?;
                observe_absent(
                    self.observations.last_mut().unwrap(),
                    controls,
                    &self.callbacks.intent,
                    cutoff,
                    || {
                        require(
                            self.callbacks.cursor == Cursor::Parent,
                            "runtime local cursor was surrendered",
                        )?;
                        self.callbacks.check().map(|_| ())
                    },
                )?;
                self.observations.last().unwrap().persist(
                    &mut self.callbacks.ledger.as_mut().unwrap().store,
                    self.observations.len() as u64,
                )?;
            }
            let grant = json!({"schema":"hermit-grouped-runtime-read-grant-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"epoch":1,"original_start":start,"cutoff":cutoff});
            self.callbacks.cursor = Cursor::GrantSubmitted;
            send(self.callbacks.peer.as_mut().unwrap(), &grant, &[], cutoff)?;
            self.callbacks.cursor = Cursor::Peer;
            let response = receive_value(
                self.callbacks.peer.as_mut().unwrap(),
                self.callbacks.peer_identity.unwrap(),
                cutoff,
            )?;
            require(
                response["schema"] == "hermit-grouped-runtime-read-done-v1"
                    && response["nonce"] == self.callbacks.intent.nonce
                    && response["incarnation"] == self.callbacks.intent.incarnation
                    && response["epoch"] == 1
                    && response["original_start"] == start
                    && response["cutoff"] == cutoff
                    && response["observations"]
                        .as_array()
                        .is_some_and(|rows| rows.len() == 2),
                "runtime peer cursor relinquishment differs",
            )?;
            validate_absence_rows(
                &response["observations"],
                &self.callbacks.intent,
                start,
                cutoff,
            )?;
            self.callbacks.ledger.as_mut().unwrap().store.append(
                json!({"kind":"runtime-peer-absence-and-cursor-return","value":response}),
            )?;
            self.callbacks.cursor = Cursor::Parent;
            self.callbacks.check()?;
            let request = json!({"schema":"hermit-grouped-runtime-peer-retire-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"original_start":start,"cutoff":cutoff});
            send(self.callbacks.peer.as_mut().unwrap(), &request, &[], cutoff)?;
            let mut expected = request;
            expected["schema"] = json!("hermit-grouped-runtime-peer-retired-v1");
            let response = receive_value(
                self.callbacks.peer.as_mut().unwrap(),
                self.callbacks.peer_identity.unwrap(),
                cutoff,
            )?;
            require(
                response == expected,
                "runtime peer alias retirement differs",
            )?;
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"runtime-peer-aliases-retired","value":response}))?;
            let request = json!({"schema":"hermit-grouped-parent-join-runtime-v1","nonce":self.callbacks.intent.nonce,
                "incarnation":self.callbacks.intent.incarnation,"original_start":start,"cutoff":cutoff});
            send(self.coordinator.as_mut().unwrap(), &request, &[], cutoff)?;
            let response = receive_value(
                self.coordinator.as_mut().unwrap(),
                self.coordinator_peer.unwrap(),
                cutoff,
            )?;
            require(
                response["schema"] == "hermit-grouped-parent-runtime-joined-v1"
                    && response["nonce"] == self.callbacks.intent.nonce
                    && response["incarnation"] == self.callbacks.intent.incarnation
                    && response["original_start"] == start
                    && response["cutoff"] == cutoff
                    && response["raw_wait_status"] == 0
                    && response["eof"] == json!([true, true])
                    && response["group_absent"] == true
                    && response["unit_absent"] == true,
                "runtime original CLI Keeper join refused",
            )?;
            require(
                owner::terminal(self.callbacks.peer_pin.as_ref().unwrap().as_raw_fd())?,
                "runtime Keeper still live after original join",
            )?;
            self.callbacks
                .ledger
                .as_mut()
                .unwrap()
                .store
                .append(json!({"kind":"original-runtime-keeper-joined","value":response}))?;
            before(cutoff)?;
            bridge.release_aliases()?;
            before(cutoff)?;
            bridge.finish()?;
            before(cutoff)?;
            self.callbacks.ledger.as_mut().unwrap().complete()?;
            self.callbacks.cursor = Cursor::Closed;
            self.terminal_complete = true;
            Ok(())
        })();
        if result.is_err() {
            self.callbacks.cursor = Cursor::Unknown;
        }
        self.remember(result)
    }
}

/// Each rewind, read, EOF and event-directory observation checks the caller's
/// actual exclusive phase and original cutoff. Shared OFDs are never read by
/// both holders at once. Returned bytes are retained separately for both passes.
fn read_control_contents(
    observation: &mut AbsenceObservation,
    controls: [BorrowedFd<'_>; 3],
    cutoff: u64,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    require(
        observation.started.is_none() && observation.cutoff == cutoff,
        "runtime absence cannot restart",
    )?;
    observation.started = Some(guardian::monotonic_ns()?);
    before(cutoff)?;
    check()?;
    for (index, fd) in controls[..2].iter().enumerate() {
        check()?;
        before(cutoff)?;
        let raw = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
        observation.rewinds.push((
            raw,
            (raw == -1)
                .then(io::Error::last_os_error)
                .and_then(|e| e.raw_os_error()),
        ));
        require(raw == 0, "runtime absence rewind failed")?;
        loop {
            check()?;
            before(cutoff)?;
            let bytes = if index == 0 {
                &mut observation.definitions
            } else {
                &mut observation.profile
            };
            let mut buffer = [0u8; 4096];
            let cap = buffer.len().min(1_048_577usize.saturating_sub(bytes.len()));
            require(
                cap > 0,
                "runtime complete absence snapshot exceeds original1MiB",
            )?;
            let raw = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), cap) };
            let error = (raw == -1).then(io::Error::last_os_error);
            observation.last_read =
                Some((index, raw, error.as_ref().and_then(io::Error::raw_os_error)));
            if let Some(error) = error {
                return Err(error);
            }
            if raw == 0 {
                observation.eof[index] = true;
                check()?;
                before(cutoff)?;
                break;
            }
            bytes.extend_from_slice(&buffer[..raw as usize]);
            require(
                bytes.len() <= 1_048_576,
                "runtime complete absence snapshot exceeds original1MiB",
            )?;
        }
    }
    check()?;
    before(cutoff)?;
    observation.completed = Some(guardian::monotonic_ns()?);
    before(cutoff)?;
    check()
}
pub(super) fn observe_absent(
    observation: &mut AbsenceObservation,
    controls: [BorrowedFd<'_>; 3],
    intent: &Intent,
    cutoff: u64,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    read_control_contents(observation, controls, cutoff, &mut check)?;
    check()?;
    before(cutoff)?;
    owner::check_runtime_absence(
        intent,
        &observation.definitions,
        &observation.profile,
        controls[2],
    )?;
    for path in [
        intent.group(),
        format!("{}/{}", intent.group(), intent.event()),
    ] {
        check()?;
        before(cutoff)?;
        let name = CString::new(path.as_str()).map_err(io::Error::other)?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        let raw = unsafe {
            libc::fstatat(
                controls[2].as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let error = (raw == -1).then(io::Error::last_os_error);
        observation.directories.push(
            json!({"path":path,"attempted":true,"returned":true,"raw":raw,
            "errno":error.as_ref().and_then(io::Error::raw_os_error)}),
        );
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ENOENT),
            "runtime final event query not absent",
        )?;
    }
    observation.completed = Some(guardian::monotonic_ns()?);
    before(cutoff)?;
    check()
}
#[derive(Debug)]
pub(super) struct AbsenceObservation {
    started: Option<u64>,
    completed: Option<u64>,
    cutoff: u64,
    definitions: Vec<u8>,
    profile: Vec<u8>,
    rewinds: Vec<(i64, Option<i32>)>,
    last_read: Option<(usize, isize, Option<i32>)>,
    eof: [bool; 2],
    directories: Vec<Value>,
}
impl AbsenceObservation {
    pub(super) fn retain(cutoff: u64) -> Self {
        Self {
            started: None,
            completed: None,
            cutoff,
            definitions: Vec::new(),
            profile: Vec::new(),
            rewinds: Vec::new(),
            last_read: None,
            eof: [false, false],
            directories: Vec::new(),
        }
    }
    pub(super) fn definitions(&self) -> &[u8] {
        &self.definitions
    }
    pub(super) fn profile(&self) -> &[u8] {
        &self.profile
    }
    pub(super) fn receipt(&self) -> Value {
        json!({"schema":"hermit-grouped-runtime-absence-v1","started":self.started,
            "completed":self.completed,"cutoff":self.cutoff,
            "definitions":{"bytes":self.definitions.len(),"sha256":hex(&Sha256::digest(&self.definitions))},
            "profile":{"bytes":self.profile.len(),"sha256":hex(&Sha256::digest(&self.profile))},
            "directories":self.directories})
    }
    pub(super) fn persist(&self, store: &mut journal::Store, sequence: u64) -> io::Result<()> {
        require(
            self.eof == [true, true] && self.completed.is_some() && self.directories.len() == 2,
            "runtime absence persistence is not complete",
        )?;
        self.persist_contents(store, sequence, "runtime-absence-bytes")?;
        before(self.cutoff)?;
        store.append(
            json!({"kind":"runtime-fresh-absence","sequence":sequence,"value":self.receipt()}),
        )?;
        before(self.cutoff)
    }
    fn persist_contents(
        &self,
        store: &mut journal::Store,
        sequence: u64,
        kind: &str,
    ) -> io::Result<()> {
        require(
            self.eof == [true, true] && self.completed.is_some(),
            "runtime contents persistence is not complete",
        )?;
        for (role, bytes) in [
            ("definitions", &self.definitions),
            ("profile", &self.profile),
        ] {
            for (chunk, part) in bytes.chunks(1024).enumerate() {
                before(self.cutoff)?;
                store.append(json!({"kind":kind,"sequence":sequence,
                    "role":role,"offset":chunk*1024,"bytes":hex(part)}))?;
            }
        }
        before(self.cutoff)?;
        store.append(json!({"kind":"runtime-control-read","sequence":sequence,"started":self.started,
            "completed":self.completed,"cutoff":self.cutoff,
            "definitions":{"bytes":self.definitions.len(),"sha256":hex(&Sha256::digest(&self.definitions))},
            "profile":{"bytes":self.profile.len(),"sha256":hex(&Sha256::digest(&self.profile))}}))?;
        before(self.cutoff)
    }
}
pub(super) fn validate_absence_rows(
    rows: &Value,
    intent: &Intent,
    origin: u64,
    cutoff: u64,
) -> io::Result<()> {
    let rows = rows
        .as_array()
        .ok_or_else(|| io::Error::other("runtime peer absence rows missing"))?;
    require(rows.len() == 2, "runtime peer absence population differs")?;
    let mut prior = origin;
    for row in rows {
        let start = row["started"]
            .as_u64()
            .ok_or_else(|| io::Error::other("runtime peer absence start missing"))?;
        let end = row["completed"]
            .as_u64()
            .ok_or_else(|| io::Error::other("runtime peer absence completion missing"))?;
        require(
            start >= prior && end >= start && end < cutoff && row["cutoff"] == cutoff,
            "runtime peer absence changed original order/cutoff",
        )?;
        for role in ["definitions", "profile"] {
            let bytes = row[role]["bytes"]
                .as_u64()
                .ok_or_else(|| io::Error::other("runtime peer absence length missing"))?;
            let digest = row[role]["sha256"]
                .as_str()
                .ok_or_else(|| io::Error::other("runtime peer absence digest missing"))?;
            require(
                bytes <= 1_048_576
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && row[role] == json!({"bytes":bytes,"sha256":digest}),
                "runtime peer absence digest or extent differs",
            )?;
        }
        let directories=json!([intent.group(),format!("{}/{}",intent.group(),intent.event())]).as_array().unwrap().iter()
            .map(|path|json!({"path":path,"attempted":true,"returned":true,"raw":-1,"errno":libc::ENOENT})).collect::<Vec<_>>();
        require(
            row == &json!({"schema":"hermit-grouped-runtime-absence-v1","started":start,"completed":end,
            "cutoff":cutoff,"definitions":row["definitions"],"profile":row["profile"],"directories":directories}),
            "runtime peer native absence results or exact fields differ",
        )?;
        prior = end;
    }
    Ok(())
}

#[cfg(test)]
mod successor_creator_tests {
    use super::*;

    fn channel_pair() -> (wire::Channel, wire::Channel) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let owned = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        for fd in &owned {
            let enabled = 1i32;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PASSCRED,
                        (&enabled as *const i32).cast(),
                        std::mem::size_of_val(&enabled) as libc::socklen_t,
                    )
                },
                0
            );
        }
        let [a, b] = owned;
        (wire::Channel::retain(a), wire::Channel::retain(b))
    }

    fn self_pin() -> OwnedFd {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw as i32) }
    }

    // This fixture establishes only the already-retained local protocol
    // premise. Actual SCM credentials, pidfds, directory and packets are real;
    // the self peer/unit/leaves stand in for completed bootstrap authentication.
    // It does not claim independent manager, Creator or native adoption proof.
    fn fixture() -> (RuntimeCleanup, wire::Channel) {
        let (source, peer) = channel_pair();
        let (bootstrap, unused) = channel_pair();
        drop(unused);
        let mut runtime = RuntimeCleanup::retain_bootstrap(bootstrap.fd, [1; 16]);
        let cutoff = guardian::monotonic_ns()
            .unwrap()
            .checked_add(1_000_000_000)
            .unwrap();
        runtime.startup_cutoff = Some(cutoff);
        runtime.creator_cutoff = Some(cutoff);
        runtime.early.ready = true;
        runtime.early.acknowledged = 34;
        runtime.source = Some(source);
        runtime.source_peer = Some(wire::Credentials {
            pid: unsafe { libc::getpid() },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        });
        runtime.source_pin = Some(self_pin());
        runtime.unit = Some(
            CString::new(format!(
                "hermit-accepted-{}.service",
                runtime.callbacks.intent.nonce
            ))
            .unwrap(),
        );
        let directory: OwnedFd = std::fs::File::open("/sys/fs/cgroup").unwrap().into();
        for _ in 0..3 {
            runtime.leaves.push(directory.try_clone().unwrap());
        }
        runtime.successor_creator.pidfd = Some(self_pin());
        runtime.successor_creator.directory = Some(directory);
        runtime.successor_creator.packet = Some(
            format!(
                "UNIT_CREATED unit={} invocation={} pid={} nonce={}\n",
                runtime.unit.as_ref().unwrap().to_str().unwrap(),
                "02".repeat(16),
                unsafe { libc::getpid() },
                runtime.callbacks.intent.nonce
            )
            .into_bytes(),
        );
        runtime.successor_creator.attempted = true;
        (runtime, peer)
    }

    fn exec(runtime: &RuntimeCleanup) -> Vec<u8> {
        format!("EXEC {}\n", runtime.callbacks.intent.nonce).into_bytes()
    }

    #[test]
    fn s2_creator_exchange_requires_exact_exec_before_adoption() {
        let (mut runtime, mut peer) = fixture();
        let cutoff = runtime.creator_cutoff;
        let descriptors = runtime
            .successor_creator
            .held_descriptors()
            .collect::<Vec<_>>();
        assert_eq!(descriptors.len(), 2);
        assert_eq!(
            runtime.adoption().unwrap_err().to_string(),
            "runtime adoption preceded initial Keeper EXEC"
        );
        peer.send_once(&exec(&runtime), &[]).unwrap();
        runtime.exchange_successor_creator().unwrap();
        assert!(runtime.successor_creator.accepted);
        assert_eq!(runtime.successor_creator.exec_packet, Some(0));
        assert_eq!(runtime.creator_cutoff, cutoff);
        let (_, _, stage, admitted_cutoff, _) = runtime.adoption().unwrap();
        assert_eq!(Some(admitted_cutoff), cutoff);
        assert_eq!(stage, admitted_cutoff);
        let index = peer.receive(2048).unwrap().unwrap();
        let packet = &peer.packets[index];
        packet.exact(2, runtime.source_peer.unwrap()).unwrap();
        assert_eq!(
            packet.bytes,
            *runtime.successor_creator.packet.as_ref().unwrap()
        );
        owner::pidfd_matches(packet.rights[0].as_raw_fd(), unsafe { libc::getpid() }).unwrap();
        assert_eq!(
            owner::filesystem(packet.rights[1].as_raw_fd()).unwrap(),
            0x6367_7270
        );
        let mut expected: libc::stat = unsafe { std::mem::zeroed() };
        let mut actual: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(descriptors[1], &mut expected) }, 0);
        assert_eq!(
            unsafe { libc::fstat(packet.rights[1].as_raw_fd(), &mut actual) },
            0
        );
        assert_eq!(
            (actual.st_dev, actual.st_ino),
            (expected.st_dev, expected.st_ino)
        );
        assert_eq!(runtime.source.as_ref().unwrap().sends.len(), 1);
        assert_eq!(
            runtime
                .announce_successor_creator()
                .unwrap_err()
                .to_string(),
            "runtime initial Creator announcement cannot repeat"
        );
        assert!(runtime.adoption().is_err());
        assert_eq!(runtime.source.as_ref().unwrap().sends.len(), 1);
        assert_eq!(
            runtime
                .successor_creator
                .held_descriptors()
                .collect::<Vec<_>>(),
            descriptors
        );
    }

    #[test]
    fn s2_creator_exchange_rejects_peer_nonce_framing_and_rights_stickily() {
        for variant in 0..5 {
            let (mut runtime, mut peer) = fixture();
            let original_peer = runtime.source_peer.unwrap();
            let mut bytes = exec(&runtime);
            match variant {
                0 => bytes[5] = b'f',
                1 => {
                    bytes.pop();
                }
                2 => bytes.push(b'\n'),
                3 => runtime.source_peer.as_mut().unwrap().uid ^= 1,
                4 => {}
                _ => unreachable!(),
            }
            let extra = self_pin();
            let rights = if variant == 4 {
                vec![extra.as_fd()]
            } else {
                vec![]
            };
            peer.send_once(&bytes, &rights).unwrap();
            let error = runtime
                .exchange_successor_creator()
                .unwrap_err()
                .to_string();
            if variant < 3 {
                assert_eq!(
                    error,
                    "runtime initial Creator did not receive exact Keeper EXEC"
                );
            } else {
                assert!(error.starts_with("grouped packet credentials, flags or rights differ;"));
            }
            assert!(!runtime.successor_creator.accepted);
            assert_eq!(runtime.successor_creator.exec_packet, Some(0));
            let source = runtime.source.as_ref().unwrap();
            assert_eq!(source.packets.len(), 1);
            assert_eq!(source.packets[0].bytes, bytes);
            assert_eq!(source.packets[0].rights.len(), usize::from(variant == 4));
            if variant == 4 {
                owner::pidfd_matches(source.packets[0].rights[0].as_raw_fd(), unsafe {
                    libc::getpid()
                })
                .unwrap();
            }
            let origin = runtime.failure_origin;
            assert!(origin.is_some());
            runtime.source_peer = Some(original_peer);
            peer.send_once(&exec(&runtime), &[]).unwrap();
            assert_eq!(
                runtime
                    .announce_successor_creator()
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert_eq!(
                runtime
                    .exchange_successor_creator()
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert_eq!(runtime.failure_origin, origin);
            assert_eq!(runtime.source.as_ref().unwrap().sends.len(), 1);
            assert_eq!(runtime.source.as_ref().unwrap().packets.len(), 1);
            assert!(runtime.adoption().is_err());
        }
    }

    #[test]
    fn s2_creator_exchange_preserves_expired_cutoff_without_acquisition_or_send() {
        let (mut runtime, mut peer) = fixture();
        runtime.successor_creator = SuccessorCreator::default();
        runtime.creator_cutoff = Some(guardian::monotonic_ns().unwrap());
        let cutoff = runtime.creator_cutoff;
        peer.send_once(&exec(&runtime), &[]).unwrap();
        let error = runtime
            .announce_successor_creator()
            .unwrap_err()
            .to_string();
        assert_eq!(error, "runtime original cutoff expired");
        let origin = runtime.failure_origin;
        assert!(origin.is_some());
        assert!(runtime.successor_creator.attempted);
        assert!(!runtime.successor_creator.send_attempted);
        assert!(!runtime.successor_creator.accepted);
        assert_eq!(runtime.successor_creator.held_descriptors().count(), 0);
        assert!(runtime.successor_creator.packet.is_none());
        assert_eq!(
            runtime
                .announce_successor_creator()
                .unwrap_err()
                .to_string(),
            error
        );
        assert_eq!(runtime.creator_cutoff, cutoff);
        assert_eq!(runtime.failure_origin, origin);
        assert!(runtime.source.as_ref().unwrap().sends.is_empty());
        assert!(runtime.source.as_ref().unwrap().packets.is_empty());
        assert!(runtime.adoption().is_err());
    }

    #[test]
    fn s2_creator_exchange_retains_failed_send_and_both_original_descriptions() {
        let (mut runtime, peer) = fixture();
        let descriptors = runtime
            .successor_creator
            .held_descriptors()
            .collect::<Vec<_>>();
        let bytes = runtime.successor_creator.packet.clone().unwrap();
        drop(peer);
        let error = runtime.exchange_successor_creator().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
        let origin = runtime.failure_origin;
        assert!(origin.is_some());
        let source = runtime.source.as_ref().unwrap();
        assert_eq!(source.sends.len(), 1);
        assert_eq!(source.sends[0].bytes, bytes);
        assert_eq!(source.sends[0].rights, descriptors);
        assert_eq!(source.sends[0].raw.unwrap().returned, -1);
        assert_eq!(source.sends[0].raw.unwrap().errno, Some(libc::EPIPE));
        assert!(source.packets.is_empty());
        assert!(!runtime.successor_creator.accepted);
        for fd in &descriptors {
            assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, libc::FD_CLOEXEC);
        }
        assert_eq!(
            runtime
                .exchange_successor_creator()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPIPE)
        );
        assert_eq!(runtime.failure_origin, origin);
        assert_eq!(runtime.source.as_ref().unwrap().sends.len(), 1);
        assert_eq!(
            runtime
                .successor_creator
                .held_descriptors()
                .collect::<Vec<_>>(),
            descriptors
        );
        assert!(runtime.adoption().is_err());
    }
}
