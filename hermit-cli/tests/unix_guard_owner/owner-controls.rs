#[cfg(test)]
mod owner_controls {
    use super::*;
    use std::os::fd::AsFd;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicUsize;

    struct Gate {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }
    impl Gate {
        fn new() -> Self {
            Self {
                state: Mutex::new((false, false)),
                changed: Condvar::new(),
            }
        }
        fn enter(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
        }
        fn entered(&self) {
            let state = self.state.lock().unwrap();
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(1), |state| !state.0)
                .unwrap();
            assert!(
                state.0 && !timeout.timed_out(),
                "controlled monitor never entered"
            );
        }
        fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }
    struct Probe {
        registration: Option<Arc<Gate>>,
        observation: Arc<Gate>,
        snapshot: Mutex<GuardOutcome>,
        observed: Mutex<GuardOutcome>,
        snapshots: AtomicUsize,
        polls: AtomicUsize,
        registration_error: bool,
        received_deadline: Mutex<Option<Instant>>,
    }
    struct Fake(Arc<Probe>);
    impl GuardMonitor for Fake {
        unsafe fn register(&mut self, _: BorrowedFd<'_>, deadline: Instant) -> io::Result<()> {
            *self.0.received_deadline.lock().unwrap() = Some(deadline);
            if let Some(gate) = &self.0.registration {
                gate.enter();
            }
            if self.0.registration_error {
                Err(io::Error::from_raw_os_error(5))
            } else {
                Ok(())
            }
        }
        fn observe(&mut self) -> GuardOutcome {
            self.0.polls.fetch_add(1, Ordering::SeqCst);
            self.0.observation.enter();
            *self.0.observed.lock().unwrap()
        }
        fn snapshot(&mut self) -> GuardOutcome {
            self.0.snapshots.fetch_add(1, Ordering::SeqCst);
            *self.0.snapshot.lock().unwrap()
        }
    }
    fn policy() -> GuardOutcome {
        GuardOutcome::Policy(GuardEvidence {
            denial: crate::unix_guard::GuardDenial {
                incarnation: 8,
                injection: 13,
                reason: 4,
                ..Default::default()
            },
            ..Default::default()
        })
    }
    fn make(
        registration: Option<Arc<Gate>>,
        error: bool,
    ) -> (
        GuardControllerOwner,
        Arc<dyn NetworkGuardControl>,
        Arc<Probe>,
    ) {
        let probe = Arc::new(Probe {
            registration,
            observation: Arc::new(Gate::new()),
            snapshot: Mutex::new(GuardOutcome::Running),
            observed: Mutex::new(GuardOutcome::Running),
            snapshots: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
            registration_error: error,
            received_deadline: Mutex::new(None),
        });
        let (owner, control) = GuardControllerOwner::from_monitor(
            Box::new(Fake(probe.clone())),
            Instant::now() + Duration::from_secs(1),
        );
        (owner, control, probe)
    }
    fn register(control: &dyn NetworkGuardControl) {
        unsafe {
            control.register_stopped_initial(
                std::io::stdout().as_fd(),
                Instant::now() + Duration::from_secs(1),
            )
        }
        .unwrap();
    }
    fn start(control: &dyn NetworkGuardControl, probe: &Probe) {
        control
            .start_observer(NetworkGuardControllerAbort::owned_controller())
            .unwrap();
        probe.observation.entered();
    }
    fn wait_finished(owner: &GuardControllerOwner) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if owner
                .shared
                .lifecycle
                .lock()
                .unwrap()
                .observer
                .as_ref()
                .unwrap()
                .is_finished()
            {
                return;
            }
            assert!(Instant::now() < deadline, "observer did not settle");
            thread::yield_now();
        }
    }
    #[test]
    fn before_initial_cannot_start_or_certify_pending() {
        let (mut owner, control, probe) = make(None, false);
        *probe.snapshot.lock().unwrap() = GuardOutcome::Pending;
        assert!(
            control
                .start_observer(NetworkGuardControllerAbort::owned_controller())
                .is_err()
        );
        assert!(owner.stop_and_join(Instant::now()).is_err());
        assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
        assert!(owner.shared.lifecycle.lock().unwrap().abort.is_none());
        assert!(
            unsafe { control.register_stopped_initial(std::io::stdout().as_fd(), Instant::now()) }
                .is_err()
        );
    }
    #[test]
    fn pending_initial_registration_survives_cancel_deadline() {
        let gate = Arc::new(Gate::new());
        let (mut owner, control, probe) = make(Some(gate.clone()), false);
        let join = thread::spawn(move || unsafe {
            control.register_stopped_initial(
                std::io::stdout().as_fd(),
                Instant::now() + Duration::from_secs(1),
            )
        });
        gate.entered();
        assert_eq!(
            owner.stop_and_join(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(probe.snapshots.load(Ordering::SeqCst), 0);
        gate.release();
        join.join().unwrap().unwrap();
        let receipt = owner
            .stop_and_join(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(!receipt.observer_was_started);
        assert_eq!(receipt.observation, NetworkGuardOutcome::Running);
        assert_eq!(owner.retained_failures()[0].operation, "stop_and_join");
    }
    #[test]
    fn real_backend_failure_retains_late_policy_as_secondary() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        start(&*control, &probe);
        match owner.settle_backend_result::<(), _>(Err(io::Error::from_raw_os_error(5))) {
            GuardedBackendResult::Failure { error, prior_guard } => {
                assert_eq!(error.raw_os_error(), Some(5));
                assert_eq!(prior_guard, None);
            }
            _ => panic!("failure lost"),
        }
        *probe.observed.lock().unwrap() = policy();
        *probe.snapshot.lock().unwrap() = policy();
        probe.observation.release();
        let receipt = owner
            .stop_and_join(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            receipt.observation,
            NetworkGuardOutcome::Policy(_)
        ));
        assert!(matches!(
            owner.retained_terminal(),
            Some(NetworkGuardTerminal::Policy(_))
        ));
        assert!(owner.retained_failures().is_empty());
    }
    #[test]
    fn success_remains_pending_and_cannot_select_backend_primary() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        start(&*control, &probe);
        assert!(matches!(
            owner.settle_backend_result::<_, io::Error>(Ok(7)),
            GuardedBackendResult::SuccessPending(7)
        ));
        // Actual report path must still select guard primary. The harness exit
        // sink panics with the selected code; it never exits this test process.
        *probe.observed.lock().unwrap() = policy();
        *probe.snapshot.lock().unwrap() = policy();
        probe.observation.release();
        wait_finished(&owner);
        let receipt = owner
            .stop_and_join(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            receipt.observation,
            NetworkGuardOutcome::Policy(_)
        ));
        assert!(
            owner
                .retain_backend_failure(NetworkGuardBackendFailure::from_error(&io::Error::other(
                    "late"
                )))
                .is_err()
        );
    }
    #[test]
    fn final_snapshot_denial_after_observer_join_is_not_published_success() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        start(&*control, &probe);
        assert!(matches!(
            owner.settle_backend_result::<_, io::Error>(Ok(9)),
            GuardedBackendResult::SuccessPending(9)
        ));
        owner.shared.stop.store(true, Ordering::Release);
        probe.observation.release();
        wait_finished(&owner);
        *probe.snapshot.lock().unwrap() = policy();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.stop_and_join(Instant::now() + Duration::from_secs(1))
        }));
        let panic = result.unwrap_err();
        assert_eq!(panic.downcast_ref::<i32>(), Some(&122));
        assert!(owner.shared.lifecycle.lock().unwrap().observer.is_none());
        assert!(
            owner
                .retain_backend_failure(NetworkGuardBackendFailure::from_error(&io::Error::other(
                    "later"
                )))
                .is_err()
        );
        assert!(matches!(
            owner.retained_terminal(),
            Some(NetworkGuardTerminal::Policy(_))
        ));
    }
    #[test]
    fn cached_observation_does_not_pump_monitor() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        let before = probe.snapshots.load(Ordering::SeqCst);
        for _ in 0..20 {
            assert_eq!(control.observation(), NetworkGuardOutcome::Running);
        }
        assert_eq!(probe.snapshots.load(Ordering::SeqCst), before);
        assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
        owner.stop_and_join(Instant::now()).unwrap();
    }
    #[test]
    fn waiter_drop_does_not_drop_actual_observer_owner() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        start(&*control, &probe);
        drop(control);
        assert!(owner.shared.lifecycle.lock().unwrap().observer.is_some());
        probe.observation.release();
        let receipt = owner
            .stop_and_join(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(receipt.observer_was_started);
        assert_eq!(receipt.observation, NetworkGuardOutcome::Running);
    }
    #[test]
    fn observer_timeout_retains_handle_and_retry_takes_final_snapshot() {
        let (mut owner, control, probe) = make(None, false);
        register(&*control);
        start(&*control, &probe);
        let before = probe.snapshots.load(Ordering::SeqCst);
        assert_eq!(
            owner.stop_and_join(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(owner.shared.lifecycle.lock().unwrap().observer.is_some());
        assert_eq!(probe.snapshots.load(Ordering::SeqCst), before);
        probe.observation.release();
        owner
            .stop_and_join(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(owner.shared.lifecycle.lock().unwrap().observer.is_none());
        assert!(probe.snapshots.load(Ordering::SeqCst) > before);
    }
    #[test]
    fn registration_error_keeps_prior_policy_and_original_error() {
        let (mut owner, control, probe) = make(None, true);
        *probe.snapshot.lock().unwrap() = policy();
        let error =
            unsafe { control.register_stopped_initial(std::io::stdout().as_fd(), Instant::now()) }
                .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        assert!(matches!(
            control.observation(),
            NetworkGuardOutcome::Policy(_)
        ));
        assert!(
            control
                .start_observer(NetworkGuardControllerAbort::owned_controller())
                .is_err()
        );
        assert!(matches!(
            owner.settle_backend_result::<(), _>(Err(error)),
            GuardedBackendResult::Failure {
                prior_guard: Some(NetworkGuardTerminal::Policy(_)),
                ..
            }
        ));
        let receipt = owner.stop_and_join(Instant::now()).unwrap();
        assert!(!receipt.observer_was_started);
        assert!(matches!(
            receipt.observation,
            NetworkGuardOutcome::Policy(_)
        ));
        assert_eq!(owner.retained_failures()[0].errno, Some(5));
    }
    #[test]
    fn initial_registration_cannot_renew_container_startup_deadline() {
        let (mut owner, control, probe) = make(None, false);
        let original = owner.startup_deadline();
        unsafe {
            control.register_stopped_initial(
                std::io::stdout().as_fd(),
                original + Duration::from_secs(60),
            )
        }
        .unwrap();
        assert_eq!(*probe.received_deadline.lock().unwrap(), Some(original));
        owner.stop_and_join(Instant::now()).unwrap();
        let (mut owner, control, probe) = make(None, false);
        let earlier = owner.startup_deadline() - Duration::from_millis(50);
        unsafe { control.register_stopped_initial(std::io::stdout().as_fd(), earlier) }.unwrap();
        assert_eq!(*probe.received_deadline.lock().unwrap(), Some(earlier));
        owner.stop_and_join(Instant::now()).unwrap();
    }
}
