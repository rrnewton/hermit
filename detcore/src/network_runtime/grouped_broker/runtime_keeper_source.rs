//! Capability Keeper proxy for the separately captured ordinary source owner.
//! The real Launcher, manager queries, wait and logs remain in that owner.
use super::*;

pub(super) struct OwnedSource {
    request: Value,
    rights: Vec<OwnedFd>,
    input_close: Option<(i32, Option<i32>)>,
    launched: Option<Value>,
    native: Vec<OwnedFd>,
    terminal: Option<Value>,
    logs: Vec<OwnedFd>,
    log_bytes: Vec<Vec<u8>>,
    sent_terminal: bool,
    owner_ack: bool,
    retired: Option<Value>,
}
impl OwnedSource {
    fn retain(rights: Vec<OwnedFd>) -> Self {
        Self {
            request: Value::Null,
            rights,
            input_close: None,
            launched: None,
            native: Vec::new(),
            terminal: None,
            logs: Vec::new(),
            log_bytes: Vec::new(),
            sent_terminal: false,
            owner_ack: false,
            retired: None,
        }
    }
    fn creator(&self) -> io::Result<&Value> {
        Ok(&self
            .launched
            .as_ref()
            .ok_or_else(|| io::Error::other("actual source launch absent"))?["creator"])
    }
    fn check_creator(&self, terminal: bool) -> io::Result<()> {
        require(
            self.native.len() == 3,
            "source original native descriptions absent",
        )?;
        common::check_creator(&self.native[1..], self.creator()?, terminal)
    }
    fn wrapper_pid(&self) -> io::Result<i32> {
        self.launched
            .as_ref()
            .and_then(|v| v["wrapper_pid"].as_i64())
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v > 1)
            .ok_or_else(|| io::Error::other("original source wrapper PID absent"))
    }
    fn group_absent(&self) -> io::Result<()> {
        let raw = unsafe { libc::kill(-self.wrapper_pid()?, 0) };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        require(
            raw == -1 && error == Some(libc::ESRCH),
            "original remote source process group remains",
        )
    }
    fn check_logs(&mut self) -> io::Result<()> {
        require(
            self.logs.len() == 2 && self.log_bytes.is_empty(),
            "remote original source logs absent or reread",
        )?;
        for index in 0..2 {
            let field = if index == 0 { "stdout" } else { "stderr" };
            let terminal = self.terminal.as_ref().unwrap();
            let count = terminal[field]["bytes"]
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(|| io::Error::other("remote source log size absent"))?;
            let fd = self.logs[index].as_raw_fd();
            let before = owner::stat(fd)?;
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            require(
                count <= 1_048_576
                    && before.size == count as i64
                    && before.mode & libc::S_IFMT == libc::S_IFREG
                    && before.mode & 0o7777 == 0o600
                    && before.uid == unsafe { libc::getuid() }
                    && before.links == 1
                    && flags >= 0
                    && flags & (libc::O_ACCMODE | libc::O_PATH) == libc::O_RDONLY,
                "remote original source log identity or bound differs",
            )?;
            self.log_bytes.push(vec![0; count + 1]);
            let bytes = self.log_bytes.last_mut().unwrap();
            let raw = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            bytes.truncate(raw as usize);
            let after = owner::stat(fd)?;
            require(
                bytes.len() == count
                    && after.same_owner(&before)
                    && after.size == before.size
                    && after.links == before.links
                    && terminal[field]["sha256"] == hex(&Sha256::digest(bytes)),
                "remote source actual log bytes changed",
            )?;
        }
        Ok(())
    }
}
impl RuntimeKeeper {
    pub(super) fn launch_owned_source(&mut self) -> io::Result<()> {
        require(
            self.owned_source.is_none(),
            "source proxy cannot replace original launch",
        )?;
        let link = self.startup.as_mut().unwrap();
        link.check()?;
        let index = common::receive(&mut link.channel, link.credentials, 2, 4096, self.stage)?;
        let packet = &mut link.channel.packets[index];
        self.owned_source = Some(Box::new(OwnedSource::retain(std::mem::take(
            &mut packet.rights,
        ))));
        self.owned_source.as_mut().unwrap().request = serde_json::from_slice(&packet.bytes)?;
        let source = self.owned_source.as_ref().unwrap();
        let owner = self.source_owner.as_mut().unwrap();
        owner.check()?;
        common::send(
            &mut owner.channel,
            &source.request,
            &[source.rights[0].as_fd(), source.rights[1].as_fd()],
            self.stage,
        )?;
        // The same Keeper-created source input endpoint was forwarded. This
        // local alias must not hide actual source EOF after owner spawn.
        let raw = unsafe { libc::close(source.rights[0].as_raw_fd()) };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        self.owned_source.as_mut().unwrap().input_close = Some((raw, error));
        require(raw == 0, "source proxy original input alias close failed")?;
        let index = common::receive(&mut owner.channel, owner.credentials, 3, 4096, self.stage)?;
        let packet = &mut owner.channel.packets[index];
        let source = self.owned_source.as_mut().unwrap();
        source.native = std::mem::take(&mut packet.rights);
        source.launched = Some(serde_json::from_slice(&packet.bytes)?);
        let value = source.launched.as_ref().unwrap();
        let intent = self.intent.as_ref().unwrap();
        let pid = source.wrapper_pid()?;
        require(
            value
                == &json!({"schema":"hermit-grouped-source-owner-launched-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"stage_deadline":self.stage,"unit":source.request["unit"],
            "wrapper_pid":pid,"wait_owner_pid":owner.credentials.pid,"creator":value["creator"]}),
            "source owner launch reply substituted original binding",
        )?;
        source.check_creator(false)?;
        owner::pidfd_matches(source.native[0].as_raw_fd(), pid)?;
        require(
            !owner::terminal(source.native[0].as_raw_fd())?,
            "source original wrapper already terminal",
        )?;
        let status = owner::read_file(&format!("/proc/{pid}/status"), 16384)?;
        let parents = status
            .lines()
            .filter_map(|line| line.strip_prefix("PPid:"))
            .map(str::trim)
            .collect::<Vec<_>>();
        require(
            parents == [owner.credentials.pid.to_string()],
            "source real Child belongs to another wait owner",
        )?;
        owner.check()?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"authenticated-original-source-owner-capture",
            "reply":value,"input_close":source.input_close}))?;
        common::send(
            &mut self.startup.as_mut().unwrap().channel,
            &json!({"schema":"hermit-grouped-runtime-source-launched-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,"unit":source.request["unit"],
            "wrapper_pid":pid,"wait_owner_pid":owner.credentials.pid}),
            &[source.native[0].as_fd(), owner.pidfd.as_fd()],
            self.stage,
        )
    }
    pub(super) fn check_owned_source_creator(&mut self, creator: &Value) -> io::Result<()> {
        let source = self
            .owned_source
            .as_ref()
            .ok_or_else(|| io::Error::other("original source proxy absent"))?;
        source.check_creator(false)?;
        let actual = source.creator()?;
        for key in ["unit", "pid", "invocation", "cgroup", "device", "inode"] {
            require(
                actual.get(key).is_some() && actual[key] == creator[key],
                "first source ACK changed actual original manager/native capture",
            )?;
        }
        require(
            creator["nonce"] == self.intent.as_ref().unwrap().nonce,
            "first source ACK changed original nonce",
        )?;
        self.source_owner.as_ref().unwrap().check()?;
        common::before(self.stage)
    }
    pub(super) fn owned_source_descriptors(&self) -> io::Result<Vec<i32>> {
        let mut fds = Vec::new();
        if let Some(owner) = &self.source_owner {
            fds.extend([owner.pidfd.as_raw_fd(), owner.channel.fd.as_raw_fd()]);
            for packet in &owner.channel.packets {
                fds.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
            }
        }
        if let Some(source) = &self.owned_source {
            fds.extend(
                source
                    .rights
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != 0 || source.input_close != Some((0, None)))
                    .map(|(_, fd)| fd.as_raw_fd()),
            );
            fds.extend(
                source
                    .native
                    .iter()
                    .chain(&source.logs)
                    .map(AsRawFd::as_raw_fd),
            );
        }
        Ok(fds)
    }
    pub(super) fn check_owned_source_completed(&self) -> io::Result<()> {
        let source = self
            .owned_source
            .as_ref()
            .ok_or_else(|| io::Error::other("original source proxy absent"))?;
        require(
            source.sent_terminal
                && source.terminal.is_some()
                && source.logs.len() == 2
                && source.log_bytes.len() == 2
                && owner::terminal(source.native[0].as_raw_fd())?,
            "actual remote source completion absent",
        )?;
        source.check_creator(true)?;
        source.group_absent()?;
        if !source.owner_ack {
            self.source_owner.as_ref().unwrap().check()?;
        }
        owner::check_no_children()?;
        common::before(self.stage)
    }
    fn retain_source_terminal(&mut self, index: usize) -> io::Result<()> {
        let link = self.source_owner.as_mut().unwrap();
        let packet = &mut link.channel.packets[index];
        packet.exact(2, link.credentials)?;
        let source = self.owned_source.as_mut().unwrap();
        require(
            source.terminal.is_none() && source.logs.is_empty(),
            "source original terminal cannot repeat",
        )?;
        source.logs = std::mem::take(&mut packet.rights);
        source.terminal = Some(serde_json::from_slice(&packet.bytes)?);
        let value = source.terminal.as_ref().unwrap();
        let intent = self.intent.as_ref().unwrap();
        require(
            journal::canonical(value)? == packet.bytes
                && value
                    == &json!({"schema":"hermit-grouped-runtime-source-terminal-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,"unit":source.request["unit"],
            "wrapper_pid":source.wrapper_pid()?,"raw_wait_status":0,"eof":[true,true],"logs_synced":true,
            "group_absent":true,"global_ECHILD_claimed":true,"stdout":value["stdout"],"stderr":value["stderr"]}),
            "remote original source terminal receipt differs",
        )?;
        require(
            self.mirrored_index == Some(33),
            "actual source terminal preceded full34 mirrored callbacks",
        )?;
        source.check_creator(true)?;
        source.group_absent()?;
        source.check_logs()?;
        require(
            owner::terminal(source.native[0].as_raw_fd())?,
            "source wrapper terminal packet precedes native terminality",
        )?;
        Ok(())
    }
    pub(super) fn progress_owned_source(&mut self) -> io::Result<()> {
        if self.owned_source.as_ref().unwrap().sent_terminal {
            return Ok(());
        }
        let link = self.source_owner.as_mut().unwrap();
        link.check()?;
        let Some(index) = link.channel.receive(4096)? else {
            return Ok(());
        };
        self.retain_source_terminal(index)?;
        self.source_owner.as_ref().unwrap().check()?;
        let source = self.owned_source.as_mut().unwrap();
        source.sent_terminal = true;
        common::send(
            &mut self.startup.as_mut().unwrap().channel,
            source.terminal.as_ref().unwrap(),
            &[source.logs[0].as_fd(), source.logs[1].as_fd()],
            self.stage,
        )
    }
    pub(super) fn acknowledge_owned_source_archive(&mut self) -> io::Result<()> {
        self.check_owned_source_completed()?;
        let source = self.owned_source.as_mut().unwrap();
        require(!source.owner_ack, "source owner ACK cannot repeat")?;
        let link = self.source_owner.as_mut().unwrap();
        link.check()?;
        let intent = self.intent.as_ref().unwrap();
        // Only the real completed archive path calls this after all16 frames,
        // continuous descriptions, full34 histories and source terminal checks.
        common::send(
            &mut link.channel,
            &json!({"schema":"hermit-grouped-source-owner-terminal-ack-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
            "wrapper_pid":source.wrapper_pid()?}),
            &[],
            self.stage,
        )?;
        source.owner_ack = true;
        Ok(())
    }
}

