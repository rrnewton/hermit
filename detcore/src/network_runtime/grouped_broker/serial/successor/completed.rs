//! Transfer the actual completed source archives, without replaying callbacks
//! into new source Journals or creating authority from their serialized rows.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::RawFd;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::super::super::Failure;
use super::super::super::guardian;
use super::super::super::hex;
use super::super::super::journal;
use super::super::super::owner;
use super::super::super::require;
use super::super::super::wire;
use super::super::Phase;
use super::RetainedSuccessor;
use super::Stage;

#[derive(Debug)]
struct CapturedRight {
    descriptor: RawFd,
    identity: owner::FileIdentity,
    flags: i32,
}
#[derive(Debug)]
struct Frame {
    bytes: Vec<u8>,
    rights: Vec<CapturedRight>,
}
#[derive(Debug)]
#[must_use = "completed source custody remains installed across transfer failure"]
pub(in super::super::super) struct CompletedSourceExport {
    // This field owns EVERY descriptor referenced by Frame.rights. No method
    // extracts it, invokes source retirement, or replaces a descriptor while
    // frames are available. Moving the struct does not move kernel FD numbers.
    source: RetainedSuccessor,
    frames: Vec<Frame>,
    next: usize,
    total_bytes: usize,
    prepared: bool,
    preparation_attempted: bool,
    acknowledgement_index: Option<usize>,
    acknowledged: bool,
    startup_retirement_attempted: bool,
    expected_ack: Option<Vec<u8>>,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
}

impl CompletedSourceExport {
    pub fn retain(source: RetainedSuccessor) -> Self {
        Self {
            source,
            frames: Vec::new(),
            next: 0,
            total_bytes: 0,
            prepared: false,
            preparation_attempted: false,
            acknowledgement_index: None,
            acknowledged: false,
            startup_retirement_attempted: false,
            expected_ack: None,
            refusal: None,
            failure_origin: None,
        }
    }

    pub fn local_custody_records(&self) -> io::Result<Vec<Value>> {
        self.source.local_custody_records()
    }
    pub fn failure_notice_origin(&self) -> io::Result<u64> {
        self.source.failure_notice_origin()
    }
    pub fn send_local_custody(&mut self, report: &Value, file: BorrowedFd<'_>) -> io::Result<()> {
        self.source.send_local_custody(report, file)
    }
    pub fn retire_local_custody(
        &mut self,
        logs: BorrowedFd<'_>,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<bool> {
        let deadline = if self.refusal.is_some() {
            guardian::clip_custody_origin(deadline, self.failure_origin)?
        } else {
            deadline
        };
        self.source.retire_local_custody(logs, deadline, cause)
    }
    pub fn notify_runtime_failure(
        &mut self,
        caller_origin: Option<u64>,
        cause: &io::Error,
    ) -> io::Result<()> {
        let origin = match (self.failure_origin, caller_origin) {
            (Some(own), Some(caller)) => Some(own.min(caller)),
            (None, caller) if self.refusal.is_none() => caller,
            _ => None,
        };
        self.source.notify_runtime_failure(origin, cause)
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

    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        self.source.check_creator()?;
        require(
            self.source.stage == Stage::Acknowledged && self.source.acknowledgement_sent(),
            "completed source transfer precedes actual S2 adoption ACK",
        )
    }

    pub fn prepare(&mut self) -> io::Result<()> {
        let result = self.prepare_inner();
        self.remember(result)
    }

