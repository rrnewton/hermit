//! Host protocol adapter around actual Detcore hooks and initialized state.
//! RPC responses and timer replacement are explicit test fixtures, not a native
//! backend or a proof of scheduler replay or runtime ticket reconciliation.

use std::num::NonZeroU64;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::Duration;

use futures::FutureExt as _;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Never;
use reverie::Pid;
use reverie::Stack;
use reverie::TimerSchedule;
use reverie::Tool;
use reverie::UnsupportedGuestProgress;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::tool_local::ThreadState;
use crate::types::DetPid;
use crate::types::LogicalTime;
use crate::types::Op;
use crate::types::SchedEvent;

static RIP_BYTES: [u8; 2] = [0x90, 0x90];

struct UnusedStack;

impl Stack for UnusedStack {
    type StackGuard = Box<()>;

    fn size(&self) -> usize {
        panic!("progress must not allocate guest stack")
    }

    fn capacity(&self) -> usize {
        panic!("progress must not allocate guest stack")
    }

    fn push<'stack, T>(&mut self, _value: T) -> Addr<'stack, T> {
        panic!("progress must not allocate guest stack")
    }

    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        panic!("progress must not allocate guest stack")
    }

    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("progress must not allocate guest stack")
    }
}

struct CapturedGuest {
    config: Config,
    state: ThreadState<()>,
    captured_count: u64,
    clock_reads: usize,
    fail_clock: bool,
    fail_rearm: bool,
    events: Mutex<Vec<SchedEvent>>,
    resource_requests: Mutex<usize>,
    scheduler_time: Option<LogicalTime>,
    timer: Option<(u64, u64, bool)>,
    timer_requests: Vec<(u64, u64, bool)>,
}

impl CapturedGuest {
    fn initialized(trace: bool) -> (Detcore, Self) {
        let config = Config {
            rng_seed: Some(71),
            max_timeslice: NonZeroU64::new(100_000),
            sequentialize_threads: true,
            record_preemptions: trace,
            clock_multiplier: Some(2.0),
            ..Config::default()
        };
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let mut state = tool.init_thread_state(Pid::from_raw(1), None);
        state.detpid = Some(DetPid::from_raw(1));
        state.past_global_first_execve = true;
        let deadline = state.thread_logical_time.as_nanos() + Duration::from_nanos(100_000);
        state.end_of_timeslice = Some(deadline);
        state.max_timeslice_end = Some(deadline);
        (
            tool,
            Self {
                config,
                state,
                captured_count: 0,
                clock_reads: 0,
                fail_clock: false,
                fail_rearm: false,
                events: Mutex::new(Vec::new()),
                resource_requests: Mutex::new(0),
                scheduler_time: None,
                timer: None,
                timer_requests: Vec::new(),
            },
        )
    }

