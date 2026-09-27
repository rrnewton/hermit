//! S2's retained Keeper side of the real created-control transfer.
//!
//! This owner lives in the isolated controller which already joined S1. It
//! consumes that controller's actual SerialOwner, not a decoded SourceTerminal.
//! Its new Guardian is a separate process. Neither an offer nor a successful
//! ACK is provider-open authority: the helper must still complete the existing
//! C adoption, leaf join/bind and the checked native ProviderLease operation.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::time::Instant;

use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::super::Failure;
use super::super::guardian;
use super::super::hex;
use super::super::journal;
use super::super::owner;
use super::super::require;
use super::super::wire;
use super::LeafPlan;
use super::Phase;
use super::SerialOwner;
pub(super) mod completed;
mod provider;
pub(in super::super) use provider::ProviderIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Retained,
    Creator,
    Authenticating,
    Offer,
    Controls,
    Roles,
    Echo,
    Acknowledged,
}

#[derive(Debug)]
struct Offer {
    name: String,
    transcript_sha256: String,
    bytes: Vec<u8>,
    expected_echo: Vec<u8>,
    acknowledgement: Vec<u8>,
    send_attempted: bool,
    echo_index: Option<usize>,
    acknowledgement_attempted: bool,
}

/// The caller installs this complete owner before initialize or progress.
/// Every received right stays in a Channel/Creator/Controls slot on refusal;
/// neither a failed send nor an ACK timeout returns those resources to a
/// retryable constructor. No destructor claims deletion or releases custody.
#[derive(Debug)]
#[must_use = "S2 failure retains S1 histories, children and original controls"]
pub(in super::super) struct RetainedSuccessor {
    source: SerialOwner,
    channel: wire::Channel,
    guardian_endpoint: OwnedFd,
    guardian: owner::Launcher,
    provider: ProviderIdentity,
    store: journal::Store,
    configured_intent: super::super::Intent,
    creator_cutoff: u64,
    creator: Option<owner::Creator>,
    manager: Option<owner::ManagerQuery>,
    image: Option<owner::EntryQuery>,
    roles: Option<owner::RoleQuery>,
    manager_snapshot: Option<owner::ManagerSnapshot>,
    image_snapshot: Option<owner::EntrySnapshot>,
    role_snapshot: Option<owner::RoleSnapshot>,
    controls: Option<owner::Controls>,
    offer: Option<Offer>,
    guardian_forward_attempted: bool,
    observations: Vec<owner::CreatedObservations>,
    census: owner::CensusInventory,
    stage: Stage,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
}

impl RetainedSuccessor {
    /// Custody only. In particular, consuming a failed SerialOwner here does
    /// not clear it or manufacture a leaf plan. Validation happens only after
    /// the caller has retained this object outside cancellable request work.
    pub fn retain(
        source: SerialOwner,
        channel: OwnedFd,
        guardian_endpoint: OwnedFd,
        guardian: owner::Launcher,
        provider: ProviderIdentity,
        journal_directory: OwnedFd,
        creator_cutoff: u64,
        intent: super::super::Intent,
    ) -> Self {
        Self {
            source,
            channel: wire::Channel::retain(channel),
            guardian_endpoint,
            guardian,
            provider,
            store: journal::Store::retain(journal_directory, &intent),
            configured_intent: intent,
            creator_cutoff,
            creator: None,
            manager: None,
            image: None,
            roles: None,
            manager_snapshot: None,
            image_snapshot: None,
            role_snapshot: None,
            controls: None,
            offer: None,
            guardian_forward_attempted: false,
            observations: Vec::new(),
            census: owner::CensusInventory::retain(),
            stage: Stage::Retained,
            refusal: None,
            failure_origin: None,
        }
    }

