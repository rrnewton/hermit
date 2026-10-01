//! Local policy boundary tests. Existing census/socket fixtures are explicit
//! component premises, not evidence of a native provider or DSR qualification.
use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::scheduler::Scheduler;
use crate::types::DetTid;
use crate::types::MmId;

fn now() -> LogicalTime {
    LogicalTime::from_nanos(1_790_000_000_000_000_000)
}

/// Controlled census and channel-ledger premises, with the real scheduler
/// grant, runtime prefix join and descriptor-close state machine. The socket
/// close below is real; this is not a provider original-Close/DSR qualification.
pub(super) struct ClosePolicyFixture {
    root: Arc<ForegroundRoot>,
    scheduler: Scheduler,
    runtime: crate::network_runtime::NetworkRuntimeResources,
    prefix: crate::network_runtime::JoinedNativePrefix,
    metadata: Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    memory: Arc<std::sync::Mutex<crate::memory::MemoryMetadata>>,
}
impl ClosePolicyFixture {
    pub(super) async fn new() -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (root, metadata, memory, _) = crate::network_runtime::controlled_foreground_root(raw);
        let mut scheduler = Scheduler::new(&crate::config::Config::default());
        scheduler.controlled_foreground_store_grant(&root);
        let epoch = scheduler
            .foreground_native_observation(root.owner(), &root)
            .unwrap()
            .epoch();
        let (runtime, prefix) =
            crate::network_runtime::controlled_joined_prefix(root.clone()).await;
        assert_eq!(
            scheduler
                .foreground_native_observation(root.owner(), &root)
                .unwrap()
                .epoch(),
            epoch
        );
        Self {
            root,
            scheduler,
            runtime,
            prefix,
            metadata,
            memory,
        }
    }
    pub(super) fn owner(&self) -> NetworkStreamOwner {
        self.root.owner()
    }

    pub(super) fn close(
        &self,
        engine: &mut NetworkReplayEngine,
        file: OpenFileId,
        socket: std::os::fd::OwnedFd,
    ) -> Result<(), NetworkReplayError> {
        let owner = self.owner();
        let grant = self
            .scheduler
            .foreground_native_observation(owner, &self.root)
            .unwrap();
        assert!(self.root.matches_memory(&self.memory));
        assert!(self.root.matches_metadata(&self.metadata));
        let control = {
            let _memory = self.memory.lock().unwrap();
            let _metadata = self.metadata.lock().unwrap();
            let control = engine.begin_socket_controls(owner, vec![file])?[0].1;
            self.runtime
                .with_foreground_prefix(&self.prefix, |admission| {
                    engine
                        .submit_native_descriptor_close(control, admission, &grant)
                        .map_err(std::io::Error::other)
                })
                .map_err(|e| invalid(&e.to_string()))?;
            control
        };
        // Neither the issuer nor this fixture invents a receive/Connect input.
        // Keep the original real-close and exact EBADF oracle in one helper.
        tests::close_retirement_socket(socket);
        engine.confirm_descriptor_effect(owner, control, Ok(()))?;
        engine.finish_socket_control(
            owner,
            control,
            NetworkSocketControlFinish::Closed { last_alias: true },
        )
    }
}

#[tokio::test]
async fn v4_close_refuses_generic_normal_before_descriptor_submission() {
    use std::os::fd::AsRawFd;
    let f = ClosePolicyFixture::new().await;
    let (mut engine, owner, file, _, socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
    let generic = f
        .scheduler
        .foreground_epoll_observation(owner, &f.root)
        .unwrap();
    assert_eq!(
        generic.resume(),
        crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
    );
    let before = engine.native_trace_fixture();
    let error = f
        .runtime
        .with_foreground_prefix(&f.prefix, |admission| {
            engine
                .submit_native_descriptor_close(control, admission, &generic)
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        invalid("V4 close lacks its borrowed sole-initial-root grant").to_string()
    );
    assert_eq!(engine.native_trace_fixture(), before);
    assert!(
        engine
            .owned_socket_control(owner, control)
            .unwrap()
            .physical
            .pending
            .is_none()
    );
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.policy_root.is_none());
    assert!(unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) } >= 0);
}

