//! One actual worker join, independent of foreground or later submissions.
use std::sync::Arc;
use std::sync::Weak;

use super::*;

/// Opaque custody of the worker whose actual JoinHandle returned successfully.
/// This is not a whole-prefix join, native-submission exclusion, guest-memory
/// permission, or semantic completion. It deliberately has no serde constructor.
#[derive(Debug, Clone)]
pub(crate) struct JoinedNativeWorkerReceipt {
    runtime: Weak<RuntimeShared>,
    worker: NativeWorkerHandle,
}
impl JoinedNativeWorkerReceipt {
    pub(super) fn validate_runtime(&self, runtime: &Arc<RuntimeShared>) -> std::io::Result<()> {
        let expected = Arc::downgrade(runtime);
        if !self.runtime.ptr_eq(&expected) || !self.worker.runtime.ptr_eq(&expected) {
            return Err(std::io::Error::other(
                "joined native worker changed its actual runtime owner",
            ));
        }
        Ok(())
    }
    pub(super) fn same(&self, other: &Self) -> bool {
        self.runtime.ptr_eq(&other.runtime) && Arc::ptr_eq(&self.worker, &other.worker)
    }
}

impl RuntimeShared {
    pub(super) async fn join_native_worker_receipt(
        self: &Arc<Self>,
        worker: &NativeWorkerHandle,
    ) -> std::io::Result<JoinedNativeWorkerReceipt> {
        if !worker.runtime.ptr_eq(&Arc::downgrade(self)) {
            return Err(std::io::Error::other(
                "native worker join requires its actual spawning owner",
            ));
        }
        // Cancellation retains the original JoinHandle inside NativeWorker.
        // Failure, a delivered reply, and is_finished do not issue this value.
        self.join_native_worker(worker).await?;
        Ok(JoinedNativeWorkerReceipt {
            runtime: Arc::downgrade(self),
            worker: worker.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn joined_worker_receipt_keeps_exact_worker_after_actual_join_and_later_submission() {
        let (runtime, _) = super::super::tests::fixture(95);
        let (worker, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(17))
            .unwrap();
        assert_eq!(reply.await.unwrap().unwrap(), 17);
        let proof = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.shared.join_native_worker_receipt(&worker),
        )
        .await
        .unwrap()
        .unwrap();
        let weak = Arc::downgrade(&worker);
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        assert!(worker.completion.lock().await.task.is_none());
        assert_eq!(worker.completion.lock().await.terminal, Some(Ok(())));
        proof.validate_runtime(&runtime.shared).unwrap();
        assert!(proof.same(&proof.clone()));
        drop(worker);
        assert!(weak.upgrade().is_some());
        // This proof intentionally certifies one join, never a submission cut.
        let (later, result) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        result.await.unwrap().unwrap();
        let later_proof = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.shared.join_native_worker_receipt(&later),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!proof.same(&later_proof));
        proof.validate_runtime(&runtime.shared).unwrap();
        drop(proof);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn joined_worker_receipt_rejects_foreign_owner_and_failed_or_deadline_poisoned_worker() {
        let (runtime, _) = super::super::tests::fixture(96);
        let (foreign, _) = super::super::tests::fixture(96); // equal numeric incarnation is insufficient
        let (worker, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        reply.await.unwrap().unwrap();
        assert!(
            foreign
                .shared
                .join_native_worker_receipt(&worker)
                .await
                .is_err()
        );
        assert!(worker.completion.lock().await.task.is_some());
        let proof = runtime
            .shared
            .join_native_worker_receipt(&worker)
            .await
            .unwrap();
        assert!(proof.validate_runtime(&foreign.shared).is_err());
        for fail in [true, false] {
            let (worker, reply) = runtime
                .shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    if fail {
                        Err(std::io::Error::other("retained actual worker failure"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap();
            assert_eq!(reply.await.unwrap().is_err(), fail);
            if !fail {
                *worker.deadline_failure.lock().unwrap() =
                    Some("retained original deadline".into());
            }
            assert!(
                runtime
                    .shared
                    .join_native_worker_receipt(&worker)
                    .await
                    .is_err()
            );
            assert!(worker.completion.lock().await.task.is_none());
            assert!(
                runtime
                    .shared
                    .native_workers
                    .lock()
                    .unwrap()
                    .tasks
                    .iter()
                    .any(|w| Arc::ptr_eq(w, &worker))
            );
        }
    }

    #[tokio::test]
    async fn joined_worker_receipt_cancel_keeps_actual_handle_until_worker_completion() {
        let (runtime, _) = super::super::tests::fixture(97);
        let (started, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (worker, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(1)).unwrap();
                Ok(())
            })
            .unwrap();
        observed.await.unwrap();
        let mut join = Box::pin(runtime.shared.join_native_worker_receipt(&worker));
        assert!(futures::poll!(join.as_mut()).is_pending());
        drop(join);
        drop(reply);
        assert!(worker.completion.try_lock().unwrap().task.is_some());
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .iter()
                .any(|w| Arc::ptr_eq(w, &worker))
        );
        release.send(()).unwrap();
        let proof = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.shared.join_native_worker_receipt(&worker),
        )
        .await
        .unwrap()
        .unwrap();
        proof.validate_runtime(&runtime.shared).unwrap();
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }
}
