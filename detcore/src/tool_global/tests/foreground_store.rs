//! Actual local issuer/turn/worker/canonical-source components. Provider geometry,
//! mmap observation and the access-check backend below are controlled premises.
//! The writer performs real process_vm_writev; safeptrace separately tests its
//! production Stopped implementation and target-PKRU check. No BPF/E2E claim.
mod fd_identity;
mod scalar_recvfrom;
mod raw_poll;
mod sendto_entry;
mod native_source_read;
mod shared_source;
mod shared_receive;
mod shared_record_receive;
mod shared_poll;
mod socket_error;

use std::cell::Cell;
use std::io::IoSlice;
use std::io::IoSliceMut;

use reverie::Errno;
use reverie::InjectedSyscallEvent as Event;
use reverie::Tool;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::RemoteIoVec;
use reverie::syscalls::Sysno;

use super::*;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamChunk;
use crate::network_replay::StoreOutcome;
use crate::network_runtime::HelperCopyBinding;

struct Pages {
    address: *mut u8,
}
impl Pages {
    fn new() -> Self {
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                8192,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        unsafe { std::ptr::write_bytes(address, 0xa5, 8192) };
        Self {
            address: address.cast(),
        }
    }
    fn at(&self, offset: usize) -> u64 {
        (unsafe { self.address.add(offset) }) as u64
    }
    fn bytes(&self, start: usize, count: usize) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(self.address.add(start), count).to_vec() }
    }
    fn protect_second(&self, protection: i32) {
        assert_eq!(
            unsafe { libc::mprotect(self.address.add(4096).cast(), 4096, protection) },
            0
        );
    }
}
impl Drop for Pages {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::munmap(self.address.cast(), 8192) }, 0);
    }
}

struct NativeMemory<'a> {
    tid: i32,
    checks: Cell<usize>,
    writes: usize,
    qualify: Result<(), Errno>,
    reported: Option<Result<usize, Errno>>,
    panic_after_write: bool,
    after: Option<Box<dyn FnOnce() + 'a>>,
}
impl NativeMemory<'_> {
    fn new(tid: i32) -> Self {
        Self {
            tid,
            checks: Cell::new(0),
            writes: 0,
            qualify: Ok(()),
            reported: None,
            panic_after_write: false,
            after: None,
        }
    }
}
impl MemoryAccess for NativeMemory<'_> {
    fn read_vectored(&self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
        panic!("foreground store used an ordinary read")
    }
    fn write_vectored(&mut self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
        panic!("foreground store used fallback write")
    }
    fn validate_native_user_key0_write_access(&self, expected: i32) -> Result<(), Errno> {
        self.checks.set(self.checks.get() + 1);
        if self.tid != expected {
            return Err(Errno::ESRCH);
        }
        self.qualify
    }
    fn write_native_user_vectored(
        &mut self,
        expected: i32,
        local: &[IoSlice],
        remote: &[RemoteIoVec],
    ) -> Result<usize, Errno> {
        assert_eq!(expected, self.tid);
        assert_eq!(self.checks.get(), 1);
        self.writes += 1;
        let remote: Vec<_> = remote
            .iter()
            .map(|span| libc::iovec {
                iov_base: span.address() as *mut libc::c_void,
                iov_len: span.length(),
            })
            .collect();
        let raw = Errno::result(unsafe {
            libc::process_vm_writev(
                expected,
                local.as_ptr().cast(),
                local.len() as libc::c_ulong,
                remote.as_ptr().cast(),
                remote.len() as libc::c_ulong,
                0,
            )
        })
        .map(|count| count as usize);
        if let Some(after) = self.after.take() {
            after();
        }
        assert!(
            !self.panic_after_write,
            "controlled backend panic after real write"
        );
        self.reported.unwrap_or(raw)
    }
}

struct Fixture {
    state: GlobalState,
    thread: crate::ThreadState<()>,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    pages: Pages,
    bytes: Vec<u8>,
    tid: Tid,
    _peer: Option<std::os::unix::net::UnixStream>,
}
impl Fixture {
    async fn new(length: usize, offset: usize) -> Self {
        Self::new_inner(length, offset, true).await
    }
    async fn new_inner(length: usize, offset: usize, select: bool) -> Self {
        Self::new_with_peer(length, offset, select, false).await
    }
    async fn new_with_peer(length: usize, offset: usize, select: bool, keep_peer: bool) -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let (runtime, root, metadata, memory, _) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        let owner = root.owner();
        let (config, mut state) = stream_rpc_state(true);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.detpid = Some(owner.thread);
        thread.file_metadata = metadata;
        thread.memory_metadata = memory;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(&root);
        let pages = Pages::new();
        let args = reverie::syscalls::SyscallArgs::new(
            0,
            8192,
            (libc::PROT_READ | libc::PROT_WRITE) as usize,
            (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            -1isize as usize,
            0,
        );
        {
            let mut memory = thread.memory_metadata.lock().unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(
                    &root,
                    Sysno::mmap,
                    args,
                    Event::Returned(pages.at(0) as i64),
                )
                .unwrap();
        }
        let (mut engine, call, probe, effect) =
            NetworkReplayEngine::controlled_foreground_store_pending(owner);
        let bytes = (0..length)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut peer = None;
        let (runtime, observed, _) = if keep_peer {
            HelperCopyBinding::controlled_joined_peek_retaining_peer(
                runtime,
                owner,
                call,
                probe,
                &mut engine,
                &bytes,
                5,
                true,
                false,
                0,
                Some(&mut peer),
            )
            .await
        } else {
            HelperCopyBinding::controlled_joined_peek_on(
                runtime,
                owner,
                call,
                probe,
                &mut engine,
                &bytes,
                5,
                true,
                false,
            )
            .await
        };
        runtime
            .preflight_native_stream(owner, probe, &effect, &observed)
            .unwrap();
        engine
            .confirm_retained_stream_physical(owner, probe, &observed)
            .unwrap();
        runtime
            .confirm_native_stream(owner, probe, &effect, &observed)
            .unwrap();
        let lease = if select {
            let NetworkStreamChunk::Reserved {
                lease,
                selection_len,
                ..
            } = engine
                .reserve_private_receive_span(owner, call, probe, length - offset, offset)
                .unwrap()
            else {
                panic!("complete private source should select bytes")
            };
            assert_eq!(selection_len, length - offset);
            runtime.finish_native_stream_lease(owner, probe).unwrap();
            lease
        } else {
            probe
        };
        *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
        runtime.retain_foreground_store_fixture_publication(
            owner,
            call,
            state.network_engine.as_ref().unwrap().clone(),
        );
        state.network_runtime = Some(runtime);
        Self {
            state,
            thread,
            root,
            call,
            lease,
            pages,
            bytes,
            tid,
            _peer: peer,
        }
    }
    fn semantic(&self) -> String {
        self.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .foreground_store_semantic_fixture_state()
    }
    fn assert_fenced(&self) {
        let mut engine = self.state.network_engine.as_ref().unwrap().lock().unwrap();
        let before = format!("{engine:?}");
        let owner = self.root.owner();
        assert!(engine.begin_record_drain(owner, self.lease).is_err());
        assert!(engine.finish_record_drain(owner, self.lease).is_err());
        for disposition in [
            crate::network_replay::NetworkStreamChunkDisposition::Consumed,
            crate::network_replay::NetworkStreamChunkDisposition::Peeked,
            crate::network_replay::NetworkStreamChunkDisposition::CopyFailed,
        ] {
            assert!(
                engine
                    .finish_stream_chunk(owner, self.lease, disposition)
                    .is_err()
            );
        }
        assert!(engine.begin_stream_call_release(owner, self.call).is_err());
        assert!(engine.finish_stream_call_release(owner, self.call).is_err());
        assert_eq!(format!("{engine:?}"), before);
        assert!(
            matches!(engine.finish(), Err(crate::network_replay::NetworkReplayError::UnresolvedStreamCall(call)) if call == self.call)
        );
    }
}

#[tokio::test]
async fn foreground_store_actual_local_issuer_copies_whole_selection_once_and_keeps_semantic_fence()
{
    for (length, offset) in [(8, 0), (128, 7), (512, 0)] {
        let f = Fixture::new(length, offset).await;
        let semantic = f.semantic();
        let before_turn = f.state.sched.lock().unwrap().turn;
        let permit = f
            .state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(4088))
            .await
            .unwrap();
        let retained = permit.retained_store();
        assert!(retained.raw_outcome().is_none());
        let mut memory = NativeMemory::new(f.tid.as_raw());
        let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
        assert_eq!(raw, StoreOutcome::Returned(Ok(length - offset)));
        let full =
            full.expect("exact full stores and actual exclusion end issue the opaque handoff");
        assert!(Arc::ptr_eq(full.store(), &retained));
        assert_eq!(retained.raw_outcome(), Some(raw));
        assert_eq!((memory.checks.get(), memory.writes), (1, 1));
        assert_eq!(f.pages.bytes(4088, length - offset), f.bytes[offset..]);
        assert_eq!(f.pages.bytes(4087, 1), [0xa5]);
        assert_eq!(f.pages.bytes(4088 + length - offset, 1), [0xa5]);
        assert!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .validate_native_copy_exclusion(retained.exclusion())
                .is_err()
        );
        assert!(
            f.state
                .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(0))
                .await
                .is_err()
        );
        assert_eq!(f.semantic(), semantic);
        assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
        f.assert_fenced();
    }
}

#[tokio::test]
async fn foreground_store_preparation_refuses_wrong_task_mm_metadata_and_destination_without_writes()
 {
    for variant in 0..7 {
        let mut f = Fixture::new(16, 0).await;
        let mut tid = f.tid;
        let mut destination = f.pages.at(128);
        match variant {
            0 => tid = Tid::from_raw(f.tid.as_raw() + 1),
            1 => f.thread.mm_id = f.thread.mm_id.for_exec(f.thread.dettid),
            2 => {
                let copied = f.thread.memory_metadata.lock().unwrap().clone();
                f.thread.memory_metadata = Arc::new(Mutex::new(copied));
            }
            3 => {
                let copied = f.thread.file_metadata.lock().unwrap().clone();
                f.thread.file_metadata = Arc::new(Mutex::new(copied));
            }
            4 => destination = f.pages.at(8191),
            5 => destination = u64::MAX - 4,
            6 => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&f.root.owner().thread);
            }
            _ => unreachable!(),
        }
        assert!(
            f.state
                .prepare_foreground_store(tid, &f.thread, f.lease, destination)
                .await
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .foreground_store_selection(f.root.owner(), f.lease)
                .is_ok()
        );
        f.assert_fenced();
    }
}

#[tokio::test]
async fn foreground_store_copy_rechecks_epoch_mapping_root_source_and_registered_mm_before_access()
{
    for variant in 0..10 {
        let f = Fixture::new(16, 0).await;
        let permit = f
            .state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128))
            .await
            .unwrap();
        let retained = permit.retained_store();
        match variant {
            0 => f
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(&f.root),
            1 => f
                .thread
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena(),
            2 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage(),
            3 => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&f.root.owner().thread);
            }
            4..=8 => f
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .change_foreground_store_fixture(f.call, f.lease, variant - 4),
            9 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .finish_native_copy_exclusion(retained.exclusion())
                .unwrap(),
            _ => unreachable!(),
        }
        let mut memory = NativeMemory::new(f.tid.as_raw());
        assert!(
            permit.copy(f.tid, &f.thread, &mut memory).is_err(),
            "variant {variant}"
        );
        assert_eq!((memory.checks.get(), memory.writes), (0, 0));
        assert!(retained.raw_outcome().is_none());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn foreground_store_access_refusal_precedes_possible_and_never_uses_ordinary_write() {
    for errno in [Errno::EACCES, Errno::EOPNOTSUPP, Errno::ESRCH] {
        let f = Fixture::new(16, 0).await;
        let permit = f
            .state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128))
            .await
            .unwrap();
        let retained = permit.retained_store();
        let mut memory = NativeMemory::new(f.tid.as_raw());
        memory.qualify = Err(errno);
        assert!(permit.copy(f.tid, &f.thread, &mut memory).is_err());
        assert_eq!((memory.checks.get(), memory.writes), (1, 0));
        assert!(retained.raw_outcome().is_none());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .validate_native_copy_exclusion(retained.exclusion())
            .unwrap();
        f.assert_fenced();
    }
}

#[tokio::test]
async fn foreground_store_actual_partial_and_fault_retain_exact_outcome_and_canaries_without_handoff()
 {
    for start in [4088, 4096] {
        let f = Fixture::new(16, 0).await;
        let semantic = f.semantic();
        let permit = f
            .state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(start))
            .await
            .unwrap();
        let retained = permit.retained_store();
        // Deliberately stale model premise; the real VMA refuses bytes in page2.
        f.pages.protect_second(libc::PROT_READ);
        let mut memory = NativeMemory::new(f.tid.as_raw());
        let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
        assert_eq!(
            raw,
            StoreOutcome::Returned(if start == 4088 {
                Ok(8)
            } else {
                Err(libc::EFAULT)
            })
        );
        assert!(full.is_none());
        assert_eq!(retained.raw_outcome(), Some(raw));
        assert_eq!(memory.writes, 1);
        assert_eq!(
            f.pages.bytes(4088, 8),
            if start == 4088 {
                f.bytes[..8].to_vec()
            } else {
                vec![0xa5; 8]
            }
        );
        assert_eq!(f.pages.bytes(4087, 1), [0xa5]);
        assert_eq!(f.pages.bytes(4096, 4096), vec![0xa5; 4096]);
        assert_eq!(f.semantic(), semantic);
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .validate_native_copy_exclusion(retained.exclusion())
            .unwrap();
        f.assert_fenced();
    }
}

#[tokio::test]
async fn foreground_store_malformed_error_zero_and_panic_after_writes_remain_retained_and_unresolved()
 {
    for variant in 0..4 {
        let f = Fixture::new(16, 0).await;
        let permit = f
            .state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128))
            .await
            .unwrap();
        let retained = permit.retained_store();
        let weak = Arc::downgrade(&retained);
        let mut memory = NativeMemory::new(f.tid.as_raw());
        let expected = match variant {
            0 => {
                memory.reported = Some(Ok(17));
                StoreOutcome::Returned(Ok(17))
            }
            1 => {
                memory.reported = Some(Err(Errno::EIO));
                StoreOutcome::Returned(Err(libc::EIO))
            }
            2 => {
                memory.reported = Some(Ok(0));
                StoreOutcome::Returned(Ok(0))
            }
            3 => {
                memory.panic_after_write = true;
                StoreOutcome::Panicked
            }
            _ => unreachable!(),
        };
        let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
        assert_eq!(raw, expected);
        assert!(full.is_none());
        assert_eq!(retained.raw_outcome(), Some(expected));
        assert_eq!(f.pages.bytes(128, 16), f.bytes);
        assert_eq!(memory.writes, 1);
        drop(retained);
        let retained = weak
            .upgrade()
            .expect("existing Call retains possible effects after local handle drop");
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .validate_native_copy_exclusion(retained.exclusion())
            .unwrap();
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(f.root.owner(), &f.root)
            .unwrap();
        let metadata = f.thread.memory_metadata.lock().unwrap();
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .perform_foreground_store(
                    &retained,
                    &grant,
                    &metadata,
                    f.state.network_runtime.as_ref().unwrap(),
                    &mut memory
                )
                .is_err()
        );
        assert_eq!((memory.checks.get(), memory.writes), (1, 1));
        drop(metadata);
        drop(scheduler);
        f.assert_fenced();
    }
}

#[tokio::test]
async fn foreground_store_postcheck_failure_keeps_real_full_outcome_without_releasing_exclusion() {
    let f = Fixture::new(16, 0).await;
    let permit = f
        .state
        .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128))
        .await
        .unwrap();
    let retained = permit.retained_store();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    memory.after = Some(Box::new(|| runtime.revoke_foreground_lineage()));
    assert!(permit.copy(f.tid, &f.thread, &mut memory).is_err());
    assert_eq!(retained.raw_outcome(), Some(StoreOutcome::Returned(Ok(16))));
    assert_eq!(f.pages.bytes(128, 16), f.bytes);
    assert!(
        f.state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(256))
            .await
            .is_err()
    );
    f.assert_fenced();
}

#[tokio::test]
async fn foreground_store_dropped_preparation_keeps_exact_owner_and_cannot_be_reissued() {
    let f = Fixture::new(16, 0).await;
    let permit = f
        .state
        .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128))
        .await
        .unwrap();
    let retained = permit.retained_store();
    let weak = Arc::downgrade(&retained);
    drop(permit);
    drop(retained);
    let retained = weak
        .upgrade()
        .expect("Call outlives canceled local preparation");
    assert!(retained.raw_outcome().is_none());
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .validate_native_copy_exclusion(retained.exclusion())
        .unwrap();
    assert!(
        f.state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(256))
            .await
            .is_err()
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    f.assert_fenced();
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_stream_final_wait(f.root.owner())
    );
    assert!(
        weak.upgrade().is_some(),
        "terminal bookkeeping erased unknown guest stores"
    );
}

#[tokio::test]
async fn foreground_store_foreign_exclusion_and_whole_selection_over_limit_cannot_prepare() {
    let first = Fixture::new(512, 0).await;
    let second = Fixture::new(512, 0).await;
    assert_eq!(first.root.owner(), second.root.owner());
    let foreign = second
        .state
        .prepare_foreground_store(
            second.tid,
            &second.thread,
            second.lease,
            second.pages.at(128),
        )
        .await
        .unwrap();
    let foreign_store = foreign.retained_store();
    let owner = first.root.owner();
    {
        let scheduler = first.state.sched.lock().unwrap();
        let epoch = scheduler
            .foreground_native_observation(owner, &first.root)
            .unwrap()
            .epoch();
        let memory = first.thread.memory_metadata.lock().unwrap();
        let span = memory
            .original_copy_span(owner, first.pages.at(128), 512)
            .unwrap();
        let mut engine = first.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_ne!(
            engine
                .private_receive_completion(owner, first.lease)
                .unwrap(),
            foreign_store.completion()
        );
        let before = format!("{engine:?}");
        assert!(
            engine
                .prepare_foreground_store(
                    owner,
                    first.lease,
                    (first.root.clone(), &memory),
                    span,
                    foreign_store.exclusion().clone(),
                    epoch
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    assert_eq!(first.pages.bytes(0, 8192), vec![0xa5; 8192]);
    // Explicit over-bound complete-selection premise. A 512-byte view of a
    // larger delivery must never qualify that complete delivery for stores.
    first
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .change_foreground_store_fixture(first.call, first.lease, 5);
    assert!(
        first
            .state
            .prepare_foreground_store(first.tid, &first.thread, first.lease, first.pages.at(128))
            .await
            .is_err()
    );
    assert_eq!(first.pages.bytes(0, 8192), vec![0xa5; 8192]);
    first.assert_fenced();
    second.assert_fenced();
}

#[tokio::test]
async fn foreground_store_actual_worker_join_cannot_carry_preparation_across_scheduler_epoch() {
    let f = Fixture::new(16, 0).await;
    let release = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .controlled_foreground_store_worker()
        .await;
    let mut preparing =
        Box::pin(
            f.state
                .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(128)),
        );
    assert!(futures::poll!(preparing.as_mut()).is_pending());
    f.state
        .sched
        .lock()
        .unwrap()
        .controlled_foreground_store_grant(&f.root);
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), preparing)
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .foreground_store_selection(f.root.owner(), f.lease)
            .is_ok()
    );
    // Exclusion was actually installed after the join, so refusal cannot silently
    // release it to allow a second attempt under a different scheduler grant.
    assert!(
        f.state
            .prepare_foreground_store(f.tid, &f.thread, f.lease, f.pages.at(256))
            .await
            .is_err()
    );
    f.assert_fenced();
}

async fn private_drain_fixture(
    length: usize,
) -> (Fixture, crate::network_replay::FullStoreCompletion) {
    let mut f = Fixture::new_inner(length, 0, false).await;
    let runtime = f.state.network_runtime.as_ref().unwrap();
    assert_eq!(
        runtime.private_receive_lease_fixture_state(f.root.owner(), f.call),
        (1, false, false)
    );
    let old_probe = f.lease;
    let permit = f
        .state
        .prepare_private_receive_store(
            f.tid,
            &f.thread,
            f.call,
            old_probe,
            length,
            f.pages.at(4088),
        )
        .await
        .unwrap();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(length)));
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    let full = full.unwrap();
    f.lease = full.store().lease();
    assert_ne!(f.lease, old_probe);
    assert_eq!(
        runtime.private_receive_lease_fixture_state(f.root.owner(), f.call),
        (1, true, false)
    );
    assert!(
        f.state
            .prepare_private_receive_store(
                f.tid,
                &f.thread,
                f.call,
                old_probe,
                length,
                f.pages.at(0)
            )
            .await
            .is_err()
    );
    (f, full)
}

#[tokio::test]
async fn private_drain_actual_adapter_store_held_socket_worker_and_join_advance_only_physical_history()
 {
    for length in [1, 8, 512] {
        let (f, full) = private_drain_fixture(length).await;
        let owner = f.root.owner();
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = engine.lock().unwrap().private_drain_fixture_state(f.call);
        let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
        assert_eq!(
            effect,
            crate::network_replay::NetworkStreamPhysicalEffect::Drain { maximum: length }
        );
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let observed = runtime
            .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
            .await
            .unwrap();
        assert_eq!(observed.raw_return, length as i64);
        assert_eq!(observed.bytes, f.bytes);
        runtime
            .preflight_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
        engine
            .lock()
            .unwrap()
            .confirm_retained_stream_physical(owner, f.lease, &observed)
            .unwrap();
        runtime
            .confirm_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
        let after = engine.lock().unwrap().private_drain_fixture_state(f.call);
        assert_eq!(after.0, before.0);
        assert_eq!(after.1, (length as u64, 1));
        assert_eq!(after.2, (true, true, true));
        assert_eq!(after.3, before.3 + 1);
        assert_eq!(f.pages.bytes(4088, length), f.bytes);
        assert_eq!(f.pages.bytes(4087, 1), [0xa5]);
        assert_eq!(f.pages.bytes(4088 + length, 1), [0xa5]);
        assert!(engine.lock().unwrap().begin_private_drain(&full).is_err());
        assert!(
            runtime
                .controlled_private_drain(full, engine.clone(), 5, "none", true)
                .await
                .is_err()
        );
        assert_eq!(
            engine.lock().unwrap().private_drain_fixture_state(f.call),
            after
        );
        f.assert_fenced();
    }
}

#[tokio::test]
async fn private_drain_real_short_zero_wrong_bytes_wrong_cut_copy4_and_missing_join_remain_retained()
 {
    for (mutation, version, join, expected_count, expected_cut, expected_joined) in [
        ("short", 5, true, 4, (4, 1), true),
        ("zero", 5, true, 0, (0, 0), true),
        ("bytes", 5, true, 8, (8, 1), true),
        ("order", 5, true, 8, (0, 0), false),
        ("before", 5, true, 8, (0, 0), false),
        ("none", 4, true, 8, (0, 0), false),
        ("none", 5, false, 8, (0, 0), false),
    ] {
        let (f, full) = private_drain_fixture(8).await;
        let owner = f.root.owner();
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = engine.lock().unwrap().private_drain_fixture_state(f.call);
        let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let observed = runtime
            .controlled_private_drain(full.clone(), engine.clone(), version, mutation, join)
            .await
            .unwrap();
        assert_eq!(
            observed.raw_return, expected_count,
            "{mutation}/{version}/{join}"
        );
        runtime
            .preflight_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
        assert!(
            engine
                .lock()
                .unwrap()
                .confirm_retained_stream_physical(owner, f.lease, &observed)
                .is_err()
        );
        let after = engine.lock().unwrap().private_drain_fixture_state(f.call);
        assert_eq!(after.0, before.0);
        assert_eq!(after.1, expected_cut);
        assert_eq!(after.2, (true, expected_joined, false));
        assert_eq!(
            runtime.private_receive_lease_fixture_state(owner, f.call),
            (1, true, true)
        );
        assert!(engine.lock().unwrap().begin_private_drain(&full).is_err());
        assert!(
            engine
                .lock()
                .unwrap()
                .confirm_retained_stream_physical(owner, f.lease, &observed)
                .is_err()
        );
        assert_eq!(
            engine.lock().unwrap().private_drain_fixture_state(f.call),
            after
        );
        assert_eq!(f.pages.bytes(4088, 8), f.bytes);
        f.assert_fenced();
        assert!(engine.lock().unwrap().native_stream_final_wait(owner));
        assert!(
            engine
                .lock()
                .unwrap()
                .terminal_stream_admission(owner, f.call)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            engine.lock().unwrap().private_drain_fixture_state(f.call),
            after
        );
    }
}

#[tokio::test]
async fn private_drain_changed_source_selection_owner_and_cloned_handoff_cannot_submit() {
    for variant in 0..5 {
        let (f, full) = private_drain_fixture(8).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .change_foreground_store_fixture(f.call, f.lease, variant);
        let before = format!("{:?}", engine.lock().unwrap());
        assert!(
            engine.lock().unwrap().begin_private_drain(&full).is_err(),
            "variant {variant}"
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert_eq!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_lease_fixture_state(f.root.owner(), f.call),
            (1, true, false)
        );
    }
    let (a, first) = private_drain_fixture(8).await;
    let (b, foreign) = private_drain_fixture(8).await;
    let engine = a.state.network_engine.as_ref().unwrap();
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        engine
            .lock()
            .unwrap()
            .begin_private_drain(&foreign)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    engine.lock().unwrap().begin_private_drain(&first).unwrap();
    let latched = format!("{:?}", engine.lock().unwrap());
    assert!(
        engine
            .lock()
            .unwrap()
            .begin_private_drain(&first.clone())
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), latched);
    drop(first);
    drop(foreign);
    drop(b);
    a.assert_fenced();
}

#[tokio::test]
async fn private_drain_real_selection_adapter_refuses_wrong_call_source_max_and_memory_before_transfer()
 {
    for variant in 0..6 {
        let f = Fixture::new_inner(8, 0, false).await;
        let mut call = f.call;
        let mut probe = f.lease;
        let mut maximum = 8;
        let mut address = f.pages.at(0);
        match variant {
            0 => call = NetworkStreamCallId::controlled_fixture(999),
            1 => probe = serde_json::from_value(serde_json::json!(999)).unwrap(),
            2 => maximum = 0,
            3 => maximum = 513,
            4 => address = f.pages.at(8191),
            5 => {
                f.state.registered_exec_mms.lock().unwrap().clear();
            }
            _ => unreachable!(),
        }
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        assert!(
            f.state
                .prepare_private_receive_store(f.tid, &f.thread, call, probe, maximum, address)
                .await
                .is_err()
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert_eq!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_lease_fixture_state(f.root.owner(), f.call),
            (1, false, false)
        );
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn private_drain_actual_terminal_close_preserves_source_store_and_unknown_or_known_successor()
{
    for known_result in [false, true] {
        let (f, full) = private_drain_fixture(8).await;
        let owner = f.root.owner();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let engine = f.state.network_engine.as_ref().unwrap().clone();
        let source = Arc::downgrade(full.store().completion().binding());
        let store = Arc::downgrade(full.store());
        let fd = runtime.private_receive_original_fixture_fd(owner, f.call);
        let audit = no_store_duplicate_actual_original(&f);
        engine.lock().unwrap().begin_private_drain(&full).unwrap();
        let observed = if known_result {
            Some(
                runtime
                    .controlled_private_drain(full.clone(), engine.clone(), 5, "none", false)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let successor = observed
            .as_ref()
            .map(|o| Arc::downgrade(o.helper_copy.as_ref().unwrap().binding()));
        assert!(engine.lock().unwrap().native_stream_final_wait(owner));
        let mut outside = runtime.controller_disposal_owner();
        // Explicit controlled final-wait premise; this executes the actual
        // bounded physical cleanup, not a guest/kernel final-wait event.
        assert!(unsafe { outside.finish_native_controller_tasks().await }.is_err());
        fd_identity::check_original_socket_released(fd, &audit).unwrap();
        drop(audit);
        assert!(format!("{:?}", engine.lock().unwrap()).contains("TerminalPinReleased"));
        assert!(matches!(engine.lock().unwrap().finish(),
            Err(crate::network_replay::NetworkReplayError::UnresolvedStreamCall(call)) if call == f.call));
        drop(full);
        drop(observed);
        drop(outside);
        drop(f);
        assert!(source.upgrade().is_some());
        assert!(store.upgrade().is_some());
        if let Some(successor) = &successor {
            assert!(successor.upgrade().is_some());
        }
        drop(engine);
        assert!(source.upgrade().is_none());
        assert!(store.upgrade().is_none());
        if let Some(successor) = successor {
            assert!(successor.upgrade().is_none());
        }
    }
}

#[tokio::test]
async fn private_drain_actual_eagain_retains_negative_raw_result_and_refuses_retry_or_semantic_completion()
 {
    let mut f = Fixture::new_with_peer(8, 0, false, true).await;
    let owner = f.root.owner();
    let permit = f
        .state
        .prepare_private_receive_store(f.tid, &f.thread, f.call, f.lease, 8, f.pages.at(4088))
        .await
        .unwrap();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(8)));
    let full = full.unwrap();
    f.lease = full.store().lease();
    let engine = f.state.network_engine.as_ref().unwrap();
    let before = engine.lock().unwrap().private_drain_fixture_state(f.call);
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    // The fixture deliberately empties the actual queue while retaining its
    // live peer. The following bounded recv returns real EAGAIN. Geometry is a
    // controlled premise for result retention, not native BPF source proof.
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "zero", true)
        .await
        .unwrap();
    assert_eq!(observed.raw_return, -1);
    assert_eq!(observed.errno, Some(libc::EAGAIN));
    assert_eq!(
        observed.confirmation,
        crate::network_replay::NetworkStreamPhysicalResult::Errno(libc::EAGAIN)
    );
    assert!(observed.bytes.is_empty());
    let completion = observed.helper_copy.as_ref().unwrap();
    assert_eq!(
        completion.capture().manifest.returned,
        -i64::from(libc::EAGAIN)
    );
    assert!(completion.attempts().is_empty());
    completion.joined_worker().unwrap();
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    assert!(
        engine
            .lock()
            .unwrap()
            .confirm_retained_stream_physical(owner, f.lease, &observed)
            .is_err()
    );
    let after = engine.lock().unwrap().private_drain_fixture_state(f.call);
    assert_eq!(after.0, before.0);
    assert_eq!(after.1, before.1);
    assert_eq!(after.2, (true, true, false));
    assert_eq!(after.3, before.3);
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, f.call),
        (1, true, true)
    );
    assert!(engine.lock().unwrap().begin_private_drain(&full).is_err());
    assert!(
        runtime
            .controlled_private_drain(full, engine.clone(), 5, "none", true)
            .await
            .is_err()
    );
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    assert_eq!(
        engine.lock().unwrap().private_drain_fixture_state(f.call),
        after
    );
    assert_eq!(f.pages.bytes(4088, 8), f.bytes);
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    f.assert_fenced();
}

async fn private_publication_fixture(
    length: usize,
    confirm: bool,
) -> (Fixture, crate::network_replay::FullStoreCompletion) {
    let (f, full) = private_drain_fixture(length).await;
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
        .await
        .unwrap();
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, f.lease, &observed)
        .unwrap();
    if confirm {
        runtime
            .confirm_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
    }
    (f, full)
}

#[tokio::test]
async fn private_publication_actual_confirmed_pending_prepares_all_private_multi_fragment_and_partial_front()
 {
    for ends in [vec![], vec![2, 5], vec![2, 8], vec![2, 10]] {
        let (f, full) = private_publication_fixture(8, true).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let mut start = 0;
        let mut fragments = Vec::new();
        for end in &ends {
            fragments.push((start..*end).map(|n| n as u8).collect());
            start = *end;
        }
        engine
            .lock()
            .unwrap()
            .publish_private_prefix_fixture(f.call, &fragments);
        let before = f.semantic();
        let runtime_before = runtime.private_publication_runtime_fixture_state();
        let prepared = f
            .state
            .prepare_foreground_receive_publication(&full)
            .unwrap();
        let summary = prepared.private_publication_fixture_summary();
        let published = ends.last().copied().unwrap_or(0);
        assert_eq!(summary.1, published.min(8)..8);
        assert_eq!(
            (summary.2, summary.3, summary.4),
            (8, published.max(8) as u64, published > 8)
        );
        let expected = match ends.as_slice() {
            [] => vec![],
            [2, 5] => vec![(2, true), (3, true)],
            [2, 8] => vec![(2, true), (6, true)],
            [2, 10] => vec![(2, true), (6, false)],
            _ => unreachable!(),
        };
        assert_eq!(summary.0, expected);
        assert!(Arc::ptr_eq(
            &prepared,
            &engine
                .lock()
                .unwrap()
                .private_publication_fixture(f.call)
                .unwrap()
        ));
        assert_eq!(f.semantic(), before);
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            runtime_before
        );
        assert_eq!(f.pages.bytes(4088, 8), f.bytes);
        let weak = Arc::downgrade(&prepared);
        drop(prepared);
        assert!(weak.upgrade().is_some());
        let attached = format!("{:?}", engine.lock().unwrap());
        assert!(
            f.state
                .prepare_foreground_receive_publication(&full.clone())
                .is_err()
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), attached);
        f.assert_fenced();
        drop(full);
        drop(f);
        assert!(weak.upgrade().is_none());
    }
}

