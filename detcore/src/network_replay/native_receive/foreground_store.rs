//! Exact bounded foreground stores retained by the existing stream Call.
//! A complete copy is not stream consumption, trace publication or a return.
use std::sync::Arc;
use std::sync::Mutex;

use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::RemoteIoVec;

use super::*;
use crate::memory::MemoryMetadata;
use crate::memory::OriginalCopySpan;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::HelperCopyCompletion;
use crate::network_runtime::NativeCopyExclusion;
use crate::network_runtime::NetworkRuntimeResources;
use crate::scheduler::ordinary_fd::OrdinaryFdObservation;

/// Raw synchronous backend result. An impossible count or panic stays unknown;
/// it cannot be collapsed to known zero effects or a retryable preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreOutcome {
    Returned(Result<usize, i32>),
    Panicked,
}
#[derive(Debug)]
enum Phase {
    Prepared,
    CheckingAccess,
    Possible,
    // Preserve the actual outcome through failed post-write checks and drop;
    // only the controlled diagnostic accessor reads this unresolved payload.
    Observed { _outcome: StoreOutcome },
    Checked(StoreOutcome),
    ExclusionEnded(StoreOutcome),
}

/// These two opaque source families share exactly one access checker, raw
/// native write and retained-outcome transaction. Replay never manufactures a
/// native helper completion or physical copy geometry.
#[derive(Debug, Clone)]
pub(crate) enum ForegroundStoreSource {
    Record(HelperCopyCompletion),
    Replay(Arc<ReplayStoreSource>),
}
impl ForegroundStoreSource {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        match self {
            Self::Record(completion) => completion.binding().owner(),
            Self::Replay(source) => source.owner(),
        }
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Record(left), Self::Record(right)) => left == right,
            (Self::Replay(left), Self::Replay(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Record(completion) => &completion.capture().committed,
            Self::Replay(source) => source.bytes(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ForegroundStore {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    source: ForegroundStoreSource,
    source_offset: usize,
    length: usize,
    root: Arc<ForegroundRoot>,
    span: OriginalCopySpan,
    exclusion: NativeCopyExclusion,
    epoch: u64,
    phase: Mutex<Phase>,
}
impl ForegroundStore {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.call
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.root
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn span(&self) -> &OriginalCopySpan {
        &self.span
    }
    pub(crate) fn exclusion(&self) -> &NativeCopyExclusion {
        &self.exclusion
    }
    pub(crate) fn length(&self) -> usize {
        self.length
    }
    pub(crate) fn source_offset(&self) -> usize {
        self.source_offset
    }
    pub(crate) fn source(&self) -> &ForegroundStoreSource {
        &self.source
    }
    pub(crate) fn record_completion(&self) -> Result<&HelperCopyCompletion, NetworkReplayError> {
        match &self.source {
            ForegroundStoreSource::Record(completion) => Ok(completion),
            ForegroundStoreSource::Replay(_) => {
                Err(invalid("Replay source has no Record helper completion"))
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn completion(&self) -> &HelperCopyCompletion {
        self.record_completion()
            .expect("controlled Record fixture has its actual helper completion")
    }
    #[cfg(test)]
    pub(crate) fn raw_outcome(&self) -> Option<StoreOutcome> {
        match &*self.phase.lock().unwrap() {
            Phase::Observed { _outcome: raw }
            | Phase::Checked(raw)
            | Phase::ExclusionEnded(raw) => Some(raw.clone()),
            _ => None,
        }
    }
}
/// Issued only after exact full stores and actual end of the same interval.
/// A consumer must also match this Arc against the retained Call and use it once.
#[derive(Debug, Clone)]
pub(crate) struct FullStoreCompletion {
    store: Arc<ForegroundStore>,
}
impl PartialEq for FullStoreCompletion {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.store, &other.store)
    }
}
impl Eq for FullStoreCompletion {}
impl FullStoreCompletion {
    pub(crate) fn store(&self) -> &Arc<ForegroundStore> {
        &self.store
    }

    pub(crate) fn has_ended_full_store(&self) -> bool {
        matches!(&*self.store.phase.lock().unwrap(),
            Phase::ExclusionEnded(StoreOutcome::Returned(Ok(n))) if *n == self.store.length)
    }
}

impl NetworkReplayEngine {
    pub(crate) fn foreground_store_selection(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<(ForegroundStoreSource, usize), NetworkReplayError> {
        if self.mode() == NetworkEngineMode::Replay {
            let source = self.replay_store_selection(owner, lease)?;
            let length = source.bytes().len();
            return Ok((ForegroundStoreSource::Replay(source), length));
        }
        let delivery = self.owned_shadow_delivery(owner, lease)?;
        let source = self
            .owned_stream_call(owner, delivery.call)?
            .private_receive
            .as_ref()
            .ok_or_else(|| invalid("foreground preparation lacks private source"))?;
        let offset = delivery
            .private_offset
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        self.check_private_store_selection(
            owner,
            lease,
            &source.completion,
            offset,
            delivery.selected_len,
        )?;
        if self.stream_calls[&delivery.call].foreground_store.is_some() {
            return Err(invalid("foreground preparation is one use"));
        }
        Ok((
            ForegroundStoreSource::Record(source.completion.clone()),
            delivery.selected_len,
        ))
    }
    fn check_private_store_selection(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        completion: &HelperCopyCompletion,
        source_offset: usize,
        length: usize,
    ) -> Result<NetworkStreamCallId, NetworkReplayError> {
        let delivery = self.owned_shadow_delivery(owner, lease)?;
        let state = self.owned_stream_call(owner, delivery.call)?;
        let operation = self.owned_stream_operation(owner, lease)?;
        let source = state
            .private_receive
            .as_ref()
            .ok_or_else(|| invalid("foreground copy lost canonical source"))?;
        let StreamOperationKind::Delivery {
            at_offset,
            peek_offset,
            selection_len,
            ..
        } = &operation.kind
        else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if self.mode() != NetworkEngineMode::Record
            || state.phase != StreamCallPhase::Active
            || !state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || operation.abandoned
            || source.completion != *completion
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
            || delivery.private_offset != Some(source_offset)
            || delivery.selected_len != length
            || *peek_offset != source_offset
            || *selection_len != length
            || *at_offset != source.cut.bytes
            || self.stream_delivery.get(&operation.open_file) != Some(&lease)
            || delivery.pending.is_some()
            || delivery.drain_started
            || delivery.drained != 0
            || delivery.peek_cursor_confirmed
            || length == 0
            || length > NETWORK_STREAM_CHUNK_LIMIT
            || source_offset
                .checked_add(length)
                .is_none_or(|end| end > source.length)
        {
            return Err(invalid(
                "foreground copy changed its complete bounded live selection",
            ));
        }
        self.check_private_receive_cut(owner, delivery.call, source)?;
        if self
            .channels
            .get(&operation.channel)
            .is_none_or(|c| c.inbound_consumed != source.cut.bytes)
        {
            return Err(invalid("foreground copy changed semantic prefix origin"));
        }
        Ok(delivery.call)
    }

    pub(crate) fn prepare_foreground_store(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        source_memory: (Arc<ForegroundRoot>, &MemoryMetadata),
        span: OriginalCopySpan,
        exclusion: NativeCopyExclusion,
        epoch: u64,
    ) -> Result<Arc<ForegroundStore>, NetworkReplayError> {
        let (root, memory) = source_memory;
        let (source, length) = self.foreground_store_selection(owner, lease)?;
        let (call, offset) = match &source {
            ForegroundStoreSource::Record(completion) => {
                let offset = self
                    .owned_shadow_delivery(owner, lease)?
                    .private_offset
                    .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
                (
                    self.check_private_store_selection(owner, lease, completion, offset, length)?,
                    offset,
                )
            }
            ForegroundStoreSource::Replay(source) => (self.check_replay_store_source(source)?, 0),
        };
        if self.stream_calls[&call].foreground_store.is_some()
            || !root.is_current(owner)
            || !span.matches_root(&root)
            || span.length() != length as u64
            || !exclusion.matches_source(&root, &source)
        {
            return Err(invalid(
                "foreground preparation replaced existing store or exact memory/source custody",
            ));
        }
        memory
            .validate_original_copy_span(owner, &span)
            .map_err(invalid)?;
        let store = Arc::new(ForegroundStore {
            owner,
            call,
            lease,
            source,
            source_offset: offset,
            length,
            root,
            span,
            exclusion,
            epoch,
            phase: Mutex::new(Phase::Prepared),
        });
        self.stream_calls.get_mut(&call).unwrap().foreground_store = Some(store.clone());
        Ok(store)
    }

    pub(super) fn check_foreground_store(
        &self,
        store: &Arc<ForegroundStore>,
    ) -> Result<(), NetworkReplayError> {
        let call = match &store.source {
            ForegroundStoreSource::Record(completion) => self.check_private_store_selection(
                store.owner,
                store.lease,
                completion,
                store.source_offset,
                store.length,
            )?,
            ForegroundStoreSource::Replay(source) => {
                if store.source_offset != 0
                    || store.length != source.bytes().len()
                    || store.owner != source.owner()
                    || store.lease != source.lease()
                {
                    return Err(invalid("Replay foreground copy changed source coordinates"));
                }
                self.check_replay_store_source(source)?
            }
        };
        if call != store.call
            || self.stream_calls[&store.call]
                .foreground_store
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, store))
        {
            return Err(invalid(
                "foreground copy lost its exact retained preparation",
            ));
        }
        Ok(())
    }

    fn check_foreground_access(
        &self,
        store: &Arc<ForegroundStore>,
        grant: &OrdinaryFdObservation<'_>,
        metadata: &MemoryMetadata,
        runtime: &NetworkRuntimeResources,
    ) -> Result<(), NetworkReplayError> {
        if grant.owner() != store.owner
            || grant.epoch() != store.epoch
            || !store.root.is_current(store.owner)
        {
            return Err(invalid("foreground copy changed its granted task/MM/epoch"));
        }
        metadata
            .validate_original_copy_span(store.owner, &store.span)
            .map_err(invalid)?;
        runtime
            .validate_native_copy_exclusion(&store.exclusion)
            .map_err(|error| invalid(&error.to_string()))?;
        self.check_foreground_store(store)
    }

    /// Synchronous issuer, with the actual scheduler and memory guards held.
    /// No caller-supplied count or closure can mint a physical store outcome.
    /// Even a malformed backend count or panic remains retained before return.
    pub(crate) fn perform_foreground_store<M: MemoryAccess>(
        &mut self,
        store: &Arc<ForegroundStore>,
        grant: &OrdinaryFdObservation<'_>,
        metadata: &MemoryMetadata,
        runtime: &NetworkRuntimeResources,
        memory: &mut M,
    ) -> Result<(StoreOutcome, Option<FullStoreCompletion>), NetworkReplayError> {
        self.check_foreground_access(store, grant, metadata, runtime)?;
        let mut phase = store.phase.lock().unwrap();
        if !matches!(*phase, Phase::Prepared) {
            return Err(invalid("foreground store is one use"));
        }
        // The consumed local attempt cannot retry under another access result.
        // This phase precedes all possible writes and remains on refusal/panic.
        *phase = Phase::CheckingAccess;
        // This arena is the actual unmodified private-anonymous mmap, hence
        // protection key zero. The stopped backend must also qualify the
        // target's current key-zero rights; cross-process VMA checks do not.
        let tid = store.root.association().process();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            memory.validate_native_user_key0_write_access(tid)
        }))
        .map_err(|_| invalid("foreground native access checker panicked before possible stores"))?
        .map_err(|error| invalid(&format!("foreground native access refused: {error}")))?;
        let address = AddrMut::<u8>::from_raw(store.span.address() as usize)
            .ok_or_else(|| invalid("foreground copy lost its nonnull bounded destination"))?;
        *phase = Phase::Possible;
        let bytes = &store.source.bytes()[store.source_offset..store.source_offset + store.length];
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let local = [std::io::IoSlice::new(bytes)];
            // Preserve page-level partial results in the single native call.
            // The complete selection is <=512 bytes, spanning at most two pages.
            // Remote destinations remain numeric, never references in this process.
            const PAGE_SIZE: usize = 4096;
            let first_len = store.length.min(PAGE_SIZE - address.as_raw() % PAGE_SIZE);
            let first = RemoteIoVec::new(address, first_len).map_err(|error| error.into_raw())?;
            let result = if first_len < store.length {
                let tail = address
                    .as_raw()
                    .checked_add(first_len)
                    .and_then(AddrMut::from_raw)
                    .ok_or(libc::EFAULT)?;
                let second = RemoteIoVec::new(tail, store.length - first_len)
                    .map_err(|error| error.into_raw())?;
                memory.write_native_user_vectored(tid, &local, &[first, second])
            } else {
                memory.write_native_user_vectored(tid, &local, &[first])
            };
            result.map_err(|error| error.into_raw())
        })) {
            Ok(raw) => StoreOutcome::Returned(raw),
            Err(_) => StoreOutcome::Panicked,
        };
        *phase = Phase::Observed {
            _outcome: outcome.clone(),
        };
        // These checks can fail after real writes. Observed is deliberately
        // retained, never reset to Prepared or converted to zero effects.
        self.check_foreground_access(store, grant, metadata, runtime)?;
        if outcome == StoreOutcome::Returned(Ok(store.length)) {
            *phase = Phase::Checked(outcome.clone());
        }
        drop(phase);
        let full = if outcome == StoreOutcome::Returned(Ok(store.length)) {
            Some(self.finish_foreground_store(store, runtime)?)
        } else {
            None
        };
        Ok((outcome, full))
    }

    /// No bool or serialized count issues this handoff. This calls the actual
    /// runtime end operation only after the exact full outcome is retained.
    fn finish_foreground_store(
        &mut self,
        store: &Arc<ForegroundStore>,
        runtime: &NetworkRuntimeResources,
    ) -> Result<FullStoreCompletion, NetworkReplayError> {
        self.check_foreground_store(store)?;
        let mut phase = store.phase.lock().unwrap();
        if !matches!(&*phase,Phase::Checked(StoreOutcome::Returned(Ok(n))) if *n==store.length) {
            return Err(invalid(
                "foreground store lacks an exact full successful raw outcome",
            ));
        }
        runtime
            .finish_native_copy_exclusion(&store.exclusion)
            .map_err(|e| invalid(&e.to_string()))?;
        *phase = Phase::ExclusionEnded(StoreOutcome::Returned(Ok(store.length)));
        Ok(FullStoreCompletion {
            store: store.clone(),
        })
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Reuses the existing Call/probe setup with explicit controlled original
    /// file geometry. This supplies no native installation/BPF evidence.
    pub(crate) fn controlled_foreground_store_pending(
        owner: NetworkStreamOwner,
    ) -> (
        Self,
        NetworkStreamCallId,
        NetworkStreamLeaseId,
        NetworkStreamPhysicalEffect,
    ) {
        let (mut engine, call, lease, effect) = Self::controlled_pending_helper_for_owner(owner);
        let file = engine.stream_calls[&call].open_file.unwrap();
        let binding = crate::types::FdSlotBinding {
            slot: crate::types::FdSlot {
                files: crate::types::FilesId::initial(owner.thread),
                fd: 17,
            },
            open_file: file,
            generation: 1,
        };
        engine
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&file)
            .unwrap()
            .native = Some(NativeReceive {
            identity: FileIdentity::controlled_fixture(7, 19),
            binding,
            birth: call,
            physical_observed: Cut::ZERO,
        });
        (engine, call, lease, effect)
    }

    pub(crate) fn foreground_store_semantic_fixture_state(&self) -> String {
        format!("{:?}/{:?}/{:?}", self.channels, self.mode, self.shadow)
    }

    pub(crate) fn change_foreground_store_fixture(
        &mut self,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        variant: usize,
    ) {
        let file = self.stream_calls[&call].open_file.unwrap();
        match variant {
            0 => {
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&file)
                    .unwrap()
                    .native
                    .as_mut()
                    .unwrap()
                    .physical_observed = Cut { bytes: 1, order: 1 }
            }
            1 => self.stream_calls.get_mut(&call).unwrap().native_receive[0].joined = false,
            2 => self.shadow_deliveries.get_mut(&lease).unwrap().selected_len += 1,
            3 => self.stream_calls.get_mut(&call).unwrap().abandoned = true,
            4 => {
                self.channels
                    .get_mut(&self.stream_operations[&lease].channel)
                    .unwrap()
                    .inbound_consumed += 1
            }
            5 => {
                self.shadow_deliveries.get_mut(&lease).unwrap().selected_len = 513;
                let StreamOperationKind::Delivery { selection_len, .. } =
                    &mut self.stream_operations.get_mut(&lease).unwrap().kind
                else {
                    panic!()
                };
                *selection_len = 513;
                self.stream_calls
                    .get_mut(&call)
                    .unwrap()
                    .private_receive
                    .as_mut()
                    .unwrap()
                    .length = 513;
            }
            _ => panic!("unknown explicit changed-authority fixture"),
        }
    }
}
