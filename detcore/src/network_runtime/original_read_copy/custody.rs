//! Canonical raw receipt custody shared by the native Driver and its engine Call.
//! Completed Units are observations, not release/consumption certificates. There
//! is deliberately no receipt discharge API until the actual semantic join exists.
use std::sync::Arc;
use std::sync::Mutex;

use super::*;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;

#[derive(Debug, Default)]
enum Records {
    #[default]
    Empty,
    Raw(Vec<Record>),
    Collected(Arc<Capture>),
}
impl Records {
    fn slice(&self) -> &[Record] {
        match self {
            Self::Empty => &[],
            Self::Raw(records) => records,
            Self::Collected(capture) => &capture.records,
        }
    }
}
#[derive(Debug, Default)]
struct State {
    binding: Option<(NetworkStreamOwner, NetworkStreamCallId, u64)>,
    records: Records,
    prefix: StreamingPrefix,
    prepared_wire: Option<crate::network_runtime::copy_wire_authority::PreparedCopyAuthority>,
    wire_started: bool,
    wire_binding: bool,
    empty_terminal: Option<crate::network_runtime::copy_wire_authority::EmptyCopyTerminalAuthority>,
    terminal_validated: bool,
}
#[derive(Debug, Default)]
pub(crate) struct ReadCopyCustody {
    state: Mutex<State>,
}

/// Private, non-wire capability to a newly completed range in the same raw store.
/// Cloning this metadata does not clone payload or manufacture native completion.
#[derive(Debug, Clone)]
pub(crate) struct CompletedDelta {
    custody: Arc<ReadCopyCustody>,
    first: usize,
    end: usize,
    selection: OriginalSelection,
    native_attempts: Vec<NativeAttempt>,
}