#[tokio::test]
async fn private_publication_requires_actual_engine_drain_and_positive_runtime_confirmation() {
    let (f, full) = private_drain_fixture(8).await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        f.state
            .prepare_foreground_receive_publication(&full)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
        .await
        .unwrap();
    runtime
        .preflight_native_stream(f.root.owner(), f.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(f.root.owner(), f.lease, &observed)
        .unwrap();
    let before = format!("{:?}", engine.lock().unwrap());
    let pending = runtime.private_publication_runtime_fixture_state();
    assert!(
        f.state
            .prepare_foreground_receive_publication(&full)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(runtime.private_publication_runtime_fixture_state(), pending);
    runtime
        .confirm_native_stream(f.root.owner(), f.lease, &effect, &observed)
        .unwrap();
    f.state
        .prepare_foreground_receive_publication(&full)
        .unwrap();
    f.assert_fenced();
}

#[tokio::test]
async fn private_publication_missing_foreign_and_changed_runtime_custody_never_prepares() {
    for variant in 0..10 {
        let (f, full) = private_publication_fixture(8, true).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let foreign = if variant == 9 {
            Some(private_publication_fixture(8, true).await)
        } else {
            None
        };
        if variant < 8 {
            runtime.change_private_publication_runtime_fixture(f.root.owner(), f.call, variant);
        } else if variant == 8 {
            runtime
                .finish_native_stream_lease(f.root.owner(), f.lease)
                .unwrap();
        }
        let before = format!("{:?}", engine.lock().unwrap());
        let pending = runtime.private_publication_runtime_fixture_state();
        let result = if let Some((other, _)) = &foreign {
            other
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .prepare_private_receive_publication(&mut engine.lock().unwrap(), &full)
        } else {
            runtime.prepare_private_receive_publication(&mut engine.lock().unwrap(), &full)
        };
        assert!(result.is_err(), "variant {variant}");
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert_eq!(runtime.private_publication_runtime_fixture_state(), pending);
        assert!(
            engine
                .lock()
                .unwrap()
                .private_publication_fixture(f.call)
                .is_none()
        );
        f.assert_fenced();
    }
}

#[tokio::test]
async fn private_publication_changed_local_root_mm_epoch_and_strict_mode_never_attaches() {
    for variant in 0..4 {
        let (mut f, full) = private_publication_fixture(8, true).await;
        match variant {
            0 => {
                f.state.registered_exec_mms.lock().unwrap().clear();
            }
            1 => f
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(&f.root),
            2 => f.state.cfg.sequentialize_threads = false,
            3 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage(),
            _ => unreachable!(),
        }
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        assert!(
            f.state
                .prepare_foreground_receive_publication(&full)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    }
}

#[tokio::test]
async fn private_publication_changed_semantic_physical_store_epoch_and_join_refuse_atomically() {
    for variant in 0..8 {
        let (f, full) = private_publication_fixture(8, true).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .change_private_publication_fixture(f.call, variant);
        let before = format!("{:?}", engine.lock().unwrap());
        assert!(
            f.state
                .prepare_foreground_receive_publication(&full)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert!(
            engine
                .lock()
                .unwrap()
                .private_publication_fixture(f.call)
                .is_none()
        );
    }
}

#[tokio::test]
async fn private_publication_later_published_fragment_mismatch_leaves_full_retained_obligations() {
    let (f, full) = private_publication_fixture(8, true).await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .publish_private_prefix_fixture(f.call, &[vec![0, 1], vec![99, 3, 4]]);
    let before = format!("{:?}", engine.lock().unwrap());
    let pending = runtime.private_publication_runtime_fixture_state();
    assert!(
        f.state
            .prepare_foreground_receive_publication(&full)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(runtime.private_publication_runtime_fixture_state(), pending);
    assert_eq!(f.pages.bytes(4088, 8), f.bytes);
    f.assert_fenced();
}

impl Fixture {
    async fn new_native(length: usize) -> Self {
        let offset = 0;
        let select = false;
        let keep_peer = true;
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let (runtime, root, metadata, memory, _) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        let owner = root.owner();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let mut state = GlobalState::initialize(&config, false);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.detpid = Some(owner.thread);
        thread.file_metadata = metadata;
        thread.memory_metadata = memory;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(&root);
        let pages = Pages::new();
        let args = reverie::syscalls::SyscallArgs::new(
            0,
            8192,
            (libc::PROT_READ | libc::PROT_WRITE) as usize,
            (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            -1isize as usize,
            0,
        );
        {
            let mut memory = thread.memory_metadata.lock().unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(
                    &root,
                    Sysno::mmap,
                    args,
                    Event::Returned(pages.at(0) as i64),
                )
                .unwrap();
        }
        let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
        let (mut engine, call, probe, effect) = {
            let scheduler = state.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .unwrap();
            runtime
                .with_foreground_prefix(&prefix, |admission| {
                    Ok(NetworkReplayEngine::controlled_native_receive_pending(
                        owner, admission, &grant,
                    ))
                })
                .unwrap()
        };
        let bytes = (0..length)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut peer = None;
        let (runtime, observed, _) = if keep_peer {
            HelperCopyBinding::controlled_joined_peek_retaining_peer(
                runtime,
                owner,
                call,
                probe,
                &mut engine,
                &bytes,
                5,
                true,
                false,
                0,
                Some(&mut peer),
            )
            .await
        } else {
            HelperCopyBinding::controlled_joined_peek_on(
                runtime,
                owner,
                call,
                probe,
                &mut engine,
                &bytes,
                5,
                true,
                false,
            )
            .await
        };
        runtime
            .preflight_native_stream(owner, probe, &effect, &observed)
            .unwrap();
        engine
            .confirm_retained_stream_physical(owner, probe, &observed)
            .unwrap();
        runtime
            .confirm_native_stream(owner, probe, &effect, &observed)
            .unwrap();
        let lease = if select {
            let NetworkStreamChunk::Reserved {
                lease,
                selection_len,
                ..
            } = engine
                .reserve_private_receive_span(owner, call, probe, length - offset, offset)
                .unwrap()
            else {
                panic!("complete private source should select bytes")
            };
            assert_eq!(selection_len, length - offset);
            runtime.finish_native_stream_lease(owner, probe).unwrap();
            lease
        } else {
            probe
        };
        *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
        runtime.retain_foreground_store_fixture_publication(
            owner,
            call,
            state.network_engine.as_ref().unwrap().clone(),
        );
        state.network_runtime = Some(runtime);
        Self {
            state,
            thread,
            root,
            call,
            lease,
            pages,
            bytes,
            tid,
            _peer: peer,
        }
    }
}

#[tokio::test]
async fn native_publication_commits_exact_store_drain_and_semantic_lease_once() {
    let mut f = Fixture::new_native(8).await;
    let owner = f.root.owner();
    let permit = f
        .state
        .prepare_private_receive_store(f.tid, &f.thread, f.call, f.lease, 8, f.pages.at(4088))
        .await
        .unwrap();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(8)));
    let full = full.unwrap();
    f.lease = full.store().lease();
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    assert_eq!(f.pages.bytes(4088, 8), f.bytes);
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
        .await
        .unwrap();
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, f.lease, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    let prepared = f
        .state
        .prepare_foreground_receive_publication(&full)
        .unwrap();
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, f.call),
        (1, true, false),
        "one actual positively confirmed delivery lease precedes publication"
    );
    let count = f
        .state
        .publish_foreground_native_receive(&prepared)
        .unwrap();
    assert_eq!(count, 8);
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, f.call),
        (0, false, false),
        "publication retired the exact actual lease and its predecessor/Pending custody"
    );
    assert!(
        f.state
            .publish_foreground_native_receive(&prepared)
            .is_err()
    );
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, f.call),
        (0, false, false)
    );
    let trace = engine.lock().unwrap().native_trace_fixture();
    trace.validate().unwrap();
    let row = trace.native_receive_observations.last().unwrap();
    assert_eq!((row.stream_offset, row.length), (0, 8));
    assert!(
        row.fragments.iter().all(|x| x.disposition
            == detcore_model::network_trace::NetworkNativeCopyDispositionV4::Consume)
    );
    assert_eq!(row.fragments.first().unwrap().physical_before, 0);
    assert_eq!(row.fragments.last().unwrap().physical_after, 8);
    assert_eq!(
        trace.inputs.last().unwrap().event,
        detcore_model::network_trace::NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: f.bytes.clone()
        }
    );
    assert_eq!(f.pages.bytes(0, 4088), vec![0xa5; 4088]);
    assert_eq!(f.pages.bytes(4096, 4096), vec![0xa5; 4096]);
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, f.call)
        .unwrap();
    runtime.release_native_stream(owner, f.call).await.unwrap();
    runtime.finish_native_stream_release(owner, f.call).unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, f.call)
        .unwrap();
    assert_eq!(memory.writes, 1);
    // Offline shared-engine replay consumes the same published bytes with a
    // different read partition; this is not yet a Guest/backend witness.
    let mut replay = NetworkReplayEngine::replay_native_receive(trace.clone()).unwrap();
    let channel = trace.channels[0].id;
    let ofd = crate::types::OpenFileId::new_socket(owner.thread, 9);
    let profile = &trace.fresh_stream_profiles[0];
    replay
        .register_stream_socket(
            ofd,
            profile.key,
            crate::network_replay::NetworkStreamNamespace {
                device: 7,
                inode: 11,
            },
            None,
        )
        .unwrap();
    replay.bind(ofd, channel).unwrap();
    let now = trace.inputs.last().unwrap().release.not_before_global_time;
    replay.release_eligible(now).unwrap();
    assert_eq!(
        replay.take_connection_outcome(ofd).unwrap(),
        Some(crate::network_replay::ConnectionOutcome::Connect(
            detcore_model::network_trace::NetworkConnectionResultV2::Connected
        ))
    );
    replay.release_eligible(now).unwrap();
    let mut bytes = Vec::new();
    for n in [3, 5] {
        let crate::network_replay::StreamReceiveOutcome::Bytes(part) =
            replay.receive_stream(ofd, n, false).unwrap()
        else {
            panic!("published byte prefix")
        };
        bytes.extend(part);
    }
    assert_eq!(bytes, f.bytes);
    replay.finish().unwrap();
}

impl Fixture {
    async fn new_replay(length: usize) -> (Self, crate::network_replay::FullStoreCompletion) {
        let mut f = Self::new_native(length).await;
        let owner = f.root.owner();
        let permit = f
            .state
            .prepare_private_receive_store(
                f.tid,
                &f.thread,
                f.call,
                f.lease,
                length,
                f.pages.at(128),
            )
            .await
            .unwrap();
        let mut memory = NativeMemory::new(f.tid.as_raw());
        let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
        assert_eq!(raw, StoreOutcome::Returned(Ok(length)));
        let full = full.unwrap();
        f.lease = full.store().lease();
        let engine = f.state.network_engine.as_ref().unwrap();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
        let observed = runtime
            .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
            .await
            .unwrap();
        runtime
            .preflight_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
        engine
            .lock()
            .unwrap()
            .confirm_retained_stream_physical(owner, f.lease, &observed)
            .unwrap();
        runtime
            .confirm_native_stream(owner, f.lease, &effect, &observed)
            .unwrap();
        let prepared = f
            .state
            .prepare_foreground_receive_publication(&full)
            .unwrap();
        assert_eq!(
            f.state
                .publish_foreground_native_receive(&prepared)
                .unwrap(),
            length
        );
        let trace = engine.lock().unwrap().native_trace_fixture();
        trace.validate().unwrap();
        engine
            .lock()
            .unwrap()
            .begin_stream_call_release(owner, f.call)
            .unwrap();
        runtime.release_native_stream(owner, f.call).await.unwrap();
        runtime.finish_native_stream_release(owner, f.call).unwrap();
        engine
            .lock()
            .unwrap()
            .finish_stream_call_release(owner, f.call)
            .unwrap();
        runtime
            .join_foreground_prefix(f.root.clone())
            .await
            .unwrap();
        let (replay, call) = NetworkReplayEngine::controlled_replay_store_call(owner, trace);
        *engine.lock().unwrap() = replay;
        f.call = call;
        unsafe { std::ptr::write_bytes(f.pages.address, 0xa5, 8192) };
        (f, full)
    }
}

#[tokio::test]
async fn replay_foreground_store_uses_one_native_write_then_exact_queue_commit() {
    let (f, _) = Fixture::new_replay(8).await;
    let owner = f.root.owner();
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &f.thread, f.call, 8, f.pages.at(4088), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let retained = permit.retained_store();
    assert!(retained.record_completion().is_err());
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(8)));
    let full = full.unwrap();
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    assert_eq!(f.pages.bytes(4088, 8), f.bytes);
    assert_eq!(f.pages.bytes(0, 4088), vec![0xa5; 4088]);
    assert_eq!(f.pages.bytes(4096, 4096), vec![0xa5; 4096]);
    let engine = f.state.network_engine.as_ref().unwrap();
    assert!(engine.lock().unwrap().begin_private_drain(&full).is_err());
    assert!(
        f.state
            .prepare_foreground_receive_publication(&full)
            .is_err()
    );
    assert_eq!(f.state.commit_replay_receive_store(&full).unwrap(), 8);
    assert!(f.state.commit_replay_receive_store(&full).is_err());
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, f.call)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, f.call)
        .unwrap();
    engine.lock().unwrap().finish().unwrap();
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert_eq!(memory.writes, 1);
}

#[tokio::test]
async fn replay_foreground_store_rejects_record_authority_without_store_or_queue_effects() {
    let (f, record_full) = Fixture::new_replay(8).await;
    let before = f.semantic();
    assert!(f.state.commit_replay_receive_store(&record_full).is_err());
    assert_eq!(f.semantic(), before);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &f.thread, f.call, 8, f.pages.at(128), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let retained = permit.retained_store();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    memory.qualify = Err(Errno::EACCES);
    assert!(permit.copy(f.tid, &f.thread, &mut memory).is_err());
    assert_eq!((memory.checks.get(), memory.writes), (1, 0));
    assert!(retained.raw_outcome().is_none());
    assert_eq!(f.semantic(), before);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert!(
        f.state
            .prepare_replay_receive(f.tid, &f.thread, f.call, 8, f.pages.at(128), false)
            .await
            .is_err()
    );
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .validate_native_copy_exclusion(retained.exclusion())
        .unwrap();
}

#[tokio::test]
async fn replay_foreground_partial_and_fault_keep_source_queue_and_raw_outcome() {
    for start in [4088, 4096] {
        let (f, _) = Fixture::new_replay(16).await;
        let before = f.semantic();
        let crate::network_replay::ReceiveSelection::Bytes(permit) = f
            .state
            .prepare_replay_receive(f.tid, &f.thread, f.call, 16, f.pages.at(start), false)
            .await
            .unwrap()
        else {
            panic!("positive replay control requires Bytes")
        };
        let retained = permit.retained_store();
        f.pages.protect_second(libc::PROT_READ);
        let mut memory = NativeMemory::new(f.tid.as_raw());
        let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
        assert_eq!(
            raw,
            StoreOutcome::Returned(if start == 4088 {
                Ok(8)
            } else {
                Err(libc::EFAULT)
            })
        );
        assert!(full.is_none());
        assert_eq!(retained.raw_outcome(), Some(raw));
        assert_eq!((memory.checks.get(), memory.writes), (1, 1));
        assert_eq!(f.semantic(), before);
        assert_eq!(f.pages.bytes(0, 4088), vec![0xa5; 4088]);
        assert_eq!(
            f.pages.bytes(4088, 8),
            if start == 4088 {
                f.bytes[..8].to_vec()
            } else {
                vec![0xa5; 8]
            }
        );
        assert_eq!(f.pages.bytes(4096, 4096), vec![0xa5; 4096]);
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .begin_stream_call_release(f.root.owner(), f.call)
                .is_err()
        );
        assert!(
            f.state
                .prepare_replay_receive(f.tid, &f.thread, f.call, 16, f.pages.at(128), false)
                .await
                .is_err()
        );
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .validate_native_copy_exclusion(retained.exclusion())
            .unwrap();
    }
}

#[tokio::test]
async fn replay_foreground_wrong_task_refuses_before_access_or_store() {
    let (f, _) = Fixture::new_replay(8).await;
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &f.thread, f.call, 8, f.pages.at(128), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let retained = permit.retained_store();
    let before = f.semantic();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    assert!(
        permit
            .copy(Tid::from_raw(f.tid.as_raw() + 1), &f.thread, &mut memory)
            .is_err()
    );
    assert_eq!((memory.checks.get(), memory.writes), (0, 0));
    assert!(retained.raw_outcome().is_none());
    assert_eq!(f.semantic(), before);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn replay_foreground_commit_rejects_intervening_worker_even_after_later_join() {
    let (f, _) = Fixture::new_replay(8).await;
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &f.thread, f.call, 8, f.pages.at(128), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(8)));
    let full = full.unwrap();
    let before = f.semantic();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let release = runtime.controlled_foreground_store_worker().await;
    assert!(f.state.commit_replay_receive_store(&full).is_err());
    assert_eq!(f.semantic(), before);
    assert_eq!(memory.writes, 1);
    release.send(()).unwrap();
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert!(
        f.state.commit_replay_receive_store(&full).is_err(),
        "a fresh join cannot replace the original ended-store prefix"
    );
    assert_eq!(f.semantic(), before);
    assert_eq!(f.pages.bytes(128, 8), f.bytes);
    assert_eq!(f.pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(f.pages.bytes(136, 8192 - 136), vec![0xa5; 8192 - 136]);
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .begin_stream_call_release(f.root.owner(), f.call)
            .is_err()
    );
}

// Additive test-only successor. Initial root/capability/model and VMA observation
// remain controlled premises; the actual FD publication, foreground scheduler
// grant, ordinary reader RPC, local issuer and native store/commit execute.
struct ReplayIssuerFixture {
    config: Config,
    state: GlobalState,
    thread: crate::ThreadState<()>,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    pages: Pages,
    tid: Tid,
    binding: crate::types::FdSlotBinding,
    read: crate::network_replay::NetworkFdReadAdmission,
}
impl ReplayIssuerFixture {
    async fn new() -> Self {
        Self::new_mode(false).await
    }
    async fn new_record() -> Self {
        Self::new_mode(true).await
    }
    async fn new_mode(record: bool) -> Self {
        Self::new_trace(
            record,
            NetworkReplayEngine::controlled_replay_two_row_trace(),
            true,
        )
        .await
        .0
    }
    async fn new_trace(
        record: bool,
        trace: detcore_model::network_trace::NetworkTraceV4,
        release_bytes: bool,
    ) -> (Self, Resources) {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let (runtime, root, metadata, memory, claim) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        let owner = root.owner();
        let mut bytes = Vec::new();
        detcore_model::network_trace::NetworkTrace::V4(trace.clone())
            .write_framed(&mut bytes)
            .unwrap();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: trace.epoch,
            network_trace_input: (!record).then_some(bytes),
            ..Config::default()
        };
        config.network_trace.policy = if record {
            NetworkPolicy::Record
        } else {
            NetworkPolicy::Replay
        };
        let mut state = GlobalState::initialize(&config, false);
        state.network_runtime = Some(runtime);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.detpid = Some(owner.thread);
        thread.file_metadata = metadata;
        thread.memory_metadata = memory;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(&root);
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            if record {
                *engine = NetworkReplayEngine::record_native_receive(config.epoch);
            }
            engine.fd_table_fixture_enable();
            engine
                .register_initial_census(root.association(), &claim, owner.thread)
                .unwrap();
        }
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        let pages = Pages::new();
        let args = reverie::syscalls::SyscallArgs::new(
            0,
            8192,
            (libc::PROT_READ | libc::PROT_WRITE) as usize,
            (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            -1isize as usize,
            0,
        );
        {
            let mut memory = thread.memory_metadata.lock().unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(
                    &root,
                    Sysno::mmap,
                    args,
                    Event::Returned(pages.at(0) as i64),
                )
                .unwrap();
        }
        let mut guest = owned_read_guest(&config, &state, thread);
        let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Socket).await;
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            if record {
                engine.controlled_connect_socket_premise(binding.open_file);
                // Controlled original-installed identity/profile premise. This
                // supplies no native capture, successful Connect or entry.
                guest.thread.file_metadata.lock().unwrap().bind_native_installation(binding,
                    crate::network_runtime::original_installation::FileIdentity::controlled_fixture(3,109)).unwrap();
                let definition = &trace.channels[0];
                engine
                    .ensure_channel(
                        binding.open_file,
                        NetworkChannelBinding {
                            transport: definition.transport,
                            role: definition.role,
                            peer_address: definition.peer_address.clone(),
                            requested_local_constraint: None,
                            observed_local_address: None,
                            accepted_from: None,
                            selected_channel: None,
                        },
                    )
                    .unwrap();
            } else {
                engine
                    .register_stream_socket(
                        binding.open_file,
                        trace.fresh_stream_profiles[0].key,
                        crate::network_replay::NetworkStreamNamespace {
                            device: 7,
                            inode: 11,
                        },
                        None,
                    )
                    .unwrap();
                engine
                    .bind(binding.open_file, trace.channels[0].id)
                    .unwrap();
                let now = trace.epoch_global_time().unwrap();
                engine.release_eligible(now).unwrap();
                assert_eq!(
                    engine.take_connection_outcome(binding.open_file).unwrap(),
                    Some(crate::network_replay::ConnectionOutcome::Connect(
                        detcore_model::network_trace::NetworkConnectionResultV2::Connected
                    ))
                );
                if release_bytes {
                    engine.release_eligible(now).unwrap();
                }
            }
        }
        grant_owned_read_foreground(&state, &mut guest).await;
        // The existing helper asserted this exact published request equals the
        // actual finish_selected_turn result and resumed Normal. Preserve that
        // committed turn for real scheduler accounting in the wait controls.
        let committed_turn = guest
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|request| {
                if let GlobalRequest::RequestResources(resources, _) = request {
                    Some(resources.clone())
                } else {
                    None
                }
            })
            .expect("the fixture actually completed its initial resource request");
        let reply = super::super::network_request(
            &mut guest,
            NetworkRequest::BeginOrdinaryFdRead {
                files: binding.slot.files,
                fd: binding.slot.fd,
            },
        )
        .await
        .unwrap();
        let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
        else {
            panic!("real ordinary reader was not admitted")
        };
        let read = *read;
        assert_eq!(read.binding, Some(binding));
        assert!(read.control.is_some());
        let thread = guest.thread;
        (
            Self {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            committed_turn,
        )
    }
}

#[tokio::test]
async fn replay_entry_uses_real_reader_and_transfers_table_control_to_one_logical_call() {
    let f = ReplayIssuerFixture::new().await;
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 1, 1, 0)
    );
    let before_runtime = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let call = f
        .state
        .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
        .unwrap();
    assert!(!call.physical_pin_required);
    assert_eq!(call.open_file, f.binding.open_file);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    let after = format!("{:?}", engine.lock().unwrap());
    assert!(
        f.state
            .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), after);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, call.id)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, call.id)
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
}

#[tokio::test]
async fn replay_entry_refusals_preserve_the_actual_reader_table_and_call_population() {
    for variant in 0..9 {
        let mut f = ReplayIssuerFixture::new().await;
        let owner = f.root.owner();
        let mut tid = f.tid;
        let mut destination = f.pages.at(128);
        let mut maximum = 8;
        let mut read = f.read.clone();
        match variant {
            0 => tid = Tid::from_raw(f.tid.as_raw() + 1),
            1 => f.thread.mm_id = f.thread.mm_id.for_exec(owner.thread),
            2 => {
                let copied = f.thread.file_metadata.lock().unwrap().clone();
                f.thread.file_metadata = Arc::new(Mutex::new(copied));
            }
            3 => {
                f.thread.memory_metadata =
                    Arc::new(Mutex::new(crate::memory::MemoryMetadata::default()))
            }
            4 => destination = u64::MAX - 3,
            5 => maximum = 513,
            6 => read.binding = None,
            7 => read.control = None,
            8 => f.state.cfg.sequentialize_threads = false,
            _ => unreachable!(),
        }
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let runtime_before = runtime.private_publication_runtime_fixture_state();
        assert!(
            f.state
                .begin_replay_receive_call(tid, &f.thread, read, destination, maximum)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(
            format!("{:?}", engine.lock().unwrap()),
            before,
            "variant {variant}"
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 1, 1, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            runtime_before
        );
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        engine
            .lock()
            .unwrap()
            .finish_fd_read(owner, f.read)
            .unwrap();
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
    }
}

#[tokio::test]
async fn replay_real_issuer_store_and_commit_cross_rows_with_exact_producer_frontier() {
    let f = ReplayIssuerFixture::new().await;
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    let call = f
        .state
        .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 5)
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
    );
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &f.thread, call.id, 5, f.pages.at(128), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let store = permit.retained_store();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(5)));
    let full = full.unwrap();
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    assert_eq!(f.pages.bytes(128, 5), b"abcde");
    assert_eq!(f.pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(f.pages.bytes(133, 8192 - 133), vec![0xa5; 8192 - 133]);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
    );
    assert_eq!(f.state.commit_replay_receive_store(&full).unwrap(), 5);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (5, vec![b"fgh".to_vec()], vec![0, 1, 2])
    );
    let after = format!("{:?}", engine.lock().unwrap());
    assert!(f.state.commit_replay_receive_store(&full).is_err());
    assert_eq!(format!("{:?}", engine.lock().unwrap()), after);
    assert!(
        engine
            .lock()
            .unwrap()
            .finish_stream_chunk(
                owner,
                store.lease(),
                crate::network_replay::NetworkStreamChunkDisposition::Consumed
            )
            .is_err()
    );
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, call.id)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, call.id)
        .unwrap();
    assert!(
        engine.lock().unwrap().finish().is_err(),
        "the suffix remains required"
    );
    let mut guest = owned_read_guest(&f.config, &f.state, f.thread);
    let reply = super::super::network_request(
        &mut guest,
        NetworkRequest::BeginOrdinaryFdRead {
            files: f.binding.slot.files,
            fd: f.binding.slot.fd,
        },
    )
    .await
    .unwrap();
    let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
    else {
        panic!("second actual reader")
    };
    let read = *read;
    let second = f
        .state
        .begin_replay_receive_call(f.tid, &guest.thread, read, f.pages.at(256), 3)
        .unwrap();
    let crate::network_replay::ReceiveSelection::Bytes(permit) = f
        .state
        .prepare_replay_receive(f.tid, &guest.thread, second.id, 3, f.pages.at(256), false)
        .await
        .unwrap()
    else {
        panic!("positive replay control requires Bytes")
    };
    let mut second_memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit
        .copy(f.tid, &guest.thread, &mut second_memory)
        .unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(3)));
    assert_eq!(
        f.state.commit_replay_receive_store(&full.unwrap()).unwrap(),
        3
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (8, vec![], vec![0, 1, 2, 3])
    );
    assert_eq!(f.pages.bytes(256, 3), b"fgh");
    assert_eq!(f.pages.bytes(259, 8192 - 259), vec![0xa5; 8192 - 259]);
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, second.id)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, second.id)
        .unwrap();
    // Both receive Calls ended, but the actual FD-table owner is still live.
    // Complete the existing owner-retirement RPC before requiring final replay.
    assert!(matches!(
        engine.lock().unwrap().finish(),
        Err(NetworkReplayError::FdPublicationProtocol(message))
            if message == "network OFD lifetime protocol: OutstandingOwners"
    ));
    let reply = f
        .state
        .receive_rpc(
            f.tid,
            (
                DetTime::new(&f.config),
                owner.mm,
                GlobalRequest::NetworkOwnerGone,
            ),
        )
        .await;
    assert_eq!(reply, (None, GlobalResponse::NetworkOwnerGone));
    engine.lock().unwrap().finish().unwrap();
    assert_eq!((second_memory.checks.get(), second_memory.writes), (1, 1));
}

#[tokio::test]
async fn private_record_entry_stamp_failure_releases_actual_unsubmitted_reader_owners() {
    let f = ReplayIssuerFixture::new_record().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    // The real stamp's time predicate refuses after the actual reader-to-Call
    // transfer. No native capture worker or fabricated errno is introduced.
    let mut older = f.config.clone();
    older.epoch -= chrono::Duration::seconds(1);
    *f.state.global_time.lock().unwrap() = crate::types::GlobalTime::new(&older);
    let error = f
        .state
        .begin_private_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
        .await
        .unwrap_err();
    let NetworkRpcError::Internal(message) = error.primary().clone() else {
        panic!("expected entry protocol refusal")
    };
    assert_eq!(
        message,
        NetworkReplayError::from(
            detcore_model::network_trace::NetworkTraceValidationError::ReleaseBeforeEpoch
        )
        .to_string()
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    let after = format!("{:?}", engine.lock().unwrap());
    assert!(
        f.state
            .begin_private_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
            .await
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), after);
}