#[tokio::test]
async fn v4_close_refuses_a_different_root_arc_before_descriptor_submission() {
    let f = ClosePolicyFixture::new().await;
    let (other, _metadata, _memory, _) =
        crate::network_runtime::controlled_foreground_root(f.root.thread());
    assert_eq!(f.owner(), other.owner());
    assert!(!Arc::ptr_eq(&f.root, &other));
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(other).await;
    let (mut engine, owner, file, _, _socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
    let grant = f
        .scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let error = runtime
        .with_foreground_prefix(&prefix, |admission| {
            engine
                .submit_native_descriptor_close(control, admission, &grant)
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        invalid("V4 close lacks its borrowed sole-initial-root grant").to_string()
    );
    assert!(
        engine
            .owned_socket_control(owner, control)
            .unwrap()
            .physical
            .pending
            .is_none()
    );
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.policy_root.is_none());
}

#[tokio::test]
async fn v4_close_cannot_acquire_policy_after_descriptor_submission() {
    let f = ClosePolicyFixture::new().await;
    let (mut engine, owner, file, _, _socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
    // Deliberately exercise the older submission path without policy. This is
    // a phase-negative fixture; no physical close/result is fabricated here.
    engine
        .submit_descriptor_effect(owner, control, NetworkDescriptorEffect::CloseDescriptor)
        .unwrap();
    let grant = f
        .scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let error = f
        .runtime
        .with_foreground_prefix(&f.prefix, |admission| {
            engine
                .submit_native_descriptor_close(control, admission, &grant)
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        invalid("V4 close policy admission requires an unsubmitted control").to_string()
    );
    assert_eq!(
        engine
            .owned_socket_control(owner, control)
            .unwrap()
            .physical
            .pending,
        Some(NetworkDescriptorEffect::CloseDescriptor)
    );
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.policy_root.is_none());
    assert!(native.trace.release_model.nodes().is_empty());
}

#[tokio::test]
async fn v4_close_refuses_pending_option_without_losing_its_custody() {
    let f = ClosePolicyFixture::new().await;
    let (mut engine, owner, file, _, _socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
    engine
        .submit_socket_option(
            owner,
            control,
            NetworkStreamSocketOption::ReceiveLowWater(1),
        )
        .unwrap();
    let before = engine
        .owned_socket_control(owner, control)
        .unwrap()
        .physical
        .option_pending
        .clone();
    assert!(before.is_some());
    let grant = f
        .scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let error = f
        .runtime
        .with_foreground_prefix(&f.prefix, |admission| {
            engine
                .submit_native_descriptor_close(control, admission, &grant)
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        invalid("V4 close policy admission requires an unsubmitted control").to_string()
    );
    let held = engine.owned_socket_control(owner, control).unwrap();
    assert_eq!(held.physical.option_pending, before);
    assert!(held.physical.pending.is_none());
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.policy_root.is_none());
}

#[tokio::test]
async fn v4_close_cannot_switch_its_retained_policy_root() {
    let f = ClosePolicyFixture::new().await;
    let (mut engine, _, file, _, socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    f.close(&mut engine, file, socket).unwrap();
    // A separately reconstructed census/root with equal numeric identity is
    // deliberately not the recorder's retained authority. Both borrows use
    // their own actual issuer; only the cross-recorder Arc check may refuse.
    let other = ClosePolicyFixture::new().await;
    assert_eq!(f.owner(), other.owner());
    assert!(!Arc::ptr_eq(&f.root, &other.root));
    let second = OpenFileId::new_socket(DetTid::from_raw(61), 2);
    tests::retirement_binding(&mut engine, second);
    let control = engine
        .begin_socket_controls(other.owner(), vec![second])
        .unwrap()[0]
        .1;
    let grant = other
        .scheduler
        .foreground_native_observation(other.owner(), &other.root)
        .unwrap();
    let before = engine.native_trace_fixture();
    let error = other
        .runtime
        .with_foreground_prefix(&other.prefix, |admission| {
            engine
                .submit_native_descriptor_close(control, admission, &grant)
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        invalid("V4 close changed its retained initial-root authority").to_string()
    );
    assert_eq!(engine.native_trace_fixture(), before);
    assert!(
        engine
            .owned_socket_control(other.owner(), control)
            .unwrap()
            .physical
            .pending
            .is_none()
    );
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(Arc::ptr_eq(native.policy_root.as_ref().unwrap(), &f.root));
}

#[tokio::test]
async fn v4_entry_refuses_generic_normal_grant_at_the_policy_boundary() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut s = Scheduler::new(&crate::config::Config::default());
    s.controlled_foreground_store_grant(&root);
    let generic = s.foreground_epoll_observation(owner, &root).unwrap();
    assert_eq!(
        generic.resume(),
        crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
    );
    assert!(root.is_sole_initial_root(owner));
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
    let error = runtime
        .with_foreground_prefix(&prefix, |admission| {
            engine
                .stamp_native_receive_entry(attempt, admission, &generic, now())
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("borrowed sole-initial-root grant")
    );
    assert!(engine.stream_calls[&call].native_entry.is_none());
    assert_eq!(
        engine.stream_calls[&call].phase,
        StreamCallPhase::PinAcquireSubmitted
    );
    assert!(
        engine.begin_native_entry_stamp(owner, call).is_err(),
        "failed attempt was refreshed"
    );
}

#[tokio::test]
async fn v4_entry_accepts_actual_sole_root_borrow_before_capture() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut s = Scheduler::new(&crate::config::Config::default());
    s.controlled_foreground_store_grant(&root);
    let grant = s.foreground_native_observation(owner, &root).unwrap();
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
    runtime
        .with_foreground_prefix(&prefix, |admission| {
            engine
                .stamp_native_receive_entry(attempt, admission, &grant, now())
                .map_err(std::io::Error::other)
        })
        .unwrap();
    let release = engine
        .native_entry_release(owner, call, &root, grant.epoch(), now())
        .unwrap();
    assert_eq!(release.receive_entry_cut, NetworkReceiveEntryCutV4(0));
    assert!(release.prerequisites.is_empty());
    engine
        .validate_native_foreground_call(call, &grant, now())
        .unwrap();
    let generic = s.foreground_epoll_observation(owner, &root).unwrap();
    let error = engine
        .validate_native_foreground_call(call, &generic, now())
        .unwrap_err();
    assert!(error.to_string().contains("current sole-root borrow"));
}

#[tokio::test]
async fn v4_entry_refuses_a_different_root_arc_with_identical_ids() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let (other, _other_metadata, _other_memory, _) =
        crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    assert_eq!(owner, other.owner());
    assert!(!Arc::ptr_eq(&root, &other));
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(other).await;
    let mut s = Scheduler::new(&crate::config::Config::default());
    s.controlled_foreground_store_grant(&root);
    let grant = s.foreground_native_observation(owner, &root).unwrap();
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
    let error = runtime
        .with_foreground_prefix(&prefix, |admission| {
            engine
                .stamp_native_receive_entry(attempt, admission, &grant, now())
                .map_err(std::io::Error::other)
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("borrowed sole-initial-root grant")
    );
    assert!(engine.stream_calls[&call].native_entry.is_none());
}

#[tokio::test]
async fn v4_call_cannot_adopt_a_later_actual_normal_grant() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut s = Scheduler::new(&crate::config::Config::default());
    s.controlled_foreground_store_grant(&root);
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let original_epoch = {
        let grant = s.foreground_native_observation(owner, &root).unwrap();
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |admission| {
                engine
                    .stamp_native_receive_entry(attempt, admission, &grant, now())
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        engine
            .validate_native_foreground_call(call, &grant, now())
            .unwrap();
        grant.epoch()
    };
    s.controlled_foreground_store_grant(&root);
    let grant = s.foreground_native_observation(owner, &root).unwrap();
    assert!(grant.epoch() > original_epoch);
    let error = engine
        .validate_native_foreground_call(call, &grant, now())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("one-use entry/root/grant/ledger")
    );
}

#[tokio::test]
async fn v4_transmit_cannot_invent_entry_from_current_frontier() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut s = Scheduler::new(&crate::config::Config::default());
    s.controlled_foreground_store_grant(&root);
    let grant = s.foreground_native_observation(owner, &root).unwrap();
    let (mut engine, call, _, _) = runtime
        .with_foreground_prefix(&prefix, |admission| {
            Ok(NetworkReplayEngine::controlled_native_receive_pending(
                owner, admission, &grant,
            ))
        })
        .unwrap();
    let release = engine.native_transmit_entry(owner, call).unwrap();
    assert_eq!(release.receive_entry_cut, NetworkReceiveEntryCutV4(2));
    // Deliberate missing-source mutant, while preserving the valid trace,
    // actual root, and Call. A snapshot cannot replace this removed provenance.
    engine.stream_calls.get_mut(&call).unwrap().native_entry = None;
    let error = engine.native_transmit_entry(owner, call).unwrap_err();
    assert!(error.to_string().contains("pre-capture sole-root entry"));
}

#[test]
fn v4_retirement_without_entry_proof_latches_failure_without_progress() {
    let owner = NetworkStreamOwner {
        thread: DetTid::from_raw(61),
        mm: MmId::initial(DetTid::from_raw(61)),
    };
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let open_file = engine.stream_calls[&call].open_file.unwrap();
    let channel = engine.bound_channel(open_file).unwrap();
    let error = engine.retain_native_retirement(channel).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("retained sole-initial-root history")
    );
    assert!(engine.check_native_retirement().is_err());
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.trace.release_model.nodes().is_empty());
}