/// An exact completed copy5 observation retained by the same raw owner. It has
/// no constructor from serialized fields and grants no memory/publication effect.
#[derive(Debug, Clone)]
pub(crate) struct NativeAttempt {
    custody: Arc<ReadCopyCustody>,
    binding: (NetworkStreamOwner, NetworkStreamCallId, u64),
    selection: OriginalSelection,
    operation: u64,
    ordinal: usize,
    unit: CompletedUnit,
}
impl NativeAttempt {
    pub(crate) fn belongs_to(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        custody: &Arc<ReadCopyCustody>,
    ) -> bool {
        self.binding == (owner, call, self.selection.command)
            && self.selection.call == call.native_command_call()
            && Arc::ptr_eq(&self.custody, custody)
    }
    pub(crate) fn selection(&self) -> &OriginalSelection {
        &self.selection
    }
    pub(crate) fn operation(&self) -> u64 {
        self.operation
    }
    pub(crate) fn ordinal(&self) -> usize {
        self.ordinal
    }
    pub(crate) fn unit(&self) -> &CompletedUnit {
        &self.unit
    }
    pub(crate) fn same_command(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.custody, &other.custody)
            && self.binding == other.binding
            && self.selection == other.selection
            && self.operation == other.operation
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.same_command(other) && self.ordinal == other.ordinal && self.unit == other.unit
    }
}
impl CompletedDelta {
    pub(crate) fn selection(&self) -> &OriginalSelection {
        &self.selection
    }
    /// Extraction happened under custody; reading these owned handles takes no
    /// lock and cannot invert the engine/custody order.
    pub(crate) fn native_attempts(&self) -> &[NativeAttempt] {
        &self.native_attempts
    }
    /// Borrow the unchanged native DATA plus following UNIT record. This
    /// callback runs under raw custody and must not reenter the engine/Driver.
    pub(crate) fn with_unit<T>(
        &self,
        ordinal: usize,
        visit: impl FnOnce(&CompletedUnit, &[Record]) -> T,
    ) -> io::Result<T> {
        if ordinal < self.first || ordinal >= self.end {
            return Err(invalid(
                "Read completed-unit ordinal is outside this exact delta",
            ));
        }
        let state = self.custody.lock()?;
        let unit = &state.prefix.prefix.units[ordinal];
        let first = unit
            .observation
            .as_ref()
            .map_or(unit.first, |observation| observation.begin_record);
        Ok(visit(unit, &state.records.slice()[first..=unit.end]))
    }
    pub(crate) fn next(&self, custody: &Arc<ReadCopyCustody>, first: usize) -> io::Result<usize> {
        if !Arc::ptr_eq(custody, &self.custody) || self.first != first || self.end <= first {
            return Err(invalid(
                "Read completed range changed its same-Call custody/cursor",
            ));
        }
        Ok(self.end)
    }
}
impl ReadCopyCustody {
    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| invalid("Read raw receipt custody is poisoned"))
    }
    pub(crate) fn len(&self) -> io::Result<usize> {
        Ok(self.lock()?.records.slice().len())
    }

    /// Retain the exact prepare capability on this same Call before native
    /// effects. It has no numeric selected-task or payload authority yet.
    pub(crate) fn retain_preparation(
        &self,
        wire: crate::network_runtime::copy_wire_authority::PreparedCopyAuthority,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        if state.wire_started
            || state.prefix.refused
            || state.prefix.selection.is_some()
            || !state.records.slice().is_empty()
        {
            return Err(invalid(
                "Read wire preparation repeated or followed native receipts",
            ));
        }
        state.wire_started = true;
        state.prepared_wire = Some(wire);
        Ok(())
    }
    pub(crate) fn take_preparation(
        &self,
    ) -> io::Result<crate::network_runtime::copy_wire_authority::PreparedCopyAuthority> {
        let mut state = self.lock()?;
        if !state.wire_started || state.wire_binding || state.prefix.wire.is_some() {
            return Err(invalid(
                "Read wire binding repeated or has no actual preparation",
            ));
        }
        let wire = state
            .prepared_wire
            .take()
            .ok_or_else(|| invalid("Read prepared wire authority missing"))?;
        state.wire_binding = true;
        Ok(wire)
    }
    pub(crate) fn has_wire_authority(&self) -> io::Result<bool> {
        Ok(self.lock()?.prefix.wire.is_some())
    }
    pub(crate) fn has_empty_terminal_authority(&self) -> io::Result<bool> {
        Ok(self.lock()?.empty_terminal.is_some())
    }
    /// Physical diagnostic custody for an actually dead task with no selected
    /// Call row. This is not a grammar or file-selection capability. It stays
    /// pending until the same raw store receives the finite empty terminal cut.
    pub(crate) fn prepare_empty_terminal(
        &self,
        authority: crate::network_runtime::copy_wire_authority::EmptyCopyTerminalAuthority,
        terminal: &OriginalTerminal,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        let result = (|| {
            let (owner, call, command) = state
                .binding
                .ok_or_else(|| invalid("empty terminal custody has no exact engine Call"))?;
            authority.validate_binding(owner, call, command, terminal)?;
            if !state.wire_started
                || !state.wire_binding
                || state.prepared_wire.is_some()
                || state.empty_terminal.is_some()
                || state.prefix.wire.is_some()
                || state.prefix.refused
                || state.prefix.selection.is_some()
                || state.prefix.end.is_some()
                || !state.records.slice().is_empty()
                || !state.prefix.prefix.units.is_empty()
                || state.prefix.prefix.processed != 0
                || state.terminal_validated
            {
                return Err(invalid(
                    "empty terminal authority changed prior raw or selected custody",
                ));
            }
            state.empty_terminal = Some(authority);
            state.wire_binding = false;
            Ok(())
        })();
        if result.is_err() {
            state.prefix.refused = true;
            state.terminal_validated = false;
        }
        result
    }
    /// Retain every raw record first, even though this capability permits no
    /// data interpretation. Only the actual finite ThreadTerminal cut and an
    /// empty unchanged canonical store can become eligible for validation.
    pub(crate) fn append_empty_terminal(
        &self,
        terminal: &OriginalTerminal,
        first: u64,
        records: Vec<Record>,
        end: Option<End>,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        let result = (|| {
            if first != state.records.slice().len() as u64
                || matches!(state.records, Records::Collected(_))
            {
                return Err(invalid(
                    "empty terminal raw append changed exact position or final collection",
                ));
            }
            if matches!(state.records, Records::Empty) {
                state.records = Records::Raw(Vec::new());
            }
            let Records::Raw(retained) = &mut state.records else {
                unreachable!()
            };
            retained.extend(records);
            let (owner, call, command) = state
                .binding
                .ok_or_else(|| invalid("empty terminal raw has no exact engine Call"))?;
            state
                .empty_terminal
                .as_ref()
                .ok_or_else(|| invalid("empty terminal raw has no retained authority"))?
                .validate_binding(owner, call, command, terminal)?;
            if !state.wire_started
                || state.wire_binding
                || state.prepared_wire.is_some()
                || state.prefix.wire.is_some()
                || state.prefix.selection.is_some()
                || state.prefix.refused
                || !state.records.slice().is_empty()
                || !state.prefix.prefix.units.is_empty()
                || state.prefix.prefix.processed != 0
                || end != Some(End::ThreadTerminal)
                || state.prefix.end.is_some_and(|prior| Some(prior) != end)
            {
                return Err(invalid(
                    "unselected terminal has nonempty, malformed or nonterminal copy custody",
                ));
            }
            state.prefix.end = end;
            Ok(())
        })();
        if result.is_err() {
            state.prefix.refused = true;
            state.terminal_validated = false;
        }
        result
    }
    /// Only Controller's consuming retained-selection join can construct this
    /// bound capability. A record/manifest never selects the parser version.
    pub(crate) fn prepare(&self, wire: CopyWireAuthority) -> io::Result<()> {
        let prefix = StreamingPrefix::for_authority(wire)?;
        let mut state = self.lock()?;
        if !state.wire_started
            || !state.wire_binding
            || state.prefix.wire.is_some()
            || state.empty_terminal.is_some()
            || state.prefix.refused
            || state.prefix.selection.is_some()
            || !state.records.slice().is_empty()
        {
            return Err(invalid(
                "Read wire authority changed after binding or native receipt",
            ));
        }
        state.prefix = prefix;
        state.wire_binding = false;
        Ok(())
    }

    pub(crate) fn bind(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        command: u64,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        let binding = (owner, call, command);
        if command == 0
            || state.binding.is_some_and(|prior| prior != binding)
            || state.binding.is_none()
                && (!state.records.slice().is_empty() || state.prefix.selection.is_some())
        {
            return Err(invalid("Read custody changed its existing engine Call"));
        }
        state.binding = Some(binding);
        Ok(())
    }

    /// The caller has already checked the prepared request and exact chunk
    /// position. Store every raw byte before the incremental parser sees it.
    pub(crate) fn append(
        &self,
        selection: &OriginalSelection,
        first: u64,
        records: Vec<Record>,
        end: Option<End>,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        if first != state.records.slice().len() as u64
            || matches!(state.records, Records::Collected(_))
        {
            return Err(invalid(
                "Read raw append changed position or final collection",
            ));
        }
        if matches!(state.records, Records::Empty) {
            state.records = Records::Raw(Vec::new());
        }
        let State {
            records: retained,
            prefix,
            wire_started,
            ..
        } = &mut *state;
        let Records::Raw(retained) = retained else {
            unreachable!()
        };
        retained.extend(records);
        if *wire_started && prefix.wire.is_none() {
            prefix.refused = true;
            return Err(invalid(
                "Read raw receipt preceded its actual prepared selection join",
            ));
        }
        prefix.advance(selection, retained, end)
    }

    /// Helper replies enter canonical custody before validating their envelope.
    /// The protocol bounds one frame; a malformed actual frame still stays in
    /// this same store and cannot be replaced with an apparently empty success.
    pub(crate) fn append_helper_chunk(
        &self,
        selection: &OriginalSelection,
        prepared: u64,
        first: u64,
        chunk: Chunk,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        let expected = state.records.slice().len() as u64;
        if matches!(state.records, Records::Collected(_)) {
            state.prefix.refused = true;
            return Err(invalid("helper raw receipt followed final collection"));
        }
        let valid = chunk.prepared == prepared
            && chunk.first == first
            && first == expected
            && chunk.records.len() <= RECORDS_PER_REPLY
            && (!chunk.records.is_empty() || chunk.end.is_some());
        if matches!(state.records, Records::Empty) {
            state.records = Records::Raw(Vec::new());
        }
        let State {
            records,
            prefix,
            wire_started,
            ..
        } = &mut *state;
        let Records::Raw(records) = records else {
            unreachable!()
        };
        records.extend(chunk.records);
        if !valid || !*wire_started || prefix.wire.is_none() {
            prefix.refused = true;
            return Err(invalid(
                "helper copy frame changed exact prepared prefix or authority",
            ));
        }
        prefix.advance(selection, records, chunk.end)
    }

    /// Only the canonical collector can issue a helper completion. This check
    /// is made outside engine/native registry locks, before its opaque handle.
    pub(crate) fn check_collected_capture(&self, capture: &Arc<Capture>) -> io::Result<()> {
        let state = self.lock()?;
        if state.prefix.refused
            || !state.wire_started
            || state.prefix.wire.is_none()
            || !matches!(&state.records,Records::Collected(actual) if Arc::ptr_eq(actual,capture))
        {
            return Err(invalid(
                "helper completion is not the canonical bound capture",
            ));
        }
        Ok(())
    }

    /// Export earlier valid Units even when a subsequent DATA/UNIT was refused.
    /// The parser's processed cursor and immutable range avoid suffix rescans.
    pub(crate) fn completed_since(
        self: &Arc<Self>,
        first: usize,
    ) -> io::Result<Option<CompletedDelta>> {
        let state = self.lock()?;
        let end = state.prefix.prefix.units.len();
        if first > end {
            return Err(invalid("Read completed cursor passed its retained prefix"));
        }
        if first == end {
            return Ok(None);
        }
        let selection = state
            .prefix
            .selection
            .clone()
            .ok_or_else(|| invalid("Read completed range lost its selected Call"))?;
        let mut native_attempts = Vec::new();
        for ordinal in first..end {
            let unit = &state.prefix.prefix.units[ordinal];
            if unit.observation.is_none() {
                continue;
            } // Historical copy4 stays copy4.
            let wire = state
                .prefix
                .wire
                .as_ref()
                .ok_or_else(|| invalid("copy5 attempt has no retained negotiated authority"))?;
            if wire.version() != frontier::VERSION {
                return Err(invalid("copy5 attempt changed its prepared grammar"));
            }
            wire.validate_selection(&selection)?;
            let binding = state
                .binding
                .ok_or_else(|| invalid("copy5 attempt has no exact Call binding"))?;
            if binding.2 != selection.command || binding.1.native_command_call() != selection.call {
                return Err(invalid("copy5 attempt changed same-Call command custody"));
            }
            native_attempts.push(NativeAttempt {
                custody: self.clone(),
                binding,
                selection: selection.clone(),
                operation: wire.operation(),
                ordinal,
                unit: unit.clone(),
            });
        }
        Ok(Some(CompletedDelta {
            custody: self.clone(),
            first,
            end,
            selection,
            native_attempts,
        }))
    }

    pub(crate) fn collect(&self, effect: &OriginalEffect) -> io::Result<Arc<Capture>> {
        let mut state = self.lock()?;
        let result = (|| {
            if state.wire_started && state.prefix.wire.is_none() {
                return Err(invalid(
                    "Read collection preceded its authenticated wire selection",
                ));
            }
            let end = Some(End::OriginalExit {
                protocol: effect.read_copy.is_some_and(|m| m.present == 1),
            });
            if state.prefix.end != end || state.terminal_validated {
                return Err(invalid(
                    "Read final collection lacks the same actual EXIT boundary",
                ));
            }
            let State {
                records, prefix, ..
            } = &mut *state;
            prefix.advance(&effect.original.selection, records.slice(), end)?;
            // Every fallible check and allocation precedes moving the raw store.
            // A failed manifest/positive-prefix join cannot erase its observations.
            let (manifest, units) =
                validate_units_for_version(effect, records.slice(), prefix.wire_version())?;
            if let Records::Collected(capture) = records {
                if capture.manifest != manifest {
                    return Err(invalid(
                        "Read final collection changed its retained manifest",
                    ));
                }
                return Ok(capture.clone());
            }
            let committed = materialize_committed(manifest, &units, records.slice())?;
            let raw = match std::mem::take(records) {
                Records::Empty => Vec::new(),
                Records::Raw(raw) => raw,
                Records::Collected(_) => unreachable!(),
            };
            let capture = Arc::new(Capture {
                manifest,
                committed,
                records: raw,
                units,
            });
            *records = Records::Collected(capture.clone());
            Ok(capture)
        })();
        if result.is_err() {
            state.prefix.refused = true;
        }
        result
    }

    /// Component fixture construction stays within the private provider owner;
    /// it invokes the actual collector without exposing provider/FFI modules.
    #[cfg(test)]
    pub(crate) fn collect_controlled_original_read(
        &self,
        selected: OriginalSelection,
        returned: i32,
        summary: Summary,
    ) -> io::Result<Arc<Capture>> {
        let mut raw = crate::network_runtime::accepted_provider_ffi::OriginalEffect::default();
        raw.command.operation = 11;
        raw.command.command = selected.command;
        raw.command.returned = returned;
        raw.command.phase = 1;
        raw.command.identity.provider = selected.provider;
        raw.command.task = selected.task;
        raw.command.start_boottime = selected.task_start;
        raw.command.original_count = selected.original_count;
        raw.original.returned = returned;
        raw.original.complete = 1;
        let mut effect: OriginalEffect = raw.into();
        effect.original.selection = selected;
        let selected = &effect.original.selection;
        effect.read_copy = Some(Manifest {
            provider: selected.provider,
            command: selected.command,
            call: selected.call,
            task: selected.task,
            task_start: selected.task_start,
            present: 1,
            returned: i64::from(returned),
            summary,
        });
        self.collect(&effect)
    }

    pub(crate) fn validate_terminal(
        &self,
        terminal: &OriginalTerminal,
        end: End,
    ) -> io::Result<()> {
        let mut state = self.lock()?;
        let result = (|| {
            if let Some(authority) = state.empty_terminal.as_ref() {
                let (owner, call, command) = state.binding.ok_or_else(|| {
                    invalid("empty terminal validation lost its exact engine Call")
                })?;
                authority.validate_binding(owner, call, command, terminal)?;
                if end != End::ThreadTerminal
                    || state.prefix.end != Some(end)
                    || state.prefix.refused
                    || !state.records.slice().is_empty()
                    || !state.prefix.prefix.units.is_empty()
                    || state.prefix.prefix.processed != 0
                    || state.prefix.selection.is_some()
                    || state.prefix.wire.is_some()
                    || !state.wire_started
                    || state.wire_binding
                    || state.prepared_wire.is_some()
                {
                    return Err(invalid(
                        "empty terminal validation has no exact finite zero-record cut",
                    ));
                }
                state.terminal_validated = true;
                return Ok(());
            }
            if state.wire_started && state.prefix.wire.is_none() {
                return Err(invalid(
                    "Read terminal validation preceded its authenticated wire selection",
                ));
            }
            if state.prefix.end != Some(end) || matches!(state.records, Records::Collected(_)) {
                return Err(invalid(
                    "Read terminal custody changed its retained boundary",
                ));
            }
            let State {
                records, prefix, ..
            } = &mut *state;
            prefix.advance(&terminal.original.selection, records.slice(), Some(end))?;
            validate_terminal_prefix_for_version(
                terminal,
                records.slice(),
                end,
                prefix.wire_version(),
            )?;
            state.terminal_validated = true;
            Ok(())
        })();
        if result.is_err() {
            state.terminal_validated = false;
            state.prefix.refused = true;
        }
        result
    }

    /// Collection and physical ACK do not discharge semantic observations.
    /// Stage1 intentionally has no way to discard a completed Unit, malformed
    /// suffix or partial DATA. A zero-unit protocol EOF/error is still a
    /// semantic receipt. Only an empty non-protocol or terminal proof can retire.
    pub(crate) fn require_no_unjoined_receipts(&self, received: usize) -> io::Result<()> {
        let state = self.lock()?;
        let empty_terminal = state.terminal_validated
            && matches!(
                state.prefix.end,
                Some(End::ThreadTerminal | End::OriginalExit { protocol: false })
            );
        let non_protocol = matches!(&state.records,
            Records::Collected(capture) if capture.manifest.present==0);
        let empty_diagnostic = state.empty_terminal.is_some()
            && state.terminal_validated
            && state.prefix.end == Some(End::ThreadTerminal)
            && state.prefix.selection.is_none();
        if state.wire_started && state.prefix.wire.is_none() && !empty_diagnostic
            || received != state.prefix.prefix.units.len()
            || received != 0
            || !state.records.slice().is_empty()
            || state.prefix.refused
            || !empty_terminal && !non_protocol
        {
            return Err(invalid("Read Call retains unjoined kernel copy receipts"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    fn effect(
        returned: i32,
        requested: u64,
        final_count: u64,
        records: &[Record],
    ) -> OriginalEffect {
        let mut raw = ffi::OriginalEffect::default();
        raw.command.operation = 11;
        raw.command.command = 7;
        raw.command.returned = returned;
        raw.command.phase = 1;
        raw.command.identity.provider = 3;
        raw.command.task = 13;
        raw.command.start_boottime = 17;
        raw.command.original_count = requested;
        raw.original.returned = returned;
        raw.original.complete = 1;
        raw.original.selection = ffi::OriginalSelection {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            table: 19,
            file: 23,
            ready: 1,
            fdput_flags: 1,
            original_count: requested,
            ..Default::default()
        };
        let mut result: OriginalEffect = raw.into();
        result.read_copy = Some(Manifest {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            present: 1,
            returned: i64::from(returned),
            summary: Summary {
                version: COPY_VERSION,
                initial_count: requested,
                attempts: records.last().map_or(0, |r| r.attempt),
                records: records.len() as u64,
                copied: records
                    .iter()
                    .filter(|r| r.kind == DATA)
                    .map(|r| u64::from(r.length))
                    .sum(),
                final_count,
                protocol_returned: i64::from(returned) as u64,
                protocol_complete: 1,
            },
        });
        result
    }
    fn record(sequence: u64, attempt: u64, offset: u64, bytes: &[u8]) -> Record {
        assert!(!bytes.is_empty() && bytes.len() <= RECORD_BYTES);
        let mut initialized = vec![0; RECORD_BYTES];
        initialized[..bytes.len()].copy_from_slice(bytes);
        Record {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            sequence,
            attempt,
            offset,
            length: bytes.len() as u32,
            kind: 1,
            bytes: initialized,
        }
    }

    fn with_units(records: Vec<Record>, results: &[(u64, u64, i64)]) -> Vec<Record> {
        let mut all = Vec::new();
        for (i, (requested, order, returned)) in results.iter().copied().enumerate() {
            let mut unit_records: Vec<_> = records
                .iter()
                .filter(|r| r.attempt == i as u64 + 1)
                .cloned()
                .collect();
            let offset = unit_records.first().map_or(0, |r| r.offset);
            let copied = unit_records.iter().map(|r| u64::from(r.length)).sum();
            all.append(&mut unit_records);
            let fields = [
                23,
                order,
                offset,
                requested,
                copied,
                returned as u64,
                offset,
                1,
                CONSUME,
            ];
            let mut bytes = vec![0; RECORD_BYTES];
            for (slot, field) in bytes[..72].chunks_exact_mut(8).zip(fields) {
                slot.copy_from_slice(&field.to_le_bytes());
            }
            all.push(Record {
                provider: 3,
                command: 7,
                call: 11,
                task: 13,
                task_start: 17,
                sequence: 0,
                attempt: i as u64 + 1,
                offset,
                length: 72,
                kind: UNIT,
                bytes,
            });
        }
        for (index, r) in all.iter_mut().enumerate() {
            r.sequence = index as u64 + 1;
        }
        all
    }

    #[test]
    fn completed_receipts_are_incremental_before_exit_and_keep_interleaved_orders() {
        let records = with_units(
            vec![record(1, 1, 0, b"abc"), record(2, 2, 3, b"def")],
            &[(3, 1, 0), (3, 4, 0)],
        );
        let effect = effect(6, 6, 0, &records);
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(&effect.original.selection, 0, records[..2].to_vec(), None)
            .unwrap();
        let first = custody.completed_since(0).unwrap().unwrap();
        assert_eq!(first.next(&custody, 0).unwrap(), 1);
        first
            .with_unit(0, |unit, raw| {
                assert_eq!(unit.native.order, 1);
                assert_eq!(raw, &records[..2]);
            })
            .unwrap();
        assert!(first.with_unit(1, |_, _| ()).is_err());
        assert!(custody.completed_since(1).unwrap().is_none());
        custody
            .append(&effect.original.selection, 2, records[2..].to_vec(), None)
            .unwrap();
        let second = custody.completed_since(1).unwrap().unwrap();
        assert_eq!(second.next(&custody, 1).unwrap(), 2);
        second
            .with_unit(1, |unit, raw| {
                assert_eq!(unit.native.order, 4);
                assert_eq!(raw, &records[2..]);
            })
            .unwrap();
        assert!(second.next(&custody, 0).is_err());
        assert!(
            second
                .next(&Arc::new(ReadCopyCustody::default()), 1)
                .is_err()
        );
        assert!(custody.completed_since(3).is_err());
        assert_eq!(custody.lock().unwrap().prefix.end, None);
        assert!(custody.require_no_unjoined_receipts(2).is_err());
    }

    #[test]
    fn malformed_suffix_retains_raw_bytes_and_prior_complete_receipts() {
        let mut records = with_units(
            vec![record(1, 1, 0, b"abc"), record(2, 2, 3, b"def")],
            &[(3, 1, 0), (3, 4, 0)],
        );
        let effect = effect(6, 6, 0, &records);
        records[3].provider += 1;
        let custody = Arc::new(ReadCopyCustody::default());
        assert!(
            custody
                .append(&effect.original.selection, 0, records.clone(), None)
                .is_err()
        );
        let first = custody.completed_since(0).unwrap().unwrap();
        assert_eq!(first.next(&custody, 0).unwrap(), 1);
        first
            .with_unit(0, |unit, raw| {
                assert_eq!(unit.native.order, 1);
                assert_eq!(raw, &records[..2]);
            })
            .unwrap();
        {
            let held = custody.lock().unwrap();
            assert_eq!(held.records.slice(), records);
            assert_eq!(held.prefix.prefix.processed, 3);
            assert_eq!(held.prefix.prefix.unit_bytes, 3);
            assert!(held.prefix.refused);
        }
        // A later maintenance call cannot repair/erase the malformed suffix.
        assert!(
            custody
                .append(
                    &effect.original.selection,
                    4,
                    vec![],
                    Some(End::ThreadTerminal)
                )
                .is_err()
        );
        assert_eq!(custody.lock().unwrap().records.slice(), records);
        assert!(custody.require_no_unjoined_receipts(1).is_err());
    }

    #[test]
    fn partial_terminal_data_is_not_completed_or_semantically_retired() {
        let records = vec![record(1, 1, 0, b"visible")];
        let effect = effect(-libc::EFAULT, 32, 32, &records);
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(
                &effect.original.selection,
                0,
                records.clone(),
                Some(End::ThreadTerminal),
            )
            .unwrap();
        assert!(custody.completed_since(0).unwrap().is_none());
        assert_eq!(custody.lock().unwrap().records.slice(), records);
        assert_eq!(custody.lock().unwrap().prefix.prefix.unit_bytes, 7);
        let mut terminal = OriginalTerminal {
            command: effect.command.clone(),
            original: effect.original.clone(),
            call: 11,
            fd_call_present: 1,
            task_absent: 1,
        };
        terminal.command.phase = 0;
        terminal.original.complete = 0;
        custody
            .validate_terminal(&terminal, End::ThreadTerminal)
            .unwrap();
        assert!(custody.lock().unwrap().terminal_validated);
        assert!(custody.require_no_unjoined_receipts(0).is_err());
        assert!(custody.collect(&effect).is_err());
        assert_eq!(custody.lock().unwrap().records.slice(), records);
    }

    #[test]
    fn final_collection_moves_one_raw_store_and_preserves_same_unit_views() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
        let effect = effect(3, 3, 0, &records);
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(
                &effect.original.selection,
                0,
                records.clone(),
                Some(End::OriginalExit { protocol: true }),
            )
            .unwrap();
        let delta = custody.completed_since(0).unwrap().unwrap();
        let allocation = custody.lock().unwrap().records.slice().as_ptr();
        let capture = custody.collect(&effect).unwrap();
        assert_eq!(capture.records.as_ptr(), allocation);
        assert_eq!(capture.records, records);
        assert_eq!(capture.committed, b"abc");
        assert!(Arc::ptr_eq(&capture, &custody.collect(&effect).unwrap()));
        delta
            .with_unit(0, |unit, raw| {
                assert_eq!(unit, &capture.units[0]);
                assert_eq!(raw, capture.records);
            })
            .unwrap();
        assert!(custody.require_no_unjoined_receipts(1).is_err());
        assert!(
            custody
                .append(&effect.original.selection, 2, vec![], None)
                .is_err()
        );
        assert_eq!(custody.lock().unwrap().records.slice(), records);
    }

    #[test]
    fn invalid_final_manifest_never_takes_or_replaces_raw_receipts() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
        let effect = effect(3, 3, 0, &records);
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(
                &effect.original.selection,
                0,
                records.clone(),
                Some(End::OriginalExit { protocol: true }),
            )
            .unwrap();
        let allocation = custody.lock().unwrap().records.slice().as_ptr();
        let mut changed = effect.clone();
        changed.read_copy.as_mut().unwrap().summary.records += 1;
        assert!(custody.collect(&changed).is_err());
        assert_eq!(custody.lock().unwrap().records.slice().as_ptr(), allocation);
        assert_eq!(custody.lock().unwrap().records.slice(), records);
        assert_eq!(
            custody
                .completed_since(0)
                .unwrap()
                .unwrap()
                .next(&custody, 0)
                .unwrap(),
            1
        );
        assert!(custody.collect(&effect).is_err());
        assert!(custody.require_no_unjoined_receipts(1).is_err());
    }

    #[test]
    fn zero_unit_protocol_receipts_remain_unjoined_but_actual_non_protocol_read_can_retire() {
        for returned in [0, -libc::EAGAIN, -libc::EINTR] {
            let effect = effect(returned, 0, 0, &[]);
            let custody = Arc::new(ReadCopyCustody::default());
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            custody
                .append(
                    &effect.original.selection,
                    0,
                    vec![],
                    Some(End::OriginalExit { protocol: true }),
                )
                .unwrap();
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            assert_eq!(
                custody.collect(&effect).unwrap().manifest.returned,
                i64::from(returned)
            );
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            let terminal = OriginalTerminal {
                command: effect.command.clone(),
                original: effect.original.clone(),
                call: 11,
                fd_call_present: 1,
                task_absent: 1,
            };
            let terminal_custody = Arc::new(ReadCopyCustody::default());
            terminal_custody
                .append(
                    &effect.original.selection,
                    0,
                    vec![],
                    Some(End::OriginalExit { protocol: true }),
                )
                .unwrap();
            terminal_custody
                .validate_terminal(&terminal, End::OriginalExit { protocol: true })
                .unwrap();
            assert!(terminal_custody.require_no_unjoined_receipts(0).is_err());
        }
        let mut ordinary = effect(4, 4, 0, &[]);
        let manifest = ordinary.read_copy.as_mut().unwrap();
        manifest.present = 0;
        manifest.summary = Summary::default();
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(
                &ordinary.original.selection,
                0,
                vec![],
                Some(End::OriginalExit { protocol: false }),
            )
            .unwrap();
        assert!(custody.require_no_unjoined_receipts(0).is_err());
        assert_eq!(custody.collect(&ordinary).unwrap().manifest.returned, 4);
        custody.require_no_unjoined_receipts(0).unwrap();
        assert!(custody.require_no_unjoined_receipts(1).is_err());
        let empty = Arc::new(ReadCopyCustody::default());
        empty
            .append(
                &ordinary.original.selection,
                0,
                vec![],
                Some(End::ThreadTerminal),
            )
            .unwrap();
        assert!(empty.require_no_unjoined_receipts(0).is_err());
        let terminal = OriginalTerminal {
            command: ordinary.command.clone(),
            original: ordinary.original.clone(),
            call: 11,
            fd_call_present: 1,
            task_absent: 1,
        };
        empty
            .validate_terminal(&terminal, End::ThreadTerminal)
            .unwrap();
        empty.require_no_unjoined_receipts(0).unwrap();
    }

    #[test]
    fn poisoned_raw_custody_refuses_retirement_without_erasing_observations() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
        let effect = effect(3, 3, 0, &records);
        let custody = Arc::new(ReadCopyCustody::default());
        custody
            .append(&effect.original.selection, 0, records.clone(), None)
            .unwrap();
        assert!(
            std::panic::catch_unwind({
                let custody = custody.clone();
                move || {
                    let _held = custody.state.lock().unwrap();
                    panic!("controlled raw owner poison");
                }
            })
            .is_err()
        );
        assert!(custody.len().is_err());
        assert!(custody.completed_since(0).is_err());
        assert!(custody.require_no_unjoined_receipts(0).is_err());
        let held = custody.state.lock().unwrap_err().into_inner();
        assert_eq!(held.records.slice(), records);
        assert_eq!(held.prefix.prefix.units.len(), 1);
    }
    #[test]
    fn terminal_receipt_refusal_revokes_prior_validation_and_preserves_raw_custody() {
        for with_data in [false, true] {
            for invalid in 0..5 {
                let records = if with_data {
                    vec![record(1, 1, 0, b"raw")]
                } else {
                    vec![]
                };
                let effect = effect(-libc::EFAULT, 8, 8, &records);
                let terminal = OriginalTerminal {
                    command: effect.command.clone(),
                    original: effect.original.clone(),
                    call: 11,
                    fd_call_present: 1,
                    task_absent: 1,
                };
                let custody = Arc::new(ReadCopyCustody::default());
                custody
                    .append(
                        &effect.original.selection,
                        0,
                        records.clone(),
                        Some(End::ThreadTerminal),
                    )
                    .unwrap();
                custody
                    .validate_terminal(&terminal, End::ThreadTerminal)
                    .unwrap();
                assert!(custody.lock().unwrap().terminal_validated);
                assert_eq!(custody.require_no_unjoined_receipts(0).is_ok(), !with_data);
                let mut bad = terminal.clone();
                let mut end = End::ThreadTerminal;
                match invalid {
                    0 => bad.task_absent = 0,
                    1 => bad.command.operation = 10,
                    2 => bad.call = 0,
                    3 => bad.original.selection.command += 1,
                    4 => end = End::OriginalExit { protocol: false },
                    _ => unreachable!(),
                }
                assert!(
                    custody.validate_terminal(&bad, end).is_err(),
                    "mutation {invalid}"
                );
                {
                    let held = custody.lock().unwrap();
                    assert!(!held.terminal_validated);
                    assert!(held.prefix.refused);
                    assert_eq!(held.records.slice(), records);
                }
                assert!(custody.require_no_unjoined_receipts(0).is_err());
                assert!(
                    custody
                        .validate_terminal(&terminal, End::ThreadTerminal)
                        .is_err()
                );
                assert!(custody.require_no_unjoined_receipts(0).is_err());
                assert_eq!(custody.lock().unwrap().records.slice(), records);
            }
        }
    }

    #[test]
    fn invalid_first_terminal_receipt_cannot_be_corrected_into_retirement_authority() {
        for with_data in [false, true] {
            let records = if with_data {
                vec![record(1, 1, 0, b"raw")]
            } else {
                vec![]
            };
            let effect = effect(-libc::EFAULT, 8, 8, &records);
            let terminal = OriginalTerminal {
                command: effect.command.clone(),
                original: effect.original.clone(),
                call: 11,
                fd_call_present: 1,
                task_absent: 1,
            };
            let custody = Arc::new(ReadCopyCustody::default());
            custody
                .append(
                    &effect.original.selection,
                    0,
                    records.clone(),
                    Some(End::ThreadTerminal),
                )
                .unwrap();
            let mut bad = terminal.clone();
            bad.task_absent = 0;
            assert!(
                custody
                    .validate_terminal(&bad, End::ThreadTerminal)
                    .is_err()
            );
            assert!(
                custody
                    .validate_terminal(&terminal, End::ThreadTerminal)
                    .is_err()
            );
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            let held = custody.lock().unwrap();
            assert!(held.prefix.refused);
            assert!(!held.terminal_validated);
            assert_eq!(held.records.slice(), records);
        }
    }

    #[test]
    fn idempotent_collection_refuses_changed_valid_manifest_and_preserves_original_arc() {
        for mutation in 0..3 {
            let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
            let effect = effect(3, 8, 5, &records);
            let custody = Arc::new(ReadCopyCustody::default());
            custody
                .append(
                    &effect.original.selection,
                    0,
                    records.clone(),
                    Some(End::OriginalExit { protocol: true }),
                )
                .unwrap();
            let original = custody.collect(&effect).unwrap();
            let mut changed = effect.clone();
            match mutation {
                0 => {
                    let manifest = changed.read_copy.as_mut().unwrap();
                    manifest.summary.initial_count = 7;
                    manifest.summary.final_count = 4;
                    // Each receipt is internally valid. Repeated collection
                    // must still preserve the first exact native provenance.
                    assert!(validate_records(&changed, &records).is_ok());
                }
                1 | 2 => {
                    changed.command.operation = if mutation == 1 { 21 } else { 22 };
                    // Existing operation/flag authority already refuses these.
                    assert!(validate_records(&changed, &records).is_err());
                }
                _ => unreachable!(),
            }
            assert!(custody.collect(&changed).is_err(), "mutation {mutation}");
            {
                let held = custody.lock().unwrap();
                let Records::Collected(retained) = &held.records else {
                    panic!("lost collected raw owner")
                };
                assert!(Arc::ptr_eq(retained, &original));
                assert_eq!(retained.manifest, effect.read_copy.unwrap());
                assert_eq!(retained.records, records);
                assert_eq!(retained.records.as_ptr(), original.records.as_ptr());
                assert!(held.prefix.refused);
            }
            assert!(custody.collect(&effect).is_err());
            assert!(custody.require_no_unjoined_receipts(1).is_err());
            assert_eq!(original.records, records);
        }
    }
}

