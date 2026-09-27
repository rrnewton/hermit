//! Successful continuation of the original prepared creation owner. The
//! source's actual callback/dual-journal bodies and failed-prefix path remain
//! unchanged. The outside runtime Keeper is installed before the first ACK.
use super::super::entry::runtime_creation::RuntimeCreationCustody;
use super::super::serial;
use super::*;

#[derive(Debug)]
struct ReleasedPreparation {
    // Kept after actual explicit C alias release. These originals are never
    // converted into source authority or destructively replaced on an error.
    intent: Intent,
    peer: Option<wire::Credentials>,
    ready: Option<Readiness>,
    peer_ledger: Option<journal::SourceLedgerReader>,
    ledger: journal::RemovalJournal,
    cursor: Cursor,
    agreement: Option<AsymmetricAgreement>,
    stage: Instant,
    native_stage: u64,
    deadline: Option<Instant>,
    native_cutoff: Option<u64>,
    release: Option<ffi::Release>,
    primary: Option<Failure>,
    callback_failure: Option<Failure>,
    closed: bool,
    removal_sequence: u64,
    bridge: ffi::CleanupBridge,
    inventory: owner::CensusInventory,
    no_provider: NoProviderCreated,
}

#[derive(Debug)]
pub(in super::super) struct PreparedSource {
    original: Option<CleanupEnvelope>,
    runtime: Option<RuntimeCreationCustody>,
    runtime_sent: bool,
    runtime_acknowledged: bool,
    success_observed: bool,
    released: Option<ReleasedPreparation>,
    serial: Option<serial::SerialOwner>,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
    keeper_prefix: Option<Value>,
    keeper_initial: Option<Value>,
    namespace: Option<owner::NamespaceCustody>,
    custody_writer: guardian::CustodyShutdown,
    custody_failure: Option<Failure>,
}
impl PreparedSource {
    /// The closed startup controller has no Provider path. It retains all
    /// original resources here before loading, launching or acknowledging.
    pub fn retain(
        holder: guardian::Holder,
        bridge_file: OwnedFd,
        bridge_digest: [u8; 32],
        keeper_channel: OwnedFd,
        cleanup_directory: OwnedFd,
        source_logs: OwnedFd,
        keeper_logs: OwnedFd,
        runtime: RuntimeCreationCustody,
    ) -> Self {
        let (intent, _, stage, native_stage, _) = holder.original_context();
        let intent = intent.clone();
        let callbacks = Box::new(CallbackOwner {
            intent: intent.clone(),
            parent: wire::Channel::retain(keeper_channel),
            peer: None,
            ready: None,
            peer_ledger: None,
            ledger: journal::RemovalJournal::retain(cleanup_directory, intent),
            cursor: Cursor::retained(),
            agreement: None,
            stage,
            native_stage,
            deadline: None,
            native_cutoff: None,
            release: None,
            primary: None,
            callback_failure: None,
            closed: false,
            removal_sequence: 0,
        });
        Self {
            original: Some(CleanupEnvelope {
                no_provider: NoProviderCreated { _closed_entry: () },
                holder,
                bridge: ffi::CleanupBridge::retain(bridge_file, bridge_digest),
                callbacks,
                source: None,
                keeper: None,
                source_logs,
                keeper_logs,
                registration_attempted: false,
                prepared: false,
                inventory: owner::CensusInventory::retain(),
                recovery_steps: [ffi::RecoveryStep::default(); 17],
            }),
            runtime: Some(runtime),
            runtime_sent: false,
            runtime_acknowledged: false,
            success_observed: false,
            released: None,
            serial: None,
            refusal: None,
            failure_origin: None,
            keeper_prefix: None,
            keeper_initial: None,
            namespace: None,
            custody_writer: guardian::CustodyShutdown::default(),
            custody_failure: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if self.refusal.is_none() {
                self.refusal = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        }
        result
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        Ok(())
    }
    pub fn prepare(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            self.original
                .as_mut()
                .ok_or_else(|| io::Error::other("source preparation already moved"))?
                .prepare_before_source()
        })();
        self.remember(result)
    }
    /// Install a real Child before checking it. Caller retains this owner on
    /// every error; no fresh numeric process identifier can replace the child.
    pub fn install_keeper(&mut self, keeper: &mut Option<owner::Launcher>) -> io::Result<()> {
        self.check()?;
        let original = self
            .original
            .as_mut()
            .ok_or_else(|| io::Error::other("keeper installation after source transition"))?;
        require(
            original.keeper.is_none() && keeper.is_some(),
            "keeper ownership installation repeated or absent",
        )?;
        original.keeper = keeper.take();
        let result = (|| {
            original
                .keeper
                .as_mut()
                .unwrap()
                .initialize(original.keeper_logs.as_raw_fd())?;
            original.callbacks.peer = Some(wire::Credentials {
                pid: original.keeper.as_ref().unwrap().child.id() as i32,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            });
            Ok(())
        })();
        self.remember(result)
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
            self.check()?;
            self.runtime
                .as_mut()
                .ok_or_else(|| io::Error::other("outside source owner moved"))?
                .launch_source(unit, executable, arguments, input, image)
        })();
        self.remember(result)
    }
    pub fn receive_source_launch(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            let runtime = self
                .runtime
                .as_mut()
                .ok_or_else(|| io::Error::other("outside source owner moved"))?;
            if !runtime.receive_source_launch()? {
                return Ok(false);
            }
            self.original
                .as_mut()
                .ok_or_else(|| io::Error::other("original Guardian moved"))?
                .holder
                .bind_remote_source_launcher(runtime.source_lease()?)?;
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn parent(&mut self) -> &mut wire::Channel {
        &mut self.original.as_mut().unwrap().callbacks.parent
    }
    pub fn source_pidfd(&self) -> BorrowedFd<'_> {
        self.runtime.as_ref().unwrap().source_pidfd()
    }
    pub fn source_pid(&self) -> u32 {
        self.runtime.as_ref().unwrap().source_pid() as u32
    }
    pub fn keeper_peer(&self) -> wire::Credentials {
        self.original.as_ref().unwrap().callbacks.peer.unwrap()
    }
    pub fn drain(&mut self) -> io::Result<()> {
        let original = self
            .original
            .as_mut()
            .ok_or_else(|| io::Error::other("original launchers moved"))?;
        if let Some(keeper) = &mut original.keeper {
            keeper.drain()?;
        }
        if let Some(source) = &mut original.source {
            source.drain()?;
        }
        Ok(())
    }
    pub fn progress_creation(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            self.drain()?;
            let original = self
                .original
                .as_mut()
                .ok_or_else(|| io::Error::other("creation owner moved"))?;
            original.holder.progress()?;
            if original.holder.awaiting_creation_preparation() {
                original.prepare_controls()?;
            }
            if original.prepared && original.callbacks.ready.is_none() {
                original.receive_readiness()?;
            }
            if original.callbacks.ready.is_some() && !self.runtime_sent {
                if self.keeper_initial.is_none() {
                    if let Some(index) = original.callbacks.parent.receive(4096)? {
                        let packet = &mut original.callbacks.parent.packets[index];
                        packet.exact(1, original.callbacks.peer.unwrap())?;
                        let value: Value = serde_json::from_slice(&packet.bytes)?;
                        require(
                            packet.bytes
                                == journal::canonical(&json!({
                            "schema":"hermit-grouped-creation-keeper-initial-v1","nonce":original.callbacks.intent.nonce,
                            "incarnation":original.callbacks.intent.incarnation,"stage_deadline":original.callbacks.native_stage,
                            "keeper_store":value["keeper_store"]}))?,
                            "initial Keeper healthy Store commitment differs",
                        )?;
                        let creator = original.holder.creation_creator()?;
                        self.namespace = Some(owner::NamespaceCustody::retain(
                            packet.rights.remove(0),
                            creator.peer.pid,
                        ));
                        self.keeper_initial = Some(value);
                    }
                }
                if let Some(initial) = &self.keeper_initial {
                    let namespace = self
                        .namespace
                        .as_mut()
                        .ok_or_else(|| io::Error::other("original namespace custody absent"))?;
                    let namespace_ready = namespace.progress(
                        original.holder.creation_creator()?,
                        original.callbacks.stage,
                    )?;
                    if namespace_ready && original.holder.held_creation_sequence() == Some(1) {
                        let guardian = original.holder.creation_source_history()?;
                        let keeper = original
                            .callbacks
                            .peer_ledger
                            .as_mut()
                            .unwrap()
                            .read(&initial["keeper_store"])?;
                        // Both real Store fsyncs completed. Source is still
                        // waiting on its first Guardian ACK; no write can run.
                        original
                            .callbacks
                            .ledger
                            .store
                            .append(json!({"kind":"actual-source-namespace-query",
                            "query":namespace.completed_record(original.callbacks.stage)?}))?;
                        self.runtime_sent = true;
                        self.runtime.as_mut().unwrap().send(
                            original.holder.creation_creator()?,
                            original.holder.creation_controls()?,
                            original
                                .keeper
                                .as_ref()
                                .unwrap()
                                .pidfd
                                .as_ref()
                                .unwrap()
                                .as_fd(),
                            original.keeper.as_ref().unwrap().child.id() as i32,
                            original.holder.creation_source_rights()?,
                            original
                                .callbacks
                                .peer_ledger
                                .as_ref()
                                .unwrap()
                                .retained_rights()?,
                            &guardian,
                            &keeper,
                        )?;
                    }
                }
            }
            if self.runtime_sent && !self.runtime_acknowledged {
                self.runtime_acknowledged = self
                    .runtime
                    .as_mut()
                    .unwrap()
                    .receive_ack(original.holder.creation_controls()?)?;
            }
            if original.holder.held_creation_sequence().is_some() && self.runtime_acknowledged {
                self.runtime.as_ref().unwrap().before_source_ack(
                    original.holder.creation_creator()?,
                    original.holder.creation_controls()?,
                )?;
                original
                    .holder
                    .acknowledge_creation_when_ready(original.callbacks.ready.as_ref())?;
            }
            if self.runtime_acknowledged && self.runtime.as_ref().unwrap().mirrored_sequence() < 34
            {
                if self.keeper_prefix.is_none() {
                    if let Some(index) = original.callbacks.parent.receive(4096)? {
                        let packet = &original.callbacks.parent.packets[index];
                        packet.exact(0, original.callbacks.peer.unwrap())?;
                        let value: Value = serde_json::from_slice(&packet.bytes)?;
                        let sequence = self.runtime.as_ref().unwrap().mirrored_sequence() + 1;
                        require(
                            packet.bytes
                                == journal::canonical(&json!({
                            "schema":"hermit-grouped-creation-keeper-prefix-v1",
                            "nonce":original.callbacks.intent.nonce,"incarnation":original.callbacks.intent.incarnation,
                            "stage_deadline":original.callbacks.native_stage,"sequence":sequence,
                            "keeper_store":value["keeper_store"]}))?,
                            "source Keeper durable prefix changed original sequence or fields",
                        )?;
                        let guardian = original.holder.creation_source_history()?;
                        let keeper = original
                            .callbacks
                            .peer_ledger
                            .as_mut()
                            .unwrap()
                            .read(&value["keeper_store"])?;
                        require(
                            guardian.frames.len() as u64 == sequence
                                && guardian.frames == keeper.frames,
                            "source Keeper mirror lacks exact current dual callback histories",
                        )?;
                        self.keeper_prefix = Some(value);
                        self.runtime
                            .as_mut()
                            .unwrap()
                            .begin_prefix(sequence, &guardian, &keeper)?;
                    }
                }
                if self.keeper_prefix.is_some()
                    && self.runtime.as_mut().unwrap().receive_prefix_ack()?
                {
                    self.runtime.as_ref().unwrap().before_source_ack(
                        original.holder.creation_creator()?,
                        original.holder.creation_controls()?,
                    )?;
                    original.callbacks.ready.as_ref().unwrap().check(
                        original.holder.creation_controls()?,
                        &original.callbacks.intent,
                        original.callbacks.native_stage,
                    )?;
                    let mut value = self.keeper_prefix.as_ref().unwrap().clone();
                    value["schema"] = json!("hermit-grouped-creation-keeper-prefix-ack-v1");
                    // No retry or clearing on an ambiguous send. The source C
                    // cannot proceed past its actual second ACK beforehand.
                    original
                        .callbacks
                        .parent
                        .send_once(&journal::canonical(&value)?, &[])?;
                    self.keeper_prefix = None;
                }
            }
            if original.holder.source_exit_observed() {
                require(
                    self.runtime_acknowledged
                        && self.runtime.as_ref().unwrap().mirrored_sequence() == 34
                        && self.keeper_prefix.is_none(),
                    "source exited before outside durable custody of all original34 callbacks",
                )?;
                // This actual record enforces complete original17 pairs and
                // natural source exit. It is not the later serial join proof.
                original.holder.successful_exit_record()?;
                self.runtime
                    .as_ref()
                    .unwrap()
                    .check_controls(original.holder.creation_controls()?)?;
                self.success_observed = true;
            }
            Ok(self.success_observed)
        })();
        self.remember(result)
    }
    pub fn begin_serial_join(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.success_observed
                    && self.runtime_acknowledged
                    && self.serial.is_none()
                    && self.released.is_none(),
                "successful serial transition is early or repeated",
            )?;
            let original = self
                .original
                .as_mut()
                .ok_or_else(|| io::Error::other("source preparation absent"))?;
            original.holder.successful_exit_record()?;
            self.runtime
                .as_ref()
                .unwrap()
                .check_controls(original.holder.creation_controls()?)?;
            require(
                original.callbacks.primary.is_none()
                    && original.callbacks.callback_failure.is_none()
                    && original.callbacks.agreement.is_none()
                    && original.callbacks.removal_sequence == 0,
                "failed cleanup cannot become successful S1",
            )?;
            self.namespace
                .as_ref()
                .ok_or_else(|| io::Error::other("serial source lacks authenticated namespace"))?
                .completed(original.callbacks.stage)?;
            unsafe {
                original.bridge.release_aliases()?;
                original.bridge.free()?;
            }
            // Every fallible/native check is above. Preserve both halves before
            // returning to any caller; no original history or failed state is reset.
            let original = self.original.take().unwrap();
            let CallbackOwner {
                parent,
                intent,
                peer,
                ready,
                peer_ledger,
                ledger,
                cursor,
                agreement,
                stage,
                native_stage,
                deadline,
                native_cutoff,
                release,
                primary,
                callback_failure,
                closed,
                removal_sequence,
            } = *original.callbacks;
            self.serial = Some(serial::SerialOwner::retain(
                serial::InFlightSource::retain_remote(
                    original.holder,
                    self.runtime.take().unwrap(),
                    original.keeper.unwrap(),
                    parent,
                    serial::SourceLogs::retain(original.source_logs, original.keeper_logs),
                    self.namespace.take().unwrap(),
                ),
            ));
            self.released = Some(ReleasedPreparation {
                intent,
                peer,
                ready,
                peer_ledger,
                ledger,
                cursor,
                agreement,
                stage,
                native_stage,
                deadline,
                native_cutoff,
                release,
                primary,
                callback_failure,
                closed,
                removal_sequence,
                bridge: original.bridge,
                inventory: original.inventory,
                no_provider: original.no_provider,
            });
            Ok(())
        })();
        self.remember(result)
    }
    pub fn progress_serial(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            self.serial
                .as_mut()
                .ok_or_else(|| io::Error::other("serial join not installed"))?
                .progress_source()
        })();
        self.remember(result)
    }
    pub fn take_leaf_namespace(&mut self) -> io::Result<owner::PreparedLeafNamespace> {
        self.check()?;
        self.serial
            .as_mut()
            .ok_or_else(|| io::Error::other("prepared namespace lacks original serial owner"))?
            .take_leaf_namespace()
    }
    pub fn prepare_leaf_plan(&mut self, request: serial::SuccessorRequest) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            self.serial
                .as_mut()
                .ok_or_else(|| io::Error::other("serial owner absent"))?
                .into_leaf_plan(request)
        })();
        self.remember(result)
    }
    /// Destination first prechecks its empty slot. This moves the same private
    /// prepared source once; failure/remainder/runtime owners stay installed.
    pub fn take_serial(&mut self) -> io::Result<serial::SerialOwner> {
        self.check()?;
        self.serial
            .take()
            .ok_or_else(|| io::Error::other("serial source already transferred"))
    }
    pub fn local_custody_records(&self) -> io::Result<Vec<Value>> {
        if let Some(serial) = &self.serial {
            return serial.local_custody_records();
        }
        let original = self
            .original
            .as_ref()
            .ok_or_else(|| io::Error::other("source records moved to successor"))?;
        let mut records = original.holder.local_custody_records("source-guardian");
        if let Some(namespace) = &self.namespace {
            records.push(
                json!({"kind":"query","role":"source-namespace","record":namespace.evidence()}),
            );
        }
        if let Some(keeper) = &original.keeper {
            records.push(json!({"kind":"launcher","role":"source-keeper","record":keeper.custody_evidence()}));
        }
        Ok(records)
    }
    pub fn begin_s2_guardian(&mut self, guardian: &owner::Launcher) -> io::Result<()> {
        self.serial
            .as_mut()
            .ok_or_else(|| io::Error::other("S2 Guardian registration before serial join"))?
            .begin_s2_guardian(guardian)
    }
    pub fn receive_s2_guardian_ack(&mut self) -> io::Result<bool> {
        self.serial
            .as_mut()
            .ok_or_else(|| io::Error::other("S2 Guardian ACK before serial join"))?
            .receive_s2_guardian_ack()
    }
    pub fn failure_notice_origin(&self) -> io::Result<u64> {
        if let Some(runtime) = &self.runtime {
            return runtime.failure_notice_origin();
        }
        if let Some(serial) = &self.serial {
            return serial.failure_notice_origin();
        }
        Err(io::Error::other(
            "original failure notice moved to successor",
        ))
    }
    pub fn send_local_custody(&mut self, report: &Value, file: BorrowedFd<'_>) -> io::Result<()> {
        if let Some(runtime) = &mut self.runtime {
            return runtime.send_local_custody(report, file);
        }
        if let Some(serial) = &mut self.serial {
            return serial.send_local_custody(report, file);
        }
        Err(io::Error::other(
            "source custody channel moved to successor",
        ))
    }
    pub fn retire_local_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<bool> {
        let deadline = if self.refusal.is_some() {
            guardian::clip_custody_origin(deadline, self.failure_origin)?
        } else {
            deadline
        };
        if let Some(serial) = &mut self.serial {
            return serial.retire_local_custody(deadline, cause);
        }
        let original = self
            .original
            .as_mut()
            .ok_or_else(|| io::Error::other("source local custody moved"))?;
        if let Err(error) = self.custody_writer.progress(&original.callbacks.parent) {
            self.custody_failure
                .get_or_insert_with(|| Failure::capture(&error));
        }
        let queries = original.holder.retire_local_custody(deadline, cause);
        let namespace = self
            .namespace
            .as_mut()
            .map_or(Ok(owner::QueryRetirement::NoChild), |owner| {
                owner.retire_custody(deadline, cause)
            });
        let namespace_done = match namespace {
            Ok(owner::QueryRetirement::Pending) => false,
            Ok(owner::QueryRetirement::Retired | owner::QueryRetirement::NoChild) => true,
            Err(error) => {
                self.custody_failure
                    .get_or_insert_with(|| Failure::capture(&error));
                false
            }
        };
        let mut complete = false;
        match queries {
            Ok(false) => {}
            Ok(true) => {
                require(
                    original.source.is_none(),
                    "outside source cleanup found an unexpected local source Child",
                )?;
                complete = match &mut original.keeper {
                    Some(keeper) => matches!(
                        keeper.retire_custody(original.keeper_logs.as_raw_fd(), deadline, cause)?,
                        owner::QueryRetirement::Retired | owner::QueryRetirement::NoChild
                    ),
                    None => {
                        owner::check_no_children()?;
                        true
                    }
                };
            }
            Err(error) => {
                self.custody_failure
                    .get_or_insert_with(|| Failure::capture(&error));
            }
        }
        if let Some(error) = &self.custody_failure {
            return Err(error.error());
        }
        Ok(complete && namespace_done)
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
        if let Some(runtime) = &mut self.runtime {
            runtime.notify_failure(origin, cause)
        } else if let Some(serial) = &mut self.serial {
            serial.notify_runtime_failure(origin, cause)
        } else {
            Err(io::Error::other(
                "outside source custody already moved to successor",
            ))
        }
    }
}
