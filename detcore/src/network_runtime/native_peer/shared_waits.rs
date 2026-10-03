//! Complete actual native Call census for one serialized shared attempt. A
//! retained lifetime pin is allowed only for its exact authenticated engine Call.
use super::*;
use crate::network_replay::shared_waits::SharedCallCensus;

impl Calls {
    pub(in crate::network_runtime) fn require_shared_quiescence(
        &self,
        census: &SharedCallCensus,
    ) -> io::Result<()> {
        let expected = census.rows.iter().filter(|row| row.native.is_some());
        if expected.clone().count() != self.calls.len() {
            return Err(io::Error::other(
                "shared census omitted or added a native Call",
            ));
        }
        for row in expected {
            let state = self
                .calls
                .get(&row.call)
                .ok_or_else(|| io::Error::other("shared census lost actual retained pin"))?;
            if state.id != row.call
                || state.owner != row.owner
                || state.identity != row.native
                || state.acquisition.is_err()
                || state.original.is_none()
                || state.publication.is_none()
                || state.invocation.is_some()
                || state.terminal.is_some()
                || state.releasing
                || state.release.is_some()
                || !state.leases.is_empty()
            {
                return Err(io::Error::other(
                    "shared census retains unretired native/helper/cursor debt",
                ));
            }
        }
        Ok(())
    }
}

impl Calls {
    /// The selected acquisition is the only permitted difference from the
    /// retained suspended peer census. No numeric omission grants quiescence.
    pub(in crate::network_runtime) fn require_shared_capture(
        &self,
        peers: &SharedCallCensus,
        origin: &crate::network_replay::shared_waits::SharedCaptureOrigin,
        acquired: bool,
    ) -> io::Result<()> {
        let expected = peers.rows.iter().filter(|row| row.native.is_some());
        if expected.clone().count() + usize::from(acquired) != self.calls.len()
            || peers.rows.iter().any(|row| row.call == origin.call())
            || (!acquired && self.calls.contains_key(&origin.call()))
        {
            return Err(io::Error::other(
                "shared capture changed complete native Call population",
            ));
        }
        for row in expected {
            let state = self
                .calls
                .get(&row.call)
                .ok_or_else(|| io::Error::other("shared capture lost original peer pin"))?;
            if state.id != row.call
                || state.owner != row.owner
                || state.identity != row.native
                || state.acquisition.is_err()
                || state.original.is_none()
                || state.publication.is_none()
                || state.invocation.is_some()
                || state.terminal.is_some()
                || state.releasing
                || state.release.is_some()
                || !state.leases.is_empty()
            {
                return Err(io::Error::other(
                    "shared capture peer retains native effect debt",
                ));
            }
        }
        if acquired {
            let state = self
                .calls
                .get(&origin.call())
                .ok_or_else(|| io::Error::other("shared capture lost selected actual pin"))?;
            if state.id != origin.call()
                || state.owner != origin.owner()
                || state.identity != Some(origin.identity())
                || state.acquisition != Ok(())
                || state.original.is_none()
                || state.publication.is_none()
                || state.invocation.is_some()
                || state.terminal.is_some()
                || state.releasing
                || state.release.is_some()
                || !state.leases.is_empty()
            {
                return Err(io::Error::other(
                    "shared capture lacks exact successful idle acquisition",
                ));
            }
        }
        Ok(())
    }
}

impl Calls {
    pub(in crate::network_runtime) fn shared_probe_engine(
        &self,
        origin: &crate::network_replay::shared_waits::SharedRecordProbe,
    ) -> io::Result<std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>>
    {
        let state = self
            .calls
            .get(&origin.call())
            .filter(|c| {
                c.owner == origin.owner()
                    && c.identity == Some(origin.identity())
                    && c.original.is_some()
                    && c.acquisition == Ok(())
                    && !c.releasing
                    && c.release.is_none()
                    && c.terminal.is_none()
                    && c.invocation.is_none()
            })
            .ok_or_else(|| io::Error::other("shared probe lost captured original Call"))?;
        Ok(state
            .publication
            .as_ref()
            .ok_or_else(|| io::Error::other("shared probe lost original engine owner"))?
            .engine
            .clone())
    }