#[cfg(test)]
mod empty_terminal_tests {
    use super::*;
    use crate::network_runtime::ProviderWireFormat;
    use crate::network_runtime::copy_wire_authority::controlled_empty_copy_terminal_authority;
    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(13);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn selected() -> OriginalSelection {
        OriginalSelection {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            table: 19,
            file: 23,
            ready: 1,
            requested_fd: 88,
            user_address: 0x1000,
            original_count: 8,
            owner_mm: 0,
            fdput_flags: 0,
            address_length: 0,
        }
    }
    fn call() -> NetworkStreamCallId {
        serde_json::from_value(serde_json::json!(11)).unwrap()
    }
    fn prepare(phase: u64) -> (Arc<ReadCopyCustody>, OriginalTerminal) {
        let custody = Arc::new(ReadCopyCustody::default());
        custody.bind(owner(), call(), 7).unwrap();
        let (authority, terminal) = controlled_empty_copy_terminal_authority(
            ProviderWireFormat::Abi8Copy5,
            owner(),
            selected(),
            phase,
            |prepared| {
                custody.retain_preparation(prepared)?;
                custody.take_preparation()
            },
        )
        .unwrap();
        custody
            .prepare_empty_terminal(authority, &terminal)
            .unwrap();
        (custody, terminal)
    }
    fn raw(kind: u32) -> Record {
        Record {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            sequence: 1,
            attempt: 1,
            offset: 0,
            length: 1,
            kind,
            bytes: vec![0; RECORD_BYTES],
        }
    }
    #[test]
    fn actual_unselected_terminal_requires_finite_empty_cut_without_issuing_result_or_delta() {
        for phase in [1, 2, 3] {
            let (custody, terminal) = prepare(phase);
            assert_eq!(terminal.fd_call_present, 0);
            assert_eq!(terminal.task_absent, 1);
            assert!(custody.completed_since(0).unwrap().is_none());
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            custody
                .append_empty_terminal(&terminal, 0, vec![], Some(End::ThreadTerminal))
                .unwrap();
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            custody
                .validate_terminal(&terminal, End::ThreadTerminal)
                .unwrap();
            custody.require_no_unjoined_receipts(0).unwrap();
            assert!(custody.require_no_unjoined_receipts(1).is_err());
            assert!(custody.completed_since(0).unwrap().is_none());
            let state = custody.lock().unwrap();
            assert!(state.empty_terminal.is_some() && state.prefix.wire.is_none());
            assert!(state.prefix.selection.is_none() && state.prefix.prefix.units.is_empty());
            assert!(state.records.slice().is_empty());
            assert!(!matches!(state.records, Records::Collected(_))); // no Capture or semantic result
        }
    }
    #[test]
    fn unselected_terminal_nonempty_raw_is_retained_and_cannot_be_replaced_by_empty() {
        for kind in [DATA, UNIT, frontier::BEGIN, frontier::FINISH, 99] {
            let (custody, terminal) = prepare(3);
            let record = raw(kind);
            assert!(
                custody
                    .append_empty_terminal(
                        &terminal,
                        0,
                        vec![record.clone()],
                        Some(End::ThreadTerminal)
                    )
                    .is_err()
            );
            assert_eq!(custody.lock().unwrap().records.slice(), [record]);
            assert!(custody.completed_since(0).unwrap().is_none());
            assert!(
                custody
                    .append_empty_terminal(&terminal, 1, vec![], Some(End::ThreadTerminal))
                    .is_err()
            );
            assert!(
                custody
                    .validate_terminal(&terminal, End::ThreadTerminal)
                    .is_err()
            );
            assert!(custody.require_no_unjoined_receipts(0).is_err());
            assert_eq!(custody.len().unwrap(), 1);
            assert!(custody.has_empty_terminal_authority().unwrap());
        }
    }
    #[test]
    fn unselected_terminal_changed_receipt_refusal_is_sticky_before_or_after_validation() {
        for already_valid in [false, true] {
            for mutation in 0..9 {
                let (custody, terminal) = prepare(2);
                custody
                    .append_empty_terminal(&terminal, 0, vec![], Some(End::ThreadTerminal))
                    .unwrap();
                if already_valid {
                    custody
                        .validate_terminal(&terminal, End::ThreadTerminal)
                        .unwrap();
                }
                let mut changed = terminal.clone();
                match mutation {
                    0 => changed.call += 1,
                    1 => changed.command.command += 1,
                    2 => changed.command.operation = 21,
                    3 => changed.command.original_count += 1,
                    4 => changed.command.phase = 4,
                    5 => changed.task_absent = 0,
                    6 => changed.fd_call_present = 1,
                    7 => changed.original.selection.ready = 1,
                    8 => changed.original.address[0] = 1,
                    _ => unreachable!(),
                }
                assert!(
                    custody
                        .validate_terminal(&changed, End::ThreadTerminal)
                        .is_err()
                );
                assert!(
                    custody
                        .validate_terminal(&terminal, End::ThreadTerminal)
                        .is_err()
                );
                assert!(custody.require_no_unjoined_receipts(0).is_err());
                let state = custody.lock().unwrap();
                assert!(!state.terminal_validated);
                assert!(state.empty_terminal.is_some() && state.records.slice().is_empty());
            }
        }
    }
    #[test]
    fn unselected_terminal_cannot_invent_exit_or_use_missing_authority_after_cancellation() {
        for end in [
            None,
            Some(End::OriginalExit { protocol: false }),
            Some(End::OriginalExit { protocol: true }),
        ] {
            let (custody, terminal) = prepare(2);
            // Guest callback cancellation can drop its own reference; the
            // existing Driver/engine Arc still owns the pending token and cut.
            let pending = custody.clone();
            drop(custody);
            assert!(pending.has_empty_terminal_authority().unwrap());
            assert!(pending.require_no_unjoined_receipts(0).is_err());
            assert!(
                pending
                    .append_empty_terminal(&terminal, 0, vec![], end)
                    .is_err()
            );
            assert!(
                pending
                    .append_empty_terminal(&terminal, 0, vec![], Some(End::ThreadTerminal))
                    .is_err()
            );
            assert!(pending.require_no_unjoined_receipts(0).is_err());
        }
        let (held, terminal) = prepare(2);
        let absent = ReadCopyCustody::default();
        absent.bind(owner(), call(), 7).unwrap();
        assert!(
            absent
                .append_empty_terminal(&terminal, 0, vec![raw(DATA)], Some(End::ThreadTerminal))
                .is_err()
        );
        assert_eq!(absent.len().unwrap(), 1);
        assert!(absent.require_no_unjoined_receipts(0).is_err());
        assert!(held.require_no_unjoined_receipts(0).is_err());
    }
    #[test]
    fn unselected_terminal_capability_cannot_move_to_another_engine_call_or_ordinary_parser() {
        let custody = ReadCopyCustody::default();
        let wrong_call = serde_json::from_value(serde_json::json!(12)).unwrap();
        custody.bind(owner(), wrong_call, 7).unwrap();
        let (authority, terminal) = controlled_empty_copy_terminal_authority(
            ProviderWireFormat::Abi8Copy5,
            owner(),
            selected(),
            2,
            |prepared| {
                custody.retain_preparation(prepared)?;
                custody.take_preparation()
            },
        )
        .unwrap();
        assert!(
            custody
                .prepare_empty_terminal(authority, &terminal)
                .is_err()
        );
        assert!(custody.require_no_unjoined_receipts(0).is_err());
        let (custody, terminal) = prepare(3);
        assert!(
            custody
                .append(
                    &terminal.original.selection,
                    0,
                    vec![],
                    Some(End::ThreadTerminal)
                )
                .is_err()
        );
        assert!(
            custody
                .validate_terminal(&terminal, End::ThreadTerminal)
                .is_err()
        );
        assert!(custody.require_no_unjoined_receipts(0).is_err());
        assert!(custody.completed_since(0).unwrap().is_none());
    }

