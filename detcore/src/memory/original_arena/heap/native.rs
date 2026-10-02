//! Genuine ptrace brk observations and guest bytes with a CONTROLLED root.
//! This does not bootstrap Detcore's provider, exercise its full Store commit,
//! or qualify the HTTP path. The memory operation is the existing Guest API.
use std::sync::Mutex;

use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;

use super::*;

const WRITE_MARKER: u64 = 0x4252_4b01;
const SHRUNK_MARKER: u64 = 0x4252_4b02;
const BYTES: [u8; 8] = *b"heap-914";

#[derive(Default)]
struct Observations {
    brk: Vec<(SyscallArgs, Event)>,
    writes: Vec<(u64, usize)>,
    stale_after_shrink: usize,
    marker_returns: Vec<(u64, i64)>,
    terminal: Vec<(Tid, ExitStatus)>,
}

#[derive(Default)]
struct Log(Mutex<Observations>);

#[reverie::global_tool]
impl GlobalTool for Log {
    type Config = ();
    type Request = ();
    type Response = ();

    async fn receive_rpc(&self, _from: Tid, _message: ()) {}
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct NativeState {
    #[serde(skip)]
    root: Option<Arc<ForegroundRoot>>,
    // Retain the controlled root's metadata and memory through native exit.
    #[serde(skip)]
    _metadata: Option<Arc<Mutex<crate::tool_local::FileMetadata>>>,
    #[serde(skip)]
    memory: Option<Arc<Mutex<MemoryMetadata>>>,
    #[serde(skip)]
    span: Option<OriginalCopySpan>,
}

#[derive(Default)]
struct HeapTool;

#[reverie::tool]
impl Tool for HeapTool {
    type GlobalState = Log;
    type ThreadState = NativeState;

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscalls([Sysno::brk, Sysno::getpid]);
        subscriptions
    }

    fn observe_injected_syscalls(_config: &()) -> bool {
        true
    }
    fn observe_injected_syscall_preparation(_config: &()) -> bool {
        true
    }

    fn on_injected_syscall_observed(
        &self,
        tid: Tid,
        global: &Log,
        state: &mut NativeState,
        nr: Sysno,
        args: SyscallArgs,
        event: Event,
    ) {
        if nr != Sysno::brk {
            return;
        }
        if state.root.is_none() {
            // Only root establishment is controlled. The actual backend owns
            // this tid and supplies every brk event/argument/native result.
            let (root, metadata, memory, _claim) =
                crate::network_runtime::controlled_foreground_root(tid.as_raw());
            state.root = Some(root);
            state._metadata = Some(metadata);
            state.memory = Some(memory);
        }
        state
            .memory
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .observe_original_arena(state.root.as_ref().unwrap(), nr, args, event)
            .expect("actual native brk observation must preserve exact custody");
        global.0.lock().unwrap().brk.push((args, event));
    }

    fn on_backend_thread_terminal(
        &self,
        tid: Tid,
        global: &Log,
        _state: &mut NativeState,
        status: ExitStatus,
    ) {
        global.0.lock().unwrap().terminal.push((tid, status));
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let mut marker = None;
        if matches!(call, Syscall::Getpid(_)) {
            let regs = guest.regs().await;
            if matches!(regs.rdi, WRITE_MARKER | SHRUNK_MARKER) {
                assert_eq!(regs.rdx, BYTES.len() as u64);
                let state = guest.thread_state();
                let root = state.root.as_ref().expect("native brk must precede marker");
                let memory = state.memory.as_ref().unwrap();
                if regs.rdi == WRITE_MARKER {
                    let span = memory
                        .lock()
                        .unwrap()
                        .original_copy_span(root.owner(), regs.rsi, BYTES.len() as u64)
                        .expect("only actual grown pages may back selected bytes");
                    // Do not assert that the unused 102400-byte logical tail
                    // is mapped. This test performs exactly this eight-byte
                    // Guest memory operation, then the guest reads each byte.
                    let count = guest
                        .memory()
                        .write(AddrMut::from_raw(regs.rsi as usize).unwrap(), &BYTES)?;
                    assert_eq!(count, BYTES.len());
                    guest
                        .local_global_state()
                        .unwrap()
                        .0
                        .lock()
                        .unwrap()
                        .writes
                        .push((regs.rsi, count));
                    guest.thread_state_mut().span = Some(span);
                } else {
                    let memory = memory.lock().unwrap();
                    let span = state.span.as_ref().expect("selected store marker ran");
                    assert!(
                        memory
                            .validate_original_copy_span(root.owner(), span)
                            .is_err()
                    );
                    assert!(
                        memory
                            .original_copy_span(root.owner(), regs.rsi, BYTES.len() as u64)
                            .is_err()
                    );
                    guest
                        .local_global_state()
                        .unwrap()
                        .0
                        .lock()
                        .unwrap()
                        .stale_after_shrink += 1;
                }
                marker = Some(regs.rdi);
            }
        }
        // Markers remain genuine getpid operations with the unchanged syscall
        // and native result, not emulated writes or new product support.
        let result = guest.inject(call).await?;
        if let Some(marker) = marker {
            assert_eq!(result, i64::from(guest.pid().as_raw()));
            guest
                .local_global_state()
                .unwrap()
                .0
                .lock()
                .unwrap()
                .marker_returns
                .push((marker, result));
        }
        Ok(result)
    }
}

