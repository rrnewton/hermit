//! Missing birth is never repaired by a public socket profile or safe option.
use super::*;

#[test]
fn finite_close_imported_profile_remains_without_birth() {
    let (mut engine, owner, _metadata, binding) =
        crate::network_replay::original_installation::controlled_receive_origin();
    assert!(
        engine.shadow.as_ref().unwrap().sockets[&binding.open_file]
            .finite_close
            .is_none()
    );
    for (level, option) in [
        (libc::SOL_SOCKET, libc::SO_SNDTIMEO),
        (libc::SOL_SOCKET, libc::SO_LINGER),
        (libc::IPPROTO_TCP, 31),
    ] {
        let control = engine
            .begin_socket_controls(owner, vec![binding.open_file])
            .unwrap()[0]
            .1;
        engine
            .observe_finite_close_option_attempt(owner, control, level, option)
            .unwrap();
        assert!(
            engine.shadow.as_ref().unwrap().sockets[&binding.open_file]
                .finite_close
                .is_none()
        );
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }
}

struct BirthFixture {
    engine: NetworkReplayEngine,
    owner: NetworkStreamOwner,
    binding: crate::types::FdSlotBinding,
    receipt: Installation,
    root: Arc<crate::network_runtime::ForegroundRoot>,
}

// Provider installation rows are controlled. The scheduler issues its real
// Normal observation, and the actual installation publisher plus both ACKs
// retain logical Socket origin. This fixture does not certify physical Close;
// every foreground use additionally needs the current release-profile proof.
fn born(replay: bool) -> BirthFixture {
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::Kind;
    use crate::network_runtime::original_installation::installation_fixture;
    use crate::network_runtime::original_installation::installation_with_controlled_birth;
    use crate::network_runtime::socket_origin::SocketBirthAuthority;
    use crate::network_runtime::socket_origin::controlled_birth_receipt;
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, metadata, _memory, claim) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    trace.release_model =
        detcore_model::network_trace::NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
            nodes: trace.release_model.nodes().to_vec(),
        };
    let profile = trace.fresh_stream_profiles[0].clone();
    let mut engine = if replay {
        NetworkReplayEngine::replay_shared_mm_attempts(trace).unwrap()
    } else {
        NetworkReplayEngine::record_shared_mm_attempts(trace.epoch)
    };
    engine.fd_table_fixture_enable();
    engine
        .register_initial_census(root.association(), &claim, owner.thread)
        .unwrap();
    engine
        .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
        .unwrap();
    let NetworkFdMutationBegin::Admitted(mutation) = engine
        .begin_fd_mutation(owner, root.files(), NetworkFdMutationKind::Socket)
        .unwrap()
    else {
        panic!("Socket admission");
    };
    let mutation = *mutation;
    engine
        .submit_fd_mutation(owner, mutation.publication.permit)
        .unwrap();
    let admission = engine
        .begin_original_socket(
            owner,
            Arguments {
                kind: Kind::Socket,
                operation: crate::resources::ExternalOpId::new(owner.thread, 10),
                files: root.files(),
                binding: None,
                fd: libc::AF_INET,
                address: libc::SOCK_STREAM as u64,
                length: 0,
                original_count: 0,
            },
            mutation.clone(),
        )
        .unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let authority = SocketBirthAuthority::from_original(root.clone(), &grant, &admission).unwrap();
    let birth = controlled_birth_receipt(authority, owner, &admission).unwrap();
    engine
        .original_connect_provider_submitted(owner, &admission)
        .unwrap();
    engine
        .original_call_prepared(owner, &admission, None, 71)
        .unwrap();
    engine.original_connect_invoked(owner, &admission).unwrap();
    engine
        .original_connect_selected(owner, &admission, 71, (7, 31, 101, 13, 19))
        .unwrap();
    engine
        .original_connect_returned(owner, &admission, 17)
        .unwrap();
    engine
        .original_connect_provider_retired(owner, &admission, 17)
        .unwrap();
    engine
        .original_connect_pin_released(owner, &admission)
        .unwrap();
    let receipt = installation_with_controlled_birth(
        installation_fixture(
            owner,
            metadata.clone(),
            mutation.publication.permit,
            Source::Socket(admission.call),
            71,
            17,
            false,
        ),
        birth,
    )
    .unwrap();
    engine
        .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17))
        .unwrap();
    let binding = engine
        .publish_original_installation(
            owner,
            &mutation.publication,
            &receipt,
            (&metadata, &mut metadata.lock().unwrap()),
            (
                nix::fcntl::OFlag::O_CLOEXEC,
                None,
                Some(super::super::original_installation::FreshStreamEnrollment {
                    key: profile.key,
                    namespace: super::super::tests::test_socket_namespace(),
                    observed_profile: (!replay).then_some(profile),
                }),
            ),
            LogicalTime::ZERO,
        )
        .unwrap();
    engine
        .original_socket_publication_finished(owner, &admission, mutation.publication.permit)
        .unwrap();
    engine.finish_original_connect(owner, &admission).unwrap();
    assert!(engine.fd_publication_history.is_empty());
    assert!(engine.fd_installations.is_empty());
    BirthFixture {
        engine,
        owner,
        binding,
        receipt,
        root,
    }
}