    fn stage_timer(&mut self, schedule: TimerSchedule, precise: bool) -> Result<(), Error> {
        let (rcbs, suffix) = match schedule {
            TimerSchedule::Rcbs(rcbs) => (rcbs, 0),
            TimerSchedule::RcbsAndInstructions(rcbs, suffix) => (rcbs, suffix),
            TimerSchedule::Time(_) => panic!("unexpected wall-clock timer"),
        };
        self.timer_requests.push((rcbs, suffix, precise));
        if self.fail_rearm {
            return Err(Error::Tool(anyhow::anyhow!("test rearm refusal")));
        }
        self.timer = Some((rcbs, suffix, precise));
        Ok(())
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for CapturedGuest {
    async fn send_rpc(
        &self,
        (_, _, request): <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        match request {
            GlobalRequest::TraceSchedEvent(event, _) => {
                self.events.lock().unwrap().push(event);
                (
                    None,
                    GlobalResponse::TraceSchedEvent(
                        serde_json::from_value(serde_json::json!({
                            "print_stack_strace": null,
                            "timeslice": null
                        }))
                        .unwrap(),
                    ),
                )
            }
            GlobalRequest::RequestResources(_, _) => {
                *self.resource_requests.lock().unwrap() += 1;
                (
                    self.scheduler_time,
                    GlobalResponse::RequestResources(ResumeStatus::Normal),
                )
            }
            other => panic!("unexpected progress RPC: {other:?}"),
        }
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl<T> Guest<T> for CapturedGuest
where
    T: Tool<GlobalState = GlobalState, ThreadState = ThreadState<()>>,
{
    type Memory = LocalMemory;
    type Stack = UnusedStack;

    fn tid(&self) -> Pid {
        Pid::from_raw(1)
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(1)
    }

    fn ppid(&self) -> Option<Pid> {
        None
    }

    fn memory(&self) -> Self::Memory {
        LocalMemory::new()
    }

    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.state
    }

    fn thread_state(&self) -> &ThreadState<()> {
        &self.state
    }

    async fn regs(&mut self) -> libc::user_regs_struct {
        libc::user_regs_struct {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbp: 0,
            rbx: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rax: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            orig_rax: 0,
            rip: RIP_BYTES.as_ptr() as u64,
            cs: 0,
            eflags: 0,
            rsp: 0,
            ss: 0,
            fs_base: 0,
            gs_base: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
        }
    }

    async fn set_regs(&mut self, _regs: libc::user_regs_struct) -> Result<(), Error> {
        panic!("progress must not edit registers")
    }

    async fn stack(&mut self) -> UnusedStack {
        panic!("progress must not access guest stack")
    }

    async fn daemonize(&mut self) {
        panic!("progress must not daemonize")
    }

    async fn inject<S: SyscallInfo>(&mut self, _syscall: S) -> Result<i64, Errno> {
        panic!("progress must not synthesize syscalls")
    }

    async fn tail_inject<S: SyscallInfo>(&mut self, _syscall: S) -> Never {
        panic!("progress must not synthesize terminal syscalls")
    }

    fn set_timer(&mut self, schedule: TimerSchedule) -> Result<(), Error> {
        self.stage_timer(schedule, false)
    }

    fn set_timer_precise(&mut self, schedule: TimerSchedule) -> Result<(), Error> {
        self.stage_timer(schedule, true)
    }

    fn read_clock(&mut self) -> Result<u64, Error> {
        self.clock_reads += 1;
        if self.fail_clock {
            Err(Error::Tool(anyhow::anyhow!("test missing observation")))
        } else {
            Ok(self.captured_count)
        }
    }
}

#[derive(Default)]
struct UnsupportedTool;

#[reverie::tool]
impl Tool for UnsupportedTool {
    type GlobalState = GlobalState;
    type ThreadState = ThreadState<()>;
}

#[tokio::test]
async fn unsupported_default_refuses_without_touching_guest_progress() {
    let (_, mut guest) = CapturedGuest::initialized(true);
    guest.captured_count = 11;
    let before = serde_json::to_vec(&guest.state).unwrap();
    let error = UnsupportedTool
        .handle_guest_progress(&mut guest)
        .await
        .unwrap_err();
    let Error::Tool(error) = error else {
        panic!("unsupported progress became a syscall errno");
    };
    assert!(error.downcast_ref::<UnsupportedGuestProgress>().is_some());
    assert_eq!(serde_json::to_vec(&guest.state).unwrap(), before);
    assert_eq!(guest.clock_reads, 0);
    assert!(guest.events.lock().unwrap().is_empty());
    assert!(guest.timer_requests.is_empty());
}

#[tokio::test]
async fn captured_counts_advance_exact_shared_time_without_event_or_entropy_charge() {
    let (tool, mut guest) = CapturedGuest::initialized(false);
    let before = guest.state.clone();
    let initial = before.thread_logical_time.as_nanos();
    for count in [3, 7, 7, 11] {
        guest.captured_count = count;
        tool.handle_guest_progress(&mut guest).await.unwrap();
        let mut expected = before.thread_logical_time.clone();
        expected.add_rcbs(count);
        assert_eq!(
            serde_json::to_value(&guest.state.thread_logical_time).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(
            guest.state.thread_logical_time.as_nanos() - initial,
            LogicalTime::from_nanos(count * 20)
        );
        assert_eq!(guest.state.committed_clock_value, count);
        assert_eq!(guest.timer, Some((5_000 - count, 0, true)));
        assert!(guest.state.prng == before.prng);
        assert!(guest.state.chaos_prng == before.chaos_prng);
        assert_eq!(
            serde_json::to_value(&guest.state.stats).unwrap(),
            serde_json::to_value(&before.stats).unwrap()
        );
    }
    assert_eq!(guest.clock_reads, 4);
    assert_eq!(*guest.resource_requests.lock().unwrap(), 0);
}

#[tokio::test]
async fn actual_trace_records_include_otherinstructions_at_equal_count() {
    let (tool, mut guest) = CapturedGuest::initialized(true);
    guest.captured_count = 3;
    tool.handle_guest_progress(&mut guest).await.unwrap();
    let time = guest.state.thread_logical_time.as_nanos();
    tool.handle_guest_progress(&mut guest).await.unwrap();
    let events = guest.events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].op, Op::Branch);
    assert_eq!(events[0].count, 3);
    assert_eq!(events[0].end_rip, None);
    assert_eq!(events[0].end_time, Some(LogicalTime::from_nanos(60)));
    for event in &events[1..] {
        assert_eq!(event.op, Op::OtherInstructions);
        assert_eq!(event.count, 1);
        assert_eq!(event.end_rip.unwrap().get(), RIP_BYTES.as_ptr() as usize);
        assert_eq!(event.end_time, Some(LogicalTime::from_nanos(60)));
    }
    assert_eq!(guest.state.thread_logical_time.as_nanos(), time);
    println!(
        "actual shared trace events: {}",
        serde_json::to_string(&*events).unwrap()
    );
}

#[tokio::test]
async fn expired_slice_performs_resource_rpc_and_applies_coordinator_time() {
    let (tool, mut guest) = CapturedGuest::initialized(false);
    let before = guest.state.clone();
    let initial = before.thread_logical_time.as_nanos();
    guest.state.end_of_timeslice = Some(initial + Duration::from_nanos(20));
    guest.scheduler_time = Some(initial + Duration::from_nanos(100));
    guest.captured_count = 3;
    tool.handle_guest_progress(&mut guest).await.unwrap();
    assert_eq!(*guest.resource_requests.lock().unwrap(), 1);
    assert_eq!(
        guest.state.thread_logical_time.as_nanos(),
        initial + Duration::from_nanos(100)
    );
    assert_eq!(guest.state.committed_clock_value, 3);
    assert_eq!(
        guest.state.stats.timeslice_count,
        before.stats.timeslice_count + 1
    );
    assert_eq!(guest.state.stats.syscall_count, before.stats.syscall_count);
    assert_eq!(guest.state.stats.signal_count, before.stats.signal_count);
    assert!(guest.state.prng == before.prng);
    assert!(guest.state.chaos_prng == before.chaos_prng);
    assert!(guest.timer.unwrap().0 > 0);
}

#[tokio::test]
async fn unavailable_capture_panics_without_committing_or_rearming() {
    let (tool, mut guest) = CapturedGuest::initialized(false);
    guest.fail_clock = true;
    let before = serde_json::to_vec(&guest.state).unwrap();
    let result = AssertUnwindSafe(tool.handle_guest_progress(&mut guest))
        .catch_unwind()
        .await;
    assert!(result.is_err());
    assert_eq!(serde_json::to_vec(&guest.state).unwrap(), before);
    assert!(guest.timer_requests.is_empty());
}

#[tokio::test]
async fn rearm_failure_is_not_success_and_can_follow_partial_commitment() {
    let (tool, mut guest) = CapturedGuest::initialized(false);
    guest.timer = Some((0, 3, true));
    guest.fail_rearm = true;
    guest.captured_count = 3;
    let result = AssertUnwindSafe(tool.handle_guest_progress(&mut guest))
        .catch_unwind()
        .await;
    assert!(result.is_err());
    assert_eq!(guest.state.committed_clock_value, 3);
    assert_eq!(guest.timer, Some((0, 3, true)));
    assert_eq!(guest.timer_requests, vec![(4_997, 0, true)]);
}

#[tokio::test]
async fn progress_reports_exact_rearm_after_prior_instruction_timer_request() {
    let (tool, mut guest) = CapturedGuest::initialized(false);
    guest
        .stage_timer(TimerSchedule::RcbsAndInstructions(0, 3), true)
        .unwrap();
    guest.captured_count = 3;
    tool.handle_guest_progress(&mut guest).await.unwrap();
    println!(
        "prior request and actual shared rearm requests={:?}; native timer resolver not tested",
        guest.timer_requests
    );
    assert_eq!(guest.timer_requests, vec![(0, 3, true), (4_997, 0, true)]);
    assert_eq!(guest.timer, Some((4_997, 0, true)));
    assert_eq!(guest.state.committed_clock_value, 3);
}