#[tokio::test]
async fn unsubmitted_entry_cleanup_requires_spent_same_call_and_original_runtime_prefix() {
    let f = ReplayIssuerFixture::new_record().await;
    let other = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let call = engine
        .lock()
        .unwrap()
        .begin_native_stream_call_from_read(owner, f.read.clone())
        .unwrap();
    let foreign_call = other
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .begin_native_stream_call_from_read(other.root.owner(), other.read.clone())
        .unwrap();
    assert_eq!(
        call.id, foreign_call.id,
        "same serialized number is not Call identity"
    );
    let mut attempt = engine
        .lock()
        .unwrap()
        .begin_native_entry_stamp(owner, call.id)
        .unwrap();
    let retained = attempt.retain_unsubmitted_recovery(&prefix).unwrap();
    assert!(
        attempt.retain_unsubmitted_recovery(&prefix).is_err(),
        "original prefix is one use"
    );
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        {
            let mut e = engine.lock().unwrap();
            runtime.with_foreground_prefix(&prefix, |proof| {
                e.cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
        }
        .is_err(),
        "live attempt cannot be cleaned up"
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    drop(attempt);
    let foreign_engine = other.state.network_engine.as_ref().unwrap();
    let foreign_before = format!("{:?}", foreign_engine.lock().unwrap());
    assert!(
        {
            let mut e = foreign_engine.lock().unwrap();
            runtime.with_foreground_prefix(&prefix, |proof| {
                e.cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
        }
        .is_err()
    );
    assert_eq!(
        format!("{:?}", foreign_engine.lock().unwrap()),
        foreign_before
    );
    let other_prefix = other
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .join_foreground_prefix(other.root.clone())
        .await
        .unwrap();
    assert!(
        {
            let mut e = engine.lock().unwrap();
            other
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .with_foreground_prefix(&other_prefix, |proof| {
                    e.cancel_unsubmitted_native_entry(&retained, proof)
                        .map_err(std::io::Error::other)
                })
        }
        .is_err()
    );
    {
        let mut e = engine.lock().unwrap();
        runtime.with_foreground_prefix(&prefix, |proof| {
            e.cancel_unsubmitted_native_entry(&retained, proof)
                .map_err(std::io::Error::other)
        })
    }
    .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    let after = format!("{:?}", engine.lock().unwrap());
    assert!(
        {
            let mut e = engine.lock().unwrap();
            runtime.with_foreground_prefix(&prefix, |proof| {
                e.cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
        }
        .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), after);
}

#[tokio::test]
async fn unsubmitted_entry_cleanup_refuses_new_worker_and_cannot_replace_its_join() {
    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let call = engine
        .lock()
        .unwrap()
        .begin_native_stream_call_from_read(owner, f.read.clone())
        .unwrap();
    let mut attempt = engine
        .lock()
        .unwrap()
        .begin_native_entry_stamp(owner, call.id)
        .unwrap();
    let retained = attempt.retain_unsubmitted_recovery(&prefix).unwrap();
    drop(attempt);
    let worker = runtime.controlled_foreground_store_worker().await;
    worker.send(()).unwrap();
    let newer = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        {
            let mut e = engine.lock().unwrap();
            runtime.with_foreground_prefix(&prefix, |proof| {
                e.cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
        }
        .is_err()
    );
    assert!(
        {
            let mut e = engine.lock().unwrap();
            runtime.with_foreground_prefix(&newer, |proof| {
                e.cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
        }
        .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 1, 1, 1)
    );
    assert!(
        engine
            .lock()
            .unwrap()
            .begin_native_entry_stamp(owner, call.id)
            .is_err()
    );
}

#[tokio::test]
async fn unsubmitted_entry_cleanup_refuses_a_successfully_issued_entry() {
    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let scheduler = f.state.sched.lock().unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    let call = engine
        .begin_native_stream_call_from_read(owner, f.read.clone())
        .unwrap();
    let mut attempt = engine.begin_native_entry_stamp(owner, call.id).unwrap();
    let retained = attempt.retain_unsubmitted_recovery(&prefix).unwrap();
    let now = f.state.global_time.lock().unwrap().as_nanos();
    runtime
        .with_foreground_prefix(&prefix, |proof| {
            engine
                .stamp_native_receive_entry(attempt, proof, &grant, now)
                .map_err(std::io::Error::other)
        })
        .unwrap();
    let before = format!("{engine:?}");
    assert!(
        runtime
            .with_foreground_prefix(&prefix, |proof| engine
                .cancel_unsubmitted_native_entry(&retained, proof)
                .map_err(std::io::Error::other))
            .is_err()
    );
    assert_eq!(format!("{engine:?}"), before);
    assert_eq!(
        engine.native_capture_fixture_counts(f.binding.open_file),
        (1, 1, 1, 1)
    );
    assert!(engine.begin_native_entry_stamp(owner, call.id).is_err());
}

#[tokio::test]
async fn unsubmitted_entry_cleanup_rejects_first_binding_to_foreign_root_metadata() {
    let f = ReplayIssuerFixture::new_record().await;
    let other = ReplayIssuerFixture::new_record().await;
    assert_eq!(f.root.owner(), other.root.owner());
    assert!(!Arc::ptr_eq(
        &f.thread.file_metadata,
        &other.thread.file_metadata
    ));
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let foreign = other.state.network_runtime.as_ref().unwrap();
    let prefix = foreign
        .join_foreground_prefix(other.root.clone())
        .await
        .unwrap();
    let (_call, retained, before) = {
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        let call = engine
            .begin_native_stream_call_from_read(owner, f.read.clone())
            .unwrap();
        let mut attempt = engine.begin_native_entry_stamp(owner, call.id).unwrap();
        let retained = attempt.retain_unsubmitted_recovery(&prefix).unwrap();
        drop(attempt);
        let before = format!("{engine:?}");
        let error = foreign
            .with_foreground_prefix(&prefix, |proof| {
                engine
                    .cancel_unsubmitted_native_entry(&retained, proof)
                    .map_err(std::io::Error::other)
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed actual engine/root metadata custody")
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine.native_capture_fixture_counts(f.binding.open_file),
            (1, 1, 1, 1)
        );
        assert!(engine.begin_native_entry_stamp(owner, call.id).is_err());
        (call, retained, before)
    };
    let own_prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        runtime
            .with_foreground_prefix(&own_prefix, |proof| engine
                .cancel_unsubmitted_native_entry(&retained, proof)
                .map_err(std::io::Error::other))
            .is_err()
    );
    assert_eq!(
        format!("{engine:?}"),
        before,
        "later correct join cannot replace the attempted prefix"
    );
}

/// Real private Store/Drain completion leaves the exact runtime pin and engine
/// Call ready for the production release RPC. Provider geometry remains the
/// fixture's explicit controlled premise; no E2E authority is claimed here.
async fn native_release_rpc_fixture() -> Fixture {
    let mut f = Fixture::new_native(8).await;
    let owner = f.root.owner();
    let permit = f
        .state
        .prepare_private_receive_store(f.tid, &f.thread, f.call, f.lease, 8, f.pages.at(128))
        .await
        .unwrap();
    let mut memory = NativeMemory::new(f.tid.as_raw());
    let (raw, full) = permit.copy(f.tid, &f.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(8)));
    assert_eq!(f.pages.bytes(128, 8), f.bytes);
    let full = full.unwrap();
    f.lease = full.store().lease();
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
        .await
        .unwrap();
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, f.lease, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    let prepared = f
        .state
        .prepare_foreground_receive_publication(&full)
        .unwrap();
    assert_eq!(
        f.state
            .publish_foreground_native_receive(&prepared)
            .unwrap(),
        8
    );
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, f.call),
        (0, false, false)
    );
    engine
        .lock()
        .unwrap()
        .native_trace_fixture()
        .validate()
        .unwrap();
    f
}

async fn native_release_rpc(f: &Fixture) -> GlobalResponse {
    let owner = f.root.owner();
    let (clock, response) = f
        .state
        .receive_rpc(
            f.tid,
            (
                DetTime::new(&f.state.cfg),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeReleaseStreamCall { call: f.call }),
            ),
        )
        .await;
    assert_eq!(clock, None);
    response
}

fn native_release_peer_eof(f: &mut Fixture) {
    use std::io::Read;
    let peer = f._peer.as_mut().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    assert_eq!(
        peer.read(&mut [0u8; 1]).unwrap(),
        0,
        "the runtime's actual original pin was closed"
    );
}

#[tokio::test]
async fn native_retirement_rpc_real_pin_close_keeps_error_and_acknowledges_runtime() {
    let mut f = native_release_rpc_fixture().await;
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    let (ofd, channel, before) = {
        let mut e = engine.lock().unwrap();
        let ofd = e.stream_call_open_file(owner, f.call).unwrap();
        let channel = e.channel_for(ofd).unwrap();
        assert_eq!(
            e.retire_open_file(ofd),
            None,
            "actual Call still owns the final pin"
        );
        e.controlled_native_retirement_ledger_defect();
        (ofd, channel, e.native_trace_fixture())
    };
    let response = native_release_rpc(&f).await;
    let GlobalResponse::Network(Err(NetworkRpcError::Internal(primary))) = response else {
        panic!("invalid journal must retain its internal failure: {response:?}")
    };
    assert!(primary.contains("NativeRetirement"));
    assert!(primary.contains("NonCanonicalNode"));
    {
        let e = engine.lock().unwrap();
        assert!(
            matches!(e.stream_call_open_file(owner, f.call), Err(NetworkReplayError::UnknownStreamCall(id)) if id == f.call)
        );
        assert_eq!(e.channel_for(ofd), Some(channel));
        assert_eq!(e.native_trace_fixture(), before);
        assert_eq!(e.finish().unwrap_err().to_string(), primary);
    }
    native_release_peer_eof(&mut f);
    let runtime = f.state.network_runtime.as_ref().unwrap();
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .expect("known release must retire runtime custody despite semantic journal failure");
    assert!(
        runtime.finish_native_stream_release(owner, f.call).is_err(),
        "the one actual release was already acknowledged"
    );
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .finish()
            .unwrap_err()
            .to_string(),
        primary
    );
}

#[tokio::test]
async fn native_retirement_rpc_real_pin_close_finishes_both_owners_once() {
    let mut f = native_release_rpc_fixture().await;
    let owner = f.root.owner();
    let ofd = {
        let mut e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        let ofd = e.stream_call_open_file(owner, f.call).unwrap();
        assert_eq!(e.retire_open_file(ofd), None);
        ofd
    };
    assert_eq!(
        native_release_rpc(&f).await,
        GlobalResponse::Network(Ok(NetworkReply::Unit))
    );
    native_release_peer_eof(&mut f);
    {
        let e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_eq!(e.channel_for(ofd), None);
        let trace = e.native_trace_fixture();
        trace.validate().unwrap();
        assert_eq!(
            trace
                .release_model
                .nodes()
                .iter()
                .filter(|node| matches!(
                    node.kind,
                    detcore_model::network_trace::NetworkReleaseNodeKindV4::Progress {
                        milestone: detcore_model::network_trace::NetworkProgressV4::Retired,
                        ..
                    }
                ))
                .count(),
            1
        );
    }
    let runtime = f.state.network_runtime.as_ref().unwrap();
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert!(runtime.finish_native_stream_release(owner, f.call).is_err());
}

#[tokio::test]
async fn native_retirement_rpc_unresolved_control_keeps_live_pin_and_call() {
    use std::io::Read;
    let mut f = native_release_rpc_fixture().await;
    let owner = f.root.owner();
    let (ofd, control) = {
        let mut e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        let ofd = e.stream_call_open_file(owner, f.call).unwrap();
        (ofd, e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1)
    };
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before = runtime.private_publication_runtime_fixture_state();
    let response = native_release_rpc(&f).await;
    assert!(
        matches!(response, GlobalResponse::Network(Err(NetworkRpcError::Internal(ref message))) if message.contains("StreamOperationBusy"))
    );
    assert_eq!(runtime.private_publication_runtime_fixture_state(), before);
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .stream_call_open_file(owner, f.call)
            .unwrap(),
        ofd
    );
    let peer = f._peer.as_mut().unwrap();
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.read(&mut [0u8; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    peer.set_nonblocking(false).unwrap();
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_socket_control(
            owner,
            control,
            crate::network_replay::NetworkSocketControlFinish::Unchanged,
        )
        .unwrap();
    assert_eq!(
        native_release_rpc(&f).await,
        GlobalResponse::Network(Ok(NetworkReply::Unit))
    );
    native_release_peer_eof(&mut f);
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
}

// F3: controlled original-capture identity only; the run-owned worker retains
// an actual OwnedFd or a controlled errno-bearing failure in the production
// Calls ledger. No pidfd_getfd authentication or kernel errno issuer is claimed.
async fn f3_capture_fixture() -> (
    ReplayIssuerFixture,
    crate::network_replay::NetworkStreamCall,
    u64,
) {
    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let joined = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let (call, epoch) = {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        let call = engine
            .begin_native_stream_call_from_read(owner, f.read.clone())
            .unwrap();
        let attempt = engine.begin_native_entry_stamp(owner, call.id).unwrap();
        runtime
            .with_foreground_prefix(&joined, |prefix| {
                engine
                    .stamp_native_receive_entry(
                        attempt,
                        prefix,
                        &grant,
                        f.state.global_time.lock().unwrap().as_nanos(),
                    )
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        (call, grant.epoch())
    };
    (f, call, epoch)
}

#[tokio::test]
async fn f3_private_capture_revoked_mm_releases_real_pin_and_both_ledgers() {
    use std::io::Read;
    for failed in [false, true] {
        let (f, call, epoch) = f3_capture_fixture().await;
        let owner = f.root.owner();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let engine = f.state.network_engine.as_ref().unwrap();
        let trace = engine.lock().unwrap().native_trace_fixture();
        let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let captured = runtime
            .capture_native_stream_with(
                owner,
                call.id,
                move || {
                    if failed {
                        drop(pin);
                        Err(std::io::Error::from_raw_os_error(libc::EBADF))
                    } else {
                        Ok(pin.into())
                    }
                },
                f.state.native_capture_recovery().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            captured,
            if failed {
                crate::network_replay::NetworkStreamPinOutcome::Failed(libc::EBADF)
            } else {
                crate::network_replay::NetworkStreamPinOutcome::Acquired
            }
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (1, 1, 1, 1)
        );
        if !failed {
            assert_eq!(
                peer.read(&mut [0]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        f.state
            .registered_exec_mms
            .lock()
            .unwrap()
            .remove(&owner.thread);
        let error = f
            .state
            .complete_private_receive_capture(
                f.tid,
                &f.thread,
                &f.root,
                epoch, (call,
                f.read.control.unwrap()),
                captured,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.primary(),
            &NetworkRpcError::internal("private entry changed actual local task/MM metadata")
        );
        assert!(matches!(
            error.custody(),
            super::super::foreground_store::ReceiveAdmissionCustody::Released
        ));
        assert!(error.cleanup_diagnostic().is_none());
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        runtime
            .join_foreground_prefix(f.root.clone())
            .await
            .unwrap();
        assert_eq!(
            peer.read(&mut [0]).unwrap(),
            0,
            "actual captured pin must be closed"
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn f3_actual_begin_preflight_returns_exact_reader_and_consuming_cleanup_releases_it() {
    use super::super::foreground_store::ReceiveAdmissionCustody;
    for record in [false, true] {
        let f = ReplayIssuerFixture::new_mode(record).await;
        let owner = f.root.owner();
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        let failure = if record {
            f.state
                .begin_private_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 513)
                .await
                .unwrap_err()
        } else {
            f.state
                .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 513)
                .unwrap_err()
        };
        let ReceiveAdmissionCustody::ReturnedRead(read) = failure.custody() else {
            panic!("preflight retained exact supplied reader")
        };
        assert_eq!(read, &f.read);
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        let primary = failure.primary().clone();
        let other = ReplayIssuerFixture::new_mode(record).await;
        let failure = other.state.cleanup_receive_admission_failure(failure).await;
        assert_eq!(failure.primary(), &primary);
        assert_eq!(
            failure.cleanup_diagnostic(),
            Some(&NetworkRpcError::internal(
                "admission cleanup changed actual engine"
            ))
        );
        assert!(
            matches!(failure.custody(),ReceiveAdmissionCustody::ReturnedRead(read) if read==&f.read)
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        let failure = f.state.cleanup_receive_admission_failure(failure).await;
        assert_eq!(failure.primary(), &primary);
        assert!(matches!(
            failure.custody(),
            ReceiveAdmissionCustody::Released
        ));
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert!(
            engine
                .lock()
                .unwrap()
                .finish_fd_read(owner, f.read.clone())
                .is_err()
        );
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn f3_private_capture_known_errno_releases_both_owners_without_guest_or_journal_effect() {
    use super::super::foreground_store::ReceiveAdmissionCustody;
    let (f, call, epoch) = f3_capture_fixture().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let trace = engine.lock().unwrap().native_trace_fixture();
    let captured = runtime
        .capture_native_stream_with(
            owner,
            call.id,
            || Err(std::io::Error::from_raw_os_error(libc::EACCES)),
            f.state.native_capture_recovery().unwrap(),
        )
        .await
        .unwrap();
    let failure = f
        .state
        .complete_private_receive_capture(
            f.tid,
            &f.thread,
            &f.root,
            epoch, (call,
            f.read.control.unwrap()),
            captured,
        )
        .await
        .unwrap_err();
    assert_eq!(
        failure.primary(),
        &NetworkRpcError::internal(format!(
            "private original capture failed with errno {}",
            libc::EACCES
        ))
    );
    assert!(failure.cleanup_diagnostic().is_none());
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::Released
    ));
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn f3_private_capture_unknown_keeps_exact_call_and_first_failure() {
    use super::super::foreground_store::ReceiveAdmissionCustody;
    let (f, call, _) = f3_capture_fixture().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let trace = engine.lock().unwrap().native_trace_fixture();
    let primary = runtime
        .capture_native_stream_with(
            owner,
            call.id,
            || Err(std::io::Error::other("controlled capture outcome unknown")),
            f.state.native_capture_recovery().unwrap(),
        )
        .await
        .unwrap_err();
    let primary = NetworkRpcError::internal(primary.to_string());
    let before = format!("{:?}", engine.lock().unwrap());
    let foreign = NetworkStreamOwner {
        thread: owner.thread,
        mm: owner.mm.for_exec(owner.thread),
    };
    assert!(
        engine
            .lock()
            .unwrap()
            .abandon_failed_native_receive_admission(foreign, call.id)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    let failure = f
        .state
        .fail_private_receive_capture(owner, call, primary.clone())
        .await;
    assert_eq!(failure.primary(), &primary);
    assert!(failure.cleanup_diagnostic().is_some());
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::RetainedCall(_)
    ));
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 1, 1, 1)
    );
    assert!(engine.lock().unwrap().finish().is_err());
    assert!(
        runtime
            .join_foreground_prefix(f.root.clone())
            .await
            .is_err()
    );
    let before = format!("{:?}", engine.lock().unwrap());
    let other = ReplayIssuerFixture::new_record().await;
    let failure = other.state.cleanup_receive_admission_failure(failure).await;
    assert_eq!(failure.primary(), &primary);
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::RetainedCall(_)
    ));
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn f3_private_capture_success_transfers_real_pin_then_releases_once() {
    use std::io::Read;
    let (f, call, epoch) = f3_capture_fixture().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let captured = runtime
        .capture_native_stream_with(
            owner,
            call.id,
            move || Ok(pin.into()),
            f.state.native_capture_recovery().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.state
            .complete_private_receive_capture(
                f.tid,
                &f.thread,
                &f.root,
                epoch, (call,
                f.read.control.unwrap()),
                captured
            )
            .await
            .unwrap(),
        call
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        peer.read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, call.id)
        .unwrap();
    runtime.release_native_stream(owner, call.id).await.unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, call.id)
        .unwrap();
    runtime
        .finish_native_stream_release(owner, call.id)
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn f3_replay_transfer_ordinal_refusal_returns_exact_reader_without_runtime_effect() {
    use super::super::foreground_store::ReceiveAdmissionCustody;
    let f = ReplayIssuerFixture::new().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    // Controlled ordinal exhaustion reaches the actual checked transfer path;
    // it does not replace any observed Linux outcome or weaken its comparator.
    engine.lock().unwrap().controlled_exhaust_receive_call_ids();
    let before = format!("{:?}", engine.lock().unwrap());
    let runtime_before = runtime.private_publication_runtime_fixture_state();
    let failure = f
        .state
        .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
        .unwrap_err();
    assert_eq!(
        failure.primary(),
        &NetworkRpcError::internal(NetworkReplayError::Overflow.to_string())
    );
    assert!(failure.cleanup_diagnostic().is_none());
    assert!(
        matches!(failure.custody(),ReceiveAdmissionCustody::ReturnedRead(read) if read==&f.read)
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        runtime_before
    );
    let failure = f.state.cleanup_receive_admission_failure(failure).await;
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::Released
    ));
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        runtime_before
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn f3_private_capture_changed_grant_releases_real_pin_without_reentering_guest() {
    use std::io::Read;

    use super::super::foreground_store::ReceiveAdmissionCustody;
    let (f, call, epoch) = f3_capture_fixture().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let trace = engine.lock().unwrap().native_trace_fixture();
    let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let captured = runtime
        .capture_native_stream_with(
            owner,
            call.id,
            move || Ok(pin.into()),
            f.state.native_capture_recovery().unwrap(),
        )
        .await
        .unwrap();
    f.state
        .sched
        .lock()
        .unwrap()
        .controlled_foreground_store_grant(&f.root);
    let failure = f
        .state
        .complete_private_receive_capture(
            f.tid,
            &f.thread,
            &f.root,
            epoch, (call,
            f.read.control.unwrap()),
            captured,
        )
        .await
        .unwrap_err();
    assert_eq!(
        failure.primary(),
        &NetworkRpcError::internal("private capture crossed foreground grant")
    );
    assert!(failure.cleanup_diagnostic().is_none());
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::Released
    ));
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn f3_returned_reader_cleanup_wakes_actual_pending_fd_admission() {
    use super::super::foreground_store::ReceiveAdmissionCustody;
    let mut f = ReplayIssuerFixture::new_record().await;
    // Native receive refuses NoSeq before transfer. Its real logical reader
    // still holds the table, and the ordinary NoSeq RPC is a runnable contender.
    f.state.cfg.sequentialize_threads = false;
    let failure = f
        .state
        .begin_private_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 8)
        .await
        .unwrap_err();
    assert!(
        matches!(failure.custody(),ReceiveAdmissionCustody::ReturnedRead(read) if read==&f.read)
    );
    let owner = f.root.owner();
    let mut contender = std::pin::pin!(f.state.receive_rpc(
        f.tid,
        (
            DetTime::new(&f.state.cfg),
            owner.mm,
            GlobalRequest::Network(NetworkRequest::BeginOrdinaryFdRead {
                files: f.binding.slot.files,
                fd: f.binding.slot.fd
            })
        )
    ));
    assert!(
        futures::poll!(contender.as_mut()).is_pending(),
        "actual RPC must register its wait before release"
    );
    let failure = f.state.cleanup_receive_admission_failure(failure).await;
    assert!(matches!(
        failure.custody(),
        ReceiveAdmissionCustody::Released
    ));
    assert!(failure.cleanup_diagnostic().is_none());
    let std::task::Poll::Ready((
        _,
        GlobalResponse::Network(Ok(NetworkReply::FdRead(
            crate::network_replay::NetworkFdReadBegin::Admitted(next),
        ))),
    )) = futures::poll!(contender.as_mut())
    else {
        panic!("local release did not wake the already registered RPC contender")
    };
    let next = *next;
    assert_eq!(next.binding, Some(f.binding));
    assert_ne!(next.publication.permit, f.read.publication.permit);
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(owner, next)
        .unwrap();
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}
// The dispatcher is actual Detcore code, the RPC target is this exact global,
// and memory writes use real process_vm_writev on this thread's mapped arena.
// Root/profile/MM observations and the key0 access check are controlled premises;
// this does not qualify the ptrace backend or a provider/capture implementation.
struct ScalarForegroundGuest<'a> {
    global: &'a GlobalState,
    expose_local_global: bool,
    config: &'a Config,
    thread: crate::ThreadState<()>,
    tid: Tid,
    requests: Arc<Mutex<Vec<GlobalRequest>>>,
    responses: Arc<Mutex<Vec<GlobalResponse>>>,
    replace_metadata_after_normal: Arc<std::sync::atomic::AtomicBool>,
    metadata_hook_fired: Arc<std::sync::atomic::AtomicBool>,
    replacement_thread: std::sync::OnceLock<crate::ThreadState<()>>,
    memory_events: Arc<Mutex<Vec<&'static str>>>,
    range_calls: Arc<Mutex<Vec<(i32, u64, usize)>>>,
    recvfrom_range_calls: Arc<Mutex<Vec<(reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs)>>>,
    range_verdict: reverie::OriginalReadRangeVerdict,
    // Opt-in Record observation timer: one local Timespec slot, one real
    // nfds=0 ppoll. Every other stack, write or injection still panics.
    record_timer: Option<Box<[u8; 64]>>,
    // Controlled host-signal premise: the kernel interrupts that ppoll.
    record_timer_eintr: bool,
    // Opt-in controlled provider rows for an ACTUAL same-OFD retry Peek. The
    // old fixtures keep the production provider-controller refusal unchanged.
    controlled_retry_call: Option<NetworkStreamCallId>,
    record_timer_arrival: Option<(std::os::unix::net::UnixStream, bool)>,
    // Opt-in SO_ERROR ABI tests use real permission-respecting copies.
    socket_error_access: bool,
    shared_store: Option<Arc<shared_receive::ControlledStore>>,
    shared_record_store: Option<Arc<shared_record_receive::ControlledRecordStore>>,
    // Opt-in supplied original Poll context; actual PVM input/output in tests.
    shared_poll: Option<Arc<shared_poll::ControlledPoll>>,
}
impl ScalarForegroundGuest<'_> {
    fn enable_record_timer(&mut self) {
        assert!(self.record_timer.replace(Box::new([0; 64])).is_none());
    }
    fn record_timer_slot(&self) -> Option<usize> {
        self.record_timer
            .as_ref()
            .map(|slot| slot.as_ptr() as usize)
    }
}
struct ScalarForegroundMemory {
    tid: i32,
    checked: Cell<bool>,
    events: Arc<Mutex<Vec<&'static str>>>,
    timer: Option<usize>,
    socket_error_access: bool,
}
impl MemoryAccess for ScalarForegroundMemory {
    fn read_vectored(&self, remote: &[IoSlice], local: &mut [IoSliceMut]) -> Result<usize, Errno> {
        if self.socket_error_access {
            self.events.lock().unwrap().push("socket-error-read");
            return Errno::result(unsafe { libc::process_vm_readv(self.tid,
                local.as_ptr().cast(), local.len() as _, remote.as_ptr().cast(), remote.len() as _, 0) })
                .map(|n| n as usize);
        }
        panic!("V4 scalar dispatcher used an ordinary memory read")
    }
    fn write_vectored(
        &mut self,
        local: &[IoSlice],
        remote: &mut [IoSliceMut],
    ) -> Result<usize, Errno> {
        if self.socket_error_access {
            self.events.lock().unwrap().push("socket-error-write");
            return Errno::result(unsafe { libc::process_vm_writev(self.tid,
                local.as_ptr().cast(), local.len() as _, remote.as_ptr().cast(), remote.len() as _, 0) })
                .map(|n| n as usize);
        }
        let size = std::mem::size_of::<libc::timespec>();
        let slot = self
            .timer
            .expect("V4 scalar dispatcher used a fallback memory write");
        assert!(
            local.len() == 1
                && remote.len() == 1
                && local[0].len() == size
                && remote[0].len() == size
                && remote[0].as_ptr() as usize == slot,
            "only the Record timer Timespec may be written"
        );
        remote[0].copy_from_slice(&local[0]);
        self.events.lock().unwrap().push("timer-write");
        Ok(size)
    }
    fn validate_native_user_key0_write_access(&self, expected: i32) -> Result<(), Errno> {
        assert_eq!(expected, self.tid);
        assert!(!self.checked.replace(true));
        self.events.lock().unwrap().push("access-check");
        Ok(())
    }
    fn write_native_user_vectored(
        &mut self,
        expected: i32,
        local: &[IoSlice],
        remote: &[RemoteIoVec],
    ) -> Result<usize, Errno> {
        assert_eq!(expected, self.tid);
        assert!(self.checked.get());
        self.events.lock().unwrap().push("native-write");
        let remote: Vec<_> = remote
            .iter()
            .map(|span| libc::iovec {
                iov_base: span.address() as *mut libc::c_void,
                iov_len: span.length(),
            })
            .collect();
        Errno::result(unsafe {
            libc::process_vm_writev(
                expected,
                local.as_ptr().cast(),
                local.len() as libc::c_ulong,
                remote.as_ptr().cast(),
                remote.len() as libc::c_ulong,
                0,
            )
        })
        .map(|n| n as usize)
    }
}
#[reverie::tool]
impl GlobalRPC<GlobalState> for ScalarForegroundGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.requests.lock().unwrap().push(message.2.clone());
        let controlled_peek = match (&message.2, self.controlled_retry_call) {
            (
                GlobalRequest::Network(NetworkRequest::NativeStreamEffect { lease, effect }),
                Some(call),
            ) if matches!(effect, NetworkStreamPhysicalEffect::Peek { .. }) => {
                Some((call, *lease, effect.clone()))
            }
            _ => None,
        };
        let response = if let Some((call, lease, effect)) = controlled_peek {
            assert_eq!(effect, NetworkStreamPhysicalEffect::Peek { maximum: 1024 });
            let owner = NetworkStreamOwner {
                thread: self.thread.dettid,
                mm: self.thread.mm_id,
            };
            let runtime = self.global.network_runtime.as_ref().unwrap();
            let engine = self.global.network_engine.as_ref().unwrap();
            let root = runtime.foreground_root(owner).unwrap();
            assert!(
                self.global
                    .sched
                    .lock()
                    .unwrap()
                    .foreground_native_observation(owner, &root)
                    .is_ok()
            );
            engine
                .lock()
                .unwrap()
                .submit_retained_stream_physical(owner, lease, effect.clone())
                .unwrap();
            let observed = runtime
                .controlled_receive_retry_peek(owner, call, lease, engine.clone())
                .await
                .unwrap();
            observed
                .helper_copy
                .as_ref()
                .unwrap()
                .joined_worker()
                .unwrap();
            runtime
                .preflight_native_stream(owner, lease, &effect, &observed)
                .unwrap();
            engine
                .lock()
                .unwrap()
                .confirm_retained_stream_physical(owner, lease, &observed)
                .unwrap();
            runtime
                .confirm_native_stream(owner, lease, &effect, &observed)
                .unwrap();
            (
                None,
                GlobalResponse::Network(Ok(NetworkReply::NativeStreamObservation(observed))),
            )
        } else {
            self.global.receive_rpc(self.tid, message).await
        };
        if self
            .replace_metadata_after_normal
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            // Controlled metadata-arrival premise after the REAL Normal reply,
            // independent of whether that RPC carries a thread-clock update.
            assert_eq!(
                response.1,
                GlobalResponse::RequestResources(ResumeStatus::Normal)
            );
            assert!(self.replacement_thread.get().is_none());
            let mut changed = self.thread.clone();
            let copied = changed.file_metadata.lock().unwrap().clone();
            changed.file_metadata = Arc::new(Mutex::new(copied));
            assert!(self.replacement_thread.set(changed).is_ok());
            assert!(
                !self
                    .metadata_hook_fired
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            );
        }
        self.responses.lock().unwrap().push(response.1.clone());
        response
    }
    fn config(&self) -> &Config {
        self.config
    }
}
#[reverie::tool]
impl Guest<Detcore> for ScalarForegroundGuest<'_> {
    type Memory = ScalarForegroundMemory;
    type Stack = ScalarTimerStack;
    fn tid(&self) -> Tid {
        self.tid
    }
    fn pid(&self) -> Tid {
        self.tid
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn local_global_state(&self) -> Option<&GlobalState> {
        self.expose_local_global.then_some(self.global)
    }
    fn with_followed_store<R>(&self, original: reverie::syscalls::Syscall, action: impl FnOnce(&mut dyn reverie::syscalls::FollowedStore) -> R) -> Result<R, reverie::syscalls::NativeUserStoreRefusal> {
        if let Some(writer) = &self.shared_record_store {
            return writer.with(original, false, action);
        }
        self.shared_store.as_ref().ok_or(reverie::syscalls::NativeUserStoreRefusal::Evidence(reverie::syscalls::NativeUserReadRefusal::UnsupportedBackend))?.with(original,false,action)
    }
    fn with_restored_followed_store<R>(&self, original: reverie::syscalls::Syscall, action: impl FnOnce(&mut dyn reverie::syscalls::FollowedStore) -> R) -> Result<R, reverie::syscalls::NativeUserStoreRefusal> {
        if let Some(writer) = &self.shared_record_store {
            return writer.with(original, true, action);
        }
        self.shared_store.as_ref().ok_or(reverie::syscalls::NativeUserStoreRefusal::Evidence(reverie::syscalls::NativeUserReadRefusal::UnsupportedBackend))?.with(original,true,action)
    }
    async fn capture_original_followed_poll(
        &mut self,
        original: reverie::syscalls::Syscall,
        retention: Box<dyn Send + Sync>,
    ) -> Result<reverie::syscalls::OriginalPollInput, reverie::syscalls::NativeUserReadError> {
        self.shared_poll
            .as_ref()
            .ok_or(reverie::syscalls::NativeUserReadError::Refused(
                reverie::syscalls::NativeUserReadRefusal::UnsupportedBackend,
            ))?
            .capture(original, retention)
    }
    async fn join_followed_observation_timers(
        &mut self,
        original: reverie::syscalls::Syscall,
    ) -> Result<(), reverie::Error> {
        self.shared_poll
            .as_ref()
            .ok_or_else(|| reverie::Error::Tool(anyhow::anyhow!("backend has no peer timer join")))?
            .join(original)
    }
    fn with_followed_poll_store<R>(
        &self,
        original: reverie::syscalls::Syscall,
        action: impl FnOnce(&mut dyn reverie::syscalls::FollowedPollStore) -> R,
    ) -> Result<R, reverie::syscalls::NativeUserStoreRefusal> {
        self.shared_poll
            .as_ref()
            .ok_or(reverie::syscalls::NativeUserStoreRefusal::Evidence(
                reverie::syscalls::NativeUserReadRefusal::UnsupportedBackend,
            ))?
            .with(original, action)
    }
    fn inspect_original_read_range(
        &self,
        read: reverie::syscalls::Read,
    ) -> Result<reverie::OriginalReadRangeVerdict, reverie::Error> {
        // Explicit controlled backend-entry/range premise. Actual native ABI,
        // policy and range classification are separately qualified by Stage B;
        // this fixture exercises the real Hermit dispatcher and exact cleanup.
        self.range_calls.lock().unwrap().push((
            read.fd(),
            read.buf().map_or(0, |address| address.as_raw()) as u64,
            read.len(),
        ));
        Ok(self.range_verdict)
    }
    fn inspect_original_recvfrom_range(
        &self,
        receive: reverie::syscalls::Recvfrom,
    ) -> Result<reverie::OriginalReadRangeVerdict, reverie::Error> {
        use reverie::syscalls::SyscallInfo;
        // The same controlled backend-range premise, retaining the actual
        // Recvfrom number and all six arguments independently of store length.
        self.recvfrom_range_calls.lock().unwrap().push(receive.into_parts());
        Ok(self.range_verdict)
    }
    fn memory(&self) -> Self::Memory {
        ScalarForegroundMemory {
            tid: self.tid.as_raw(),
            checked: Cell::new(false),
            events: self.memory_events.clone(),
            timer: self.record_timer_slot(),
            socket_error_access: self.socket_error_access,
        }
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        self.replacement_thread.get().unwrap_or(&self.thread)
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        self.replacement_thread
            .get_mut()
            .unwrap_or(&mut self.thread)
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("unexpected regs")
    }
    async fn stack(&mut self) -> Self::Stack {
        let slot = self.record_timer_slot().expect("unexpected stack");
        ScalarTimerStack {
            slot: Some(slot),
            events: self.memory_events.clone(),
        }
    }
    async fn daemonize(&mut self) {
        panic!("unexpected daemonization")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        syscall: S,
    ) -> Result<i64, Errno> {
        let slot = self
            .record_timer_slot()
            .expect("V4 scalar dispatcher must not inject native or fallback Read");
        let (nr, args) = syscall.into_parts();
        assert_eq!(
            nr,
            reverie::syscalls::Sysno::ppoll,
            "only the Record observation timer may be injected"
        );
        assert_eq!(
            (
                args.arg0,
                args.arg1,
                args.arg2,
                args.arg3,
                args.arg4
            ),
            (0, 0, slot, 0, 0)
        );
        let timeout = unsafe { std::ptr::read_unaligned(slot as *const libc::timespec) };
        assert_eq!(
            (timeout.tv_sec, timeout.tv_nsec),
            (0, 1_000_000),
            "1ms observation bound, not a timeout"
        );
        self.memory_events.lock().unwrap().push("timer-ppoll");
        if self.record_timer_eintr {
            if let Some((mut peer, eof)) = self.record_timer_arrival.take() {
                use std::io::Write;
                if eof { peer.shutdown(std::net::Shutdown::Write).unwrap(); }
                else { peer.write_all(b"abc").unwrap(); }
            }
            return Err(Errno::EINTR);
        }
        // The same real nfds=0 kernel timer, issued by this test thread.
        Errno::result(unsafe { libc::ppoll(std::ptr::null_mut(), 0, &timeout, std::ptr::null()) })
            .map(i64::from)
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("unexpected tail injection")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("unexpected timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("unexpected timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("unexpected host clock")
    }
}

fn scalar_foreground_guest<'a>(
    global: &'a GlobalState,
    config: &'a Config,
    thread: crate::ThreadState<()>,
    tid: Tid,
) -> ScalarForegroundGuest<'a> {
    ScalarForegroundGuest {
        global,
        expose_local_global: true,
        config,
        thread,
        tid,
        requests: Arc::new(Mutex::new(vec![])),
        responses: Arc::new(Mutex::new(vec![])),
        replace_metadata_after_normal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        metadata_hook_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        replacement_thread: std::sync::OnceLock::new(),
        memory_events: Arc::new(Mutex::new(vec![])),
        range_calls: Arc::new(Mutex::new(vec![])),
        recvfrom_range_calls: Arc::new(Mutex::new(vec![])),
        range_verdict: reverie::OriginalReadRangeVerdict::Allowed,
        record_timer: None,
        record_timer_eintr: false,
        controlled_retry_call: None,
        record_timer_arrival: None,
        socket_error_access: false,
        shared_store: None,
        shared_record_store: None,
        shared_poll: None,
    }
}

struct ScalarTimerStack {
    slot: Option<usize>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
struct ScalarTimerStackGuard;
impl Drop for ScalarTimerStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for ScalarTimerStack {
    type StackGuard = ScalarTimerStackGuard;
    fn size(&self) -> usize {
        panic!("unexpected stack size")
    }
    fn capacity(&self) -> usize {
        panic!("unexpected stack capacity")
    }
    fn push<'stack, V>(&mut self, _: V) -> reverie::syscalls::Addr<'stack, V> {
        panic!("unexpected stack push")
    }
    fn reserve<'stack, V>(&mut self) -> reverie::syscalls::AddrMut<'stack, V> {
        assert!(std::mem::size_of::<V>() <= 64);
        self.events.lock().unwrap().push("timer-reserve");
        reverie::syscalls::AddrMut::from_raw(self.slot.take().expect("one Timespec reservation"))
            .unwrap()
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        assert!(
            self.slot.is_none(),
            "commit without the Timespec reservation"
        );
        Ok(ScalarTimerStackGuard)
    }
}

fn scalar_foreground_requests(guest: &ScalarForegroundGuest<'_>) -> Vec<&'static str> {
    use crate::network_replay::NetworkFdPublicationReply;
    use crate::network_replay::NetworkFdPublicationRequest;
    let requests = guest.requests.lock().unwrap();
    let responses = guest.responses.lock().unwrap();
    assert_eq!(requests.len(), responses.len());
    // Even a fully published table performs this exact ordinary preflight.
    // Match the actual returned permit to its empty release; no Publish or
    // Acknowledge, extra acquisition, native capture or helper RPC is accepted.
    let [
        GlobalRequest::Network(NetworkRequest::FdPublication(
            NetworkFdPublicationRequest::Acquire { files },
        )),
        GlobalRequest::Network(NetworkRequest::FdPublication(
            NetworkFdPublicationRequest::ReleaseEmpty { permit },
        )),
        ..,
    ] = requests.as_slice()
    else {
        panic!("dispatcher omitted its exact table-publication preflight: {requests:?}")
    };
    assert_eq!(*files, guest.thread.file_metadata.lock().unwrap().files_id);
    assert_eq!(permit.files, *files);
    assert_eq!(
        permit.owner,
        NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id
        }
    );
    let GlobalResponse::Network(Ok(NetworkReply::FdPublication(
        NetworkFdPublicationReply::Admitted(admission),
    ))) = &responses[0]
    else {
        panic!(
            "table preflight was not actually admitted: {:?}",
            responses[0]
        )
    };
    assert_eq!(admission.permit, *permit);
    assert!(admission.recovery.is_none());
    assert_eq!(
        responses[1],
        GlobalResponse::Network(Ok(NetworkReply::FdPublication(
            NetworkFdPublicationReply::Released
        )))
    );
    requests.iter().enumerate().map(|(i,request)| match request {
        GlobalRequest::Network(NetworkRequest::FdPublication(
            NetworkFdPublicationRequest::Acquire { .. })) => "publication-acquire",
        GlobalRequest::Network(NetworkRequest::FdPublication(
            NetworkFdPublicationRequest::ReleaseEmpty { .. })) => "publication-empty-release",
        GlobalRequest::Network(NetworkRequest::BeginOrdinaryFdRead { .. }) => "ordinary-reader",
        GlobalRequest::Network(NetworkRequest::StreamSocketState { .. }) => "socket-state",
        GlobalRequest::GlobalTimeLowerBound => {
            assert!(matches!(responses[i], GlobalResponse::GlobalTimeLowerBound(_)));
            "global-time"
        },
        GlobalRequest::Network(NetworkRequest::ReleaseEligible(now)) => {
            assert!(i > 0 && matches!(requests[i-1], GlobalRequest::GlobalTimeLowerBound));
            assert_eq!(responses[i-1], GlobalResponse::GlobalTimeLowerBound(*now));
            assert!(matches!(responses[i], GlobalResponse::Network(Ok(NetworkReply::ReadyChannels(_)))));
            "release-eligible"
        },
        GlobalRequest::RequestResources(resources, process) => {
            assert_eq!(*process, guest.thread.dettid);
            assert_eq!(resources.tid, guest.thread.dettid);
            assert_eq!(resources.resources.len(), 1);
            assert!(matches!(resources.resources.iter().next(), Some((ResourceID::NetworkCallWaitSet {
                interests, deadline: None, zero_wait: None }, Permission::R))
                if interests.len()==1 && interests[0].1==crate::resources::NetworkWaitKind::ReadableAtLeast(1)));
            assert_eq!(resources.signal_interrupt_errno(), Some(Errno::ERESTARTSYS.into_raw()));
            assert!(matches!(responses[i], GlobalResponse::RequestResources(ResumeStatus::Normal | ResumeStatus::Signaled(_))));
            "call-wait"
        },
        GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) => {
            assert_eq!(admission.publication.permit.owner, NetworkStreamOwner {
                thread: guest.thread.dettid, mm: guest.thread.mm_id });
            assert_eq!(responses[i], GlobalResponse::Network(Ok(NetworkReply::Unit)));
            "reader-finish"
        },
        GlobalRequest::Network(NetworkRequest::BeginStreamCallRelease { .. }) => "release-begin",
        GlobalRequest::Network(NetworkRequest::FinishStreamCallRelease { .. }) => "release-finish",
        other => panic!("unexpected V4 scalar RPC (no capture/probe/helper is permitted): {other:?}"),
    }).collect()
}