fn read_birth(f: &mut BirthFixture, fd: i32) -> Option<Arc<FiniteCloseBirth>> {
    let NetworkFdReadBegin::Admitted(read) = f
        .engine
        .begin_fd_read(f.owner, f.binding.slot.files, fd)
        .unwrap()
    else {
        panic!("reader admission");
    };
    let birth = f
        .engine
        .finite_close_birth_for_read(f.owner, &read)
        .unwrap();
    f.engine.finish_fd_read(f.owner, *read).unwrap();
    birth
}

#[test]
fn finite_close_real_publisher_issues_both_modes_without_replay_receive_authority() {
    for replay in [false, true] {
        let mut f = born(replay);
        let birth = read_birth(&mut f, 17).unwrap();
        assert!(birth.matches_initial_root(&f.root));
        assert_eq!(
            f.engine.shadow.as_ref().unwrap().sockets[&f.binding.open_file].has_native_receive(),
            !replay
        );
        assert!(Arc::ptr_eq(&birth, &read_birth(&mut f, 17).unwrap()));
        let mut wrong = f.owner;
        wrong.mm = wrong.mm.for_exec(wrong.thread);
        let NetworkFdReadBegin::Admitted(read) = f
            .engine
            .begin_fd_read(f.owner, f.binding.slot.files, 17)
            .unwrap()
        else {
            panic!("reader");
        };
        assert!(f.engine.finite_close_birth_for_read(wrong, &read).is_err());
        let mut changed = (*read).clone();
        changed.fd += 1;
        assert!(
            f.engine
                .finite_close_birth_for_read(f.owner, &changed)
                .is_err()
        );
        f.engine.finish_fd_read(f.owner, *read).unwrap();
    }
}

// The real descriptor-mutation/publisher path consumes the explicit controlled
// successful dup result. No kernel dup or file-metadata/native claim is made.
fn alias(f: &mut BirthFixture) {
    let NetworkFdMutationBegin::Admitted(mutation) = f
        .engine
        .begin_fd_mutation(
            f.owner,
            f.binding.slot.files,
            NetworkFdMutationKind::Alias {
                source_fd: 17,
                source: Some(f.binding),
                kind: NetworkFdInstallKind::Dup,
                cloexec: false,
                destination: None,
                replaced: None,
            },
        )
        .unwrap()
    else {
        panic!("alias admission");
    };
    let permit = mutation.publication.permit;
    f.engine.submit_fd_mutation(f.owner, permit).unwrap();
    f.engine
        .confirm_fd_mutation_result(f.owner, permit, Ok(18))
        .unwrap();
    let generation = mutation.publication.acknowledged_generation + 1;
    let change = crate::types::NetworkFdSlotReplacement {
        files: permit.files,
        installation_generation: generation,
        before: None,
        after: Some(crate::types::NetworkFdSlot {
            binding: crate::types::FdSlotBinding {
                slot: crate::types::FdSlot {
                    files: permit.files,
                    fd: 18,
                },
                generation,
                open_file: f.binding.open_file,
            },
            cloexec: false,
        }),
    };
    let effect = f
        .engine
        .confirm_fd_installation(f.owner, permit, change)
        .unwrap();
    let batch = NetworkFdPublicationBatch {
        files: permit.files,
        sequence: mutation.publication.acknowledged_sequence + 1,
        previous_generation: mutation.publication.acknowledged_generation,
        through_generation: generation,
        entries: vec![NetworkFdPublicationEntry {
            replacement: change,
            effect,
        }],
    };
    assert_eq!(
        f.engine
            .publish_fd_publication(f.owner, permit, &batch)
            .unwrap(),
        batch
    );
    f.engine
        .acknowledge_fd_publication(f.owner, permit, &batch)
        .unwrap();
}