    pub(in crate::network_runtime) fn require_shared_probe(
        &self,
        peers: &SharedCallCensus,
        origin: &crate::network_replay::shared_waits::SharedRecordProbe,
        bound: bool,
    ) -> io::Result<()> {
        let native = peers.rows.iter().filter(|r| r.native.is_some());
        if self.calls.len() != native.clone().count() + 1
            || peers.rows.iter().any(|r| r.call == origin.call())
        {
            return Err(io::Error::other("shared probe omitted a native Call"));
        }
        for row in native {
            let c = self
                .calls
                .get(&row.call)
                .ok_or_else(|| io::Error::other("shared probe lost peer pin"))?;
            if c.id != row.call
                || c.owner != row.owner
                || c.identity != row.native
                || c.acquisition != Ok(())
                || c.original.is_none()
                || c.publication.is_none()
                || c.invocation.is_some()
                || c.terminal.is_some()
                || c.releasing
                || c.release.is_some()
                || !c.leases.is_empty()
            {
                return Err(io::Error::other("shared probe peer has native effect debt"));
            }
        }
        self.shared_probe_engine(origin)?;
        let selected = &self.calls[&origin.call()];
        if selected.leases.len() != usize::from(bound)
            || (bound && !selected.leases.contains_key(&origin.lease()))
        {
            return Err(io::Error::other(
                "shared probe changed selected native lease",
            ));
        }
        Ok(())
    }

