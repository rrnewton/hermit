//! Same-Call kernel copy units and their independent original Read EXIT.
//! A failed helper can leave guest-visible bytes and then revert its iterator.
//! A successful Consume advances native consumption order. Version4 Observe
//! units and failures retain that frontier without consuming bytes; neither fragments
//! alone nor whole-Call completion order establish stream release.
use std::io;

use serde::Deserialize;
use serde::Serialize;

use super::accepted_provider::OriginalEffect;
use super::accepted_provider::OriginalSelection;
use super::accepted_provider::OriginalTerminal;

mod custody;
mod frontier;
pub(crate) use custody::CompletedDelta;
pub(crate) use custody::NativeAttempt;
pub(crate) use custody::ReadCopyCustody;

use super::copy_wire_authority::CopyWireAuthority;

pub(crate) const MAX_READ: u64 = 0x7fff_f000;
pub(crate) const RECORD_BYTES: usize = 512;
pub(crate) const RECORDS_PER_REPLY: usize = 1;
const DATA: u32 = 1;
const UNIT: u32 = 3;
const COPY_VERSION: u64 = 4;
pub(crate) const CONSUME: u64 = 1;
pub(crate) const OBSERVE: u64 = 2;

/// Expected disposition comes from the authenticated command, never payload.
#[derive(Clone, Copy, Debug)]
struct CopyAuthority {
    operation: u64,
    flags: i32,
    disposition: u64,
}
impl Default for CopyAuthority {
    fn default() -> Self {
        Self {
            operation: 11,
            flags: 0,
            disposition: CONSUME,
        }
    }
}
impl CopyAuthority {
    fn from_shape(operation: u64, flags: i32) -> io::Result<Self> {
        let disposition = match (operation, flags) {
            (11, 0) | (21, 0x40) => CONSUME,
            (22, 0x42) => OBSERVE,
            _ => return Err(invalid("receive copy operation/flags mismatch")),
        };
        Ok(Self {
            operation,
            flags,
            disposition,
        })
    }
    fn selection(self, selected: &OriginalSelection) -> io::Result<()> {
        if selected.address_length != self.flags
            || selected.original_count > MAX_READ && self.operation != 11
            || self.operation != 11
                && (selected.fdput_flags != 0 || selected.ready != 1 || selected.file == 0)
        {
            return Err(invalid(
                "receive copy selection changed its authenticated issuer",
            ));
        }
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Unit {
    pub file: u64,
    pub order: u64,
    pub offset: u64,
    pub requested: u64,
    pub copied: u64,
    pub returned: i64,
    pub position: u64,
    pub transport: u64,
    pub disposition: u64,
}
impl Unit {
    fn from_record(record: &Record) -> io::Result<Self> {
        if record.kind != UNIT || record.length != 72 || record.bytes.len() != RECORD_BYTES {
            return Err(invalid("Read unit wire shape"));
        }
        let mut fields = [0u64; 9];
        for (field, bytes) in fields.iter_mut().zip(record.bytes[..72].chunks_exact(8)) {
            *field = u64::from_le_bytes(bytes.try_into().unwrap());
        }
        Ok(Self {
            file: fields[0],
            order: fields[1],
            offset: fields[2],
            requested: fields[3],
            copied: fields[4],
            returned: fields[5] as i64,
            position: fields[6],
            transport: fields[7],
            disposition: fields[8],
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletedUnit {
    pub native: Unit,
    /// Half-open range of data records. The following record is the exact
    /// native helper result, not another data fragment or syscall return.
    pub first: usize,
    pub end: usize,
    /// Present only after an explicitly negotiated copy5 Begin/End pair.
    /// These current geometry/byte facts are not a semantic release authority.
    pub observation: Option<frontier::AttemptObservation>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Progress {
    pub records: u64,
    /// Authored only by the actual original syscall EXIT ring record.
    pub exited: u64,
    /// Positive retained PIDFD_THREAD death and the finite ring drain cut.
    pub terminal: u64,
    /// Socket protocol presence from the actual EXIT summary, not record absence.
    pub protocol: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub version: u64,
    pub initial_count: u64,
    pub attempts: u64,
    pub records: u64,
    pub copied: u64,
    pub final_count: u64,
    pub protocol_returned: u64,
    pub protocol_complete: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub provider: u64,
    pub command: u64,
    pub call: u64,
    pub task: u64,
    pub task_start: u64,
    pub present: u64,
    pub returned: i64,
    pub summary: Summary,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawRecord {
    pub provider: u64,
    pub command: u64,
    pub call: u64,
    pub task: u64,
    pub task_start: u64,
    pub sequence: u64,
    pub attempt: u64,
    pub offset: u64,
    pub length: u32,
    pub kind: u32,
    pub bytes: [u8; RECORD_BYTES],
}
impl Default for RawRecord {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub provider: u64,
    pub command: u64,
    pub call: u64,
    pub task: u64,
    pub task_start: u64,
    pub sequence: u64,
    pub attempt: u64,
    pub offset: u64,
    pub length: u32,
    pub kind: u32,
    pub bytes: Vec<u8>,
}
impl From<RawRecord> for Record {
    fn from(r: RawRecord) -> Self {
        Self {
            provider: r.provider,
            command: r.command,
            call: r.call,
            task: r.task,
            task_start: r.task_start,
            sequence: r.sequence,
            attempt: r.attempt,
            offset: r.offset,
            length: r.length,
            kind: r.kind,
            bytes: r.bytes.to_vec(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum End {
    OriginalExit { protocol: bool },
    ThreadTerminal,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Chunk {
    pub prepared: u64,
    pub first: u64,
    pub records: Vec<Record>,
    /// Only the last chunk has an end marker. Neither marker is a native
    /// syscall result; final collection or terminal retirement remains required.
    pub end: Option<End>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Capture {
    pub manifest: Manifest,
    /// Native return remains independent of these observations. Only this
    /// actual positive prefix has positional coverage. Stream publication also
    /// requires native consumption ordering and immutable payload provenance.
    /// Observe units describe Peek bytes and do not consume the stream.
    pub committed: Vec<u8>,
    /// Actual copy attempts beyond a committed prefix remain observations,
    /// never a second syscall return or an automatically consumed suffix.
    pub records: Vec<Record>,
    pub units: Vec<CompletedUnit>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
impl Manifest {
    pub(crate) fn validate(self, effect: &OriginalEffect) -> io::Result<()> {
        self.validate_for_version(effect, COPY_VERSION)
    }
    /// `version` comes from the authenticated package/prepare contract, never
    /// from this manifest or a raw frame. Historical validate remains copy4.
    pub(crate) fn validate_for_version(
        self,
        effect: &OriginalEffect,
        version: u64,
    ) -> io::Result<()> {
        if !matches!(version, COPY_VERSION | frontier::VERSION) {
            return Err(invalid("unsupported negotiated native copy version"));
        }
        let selection = &effect.original.selection;
        let authority =
            CopyAuthority::from_shape(effect.command.operation, selection.address_length)?;
        authority.selection(selection)?;
        if effect.command.phase != 1
            || effect.command.identity.provider != selection.provider
            || effect.command.task != selection.task
            || effect.command.start_boottime != selection.task_start
            || effect.command.original_count != selection.original_count
            || selection.ready != 1
            || self.provider != selection.provider
            || self.command != selection.command
            || self.command != effect.command.command
            || self.call != selection.call
            || self.task != selection.task
            || self.task_start != selection.task_start
            || self.provider == 0
            || self.command == 0
            || self.call == 0
            || self.task == 0
            || self.task_start == 0
            || self.returned != i64::from(effect.original.returned)
            || self.returned != i64::from(effect.command.returned)
            || effect.original.complete != 1
            || effect.original.problem != 0
        {
            return Err(invalid(
                "Read copy manifest changed actual selected/completed Call",
            ));
        }
        if self.present == 0 {
            if authority.operation != 11 || self.summary != Summary::default() {
                return Err(invalid("absent Read protocol carries copy state"));
            }
            return Ok(());
        }
        let s = self.summary;
        if self.present != 1
            || selection.file == 0
            || s.version != version
            || s.protocol_complete != 1
            || s.initial_count > MAX_READ
            || s.initial_count > selection.original_count
            || authority.operation != 11 && s.initial_count != selection.original_count
            || s.final_count > s.initial_count
            || s.protocol_returned as i64 != self.returned
            || !(-4095..=MAX_READ as i64).contains(&self.returned)
            || self.returned > 0 && s.initial_count - s.final_count != self.returned as u64
        {
            return Err(invalid(
                "Read copy manifest lacks exact protocol/actual-exit join",
            ));
        }
        Ok(())
    }
}

/// Validate the native unit boundaries before materializing a result buffer.
/// `order` is supplied by the socket-locked producer on the existing file
/// generation. Gaps between this Call's units are legitimate concurrent Calls;
/// the shared ingress consumer must merge them and verify observation coverage.
#[derive(Debug, Default)]
struct Prefix {
    units: Vec<CompletedUnit>,
    copied: u64,
    cursor: u64,
    unit_bytes: u64,
    order: u64,
    transport: u64,
    first: usize,
    processed: usize,
    active_begin: Option<(frontier::Begin, usize)>,
    frontier: Option<(u64, u64)>,
}
impl Prefix {
    fn advance(
        &mut self,
        selection: &OriginalSelection,
        records: &[Record],
        authority: CopyAuthority,
        version: u64,
    ) -> io::Result<()> {
        if !matches!(version, COPY_VERSION | frontier::VERSION) {
            return Err(invalid("unsupported negotiated native copy version"));
        }
        authority.selection(selection)?;
        let maximum = selection.original_count.min(MAX_READ);
        if records.len() < self.processed {
            return Err(invalid("Read copy retained prefix was truncated"));
        }
        if !records.is_empty()
            && (selection.ready != 1
                || selection.file == 0
                || selection.provider == 0
                || selection.command == 0
                || selection.call == 0
                || selection.task == 0
                || selection.task_start == 0)
        {
            return Err(invalid("Read copy prefix lacks actual selected Call"));
        }
        for (index, r) in records.iter().enumerate().skip(self.processed) {
            let length = u64::from(r.length);
            if r.provider != selection.provider
                || r.command != selection.command
                || r.call != selection.call
                || r.task != selection.task
                || r.task_start != selection.task_start
                || r.sequence != index as u64 + 1
                || r.attempt != self.units.len() as u64 + 1
                || length == 0
                || length > RECORD_BYTES as u64
                || r.bytes.len() != RECORD_BYTES
                || r.bytes[r.length as usize..].iter().any(|b| *b != 0)
            {
                return Err(invalid(
                    "Read copy identity, unit order or initialized length mismatch",
                ));
            }
            match r.kind {
                frontier::BEGIN if version == frontier::VERSION => {
                    if self.active_begin.is_some()
                        || self.unit_bytes != 0
                        || r.offset != self.cursor
                    {
                        return Err(invalid("copy5 Begin overlaps an unresolved attempt"));
                    }
                    let begin = frontier::Begin::from_record(r)?;
                    begin.validate(selection.file, maximum, self.cursor, authority)?;
                    if self.transport != 0 && begin.transport != self.transport {
                        return Err(invalid("copy5 Begin changed the retained transport"));
                    }
                    if let Some((bytes, order)) = self.frontier {
                        if begin.before < bytes
                            || begin.order < order
                            || (begin.before == bytes) != (begin.order == order)
                            || authority.disposition == OBSERVE && begin.before != bytes
                        {
                            return Err(invalid(
                                "copy5 Begin regressed or changed the locked Peek frontier",
                            ));
                        }
                    }
                    self.active_begin = Some((begin, index));
                    self.first = index + 1;
                }
                DATA => {
                    if version == frontier::VERSION {
                        let (begin, _) = self.active_begin.as_ref().ok_or_else(|| {
                            invalid("copy5 DATA arrived before an authenticated Begin")
                        })?;
                        if self.unit_bytes > begin.requested
                            || length > begin.requested - self.unit_bytes
                        {
                            return Err(invalid("copy5 DATA exceeds the actual requested copy"));
                        }
                    }
                    if r.offset != self.cursor + self.unit_bytes
                        || r.offset > maximum
                        || length > maximum - r.offset
                    {
                        return Err(invalid(
                            "Read fragment escapes actual unit iterator position",
                        ));
                    }
                    self.copied = self
                        .copied
                        .checked_add(length)
                        .ok_or_else(|| invalid("Read copy count overflow"))?;
                    self.unit_bytes = self
                        .unit_bytes
                        .checked_add(length)
                        .ok_or_else(|| invalid("Read unit count overflow"))?;
                }
                kind if kind == UNIT && version == COPY_VERSION
                    || kind == frontier::FINISH && version == frontier::VERSION =>
                {
                    let (unit, observation) = if version == frontier::VERSION {
                        let (begin, begin_record) = self
                            .active_begin
                            .ok_or_else(|| invalid("copy5 End arrived without its actual Begin"))?;
                        let finish = frontier::Finish::from_record(r)?;
                        begin.validate_end(finish, self.unit_bytes)?;
                        (
                            finish.unit,
                            Some(frontier::AttemptObservation {
                                begin_record,
                                begin,
                                after: finish.after,
                            }),
                        )
                    } else {
                        (Unit::from_record(r)?, None)
                    };
                    if unit.file == 0
                        || unit.file != selection.file
                        || unit.offset != self.cursor
                        || r.offset != self.cursor
                        || self.cursor > maximum
                        || unit.requested == 0
                        || unit.requested > maximum - self.cursor
                        || unit.copied != self.unit_bytes
                        || unit.copied > unit.requested
                        || !matches!(unit.transport, 1 | 2)
                        || self.transport != 0 && unit.transport != self.transport
                        || unit.position > u32::MAX as u64
                        || !matches!(unit.returned, 0 | -14)
                        || unit.disposition != authority.disposition
                        || unit.returned == 0 && unit.copied != unit.requested
                        || unit.returned == 0
                            && unit.disposition == CONSUME
                            && (unit.order == 0 || unit.order <= self.order)
                        || (unit.returned != 0 || unit.disposition == OBSERVE)
                            && unit.order < self.order
                        || unit.returned != 0 && unit.copied == unit.requested
                    {
                        return Err(invalid(
                            "Read helper result, selected file or consumption order mismatch",
                        ));
                    }
                    self.order = unit.order;
                    if unit.returned == 0 {
                        self.cursor += unit.copied;
                    }
                    self.transport = unit.transport;
                    if let Some(observation) = observation {
                        self.frontier = Some((observation.after, unit.order));
                    }
                    self.units.push(CompletedUnit {
                        native: unit,
                        first: self.first,
                        end: index,
                        observation,
                    });
                    self.active_begin = None;
                    self.first = index + 1;
                    self.unit_bytes = 0;
                }
                _ => return Err(invalid("unexpected Read copy record kind")),
            }
            self.processed = index + 1;
        }
        Ok(())
    }
    fn require_closed(&self, length: usize) -> io::Result<()> {
        if self.first != length || self.unit_bytes != 0 || self.active_begin.is_some() {
            return Err(invalid("Read prefix lacks its final native unit result"));
        }
        Ok(())
    }
}
fn validate_prefix(
    selection: &OriginalSelection,
    records: &[Record],
    allow_partial: bool,
    authority: CopyAuthority,
    version: u64,
) -> io::Result<Prefix> {
    let mut prefix = Prefix::default();
    prefix.advance(selection, records, authority, version)?;
    if !allow_partial {
        prefix.require_closed(records.len())?;
    }
    Ok(prefix)
}

/// Incremental validation retained by the existing original Call. Raw records
/// are owned separately by that same Call and stay immutable until collection.
/// A refusal is sticky; neither a corrected response nor later EXIT erases it.
/// This proves no stream release, return, consumption frontier or EOF.
#[derive(Debug, Default)]
pub(crate) struct StreamingPrefix {
    authority: CopyAuthority,
    wire: Option<CopyWireAuthority>,
    selection: Option<OriginalSelection>,
    prefix: Prefix,
    end: Option<End>,
    refused: bool,
}
impl StreamingPrefix {
    pub(crate) fn for_authority(wire: CopyWireAuthority) -> io::Result<Self> {
        if !matches!(wire.version(), COPY_VERSION | frontier::VERSION) {
            return Err(invalid("unsupported prepared native copy version"));
        }
        let authority = CopyAuthority::from_shape(wire.operation(), wire.flags())?;
        Ok(Self {
            authority,
            wire: Some(wire),
            ..Default::default()
        })
    }
    pub(super) fn wire_version(&self) -> u64 {
        self.wire
            .as_ref()
            .map_or(COPY_VERSION, CopyWireAuthority::version)
    }
    pub(crate) fn collect(
        &mut self,
        effect: &OriginalEffect,
        records: Vec<Record>,
    ) -> io::Result<Capture> {
        let result = (|| {
            let end = Some(End::OriginalExit {
                protocol: effect.read_copy.is_some_and(|m| m.present == 1),
            });
            if self.end != end {
                return Err(invalid("copy collection changed retained actual EXIT"));
            }
            self.advance(&effect.original.selection, &records, end)?;
            let (manifest, units) =
                validate_units_for_version(effect, &records, self.wire_version())?;
            let committed = materialize_committed(manifest, &units, &records)?;
            Ok(Capture {
                manifest,
                committed,
                records,
                units,
            })
        })();
        if result.is_err() {
            self.refused = true;
        }
        result
    }
    pub(crate) fn for_helper(operation: u64, flags: i32) -> io::Result<Self> {
        if !matches!(operation, 21 | 22) {
            return Err(invalid("helper receive operation required"));
        }
        Ok(Self {
            authority: CopyAuthority::from_shape(operation, flags)?,
            ..Default::default()
        })
    }
    pub(crate) fn advance(
        &mut self,
        selection: &OriginalSelection,
        records: &[Record],
        end: Option<End>,
    ) -> io::Result<()> {
        if self.refused {
            return Err(invalid("Read copy prefix previously refused"));
        }
        let result = (|| {
            if let Some(wire) = &self.wire {
                wire.validate_selection(selection)?;
            }
            if self
                .selection
                .as_ref()
                .is_some_and(|previous| previous != selection)
                || self.end.is_some() && (self.end != end || records.len() != self.prefix.processed)
            {
                return Err(invalid(
                    "Read copy prefix changed selection or terminal boundary",
                ));
            }
            if self.selection.is_none() {
                self.selection = Some(selection.clone());
            }
            let version = self.wire_version();
            self.prefix
                .advance(selection, records, self.authority, version)?;
            if matches!(end, Some(End::OriginalExit { protocol: false }))
                && (self.authority.operation != 11 || !records.is_empty())
            {
                return Err(invalid("non-protocol Read EXIT carries copy records"));
            }
            if matches!(end, Some(End::OriginalExit { .. })) {
                self.prefix.require_closed(records.len())?;
            }
            self.end = end;
            Ok(())
        })();
        if result.is_err() {
            self.refused = true;
        }
        result
    }
}
fn validate_units(
    effect: &OriginalEffect,
    records: &[Record],
) -> io::Result<(Manifest, Vec<CompletedUnit>)> {
    validate_units_for_version(effect, records, COPY_VERSION)
}
fn validate_units_for_version(
    effect: &OriginalEffect,
    records: &[Record],
    version: u64,
) -> io::Result<(Manifest, Vec<CompletedUnit>)> {
    let manifest = effect
        .read_copy
        .ok_or_else(|| invalid("Read copy manifest missing"))?;
    manifest.validate_for_version(effect, version)?;
    if records.len() as u64 != manifest.summary.records {
        return Err(invalid(
            "Read copy sequence is truncated or contains extra records",
        ));
    }
    let authority = CopyAuthority::from_shape(
        effect.command.operation,
        effect.original.selection.address_length,
    )?;
    let prefix = validate_prefix(
        &effect.original.selection,
        records,
        false,
        authority,
        version,
    )?;
    let expected = if manifest.present == 1 {
        manifest.returned.max(0) as u64
    } else {
        0
    };
    if prefix.units.len() as u64 != manifest.summary.attempts
        || prefix.copied != manifest.summary.copied
        || prefix.cursor != expected
        || manifest.present == 0 && !records.is_empty()
        || prefix.units.iter().any(|u| {
            u.native.offset > manifest.summary.initial_count
                || u.native.requested > manifest.summary.initial_count - u.native.offset
        })
    {
        return Err(invalid(
            "Read EXIT lacks complete unit results or exact positive-prefix coverage",
        ));
    }
    Ok((manifest, prefix.units))
}
pub(crate) fn validate_terminal_prefix(
    terminal: &OriginalTerminal,
    records: &[Record],
    end: End,
) -> io::Result<()> {
    validate_terminal_prefix_for_version(terminal, records, end, COPY_VERSION)
}
pub(crate) fn validate_terminal_prefix_for_version(
    terminal: &OriginalTerminal,
    records: &[Record],
    end: End,
    version: u64,
) -> io::Result<()> {
    if !matches!(terminal.command.operation, 11 | 21 | 22)
        || terminal.task_absent != 1
        || terminal.call == 0
    {
        return Err(invalid(
            "Read prefix lacks actual original-task terminal custody",
        ));
    }
    if !records.is_empty() {
        let selected = &terminal.original.selection;
        if terminal.fd_call_present != 1
            || selected.command != terminal.command.command
            || selected.call != terminal.call
            || selected.provider != terminal.command.identity.provider
            || selected.task != terminal.command.task
            || selected.task_start != terminal.command.start_boottime
            || selected.original_count != terminal.command.original_count
        {
            return Err(invalid(
                "terminal Read prefix changed its original selection",
            ));
        }
    }
    let authority = CopyAuthority::from_shape(
        terminal.command.operation,
        terminal.original.selection.address_length,
    )?;
    let prefix = validate_prefix(
        &terminal.original.selection,
        records,
        end == End::ThreadTerminal,
        authority,
        version,
    )?;
    if let End::OriginalExit { protocol } = end {
        if terminal.command.phase != 1
            || terminal.original.complete != 1
            || terminal.original.returned != terminal.command.returned
            || protocol && prefix.cursor != i64::from(terminal.original.returned).max(0) as u64
            || !protocol && (authority.operation != 11 || !records.is_empty())
        {
            return Err(invalid(
                "terminal Read lost the previously observed actual EXIT",
            ));
        }
    }
    Ok(())
}
pub(crate) fn validate_records(
    effect: &OriginalEffect,
    records: &[Record],
) -> io::Result<Manifest> {
    validate_units(effect, records).map(|(manifest, _)| manifest)
}
pub(crate) fn validate_records_for_version(
    effect: &OriginalEffect,
    records: &[Record],
    version: u64,
) -> io::Result<Manifest> {
    validate_units_for_version(effect, records, version).map(|(manifest, _)| manifest)
}

pub(crate) fn decode(effect: &OriginalEffect, records: Vec<Record>) -> io::Result<Capture> {
    let (manifest, units) = validate_units(effect, &records)?;
    let committed = materialize_committed(manifest, &units, &records)?;
    Ok(Capture {
        manifest,
        committed,
        records,
        units,
    })
}
fn materialize_committed(
    manifest: Manifest,
    units: &[CompletedUnit],
    records: &[Record],
) -> io::Result<Vec<u8>> {
    let length = if manifest.present == 1 {
        usize::try_from(manifest.returned.max(0)).map_err(|_| invalid("Read return range"))?
    } else {
        0
    };
    // The allocation follows the authenticated Linux return, never request len.
    let mut committed = vec![0; length];
    for unit in units {
        if unit.native.returned != 0 {
            continue;
        }
        for r in &records[unit.first..unit.end] {
            let start =
                usize::try_from(r.offset).map_err(|_| invalid("Read copy position range"))?;
            let end = start + r.length as usize;
            committed[start..end].copy_from_slice(&r.bytes[..r.length as usize]);
        }
    }
    Ok(committed)
}

impl Capture {
    /// Returns the position-covered source observations for this actual result.
    /// A retained kernel reference proves lifetime, not immutable bytes; callers
    /// must establish that provenance and stream order before publication.
    pub(crate) fn observed_prefix(&self, returned: i64) -> io::Result<&[u8]> {
        if returned <= 0
            || self.manifest.returned != returned
            || self.manifest.present != 1
            || self.committed.len() as u64 != returned as u64
        {
            return Err(invalid(
                "positive network Read lacks position-covered source observations",
            ));
        }
        Ok(&self.committed)
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
    fn streaming_prefix_accepts_unit_boundaries_without_inventing_exit() {
        let records = with_units(
            vec![record(1, 1, 0, b"abc"), record(2, 2, 3, b"def")],
            &[(3, 1, 0), (3, 4, 0)],
        );
        let observed = effect(6, 6, 0, &records);
        let mut stream = StreamingPrefix::default();
        for end in 0..=records.len() {
            let prefix = &records[..end];
            stream
                .advance(&observed.original.selection, prefix, None)
                .unwrap();
            assert_eq!(stream.prefix.processed, end);
            // Idempotent maintenance does not parse already accepted records.
            stream
                .advance(&observed.original.selection, prefix, None)
                .unwrap();
            assert_eq!(stream.prefix.processed, end);
            let mut terminal = StreamingPrefix::default();
            terminal
                .advance(
                    &observed.original.selection,
                    prefix,
                    Some(End::ThreadTerminal),
                )
                .unwrap();
            let mut exited = StreamingPrefix::default();
            let closed = end == 0 || records[end - 1].kind == UNIT;
            assert_eq!(
                exited
                    .advance(
                        &observed.original.selection,
                        prefix,
                        Some(End::OriginalExit { protocol: true })
                    )
                    .is_ok(),
                closed,
                "prefix {end}"
            );
        }
        stream
            .advance(
                &observed.original.selection,
                &records,
                Some(End::OriginalExit { protocol: true }),
            )
            .unwrap();
        assert_eq!(stream.prefix.units.len(), 2);
        assert_eq!(stream.prefix.cursor, 6);
        assert_eq!(stream.prefix.units[1].native.order, 4);
        // Final byte-count and EXIT joins remain a separate obligation.
        assert!(decode(&observed, records[..2].to_vec()).is_err());
        assert_eq!(decode(&observed, records).unwrap().committed, b"abcdef");
    }

    #[test]
    fn streaming_prefix_refuses_bad_identity_and_payload_before_call_exit() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
        let observed = effect(3, 3, 0, &records);
        for change in 0..16 {
            let mut selected = observed.original.selection.clone();
            let mut r = records.clone();
            match change {
                0 => r[0].provider += 1,
                1 => r[0].command += 1,
                2 => r[0].call += 1,
                3 => r[0].task += 1,
                4 => r[0].task_start += 1,
                5 => r[0].sequence += 1,
                6 => r[0].attempt += 1,
                7 => r[0].offset += 1,
                8 => r[0].length = 0,
                9 => r[0].length = 513,
                10 => {
                    r[0].bytes.pop();
                }
                11 => r[0].bytes[3] = 1,
                12 => r[0].kind = 2,
                13 => selected.ready = 0,
                14 => selected.file = 0,
                15 => selected.original_count = 2,
                _ => unreachable!(),
            }
            let mut stream = StreamingPrefix::default();
            // No UNIT, summary or EXIT has arrived to reveal this mismatch on
            // an eventual result; retain refusal even if a later record fits.
            assert!(
                stream.advance(&selected, &r[..1], None).is_err(),
                "change {change}"
            );
            assert!(
                stream
                    .advance(&observed.original.selection, &records, None)
                    .is_err()
            );
        }
    }

    #[test]
    fn streaming_prefix_binds_selection_and_terminal_boundary() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 1, 0)]);
        let observed = effect(3, 3, 0, &records);
        for change in 0..5 {
            let mut stream = StreamingPrefix::default();
            stream
                .advance(&observed.original.selection, &records, None)
                .unwrap();
            let mut selected = observed.original.selection.clone();
            match change {
                0 => selected.table += 1,
                1 => selected.file += 1,
                2 => selected.owner_mm += 1,
                3 => selected.original_count += 1,
                4 => selected.requested_fd += 1,
                _ => unreachable!(),
            }
            assert!(stream.advance(&selected, &records, None).is_err());
        }
        for end in [End::ThreadTerminal, End::OriginalExit { protocol: true }] {
            let mut stream = StreamingPrefix::default();
            stream
                .advance(&observed.original.selection, &records, Some(end))
                .unwrap();
            stream
                .advance(&observed.original.selection, &records, Some(end))
                .unwrap();
            assert!(
                stream
                    .advance(&observed.original.selection, &records, None)
                    .is_err()
            );
        }
        let mut stream = StreamingPrefix::default();
        stream
            .advance(&observed.original.selection, &records, None)
            .unwrap();
        assert!(
            stream
                .advance(&observed.original.selection, &records[..1], None)
                .is_err()
        );
        let mut stream = StreamingPrefix::default();
        assert!(
            stream
                .advance(
                    &observed.original.selection,
                    &records,
                    Some(End::OriginalExit { protocol: false })
                )
                .is_err()
        );
        let mut ordinary = StreamingPrefix::default();
        ordinary
            .advance(
                &observed.original.selection,
                &[],
                Some(End::OriginalExit { protocol: false }),
            )
            .unwrap();
    }

    #[test]
    fn read_copy_wire_layout_matches_the_maintained_c_abi() {
        assert_eq!(std::mem::size_of::<RawRecord>(), 584);
        assert_eq!(std::mem::size_of::<Unit>(), 72);
        assert_eq!(std::mem::offset_of!(Unit, disposition), 64);
        assert_eq!(std::mem::offset_of!(RawRecord, bytes), 72);
        assert_eq!(std::mem::size_of::<Summary>(), 64);
        assert_eq!(std::mem::size_of::<Manifest>(), 120);
        assert_eq!(std::mem::offset_of!(Manifest, summary), 56);
    }

    #[test]
    fn actual_short_return_selects_65534_bytes_without_promoting_accessible_tail() {
        // Consumer input uses the retained native short-prefix witness's exact
        // lengths; this component test does not run the native producer.
        let returned = 65_534;
        let mut records = Vec::new();
        for start in (0..returned).step_by(RECORD_BYTES) {
            let length = (returned - start).min(RECORD_BYTES);
            records.push(record(
                records.len() as u64 + 1,
                1,
                start as u64,
                &vec![b'Z'; length],
            ));
        }
        let records = with_units(records, &[(returned as u64, 1, 0)]);
        let observed = effect(returned as i32, 69_632, 69_632 - returned as u64, &records);
        let capture = decode(&observed, records).unwrap();
        assert_eq!(
            capture.observed_prefix(returned as i64).unwrap(),
            vec![b'Z'; 65_534]
        );
        assert_eq!(capture.manifest.returned, 65_534);
        assert!(capture.observed_prefix(65_536).is_err());
        assert!(capture.observed_prefix(69_632).is_err());
    }

    #[test]
    fn reverted_retry_overwrites_the_same_iterator_positions_without_appending() {
        let records = with_units(
            vec![
                record(1, 1, 0, b"old!"),
                record(2, 2, 0, b"new"),
                record(3, 3, 3, b"er"),
            ],
            &[(5, 0, -14), (3, 1, 0), (2, 4, 0)],
        );
        let observed = effect(5, 9, 4, &records);
        let capture = decode(&observed, records).unwrap();
        assert_eq!(capture.committed, b"newer");
        assert_eq!(capture.records.iter().filter(|r| r.kind == DATA).count(), 3);
        assert_eq!(capture.units.len(), 3);
        assert_eq!(capture.units[2].native.order, 4);
        assert_eq!(capture.manifest.summary.copied, 9);
        assert_eq!(capture.manifest.returned, 5);
    }

    #[test]
    fn fault_retains_native_error_and_copy_observations_without_consumed_bytes() {
        let records = with_units(vec![record(1, 1, 0, b"visible prefix")], &[(32, 0, -14)]);
        let observed = effect(-libc::EFAULT, 32, 32, &records);
        let capture = decode(&observed, records.clone()).unwrap();
        assert_eq!(capture.manifest.returned, -i64::from(libc::EFAULT));
        assert_eq!(capture.records, records);
        assert!(capture.committed.is_empty());
        assert!(capture.observed_prefix(14).is_err());
        assert!(capture.observed_prefix(-i64::from(libc::EFAULT)).is_err());
    }

    #[test]
    fn failed_unit_keeps_nonzero_frontier_without_consuming_then_retry_advances() {
        let records = with_units(
            vec![
                record(1, 1, 0, b"abc"),
                record(2, 2, 3, b"xy"),
                record(3, 3, 3, b"xyz"),
            ],
            &[(3, 4, 0), (3, 6, -14), (3, 7, 0)],
        );
        let observed = effect(6, 9, 3, &records);
        let capture = decode(&observed, records.clone()).unwrap();
        assert_eq!(capture.committed, b"abcxyz");
        assert_eq!(capture.manifest.summary.copied, 8);
        assert_eq!(capture.units[1].native.order, 6);
        assert_eq!(capture.units[1].native.returned, -14);
        assert_eq!(capture.units[1].native.offset, 3);
        assert_eq!(capture.units[2].native.offset, 3);
        assert_eq!(capture.units[2].native.order, 7);
        for (index, order) in [(3usize, 3u64), (5, 6)] {
            let mut stale = records.clone();
            stale[index].bytes[8..16].copy_from_slice(&order.to_le_bytes());
            assert!(decode(&observed, stale).is_err());
        }
        for version in [2, 3] {
            let mut old = observed.clone();
            old.read_copy.as_mut().unwrap().summary.version = version;
            assert!(decode(&old, records.clone()).is_err());
        }
    }

    #[test]
    fn zero_and_non_socket_results_do_not_mint_positive_network_payload() {
        let observed = effect(0, 0, 0, &[]);
        let capture = decode(&observed, vec![]).unwrap();
        assert_eq!(capture.manifest.returned, 0);
        assert!(capture.committed.is_empty());
        assert!(capture.observed_prefix(0).is_err());
        let mut ordinary = effect(4, 4, 0, &[]);
        let m = ordinary.read_copy.as_mut().unwrap();
        m.present = 0;
        m.summary = Summary::default();
        let capture = decode(&ordinary, vec![]).unwrap();
        assert_eq!(capture.manifest.returned, 4);
        assert!(capture.committed.is_empty());
        assert!(capture.observed_prefix(4).is_err());
    }

    #[test]
    fn changed_identity_gaps_uninitialized_tails_and_commit_mismatch_are_refused() {
        let records = with_units(
            vec![record(1, 1, 0, b"abcd"), record(2, 2, 4, b"efgh")],
            &[(4, 1, 0), (4, 2, 0)],
        );
        let observed = effect(8, 8, 0, &records);
        assert_eq!(
            decode(&observed, records.clone()).unwrap().committed,
            b"abcdefgh"
        );
        for change in 0..13 {
            let mut e = observed.clone();
            let mut r = records.clone();
            match change {
                0 => r[0].provider += 1,
                1 => r[0].command += 1,
                2 => r[0].call += 1,
                3 => r[0].task_start += 1,
                4 => r[1].sequence += 1,
                5 => r[0].kind = 2,
                6 => r[0].bytes[4] = 1,
                7 => r[1].offset = 5,
                8 => r[1].attempt = 0,
                9 => e.read_copy.as_mut().unwrap().returned = 7,
                10 => e.read_copy.as_mut().unwrap().summary.copied += 1,
                11 => e.read_copy.as_mut().unwrap().summary.final_count = 1,
                12 => {
                    r.pop();
                }
                _ => unreachable!(),
            }
            assert!(decode(&e, r).is_err(), "corruption {change}");
        }
    }

    fn helper_effect(
        peek: bool,
        returned: i32,
        requested: u64,
        records: &[Record],
    ) -> OriginalEffect {
        let mut e = effect(
            returned,
            requested,
            requested - returned.max(0) as u64,
            records,
        );
        e.command.operation = if peek { 22 } else { 21 };
        e.original.selection.address_length = if peek { 0x42 } else { 0x40 };
        e.original.selection.fdput_flags = 0;
        e
    }
    #[test]
    fn helper_peek_retains_frontier_while_local_iterator_advances() {
        let mut records = with_units(
            vec![record(1, 1, 0, b"abc"), record(2, 2, 3, b"xyz")],
            &[(3, 0, 0), (3, 0, 0)],
        );
        for r in records.iter_mut().filter(|r| r.kind == UNIT) {
            r.bytes[64..72].copy_from_slice(&OBSERVE.to_le_bytes());
        }
        let observed = helper_effect(true, 6, 9, &records);
        let mut stream = StreamingPrefix::for_helper(22, 0x42).unwrap();
        stream
            .advance(&observed.original.selection, &records, None)
            .unwrap();
        assert_eq!(stream.prefix.cursor, 6);
        assert_eq!(stream.prefix.order, 0);
        let capture = decode(&observed, records.clone()).unwrap();
        assert_eq!(capture.committed, b"abcxyz");
        assert!(
            capture
                .units
                .iter()
                .all(|u| u.native.disposition == OBSERVE && u.native.order == 0)
        );
        // Default Read authority cannot infer Peek permission from its records.
        assert!(
            StreamingPrefix::default()
                .advance(&observed.original.selection, &records, None)
                .is_err()
        );
        for (op, flags) in [
            (11, 0),
            (21, 0x42),
            (22, 0x40),
            (22, 0),
            (22, -1),
            (23, 0x42),
        ] {
            assert!(StreamingPrefix::for_helper(op, flags).is_err());
        }
        for bad in 0..7 {
            let mut e = observed.clone();
            let mut r = records.clone();
            match bad {
                0 => r[1].bytes[64..72].copy_from_slice(&CONSUME.to_le_bytes()),
                1 => r[1].bytes[64..72].copy_from_slice(&0u64.to_le_bytes()),
                2 => r[1].bytes[64..72].copy_from_slice(&3u64.to_le_bytes()),
                3 => r[1].length = 64,
                4 => e.original.selection.fdput_flags = 1,
                5 => e.original.selection.address_length = 0x40,
                6 => e.read_copy.as_mut().unwrap().summary.version = 3,
                _ => unreachable!(),
            }
            assert!(decode(&e, r).is_err(), "helper mutation {bad}");
        }
    }
    #[test]
    fn helper_drain_and_peek_fault_keep_independent_native_returns() {
        let records = with_units(vec![record(1, 1, 0, b"abc")], &[(3, 4, 0)]);
        let observed = helper_effect(false, 3, 7, &records);
        let capture = decode(&observed, records.clone()).unwrap();
        assert_eq!(capture.committed, b"abc");
        assert_eq!(capture.units[0].native.disposition, CONSUME);
        let mut stream = StreamingPrefix::for_helper(21, 0x40).unwrap();
        stream
            .advance(
                &observed.original.selection,
                &records,
                Some(End::OriginalExit { protocol: true }),
            )
            .unwrap();
        let mut fault = with_units(vec![record(1, 1, 0, b"ab")], &[(3, 7, -14)]);
        fault[1].bytes[64..72].copy_from_slice(&OBSERVE.to_le_bytes());
        let e = helper_effect(true, -14, 3, &fault);
        let capture = decode(&e, fault.clone()).unwrap();
        assert!(capture.committed.is_empty());
        assert_eq!(capture.records, fault);
        assert_eq!(capture.units[0].native.order, 7);
        assert_eq!(capture.manifest.returned, -14);
    }
    #[test]
    fn helpers_require_protocol_manifest_for_zero_and_local_errors() {
        for peek in [false, true] {
            for returned in [0, -libc::EAGAIN, -libc::EINTR] {
                let e = helper_effect(peek, returned, 9, &[]);
                let capture = decode(&e, vec![]).unwrap();
                assert!(capture.committed.is_empty());
                assert_eq!(capture.manifest.returned, i64::from(returned));
                let mut absent = e.clone();
                absent.read_copy.as_mut().unwrap().present = 0;
                absent.read_copy.as_mut().unwrap().summary = Summary::default();
                assert!(decode(&absent, vec![]).is_err());
                let mut stream = StreamingPrefix::for_helper(
                    if peek { 22 } else { 21 },
                    if peek { 0x42 } else { 0x40 },
                )
                .unwrap();
                assert!(
                    stream
                        .advance(
                            &e.original.selection,
                            &[],
                            Some(End::OriginalExit { protocol: false })
                        )
                        .is_err()
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "original_read_copy/frontier_tests.rs"]
mod frontier_tests;