    // Helper-specific envelope controls use actual issuer/collector APIs. The
    // provider records here are controlled grammar input, not native evidence.
    fn helper_fixture() -> (Arc<ReadCopyCustody>, OriginalEffect, Vec<Record>) {
        let custody = Arc::new(ReadCopyCustody::default());
        let mut selected = selected();
        selected.address_length = 0x40;
        selected.fdput_flags = 0;
        let wire=crate::network_runtime::copy_wire_authority::controlled_copy_authority_with_preparation(
            ProviderWireFormat::Abi7Copy4,owner(),21,selected.clone(),|prepared| {
                custody.bind(owner(),call(),selected.command)?;
                custody.retain_preparation(prepared)?;custody.take_preparation()
            }).unwrap();
        custody.prepare(wire).unwrap();
        let mut native = crate::network_runtime::accepted_provider_ffi::OriginalEffect::default();
        native.command.command = 7;
        native.command.operation = 21;
        native.command.phase = 1;
        native.command.task = 13;
        native.command.start_boottime = 17;
        native.command.identity.provider = 3;
        native.command.returned = 3;
        native.command.original_count = 8;
        native.original.complete = 1;
        native.original.returned = 3;
        let mut effect: OriginalEffect = native.into();
        effect.original.selection = selected;
        let mut data = raw(1);
        data.length = 3;
        data.bytes[..3].copy_from_slice(b"abc");
        let mut unit = raw(3);
        unit.sequence = 2;
        unit.length = 72;
        for (word, value) in unit
            .bytes
            .chunks_exact_mut(8)
            .zip([23u64, 1, 0, 3, 3, 0, 42, 1, 1])
        {
            word.copy_from_slice(&value.to_le_bytes());
        }
        let records = vec![data, unit];
        effect.read_copy = Some(Manifest {
            provider: 3,
            command: 7,
            call: 11,
            task: 13,
            task_start: 17,
            present: 1,
            returned: 3,
            summary: Summary {
                version: 4,
                initial_count: 8,
                attempts: 1,
                records: 2,
                copied: 3,
                final_count: 5,
                protocol_returned: 3,
                protocol_complete: 1,
            },
        });
        (custody, effect, records)
    }
    #[test]
    fn helper_wrong_envelope_retains_all_raw_and_prior_delta_before_sticky_refusal() {
        for variant in 0..5 {
            let (custody, effect, records) = helper_fixture();
            for (first, r) in records.iter().cloned().enumerate() {
                custody
                    .append_helper_chunk(
                        &effect.original.selection,
                        1,
                        first as u64,
                        Chunk {
                            prepared: 1,
                            first: first as u64,
                            records: vec![r],
                            end: None,
                        },
                    )
                    .unwrap();
            }
            let delta = custody.completed_since(0).unwrap().unwrap();
            let mut suffix = records[0].clone();
            suffix.sequence = 3;
            suffix.attempt = 2;
            suffix.offset = 3;
            let mut chunk = Chunk {
                prepared: 1,
                first: 2,
                records: vec![suffix.clone()],
                end: None,
            };
            let mut first = 2;
            match variant {
                0 => chunk.prepared = 2,
                1 => chunk.first = 3,
                2 => first = 3,
                3 => chunk.records.push(suffix.clone()),
                4 => chunk.records[0].provider += 1,
                _ => unreachable!(),
            }
            let actual = chunk.records.clone();
            assert!(
                custody
                    .append_helper_chunk(&effect.original.selection, 1, first, chunk)
                    .is_err()
            );
            let retained = custody.lock().unwrap();
            assert_eq!(&retained.records.slice()[..2], records);
            assert_eq!(&retained.records.slice()[2..], actual);
            assert!(retained.prefix.refused);
            drop(retained);
            delta
                .with_unit(0, |unit, raw| {
                    assert_eq!(unit.native.copied, 3);
                    assert_eq!(raw, records);
                })
                .unwrap();
            assert!(custody.collect(&effect).is_err());
            assert!(custody.require_no_unjoined_receipts(1).is_err());
        }
    }
    #[test]
    fn helper_cancel_before_collection_keeps_same_raw_owner_and_rejects_empty_nonterminal_frame() {
        let (custody, effect, records) = helper_fixture();
        let engine_owner = custody.clone();
        custody
            .append_helper_chunk(
                &effect.original.selection,
                1,
                0,
                Chunk {
                    prepared: 1,
                    first: 0,
                    records: vec![records[0].clone()],
                    end: None,
                },
            )
            .unwrap();
        drop(custody);
        assert_eq!(engine_owner.len().unwrap(), 1);
        assert!(
            engine_owner
                .append_helper_chunk(
                    &effect.original.selection,
                    1,
                    1,
                    Chunk {
                        prepared: 1,
                        first: 1,
                        records: vec![],
                        end: None
                    }
                )
                .is_err()
        );
        assert_eq!(engine_owner.lock().unwrap().records.slice(), &records[..1]);
        assert!(engine_owner.require_no_unjoined_receipts(0).is_err());
    }
}

