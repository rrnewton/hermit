//! Private recovery state for creation before the runtime handoff. The actual
//! source, writer and controller descriptions are installed before first ACK.
//! A callback cannot derive authority from a received terminal boolean.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EarlyCursor {
    Source,
    Local,
    Submitted,
    Peer,
    Relinquished,
    Refused,
}

/// Minted only when both original pre-admission owners have retained custody.
/// Successful full handoff consumes it before the provider ACK can be sent.
struct AdmissionHeld;

pub(super) struct Prefix {
    value: Value,
    histories: [journal::SourceHistory; 2],
    service_ack: Option<Value>,
    source_ack_attempted: bool,
}

/// Independently owned duplicates of the original native pins. No serialized
/// constructor, successful-source conversion or numeric terminal setter.
struct EarlyNative {
    rights: Vec<OwnedFd>, // original Creator pidfd, cgroup, source Keeper, controller
    creator: Value,
    keeper_pid: i32,
    controller_pid: i32,
    captured: bool,
    source_join: Option<source::SourceRetired>,
    s2_guardian: Option<OwnedFd>,
    s2_guardian_registration: Option<Value>,
    s2_guardian_ack_attempted: bool,
}
impl EarlyNative {
    fn retain(creator: Value, keeper_pid: i32, controller_pid: i32) -> Self {
        Self {
            rights: Vec::new(),
            creator,
            keeper_pid,
            controller_pid,
            captured: false,
            source_join: None,
            s2_guardian: None,
            s2_guardian_registration: None,
            s2_guardian_ack_attempted: false,
        }
    }
    fn initialize(&mut self, source: &[OwnedFd], controller: BorrowedFd<'_>) -> io::Result<()> {
        require(
            !self.captured && self.rights.is_empty() && source.len() == 3,
            "early native capture is incomplete or repeated",
        )?;
        for fd in source {
            self.rights.push(common::duplicate(fd.as_fd())?);
        }
        self.rights.push(common::duplicate(controller)?);
        common::check_creator(&self.rights[..2], &self.creator, false)?;
        owner::pidfd_matches(self.rights[2].as_raw_fd(), self.keeper_pid)?;
        owner::pidfd_matches(self.rights[3].as_raw_fd(), self.controller_pid)?;
        require(
            !owner::terminal(self.rights[2].as_raw_fd())?
                && !owner::terminal(self.rights[3].as_raw_fd())?,
            "early original writers already terminal",
        )?;
        self.captured = true;
        Ok(())
    }
    fn terminal(&self, cutoff: u64) -> io::Result<bool> {
        common::before(cutoff)?;
        require(
            self.captured && self.rights.len() == 4,
            "early original native capture absent",
        )?;
        let directory = owner::stat(self.rights[1].as_raw_fd())?;
        require(
            self.creator["device"] == directory.device
                && self.creator["inode"] == directory.inode
                && directory.mode & libc::S_IFMT == libc::S_IFDIR
                && owner::filesystem(self.rights[1].as_raw_fd())? == 0x6367_7270,
            "early original cgroup object changed",
        )?;
        let source = owner::read_retained_cgroup(
            self.rights[0].as_fd(),
            self.rights[1].as_fd(),
            &directory,
        )?;
        let source_terminal = matches!(source, owner::CgroupReadbackProgress::Observed(ref p)
            if p.creator_terminal && p.unlinked);
        let writers = owner::terminal(self.rights[2].as_raw_fd())?
            && owner::terminal(self.rights[3].as_raw_fd())?
            && match &self.s2_guardian {
                Some(pin) => owner::terminal(pin.as_raw_fd())?,
                None => true,
            };
        common::before(cutoff)?;
        Ok(source_terminal && writers)
    }
    fn check(&self, cutoff: u64) -> io::Result<()> {
        require(
            self.terminal(cutoff)?,
            "early original source/writers remain live or cgroup linked",
        )?;
        self.source_join
            .as_ref()
            .ok_or_else(|| io::Error::other("early actual source Child/manager join absent"))?
            .check(cutoff)
    }
}

