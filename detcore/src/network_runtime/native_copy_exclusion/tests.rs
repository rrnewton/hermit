//! Real local PIDFD/owned-worker components with controlled provider geometry.
//! These controls do not qualify guest stores, native BPF or Record activation.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;
use crate::network_replay::NetworkReplayEngine;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamPhysicalEffect;

struct Fixture {
    runtime: NetworkRuntimeResources,
    root: Arc<ForegroundRoot>,
    _metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    _memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    engine: Arc<Mutex<NetworkReplayEngine>>,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    // Retain the exact physical effect and its original lifetime/drop order.
    _effect: NetworkStreamPhysicalEffect,
    observed: native_peer::Observation,
}
impl Fixture {
    async fn new(join: bool, confirm: bool, publication: bool) -> Self {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (runtime, root, metadata, memory, _) = super::super::controlled_foreground_runtime(tid);
        let owner = root.owner();
        let (mut engine, call, lease, effect) =
            NetworkReplayEngine::controlled_pending_helper_for_owner(owner);
        let (runtime, observed, _) = HelperCopyBinding::controlled_joined_peek_on(
            runtime,
            owner,
            call,
            lease,
            &mut engine,
            b"retained native source",
            5,
            join,
            false,
        )
        .await;
        let engine = Arc::new(Mutex::new(engine));
        if publication {
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .retain_capture_publication(
                    owner,
                    call,
                    NativeCaptureRecovery::new(
                        engine.clone(),
                        Arc::new(tokio::sync::Notify::new()),
                        |_| {},
                    ),
                )
                .unwrap();
        }
        if confirm {
            runtime
                .confirm_native_stream(owner, lease, &effect, &observed)
                .unwrap();
        }
        Self {
            runtime,
            root,
            _metadata: metadata,
            _memory: memory,
            engine,
            call,
            lease,
            _effect: effect,
            observed,
        }
    }
    fn completion(&self) -> HelperCopyCompletion {
        self.observed.helper_copy.clone().unwrap()
    }
    async fn exclude(&self) -> std::io::Result<NativeCopyExclusion> {
        tokio::time::timeout(
            Duration::from_secs(1),
            self.runtime
                .exclude_native_for_copy(self.root.clone(), self.completion()),
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn copy_exclusion_retains_exact_source_after_pending_retirement_and_is_one_use() {
    let f = Fixture::new(true, true, true).await;
    f.runtime
        .finish_native_stream_lease(f.root.owner(), f.lease)
        .unwrap();
    let proof = f.exclude().await.unwrap();
    let weak = Arc::downgrade(&proof.interval);
    f.runtime.validate_native_copy_exclusion(&proof).unwrap();
    assert!(
        matches!(&proof.interval.source, ForegroundStoreSource::Record(completion)
        if completion == &f.completion()),
        "the actual interval retains the exact Record completion; Replay is not equivalent"
    );
    assert!(f.exclude().await.is_err());
    let retained = proof.clone();
    drop(proof);
    assert!(
        weak.upgrade().is_some(),
        "callback drop cannot end admission exclusion"
    );
    f.runtime.validate_native_copy_exclusion(&retained).unwrap();
    f.runtime.finish_native_copy_exclusion(&retained).unwrap();
    assert!(f.runtime.validate_native_copy_exclusion(&retained).is_err());
    assert!(f.runtime.finish_native_copy_exclusion(&retained).is_err());
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .copy_exclusion
            .is_none()
    );
    // Native exclusion cannot discharge canonical copy ownership in the engine.
    assert!(
        f.engine
            .lock()
            .unwrap()
            .finish_stream_call_release(f.root.owner(), f.call)
            .is_err()
    );
    let (worker, reply) = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        f.runtime.shared.join_native_worker(&worker),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn copy_exclusion_refuses_actual_worker_submission_before_its_effect() {
    let f = Fixture::new(true, true, true).await;
    let proof = f.exclude().await.unwrap();
    let generation = f
        .runtime
        .shared
        .native_workers
        .lock()
        .unwrap()
        .submission_generation;
    let ran = Arc::new(AtomicBool::new(false));
    let child = ran.clone();
    assert!(
        f.runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                child.store(true, Ordering::Release);
                Ok(())
            })
            .is_err()
    );
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .submission_generation,
        generation
    );
    assert!(
        f.runtime
            .shared
            .native_terminal_failure
            .lock()
            .unwrap()
            .is_some()
    );
    assert!(f.runtime.validate_native_copy_exclusion(&proof).is_err());
    assert!(f.runtime.finish_native_copy_exclusion(&proof).is_err());
    assert!(proof.interval.active());
    // Explicit backend-ended component premise, never an ordinary-RPC release.
    f.runtime.shared.abandon_copy_exclusion_after_backend();
    assert!(!proof.interval.active());
    assert!(f.runtime.shared.native_workers.lock().unwrap().closed);
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .copy_exclusion
            .is_some()
    );
    assert!(
        f.runtime
            .shared
            .finish_native_workers(std::time::Instant::now() + Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn copy_exclusion_requires_actual_runtime_root_join_confirmation_publication_and_sole_call() {
    for variant in 0..6 {
        let f = Fixture::new(variant != 0, variant != 1, variant != 2).await;
        let mut root = f.root.clone();
        let mut keep_foreign = None;
        match variant {
            0..=2 => {}
            3 => {
                let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
                let other = super::super::controlled_foreground_runtime(tid);
                assert_eq!(other.1.owner(), f.root.owner());
                root = other.1.clone();
                keep_foreign = Some(other);
            }
            4 => f.runtime.revoke_foreground_lineage(),
            5 => {
                let other = NetworkStreamCallId::controlled_fixture(999);
                f.runtime
                    .shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .capture_failed(f.root.owner(), other, libc::EBADF)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = format!("{:?}", f.runtime.shared.native_workers.lock().unwrap());
        assert!(
            f.runtime
                .exclude_native_for_copy(root, f.completion())
                .await
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(
            format!("{:?}", f.runtime.shared.native_workers.lock().unwrap()),
            before
        );
        assert!(
            f.runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .copy_exclusion
                .is_none()
        );
        drop(keep_foreign);
    }
}

#[tokio::test]
async fn copy_exclusion_cancellation_keeps_actual_worker_until_its_alias_is_closed() {
    use std::io::Read;
    let f = Fixture::new(true, true, true).await;
    let (released, release) = std::sync::mpsc::channel();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let (worker, reply) = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), move || {
            started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(1)).unwrap();
            drop(pin);
            Ok(())
        })
        .unwrap();
    observed.await.unwrap();
    drop(reply);
    let mut pending = Box::pin(
        f.runtime
            .exclude_native_for_copy(f.root.clone(), f.completion()),
    );
    assert!(futures::poll!(pending.as_mut()).is_pending());
    assert_eq!(
        peer.read(&mut [0u8; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .copy_exclusion
            .is_none()
    );
    drop(pending);
    assert!(worker.completion.try_lock().unwrap().task.is_some());
    released.send(()).unwrap();
    let proof = f.exclude().await.unwrap();
    assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    assert_eq!(worker.completion.lock().await.terminal, Some(Ok(())));
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .is_empty()
    );
    f.runtime.finish_native_copy_exclusion(&proof).unwrap();
}

#[tokio::test]
async fn copy_exclusion_new_submission_during_actual_join_cannot_issue_a_stale_interval() {
    let f = Fixture::new(true, true, true).await;
    let (released, release) = std::sync::mpsc::channel();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (_worker, reply) = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), move || {
            started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(1)).unwrap();
            Ok(())
        })
        .unwrap();
    observed.await.unwrap();
    drop(reply);
    let mut pending = Box::pin(
        f.runtime
            .exclude_native_for_copy(f.root.clone(), f.completion()),
    );
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let (later, result) = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    released.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .copy_exclusion
            .is_none()
    );
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|w| Arc::ptr_eq(w, &later))
    );
    let proof = f.exclude().await.unwrap();
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .is_empty()
    );
    f.runtime.finish_native_copy_exclusion(&proof).unwrap();
}