/// Authenticated completion of the separately retained original wait owner,
/// accompanied by this Keeper's continuous wrapper/creator/cgroup checks.
/// No Child or local wait result is constructed from the packet.
pub(super) struct SourceRetired {
    wrapper: OwnedFd,
    origin: u64,
    cutoff: u64,
}
impl SourceRetired {
    pub(super) fn check(&self, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        require(
            cutoff <= self.cutoff
                && guardian::monotonic_ns()? >= self.origin
                && owner::terminal(self.wrapper.as_raw_fd())?,
            "remote original source retirement proof differs",
        )?;
        owner::check_no_children()?;
        common::before(cutoff)
    }
    pub(super) fn descriptor(&self) -> i32 {
        self.wrapper.as_raw_fd()
    }
}
impl RuntimeKeeper {
    pub(super) fn retire_owned_source_after_failure(
        &mut self,
        origin: u64,
        cutoff: u64,
    ) -> io::Result<SourceRetired> {
        let intent = self.intent()?.clone();
        let requested_origin = self
            .peer_failure_origin
            .map_or(origin, |old| old.min(origin));
        let requested_cutoff = cutoff.min(
            requested_origin
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("source failure cutoff overflow"))?,
        );
        let mut requested = false;
        if !self.owned_source.as_ref().unwrap().owner_ack
            && !owner::terminal(self.source_owner.as_ref().unwrap().pidfd.as_raw_fd())?
        {
            let link = self.source_owner.as_mut().unwrap();
            link.check()?;
            // Carry the original first cause before waiting for controller death;
            // the actual owner still refuses manager effects until that death.
            common::send(
                &mut link.channel,
                &json!({"schema":"hermit-grouped-source-owner-retire-v1",
                "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
                "original_start":requested_origin,"cutoff":requested_cutoff}),
                &[],
                requested_cutoff,
            )?;
            requested = true;
        }