/// Stable callback allocation, distinct from Bridge and its Rust input slice.
/// The parent owns all originals throughout synchronous C calls. The moved
/// provider/ledger remain here on every callback error.
pub(super) struct EarlyCallbacks {
    intent: Intent,
    native: EarlyNative,
    pub(super) provider: Option<Link>,
    pub(super) ledger: Option<journal::RemovalJournal>,
    controls: [i32; 3],
    admission: Option<AdmissionHeld>,
    agreement: Option<Value>,
    agreement_digest: Option<String>,
    steps: Vec<ffi::RecoveryStep>,
    eligible: u32,
    observed_mask: Option<u32>,
    origin: Option<u64>,
    cutoff: Option<u64>,
    cursor: EarlyCursor,
    removal_sequence: u64,
    callback_failure: Option<Failure>,
}
impl EarlyCallbacks {
    fn retain(intent: Intent, native: EarlyNative, controls: [i32; 3]) -> Self {
        Self {
            intent,
            native,
            provider: None,
            ledger: None,
            controls,
            admission: None,
            agreement: None,
            agreement_digest: None,
            steps: Vec::new(),
            eligible: 0,
            observed_mask: None,
            origin: None,
            cutoff: None,
            cursor: EarlyCursor::Source,
            removal_sequence: 0,
            callback_failure: None,
        }
    }
    fn check(&self) -> io::Result<u64> {
        if let Some(error) = &self.callback_failure {
            return Err(error.error());
        }
        let cutoff = self
            .cutoff
            .ok_or_else(|| io::Error::other("early original cutoff absent"))?;
        common::before(cutoff)?;
        require(
            self.admission.is_some() && self.agreement.is_some(),
            "early cleanup lacks held admission/agreement",
        )?;
        self.native.check(cutoff)?;
        self.provider
            .as_ref()
            .ok_or_else(|| io::Error::other("early original live peer absent"))?
            .check()?;
        Ok(cutoff)
    }
    fn local(&self) -> io::Result<u64> {
        let cutoff = self.check()?;
        require(
            self.cursor == EarlyCursor::Local,
            "early shared cursor is not local",
        )?;
        Ok(cutoff)
    }
    fn send(&mut self, value: &Value) -> io::Result<()> {
        let cutoff = self.check()?;
        common::send(
            &mut self.provider.as_mut().unwrap().channel,
            value,
            &[],
            cutoff,
        )?;
        self.check().map(|_| ())
    }
    fn receive(&mut self) -> io::Result<Value> {
        let cutoff = self.check()?;
        let peer = self.provider.as_mut().unwrap();
        let value = common::receive_value(&mut peer.channel, peer.credentials, cutoff)?;
        self.check()?;
        Ok(value)
    }
    fn latch(&mut self, result: io::Result<()>) -> libc::c_int {
        match result {
            Ok(()) => 0,
            Err(error) => {
                self.callback_failure
                    .get_or_insert_with(|| Failure::capture(&error));
                self.cursor = EarlyCursor::Refused;
                unsafe {
                    *libc::__errno_location() = error.raw_os_error().unwrap_or(libc::EPROTO);
                }
                -1
            }
        }
    }
    fn acknowledge(
        &mut self,
        native: &ffi::Owner,
        descriptors: *const libc::c_int,
        steps: *const ffi::RecoveryStep,
        count: usize,
    ) -> io::Result<()> {
        let cutoff = self.local()?;
        require(
            count > 0
                && count == self.steps.len()
                && count <= 17
                && self.eligible != 0
                && native.incarnation == self.intent.incarnation
                && native.attempted_sites == self.eligible
                && !descriptors.is_null()
                && !steps.is_null(),
            "early C recovery changed eligible history",
        )?;
        require(
            unsafe { std::slice::from_raw_parts(steps, count) } == self.steps.as_slice(),
            "early C recovery changed exact original steps",
        )?;
        for (index, actual) in unsafe { std::slice::from_raw_parts(descriptors, 3) }
            .iter()
            .enumerate()
        {
            common::same_ofd(unsafe { BorrowedFd::borrow_raw(*actual) }, unsafe {
                BorrowedFd::borrow_raw(self.controls[index])
            })?;
        }
        let digest = self.agreement_digest.as_ref().unwrap().clone();
        // The sender relinquishes first. An ambiguous send is permanently
        // retained as Submitted/Refused; it can never reclaim the shared OFD.
        self.cursor = EarlyCursor::Submitted;
        self.send(
            &json!({"schema":"hermit-cleanup-read-grant-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"epoch":1,"agreement":digest,
            "eligible":self.eligible,"cutoff":cutoff}),
        )?;
        self.cursor = EarlyCursor::Peer;
        let response = self.receive()?;
        let observed = response["observed_mask"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("early peer actual definition mask absent"))?;
        require(
            observed & !self.eligible == 0
                && response
                    == json!({"schema":"hermit-cleanup-read-done-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"epoch":1,"agreement":digest,"cutoff":cutoff,"observed_mask":observed}),
            "early peer cursor relinquishment differs",
        )?;
        self.observed_mask = Some(observed);
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"early-peer-read-relinquished","value":response}))?;
        self.check()?;
        self.cursor = EarlyCursor::Local;
        Ok(())
    }
    fn remove(
        &mut self,
        native: &ffi::Owner,
        write: &ffi::Write,
        line: *const libc::c_char,
    ) -> io::Result<()> {
        let cutoff = self.local()?;
        require(
            !line.is_null() && write.submitted < 256 && self.removal_sequence < 34,
            "early C removal extent or original34 bound differs",
        )?;
        let mask = self
            .observed_mask
            .ok_or_else(|| io::Error::other("early removal actual mask absent"))?;
        let roles = (1..=17)
            .rev()
            .filter(|role| mask & (1 << (role - 1)) != 0)
            .collect::<Vec<_>>();
        require(
            self.removal_sequence < 2 * roles.len() as u64
                && write.role == roles[self.removal_sequence as usize / 2],
            "early C removal changed actual reverse present-role population",
        )?;
        let bytes = unsafe { std::slice::from_raw_parts(line.cast::<u8>(), write.submitted) };
        let owner = rust_owner(native)?;
        let write = rust_write(write)?;
        self.ledger
            .as_mut()
            .unwrap()
            .append(owner.clone(), write.clone(), bytes)?;
        self.removal_sequence += 1;
        let sequence = self.removal_sequence;
        self.send(
            &json!({"schema":"hermit-cleanup-remove-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":sequence,"owner":owner,"write":write,
            "line":hex(bytes),"cutoff":cutoff}),
        )?;
        let response = self.receive()?;
        require(
            response
                == json!({"schema":"hermit-cleanup-remove-ack-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":sequence,"cutoff":cutoff}),
            "early removal actual peer ACK differs",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"early-remove-peer-ACK","value":response}))?;
        self.local().map(|_| ())
    }
}
fn rust_owner(value: &ffi::Owner) -> io::Result<journal::OwnerSnapshot> {
    fn name(bytes: &[libc::c_char]) -> io::Result<String> {
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("early native name lacks NUL"))?;
        require(
            bytes[end..].iter().all(|b| *b == 0),
            "early native name has trailing bytes",
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
fn rust_write(value: &ffi::Write) -> io::Result<journal::Write> {
    require(
        value.error >= 0 && value.started >= 0 && value.completed >= 0,
        "early C write cannot convert negative flags",
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
unsafe extern "C" fn acknowledge(
    context: *mut libc::c_void,
    owner: *const ffi::Owner,
    descriptors: *const libc::c_int,
    steps: *const ffi::RecoveryStep,
    count: usize,
) -> libc::c_int {
    if context.is_null() || owner.is_null() {
        unsafe {
            *libc::__errno_location() = libc::EPROTO;
        }
        return -1;
    }
    let state = unsafe { &mut *context.cast::<EarlyCallbacks>() };
    let result = state.acknowledge(unsafe { &*owner }, descriptors, steps, count);
    state.latch(result)
}
unsafe extern "C" fn remove(
    context: *mut libc::c_void,
    owner: *const ffi::Owner,
    write: *const ffi::Write,
    line: *const libc::c_char,
) -> libc::c_int {
    if context.is_null() || owner.is_null() || write.is_null() {
        unsafe {
            *libc::__errno_location() = libc::EPROTO;
        }
        return -1;
    }
    let state = unsafe { &mut *context.cast::<EarlyCallbacks>() };
    let result = state.remove(unsafe { &*owner }, unsafe { &*write }, line);
    state.latch(result)
}

impl RuntimeKeeper {
    pub(super) fn install_early_cleanup(&mut self) -> io::Result<()> {
        require(
            self.early_callbacks.is_none() && self.initial_histories.is_none(),
            "early preparation cannot repeat",
        )?;
        let intent = self.intent()?.clone();
        let guardian = self.early_stores[0].read(&self.early[0].0["initial_guardian_store"])?;
        let keeper = self.early_stores[1].read(&self.early[0].0["initial_keeper_store"])?;
        self.initial_histories = Some([guardian, keeper]);
        let histories = self.initial_histories.as_ref().unwrap();
        require(
            histories[0].frames.len() == 1
                && histories[1].frames.is_empty()
                && histories[0].frames[0].write.role == 1
                && histories[0].frames[0].write.started == 0,
            "early preparation lacks actual held first intent and healthy empty Keeper",
        )?;
        let controller = self.startup.as_ref().unwrap();
        let keeper_pid = self.early[0].0["keeper_pid"].as_i64().unwrap() as i32;
        let native = EarlyNative::retain(
            self.early[0].0["creator"].clone(),
            keeper_pid,
            controller.credentials.pid,
        );
        let controls = self.early[1]
            .2
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        self.early_callbacks = Some(Box::new(EarlyCallbacks::retain(
            intent.clone(),
            native,
            [controls[0], controls[1], controls[2]],
        )));
        let callbacks = self.early_callbacks.as_mut().unwrap();
        callbacks
            .native
            .initialize(&self.early[0].2, controller.pidfd.as_fd())?;
        let context = (&mut **callbacks as *mut EarlyCallbacks).cast();
        unsafe {
            self.bridge.as_mut().unwrap().prepare(
                intent.incarnation,
                &intent.nonce,
                [
                    BorrowedFd::borrow_raw(controls[0]),
                    BorrowedFd::borrow_raw(controls[1]),
                    BorrowedFd::borrow_raw(controls[2]),
                ],
                self.stage,
                remove,
                context,
            )?;
        }
        common::before(self.stage)
    }

    pub(super) fn forward_early_custody(&mut self) -> io::Result<()> {
        for sequence in 0..5 {
            self.provider.as_ref().unwrap().check()?;
            let rights = if sequence == 2 || sequence == 3 {
                self.early_stores[sequence - 2].held_rights()
            } else {
                &self.early[sequence].2
            };
            let borrowed = rights.iter().map(AsFd::as_fd).collect::<Vec<_>>();
            common::send(
                &mut self.provider.as_mut().unwrap().channel,
                &self.early[sequence].0,
                &borrowed,
                self.stage,
            )?;
        }
        let peer = self.provider.as_mut().unwrap();
        let index = common::receive(&mut peer.channel, peer.credentials, 3, 4096, self.stage)?;
        let packet = &peer.channel.packets[index];
        let mut expected = self.early[4].0.clone();
        expected["schema"] = json!("hermit-grouped-runtime-service-creation-custody-v1");
        require(
            packet.bytes == journal::canonical(&expected)?,
            "early live service custody ACK changed",
        )?;
        for index in 0..3 {
            common::same_ofd(self.early[1].2[index].as_fd(), packet.rights[index].as_fd())?;
        }
        self.provider.as_ref().unwrap().check()?;
        self.ledger.as_mut().unwrap().store.append(json!({"kind":"actual-service-pre-admission-custody",
            "value":expected,"initial_guardian_store":self.initial_histories.as_ref().unwrap()[0].commitment(),
            "initial_keeper_store":self.initial_histories.as_ref().unwrap()[1].commitment()}))?;
        // This token records this actual still-withheld native handoff. It is
        // neither accepted from JSON nor reconstructible after a successful ACK.
        self.early_callbacks.as_mut().unwrap().admission = Some(AdmissionHeld);
        common::before(self.stage)
    }

    fn mirror_prefix(&mut self, value: Value) -> io::Result<()> {
        let intent = self.intent()?.clone();
        let sequence = self.prefixes.len() + 1;
        require(
            sequence <= 34
                && !self.admission_transferred
                && self.early_callbacks.as_ref().unwrap().admission.is_some(),
            "early prefix exceeds original34 or follows handoff",
        )?;
        require(
            value
                == json!({"schema":"hermit-grouped-runtime-creation-prefix-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
            "sequence":sequence,"guardian_store":value["guardian_store"],"keeper_store":value["keeper_store"]}),
            "early current callback identity or sequence differs",
        )?;
        self.startup.as_ref().unwrap().check()?;
        common::check_creator(&self.early[0].2[..2], &self.early[0].0["creator"], false)?;
        require(
            !owner::terminal(self.early[0].2[2].as_raw_fd())?,
            "early source Keeper died before mirror",
        )?;
        let guardian = self.early_stores[0].read(&value["guardian_store"])?;
        let keeper = self.early_stores[1].read(&value["keeper_store"])?;
        self.prefixes.push(Prefix {
            value,
            histories: [guardian, keeper],
            service_ack: None,
            source_ack_attempted: false,
        });
        let current = self.prefixes.last().unwrap();
        require(
            current.histories[0].frames.len() == sequence
                && current.histories[0].frames == current.histories[1].frames,
            "early callback lacks exact actual dual Store prefix",
        )?;
        if sequence > 1 {
            let previous = &self.prefixes[sequence - 2];
            for index in 0..2 {
                require(
                    current.histories[index].frames[..sequence - 1]
                        == previous.histories[index].frames,
                    "early callback rewrote an original mirrored prefix",
                )?;
            }
        }
        let request = current.value.clone();
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-outside-held-callback-prefix","value":request}))?;
        self.provider.as_ref().unwrap().check()?;
        common::send(
            &mut self.provider.as_mut().unwrap().channel,
            &request,
            &[],
            self.stage,
        )?;
        let peer = self.provider.as_mut().unwrap();
        let response = common::receive_value(&mut peer.channel, peer.credentials, self.stage)?;
        let mut expected = request;
        expected["schema"] = json!("hermit-grouped-runtime-creation-prefix-ack-v1");
        self.prefixes.last_mut().unwrap().service_ack = Some(response.clone());
        if response["schema"] == "hermit-grouped-runtime-creation-peer-refused-v1" {
            return Err(self.retain_peer_failure(&response)?);
        }
        require(
            response == expected,
            "early service durable prefix ACK differs",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-service-held-prefix-ACK","value":response}))?;
        self.provider.as_ref().unwrap().check()?;
        self.startup.as_ref().unwrap().check()?;
        // Select only this validated, locally durable, peer-acknowledged
        // candidate. Install before send so an ambiguous native ACK cannot
        // exclude a write which the source may actually have been released to.
        self.mirrored_index = Some(self.prefixes.len() - 1);
        self.prefixes.last_mut().unwrap().source_ack_attempted = true;
        common::send(
            &mut self.startup.as_mut().unwrap().channel,
            &expected,
            &[],
            self.stage,
        )
    }

    pub(super) fn retain_peer_failure(&mut self, value: &Value) -> io::Result<io::Error> {
        let origin = value["first_failure_origin"]
            .as_u64()
            .ok_or_else(|| io::Error::other("early live peer first failure origin absent"))?;
        let cause = value["cause"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| io::Error::other("early live peer first cause absent"))?;
        let intent = self.intent()?;
        require(
            value
                == &json!({"schema":"hermit-grouped-runtime-creation-peer-refused-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
            "first_failure_origin":origin,"cause":cause})
                && origin != 0
                && origin <= guardian::monotonic_ns()?,
            "early live peer refusal changed original fields or origin",
        )?;
        let error = io::Error::other(cause.to_owned());
        if self.failure.is_none() {
            self.failure = Some(Failure::capture(&error));
            self.failure_origin = Some(origin);
        }
        self.peer_failure_origin = Some(
            self.peer_failure_origin
                .map_or(origin, |old| old.min(origin)),
        );
        Ok(error)
    }

    pub(super) fn drive_creation(&mut self) -> io::Result<()> {
        loop {
            common::before(self.stage)?;
            self.progress_owned_source()?;
            self.provider.as_ref().unwrap().check()?;
            let link = self.startup.as_mut().unwrap();
            if let Some(index) = link.channel.receive(4096)? {
                let packet = &link.channel.packets[index];
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&value)? == packet.bytes,
                    "early source packet is not canonical",
                )?;
                packet.exact(
                    if value["schema"] == "hermit-grouped-runtime-s2-guardian-v1" {
                        1
                    } else {
                        0
                    },
                    link.credentials,
                )?;
                match value["schema"].as_str() {
                    Some("hermit-grouped-runtime-s2-guardian-v1") => {
                        self.register_s2_guardian(index, value)?
                    }
                    Some("hermit-grouped-runtime-creation-prefix-v1") => {
                        self.mirror_prefix(value)?
                    }
                    Some("hermit-grouped-runtime-creation-refused-v1") => {
                        return Err(self.retain_controller_failure(value)?);
                    }
                    _ => {
                        return Err(io::Error::other(
                            "early source operation is outside exact prefix protocol",
                        ));
                    }
                }
            }
            // Consume exactly one actual retained packet. The archive reader
            // takes that same frame index; no SCM_RIGHTS peek or synthetic row.
            let provider = self.provider.as_mut().unwrap();
            if let Some(index) = provider.channel.receive(wire::MAX_PACKET)? {
                let packet = &provider.channel.packets[index];
                packet.exact(0, provider.credentials)?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&value)? == packet.bytes,
                    "early provider row is not canonical",
                )?;
                if value["schema"] == "hermit-grouped-runtime-creation-peer-refused-v1" {
                    return Err(self.retain_peer_failure(&value)?);
                }
                return self.receive_completed(index);
            }
            require(
                !owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())?,
                "original creation controller terminated before completed archive",
            )?;
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn register_s2_guardian(&mut self, index: usize, value: Value) -> io::Result<()> {
        common::before(self.stage)?;
        require(
            self.mirrored_index == Some(33)
                && !self.admission_transferred
                && self
                    .early_callbacks
                    .as_ref()
                    .unwrap()
                    .native
                    .s2_guardian
                    .is_none(),
            "S2 Guardian registration precedes completed source or repeats",
        )?;
        let link = self.startup.as_mut().unwrap();
        link.check()?;
        let packet = &mut link.channel.packets[index];
        packet.exact(1, link.credentials)?;
        let pin = packet.rights.remove(0);
        let native = &mut self.early_callbacks.as_mut().unwrap().native;
        native.s2_guardian = Some(pin); // original ownership before field/native validation
        native.s2_guardian_registration = Some(value.clone());
        let pid = value["pid"]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("original S2 Guardian PID absent"))?;
        let intent = self.intent()?.clone();
        require(
            pid > 0
                && value
                    == json!({"schema":"hermit-grouped-runtime-s2-guardian-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,"pid":pid}),
            "original S2 Guardian registration fields changed",
        )?;
        let native = &self.early_callbacks.as_ref().unwrap().native;
        let pin = native.s2_guardian.as_ref().unwrap();
        owner::pidfd_matches(pin.as_raw_fd(), pid)?;
        require(
            !owner::terminal(pin.as_raw_fd())?,
            "original S2 Guardian terminal before control transfer",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-original-S2-Guardian-retained",
            "value":value}))?;
        self.provider.as_ref().unwrap().check()?;
        common::send(
            &mut self.provider.as_mut().unwrap().channel,
            &value,
            &[pin.as_fd()],
            self.stage,
        )?;
        let peer = self.provider.as_mut().unwrap();
        let response = common::receive_value(&mut peer.channel, peer.credentials, self.stage)?;
        let mut ack = value;
        ack["schema"] = json!("hermit-grouped-runtime-s2-guardian-ack-v1");
        require(
            response == ack,
            "live service did not retain exact original S2 Guardian",
        )?;
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-service-S2-Guardian-custody-ACK",
            "value":response}))?;
        self.provider.as_ref().unwrap().check()?;
        let native = &mut self.early_callbacks.as_mut().unwrap().native;
        owner::pidfd_matches(native.s2_guardian.as_ref().unwrap().as_raw_fd(), pid)?;
        require(
            !owner::terminal(native.s2_guardian.as_ref().unwrap().as_raw_fd())?,
            "original S2 Guardian died before control-transfer ACK",
        )?;
        self.startup.as_ref().unwrap().check()?;
        native.s2_guardian_ack_attempted = true;
        common::send(
            &mut self.startup.as_mut().unwrap().channel,
            &ack,
            &[],
            self.stage,
        )
    }

    pub(super) fn early_native_descriptors(&self) -> Vec<i32> {
        self.early_callbacks
            .as_ref()
            .map(|state| {
                let mut fds = state
                    .native
                    .rights
                    .iter()
                    .map(AsRawFd::as_raw_fd)
                    .collect::<Vec<_>>();
                if let Some(pin) = &state.native.s2_guardian {
                    fds.push(pin.as_raw_fd());
                }
                fds
            })
            .unwrap_or_default()
    }
    pub(super) fn check_s2_guardian_registered(&self, terminal: bool) -> io::Result<()> {
        let native = &self.early_callbacks.as_ref().unwrap().native;
        let pin = native
            .s2_guardian
            .as_ref()
            .ok_or_else(|| io::Error::other("actual S2 Guardian pin was never registered"))?;
        let value = native.s2_guardian_registration.as_ref().unwrap();
        let pid = value["pid"]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("actual registered S2 Guardian PID absent"))?;
        require(
            native.s2_guardian_ack_attempted,
            "S2 Guardian control-transfer ACK was never attempted",
        )?;
        if terminal {
            // pidfd_matches requires a live process; this phase follows the
            // acknowledged Guardian's join and retains its original pin.
            require(
                owner::terminal(pin.as_raw_fd())?,
                "actual original S2 Guardian remains live",
            )?;
        } else {
            owner::pidfd_matches(pin.as_raw_fd(), pid)?;
        }
        common::before(self.stage)
    }

    pub(super) fn check_completed_prefix(
        &self,
        index: usize,
        history: &journal::SourceHistory,
    ) -> io::Result<()> {
        require(
            self.mirrored_index == Some(33)
                && self.prefixes.len() == 34
                && index < 2
                && history.frames == self.prefixes[33].histories[index].frames,
            "completed archive changed final actual mirrored callback history",
        )
    }
    pub(super) fn transfer_early_admission(&mut self) -> io::Result<()> {
        let state = self.early_callbacks.as_mut().unwrap();
        require(
            !self.admission_transferred
                && state.admission.is_some()
                && state.provider.is_none()
                && state.ledger.is_none()
                && state.cursor == EarlyCursor::Source
                && self.startup_context_retired,
            "early admission transfer is repeated or follows recovery",
        )?;
        state.admission.take();
        self.admission_transferred = true;
        Ok(())
    }

    pub(super) fn clip_early_original_cutoff(&mut self, cutoff: u64) -> io::Result<()> {
        let callbacks = self
            .early_callbacks
            .as_mut()
            .ok_or_else(|| io::Error::other("early retained owner absent"))?;
        require(
            callbacks.cutoff.is_some_and(|original| cutoff <= original)
                && self
                    .terminal_cutoff
                    .is_some_and(|original| cutoff <= original),
            "early original failure cutoff cannot be refreshed",
        )?;
        common::before(cutoff)?;
        callbacks.cutoff = Some(cutoff);
        self.terminal_cutoff = Some(cutoff);
        Ok(())
    }

    pub(super) fn recover_failed_creation(&mut self, primary: &io::Error) {
        if self.failure.is_none() {
            self.failure = Some(Failure::capture(primary));
            self.failure_origin = guardian::monotonic_ns().ok();
        }
        // Keep the original error observable before recovery transfers the
        // journal and performs secondary operations. Receipt failure is
        // retained separately and must not suppress actual physical cleanup.
        if let Err(error) = self.record_creation_failure(primary) {
            self.failure_receipt_error
                .get_or_insert_with(|| Failure::capture(&error));
        }
        let result = self.recover_creation_inner();
        if let Err(error) = result {
            self.early_cleanup_error = Some(Failure::capture(&error));
        }
    }

    fn drain_pending_failure_notice(&mut self) -> io::Result<()> {
        let Some(provider) = self.provider.as_mut() else {
            return Ok(());
        };
        if let Some(index) = provider.channel.receive(4096)? {
            let packet = &provider.channel.packets[index];
            packet.exact(0, provider.credentials)?;
            let value: Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "early pending failure notice is not canonical",
            )?;
            self.retain_peer_failure(&value)?;
        }
        Ok(())
    }

    fn recover_creation_inner(&mut self) -> io::Result<()> {
        self.poll_controller_custody(self.stage)?;
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("early first failure origin unknown"))?;
        self.drain_pending_failure_notice()?;
        let earliest = self
            .peer_failure_origin
            .map_or(origin, |other| other.min(origin));
        let cutoff = self.stage.min(
            earliest
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("early original failure cutoff overflow"))?,
        );
        common::before(cutoff)?;
        require(
            !self.admission_transferred
                && !self.startup_context_retired
                && self.early_callbacks.is_some(),
            "early cleanup cannot recover an absent or transferred admission owner",
        )?;
        self.terminal_origin = Some(origin);
        self.terminal_cutoff = Some(cutoff);
        let callbacks = self.early_callbacks.as_mut().unwrap();
        require(
            callbacks.admission.is_some()
                && callbacks.origin.is_none()
                && callbacks.provider.is_none()
                && callbacks.ledger.is_none(),
            "early cleanup owner missing or repeated",
        )?;
        callbacks.origin = Some(origin);
        callbacks.cutoff = Some(cutoff);
        callbacks.provider = self.provider.take();
        callbacks.ledger = self.ledger.take();
        callbacks.provider.as_ref().unwrap().check()?;
        let source_join = self.retire_owned_source_after_failure(origin, cutoff)?;
        let cutoff = self.early_callbacks.as_ref().unwrap().cutoff.unwrap();
        self.request_live_parent_join(origin, cutoff)?;
        let callbacks = self.early_callbacks.as_mut().unwrap();
        callbacks.native.source_join = Some(source_join);
        // Wait only for the original held objects. No replacement process,
        // manager query, signal, reset or refreshed origin is introduced here.
        loop {
            common::before(cutoff)?;
            callbacks.provider.as_ref().unwrap().check()?;
            if callbacks.native.terminal(cutoff)? {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        callbacks.native.check(cutoff)?;
        self.finish_live_parent_join(origin, cutoff)?;
        self.finish_controller_custody(cutoff)?;
        let callbacks = self.early_callbacks.as_mut().unwrap();
        let selected = if let Some(index) = self.mirrored_index {
            &self.prefixes[index].histories
        } else {
            self.initial_histories
                .as_ref()
                .ok_or_else(|| io::Error::other("early initial healthy histories absent"))?
        };
        // The held Keeper callback prevents any native write beyond the
        // durably mirrored intent. Validate those exact acknowledged bytes;
        // retain every later byte separately, never as healthy source history.
        // Both original writers are terminal before either actual pread.
        require(
            self.early_prefix_readbacks.is_empty(),
            "early acknowledged-prefix read cannot repeat",
        )?;
        for (index, history) in selected.iter().enumerate() {
            callbacks.native.check(cutoff)?;
            let actual = self.early_stores[index].read_acknowledged_prefix(history)?;
            self.early_prefix_readbacks.push(actual);
        }
        let guardian = &self.early_prefix_readbacks[0].history;
        let keeper = &self.early_prefix_readbacks[1].history;
        let (steps, eligible, relation) = if self.mirrored_index.is_none() {
            require(
                guardian.frames.len() == 1
                    && keeper.frames.is_empty()
                    && guardian.frames[0].sequence == 1
                    && guardian.frames[0].write.started == 0,
                "early zero-write path lacks actual initial unacknowledged intent",
            )?;
            (
                Vec::new(),
                0,
                "guardian-first-intent-not-acknowledged-no-writes",
            )
        } else {
            super::super::cleanup::select_runtime_creation_recovery(
                &callbacks.intent,
                guardian,
                keeper,
            )?
        };
        callbacks.steps = steps;
        callbacks.eligible = eligible;
        let cause = self.failure.as_ref().unwrap().error().to_string();
        let agreement = json!({"schema":"hermit-grouped-runtime-creation-agreement-v1",
            "nonce":callbacks.intent.nonce,"incarnation":callbacks.intent.incarnation,"stage_deadline":self.stage,
            "original_start":origin,"cutoff":cutoff,"cause":cause,"source":callbacks.native.creator,
            "keeper_pid":callbacks.native.keeper_pid,"guardian_store":guardian.commitment(),
            "keeper_store":keeper.commitment(),"sequence":self.mirrored_index.map_or(0, |index| index + 1),"eligible":eligible,"relation":relation});
        callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-early-creation-agreement","value":agreement}))?;
        callbacks.agreement_digest = Some(hex(&Sha256::digest(journal::canonical(&agreement)?)));
        callbacks.agreement = Some(agreement.clone());
        callbacks.send(&agreement)?;
        let response = callbacks.receive()?;
        let mut expected = agreement;
        expected["schema"] = json!("hermit-grouped-runtime-creation-agreement-ack-v1");
        require(
            response == expected,
            "early actual peer history/terminal agreement differs",
        )?;
        callbacks
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-early-agreement-peer-ACK","value":response}))?;
        callbacks.cursor = EarlyCursor::Local;
        // Retain final healthy byte strings independently before C effects.
        let guardian_bytes = guardian.bytes.clone();
        let keeper_bytes = keeper.bytes.clone();
        let guardian_tail = self.early_prefix_readbacks[0].unacknowledged_tail.clone();
        let keeper_tail = self.early_prefix_readbacks[1].unacknowledged_tail.clone();
        self.write_file("early-final-guardian-history", &guardian_bytes, cutoff)?;
        self.write_file("early-final-keeper-history", &keeper_bytes, cutoff)?;
        self.write_file("early-unacknowledged-guardian-tail", &guardian_tail, cutoff)?;
        self.write_file("early-unacknowledged-keeper-tail", &keeper_tail, cutoff)?;
        self.delete_early_prefix(origin, cutoff)?;
        self.finish_early_peer(origin, cutoff)?;
        self.early_cleanup_complete = true;
        Ok(())
    }
}

