//! Native quiescence for the exact already-collected helper Call. It does not
//! weaken Calls::settled or settle that Call's semantic copy receipts.
use super::*;

impl Calls {
    pub(in crate::network_runtime) fn receive_retry_release_known(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<bool> {
        let state = self
            .calls
            .get(&call)
            .filter(|c| c.owner == owner)
            .ok_or_else(|| io::Error::other("retry release lost its actual Call"))?;
        if !state.leases.is_empty() || state.invocation.is_some() || state.terminal.is_some() {
            return Err(io::Error::other(
                "retry release retains unresolved native operations",
            ));
        }
        match state.release {
            Some(release)
                if state.releasing
                    && state.original.is_none()
                    && release.original != Some(libc::EBADF) =>
            {
                Ok(true)
            }
            None if !state.releasing && state.original.is_some() => Ok(false),
            _ => Err(io::Error::other(
                "retry release lacks a known valid close or unsubmitted pin",
            )),
        }
    }
    #[cfg(test)]
    pub(in crate::network_runtime) fn retry_fixture_pin(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> std::sync::Arc<OwnedFd> {
        self.owned(owner, call)
            .unwrap()
            .original
            .as_ref()
            .unwrap()
            .clone()
    }
    #[cfg(test)]
    pub(in crate::network_runtime) fn retry_fixture_pin_count(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> usize {
        std::sync::Arc::strong_count(self.owned(owner, call).unwrap().original.as_ref().unwrap())
    }
    /// The completed probe lease is gone, but this exact physical Call remains.
    /// This does not pretend that Calls::settled holds with a live pin.
    pub(in crate::network_runtime) fn require_receive_retry_quiescence(
        &self,
        source: &crate::network_replay::RecordNoStore,
    ) -> io::Result<()> {
        self.require_copy_quiescence(source.completion())?;
        let binding = source.completion().binding();
        let call = self
            .calls
            .get(&binding.call())
            .expect("exact sole Call checked");
        if !call.leases.is_empty() {
            return Err(io::Error::other(
                "receive retry retains an unresolved native lease",
            ));
        }
        Ok(())
    }

    pub(in crate::network_runtime) fn require_copy_quiescence(
        &self,
        completion: &super::super::HelperCopyCompletion,
    ) -> io::Result<()> {
        let binding = completion.binding();
        // No other unresolved native original may still write guest memory or
        // need a guest continuation. This deliberately narrow initial-root
        // subset must be expanded through positive provenance, not omission.
        let state = self
            .calls
            .get(&binding.call())
            .filter(|state| self.calls.len() == 1 && state.owner == binding.owner())
            .ok_or_else(|| io::Error::other("copy exclusion has another unresolved native Call"))?;
        if state.acquisition.is_err()
            || state.original.is_none()
            || state.invocation.is_some()
            || state.publication.is_none()
            || state.releasing
            || state.release.is_some()
            || state.terminal.is_some()
            || state
                .identity
                .is_none_or(|identity| !binding.matches_file(identity))
            || state
                .leases
                .values()
                .any(|lease| lease.pending.as_ref().is_some_and(|p| !p.confirmed))
        {
            return Err(io::Error::other(
                "copy exclusion lacks exact held-file quiescence",
            ));
        }
        // The source's original probe/Pending may have retired. Its immutable
        // Completion and actual joined-worker receipt survive on the engine Call.
        completion.joined_worker()?;
        Ok(())
    }
}

#[cfg(test)]
impl super::super::NetworkRuntimeResources {
    /// Actual same-OFD Peek and retained joined worker. Provider copy5 rows are
    /// controlled component premises, exactly as in the earlier helper fixture.
    pub(crate) async fn controlled_receive_retry_peek(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) -> io::Result<Observation> {
        let effect = Effect::Peek { maximum: 1024 };
        let work = {
            let mut calls = self.shared.native_streams.lock().unwrap();
            if !calls.owned(owner, call)?.leases.contains_key(&lease) {
                return Err(io::Error::other("retry helper lost actual probe lease"));
            }
            calls.prepare(owner, lease, effect.clone())?
        };
        let held = work.helper.as_ref().unwrap().clone();
        engine
            .lock()
            .unwrap()
            .bind_helper_copy(held.binding())
            .map_err(io::Error::other)?;
        let shared = self.shared.clone();
        let retained_effect = effect.clone();
        let (worker, reply) =
            self.shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    let actual = work.perform();
                    let observed = held.controlled_observation(actual, 5)?;
                    shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        &retained_effect,
                        observed.clone(),
                    )?;
                    Ok(observed)
                })?;
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)??;
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.shared.join_native_worker_receipt(&worker),
        )
        .await
        .map_err(io::Error::other)??;
        super::super::helper_receive::retain_joined_helper(
            &self.shared,
            owner,
            lease,
            &effect,
            &observed,
            joined,
        )?;
        Ok(observed)
    }
}