#[tokio::test]
async fn guest_v4_owned_read_replay_crosses_two_rows_with_real_memory_and_exact_cleanup() {
    let ReplayIssuerFixture {
        config,
        state,
        thread,
        root,
        pages,
        tid,
        binding,
        read,
    } = ReplayIssuerFixture::new().await;
    let engine = state.network_engine.as_ref().unwrap();
    // The actual dispatcher must acquire its own current reader. The fixture's
    // setup reader is completed exactly; no stale token is passed behind it.
    engine
        .lock()
        .unwrap()
        .finish_fd_read(root.owner(), read)
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
    );
    let runtime = state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let before_turn = state.sched.lock().unwrap().turn;
    let before_clock = state.global_time.lock().unwrap().as_nanos();
    let call = reverie::syscalls::Read::new()
        .with_fd(binding.slot.fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
        .with_len(5);
    let result = tool.handle_owned_read(&mut guest, call).await;
    assert!(
        matches!(result, Ok(5)),
        "actual V4 dispatcher must return five bytes: {result:?}"
    );
    assert_eq!(pages.bytes(128, 5), b"abcde");
    assert_eq!(pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(pages.bytes(133, 8192 - 133), vec![0xa5; 8192 - 133]);
    assert_eq!(
        *guest.memory_events.lock().unwrap(),
        ["access-check", "native-write"]
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (5, vec![b"fgh".to_vec()], vec![0, 1, 2])
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "release-begin",
            "release-finish"
        ]
    );
    assert_replay_release_responses(&guest, &[vec![]]);
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    assert_eq!(state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
    assert!(
        engine.lock().unwrap().finish().is_err(),
        "the three-byte suffix remains required"
    );
}

#[tokio::test]
async fn guest_v4_owned_read_preflight_refusal_consumes_actual_reader_in_both_modes() {
    for record in [false, true] {
        let ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        } = ReplayIssuerFixture::new_mode(record).await;
        let engine = state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(root.owner(), read)
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        let call = reverie::syscalls::Read::new()
            .with_fd(binding.slot.fd)
            .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
            .with_len(513);
        let error = tool.handle_owned_read(&mut guest, call).await.unwrap_err();
        let reverie::Error::Tool(error) = error else {
            panic!("admission refusal is not a Linux errno")
        };
        assert_eq!(
            format!("{error:#}"),
            format!(
                "shared network engine refused operation: {}",
                if record {
                    "private receive requires bounded strict scalar capacity"
                } else {
                    "Replay receive requires bounded strict scalar capacity"
                }
            )
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(
            scalar_foreground_requests(&guest),
            [
                "publication-acquire",
                "publication-empty-release",
                "ordinary-reader",
                "socket-state"
            ]
        );
        assert!(guest.thread.original_connect.is_none());
        assert!(guest.thread.original_file_metadata.is_none());
    }
}

#[tokio::test]
async fn guest_v4_replay_releases_now_eligible_unreleased_rows_before_copy() {
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        _committed,
    ) = ReplayIssuerFixture::new_trace(
        false,
        NetworkReplayEngine::controlled_replay_two_row_trace(),
        false,
    )
    .await;
    let engine = state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(root.owner(), read)
        .unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![], vec![0, 1])
    );
    let runtime = state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let before_turn = state.sched.lock().unwrap().turn;
    let before_clock = state.global_time.lock().unwrap().as_nanos();
    let call = reverie::syscalls::Read::new()
        .with_fd(binding.slot.fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
        .with_len(5);
    let result = tool.handle_owned_read(&mut guest, call).await;
    assert!(
        matches!(result, Ok(5)),
        "actual Guest must release eligible V4 input before selection: {result:?}"
    );
    assert_eq!(pages.bytes(128, 5), b"abcde");
    assert_eq!(pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(pages.bytes(133, 8192 - 133), vec![0xa5; 8192 - 133]);
    assert_eq!(
        *guest.memory_events.lock().unwrap(),
        ["access-check", "native-write"]
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (5, vec![b"fgh".to_vec()], vec![0, 1, 2])
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "release-begin",
            "release-finish"
        ]
    );
    assert_replay_release_responses(&guest, &[vec![NetworkChannelId(1)]]);
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    assert_eq!(state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
    assert!(
        engine.lock().unwrap().finish().is_err(),
        "three-byte suffix remains required"
    );
}

// These controls drive the actual resource transport and scheduler. Trace/root
// setup and signal/metadata arrival remain explicit controlled premises.
fn future_replay_trace() -> (detcore_model::network_trace::NetworkTraceV4, LogicalTime) {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let deadline = trace.epoch_global_time().unwrap() + LogicalTime::from_nanos(1_000_000_000);
    for input in &mut trace.inputs[1..] {
        input.release.not_before_global_time = deadline;
    }
    trace.validate().unwrap();
    (trace, deadline)
}

fn producer_blocked_replay_trace() -> detcore_model::network_trace::NetworkTraceV4 {
    use detcore_model::network_trace::NetworkProgressV4;
    use detcore_model::network_trace::NetworkReceiveEntryCutV4;
    use detcore_model::network_trace::NetworkReleaseModelV4;
    use detcore_model::network_trace::NetworkReleaseNodeIdV4;
    use detcore_model::network_trace::NetworkReleaseNodeKindV4;
    use detcore_model::network_trace::NetworkReleaseNodeV4;
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let channel = trace.channels[0].id;
    trace.outputs.push(NetworkOutputEventV2 {
        channel,
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"x".to_vec(),
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    for node in &mut nodes[2..] {
        node.id.0 += 1;
    }
    nodes.insert(
        2,
        NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(2),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::StreamPrefix {
                    exclusive_offset: 1,
                },
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        },
    );
    for n in 1..trace.inputs.len() {
        let cut = NetworkReceiveEntryCutV4(trace.inputs[n].release.receive_entry_cut.0 + 1);
        let frontier = trace.entry_frontier(cut).unwrap();
        assert_eq!(
            frontier,
            [NetworkReleaseNodeIdV4(1), NetworkReleaseNodeIdV4(2)]
        );
        trace.inputs[n].release.receive_entry_cut = cut;
        trace.inputs[n].release.prerequisites = frontier.clone();
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes
            .iter_mut()
            .find(|node| {
                matches!(node.kind,
            NetworkReleaseNodeKindV4::Input { input_ordinal } if input_ordinal == n as u64)
            })
            .unwrap()
            .prerequisites = frontier;
    }
    trace.validate().unwrap();
    trace
}

fn actual_replay_wait(
    state: &GlobalState,
    requests: &Arc<Mutex<Vec<GlobalRequest>>>,
    owner: NetworkStreamOwner,
    binding: crate::types::FdSlotBinding,
) -> (
    NetworkStreamCallId,
    Resources,
    Ivar<crate::scheduler::SchedResponse>,
) {
    let requests = requests.lock().unwrap();
    let GlobalRequest::RequestResources(actual, process) = requests.last().unwrap() else {
        panic!("actual dispatcher has not published its wait: {requests:?}")
    };
    assert_eq!(*process, owner.thread);
    assert_eq!(actual.resources.len(), 1);
    let (resource, permission) = actual.resources.iter().next().unwrap();
    let ResourceID::NetworkCallWaitSet {
        interests,
        deadline,
        zero_wait,
    } = resource
    else {
        panic!("not an actual Call wait: {resource:?}")
    };
    assert_eq!(*permission, Permission::R);
    assert!(deadline.is_none());
    assert!(zero_wait.is_none());
    assert_eq!(interests.len(), 1);
    let (call, interest) = interests[0];
    assert_eq!(
        interest,
        crate::resources::NetworkWaitKind::ReadableAtLeast(1)
    );
    let mut expected = Resources::new(owner.thread);
    expected.insert(resource.clone(), Permission::R);
    expected.set_signal_interrupt_errno(Errno::ERESTARTSYS);
    assert_eq!(*actual, expected);
    let scheduler = state.sched.lock().unwrap();
    let next = &scheduler.next_turns[&owner.thread];
    assert_eq!(next.req.try_read().unwrap().unwrap(), expected);
    assert_eq!(next.protocol.origin.as_ref().unwrap().mm, owner.mm);
    assert!(next.resp.try_read().is_none());
    let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine.stream_call_open_file(owner, call).unwrap(),
        binding.open_file
    );
    assert_eq!(
        engine.native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    let status = engine.stream_call_queue_status(owner, call).unwrap();
    assert!(!status.ingress_busy);
    assert!(!status.delivery_busy);
    assert_eq!(status.consume_epoch, 0);
    (call, expected, next.resp.clone())
}

type FutureWaitContext<'a> = (
    &'a GlobalState,
    &'a Arc<crate::network_runtime::ForegroundRoot>,
    crate::types::FdSlotBinding,
    NetworkStreamCallId,
);
type FutureWaitEvidence<'a> = (&'a Pages, &'a Arc<Mutex<Vec<&'static str>>>, u64);

async fn select_future_replay_wait(
    (state, root, binding, call): FutureWaitContext<'_>,
    request: &Resources,
    response: &Ivar<crate::scheduler::SchedResponse>,
    committed: Resources,
    deadline: LogicalTime,
    (pages, events, old_epoch): FutureWaitEvidence<'_>,
) {
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    assert!(state.global_time.lock().unwrap().as_nanos() < deadline);
    let first = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &Ok(committed),
    )
    .await;
    assert!(first.is_err(), "unavailable input must actually park");
    assert!(response.try_read().is_none());
    assert!(
        !state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .contains_tid(owner.thread)
    );
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, root)
            .is_err()
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .unwrap(),
        binding.open_file
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![], vec![0, 1])
    );
    assert_eq!(
        engine.lock().unwrap().next_release_time().unwrap(),
        Some(deadline)
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    let idle = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &first,
    )
    .await;
    assert!(
        idle.is_err(),
        "idle time advance is not a foreground selection"
    );
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), deadline);
    assert!(response.try_read().is_none());
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![], vec![0, 1])
    );
    let selected =
        crate::scheduler::do_a_turn_blocking(state.sched.clone(), state.global_time.clone(), &idle)
            .await
            .unwrap();
    assert_eq!(&selected, request);
    assert!(matches!(
        response.try_read(),
        Some(crate::scheduler::SchedResponse::Go(None))
    ));
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, root)
            .unwrap()
            .epoch()
            > old_epoch
    );
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), deadline);
    // Release is availability, never producer completion or a guest-memory write.
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    let status = engine
        .lock()
        .unwrap()
        .stream_call_queue_status(owner, call)
        .unwrap();
    assert!(!status.ingress_busy);
    assert!(!status.delivery_busy);
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
}

#[tokio::test]
async fn guest_v4_replay_future_input_parks_then_uses_actual_new_foreground_grant() {
    let (trace, deadline) = future_replay_trace();
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        committed,
    ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let old_epoch = state
        .sched
        .lock()
        .unwrap()
        .foreground_native_observation(owner, &root)
        .unwrap()
        .epoch();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let requests = guest.requests.clone();
    let events = guest.memory_events.clone();
    let syscall = reverie::syscalls::Read::new()
        .with_fd(binding.slot.fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
        .with_len(5);
    let call;
    {
        let mut pending = std::pin::pin!(tool.handle_owned_read(&mut guest, syscall));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let (actual_call, request, response) =
            actual_replay_wait(&state, &requests, owner, binding);
        call = actual_call;
        select_future_replay_wait(
            (&state, &root, binding, call), &request, &response, committed, deadline, (&pages, &events, old_epoch),
        )
        .await;
        assert_eq!(pending.await.unwrap(), 5);
    }
    assert_eq!(pages.bytes(128, 5), b"abcde");
    assert_eq!(pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(pages.bytes(133, 8192 - 133), vec![0xa5; 8192 - 133]);
    assert_eq!(*events.lock().unwrap(), ["access-check", "native-write"]);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (5, vec![b"fgh".to_vec()], vec![0, 1, 2])
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .is_err()
    );
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "global-time",
            "call-wait",
            "global-time",
            "release-eligible",
            "release-begin",
            "release-finish"
        ]
    );
    assert_replay_release_responses(&guest, &[vec![], vec![]]);
    assert_exact_call_release(&guest, call);
    assert!(engine.lock().unwrap().finish().is_err());
}

#[tokio::test]
async fn guest_v4_replay_unfinished_producer_cannot_be_replaced_by_time_or_selection() {
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        committed,
    ) = ReplayIssuerFixture::new_trace(false, producer_blocked_replay_trace(), false).await;
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let requests = guest.requests.clone();
    let responses = guest.responses.clone();
    let events = guest.memory_events.clone();
    let syscall = reverie::syscalls::Read::new()
        .with_fd(binding.slot.fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
        .with_len(5);
    let mut pending = std::pin::pin!(tool.handle_owned_read(&mut guest, syscall));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let (call, _request, response) = actual_replay_wait(&state, &requests, owner, binding);
    let first = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &Ok(committed),
    )
    .await;
    assert!(first.is_err());
    assert!(response.try_read().is_none());
    let parked_clock = state.global_time.lock().unwrap().as_nanos();
    assert_eq!(engine.lock().unwrap().next_release_time().unwrap(), None);
    let idle = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &first,
    )
    .await;
    assert!(idle.is_err());
    assert!(response.try_read().is_none());
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), parked_clock);
    assert!(format!("{:?}",state.sched.lock().unwrap()).contains(
        "terminal_deadlock: Some(\"network replay wait cannot be satisfied by time or outbound progress\")"));
    assert!(
        !state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .contains_tid(owner.thread)
    );
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, &root)
            .is_err()
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .unwrap(),
        binding.open_file
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![], vec![0, 1])
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(requests.lock().unwrap().len(), 8);
    assert_eq!(responses.lock().unwrap().len(), 7);
    assert!(engine.lock().unwrap().finish().is_err());
    // This bounded negative fixture retains the unresolved logical Call. Dropping
    // its pending component future is NOT runtime cleanup or a successful run.
}

#[tokio::test]
async fn guest_v4_replay_signal_before_park_returns_restart_errno_and_cleans_same_call() {
    let (trace, _) = future_replay_trace();
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        committed,
    ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let requests = guest.requests.clone();
    let events = guest.memory_events.clone();
    let syscall = reverie::syscalls::Read::new()
        .with_fd(binding.slot.fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
        .with_len(5);
    let call;
    {
        let mut pending = std::pin::pin!(tool.handle_owned_read(&mut guest, syscall));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let (actual_call, _request, response) =
            actual_replay_wait(&state, &requests, owner, binding);
        call = actual_call;
        // Explicit controlled signal-arrival premise, using the SAME transport
        // response. Real scheduler selection alone produces Signaled.
        let mut inbound = Resources::new(owner.thread);
        inbound.insert(
            ResourceID::InboundSignal(SigWrapper::from(Signal::SIGUSR1)),
            Permission::W,
        );
        inbound.set_signal_interrupt_errno(Errno::ERESTARTSYS);
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&owner.thread)
            .unwrap()
            .req = Ivar::full(Ok(inbound.clone()));
        let selected = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Ok(committed),
        )
        .await
        .unwrap();
        assert_eq!(selected, inbound);
        assert!(
            matches!(response.try_read(),Some(crate::scheduler::SchedResponse::Signaled(Some(signals)))
            if signals==vec![SigWrapper::from(Signal::SIGUSR1)])
        );
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .foreground_native_observation(owner, &root)
                .is_err()
        );
        let error = pending.await.unwrap_err();
        assert!(
            matches!(error, reverie::Error::Errno(Errno::ERESTARTSYS)),
            "{error:?}"
        );
    }
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(binding.open_file),
        (0, vec![], vec![0, 1])
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "global-time",
            "call-wait",
            "release-begin",
            "release-finish"
        ]
    );
    assert_replay_release_responses(&guest, &[vec![]]);
    assert_exact_call_release(&guest, call);
    assert!(engine.lock().unwrap().finish().is_err());
}

#[tokio::test]
async fn guest_v4_replay_reentry_rechecks_registered_mm_root_and_normal_grant() {
    for variant in 0..3 {
        let (trace, deadline) = future_replay_trace();
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            committed,
        ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
        let owner = root.owner();
        let engine = state.network_engine.as_ref().unwrap();
        engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let old_metadata = thread.file_metadata.clone();
        let before_runtime = state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state();
        let old_epoch = state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, &root)
            .unwrap()
            .epoch();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        let requests = guest.requests.clone();
        let events = guest.memory_events.clone();
        let replace_metadata = guest.replace_metadata_after_normal.clone();
        let hook_fired = guest.metadata_hook_fired.clone();
        let syscall = reverie::syscalls::Read::new()
            .with_fd(binding.slot.fd)
            .with_buf(reverie::syscalls::AddrMut::from_raw(pages.at(128) as usize))
            .with_len(5);
        let call;
        {
            let mut pending = std::pin::pin!(tool.handle_owned_read(&mut guest, syscall));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let (actual_call, request, response) =
                actual_replay_wait(&state, &requests, owner, binding);
            call = actual_call;
            select_future_replay_wait(
                (&state, &root, binding, call), &request, &response, committed, deadline, (&pages, &events, old_epoch),
            )
            .await;
            match variant {
                0 => {
                    assert_eq!(
                        state
                            .registered_exec_mms
                            .lock()
                            .unwrap()
                            .remove(&owner.thread),
                        Some(owner.mm)
                    );
                }
                1 => state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .revoke_foreground_lineage(),
                2 => replace_metadata.store(true, std::sync::atomic::Ordering::SeqCst),
                _ => unreachable!(),
            }
            assert!(
                !hook_fired.load(std::sync::atomic::Ordering::SeqCst),
                "variant {variant}: no metadata replacement before real response delivery"
            );
            let result = pending.await;
            // Prove each controlled invalidation actually happened before
            // judging its typed result; absence of a fixture premise is RED.
            match variant {
                0 => assert_eq!(
                    state.registered_exec_mms.lock().unwrap().get(&owner.thread),
                    None
                ),
                1 => assert!(!root.is_current(owner)),
                2 => assert!(
                    hook_fired.load(std::sync::atomic::Ordering::SeqCst),
                    "metadata variant did not run its actual Normal-response hook"
                ),
                _ => unreachable!(),
            }
            assert!(
                result.is_err(),
                "reentry variant {variant} unexpectedly returned {result:?}"
            );
            let error = result.unwrap_err();
            let reverie::Error::Tool(error) = error else {
                panic!("expected typed reentry refusal")
            };
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "shared network engine refused operation: {}",
                    if variant == 1 {
                        "foreground ctl lineage was revoked by an unsupported physical owner"
                    } else {
                        "Replay selection changed actual task/MM metadata"
                    }
                )
            );
        }
        if variant == 2 {
            assert!(guest.replacement_thread.get().is_some());
            assert!(!Arc::ptr_eq(
                &old_metadata,
                &guest.thread_state().file_metadata
            ));
        }
        let original_files = old_metadata.lock().unwrap().files_id;
        assert_eq!(
            original_files,
            guest.thread_state().file_metadata.lock().unwrap().files_id
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .controlled_replay_delivery_state(binding.open_file),
            (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(
            scalar_foreground_requests(&guest),
            [
                "publication-acquire",
                "publication-empty-release",
                "ordinary-reader",
                "socket-state",
                "global-time",
                "release-eligible",
                "global-time",
                "call-wait",
                "global-time",
                "release-eligible",
                "release-begin",
                "release-finish"
            ]
        );
        assert_replay_release_responses(&guest, &[vec![], vec![]]);
        assert_exact_call_release(&guest, call);
        assert!(engine.lock().unwrap().finish().is_err());
    }
}

#[tokio::test]
async fn guest_v4_replay_nonblocking_empty_retains_explicit_unsupported_outcome() {
    for variant in 0..4 {
        let (trace, _) = future_replay_trace();
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            _committed,
        ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
        let engine = state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(root.owner(), read)
            .unwrap();
        // Controlled O_NONBLOCK premise, not native fcntl qualification.
        thread
            .with_detfd(binding.slot.fd, |fd| fd.set_nonblocking(true))
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_runtime = state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        let before_turn = state.sched.lock().unwrap().turn;
        let before_clock = state.global_time.lock().unwrap().as_nanos();
        let address = match variant {
            0 => pages.at(128),
            1 => 0,
            2 => pages.at(4096),
            3 => pages.at(4093),
            _ => unreachable!(),
        };
        pages.protect_second(libc::PROT_NONE);
        let syscall = reverie::syscalls::Read::new()
            .with_fd(binding.slot.fd)
            .with_buf(reverie::syscalls::AddrMut::from_raw(address as usize))
            .with_len(5);
        let error = tool
            .handle_owned_read(&mut guest, syscall)
            .await
            .unwrap_err();
        pages.protect_second(libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(
            *guest.range_calls.lock().unwrap(),
            [(binding.slot.fd, address, 5)]
        );
        assert!(
            matches!(error, reverie::Error::Errno(Errno::EAGAIN)),
            "actual nonblocking empty Replay Read must return exact EAGAIN: {error:?}"
        );
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .controlled_replay_delivery_state(binding.open_file),
            (0, vec![], vec![0, 1])
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(
            scalar_foreground_requests(&guest),
            [
                "publication-acquire",
                "publication-empty-release",
                "ordinary-reader",
                "socket-state",
                "global-time",
                "release-eligible",
                "release-begin",
                "release-finish"
            ]
        );
        assert_replay_release_responses(&guest, &[vec![]]);
        assert_eq!(state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert!(engine.lock().unwrap().finish().is_err());
    }
}

fn assert_replay_release_responses(
    guest: &ScalarForegroundGuest<'_>,
    expected: &[Vec<NetworkChannelId>],
) {
    let requests = guest.requests.lock().unwrap();
    let responses = guest.responses.lock().unwrap();
    let actual: Vec<_> = requests
        .iter()
        .enumerate()
        .filter_map(|(i, request)| {
            if let GlobalRequest::Network(NetworkRequest::ReleaseEligible(now)) = request {
                assert!(matches!(
                    requests[i - 1],
                    GlobalRequest::GlobalTimeLowerBound
                ));
                assert_eq!(responses[i - 1], GlobalResponse::GlobalTimeLowerBound(*now));
                let GlobalResponse::Network(Ok(NetworkReply::ReadyChannels(channels))) =
                    &responses[i]
                else {
                    panic!("release was not accepted: {:?}", responses[i])
                };
                Some(channels.to_vec())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(actual, expected);
}
fn assert_exact_call_release(guest: &ScalarForegroundGuest<'_>, call: NetworkStreamCallId) {
    let requests = guest.requests.lock().unwrap();
    assert_eq!(
        &requests[requests.len() - 2..],
        &[
            GlobalRequest::Network(NetworkRequest::BeginStreamCallRelease { id: call }),
            GlobalRequest::Network(NetworkRequest::FinishStreamCallRelease { id: call })
        ]
    );
}

// Shutdown preflight controls use the actual Guest dispatcher/RPC/global engine.
// Socket installation and local-reference visibility are controlled premises;
// forbidden native injection and all ordinary memory fallbacks remain panics.
async fn shutdown_unenrolled_binding(f: &ReplayIssuerFixture) -> crate::types::FdSlotBinding {
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = owned_read_guest(&f.config, &f.state, f.thread.clone());
    guest
        .thread
        .add_fd(
            8,
            nix::fcntl::OFlag::empty(),
            crate::fd::FdType::Socket,
            None,
        )
        .unwrap();
    {
        let mut metadata = guest.thread.file_metadata.lock().unwrap();
        assert_eq!(metadata.pending_network_installations().len(), 1);
        let replacement = metadata.pending_network_installations()[0];
        let effect = f
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .fd_publication_fixture_effect(f.root.owner(), replacement);
        metadata
            .associate_network_installation(replacement.installation_generation, effect)
            .unwrap();
    }
    tool.publish_network_fd_installations(&mut guest)
        .await
        .unwrap();
    let binding = guest.thread.descriptor_binding(8).unwrap();
    assert_ne!(binding.open_file, f.binding.open_file);
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .stream_socket_state(binding.open_file)
            .unwrap()
            .is_none()
    );
    binding
}

#[tokio::test]
async fn guest_v4_record_shutdown_refuses_before_control_or_native_effect() {
    for unenrolled in [false, true] {
        for expose_local in [true, false] {
            for how in [libc::SHUT_RD, libc::SHUT_WR, libc::SHUT_RDWR] {
                let f = ReplayIssuerFixture::new_record().await;
                let engine = f.state.network_engine.as_ref().unwrap();
                engine
                    .lock()
                    .unwrap()
                    .finish_fd_read(f.root.owner(), f.read.clone())
                    .unwrap();
                let binding = if unenrolled {
                    shutdown_unenrolled_binding(&f).await
                } else {
                    f.binding
                };
                assert_eq!(
                    engine
                        .lock()
                        .unwrap()
                        .stream_socket_state(binding.open_file)
                        .unwrap()
                        .is_none(),
                    unenrolled
                );
                let before = format!("{:?}", engine.lock().unwrap());
                let before_trace = engine.lock().unwrap().native_trace_fixture();
                let runtime = f.state.network_runtime.as_ref().unwrap();
                let before_runtime = runtime.private_publication_runtime_fixture_state();
                let before_turn = f.state.sched.lock().unwrap().turn;
                let before_clock = f.state.global_time.lock().unwrap().as_nanos();
                let tool: Detcore = Detcore::new(f.tid, &f.config);
                let mut guest =
                    scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
                guest.expose_local_global = expose_local;
                assert_eq!(guest.local_global_state().is_some(), expose_local);
                let call = reverie::syscalls::Shutdown::new()
                    .with_fd(binding.slot.fd)
                    .with_how(how);
                assert!(tool.network_io_owns(&mut guest, call.into()));
                let result = {
                    let mut pending =
                        std::pin::pin!(tool.handle_network_io(&mut guest, call.into()));
                    futures::poll!(pending.as_mut())
                };
                let std::task::Poll::Ready(Err(reverie::Error::Tool(error))) = result else {
                    panic!("Shutdown must refuse before any control or external wait: {result:?}")
                };
                assert_eq!(
                    format!("{error:#}"),
                    format!(
                        "shared network engine refused operation: {}",
                        NetworkReplayError::WrongMode
                    )
                );
                // Exact complete spelling compiles on the predecessor for RED;
                // the final candidate also gains typed singleton equality.
                assert_eq!(
                    format!("{:?}", *guest.requests.lock().unwrap()),
                    "[Network(PreflightSocketShutdown)]"
                );
                assert_eq!(
                    *guest.requests.lock().unwrap(),
                    [GlobalRequest::Network(
                        NetworkRequest::PreflightSocketShutdown
                    )]
                );
                assert_eq!(
                    *guest.responses.lock().unwrap(),
                    [GlobalResponse::Network(Err(NetworkRpcError::internal(
                        NetworkReplayError::WrongMode.to_string()
                    )))]
                );
                assert!(guest.memory_events.lock().unwrap().is_empty());
                assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
                assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
                assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
                assert_eq!(
                    engine
                        .lock()
                        .unwrap()
                        .native_capture_fixture_counts(binding.open_file),
                    (0, 0, 0, 0)
                );
                assert_eq!(
                    engine
                        .lock()
                        .unwrap()
                        .native_capture_fixture_counts(f.binding.open_file),
                    (0, 0, 0, 0)
                );
                assert_eq!(
                    runtime.private_publication_runtime_fixture_state(),
                    before_runtime
                );
                assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
                assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
                assert!(guest.thread.original_connect.is_none());
                assert!(guest.thread.original_file_metadata.is_none());
            }
        }
    }
}

#[tokio::test]
async fn native_record_shutdown_submit_refusal_keeps_control_releasable() {
    for direction in [
        NetworkShutdownV2::Read,
        NetworkShutdownV2::Write,
        NetworkShutdownV2::Both,
    ] {
        let f = ReplayIssuerFixture::new_record().await;
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        engine
            .finish_fd_read(f.root.owner(), f.read.clone())
            .unwrap();
        let lease = engine
            .begin_socket_controls(f.root.owner(), vec![f.binding.open_file])
            .unwrap()[0]
            .1;
        let before = format!("{engine:?}");
        let trace = engine.native_trace_fixture();
        assert!(matches!(
            engine.submit_stream_physical(
                f.root.owner(),
                lease,
                crate::network_replay::NetworkStreamPhysicalEffect::Shutdown { direction }
            ),
            Err(NetworkReplayError::WrongMode)
        ));
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(engine.native_trace_fixture(), trace);
        engine
            .finish_socket_control(
                f.root.owner(),
                lease,
                crate::network_replay::NetworkSocketControlFinish::Unchanged,
            )
            .unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.native_trace_fixture(), trace);
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn guest_network_shutdown_invalid_direction_precedes_preflight() {
    for record in [false, true] {
        for expose_local in [true, false] {
            let f = ReplayIssuerFixture::new_mode(record).await;
            let engine = f.state.network_engine.as_ref().unwrap();
            engine
                .lock()
                .unwrap()
                .finish_fd_read(f.root.owner(), f.read.clone())
                .unwrap();
            let before = format!("{:?}", engine.lock().unwrap());
            let before_turn = f.state.sched.lock().unwrap().turn;
            let before_clock = f.state.global_time.lock().unwrap().as_nanos();
            let runtime_before = f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_publication_runtime_fixture_state();
            let tool: Detcore = Detcore::new(f.tid, &f.config);
            let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
            guest.expose_local_global = expose_local;
            let invalid_fd = reverie::syscalls::Shutdown::new()
                .with_fd(-1)
                .with_how(libc::SHUT_RDWR);
            assert!(!tool.network_io_owns(&mut guest, invalid_fd.into()));
            let invalid_how = reverie::syscalls::Shutdown::new()
                .with_fd(f.binding.slot.fd)
                .with_how(-1);
            let result = tool.handle_network_io(&mut guest, invalid_how.into()).await;
            assert!(
                matches!(result, Err(reverie::Error::Errno(Errno::EINVAL))),
                "invalid how changed result: {result:?}"
            );
            assert!(guest.requests.lock().unwrap().is_empty());
            assert!(guest.responses.lock().unwrap().is_empty());
            assert!(guest.memory_events.lock().unwrap().is_empty());
            assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(f.binding.open_file),
                (0, 0, 0, 0)
            );
            assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
            assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
            assert_eq!(
                f.state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .private_publication_runtime_fixture_state(),
                runtime_before
            );
            assert!(guest.thread.original_connect.is_none());
            assert!(guest.thread.original_file_metadata.is_none());
        }
    }
}

fn shutdown_replay_trace(
    direction: NetworkShutdownV2,
) -> detcore_model::network_trace::NetworkTraceV4 {
    use detcore_model::network_trace::NetworkProgressV4;
    use detcore_model::network_trace::NetworkReleaseModelV4;
    use detcore_model::network_trace::NetworkReleaseNodeIdV4;
    use detcore_model::network_trace::NetworkReleaseNodeKindV4;
    use detcore_model::network_trace::NetworkReleaseNodeV4;
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let channel = trace.channels[0].id;
    assert!(trace.outputs.is_empty());
    trace.outputs.push(NetworkOutputEventV2 {
        channel,
        event: NetworkOutputKindV2::Shutdown {
            stream_offset: 0,
            direction,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    assert_eq!(nodes.len(), 4);
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(4),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel,
            milestone: NetworkProgressV4::LocalShutdown { output_ordinal: 0 },
        },
        prerequisites: vec![NetworkReleaseNodeIdV4(1)],
    });
    trace.validate().unwrap();
    trace
}

fn assert_shutdown_transaction(
    guest: &ScalarForegroundGuest<'_>,
    open_file: OpenFileId,
    before_socket: crate::network_replay::NetworkStreamSocketState,
    direction: NetworkShutdownV2,
) {
    use crate::network_replay::NetworkSocketControlFinish;
    use crate::network_replay::NetworkStreamPhysicalEffect;
    use crate::network_replay::NetworkStreamPhysicalResult;
    let requests = guest.requests.lock().unwrap();
    let responses = guest.responses.lock().unwrap();
    assert_eq!(requests.len(), 7);
    assert_eq!(responses.len(), 7);
    assert_eq!(
        responses[0],
        GlobalResponse::Network(Ok(NetworkReply::Unit))
    );
    for i in [1, 2] {
        assert_eq!(
            responses[i],
            GlobalResponse::Network(Ok(NetworkReply::StreamSocketState(Some(
                before_socket.clone()
            ))))
        );
    }
    let GlobalResponse::Network(Ok(NetworkReply::SocketControl(control))) = &responses[3] else {
        panic!(
            "actual Shutdown control was not acquired: {:?}",
            responses[3]
        )
    };
    assert_eq!(control.pending_error, None);
    assert_eq!(control.options, Some(before_socket.options.clone()));
    assert_eq!(control.consume_epoch, before_socket.consume_epoch);
    assert_eq!(
        *requests,
        [
            GlobalRequest::Network(NetworkRequest::PreflightSocketShutdown),
            GlobalRequest::Network(NetworkRequest::StreamSocketState { open_file }),
            GlobalRequest::Network(NetworkRequest::StreamSocketState { open_file }),
            GlobalRequest::Network(NetworkRequest::BeginSocketControl { open_file }),
            GlobalRequest::Network(NetworkRequest::SubmitStreamPhysical {
                lease: control.lease,
                effect: NetworkStreamPhysicalEffect::Shutdown { direction }
            }),
            GlobalRequest::Network(NetworkRequest::ConfirmStreamPhysical {
                lease: control.lease,
                result: NetworkStreamPhysicalResult::Shutdown { result: Ok(()) }
            }),
            GlobalRequest::Network(NetworkRequest::FinishSocketControl {
                lease: control.lease,
                disposition: NetworkSocketControlFinish::Unchanged
            }),
        ]
    );
    for i in [4, 5, 6] {
        assert_eq!(
            responses[i],
            GlobalResponse::Network(Ok(NetworkReply::Unit))
        );
    }
}

#[tokio::test]
async fn guest_v4_replay_shutdown_keeps_payload_and_exact_control_transaction() {
    for (how, direction) in [
        (libc::SHUT_RD, NetworkShutdownV2::Read),
        (libc::SHUT_WR, NetworkShutdownV2::Write),
        (libc::SHUT_RDWR, NetworkShutdownV2::Both),
    ] {
        for expose_local in [true, false] {
            let (f, _) =
                ReplayIssuerFixture::new_trace(false, shutdown_replay_trace(direction), true).await;
            let engine = f.state.network_engine.as_ref().unwrap();
            engine
                .lock()
                .unwrap()
                .finish_fd_read(f.root.owner(), f.read.clone())
                .unwrap();
            let before_trace = engine.lock().unwrap().native_trace_fixture();
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .controlled_replay_delivery_state(f.binding.open_file),
                (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1])
            );
            let before_socket = engine
                .lock()
                .unwrap()
                .stream_socket_state(f.binding.open_file)
                .unwrap()
                .unwrap();
            let runtime = f.state.network_runtime.as_ref().unwrap();
            let before_runtime = runtime.private_publication_runtime_fixture_state();
            let before_turn = f.state.sched.lock().unwrap().turn;
            let before_clock = f.state.global_time.lock().unwrap().as_nanos();
            let tool: Detcore = Detcore::new(f.tid, &f.config);
            let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
            guest.expose_local_global = expose_local;
            assert_eq!(guest.local_global_state().is_some(), expose_local);
            let call = reverie::syscalls::Shutdown::new()
                .with_fd(f.binding.slot.fd)
                .with_how(how);
            assert_eq!(
                tool.handle_network_io(&mut guest, call.into())
                    .await
                    .unwrap(),
                0
            );
            assert_shutdown_transaction(&guest, f.binding.open_file, before_socket, direction);
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .controlled_replay_delivery_state(f.binding.open_file),
                (0, vec![b"ab".to_vec(), b"cdefgh".to_vec()], vec![0, 1, 4])
            );
            let status = engine
                .lock()
                .unwrap()
                .stream_queue_status(f.binding.open_file)
                .unwrap();
            assert_eq!(status.queued_bytes, 8);
            assert_eq!(
                status.local_read_shutdown,
                direction != NetworkShutdownV2::Write
            );
            assert!(!status.eof);
            assert_eq!(status.error, None);
            assert_eq!(
                status.readiness.hangup,
                direction == NetworkShutdownV2::Both
            );
            assert!(status.readiness.readable);
            assert!(!status.ingress_busy);
            assert!(!status.delivery_busy);
            assert_eq!(status.consume_epoch, 0);
            let before_probe = format!("{:?}", engine.lock().unwrap());
            let transmit = engine
                .lock()
                .unwrap()
                .transmit_stream(f.binding.open_file, &[]);
            if direction == NetworkShutdownV2::Read {
                assert!(matches!(
                    transmit,
                    Ok(crate::network_replay::StreamTransmitOutcome::Accepted(0))
                ));
            } else {
                assert!(matches!(
                    transmit,
                    Err(NetworkReplayError::TransportMismatch(NetworkChannelId(1)))
                ));
            }
            assert_eq!(format!("{:?}", engine.lock().unwrap()), before_probe);
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(f.binding.open_file),
                (0, 0, 0, 0)
            );
            assert_eq!(
                runtime.private_publication_runtime_fixture_state(),
                before_runtime
            );
            assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
            assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
            assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert!(guest.memory_events.lock().unwrap().is_empty());
            assert!(guest.thread.original_connect.is_none());
            assert!(guest.thread.original_file_metadata.is_none());
            assert!(
                engine.lock().unwrap().finish().is_err(),
                "all eight bytes remain required"
            );
        }
    }
}