impl RuntimeKeeper {
    fn delete_early_prefix(&mut self, origin: u64, cutoff: u64) -> io::Result<()> {
        self.early_callbacks.as_ref().unwrap().local()?;
        let eligible = self.early_callbacks.as_ref().unwrap().eligible;
        if eligible != 0 {
            // The submitted slice is independent of the callback Box. No
            // mutable callback can alias its Rust input through that Box.
            let steps = self.early_callbacks.as_ref().unwrap().steps.clone();
            let context =
                (&mut **self.early_callbacks.as_mut().unwrap() as *mut EarlyCallbacks).cast();
            let original = ffi::Release {
                release_start: origin,
                first_failure_origin: self
                    .peer_failure_origin
                    .map_or(origin, |earlier| earlier.min(origin)),
                enclosing_cutoff: cutoff,
                has_first_failure: 1,
            };
            unsafe {
                self.bridge
                    .as_mut()
                    .unwrap()
                    .adopt(&steps, acknowledge, context, original)?;
            }
            self.early_callbacks.as_ref().unwrap().local()?;
            let actual = self
                .bridge
                .as_ref()
                .unwrap()
                .status()
                .ok_or_else(|| io::Error::other("early recovered C status absent"))?;
            require(
                actual.adopted == 1
                    && actual.refused == 0
                    && actual.owner.attempted_sites == eligible
                    && Some(actual.owner.verified_sites)
                        == self.early_callbacks.as_ref().unwrap().observed_mask,
                "early adopted C current mask differs from independent peer observation",
            )?;
            unsafe {
                self.bridge.as_mut().unwrap().delete()?;
            }
            self.early_callbacks.as_ref().unwrap().local()?;
            let state = self.early_callbacks.as_mut().unwrap();
            state.ledger.as_mut().unwrap().complete()?;
            require(
                state.removal_sequence == 2 * state.observed_mask.unwrap().count_ones() as u64,
                "early C removal callback population differs from actual owned definitions",
            )?;
            for index in 0..2 {
                self.early_callbacks.as_ref().unwrap().local()?;
                let actual = unsafe { self.bridge.as_mut().unwrap().observe_absent()? };
                let metadata = actual
                    .metadata
                    .ok_or_else(|| io::Error::other("early C observation metadata absent"))?;
                let observed = metadata.observation;
                let definitions = actual
                    .definitions()
                    .ok_or_else(|| io::Error::other("early C definitions not copied"))?
                    .to_vec();
                let profile = actual
                    .profile()
                    .ok_or_else(|| io::Error::other("early C profile not copied"))?
                    .to_vec();
                require(
                    observed.complete == 1
                        && observed.started_observed == 1
                        && observed.finished_observed == 1
                        && observed.cutoff == cutoff
                        && observed.started_ns >= origin
                        && observed.finished_ns >= observed.started_ns
                        && observed.finished_ns < cutoff,
                    "early C absence original observation bound differs",
                )?;
                let intent = self.intent()?.clone();
                let row = json!({"schema":"hermit-grouped-runtime-absence-v1",
                    "started":observed.started_ns,"completed":observed.finished_ns,"cutoff":cutoff,
                    "definitions":{"bytes":definitions.len(),"sha256":hex(&Sha256::digest(&definitions))},
                    "profile":{"bytes":profile.len(),"sha256":hex(&Sha256::digest(&profile))},
                    "directories":[
                        {"path":intent.group(),"attempted":observed.group_directory.attempted == 1,
                            "returned":observed.group_directory.returned == 1,"raw":observed.group_directory.result,
                            "errno":observed.group_directory.error},
                        {"path":format!("{}/{}",intent.group(),intent.event()),"attempted":observed.event_directory.attempted == 1,
                            "returned":observed.event_directory.returned == 1,"raw":observed.event_directory.result,
                            "errno":observed.event_directory.error}]});
                self.observations.push(row.clone());
                self.write_file(
                    &format!("early-absence-{index}-definitions"),
                    &definitions,
                    cutoff,
                )?;
                self.write_file(&format!("early-absence-{index}-profile"), &profile, cutoff)?;
                self.early_callbacks
                    .as_mut()
                    .unwrap()
                    .ledger
                    .as_mut()
                    .unwrap()
                    .store
                    .append(json!({"kind":"actual-early-C-absence","index":index,"value":row}))?;
            }
        } else {
            // No attempted source write was released. Never set C adopted or
            // deleted merely to get its absence API past its real preconditions.
            for index in 0..2 {
                self.early_observations
                    .push(common::AbsenceObservation::retain(cutoff));
                let state = self.early_callbacks.as_ref().unwrap();
                let controls = self.early[1].2.iter().map(AsFd::as_fd).collect::<Vec<_>>();
                common::observe_absent(
                    self.early_observations.last_mut().unwrap(),
                    [controls[0], controls[1], controls[2]],
                    &state.intent,
                    cutoff,
                    || state.local().map(|_| ()),
                )?;
                let observation = self.early_observations.last().unwrap();
                let definitions = observation.definitions().to_vec();
                let profile = observation.profile().to_vec();
                let row = observation.receipt();
                self.observations.push(row.clone());
                self.write_file(
                    &format!("early-absence-{index}-definitions"),
                    &definitions,
                    cutoff,
                )?;
                self.write_file(&format!("early-absence-{index}-profile"), &profile, cutoff)?;
                self.early_callbacks
                    .as_mut()
                    .unwrap()
                    .ledger
                    .as_mut()
                    .unwrap()
                    .store
                    .append(
                        json!({"kind":"actual-early-zero-write-absence","index":index,"value":row}),
                    )?;
            }
        }
        common::validate_absence_rows(&json!(self.observations), self.intent()?, origin, cutoff)?;
        self.early_callbacks.as_ref().unwrap().local()?;
        // Actual C context/status/history and copied absence bytes survive
        // explicit local alias retirement. It cannot erase original failure.
        unsafe {
            self.bridge.as_mut().unwrap().release_aliases()?;
        }
        self.early_callbacks.as_ref().unwrap().local()?;
        self.write_file(
            "early-recovery-C-context.txt",
            format!(
                "status={:#?}\nhistory={:#?}\nattempts={:#?}\nreadbacks={:#?}\n",
                self.bridge.as_ref().unwrap().status(),
                self.bridge.as_ref().unwrap().history(),
                self.bridge.as_ref().unwrap().attempts(),
                self.bridge.as_ref().unwrap().readbacks()
            )
            .as_bytes(),
            cutoff,
        )?;
        unsafe {
            self.bridge.as_mut().unwrap().free()?;
        }
        self.early_callbacks.as_ref().unwrap().local()?;
        require(
            !self.bridge.as_ref().unwrap().context_retained()
                && !self.bridge.as_ref().unwrap().loader_retained(),
            "early C context/loader retained after successful local free",
        )?;
        self.write_file(
            "early-recovery-C-free.txt",
            format!(
                "attempts={:#?}\nloader_close={:#?}\n",
                self.bridge.as_ref().unwrap().attempts(),
                self.bridge.as_ref().unwrap().loader_close()
            )
            .as_bytes(),
            cutoff,
        )?;
        Ok(())
    }