    pub(in crate::network_runtime) fn preflight_shared_probe_retirement(
        &self,
        origin: &crate::network_replay::shared_waits::SharedRecordProbe,
        effects: &[std::sync::Arc<super::super::shared_waits::JoinedSharedEffect>],
    ) -> io::Result<()> {
        let state = self
            .calls
            .get(&origin.call())
            .ok_or_else(|| io::Error::other("shared probe Call absent"))?;
        let lease = state
            .leases
            .get(&origin.lease())
            .ok_or_else(|| io::Error::other("shared probe lease absent"))?;
        let pending = lease
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::other("shared probe never ran final scan"))?;
        let last = effects
            .last()
            .ok_or_else(|| io::Error::other("shared probe lacks joined effects"))?;
        if state.owner != origin.owner()
            || !pending.confirmed
            || pending.effect != Effect::PollState
            || pending.result.as_ref() != Some(last.observed())
            || last.step().effect() != &Effect::PollState
            || lease.private_predecessor.is_some()
            || lease.no_store_join.is_some()
        {
            return Err(io::Error::other(
                "shared probe final result is not exact confirmed scan",
            ));
        }
        let peeks: Vec<_> = effects
            .iter()
            .filter(|e| matches!(e.step().effect(), Effect::Peek { .. }))
            .collect();
        match (lease.peek.as_ref(), peeks.as_slice()) {
            (None, []) => {}
            (Some(actual), [effect]) if actual == effect.observed() => {
                actual
                    .helper_copy
                    .as_ref()
                    .ok_or_else(|| io::Error::other("shared probe lost helper retirement"))?
                    .joined_worker()?;
            }
            _ => {
                return Err(io::Error::other(
                    "shared probe changed retained Peek/worker history",
                ));
            }
        }
        Ok(())
    }
}
impl Calls {
    /// Called with the same mutex after exact source preflight and engine
    /// reservation. Preserve the original Peek owner before retiring its lease.
    pub(in crate::network_runtime) fn transfer_shared_record_delivery(
        &mut self,
        source: &std::sync::Arc<crate::network_replay::shared_waits::SharedRecordReceiveSource>,
    ) -> io::Result<()> {
        let owner = source.owner();
        let call = source.call();
        let state = self.owned(owner, call)?;
        let old = state
            .leases
            .get(&source.origin().lease())
            .ok_or_else(|| io::Error::other("shared delivery lost original probe lease"))?;
        if state.identity != Some(source.origin().identity())
            || state.acquisition != Ok(())
            || state.original.is_none()
            || state.publication.is_none()
            || state.invocation.is_some()
            || state.releasing
            || state.release.is_some()
            || state.terminal.is_some()
            || state.leases.len() != 1
            || source.lease() == source.origin().lease()
            || state.leases.contains_key(&source.lease())
            || old.private_predecessor.is_some()
            || old.no_store_join.is_some()
            || old.peek.as_ref().and_then(|p| p.helper_copy.as_ref()) != Some(source.predecessor())
            || old.pending.as_ref().is_none_or(|p| {
                !p.confirmed
                    || p.effect != Effect::PollState
                    || p.result.as_ref() != source.effects().last().map(|e| e.observed())
            })
        {
            return Err(io::Error::other(
                "shared delivery changed exact confirmed probe/file predecessor",
            ));
        }
        source.predecessor().joined_worker()?;
        let successor = Lease {
            private_predecessor: Some(source.predecessor().clone()),
            ..Lease::default()
        };
        assert!(
            state.leases.insert(source.lease(), successor).is_none(),
            "engine allocated a live native delivery lease"
        );
        state
            .leases
            .remove(&source.origin().lease())
            .expect("same-lock original probe preflight");
        Ok(())
    }
    pub(in crate::network_runtime) fn require_shared_record_delivery(
        &self,
        peers: &SharedCallCensus,
        source: &std::sync::Arc<crate::network_replay::shared_waits::SharedRecordReceiveSource>,
    ) -> io::Result<()> {
        let expected = peers.rows.iter().filter(|r| r.native.is_some());
        if self.calls.len() != expected.clone().count() + 1
            || peers.rows.iter().any(|r| r.call == source.call())
        {
            return Err(io::Error::other(
                "shared delivery changed complete native population",
            ));
        }
        for row in expected {
            let c = self
                .calls
                .get(&row.call)
                .ok_or_else(|| io::Error::other("shared delivery omitted native peer"))?;
            if c.id != row.call
                || c.owner != row.owner
                || c.identity != row.native
                || c.acquisition != Ok(())
                || c.original.is_none()
                || c.publication.is_none()
                || c.invocation.is_some()
                || c.terminal.is_some()
                || c.releasing
                || c.release.is_some()
                || !c.leases.is_empty()
            {
                return Err(io::Error::other(
                    "shared delivery peer retains unresolved native work",
                ));
            }
        }
        self.shared_probe_engine(source.origin())?;
        let c = &self.calls[&source.call()];
        let lease = c
            .leases
            .get(&source.lease())
            .ok_or_else(|| io::Error::other("shared delivery lost its actual successor"))?;
        if c.leases.len() != 1
            || lease.private_predecessor.as_ref() != Some(source.predecessor())
            || lease.peek.is_some()
            || lease.no_store_join.is_some()
        {
            return Err(io::Error::other(
                "shared delivery changed exact original predecessor",
            ));
        }
        Ok(())
    }
    pub(in crate::network_runtime) fn prepare_shared_record_drain(
        &mut self,
        submission: &std::sync::Arc<
            crate::network_replay::shared_waits::SharedRecordDrainSubmission,
        >,
    ) -> io::Result<Execution> {
        let source = submission.source();
        let state = self.owned(source.owner(), source.call())?;
        let record = state
            .leases
            .get_mut(&source.lease())
            .ok_or_else(|| io::Error::other("shared Drain lost successor lease"))?;
        let effect = Effect::Drain {
            maximum: source.len(),
        };
        if record.pending.is_some()
            || record.private_predecessor.as_ref() != Some(source.predecessor())
            || record.peek.is_some()
            || record.no_store_join.is_some()
            || state.identity != Some(source.origin().identity())
            || state.acquisition != Ok(())
            || state.invocation.is_some()
            || state.original.is_none()
            || state.publication.is_none()
            || state.releasing
            || state.release.is_some()
            || state.terminal.is_some()
        {
            return Err(io::Error::other(
                "shared Drain lost original stored successor",
            ));
        }
        validate(&effect)?;
        let held = super::super::helper_receive::Held::new_successor(
            source.owner(),
            source.call(),
            source.lease(),
            source.origin().identity(),
            effect.clone(),
            Some(source.predecessor().clone()),
        )?;
        record.pending = Some(Pending {
            effect: effect.clone(),
            result: None,
            confirmed: false,
            helper: Some(held.clone()),
        });
        Ok(Execution {
            original: state.original.as_ref().unwrap().clone(),
            effect,
            helper: Some(held),
            publication: state.publication.clone(),
        })
    }
}