    pub fn local_custody_records(&self) -> io::Result<Vec<serde_json::Value>> {
        let mut records = self.source.local_custody_records()?;
        records.push(json!({"kind":"launcher","role":"successor-guardian","record":self.guardian.custody_evidence()}));
        for (role, record) in [
            (
                "manager",
                self.manager.as_ref().map(owner::ManagerQuery::evidence),
            ),
            (
                "image",
                self.image.as_ref().map(owner::EntryQuery::evidence),
            ),
            ("roles", self.roles.as_ref().map(owner::RoleQuery::evidence)),
        ] {
            if let Some(record) = record {
                records.push(
                    json!({"kind":"query","role":format!("successor-{role}"),"record":record}),
                );
            }
        }
        Ok(records)
    }
    pub fn failure_notice_origin(&self) -> io::Result<u64> {
        self.source.failure_notice_origin()
    }
    pub fn send_local_custody(
        &mut self,
        report: &serde_json::Value,
        file: std::os::fd::BorrowedFd<'_>,
    ) -> io::Result<()> {
        self.source.send_local_custody(report, file)
    }
    pub fn retire_local_custody(
        &mut self,
        logs: std::os::fd::BorrowedFd<'_>,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<bool> {
        let deadline = if self.refusal.is_some() {
            guardian::clip_custody_origin(deadline, self.failure_origin)?
        } else {
            deadline
        };
        let mut outcomes = Vec::new();
        if let Some(query) = &mut self.manager {
            outcomes.push(match query.successful_resources_retired() {
                Ok(true) => Ok(owner::QueryRetirement::Retired),
                Ok(false) => query.retire_custody(deadline, cause),
                Err(error) => Err(error),
            });
        }
        if let Some(query) = &mut self.image {
            outcomes.push(match query.successful_resources_retired() {
                Ok(true) => Ok(owner::QueryRetirement::Retired),
                Ok(false) => query.retire_custody(deadline, cause),
                Err(error) => Err(error),
            });
        }
        if let Some(query) = &mut self.roles {
            outcomes.push(match query.successful_resources_retired() {
                Ok(true) => Ok(owner::QueryRetirement::Retired),
                Ok(false) => query.retire_custody(deadline, cause),
                Err(error) => Err(error),
            });
        }
        let mut complete = true;
        let mut first = None;
        for outcome in outcomes {
            match outcome {
                Ok(owner::QueryRetirement::Pending) => complete = false,
                Ok(owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired) => {}
                Err(error) => {
                    complete = false;
                    if first.is_none() {
                        first = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first {
            return Err(error);
        }
        if !complete {
            return Ok(false);
        }
        // S1's local Keeper and queries already joined before S2 can exist.
        // The sole remaining original local Child is this S2 Guardian.
        Ok(matches!(
            self.guardian
                .retire_custody(logs.as_raw_fd(), deadline, cause)?,
            owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired
        ))
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
        if let Err(error) = &result {
            if self.refusal.is_none() {
                self.refusal = Some(Failure::capture(error));
                // Failure to sample remains unknown. Never substitute a later
                // cleanup request's clock reading for this original origin.
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        }
        result
    }

    fn plan(&self) -> io::Result<&LeafPlan> {
        self.source.check()?;
        let Some(Phase::Planned(plan)) = &self.source.phase else {
            return Err(io::Error::other("S2 lacks the actual consumed leaf plan"));
        };
        require(
            plan.intent_attempted
                && plan.intent_complete
                && plan.refused.is_none()
                && plan.created_pairs.is_some(),
            "S2 leaf intent is incomplete or refused",
        )?;
        plan.source.source.original.check()?;
        Ok(plan)
    }

    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        let plan = self.plan()?;
        require(
            self.configured_intent.nonce == plan.source.source.intent().nonce
                && self.configured_intent.incarnation == plan.source.source.intent().incarnation,
            "S2 journal configuration differs from actual leaf custody",
        )?;
        let now = guardian::monotonic_ns()?;
        require(
            now < plan.source.source.native_deadline()
                && Instant::now() < plan.source.source.deadline(),
            "S2 exceeded the original complete startup deadline",
        )?;
        require(
            self.creator_cutoff != 0 && self.creator_cutoff <= plan.source.source.native_deadline(),
            "S2 creator cutoff differs from original startup bound",
        )?;
        if !matches!(self.stage, Stage::Echo | Stage::Acknowledged) {
            require(
                now < self.creator_cutoff,
                "S2 original creator cutoff elapsed",
            )?;
        }
        Ok(())
    }

    pub fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(self.stage == Stage::Retained, "S2 initialize cannot repeat")?;
            owner::protected_holder()?;
            let now = guardian::monotonic_ns()?;
            require(
                self.creator_cutoff > now && self.creator_cutoff - now <= 1_000_000_000,
                "S2 cutoff is not the original bounded creator window",
            )?;
            self.channel.validate()?;
            check_launcher(&self.guardian)?;
            self.check_guardian_endpoint()?;
            let plan = self.plan()?;
            let unit = plan.request.unit.clone();
            let arguments = plan.request.arguments.clone();
            let run = plan.source.source.intent().nonce.clone();
            let deadline = plan.source.source.native_deadline();
            self.provider
                .check_bound(&unit, &arguments, &run, deadline)?;
            let plan = self.plan()?;
            let pairs = plan.created_pairs.as_ref().unwrap();
            let header = json!({"schema":"hermit-grouped-successor-keeper-v1",
                "nonce":plan.source.source.intent().nonce,
                "incarnation":plan.source.source.intent().incarnation,
                "source_unit":plan.source.source.original.unit,
                "unit":plan.request.unit,
                "stage_deadline":plan.source.source.native_deadline(),
                "creator_cutoff":self.creator_cutoff,
                "transcript_sha256":hex(&Sha256::digest(pairs))});
            self.store.initialize(header)?;
            self.stage = Stage::Creator;
            self.check()
        })();
        self.remember(result)
    }

    fn check_guardian_endpoint(&self) -> io::Result<()> {
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        let raw = unsafe {
            libc::getsockopt(
                self.guardian_endpoint.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut size,
            )
        };
        if raw != 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            size as usize == std::mem::size_of_val(&peer)
                && peer.pid == self.guardian.child.id() as i32
                && peer.pid != unsafe { libc::getpid() }
                && peer.uid == unsafe { libc::getuid() }
                && peer.gid == unsafe { libc::getgid() },
            "S2 Guardian endpoint is not the retained independent child",
        )?;
        check_launcher(&self.guardian)
    }

