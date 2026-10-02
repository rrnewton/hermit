//! Independent runtime Keeper for the actual grouped provider. This process
//! owns descriptions received from the original source before its first ACK.
//! An archive authenticates those same descriptions; it cannot replace them.
//! The startup C context is retired during handoff, never given a new clock.
use std::ffi::CString;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

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
use super::owner;
use super::require;
use super::runtime_cleanup::SourceArchive;
use super::runtime_cleanup::{self as common};
use super::wire;
#[path = "runtime_keeper_custody.rs"]
mod custody;
#[path = "runtime_keeper_early.rs"]
mod early;
#[path = "runtime_keeper_source.rs"]
mod source;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cursor {
    Source,
    Provider,
    Keeper,
    Relinquished,
    Refused,
}

#[derive(Debug)]
struct Link {
    channel: wire::Channel,
    pidfd: OwnedFd,
    credentials: wire::Credentials,
}
impl Link {
    fn check(&self) -> io::Result<()> {
        self.channel.validate()?;
        owner::pidfd_matches(self.pidfd.as_raw_fd(), self.credentials.pid)?;
        require(
            !owner::terminal(self.pidfd.as_raw_fd())?,
            "runtime original peer became terminal",
        )
    }
}

#[derive(Debug)]
struct CloseRow {
    fd: i32,
    attempted: bool,
    raw: Option<i32>,
    errno: Option<i32>,
}

#[derive(Debug)]
struct SnapshotRow {
    // Keep the original observation identity alongside its read results.
    _fd: i32,
    rewind: Option<(i64, Option<i32>)>,
    reads: Vec<(isize, Option<i32>)>,
    bytes: Vec<u8>,
    eof: bool,
}

/// Every received right is moved here before parsing. The entry retains the
/// entire state in ManuallyDrop; explicit recorded close calls are the only
/// local alias retirement, including descriptors held inside shared readers.
struct RuntimeKeeper {
    input: wire::Channel,
    parent: Option<wire::Credentials>,
    run: [u8; 16],
    intent: Option<Intent>,
    stage: u64,
    bootstrap: Vec<(Value, Vec<u8>, Vec<OwnedFd>)>,
    local_channels: Vec<Option<wire::Channel>>,
    channel_exports: Vec<OwnedFd>,
    cli_pidfd: Option<OwnedFd>,
    run_pidfd: Option<OwnedFd>,
    startup: Option<Link>,
    provider: Option<Link>,
    bridge: Option<ffi::CleanupBridge>,
    bridge_file: Option<i32>,
    ledger: Option<journal::RemovalJournal>,
    ledger_directory: Option<i32>,
    receipt_directory: Option<OwnedFd>,
    early: Vec<(Value, Vec<u8>, Vec<OwnedFd>)>,
    early_stores: Vec<journal::SourceLedgerReader>,
    early_callbacks: Option<Box<early::EarlyCallbacks>>,
    owned_source: Option<Box<source::OwnedSource>>,
    source_owner: Option<Link>,
    initial_histories: Option<[journal::SourceHistory; 2]>,
    early_prefix_readbacks: Vec<journal::AcknowledgedSourceReadback>,
    prefixes: Vec<early::Prefix>,
    mirrored_index: Option<usize>,
    early_cleanup_complete: bool,
    early_observations: Vec<common::AbsenceObservation>,
    early_cleanup_error: Option<Failure>,
    admission_transferred: bool,
    archive_ack_attempted: bool,
    runtime_commit: Option<Value>,
    runtime_commit_ack_attempted: bool,
    archive: SourceArchive,
    cursor: Cursor,
    startup_context_retired: bool,
    terminal_origin: Option<u64>,
    terminal_cutoff: Option<u64>,
    local_controller_terminal: Option<u64>,
    failure: Option<Failure>,
    failure_origin: Option<u64>,
    failure_receipt_attempted: bool,
    failure_receipt_error: Option<Failure>,
    peer_failure_origin: Option<u64>,
    parent_join_request: Option<Value>,
    parent_join_reply: Option<Value>,
    controller_refusal: Option<Value>,
    controller_custody: Option<custody::ControllerCustody>,
    controller_eof: bool,
    removal_sequence: u64,
    observations: Vec<Value>,
    snapshots: Vec<SnapshotRow>,
    directory_reads: Vec<Value>,
    closes: Vec<CloseRow>,
    census: owner::CensusInventory,
    retirement_started: bool,
}

fn peer_credentials(fd: BorrowedFd<'_>) -> io::Result<wire::Credentials> {
    let mut value: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&value) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    require(
        size as usize == std::mem::size_of_val(&value)
            && value.pid > 1
            && value.pid != unsafe { libc::getpid() },
        "runtime bootstrap native peer differs",
    )?;
    Ok(wire::Credentials {
        pid: value.pid,
        uid: value.uid,
        gid: value.gid,
    })
}

fn bytes32(value: &Value) -> io::Result<[u8; 32]>{
    let text = value
        .as_str()
        .ok_or_else(|| io::Error::other("runtime library digest missing"))?;
    require(
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "runtime library digest malformed",
    )?;
    let mut result = [0; 32];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        result[index] =
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).map_err(io::Error::other)?;
    }
    require(result != [0; 32], "runtime library digest absent")?;
    Ok(result)
}

fn decode_line(value: &Value) -> io::Result<Vec<u8>> {
    let text = value
        .as_str()
        .ok_or_else(|| io::Error::other("runtime removal line missing"))?;
    require(
        text.len() < 512
            && text.len() % 2 == 0
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "runtime removal line exceeds original exact bound",
    )?;
    text.as_bytes()
        .as_chunks::<2>().0.iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).map_err(io::Error::other)
        })
        .collect()
}

