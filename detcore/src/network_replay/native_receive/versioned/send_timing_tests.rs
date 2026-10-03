//! Engine components, not native send qualification. Connected/profile and
//! physical-return values are explicit controlled premises. The Call, entry,
//! control lease, retained root borrow, timing claim and scheduler receipt all
//! come from their existing issuers; no private token fields are constructed.
use std::sync::Arc;
use std::sync::Mutex;

use chrono::TimeZone;

use super::*;
use crate::config::Config;
use crate::network_runtime::ForegroundRoot;
use crate::resources::ExternalOpId;
use crate::scheduler::Scheduler;
use crate::scheduler::send_handback::SendHandbackReceipt;
use crate::scheduler::send_handback::SendTimingError;
use crate::scheduler::send_handback::SendTimingHandle;
use crate::types::GlobalTime;

const SOURCE: &[u8] = b"retained source";

struct Fixture {
    root: Arc<ForegroundRoot>,
    // Keep the existing root's actual weak metadata/MM associations alive.
    _metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    _memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    _runtime: crate::network_runtime::NetworkRuntimeResources,
    scheduler: Scheduler,
    global: Arc<Mutex<GlobalTime>>,
    engine: NetworkReplayEngine,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    old_capture_lease: NetworkStreamLeaseId,
    operation: ExternalOpId,
    normal_epoch: u64,
}

fn config() -> Config {
    Config {
        epoch: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        ..Config::default()
    }
}