#[tokio::test]
async fn guest_rpc_only_legacy_shutdown_keeps_existing_modeled_transition() {
    let f = ReplayIssuerFixture::new().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    // Controlled V3 trace/profile setup replaces an entirely unowned fixture
    // before measurement; the dispatcher and subsequent RPCs remain actual.
    let profile =
        NetworkReplayEngine::controlled_replay_two_row_trace().fresh_stream_profiles[0].clone();
    let definition = NetworkReplayEngine::controlled_replay_two_row_trace().channels[0].clone();
    let mut record = NetworkReplayEngine::record_shadow(f.config.epoch);
    record
        .register_stream_socket(
            f.binding.open_file,
            profile.key,
            crate::network_replay::NetworkStreamNamespace {
                device: 7,
                inode: 11,
            },
            Some(profile.clone()),
        )
        .unwrap();
    let channel = record
        .ensure_channel(
            f.binding.open_file,
            NetworkChannelBinding {
                transport: definition.transport,
                role: definition.role,
                peer_address: definition.peer_address,
                requested_local_constraint: None,
                observed_local_address: None,
                accepted_from: None,
                selected_channel: None,
            },
        )
        .unwrap();
    record
        .record_output(NetworkOutputEventV2 {
            channel,
            event: NetworkOutputKindV2::Shutdown {
                stream_offset: 0,
                direction: NetworkShutdownV2::Read,
            },
        })
        .unwrap();
    let trace = record.into_recorded_versioned_trace().unwrap();
    assert!(matches!(
        trace,
        detcore_model::network_trace::NetworkTrace::V3(_)
    ));
    let mut replay = NetworkReplayEngine::replay_versioned(trace).unwrap();
    replay
        .register_stream_socket(
            f.binding.open_file,
            profile.key,
            crate::network_replay::NetworkStreamNamespace {
                device: 7,
                inode: 11,
            },
            None,
        )
        .unwrap();
    replay.bind(f.binding.open_file, channel).unwrap();
    *engine.lock().unwrap() = replay;
    assert_eq!(f.state.native_receive_mode(), None);
    let before_socket = engine
        .lock()
        .unwrap()
        .stream_socket_state(f.binding.open_file)
        .unwrap()
        .unwrap();
    let before_runtime = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let before_turn = f.state.sched.lock().unwrap().turn;
    let before_clock = f.state.global_time.lock().unwrap().as_nanos();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    guest.expose_local_global = false;
    assert!(guest.local_global_state().is_none());
    let call = reverie::syscalls::Shutdown::new()
        .with_fd(f.binding.slot.fd)
        .with_how(libc::SHUT_RD);
    assert_eq!(
        tool.handle_network_io(&mut guest, call.into())
            .await
            .unwrap(),
        0
    );
    assert_shutdown_transaction(
        &guest,
        f.binding.open_file,
        before_socket,
        NetworkShutdownV2::Read,
    );
    let status = engine
        .lock()
        .unwrap()
        .stream_queue_status(f.binding.open_file)
        .unwrap();
    assert_eq!(status.queued_bytes, 0);
    assert!(status.local_read_shutdown);
    assert!(!status.eof);
    assert!(!status.readiness.hangup);
    assert_eq!(status.error, None);
    assert!(!status.ingress_busy);
    assert!(!status.delivery_busy);
    assert_eq!(status.consume_epoch, 0);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    engine.lock().unwrap().finish().unwrap();
}

// The socket return, held file, native worker and join below are actual.
// Provider rows and TCP identity/geometry are controlled component premises.
struct NativeNoStoreFixture {
    fixture: Fixture,
    observed: crate::network_runtime::native_peer::Observation,
    effect: NetworkStreamPhysicalEffect,
}
impl NativeNoStoreFixture {
    async fn new(eof: bool, version: u64, join: bool) -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let (runtime, root, metadata, memory, _) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        let owner = root.owner();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let mut state = GlobalState::initialize(&config, false);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.detpid = Some(owner.thread);
        thread.file_metadata = metadata;
        thread.memory_metadata = memory;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(&root);
        let pages = Pages::new();
        let args = reverie::syscalls::SyscallArgs::new(
            0,
            8192,
            (libc::PROT_READ | libc::PROT_WRITE) as usize,
            (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            -1isize as usize,
            0,
        );
        {
            let mut memory = thread.memory_metadata.lock().unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(
                    &root,
                    Sysno::mmap,
                    args,
                    Event::Returned(pages.at(0) as i64),
                )
                .unwrap();
        }
        let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
        let (mut engine, call, probe, effect) = {
            let scheduler = state.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .unwrap();
            runtime
                .with_foreground_prefix(&prefix, |admission| {
                    Ok(NetworkReplayEngine::controlled_native_receive_pending(
                        owner, admission, &grant,
                    ))
                })
                .unwrap()
        };
        let bytes = Vec::new();
        let mut peer = None;
        let (runtime, observed, _) = HelperCopyBinding::controlled_joined_peek_retaining_peer(
            runtime,
            owner,
            call,
            probe,
            &mut engine,
            &bytes,
            version,
            join,
            eof,
            0,
            Some(&mut peer),
        )
        .await;
        runtime
            .preflight_native_stream(owner, probe, &effect, &observed)
            .unwrap();
        let lease = probe;
        *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
        runtime.retain_foreground_store_fixture_publication(
            owner,
            call,
            state.network_engine.as_ref().unwrap().clone(),
        );
        state.network_runtime = Some(runtime);
        Self {
            observed,
            effect,
            fixture: Fixture {
                state,
                thread,
                root,
                call,
                lease,
                pages,
                bytes,
                tid,
                _peer: peer,
            },
        }
    }
    fn confirm(&self, eof: bool) {
        let f = &self.fixture;
        let completion = self.observed.helper_copy.as_ref().unwrap();
        completion.joined_worker().unwrap();
        assert_eq!(self.observed.raw_return, if eof { 0 } else { -1 });
        assert_eq!(
            self.observed.errno,
            if eof { None } else { Some(libc::EAGAIN) }
        );
        assert!(self.observed.bytes.is_empty());
        assert_eq!(completion.capture().manifest.present, 1);
        assert_eq!(completion.capture().manifest.summary.version, 5);
        assert_eq!(
            completion.capture().manifest.returned,
            if eof { 0 } else { -i64::from(libc::EAGAIN) }
        );
        assert!(completion.capture().units.is_empty());
        assert!(completion.capture().records.is_empty());
        assert!(completion.capture().committed.is_empty());
        assert!(completion.attempts().is_empty());
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .confirm_retained_stream_physical(f.root.owner(), f.lease, &self.observed)
            .expect("actual canonical joined no-unit receive must stage its retained source");
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .confirm_native_stream(f.root.owner(), f.lease, &self.effect, &self.observed)
            .unwrap();
    }
}
impl NativeNoStoreFixture {
    async fn complete(&self) -> crate::network_replay::NoStoreReturn {
        let f = &self.fixture;
        f.state
            .complete_foreground_record_no_store(f.tid, &f.thread, f.call, f.lease)
            .await
            .unwrap()
            .into_outcome()
    }
    fn trace(&self) -> detcore_model::network_trace::NetworkTraceV4 {
        self.fixture
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture()
    }
    fn frontier(&self) -> (u64, u64, u64, u64, bool, bool, usize, usize) {
        let f = &self.fixture;
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .no_store_fixture_frontier(f.root.owner(), f.call)
    }
    async fn assert_release(&mut self) {
        let f = &mut self.fixture;
        let runtime = f.state.network_runtime.as_ref().unwrap();
        assert_eq!(
            runtime.private_receive_lease_fixture_state(f.root.owner(), f.call),
            (0, false, false)
        );
        assert_eq!(
            native_release_rpc(f).await,
            GlobalResponse::Network(Ok(NetworkReply::Unit))
        );
        native_release_peer_eof(f);
        let runtime = f.state.network_runtime.as_ref().unwrap();
        runtime
            .join_foreground_prefix(f.root.clone())
            .await
            .unwrap();
        assert!(
            runtime
                .finish_native_stream_release(f.root.owner(), f.call)
                .is_err()
        );
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .stream_call_open_file(f.root.owner(), f.call)
                .is_err()
        );
    }
}

#[tokio::test]
async fn native_record_no_store_actual_eof_publishes_one_entry_bound_terminal_without_copy_geometry()
 {
    use detcore_model::network_trace::NetworkInputKindV2;
    use detcore_model::network_trace::NetworkReleaseNodeKindV4;
    use detcore_model::network_trace::NetworkShutdownV2;
    for low_water in [1i32, 2] {
        let mut f = NativeNoStoreFixture::new(true, 5, true).await;
        if low_water == 2 {
            let q = &f.fixture;
            let fd = q
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_original_fixture_fd(q.root.owner(), q.call);
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVLOWAT,
                        (&low_water as *const i32).cast(),
                        std::mem::size_of::<i32>() as libc::socklen_t,
                    )
                },
                0
            );
            let mut actual = 0i32;
            let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVLOWAT,
                        (&mut actual as *mut i32).cast(),
                        &mut length,
                    )
                },
                0
            );
            assert_eq!(length as usize, std::mem::size_of::<i32>());
            assert_eq!(actual, low_water);
            // Independent native EOF observation on the same actual retained OFD.
            // This fixture's provider metadata remains an explicit component premise.
            let mut byte = 0xa5u8;
            assert_eq!(
                unsafe {
                    libc::recv(
                        fd,
                        (&mut byte as *mut u8).cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                },
                0
            );
            assert_eq!(byte, 0xa5);
            q.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .change_no_store_fixture(q.root.owner(), q.call, q.lease, 5);
        }
        f.confirm(true); // exact existing confirmation boundary from raw RED
        let before = f.trace();
        let frontier = f.frontier();
        let turn = f.fixture.state.sched.lock().unwrap().turn;
        let now = f.fixture.state.global_time.lock().unwrap().as_nanos();
        let cut = before.release_model.nodes().len() as u64;
        let prerequisites = before
            .entry_frontier(detcore_model::network_trace::NetworkReceiveEntryCutV4(cut))
            .unwrap();
        assert_eq!(
            f.complete().await,
            crate::network_replay::NoStoreReturn::Eof
        );
        let after = f.trace();
        after.validate().unwrap();
        assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
        assert_eq!(after.inputs.len(), before.inputs.len() + 1);
        let input = after.inputs.last().unwrap();
        assert_eq!(
            input.event,
            NetworkInputKindV2::PeerShutdown {
                stream_offset: 0,
                direction: NetworkShutdownV2::Write
            }
        );
        assert_eq!(input.release.receive_entry_cut.0, cut);
        assert_eq!(input.release.prerequisites, prerequisites);
        assert_eq!(input.release.not_before_global_time, now);
        assert_eq!(
            &after.release_model.nodes()[..cut as usize],
            before.release_model.nodes()
        );
        assert_eq!(after.release_model.nodes().len(), cut as usize + 1);
        assert_eq!(
            after.release_model.nodes().last().unwrap().kind,
            NetworkReleaseNodeKindV4::Input {
                input_ordinal: input.ordinal
            }
        );
        assert_eq!(
            after.native_receive_observations,
            before.native_receive_observations
        );
        assert_eq!(frontier, (0, 0, 0, 0, false, false, 1, 1));
        assert_eq!(f.frontier(), (0, 0, 0, 0, true, true, 0, 0));
        assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(f.fixture.state.sched.lock().unwrap().turn, turn);
        assert_eq!(f.fixture.state.global_time.lock().unwrap().as_nanos(), now);
        assert!(
            f.fixture
                .state
                .complete_foreground_record_no_store(
                    f.fixture.tid,
                    &f.fixture.thread,
                    f.fixture.call,
                    f.fixture.lease
                )
                .await
                .is_err()
        );
        assert_eq!(f.trace(), after);
        f.assert_release().await;
    }
}

#[tokio::test]
async fn native_record_no_store_actual_eagain_completes_local_attempt_without_input() {
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false); // exact existing confirmation boundary from raw RED
    let before = f.trace();
    let turn = f.fixture.state.sched.lock().unwrap().turn;
    let now = f.fixture.state.global_time.lock().unwrap().as_nanos();
    assert_eq!(
        f.complete().await,
        crate::network_replay::NoStoreReturn::WouldBlock
    );
    assert_eq!(f.trace(), before);
    assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(f.fixture.state.sched.lock().unwrap().turn, turn);
    assert_eq!(f.fixture.state.global_time.lock().unwrap().as_nanos(), now);
    assert!(
        f.fixture
            .state
            .complete_foreground_record_no_store(
                f.fixture.tid,
                &f.fixture.thread,
                f.fixture.call,
                f.fixture.lease
            )
            .await
            .is_err()
    );
    assert_eq!(f.trace(), before);
    f.assert_release().await;
}

#[tokio::test]
async fn native_record_no_store_foreign_missing_unjoined_and_malformed_proofs_keep_both_owners() {
    for variant in 0..9 {
        let f =
            NativeNoStoreFixture::new(true, if variant == 7 { 4 } else { 5 }, variant != 6).await;
        let other = NativeNoStoreFixture::new(true, 5, true).await;
        let mut observed = f.observed.clone();
        match variant {
            0 => observed.helper_copy = None,
            1 => {
                observed = serde_json::from_slice(&serde_json::to_vec(&observed).unwrap()).unwrap()
            }
            2 => observed.helper_copy = other.observed.helper_copy.clone(),
            3 => observed.raw_return = 1,
            4 => observed.bytes.push(99),
            5 => observed.confirmation = NetworkStreamPhysicalResult::Errno(libc::EINTR),
            6 => assert!(
                observed
                    .helper_copy
                    .as_ref()
                    .unwrap()
                    .joined_worker()
                    .is_err()
            ),
            7 => assert_eq!(
                observed
                    .helper_copy
                    .as_ref()
                    .unwrap()
                    .capture()
                    .manifest
                    .summary
                    .version,
                4
            ),
            8 => observed.errno = Some(libc::EINTR),
            _ => unreachable!(),
        }
        let q = &f.fixture;
        let engine = q.state.network_engine.as_ref().unwrap();
        let runtime = q.state.network_runtime.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        let custody = runtime.private_publication_runtime_fixture_state();
        assert!(
            engine
                .lock()
                .unwrap()
                .confirm_retained_stream_physical(q.root.owner(), q.lease, &observed)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(
            format!("{:?}", engine.lock().unwrap()),
            before,
            "variant {variant}"
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            custody,
            "variant {variant}"
        );
        assert!(
            q.state
                .complete_foreground_record_no_store(q.tid, &q.thread, q.call, q.lease)
                .await
                .is_err()
        );
        q.assert_fenced();
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn native_record_no_store_entry_root_mm_grant_cursor_and_frontier_refusals_are_atomic() {
    for variant in 0..11 {
        // EOF legitimately completes below SO_RCVLOWAT. Exercise the existing
        // option-profile refusal with WouldBlock instead, so that it cannot
        // mask the later independently reached root/MM/grant negatives.
        let eof = variant != 5;
        let f = NativeNoStoreFixture::new(eof, 5, true).await;
        f.confirm(eof);
        let q = &f.fixture;
        let owner = q.root.owner();
        let engine = q.state.network_engine.as_ref().unwrap();
        let runtime = q.state.network_runtime.as_ref().unwrap();
        let source = engine
            .lock()
            .unwrap()
            .record_no_store_source(owner, q.call, q.lease)
            .unwrap();
        runtime
            .join_record_no_store(q.root.clone(), source)
            .await
            .unwrap();
        match variant {
            0..=7 => engine
                .lock()
                .unwrap()
                .change_no_store_fixture(owner, q.call, q.lease, variant),
            8 => runtime.revoke_foreground_lineage(),
            9 => {
                assert_eq!(
                    q.state
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .remove(&owner.thread),
                    Some(owner.mm)
                );
            }
            10 => q
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(&q.root),
            _ => unreachable!(),
        }
        let before = format!("{:?}", engine.lock().unwrap());
        let custody = runtime.private_publication_runtime_fixture_state();
        let primary = q
            .state
            .complete_foreground_record_no_store(q.tid, &q.thread, q.call, q.lease)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{:?}", engine.lock().unwrap()),
            before,
            "variant {variant}"
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            custody,
            "variant {variant}"
        );
        assert_eq!(
            q.state
                .complete_foreground_record_no_store(q.tid, &q.thread, q.call, q.lease)
                .await
                .unwrap_err(),
            primary
        );
        q.assert_fenced();
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn native_record_no_store_native_submission_cannot_be_hidden_by_a_later_join() {
    let f = NativeNoStoreFixture::new(true, 5, true).await;
    f.confirm(true);
    let q = &f.fixture;
    let owner = q.root.owner();
    let engine = q.state.network_engine.as_ref().unwrap();
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let source = engine
        .lock()
        .unwrap()
        .record_no_store_source(owner, q.call, q.lease)
        .unwrap();
    let joined = runtime
        .join_record_no_store(q.root.clone(), source.clone())
        .await
        .unwrap();
    let release = runtime.controlled_foreground_store_worker().await;
    release.send(()).unwrap();
    let later = runtime
        .join_record_no_store(q.root.clone(), source)
        .await
        .unwrap_err();
    assert!(
        later
            .to_string()
            .contains("cannot replace its first joined native prefix")
    );
    let before = format!("{:?}", engine.lock().unwrap());
    let custody = runtime.private_publication_runtime_fixture_state();
    let scheduler = q.state.sched.lock().unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &q.root)
        .unwrap();
    let error = runtime
        .complete_record_no_store(
            joined,
            &mut engine.lock().unwrap(),
            grant.epoch(),
            q.state.global_time.lock().unwrap().as_nanos(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed the joined native submission prefix")
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    assert_eq!(runtime.private_publication_runtime_fixture_state(), custody);
    drop(scheduler);
    q.assert_fenced();
}

#[tokio::test]
async fn native_record_no_store_commit_then_actual_release_clears_both_ledgers_once() {
    for eof in [false, true] {
        let mut f = NativeNoStoreFixture::new(eof, 5, true).await;
        f.confirm(eof);
        let q = &f.fixture;
        let runtime = q.state.network_runtime.as_ref().unwrap();
        let before = runtime.private_publication_runtime_fixture_state();
        assert!(matches!(
            native_release_rpc(q).await,
            GlobalResponse::Network(Err(_))
        ));
        assert_eq!(runtime.private_publication_runtime_fixture_state(), before);
        assert_eq!(
            runtime.private_receive_lease_fixture_state(q.root.owner(), q.call),
            (1, false, false)
        );
        assert_eq!(
            f.complete().await,
            if eof {
                crate::network_replay::NoStoreReturn::Eof
            } else {
                crate::network_replay::NoStoreReturn::WouldBlock
            }
        );
        f.assert_release().await;
        assert!(matches!(
            native_release_rpc(&f.fixture).await,
            GlobalResponse::Network(Err(_))
        ));
        assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

fn no_store_duplicate_actual_original(f: &Fixture) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    let fd = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_receive_original_fixture_fd(f.root.owner(), f.call);
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(duplicate >= 0);
    unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) }
}

async fn no_store_same_ofd_successor(
    f: Fixture,
    original: std::os::fd::OwnedFd,
) -> NativeNoStoreFixture {
    no_store_same_ofd_successor_policy(f, original, None).await
}

async fn no_store_same_ofd_successor_policy(
    mut f: Fixture,
    original: std::os::fd::OwnedFd,
    finite_microseconds: Option<i64>,
) -> NativeNoStoreFixture {
    use std::os::fd::AsRawFd;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let ofd = engine
        .lock()
        .unwrap()
        .stream_call_open_file(owner, f.call)
        .unwrap();
    assert_eq!(
        native_release_rpc(&f).await,
        GlobalResponse::Network(Ok(NetworkReply::Unit))
    );
    if let Some(microseconds) = finite_microseconds {
        // Actual held OFD option, read back before its next original Call.
        // The following ordinary modeled option transaction is still an
        // explicit socket-profile premise, not a claimed provider observation.
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: microseconds,
        };
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    original.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&timeout as *const libc::timeval).cast(),
                    std::mem::size_of_val(&timeout) as libc::socklen_t,
                )
            },
            0
        );
        let mut readback = libc::timeval {
            tv_sec: -1,
            tv_usec: -1,
        };
        let mut length = std::mem::size_of_val(&readback) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    original.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&mut readback as *mut libc::timeval).cast(),
                    &mut length,
                )
            },
            0
        );
        assert_eq!(length, std::mem::size_of_val(&readback) as libc::socklen_t);
        assert!(readback.tv_sec >= 0 && readback.tv_usec >= 0);
        assert!(readback.tv_sec != 0 || readback.tv_usec != 0);
        let mut e = engine.lock().unwrap();
        let control = e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        e.submit_stream_physical(
            owner,
            control,
            NetworkStreamPhysicalEffect::SetSocketOption {
                option: crate::network_replay::NetworkStreamSocketOption::ReceiveTimeout {
                    seconds: readback.tv_sec,
                    microseconds: readback.tv_usec,
                },
            },
        )
        .unwrap();
        e.confirm_stream_physical(
            owner,
            control,
            NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
        )
        .unwrap();
        e.finish_socket_control(
            owner,
            control,
            crate::network_replay::NetworkSocketControlFinish::Unchanged,
        )
        .unwrap();
    }
    // The explicitly held modeled guest FD keeps this actual same OFD alive.
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let (call, control, epoch) = {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let mut e = engine.lock().unwrap();
        let control = e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        let call = e.begin_stream_call(owner, control).unwrap();
        let attempt = e.begin_native_entry_stamp(owner, call.id).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |admission| {
                e.stamp_native_receive_entry(
                    attempt,
                    admission,
                    &grant,
                    f.state.global_time.lock().unwrap().as_nanos(),
                )
                .map_err(std::io::Error::other)
            })
            .unwrap();
        (call, control, grant.epoch())
    };
    let outcome = runtime
        .controlled_recapture_no_store(
            owner,
            call.id,
            original,
            f.state.native_capture_recovery().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        crate::network_replay::NetworkStreamPinOutcome::Acquired
    );
    let admitted = f
        .state
        .complete_private_receive_capture(f.tid, &f.thread, &f.root, epoch, (call, control), outcome)
        .await
        .unwrap();
    f.call = admitted.id;
    if finite_microseconds.is_some() {
        let read = scalar_read(17, f.pages.at(128), 8);
        let invocation = f
            .state
            .controlled_receive_invocation(f.tid, &f.thread, read, admitted, false);
        f.state
            .controlled_bind_receive_policy(&invocation, f.tid, &f.thread, read)
            .unwrap();
    }
    let effect = NetworkStreamPhysicalEffect::Peek { maximum: 1024 };
    f.lease = {
        let mut e = engine.lock().unwrap();

        e
            .begin_shadow_probe(
                owner,
                f.call,
                f.state.global_time.lock().unwrap().as_nanos(),
            )
            .unwrap()
            .lease
    };
    runtime
        .bind_native_stream_lease(owner, f.call, f.lease)
        .unwrap();
    let observed =
        no_store_actual_cursor_effect(&f, NetworkStreamPhysicalEffect::ReadPeekOffset).await;
    let NetworkStreamPhysicalResult::PeekOffset(original_cursor) = observed.confirmation else {
        panic!("native cursor read failed: {observed:?}")
    };
    assert_eq!(original_cursor, actual_no_store_cursor(&f));
    if original_cursor >= 0 {
        no_store_actual_cursor_effect(&f, NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 })
            .await;
        assert_eq!(actual_no_store_cursor(&f), -1);
    }
    engine
        .lock()
        .unwrap()
        .submit_stream_physical(owner, f.lease, effect.clone())
        .unwrap();
    let observed = if finite_microseconds.is_some() {
        // The finite fixture retains a live, empty peer, not EOF. This helper
        // performs the actual same-OFD probe and retains its joined worker;
        // finite_record_fixture's unchanged confirm(false) requires EAGAIN.
        runtime
            .controlled_receive_retry_peek(owner, f.call, f.lease, engine.clone())
            .await
    } else {
        runtime
            .controlled_existing_no_store_peek(owner, f.call, f.lease, engine.clone())
            .await
    }
    .unwrap();
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    NativeNoStoreFixture {
        fixture: f,
        observed,
        effect,
    }
}

#[tokio::test]
async fn native_record_no_store_repeated_eof_keeps_one_terminal_row() {
    for cursor in [-1, 6] {
        let first = NativeNoStoreFixture::new(true, 5, true).await;
        first.confirm(true);
        let duplicate = no_store_duplicate_actual_original(&first.fixture);
        assert_eq!(
            first.complete().await,
            crate::network_replay::NoStoreReturn::Eof
        );
        if cursor >= 0 {
            let q = &first.fixture;
            let fd = q
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_original_fixture_fd(q.root.owner(), q.call);
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_PEEK_OFF,
                        (&cursor as *const i32).cast(),
                        std::mem::size_of::<i32>() as libc::socklen_t,
                    )
                },
                0
            );
            assert_eq!(actual_no_store_cursor(q), cursor);
            q.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .no_store_fixture_set_cursor(q.root.owner(), q.call, cursor);
        }
        let before = first.trace();
        let old_call = first.fixture.call;
        let mut next = no_store_same_ofd_successor(first.fixture, duplicate).await;
        assert_ne!(next.fixture.call, old_call);
        next.confirm(true);
        assert_eq!(actual_no_store_cursor(&next.fixture), -1);
        if cursor >= 0 {
            let q = &next.fixture;
            let engine = q.state.network_engine.as_ref().unwrap();
            let runtime = q.state.network_runtime.as_ref().unwrap();
            let before = format!("{:?}", engine.lock().unwrap());
            let custody = runtime.private_publication_runtime_fixture_state();
            let error = q
                .state
                .complete_foreground_record_no_store(q.tid, &q.thread, q.call, q.lease)
                .await
                .unwrap_err();
            assert!(format!("{error:?}").contains("UnresolvedStreamOperation"));
            assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
            assert_eq!(runtime.private_publication_runtime_fixture_state(), custody);
            no_store_actual_cursor_effect(
                q,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: cursor },
            )
            .await;
            assert_eq!(actual_no_store_cursor(q), cursor);
        }
        assert_eq!(
            next.complete().await,
            crate::network_replay::NoStoreReturn::Eof
        );
        assert_eq!(actual_no_store_cursor(&next.fixture), cursor);
        assert_eq!(next.trace(), before);
        assert_eq!(next.frontier(), (0, 0, 0, 0, true, true, 0, 0));
        assert_eq!(next.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
        next.assert_release().await;
    }
}

#[tokio::test]
async fn native_record_no_store_eof_follows_actual_store_drain_prefix() {
    use detcore_model::network_trace::NetworkInputKindV2;
    use detcore_model::network_trace::NetworkShutdownV2;
    let f = native_release_rpc_fixture().await;
    f._peer
        .as_ref()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let before = f
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    assert_eq!(before.native_receive_observations.len(), 1);
    assert_eq!(before.native_receive_observations[0].length, 8);
    let duplicate = no_store_duplicate_actual_original(&f);
    let bytes = f.bytes.clone();
    let mut next = no_store_same_ofd_successor(f, duplicate).await;
    next.confirm(true);
    assert_eq!(next.frontier(), (8, 1, 8, 8, false, false, 1, 1));
    assert_eq!(
        next.complete().await,
        crate::network_replay::NoStoreReturn::Eof
    );
    let after = next.trace();
    after.validate().unwrap();
    assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
    assert_eq!(after.inputs.len(), before.inputs.len() + 1);
    assert_eq!(
        after.inputs.last().unwrap().event,
        NetworkInputKindV2::PeerShutdown {
            stream_offset: 8,
            direction: NetworkShutdownV2::Write
        }
    );
    assert_eq!(
        after.native_receive_observations,
        before.native_receive_observations
    );
    assert_eq!(next.frontier(), (8, 1, 8, 8, true, true, 0, 0));
    assert_eq!(next.fixture.pages.bytes(128, 8), bytes);
    assert_eq!(next.fixture.pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(
        next.fixture.pages.bytes(136, 8192 - 136),
        vec![0xa5; 8192 - 136]
    );
    next.assert_release().await;
}

fn actual_no_store_cursor(f: &Fixture) -> i32 {
    let fd = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_receive_original_fixture_fd(f.root.owner(), f.call);
    let mut value = 0i32;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&mut value as *mut i32).cast(),
                &mut len,
            )
        },
        0
    );
    assert_eq!(len, std::mem::size_of::<i32>() as libc::socklen_t);
    value
}
async fn no_store_actual_cursor_effect(
    f: &Fixture,
    effect: NetworkStreamPhysicalEffect,
) -> crate::network_runtime::native_peer::Observation {
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .submit_retained_stream_physical(owner, f.lease, effect.clone())
        .unwrap();
    let observed = runtime
        .execute_native_stream(owner, f.lease, effect.clone())
        .await
        .unwrap();
    assert_eq!(observed.raw_return, 0);
    assert_eq!(observed.errno, None);
    assert!(observed.bytes.is_empty());
    assert!(observed.helper_copy.is_none());
    runtime
        .preflight_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, f.lease, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(owner, f.lease, &effect, &observed)
        .unwrap();
    observed
}

