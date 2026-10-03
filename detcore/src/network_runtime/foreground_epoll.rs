//! Narrow original ctl authority reuses actual task and native-worker custody.
use std::sync::Arc;

use super::*;
use crate::network_replay::NetworkStreamOwner;

impl NetworkRuntimeResources {
    /// The caller already holds the scheduler. Keep the physical census fixed
    /// through the synchronous grant/engine transaction; never await or submit
    /// native work from this callback. No borrowed value can escape the lock.
    pub(crate) fn with_shared_foreground_lineage<T>(
        &self,
        owner: NetworkStreamOwner,
        use_lineage: impl FnOnce(&SharedForegroundLineage<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let physical = self.shared.physical.lock().unwrap();
        let lineage = physical.shared_foreground_lineage(owner)?;
        use_lineage(&lineage)
    }

    pub(crate) fn reserve_replay_source_interval(
        &self,
        prefix: &JoinedNativePrefix,
    ) -> std::io::Result<NativeSourceInterval> {
        if !Arc::ptr_eq(&prefix.root, &self.foreground_root(prefix.root.owner())?) {
            return Err(std::io::Error::other("Replay source changed its registered physical root"));
        }
        let mut owned = self.shared.native_workers.lock().unwrap();
        if !prefix.shared.ptr_eq(&Arc::downgrade(&self.shared)) {
            return Err(std::io::Error::other("Replay source changed its original joined runtime"));
        }
        self.shared.reserve_source_interval(&mut owned, prefix.root.clone(), prefix.generation)
    }

    pub(crate) fn bind_foreground_metadata(
        &self,
        owner: NetworkStreamOwner,
        metadata: &Arc<Mutex<crate::tool_local::FileMetadata>>,
        memory: &Arc<Mutex<crate::memory::MemoryMetadata>>,
    ) -> std::io::Result<()> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .bind_foreground_metadata(owner, metadata, memory)
    }
    pub(crate) fn foreground_root(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Arc<ForegroundRoot>> {
        self.shared.physical.lock().unwrap().foreground_root(owner)
    }
    pub(crate) fn revoke_foreground_lineage(&self) {
        self.shared
            .physical
            .lock()
            .unwrap()
            .revoke_foreground_lineage();
    }
}

/// Positive completion of the exact retained execution prefix. A timing flag,
/// an empty task vector or a deserialized value cannot construct this proof.
#[derive(Debug, Clone)]
pub(crate) struct JoinedNativePrefix {
    pub(super) shared: std::sync::Weak<RuntimeShared>,
    pub(super) root: Arc<ForegroundRoot>,
    pub(super) generation: u64,
}
impl JoinedNativePrefix {
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.root
    }
    pub(crate) fn same_prefix(&self, other: &Self) -> bool {
        self.shared.ptr_eq(&other.shared)
            && Arc::ptr_eq(&self.root, &other.root)
            && self.generation == other.generation
    }
}
impl NetworkRuntimeResources {
    /// The caller first proves no semantic operation can still need a guest
    /// continuation. Calls::settled independently requires actual retained
    /// release before any row can disappear. Remaining handles are joined,
    /// not classified according to whether a host thread happens to be ready.
    pub(crate) async fn join_foreground_prefix(
        &self,
        root: Arc<ForegroundRoot>,
    ) -> std::io::Result<JoinedNativePrefix> {
        let (generation, workers) = {
            let owned = self.shared.native_workers.lock().unwrap();
            if owned.closed || owned.source_read_active() || !root.is_current(root.owner()) {
                return Err(std::io::Error::other(
                    "foreground prefix lost open root custody",
                ));
            }
            if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
                return Err(std::io::Error::other(error.clone()));
            }
            self.shared.native_streams.lock().unwrap().settled()?;
            (owned.submission_generation, owned.tasks.clone())
        };
        for worker in workers {
            self.shared.join_native_worker(&worker).await?;
        }
        let prefix = JoinedNativePrefix {
            shared: Arc::downgrade(&self.shared),
            root,
            generation,
        };
        self.validate_foreground_prefix(&prefix)?;
        Ok(prefix)
    }
    pub(crate) fn validate_foreground_prefix(
        &self,
        prefix: &JoinedNativePrefix,
    ) -> std::io::Result<()> {
        let owned = self.shared.native_workers.lock().unwrap();
        if !prefix.shared.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.closed
            || owned.source_read_active()
            || owned.submission_generation != prefix.generation
            || !prefix.root.is_current(prefix.root.owner())
        {
            return Err(std::io::Error::other(
                "foreground joined prefix changed runtime/submission/root",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        // This is secondary consistency after actual joins, never its issuer.
        self.shared.native_streams.lock().unwrap().settled()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::*;
    #[tokio::test]
    async fn foreground_prefix_joins_actual_worker_destructors_after_call_result_and_cancel() {
        let (runtime, _) = super::super::tests::fixture(91);
        let (root, _metadata, _memory, _) = super::super::controlled_foreground_root(61);
        let (released, release) = std::sync::mpsc::channel();
        let (started, observed) = tokio::sync::oneshot::channel();
        use std::io::Read;
        let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let mark = closed.clone();
        let (worker, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                let _ = started.send(());
                release.recv_timeout(Duration::from_secs(1)).unwrap();
                // A real alias must be released, not merely logically retired.
                drop(pin);
                mark.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();
        observed.await.unwrap();
        drop(reply);
        let mut first = Box::pin(runtime.join_foreground_prefix(root.clone()));
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(!closed.load(Ordering::Acquire));
        assert_eq!(
            peer.read(&mut [0u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(first); // cancellation keeps the exact run-owned JoinHandle
        assert!(worker.completion.try_lock().unwrap().task.is_some());
        released.send(()).unwrap();
        let joined =
            tokio::time::timeout(Duration::from_secs(1), runtime.join_foreground_prefix(root))
                .await
                .unwrap()
                .unwrap();
        assert!(closed.load(Ordering::Acquire));
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
        assert_eq!(worker.completion.lock().await.terminal, Some(Ok(())));
        runtime.validate_foreground_prefix(&joined).unwrap();
        let (_next, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        reply.await.unwrap().unwrap();
        assert!(
            runtime.validate_foreground_prefix(&joined).is_err(),
            "a host-finished later worker is not the joined prefix"
        );
    }
    #[tokio::test]
    async fn foreground_prefix_rejects_quarantine_failure_unknown_calls_and_closed_admission() {
        let (runtime, _) = super::super::tests::fixture(92);
        let (root, _metadata, _memory, _) = super::super::controlled_foreground_root(61);
        *runtime.shared.native_terminal_failure.lock().unwrap() =
            Some("retained uncertain helper".into());
        assert!(
            runtime
                .join_foreground_prefix(root.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("uncertain")
        );
        *runtime.shared.native_terminal_failure.lock().unwrap() = None;
        runtime.shared.native_workers.lock().unwrap().closed = true;
        assert!(runtime.join_foreground_prefix(root.clone()).await.is_err());
        runtime.shared.native_workers.lock().unwrap().closed = false;
        let call = crate::network_replay::NetworkStreamCallId::controlled_fixture(19);
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .capture_failed(root.owner(), call, libc::EBADF)
            .unwrap();
        assert!(runtime.join_foreground_prefix(root.clone()).await.is_err());
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .finish_failed_capture(root.owner(), call, libc::EBADF)
            .unwrap();
        runtime.join_foreground_prefix(root).await.unwrap();
    }
}

#[cfg(test)]
pub(crate) async fn controlled_joined_prefix(
    root: Arc<ForegroundRoot>,
) -> (NetworkRuntimeResources, JoinedNativePrefix) {
    let (runtime, _) = super::tests::fixture(93);
    let joined = runtime.join_foreground_prefix(root).await.unwrap();
    (runtime, joined)
}

/// Borrowed only while the real native-worker admission mutex is held. The
/// wrapper cannot escape that synchronous callback or certify any kernel effect.
pub(crate) struct ForegroundEntryAdmission<'a> {
    prefix: &'a JoinedNativePrefix,
    _admission: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ForegroundEntryAdmission<'_> {
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.prefix.root
    }
    pub(crate) fn is_original_prefix(&self, original: &JoinedNativePrefix) -> bool {
        self.prefix.shared.ptr_eq(&original.shared)
            && Arc::ptr_eq(&self.prefix.root, &original.root)
            && self.prefix.generation == original.generation
    }
}
impl NetworkRuntimeResources {
    pub(crate) fn with_foreground_prefix<T>(
        &self,
        prefix: &JoinedNativePrefix,
        stamp: impl FnOnce(&ForegroundEntryAdmission<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let owned = self.shared.native_workers.lock().unwrap();
        if !prefix.shared.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.closed
            || owned.copy_exclusion.is_some()
            || owned.source_read_active()
            || !owned.tasks.is_empty()
            || owned.submission_generation != prefix.generation
            || !prefix.root.is_current(prefix.root.owner())
        {
            return Err(std::io::Error::other(
                "receive entry changed the actually joined native prefix",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.settled()?;
        // Lock order: scheduler/metadata/engine -> worker admission -> Calls.
        // Retain both guards through the synchronous engine transaction; a
        // stamp or unsubmitted cleanup cannot race a new native Call admission.
        // The callback performs no await, syscall or worker submission.
        stamp(&ForegroundEntryAdmission {
            prefix,
            _admission: &owned,
            _calls: &calls,
        })
    }
}

#[cfg(test)]
mod unsubmitted_tests {
    #[tokio::test]
    async fn foreground_entry_borrow_refuses_any_actual_runtime_capture_row() {
        let (runtime, _) = super::super::tests::fixture(94);
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (root, _metadata, _memory, _) = super::super::controlled_foreground_root(raw);
        let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
        let call = crate::network_replay::NetworkStreamCallId::controlled_fixture(47);
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .capture_failed(root.owner(), call, libc::EBADF)
            .unwrap();
        let before = format!("{:?}", runtime.shared.native_streams.lock().unwrap());
        let invoked = std::cell::Cell::new(false);
        assert!(
            runtime
                .with_foreground_prefix(&prefix, |_| {
                    invoked.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(!invoked.get());
        assert_eq!(
            format!("{:?}", runtime.shared.native_streams.lock().unwrap()),
            before
        );
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .finish_failed_capture(root.owner(), call, libc::EBADF)
            .unwrap();
        runtime.with_foreground_prefix(&prefix, |_| Ok(())).unwrap();
    }
}