/// Explicit provider input for engine component controls. The actual private
/// Controller issuer, negotiated parser and canonical store remain in the path.
/// These records are controlled metadata, never a native BPF witness.
#[cfg(test)]
impl ReadCopyCustody {
    pub(crate) fn controlled_append_v5(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        selected: OriginalSelection,
        attempts: &[(u64, u64, u64, u64, Vec<u8>, i64)],
    ) -> io::Result<Vec<Record>> {
        self.bind(owner, call, selected.command)?;
        if !self.lock()?.wire_started {
            let wire=crate::network_runtime::copy_wire_authority::controlled_copy_authority_with_preparation(
                crate::network_runtime::ProviderWireFormat::Abi8Copy5,owner,11,selected.clone(),
                |prepared| {self.retain_preparation(prepared)?;self.take_preparation()})?;
            self.prepare(wire)?;
        }
        let (mut sequence, mut ordinal, mut cursor) = {
            let state = self.lock()?;
            (
                state.records.slice().len() as u64 + 1,
                state.prefix.prefix.units.len() as u64 + 1,
                state.prefix.prefix.cursor,
            )
        };
        let first = sequence - 1;
        let mut records = Vec::new();
        let mut word_record = |ordinal: u64, offset: u64, kind: u32, words: &[u64]| {
            let mut bytes = vec![0; RECORD_BYTES];
            for (slot, value) in bytes.chunks_exact_mut(8).zip(words) {
                slot.copy_from_slice(&value.to_le_bytes());
            }
            let record = Record {
                provider: selected.provider,
                command: selected.command,
                call: selected.call,
                task: selected.task,
                task_start: selected.task_start,
                sequence,
                attempt: ordinal,
                offset,
                length: (words.len() * 8) as u32,
                kind,
                bytes,
            };
            sequence += 1;
            record
        };
        for (before, order, requested, available, payload, returned) in attempts {
            records.push(word_record(
                ordinal,
                cursor,
                frontier::BEGIN,
                &[
                    selected.file,
                    *before,
                    *before,
                    *order,
                    cursor,
                    *requested,
                    *available,
                    0,
                    *available,
                    0,
                    42,
                    1,
                    CONSUME,
                ],
            ));
            for (index, chunk) in payload.chunks(RECORD_BYTES).enumerate() {
                let mut record =
                    word_record(ordinal, cursor + (index * RECORD_BYTES) as u64, DATA, &[]);
                record.length = chunk.len() as u32;
                record.bytes[..chunk.len()].copy_from_slice(chunk);
                records.push(record);
            }
            let consumes = *returned == 0;
            records.push(word_record(
                ordinal,
                cursor,
                frontier::FINISH,
                &[
                    selected.file,
                    *order + u64::from(consumes),
                    cursor,
                    *requested,
                    payload.len() as u64,
                    *returned as u64,
                    42,
                    1,
                    CONSUME,
                    *before,
                    *before + if consumes { payload.len() as u64 } else { 0 },
                ],
            ));
            if consumes {
                cursor += payload.len() as u64;
            }
            ordinal += 1;
        }
        self.append(&selected, first, records.clone(), None)?;
        Ok(records)
    }
}