// Stage C actual Guest controls. Backend original-entry/range verdicts and
// root/MM/VMA setup are explicit controlled premises; Stage B separately
// qualifies the actual ptrace issuer. No helper/native socket effect is used.
fn scalar_eof_trace(
    bytes: bool,
    future: bool,
    blocked: bool,
) -> (detcore_model::network_trace::NetworkTraceV4, LogicalTime) {
    use detcore_model::network_trace::NetworkInputEventV4;
    use detcore_model::network_trace::NetworkReceiveEntryCutV4;
    use detcore_model::network_trace::NetworkReleaseModelV4;
    use detcore_model::network_trace::NetworkReleaseNodeIdV4;
    use detcore_model::network_trace::NetworkReleaseNodeKindV4;
    use detcore_model::network_trace::NetworkReleaseNodeV4;
    use detcore_model::network_trace::NetworkReleaseV4;
    assert!(!bytes || !blocked);
    let mut trace = if blocked {
        producer_blocked_replay_trace()
    } else {
        NetworkReplayEngine::controlled_replay_two_row_trace()
    };
    if !bytes {
        trace.inputs.truncate(1);
        trace.native_receive_observations.clear();
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.truncate(if blocked { 3 } else { 2 });
    }
    let now = trace.epoch_global_time().unwrap();
    let deadline = now + LogicalTime::from_nanos(if future { 1_000_000_000 } else { 0 });
    let ordinal = trace.inputs.len() as u64;
    let id = trace.release_model.nodes().len() as u64;
    let cut = NetworkReceiveEntryCutV4(id);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: deadline,
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: detcore_model::network_trace::NetworkInputKindV2::PeerShutdown {
            stream_offset: if bytes { 8 } else { 0 },
            direction: detcore_model::network_trace::NetworkShutdownV2::Write,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(id),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    trace.validate().unwrap();
    (trace, deadline)
}
fn scalar_read(fd: i32, destination: u64, count: usize) -> reverie::syscalls::Read {
    reverie::syscalls::Read::new()
        .with_fd(fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(destination as usize))
        .with_len(count)
}
fn assert_scalar_no_store_rpc(guest: &ScalarForegroundGuest<'_>) {
    assert_eq!(
        scalar_foreground_requests(guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "release-begin",
            "release-finish"
        ]
    );
}

async fn finish_scalar_replay_after_owner_exit(
    state: &GlobalState,
    config: &Config,
    tid: Tid,
    owner: NetworkStreamOwner,
) {
    let engine = state.network_engine.as_ref().unwrap();
    // Receive Calls are gone; fixture teardown must also release the actual
    // FD-table owner through the same RPC used by production task exit.
    assert!(matches!(engine.lock().unwrap().finish(),
        Err(NetworkReplayError::FdPublicationProtocol(message))
            if message=="network OFD lifetime protocol: OutstandingOwners"));
    let reply = state
        .receive_rpc(
            tid,
            (
                DetTime::new(config),
                owner.mm,
                GlobalRequest::NetworkOwnerGone,
            ),
        )
        .await;
    assert_eq!(reply, (None, GlobalResponse::NetworkOwnerGone));
    engine.lock().unwrap().finish().unwrap();
}

#[tokio::test]
async fn guest_v4_replay_eof_returns_zero_for_null_protected_and_cross_page_without_store() {
    for variant in 0..3 {
        let (trace, _) = scalar_eof_trace(false, false, false);
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            _,
        ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
        let engine = state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(root.owner(), read)
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let before_turn = state.sched.lock().unwrap().turn;
        let before_clock = state.global_time.lock().unwrap().as_nanos();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        pages.protect_second(libc::PROT_NONE);
        let address = [0, pages.at(4096), pages.at(4092)][variant];
        let result = tool
            .handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, 8))
            .await;
        pages.protect_second(libc::PROT_READ | libc::PROT_WRITE);
        assert!(
            matches!(result, Ok(0)),
            "actual EOF Guest variant{variant}: {result:?}"
        );
        assert_eq!(
            *guest.range_calls.lock().unwrap(),
            [(binding.slot.fd, address, 8)]
        );
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        let after = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file);
        assert_eq!(after.consumed, 0);
        assert!(after.bytes.is_empty());
        assert!(after.eof_offsets.is_empty());
        assert!(after.peer_closed);
        assert_eq!(after.consume_epoch, 1);
        assert_eq!(after.completed, [0, 1, 2]);
        assert_eq!(after.consumed_eof, [1]);
        assert_eq!(after.released, [true, true]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert_scalar_no_store_rpc(&guest);
        assert_replay_release_responses(&guest, &[vec![NetworkChannelId(1)]]);
        finish_scalar_replay_after_owner_exit(&state, &config, tid, root.owner()).await;
    }
}

#[tokio::test]
async fn guest_v4_replay_eof_consumes_after_two_rows_and_repeats_without_new_progress() {
    let (trace, _) = scalar_eof_trace(true, false, false);
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        _,
    ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
    let engine = state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(root.owner(), read)
        .unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let before_turn = state.sched.lock().unwrap().turn;
    let before_clock = state.global_time.lock().unwrap().as_nanos();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let mut terminal = None;
    for (index, (count, expected)) in [(5, 5), (3, 3), (8, 0), (8, 0)].into_iter().enumerate() {
        guest.requests.lock().unwrap().clear();
        guest.responses.lock().unwrap().clear();
        let address = if index < 2 {
            pages.at(128 + index * 16)
        } else {
            0
        };
        assert_eq!(
            tool.handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, count))
                .await
                .unwrap(),
            expected
        );
        assert_scalar_no_store_rpc(&guest);
        assert_replay_release_responses(
            &guest,
            &[if index == 0 {
                vec![NetworkChannelId(1)]
            } else {
                vec![]
            }],
        );
        let after = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file);
        assert_eq!(after.consumed, if index == 0 { 5 } else { 8 });
        if index < 2 {
            assert_eq!(
                after.completed,
                if index == 0 {
                    vec![0, 1, 2]
                } else {
                    vec![0, 1, 2, 3]
                }
            );
            assert_eq!(
                after.bytes,
                if index == 0 {
                    vec![b"fgh".to_vec()]
                } else {
                    vec![]
                }
            );
            assert_eq!(after.eof_offsets, [8]);
            assert!(!after.peer_closed);
            assert!(after.consumed_eof.is_empty());
        } else {
            assert_eq!(after.completed, [0, 1, 2, 3, 4]);
            assert_eq!(after.consumed_eof, [3]);
            assert!(after.bytes.is_empty());
            assert!(after.eof_offsets.is_empty());
            assert!(after.peer_closed);
            assert_eq!(after.consume_epoch, 3);
            if let Some(before) = &terminal {
                assert_eq!(&after, before);
            } else {
                terminal = Some(after);
            }
        }
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
    }
    assert_eq!(pages.bytes(128, 5), b"abcde");
    assert_eq!(pages.bytes(144, 3), b"fgh");
    assert_eq!(pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(pages.bytes(133, 11), vec![0xa5; 11]);
    assert_eq!(pages.bytes(147, 8192 - 147), vec![0xa5; 8192 - 147]);
    assert_eq!(
        *guest.memory_events.lock().unwrap(),
        [
            "access-check",
            "native-write",
            "access-check",
            "native-write"
        ]
    );
    assert_eq!(
        *guest.range_calls.lock().unwrap(),
        [
            (binding.slot.fd, pages.at(128), 5),
            (binding.slot.fd, pages.at(144), 3),
            (binding.slot.fd, 0, 8),
            (binding.slot.fd, 0, 8)
        ]
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
    finish_scalar_replay_after_owner_exit(&state, &config, tid, root.owner()).await;
}

#[tokio::test]
async fn guest_v4_replay_no_store_above_user_range_faults_before_release_or_consumption() {
    for eof in [false, true] {
        let trace = if eof {
            scalar_eof_trace(false, false, false).0
        } else {
            future_replay_trace().0
        };
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            _,
        ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
        let engine = state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(root.owner(), read)
            .unwrap();
        thread
            .with_detfd(binding.slot.fd, |fd| fd.set_nonblocking(true))
            .unwrap();
        let before = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file);
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_runtime = state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state();
        let before_turn = state.sched.lock().unwrap().turn;
        let before_clock = state.global_time.lock().unwrap().as_nanos();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        guest.range_verdict = reverie::OriginalReadRangeVerdict::Fault;
        let address = 0xffff_ffff_ffff_f000;
        let result = tool
            .handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, 8))
            .await;
        assert!(
            matches!(result, Err(reverie::Error::Errno(Errno::EFAULT))),
            "{result:?}"
        );
        assert_eq!(
            *guest.range_calls.lock().unwrap(),
            [(binding.slot.fd, address, 8)]
        );
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .replay_no_store_fixture_state(binding.open_file),
            before
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(
            scalar_foreground_requests(&guest),
            [
                "publication-acquire",
                "publication-empty-release",
                "ordinary-reader",
                "socket-state",
                "reader-finish"
            ]
        );
        assert_eq!(state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert!(engine.lock().unwrap().finish().is_err());
    }
}

#[tokio::test]
async fn guest_v4_replay_positive_selection_keeps_mapped_span_refusal_and_exact_cleanup() {
    for outside in [false, true] {
        let (trace, _) = scalar_eof_trace(true, false, false);
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            _,
        ) = ReplayIssuerFixture::new_trace(false, trace, true).await;
        let engine = state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(root.owner(), read)
            .unwrap();
        let before = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file);
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_runtime = state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state();
        let before_turn = state.sched.lock().unwrap().turn;
        let before_clock = state.global_time.lock().unwrap().as_nanos();
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        let address = if outside { pages.at(8190) } else { 0 };
        let error = tool
            .handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, 8))
            .await
            .unwrap_err();
        let reverie::Error::Tool(error) = error else {
            panic!("mapping refusal is not native EFAULT")
        };
        assert_eq!(
            format!("{error:#}"),
            "shared network engine refused operation: receive destination lacks an authenticated private anonymous span"
        );
        assert_eq!(
            *guest.range_calls.lock().unwrap(),
            [(binding.slot.fd, address, 8)]
        );
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .replay_no_store_fixture_state(binding.open_file),
            before
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_scalar_no_store_rpc(&guest);
        assert_replay_release_responses(&guest, &[vec![]]);
        assert_eq!(state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert!(engine.lock().unwrap().finish().is_err());
    }
    // Positive sensitivity: the original range is 12, but only the selected
    // eight bytes require a mapping. The following four bytes have no recorded
    // arena authority, so validating maximum instead of selected length fails.
    let (trace, _) = scalar_eof_trace(true, false, false);
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        _,
    ) = ReplayIssuerFixture::new_trace(false, trace, true).await;
    let engine = state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(root.owner(), read)
        .unwrap();
    let address = pages.at(8184);
    assert!(
        thread
            .memory_metadata
            .lock()
            .unwrap()
            .original_copy_span(root.owner(), address, 12)
            .is_err()
    );
    assert!(
        thread
            .memory_metadata
            .lock()
            .unwrap()
            .original_copy_span(root.owner(), address, 8)
            .is_ok()
    );
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let before_turn = state.sched.lock().unwrap().turn;
    let before_clock = state.global_time.lock().unwrap().as_nanos();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    assert_eq!(
        tool.handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, 12))
            .await
            .unwrap(),
        8
    );
    assert_eq!(
        *guest.range_calls.lock().unwrap(),
        [(binding.slot.fd, address, 12)]
    );
    assert_eq!(
        *guest.memory_events.lock().unwrap(),
        ["access-check", "native-write"]
    );
    assert_eq!(pages.bytes(0, 8184), vec![0xa5; 8184]);
    assert_eq!(pages.bytes(8184, 8), b"abcdefgh");
    let after = engine
        .lock()
        .unwrap()
        .replay_no_store_fixture_state(binding.open_file);
    assert_eq!(after.consumed, 8);
    assert!(after.bytes.is_empty());
    assert_eq!(after.eof_offsets, [8]);
    assert!(!after.peer_closed);
    assert_eq!(after.consume_epoch, 1);
    assert!(after.consumed_eof.is_empty());
    assert_eq!(after.completed, [0, 1, 2, 3]);
    assert_eq!(after.released, [true, true, true, true]);
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_scalar_no_store_rpc(&guest);
    assert_replay_release_responses(&guest, &[vec![]]);
    assert_eq!(state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
    assert!(
        engine.lock().unwrap().finish().is_err(),
        "released EOF still requires actual no-store consumption"
    );
}

async fn select_future_eof_wait(
    (state, root, binding, call): FutureWaitContext<'_>,
    request: &Resources,
    response: &Ivar<crate::scheduler::SchedResponse>,
    committed: Resources,
    deadline: LogicalTime,
    (pages, events, old_epoch): FutureWaitEvidence<'_>,
) {
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    assert!(state.global_time.lock().unwrap().as_nanos() < deadline);
    let first = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &Ok(committed),
    )
    .await;
    assert!(first.is_err());
    assert!(response.try_read().is_none());
    assert!(
        !state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .contains_tid(owner.thread)
    );
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, root)
            .is_err()
    );
    let parked = engine
        .lock()
        .unwrap()
        .replay_no_store_fixture_state(binding.open_file);
    assert_eq!(parked.completed, [0, 1]);
    assert!(parked.consumed_eof.is_empty());
    assert!(parked.eof_offsets.is_empty());
    assert_eq!(parked.released, [true, false]);
    assert!(!parked.peer_closed);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .unwrap(),
        binding.open_file
    );
    assert_eq!(
        engine.lock().unwrap().next_release_time().unwrap(),
        Some(deadline)
    );
    let idle = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &first,
    )
    .await;
    assert!(idle.is_err());
    assert!(response.try_read().is_none());
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), deadline);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file),
        parked
    );
    let selected =
        crate::scheduler::do_a_turn_blocking(state.sched.clone(), state.global_time.clone(), &idle)
            .await
            .unwrap();
    assert_eq!(&selected, request);
    assert!(matches!(
        response.try_read(),
        Some(crate::scheduler::SchedResponse::Go(None))
    ));
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, root)
            .unwrap()
            .epoch()
            > old_epoch
    );
    let released = engine
        .lock()
        .unwrap()
        .replay_no_store_fixture_state(binding.open_file);
    assert_eq!(released.completed, [0, 1]);
    assert!(released.consumed_eof.is_empty());
    assert_eq!(released.eof_offsets, [0]);
    assert_eq!(released.released, [true, true]);
    assert!(!released.peer_closed);
    assert_eq!(released.consume_epoch, 0);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .unwrap(),
        binding.open_file
    );
    let status = engine
        .lock()
        .unwrap()
        .stream_call_queue_status(owner, call)
        .unwrap();
    assert!(!status.ingress_busy);
    assert!(!status.delivery_busy);
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
}

#[tokio::test]
async fn guest_v4_replay_future_eof_uses_real_wait_then_consumes_exact_input() {
    let (trace, deadline) = scalar_eof_trace(false, true, false);
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        committed,
    ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let old_epoch = state
        .sched
        .lock()
        .unwrap()
        .foreground_native_observation(owner, &root)
        .unwrap()
        .epoch();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let requests = guest.requests.clone();
    let events = guest.memory_events.clone();
    let call;
    {
        let mut pending =
            std::pin::pin!(tool.handle_owned_read(&mut guest, scalar_read(binding.slot.fd, 0, 8)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let (actual, request, response) = actual_replay_wait(&state, &requests, owner, binding);
        call = actual;
        select_future_eof_wait(
            (&state, &root, binding, call), &request, &response, committed, deadline, (&pages, &events, old_epoch),
        )
        .await;
        assert_eq!(pending.await.unwrap(), 0);
    }
    let after = engine
        .lock()
        .unwrap()
        .replay_no_store_fixture_state(binding.open_file);
    assert_eq!(after.completed, [0, 1, 2]);
    assert_eq!(after.consumed_eof, [1]);
    assert!(after.peer_closed);
    assert_eq!(after.consumed, 0);
    assert!(after.bytes.is_empty());
    assert!(after.eof_offsets.is_empty());
    assert_eq!(after.consume_epoch, 1);
    assert_eq!(
        *guest.range_calls.lock().unwrap(),
        [(binding.slot.fd, 0, 8)]
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .is_err()
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), deadline);
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "global-time",
            "release-eligible",
            "global-time",
            "call-wait",
            "global-time",
            "release-eligible",
            "release-begin",
            "release-finish"
        ]
    );
    assert_replay_release_responses(&guest, &[vec![], vec![]]);
    assert_exact_call_release(&guest, call);
    finish_scalar_replay_after_owner_exit(&state, &config, tid, root.owner()).await;
}

#[tokio::test]
async fn guest_v4_replay_eof_blocked_producer_is_not_completed_by_time_or_readiness() {
    let (trace, _) = scalar_eof_trace(false, false, true);
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        committed,
    ) = ReplayIssuerFixture::new_trace(false, trace, false).await;
    let owner = root.owner();
    let engine = state.network_engine.as_ref().unwrap();
    engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
    let before = engine
        .lock()
        .unwrap()
        .replay_no_store_fixture_state(binding.open_file);
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    let requests = guest.requests.clone();
    let responses = guest.responses.clone();
    let events = guest.memory_events.clone();
    let mut pending =
        std::pin::pin!(tool.handle_owned_read(&mut guest, scalar_read(binding.slot.fd, 0, 8)));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let (call, _request, response) = actual_replay_wait(&state, &requests, owner, binding);
    let far = state.global_time.lock().unwrap().as_nanos() + LogicalTime::from_nanos(9_000_000_000);
    assert!(
        engine
            .lock()
            .unwrap()
            .release_eligible(far)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file),
        before
    );
    let first = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &Ok(committed),
    )
    .await;
    assert!(first.is_err());
    assert!(response.try_read().is_none());
    let parked_clock = state.global_time.lock().unwrap().as_nanos();
    assert_eq!(engine.lock().unwrap().next_release_time().unwrap(), None);
    let idle = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &first,
    )
    .await;
    assert!(idle.is_err());
    assert!(response.try_read().is_none());
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), parked_clock);
    assert!(format!("{:?}",state.sched.lock().unwrap()).contains(
        "terminal_deadlock: Some(\"network replay wait cannot be satisfied by time or outbound progress\")"));
    assert!(
        !state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .contains_tid(owner.thread)
    );
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, &root)
            .is_err()
    );
    let status = engine
        .lock()
        .unwrap()
        .stream_call_queue_status(owner, call)
        .unwrap();
    assert!(!status.readiness.readable);
    assert!(!status.eof);
    assert!(!status.ingress_busy);
    assert!(!status.delivery_busy);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file),
        before
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, call)
            .unwrap(),
        binding.open_file
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (1, 0, 0, 1)
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(requests.lock().unwrap().len(), 8);
    assert_eq!(responses.lock().unwrap().len(), 7);
    assert!(engine.lock().unwrap().finish().is_err());
    // Bounded retained logical failure. Dropping this still-pending component
    // future is neither completed runtime cleanup nor a successful execution.
}

#[tokio::test]
async fn guest_v4_replay_no_store_rechecks_root_mm_grant_and_same_call_before_commit() {
    for variant in 0..4 {
        let (trace, _) = scalar_eof_trace(false, false, false);
        let (
            ReplayIssuerFixture {
                config,
                state,
                thread,
                root,
                pages,
                tid,
                binding,
                read,
            },
            _,
        ) = ReplayIssuerFixture::new_trace(false, trace, true).await;
        let owner = root.owner();
        let engine = state.network_engine.as_ref().unwrap();
        engine.lock().unwrap().finish_fd_read(owner, read).unwrap();
        let before = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(binding.open_file);
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let release = runtime.controlled_foreground_store_worker().await;
        let tool: Detcore = Detcore::new(tid, &config);
        let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
        let requests = guest.requests.clone();
        let events = guest.memory_events.clone();
        let call;
        let expected_primary;
        {
            let mut pending = std::pin::pin!(
                tool.handle_owned_read(&mut guest, scalar_read(binding.slot.fd, 0, 8))
            );
            assert!(
                futures::poll!(pending.as_mut()).is_pending(),
                "actual retained worker join must suspend no-store commit"
            );
            assert_eq!(
                requests.lock().unwrap().len(),
                6,
                "no scheduler wait or Store across worker join"
            );
            call = engine.lock().unwrap().replay_no_store_fixture_call(owner);
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .replay_no_store_fixture_state(binding.open_file),
                before
            );
            match variant {
                0 => {
                    assert_eq!(
                        state
                            .registered_exec_mms
                            .lock()
                            .unwrap()
                            .remove(&owner.thread),
                        Some(owner.mm)
                    );
                    assert!(
                        !state
                            .registered_exec_mms
                            .lock()
                            .unwrap()
                            .contains_key(&owner.thread)
                    );
                    expected_primary = "Replay selection changed actual task/MM metadata".into();
                }
                1 => {
                    runtime.revoke_foreground_lineage();
                    assert!(!root.is_current(owner));
                    expected_primary =
                        "foreground joined prefix changed runtime/submission/root".into();
                }
                2 => {
                    // Controlled corruption of the retained epoch only. This
                    // issues no replacement grant, time or response.
                    let mut scheduler = state.sched.lock().unwrap();
                    let turn = scheduler.next_turns.get_mut(&owner.thread).unwrap();
                    let old = turn.protocol.epoch;
                    turn.protocol.epoch = old.checked_add(1).unwrap();
                    assert_ne!(turn.protocol.epoch, old);
                    assert!(
                        scheduler
                            .foreground_native_observation(owner, &root)
                            .is_err()
                    );
                    expected_primary =
                        "foreground ctl lacks unchanged sole native root grant".into();
                }
                3 => {
                    let mut engine = engine.lock().unwrap();
                    engine.begin_stream_call_release(owner, call).unwrap();
                    engine.finish_stream_call_release(owner, call).unwrap();
                    assert!(matches!(engine.stream_call_open_file(owner,call),
                        Err(NetworkReplayError::UnknownStreamCall(id)) if id==call));
                    expected_primary = NetworkReplayError::UnknownStreamCall(call).to_string();
                    assert_eq!(
                        engine.native_capture_fixture_counts(binding.open_file),
                        (0, 0, 0, 0)
                    );
                }
                _ => unreachable!(),
            }
            release.send(()).unwrap();
            let error = pending.await.unwrap_err();
            let reverie::Error::Tool(error) = error else {
                panic!("stale no-store custody must be a typed refusal")
            };
            assert_eq!(
                error.root_cause().to_string(),
                expected_primary,
                "variant{variant}"
            );
            let mut expected_chain = vec![];
            if variant == 3 {
                expected_chain.push(format!("secondary network cleanup failure: shared network engine refused operation: {expected_primary}"));
            }
            expected_chain.push("shared network engine refused operation".to_owned());
            expected_chain.push(expected_primary.clone());
            assert_eq!(
                error.chain().map(ToString::to_string).collect::<Vec<_>>(),
                expected_chain,
                "variant{variant}"
            );
        }
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            *guest.range_calls.lock().unwrap(),
            [(binding.slot.fd, 0, 8)]
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .replay_no_store_fixture_state(binding.open_file),
            before
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        if variant == 3 {
            assert_eq!(
                scalar_foreground_requests(&guest),
                [
                    "publication-acquire",
                    "publication-empty-release",
                    "ordinary-reader",
                    "socket-state",
                    "global-time",
                    "release-eligible",
                    "release-begin"
                ]
            );
        } else {
            assert_scalar_no_store_rpc(&guest);
            assert_exact_call_release(&guest, call);
        }
        assert!(engine.lock().unwrap().finish().is_err());
    }
}

// Actual Record Guest continuation after a canonical completed native probe.
// The provider metadata/initial capture are the existing explicit component
// premises. This does not claim whole-dispatch pidfd_getfd/provider activation.
async fn record_guest_canonical_fixture(eof: bool, cursor: i32) -> NativeNoStoreFixture {
    let first = NativeNoStoreFixture::new(eof, 5, true).await;
    first.confirm(eof);
    assert_eq!(actual_no_store_cursor(&first.fixture), -1);
    if cursor < 0 {
        return first;
    }
    assert!(eof);
    assert_eq!(cursor, 6);
    let duplicate = no_store_duplicate_actual_original(&first.fixture);
    assert_eq!(
        first.complete().await,
        crate::network_replay::NoStoreReturn::Eof
    );
    let q = &first.fixture;
    let owner = q.root.owner();
    let fd = q
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_receive_original_fixture_fd(owner, q.call);
    assert_eq!(
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&cursor as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        },
        0
    );
    assert_eq!(actual_no_store_cursor(q), cursor);
    q.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .no_store_fixture_set_cursor(owner, q.call, cursor);
    let next = no_store_same_ofd_successor(first.fixture, duplicate).await;
    next.confirm(true);
    assert_eq!(actual_no_store_cursor(&next.fixture), -1);
    next
}

async fn record_guest_after_canonical_probe<'a>(
    f: &'a NativeNoStoreFixture,
    address: u64,
    nonblocking: bool,
    restore: Option<i32>,
) -> (Result<i64, reverie::Error>, ScalarForegroundGuest<'a>) {
    let q = &f.fixture;
    let owner = q.root.owner();
    let ofd = q
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .stream_call_open_file(owner, q.call)
        .unwrap();
    let admitted = crate::network_replay::NetworkStreamCall {
        id: q.call,
        open_file: ofd,
        physical_pin_required: true,
    };
    let tool: Detcore = Detcore::new(q.tid, &q.state.cfg);
    let mut guest = scalar_foreground_guest(&q.state, &q.state.cfg, q.thread.clone(), q.tid);
    if let Some(value) = restore {
        assert_eq!(actual_no_store_cursor(q), -1);
        let reply = guest
            .send_rpc((
                DetTime::new(&q.state.cfg),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
                    lease: q.lease,
                    effect: NetworkStreamPhysicalEffect::SetPeekOffset { value },
                }),
            ))
            .await;
        let (None, GlobalResponse::Network(Ok(NetworkReply::NativeStreamObservation(observed)))) =
            reply
        else {
            panic!("actual original cursor restoration refused: {reply:?}")
        };
        assert_eq!(observed.raw_return, 0);
        assert_eq!(observed.errno, None);
        assert_eq!(observed.confirmation, NetworkStreamPhysicalResult::Unit);
        assert!(observed.bytes.is_empty());
        assert!(observed.helper_copy.is_none());
        assert_eq!(actual_no_store_cursor(q), value);
    }
    let prepared = tool
        .complete_v4_private_probe_observation(q.lease, f.observed.clone())
        .map(Some);
    let result = tool
        .foreground_v4_receive_after_probe(
            &mut guest,
            scalar_read(17, address, 8),
            admitted, (crate::network_replay::NetworkEngineMode::Record,
            nonblocking),
            prepared,
            None,
        )
        .await;
    (result, guest)
}

fn assert_record_guest_pin_released(
    f: &NativeNoStoreFixture,
    guest: &ScalarForegroundGuest<'_>,
    ofd: OpenFileId,
    original: i32,
    audit: &std::os::fd::OwnedFd,
    cursor: i32,
    restore: Option<i32>,
) {
    use std::os::fd::AsRawFd;
    let q = &f.fixture;
    let owner = q.root.owner();
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let mut expected = vec![];
    if let Some(value) = restore {
        expected.push(GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
            lease: q.lease,
            effect: NetworkStreamPhysicalEffect::SetPeekOffset { value },
        }));
    }
    expected.push(GlobalRequest::Network(
        NetworkRequest::NativeReleaseStreamCall { call: q.call },
    ));
    assert_eq!(*guest.requests.lock().unwrap(), expected);
    assert_eq!(
        guest.responses.lock().unwrap().last(),
        Some(&GlobalResponse::Network(Ok(NetworkReply::Unit)))
    );
    assert_eq!(guest.responses.lock().unwrap().len(), expected.len());
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert!(
        guest.range_calls.lock().unwrap().is_empty(),
        "this control begins after the authenticated range/capture boundary"
    );
    assert_eq!(
        q.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_capture_fixture_counts(ofd),
        (0, 0, 0, 0)
    );
    assert!(
        matches!(q.state.network_engine.as_ref().unwrap().lock().unwrap().stream_call_open_file(owner,q.call),
        Err(NetworkReplayError::UnknownStreamCall(id)) if id==q.call)
    );
    assert!(
        runtime.finish_native_stream_release(owner, q.call).is_err(),
        "actual physical completion was already consumed once"
    );
    fd_identity::check_original_socket_released(original, audit).unwrap();
    let mut actual = 0i32;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                audit.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&mut actual as *mut i32).cast(),
                &mut length,
            )
        },
        0
    );
    assert_eq!(length, std::mem::size_of::<i32>() as libc::socklen_t);
    assert_eq!(actual, cursor);
}

#[tokio::test]
async fn guest_v4_record_canonical_eof_post_probe_returns_zero_without_store_and_releases_pin() {
    use detcore_model::network_trace::NetworkInputKindV2;
    use detcore_model::network_trace::NetworkReceiveEntryCutV4;
    use detcore_model::network_trace::NetworkShutdownV2;
    for cursor in [-1, 6] {
        for variant in 0..3 {
            let mut f = record_guest_canonical_fixture(true, cursor).await;
            let q = &f.fixture;
            let owner = q.root.owner();
            let engine = q.state.network_engine.as_ref().unwrap();
            let ofd = engine
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .unwrap();
            let original = q
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_original_fixture_fd(owner, q.call);
            let audit = no_store_duplicate_actual_original(q);
            let before = f.trace();
            let before_frontier = f.frontier();
            let turn = q.state.sched.lock().unwrap().turn;
            let now = q.state.global_time.lock().unwrap().as_nanos();
            let cut = before.release_model.nodes().len() as u64;
            let prerequisites = before
                .entry_frontier(NetworkReceiveEntryCutV4(cut))
                .unwrap();
            let address = [0, q.pages.at(4096), q.pages.at(4092)][variant];
            q.pages.protect_second(libc::PROT_NONE);
            let restore = (cursor >= 0).then_some(cursor);
            let (result, guest) =
                record_guest_after_canonical_probe(&f, address, false, restore).await;
            q.pages.protect_second(libc::PROT_READ | libc::PROT_WRITE);
            assert!(
                matches!(result, Ok(0)),
                "actual Record EOF continuation: {result:?}"
            );
            let after = f.trace();
            after.validate().unwrap();
            if before_frontier.4 {
                assert_eq!(after, before);
            } else {
                assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
                assert_eq!(after.inputs.len(), before.inputs.len() + 1);
                assert_eq!(
                    after.inputs.last().unwrap().event,
                    NetworkInputKindV2::PeerShutdown {
                        stream_offset: 0,
                        direction: NetworkShutdownV2::Write
                    }
                );
                assert_eq!(
                    after.inputs.last().unwrap().release.receive_entry_cut.0,
                    cut
                );
                assert_eq!(
                    after.inputs.last().unwrap().release.prerequisites,
                    prerequisites
                );
                assert_eq!(
                    after.inputs.last().unwrap().release.not_before_global_time,
                    now
                );
                assert_eq!(
                    &after.release_model.nodes()[..before.release_model.nodes().len()],
                    before.release_model.nodes()
                );
                assert_eq!(
                    after.release_model.nodes().len(),
                    before.release_model.nodes().len() + 1
                );
            }
            assert_eq!(
                after.native_receive_observations,
                before.native_receive_observations
            );
            assert_record_guest_pin_released(&f, &guest, ofd, original, &audit, cursor, restore);
            assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert_eq!(q.state.sched.lock().unwrap().turn, turn);
            assert_eq!(q.state.global_time.lock().unwrap().as_nanos(), now);
            drop(guest);
            drop(audit);
            native_release_peer_eof(&mut f.fixture);
        }
    }
}

#[tokio::test]
async fn guest_v4_record_canonical_empty_post_probe_returns_local_eagain_or_uninvoked_blocking_refusal()
 {
    for nonblocking in [true, false] {
        for variant in 0..3 {
            let mut f = record_guest_canonical_fixture(false, -1).await;
            let q = &f.fixture;
            let owner = q.root.owner();
            let ofd = q
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .unwrap();
            let original = q
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .private_receive_original_fixture_fd(owner, q.call);
            let audit = no_store_duplicate_actual_original(q);
            let before = f.trace();
            let turn = q.state.sched.lock().unwrap().turn;
            let now = q.state.global_time.lock().unwrap().as_nanos();
            let address = [0, q.pages.at(4096), q.pages.at(4092)][variant];
            q.pages.protect_second(libc::PROT_NONE);
            let (result, guest) =
                record_guest_after_canonical_probe(&f, address, nonblocking, None).await;
            q.pages.protect_second(libc::PROT_READ | libc::PROT_WRITE);
            if nonblocking {
                assert!(
                    matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
                    "actual local nonblocking empty attempt: {result:?}"
                );
            } else {
                // Nearby negative: without the authenticated original invocation
                // a blocking empty attempt fails closed before any wait or rearm.
                let Err(reverie::Error::Tool(error)) = result else {
                    panic!("blocking Record without invocation must refuse")
                };
                assert_eq!(
                    error.to_string(),
                    "shared network engine refused operation: V4 Record blocking retry lacks its authenticated original invocation"
                );
                assert_eq!(error.chain().count(), 1);
            }
            assert_eq!(
                f.trace(),
                before,
                "EAGAIN is a local scheduling outcome and creates no input/progress row"
            );
            assert_record_guest_pin_released(&f, &guest, ofd, original, &audit, -1, None);
            assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert_eq!(q.state.sched.lock().unwrap().turn, turn);
            assert_eq!(q.state.global_time.lock().unwrap().as_nanos(), now);
            drop(guest);
            drop(audit);
            native_release_peer_eof(&mut f.fixture);
        }
    }
}

#[tokio::test]
async fn guest_v4_record_original_range_fault_precedes_native_capture_and_releases_reader() {
    let ReplayIssuerFixture {
        config,
        state,
        thread,
        root,
        pages,
        tid,
        binding,
        read,
    } = ReplayIssuerFixture::new_record().await;
    let engine = state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(root.owner(), read)
        .unwrap();
    let before = engine.lock().unwrap().native_trace_fixture();
    let runtime = state.network_runtime.as_ref().unwrap();
    let physical = runtime.private_publication_runtime_fixture_state();
    let turn = state.sched.lock().unwrap().turn;
    let now = state.global_time.lock().unwrap().as_nanos();
    let tool: Detcore = Detcore::new(tid, &config);
    let mut guest = scalar_foreground_guest(&state, &config, thread, tid);
    guest.range_verdict = reverie::OriginalReadRangeVerdict::Fault;
    let address = 0xffff_ffff_ffff_f000;
    let result = tool
        .handle_owned_read(&mut guest, scalar_read(binding.slot.fd, address, 8))
        .await;
    assert!(
        matches!(result, Err(reverie::Error::Errno(Errno::EFAULT))),
        "{result:?}"
    );
    assert_eq!(
        *guest.range_calls.lock().unwrap(),
        [(binding.slot.fd, address, 8)]
    );
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before);
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        physical
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        scalar_foreground_requests(&guest),
        [
            "publication-acquire",
            "publication-empty-release",
            "ordinary-reader",
            "socket-state",
            "reader-finish"
        ]
    );
    assert_eq!(state.sched.lock().unwrap().turn, turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), now);
}

