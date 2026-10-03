//! Component premises only: no stopped-task source or native reader is minted.
use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;

struct Fixture {
    runtime: NetworkRuntimeResources,
    root: Arc<ForegroundRoot>,
    _files: Arc<Mutex<crate::tool_local::FileMetadata>>,
    _memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
}

impl Fixture {
    fn new() -> Self {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (runtime, root, files, memory, _) = super::super::controlled_foreground_runtime(tid);
        Self {
            runtime,
            root,
            _files: files,
            _memory: memory,
        }
    }
    async fn prefix(&self) -> JoinedNativePrefix {
        self.runtime
            .join_foreground_prefix(self.root.clone())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn replay_source_interval_retention_excludes_foreground_until_owner_retires() {
    let f = Fixture::new();
    let prefix = f.prefix().await;
    let interval = f.runtime.reserve_replay_source_interval(&prefix).unwrap();
    assert_eq!(
        f.runtime
            .with_source_interval(&interval, || Ok(17))
            .unwrap(),
        17
    );
    let retained_backend = interval.keepalive();
    drop(interval);
    assert!(f.runtime.reserve_replay_source_interval(&prefix).is_err());
    assert!(f.runtime.validate_foreground_prefix(&prefix).is_err());
    let invoked = Cell::new(false);
    assert!(
        f.runtime
            .with_foreground_prefix(&prefix, |_| {
                invoked.set(true);
                Ok(())
            })
            .is_err()
    );
    assert!(!invoked.get());
    drop(retained_backend);
    f.runtime
        .with_foreground_prefix(&prefix, |_| Ok(()))
        .unwrap();
}

#[tokio::test]
async fn replay_source_interval_refuses_actual_worker_submission_before_closure() {
    let f = Fixture::new();
    let interval = f
        .runtime
        .reserve_replay_source_interval(&f.prefix().await)
        .unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let observed = ran.clone();
    let error = f
        .runtime
        .shared
        .start_native_worker(tokio::runtime::Handle::current(), move || {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "native submission attempted during retained source-read interval"
    );
    assert!(!ran.load(Ordering::SeqCst));
    assert!(
        f.runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .is_empty()
    );
    assert!(
        f.runtime
            .with_source_interval(&interval, || Ok(()))
            .is_err()
    );
    drop(interval);
    assert!(
        f.runtime
            .join_foreground_prefix(f.root.clone())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn replay_source_interval_terminal_cutoff_cannot_drop_backend_keepalive() {
    let f = Fixture::new();
    let interval = f
        .runtime
        .reserve_replay_source_interval(&f.prefix().await)
        .unwrap();
    let retained_backend = interval.keepalive();
    drop(interval);
    f.runtime.shared.native_workers.lock().unwrap().closed = true;
    assert!(!f.runtime.shared.native_workers_terminated());
    assert!(
        f.runtime
            .shared
            .finish_terminal_streams(std::time::Instant::now())
            .await
            .is_err()
    );
    drop(retained_backend);
    assert!(f.runtime.shared.native_workers_terminated());
    assert!(f.runtime.shared.native_workers.lock().unwrap().closed);
}

#[tokio::test]
async fn replay_source_interval_refuses_foreign_runtime_and_registered_root() {
    let f = Fixture::new();
    let other = Fixture::new();
    let prefix = f.prefix().await;
    assert!(
        other
            .runtime
            .reserve_replay_source_interval(&prefix)
            .is_err()
    );
    let foreign_prefix = f
        .runtime
        .join_foreground_prefix(other.root.clone())
        .await
        .unwrap();
    assert!(
        f.runtime
            .reserve_replay_source_interval(&foreign_prefix)
            .is_err()
    );
    let interval = f.runtime.reserve_replay_source_interval(&prefix).unwrap();
    let called = Cell::new(false);
    assert!(
        other
            .runtime
            .with_source_interval(&interval, || {
                called.set(true);
                Ok(())
            })
            .is_err()
    );
    assert!(!called.get());
    f.runtime
        .with_source_interval(&interval, || Ok(()))
        .unwrap();
}

#[tokio::test]
async fn replay_source_interval_changed_generation_or_root_never_publishes() {
    for revoke in [false, true] {
        let f = Fixture::new();
        let interval = f
            .runtime
            .reserve_replay_source_interval(&f.prefix().await)
            .unwrap();
        if revoke {
            f.runtime.revoke_foreground_lineage();
        } else {
            // Explicit component corruption, not a claimed native effect.
            f.runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .submission_generation += 1;
        }
        let called = Cell::new(false);
        assert!(
            f.runtime
                .with_source_interval(&interval, || {
                    called.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(!called.get());
    }
}