impl Fixture {
    async fn new() -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (root, metadata, memory, _) = crate::network_runtime::controlled_foreground_root(raw);
        let owner = root.owner();
        let (runtime, prefix) =
            crate::network_runtime::controlled_joined_prefix(root.clone()).await;
        let config = config();
        let global = Arc::new(Mutex::new(GlobalTime::new(&config)));
        let mut scheduler = Scheduler::new(&config);
        scheduler.controlled_publish_send_clock(&global);
        scheduler.controlled_foreground_store_grant(&root);
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .unwrap();
        let normal_epoch = grant.epoch();
        let (mut engine, call) = tests::unsubmitted_entry(owner);
        let open_file = engine.stream_calls[&call].open_file.unwrap();
        let old_capture_lease = engine.socket_controls[&open_file].lease;
        let channel = engine.bound_channel(open_file).unwrap();
        let key = engine.shadow.as_ref().unwrap().sockets[&open_file].key;
        engine.retain_native_fresh_send(key);
        // Reuse the existing fixture's Connected/profile premises, not a new
        // native authority. The entry below is issued AFTER these prior rows.
        let EngineState::Native(native) = &mut engine.mode else {
            unreachable!()
        };
        native.trace.inputs.push(NetworkInputEventV4 {
            ordinal: 0,
            channel,
            release: NetworkReleaseV4 {
                not_before_global_time: scheduler.committed_time,
                receive_entry_cut: NetworkReceiveEntryCutV4(0),
                prerequisites: vec![],
            },
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        });
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.extend([
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(0),
                kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 0 },
                prerequisites: vec![],
            },
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(1),
                kind: NetworkReleaseNodeKindV4::Progress {
                    channel,
                    milestone: NetworkProgressV4::Established {
                        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
                    },
                },
                prerequisites: vec![NetworkReleaseNodeIdV4(0)],
            },
        ]);
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |admission| {
                engine
                    .stamp_native_receive_entry(
                        attempt,
                        admission,
                        &grant,
                        scheduler.committed_time,
                    )
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        engine
            .validate_native_foreground_call(call, &grant, scheduler.committed_time)
            .unwrap();
        // Pin-acquired is the same explicit component premise as the existing
        // controlled_native_receive_pending; no socket/kernel operation runs.
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(
                owner,
                old_capture_lease,
                NetworkSocketControlFinish::Unchanged,
            )
            .unwrap();
        let lease = engine
            .begin_native_transmit(owner, call, SOURCE.to_vec(), libc::MSG_NOSIGNAL)
            .unwrap();
        assert_ne!(lease, old_capture_lease);
        engine.native_trace_fixture().validate().unwrap();
        Self {
            root,
            _metadata: metadata,
            _memory: memory,
            _runtime: runtime,
            scheduler,
            global,
            engine,
            call,
            lease,
            old_capture_lease,
            operation: ExternalOpId::new(owner.thread, 19),
            normal_epoch,
        }
    }
    fn owner(&self) -> NetworkStreamOwner {
        self.root.owner()
    }
    fn claim(&mut self) -> Result<SendTimingHandle, NetworkReplayError> {
        self.scheduler
            .claim_send_timing(&mut self.engine, &self.root, self.lease, self.operation)
    }
    fn submit(&mut self) {
        self.engine
            .submit_stream_physical(
                self.owner(),
                self.lease,
                NetworkStreamPhysicalEffect::Transmit {
                    bytes: SOURCE.to_vec(),
                    flags: libc::MSG_NOSIGNAL,
                },
            )
            .unwrap();
    }
    fn handback(&mut self, handle: &SendTimingHandle) -> SendHandbackReceipt {
        self.scheduler
            .controlled_complete_engine_send(self.owner(), self.operation, &self.global);
        self.scheduler.take_send_handback(handle).unwrap()
    }
    fn unclaimed(&self) {
        let p = self
            .engine
            .owned_socket_control(self.owner(), self.lease)
            .unwrap()
            .physical
            .transmit_pending
            .as_ref()
            .unwrap();
        assert_eq!(p.call, self.call);
        assert!(!p._timing_claimed);
        assert!(
            p._timing_identity.is_none()
                && p._timing_normal_epoch.is_none()
                && p._timing_receipt.is_none()
        );
    }
    fn has_receipt(&self) -> bool {
        self.engine
            .owned_socket_control(self.owner(), self.lease)
            .unwrap()
            .physical
            .transmit_pending
            .as_ref()
            .unwrap()
            ._timing_receipt
            .is_some()
    }
    fn legacy_confirmation(&mut self) -> Result<(), NetworkReplayError> {
        // Deliberately the old writer's existing unauthenticated result shape.
        // It must NOT become original-send EXIT authority after enrollment.
        self.engine
            .confirm_native_transmit_if_pending(
                self.owner(),
                self.lease,
                &crate::network_runtime::native_peer::Observation {
                    raw_return: SOURCE.len() as i64,
                    errno: None,
                    bytes: vec![],
                    confirmation: NetworkStreamPhysicalResult::Transmitted {
                        count: SOURCE.len(),
                    },
                    helper_copy: None,
                },
            )
            .expect("actual retained transmit is present")
    }
}

#[tokio::test]
async fn engine_claim_accepts_same_call_root_and_consumes_scheduler_receipt_once() {
    let mut f = Fixture::new().await;
    f.unclaimed();
    let before = f.engine.native_trace_fixture();
    let handle = f.claim().unwrap();
    let p = f
        .engine
        .owned_socket_control(f.owner(), f.lease)
        .unwrap()
        .physical
        .transmit_pending
        .as_ref()
        .unwrap();
    assert_eq!(p.call, f.call);
    assert_eq!(p._timing_normal_epoch, Some(f.normal_epoch));
    assert!(p._timing_claimed && p._timing_identity.is_some() && !p.submitted);
    assert!(
        matches!(f.claim(), Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id == f.lease)
    );
    f.submit();
    let receipt = f.handback(&handle);
    assert_eq!(receipt.operation(), f.operation);
    let stamps = receipt.stamps();
    assert_eq!(stamps[0].epoch, f.normal_epoch);
    assert_eq!(stamps[1].epoch, f.normal_epoch + 1);
    assert_eq!(stamps[2].epoch, stamps[1].epoch);
    assert_eq!(stamps[3].epoch, f.normal_epoch + 2);
    assert!(stamps[3].publication > stamps[2].publication);
    assert_eq!(
        f.scheduler.take_send_handback(&handle).unwrap_err(),
        SendTimingError::Missing
    );
    f.engine
        .accept_send_handback(f.owner(), f.lease, receipt)
        .unwrap();
    assert!(f.has_receipt());
    assert_eq!(
        f.engine.native_trace_fixture(),
        before,
        "temporal evidence writes no physical output"
    );
}