#[tokio::test]
async fn guest_v4_record_unrestored_cursor_keeps_primary_and_actual_no_store_custody() {
    use std::os::fd::AsRawFd;
    let f = record_guest_canonical_fixture(true, 6).await;
    let q = &f.fixture;
    let owner = q.root.owner();
    assert_eq!(actual_no_store_cursor(q), -1);
    let engine = q.state.network_engine.as_ref().unwrap();
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let before_engine = format!("{:?}", engine.lock().unwrap());
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before = f.trace();
    let (result, guest) = record_guest_after_canonical_probe(&f, 0, false, None).await;
    let Err(reverie::Error::Tool(error)) = result else {
        panic!("unrestored actual cursor must refuse typed completion")
    };
    let primary = NetworkReplayError::UnresolvedStreamOperation(q.lease).to_string();
    assert_eq!(error.root_cause().to_string(), primary);
    assert_eq!(
        error.chain().map(ToString::to_string).collect::<Vec<_>>(),
        [
            format!(
                "secondary network cleanup failure: shared network engine refused operation: {primary}"
            ),
            "shared network engine refused operation".to_owned(),
            primary
        ]
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before_engine);
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(f.trace(), before);
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, q.call),
        (1, false, false)
    );
    assert_eq!(actual_no_store_cursor(q), -1);
    assert!(
        engine
            .lock()
            .unwrap()
            .stream_call_open_file(owner, q.call)
            .is_ok()
    );
    assert_eq!(
        *guest.requests.lock().unwrap(),
        [GlobalRequest::Network(
            NetworkRequest::NativeReleaseStreamCall { call: q.call }
        )]
    );
    assert_eq!(
        *guest.responses.lock().unwrap(),
        [GlobalResponse::Network(Err(NetworkRpcError::internal(
            NetworkReplayError::UnresolvedStreamOperation(q.lease).to_string()
        )))]
    );
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert!(guest.range_calls.lock().unwrap().is_empty());
    assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
    let mut byte = 0u8;
    assert_eq!(
        unsafe {
            libc::recv(
                q._peer.as_ref().unwrap().as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EAGAIN),
        "actual peer has no EOF while original pin remains held"
    );
    // Retained failure evidence only. The source and physical pin are not
    // acknowledged, cleared, replaced, or labelled successfully cleaned up.
}

// Same-Call retry components: actual held sockets, canonical helper observations,
// worker joins, resource requests/selections and Store/Drain. Initial provider
// identity and callback range/flags remain explicit controlled premises.
fn retry_invocation(
    f: &NativeNoStoreFixture,
    nonblocking: bool,
) -> (
    reverie::syscalls::Read,
    crate::tool_global::CheckedReadInvocation,
) {
    retry_invocation_at_fd(f, nonblocking, 17)
}
fn retry_invocation_at_fd(
    f: &NativeNoStoreFixture,
    nonblocking: bool,
    fd: i32,
) -> (
    reverie::syscalls::Read,
    crate::tool_global::CheckedReadInvocation,
) {
    let q = &f.fixture;
    let owner = q.root.owner();
    let ofd = q
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .stream_call_open_file(owner, q.call)
        .unwrap();
    let read = scalar_read(fd, q.pages.at(128), 8);
    let invocation = q.state.controlled_receive_invocation(
        q.tid,
        &q.thread,
        read,
        crate::network_replay::NetworkStreamCall {
            id: q.call,
            open_file: ofd,
            physical_pin_required: true,
        },
        nonblocking,
    );
    (read, invocation)
}
async fn retry_complete(f: &NativeNoStoreFixture) -> crate::network_replay::CompletedNoStore {
    let q = &f.fixture;
    q.state
        .complete_foreground_record_no_store(q.tid, &q.thread, q.call, q.lease)
        .await
        .unwrap()
}
async fn assert_retry_failure_released(
    f: &mut NativeNoStoreFixture,
    failure: Box<crate::tool_global::ReceiveRetryFailure>,
) {
    let primary = failure.primary().clone();
    let q = &mut f.fixture;
    let failure = q.state.cleanup_receive_retry_failure(failure).await;
    assert_eq!(failure.primary(), &primary);
    assert!(
        failure.released(),
        "retry cleanup retained custody: {:?}",
        failure.cleanup_diagnostic()
    );
    assert!(
        failure.cleanup_diagnostic().is_none(),
        "{:?}",
        failure.cleanup_diagnostic()
    );
    native_release_peer_eof(q);
    q.state
        .network_runtime
        .as_ref()
        .unwrap()
        .join_foreground_prefix(q.root.clone())
        .await
        .unwrap();
    assert!(
        q.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .stream_call_open_file(q.root.owner(), q.call)
            .is_err()
    );
    assert!(
        q.state
            .network_runtime
            .as_ref()
            .unwrap()
            .finish_native_stream_release(q.root.owner(), q.call)
            .is_err()
    );
}
async fn retry_next_turn(f: &Fixture, signal: bool) {
    let owner = f.root.owner();
    let old = f
        .state
        .sched
        .lock()
        .unwrap()
        .foreground_native_observation(owner, &f.root)
        .unwrap()
        .epoch();
    let mut guest = scalar_foreground_guest(&f.state, &f.state.cfg, f.thread.clone(), f.tid);
    let request = Resources::new(owner.thread);
    let mut pending = std::pin::pin!(super::super::resource_request(&mut guest, request.clone()));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let mut expected = request.clone();
    if signal {
        // Controlled arrival; actual scheduler response is Signaled. This is
        // neither native signal delivery nor blocked-Read restart evidence.
        expected.insert(
            ResourceID::InboundSignal(SigWrapper::from(Signal::SIGUSR1)),
            Permission::W,
        );
        f.state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&owner.thread)
            .unwrap()
            .req = Ivar::full(Ok(expected.clone()));
    }
    let selected = crate::scheduler::do_a_turn_blocking(
        f.state.sched.clone(),
        f.state.global_time.clone(),
        &Ok(request),
    )
    .await
    .unwrap();
    assert_eq!(selected, expected);
    let resumed = pending.await;
    if signal {
        assert!(matches!(resumed, ResumeStatus::Signaled(_)));
        assert!(
            f.state
                .sched
                .lock()
                .unwrap()
                .foreground_native_observation(owner, &f.root)
                .is_err()
        );
    } else {
        assert_eq!(resumed, ResumeStatus::Normal);
        assert!(
            f.state
                .sched
                .lock()
                .unwrap()
                .foreground_native_observation(owner, &f.root)
                .unwrap()
                .epoch()
                > old
        );
    }
}
async fn retry_probe(f: &mut NativeNoStoreFixture) {
    let q = &mut f.fixture;
    let owner = q.root.owner();
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let engine = q.state.network_engine.as_ref().unwrap();
    q.lease = engine
        .lock()
        .unwrap()
        .begin_shadow_probe(
            owner,
            q.call,
            q.state.global_time.lock().unwrap().as_nanos(),
        )
        .unwrap()
        .lease;
    runtime
        .bind_native_stream_lease(owner, q.call, q.lease)
        .unwrap();
    let original = actual_no_store_cursor(q);
    let observed =
        no_store_actual_cursor_effect(q, NetworkStreamPhysicalEffect::ReadPeekOffset).await;
    assert_eq!(
        observed.confirmation,
        NetworkStreamPhysicalResult::PeekOffset(original)
    );
    if original >= 0 {
        no_store_actual_cursor_effect(q, NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 })
            .await;
    }
    let effect = NetworkStreamPhysicalEffect::Peek { maximum: 1024 };
    engine
        .lock()
        .unwrap()
        .submit_retained_stream_physical(owner, q.lease, effect.clone())
        .unwrap();
    let observed = runtime
        .controlled_receive_retry_peek(owner, q.call, q.lease, engine.clone())
        .await
        .unwrap();
    observed
        .helper_copy
        .as_ref()
        .unwrap()
        .joined_worker()
        .unwrap();
    runtime
        .preflight_native_stream(owner, q.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, q.lease, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(owner, q.lease, &effect, &observed)
        .unwrap();
    if original >= 0 {
        no_store_actual_cursor_effect(
            q,
            NetworkStreamPhysicalEffect::SetPeekOffset { value: original },
        )
        .await;
    }
    assert_eq!(actual_no_store_cursor(q), original);
    f.effect = effect;
    f.observed = observed;
}
fn assert_initial_retry_refusal(f: &Fixture) {
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    let primary = engine
        .begin_native_entry_stamp(f.root.owner(), f.call)
        .unwrap_err();
    assert!(
        matches!(primary,NetworkReplayError::FdPublicationProtocol(message)
        if message=="native receive entry attempt is one use on its actual Call")
    );
}
async fn retry_publish_bytes(f: &mut NativeNoStoreFixture, bytes: &[u8]) {
    let q = &mut f.fixture;
    let owner = q.root.owner();
    assert_eq!(f.observed.raw_return, bytes.len() as i64);
    assert_eq!(f.observed.errno, None);
    assert_eq!(f.observed.bytes, bytes);
    let permit = q
        .state
        .prepare_private_receive_store(q.tid, &q.thread, q.call, q.lease, 8, q.pages.at(128))
        .await
        .unwrap();
    let mut memory = NativeMemory::new(q.tid.as_raw());
    let (raw, full) = permit.copy(q.tid, &q.thread, &mut memory).unwrap();
    assert_eq!(raw, StoreOutcome::Returned(Ok(bytes.len())));
    let full = full.unwrap();
    assert_eq!((memory.checks.get(), memory.writes), (1, 1));
    q.lease = full.store().lease();
    let engine = q.state.network_engine.as_ref().unwrap();
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let effect = engine.lock().unwrap().begin_private_drain(&full).unwrap();
    let observed = runtime
        .controlled_private_drain(full.clone(), engine.clone(), 5, "none", true)
        .await
        .unwrap();
    runtime
        .preflight_native_stream(owner, q.lease, &effect, &observed)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .confirm_retained_stream_physical(owner, q.lease, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(owner, q.lease, &effect, &observed)
        .unwrap();
    let prepared = q
        .state
        .prepare_foreground_receive_publication(&full)
        .unwrap();
    assert_eq!(
        q.state
            .publish_foreground_native_receive(&prepared)
            .unwrap(),
        bytes.len()
    );
    assert_eq!(q.pages.bytes(128, bytes.len()), bytes);
    assert_eq!(q.pages.bytes(0, 128), vec![0xa5; 128]);
    assert_eq!(
        q.pages.bytes(128 + bytes.len(), 8192 - 128 - bytes.len()),
        vec![0xa5; 8192 - 128 - bytes.len()]
    );
    assert_eq!(
        runtime.private_receive_lease_fixture_state(owner, q.call),
        (0, false, false)
    );
}

#[tokio::test]
async fn private_record_retry_rearms_only_completed_eagain_on_same_call() {
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false);
    let (read, mut invocation) = retry_invocation(&f, false);
    let call = f.fixture.call;
    let fd = f
        .fixture
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_receive_original_fixture_fd(f.fixture.root.owner(), call);
    let before = f.trace();
    let completed = retry_complete(&f).await;
    assert_eq!(f.trace(), before);
    assert_initial_retry_refusal(&f.fixture);
    retry_next_turn(&f.fixture, false).await;
    let q = &f.fixture;
    q.state
        .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
        .await
        .unwrap();
    assert_initial_retry_refusal(q);
    assert_eq!(q.call, call);
    assert_eq!(
        q.state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_receive_original_fixture_fd(q.root.owner(), call),
        fd
    );
    retry_probe(&mut f).await;
    assert_eq!(f.observed.errno, Some(libc::EAGAIN));
    assert_eq!(f.fixture.call, call);
    assert_eq!(
        retry_complete(&f).await.into_outcome(),
        crate::network_replay::NoStoreReturn::WouldBlock
    );
    assert_eq!(f.trace(), before);
    assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    f.assert_release().await;
}

#[tokio::test]
async fn private_record_retry_repeated_empty_then_bytes_uses_fresh_entries() {
    use std::io::Write;
    for cursor in [-1, 6] {
        let mut f = NativeNoStoreFixture::new(false, 5, true).await;
        f.confirm(false);
        let (read, mut invocation) = retry_invocation(&f, false);
        let before = f.trace();
        let original = f
            .fixture
            .state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_receive_original_fixture_fd(f.fixture.root.owner(), f.fixture.call);
        let mut completed = retry_complete(&f).await;
        if cursor >= 0 {
            // Explicit external fixture setup on the SAME actual OFD. Every
            // subsequent probe obtains its cursor through actual getsockopt.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        original,
                        libc::SOL_SOCKET,
                        libc::SO_PEEK_OFF,
                        (&cursor as *const i32).cast(),
                        std::mem::size_of::<i32>() as libc::socklen_t,
                    )
                },
                0
            );
            f.fixture
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .no_store_fixture_set_cursor(f.fixture.root.owner(), f.fixture.call, cursor);
        }
        for _ in 0..2 {
            retry_next_turn(&f.fixture, false).await;
            let q = &f.fixture;
            q.state
                .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
                .await
                .unwrap();
            retry_probe(&mut f).await;
            assert_eq!(f.observed.errno, Some(libc::EAGAIN));
            assert_eq!(actual_no_store_cursor(&f.fixture), cursor);
            completed = retry_complete(&f).await;
            assert_eq!(f.trace(), before);
            assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
            assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
        }
        f.fixture
            ._peer
            .as_mut()
            .unwrap()
            .write_all(b"abcdefgh")
            .unwrap();
        retry_next_turn(&f.fixture, false).await;
        let q = &f.fixture;
        q.state
            .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
            .await
            .unwrap();
        let entry_time = q.state.global_time.lock().unwrap().as_nanos();
        retry_probe(&mut f).await;
        assert_eq!(actual_no_store_cursor(&f.fixture), cursor);
        retry_publish_bytes(&mut f, b"abcdefgh").await;
        let after = f.trace();
        after.validate().unwrap();
        assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
        assert_eq!(after.inputs.len(), before.inputs.len() + 1);
        assert_eq!(
            after.native_receive_observations.len(),
            before.native_receive_observations.len() + 1
        );
        assert_eq!(
            after.inputs.last().unwrap().release.receive_entry_cut.0,
            before.release_model.nodes().len() as u64
        );
        assert!(after.inputs.last().unwrap().release.not_before_global_time >= entry_time);
        assert_eq!(f.frontier(), (8, 1, 8, 8, false, false, 0, 0));
        assert_initial_retry_refusal(&f.fixture);
        f.assert_release().await;
    }
}

#[tokio::test]
async fn private_record_retry_empty_then_eof_preserves_terminal_frontier() {
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false);
    let (read, mut invocation) = retry_invocation(&f, false);
    let before = f.trace();
    let completed = retry_complete(&f).await;
    retry_next_turn(&f.fixture, false).await;
    let q = &f.fixture;
    q.state
        .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
        .await
        .unwrap();
    q._peer
        .as_ref()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    retry_probe(&mut f).await;
    assert_eq!(f.observed.raw_return, 0);
    assert_eq!(f.observed.errno, None);
    let completed = retry_complete(&f).await;
    assert!(
        completed.into_record_empty().is_err(),
        "EOF must never grant retry"
    );
    let after = f.trace();
    after.validate().unwrap();
    assert_eq!(after.inputs.len(), before.inputs.len() + 1);
    assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
    assert_eq!(
        after.native_receive_observations,
        before.native_receive_observations
    );
    assert_eq!(f.frontier(), (0, 0, 0, 0, true, true, 0, 0));
    assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    f.assert_release().await;
}

#[tokio::test]
async fn private_record_retry_refuses_stale_wrong_call_eof_and_nonblocking() {
    for variant in 0..4 {
        let mut f = NativeNoStoreFixture::new(variant == 2, 5, true).await;
        f.confirm(variant == 2);
        let (read, mut invocation) = retry_invocation(&f, variant == 3);
        let completed = retry_complete(&f).await;
        let stale = completed.duplicate_retry_fixture();
        let before = f.trace();
        retry_next_turn(&f.fixture, false).await;
        if variant == 3 {
            // The lower stamp requires this private witness. Even a real
            // canonical empty source and new Normal grant cannot issue one
            // from an original nonblocking callback.
            let q = &f.fixture;
            let error = q
                .state
                .controlled_with_checked_blocking_retry(q.tid, &q.thread, read, &invocation, |_| {
                    panic!("nonblocking callback minted lower-stamp authority")
                })
                .err()
                .unwrap();
            assert_eq!(
                error,
                NetworkRpcError::internal(
                    "receive retry lacks a checked blocking invocation and new Normal grant"
                )
            );
        }
        let mut foreign = None;
        let completed = if variant == 1 {
            let other = NativeNoStoreFixture::new(false, 5, true).await;
            other.confirm(false);
            let result = retry_complete(&other).await;
            foreign = Some(other);
            result
        } else {
            completed
        };
        if variant == 0 {
            let q = &f.fixture;
            q.state
                .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
                .await
                .unwrap();
            let error = q
                .state
                .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, stale)
                .await
                .unwrap_err();
            assert!(error.primary().to_string().contains("one use"));
        } else {
            let q = &f.fixture;
            let error = q
                .state
                .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
                .await
                .unwrap_err();
            let expected = if variant == 2 {
                "retry requires a committed canonical Record EAGAIN"
            } else {
                "receive retry changed original blocking invocation or completed Call"
            };
            assert!(
                error.primary().to_string().contains(expected),
                "{}",
                error.primary()
            );
            assert!(!error.released());
        }
        assert_eq!(f.trace(), before);
        assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
        f.assert_release().await;
        if let Some(mut other) = foreign {
            other.assert_release().await;
        }
    }
    // A valid private witness for another actual callback must fail at the
    // lower engine transaction, independently of the public resume checks.
    let mut target = NativeNoStoreFixture::new(false, 5, true).await;
    target.confirm(false);
    let mut foreign = NativeNoStoreFixture::new(false, 5, true).await;
    foreign.confirm(false);
    let (foreign_read, foreign_invocation) = retry_invocation(&foreign, false);
    let attempt = retry_complete(&target)
        .await
        .into_record_empty()
        .unwrap()
        .begin()
        .unwrap();
    assert_eq!(
        retry_complete(&foreign).await.into_outcome(),
        crate::network_replay::NoStoreReturn::WouldBlock
    );
    retry_next_turn(&target.fixture, false).await;
    retry_next_turn(&foreign.fixture, false).await;
    let before = target.trace();
    let foreign_before = foreign.trace();
    {
        let q = &target.fixture;
        let other = &foreign.fixture;
        let runtime = q.state.network_runtime.as_ref().unwrap();
        let joined = runtime.join_receive_retry_prefix(&attempt).await.unwrap();
        let scheduler = q.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(q.root.owner(), &q.root)
            .unwrap();
        let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
        let error = other
            .state
            .controlled_with_checked_blocking_retry(
                other.tid,
                &other.thread,
                foreign_read,
                &foreign_invocation,
                |checked| {
                    runtime.with_receive_retry_prefix(&joined, &attempt, |admission| {
                        engine
                            .stamp_native_receive_retry(
                                &attempt,
                                admission,
                                checked,
                                &grant,
                                q.state.global_time.lock().unwrap().as_nanos(),
                            )
                            .map_err(std::io::Error::other)
                    })
                },
            )
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("receive retry lacks its exact checked blocking invocation")
        );
        assert_eq!(attempt.check().unwrap_err().to_string(), error.to_string());
    }
    assert_eq!(target.trace(), before);
    assert_eq!(foreign.trace(), foreign_before);
    assert_eq!(target.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    assert_eq!(target.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(foreign.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_initial_retry_refusal(&target.fixture);
    drop(attempt);
    target.assert_release().await;
    foreign.assert_release().await;
    let (
        ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read,
        },
        _,
    ) = ReplayIssuerFixture::new_trace(false, future_replay_trace().0, false).await;
    let before = state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    let call = state
        .begin_replay_receive_call(tid, &thread, read, pages.at(128), 8)
        .unwrap();
    let crate::network_replay::ReceiveSelection::NoStore(completed) = state
        .prepare_replay_receive(tid, &thread, call.id, 8, pages.at(128), true)
        .await
        .unwrap()
    else {
        panic!("actual untouched Replay emptiness must issue local no-store")
    };
    assert!(
        completed.into_record_empty().is_err(),
        "Replay EAGAIN cannot mint native retry authority"
    );
    for request in [
        NetworkRequest::BeginStreamCallRelease { id: call.id },
        NetworkRequest::FinishStreamCallRelease { id: call.id },
    ] {
        assert_eq!(
            state
                .receive_rpc(
                    tid,
                    (
                        DetTime::new(&config),
                        root.owner().mm,
                        GlobalRequest::Network(request)
                    )
                )
                .await
                .1,
            GlobalResponse::Network(Ok(NetworkReply::Unit))
        );
    }
    assert_eq!(
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture(),
        before
    );
    assert_eq!(
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_capture_fixture_counts(binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn private_record_retry_requires_new_normal_grant_and_same_invocation() {
    for variant in 0..7 {
        let mut f = NativeNoStoreFixture::new(false, 5, true).await;
        f.confirm(false);
        let (read, mut invocation) = retry_invocation(&f, false);
        let completed = retry_complete(&f).await;
        let repeated = completed.duplicate_retry_fixture();
        let before = f.trace();
        let q = &f.fixture;
        if variant != 0 {
            retry_next_turn(q, variant == 1).await;
        }
        let mut changed = q.thread.clone();
        let mut tid = q.tid;
        let mut changed_read = read;
        match variant {
            2 => tid = Tid::from_raw(tid.as_raw() + 1),
            3 => {
                changed.memory_metadata =
                    Arc::new(Mutex::new(q.thread.memory_metadata.lock().unwrap().clone()))
            }
            4 => {
                changed.file_metadata =
                    Arc::new(Mutex::new(q.thread.file_metadata.lock().unwrap().clone()))
            }
            5 => {
                changed.mm_id =
                    crate::types::MmId::initial(crate::types::DetTid::from_raw(q.tid.as_raw() + 1))
            }
            6 => changed_read = read.with_len(7),
            _ => {}
        }
        let failure = q
            .state
            .resume_private_receive_call(tid, &changed, changed_read, &mut invocation, completed)
            .await
            .unwrap_err();
        let primary = failure.primary().clone();
        assert!(!failure.released());
        let again = q
            .state
            .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, repeated)
            .await
            .unwrap_err();
        assert_eq!(
            again.primary(),
            &primary,
            "restoring fixture cannot clear first failure"
        );
        assert_eq!(f.trace(), before);
        assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_retry_failure_released(&mut f, again).await;
    }
}

#[tokio::test]
async fn private_record_retry_first_join_failure_cannot_refresh_prefix() {
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false);
    let completed = retry_complete(&f).await;
    let attempt = completed.into_record_empty().unwrap().begin().unwrap();
    let q = &f.fixture;
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let joined = runtime.join_receive_retry_prefix(&attempt).await.unwrap();
    let release = runtime.controlled_foreground_store_worker().await;
    release.send(()).unwrap();
    let later = runtime
        .join_receive_retry_prefix(&attempt)
        .await
        .unwrap_err();
    assert!(
        later
            .to_string()
            .contains("cannot replace its first native submission prefix")
    );
    let again = runtime
        .with_receive_retry_prefix(&joined, &attempt, |_| Ok(()))
        .unwrap_err();
    assert_eq!(again.to_string(), later.to_string());
    // The actual worker remains in its owner; explicit component joining below
    // is cleanup, never reissuance of the failed prefix or another helper.
    runtime.controlled_join_retry_workers().await;
    assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    drop(attempt);
    f.assert_release().await;
}

#[tokio::test]
async fn private_record_retry_unknown_worker_retains_pin_until_actual_join() {
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false);
    let (read, mut invocation) = retry_invocation(&f, false);
    let completed = retry_complete(&f).await;
    let stale = completed.duplicate_retry_fixture();
    retry_next_turn(&f.fixture, false).await;
    let q = &f.fixture;
    let runtime = q.state.network_runtime.as_ref().unwrap();
    let (release, worker) = runtime
        .controlled_retry_pin_worker(q.root.owner(), q.call)
        .await;
    assert_eq!(
        runtime.controlled_retry_pin_count(q.root.owner(), q.call),
        2
    );
    let before = f.trace();
    {
        let mut pending = std::pin::pin!(q.state.resume_private_receive_call(
            q.tid,
            &q.thread,
            read,
            &mut invocation,
            completed
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        assert_eq!(
            runtime.private_receive_lease_fixture_state(q.root.owner(), q.call),
            (0, false, false)
        );
        assert!(
            runtime
                .release_native_stream(q.root.owner(), q.call)
                .await
                .is_err()
        );
    }
    release.send(()).unwrap();
    worker.join().await;
    assert_eq!(
        runtime.controlled_retry_pin_count(q.root.owner(), q.call),
        1
    );
    let error = q
        .state
        .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, stale)
        .await
        .unwrap_err();
    assert!(
        error
            .primary()
            .to_string()
            .contains("issuer ended before joint admission")
    );
    assert_eq!(
        runtime.controlled_retry_pin_count(q.root.owner(), q.call),
        1
    );
    assert_eq!(f.trace(), before);
    assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    assert_retry_failure_released(&mut f, error).await;
}

#[tokio::test]
async fn private_record_retry_same_ofd_survives_numeric_fd_replacement() {
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut f = NativeNoStoreFixture::new(false, 5, true).await;
    f.confirm(false);
    let duplicate = no_store_duplicate_actual_original(&f.fixture);
    let slot = duplicate.as_raw_fd();
    // The controlled callback receipt binds the real original-OFD alias BEFORE
    // completion and close. This is exactly the numeric operand replaced below.
    let (read, mut invocation) = retry_invocation_at_fd(&f, false, slot);
    assert_eq!(read.fd(), slot);
    let completed = retry_complete(&f).await;
    // Keep the target slot owned while allocating the replacement. Closing it
    // first lets another parallel test allocate it, and dup3 would then close
    // that test's descriptor instead of our original alias.
    let (replacement, mut replacement_peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert_ne!(replacement.as_raw_fd(), slot);
    assert_eq!(
        unsafe { libc::dup3(replacement.as_raw_fd(), slot, libc::O_CLOEXEC) },
        slot
    );
    drop(replacement);
    let replacement = std::os::unix::net::UnixStream::from(duplicate);
    replacement_peer.write_all(b"separate").unwrap();
    f.fixture
        ._peer
        .as_mut()
        .unwrap()
        .write_all(b"original")
        .unwrap();
    retry_next_turn(&f.fixture, false).await;
    let q = &f.fixture;
    q.state
        .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
        .await
        .unwrap();
    retry_probe(&mut f).await;
    assert_eq!(f.observed.bytes, b"original");
    retry_publish_bytes(&mut f, b"original").await;
    let mut replacement = replacement;
    replacement.set_nonblocking(true).unwrap();
    let mut bytes = [0u8; 8];
    replacement.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"separate");
    drop(replacement);
    drop(replacement_peer);
    f.assert_release().await;
}

// Actual dispatcher blocking Record retry: real scheduler turns for the existing
// observation timer, a real nfds=0 ppoll, the real same-Call resume and the
// production re-probe RPCs. The callback range/flags remain the controlled premise.
async fn record_blocking_dispatch<'a>(
    f: &'a NativeNoStoreFixture,
    signal: bool,
    between: impl FnOnce(&Fixture),
) -> (
    Result<i64, reverie::Error>,
    ScalarForegroundGuest<'a>,
    crate::resources::ExternalOpId,
) {
    let q = &f.fixture;
    let owner = q.root.owner();
    let ofd = q
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .stream_call_open_file(owner, q.call)
        .unwrap();
    let admitted = crate::network_replay::NetworkStreamCall {
        id: q.call,
        open_file: ofd,
        physical_pin_required: true,
    };
    let (read, invocation) = retry_invocation(f, false);
    let tool: Detcore = Detcore::new(q.tid, &q.state.cfg);
    let mut guest = scalar_foreground_guest(&q.state, &q.state.cfg, q.thread.clone(), q.tid);
    guest.enable_record_timer();
    guest.record_timer_eintr = signal;
    let operation = crate::resources::ExternalOpId::new(owner.thread, q.thread.stats.syscall_count);
    let prepared = tool
        .complete_v4_private_probe_observation(q.lease, f.observed.clone())
        .map(Some);
    let result = {
        let mut pending = std::pin::pin!(tool.foreground_v4_receive_after_probe(
            &mut guest,
            read,
            admitted, (crate::network_replay::NetworkEngineMode::Record,
            false),
            prepared,
            Some(invocation)
        ));
        assert!(
            futures::poll!(pending.as_mut()).is_pending(),
            "blocking empty Record must wait, not refuse"
        );
        {
            let sched = q.state.sched.lock().unwrap();
            let request = sched.next_turns[&owner.thread]
                .req
                .try_read()
                .unwrap()
                .unwrap();
            assert_eq!(request.resources.len(), 1);
            assert!(
                request
                    .resources
                    .contains_key(&ResourceID::BlockingNetworkCapture(operation))
            );
            assert_eq!(
                request.signal_interrupt_errno(),
                Some(Errno::ERESTARTSYS.into_raw())
            );
        }
        let selected = q.state.sched.lock().unwrap().select_test_turn().unwrap();
        assert_eq!(selected.0, owner.thread);
        assert!(
            crate::scheduler::finish_selected_turn(
                q.state.sched.clone(),
                q.state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2
            )
            .await
            .is_err(),
            "capture grant parks outside the run queue"
        );
        {
            between(q);
            assert!(
                futures::poll!(pending.as_mut()).is_pending(),
                "timer continuation waits for its grant"
            );
            {
                let mut sched = q.state.sched.lock().unwrap();
                let request = sched.next_turns[&owner.thread]
                    .req
                    .try_read()
                    .unwrap()
                    .unwrap();
                assert_eq!(request.resources.len(), 1);
                assert!(
                    request
                        .resources
                        .contains_key(&ResourceID::BlockedExternalContinue(operation))
                );
                assert!(sched.harvest_external_io_for_test().is_ok());
            }
            let selected = q.state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(
                crate::scheduler::finish_selected_turn(
                    q.state.sched.clone(),
                    q.state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await
                .is_ok()
            );
            pending.await
        }
    };
    (result, guest, operation)
}

fn record_blocking_request_names(
    guest: &ScalarForegroundGuest<'_>,
    operation: crate::resources::ExternalOpId,
) -> Vec<&'static str> {
    guest
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| match request {
            GlobalRequest::RequestResources(resources, _)
                if resources
                    .resources
                    .contains_key(&ResourceID::BlockingNetworkCapture(operation)) =>
            {
                "timer-begin"
            }
            GlobalRequest::RequestResources(resources, _)
                if resources
                    .resources
                    .contains_key(&ResourceID::BlockedExternalContinue(operation)) =>
            {
                "timer-continue"
            }
            GlobalRequest::GlobalTimeLowerBound => "global-time",
            GlobalRequest::Network(NetworkRequest::BeginShadowProbe { .. }) => "probe-begin",
            GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
                effect: NetworkStreamPhysicalEffect::ReadPeekOffset,
                ..
            }) => "cursor-read",
            GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
                effect: NetworkStreamPhysicalEffect::Peek { .. },
                ..
            }) => "peek",
            GlobalRequest::Network(NetworkRequest::NativeReleaseStreamCall { .. }) => "release",
            other => panic!("unexpected blocking Record retry RPC: {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn guest_v4_record_blocking_empty_waits_then_rearms_same_call_with_fresh_production_probe() {
    use std::io::Write;
    for arrival in 0..3 {
        let f = record_guest_canonical_fixture(false, -1).await;
        let before = f.trace();
        let turn = f.fixture.state.sched.lock().unwrap().turn;
        let old = f.fixture.lease;
        let (result, guest, operation) = record_blocking_dispatch(&f, false, |q| match arrival {
            0 => {}
            1 => q
                ._peer
                .as_ref()
                .unwrap()
                .try_clone()
                .unwrap()
                .write_all(b"abcdefgh")
                .unwrap(),
            _ => q
                ._peer
                .as_ref()
                .unwrap()
                .shutdown(std::net::Shutdown::Write)
                .unwrap(),
        })
        .await;
        // Explicit fixture premise: no provider controller exists, so the
        // production helper PEEK is the first thing this fixture cannot do.
        // Reaching it proves the wait, the same-Call resume and the fresh
        // probe; the end-to-end store is qualified by the provider run.
        let Err(reverie::Error::Tool(error)) = result else {
            panic!("arrival{arrival}: {result:?}")
        };
        assert_eq!(
            error.root_cause().to_string(),
            "helper receive lacks provider controller",
            "arrival{arrival}: {error:#}"
        );
        assert_eq!(
            *guest.memory_events.lock().unwrap(),
            ["timer-reserve", "timer-write", "timer-ppoll"]
        );
        assert_eq!(
            record_blocking_request_names(&guest, operation),
            [
                "global-time",
                "timer-begin",
                "timer-continue",
                "probe-begin",
                "cursor-read",
                "peek",
                "release"
            ],
            "arrival{arrival}"
        );
        let q = &f.fixture;
        let leases: Vec<_> = guest
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match request {
                GlobalRequest::Network(NetworkRequest::NativeStreamEffect { lease, .. }) => {
                    Some(*lease)
                }
                _ => None,
            })
            .collect();
        assert_eq!(leases.len(), 2);
        assert_eq!(leases[0], leases[1]);
        assert_ne!(leases[0], old, "fresh probe lease");
        assert!(
            guest
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| matches!(request,
            GlobalRequest::Network(NetworkRequest::BeginShadowProbe{call}) if *call==q.call)),
            "same Call"
        );
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(f.trace(), before);
        assert_eq!(
            q.state.sched.lock().unwrap().turn,
            turn + 2,
            "timer begin and continuation turns only"
        );
        // Retained failure evidence: the unknown helper PEEK keeps its lease.
    }
}

#[tokio::test]
async fn guest_v4_record_blocking_empty_signal_during_wait_restarts_and_releases_pin() {
    let mut f = record_guest_canonical_fixture(false, -1).await;
    let before = f.trace();
    let (result, guest, operation) = record_blocking_dispatch(&f, true, |_| {}).await;
    assert!(
        matches!(result, Err(reverie::Error::Errno(Errno::ERESTARTSYS))),
        "signal must restart the read: {result:?}"
    );
    let q = &f.fixture;
    assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(
        *guest.memory_events.lock().unwrap(),
        ["timer-reserve", "timer-write", "timer-ppoll"]
    );
    assert_eq!(
        record_blocking_request_names(&guest, operation),
        ["global-time", "timer-begin", "timer-continue", "release"]
    );
    assert_eq!(f.trace(), before, "a signaled wait records no input");
    assert!(
        q.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .stream_call_open_file(q.root.owner(), q.call)
            .is_err()
    );
    drop(guest);
    native_release_peer_eof(&mut f.fixture);
}