#[test]
fn native_heap_observer_growth_bytes_and_shrink_with_controlled_root() {
    let (output, global) = reverie_ptrace::testing::test_fn_with_config::<HeapTool, _>(
        || unsafe {
            // The isolated child uses only raw syscalls and volatile accesses
            // until it exits. No allocator may mistake the extra brk pages for
            // its own heap, and no Rust destructor runs after the raw shrink.
            let old = libc::syscall(libc::SYS_brk, 0usize);
            if old <= 0 {
                libc::_exit(51);
            }
            let Some(page) = (old as usize)
                .checked_add(PAGE_SIZE - 1)
                .map(|address| address & !(PAGE_SIZE - 1))
            else {
                libc::_exit(52);
            };
            let Some(end) = page.checked_add(2 * PAGE_SIZE) else {
                libc::_exit(53);
            };
            if libc::syscall(libc::SYS_brk, end) != end as libc::c_long {
                libc::_exit(54);
            }
            let destination = end - BYTES.len();
            if libc::syscall(libc::SYS_getpid, WRITE_MARKER, destination, BYTES.len()) <= 0 {
                libc::_exit(55);
            }
            for (index, expected) in BYTES.iter().enumerate() {
                if std::ptr::read_volatile((destination + index) as *const u8) != *expected {
                    libc::_exit(56);
                }
            }
            if libc::syscall(libc::SYS_brk, old) != old {
                libc::_exit(57);
            }
            if libc::syscall(libc::SYS_getpid, SHRUNK_MARKER, destination, BYTES.len()) <= 0 {
                libc::_exit(58);
            }
            libc::_exit(0);
        },
        (),
        true,
    )
    .expect("real guest brk/store fixture must complete");
    // test_fn has completed the original tracee and reaped it before acceptance.
    assert_eq!(output.status, ExitStatus::Exited(0));
    let observations = global.0.lock().unwrap();
    assert_eq!(observations.brk.len(), 6, "{:?}", observations.brk);
    let [
        (query, Event::Prepared),
        (query_return, Event::Returned(old)),
        (growth, Event::Prepared),
        (growth_return, Event::Returned(end)),
        (shrink, Event::Prepared),
        (shrink_return, Event::Returned(restored)),
    ] = observations.brk.as_slice()
    else {
        panic!("exact real brk phases required");
    };
    assert_eq!(query, query_return);
    assert_eq!(growth, growth_return);
    assert_eq!(shrink, shrink_return);
    assert_eq!(query.arg0, 0);
    assert!(*old > 0);
    assert_eq!(growth.arg0 as i64, *end);
    assert_eq!(shrink.arg0 as i64, *old);
    assert_eq!(restored, old);
    let page = (*old as u64 + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    assert_eq!(*end as u64, page + 2 * PAGE_SIZE as u64);
    assert_eq!(
        observations.writes,
        [(*end as u64 - BYTES.len() as u64, BYTES.len())]
    );
    assert_eq!(observations.stale_after_shrink, 1);
    assert_eq!(observations.terminal.len(), 1);
    let (tid, status) = observations.terminal[0];
    assert_eq!(status, ExitStatus::Exited(0));
    assert_eq!(
        observations.marker_returns,
        [
            (WRITE_MARKER, i64::from(tid.as_raw())),
            (SHRUNK_MARKER, i64::from(tid.as_raw())),
        ]
    );
}