#[test]
fn finite_close_unknown_attempt_revokes_all_aliases_before_any_native_result() {
    for replay in [false, true] {
        for (level, option) in [
            (libc::SOL_SOCKET, libc::SO_LINGER),
            (libc::IPPROTO_TCP, 31),
            (282, 1),
            (libc::SOL_SOCKET, libc::SO_RCVBUF),
        ] {
            let mut f = born(replay);
            let birth = read_birth(&mut f, 17).unwrap();
            alias(&mut f);
            assert!(Arc::ptr_eq(&birth, &read_birth(&mut f, 18).unwrap()));
            for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO, libc::SO_RCVLOWAT] {
                let control = f
                    .engine
                    .begin_socket_controls(f.owner, vec![f.binding.open_file])
                    .unwrap()[0]
                    .1;
                f.engine
                    .observe_finite_close_option_attempt(f.owner, control, libc::SOL_SOCKET, option)
                    .unwrap();
                f.engine
                    .finish_socket_control(f.owner, control, NetworkSocketControlFinish::Unchanged)
                    .unwrap();
                assert!(Arc::ptr_eq(&birth, &read_birth(&mut f, 18).unwrap()));
            }
            let control = f
                .engine
                .begin_socket_controls(f.owner, vec![f.binding.open_file])
                .unwrap()[0]
                .1;
            f.engine
                .observe_finite_close_option_attempt(f.owner, control, level, option)
                .unwrap();
            assert!(
                f.engine
                    .validate_finite_close_birth(f.binding.open_file, &birth)
                    .is_err()
            );
            // No native effect was submitted; fault/error/cancel cleanup cannot
            // put back the revoked authority.
            f.engine
                .finish_socket_control(f.owner, control, NetworkSocketControlFinish::Unchanged)
                .unwrap();
            assert!(read_birth(&mut f, 17).is_none());
            assert!(read_birth(&mut f, 18).is_none());
            f.engine
                .validate_finite_close_installation(f.binding, &f.receipt)
                .unwrap();
            f.engine
                .retain_finite_close_installation(f.binding, &f.receipt);
            assert!(
                read_birth(&mut f, 17).is_none(),
                "same original re-enrollment cannot restore birth"
            );
        }
    }
}

#[test]
fn finite_close_wrong_control_cannot_revoke_or_replace_another_birth() {
    let mut f = born(false);
    let birth = read_birth(&mut f, 17).unwrap();
    let control = f
        .engine
        .begin_socket_controls(f.owner, vec![f.binding.open_file])
        .unwrap()[0]
        .1;
    let mut foreign = f.owner;
    foreign.mm = foreign.mm.for_exec(foreign.thread);
    assert!(
        f.engine
            .observe_finite_close_option_attempt(
                foreign,
                control,
                libc::SOL_SOCKET,
                libc::SO_LINGER
            )
            .is_err()
    );
    f.engine
        .validate_finite_close_birth(f.binding.open_file, &birth)
        .unwrap();
    f.engine
        .finish_socket_control(f.owner, control, NetworkSocketControlFinish::Unchanged)
        .unwrap();
    assert!(Arc::ptr_eq(&birth, &read_birth(&mut f, 17).unwrap()));
}
