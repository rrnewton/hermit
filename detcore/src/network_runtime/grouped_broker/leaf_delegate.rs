//! Real manager-opened leaf handoff. All native custody remains in this owner
//! on refusal. A delivery follows actual helper/wrapper retirement and supplies
//! descriptors for native adoption; it cannot issue a ProviderLease.
use std::ffi::OsString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

use super::super::guardian;
use super::super::journal;
use super::super::wire;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Retained,
    Creator,
    Authenticating,
    Leaves,
    Terminal,
    Stop,
    Join,
    Joined,
}

#[derive(Debug)]
#[must_use = "leaf refusal retains the actual launcher and all received rights"]
pub(in super::super) struct LeafDelegate {
    intent: Intent,
    unit: String,
    deadline: Instant,
    native_deadline: u64,
    creator_cutoff: Option<u64>,
    image: EntryImage,
    prepared: Option<PreparedLeafNamespace>,
    setup_image: Option<EntryImage>,
    namespace_query: Option<NamespaceQuery>,
    namespace_snapshot: Option<NamespaceSnapshot>,
    setup_cutoff_sent: bool,
    channel: wire::Channel,
    logs: OwnedFd,
    launcher: Option<Launcher>,
    launcher_lease: Option<LauncherLease>,
    creator: Option<Creator>,
    manager: Option<ManagerQuery>,
    entry: Option<EntryQuery>,
    named: Option<CommandQuery>,
    manager_snapshot: Option<ManagerSnapshot>,
    entry_snapshot: Option<EntrySnapshot>,
    named_snapshot: Option<[FileIdentity; 3]>,
    leaves: Vec<OwnedFd>,
    descriptions: Option<Value>,
    terminal_query: Option<ManagerQuery>,
    terminal_snapshot: Option<ManagerSnapshot>,
    stop: Option<ManagerStop>,
    channel_eof: bool,
    phase: Phase,
    census: CensusInventory,
    delivery: Option<Vec<u8>>,
    delivery_ack: Option<Vec<u8>>,
    delivery_cutoff: Option<u64>,
    delivered: bool,
    acknowledged: bool,
    failure: Option<Failure>,
    failure_origin: Option<u64>,
    failure_record_attempted: bool,
    failure_record: Option<(OwnedFd, Vec<u8>)>,
    failure_record_synced: bool,
    failure_record_error: Option<Failure>,
    retirement_cutoff: Option<Instant>,
    cleanup_query: Option<ManagerQuery>,
    cleanup_snapshot: Option<ManagerSnapshot>,
    cleanup_stop: Option<ManagerStop>,
    cleanup_forget: Option<ManagerForgetFailed>,
    cleanup_record: Option<(OwnedFd, Vec<u8>)>,
    cleanup_record_synced: bool,
    cleanup_unit_done: bool,
    retired: bool,
}
impl LeafDelegate {
    pub fn retain(
        intent: Intent,
        unit: String,
        deadline: Instant,
        native_deadline: u64,
        image: OwnedFd,
        arguments: Vec<OsString>,
        channel: OwnedFd,
        logs: OwnedFd,
    ) -> Self {
        Self {
            intent,
            unit,
            deadline,
            native_deadline,
            creator_cutoff: None,
            image: EntryImage::retain(image, arguments),
            prepared: None,
            setup_image: None,
            namespace_query: None,
            namespace_snapshot: None,
            setup_cutoff_sent: false,
            channel: wire::Channel::retain(channel),
            logs,
            launcher: None,
            launcher_lease: None,
            creator: None,
            manager: None,
            entry: None,
            named: None,
            manager_snapshot: None,
            entry_snapshot: None,
            named_snapshot: None,
            leaves: Vec::new(),
            descriptions: None,
            terminal_query: None,
            terminal_snapshot: None,
            stop: None,
            channel_eof: false,
            phase: Phase::Retained,
            census: CensusInventory::retain(),
            delivery: None,
            delivery_ack: None,
            delivery_cutoff: None,
            delivered: false,
            acknowledged: false,
            failure: None,
            failure_origin: None,
            failure_record_attempted: false,
            failure_record: None,
            failure_record_synced: false,
            failure_record_error: None,
            retirement_cutoff: None,
            cleanup_query: None,
            cleanup_snapshot: None,
            cleanup_stop: None,
            cleanup_forget: None,
            cleanup_record: None,
            cleanup_record_synced: false,
            cleanup_unit_done: false,
            retired: false,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refuse(error);
        }
        result
    }
    pub fn refuse(&mut self, cause: &io::Error) {
        if self.failure.is_none() {
            self.failure = Some(Failure::capture(cause));
            self.failure_origin = guardian::monotonic_ns().ok();
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(failure) = &self.failure {
            return Err(failure.error());
        }
        let now = guardian::monotonic_ns()?;
        require(
            now < self.native_deadline && Instant::now() < self.deadline,
            "leaf original startup deadline expired",
        )?;
        if matches!(
            self.phase,
            Phase::Creator | Phase::Authenticating | Phase::Leaves
        ) {
            require(
                self.creator_cutoff.is_some_and(|cutoff| now < cutoff),
                "leaf original creator1s expired",
            )?;
        }
        Ok(())
    }
    /// Prepare the continuously held expected image before a helper exists.
    /// This static work consumes the original stage, not the Creator window.
    pub fn prepare_image(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.phase == Phase::Retained
                    && self.launcher.is_none()
                    && self.creator.is_none()
                    && self.creator_cutoff.is_none()
                    && self.image.identity.is_none()
                    && self.image.digest.is_none(),
                "leaf expected image preparation is one-use and precedes launch",
            )?;
            self.image.initialize()?;
            self.check()
        })();
        self.remember(result)
    }
    /// Move preparation into the eventual failed-admission owner before doing
    /// any fallible validation. Only the real completed SourceTerminal can
    /// issue PreparedLeafNamespace; a serialized namespace identity cannot.
    pub fn install_preparation(
        &mut self,
        namespace: &mut Option<PreparedLeafNamespace>,
        image: &mut Option<OwnedFd>,
        expected: [u8; 32],
    ) -> io::Result<()> {
        let result = (|| {
            require(
                self.phase == Phase::Retained
                    && self.launcher.is_none()
                    && self.prepared.is_none()
                    && self.setup_image.is_none()
                    && namespace.is_some()
                    && image.is_some(),
                "leaf preparation repeated or absent",
            )?;
            self.prepared = namespace.take();
            self.setup_image = Some(EntryImage::retain(
                image.take().unwrap(),
                vec![OsString::from("hermit-grouped-namespace-setup")],
            ));
            self.check()?;
            let setup = self.setup_image.as_mut().unwrap();
            setup.initialize()?;
            require(
                setup.digest == Some(expected)
                    && setup.identity.as_ref().unwrap().size <= 1_048_576,
                "trusted installation setup image differs from held package member",
            )?;
            self.census()?;
            self.check()
        })();
        self.remember(result)
    }
    pub fn preparation(&self) -> io::Result<(&PreparedLeafNamespace, i32, i32)> {
        self.check()?;
        require(
            self.phase == Phase::Retained && self.launcher.is_none(),
            "prepared launch descriptions requested after actual spawn",
        )?;
        let namespace = self
            .prepared
            .as_ref()
            .ok_or_else(|| io::Error::other("completed source namespace absent"))?;
        let setup = self
            .setup_image
            .as_ref()
            .ok_or_else(|| io::Error::other("held trusted setup image absent"))?;
        require(
            setup.digest.is_some() && setup.identity.is_some(),
            "setup image preparation incomplete",
        )?;
        Ok((
            namespace,
            setup.file.as_raw_fd(),
            self.image.file.as_raw_fd(),
        ))
    }
    pub fn install_launcher(&mut self, launcher: &mut Option<Launcher>) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.phase == Phase::Retained && self.launcher.is_none() && launcher.is_some(),
                "leaf actual launcher installation repeated or absent",
            )?;
            self.launcher = launcher.take();
            Ok(())
        })();
        self.remember(result)
    }
    pub fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.phase == Phase::Retained && self.creator_cutoff.is_none(),
                "leaf initialization is one-use",
            )?;
            let now = guardian::monotonic_ns()?;
            self.creator_cutoff = Some(
                now.checked_add(1_000_000_000)
                    .ok_or_else(|| io::Error::other("leaf cutoff overflow"))?
                    .min(self.native_deadline),
            );
            self.phase = Phase::Creator;
            require(
                self.native_deadline - now <= 20_000_000_000
                    && self.deadline.saturating_duration_since(Instant::now())
                        <= Duration::from_secs(20),
                "leaf original20s startup bound extended",
            )?;
            require(
                super::super::valid_nonce(&self.intent.nonce)
                    && self.intent.incarnation != 0
                    && self
                        .unit
                        .strip_prefix("hermit-accepted-")
                        .and_then(|s| s.strip_suffix(".service"))
                        .is_some_and(super::super::valid_nonce),
                "leaf original unit or intent malformed",
            )?;
            protected_holder()?;
            self.channel.validate()?;
            let launcher = self
                .launcher
                .as_mut()
                .ok_or_else(|| io::Error::other("leaf actual launcher absent"))?;
            launcher.initialize(self.logs.as_raw_fd())?;
            self.launcher_lease = Some(launcher.source_lease()?);
            require(
                self.image.identity.is_some() && self.image.digest.is_some(),
                "leaf expected image was not prepared before launch",
            )?;
            self.census()?;
            self.check()?;
            require(
                self.prepared.is_some() && self.setup_image.is_some() && !self.setup_cutoff_sent,
                "leaf launch has no original prepared namespace",
            )?;
            // The original wrapper-start sample above is unchanged. This
            // separate zero-right frame conveys it to trusted setup; it is
            // neither EXEC nor a Creator acknowledgment and cannot refresh it.
            self.setup_cutoff_sent = true;
            self.channel.send_once(
                format!(
                    "PREPARED_MOUNT_V1 nonce={} cutoff={}\n",
                    self.intent.nonce,
                    self.creator_cutoff.unwrap()
                )
                .as_bytes(),
                &[],
            )?;
            self.check()
        })();
        self.remember(result)
    }
    fn census(&mut self) -> io::Result<()> {
        let mut fds = vec![
            self.channel.fd.as_raw_fd(),
            self.logs.as_raw_fd(),
            self.image.file.as_raw_fd(),
        ];
        fds.extend(self.leaves.iter().map(AsRawFd::as_raw_fd));
        if let Some(namespace) = &self.prepared {
            fds.extend(namespace.held_descriptors());
        }
        if let Some(setup) = &self.setup_image {
            fds.push(setup.file.as_raw_fd());
        }
        if let Some(query) = &self.namespace_query {
            fds.extend(query.held_descriptors());
        }
        if let Some((fd, _)) = &self.failure_record {
            fds.push(fd.as_raw_fd());
        }
        if let Some(c) = &self.creator {
            fds.extend([c.pidfd.as_raw_fd(), c.directory.as_raw_fd()]);
        }
        if let Some(l) = &self.launcher {
            fds.extend(l.pidfd.iter().map(AsRawFd::as_raw_fd));
            fds.extend(l.log_files.iter().flatten().map(AsRawFd::as_raw_fd));
            fds.extend(l.child.stdout.iter().map(AsRawFd::as_raw_fd));
            fds.extend(l.child.stderr.iter().map(AsRawFd::as_raw_fd));
        }
        self.census.observe(&fds, self.deadline)
    }
    fn creator_live(&self) -> io::Result<()> {
        self.check()?;
        let c = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("leaf Creator absent"))?;
        pidfd_matches(c.pidfd.as_raw_fd(), c.peer.pid)?;
        require(
            !terminal(c.pidfd.as_raw_fd())?,
            "leaf Creator exited during admission",
        )
    }
    fn leaf_message(&self, schema: &str) -> Value {
        json!({"schema":schema,"nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
            "stage_deadline":self.native_deadline,"unit":self.unit,"roles":["ID","FORMAT","ENABLE"]})
    }
    fn named_arguments(&self) -> Vec<String> {
        let base = format!(
            "/sys/kernel/tracing/events/{}/{}",
            self.intent.group(),
            self.intent.event()
        );
        let mut args = vec![
            "-n".into(),
            "/usr/bin/stat".into(),
            "--format=%d %i %f %u %g".into(),
            "--".into(),
        ];
        args.extend(["id", "format", "enable"].map(|leaf| format!("{base}/{leaf}")));
        args
    }
    fn authenticate_leaves(&mut self) -> io::Result<()> {
        self.creator_live()?;
        require(
            self.leaves.len() == 3 && self.named_snapshot.is_some(),
            "leaf rights or named query absent",
        )?;
        for (index, fd) in self.leaves.iter().enumerate() {
            let identity = stat(fd.as_raw_fd())?;
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            require(
                identity.same_owner(&self.named_snapshot.as_ref().unwrap()[index])
                    && filesystem(fd.as_raw_fd())? == 0x7472_6163
                    && identity.uid == 0
                    && identity.gid == 0
                    && identity.mode & libc::S_IFMT == libc::S_IFREG
                    && leaf_role_flags(flags)
                    && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
                "leaf is not its exact manager-opened root-owned tracefs role",
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
                        self.leaves[i].as_raw_fd() as libc::c_ulong,
                        self.leaves[j].as_raw_fd() as libc::c_ulong,
                    )
                };
                require(
                    if i == j {
                        raw == 0
                    } else {
                        (1..=3).contains(&raw)
                    },
                    "leaf aliases or actual OFD comparison failed",
                )?;
            }
        }
        self.descriptions = Some(serde_json::to_value(
            self.leaves
                .iter()
                .map(|fd| describe_fd(fd.as_raw_fd()))
                .collect::<io::Result<Vec<_>>>()?,
        )?);
        self.creator_live()
    }
    fn check_leaves(&self) -> io::Result<()> {
        require(self.leaves.len() == 3, "original leaf custody incomplete")?;
        let d = self
            .descriptions
            .as_ref()
            .and_then(Value::as_array)
            .ok_or_else(|| io::Error::other("original leaf descriptions absent"))?;
        require(d.len() == 3, "original leaf description count differs")?;
        for (fd, description) in self.leaves.iter().zip(d) {
            verify_description(fd.as_raw_fd(), description)?;
        }
        Ok(())
    }
    pub fn progress(&mut self) -> io::Result<bool> {
        let result = self.progress_inner();
        self.remember(result)
    }
    fn progress_inner(&mut self) -> io::Result<bool> {
        self.check()?;
        if self.phase == Phase::Joined {
            self.check_leaves()?;
            return Ok(true);
        }
        self.launcher
            .as_mut()
            .ok_or_else(|| io::Error::other("leaf actual launcher absent"))?
            .drain()?;
        match self.phase {
            Phase::Retained => return Err(io::Error::other("leaf initialization absent")),
            Phase::Creator => {
                let Some(index) = self.channel.receive(2048)? else {
                    return Ok(false);
                };
                self.creator = Some(Creator::retain(
                    &mut self.channel.packets[index],
                    &self.intent,
                    &self.unit,
                )?);
                self.creator_live()?;
                self.manager = Some(ManagerQuery::retain(self.unit.clone()));
                self.entry = Some(EntryQuery::retain(self.creator.as_ref().unwrap().peer.pid));
                self.named = Some(CommandQuery::retain(self.named_arguments()));
                self.namespace_query = Some(NamespaceQuery::retain(
                    self.creator.as_ref().unwrap().peer.pid,
                ));
                self.phase = Phase::Authenticating;
                self.manager.as_mut().unwrap().start()?;
                self.entry.as_mut().unwrap().start()?;
                self.named.as_mut().unwrap().start()?;
                self.namespace_query.as_mut().unwrap().start()?;
            }
            Phase::Authenticating => {
                self.creator_live()?;
                if self.manager_snapshot.is_none() {
                    self.manager_snapshot = self.manager.as_mut().unwrap().poll(self.deadline)?;
                }
                if self.entry_snapshot.is_none() {
                    self.entry_snapshot = self.entry.as_mut().unwrap().poll(self.deadline)?;
                }
                if self.named_snapshot.is_none()
                    && self.named.as_mut().unwrap().poll(self.deadline)?
                {
                    self.named_snapshot = Some(parse_named(&self.named.as_ref().unwrap().stdout)?);
                }
                if self.namespace_snapshot.is_none() {
                    self.namespace_snapshot =
                        self.namespace_query.as_mut().unwrap().poll(self.deadline)?;
                }
                if self.manager_snapshot.is_none()
                    || self.entry_snapshot.is_none()
                    || self.named_snapshot.is_none()
                    || self.namespace_snapshot.is_none()
                {
                    return Ok(false);
                }
                self.creator.as_mut().unwrap().authenticate(
                    self.manager_snapshot.as_ref().unwrap(),
                    &self.image,
                    self.entry_snapshot.as_ref().unwrap(),
                )?;
                require(
                    self.namespace_snapshot.as_ref().unwrap().pid()
                        == self.creator.as_ref().unwrap().peer.pid,
                    "leaf namespace query differs from actual Creator PID",
                )?;
                self.prepared
                    .as_ref()
                    .ok_or_else(|| io::Error::other("original prepared namespace lost"))?
                    .check_match(self.namespace_snapshot.as_ref().unwrap())?;
                self.creator_live()?;
                self.phase = Phase::Leaves;
                self.channel
                    .send_once(format!("EXEC {}\n", self.intent.nonce).as_bytes(), &[])?;
            }
            Phase::Leaves => {
                self.creator_live()?;
                let Some(index) = self.channel.receive(4096)? else {
                    return Ok(false);
                };
                let expected = journal::canonical(&self.leaf_message("hermit-grouped-leaves-v1"))?;
                let packet = &mut self.channel.packets[index];
                packet.exact(3, self.creator.as_ref().unwrap().peer)?;
                require(
                    packet.bytes == expected,
                    "leaf role transfer changed original entry",
                )?;
                self.leaves = std::mem::take(&mut packet.rights);
                self.authenticate_leaves()?;
                self.census()?;
                self.check()?;
                let ack = journal::canonical(&self.leaf_message("hermit-grouped-leaves-ack-v1"))?;
                self.phase = Phase::Terminal;
                self.channel.send_once(&ack, &[])?;
            }
            Phase::Terminal => {
                if !terminal(self.creator.as_ref().unwrap().pidfd.as_raw_fd())? {
                    return Ok(false);
                }
                if self.terminal_query.is_none() {
                    self.terminal_query = Some(ManagerQuery::retain(self.unit.clone()));
                    self.terminal_query.as_mut().unwrap().start()?;
                }
                if self.terminal_snapshot.is_none() {
                    self.terminal_snapshot =
                        self.terminal_query.as_mut().unwrap().poll(self.deadline)?;
                }
                let Some(snapshot) = &self.terminal_snapshot else {
                    return Ok(false);
                };
                let creator = self.creator.as_ref().unwrap();
                if creator.check_terminal_snapshot_progress(snapshot, true)?
                    == TerminalProgress::Pending
                {
                    return Ok(false);
                }
                self.stop = Some(ManagerStop::retain(creator));
                self.phase = Phase::Stop;
            }
            Phase::Stop => {
                let stop = self.stop.as_mut().unwrap();
                if !stop.started()?
                    && stop.try_start(
                        self.creator.as_ref().unwrap(),
                        self.terminal_snapshot.as_ref().unwrap(),
                        self.launcher_lease.as_ref().unwrap(),
                        self.deadline,
                    )? == StopStart::Pending
                {
                    return Ok(false);
                }
                if !stop.poll(self.deadline)? {
                    return Ok(false);
                }
                self.phase = Phase::Join;
            }
            Phase::Join => {
                if !self.channel_eof {
                    let Some(index) = self.channel.receive(2048)? else {
                        return Ok(false);
                    };
                    let p = &self.channel.packets[index];
                    require(
                        p.raw.returned == 0
                            && p.raw.errno.is_none()
                            && p.bytes.is_empty()
                            && p.credentials.is_empty()
                            && p.rights.is_empty()
                            && p.rights_messages == 0
                            && p.flags == libc::MSG_CMSG_CLOEXEC,
                        "leaf channel lacks exact EOF",
                    )?;
                    self.channel_eof = true;
                }
                if self
                    .creator
                    .as_ref()
                    .unwrap()
                    .readback_progress()?
                    .terminal_progress()?
                    == TerminalProgress::Pending
                {
                    return Ok(false);
                }
                let launcher = self.launcher.as_mut().unwrap();
                if launcher.eof != [true, true]
                    || !terminal(launcher.pidfd.as_ref().unwrap().as_raw_fd())?
                {
                    return Ok(false);
                }
                launcher.reap_success(self.logs.as_raw_fd())?;
                // These five queries have no later descriptor export. Retain
                // their actual successful completion/close receipts while
                // releasing the fifteen descriptions before S2 allocation.
                // Creator, leaves, namespace and source custody stay held.
                for query in [
                    &mut self.manager.as_mut().unwrap().query,
                    &mut self.entry.as_mut().unwrap().query,
                    self.named.as_mut().unwrap(),
                    &mut self.terminal_query.as_mut().unwrap().query,
                    &mut self.stop.as_mut().unwrap().query,
                ] {
                    query.retire_successful_resources(self.deadline)?;
                }
                // This separate leaf query is complete too; the original S1
                // prepared namespace query and its pins remain owned.
                self.namespace_query
                    .as_mut()
                    .unwrap()
                    .retire_successful_resources(self.deadline)?;
                self.census()?;
                self.check_leaves()?;
                self.check()?;
                self.phase = Phase::Joined;
                return Ok(true);
            }
            Phase::Joined => unreachable!(),
        }
        self.check()?;
        Ok(false)
    }
    pub fn deliver(&mut self, channel: &mut wire::Channel, creator_cutoff: u64) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.phase == Phase::Joined
                    && self.delivery.is_none()
                    && self.delivery_cutoff.is_none(),
                "leaf delivery is one-use after actual join",
            )?;
            let now = guardian::monotonic_ns()?;
            require(
                creator_cutoff > now
                    && creator_cutoff - now <= 1_000_000_000
                    && creator_cutoff <= self.native_deadline,
                "successor creator cutoff is not original bounded1s",
            )?;
            self.delivery_cutoff = Some(creator_cutoff);
            self.check_leaves()?;
            self.census()?;
            self.delivery = Some(journal::canonical(
                &json!({"schema":"hermit-grouped-successor-leaves-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native_deadline,
                "creator_cutoff":creator_cutoff,"creator":self.creator.as_ref().unwrap().evidence()?,
                "roles":["ID","FORMAT","ENABLE"],"descriptions":self.descriptions}),
            )?);
            self.delivery_ack = Some(journal::canonical(
                &json!({"schema":"hermit-grouped-successor-leaves-ack-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native_deadline,
                "creator_cutoff":creator_cutoff,"descriptions":self.descriptions}),
            )?);
            channel.send_once(
                self.delivery.as_ref().unwrap(),
                &self.leaves.iter().map(AsFd::as_fd).collect::<Vec<_>>(),
            )?;
            self.check()?;
            require(
                guardian::monotonic_ns()? < creator_cutoff,
                "leaf delivery exceeded successor cutoff",
            )?;
            self.delivered = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn receive_delivery_ack(
        &mut self,
        channel: &mut wire::Channel,
        peer: wire::Credentials,
    ) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.delivered && !self.acknowledged,
                "leaf ACK precedes delivery or repeats",
            )?;
            require(
                guardian::monotonic_ns()? < self.delivery_cutoff.unwrap(),
                "leaf ACK exceeded successor cutoff",
            )?;
            let Some(index) = channel.receive(4096)? else {
                return Ok(false);
            };
            let p = &channel.packets[index];
            p.exact(0, peer)?;
            require(
                Some(&p.bytes) == self.delivery_ack.as_ref(),
                "service leaf custody ACK differs",
            )?;
            self.check_leaves()?;
            self.census()?;
            self.check()?;
            require(
                guardian::monotonic_ns()? < self.delivery_cutoff.unwrap(),
                "leaf ACK exceeded successor cutoff",
            )?;
            self.acknowledged = true;
            Ok(true)
        })();
        self.remember(result)
    }
    /// Retirement preserves failure and never grants Joined/delivery authority.
    /// The caller supplies its earlier outer cutoff, not a new duration.
    pub fn retire_custody(&mut self, original_cutoff: Instant) -> io::Result<bool> {
        if self.failure.is_none() {
            self.refuse(&io::Error::other(
                "leaf owner cancelled before completed startup",
            ));
        }
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("leaf original failure origin unknown"))?;
        let sampled = Instant::now();
        let now = guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000,
            "leaf original failure1s expired",
        )?;
        let cutoff = (sampled + Duration::from_nanos(1_000_000_000 - (now - origin)))
            .min(self.deadline)
            .min(original_cutoff);
        let cutoff = *self
            .retirement_cutoff
            .insert(self.retirement_cutoff.map_or(cutoff, |old| old.min(cutoff)));
        require(
            Instant::now() < cutoff,
            "leaf original retirement cutoff expired",
        )?;
        if self.retired {
            return Ok(true);
        }
        // The controller already sent its original failure before entering
        // this path. Persist the untouched phase/query observations before
        // retirement polls them; diagnostic failure must not stop cleanup.
        if !self.failure_record_attempted {
            if let Err(error) = self.persist_first_failure(cutoff) {
                self.failure_record_error = Some(Failure::capture(&error));
            }
        }
        let cause = self.failure.as_ref().unwrap().error();
        let mut pending = false;
        if let Some(query) = &mut self.namespace_query {
            if !query.successful_resources_retired()? && self.namespace_snapshot.is_none() {
                pending |= query.retire_custody(cutoff, &cause)? == QueryRetirement::Pending;
            }
        }
        for query in self
            .manager
            .iter_mut()
            .map(|q| &mut q.query)
            .chain(self.entry.iter_mut().map(|q| &mut q.query))
            .chain(self.named.iter_mut())
            .chain(self.terminal_query.iter_mut().map(|q| &mut q.query))
            .chain(self.stop.iter_mut().map(|q| &mut q.query))
        {
            if query.successful_resource_retirement.is_some() {
                // A partial or failed one-use close cannot become successful
                // cleanup, and must never be retried against a reused number.
                require(
                    query.successful_resources_retired()?,
                    "leaf successful query resource retirement incomplete",
                )?;
            } else if query.completed.is_none() {
                pending |= query.retire_custody(cutoff, &cause)? == QueryRetirement::Pending;
            }
        }
        if pending {
            return Ok(false);
        }
        if let Some(creator) = &self.creator {
            if !terminal(creator.pidfd.as_raw_fd())? {
                return Ok(false);
            }
            let already_stopped = self
                .stop
                .as_ref()
                .is_some_and(|s| s.query.completed.is_some());
            if creator.captured && !self.cleanup_unit_done && !already_stopped {
                if self.cleanup_query.is_none() {
                    self.cleanup_query = Some(ManagerQuery::retain(self.unit.clone()));
                    self.cleanup_query.as_mut().unwrap().start()?;
                }
                if self.cleanup_snapshot.is_none() {
                    self.cleanup_snapshot = self.cleanup_query.as_mut().unwrap().poll(cutoff)?;
                }
                let Some(snapshot) = &self.cleanup_snapshot else {
                    return Ok(false);
                };
                if creator.check_terminal_snapshot_progress(snapshot, false)?
                    == TerminalProgress::Pending
                {
                    return Ok(false);
                }
                let failed = snapshot.property("ActiveState") == Some("failed");
                self.persist_cleanup_record(cutoff)?;
                let creator = self.creator.as_ref().unwrap();
                let snapshot = self.cleanup_snapshot.as_ref().unwrap();
                if failed {
                    if self.cleanup_forget.is_none() {
                        self.cleanup_forget = Some(ManagerForgetFailed::retain(creator));
                        self.cleanup_forget.as_mut().unwrap().start(
                            creator,
                            snapshot,
                            self.launcher_lease
                                .as_ref()
                                .ok_or_else(|| io::Error::other("leaf launcher lease absent"))?,
                            cutoff,
                        )?;
                    }
                    if !self.cleanup_forget.as_mut().unwrap().poll(cutoff)? {
                        return Ok(false);
                    }
                } else {
                    if self.cleanup_stop.is_none() {
                        self.cleanup_stop = Some(ManagerStop::retain(creator));
                    }
                    let stop = self.cleanup_stop.as_mut().unwrap();
                    if !stop.started()?
                        && stop.try_start(
                            creator,
                            snapshot,
                            self.launcher_lease
                                .as_ref()
                                .ok_or_else(|| io::Error::other("leaf launcher lease absent"))?,
                            cutoff,
                        )? == StopStart::Pending
                    {
                        return Ok(false);
                    }
                    if !stop.poll(cutoff)? {
                        return Ok(false);
                    }
                }
                self.cleanup_unit_done = true;
            }
            let creator = self.creator.as_ref().unwrap();
            if creator.captured
                && creator.readback_progress()?.terminal_progress()? == TerminalProgress::Pending
            {
                return Ok(false);
            }
        }
        if let Some(launcher) = &mut self.launcher {
            if launcher.retire_custody(self.logs.as_raw_fd(), cutoff, &cause)?
                == QueryRetirement::Pending
            {
                return Ok(false);
            }
        } else {
            check_no_children()?;
        }
        require(
            Instant::now() < cutoff,
            "leaf retirement exceeded original cutoff",
        )?;
        // An unqualified receipt never authorizes manager namespace mutation.
        // The real Child is still retired above, but incomplete manager custody
        // stays an explicit cleanup error rather than a successful retirement.
        require(
            self.launcher.is_none() || self.creator.as_ref().is_some_and(|c| c.captured),
            "leaf child retired without captured manager invocation; namespace retirement unproved",
        )?;
        self.retired = true;
        Ok(true)
    }
    fn persist_first_failure(&mut self, cutoff: Instant) -> io::Result<()> {
        require(
            !self.failure_record_attempted,
            "leaf first failure receipt cannot repeat",
        )?;
        self.failure_record_attempted = true;
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("leaf first failure receipt has no original origin"))?;
        let now = guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000 && Instant::now() < cutoff,
            "leaf first failure receipt original cutoff expired",
        )?;
        let bytes = journal::canonical(&json!({"schema":"hermit-grouped-leaf-first-failure-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
            "sampled_native_ns":now,"first_failure_origin":origin,
            "stage_deadline":self.native_deadline,"creator_cutoff":self.creator_cutoff,
            "leaf":self.diagnostics(),
            "snapshots":{"manager":self.manager_snapshot.is_some(),"entry":self.entry_snapshot.is_some(),
                "named":self.named_snapshot.is_some()},
            "query_deadlines_debug":{
                "manager":self.manager.as_ref().and_then(|q|q.query.deadline).map(|d|format!("{d:?}")),
                "entry":self.entry.as_ref().and_then(|q|q.query.deadline).map(|d|format!("{d:?}")),
                "named":self.named.as_ref().and_then(|q|q.deadline).map(|d|format!("{d:?}"))},
            "channel":{"received_packets":self.channel.packets.len(),"send_attempts":self.channel.sends.len(),
                "last_receive":self.channel.last_receive.map(|r|json!({"raw":r.returned,"errno":r.errno})),
                "last_packet":self.channel.packets.last().map(|p|json!({"raw":p.raw.returned,"errno":p.raw.errno,
                    "bytes_hex":super::super::hex(&p.bytes),"flags":p.flags,"rights_messages":p.rights_messages,
                    "rights":p.rights.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
                    "credentials":p.credentials.iter().map(|c|json!({"pid":c.pid,"uid":c.uid,"gid":c.gid})).collect::<Vec<_>>()}))}}))?;
        require(
            bytes.len() <= 1_048_576 && Instant::now() < cutoff,
            "leaf first failure receipt exceeds original byte/time bound",
        )?;
        let raw = unsafe {
            libc::openat(
                self.logs.as_raw_fd(),
                c"first-failure.json".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // Actual descriptor ownership is installed before any fallible write.
        // Partial files stay owned by this same refused Leaf until process exit.
        self.failure_record = Some((unsafe { OwnedFd::from_raw_fd(raw) }, bytes));
        let (fd, bytes) = self.failure_record.as_ref().ok_or_else(|| {
            io::Error::other("leaf first failure receipt lost retained ownership")
        })?;
        let mut offset = 0;
        while offset < bytes.len() {
            require(
                Instant::now() < cutoff,
                "leaf first failure receipt original cutoff expired",
            )?;
            let raw = unsafe {
                libc::pwrite(
                    fd.as_raw_fd(),
                    bytes[offset..].as_ptr().cast(),
                    (bytes.len() - offset).min(4096),
                    offset as i64,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(raw > 0, "leaf first failure receipt write stalled")?;
            offset += raw as usize;
        }
        for fd in [fd.as_raw_fd(), self.logs.as_raw_fd()] {
            require(
                Instant::now() < cutoff,
                "leaf first failure receipt original cutoff expired",
            )?;
            if unsafe { libc::fsync(fd) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        require(
            Instant::now() < cutoff,
            "leaf first failure receipt original cutoff expired",
        )?;
        self.failure_record_synced = true;
        Ok(())
    }
    fn persist_cleanup_record(&mut self, cutoff: Instant) -> io::Result<()> {
        if self.cleanup_record_synced {
            return Ok(());
        }
        require(
            Instant::now() < cutoff && self.cleanup_record.is_none(),
            "leaf failure receipt repeated or late",
        )?;
        let bytes = journal::canonical(&json!({"schema":"hermit-grouped-leaf-retirement-v1",
            "nonce":self.intent.nonce,"unit":self.unit,"first_failure":self.failure.as_ref().map(|f|&f.message),
            "failure_origin":self.failure_origin,"creator":self.creator.as_ref().unwrap().receipt(),
            "manager":self.cleanup_snapshot.as_ref().unwrap().evidence(),
            "launcher":self.launcher.as_ref().map(Launcher::custody_evidence)}))?;
        require(
            bytes.len() <= 1_048_576,
            "leaf failure receipt exceeds original1MiB",
        )?;
        let raw = unsafe {
            libc::openat(
                self.logs.as_raw_fd(),
                c"retirement.json".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.cleanup_record = Some((unsafe { OwnedFd::from_raw_fd(raw) }, bytes));
        let (fd, bytes) = self.cleanup_record.as_ref().unwrap();
        let mut offset = 0;
        while offset < bytes.len() {
            require(
                Instant::now() < cutoff,
                "leaf failure receipt cutoff expired",
            )?;
            let raw = unsafe {
                libc::pwrite(
                    fd.as_raw_fd(),
                    bytes[offset..].as_ptr().cast(),
                    bytes.len() - offset,
                    offset as i64,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(raw > 0, "leaf failure receipt made no progress")?;
            offset += raw as usize;
        }
        if unsafe { libc::fsync(fd.as_raw_fd()) } != 0
            || unsafe { libc::fsync(self.logs.as_raw_fd()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            Instant::now() < cutoff,
            "leaf failure receipt cutoff expired",
        )?;
        self.cleanup_record_synced = true;
        Ok(())
    }
    pub fn diagnostics(&self) -> Value {
        json!({"unit":self.unit,"phase":format!("{:?}",self.phase),"failure_origin":self.failure_origin,
            "first_failure":self.failure.as_ref().map(|f|&f.message),"leaf_rights":self.leaves.len(),
            "delivered":self.delivered,"acknowledged":self.acknowledged,"custody_retired":self.retired,
            "failure_record_attempted":self.failure_record_attempted,
            "failure_record_fd":self.failure_record.as_ref().map(|(fd,_)|fd.as_raw_fd()),
            "failure_record_synced":self.failure_record_synced,
            "failure_record_error":self.failure_record_error.as_ref().map(|f|json!({"message":f.message,"errno":f.errno})),
            "launcher":self.launcher.as_ref().map(Launcher::custody_evidence),
            "manager":self.manager.as_ref().map(ManagerQuery::evidence),
            "entry":self.entry.as_ref().map(EntryQuery::evidence),"named":self.named.as_ref().map(CommandQuery::evidence),
            "namespace":self.namespace_query.as_ref().map(NamespaceQuery::evidence),
            "prepared_namespace_held":self.prepared.is_some(),"setup_cutoff_sent":self.setup_cutoff_sent,
            "terminal":self.terminal_query.as_ref().map(ManagerQuery::evidence),"stop":self.stop.as_ref().map(ManagerStop::evidence),
            "cleanup_query":self.cleanup_query.as_ref().map(ManagerQuery::evidence),
            "cleanup_stop":self.cleanup_stop.as_ref().map(ManagerStop::evidence),
            "cleanup_forget":self.cleanup_forget.as_ref().map(ManagerForgetFailed::evidence)})
    }
}
fn parse_named(bytes: &[u8]) -> io::Result<[FileIdentity; 3]> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    require(
        text.is_ascii() && text.ends_with('\n'),
        "named leaf metadata framing differs",
    )?;
    let lines: Vec<_> = text.lines().collect();
    require(lines.len() == 3, "named leaf metadata population differs")?;
    let mut identities = Vec::with_capacity(3);
    for line in lines {
        let fields: Vec<_> = line.split(' ').collect();
        require(
            fields.len() == 5 && fields.iter().all(|s| !s.is_empty()),
            "named leaf metadata fields differ",
        )?;
        let decimal = |value: &str| -> io::Result<u64> {
            require(
                value.bytes().all(|b| b.is_ascii_digit()),
                "named leaf decimal malformed",
            )?;
            value.parse().map_err(io::Error::other)
        };
        require(
            fields[2]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "named leaf mode malformed",
        )?;
        let identity = FileIdentity {
            device: decimal(fields[0])?,
            inode: decimal(fields[1])?,
            mode: u32::from_str_radix(fields[2], 16).map_err(io::Error::other)?,
            uid: decimal(fields[3])?.try_into().map_err(io::Error::other)?,
            gid: decimal(fields[4])?.try_into().map_err(io::Error::other)?,
            links: 0,
            size: 0,
        };
        require(
            identity.uid == 0
                && identity.gid == 0
                && identity.inode != 0
                && identity.mode & libc::S_IFMT == libc::S_IFREG,
            "named leaf is symlink, wrong owner or wrong type",
        )?;
        identities.push(identity);
    }
    Ok(identities.try_into().unwrap())
}

// ID, FORMAT and ENABLE are all manager-opened read-only descriptions. The
// leaf helper checks this same access contract before transferring the rights.
fn leaf_role_flags(flags: libc::c_int) -> bool {
    flags >= 0
        && flags & libc::O_ACCMODE == libc::O_RDONLY
        && flags & !(libc::O_ACCMODE | 0o100000 | libc::O_NOFOLLOW) == 0
}

#[cfg(test)]
mod leaf_role_flag_tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    use super::leaf_role_flags;

    fn actual_flags(access: libc::c_int) -> libc::c_int {
        // These controls exercise only the production descriptor-flag guard;
        // /dev/null does not issue tracefs or named-role identity authority.
        let raw = unsafe { libc::open(c"/dev/null".as_ptr(), access | libc::O_CLOEXEC) };
        assert!(
            raw >= 0,
            "actual control open: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert!(
            flags >= 0,
            "actual control F_GETFL: {}",
            std::io::Error::last_os_error()
        );
        flags
    }

    #[test]
    fn actual_readonly_leaf_flags_are_accepted() {
        for access in [libc::O_RDONLY, libc::O_RDONLY | libc::O_NOFOLLOW] {
            assert!(leaf_role_flags(actual_flags(access)));
        }
    }

    #[test]
    fn actual_writable_path_and_extra_flag_descriptions_are_refused() {
        for access in [
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_PATH,
            libc::O_RDONLY | libc::O_APPEND,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ] {
            assert!(!leaf_role_flags(actual_flags(access)), "access {access:#x}");
        }
        assert!(!leaf_role_flags(-1));
    }
}