    fn check_creator(&self) -> io::Result<()> {
        self.check()?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("S2 Creator absent"))?;
        require(creator.admitted, "S2 Creator was not authenticated")?;
        // The original CLI owns the real provider wrapper. This independent
        // controller receives its captured native handles; it neither invents
        // Child ownership nor requires the provider to be its Unix parent.
        self.provider.matches_creator(creator)?;
        owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
        creator.process_policy()?;
        let current = creator.readback()?;
        require(
            !current.creator_terminal
                && !current.unlinked
                && current
                    .procs
                    .as_ref()
                    .is_some_and(|p| p.lines().any(|line| line == creator.peer.pid.to_string())),
            "S2 Creator left the retained live cgroup",
        )
    }

    fn census(&mut self) -> io::Result<()> {
        let plan = self.plan()?;
        let deadline = plan.source.source.deadline();
        let mut required = plan.source.source.original.required_descriptors();
        required.extend([
            self.channel.fd.as_raw_fd(),
            self.guardian_endpoint.as_raw_fd(),
        ]);
        required.extend(self.provider.descriptors());
        required.extend(
            self.creator
                .iter()
                .flat_map(|c| [c.pidfd.as_raw_fd(), c.directory.as_raw_fd()]),
        );
        required.extend(
            self.controls
                .iter()
                .flat_map(|c| c.fds.iter().map(AsRawFd::as_raw_fd)),
        );
        required.extend(
            self.channel
                .packets
                .iter()
                .flat_map(|p| p.rights.iter().map(AsRawFd::as_raw_fd)),
        );
        for launcher in [&self.guardian] {
            required.extend(launcher.pidfd.iter().map(AsRawFd::as_raw_fd));
            required.extend(launcher.log_files.iter().flatten().map(AsRawFd::as_raw_fd));
            required.extend(launcher.child.stdout.iter().map(AsRawFd::as_raw_fd));
            required.extend(launcher.child.stderr.iter().map(AsRawFd::as_raw_fd));
        }
        self.census.observe(&required, deadline)
    }

    /// No same-process read lease is issued while S2 owns the cursor. The
    /// authenticated maintained helper sends this exact echo only after its
    /// own fresh reads, then waits for this one-use ACK before any continuation.
    fn observe_during_echo(&mut self) -> io::Result<()> {
        require(
            self.stage == Stage::Echo
                && self
                    .offer
                    .as_ref()
                    .is_some_and(|o| o.echo_index.is_some() && !o.acknowledgement_attempted),
            "S2 cursor is not surrendered by its exact echo",
        )?;
        self.check_creator()?;
        let Some(Phase::Planned(plan)) = &mut self.source.phase else {
            unreachable!()
        };
        let source = &mut plan.source;
        let mut lease = source.cursor.local_read(source.source.controls()?)?;
        let observation = owner::observe_created(&mut lease, source.source.intent())?;
        self.observations.push(observation);
        self.check_creator()
    }

    fn make_offer(&mut self) -> io::Result<()> {
        require(self.offer.is_none(), "S2 created offer cannot be replaced")?;
        let mut name = [0u8; 16];
        let raw = unsafe { libc::syscall(libc::SYS_getrandom, name.as_mut_ptr(), name.len(), 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            raw as usize == name.len(),
            "S2 offer randomness returned a short result",
        )?;
        let name = hex(&name);
        let plan = self.plan()?;
        let original = plan.created_pairs.as_ref().unwrap();
        let pairs: serde_json::Value = serde_json::from_slice(original)?;
        require(
            journal::canonical(&pairs)? == *original
                && pairs.as_array().is_some_and(|p| p.len() == 17),
            "S2 retained completed pairs changed",
        )?;
        let transcript_sha256 = hex(&Sha256::digest(original));
        let intent = plan.source.source.intent();
        let bytes = journal::canonical(&json!({"schema":"hermit-grouped-created-transfer-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"offer":name,
            "pairs":pairs,"roles":["CONTROL","PROFILE","EVENTS"],
            "transcript_sha256":transcript_sha256}))?;
        require(
            bytes.len() <= wire::MAX_PACKET,
            "S2 offer exceeds original packet bound",
        )?;
        let mut request = json!({"schema":"hermit-grouped-adopt-created-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"offer":name,
            "transcript_sha256":transcript_sha256});
        let expected_echo = journal::canonical(&request)?;
        request["schema"] = json!("hermit-grouped-adopt-created-ack-v1");
        let acknowledgement = journal::canonical(&request)?;
        self.offer = Some(Offer {
            name,
            transcript_sha256,
            bytes,
            expected_echo,
            acknowledgement,
            send_attempted: false,
            echo_index: None,
            acknowledgement_attempted: false,
        });
        Ok(())
    }

    pub fn progress(&mut self) -> io::Result<bool> {
        let result = self.progress_inner();
        self.remember(result)
    }

    fn progress_inner(&mut self) -> io::Result<bool> {
        self.check()?;
        self.guardian.drain()?;
        match self.stage {
            Stage::Retained => Err(io::Error::other("S2 owner is not initialized")),
            Stage::Creator => {
                let Some(index) = self.channel.receive(512)? else {
                    return Ok(false);
                };
                let plan = self.plan()?;
                let intent = plan.source.source.intent().clone();
                let unit = plan.request.unit.clone();
                self.creator = Some(owner::Creator::retain(
                    &mut self.channel.packets[index],
                    &intent,
                    &unit,
                )?);
                self.store.append(json!({"kind":"successor-created","value":self.creator.as_ref().unwrap().receipt()}))?;
                let creator = self.creator.as_ref().unwrap();
                owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                self.manager = Some(owner::ManagerQuery::retain(unit));
                self.image = Some(owner::EntryQuery::retain(creator.peer.pid));
                self.stage = Stage::Authenticating;
                self.manager.as_mut().unwrap().start()?;
                self.image.as_mut().unwrap().start()?;
                Ok(true)
            }
            Stage::Authenticating => {
                let deadline = self.plan()?.source.source.deadline();
                if self.manager_snapshot.is_none() {
                    self.manager_snapshot = self.manager.as_mut().unwrap().poll(deadline)?;
                }
                if self.image_snapshot.is_none() {
                    self.image_snapshot = self.image.as_mut().unwrap().poll(deadline)?;
                }
                let (Some(manager), Some(image)) = (&self.manager_snapshot, &self.image_snapshot)
                else {
                    return Ok(false);
                };
                self.provider
                    .authenticate(self.creator.as_mut().unwrap(), manager, image)?;
                self.check_creator()?;
                self.store.append(json!({"kind":"successor-authenticated",
                    "creator":self.creator.as_ref().unwrap().evidence()?,
                    "manager":self.manager.as_ref().unwrap().evidence(),
                    "image":self.image.as_ref().unwrap().evidence()}))?;
                self.check_creator()?;
                self.manager
                    .as_mut()
                    .unwrap()
                    .retire_successful_resources(deadline)?;
                self.image
                    .as_mut()
                    .unwrap()
                    .retire_successful_resources(deadline)?;
                self.check_creator()?;
                let packet = format!("EXEC {}\n", self.plan()?.source.source.intent().nonce);
                self.store.append(
                    json!({"kind":"successor-exec-ack-intent","packet":hex(packet.as_bytes())}),
                )?;
                self.stage = Stage::Offer;
                self.check_creator()?;
                self.channel.send_once(packet.as_bytes(), &[])?;
                Ok(true)
            }
            Stage::Offer => {
                self.check_creator()?;
                require(
                    !self.guardian_forward_attempted,
                    "S2 Guardian forwarding cannot repeat",
                )?;
                self.guardian_forward_attempted = true;
                self.check_guardian_endpoint()?;
                let plan = self.plan()?;
                let intent = plan.source.source.intent();
                let forward =
                    journal::canonical(&json!({"schema":"hermit-grouped-guardian-channel-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,
                    "stage_deadline":plan.source.source.native_deadline()}))?;
                self.store.append(
                    json!({"kind":"successor-guardian-forward-intent","packet":hex(&forward)}),
                )?;
                self.check_creator()?;
                self.channel
                    .send_once(&forward, &[self.guardian_endpoint.as_fd()])?;
                self.make_offer()?;
                self.store.append(json!({"kind":"successor-offer-intent",
                    "packet_bytes":self.offer.as_ref().unwrap().bytes.len(),
                    "packet_sha256":hex(&Sha256::digest(&self.offer.as_ref().unwrap().bytes)),
                    "offer":self.offer.as_ref().unwrap().name,
                    "transcript_sha256":self.offer.as_ref().unwrap().transcript_sha256}))?;
                self.check_creator()?;
                self.census()?;
                self.offer.as_mut().unwrap().send_attempted = true;
                self.stage = Stage::Controls;
                let Some(Phase::Planned(plan)) = &self.source.phase else {
                    unreachable!()
                };
                let controls = plan.source.source.controls()?;
                let rights: Vec<_> = controls.fds.iter().map(AsFd::as_fd).collect();
                self.channel
                    .send_once(&self.offer.as_ref().unwrap().bytes, &rights)?;
                self.check_creator()?;
                self.census()?;
                Ok(true)
            }
            Stage::Controls => {
                self.check_creator()?;
                let Some(index) = self.channel.receive(2048)? else {
                    return Ok(false);
                };
                let intent = self.plan()?.source.source.intent().clone();
                self.controls = Some(owner::Controls::retain(
                    &mut self.channel.packets[index],
                    self.creator.as_ref().unwrap(),
                    &intent,
                )?);
                self.roles = Some(owner::RoleQuery::retain());
                self.stage = Stage::Roles;
                self.roles.as_mut().unwrap().start()?;
                Ok(true)
            }
            Stage::Roles => {
                let deadline = self.plan()?.source.source.deadline();
                let Some(snapshot) = self.roles.as_mut().unwrap().poll(deadline)? else {
                    return Ok(false);
                };
                self.role_snapshot = Some(snapshot);
                self.controls
                    .as_mut()
                    .unwrap()
                    .authenticate(self.role_snapshot.as_ref().unwrap())?;
                let original = self.plan()?.source.source.controls()?;
                let received = self.controls.as_ref().unwrap();
                require_same_descriptions(&original.fds, &received.fds)?;
                self.check_creator()?;
                self.store
                    .append(json!({"kind":"successor-original-controls",
                    "controls":self.controls.as_ref().unwrap().evidence()?,
                    "roles":self.roles.as_ref().unwrap().evidence()}))?;
                self.check_creator()?;
                self.roles
                    .as_mut()
                    .unwrap()
                    .retire_successful_resources(deadline)?;
                self.check_creator()?;
                let plan = self.plan()?;
                let intent = plan.source.source.intent();
                let ready = journal::canonical(&json!({"schema":"hermit-grouped-holder-ready-v1",
                    "nonce":intent.nonce,"incarnation":intent.incarnation,"role":"keeper",
                    "stage_deadline":plan.source.source.native_deadline()}))?;
                self.store
                    .append(json!({"kind":"successor-holder-ready-intent","packet":hex(&ready)}))?;
                self.stage = Stage::Echo;
                self.check_creator()?;
                self.channel.send_once(&ready, &[])?;
                Ok(true)
            }
            Stage::Echo => {
                self.check_creator()?;
                let Some(index) = self.channel.receive(2048)? else {
                    return Ok(false);
                };
                let offer = self.offer.as_mut().unwrap();
                require(
                    offer.send_attempted
                        && offer.echo_index.is_none()
                        && !offer.acknowledgement_attempted,
                    "S2 offer has already been consumed",
                )?;
                // Occupied before validating native rights: an invalid or
                // ambiguous reply cannot be replaced by a second S2 request.
                offer.echo_index = Some(index);
                let packet = &self.channel.packets[index];
                packet.exact(3, self.creator.as_ref().unwrap().peer)?;
                require(
                    packet.bytes == offer.expected_echo,
                    "S2 exact offer echo differs",
                )?;
                let original = self.plan()?.source.source.controls()?;
                require_same_descriptions(&original.fds, &packet.rights)?;
                self.census()?;
                self.observe_during_echo()?;
                let offer = self.offer.as_ref().unwrap();
                self.store.append(json!({"kind":"successor-adoption-ack-intent",
                    "offer":offer.name,"transcript_sha256":offer.transcript_sha256,
                    "packet":hex(&offer.acknowledgement),"echo_packet":hex(&self.channel.packets[index].bytes)}))?;
                self.check_creator()?;
                self.census()?;
                let offer = self.offer.as_mut().unwrap();
                offer.acknowledgement_attempted = true;
                self.channel.send_once(&offer.acknowledgement, &[])?;
                self.check_creator()?;
                self.census()?;
                self.stage = Stage::Acknowledged;
                Ok(true)
            }
            Stage::Acknowledged => Ok(false),
        }
    }

    /// Descriptive progress only. In particular this does not return a native
    /// io/owner pointer, detach either original history, or authorize open.
    pub fn acknowledgement_sent(&self) -> bool {
        self.stage == Stage::Acknowledged && self.refusal.is_none()
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        json!({"stage":format!("{:?}",self.stage),
            "failure":self.refusal.as_ref().map(|f| &f.message),
            "failure_origin":self.failure_origin,
            "acknowledgement_sent":self.acknowledgement_sent(),
            "provider_authority":false,
            "echo_rights":self.offer.as_ref().and_then(|o|o.echo_index)
                .map(|index|self.channel.packets[index].rights.len()),
            "fresh_observations":self.observations.len()})
    }
}

