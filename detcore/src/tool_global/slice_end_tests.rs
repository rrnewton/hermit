/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared hook/RPC tests, without a guest process. The RPC fixture records
//! requests and supplies grants; it does not model scheduler turn charging.

use std::collections::VecDeque;
use std::sync::Mutex;

use reverie::Errno;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::Tool;

use super::*;
use crate::CheckIn;
use crate::ThreadState;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::tool_global::TraceSchedEventResponse;

const TID: DetTid = DetTid::from_raw(17);
const RIP: usize = 0x401000;

struct NoStack;
struct NoStackGuard;
impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for NoStack {
    type StackGuard = NoStackGuard;
    fn size(&self) -> usize {
        panic!("unexpected stack access")
    }
    fn capacity(&self) -> usize {
        panic!("unexpected stack access")
    }
    fn push<'s, T>(&mut self, _: T) -> reverie::syscalls::Addr<'s, T> {
        panic!("unexpected stack access")
    }
    fn reserve<'s, T>(&mut self) -> reverie::syscalls::AddrMut<'s, T> {
        panic!("unexpected stack access")
    }
    fn commit(self) -> Result<NoStackGuard, Errno> {
        panic!("unexpected stack access")
    }
}

struct SliceGuest {
    config: Config,
    thread: ThreadState<()>,
    records: Mutex<Vec<SchedEvent>>,
    requests: Mutex<Vec<Resources>>,
    replay: Mutex<Option<VecDeque<SchedEvent>>>,
}