impl Calls {
    pub(in crate::network_runtime) fn preflight_shared_record_drain_retirement(
        &self,
        source: &std::sync::Arc<crate::network_replay::shared_waits::SharedRecordReceiveSource>,
        observed: &Observation,
    ) -> io::Result<()> {
        let c = self
            .calls
            .get(&source.call())
            .ok_or_else(|| io::Error::other("shared Drain retirement lost actual Call"))?;
        let lease = c
            .leases
            .get(&source.lease())
            .ok_or_else(|| io::Error::other("shared Drain retirement lost successor"))?;
        let pending = lease
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::other("shared Drain retirement lacks actual Pending"))?;
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| io::Error::other("shared Drain retirement lacks actual copy5 owner"))?;
        if c.owner != source.owner()
            || c.identity != Some(source.origin().identity())
            || c.acquisition != Ok(())
            || c.original.is_none()
            || c.publication.is_none()
            || c.invocation.is_some()
            || c.terminal.is_some()
            || c.releasing
            || c.release.is_some()
            || c.leases.len() != 1
            || lease.private_predecessor.as_ref() != Some(source.predecessor())
            || lease.peek.is_some()
            || lease.no_store_join.is_some()
            || !pending.confirmed
            || pending.result.as_ref() != Some(observed)
            || pending.effect
                != (Effect::Drain {
                    maximum: source.len(),
                })
            || !completion.binding().succeeds(source.predecessor())
            || completion.binding().lease() != source.lease()
            || pending
                .helper
                .as_ref()
                .is_none_or(|h| h.check_completion(Some(completion)).is_err())
        {
            return Err(io::Error::other(
                "shared Drain retirement changed positive exact successor completion",
            ));
        }
        completion.joined_worker()?;
        Ok(())
    }
}

impl Calls {
    /// Closed original-send phase, alongside the unchanged complete peer census.
    /// 0 precedes capture, 1 requires the genuine prepared pin, 2 requires its
    /// actual provider retirement and physical release. No missing Call is idle.
    pub(in crate::network_runtime) fn require_shared_send<'a>(
        &'a self,
        peers: &SharedCallCensus,
        origin: &std::sync::Arc<crate::network_replay::shared_send::SharedRecordSend>,
        phase: u8,
    ) -> io::Result<Option<&'a super::super::original_send::BlockingCapture>> {
        let selected = origin.admission();
        let expected = peers.rows.iter().filter(|row| row.native.is_some());
        if phase > 2 || self.calls.len() != expected.clone().count() + usize::from(phase != 0)
            || peers.rows.iter().any(|row| row.call == selected.call)
            || (phase == 0 && self.calls.contains_key(&selected.call)) {
            return Err(io::Error::other("shared send changed exact native Call population"));
        }
        for row in expected {
            let c=self.calls.get(&row.call).ok_or_else(||io::Error::other("shared send lost original peer pin"))?;
            if c.id!=row.call || c.owner!=row.owner || c.identity!=row.native || c.acquisition!=Ok(())
                || c.original.is_none() || c.publication.is_none() || c.invocation.is_some()
                || c.terminal.is_some() || c.releasing || c.release.is_some() || !c.leases.is_empty() {
                return Err(io::Error::other("shared send peer retains physical effect debt"));
            }
        }
        if phase == 0 { return Ok(None); }
        let c=self.calls.get(&selected.call).ok_or_else(||io::Error::other("shared send selected native Call absent"))?;
        let original=c.invocation.as_ref().ok_or_else(||io::Error::other("shared send original invocation absent"))?;
        if c.id!=selected.call || c.owner!=origin.owner() || c.acquisition!=Ok(()) || c.identity.is_some()
            || c.terminal.is_some() || !c.leases.is_empty() || original.admission!=*selected
            || original.shared_send.as_ref().is_none_or(|held|!std::sync::Arc::ptr_eq(held,origin))
            || original.canceled || original.terminal.is_some() || original.terminating || original.failed_collection.is_some()
            || !matches!(original.pin,Some(OriginalPin::Socket{domain:libc::AF_INET,kind:libc::SOCK_STREAM,protocol:libc::IPPROTO_TCP,..}))
            || original.prepare_request.is_none() || original.prepared.is_none() {
            return Err(io::Error::other("shared send changed admitted original pin/provider owner"));
        }
        if phase == 1 {
            if c.original.is_none() || c.releasing || c.release.is_some() || original.close_queued
                || original.selection.is_some() || original.completion.is_some() || original.retired {
                return Err(io::Error::other("shared send preparation already has native effects"));
            }
            return Ok(None);
        }
        if c.original.is_some() || !c.releasing || c.release.is_none_or(|r|r.original==Some(libc::EBADF))
            || !original.close_queued || !original.retired || original.retirement_request.is_none()
            || original.selection.is_none() || original.completion_request.is_none() {
            return Err(io::Error::other("shared send lacks positive actual close/provider retirement"));
        }
        let effect=original.completion.as_ref().ok_or_else(||io::Error::other("shared send completion absent"))?;
        let capture=effect.blocking_send.as_ref().ok_or_else(||io::Error::other("shared send typed capture absent"))?;
        let timeout=super::super::original_send::BlockingTimeout::from_ticks(origin.timeout())?;
        capture.validate(effect,timeout)?;
        Ok(Some(capture))
    }
}