#[tokio::test]
async fn copy_exclusion_failed_worker_refuses_and_retains_actual_failed_handle() {
    let f = Fixture::new(true, true, true).await;
    let (worker, reply) = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), || {
            Err::<(), _>(std::io::Error::other(
                "actual exclusion component worker failure",
            ))
        })
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(f.exclude().await.is_err());
    assert!(
        worker
            .completion
            .lock()
            .await
            .terminal
            .as_ref()
            .unwrap()
            .is_err()
    );
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|w| Arc::ptr_eq(w, &worker))
    );
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .copy_exclusion
            .is_none()
    );
}

#[tokio::test]
async fn copy_exclusion_revocation_and_backend_end_keep_tombstone_without_reopening_admission() {
    let f = Fixture::new(true, true, true).await;
    let proof = f.exclude().await.unwrap();
    f.runtime.revoke_foreground_lineage();
    assert!(f.runtime.validate_native_copy_exclusion(&proof).is_err());
    assert!(f.runtime.finish_native_copy_exclusion(&proof).is_err());
    assert!(proof.interval.active());
    // This is the private backend-ended cleanup seam; possible stores stay RED.
    f.runtime.shared.abandon_copy_exclusion_after_backend();
    let owned = f.runtime.shared.native_workers.lock().unwrap();
    assert!(owned.closed);
    assert!(Arc::ptr_eq(
        owned.copy_exclusion.as_ref().unwrap(),
        &proof.interval
    ));
    assert_eq!(
        proof.interval.phase.load(Ordering::Acquire),
        ABANDONED_AFTER_BACKEND
    );
    drop(owned);
    assert!(f.runtime.shared.native_workers_terminated());
    assert!(
        f.runtime
            .shared
            .native_terminal_failure
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("possible stores retained")
    );
    assert!(
        f.runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .is_err()
    );
    assert!(f.runtime.validate_native_copy_exclusion(&proof).is_err());
}