    fn finish_early_peer(&mut self, origin: u64, cutoff: u64) -> io::Result<()> {
        let state = self.early_callbacks.as_mut().unwrap();
        state.local()?;
        let identity = json!({"nonce":state.intent.nonce,"incarnation":state.intent.incarnation,
            "original_start":origin,"cutoff":cutoff,"agreement":state.agreement_digest});
        let mut grant = identity.clone();
        grant["schema"] = json!("hermit-grouped-runtime-creation-absence-grant-v1");
        state.cursor = EarlyCursor::Submitted;
        state.send(&grant)?;
        state.cursor = EarlyCursor::Peer;
        let response = state.receive()?;
        let mut expected = identity.clone();
        expected["schema"] = json!("hermit-grouped-runtime-creation-absence-done-v1");
        expected["observations"] = response["observations"].clone();
        require(
            response == expected,
            "early peer absence identity/cutoff differs",
        )?;
        common::validate_absence_rows(&response["observations"], &state.intent, origin, cutoff)?;
        state
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-early-peer-two-absence","value":response}))?;
        // The read-done relinquishes peer access; there are no more native
        // reads or deletion operations on either side after this point.
        state.cursor = EarlyCursor::Relinquished;
        let mut retire = identity.clone();
        retire["schema"] = json!("hermit-grouped-runtime-creation-retire-v1");
        state.send(&retire)?;
        let response = state.receive()?;
        let mut expected = identity;
        expected["schema"] = json!("hermit-grouped-runtime-creation-retired-v1");
        expected["aliases_retired"] = json!(true);
        require(
            response == expected,
            "early actual service alias retirement differs",
        )?;
        state
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"actual-early-service-retired","value":response}))?;
        state.ledger.as_mut().unwrap().complete()?;
        // Peer may now exit with its original bootstrap failure. All global
        // effects have ended; retirement is local and retains its native wait
        // obligation with the original CLI owner, never a fake success0.
        self.retire_early_aliases(cutoff)
    }

    fn retire_early_aliases(&mut self, cutoff: u64) -> io::Result<()> {
        require(
            !self.retirement_started,
            "early alias retirement cannot repeat",
        )?;
        self.retirement_started = true;
        common::before(cutoff)?;
        owner::check_no_children()?;
        self.census(cutoff)?;
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
        // Both Channels retain every SCM receipt, including ACK control
        // aliases. Those real owners must not be omitted from final census.
        for channel in [
            &self.input,
            &self.startup.as_ref().unwrap().channel,
            &self
                .early_callbacks
                .as_ref()
                .unwrap()
                .provider
                .as_ref()
                .unwrap()
                .channel,
        ] {
            for packet in &channel.packets {
                descriptors.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
            }
        }
        descriptors.extend(self.early_native_descriptors());
        descriptors.push(
            self.early_callbacks
                .as_ref()
                .unwrap()
                .native
                .source_join
                .as_ref()
                .unwrap()
                .descriptor(),
        );
        descriptors.extend(self.owned_source_descriptors()?);
        if let Some(fd) = self.controller_custody_descriptor() {
            descriptors.push(fd);
        }
        descriptors.extend([
            self.cli_pidfd.as_ref().unwrap().as_raw_fd(),
            self.run_pidfd.as_ref().unwrap().as_raw_fd(),
            self.bridge_file.unwrap(),
            self.startup.as_ref().unwrap().pidfd.as_raw_fd(),
            self.startup.as_ref().unwrap().channel.fd.as_raw_fd(),
        ]);
        descriptors.sort_unstable();
        require(
            descriptors.windows(2).all(|pair| pair[0] != pair[1]),
            "early explicit custody repeats descriptor",
        )?;
        for fd in descriptors {
            self.retain_close(fd, cutoff)?;
        }
        for (chunk, rows) in self.closes.chunks(16).enumerate() {
            self.early_callbacks.as_mut().unwrap().ledger.as_mut().unwrap().store.append(json!({
                "kind":"actual-early-local-alias-retirement","chunk":chunk,
                "closes":rows.iter().map(|r| json!({"fd":r.fd,"attempted":r.attempted,"raw":r.raw,"errno":r.errno})).collect::<Vec<_>>()}))?;
        }
        self.early_callbacks
            .as_mut()
            .unwrap()
            .ledger
            .as_mut()
            .unwrap()
            .complete()?;
        common::before(cutoff)?;
        owner::check_no_children()?;
        self.census(cutoff)?;
        let state = self.early_callbacks.as_ref().unwrap();
        let ledger = state
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
        let provider = state.provider.as_ref().unwrap();
        let channel = provider.channel.fd.as_raw_fd();
        let peer_pin = provider.pidfd.as_raw_fd();
        let input = self.input.fd.as_raw_fd();
        let mut expected = vec![
            0, 1, 2, ledger, directory, receipt, census, channel, peer_pin, input,
        ];
        expected.sort_unstable();
        expected.dedup();
        require(
            self.census.last_descriptors()? == expected,
            "early final FD census has unowned custody",
        )?;
        for fd in [ledger, directory, receipt, census, channel, peer_pin, input] {
            self.retain_close(fd, cutoff)?;
        }
        owner::check_no_children()?;
        common::before(cutoff)
    }
}