fn require_same_descriptions(original: &[OwnedFd], received: &[OwnedFd]) -> io::Result<()> {
    require(
        original.len() == 3 && received.len() == 3,
        "S2 original control population differs",
    )?;
    for (original, received) in original.iter().zip(received) {
        let raw = unsafe {
            libc::syscall(
                libc::SYS_kcmp,
                libc::getpid(),
                libc::getpid(),
                0,
                original.as_raw_fd(),
                received.as_raw_fd(),
            )
        };
        require(
            raw == 0,
            "S2 echo is not the original open-file description",
        )?;
    }
    Ok(())
}

fn check_launcher(launcher: &owner::Launcher) -> io::Result<()> {
    require(
        launcher.refused.is_none() && launcher.reaped.is_none(),
        "S2 retained launcher is refused or already reaped",
    )?;
    let pidfd = launcher
        .pidfd
        .as_ref()
        .ok_or_else(|| io::Error::other("S2 retained launcher lacks original pidfd"))?;
    owner::pidfd_matches(pidfd.as_raw_fd(), launcher.child.id() as i32)?;
    require(
        !owner::terminal(pidfd.as_raw_fd())?
            && unsafe { libc::getpgid(launcher.child.id() as i32) } == launcher.child.id() as i32,
        "S2 launcher is terminal or left its original process group",
    )
}
