//! One retained local Unix readiness attempt, driven by actual backend stops.
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use reverie::InjectedSyscallEvent;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::ForegroundRoot;
use super::NetworkRuntimeResources;
use super::PidfdIdentity;
use super::guard::NetworkGuardControl;
use super::guard::NetworkGuardProbeCompletion;
use super::guard::NetworkGuardProbeId;
use super::guard::NetworkGuardProbeKind;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkStreamOwner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Retained,
    Arming,
    Armed,
    Prepared,
    Submitting,
    Entered,
    Returned,
    Completed,
    Retiring,
    Retired,
    Disarming,
    Disarmed,
}
#[derive(Debug)]
struct State {
    phase: Phase,
    probe: Option<NetworkGuardProbeId>,
    raw: Option<i64>,
    completion: Option<NetworkGuardProbeCompletion>,
    failure: Option<String>,
    terminal: bool,
}

/// The run retains another Arc before ARM. Dropping the syscall future cannot
/// dispose of ambiguous native custody or make a later attempt admissible.
#[derive(Debug)]
pub(crate) struct Local {
    pub(crate) owner: NetworkStreamOwner,
    pub(crate) syscall_count: u64,
    pub(crate) root: Arc<ForegroundRoot>,
    pub(crate) read: NetworkFdReadAdmission,
    nr: Sysno,
    arguments: [usize; 6],
    kind: NetworkGuardProbeKind,
    control: Arc<dyn NetworkGuardControl>,
    deadline: Instant,
    state: Mutex<State>,
}

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
fn arguments(args: SyscallArgs) -> [usize; 6] {
    [
        args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
    ]
}
impl Local {
    pub(crate) fn reject(&self, message: &str) {
        self.retain_failure(&fail(message));
    }
    fn retain_failure(&self, error: &io::Error) {
        self.state
            .lock()
            .unwrap()
            .failure
            .get_or_insert_with(|| error.to_string());
    }
    fn phase(&self, expected: Phase, next: Phase) -> io::Result<NetworkGuardProbeId> {
        let mut state = self.state.lock().unwrap();
        if state.failure.is_some()
            || state.terminal
            || state.phase != expected
            || Instant::now() >= self.deadline
        {
            return Err(fail("Unix probe changed phase, owner or original deadline"));
        }
        let probe = state
            .probe
            .ok_or_else(|| fail("Unix probe lacks keeper ARM receipt"))?;
        state.phase = next;
        Ok(probe)
    }
    pub(crate) fn is_retired(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.failure.is_none() && matches!(state.phase, Phase::Retired | Phase::Disarmed)
    }
    pub(crate) fn arm(&self) -> io::Result<()> {
        let result = (|| {
            {
                let mut state = self.state.lock().unwrap();
                if state.phase != Phase::Retained || state.failure.is_some() || state.terminal {
                    return Err(fail("Unix probe ARM repeated or owner lost"));
                }
                state.phase = Phase::Arming;
            }
            // SAFETY: Global's exact stopped-root/read/grant join precedes this
            // call, and the runtime already owns this attempt on every error.
            let probe = unsafe { self.control.arm_stopped_probe(self.kind, self.deadline) }?;
            if probe.incarnation == 0
                || probe.initial_sequence == 0
                || probe.sequence == 0
                || probe.kind != self.kind
            {
                return Err(fail("Unix probe ARM identity mismatch"));
            }
            let mut state = self.state.lock().unwrap();
            state.probe = Some(probe);
            state.phase = Phase::Armed;
            drop(state);
            if Instant::now() >= self.deadline {
                return Err(fail("Unix probe ARM exceeded original deadline"));
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.retain_failure(error);
        }
        result
    }
    pub(crate) fn observe(
        &self,
        nr: Sysno,
        args: SyscallArgs,
        event: InjectedSyscallEvent,
    ) -> io::Result<()> {
        let result = (|| {
            if nr != self.nr || arguments(args) != self.arguments {
                return Err(fail("Unix probe changed actual syscall tuple"));
            }
            match event {
                InjectedSyscallEvent::Prepared => {
                    self.phase(Phase::Armed, Phase::Prepared)?;
                }
                InjectedSyscallEvent::Entered => {
                    let probe = self.phase(Phase::Prepared, Phase::Submitting)?;
                    // SAFETY: this is the actual authenticated backend ENTRY,
                    // after Global rechecked owner/MM/root/read and the tuple.
                    unsafe { self.control.submit_entered_probe(probe, self.deadline) }?;
                    self.state.lock().unwrap().phase = Phase::Entered;
                }
                InjectedSyscallEvent::Returned(raw) => {
                    let probe = self.phase(Phase::Entered, Phase::Returned)?;
                    self.state.lock().unwrap().raw = Some(raw);
                    // Negative continuations are outside this finite consumer
                    // contract. Original Poll keeps the kernel's own pointer;
                    // shadow Ppoll errors can feed another outer syscall's
                    // timeout/signal protocol without an established restart
                    // proof. Retain actual raw/custody; never forge EINTR.
                    if !matches!(raw, 0 | 1) {
                        return Err(fail(
                            "unsupported actual Unix probe return; custody retained",
                        ));
                    }
                    // SAFETY: the unchanged native return was just delivered
                    // synchronously at this same retained attempt's exit stop.
                    let completion = unsafe {
                        self.control
                            .complete_returned_probe(probe, raw, self.deadline)
                    }?;
                    if completion.probe != probe
                        || completion.raw != raw
                        || completion.observations != 1
                    {
                        return Err(fail(
                            "Unix probe completion mismatched actual return/observation",
                        ));
                    }
                    let mut state = self.state.lock().unwrap();
                    state.completion = Some(completion);
                    state.phase = Phase::Completed;
                }
                InjectedSyscallEvent::InterruptedBeforeEntry => {
                    let probe = self.phase(Phase::Prepared, Phase::Disarming)?;
                    // SAFETY: only this actual same-attempt interruption proves
                    // no native ENTRY; generic cancellation does not call here.
                    unsafe { self.control.disarm_unentered_probe(probe, self.deadline) }?;
                    self.state.lock().unwrap().phase = Phase::Disarmed;
                }
                _ => return Err(fail("Unix probe unexpectedly reported a child effect")),
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.retain_failure(error);
        }
        result
    }
    /// None is an actual before-entry disarm, never a missing observation.
    pub(crate) fn returned(&self) -> io::Result<Option<i64>> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        if state.terminal || Instant::now() >= self.deadline {
            return Err(fail("Unix probe owner/deadline lost"));
        }
        match state.phase {
            Phase::Completed => state
                .raw
                .map(Some)
                .ok_or_else(|| fail("Unix probe lost actual return")),
            Phase::Disarmed => Ok(None),
            _ => Err(fail("Unix probe lacks actual return and native completion")),
        }
    }
    /// Called only after scratch and the unchanged original reader were checked.
    pub(crate) fn retire(&self) -> io::Result<()> {
        let result = (|| {
            self.phase(Phase::Completed, Phase::Retiring)?;
            let completion = self
                .state
                .lock()
                .unwrap()
                .completion
                .ok_or_else(|| fail("Unix probe lost completion"))?;
            self.control
                .retire_completed_probe(completion, self.deadline)?;
            self.state.lock().unwrap().phase = Phase::Retired;
            if Instant::now() >= self.deadline {
                return Err(fail("Unix probe retirement exceeded original deadline"));
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.retain_failure(error);
        }
        result
    }
    pub(crate) fn terminal(&self) {
        self.state.lock().unwrap().terminal = true;
        // No synthetic retirement: keeper retains the attempt until its actual
        // task-terminal protocol proves the native map/owner disposition.
    }
}

pub(crate) struct Invocation {
    pub(crate) nr: Sysno,
    pub(crate) args: SyscallArgs,
    pub(crate) kind: NetworkGuardProbeKind,
    pub(crate) syscall_count: u64,
}

impl NetworkRuntimeResources {
    pub(crate) fn retain_guard_probe(
        &self,
        owner: NetworkStreamOwner,
        root: Arc<ForegroundRoot>,
        read: NetworkFdReadAdmission,
        invocation: Invocation,
    ) -> io::Result<Option<Arc<Local>>> {
        let Invocation {
            nr,
            args,
            kind,
            syscall_count,
        } = invocation;
        let mut guard = self.shared.guard.lock().unwrap();
        let Some(guard) = guard.as_mut() else {
            return Ok(None);
        };
        if guard
            .probe
            .as_ref()
            .is_some_and(|probe| !probe.is_retired())
        {
            return Err(fail("previous Unix probe remains owned"));
        }
        let initial = guard
            .initial
            .as_ref()
            .ok_or_else(|| fail("Unix probe lacks initial guard owner"))?;
        if initial.result.is_err()
            || guard
                .observer
                .as_ref()
                .is_none_or(|(_, result)| result.is_err())
        {
            return Err(fail("Unix probe lacks admitted initial task and observer"));
        }
        let physical = self.shared.physical.lock().unwrap();
        let current = physical.get(owner)?;
        let same_exec = physical.initial_exec(owner)?.is_some_and(|receipt| {
            receipt.caller == initial.owner.thread
                && receipt.process == initial.owner.thread
                && receipt.mm == initial.owner.mm
                && receipt.mm.for_exec(receipt.process) == owner.mm
        });
        if initial.owner.thread != owner.thread
            || (initial.owner.mm != owner.mm && !same_exec)
            || PidfdIdentity::read(&initial.task)? != PidfdIdentity::read(current)?
            || !Arc::ptr_eq(&root, &physical.foreground_root(owner)?)
        {
            return Err(fail("Unix probe changed enrolled initial task/MM/root"));
        }
        let local = Arc::new(Local {
            owner,
            syscall_count,
            root,
            read,
            nr,
            arguments: arguments(args),
            kind,
            control: guard.control.clone(),
            deadline: Instant::now() + Duration::from_secs(1),
            state: Mutex::new(State {
                phase: Phase::Retained,
                probe: None,
                raw: None,
                completion: None,
                failure: None,
                terminal: false,
            }),
        });
        guard.probe = Some(local.clone());
        Ok(Some(local))
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::BorrowedFd;

    use super::super::guard::NetworkGuardControllerAbort;
    use super::super::guard::NetworkGuardOutcome;
    use super::*;
    use crate::network_replay::NetworkFdReadBegin;
    use crate::network_replay::NetworkReplayEngine;

    #[derive(Debug, Default)]
    struct Control {
        calls: Mutex<Vec<&'static str>>,
        bad_completion: Mutex<Option<usize>>,
        fail_at: Mutex<Option<&'static str>>,
    }
    impl Control {
        fn call(&self, name: &'static str) -> io::Result<()> {
            self.calls.lock().unwrap().push(name);
            if *self.fail_at.lock().unwrap() == Some(name) {
                return Err(fail("controlled unknown keeper response"));
            }
            Ok(())
        }
    }
    impl NetworkGuardControl for Control {
        unsafe fn register_stopped_initial(&self, _: BorrowedFd<'_>, _: Instant) -> io::Result<()> {
            unreachable!()
        }
        fn observation(&self) -> NetworkGuardOutcome {
            NetworkGuardOutcome::Running
        }
        fn start_observer(&self, _: NetworkGuardControllerAbort) -> io::Result<()> {
            unreachable!()
        }
        unsafe fn arm_stopped_probe(
            &self,
            kind: NetworkGuardProbeKind,
            _: Instant,
        ) -> io::Result<NetworkGuardProbeId> {
            self.call("arm")?;
            Ok(NetworkGuardProbeId {
                incarnation: 11,
                initial_sequence: 3,
                sequence: 7,
                kind,
            })
        }
        unsafe fn submit_entered_probe(
            &self,
            _: NetworkGuardProbeId,
            _: Instant,
        ) -> io::Result<()> {
            self.call("submit")
        }
        unsafe fn complete_returned_probe(
            &self,
            probe: NetworkGuardProbeId,
            raw: i64,
            _: Instant,
        ) -> io::Result<NetworkGuardProbeCompletion> {
            self.call("complete")?;
            let mut receipt = NetworkGuardProbeCompletion {
                probe,
                raw,
                observations: 1,
            };
            match *self.bad_completion.lock().unwrap() {
                Some(0) => receipt.probe.sequence += 1,
                Some(1) => receipt.raw += 1,
                Some(2) => receipt.observations = 0,
                Some(3) => receipt.observations = 2,
                Some(4) => receipt.probe.incarnation += 1,
                Some(5) => receipt.probe.initial_sequence += 1,
                _ => {}
            }
            Ok(receipt)
        }
        fn retire_completed_probe(
            &self,
            _: NetworkGuardProbeCompletion,
            _: Instant,
        ) -> io::Result<()> {
            self.call("retire")
        }
        unsafe fn disarm_unentered_probe(
            &self,
            _: NetworkGuardProbeId,
            _: Instant,
        ) -> io::Result<()> {
            self.call("disarm")
        }
    }
    fn fixture(kind: NetworkGuardProbeKind) -> (Arc<Local>, Arc<Control>, SyscallArgs) {
        // Component premise only: native entry/return and keeper receipts are
        // controlled inputs. The actual runtime transitions and FD read issuer
        // execute; this does not claim an actual stopped guest or native BPF.
        let (root, _metadata, _memory, claim) = super::super::controlled_foreground_root(917);
        let mut engine =
            NetworkReplayEngine::record_native_receive(crate::config::Config::default().epoch);
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), &claim, root.owner().thread)
            .unwrap();
        let NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(root.owner(), root.files(), 3).unwrap()
        else {
            panic!("reader admitted")
        };
        let control = Arc::new(Control::default());
        let args = SyscallArgs::new(
            0x12340,
            1,
            if kind == NetworkGuardProbeKind::Poll {
                0
            } else {
                0x12400
            },
            0,
            0,
            0,
        );
        let local = Arc::new(Local {
            owner: root.owner(),
            syscall_count: 0,
            root,
            read: *read,
            nr: if kind == NetworkGuardProbeKind::Poll {
                Sysno::poll
            } else {
                Sysno::ppoll
            },
            arguments: arguments(args),
            kind,
            control: control.clone(),
            deadline: Instant::now() + Duration::from_secs(1),
            state: Mutex::new(State {
                phase: Phase::Retained,
                probe: None,
                raw: None,
                completion: None,
                failure: None,
                terminal: false,
            }),
        });
        (local, control, args)
    }
    fn events(local: &Local, args: SyscallArgs, raw: i64) {
        local.arm().unwrap();
        local
            .observe(local.nr, args, InjectedSyscallEvent::Prepared)
            .unwrap();
        local
            .observe(local.nr, args, InjectedSyscallEvent::Entered)
            .unwrap();
        assert!(
            local.returned().is_err(),
            "entry alone never authorizes scratch"
        );
        local
            .observe(local.nr, args, InjectedSyscallEvent::Returned(raw))
            .unwrap();
    }
    #[test]
    fn local_guard_probe_requires_actual_completion_and_explicit_retirement() {
        for (kind, raw) in [
            (NetworkGuardProbeKind::Poll, 0),
            (NetworkGuardProbeKind::Poll, 1),
            (NetworkGuardProbeKind::Ppoll, 0),
            (NetworkGuardProbeKind::Ppoll, 1),
        ] {
            let (local, control, args) = fixture(kind);
            events(&local, args, raw);
            assert_eq!(local.returned().unwrap(), Some(raw));
            assert!(!local.is_retired());
            assert_eq!(
                *control.calls.lock().unwrap(),
                ["arm", "submit", "complete"]
            );
            local.retire().unwrap();
            assert!(local.is_retired());
            assert_eq!(
                *control.calls.lock().unwrap(),
                ["arm", "submit", "complete", "retire"]
            );
        }
    }
    #[test]
    fn local_guard_probe_real_negative_return_is_retained_without_scratch_restart() {
        for (kind, raw) in [
            (NetworkGuardProbeKind::Poll, -516),
            (NetworkGuardProbeKind::Ppoll, -514),
            (NetworkGuardProbeKind::Ppoll, -4),
            (NetworkGuardProbeKind::Poll, -14),
        ] {
            let (local, control, args) = fixture(kind);
            local.arm().unwrap();
            local
                .observe(local.nr, args, InjectedSyscallEvent::Prepared)
                .unwrap();
            local
                .observe(local.nr, args, InjectedSyscallEvent::Entered)
                .unwrap();
            assert!(
                local
                    .observe(local.nr, args, InjectedSyscallEvent::Returned(raw))
                    .is_err()
            );
            assert_eq!(local.state.lock().unwrap().raw, Some(raw));
            assert!(local.returned().is_err());
            assert!(!local.is_retired());
            assert_eq!(*control.calls.lock().unwrap(), ["arm", "submit"]);
        }
    }
    #[test]
    fn local_guard_probe_missing_duplicate_or_changed_boundary_never_retires() {
        for mask in 0u8..7 {
            let (local, _, args) = fixture(NetworkGuardProbeKind::Poll);
            local.arm().unwrap();
            for (bit, event) in [
                (1, InjectedSyscallEvent::Prepared),
                (2, InjectedSyscallEvent::Entered),
                (4, InjectedSyscallEvent::Returned(0)),
            ] {
                if mask & bit != 0 {
                    let _ = local.observe(local.nr, args, event);
                }
            }
            assert!(local.returned().is_err(), "omission mask {mask}");
            assert!(!local.is_retired());
        }
        for mutation in 0..7 {
            let (local, _, args) = fixture(NetworkGuardProbeKind::Poll);
            local.arm().unwrap();
            let mut raw = arguments(args);
            let nr = if mutation == 6 {
                Sysno::ppoll
            } else {
                raw[mutation] ^= 1;
                local.nr
            };
            let args = SyscallArgs::new(raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]);
            assert!(
                local
                    .observe(nr, args, InjectedSyscallEvent::Prepared)
                    .is_err()
            );
            assert!(local.returned().is_err());
        }
        for repeated in [
            InjectedSyscallEvent::Prepared,
            InjectedSyscallEvent::Entered,
            InjectedSyscallEvent::Returned(0),
        ] {
            let (local, _, args) = fixture(NetworkGuardProbeKind::Poll);
            events(&local, args, 0);
            assert!(local.observe(local.nr, args, repeated).is_err());
            assert!(local.returned().is_err());
        }
    }
    #[test]
    fn local_guard_probe_false_completion_and_unknown_control_stay_owned() {
        for mutation in 0..6 {
            let (local, control, args) = fixture(NetworkGuardProbeKind::Poll);
            *control.bad_completion.lock().unwrap() = Some(mutation);
            local.arm().unwrap();
            local
                .observe(local.nr, args, InjectedSyscallEvent::Prepared)
                .unwrap();
            local
                .observe(local.nr, args, InjectedSyscallEvent::Entered)
                .unwrap();
            assert!(
                local
                    .observe(local.nr, args, InjectedSyscallEvent::Returned(0))
                    .is_err()
            );
            assert!(local.returned().is_err());
            assert!(!local.is_retired());
        }
        for stage in ["arm", "submit", "complete", "retire"] {
            let (local, control, args) = fixture(NetworkGuardProbeKind::Poll);
            *control.fail_at.lock().unwrap() = Some(stage);
            let _ = local.arm();
            for event in [
                InjectedSyscallEvent::Prepared,
                InjectedSyscallEvent::Entered,
                InjectedSyscallEvent::Returned(0),
            ] {
                let _ = local.observe(local.nr, args, event);
            }
            let _ = local.retire();
            assert!(!local.is_retired(), "unknown {stage}");
        }
    }
    #[test]
    fn local_guard_probe_unentered_disarm_is_distinct_from_cancellation_and_terminal() {
        let (local, control, args) = fixture(NetworkGuardProbeKind::Poll);
        local.arm().unwrap();
        local
            .observe(local.nr, args, InjectedSyscallEvent::Prepared)
            .unwrap();
        local
            .observe(local.nr, args, InjectedSyscallEvent::InterruptedBeforeEntry)
            .unwrap();
        assert_eq!(local.returned().unwrap(), None);
        assert!(local.is_retired());
        assert_eq!(*control.calls.lock().unwrap(), ["arm", "disarm"]);
        for entered in [false, true] {
            let (local, control, args) = fixture(NetworkGuardProbeKind::Poll);
            local.arm().unwrap();
            local
                .observe(local.nr, args, InjectedSyscallEvent::Prepared)
                .unwrap();
            if entered {
                local
                    .observe(local.nr, args, InjectedSyscallEvent::Entered)
                    .unwrap();
            }
            let retained = local.clone();
            drop(local); // cancelled future does not delete run custody
            retained.terminal();
            assert!(!retained.is_retired());
            assert!(retained.returned().is_err());
            assert!(!control.calls.lock().unwrap().contains(&"disarm"));
        }
    }
}