impl RuntimeKeeper {
    fn request_live_parent_join(&mut self, origin: u64, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        if owner::terminal(self.cli_pidfd.as_ref().unwrap().as_raw_fd())? {
            return Ok(());
        }
        require(
            self.parent_join_request.is_none(),
            "original live parent join request cannot repeat",
        )?;
        let intent = self.intent()?.clone();
        let request = json!({"schema":"hermit-grouped-runtime-startup-failed-join-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"original_start":origin,"cutoff":cutoff});
        self.parent_join_request = Some(request.clone());
        let result = common::send(&mut self.input, &request, &[], cutoff);
        if let Err(error) = result {
            if owner::terminal(self.cli_pidfd.as_ref().unwrap().as_raw_fd())? {
                self.parent_join_reply =
                    Some(json!({"kind":"original-parent-terminal-during-join-send",
                    "error":error.to_string(),"original_start":origin,"cutoff":cutoff}));
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
    fn finish_live_parent_join(&mut self, origin: u64, cutoff: u64) -> io::Result<()> {
        if self.parent_join_request.is_none() || self.parent_join_reply.is_some() {
            return Ok(());
        }
        loop {
            common::before(cutoff)?;
            if owner::terminal(self.cli_pidfd.as_ref().unwrap().as_raw_fd())? {
                self.parent_join_reply = Some(
                    json!({"kind":"original-parent-terminal-during-join-receive",
                    "original_start":origin,"cutoff":cutoff}),
                );
                break;
            }
            if let Some(index) = self.input.receive(4096)? {
                let packet = &self.input.packets[index];
                packet.exact(0, self.parent.unwrap())?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                self.parent_join_reply = Some(value.clone());
                require(
                    packet.bytes == journal::canonical(&value)?,
                    "actual parent failed-join row is not canonical",
                )?;
                let raw = value["raw_wait_status"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| io::Error::other("actual parent failed-join raw wait absent"))?;
                if value["schema"] == "hermit-grouped-runtime-startup-already-joined-v1" {
                    let intent = self.intent()?;
                    let pid = self.startup.as_ref().unwrap().credentials.pid;
                    require(
                        value
                            == json!({"schema":"hermit-grouped-runtime-startup-already-joined-v1",
                        "nonce":intent.nonce,"incarnation":intent.incarnation,"original_start":origin,"cutoff":cutoff,
                        "pid":pid,"raw_wait_status":0,"eof":[true,true],"group_absent":true,"logs_synced":true,
                        "global_ECHILD_claimed":false,"retained_join":{"schema":"hermit-grouped-parent-startup-joined-v1",
                            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
                            "raw_wait_status":0,"eof":[true,true],"group_absent":true}}),
                        "actual already-joined startup differs from original successful wait",
                    )?;
                    require(
                        owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())?,
                        "already-joined original startup remains live",
                    )?;
                    break;
                }
                require(
                    libc::WIFEXITED(raw) && libc::WEXITSTATUS(raw) > 0,
                    "actual parent failed-join is not natural nonzero",
                )?;
                let intent = self.intent()?;
                let pid = self.startup.as_ref().unwrap().credentials.pid;
                require(
                    value
                        == json!({"schema":"hermit-grouped-runtime-startup-failed-joined-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,"original_start":origin,"cutoff":cutoff,
                    "pid":pid,"raw_wait_status":raw,"eof":[true,true],"group_absent":true,"logs_synced":true,
                    "global_ECHILD_claimed":false,"waitid":{"pid":pid,"waitid_raw":0,"waitid_pid":pid,
                        "waitid_code":libc::CLD_EXITED,"waitid_status":libc::WEXITSTATUS(raw),"wait_consumed":false,
                        "stdout_eof":true,"stderr_eof":true,"logs_synced":true,"global_ECHILD_claimed":false}}),
                    "actual parent failed-join changed original child or cutoff",
                )?;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // This receipt is an additional original parent cleanup obligation.
        // It never substitutes for the locally owned SourceRetired capability.
        self.early_callbacks
            .as_mut()
            .unwrap()
            .ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({
            "kind":"original-live-parent-startup-join","value":self.parent_join_reply}))?;
        common::before(cutoff)
    }
}