impl RuntimeKeeper {
    fn retain(input: OwnedFd, run: [u8; 16], stage: u64) -> Self {
        Self {
            input: wire::Channel::retain(input),
            parent: None,
            run,
            intent: None,
            stage,
            bootstrap: Vec::new(),
            local_channels: Vec::new(),
            channel_exports: Vec::new(),
            cli_pidfd: None,
            run_pidfd: None,
            startup: None,
            provider: None,
            bridge: None,
            bridge_file: None,
            ledger: None,
            ledger_directory: None,
            receipt_directory: None,
            early: Vec::new(),
            early_stores: Vec::new(),
            early_callbacks: None,
            owned_source: None,
            source_owner: None,
            initial_histories: None,
            early_prefix_readbacks: Vec::new(),
            prefixes: Vec::new(),
            mirrored_index: None,
            early_cleanup_complete: false,
            early_observations: Vec::new(),
            early_cleanup_error: None,
            admission_transferred: false,
            archive_ack_attempted: false,
            runtime_commit: None,
            runtime_commit_ack_attempted: false,
            archive: SourceArchive::retain(),
            cursor: Cursor::Source,
            startup_context_retired: false,
            terminal_origin: None,
            terminal_cutoff: None,
            local_controller_terminal: None,
            failure: None,
            failure_origin: None,
            failure_receipt_attempted: false,
            failure_receipt_error: None,
            peer_failure_origin: None,
            parent_join_request: None,
            parent_join_reply: None,
            controller_refusal: None,
            controller_custody: None,
            controller_eof: false,
            removal_sequence: 0,
            observations: Vec::new(),
            snapshots: Vec::new(),
            directory_reads: Vec::new(),
            closes: Vec::new(),
            census: owner::CensusInventory::retain(),
            retirement_started: false,
        }
    }

    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if self.failure.is_none() {
                self.failure = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
            self.cursor = Cursor::Refused;
        }
        result
    }

    fn intent(&self) -> io::Result<&Intent> {
        self.intent
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime original intent absent"))
    }

    fn census(&mut self, cutoff: u64) -> io::Result<()> {
        let now = guardian::monotonic_ns()?;
        require(now < cutoff, "runtime FD census original cutoff expired")?;
        self.census.observe(
            &[self.input.fd.as_raw_fd()],
            Instant::now() + Duration::from_nanos(cutoff - now),
        )?;
        common::before(cutoff)
    }

    fn bootstrap(&mut self) -> io::Result<()> {
        common::before(self.stage)?;
        self.input.validate()?;
        self.parent = Some(peer_credentials(self.input.fd.as_fd())?);
        let peer = self.parent.unwrap();
        for sequence in 0..5 {
            let count = [3, 1, 1, 1, 2][sequence];
            let index = common::receive(&mut self.input, peer, count, 4096, self.stage)?;
            let packet = &mut self.input.packets[index];
            let bytes = packet.bytes.clone();
            self.bootstrap
                .push((Value::Null, bytes, std::mem::take(&mut packet.rights)));
            let current = self.bootstrap.last_mut().unwrap();
            current.0 = serde_json::from_slice(&current.1)?;
            if sequence == 0 {
                let header = &current.0;
                require(
                    header["schema"] == "hermit-grouped-runtime-keeper-bootstrap-v1"
                        && header["run"] == hex(&self.run)
                        && header["stage_deadline"] == self.stage
                        && header["incarnation"]
                            == u64::from_le_bytes(self.run[..8].try_into().unwrap()),
                    "runtime bootstrap changed original run/stage",
                )?;
                let expected = bytes32(&header["library_sha256"])?;
                self.intent = Some(Intent::new(
                    hex(&self.run),
                    header["incarnation"].as_u64().unwrap(),
                )?);
                let mut rights = std::mem::take(&mut current.2).into_iter();
                let file = rights.next().unwrap();
                self.bridge_file = Some(file.as_raw_fd());
                self.bridge = Some(ffi::CleanupBridge::retain(file, expected));
                let directory = rights.next().unwrap();
                self.ledger_directory = Some(directory.as_raw_fd());
                self.ledger = Some(journal::RemovalJournal::retain(
                    directory,
                    self.intent.as_ref().unwrap().clone(),
                ));
                self.cli_pidfd = rights.next();
                owner::pidfd_matches(self.cli_pidfd.as_ref().unwrap().as_raw_fd(), peer.pid)?;
                self.receipt_directory = Some(common::duplicate(unsafe {
                    BorrowedFd::borrow_raw(self.ledger_directory.unwrap())
                })?);
                self.ledger.as_mut().unwrap().initialize("runtime-keeper")?;
                unsafe {
                    self.bridge.as_mut().unwrap().initialize()?;
                }
                self.make_peer_channels()?;
            } else if sequence < 3 {
                let role = if sequence == 1 { "startup" } else { "provider" };
                let pid = current.0["pid"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| io::Error::other("runtime linked native PID malformed"))?;
                require(
                    current.0
                        == json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
                    "sequence":sequence,"role":role,"run":hex(&self.run),
                    "stage_deadline":self.stage,"pid":pid}),
                    "runtime linked role changed",
                )?;
                let mut rights = std::mem::take(&mut current.2).into_iter();
                let pidfd = rights.next().unwrap();
                let channel = self.local_channels[sequence - 1].take().unwrap();
                let link = Link {
                    channel,
                    pidfd,
                    credentials: common::credentials(pid)?,
                };
                if sequence == 1 {
                    self.startup = Some(link);
                } else {
                    self.provider = Some(link);
                }
                let retained = if sequence == 1 {
                    self.startup.as_ref().unwrap()
                } else {
                    self.provider.as_ref().unwrap()
                };
                retained.check()?;
            } else if sequence == 3 {
                require(
                    current.0
                        == json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
                    "sequence":3,"role":"run_controller","run":hex(&self.run),
                    "stage_deadline":self.stage}),
                    "runtime run-controller role changed",
                )?;
                self.run_pidfd = current.2.pop();
                let fd = self.run_pidfd.as_ref().unwrap().as_raw_fd();
                let info = owner::read_file(&format!("/proc/self/fdinfo/{fd}"), 4096)?;
                let values: Vec<_> = info
                    .lines()
                    .filter_map(|line| line.strip_prefix("Pid:"))
                    .collect();
                require(values.len() == 1, "runtime controller is not a real pidfd")?;
                let pid: i32 = values[0].trim().parse().map_err(io::Error::other)?;
                owner::pidfd_matches(fd, pid)?;
            } else {
                let pid = current.0["pid"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| io::Error::other("runtime source-owner PID absent"))?;
                require(
                    current.0
                        == json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
                    "sequence":4,"role":"source_owner","run":hex(&self.run),
                    "stage_deadline":self.stage,"pid":pid}),
                    "runtime source-owner role differs",
                )?;
                let mut rights = std::mem::take(&mut current.2).into_iter();
                let pidfd = rights.next().unwrap();
                let channel = wire::Channel::retain(rights.next().unwrap());
                self.source_owner = Some(Link {
                    channel,
                    pidfd,
                    credentials: common::credentials(pid)?,
                });
                let retained = self.source_owner.as_ref().unwrap();
                retained.check()?;
                require(
                    peer_credentials(retained.channel.fd.as_fd())? == retained.credentials,
                    "runtime source-owner channel was not created by the actual owner",
                )?;
                owner::check_source_authority_policy(
                    pid,
                    retained.credentials.uid,
                    retained.credentials.gid,
                )?;
            }
            common::before(self.stage)?;
        }
        self.census(self.stage)?;
        owner::check_no_children()?;
        common::send(
            &mut self.input,
            &json!({"schema":"hermit-grouped-runtime-keeper-bootstrap-ready-v1",
            "run":hex(&self.run),"incarnation":self.intent.as_ref().unwrap().incarnation,
            "stage_deadline":self.stage}),
            &[],
            self.stage,
        )
    }

    fn make_peer_channels(&mut self) -> io::Result<()> {
        require(
            self.local_channels.is_empty() && self.channel_exports.is_empty(),
            "runtime native peer pairs cannot repeat",
        )?;
        for kind in ["creation", "provider"] {
            common::before(self.stage)?;
            let mut fds = [-1; 2];
            if unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // Both real returned owners enter retained state before options,
            // a transfer or any later operation can fail.
            self.local_channels.push(Some(wire::Channel::retain(unsafe {
                OwnedFd::from_raw_fd(fds[0])
            })));
            self.channel_exports
                .push(unsafe { OwnedFd::from_raw_fd(fds[1]) });
            let enabled = 1i32;
            for fd in fds {
                if unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_PASSCRED,
                        (&enabled as *const i32).cast(),
                        std::mem::size_of_val(&enabled) as libc::socklen_t,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            let intent = self.intent.as_ref().unwrap();
            let value = json!({"schema":format!("hermit-grouped-runtime-{kind}-channel-v1"),
                "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage});
            common::send(
                &mut self.input,
                &value,
                &[self.channel_exports.last().unwrap().as_fd()],
                self.stage,
            )?;
            let exported = self.channel_exports.last().unwrap().as_raw_fd();
            // The successful send retained an actual SCM_RIGHTS copy at the
            // receiving socket. The local exported endpoint must not mask EOF.
            self.retain_close(exported, self.stage)?;
        }
        Ok(())
    }

    fn receive_early(&mut self) -> io::Result<()> {
        let intent = self.intent()?.clone();
        for sequence in 0..5 {
            common::before(self.stage)?;
            let link = self.startup.as_mut().unwrap();
            link.check()?;
            let index = common::receive(
                &mut link.channel,
                link.credentials,
                [3, 3, 2, 2, 0][sequence],
                4096,
                self.stage,
            )?;
            let packet = &mut link.channel.packets[index];
            self.early.push((
                Value::Null,
                packet.bytes.clone(),
                std::mem::take(&mut packet.rights),
            ));
            let frame = self.early.last_mut().unwrap();
            frame.0 = serde_json::from_slice(&frame.1)?;
            if sequence == 0 {
                let keeper = frame.0["keeper_pid"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| io::Error::other("runtime original source Keeper PID absent"))?;
                require(
                    frame.0
                        == json!({"schema":"hermit-grouped-runtime-creation-custody-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
                    "creator":frame.0["creator"],"keeper_pid":keeper,
                    "initial_guardian_store":frame.0["initial_guardian_store"],
                    "initial_keeper_store":frame.0["initial_keeper_store"]}),
                    "runtime first-ACK creator identity changed",
                )?;
                common::check_creator(&frame.2[..2], &frame.0["creator"], false)?;
                owner::pidfd_matches(frame.2[2].as_raw_fd(), keeper)?;
                require(
                    !owner::terminal(frame.2[2].as_raw_fd())?,
                    "runtime original source Keeper already terminal",
                )?;
            } else if sequence < 4 {
                let role = ["", "controls", "guardian_store", "keeper_store"][sequence];
                require(
                    frame.0
                        == json!({"schema":"hermit-grouped-runtime-creation-rights-v1",
                    "sequence":sequence,"role":role,"nonce":intent.nonce,
                    "incarnation":intent.incarnation,"stage_deadline":self.stage}),
                    "runtime first-ACK role changed",
                )?;
                if sequence == 1 {
                    common::check_controls(&frame.2)?;
                } else {
                    self.early_stores.push(journal::SourceLedgerReader::retain(
                        std::mem::take(&mut frame.2),
                        intent.clone(),
                    ));
                    self.early_stores.last_mut().unwrap().initialize()?;
                }
            }
        }
        let mut digest = Sha256::new();
        let mut count = 0usize;
        for (_, bytes, _) in &self.early[..4] {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
            count += bytes.len();
        }
        let expected = json!({"schema":"hermit-grouped-runtime-creation-end-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
            "preceding_frames":4,"preceding_bytes":count,"framed_sha256":hex(&digest.finalize())});
        require(
            self.early[4].0 == expected,
            "runtime first-ACK custody transcript changed",
        )?;
        self.check_owned_source_creator(&self.early[0].0["creator"].clone())?;
        self.install_early_cleanup()?;
        self.forward_early_custody()?;
        common::check_creator(&self.early[0].2[..2], &self.early[0].0["creator"], false)?;
        let status = self.bridge.as_ref().unwrap().status().ok_or_else(|| {
            io::Error::other("runtime early C preparation has no actual readback")
        })?;
        require(
            status.prepared == 1 && status.refused == 0 && status.stage_cutoff == self.stage,
            "runtime early C preparation did not succeed",
        )?;
        self.ledger.as_mut().unwrap().store.append(
            json!({"kind":"actual-first-ACK-cleanup-custody",
            "source":self.early[0].0,"end":expected,"C_prepared":status.prepared,
            "C_stage_cutoff":status.stage_cutoff}),
        )?;
        self.census(self.stage)?;
        let mut ack = expected;
        ack["schema"] = json!("hermit-grouped-runtime-creation-custody-ack-v1");
        let link = self.startup.as_mut().unwrap();
        link.check()?;
        let fds = &self.early[1].2;
        common::send(
            &mut link.channel,
            &ack,
            &[fds[0].as_fd(), fds[1].as_fd(), fds[2].as_fd()],
            self.stage,
        )
    }

    fn receive_completed(&mut self, first: usize) -> io::Result<()> {
        let intent = self.intent()?.clone();
        require(
            self.mirrored_index == Some(33)
                && self.prefixes.len() == 34
                && !self.admission_transferred,
            "runtime completed archive precedes all34 actual mirrored callbacks",
        )?;
        self.check_owned_source_completed()?;
        self.check_s2_guardian_registered(false)?;
        let link = self.provider.as_mut().unwrap();
        link.check()?;
        let peer = link.credentials;
        let stage = self.stage;
        let mut notice = None;
        let received = self.archive.receive_started_inspecting(
            &mut link.channel,
            peer,
            &intent,
            stage,
            first,
            |packet| {
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                if value["schema"] == "hermit-grouped-runtime-creation-peer-refused-v1" {
                    packet.exact(0, peer)?;
                    let origin = value["first_failure_origin"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("mid-archive peer origin absent"))?;
                    let cause = value["cause"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| io::Error::other("mid-archive peer cause absent"))?
                        .to_owned();
                    require(
                        value
                            == json!({"schema":"hermit-grouped-runtime-creation-peer-refused-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,
                    "first_failure_origin":origin,"cause":cause})
                            && journal::canonical(&value)? == packet.bytes
                            && origin > 0
                            && origin <= guardian::monotonic_ns()?,
                        "mid-archive peer refusal changed original identity",
                    )?;
                    notice = Some(value);
                    return Err(io::Error::other(cause));
                }
                Ok(())
            },
        );
        if let Some(value) = notice {
            return Err(self.retain_peer_failure(&value)?);
        }
        received?;
        for index in 0..3 {
            common::same_ofd(
                self.early[1].2[index].as_fd(),
                self.archive.controls()?[index],
            )?;
        }
        for index in 0..2 {
            let old = self.early_stores[index].held_rights();
            let new = self.archive.readers[index].held_rights();
            require(
                old.len() == 2 && new.len() == 2,
                "runtime continuous Store population changed",
            )?;
            for role in 0..2 {
                common::same_ofd(old[role].as_fd(), new[role].as_fd())?;
            }
            let name = if index == 0 {
                "guardian_store"
            } else {
                "keeper_store"
            };
            let final_history = self.early_stores[index].read(&self.archive.frames[0].0[name])?;
            self.check_completed_prefix(index, &final_history)?;
        }
        require(
            self.early[0].0["creator"] == self.archive.frames[0].0["guardian"]["creator"],
            "runtime completed source replaced first-ACK Creator",
        )?;
        common::check_creator(&self.early[0].2[..2], &self.early[0].0["creator"], true)?;
        require(
            self.cursor == Cursor::Source,
            "runtime cursor transfer repeated",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-complete-source-handoff",
            "end":self.archive.acknowledgement("hermit-grouped-runtime-keeper-custody-v1")?}))?;
        // This first ACK confirms actual archive custody only. The original
        // prepared cleanup context and private admission owner remain live
        // until the service has installed its real callback and joined startup.
        self.census(self.stage)?;
        self.acknowledge_owned_source_archive()?;
        let ack = self
            .archive
            .acknowledgement("hermit-grouped-runtime-keeper-custody-v1")?;
        let controls = self.archive.controls()?;
        let link = self.provider.as_mut().unwrap();
        link.check()?;
        require(
            !self.archive_ack_attempted,
            "runtime archive ACK cannot repeat",
        )?;
        self.archive_ack_attempted = true;
        common::send(&mut link.channel, &ack, &controls, self.stage)
    }

    fn await_runtime_commit(&mut self) -> io::Result<()> {
        require(
            self.archive_ack_attempted
                && !self.admission_transferred
                && self.runtime_commit.is_none()
                && !self.startup_context_retired
                && self.cursor == Cursor::Source,
            "runtime commit lacks retained pre-open custody",
        )?;
        // The service owns the actual startup Child join through its retained
        // parent channel. A custody ACK alone never grants its Provider lease.
        let value = loop {
            common::before(self.stage)?;
            self.poll_controller_custody(self.stage)?;
            if self.controller_refusal.is_some() {
                return Err(self.failure.as_ref().unwrap().error());
            }
            self.provider.as_ref().unwrap().check()?;
            let peer = self.provider.as_mut().unwrap();
            if let Some(index) = peer.channel.receive(4096)? {
                let packet = &peer.channel.packets[index];
                packet.exact(0, peer.credentials)?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    packet.bytes == journal::canonical(&value)?,
                    "runtime commit is not canonical",
                )?;
                if value["schema"] == "hermit-grouped-runtime-creation-peer-refused-v1" {
                    return Err(self.retain_peer_failure(&value)?);
                }
                break value;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        self.runtime_commit = Some(value.clone()); // retain before validation/effects
        let intent = self.intent()?.clone();
        require(
            value
                == json!({"schema":"hermit-grouped-runtime-provider-commit-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,"sequence":1}),
            "runtime pre-open commit changed original identity or stage",
        )?;
        require(
            owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())?,
            "runtime pre-open commit precedes actual original startup terminality",
        )?;
        self.check_owned_source_completed()?;
        self.check_s2_guardian_registered(true)?;
        common::check_creator(&self.early[0].2[..2], &self.early[0].0["creator"], true)?;
        self.archive
            .acknowledgement("hermit-grouped-runtime-keeper-custody-v1")?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-runtime-pre-open-commit",
            "value":value}))?;
        // Retire this exact old context while its original startup bound still
        // holds. The continuously owned original controls/Stores stay here.
        common::before(self.stage)?;
        unsafe {
            self.bridge.as_mut().unwrap().release_aliases()?;
        }
        common::before(self.stage)?;
        self.write_file(
            "early-cleanup-context.txt",
            format!(
                "status={:?}\nhistory={:?}\nattempts={:?}\nreadbacks={:?}\n",
                self.bridge.as_ref().unwrap().status(),
                self.bridge.as_ref().unwrap().history(),
                self.bridge.as_ref().unwrap().attempts(),
                self.bridge.as_ref().unwrap().readbacks()
            )
            .as_bytes(),
            self.stage,
        )?;
        unsafe {
            self.bridge.as_mut().unwrap().free()?;
        }
        common::before(self.stage)?;
        require(
            !self.bridge.as_ref().unwrap().context_retained()
                && !self.bridge.as_ref().unwrap().loader_retained(),
            "runtime old C context remains live",
        )?;
        self.write_file(
            "early-cleanup-free.txt",
            format!(
                "attempts={:#?}\nloader_close={:#?}\n",
                self.bridge.as_ref().unwrap().attempts(),
                self.bridge.as_ref().unwrap().loader_close()
            )
            .as_bytes(),
            self.stage,
        )?;
        self.startup_context_retired = true;
        self.transfer_early_admission()?;
        self.cursor = Cursor::Provider;
        self.census(self.stage)?;
        let mut ack = value;
        ack["schema"] = json!("hermit-grouped-runtime-provider-commit-ack-v1");
        self.runtime_commit_ack_attempted = true;
        let link = self.provider.as_mut().unwrap();
        link.check()?;
        common::send(&mut link.channel, &ack, &[], self.stage)
    }

    fn observe_controller(&mut self) -> io::Result<bool> {
        let terminal = owner::terminal(
            self.run_pidfd
                .as_ref()
                .ok_or_else(|| io::Error::other("runtime original controller pidfd absent"))?
                .as_raw_fd(),
        )?;
        if terminal && self.local_controller_terminal.is_none() {
            self.local_controller_terminal = Some(guardian::monotonic_ns()?);
        }
        Ok(terminal)
    }

    fn begin_terminal(&mut self) -> io::Result<()> {
        require(
            self.startup_context_retired
                && self.cursor == Cursor::Provider
                && self.terminal_origin.is_none(),
            "runtime terminal precedes real handoff or repeats",
        )?;
        let request = loop {
            if self.observe_controller()? {
                common::before(
                    self.local_controller_terminal
                        .unwrap()
                        .checked_add(1_000_000_000)
                        .ok_or_else(|| {
                            io::Error::other("runtime local terminal cutoff overflow")
                        })?,
                )?;
            }
            let link = self.provider.as_mut().unwrap();
            link.check()?;
            if let Some(index) = link.channel.receive(4096)? {
                let packet = &link.channel.packets[index];
                packet.exact(0, link.credentials)?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&value)? == packet.bytes,
                    "runtime terminal request is not canonical",
                )?;
                break value;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let origin = request["original_start"]
            .as_u64()
            .ok_or_else(|| io::Error::other("runtime original release origin absent"))?;
        let cutoff = request["cutoff"]
            .as_u64()
            .ok_or_else(|| io::Error::other("runtime original release cutoff absent"))?;
        // Pin before native checks or durability can fail. No retry/refresh.
        self.terminal_origin = Some(origin);
        self.terminal_cutoff = Some(cutoff);
        let now = guardian::monotonic_ns()?;
        let intent = self.intent()?;
        require(
            request
                == json!({"schema":"hermit-grouped-runtime-terminal-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"sequence":1,
            "original_start":origin,"cutoff":cutoff})
                && origin != 0
                && origin <= now
                && cutoff > now
                && cutoff
                    <= origin
                        .checked_add(1_000_000_000)
                        .ok_or_else(|| io::Error::other("runtime original release overflow"))?,
            "runtime terminal identity or original one-second bound differs",
        )?;
        require(
            self.observe_controller()?,
            "runtime controller still live at terminal request",
        )?;
        require(
            cutoff
                <= self
                    .local_controller_terminal
                    .unwrap()
                    .checked_add(1_000_000_000)
                    .ok_or_else(|| io::Error::other("runtime local release cutoff overflow"))?,
            "runtime peer extended earlier observed controller cutoff",
        )?;
        self.check_terminal()?;
        self.ledger.as_mut().unwrap().store.append(
            json!({"kind":"actual-controller-terminal-release",
            "request":request,"native_controller_terminal":true,
            "local_first_terminal":self.local_controller_terminal}),
        )?;
        let mut ack = request;
        ack["schema"] = json!("hermit-grouped-runtime-terminal-ack-v1");
        self.send_provider(&ack)
    }

    fn check_terminal(&self) -> io::Result<u64> {
        if let Some(failure) = &self.failure {
            return Err(failure.error());
        }
        let cutoff = self
            .terminal_cutoff
            .ok_or_else(|| io::Error::other("runtime terminal cutoff absent"))?;
        common::before(cutoff)?;
        require(
            owner::terminal(self.run_pidfd.as_ref().unwrap().as_raw_fd())?,
            "runtime original controller is not terminal",
        )?;
        self.provider.as_ref().unwrap().check()?;
        require(
            owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())?,
            "runtime original startup helper remains live at terminal release",
        )?;
        common::check_creator(&self.early[0].2[..2], &self.early[0].0["creator"], true)?;
        Ok(cutoff)
    }

    fn send_provider(&mut self, value: &Value) -> io::Result<()> {
        let cutoff = self.check_terminal()?;
        common::send(
            &mut self.provider.as_mut().unwrap().channel,
            value,
            &[],
            cutoff,
        )?;
        self.check_terminal().map(|_| ())
    }

    fn receive_provider(&mut self) -> io::Result<Value> {
        let cutoff = self.check_terminal()?;
        let link = self.provider.as_mut().unwrap();
        let result = common::receive_value(&mut link.channel, link.credentials, cutoff)?;
        self.check_terminal()?;
        Ok(result)
    }

    fn removal(&mut self, request: Value) -> io::Result<()> {
        let cutoff = self.check_terminal()?;
        require(
            self.cursor == Cursor::Provider && self.removal_sequence < 34,
            "runtime removal lacks original provider cursor or exceeds34",
        )?;
        let intent = self.intent()?.clone();
        let sequence = self.removal_sequence + 1;
        let owner: journal::OwnerSnapshot = serde_json::from_value(request["owner"].clone())?;
        let write: journal::Write = serde_json::from_value(request["write"].clone())?;
        let line = decode_line(&request["line"])?;
        require(
            request
                == json!({"schema":"hermit-cleanup-remove-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"sequence":sequence,"owner":owner,"write":write,
            "line":hex(&line),"cutoff":cutoff}),
            "runtime removal identity/sequence/cutoff changed",
        )?;
        require(
            write.role == 17 - (self.removal_sequence / 2) as u32,
            "runtime removal skipped an original full-history role",
        )?;
        self.ledger.as_mut().unwrap().append(owner, write, &line)?;
        self.removal_sequence = sequence;
        self.send_provider(
            &json!({"schema":"hermit-cleanup-remove-ack-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"sequence":sequence,"cutoff":cutoff}),
        )
    }

    fn snapshot(&mut self, index: usize) -> io::Result<usize> {
        require(
            index < 2 && self.cursor == Cursor::Keeper,
            "runtime read lacks granted original epoch",
        )?;
        self.check_terminal()?;
        common::check_controls(&self.early[1].2)?;
        let fd = self.early[1].2[index].as_raw_fd();
        require(
            self.snapshots.len() < 4,
            "runtime original two complete scans exhausted",
        )?;
        let retained = self.snapshots.len();
        self.snapshots.push(SnapshotRow {
            _fd: fd,
            rewind: None,
            reads: Vec::new(),
            bytes: Vec::new(),
            eof: false,
        });
        let raw = unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        self.snapshots[retained].rewind = Some((raw, error));
        require(raw == 0, "runtime original seq-file rewind failed")?;
        loop {
            require(
                self.cursor == Cursor::Keeper,
                "runtime read epoch relinquished",
            )?;
            self.check_terminal()?;
            common::check_controls(&self.early[1].2)?;
            let mut buffer = [0u8; 4096];
            let cap = buffer
                .len()
                .min(1_048_577usize.saturating_sub(self.snapshots[retained].bytes.len()));
            require(cap > 0, "runtime full census exceeds original1MiB")?;
            let raw = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), cap) };
            let error = if raw < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            self.snapshots[retained].reads.push((raw, error));
            if raw < 0 {
                return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO)));
            }
            require(
                raw as usize <= cap,
                "runtime native read exceeded submitted extent",
            )?;
            if raw > 0 {
                self.snapshots[retained]
                    .bytes
                    .extend_from_slice(&buffer[..raw as usize]);
            } else {
                self.snapshots[retained].eof = true;
            }
            self.check_terminal()?;
            if raw == 0 {
                return Ok(retained);
            }
            require(
                self.snapshots[retained].bytes.len() <= 1_048_576,
                "runtime full census exceeds original1MiB",
            )?;
        }
    }

    fn scan_absent(&mut self, index: usize) -> io::Result<Value> {
        let cutoff = self.check_terminal()?;
        let started = guardian::monotonic_ns()?;
        let definitions = self.snapshot(0)?;
        let profile = self.snapshot(1)?;
        let definitions = self.snapshots[definitions].bytes.clone();
        let profile = self.snapshots[profile].bytes.clone();
        let intent = self.intent()?.clone();
        owner::check_runtime_absence(&intent, &definitions, &profile, self.early[1].2[2].as_fd())?;
        let mut directories = Vec::new();
        for name in [
            intent.group(),
            format!("{}/{}", intent.group(), intent.event()),
        ] {
            self.check_terminal()?;
            require(
                self.cursor == Cursor::Keeper,
                "runtime directory read lacks epoch",
            )?;
            let name = CString::new(name).unwrap();
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let raw = unsafe {
                libc::fstatat(
                    self.early[1].2[2].as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            let error = if raw < 0 {
                Some(io::Error::last_os_error().raw_os_error())
            } else {
                None
            };
            let row = json!({"path":name.to_str().unwrap(),"attempted":true,"returned":true,
                "raw":raw,"errno":error.flatten()});
            self.directory_reads.push(row.clone());
            directories.push(row);
            require(
                raw == -1 && error.flatten() == Some(libc::ENOENT),
                "runtime original event directory remains",
            )?;
            self.check_terminal()?;
        }
        self.write_file(
            &format!("absence-{index}-definitions"),
            &definitions,
            cutoff,
        )?;
        self.write_file(&format!("absence-{index}-profile"), &profile, cutoff)?;
        let finished = guardian::monotonic_ns()?;
        let observation = json!({"schema":"hermit-grouped-runtime-absence-v1",
            "started":started,"completed":finished,"cutoff":cutoff,
            "definitions":{"bytes":definitions.len(),"sha256":hex(&Sha256::digest(&definitions))},
            "profile":{"bytes":profile.len(),"sha256":hex(&Sha256::digest(&profile))},
            "directories":directories});
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-runtime-absence",
            "index":index,"observation":observation}))?;
        self.check_terminal()?;
        Ok(observation)
    }

    fn read_grant(&mut self, request: Value) -> io::Result<()> {
        let cutoff = self.check_terminal()?;
        let intent = self.intent()?.clone();
        require(
            self.removal_sequence == 34
                && self.cursor == Cursor::Provider
                && self.observations.is_empty()
                && request
                    == json!({
                "schema":"hermit-grouped-runtime-read-grant-v1","nonce":intent.nonce,
                "incarnation":intent.incarnation,"epoch":1,
                "original_start":self.terminal_origin,"cutoff":cutoff}),
            "runtime read grant is early/repeated or changed original owner",
        )?;
        self.ledger.as_mut().unwrap().complete()?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-runtime-read-grant","request":request}))?;
        self.cursor = Cursor::Keeper;
        for index in 0..2 {
            let row = self.scan_absent(index)?;
            self.observations.push(row);
        }
        // Ambiguous send can never reclaim this epoch or permit another read.
        self.cursor = Cursor::Relinquished;
        self.send_provider(&json!({"schema":"hermit-grouped-runtime-read-done-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"epoch":1,
            "original_start":self.terminal_origin,"cutoff":cutoff,"observations":self.observations}))
    }

    fn channel_failure_record(channel: &wire::Channel) -> Value {
        json!({"fd":channel.fd.as_raw_fd(),"received_packets":channel.packets.len(),
            "send_attempts":channel.sends.len(),
            "refused":channel.refused.as_ref().map(|f|json!({"message":f.message,
                "kind":format!("{:?}",f.kind),"errno":f.errno})),
            "last_receive":channel.last_receive.map(|r|json!({"raw":r.returned,"errno":r.errno})),
            "last_packet":channel.packets.last().map(|p|json!({
                "raw":p.raw.returned,"errno":p.raw.errno,"flags":p.flags,
                "rights_messages":p.rights_messages,"rights":p.rights.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
                "credentials":p.credentials.iter().map(|c|json!({"pid":c.pid,"uid":c.uid,"gid":c.gid})).collect::<Vec<_>>(),
                "bytes":p.bytes.len(),"sha256":hex(&Sha256::digest(&p.bytes)),"bytes_hex":hex(&p.bytes)})),
            "last_send":channel.sends.last().map(|s|json!({"bytes":s.bytes.len(),
                "sha256":hex(&Sha256::digest(&s.bytes)),"rights":s.rights,
                "raw":s.raw.map(|r|r.returned),"errno":s.raw.and_then(|r|r.errno)}))})
    }

    fn record_creation_failure(&mut self, primary: &io::Error) -> io::Result<()> {
        require(
            !self.failure_receipt_attempted,
            "runtime first failure receipt cannot repeat",
        )?;
        self.failure_receipt_attempted = true;
        // The caller latches the first cause and its origin before any receipt
        // work. Diagnostics consume the same original one-second recovery
        // window; they neither refresh it nor confer cleanup authority.
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("runtime first failure origin absent"))?;
        let earliest = self
            .peer_failure_origin
            .map_or(origin, |other| other.min(origin));
        let cutoff = self.stage.min(
            earliest
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("runtime first failure cutoff overflow"))?,
        );
        common::before(cutoff)?;
        let first = self
            .failure
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime first failure cause absent"))?;
        let bytes = journal::canonical(
            &json!({"schema":"hermit-grouped-runtime-keeper-first-failure-v1",
            "run":hex(&self.run),"stage_deadline":self.stage,"first_failure_origin":origin,
            "peer_failure_origin":self.peer_failure_origin,"diagnostic_cutoff":cutoff,
            "first_failure":{"message":first.message,"kind":format!("{:?}",first.kind),"errno":first.errno},
            "creation_error":{"message":primary.to_string(),"kind":format!("{:?}",primary.kind()),"errno":primary.raw_os_error()},
            "cursor":format!("{:?}",self.cursor),"mirrored_index":self.mirrored_index,
            "prefixes":self.prefixes.len(),"early_frames":self.early.len(),
            "source_proxy_retained":self.owned_source.is_some(),"controller_refusal":self.controller_refusal,
            "controller_eof":self.controller_eof,"admission_transferred":self.admission_transferred,
            "archive_ack_attempted":self.archive_ack_attempted,"runtime_commit":self.runtime_commit,
            "runtime_commit_ack_attempted":self.runtime_commit_ack_attempted,
            "startup_context_retired":self.startup_context_retired,
            "archive_frames":self.archive.frames.iter().map(|(_,bytes,rights)|json!({
                "bytes":bytes.len(),"sha256":hex(&Sha256::digest(bytes)),
                "rights":rights.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "ledger":self.ledger.as_ref().map(|l|json!({"bytes":l.store.content.len(),
                "sha256":hex(&Sha256::digest(&l.store.content)),"refused":l.store.refused.as_ref().map(|f|json!({
                    "message":f.message,"kind":format!("{:?}",f.kind),"errno":f.errno}))})),
            "input":Self::channel_failure_record(&self.input),
            "startup":self.startup.as_ref().map(|l|Self::channel_failure_record(&l.channel)),
            "provider":self.provider.as_ref().map(|l|Self::channel_failure_record(&l.channel)),
            "source_owner":self.source_owner.as_ref().map(|l|Self::channel_failure_record(&l.channel))}),
        )?;
        let name = "runtime-keeper-first-failure.json";
        self.write_file(name, &bytes, cutoff)?;
        // A refused Store remains refused. The independent file carries the
        // bounded detail; a healthy original journal receives only its hash.
        if let Some(ledger) = self.ledger.as_mut().filter(|l| l.store.refused.is_none()) {
            ledger.store.append(
                json!({"kind":"actual-runtime-first-failure-file","file":name,
                "bytes":bytes.len(),"sha256":hex(&Sha256::digest(&bytes))}),
            )?;
        }
        common::before(cutoff)
    }

    fn write_file(&mut self, name: &str, bytes: &[u8], cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        require(
            bytes.len() <= 1_048_576 && !name.contains('/'),
            "runtime receipt original file bound differs",
        )?;
        let directory = self.receipt_directory.as_ref().unwrap().as_raw_fd();
        let name = CString::new(name).map_err(io::Error::other)?;
        let fd = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Occupy the raw owner row immediately. On any failure it remains here
        // for explicit bounded disposal, never silently closed by stack unwind.
        let row = self.closes.len();
        self.closes.push(CloseRow {
            fd,
            attempted: false,
            raw: None,
            errno: None,
        });
        let mut done = 0;
        while done < bytes.len() {
            common::before(cutoff)?;
            let cap = (bytes.len() - done).min(4096);
            let raw = unsafe {
                libc::pwrite(fd, bytes[done..].as_ptr().cast(), cap, done as libc::off_t)
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(raw > 0, "runtime receipt write made no progress")?;
            done += raw as usize;
        }
        common::before(cutoff)?;
        if unsafe { libc::fsync(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        common::before(cutoff)?;
        if unsafe { libc::fsync(directory) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.close_row(row, cutoff)
    }

    fn close_row(&mut self, index: usize, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        let row = &mut self.closes[index];
        require(!row.attempted, "runtime alias close cannot repeat")?;
        row.attempted = true;
        let raw = unsafe { libc::close(row.fd) };
        row.raw = Some(raw);
        row.errno = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        if raw < 0 {
            return Err(io::Error::from_raw_os_error(row.errno.unwrap_or(libc::EIO)));
        }
        common::before(cutoff)
    }

    fn retain_close(&mut self, fd: i32, cutoff: u64) -> io::Result<()> {
        let index = self.closes.len();
        self.closes.push(CloseRow {
            fd,
            attempted: false,
            raw: None,
            errno: None,
        });
        self.close_row(index, cutoff)
    }

    fn retire(&mut self, request: Value) -> io::Result<()> {
        let cutoff = self.check_terminal()?;
        let intent = self.intent()?.clone();
        require(
            !self.retirement_started
                && self.cursor == Cursor::Relinquished
                && self.observations.len() == 2
                && self.removal_sequence == 34
                && request
                    == json!({"schema":"hermit-grouped-runtime-peer-retire-v1",
                "nonce":intent.nonce,"incarnation":intent.incarnation,
                "original_start":self.terminal_origin,"cutoff":cutoff}),
            "runtime alias retirement is early or repeated",
        )?;
        self.retirement_started = true;
        self.ledger.as_mut().unwrap().complete()?;
        self.census(cutoff)?;
        owner::check_no_children()?;
        let mut descriptors = Vec::new();
        for (_, _, rights) in self
            .bootstrap
            .iter()
            .chain(&self.early)
            .chain(&self.archive.frames)
        {
            descriptors.extend(rights.iter().map(AsRawFd::as_raw_fd));
        }
        for reader in self.early_stores.iter().chain(&self.archive.readers) {
            descriptors.extend(reader.held_rights().iter().map(AsRawFd::as_raw_fd));
        }
        descriptors.extend(self.early_native_descriptors());
        descriptors.extend(self.owned_source_descriptors()?);
        for channel in [
            &self.input,
            &self.startup.as_ref().unwrap().channel,
            &self.provider.as_ref().unwrap().channel,
        ] {
            for packet in &channel.packets {
                descriptors.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
            }
        }
        for fd in [&self.cli_pidfd, &self.run_pidfd] {
            descriptors.push(fd.as_ref().unwrap().as_raw_fd());
        }
        descriptors.push(self.bridge_file.unwrap());
        descriptors.push(self.startup.as_ref().unwrap().pidfd.as_raw_fd());
        descriptors.push(self.startup.as_ref().unwrap().channel.fd.as_raw_fd());
        descriptors.sort_unstable();
        require(
            descriptors.windows(2).all(|pair| pair[0] != pair[1]),
            "runtime owned aliases duplicated by number",
        )?;
        for fd in descriptors {
            self.retain_close(fd, cutoff)?;
        }
        for (chunk, rows) in self.closes.chunks(16).enumerate() {
            self.ledger.as_mut().unwrap().store.append(
                json!({"kind":"actual-runtime-local-alias-retirement",
                "chunk":chunk,"closes":rows.iter().map(|r|json!({"fd":r.fd,"attempted":r.attempted,
                    "raw":r.raw,"errno":r.errno})).collect::<Vec<_>>()}),
            )?;
        }
        self.ledger.as_mut().unwrap().store.verify()?;
        common::before(cutoff)?;
        owner::check_no_children()?;
        // Descriptor census is owned from its first observation; the final
        // readback includes it. No disappearing-census-descriptor exemption.
        self.census(cutoff)?;
        let final_fds = self.census.last_descriptors()?;
        let ledger = self
            .ledger
            .as_ref()
            .unwrap()
            .store
            .file
            .as_ref()
            .unwrap()
            .as_raw_fd();
        let directory = self.ledger_directory.unwrap();
        let receipt = self.receipt_directory.as_ref().unwrap().as_raw_fd();
        let census = self.census.held_descriptor()?;
        let channel = self.provider.as_ref().unwrap().channel.fd.as_raw_fd();
        let peer_pin = self.provider.as_ref().unwrap().pidfd.as_raw_fd();
        let input = self.input.fd.as_raw_fd();
        let mut expected = vec![
            0, 1, 2, ledger, directory, receipt, census, channel, peer_pin, input,
        ];
        expected.sort_unstable();
        expected.dedup();
        require(
            final_fds == expected,
            "runtime final FD population contains an unowned alias",
        )?;
        for fd in [ledger, directory, receipt, census] {
            self.retain_close(fd, cutoff)?;
        }
        let ack = json!({"schema":"hermit-grouped-runtime-peer-retired-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,
            "original_start":self.terminal_origin,"cutoff":cutoff});
        self.provider.as_ref().unwrap().check()?;
        common::send(
            &mut self.provider.as_mut().unwrap().channel,
            &ack,
            &[],
            cutoff,
        )?;
        self.retain_close(peer_pin, cutoff)?;
        self.retain_close(channel, cutoff)?;
        self.retain_close(input, cutoff)?;
        owner::check_no_children()?;
        common::before(cutoff)
    }

    fn run(&mut self) -> io::Result<()> {
        self.bootstrap()?;
        let creation = (|| {
            self.launch_owned_source()?;
            self.receive_early()?;
            self.drive_creation()?;
            self.await_runtime_commit()
        })();
        if let Err(error) = creation {
            self.recover_failed_creation(&error);
            return Err(error);
        }
        self.begin_terminal()?;
        loop {
            let request = self.receive_provider()?;
            match request["schema"].as_str() {
                Some("hermit-cleanup-remove-v1") => self.removal(request)?,
                Some("hermit-grouped-runtime-read-grant-v1") => self.read_grant(request)?,
                Some("hermit-grouped-runtime-peer-retire-v1") => return self.retire(request),
                _ => {
                    return Err(io::Error::other(
                        "runtime peer operation is not in the retained terminal protocol",
                    ));
                }
            }
        }
    }
}

/// Private actual helper entry. Caller supplies the authenticated CLI bootstrap
/// endpoint and original startup deadline. No JSON/boolean constructor can mint
/// the source, cursor or runtime cleanup owner used above.
///
/// # Safety
/// Invoke only as the dedicated authenticated helper process: this changes
/// process-wide limits and subreaper state, then exits without unwinding.
/// The caller must transfer the original bootstrap endpoint and must not
/// depend on any other thread or Rust destructor running in this process.
pub unsafe fn run_grouped_runtime_keeper_process(input: OwnedFd, run: [u8; 16], stage: u64) -> ! {
    let mut state = ManuallyDrop::new(RuntimeKeeper::retain(input, run, stage));
    let result = (|| {
        require(run != [0; 16], "runtime run nonce absent")?;
        let now = guardian::monotonic_ns()?;
        require(
            stage > now && stage - now <= 20_000_000_000,
            "runtime original startup bound differs",
        )?;
        // Reassert the shared capability-unit bound. Parent launch custody checks
        // this keeper against the same exact value as the source and provider.
        let bound = crate::network_runtime::capability_unit::CAPABILITY_UNIT_NOFILE;
        let limit = libc::rlimit {
            rlim_cur: bound,
            rlim_max: bound,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
            || unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        owner::protected_holder()?;
        owner::check_no_children()?;
        state.run()
    })();
    let result = state.remember(result);
    // Bounded actual result and retained custody state go to the official
    // parent's bounded stream receipt. Error exit is never cleanup success.
    eprintln!(
        "{}",
        json!({"schema":"hermit-grouped-runtime-keeper-result-v1",
        "run":hex(&run),"success":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),
        "failure_origin":state.failure_origin,"terminal_origin":state.terminal_origin,
        "failure_receipt_attempted":state.failure_receipt_attempted,
        "failure_receipt_error":state.failure_receipt_error.as_ref().map(|f|f.error().to_string()),
        "terminal_cutoff":state.terminal_cutoff,"removal_callbacks":state.removal_sequence,
        "observations":state.observations,"early_context_retired":state.startup_context_retired,
        "archive_ack_attempted":state.archive_ack_attempted,"runtime_commit":state.runtime_commit,
        "runtime_commit_ack_attempted":state.runtime_commit_ack_attempted,
        "early_cleanup_complete":state.early_cleanup_complete,
        "early_cleanup_error":state.early_cleanup_error.as_ref().map(|e|e.error().to_string()),
        "closes":state.closes.iter().map(|r|json!({"fd":r.fd,"attempted":r.attempted,
            "raw":r.raw,"errno":r.errno})).collect::<Vec<_>>()})
    );
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) }
}
