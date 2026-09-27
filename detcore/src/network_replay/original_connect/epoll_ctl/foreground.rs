//! Positive, deliberately narrow authority retained by the existing Call.
//! No production gate routes here until actual runtime qualification succeeds.
use super::*;
use crate::memory::MemoryMetadata;
use crate::memory::OriginalArena;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::JoinedNativePrefix;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Admitted,
    Prepared,
    Returned,
}
#[derive(Debug, Clone)]
pub(super) struct ForegroundAdmission {
    root: Arc<ForegroundRoot>,
    // Retains the actual joined-prefix issuer, not a caller-supplied boolean.
    _joined: JoinedNativePrefix,
    arena: Option<OriginalArena>,
    pub(super) bindings: [FdSlotBinding; 2],
    identities: [FileIdentity; 2],
    epoch: u64,
    phase: Phase,
}
impl NetworkReplayEngine {
    /// This check runs before any await of prior execution tails, then again
    /// under scheduler -> metadata -> engine immediately before new admission.
    pub(crate) fn foreground_epoll_preflight(
        &self,
        owner: NetworkStreamOwner,
        arguments: &Arguments,
        root: &ForegroundRoot,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
    ) -> Result<[FdSlotBinding; 2], NetworkReplayError> {
        if !root.is_current(owner)
            || !root.matches_metadata(actual)
            || arguments.files != root.files()
            || arguments.kind != Kind::EpollCtl
            || arguments.binding.is_some()
            || !matches!(
                arguments.length,
                libc::EPOLL_CTL_ADD | libc::EPOLL_CTL_MOD | libc::EPOLL_CTL_DEL
            )
            || !self.fd_table_capability()
            || !self.foreground_fd_mutations_settled()
            || !self.stream_calls.is_empty()
            || !self.stream_operations.is_empty()
            || !self.socket_controls.is_empty()
            || !self.shadow_probes.is_empty()
            || !self.shadow_deliveries.is_empty()
            || !self.fd_installations.is_empty()
            || self.fd_publications.values().any(|state| {
                state.active.is_some()
                    || state.pending.is_some()
                    || state.enrollment.is_some()
                    || state.reader.is_some()
            })
        {
            return Err(protocol(
                "foreground ctl has unresolved semantic native custody",
            ));
        }
        self.validate_fd_metadata(owner, arguments.files, actual, local)?;
        self.lifetime
            .validate_foreground_epoll_root(
                lifetime::TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                arguments.files,
            )
            .map_err(|error| protocol(&error.to_string()))?;
        let pair = [
            local
                .descriptor_binding(arguments.fd)
                .map_err(|_| protocol("foreground ctl epoll absent"))?,
            local
                .descriptor_binding(arguments.original_count as u32 as i32)
                .map_err(|_| protocol("foreground ctl target absent"))?,
        ];
        let fresh = |binding: FdSlotBinding, kind: NetworkFdInstallKind| {
            let kind = match kind {
                NetworkFdInstallKind::Socket => lifetime::OriginalCreationKind::Socket,
                NetworkFdInstallKind::EpollCreate => lifetime::OriginalCreationKind::Epoll,
                _ => return false,
            };
            self.lifetime.has_original_creation(
                lifetime::TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                binding,
                kind,
            )
        };
        // Inherited anonymous inodes cannot certify an epoll/UFFD subtype;
        // all live socket/epoll slots must have original local creation proof.
        for (&fd, descriptor) in &local.file_handles {
            let binding = local
                .descriptor_binding(fd)
                .map_err(|_| protocol("foreground descriptor changed"))?;
            let okay = match descriptor.ty() {
                crate::fd::FdType::Inherited { .. } | crate::fd::FdType::Userfaultfd => false,
                crate::fd::FdType::Epoll => fresh(binding, NetworkFdInstallKind::EpollCreate),
                crate::fd::FdType::Socket => fresh(binding, NetworkFdInstallKind::Socket),
                _ => true,
            };
            if !okay {
                return Err(protocol(
                    "foreground ctl has inherited or aliased special-file authority",
                ));
            }
        }
        if local.file_handles[&arguments.fd].ty() != crate::fd::FdType::Epoll
            || local.file_handles[&(arguments.original_count as u32 as i32)].ty()
                != crate::fd::FdType::Socket
            || pair.iter().any(|binding| {
                local
                    .file_handles
                    .values()
                    .filter(|descriptor| descriptor.open_file_id() == binding.open_file)
                    .count()
                    != 1
            })
            || self
                .bindings
                .get(&pair[1].open_file)
                .and_then(|channel| self.channels.get(channel))
                .is_none_or(|channel| channel.transport != NetworkTransportV2::Tcp)
        {
            return Err(protocol(
                "foreground ctl requires fresh unaliased epoll and enrolled TCP target",
            ));
        }
        Ok(pair)
    }
    pub(crate) fn begin_foreground_epoll_ctl(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
        memory: &MemoryMetadata,
        joined: JoinedNativePrefix,
        epoch: u64,
    ) -> Result<Admission, NetworkReplayError> {
        let root = joined.root().clone();
        let bindings = self.foreground_epoll_preflight(owner, &arguments, &root, actual, local)?;
        let mut identities = Vec::new();
        for binding in bindings {
            identities.push(
                local
                    .native_binding_identity(binding)
                    .ok_or_else(|| protocol("foreground ctl lacks held native file identity"))?,
            );
        }
        let arena = if arguments.length == libc::EPOLL_CTL_DEL {
            None
        } else {
            Some(
                memory
                    .original_event_arena(owner, arguments.address)
                    .map_err(protocol)?,
            )
        };
        if arena
            .as_ref()
            .is_some_and(|arena| !arena.matches_root(&root))
        {
            return Err(protocol(
                "foreground ctl arena belongs to a different original root",
            ));
        }
        let foreground = ForegroundAdmission {
            root,
            _joined: joined,
            arena,
            bindings,
            identities: [identities[0], identities[1]],
            epoch,
            phase: Phase::Admitted,
        };
        let admission = self.begin_original_epoll_ctl(owner, arguments)?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .epoll_control
            .as_mut()
            .unwrap()
            .foreground = Some(foreground);
        Ok(admission)
    }
    /// Called only by the actual synchronous Prepared observer. Generic ctl
    /// keeps its existing path; a serialized Admission cannot create this cell.
    pub(crate) fn prepare_foreground_epoll_ctl(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        local: &FileMetadata,
        memory: &MemoryMetadata,
        epoch: u64,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        let Some(control) = &original.epoll_control else {
            return Ok(());
        };
        let Some(foreground) = &control.foreground else {
            return Ok(());
        };
        if original.arguments != admission.arguments
            || original.uninvoked
            || original.final_wait
            || original.command.is_none()
            || original.backend_result.is_some()
            || foreground.phase != Phase::Admitted
            || foreground.epoch != epoch
            || !foreground.root.is_current(owner)
        {
            return Err(protocol(
                "foreground ctl changed its admitted native preparation",
            ));
        }
        for (binding, identity) in foreground.bindings.iter().zip(&foreground.identities) {
            if local.descriptor_binding(binding.slot.fd).ok() != Some(*binding)
                || local.native_binding_identity(*binding) != Some(*identity)
            {
                return Err(protocol(
                    "foreground ctl changed its admitted native descriptor",
                ));
            }
        }
        if let Some(arena) = &foreground.arena {
            memory
                .validate_original_event_arena(owner, admission.arguments.address, arena)
                .map_err(protocol)?;
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .epoll_control
            .as_mut()
            .unwrap()
            .foreground
            .as_mut()
            .unwrap()
            .phase = Phase::Prepared;
        Ok(())
    }
    pub(crate) fn foreground_epoll_root(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<Option<Arc<ForegroundRoot>>, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("foreground ctl changed arguments"));
        }
        Ok(original
            .epoll_control
            .as_ref()
            .and_then(|control| control.foreground.as_ref())
            .map(|f| f.root.clone()))
    }
    // Used inside the existing backend raw-result issuer, never a callback RPC.
    pub(in crate::network_replay::original_connect) fn observe_foreground_epoll_return(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        let Some(foreground) = original
            .epoll_control
            .as_ref()
            .and_then(|c| c.foreground.as_ref())
        else {
            return Ok(());
        };
        if foreground.phase != Phase::Prepared || !foreground.root.is_current(owner) {
            return Err(protocol(
                "foreground ctl native return lacks exact preparation",
            ));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .epoll_control
            .as_mut()
            .unwrap()
            .foreground
            .as_mut()
            .unwrap()
            .phase = Phase::Returned;
        Ok(())
    }
    pub(crate) fn foreground_epoll_returned(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        epoch: u64,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.backend_result.is_none()
            || original
                .epoll_control
                .as_ref()
                .and_then(|c| c.foreground.as_ref())
                .is_none_or(|f| {
                    f.phase != Phase::Returned || f.epoch != epoch || !f.root.is_current(owner)
                })
        {
            return Err(protocol(
                "foreground ctl has no actual retained native return",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reverie::InjectedSyscallEvent as Event;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;
    async fn fixture(
        operation: i32,
    ) -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Arguments,
        Arc<Mutex<FileMetadata>>,
        Arc<Mutex<MemoryMetadata>>,
        crate::network_runtime::NetworkRuntimeResources,
        JoinedNativePrefix,
    ) {
        let (root, actual, memory, claim) = crate::network_runtime::controlled_foreground_root(61);
        let owner = root.owner();
        let mut engine = NetworkReplayEngine::record(
            chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
        );
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), &claim, owner.thread)
            .unwrap();
        // Explicit accepted native publication premises using the maintained
        // installation transaction fixture, never a live provider assertion.
        for (fd, ty, kind, seq) in [
            (
                7,
                crate::fd::FdType::Epoll,
                NetworkFdInstallKind::EpollCreate,
                1,
            ),
            (
                8,
                crate::fd::FdType::Socket,
                NetworkFdInstallKind::Socket,
                2,
            ),
        ] {
            let (mut next, change) = actual
                .lock()
                .unwrap()
                .prepare_original_installation_typed(
                    owner.thread,
                    fd,
                    nix::fcntl::OFlag::empty(),
                    ty,
                    None,
                )
                .unwrap();
            let binding = change.after.unwrap().binding;
            next.bind_native_installation(
                binding,
                FileIdentity::controlled_fixture(root.native_identity().0, fd as u64 + 30),
            )
            .unwrap();
            assert!(next.acknowledge_network_installations(&[change]));
            *actual.lock().unwrap() = next;
            let mut effect = engine.fd_publication_fixture_effect(owner, change);
            effect.kind = kind;
            engine.fd_installations.get_mut(&effect.lease).unwrap().kind = kind;
            // This fixture already supplies an original native creation premise;
            // generic numeric-result fixtures intentionally retain no such proof.
            engine.fd_installations.get_mut(&effect.lease).unwrap().original_creation =
                Some(crate::network_replay::original_installation::OriginalCreationAuthority::controlled_fixture(owner));
            let permit = engine
                .acquire_fd_publication(owner, root.files())
                .unwrap()
                .permit;
            let batch = NetworkFdPublicationBatch {
                files: root.files(),
                sequence: seq,
                previous_generation: seq - 1,
                through_generation: seq,
                entries: vec![NetworkFdPublicationEntry {
                    replacement: change,
                    effect,
                }],
            };
            engine
                .publish_fd_publication(owner, permit, &batch)
                .unwrap();
            engine
                .acknowledge_fd_publication(owner, permit, &batch)
                .unwrap();
        }
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        let channel = NetworkChannelId(1);
        engine
            .record_channel(NetworkChannelV2 {
                id: channel,
                transport: NetworkTransportV2::Tcp,
                role: detcore_model::network_trace::NetworkEndpointRoleV2::OutboundClient,
                local_address: None,
                peer_address: None,
                accepted_from: None,
            })
            .unwrap();
        let binding = actual.lock().unwrap().descriptor_binding(8).unwrap();
        engine.bindings.insert(binding.open_file, channel);
        engine.reverse_bindings.insert(channel, binding.open_file);
        let args = SyscallArgs {
            arg0: 0,
            arg1: 4096,
            arg2: (libc::PROT_READ | libc::PROT_WRITE) as usize,
            arg3: (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            arg4: -1isize as usize,
            arg5: 0,
        };
        memory
            .lock()
            .unwrap()
            .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
            .unwrap();
        memory
            .lock()
            .unwrap()
            .observe_original_arena(&root, Sysno::mmap, args, Event::Returned(0x8000))
            .unwrap();
        let (runtime, joined) =
            crate::network_runtime::controlled_joined_prefix(root.clone()).await;
        let arguments = Arguments {
            kind: Kind::EpollCtl,
            operation: ExternalOpId::new(owner.thread, 19),
            files: root.files(),
            binding: None,
            fd: 7,
            address: 0x8000,
            length: operation,
            original_count: 8,
        };
        (engine, owner, arguments, actual, memory, runtime, joined)
    }
    #[tokio::test]
    async fn foreground_ctl_uses_exact_prepared_phase_epoch_and_actual_return_on_same_call() {
        for operation in [
            libc::EPOLL_CTL_ADD,
            libc::EPOLL_CTL_MOD,
            libc::EPOLL_CTL_DEL,
        ] {
            let (mut engine, owner, args, actual, memory, _runtime, joined) =
                fixture(operation).await;
            assert!(
                engine.fd_publication_history.is_empty(),
                "full receipts must remain pruned after ACK"
            );
            assert_eq!(engine.lifetime.pending_publication_payloads_for_test(), 0);
            for (fd, kind) in [
                (7, lifetime::OriginalCreationKind::Epoll),
                (8, lifetime::OriginalCreationKind::Socket),
            ] {
                let binding = actual.lock().unwrap().descriptor_binding(fd).unwrap();
                assert!(engine.lifetime.has_original_creation(
                    lifetime::TaskOwner {
                        tid: owner.thread,
                        mm: owner.mm
                    },
                    binding,
                    kind
                ));
            }
            let admission = engine
                .begin_foreground_epoll_ctl(
                    owner,
                    args,
                    &actual,
                    &actual.lock().unwrap(),
                    &memory.lock().unwrap(),
                    joined,
                    9,
                )
                .unwrap();
            assert!(
                engine
                    .foreground_epoll_returned(owner, &admission, 9)
                    .is_err()
            );
            engine
                .original_connect_provider_submitted(owner, &admission)
                .unwrap();
            engine
                .original_call_prepared(owner, &admission, None, 17)
                .unwrap();
            engine.original_connect_invoked(owner, &admission).unwrap();
            assert!(
                engine
                    .original_connect_returned(owner, &admission, 0)
                    .is_err()
            );
            assert!(
                engine
                    .prepare_foreground_epoll_ctl(
                        owner,
                        &admission,
                        &actual.lock().unwrap(),
                        &memory.lock().unwrap(),
                        10
                    )
                    .is_err()
            );
            engine
                .prepare_foreground_epoll_ctl(
                    owner,
                    &admission,
                    &actual.lock().unwrap(),
                    &memory.lock().unwrap(),
                    9,
                )
                .unwrap();
            assert!(
                engine
                    .foreground_epoll_returned(owner, &admission, 9)
                    .is_err()
            );
            let returned = -i64::from(libc::ENOENT);
            engine
                .original_connect_returned(owner, &admission, returned)
                .unwrap();
            engine
                .foreground_epoll_returned(owner, &admission, 9)
                .unwrap();
            assert!(
                engine
                    .foreground_epoll_returned(owner, &admission, 10)
                    .is_err()
            );
            assert!(
                engine.finish_original_connect(owner, &admission).is_err(),
                "raw return cannot replace native pair/effect/retirement"
            );
            assert_eq!(
                engine.stream_calls[&admission.call]
                    .original
                    .as_ref()
                    .unwrap()
                    .backend_result,
                Some(returned)
            );
        }
    }
    #[tokio::test]
    async fn foreground_ctl_preflight_refuses_inherited_alias_pending_and_stale_arena_without_call()
    {
        for variant in 0..5 {
            let (mut engine, owner, args, actual, memory, _runtime, joined) =
                fixture(libc::EPOLL_CTL_ADD).await;
            match variant {
                // ACK already prunes full publication history. Remove the
                // actual surviving live authority, retaining the same refusal oracle.
                0 => {
                    let binding = actual.lock().unwrap().descriptor_binding(7).unwrap();
                    engine.lifetime.clear_original_creation_for_test(binding);
                }
                1 => {
                    let _ = engine.acquire_fd_publication(owner, args.files).unwrap();
                }
                2 => {
                    memory.lock().unwrap().invalidate_original_arena();
                }
                3 => {
                    let old = actual.lock().unwrap().file_handles[&7].clone().with_fd(17);
                    let (mut next, _) = actual
                        .lock()
                        .unwrap()
                        .prepare_original_installation_typed(
                            owner.thread,
                            17,
                            nix::fcntl::OFlag::empty(),
                            crate::fd::FdType::Epoll,
                            None,
                        )
                        .unwrap();
                    next.file_handles.insert(17, old);
                    *actual.lock().unwrap() = next;
                }
                4 => {
                    let (next, _) = actual
                        .lock()
                        .unwrap()
                        .prepare_original_installation_typed(
                            owner.thread,
                            17,
                            nix::fcntl::OFlag::empty(),
                            crate::fd::FdType::Inherited { mode: 0 },
                            None,
                        )
                        .unwrap();
                    *actual.lock().unwrap() = next;
                }
                _ => unreachable!(),
            }
            assert!(
                engine
                    .begin_foreground_epoll_ctl(
                        owner,
                        args,
                        &actual,
                        &actual.lock().unwrap(),
                        &memory.lock().unwrap(),
                        joined,
                        9
                    )
                    .is_err(),
                "case {variant}"
            );
            assert!(
                engine.stream_calls.is_empty(),
                "refusal allocated a partial Call"
            );
        }
    }
}