#[tokio::test]
async fn guest_v4_record_blocking_retry_refusal_uses_retry_cleanup_not_second_release() {
    let mut f = record_guest_canonical_fixture(false, -1).await;
    let before = f.trace();
    let (result, guest, operation) = record_blocking_dispatch(&f, false, |q| {
        let owner = q.root.owner();
        assert_eq!(
            q.state
                .registered_exec_mms
                .lock()
                .unwrap()
                .remove(&owner.thread),
            Some(owner.mm)
        );
    })
    .await;
    let Err(reverie::Error::Tool(error)) = result else {
        panic!("changed MM must refuse the retry: {result:?}")
    };
    assert_eq!(
        error.root_cause().to_string(),
        "checked Read changed its actual invocation/root/MM/arguments"
    );
    assert_eq!(
        error.chain().count(),
        2,
        "primary only; retry cleanup released: {error:#}"
    );
    let names = record_blocking_request_names(&guest, operation);
    assert_eq!(
        names,
        ["global-time", "timer-begin", "timer-continue"],
        "no re-probe and no second dispatcher release"
    );
    let q = &f.fixture;
    assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(f.trace(), before);
    assert!(
        q.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .stream_call_open_file(q.root.owner(), q.call)
            .is_err()
    );
    drop(guest);
    native_release_peer_eof(&mut f.fixture);
}

// Uses the existing modeled socket-option transaction, not a new timeout or
// receive-policy constructor. This setup and the regression compile on the
// pre-fix source; the actual owned-Read dispatcher must issue its own policy.
fn finite_replay_fixture_timeout(f: &ReplayIssuerFixture, seconds: i64, microseconds: i64) {
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    engine.finish_fd_read(f.root.owner(), f.read.clone()).unwrap();
    let control = engine.begin_socket_controls(f.root.owner(), vec![f.binding.open_file])
        .unwrap()[0].1;
    engine.submit_stream_physical(
        f.root.owner(), control, NetworkStreamPhysicalEffect::SetSocketOption {
            option: crate::network_replay::NetworkStreamSocketOption::ReceiveTimeout {
                seconds, microseconds,
            },
        },
    ).unwrap();
    engine.confirm_stream_physical(
        f.root.owner(), control,
        NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
    ).unwrap();
    engine.finish_socket_control(
        f.root.owner(), control,
        crate::network_replay::NetworkSocketControlFinish::Unchanged,
    ).unwrap();
}

#[tokio::test]
async fn guest_v4_finite_replay_positive_precedes_due_deadline() {
    // Negative seconds normalize to FiniteTicks(0), not Infinite. Available
    // bytes precede this already-due deadline as well as a future finite one.
    for (seconds, microseconds) in [(-1, 0), (0, 5_000)] {
        let f = ReplayIssuerFixture::new().await;
        finite_replay_fixture_timeout(&f, seconds, microseconds);
        let engine = f.state.network_engine.as_ref().unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_runtime = f.state.network_runtime.as_ref().unwrap()
            .private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let read = reverie::syscalls::Read::new()
            .with_fd(f.binding.slot.fd)
            .with_buf(reverie::syscalls::AddrMut::from_raw(f.pages.at(128) as usize))
            .with_len(5);
        let result = tool.handle_owned_read(&mut guest, read).await;
        // Both the before refusal and the fixed completion must release the
        // original reader/Call before the same semantic assertion is reached.
        assert_eq!(engine.lock().unwrap().native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0));
        assert_eq!(f.state.network_runtime.as_ref().unwrap()
            .private_publication_runtime_fixture_state(), before_runtime);
        assert!(matches!(result, Ok(5)), "finite Read must return queued bytes: {result:?}");
        assert_eq!(f.pages.bytes(128, 5), b"abcde");
        assert_eq!(f.pages.bytes(0, 128), vec![0xa5; 128]);
        assert_eq!(f.pages.bytes(133, 8192 - 133), vec![0xa5; 8192 - 133]);
        assert_eq!(engine.lock().unwrap().controlled_replay_delivery_state(f.binding.open_file),
            (5, vec![b"fgh".to_vec()], vec![0, 1, 2]));
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(*guest.memory_events.lock().unwrap(), ["access-check", "native-write"]);
        assert!(!guest.requests.lock().unwrap().iter()
            .any(|request| matches!(request, GlobalRequest::RequestResources(..))));
    }
}

#[tokio::test]
async fn guest_v4_finite_replay_empty_deadlines_repeat_exactly_on_same_trace() {
    let (trace, input_time) = future_replay_trace();
    let mut repetitions = vec![];
    for _ in 0..2 {
        let (f, mut committed) = ReplayIssuerFixture::new_trace(false, trace.clone(), false).await;
        finite_replay_fixture_timeout(&f, 0, 5_000);
        let owner = f.root.owner();
        let engine = f.state.network_engine.as_ref().unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_runtime = f
            .state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let requests = guest.requests.clone();
        let mut trajectory = vec![];
        // A later Read gets a new original deadline. An empty retry of the
        // same Read never does. No timeout/empty row is added to the trace.
        for _ in 0..2 {
            let start = f.state.global_time.lock().unwrap().as_nanos();
            let deadline = start + LogicalTime::from_nanos(5_000_000);
            assert!(deadline < input_time);
            let result;
            let call;
            {
                let mut pending = std::pin::pin!(tool.handle_owned_read(
                    &mut guest,
                    scalar_read(f.binding.slot.fd, f.pages.at(128), 5),
                ));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                let (expected, response) = {
                    let all = requests.lock().unwrap();
                    let GlobalRequest::RequestResources(actual, process) = all.last().unwrap()
                    else {
                        panic!("finite Read did not publish its actual wait: {all:?}");
                    };
                    assert_eq!(*process, owner.thread);
                    assert_eq!(actual.resources.len(), 1);
                    let (resource, permission) = actual.resources.iter().next().unwrap();
                    let ResourceID::NetworkCallWaitSet {
                        interests,
                        deadline: actual_end,
                        zero_wait,
                    } = resource
                    else {
                        panic!("finite Read used a different resource: {resource:?}");
                    };
                    assert_eq!(*permission, Permission::R);
                    assert_eq!(*actual_end, Some(deadline));
                    assert!(zero_wait.is_none());
                    assert_eq!(interests.len(), 1);
                    call = interests[0].0;
                    assert_eq!(
                        interests[0].1,
                        crate::resources::NetworkWaitKind::ReadableAtLeast(1)
                    );
                    let mut expected = Resources::new(owner.thread);
                    expected.insert(resource.clone(), Permission::R);
                    expected.set_signal_interrupt_errno(Errno::EINTR);
                    assert_eq!(*actual, expected);
                    let scheduler = f.state.sched.lock().unwrap();
                    let next = &scheduler.next_turns[&owner.thread];
                    assert_eq!(next.req.try_read().unwrap().unwrap(), expected);
                    assert_eq!(next.protocol.origin.as_ref().unwrap().mm, owner.mm);
                    assert!(next.resp.try_read().is_none());
                    (expected, next.resp.clone())
                };
                let saved = engine
                    .lock()
                    .unwrap()
                    .saved_receive_policy(owner, call)
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.started(), start);
                assert_eq!(saved.deadline(), Some(deadline));
                assert!(!saved.nonblocking());
                let before_engine = format!("{:?}", engine.lock().unwrap());
                assert!(
                    engine
                        .lock()
                        .unwrap()
                        .bind_saved_receive_policy(owner, call, saved.clone())
                        .is_err(),
                    "duplicate policy issuance must refuse"
                );
                let admitted = crate::network_replay::NetworkStreamCall {
                    id: call,
                    open_file: f.binding.open_file,
                    physical_pin_required: false,
                };
                assert!(
                    f.state
                        .saved_receive_policy(
                            f.tid,
                            &f.thread,
                            scalar_read(f.binding.slot.fd + 1, f.pages.at(128), 5),
                            admitted,
                            false
                        )
                        .is_err()
                );
                assert!(
                    f.state
                        .saved_receive_policy(
                            Tid::from_raw(f.tid.as_raw() + 1),
                            &f.thread,
                            scalar_read(f.binding.slot.fd, f.pages.at(128), 5),
                            admitted,
                            false
                        )
                        .is_err()
                );
                assert_eq!(format!("{:?}", engine.lock().unwrap()), before_engine);
                let parked = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &Ok(committed),
                )
                .await;
                assert!(parked.is_err());
                assert!(response.try_read().is_none());
                let idle = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &parked,
                )
                .await;
                assert!(idle.is_err());
                assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), deadline);
                assert!(response.try_read().is_none());
                committed = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &idle,
                )
                .await
                .unwrap();
                assert_eq!(committed, expected);
                assert!(matches!(
                    response.try_read(),
                    Some(crate::scheduler::SchedResponse::Go(None))
                ));
                // These selection negatives require the actual new Normal
                // grant. The parked request correctly has no such authority.
                // Do not resume the original pending callback until all three
                // exact span/flags refusals have left engine state unchanged.
                let before_engine = format!("{:?}", engine.lock().unwrap());
                for (maximum, destination, nonblocking) in [
                    (4, f.pages.at(128), false),
                    (5, f.pages.at(129), false),
                    (5, f.pages.at(128), true),
                ] {
                    let Err(error) = f
                        .state
                        .prepare_replay_receive(
                            f.tid,
                            &f.thread,
                            call,
                            maximum,
                            destination,
                            nonblocking,
                        )
                        .await
                    else {
                        panic!("changed original span/flags acquired a selection");
                    };
                    assert_eq!(
                        error,
                        NetworkRpcError::internal("Replay selection changed saved Read span/flags")
                    );
                }
                assert_eq!(format!("{:?}", engine.lock().unwrap()), before_engine);
                assert!(Arc::ptr_eq(
                    &saved,
                    &engine
                        .lock()
                        .unwrap()
                        .saved_receive_policy(owner, call)
                        .unwrap()
                        .unwrap()
                ));
                result = pending.await;
                assert!(
                    f.state.receive_policy_expired(&saved).is_err(),
                    "released policy is stale"
                );
            }
            // Check actual Call/owner cleanup before the result oracle.
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(f.binding.open_file),
                (0, 0, 0, 0)
            );
            assert!(
                engine
                    .lock()
                    .unwrap()
                    .stream_call_open_file(owner, call)
                    .is_err()
            );
            assert_exact_call_release(&guest, call);
            assert!(
                matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
                "{result:?}"
            );
            let end = f.state.global_time.lock().unwrap().as_nanos();
            assert_eq!(end, deadline);
            trajectory.push((start, deadline, end, Errno::EAGAIN.into_raw()));
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .controlled_replay_delivery_state(f.binding.open_file),
                (0, vec![], vec![0, 1])
            );
            assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert!(guest.memory_events.lock().unwrap().is_empty());
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
            assert_eq!(
                f.state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .private_publication_runtime_fixture_state(),
                before_runtime
            );
            guest.requests.lock().unwrap().clear();
            guest.responses.lock().unwrap().clear();
        }
        repetitions.push(trajectory);
    }
    assert_eq!(
        repetitions[0], repetitions[1],
        "same trace must repeat full deadline/clock/results"
    );
}

#[tokio::test]
async fn guest_v4_finite_replay_immediate_empty_and_eof_keep_exact_no_store_effects() {
    for eof in [false, true] {
        let trace = if eof {
            scalar_eof_trace(false, false, false).0
        } else {
            future_replay_trace().0
        };
        let (f, _) = ReplayIssuerFixture::new_trace(false, trace, false).await;
        finite_replay_fixture_timeout(&f, -1, 0);
        let engine = f.state.network_engine.as_ref().unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_clock = f.state.global_time.lock().unwrap().as_nanos();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let result = tool
            .handle_owned_read(&mut guest, scalar_read(f.binding.slot.fd, 0, 5))
            .await;
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        if eof {
            assert!(matches!(result, Ok(0)), "{result:?}");
        } else {
            assert!(
                matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
                "{result:?}"
            );
        }
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert_scalar_no_store_rpc(&guest);
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        let after = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(f.binding.open_file);
        assert_eq!(after.consumed, 0);
        assert!(after.bytes.is_empty());
        assert_eq!(after.peer_closed, eof);
        assert_eq!(after.consume_epoch, usize::from(eof) as u64);
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        if eof {
            finish_scalar_replay_after_owner_exit(&f.state, &f.config, f.tid, f.root.owner()).await;
        }
    }
}

#[tokio::test]
async fn guest_v4_finite_nonblocking_empty_never_waits_or_advances_clock() {
    let (f, _) = ReplayIssuerFixture::new_trace(false, future_replay_trace().0, false).await;
    finite_replay_fixture_timeout(&f, 0, 5_000);
    f.thread
        .with_detfd(f.binding.slot.fd, |fd| fd.set_nonblocking(true))
        .unwrap();
    let start = f.state.global_time.lock().unwrap().as_nanos();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let result = tool
        .handle_owned_read(&mut guest, scalar_read(f.binding.slot.fd, 0, 5))
        .await;
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert!(
        matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
        "{result:?}"
    );
    assert_scalar_no_store_rpc(&guest);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), start);
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn guest_v4_finite_policy_refuses_low_water_before_available_bytes() {
    let f = ReplayIssuerFixture::new().await;
    finite_replay_fixture_timeout(&f, 0, 5_000);
    let engine = f.state.network_engine.as_ref().unwrap();
    {
        let mut e = engine.lock().unwrap();
        let timeout = e
            .stream_socket_state(f.binding.open_file)
            .unwrap()
            .unwrap()
            .options
            .receive_timeout;
        let control = e
            .begin_socket_controls(f.root.owner(), vec![f.binding.open_file])
            .unwrap()[0]
            .1;
        // The Replay fixture already has ingress. Explicitly user-lock its
        // modeled buffer through the existing setter before normalizing LOWAT;
        // do not bypass the unresolved-autotuning guard or query a host socket.
        for option in [
            crate::network_replay::NetworkStreamSocketOption::ReceiveBuffer(131072),
            crate::network_replay::NetworkStreamSocketOption::ReceiveLowWater(2),
        ] {
            e.submit_stream_physical(
                f.root.owner(),
                control,
                NetworkStreamPhysicalEffect::SetSocketOption { option },
            )
            .unwrap();
            e.confirm_stream_physical(
                f.root.owner(),
                control,
                NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
            )
            .unwrap();
        }
        e.finish_socket_control(
            f.root.owner(),
            control,
            crate::network_replay::NetworkSocketControlFinish::Unchanged,
        )
        .unwrap();
        let socket = e.stream_socket_state(f.binding.open_file).unwrap().unwrap();
        assert!(socket.options.receive_buffer.user_locked);
        assert_eq!(socket.options.receive_low_water, 2);
        assert_eq!(socket.options.receive_timeout, timeout);
    }
    let before = engine
        .lock()
        .unwrap()
        .controlled_replay_delivery_state(f.binding.open_file);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let result = tool
        .handle_owned_read(
            &mut guest,
            scalar_read(f.binding.slot.fd, f.pages.at(128), 5),
        )
        .await;
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    let Err(reverie::Error::Tool(error)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(
        error.root_cause().to_string(),
        "saved receive policy requires low-water one"
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        before
    );
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert!(!guest.requests.lock().unwrap().iter().any(|r| matches!(
        r,
        GlobalRequest::RequestResources(..)
            | GlobalRequest::Network(NetworkRequest::ReleaseEligible(_))
    )));
}

#[tokio::test]
async fn guest_v4_finite_deadline_rejects_sentinel_and_overflow_after_call_cleanup() {
    for excess in 0..3u64 {
        let f = ReplayIssuerFixture::new().await;
        finite_replay_fixture_timeout(&f, 0, 5_000);
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let start = LogicalTime::from_nanos(u64::MAX - 5_000_001 + excess);
        guest.thread.thread_logical_time.advance_to(start);
        f.state.global_time.lock().unwrap().update_global_time(
            f.root.owner().thread,
            start,
            guest.thread.thread_logical_time.inherited_nanos(),
        );
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), start);
        let result = tool
            .handle_owned_read(
                &mut guest,
                scalar_read(f.binding.slot.fd, f.pages.at(128), 5),
            )
            .await;
        let engine = f.state.network_engine.as_ref().unwrap();
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        if excess == 0 {
            assert!(
                matches!(result, Ok(5)),
                "finite MAX-1 must remain valid: {result:?}"
            );
            assert_eq!(f.pages.bytes(128, 5), b"abcde");
        } else {
            let Err(reverie::Error::Tool(error)) = result else {
                panic!("{result:?}");
            };
            assert_eq!(error.root_cause().to_string(), "receive deadline overflow");
            assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert!(guest.memory_events.lock().unwrap().is_empty());
        }
    }
}

async fn finite_record_fixture() -> NativeNoStoreFixture {
    let first = NativeNoStoreFixture::new(false, 5, true).await;
    first.confirm(false);
    let duplicate = no_store_duplicate_actual_original(&first.fixture);
    assert_eq!(
        first.complete().await,
        crate::network_replay::NoStoreReturn::WouldBlock
    );
    let next = no_store_same_ofd_successor_policy(first.fixture, duplicate, Some(5_000)).await;
    next.confirm(false);
    next
}

#[tokio::test]
async fn guest_v4_finite_record_empty_expiry_before_or_during_clock_rpc_does_not_rearm() {
    for during_rpc in [false, true] {
        let mut f = finite_record_fixture().await;
        let q = &f.fixture;
        let owner = q.root.owner();
        let before = f.trace();
        let engine = q.state.network_engine.as_ref().unwrap();
        let saved = engine
            .lock()
            .unwrap()
            .saved_receive_policy(owner, q.call)
            .unwrap()
            .unwrap();
        let deadline = saved.deadline().unwrap();
        let (read, invocation) = retry_invocation(&f, false);
        let admitted = crate::network_replay::NetworkStreamCall {
            id: q.call,
            open_file: engine
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .unwrap(),
            physical_pin_required: true,
        };
        let tool: Detcore = Detcore::new(q.tid, &q.state.cfg);
        let mut guest = scalar_foreground_guest(&q.state, &q.state.cfg, q.thread.clone(), q.tid);
        // A real request carries this thread's unreported logical progress.
        // No fake grant, timer result or synthesized kernel errno is installed.
        guest.thread.thread_logical_time.advance_to(deadline);
        if !during_rpc {
            q.state.global_time.lock().unwrap().update_global_time(
                owner.thread,
                deadline,
                guest.thread.thread_logical_time.inherited_nanos(),
            );
        } else {
            assert!(q.state.global_time.lock().unwrap().as_nanos() < deadline);
        }
        let prepared = tool
            .complete_v4_private_probe_observation(q.lease, f.observed.clone())
            .map(Some);
        let result = tool
            .foreground_v4_receive_after_probe(
                &mut guest,
                read,
                admitted, (crate::network_replay::NetworkEngineMode::Record,
                false),
                prepared,
                Some(invocation),
            )
            .await;
        assert!(
            engine
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .is_err()
        );
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(f.trace(), before);
        let names = record_blocking_request_names(
            &guest,
            crate::resources::ExternalOpId::new(owner.thread, q.thread.stats.syscall_count),
        );
        assert_eq!(
            names,
            if during_rpc {
                vec!["global-time", "release"]
            } else {
                vec!["release"]
            }
        );
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(q.state.global_time.lock().unwrap().as_nanos(), deadline);
        drop(guest);
        native_release_peer_eof(&mut f.fixture);
        assert!(
            matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn guest_v4_finite_record_interrupted_timer_reprobes_before_data_eof_or_errno() {
    for case in 0..6 {
        let mut f = finite_record_fixture().await;
        let q = &f.fixture;
        let owner = q.root.owner();
        let before = f.trace();
        let engine = q.state.network_engine.as_ref().unwrap();
        let (read, invocation) = retry_invocation(&f, false);
        let admitted = crate::network_replay::NetworkStreamCall {
            id: q.call,
            open_file: engine
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .unwrap(),
            physical_pin_required: true,
        };
        let tool: Detcore = Detcore::new(q.tid, &q.state.cfg);
        let mut guest = scalar_foreground_guest(&q.state, &q.state.cfg, q.thread.clone(), q.tid);
        guest.enable_record_timer();
        guest.record_timer_eintr = true; // Explicit modeled EINTR; not native signal delivery.
        guest.controlled_retry_call = Some(q.call);
        if case % 3 == 1 {
            let runtime = q.state.network_runtime.as_ref().unwrap();
            runtime
                .arm_controlled_private_drain(owner, q.call, engine.clone())
                .unwrap();
            assert!(
                runtime
                    .arm_controlled_private_drain(owner, q.call, engine.clone())
                    .is_err()
            );
            assert!(
                !runtime
                    .controlled_private_drain_consumed(owner, q.call)
                    .unwrap()
            );
        }
        if case % 3 != 0 {
            guest.record_timer_arrival = Some((
                q._peer.as_ref().unwrap().try_clone().unwrap(),
                case % 3 == 2,
            ));
        }
        let operation =
            crate::resources::ExternalOpId::new(owner.thread, q.thread.stats.syscall_count);
        let prepared = tool
            .complete_v4_private_probe_observation(q.lease, f.observed.clone())
            .map(Some);
        let result;
        {
            let mut pending = std::pin::pin!(tool.foreground_v4_receive_after_probe(
                &mut guest,
                read,
                admitted, (crate::network_replay::NetworkEngineMode::Record,
                false),
                prepared,
                Some(invocation)
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            {
                let sched = q.state.sched.lock().unwrap();
                let request = sched.next_turns[&owner.thread]
                    .req
                    .try_read()
                    .unwrap()
                    .unwrap();
                assert_eq!(request.resources.len(), 1);
                assert!(
                    request
                        .resources
                        .contains_key(&ResourceID::BlockingNetworkCapture(operation))
                );
                assert_eq!(
                    request.signal_interrupt_errno(),
                    Some(Errno::EINTR.into_raw())
                );
            }
            let selected = q.state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(
                crate::scheduler::finish_selected_turn(
                    q.state.sched.clone(),
                    q.state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await
                .is_err()
            );
            assert!(futures::poll!(pending.as_mut()).is_pending());
            {
                let mut sched = q.state.sched.lock().unwrap();
                let request = sched.next_turns[&owner.thread]
                    .req
                    .try_read()
                    .unwrap()
                    .unwrap();
                assert_eq!(request.resources.len(), 1);
                assert!(
                    request
                        .resources
                        .contains_key(&ResourceID::BlockedExternalContinue(operation))
                );
                assert_eq!(
                    request.signal_interrupt_errno(),
                    Some(Errno::EINTR.into_raw())
                );
                assert!(sched.harvest_external_io_for_test().is_ok());
            }
            if case >= 3 {
                let deadline = engine
                    .lock()
                    .unwrap()
                    .saved_receive_policy(owner, q.call)
                    .unwrap()
                    .unwrap()
                    .deadline()
                    .unwrap();
                let mut time = q.state.global_time.lock().unwrap();
                let now = time.as_nanos();
                assert!(now < deadline);
                time.add_extra_time(deadline.duration_since(now)); // Controlled elapsed-idle premise.
            }
            let selected = q.state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(
                crate::scheduler::finish_selected_turn(
                    q.state.sched.clone(),
                    q.state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await
                .is_ok()
            );
            result = pending.await;
        }
        assert!(
            engine
                .lock()
                .unwrap()
                .stream_call_open_file(owner, q.call)
                .is_err(),
            "finite timer case {case}: Call retained after result {result:?}"
        );
        if case % 3 == 1 {
            let runtime = q.state.network_runtime.as_ref().unwrap();
            assert!(
                runtime
                    .controlled_private_drain_consumed(owner, q.call)
                    .unwrap()
            );
            assert!(
                runtime
                    .arm_controlled_private_drain(owner, q.call, engine.clone())
                    .is_err()
            );
        }
        let after = f.trace();
        if case % 3 == 1 {
            assert_eq!(q.pages.bytes(128, 3), b"abc");
            assert_eq!(q.pages.bytes(0, 128), vec![0xa5; 128]);
            assert_eq!(q.pages.bytes(131, 8192 - 131), vec![0xa5; 8192 - 131]);
            assert_eq!(
                *guest.memory_events.lock().unwrap(),
                [
                    "timer-reserve",
                    "timer-write",
                    "timer-ppoll",
                    "access-check",
                    "native-write"
                ]
            );
        } else {
            assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
            assert_eq!(
                *guest.memory_events.lock().unwrap(),
                ["timer-reserve", "timer-write", "timer-ppoll"]
            );
        }
        assert_eq!(&after.inputs[..before.inputs.len()], before.inputs);
        assert_eq!(
            after.inputs.len(),
            before.inputs.len() + usize::from(case % 3 != 0)
        );
        if case % 3 == 0 {
            assert_eq!(after, before);
        }
        after.validate().unwrap();
        assert_eq!(
            record_blocking_request_names(&guest, operation),
            [
                "global-time",
                "timer-begin",
                "timer-continue",
                "probe-begin",
                "cursor-read",
                "peek",
                "release"
            ]
        );
        drop(guest);
        native_release_peer_eof(&mut f.fixture);
        match case % 3 {
            1 => assert!(
                matches!(result, Ok(3)),
                "partial bytes precede interruption/deadline: {result:?}"
            ),
            2 => assert!(
                matches!(result, Ok(0)),
                "EOF precedes interruption/deadline: {result:?}"
            ),
            _ if case >= 3 => assert!(
                matches!(result, Err(reverie::Error::Errno(Errno::EAGAIN))),
                "{result:?}"
            ),
            _ => assert!(
                matches!(result, Err(reverie::Error::Errno(Errno::EINTR))),
                "{result:?}"
            ),
        }
    }
}

#[tokio::test]
async fn finite_record_two_completed_empties_retain_original_policy_and_deadline() {
    let mut f = finite_record_fixture().await;
    let q = &f.fixture;
    let owner = q.root.owner();
    let engine = q.state.network_engine.as_ref().unwrap().clone();
    let saved = engine
        .lock()
        .unwrap()
        .saved_receive_policy(owner, q.call)
        .unwrap()
        .unwrap();
    let (read, mut invocation) = retry_invocation(&f, false);
    let before = f.trace();
    let completed = retry_complete(&f).await;
    retry_next_turn(q, false).await;
    q.state
        .resume_private_receive_call(q.tid, &q.thread, read, &mut invocation, completed)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &saved,
        &engine
            .lock()
            .unwrap()
            .saved_receive_policy(owner, q.call)
            .unwrap()
            .unwrap()
    ));
    retry_probe(&mut f).await;
    assert_eq!(f.observed.errno, Some(libc::EAGAIN));
    assert_eq!(
        retry_complete(&f).await.into_outcome(),
        crate::network_replay::NoStoreReturn::WouldBlock
    );
    assert_eq!(f.trace(), before);
    assert_eq!(f.frontier(), (0, 0, 0, 0, false, false, 0, 0));
    assert_eq!(f.fixture.pages.bytes(0, 8192), vec![0xa5; 8192]);
    let retained = engine
        .lock()
        .unwrap()
        .saved_receive_policy(owner, f.fixture.call)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&saved, &retained));
    assert_eq!(saved.deadline(), retained.deadline());
    f.assert_release().await;
}

// Controlled signal arrival replaces the actual pending request; only the
// real scheduler issues Signaled. It supplies no Normal observation grant.
async fn select_finite_signal(
    state: &GlobalState,
    root: &Arc<crate::network_runtime::ForegroundRoot>,
) {
    let owner = root.owner();
    let mut inbound = Resources::new(owner.thread);
    inbound.insert(
        ResourceID::InboundSignal(SigWrapper::from(Signal::SIGUSR1)),
        Permission::W,
    );
    inbound.set_signal_interrupt_errno(Errno::EINTR);
    let response = {
        let mut sched = state.sched.lock().unwrap();
        let next = sched.next_turns.get_mut(&owner.thread).unwrap();
        assert_eq!(
            next.req
                .try_read()
                .unwrap()
                .unwrap()
                .signal_interrupt_errno(),
            Some(Errno::EINTR.into_raw())
        );
        next.req = Ivar::full(Ok(inbound.clone()));
        next.resp.clone()
    };
    let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
    assert_eq!(
        crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected.0,
            selected.1,
            selected.2
        )
        .await
        .unwrap(),
        inbound
    );
    assert!(
        matches!(response.try_read(), Some(crate::scheduler::SchedResponse::Signaled(Some(signals)))
        if signals == vec![SigWrapper::from(Signal::SIGUSR1)])
    );
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, root)
            .is_err()
    );
}

#[tokio::test]
async fn guest_v4_finite_actual_signaled_refuses_without_normal_grant_in_record_and_replay() {
    {
        let mut f = finite_record_fixture().await;
        let q = &f.fixture;
        let engine = q.state.network_engine.as_ref().unwrap();
        let before = f.trace();
        let (read, invocation) = retry_invocation(&f, false);
        let admitted = crate::network_replay::NetworkStreamCall {
            id: q.call,
            open_file: engine
                .lock()
                .unwrap()
                .stream_call_open_file(q.root.owner(), q.call)
                .unwrap(),
            physical_pin_required: true,
        };
        let tool: Detcore = Detcore::new(q.tid, &q.state.cfg);
        let mut guest = scalar_foreground_guest(&q.state, &q.state.cfg, q.thread.clone(), q.tid);
        guest.enable_record_timer();
        let prepared = tool
            .complete_v4_private_probe_observation(q.lease, f.observed.clone())
            .map(Some);
        let result;
        {
            let mut pending = std::pin::pin!(tool.foreground_v4_receive_after_probe(
                &mut guest,
                read,
                admitted, (crate::network_replay::NetworkEngineMode::Record,
                false),
                prepared,
                Some(invocation)
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            select_finite_signal(&q.state, &q.root).await;
            result = pending.await;
        }
        assert!(
            engine
                .lock()
                .unwrap()
                .stream_call_open_file(q.root.owner(), q.call)
                .is_err()
        );
        assert_eq!(f.trace(), before);
        assert_eq!(q.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            *guest.memory_events.lock().unwrap(),
            ["timer-reserve", "timer-write"]
        );
        drop(guest);
        native_release_peer_eof(&mut f.fixture);
        let Err(reverie::Error::Tool(error)) = result else {
            panic!("{result:?}");
        };
        assert_eq!(
            error.root_cause().to_string(),
            "shared network engine refused operation: finite receive SignalResume lacks a fresh observation grant"
        );
    }
    {
        let (f, _) = ReplayIssuerFixture::new_trace(false, future_replay_trace().0, false).await;
        finite_replay_fixture_timeout(&f, 0, 5_000);
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = engine.lock().unwrap().native_trace_fixture();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let result;
        {
            let mut pending = std::pin::pin!(tool.handle_owned_read(
                &mut guest,
                scalar_read(f.binding.slot.fd, f.pages.at(128), 5)
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            select_finite_signal(&f.state, &f.root).await;
            result = pending.await;
        }
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before);
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        let Err(reverie::Error::Tool(error)) = result else {
            panic!("{result:?}");
        };
        assert_eq!(
            error.root_cause().to_string(),
            "shared network engine refused operation: finite receive SignalResume lacks a fresh observation grant"
        );
    }
}

#[tokio::test]
async fn guest_v4_finite_replay_input_before_equal_after_deadline_repeats_exactly() {
    for offset in [-1i64, 0, 1] {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let epoch = trace.epoch_global_time().unwrap();
        let deadline = epoch + LogicalTime::from_nanos(5_000_000);
        let input_time = LogicalTime::from_nanos((deadline.as_nanos() as i64 + offset) as u64);
        for input in &mut trace.inputs[1..] {
            input.release.not_before_global_time = input_time;
        }
        trace.validate().unwrap();
        let mut outcomes = vec![];
        for _ in 0..2 {
            let (f, committed) = ReplayIssuerFixture::new_trace(false, trace.clone(), false).await;
            finite_replay_fixture_timeout(&f, 0, 5_000);
            assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), epoch);
            let tool: Detcore = Detcore::new(f.tid, &f.config);
            let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
            let requests = guest.requests.clone();
            let result;
            let call;
            {
                let mut pending = std::pin::pin!(tool.handle_owned_read(
                    &mut guest,
                    scalar_read(f.binding.slot.fd, f.pages.at(128), 5)
                ));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                let expected = {
                    let all = requests.lock().unwrap();
                    let GlobalRequest::RequestResources(request, _) = all.last().unwrap() else {
                        panic!("missing wait");
                    };
                    assert_eq!(request.resources.len(), 1);
                    let (resource, permission) = request.resources.iter().next().unwrap();
                    assert_eq!(*permission, Permission::R);
                    let ResourceID::NetworkCallWaitSet {
                        interests,
                        deadline: end,
                        zero_wait,
                    } = resource
                    else {
                        panic!("wrong wait: {request:?}");
                    };
                    assert_eq!(*end, Some(deadline));
                    assert!(zero_wait.is_none());
                    assert_eq!(interests.len(), 1);
                    call = interests[0].0;
                    assert_eq!(
                        interests[0].1,
                        crate::resources::NetworkWaitKind::ReadableAtLeast(1)
                    );
                    assert_eq!(
                        request.signal_interrupt_errno(),
                        Some(Errno::EINTR.into_raw())
                    );
                    request.clone()
                };
                let parked = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &Ok(committed),
                )
                .await;
                assert!(parked.is_err());
                let idle = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &parked,
                )
                .await;
                assert!(idle.is_err());
                assert_eq!(
                    f.state.global_time.lock().unwrap().as_nanos(),
                    std::cmp::min(input_time, deadline)
                );
                let selected = crate::scheduler::do_a_turn_blocking(
                    f.state.sched.clone(),
                    f.state.global_time.clone(),
                    &idle,
                )
                .await
                .unwrap();
                assert_eq!(selected, expected);
                result = pending.await;
            }
            let engine = f.state.network_engine.as_ref().unwrap();
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(f.binding.open_file),
                (0, 0, 0, 0)
            );
            assert_exact_call_release(&guest, call);
            let raw = match result {
                Ok(n) => n,
                Err(reverie::Error::Errno(errno)) => -i64::from(errno.into_raw()),
                other => panic!("{other:?}"),
            };
            assert_eq!(
                raw,
                if offset <= 0 {
                    5
                } else {
                    -i64::from(libc::EAGAIN)
                }
            );
            if offset <= 0 {
                assert_eq!(f.pages.bytes(128, 5), b"abcde");
            } else {
                assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
            }
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
            outcomes.push((
                raw,
                deadline,
                f.state.global_time.lock().unwrap().as_nanos(),
            ));
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }
}