/// Borrowed only from the actual live Calls ledger while native admission is
/// locked. The engine additionally verifies cursor and origin before commit.
pub(crate) struct ConfirmedNoStore<'a> {
    call: &'a Call,
    lease: NetworkStreamLeaseId,
}
impl ConfirmedNoStore<'_> {
    pub(crate) fn matches(
        &self,
        source: &crate::network_replay::RecordNoStore,
        identity: super::super::original_installation::FileIdentity,
        cursor: Option<i32>,
    ) -> bool {
        let completion = source.completion();
        let binding = completion.binding();
        let Some(record) = self.call.leases.get(&self.lease) else {
            return false;
        };
        let Some(pending) = record.pending.as_ref() else {
            return false;
        };
        let Some(observed) = pending.result.as_ref() else {
            return false;
        };
        if self.call.id != binding.call()
            || self.call.owner != binding.owner()
            || self.call.identity != Some(identity)
            || !binding.matches_file(identity)
            || self.call.leases.len() != 1
            || self.lease != binding.lease()
            || record.private_predecessor.is_some()
            || !pending.confirmed
            || record.peek.as_ref() != Some(source.observed())
        {
            return false;
        }
        match pending.effect {
            Effect::Peek { .. } => {
                pending.effect == *binding.effect()
                    && observed == source.observed()
                    && pending
                        .helper
                        .as_ref()
                        .is_some_and(|h| h.check_completion(Some(completion)).is_ok())
            }
            Effect::SetPeekOffset { value } => {
                cursor == Some(value)
                    && value >= 0
                    && pending.helper.is_none()
                    && observed.helper_copy.is_none()
                    && observed.raw_return == 0
                    && observed.errno.is_none()
                    && observed.bytes.is_empty()
                    && observed.confirmation == ResultValue::Unit
            }
            _ => false,
        }
    }
}
impl Calls {
    pub(in crate::network_runtime) fn confirmed_no_store(
        &self,
        source: &crate::network_replay::RecordNoStore,
        origin: &super::super::native_copy_exclusion::NoStoreJoinOrigin,
    ) -> io::Result<ConfirmedNoStore<'_>> {
        self.require_copy_quiescence(source.completion())?;
        let binding = source.completion().binding();
        let call = self
            .calls
            .get(&binding.call())
            .expect("quiescence validated actual Call");
        if call
            .leases
            .get(&binding.lease())
            .and_then(|l| l.no_store_join.as_ref())
            .is_none_or(|first| !first.same(origin))
        {
            return Err(io::Error::other(
                "no-store completion lost its first retained native prefix",
            ));
        }
        Ok(ConfirmedNoStore {
            call,
            lease: binding.lease(),
        })
    }
    pub(in crate::network_runtime) fn retain_no_store_join(
        &mut self,
        source: &crate::network_replay::RecordNoStore,
        origin: &super::super::native_copy_exclusion::NoStoreJoinOrigin,
    ) -> io::Result<()> {
        self.require_copy_quiescence(source.completion())?;
        let binding = source.completion().binding();
        let record = self
            .calls
            .get_mut(&binding.call())
            .unwrap()
            .leases
            .get_mut(&binding.lease())
            .ok_or_else(|| io::Error::other("no-store first join lost actual lease"))?;
        if record.peek.as_ref() != Some(source.observed()) {
            return Err(io::Error::other(
                "no-store first join changed exact retained source",
            ));
        }
        match &record.no_store_join {
            Some(first) if !first.same(origin) => Err(io::Error::other(
                "no-store cannot replace its first joined native prefix",
            )),
            Some(_) => Ok(()),
            None => {
                record.no_store_join = Some(origin.clone());
                Ok(())
            }
        }
    }
    pub(in crate::network_runtime) fn retire_completed_no_store(
        &mut self,
        source: &crate::network_replay::RecordNoStore,
    ) {
        let binding = source.completion().binding();
        self.calls
            .get_mut(&binding.call())
            .expect("same locked completed Call")
            .leases
            .remove(&binding.lease())
            .expect("same positively committed no-store lease");
    }
}