#[tokio::test]
async fn v4_retirement_accepts_retained_sole_root_without_fabricated_establishment() {
    let f = ClosePolicyFixture::new().await;
    let (mut engine, _, file, channel, socket, _peer) =
        tests::retirement_fixture_for_owner(false, f.owner());
    assert!(engine.stream_calls.is_empty());
    f.close(&mut engine, file, socket).unwrap();
    assert!(engine.stream_calls.is_empty());
    engine.check_native_retirement().unwrap();
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    assert!(native.trace.inputs.is_empty());
    assert_eq!(
        native.trace.release_model.nodes(),
        &[NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(0),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::Retired
            },
            prerequisites: vec![],
        }]
    );
}

#[tokio::test]
async fn v4_retained_entry_cannot_publish_after_authenticated_shared_birth() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let mut retained = None;
    let f = ForegroundRoot::controlled_shared_birth_after_entry(raw, |runtime, root, prefix| {
        let owner = root.owner();
        let mut s = Scheduler::new(&crate::config::Config::default());
        s.controlled_foreground_store_grant(root);
        let grant = s.foreground_native_observation(owner, root).unwrap();
        let (mut engine, call) = tests::unsubmitted_entry(owner);
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        runtime
            .with_foreground_prefix(prefix, |admission| {
                engine
                    .stamp_native_receive_entry(attempt, admission, &grant, now())
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        engine
            .native_entry_release(owner, call, root, grant.epoch(), now())
            .unwrap();
        retained = Some((engine, call, grant.epoch()));
    })
    .await;
    let (engine, call, epoch) = retained.unwrap();
    assert!(f.parent.is_current(f.parent.owner()));
    assert!(f.child.is_current(f.child.owner()));
    assert!(f.parent.same_memory_authority(&f.child));
    assert!(
        matches!(engine.finish(), Err(NetworkReplayError::UnresolvedStreamCall(id)) if id == call),
        "policy loss must not hide the exact unresolved Call"
    );
    let policy_error = engine.check_native_retirement().unwrap_err();
    assert!(
        policy_error
            .to_string()
            .contains("sole-initial-root policy")
    );
    let error = engine
        .native_entry_release(f.parent.owner(), call, &f.parent, epoch, now())
        .unwrap_err();
    assert!(error.to_string().contains("sole-initial-root policy"));
    assert!(
        !engine.stream_calls[&call]
            .native_entry
            .as_ref()
            .unwrap()
            .used
    );
}

#[tokio::test]
async fn v4_finish_checks_policy_after_actual_close_resolves_custody() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let mut retained = None;
    let f = ForegroundRoot::controlled_shared_birth_after_entry(raw, |runtime, root, prefix| {
        let owner = root.owner();
        let mut scheduler = Scheduler::new(&crate::config::Config::default());
        scheduler.controlled_foreground_store_grant(root);
        let grant = scheduler
            .foreground_native_observation(owner, root)
            .unwrap();
        let (mut engine, _, file, _, socket, _peer) =
            tests::retirement_fixture_for_owner(false, owner);
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        runtime
            .with_foreground_prefix(prefix, |admission| {
                engine
                    .submit_native_descriptor_close(control, admission, &grant)
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        tests::close_retirement_socket(socket);
        engine
            .confirm_descriptor_effect(owner, control, Ok(()))
            .unwrap();
        engine
            .finish_socket_control(
                owner,
                control,
                NetworkSocketControlFinish::Closed { last_alias: true },
            )
            .unwrap();
        engine.check_stream_operations_finished().unwrap();
        retained = Some(engine);
    })
    .await;
    let engine = retained.unwrap();
    assert!(f.parent.is_current(f.parent.owner()));
    assert!(f.child.is_current(f.child.owner()));
    assert!(engine.stream_calls.is_empty());
    assert!(engine.socket_controls.is_empty());
    let expected = invalid("V4 recorder lost its retained sole-initial-root policy").to_string();
    assert_eq!(
        engine
            .check_stream_operations_finished()
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(engine.finish().unwrap_err().to_string(), expected);
    assert_eq!(
        engine.into_native_recorded_trace().unwrap_err().to_string(),
        expected
    );
}

#[test]
fn v4_finish_keeps_first_structural_retirement_error_before_call_custody() {
    let owner = NetworkStreamOwner {
        thread: DetTid::from_raw(61),
        mm: MmId::initial(DetTid::from_raw(61)),
    };
    let (mut engine, call) = tests::unsubmitted_entry(owner);
    let channel = engine
        .bound_channel(engine.stream_calls[&call].open_file.unwrap())
        .unwrap();
    let EngineState::Native(native) = &mut engine.mode else {
        unreachable!()
    };
    // Deliberately malformed ledger, not a fabricated positive backend event.
    native.trace.channels.clear();
    let error = engine.retain_native_retirement(channel).unwrap_err();
    assert!(matches!(&error, NetworkReplayError::NativeRetirement {
        channel: actual, error: NetworkTraceValidationErrorV4::InvalidReference,
    } if *actual == channel));
    assert!(engine.stream_calls.contains_key(&call));
    assert_eq!(
        format!("{:?}", engine.finish().unwrap_err()),
        format!("{error:?}")
    );
    assert_eq!(
        format!("{:?}", engine.into_native_recorded_trace().unwrap_err()),
        format!("{error:?}")
    );
}