    fn prepare_inner(&mut self) -> io::Result<()> {
        self.check()?;
        require(
            !self.preparation_attempted,
            "completed source export preparation cannot repeat",
        )?;
        self.preparation_attempted = true;
        let offer = self.source.offer.as_ref().unwrap();
        let offer_name = offer.name.clone();
        let transcript_sha256 = offer.transcript_sha256.clone();
        let provider_record = self.source.creator.as_ref().unwrap().evidence()?;
        let Some(Phase::Planned(plan)) = &mut self.source.source.phase else {
            unreachable!()
        };
        let original = &mut plan.source.source.original;
        let guardian = original.guardian.completed_leaf_archive()?;
        require(
            Some(guardian.pairs()) == plan.created_pairs.as_deref(),
            "completed source export changed original actual pairs",
        )?;
        let keeper = original.imported.borrow_after_leaf(&guardian)?;
        let intent = guardian.intent();
        let keeper_history = keeper.history_bytes();
        let keeper_commitment = json!({"sha256":hex(&Sha256::digest(keeper_history)),
            "bytes":keeper_history.len(),"frames":34});
        let header = json!({"schema":"hermit-grouped-runtime-source-header-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,
            "stage_deadline":guardian.native_deadline(),
            "offer":offer_name,"transcript_sha256":transcript_sha256,
            "provider":provider_record,
            "guardian":guardian.completed_record()?,"keeper":keeper.record(),
            "guardian_store":guardian.history().commitment(),
            "keeper_store":keeper_commitment});
        push(&mut self.frames, &mut self.total_bytes, header, &[])?;

        // Fixed roles, exact right populations, no broad generic bundle API.
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            1,
            "source_creator",
            &guardian.creator_rights(),
        )?;
        let controls: Vec<_> = guardian.controls().fds.iter().map(AsFd::as_fd).collect();
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            2,
            "controls",
            &controls,
        )?;
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            3,
            "guardian_store",
            &guardian.store_rights()?,
        )?;
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            4,
            "keeper_store",
            &keeper.store_rights(),
        )?;
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            5,
            "keeper_creator",
            &keeper.creator_rights(),
        )?;
        push_role(
            &mut self.frames,
            &mut self.total_bytes,
            6,
            "keeper_controls",
            &keeper.control_rights(),
        )?;
        for (index, query) in guardian.queries()?.iter().enumerate() {
            push(
                &mut self.frames,
                &mut self.total_bytes,
                json!({"schema":"hermit-grouped-runtime-source-query-v1",
                    "sequence":7+index,"holder":"guardian","query":index,
                    "record":query.record()?}),
                &query.rights(),
            )?;
        }
        for index in 0..4 {
            let (record, rights) = keeper.query(index)?;
            push(
                &mut self.frames,
                &mut self.total_bytes,
                json!({"schema":"hermit-grouped-runtime-source-query-v1",
                    "sequence":11+index,"holder":"keeper","query":index,
                    "record":record}),
                &rights,
            )?;
        }
        require(
            self.frames.len() == 15,
            "completed source role inventory differs",
        )?;
        let mut hash = Sha256::new();
        for frame in &self.frames {
            hash.update((frame.bytes.len() as u64).to_le_bytes());
            hash.update(&frame.bytes);
        }
        let mut end = json!({"schema":"hermit-grouped-runtime-source-end-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,
            "stage_deadline":guardian.native_deadline(),"offer":offer_name,
            "transcript_sha256":transcript_sha256,
            "preceding_frames":15,"preceding_bytes":self.total_bytes,
            "framed_sha256":hex(&hash.finalize())});
        push(&mut self.frames, &mut self.total_bytes, end.clone(), &[])?;
        end["schema"] = json!("hermit-grouped-runtime-source-dual-custody-ack-v1");
        self.expected_ack = Some(journal::canonical(&end)?);
        self.source
            .store
            .append(json!({"kind":"runtime-source-export-intent",
            "frames":self.frames.len(),"bytes":self.total_bytes,
            "header_sha256":hex(&Sha256::digest(&self.frames[0].bytes)),
            "end":end}))?;
        self.check()?;
        self.source.census()?;
        self.prepared = true;
        Ok(())
    }

    pub fn send_next(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.prepared && !self.acknowledged,
                "completed source export is unprepared or already acknowledged",
            )?;
            if self.next == self.frames.len() {
                return Ok(false);
            }
            let index = self.next;
            self.next += 1; // occupy before verification or uncertain send
            let frame = &self.frames[index];
            for right in &frame.rights {
                require(
                    owner::stat(right.descriptor)?.same_owner(&right.identity)
                        && unsafe { libc::fcntl(right.descriptor, libc::F_GETFL) } == right.flags
                        && unsafe { libc::fcntl(right.descriptor, libc::F_GETFD) }
                            == libc::FD_CLOEXEC,
                    "completed source retained descriptor changed before transfer",
                )?;
            }
            // SAFETY: prepare captured only borrows into `self.source`, and
            // this owner retains that entire immutable custody tree. No
            // operation exposed by this type closes/replaces/extracts any
            // source descriptor. Channel::send_once uses the borrows only
            // for its synchronous sendmsg. Frame numbers confer no authority
            // without those still-owned original Rust resources.
            let rights: Vec<_> = frame
                .rights
                .iter()
                .map(|right| unsafe { BorrowedFd::borrow_raw(right.descriptor) })
                .collect();
            self.source.channel.send_once(&frame.bytes, &rights)?;
            self.check()?;
            self.source.census()?;
            Ok(true)
        })();
        self.remember(result)
    }

    /// This ACK requires the actual service to retain both original source
    /// Stores/controls and its runtime Keeper's independent durable custody.
    /// It still does not retire this controller or authorize provider Ready.
    pub fn receive_dual_custody_ack(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.prepared
                    && self.next == self.frames.len()
                    && self.acknowledgement_index.is_none()
                    && !self.acknowledged,
                "runtime source ACK is early, repeated or after refusal",
            )?;
            let Some(index) = self.source.channel.receive(2048)? else {
                return Ok(false);
            };
            self.acknowledgement_index = Some(index);
            let packet = &self.source.channel.packets[index];
            packet.exact(0, self.source.creator.as_ref().unwrap().peer)?;
            require(
                Some(&packet.bytes) == self.expected_ack.as_ref(),
                "runtime source dual custody ACK differs from exact export",
            )?;
            self.source
                .store
                .append(json!({"kind":"runtime-source-dual-custody-ack",
                "packet":hex(&packet.bytes)}))?;
            self.check()?;
            self.source.census()?;
            self.acknowledged = true;
            Ok(true)
        })();
        self.remember(result)
    }

    /// Called only after the actual dual-custody ACK. This retires the real
    /// startup Guardian, then proves its unchanged natural child/EOF/ECHILD
    /// outcome; a transport ACK itself never means the actor is terminal.
    pub fn finish_startup(
        &mut self,
        parent: &mut wire::Channel,
        logs: std::os::fd::BorrowedFd<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.acknowledged && !self.startup_retirement_attempted,
                "startup retirement precedes actual runtime dual custody or repeats",
            )?;
            self.startup_retirement_attempted = true;
            let plan = self.source.plan()?;
            let intent = plan.source.source.intent().clone();
            let native = plan.source.source.native_deadline();
            let deadline = plan.source.source.deadline();
            let peer = wire::Credentials {
                pid: self.source.guardian.child.id() as i32,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            };
            parent.send_once(
                &journal::canonical(
                    &json!({"schema":"hermit-grouped-successor-guardian-release-v1",
                "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":native}),
                )?,
                &[],
            )?;
            loop {
                self.source.guardian.drain()?;
                require(
                    Instant::now() < deadline,
                    "startup Guardian release exceeded original deadline",
                )?;
                if let Some(index) = parent.receive(4096)? {
                    let packet = &parent.packets[index];
                    packet.exact(0, peer)?;
                    require(
                        packet.bytes
                            == journal::canonical(
                                &json!({"schema":"hermit-grouped-successor-guardian-released-v1",
                        "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":native}),
                            )?,
                        "startup Guardian release ACK differs",
                    )?;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            loop {
                self.source.guardian.drain()?;
                require(
                    Instant::now() < deadline && guardian::monotonic_ns()? < native,
                    "startup Guardian join exceeded original deadline",
                )?;
                if self.source.guardian.eof == [true, true]
                    && owner::terminal(self.source.guardian.pidfd.as_ref().unwrap().as_raw_fd())?
                {
                    self.source.guardian.reap_success(logs.as_raw_fd())?;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.source
                .store
                .append(json!({"kind":"actual-startup-guardian-retired",
                "pid":peer.pid,"runtime_dual_custody_ack_retained":true}))?;
            Ok(())
        })();
        self.remember(result)
    }
}

