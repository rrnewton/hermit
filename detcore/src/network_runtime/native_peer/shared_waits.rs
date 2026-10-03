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