#[tokio::test]
async fn engine_foreign_pending_same_numeric_call_and_lease_is_refused() {
    let mut target = Fixture::new().await;
    let mut foreign = Fixture::new().await;
    assert_eq!(
        (target.owner(), target.call, target.lease),
        (foreign.owner(), foreign.call, foreign.lease)
    );
    let target_handle = target.claim().unwrap();
    let foreign_handle = foreign.claim().unwrap();
    target.submit();
    foreign.submit();
    let receipt = foreign.handback(&foreign_handle);
    assert!(
        matches!(target.engine.accept_send_handback(target.owner(), target.lease, receipt),
        Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id == target.lease)
    );
    assert!(!target.has_receipt());
    let receipt = target.handback(&target_handle);
    target
        .engine
        .accept_send_handback(target.owner(), target.lease, receipt)
        .unwrap();
    assert!(target.has_receipt());
}

#[tokio::test]
async fn engine_claim_refuses_actual_retired_capture_lease_without_consuming_pending() {
    let mut f = Fixture::new().await;
    let error = f
        .scheduler
        .claim_send_timing(&mut f.engine, &f.root, f.old_capture_lease, f.operation)
        .unwrap_err();
    assert!(
        matches!(error, NetworkReplayError::UnknownStreamLease(id) if id == f.old_capture_lease)
    );
    f.unclaimed();
    assert!(f.claim().is_ok());
}

#[tokio::test]
async fn engine_accept_refuses_actual_retired_capture_lease() {
    let mut f = Fixture::new().await;
    let handle = f.claim().unwrap();
    f.submit();
    let receipt = f.handback(&handle);
    assert!(
        matches!(f.engine.accept_send_handback(f.owner(), f.old_capture_lease, receipt),
        Err(NetworkReplayError::UnknownStreamLease(id)) if id == f.old_capture_lease)
    );
    assert!(!f.has_receipt());
    assert!(
        matches!(f.engine.finish(), Err(NetworkReplayError::UnresolvedStreamCall(id)) if id == f.call)
    );
}

#[tokio::test]
async fn engine_claim_refuses_other_root_arc_with_identical_task_ids() {
    let mut f = Fixture::new().await;
    let other = Fixture::new().await;
    assert_eq!(f.owner(), other.owner());
    assert!(!Arc::ptr_eq(&f.root, &other.root));
    let grant = other
        .scheduler
        .foreground_native_observation(other.owner(), &other.root)
        .unwrap();
    let error = f
        .engine
        .take_pending_send_timing(f.lease, &grant, other.scheduler.committed_time)
        .unwrap_err();
    assert!(error.to_string().contains("current sole-root borrow"));
    f.unclaimed();
    assert!(f.claim().is_ok());
}

#[tokio::test]
async fn engine_claim_refuses_later_real_normal_epoch() {
    let mut f = Fixture::new().await;
    f.scheduler.controlled_foreground_store_grant(&f.root);
    assert!(
        f.scheduler
            .foreground_native_observation(f.owner(), &f.root)
            .unwrap()
            .epoch()
            > f.normal_epoch
    );
    let error = f.claim().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("one-use entry/root/grant/ledger")
    );
    f.unclaimed();
}

