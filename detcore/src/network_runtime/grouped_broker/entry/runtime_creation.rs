//! Linear early transfer to the independently retained runtime Keeper. It
//! precedes the first creation ACK and is not completed-source authority.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;

use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::super::Failure;
use super::super::Intent;
use super::super::guardian;
use super::super::journal;
use super::super::owner;
use super::super::require;
use super::super::wire;

#[derive(Debug)]
pub(in super::super) struct RuntimeCreationCustody {
    channel: wire::Channel,
    peer_pidfd: OwnedFd,
    peer: wire::Credentials,
    intent: Intent,
    deadline: u64,
    packets: Vec<Vec<u8>>,
    attempted: usize,
    expected_ack: Option<Vec<u8>>,
    ack_index: Option<usize>,
    source_creator: Option<serde_json::Value>,
    refused: Option<Failure>,
    failure_origin: Option<u64>,
    failure_notice_attempted: bool,
    failure_notice_sent: bool,
    notified_failure_origin: Option<u64>,
    custody_notice_attempted: bool,
    mirrored_sequence: u64,
    prefix_ack: Option<Vec<u8>>,
    prefix_packets: Vec<Vec<u8>>,
    source_input: Option<OwnedFd>,
    source_unit: Option<String>,
    source_launch_attempted: bool,
    source_launcher: Option<owner::RemoteLauncherLease>,
    source_terminal: Option<usize>,
    s2_guardian: Option<OwnedFd>,
    s2_guardian_pid: Option<i32>,
    s2_guardian_expected: Option<Vec<u8>>,
    s2_guardian_attempted: bool,
    s2_guardian_acknowledged: bool,
}
impl RuntimeCreationCustody {
    /// Infallible move of the authenticated original endpoint and actual
    /// runtime helper pidfd, not the CLI's systemd-run wrapper.
    pub fn retain(
        channel: OwnedFd,
        peer_pidfd: OwnedFd,
        peer: wire::Credentials,
        intent: Intent,
        deadline: u64,
    ) -> Self {
        Self {
            channel: wire::Channel::retain(channel),
            peer_pidfd,
            peer,
            intent,
            deadline,
            packets: Vec::new(),
            attempted: 0,
            expected_ack: None,
            ack_index: None,
            source_creator: None,
            refused: None,
            failure_origin: None,
            failure_notice_attempted: false,
            failure_notice_sent: false,
            notified_failure_origin: None,
            custody_notice_attempted: false,
            mirrored_sequence: 0,
            prefix_ack: None,
            prefix_packets: Vec::new(),
            source_input: None,
            source_unit: None,
            source_launch_attempted: false,
            source_launcher: None,
            source_terminal: None,
            s2_guardian: None,
            s2_guardian_pid: None,
            s2_guardian_expected: None,
            s2_guardian_attempted: false,
            s2_guardian_acknowledged: false,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && self.refused.is_none()
        {
                self.refused = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        result
    }
    fn check_peer(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(
            guardian::monotonic_ns()? < self.deadline,
            "runtime creation original startup expired",
        )?;
        self.channel.validate()?;
        owner::pidfd_matches(self.peer_pidfd.as_raw_fd(), self.peer.pid)?;
        require(
            !owner::terminal(self.peer_pidfd.as_raw_fd())?,
            "original runtime Keeper is terminal",
        )?;
        let mut actual: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&actual) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                self.channel.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut actual as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            length as usize == std::mem::size_of_val(&actual)
                && actual.pid == self.peer.pid
                && actual.uid == self.peer.uid
                && actual.gid == self.peer.gid,
            "runtime Keeper channel is not the original captured native process",
        )
    }
    pub fn launch_source(
        &mut self,
        unit: &str,
        executable: &str,
        arguments: &[std::ffi::OsString],
        input: &mut Option<OwnedFd>,
        image: BorrowedFd<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.check_peer()?;
            require(
                !self.source_launch_attempted && self.source_input.is_none() && input.is_some(),
                "outside source launch cannot repeat or replace input",
            )?;
            self.source_launch_attempted = true;
            self.source_input = input.take();
            self.source_unit = Some(unit.to_owned());
            let arguments = arguments
                .iter()
                .map(|arg| {
                    arg.to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| io::Error::other("original source argv is not UTF8"))
                })
                .collect::<io::Result<Vec<_>>>()?;
            let bytes = journal::canonical(
                &json!({"schema":"hermit-grouped-runtime-source-launch-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.deadline,
                "unit":unit,"executable":executable,"arguments":arguments}),
            )?;
            require(
                bytes.len() <= 4096,
                "source launch exceeds original row bound",
            )?;
            self.channel.send_once(
                &bytes,
                &[self.source_input.as_ref().unwrap().as_fd(), image],
            )?;
            self.check_peer()
        })();
        self.remember(result)
    }
    pub fn receive_source_launch(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check_peer()?;
            require(
                self.source_launch_attempted && self.source_launcher.is_none(),
                "source launch reply early or repeated",
            )?;
            let Some(index) = self.channel.receive(4096)? else {
                return Ok(false);
            };
            owner::RemoteLauncherLease::receive(
                &mut self.source_launcher,
                &mut self.channel.packets[index],
                self.peer_pidfd.as_fd(),
                self.peer,
                &self.intent,
                self.source_unit.as_ref().unwrap(),
                self.deadline,
            )?;
            // The actual outside Child now owns its stdin; this original alias
            // is relinquished once, after the exact live launch receipt.
            self.source_input.take();
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn source_pid(&self) -> i32 {
        self.source_launcher.as_ref().unwrap().pid()
    }
    pub fn source_pidfd(&self) -> BorrowedFd<'_> {
        self.source_launcher.as_ref().unwrap().pidfd()
    }
    pub fn source_lease(&self) -> io::Result<owner::LauncherLease> {
        self.source_launcher
            .as_ref()
            .ok_or_else(|| io::Error::other("actual remote source launch absent"))?
            .lend()
    }
    pub fn poll_source_terminal(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check_peer()?;
            if self.source_terminal.is_some() {
                return self.verify_source_terminal().map(|()| true);
            }
            require(
                self.mirrored_sequence == 34 && self.prefix_ack.is_none(),
                "remote normal source join precedes all actual callback mirrors",
            )?;
            let Some(index) = self.channel.receive(4096)? else {
                return Ok(false);
            };
            self.source_terminal = Some(index);
            self.verify_source_terminal()?;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn source_terminal_received(&self) -> bool {
        self.source_terminal.is_some()
    }
    pub fn verify_source_terminal(&self) -> io::Result<()> {
        self.check_peer()?;
        let launcher = self
            .source_launcher
            .as_ref()
            .ok_or_else(|| io::Error::other("original source launch absent"))?;
        launcher.check_terminal()?;
        let packet = &self.channel.packets[self
            .source_terminal
            .ok_or_else(|| io::Error::other("actual remote source terminal receipt absent"))?];
        packet.exact(2, self.peer)?;
        let record: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
        require(
            packet.bytes
                == journal::canonical(
                    &json!({"schema":"hermit-grouped-runtime-source-terminal-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.deadline,
            "unit":self.source_unit,"wrapper_pid":launcher.pid(),"raw_wait_status":0,"eof":[true,true],
            "logs_synced":true,"group_absent":true,"global_ECHILD_claimed":true,
            "stdout":record["stdout"],"stderr":record["stderr"]}),
                )?,
            "remote source terminal envelope differs from actual original normal join",
        )?;
        for (index, name) in ["stdout", "stderr"].into_iter().enumerate() {
            let fd = packet.rights[index].as_raw_fd();
            let before = owner::stat(fd)?;
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            require(
                before.mode & libc::S_IFMT == libc::S_IFREG
                    && before.mode & 0o7777 == 0o600
                    && before.uid == unsafe { libc::getuid() }
                    && before.links == 1
                    && before.size >= 0
                    && before.size <= 1_048_576
                    && flags >= 0
                    && flags & libc::O_ACCMODE == libc::O_RDONLY
                    && unsafe { libc::fcntl(fd, libc::F_GETFD) } == libc::FD_CLOEXEC,
                "remote source original log descriptor differs",
            )?;
            let mut bytes = vec![0u8; before.size as usize + 1];
            let raw = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(
                raw == before.size as isize,
                "remote source log extent changed",
            )?;
            bytes.truncate(raw as usize);
            let after = owner::stat(fd)?;
            require(
                before.same_owner(&after)
                    && before.size == after.size
                    && record[name]
                        == json!({"bytes":bytes.len(),"sha256":super::super::hex(&Sha256::digest(&bytes))}),
                "remote source actual log bytes differ from retained wait owner",
            )?;
        }
        launcher.check_terminal()
    }
    pub fn held_descriptors(&self) -> Vec<i32> {
        let mut fds = vec![self.channel.fd.as_raw_fd(), self.peer_pidfd.as_raw_fd()];
        fds.extend(self.source_input.iter().map(AsRawFd::as_raw_fd));
        fds.extend(self.s2_guardian.iter().map(AsRawFd::as_raw_fd));
        if let Some(source) = &self.source_launcher {
            fds.extend(source.held_descriptors());
        }
        for packet in &self.channel.packets {
            fds.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
        }
        fds
    }
    pub fn send(
        &mut self,
        creator: &owner::Creator,
        controls: &owner::Controls,
        keeper: (BorrowedFd<'_>, i32),
        guardian_store: [BorrowedFd<'_>; 2],
        keeper_store: [BorrowedFd<'_>; 2],
        histories: (&journal::SourceHistory, &journal::SourceHistory),
    ) -> io::Result<()> {
        let (keeper_pidfd, keeper_pid) = keeper;
        let (guardian_history, keeper_history) = histories;
        let result = (|| {
            self.check_peer()?;
            require(
                self.packets.is_empty() && self.attempted == 0 && self.expected_ack.is_none(),
                "runtime creation custody transfer cannot repeat",
            )?;
            controls.check()?;
            require(
                creator.admitted && !owner::terminal(creator.pidfd.as_raw_fd())?,
                "runtime creation lacks actual live source Creator",
            )?;
            owner::pidfd_matches(keeper_pidfd.as_raw_fd(), keeper_pid)?;
            require(
                keeper_pid > 0 && !owner::terminal(keeper_pidfd.as_raw_fd())?,
                "runtime creation lacks original live Keeper writer",
            )?;
            require(
                guardian_history.frames.len() == 1
                    && guardian_history.frames[0].sequence == 1
                    && guardian_history.frames[0].write.started == 0
                    && keeper_history.frames.is_empty(),
                "early cleanup custody lacks actual held first intent and healthy zero-callback Keeper Store",
            )?;
            let creator_record = creator.evidence()?;
            self.source_creator = Some(creator_record.clone());
            self.packets.push(journal::canonical(&json!({"schema":"hermit-grouped-runtime-creation-custody-v1",
                "nonce":self.intent.nonce, "incarnation":self.intent.incarnation,
                "stage_deadline":self.deadline, "creator":creator_record,"keeper_pid":keeper_pid,
                "initial_guardian_store":guardian_history.commitment(),"initial_keeper_store":keeper_history.commitment()}))?);
            for (sequence, role) in [(1, "controls"), (2, "guardian_store"), (3, "keeper_store")] {
                self.packets.push(journal::canonical(
                    &json!({"schema":"hermit-grouped-runtime-creation-rights-v1",
                    "sequence":sequence, "role":role, "nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation, "stage_deadline":self.deadline}),
                )?);
            }
            let mut digest = Sha256::new();
            let mut count = 0usize;
            for packet in &self.packets {
                require(
                    packet.len() <= 4096,
                    "runtime creation row exceeds original4096 limit",
                )?;
                digest.update((packet.len() as u64).to_le_bytes());
                digest.update(packet);
                count += packet.len();
            }
            let mut end = json!({"schema":"hermit-grouped-runtime-creation-end-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.deadline,
                "preceding_frames":4,"preceding_bytes":count,"framed_sha256":super::super::hex(&digest.finalize())});
            self.packets.push(journal::canonical(&end)?);
            end["schema"] = json!("hermit-grouped-runtime-creation-custody-ack-v1");
            self.expected_ack = Some(journal::canonical(&end)?);
            let control_rights = controls.fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
            let rights: [&[BorrowedFd<'_>]; 5] = [
                &[
                    creator.pidfd.as_fd(),
                    creator.directory.as_fd(),
                    keeper_pidfd,
                ],
                &control_rights,
                &guardian_store,
                &keeper_store,
                &[],
            ];
            for (index, right) in rights.into_iter().enumerate() {
                self.check_peer()?;
                self.attempted = index + 1;
                self.channel.send_once(&self.packets[index], right)?;
            }
            self.check_peer()
        })();
        self.remember(result)
    }
    pub fn receive_ack(&mut self, controls: &owner::Controls) -> io::Result<bool> {
        let result = (|| {
            self.check_peer()?;
            require(
                self.attempted == 5 && self.packets.len() == 5 && self.ack_index.is_none(),
                "runtime creation custody ACK is early or repeated",
            )?;
            let Some(index) = self.channel.receive(4096)? else {
                return Ok(false);
            };
            self.ack_index = Some(index);
            let packet = &self.channel.packets[index];
            packet.exact(3, self.peer)?;
            require(
                self.expected_ack.as_ref() == Some(&packet.bytes),
                "runtime creation custody ACK differs from exact transfer",
            )?;
            self.check_controls(controls)?;
            Ok(true)
        })();
        self.remember(result)
    }
    pub(in super::super) fn check_controls(&self, controls: &owner::Controls) -> io::Result<()> {
        self.check_peer()?;
        controls.check()?;
        let packet = &self.channel.packets[self
            .ack_index
            .ok_or_else(|| io::Error::other("runtime creation custody has no real ACK"))?];
        packet.exact(3, self.peer)?;
        require(
            controls.fds.len() == 3 && self.expected_ack.as_ref() == Some(&packet.bytes),
            "runtime creation custody ACK lost original identities",
        )?;
        for (original, held) in controls.fds.iter().zip(&packet.rights) {
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_kcmp,
                    libc::getpid(),
                    libc::getpid(),
                    0,
                    original.as_raw_fd() as libc::c_ulong,
                    held.as_raw_fd() as libc::c_ulong,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(
                raw == 0,
                "runtime Keeper retained a different control description",
            )?;
        }
        Ok(())
    }
    /// Rechecked immediately before every real callback ACK; the source's
    /// original first callback remains held if this independent owner is lost.
    pub fn before_source_ack(
        &self,
        creator: &owner::Creator,
        controls: &owner::Controls,
    ) -> io::Result<()> {
        self.check_controls(controls)?;
        require(
            self.source_creator.as_ref() == Some(&creator.evidence()?),
            "runtime creation source changed after retained cleanup preparation",
        )?;
        require(
            !owner::terminal(creator.pidfd.as_raw_fd())?,
            "source became terminal before original creation ACK",
        )
    }
    pub fn mirrored_sequence(&self) -> u64 {
        self.mirrored_sequence
    }
    /// Actual current dual histories are checked while the source's second C
    /// ACK is withheld. The outside peer must retain/durably mirror both real
    /// Stores before its exact ACK can release that original callback.
    pub fn begin_prefix(
        &mut self,
        sequence: u64,
        guardian: &journal::SourceHistory,
        keeper: &journal::SourceHistory,
    ) -> io::Result<()> {
        let result = (|| {
            self.check_peer()?;
            require(
                self.ack_index.is_some()
                    && self.prefix_ack.is_none()
                    && sequence == self.mirrored_sequence + 1
                    && sequence <= 34
                    && guardian.frames.len() as u64 == sequence
                    && guardian.frames == keeper.frames,
                "runtime prefix changed original callback sequence or dual histories",
            )?;
            let mut value = json!({"schema":"hermit-grouped-runtime-creation-prefix-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
                "stage_deadline":self.deadline,"sequence":sequence,
                "guardian_store":guardian.commitment(),"keeper_store":keeper.commitment()});
            let bytes = journal::canonical(&value)?;
            require(
                bytes.len() <= 4096 && self.prefix_packets.len() < 34,
                "runtime prefix exceeds unchanged row/callback bounds",
            )?;
            value["schema"] = json!("hermit-grouped-runtime-creation-prefix-ack-v1");
            self.prefix_ack = Some(journal::canonical(&value)?);
            self.prefix_packets.push(bytes);
            self.channel
                .send_once(self.prefix_packets.last().unwrap(), &[])?;
            self.check_peer()
        })();
        self.remember(result)
    }
    pub fn receive_prefix_ack(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check_peer()?;
            let expected = self
                .prefix_ack
                .as_ref()
                .ok_or_else(|| io::Error::other("runtime prefix ACK without submitted prefix"))?;
            let Some(index) = self.channel.receive(4096)? else {
                return Ok(false);
            };
            let packet = &self.channel.packets[index];
            packet.exact(0, self.peer)?;
            require(
                packet.bytes == *expected,
                "runtime durable prefix ACK differs from exact original dual histories",
            )?;
            self.check_peer()?;
            self.mirrored_sequence += 1;
            self.prefix_ack = None;
            Ok(true)
        })();
        self.remember(result)
    }
    /// One fixed later holder, registered from the actual retained local
    /// Child before that actor receives its first Store/control capability.
    pub fn begin_s2_guardian(&mut self, guardian: &owner::Launcher) -> io::Result<()> {
        let result = (|| {
            self.check_peer()?;
            require(
                !self.s2_guardian_attempted
                    && self.s2_guardian.is_none()
                    && self.source_terminal_received(),
                "S2 Guardian registration is early or repeated",
            )?;
            self.verify_source_terminal()?;
            self.s2_guardian_attempted = true;
            self.s2_guardian = Some(
                guardian
                    .pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("actual Guardian pidfd absent"))?
                    .try_clone()?,
            );
            let pid = guardian.child.id() as i32;
            self.s2_guardian_pid = Some(pid);
            owner::pidfd_matches(self.s2_guardian.as_ref().unwrap().as_raw_fd(), pid)?;
            require(
                !owner::terminal(self.s2_guardian.as_ref().unwrap().as_raw_fd())?,
                "actual S2 Guardian already terminal",
            )?;
            let mut value = json!({"schema":"hermit-grouped-runtime-s2-guardian-v1","nonce":self.intent.nonce,
                "incarnation":self.intent.incarnation,"stage_deadline":self.deadline,"pid":pid});
            let bytes = journal::canonical(&value)?;
            value["schema"] = json!("hermit-grouped-runtime-s2-guardian-ack-v1");
            self.s2_guardian_expected = Some(journal::canonical(&value)?);
            self.channel
                .send_once(&bytes, &[self.s2_guardian.as_ref().unwrap().as_fd()])
        })();
        self.remember(result)
    }
    pub fn receive_s2_guardian_ack(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check_peer()?;
            require(
                self.s2_guardian_attempted && !self.s2_guardian_acknowledged,
                "S2 Guardian ACK is early or repeated",
            )?;
            let Some(index) = self.channel.receive(4096)? else {
                return Ok(false);
            };
            let packet = &self.channel.packets[index];
            packet.exact(0, self.peer)?;
            require(
                self.s2_guardian_expected.as_ref() == Some(&packet.bytes),
                "S2 Guardian ACK changed original holder",
            )?;
            owner::pidfd_matches(
                self.s2_guardian.as_ref().unwrap().as_raw_fd(),
                self.s2_guardian_pid.unwrap(),
            )?;
            require(
                !owner::terminal(self.s2_guardian.as_ref().unwrap().as_raw_fd())?,
                "S2 Guardian terminal before registered handoff",
            )?;
            self.s2_guardian_acknowledged = true;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn failure_notice_origin(&self) -> io::Result<u64> {
        require(
            self.failure_notice_sent,
            "original failure notice was not sent",
        )?;
        self.notified_failure_origin
            .ok_or_else(|| io::Error::other("original notified failure origin absent"))
    }
    pub fn send_local_custody(
        &mut self,
        report: &serde_json::Value,
        file: BorrowedFd<'_>,
    ) -> io::Result<()> {
        require(
            self.failure_notice_sent && !self.custody_notice_attempted,
            "controller custody notice is early or repeated",
        )?;
        self.custody_notice_attempted = true;
        let origin = self
            .notified_failure_origin
            .ok_or_else(|| io::Error::other("notified first origin absent"))?;
        let now = guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000 && now < self.deadline,
            "controller custody notice exceeded original first failure1s",
        )?;
        owner::pidfd_matches(self.peer_pidfd.as_raw_fd(), self.peer.pid)?;
        require(
            !owner::terminal(self.peer_pidfd.as_raw_fd())?,
            "outside custody receiver is terminal",
        )?;
        let bytes = journal::canonical(report)?;
        let frame = json!({"schema":"hermit-grouped-runtime-controller-custody-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"stage_deadline":self.deadline,"first_failure_origin":origin,
            "report_bytes":bytes.len(),"report_sha256":super::super::hex(&Sha256::digest(&bytes)),
            "launcher_count":report["launcher_count"],"query_count":report["query_count"],"waitid":report["waitid"]});
        let packet = journal::canonical(&frame)?;
        require(
            packet.len() <= 4096,
            "controller custody row exceeds original bound",
        )?;
        self.channel.send_once(&packet, &[file])
    }
    pub fn notify_failure(&mut self, origin: Option<u64>, cause: &io::Error) -> io::Result<()> {
        require(
            !self.failure_notice_attempted,
            "runtime creation failure notification repeated",
        )?;
        self.failure_notice_attempted = true;
        let origin = match (self.failure_origin, origin) {
            (Some(own), Some(caller)) => own.min(caller),
            (None, Some(caller)) if self.refused.is_none() => caller,
            _ => {
                return Err(io::Error::other(
                    "runtime creation original failure origin is unknown",
                ));
            }
        };
        let now = guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000 && now < self.deadline,
            "runtime creation notification exceeded original first-failure1s",
        )?;
        owner::pidfd_matches(self.peer_pidfd.as_raw_fd(), self.peer.pid)?;
        require(
            !owner::terminal(self.peer_pidfd.as_raw_fd())?,
            "runtime cleanup owner is terminal",
        )?;
        self.notified_failure_origin = Some(origin);
        self.channel.send_once(&journal::canonical(&json!({"schema":"hermit-grouped-runtime-creation-refused-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.deadline,
            "first_failure_origin":origin,"cause":cause.to_string()}))?,&[])?;
        self.failure_notice_sent = true;
        Ok(())
    }
}