fn push_role(
    frames: &mut Vec<Frame>,
    total_bytes: &mut usize,
    sequence: usize,
    role: &str,
    rights: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let expected = match sequence {
        1 | 3 | 4 | 5 => 2,
        2 | 6 => 3,
        _ => return Err(io::Error::other("unknown completed source descriptor role")),
    };
    require(
        rights.len() == expected,
        "completed source role population differs",
    )?;
    push(
        frames,
        total_bytes,
        json!({"schema":"hermit-grouped-runtime-source-rights-v1",
        "sequence":sequence,"role":role}),
        rights,
    )
}

fn push(
    frames: &mut Vec<Frame>,
    total_bytes: &mut usize,
    value: Value,
    rights: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let bytes = journal::canonical(&value)?;
    require(
        rights.len() <= 3
            && bytes.len() <= wire::MAX_PACKET
            && frames.len() < 128
            && *total_bytes + bytes.len() <= 1_048_576,
        "completed source export exceeds original packet/receipt/aggregate bounds",
    )?;
    // Captures only descriptive aliases; callers exclusively borrow actual
    // original owners retained by CompletedSourceExport::source.
    let mut captured = Vec::with_capacity(rights.len());
    for right in rights {
        let flags = unsafe { libc::fcntl(right.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            unsafe { libc::fcntl(right.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
            "completed source original descriptor lacks CLOEXEC",
        )?;
        captured.push(CapturedRight {
            descriptor: right.as_raw_fd(),
            identity: owner::stat(right.as_raw_fd())?,
            flags,
        });
    }
    *total_bytes += bytes.len();
    frames.push(Frame {
        bytes,
        rights: captured,
    });
    Ok(())
}