        while !owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())? {
            common::before(cutoff)?;
            self.poll_controller_custody(cutoff)?;
            std::thread::sleep(Duration::from_millis(1));
        }
        while !self.controller_eof {
            self.poll_controller_custody(cutoff)?;
            if !self.controller_eof {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let earliest = self
            .peer_failure_origin
            .map_or(origin, |old| old.min(origin));
        let cutoff = cutoff.min(
            earliest
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("source failure cutoff overflow"))?,
        );
        self.clip_early_original_cutoff(cutoff)?;
        if requested
            && (earliest < requested_origin || cutoff < requested_cutoff)
            && !owner::terminal(self.source_owner.as_ref().unwrap().pidfd.as_raw_fd())?
        {
            let link = self.source_owner.as_mut().unwrap();
            common::send(
                &mut link.channel,
                &json!({"schema":"hermit-grouped-source-owner-retire-v1",
                "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
                "original_start":earliest,"cutoff":cutoff}),
                &[],
                cutoff,
            )?;
        }
        // A full archive was already admitted and its owner joined before the
        // later Provider commit. Its retained successful wait remains success.
        if !self.owned_source.as_ref().unwrap().owner_ack {
            let index = loop {
                common::before(cutoff)?;
                let link = self.source_owner.as_mut().unwrap();
                if let Some(index) = link.channel.receive(4096)? {
                    let packet = &link.channel.packets[index];
                    let value: Value = serde_json::from_slice(&packet.bytes)?;
                    if value["schema"] == "hermit-grouped-runtime-source-terminal-v1" {
                        // A genuine success can have been sent before the later
                        // S2 refusal. Retain its exact original logs and checks;
                        // do not relay a terminal to the now-dead controller.
                        self.retain_source_terminal(index)?;
                        continue;
                    }
                    packet.exact(0, link.credentials)?;
                    require(
                        journal::canonical(&value)? == packet.bytes,
                        "source retirement is not canonical",
                    )?;
                    break index;
                }
                if !requested {
                    link.check()?;
                    common::send(
                        &mut link.channel,
                        &json!({"schema":"hermit-grouped-source-owner-retire-v1",
                        "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
                        "original_start":earliest,"cutoff":cutoff}),
                        &[],
                        cutoff,
                    )?;
                    requested = true;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            let source = self.owned_source.as_mut().unwrap();
            source.retired = Some(serde_json::from_slice(
                &self.source_owner.as_ref().unwrap().channel.packets[index].bytes,
            )?);
            let value = source.retired.as_ref().unwrap();
            let actual_origin = value["original_start"]
                .as_u64()
                .ok_or_else(|| io::Error::other("source owner original failure absent"))?;
            let actual_cutoff = value["cutoff"]
                .as_u64()
                .ok_or_else(|| io::Error::other("source owner cutoff absent"))?;
            let status = value["raw_wait_status"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| io::Error::other("actual owner wait absent"))?;
            require(
                value
                    == &json!({"schema":"hermit-grouped-source-owner-retired-v1","nonce":intent.nonce,
                "incarnation":intent.incarnation,"stage_deadline":self.stage,"unit":source.request["unit"],
                "wrapper_pid":source.wrapper_pid()?,"raw_wait_status":status,"eof":[true,true],"logs_synced":true,
                "group_absent":true,"global_ECHILD_claimed":true,"original_start":actual_origin,"cutoff":actual_cutoff})
                    && actual_origin > 0
                    && actual_origin <= earliest
                    && actual_cutoff <= cutoff,
                "source owner retirement changed original bound or custody",
            )?;
            common::before(actual_cutoff)?;
            self.peer_failure_origin = Some(
                self.peer_failure_origin
                    .map_or(actual_origin, |old| old.min(actual_origin)),
            );
            self.clip_early_original_cutoff(actual_cutoff)?;
        }
        let source = self.owned_source.as_ref().unwrap();
        source.check_creator(true)?;
        source.group_absent()?;
        require(
            owner::terminal(source.native[0].as_raw_fd())?,
            "remote source wrapper is still live",
        )?;
        owner::check_no_children()?;
        let actual_origin = source
            .retired
            .as_ref()
            .and_then(|v| v["original_start"].as_u64())
            .unwrap_or(earliest);
        let actual_cutoff = source
            .retired
            .as_ref()
            .and_then(|v| v["cutoff"].as_u64())
            .unwrap_or(cutoff);
        let result = SourceRetired {
            wrapper: common::duplicate(source.native[0].as_fd())?,
            origin: actual_origin,
            cutoff: actual_cutoff,
        };
        self.early_callbacks.as_mut().unwrap().ledger.as_mut().unwrap().store.append(json!({"kind":"authenticated-source-owner-retirement",
            "reply":source.retired,"previous_positive_terminal":source.terminal,"archive_owner_ack":source.owner_ack}))?;
        result.check(actual_cutoff)?;
        Ok(result)
    }
}