#[tokio::test]
async fn engine_claim_refuses_unpublished_clock_assignment_before_enrollment() {
    let mut f = Fixture::new().await;
    // Negative corruption premise only: this public clock assignment is NOT
    // a call to the production publication operation used by Fixture::new.
    f.scheduler.committed_time = f.scheduler.committed_time + LogicalTime::from_nanos(1);
    assert!(
        matches!(f.claim(), Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id == f.lease)
    );
    f.unclaimed();
}

#[tokio::test]
async fn engine_claim_refuses_already_submitted_send() {
    let mut f = Fixture::new().await;
    f.submit();
    assert!(
        matches!(f.claim(), Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id == f.lease)
    );
    f.unclaimed();
}

#[tokio::test]
async fn engine_second_accept_cannot_replace_retained_receipt() {
    let mut f = Fixture::new().await;
    let mut other = Fixture::new().await;
    let handle = f.claim().unwrap();
    f.submit();
    let receipt = f.handback(&handle);
    f.engine
        .accept_send_handback(f.owner(), f.lease, receipt)
        .unwrap();
    let original = f
        .engine
        .owned_socket_control(f.owner(), f.lease)
        .unwrap()
        .physical
        .transmit_pending
        .as_ref()
        .unwrap()
        ._timing_receipt
        .as_ref()
        .unwrap()
        .clone();
    let other_handle = other.claim().unwrap();
    other.submit();
    let second = other.handback(&other_handle);
    assert!(
        matches!(f.engine.accept_send_handback(f.owner(), f.lease, second),
        Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id == f.lease)
    );
    let retained = f
        .engine
        .owned_socket_control(f.owner(), f.lease)
        .unwrap()
        .physical
        .transmit_pending
        .as_ref()
        .unwrap()
        ._timing_receipt
        .as_ref()
        .unwrap();
    assert!(Arc::ptr_eq(&original, retained));
    // A same-token second acceptance cannot be expressed through these
    // move-only APIs. This checks no replacement by another genuine receipt;
    // it is not isolated mutation coverage of _timing_receipt.is_some().
}

#[tokio::test]
async fn engine_enrolled_legacy_writer_refuses_before_and_after_temporal_receipt() {
    let mut f = Fixture::new().await;
    let before = f.engine.native_trace_fixture();
    let handle = f.claim().unwrap();
    f.submit();
    let error = f.legacy_confirmation().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("timed original send requires a versioned attempt writer")
    );
    assert_eq!(f.engine.native_trace_fixture(), before);
    assert!(!f.has_receipt());
    let receipt = f.handback(&handle);
    f.engine
        .accept_send_handback(f.owner(), f.lease, receipt)
        .unwrap();
    let error = f.legacy_confirmation().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("timed original send requires a versioned attempt writer")
    );
    assert_eq!(f.engine.native_trace_fixture(), before);
    assert!(f.has_receipt());
    assert!(
        !f.engine.stream_calls[&f.call]
            .native_entry
            .as_ref()
            .unwrap()
            .used
    );
    assert!(
        matches!(f.engine.finish(), Err(NetworkReplayError::UnresolvedStreamCall(id)) if id == f.call)
    );
}

#[tokio::test]
async fn engine_unenrolled_legacy_writer_keeps_exact_positive_output_neighbor() {
    let mut f = Fixture::new().await;
    f.unclaimed();
    f.submit();
    f.legacy_confirmation().unwrap();
    let trace = f.engine.native_trace_fixture();
    assert_eq!(trace.outputs.len(), 1);
    assert_eq!(trace.release_model.nodes().len(), 3);
    assert!(matches!(&trace.outputs[0].event,
        NetworkOutputKindV2::StreamBytes { stream_offset: 0, bytes } if bytes == SOURCE));
    trace.validate().unwrap();
    assert!(
        f.engine
            .owned_socket_control(f.owner(), f.lease)
            .unwrap()
            .physical
            .transmit_pending
            .is_none()
    );
    assert!(
        f.engine.stream_calls[&f.call]
            .native_entry
            .as_ref()
            .unwrap()
            .used
    );
}
