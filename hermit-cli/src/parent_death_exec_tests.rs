//! First-handler controls using the actual record/replay subtool types.
//!
//! A positive admission stops at the first prehook mutable-state access,
//! before returning a reference or performing the mutation. Real retained-image
//! execution is covered by the producer; this witness does not claim exec success.

use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use crate::recorder::Recorder;
use crate::replayer::Replayer;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct UnknownWrapper;

#[reverie::tool]
impl Tool for UnknownWrapper {
    type GlobalState = GlobalState;
    type ThreadState = ();
}

struct NoStack;

struct NoStackGuard;

impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}

impl reverie::Stack for NoStack {
    type StackGuard = NoStackGuard;

    fn size(&self) -> usize {
        panic!("unexpected stack size")
    }
    fn capacity(&self) -> usize {
        panic!("unexpected stack capacity")
    }
    fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
        panic!("unexpected stack push")
    }
    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        panic!("unexpected stack reservation")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("unexpected stack commit")
    }
}

#[derive(Debug)]
struct PrehookReached;

struct BoundaryGuest<T: Tool<GlobalState = GlobalState>> {
    config: Config,
    original: Syscall,
    enrolled: bool,
    allow_prehook_witness: bool,
    // Admission query, immutable-state access, mutable-state access, memory, RPC, injection,
    // other effects. Nothing after admission is allowed on the refusal path.
    effects: [AtomicUsize; 7],
    tool: PhantomData<T>,
}

impl<T: Tool<GlobalState = GlobalState>> BoundaryGuest<T> {
    fn unexpected(&self, index: usize) -> ! {
        self.effects[index].fetch_add(1, Ordering::SeqCst);
        panic!("unexpected first-handler effect {index}")
    }
}

#[reverie::tool]
impl<T: Tool<GlobalState = GlobalState>> GlobalRPC<GlobalState> for BoundaryGuest<T> {
    async fn send_rpc(
        &self,
        _: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.unexpected(4)
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl<T: Tool<GlobalState = GlobalState>> Guest<T> for BoundaryGuest<T> {
    type Memory = LocalMemory;
    type Stack = NoStack;

    fn parent_death_syscall_preflight(
        &self,
        call: Syscall,
    ) -> Result<reverie::ParentDeathSyscallAdmission, Error> {
        self.effects[0].fetch_add(1, Ordering::SeqCst);
        assert_eq!(call.into_parts(), self.original.into_parts());
        Ok(if self.enrolled {
            reverie::ParentDeathSyscallAdmission::Admitted
        } else {
            reverie::ParentDeathSyscallAdmission::Unenrolled
        })
    }

    fn tid(&self) -> Pid {
        Pid::from_raw(i32::MAX)
    }
    fn pid(&self) -> Pid {
        self.tid()
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        self.unexpected(3)
    }
    fn thread_state_mut(&mut self) -> &mut T::ThreadState {
        self.effects[2].fetch_add(1, Ordering::SeqCst);
        assert!(self.allow_prehook_witness, "refused exec entered prehook");
        // The existing prehook resets its per-call flag through this accessor.
        // Stop before returning &mut state, so the reset itself never executes.
        std::panic::panic_any(PrehookReached)
    }
    fn thread_state(&self) -> &T::ThreadState {
        self.unexpected(1)
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        self.unexpected(6)
    }
    async fn stack(&mut self) -> Self::Stack {
        self.unexpected(6)
    }
    async fn daemonize(&mut self) {
        self.unexpected(6)
    }
    async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        self.unexpected(5)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        self.unexpected(5)
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        self.unexpected(6)
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        self.unexpected(6)
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        self.unexpected(6)
    }
}

fn check_boundary<T: Tool<GlobalState = GlobalState>>(
    number: Sysno,
    enrolled: bool,
    kvm: bool,
    refuse: bool,
) {
    let config = Config {
        backend_is_kvm: kvm,
        max_timeslice: None,
        sequentialize_threads: false,
        syscall_clobbers_virtualized_by_backend: true,
        replay_data: Some(std::path::PathBuf::from("unused-parent-death-exec-test")),
        ..Config::default()
    };
    // No live host process is used for Recorder/Replayer output capture. No
    // recording stream is opened: refusal must precede subtool invocation.
    let tool = T::new(Pid::from_raw(i32::MAX), &config);
    let args = if number == Sysno::execve {
        SyscallArgs::new(0x1234, 0x2345, 0x3456, 0, 0, 0)
    } else {
        SyscallArgs::new(libc::AT_FDCWD as usize, 0x1234, 0x2345, 0x3456, 0, 0)
    };
    let call = Syscall::from_raw(number, args);
    let mut guest = BoundaryGuest::<T> {
        config,
        original: call,
        enrolled,
        allow_prehook_witness: !refuse,
        effects: std::array::from_fn(|_| AtomicUsize::new(0)),
        tool: PhantomData,
    };
    let mut future = Box::pin(T::handle_syscall_event(&tool, &mut guest, call));
    let mut context = Context::from_waker(std::task::Waker::noop());
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        std::future::Future::poll(future.as_mut(), &mut context)
    }));
    drop(future);
    if refuse {
        let Ok(Poll::Ready(Err(Error::Tool(error)))) = outcome else {
            panic!("enrolled wrapper exec did not fail at first admission");
        };
        assert_eq!(
            error.to_string(),
            format!(
                "KVM parent-death signal unsupported enrolled exec under record/replay or unknown subtool before {number}"
            )
        );
    } else {
        let Err(witness) = outcome else {
            panic!("supported or unenrolled call did not reach the existing prehook");
        };
        assert!(
            witness.is::<PrehookReached>(),
            "unexpected downstream failure"
        );
    }
    assert_eq!(
        guest
            .effects
            .each_ref()
            .map(|value| value.load(Ordering::SeqCst)),
        [usize::from(kvm), 0, usize::from(!refuse), 0, 0, 0, 0]
    );
}

#[test]
fn parent_death_exec_refuses_real_recorder_and_replayer_before_prehook() {
    for number in [Sysno::execve, Sysno::execveat] {
        check_boundary::<Detcore<Recorder>>(number, true, true, true);
        check_boundary::<Detcore<Replayer>>(number, true, true, true);
    }
}

#[test]
fn parent_death_exec_unknown_wrapper_cannot_inherit_noop_admission() {
    for number in [Sysno::execve, Sysno::execveat] {
        check_boundary::<Detcore<UnknownWrapper>>(number, true, true, true);
    }
}

#[test]
fn parent_death_exec_noop_and_unenrolled_reach_existing_prehook() {
    for number in [Sysno::execve, Sysno::execveat] {
        check_boundary::<Detcore>(number, true, true, false);
        check_boundary::<Detcore>(number, false, true, false);
        check_boundary::<Detcore<Recorder>>(number, false, true, false);
        check_boundary::<Detcore<Replayer>>(number, false, true, false);
        check_boundary::<Detcore<UnknownWrapper>>(number, false, true, false);
    }
}

#[test]
fn parent_death_exec_non_kvm_wrappers_do_not_query_backend_admission() {
    for number in [Sysno::execve, Sysno::execveat] {
        check_boundary::<Detcore<Recorder>>(number, true, false, false);
        check_boundary::<Detcore<Replayer>>(number, true, false, false);
    }
}