impl SliceGuest {
    fn new(replay: Option<Vec<SchedEvent>>) -> (Detcore, Self) {
        let config = Config {
            sequentialize_threads: true,
            record_preemptions: true,
            replay_schedule_from: replay.as_ref().map(|_| "not-read-by-local-hook".into()),
            ..Config::default()
        }
        .with_backend(|backend| backend.delivers_preemption_timer_events = false);
        let tool = Detcore::new(Tid::from_raw(TID.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
        thread.detpid = Some(TID);
        thread.past_global_first_execve = true;
        thread.next_timeslice(&config);
        (
            tool,
            Self {
                config,
                thread,
                records: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
                replay: Mutex::new(replay.map(VecDeque::from)),
            },
        )
    }

    fn event(&self, op: Op) -> SchedEvent {
        SchedEvent {
            dettid: TID,
            op,
            count: 1,
            start_rip: None,
            end_rip: std::num::NonZeroUsize::new(RIP),
            end_time: None,
        }
        .with_dettime(&self.thread.thread_logical_time)
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for SliceGuest {
    async fn send_rpc(
        &self,
        (time, mm, request): <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        assert_eq!(mm, self.thread.mm_id);
        assert_eq!(
            serde_json::to_value(time).unwrap(),
            serde_json::to_value(&self.thread.thread_logical_time).unwrap()
        );
        let mut replay = self.replay.lock().unwrap();
        let response = match request {
            GlobalRequest::TraceSchedEvent(event, pid, _) => {
                assert_eq!(pid, TID);
                if let Some(replay) = replay.as_mut() {
                    let expected = replay.pop_front().expect("unexpected extra trace event");
                    assert_eq!(
                        event, expected,
                        "trace event must match the scripted reply exactly"
                    );
                }
                self.records.lock().unwrap().push(event);
                GlobalResponse::TraceSchedEvent(TraceSchedEventResponse {
                    print_stack_strace: None,
                    timeslice: None,
                })
            }
            GlobalRequest::RequestResources(request, pid) => {
                assert_eq!(pid, TID);
                assert_eq!(request.resources.len(), 1);
                assert_eq!(
                    request
                        .resources
                        .get(&ResourceID::SleepUntil(LogicalTime::ZERO)),
                    Some(&Permission::W)
                );
                self.requests.lock().unwrap().push(request);
                GlobalResponse::Granted(Box::new(GlobalResponse::RequestResources(
                    ResumeStatus::Normal,
                )))
            }
            other => panic!("unexpected RPC: {other:?}"),
        };
        let response = if let Some(replay) = replay.as_ref() {
            let boundary = match replay.front() {
                Some(SchedEvent {
                    op: Op::TimesliceEnd(boundary),
                    dettid,
                    ..
                }) if *dettid == TID => Some(*boundary),
                _ => None,
            };
            GlobalResponse::WithSliceEnd(boundary, Box::new(response))
        } else {
            response
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for SliceGuest {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = NoStack;
    fn tid(&self) -> Tid {
        Tid::from_raw(TID.as_raw())
    }
    fn pid(&self) -> Tid {
        self.tid()
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        reverie::syscalls::LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // SAFETY: the register structure contains only integers.
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        regs.rip = RIP as u64;
        regs
    }
    async fn stack(&mut self) -> NoStack {
        panic!("unexpected stack access")
    }
    async fn daemonize(&mut self) {
        panic!("unexpected daemonize")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        panic!("unexpected syscall injection")
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("unexpected tail injection")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("timerless hook must not arm a timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("timerless hook must not arm a precise timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("posthook must not sample the RCB clock")
    }
}

#[test]
fn deadline_cap_records_a_posthook_end_and_replay_yields_once_without_expiry() {
    futures::executor::block_on(async {
        let (tool, mut record) = SliceGuest::new(None);
        let original_max = record.thread.max_timeslice_end;
        let now = record.thread.thread_logical_time.as_nanos();
        let floor = record.config.scheduler_turn_charge().unwrap();
        assert!(floor > 100);
        cap_timeslice_at_deadline(&mut record, Some(Duration::from_nanos(100)));
        let capped = record.thread.end_of_timeslice.unwrap();
        assert_eq!(capped, now + Duration::from_nanos(floor));
        assert!(capped < original_max.unwrap());
        assert_eq!(record.thread.max_timeslice_end, original_max);
        record.thread.thread_logical_time.advance_to(capped);
        let complete = record.event(Op::Syscall(Sysno::write, SyscallPhase::Posthook));
        trace_schedevent(&mut record, complete.clone(), false).await;
        tool.post_handler_hook(&mut record, CheckIn::Event).await;
        let events = record.records.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].op, Op::TimesliceEnd(SliceEndBoundary::Posthook));
        assert_eq!(record.requests.lock().unwrap().len(), 1);

        let (tool, mut replay) = SliceGuest::new(Some(events.clone()));
        replay.thread.thread_logical_time.advance_to(capped);
        assert!(
            !replay.thread.timeslice_expired(),
            "replay has no live deadline cap"
        );
        trace_schedevent(&mut replay, complete, false).await;
        let before = replay.thread.stats.timeslice_count;
        tool.post_handler_hook(&mut replay, CheckIn::Event).await;
        assert_eq!(*replay.records.lock().unwrap(), events);
        assert_eq!(replay.requests.lock().unwrap().len(), 1);
        assert_eq!(replay.thread.scheduler_grants, 1);
        assert_eq!(replay.thread.stats.timeslice_count, before + 1);
        assert_eq!(replay.thread.replay_slice_end, Some(None));
        assert!(!replay.thread.timeslice_expired());
    });
}

#[test]
fn no_recorded_boundary_and_no_expiry_do_not_yield() {
    futures::executor::block_on(async {
        let (tool, mut guest) = SliceGuest::new(None);
        tool.post_handler_hook(&mut guest, CheckIn::Event).await;
        assert!(guest.records.lock().unwrap().is_empty());
        assert!(guest.requests.lock().unwrap().is_empty());
        assert_eq!(guest.thread.scheduler_grants, 0);
    });
}

#[test]
fn adjacent_recorded_boundaries_each_request_one_turn() {
    futures::executor::block_on(async {
        let (_, template) = SliceGuest::new(None);
        let complete = template.event(Op::Syscall(Sysno::write, SyscallPhase::Posthook));
        let marker = template.event(Op::TimesliceEnd(SliceEndBoundary::Posthook));
        let events = vec![complete.clone(), marker.clone(), marker];
        let (tool, mut guest) = SliceGuest::new(Some(events.clone()));
        trace_schedevent(&mut guest, complete, false).await;
        tool.post_handler_hook(&mut guest, CheckIn::Event).await;
        assert_eq!(*guest.records.lock().unwrap(), events);
        assert_eq!(guest.requests.lock().unwrap().len(), 2);
        assert_eq!(guest.thread.scheduler_grants, 2);
        assert_eq!(guest.thread.replay_slice_end, Some(None));
    });
}

#[test]
#[should_panic(expected = "recorded slice end inside own-process signal hold")]
fn recorded_boundary_cannot_override_own_process_signal_hold() {
    futures::executor::block_on(async {
        let (_, template) = SliceGuest::new(None);
        let complete = template.event(Op::Syscall(Sysno::write, SyscallPhase::Posthook));
        let marker = template.event(Op::TimesliceEnd(SliceEndBoundary::Posthook));
        let (tool, mut guest) = SliceGuest::new(Some(vec![complete.clone(), marker]));
        trace_schedevent(&mut guest, complete, false).await;
        guest.thread.own_signal_turn_hold = Some(libc::SIGUSR1);
        tool.post_handler_hook(&mut guest, CheckIn::Event).await;
    });
}
