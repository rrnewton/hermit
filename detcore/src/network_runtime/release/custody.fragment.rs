/// Opaque identity. Domain authorization stays in the run-global Hermit holder:
/// owner/MM + FilesId/fd/installation generation/OFD + call/effect token.
/// No public constructor or numeric-token lookup grants custody authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReleaseToken {
    pub(crate) incarnation: u64,
    pub(crate) job_id: u64,
}
#[must_use]
pub(crate) struct ReleaseCustody {
    owner: Arc<ServiceOwner>,
    token: ReleaseToken,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustodyPhase {
    Registered,
    Submitted,
    Prepared,
    ReleasePending,
    CompletionObserved,
}
#[derive(Debug, Clone)]
pub(crate) struct CustodySnapshot {
    pub(crate) token: ReleaseToken,
    pub(crate) phase: CustodyPhase,
    pub(crate) version: u64,
    pub(crate) owns_original: bool,
    pub(crate) owns_escrow: bool,
    pub(crate) submission_uncertain: bool,
    pub(crate) interrupt_requested: bool,
    pub(crate) terminal_requested: bool,
    pub(crate) normal_close_result: Option<Result<(), i32>>,
    pub(crate) worker_reaped: Option<i32>,
    pub(crate) supervisor_reaped: Option<(i32, u64)>,
    pub(crate) error: Option<StoredError>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseContext {
    NormalClose,
    ExitBeforeClose,
    TerminalAfterNormalStart,
}
/// Proof ONLY of releasing this physical reference and reaping its worker.
/// In particular, TerminalAfterNormalStart with normal_close_result=None does
/// not settle any unknown guest read/drain/copy or primary syscall effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReleaseCompletion {
    pub(crate) token: ReleaseToken,
    pub(crate) worker_pid: i32,
    pub(crate) worker_start_ticks: u64,
    pub(crate) wait_status: i32,
    pub(crate) context: ReleaseContext,
    pub(crate) normal_close_result: Option<Result<(), i32>>,
}
impl ReleaseCustody {
    pub(crate) fn token(&self) -> ReleaseToken {
        self.token
    }
    fn waiter<'pin>(&self, holder: &'pin mut Option<OwnedFd>) -> std::io::Result<ReleaseJob<'pin>> {
        if self.owner.state.lock().unwrap().incarnation != self.token.incarnation {
            return Err(std::io::Error::other(
                "release service incarnation mismatch",
            ));
        }
        attach_owned(&self.owner, self.token.job_id, holder)
    }
    pub(crate) fn snapshot(&self) -> std::io::Result<CustodySnapshot> {
        let state = self.owner.state.lock().unwrap();
        let r = state
            .jobs
            .get(&self.token.job_id)
            .ok_or_else(|| std::io::Error::other("missing retained custody record"))?;
        Ok(CustodySnapshot {
            token: self.token,
            version: r.version,
            phase: if r.outcome_observed {
                CustodyPhase::CompletionObserved
            } else if r.started || r.start_pending.is_some() || r.original_removed {
                CustodyPhase::ReleasePending
            } else if r.possessed_pid.is_some()
                && r.possessed_pid == r.dropped_pid
                && r.worker_pidfd.is_some()
            {
                CustodyPhase::Prepared
            } else if r.submitted {
                CustodyPhase::Submitted
            } else {
                CustodyPhase::Registered
            },
            owns_original: r.pin.is_some(),
            owns_escrow: r.escrow.is_some(),
            submission_uncertain: r.submission_uncertain,
            interrupt_requested: r.interrupt_requested,
            terminal_requested: r.terminal_requested,
            normal_close_result: r.close_result,
            worker_reaped: r.reaped_status,
            supervisor_reaped: r.supervisor_reaped,
            error: r.error.clone().or_else(|| state.error.clone()),
        })
    }
    /// One run-global observer per custody, independent of guest waiters.
    /// Register after taking a snapshot, then recheck readiness; progress that
    /// raced the registration invokes this waker immediately outside our lock.
    pub(crate) fn register_waker(
        &self,
        observed_version: u64,
        waker: &std::task::Waker,
    ) -> std::io::Result<()> {
        let next = waker.clone();
        let (previous, immediate) = {
            let mut state = self.owner.state.lock().unwrap();
            let service_terminal = state.error.is_some() || state.broker_status.is_some();
            let r = state
                .jobs
                .get_mut(&self.token.job_id)
                .ok_or_else(|| std::io::Error::other("missing retained custody record"))?;
            if r.version != observed_version || r.error.is_some() || service_terminal {
                (None, Some(next))
            } else {
                (
                    r.completion_waker
                        .replace((observed_version, next))
                        .map(|(_, old)| old),
                    None,
                )
            }
        };
        drop(previous);
        if let Some(waker) = immediate {
            waker.wake();
        }
        Ok(())
    }
    pub(crate) fn request_start(&self, terminal: bool) -> std::io::Result<()> {
        {
            let mut state = self.owner.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            let r = state.jobs.get_mut(&self.token.job_id).unwrap();
            if r.started || r.start_pending.is_some() {
                return Err(std::io::Error::other("release already requested"));
            }
            if !prepared_record(r)? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "release preparation pending",
                ));
            }
            r.start_pending = Some(terminal || r.terminal_requested);
            r.changed();
        }
        self.owner.wake();
        Ok(())
    }
    pub(crate) fn try_observe_completion(&self) -> std::io::Result<Option<ReleaseCompletion>> {
        let proof = {
            let mut state = self.owner.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            let r = state.jobs.get_mut(&self.token.job_id).unwrap();
            if let Some(error) = &r.error {
                return Err(error.io());
            }
            if !r.started
                || !release_outcome_ready(
                    r.started_as_exit,
                    r.terminal_requested,
                    r.reaped_status,
                    r.close_result,
                )?
            {
                return Ok(None);
            }
            let Some((status, ticks)) = r.supervisor_reaped else {
                return Ok(None);
            };
            if r.rejected_without_child
                || r.reaped_status != Some(status)
                || ticks == 0
                || r.pin.is_some()
            {
                return Err(std::io::Error::other(
                    "physical release evidence incomplete or contradictory",
                ));
            }
            release_empty(r)?;
            let worker_pid = r
                .possessed_pid
                .ok_or_else(|| std::io::Error::other("missing exact worker identity"))?;
            if !r.outcome_observed {
                r.outcome_observed = true;
                r.changed();
            }
            ReleaseCompletion {
                token: self.token,
                worker_pid,
                worker_start_ticks: ticks,
                wait_status: status,
                context: if r.started_as_exit {
                    ReleaseContext::ExitBeforeClose
                } else if r.terminal_requested {
                    ReleaseContext::TerminalAfterNormalStart
                } else {
                    ReleaseContext::NormalClose
                },
                normal_close_result: r.close_result,
            }
        };
        self.owner.wake();
        Ok(Some(proof))
    }
    pub(crate) fn wait_prepared(&self, deadline: Instant) -> std::io::Result<()> {
        let mut holder = None;
        let result = self.waiter(&mut holder)?.await_prepared(deadline);
        result
    }
    pub(crate) fn start(&self, terminal: bool) -> std::io::Result<()> {
        let mut holder = None;
        let result = self.waiter(&mut holder)?.start(terminal);
        result
    }
    pub(crate) fn request_interrupt(&self) {
        {
            let mut state = self.owner.state.lock().unwrap();
            state
                .jobs
                .get_mut(&self.token.job_id)
                .unwrap()
                .interrupt_requested = true;
        }
        self.owner.wake();
    }
    pub(crate) fn request_terminal(&self) {
        {
            let mut state = self.owner.state.lock().unwrap();
            state
                .jobs
                .get_mut(&self.token.job_id)
                .unwrap()
                .terminal_requested = true;
        }
        self.owner.wake();
    }
    pub(crate) fn observe_completion(
        &self,
        deadline: Instant,
    ) -> std::io::Result<ReleaseCompletion> {
        let mut holder = None;
        let mut waiter = self.waiter(&mut holder)?;
        waiter.drive_to_bound(deadline)?;
        let mut state = self.owner.state.lock().unwrap();
        loop {
            let r = state.jobs.get(&self.token.job_id).unwrap();
            if let Some((status, ticks)) = r.supervisor_reaped {
                if r.rejected_without_child
                    || r.reaped_status != Some(status)
                    || ticks == 0
                    || r.pin.is_some()
                    || r.escrow.is_some()
                {
                    return Err(std::io::Error::other(
                        "physical release evidence incomplete or contradictory",
                    ));
                }
                return Ok(ReleaseCompletion {
                    token: self.token,
                    worker_pid: r
                        .possessed_pid
                        .ok_or_else(|| std::io::Error::other("missing exact worker identity"))?,
                    worker_start_ticks: ticks,
                    wait_status: status,
                    context: if r.started_as_exit {
                        ReleaseContext::ExitBeforeClose
                    } else if r.terminal_requested {
                        ReleaseContext::TerminalAfterNormalStart
                    } else {
                        ReleaseContext::NormalClose
                    },
                    normal_close_result: r.close_result,
                });
            }
            if let Some(error) = r.error.as_ref().or(state.error.as_ref()) {
                return Err(error.io());
            }
            state = bounded_wait(&self.owner, state, deadline)?;
        }
    }
}
impl Drop for ReleaseCustody {
    fn drop(&mut self) {
        {
            let mut state = self.owner.state.lock().unwrap();
            if let Some(r) = state.jobs.get_mut(&self.token.job_id) {
                r.waiters -= 1;
            }
        }
        // Capability cancellation cannot close a target, erase an unobserved
        // outcome, or remove the durable record. The run-global owner remains.
        self.owner.wake();
    }
}
#[must_use]
pub(crate) struct ReleaseDrainPending {
    pub(crate) error: std::io::Error,
    pub(crate) completed: Vec<ReleaseCompletion>,
    // Actual remaining pins/escrows/jobs/pidfds and receipts, not numeric IDs.
    // The run-global terminal holder must retain this on every Err path.
    pub(crate) service: ReleaseService,
}
pub(crate) struct DrainedReleaseService {
    pub(crate) completed: Vec<ReleaseCompletion>,
    pub(crate) incarnation: u64,
    pub(crate) final_watermark: u64,
    pub(crate) broker_pid: i32,
    pub(crate) broker_wait_status: i32,
}
impl ReleaseService {
    /// Consumes the run-global service into either complete cleanup evidence or
    /// an owned recovery capability. Never confuses broker exit with settling
    /// unknown primary syscall effects, and never discards pending physical FDs.
    pub(crate) fn drain_terminal(
        mut self,
        deadline: Instant,
    ) -> Result<DrainedReleaseService, ReleaseDrainPending> {
        let mut completed = Vec::new();
        let result = (|| {
            let custody = {
                let mut state = self.owner.state.lock().unwrap();
                let incarnation = state.incarnation;
                if state.jobs.values().any(|r| r.waiters == usize::MAX) {
                    return Err(std::io::Error::other("terminal custody count exhausted"));
                }
                let mut custody = Vec::with_capacity(state.jobs.len());
                for r in state.jobs.values_mut() {
                    r.terminal_requested = true;
                    r.waiters += 1;
                    custody.push(ReleaseCustody {
                        owner: self.owner.clone(),
                        token: ReleaseToken {
                            incarnation,
                            job_id: r.id,
                        },
                    });
                }
                custody
            };
            self.owner.wake();
            for owned in custody {
                let snapshot = owned.snapshot()?;
                if !matches!(
                    snapshot.phase,
                    CustodyPhase::ReleasePending | CustodyPhase::CompletionObserved
                ) {
                    owned.wait_prepared(deadline)?;
                    owned.start(true)?;
                }
                completed.push(owned.observe_completion(deadline)?);
            }
            self.shutdown(deadline)?;
            let state = self.owner.state.lock().unwrap();
            if state
                .jobs
                .values()
                .any(|r| r.pin.is_some() || r.escrow.is_some() || !r.outcome_observed)
            {
                return Err(std::io::Error::other(
                    "terminal drain retains unresolved custody or outcome",
                ));
            }
            Ok((
                state.incarnation,
                state.received_prefix,
                state.broker_status.unwrap(),
            ))
        })();
        match result {
            Ok((incarnation, final_watermark, broker_wait_status)) => Ok(DrainedReleaseService {
                completed,
                incarnation,
                final_watermark,
                broker_pid: self.broker_pid,
                broker_wait_status,
            }),
            Err(error) => Err(ReleaseDrainPending {
                error,
                completed,
                service: self,
            }),
        }
    }
}
