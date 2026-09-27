//! Private accepted-provider session transport. Descriptor ownership lives in
//! these run-owned queues, never in a future waiting for an acknowledgement.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkStreamOwner;

const VERSION: u32 = 1;
const MAX_MESSAGE: usize = 16 * 1024;
const MAX_RIGHTS: usize = 253; // Linux SCM_MAX_FD; receive enough to retain malformed excess.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Operation {
    PrepareNativeBirth,
    ObserveNativeBirth,
    CollectNativeBirth,
    CancelNativeBirth,
    TerminateNativeBirth,
    RetireNativeBirth,
    ObserveTerminalSocket,
    RetireTerminalSocketObservation,
    PrepareOriginalFileObservation,
    CollectOriginalFileObservation,
    RetireOriginalFileObservation,
    PrepareOriginalConnect,
    AwaitOriginalSelection,
    CollectOriginalConnect,
    ReadOriginalCopy,
    CancelOriginalConnect,
    TerminateOriginalConnect,
    RetireOriginalConnect,
    Bootstrap,
    GroupedBootstrap,
    EnrollListener,
    DrainCreations,
    PrepareSetter,
    FinishSetter,
    MatchAccepted,
    ReleaseAccepted,
    Reply,
    PrepareAccept,
    CollectAccept,
    DrainFdJournal,
    PrepareTableEnrollment,
    CollectTableEnrollment,
}

impl Operation {
    fn rights(self) -> usize {
        match self {
            Self::Bootstrap => 2, // actual controller pidfd + private controller endpoint
            Self::GroupedBootstrap => 3, // same two plus exact private grouped bootstrap endpoint
            Self::EnrollListener
            | Self::PrepareSetter
            | Self::MatchAccepted
            | Self::PrepareAccept => 2, // socket + task pidfd
            Self::ObserveTerminalSocket
            | Self::PrepareTableEnrollment
            | Self::PrepareOriginalConnect
            | Self::PrepareOriginalFileObservation
            | Self::PrepareNativeBirth
            | Self::ObserveNativeBirth => 1, // exact held target PIDFD_THREAD
            Self::RetireTerminalSocketObservation
            | Self::CollectOriginalFileObservation
            | Self::RetireOriginalFileObservation
            | Self::CollectNativeBirth
            | Self::CancelNativeBirth
            | Self::TerminateNativeBirth
            | Self::RetireNativeBirth
            | Self::DrainCreations
            | Self::FinishSetter
            | Self::ReleaseAccepted
            | Self::Reply
            | Self::CollectAccept
            | Self::DrainFdJournal
            | Self::CollectTableEnrollment
            | Self::AwaitOriginalSelection
            | Self::ReadOriginalCopy
            | Self::CollectOriginalConnect
            | Self::TerminateOriginalConnect
            | Self::CancelOriginalConnect
            | Self::RetireOriginalConnect => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Envelope {
    pub run: [u8; 16],
    pub sequence: u64,
    pub owner: Option<NetworkStreamOwner>,
    pub accept: Option<NetworkAcceptLeaseId>,
    pub operation: Operation,
    /// Operation-specific typed provider schema, decoded only after this outer
    /// correlation and rights check. This is not portable network trace data.
    pub body: Vec<u8>,
}

/// Exact consumed read-only reply. It contains no nested request body and no
/// descriptor capability, so one retained receipt has bounded size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ObservationReceipt {
    #[serde(default)]
    pub fd_journal: bool,
    pub sequence: u64,
    pub owner: Option<NetworkStreamOwner>,
    pub accept: Option<NetworkAcceptLeaseId>,
    pub body: Vec<u8>,
}
fn observation_request(envelope: &Envelope) -> bool {
    use super::accepted_provider::Request;
    (envelope.operation == Operation::DrainFdJournal
        && matches!(
            serde_json::from_slice::<Request>(&envelope.body),
            Ok(Request::AwaitFdEvent { .. })
        ))
        || envelope.operation == Operation::DrainCreations
            && matches!(
                serde_json::from_slice::<Request>(&envelope.body),
                Ok(Request::ReadStatus
                    | Request::ReadCreation { .. }
                    | Request::AwaitCreation { .. })
            )
}
fn pending_read_request(envelope: &Envelope) -> bool {
    observation_request(envelope)
        || (envelope.operation == Operation::AwaitOriginalSelection
            && matches!(
                serde_json::from_slice::<super::accepted_provider::Request>(&envelope.body),
                Ok(super::accepted_provider::Request::AwaitOriginalSelection { .. })
            ))
}
fn receipt(envelope: &Envelope, body: &[u8]) -> ObservationReceipt {
    ObservationReceipt {
        fd_journal: envelope.operation == Operation::DrainFdJournal,
        sequence: envelope.sequence,
        owner: envelope.owner,
        accept: envelope.accept,
        body: body.to_vec(),
    }
}

fn protocol(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn encode(envelope: &Envelope) -> io::Result<Vec<u8>> {
    if envelope.sequence == 0 || envelope.run == [0; 16] {
        return Err(protocol("zero accepted session identity"));
    }
    let bytes = serde_json::to_vec(&(VERSION, envelope))
        .map_err(|_| protocol("accepted session encoding failed"))?;
    if bytes.len() > MAX_MESSAGE {
        return Err(protocol("accepted session message too large"));
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> io::Result<Envelope> {
    if bytes.len() > MAX_MESSAGE {
        return Err(protocol("accepted session message too large"));
    }
    let (version, envelope): (u32, Envelope) =
        serde_json::from_slice(bytes).map_err(|_| protocol("invalid accepted session encoding"))?;
    if version != VERSION || envelope.sequence == 0 || envelope.run == [0; 16] {
        return Err(protocol("invalid accepted session version or identity"));
    }
    Ok(envelope)
}

#[derive(Debug, PartialEq, Eq)]
enum SendState {
    Prepared,
    Submitted,
    Acknowledged(Vec<u8>),
}

#[derive(Debug)]
struct Outgoing<T> {
    envelope: Envelope,
    encoded: Vec<u8>,
    rights: Vec<T>,
    state: SendState,
}

#[derive(Debug)]
struct Outbox<T> {
    wire_format: super::ProviderWireFormat,
    entries: BTreeMap<u64, Outgoing<T>>,
    next: u64,
}
impl<T> Default for Outbox<T> {
    fn default() -> Self {
        Self {
            wire_format: super::ProviderWireFormat::Abi7Copy4,
            entries: BTreeMap::new(),
            next: 1,
        }
    }
}
impl<T> Outbox<T> {
    fn prepare(
        &mut self,
        mut envelope: Envelope,
        rights: Vec<T>,
    ) -> Result<u64, (io::Error, Vec<T>)> {
        // Failure returns ownership rather than letting temporary arguments drop.
        if envelope.operation == Operation::Reply || rights.len() != envelope.operation.rights() {
            return Err((protocol("accepted request rights shape mismatch"), rights));
        }
        let Some(next) = self.next.checked_add(1) else {
            return Err((protocol("accepted session sequence exhausted"), rights));
        };
        envelope.sequence = self.next;
        let encoded = match encode(&envelope) {
            Ok(bytes) => bytes,
            Err(error) => return Err((error, rights)),
        };
        let sequence = self.next;
        self.entries.insert(
            sequence,
            Outgoing {
                envelope,
                encoded,
                rights,
                state: SendState::Prepared,
            },
        );
        self.next = next;
        Ok(sequence)
    }
    fn retire_observation(&mut self, sequence: u64, body: &[u8]) -> io::Result<ObservationReceipt> {
        let entry = self
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown observation retirement"))?;
        if !observation_request(&entry.envelope)
            || !entry.rights.is_empty()
            || entry.state != SendState::Acknowledged(body.to_vec())
        {
            return Err(protocol(
                "observation retirement lacks its exact acknowledged read",
            ));
        }
        let receipt = receipt(&entry.envelope, body);
        self.entries.remove(&sequence);
        Ok(receipt)
    }
    fn acknowledged_group(&self, sequences: &[u64]) -> io::Result<Vec<(&Envelope, &[u8], usize)>> {
        sequences
            .iter()
            .map(|sequence| {
                let entry = self
                    .entries
                    .get(sequence)
                    .ok_or_else(|| protocol("command outgoing custody missing"))?;
                let SendState::Acknowledged(body) = &entry.state else {
                    return Err(protocol("command outgoing effect unacknowledged"));
                };
                Ok((&entry.envelope, body.as_slice(), entry.rights.len()))
            })
            .collect()
    }
    fn retire_native_birth(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
        retirement: u64,
    ) -> io::Result<()> {
        let mut sequences = vec![prepared];
        sequences.extend(observed);
        sequences.push(completed);
        let views = self.acknowledged_group(&sequences)?;
        validate_native_birth_group(owner, call, prepared, observed, completed, &views)?;
        let last = self.acknowledged_group(&[retirement])?;
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let (envelope, body, rights) = last[0];
        if retirement <= completed
            || envelope.run != views[0].0.run
            || envelope.owner != Some(owner)
            || envelope.accept.is_some()
            || rights != 0
            || envelope.operation != Operation::RetireNativeBirth
            || !matches!(serde_json::from_slice::<Request>(&envelope.body),Ok(Request::RetireNativeBirth {
                call:c,prepared:p,observed:o,completed:f}) if (c,p,o,f)==(call,prepared,observed,completed))
            || !matches!(serde_json::from_slice::<Reply>(body), Ok(Reply::Retired))
        {
            return Err(protocol(
                "birth retirement acknowledgement changed exact group",
            ));
        }
        sequences.push(retirement);
        for sequence in sequences {
            self.entries.remove(&sequence);
        }
        Ok(())
    }
    fn retire_original(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        sequences: [u64; 4],
        failed_request: Option<u64>,
    ) -> io::Result<()> {
        let [prepared, selected, completed, retirement] = sequences;
        let mut views = Vec::new();
        for sequence in sequences {
            let entry = self
                .entries
                .get(&sequence)
                .ok_or_else(|| protocol("original outgoing custody missing"))?;
            let SendState::Acknowledged(body) = &entry.state else {
                return Err(protocol("original outgoing effect unacknowledged"));
            };
            views.push((&entry.envelope, body.as_slice(), entry.rights.len()));
        }
        validate_original_group(owner, call, [prepared, selected, completed], &views[..3])?;
        let copies = read_copy_outgoing_for_version(
            &self.entries,
            owner,
            call,
            prepared,
            completed,
            views[2].1,
            self.wire_format.copy_version(),
        )?;
        validate_failed_original_request(failed_request, views[2].0)?;
        if let Some(failed) = failed_request {
            let entry = self
                .entries
                .get(&failed)
                .ok_or_else(|| protocol("failed original outgoing frame missing"))?;
            let SendState::Acknowledged(body) = &entry.state else {
                return Err(protocol("failed original outgoing frame unacknowledged"));
            };
            validate_failed_original_group(
                &views[0],
                selected,
                completed,
                failed,
                &entry.envelope,
                body,
                entry.rights.len(),
            )?;
        }
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let last = views[3];
        if last.0.operation != Operation::RetireOriginalConnect
            || last.0.owner != Some(owner)
            || last.0.accept.is_some()
            || last.2 != 0
            || retirement <= completed
            || !matches!(serde_json::from_slice::<Request>(&last.0.body),Ok(Request::RetireOriginalConnect {
                call:c,prepared:p,selected:s,completed:f,failed_request:bad }) if (c,p,s,f,bad)==(call,prepared,selected,completed,failed_request))
            || !matches!(serde_json::from_slice::<Reply>(last.1), Ok(Reply::Retired))
        {
            return Err(protocol(
                "original retirement acknowledgement changed exact group",
            ));
        }
        for sequence in copies {
            self.entries.remove(&sequence);
        }
        for sequence in sequences {
            self.entries.remove(&sequence);
        }
        if let Some(failed) = failed_request {
            self.entries.remove(&failed);
        }
        Ok(())
    }
    fn acknowledge(&mut self, reply: &Envelope) -> io::Result<()> {
        let request = self
            .entries
            .get_mut(&reply.sequence)
            .ok_or_else(|| protocol("unmatched accepted acknowledgement"))?;
        if reply.operation != Operation::Reply
            || reply.run != request.envelope.run
            || reply.owner != request.envelope.owner
            || reply.accept != request.envelope.accept
        {
            return Err(protocol("accepted acknowledgement identity mismatch"));
        }
        match &request.state {
            SendState::Submitted => request.state = SendState::Acknowledged(reply.body.clone()),
            SendState::Acknowledged(prior) if prior == &reply.body => {}
            _ => {
                return Err(protocol(
                    "accepted acknowledgement changed or preceded submission",
                ));
            }
        }
        // The acknowledgement does not drop any descriptor. A subsequent
        // explicit handoff/release receipt must settle their physical custody.
        Ok(())
    }
}

/// Same finite Inbox/Outbox ownership as original calls. The auxiliary group
/// is typed separately so no guest preparation, worker identity, or command
/// can be substituted during retirement.
fn validate_terminal_socket_observation(
    owner: NetworkStreamOwner,
    call: u64,
    observed: u64,
    envelope: &Envelope,
    body: &[u8],
    rights: usize,
) -> io::Result<()> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    if observed == 0
        || envelope.sequence != observed
        || envelope.owner != Some(owner)
        || envelope.accept.is_some()
        || envelope.operation != Operation::ObserveTerminalSocket
        || rights != 1
    {
        return Err(protocol(
            "terminal Socket observation changed exact request custody",
        ));
    }
    let Request::ObserveTerminalSocket {
        call: requested,
        effect,
    } = serde_json::from_slice(&envelope.body)?
    else {
        return Err(protocol("terminal Socket observation changed request kind"));
    };
    super::terminal_socket_observation::validate_request(owner, call, &effect)?;
    let Reply::TerminalSocketObservation {
        call: returned,
        capture,
    } = serde_json::from_slice(body)?
    else {
        return Err(protocol(
            "terminal Socket observation changed retained response",
        ));
    };
    if requested != call || returned != call {
        return Err(protocol("terminal Socket observation changed Call"));
    }
    // A getter/capture failure is not retirement authority. Preserve the full
    // frame on error even if its auxiliary command was positively ACKed.
    capture.checked(&effect)?;
    Ok(())
}

#[cfg(test)]
fn validate_file_observation_group(
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    views: &[(&Envelope, &[u8], usize)],
) -> io::Result<()> {
    validate_file_observation_group_for_version(owner, call, prepared, completed, views, 4)
}
fn validate_file_observation_group_for_version(
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    views: &[(&Envelope, &[u8], usize)],
    version: u64,
) -> io::Result<()> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    if call == 0 || prepared == 0 || completed <= prepared || views.len() != 2 {
        return Err(protocol("auxiliary observation group order changed"));
    }
    let [(p, pb, pr), (c, cb, cr)] = views else {
        unreachable!()
    };
    if p.sequence != prepared
        || c.sequence != completed
        || *pr != 1
        || *cr != 0
        || p.owner != Some(owner)
        || c.owner != Some(owner)
        || p.accept.is_some()
        || c.accept.is_some()
        || p.operation != Operation::PrepareOriginalFileObservation
        || c.operation != Operation::CollectOriginalFileObservation
    {
        return Err(protocol(
            "auxiliary observation changed exact request ownership",
        ));
    }
    let Request::PrepareOriginalFileObservation {
        call: pc,
        mm,
        fd,
        role,
    } = serde_json::from_slice(&p.body)?
    else {
        return Err(protocol("auxiliary preparation changed kind"));
    };
    let Reply::Prepared(armed) = serde_json::from_slice(pb)? else {
        return Err(protocol("auxiliary preparation changed result"));
    };
    if pc != call
        || mm != owner.mm.generation()
        || fd < 0
        || armed.raw == 0
        || armed.status.returned != 0
        || armed.status.errno.is_some()
        || !role.valid()
        || armed.status.operation != role.prepare_name()
    {
        return Err(protocol(
            "auxiliary preparation has no actual armed command",
        ));
    }
    let Request::CollectOriginalFileObservation {
        call: cc,
        command,
        prepared_request,
        role: completed_role,
    } = serde_json::from_slice(&c.body)?
    else {
        return Err(protocol("auxiliary completion changed kind"));
    };
    let Reply::OriginalFileObservation {
        selection,
        effect: Some(effect),
    } = serde_json::from_slice(cb)?
    else {
        return Err(protocol(
            "auxiliary completion has no retained native effect",
        ));
    };
    let selected = &selection.raw;
    let raw = &effect.raw;
    role.check_selection(selected, call, mm, fd, command)?;
    let (_, _, count) = role.operands();
    if cc != call
        || command != armed.raw
        || prepared_request != prepared
        || completed_role != role
        || selection.status.returned != 0
        || selection.status.errno.is_some()
        || effect.status.returned != 0
        || effect.status.errno.is_some()
        || *selected != raw.original.selection
        || selected.command != command
        || selected.call != call
        || selected.owner_mm != mm
        || selected.requested_fd != fd
        || selected.provider == 0
        || selected.task == 0
        || selected.task_start == 0
        || selected.table == 0
        || selected.ready != 1
        || selected.fdput_flags & !1 != 0
        || raw.command.command != command
        || raw.command.operation != role.operation()
        || raw.command.phase != 1
        || raw.command.task != selected.task
        || raw.command.start_boottime != selected.task_start
        || raw.command.identity.provider != selected.provider
        || raw.command.original_count != count
        || raw.command.returned != raw.original.returned
        || raw.command.reserved != 0
        || raw.original.complete != 1
        || raw.original.problem != 0
        || raw.original.reserved != 0
        || raw.original.address.len() != 128
        || raw.original.address.iter().any(|b| *b != 0)
        || raw.original.copy_entered != 0
        || raw.original.copy_returned != 0
        || raw.original.copy_remaining != 0
        || raw.original.audit_entered != 0
        || raw.original.audit_returned != 0
        || raw.original.audit_result != 0
        || raw.original.security_entered != 0
        || raw.original.security_returned != 0
        || raw.original.security_result != 0
        || raw.socket.is_some()
        || (!role.is_receive() && raw.read_copy.is_some())
    {
        return Err(protocol(
            "auxiliary selection/completion changed original native operands or identity",
        ));
    }
    if role.is_receive() {
        if raw.original.returned < -4095 || i64::from(raw.original.returned) > count as i64 {
            return Err(protocol(
                "helper receive returned outside original count/errno domain",
            ));
        }
        let manifest = raw
            .read_copy
            .ok_or_else(|| protocol("helper receive lacks its native copy manifest"))?;
        manifest.validate_for_version(raw, version)?;
        if manifest.present != 1 {
            return Err(protocol(
                "helper receive lacks actual protocol selection/exit evidence",
            ));
        }
    }
    Ok(())
}

fn validate_failed_original_request(failed: Option<u64>, finish: &Envelope) -> io::Result<()> {
    use super::accepted_provider::Request;
    match serde_json::from_slice::<Request>(&finish.body)? {
        Request::TerminateOriginalConnect { failed_request, .. } if failed_request == failed => {
            Ok(())
        }
        Request::CollectOriginalConnect { .. } | Request::CancelOriginalConnect { .. }
            if failed.is_none() =>
        {
            Ok(())
        }
        _ => Err(protocol("original failed-frame ownership changed")),
    }
}
fn validate_failed_original_group(
    prepare: &(&Envelope, &[u8], usize),
    selected: u64,
    completed: u64,
    failed: u64,
    envelope: &Envelope,
    body: &[u8],
    rights: usize,
) -> io::Result<()> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    let Reply::Prepared(prepared) = serde_json::from_slice(prepare.1)? else {
        return Err(protocol("failed original lost preparation"));
    };
    let Request::PrepareOriginalConnect { call, kind, .. } =
        serde_json::from_slice(&prepare.0.body)?
    else {
        return Err(protocol("failed original changed preparation"));
    };
    if !(selected < failed && failed < completed)
        || envelope.sequence != failed
        || envelope.owner != prepare.0.owner
        || envelope.run != prepare.0.run
        || envelope.accept.is_some()
        || rights != 0
        || envelope.operation != Operation::CollectOriginalConnect
        || !matches!(serde_json::from_slice::<Request>(&envelope.body),Ok(Request::CollectOriginalConnect{call:c,command,prepared_request,kind:submitted}) if c==call && command==prepared.raw && prepared_request==prepare.0.sequence && submitted==kind)
        || !matches!(serde_json::from_slice::<Reply>(body),Ok(Reply::OriginalEffect(observation)) if observation.status.returned!=0)
    {
        return Err(protocol(
            "failed original collection is not its exact retained failed frame",
        ));
    }
    Ok(())
}
fn validate_native_birth_group(
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    observed: Option<u64>,
    completed: u64,
    views: &[(&Envelope, &[u8], usize)],
) -> io::Result<()> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    let mut sequences = vec![prepared];
    sequences.extend(observed);
    sequences.push(completed);
    if call == 0
        || prepared == 0
        || views.len() != sequences.len()
        || !sequences.windows(2).all(|v| v[0] < v[1])
    {
        return Err(protocol("birth group has invalid sequence order"));
    }
    for (n, (envelope, _, rights)) in views.iter().enumerate() {
        if envelope.sequence != sequences[n]
            || envelope.owner != Some(owner)
            || envelope.accept.is_some()
            || envelope.run == [0; 16]
            || envelope.run != views[0].0.run
            || *rights != usize::from(n == 0 || (observed.is_some() && n == 1))
        {
            return Err(protocol("birth group changed owner/run/rights"));
        }
    }
    let (prepare, prepared_body, _) = views[0];
    let Request::PrepareNativeBirth {
        call: c,
        mm,
        table,
        syscall,
    } = serde_json::from_slice(&prepare.body)?
    else {
        return Err(protocol("birth group lost preparation"));
    };
    let Reply::Prepared(p) = serde_json::from_slice(prepared_body)? else {
        return Err(protocol("birth preparation reply changed"));
    };
    if prepare.operation != Operation::PrepareNativeBirth
        || c != call
        || mm != owner.mm.generation()
        || table == 0
        || !matches!(syscall, 56 | 57 | 58 | 435)
        || p.raw == 0
        || p.status.returned != 0
        || p.status.errno.is_some()
    {
        return Err(protocol("birth preparation was not positively observed"));
    }
    let (finish, finished, _) = views[views.len() - 1];
    if let Request::TerminateNativeBirth {
        call: c,
        command,
        prepared_request,
    } = serde_json::from_slice::<Request>(&finish.body)?
    {
        if (c, command, prepared_request) != (call, p.raw, prepared)
            || finish.operation != Operation::TerminateNativeBirth
            || observed.is_none()
        {
            return Err(protocol("terminal birth changed original admitted group"));
        }
        let Reply::NativeBirthTerminated(result) = serde_json::from_slice(finished)? else {
            return Err(protocol("terminal birth reply changed"));
        };
        let r = &result.raw.command;
        let b = &result.raw.birth;
        let (observe, body, _) = views[1];
        if result.status.returned != 0
            || result.status.errno.is_some()
            || result.status.operation != "ap_retire_dead_birth"
            || result.raw.call != call
            || result.raw.fd_call_present != 1
            || result.raw.task_absent != 1
            || r.command != command
            || r.operation != 8
            || !matches!(r.phase, 1 | 3)
            || r.task == 0
            || r.start_boottime == 0
            || (r.identity.provider != 0 && r.identity.provider != b.provider)
            || b.command != command
            || b.call != call
            || b.owner_mm != mm
            || b.provider == 0
            || b.creator_table != table
            || b.creator_task != r.task
            || b.creator_start != r.start_boottime
            || b.ready != 1
            || b.problem != 0
            || b.child_task == 0
            || b.child_start == 0
            || observe.operation != Operation::ObserveNativeBirth
            || !matches!(serde_json::from_slice::<Request>(&observe.body),Ok(Request::ObserveNativeBirth {
                call:oc,command:ok,prepared_request:op,child,..}) if (oc,ok,op)==(call,command,prepared)
                    && child>0 && (r.phase!=1 || (r.returned==child && r.identity.provider==b.provider)))
            || !matches!(serde_json::from_slice::<Reply>(body),Ok(Reply::NativeBirth(value))
                if value.status.returned==0 && value.status.errno.is_none() && value.raw==*b)
        {
            return Err(protocol(
                "terminal creator lost actual admitted child or physical drain",
            ));
        }
        return Ok(());
    }
    let canceled = match serde_json::from_slice::<Request>(&finish.body)? {
        Request::CancelNativeBirth {
            call: c,
            command,
            prepared_request,
        } if (c, command, prepared_request) == (call, p.raw, prepared)
            && finish.operation == Operation::CancelNativeBirth =>
        {
            true
        }
        Request::CollectNativeBirth {
            call: c,
            command,
            prepared_request,
        } if (c, command, prepared_request) == (call, p.raw, prepared)
            && finish.operation == Operation::CollectNativeBirth =>
        {
            false
        }
        _ => return Err(protocol("birth completion changed exact command")),
    };
    if canceled {
        if observed.is_some()
            || !matches!(serde_json::from_slice::<Reply>(finished),Ok(Reply::NativeBirthCanceled {command,status})
            if command==p.raw && status.returned==0 && status.errno.is_none()
                && status.operation=="ap_cancel_uninvoked_birth")
        {
            return Err(protocol("uninvoked birth lacks exact positive disarm"));
        }
        return Ok(());
    }
    let Reply::NativeBirthEffect(result) = serde_json::from_slice(finished)? else {
        return Err(protocol("birth completion response changed"));
    };
    let r = &result.raw.command;
    let b = &result.raw.birth;
    if result.status.returned != 0
        || result.status.errno.is_some()
        || r.command != p.raw
        || r.operation != 8
        || r.phase != 1
        || r.task == 0
        || r.start_boottime == 0
        || r.identity.provider == 0
        || b.command != p.raw
        || b.call != call
        || b.owner_mm != mm
        || b.creator_table != table
        || b.provider != r.identity.provider
        || b.creator_task != r.task
        || b.creator_start != r.start_boottime
        || b.problem != 0
        || r.returned == 0
        || r.returned < -4095
    {
        return Err(protocol("birth completion lost exact native result"));
    }
    if r.returned < 0 {
        if observed.is_some() || b.ready != 0 {
            return Err(protocol("failed birth contains committed child"));
        }
        return Ok(());
    }
    let Some(_) = observed else {
        return Err(protocol("successful birth lacks child observation"));
    };
    let (observe, body, _) = views[1];
    if observe.operation != Operation::ObserveNativeBirth
        || b.ready != 1
        || !matches!(serde_json::from_slice::<Request>(&observe.body),Ok(Request::ObserveNativeBirth {
            call:c,command,prepared_request,child,..}) if (c,command,prepared_request)==(call,p.raw,prepared) && child==r.returned)
        || !matches!(serde_json::from_slice::<Reply>(body),Ok(Reply::NativeBirth(value))
            if value.status.returned==0 && value.status.errno.is_none() && value.raw==*b)
    {
        return Err(protocol("birth child and completion disagree"));
    }
    Ok(())
}

/// Preserve the pair and event bytes observed before completion. Later body
/// return/syscall completion may add facts; they cannot replace any fact that
/// was already retained by the same query. This also applies to task death,
/// where positive physical retirement supplies no missing syscall result.
fn validate_control_prefix(
    request: &super::original_epoll_ctl::Request,
    early: &super::accepted_provider::OriginalResult,
    retained: &super::accepted_provider::OriginalResult,
) -> io::Result<super::original_epoll_ctl::Capture> {
    let capture = super::original_epoll_ctl::validate_selection(request, early)?;
    let continuation = super::original_epoll_ctl::validate_selection(request, retained)?;
    if early.selection != retained.selection
        || retained.address.len() != 128
        || early.address[..96] != retained.address[..96]
        || (early.address[96..104] != [0; 8] && early.address[96..108] != retained.address[96..108])
        || (early.complete == 1 && early != retained)
        || capture != continuation
    {
        return Err(protocol(
            "original control changed its retained selection/copy facts",
        ));
    }
    Ok(capture)
}

/// A dead actor can leave a newly diagnosed capture failure after an already
/// observed pair. Positive physical retirement may dispose of its command,
/// but the exact diagnostic receipt stays OriginalTerminated and the engine
/// must retain its failed-run tombstone. This cannot issue a successful capture.
fn validate_control_terminal(
    request: &super::original_epoll_ctl::Request,
    early: &super::accepted_provider::OriginalResult,
    retained: &super::accepted_provider::OriginalResult,
) -> io::Result<()> {
    // Exact AP_FD_* diagnostic domain in the bound provider ABI. New/unknown
    // bits are not accepted by this terminal-only validation path.
    const KNOWN_FD_PROBLEMS: u64 = 1 | 2 | 4 | 8 | 16 | 32 | 64;
    if retained.problem & !KNOWN_FD_PROBLEMS != 0 {
        return Err(protocol(
            "original control terminal has an unknown diagnostic",
        ));
    }
    // This local validation copy never replaces the raw transport receipt.
    // Clear only known late diagnostic bits; every native fact still passes
    // the same canonical shape and monotonicity checks as an ordinary prefix.
    let mut continuation = retained.clone();
    continuation.problem = 0;
    validate_control_prefix(request, early, &continuation)?;
    Ok(())
}

fn validate_original_group(
    owner: NetworkStreamOwner,
    call: u64,
    sequences: [u64; 3],
    views: &[(&Envelope, &[u8], usize)],
) -> io::Result<()> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    if call == 0
        || views.len() != 3
        || !(sequences[0] < sequences[1] && sequences[1] < sequences[2])
    {
        return Err(protocol("original group has invalid sequence order"));
    }
    for (n, (envelope, _, rights)) in views.iter().enumerate() {
        if envelope.sequence != sequences[n]
            || envelope.owner != Some(owner)
            || envelope.accept.is_some()
            || envelope.run == [0; 16]
            || envelope.run != views[0].0.run
            || *rights != usize::from(n == 0)
        {
            return Err(protocol("original group changed owner/rights"));
        }
    }
    let (prepare, prepared, _) = views[0];
    let (query, selected, _) = views[1];
    let (finish, completed, _) = views[2];
    if prepare.operation != Operation::PrepareOriginalConnect
        || query.operation != Operation::AwaitOriginalSelection
        || !matches!(
            finish.operation,
            Operation::CollectOriginalConnect
                | Operation::CancelOriginalConnect
                | Operation::TerminateOriginalConnect
        )
    {
        return Err(protocol("original group changed operation"));
    }
    let Request::PrepareOriginalConnect {
        kind,
        call: c,
        mm,
        fd,
        address,
        length,
        original_count,
    } = serde_json::from_slice(&prepare.body)?
    else {
        return Err(protocol("original group lacks preparation"));
    };
    let Reply::Prepared(p) = serde_json::from_slice(prepared)? else {
        return Err(protocol("original preparation response changed"));
    };
    if c != call
        || mm != owner.mm.generation()
        || p.status.returned != 0
        || p.raw == 0
        || (kind == crate::network_replay::original_connect::Kind::Close
            && (address != 0 || length != 0))
    {
        return Err(protocol("original preparation was not positively observed"));
    }
    if let crate::network_replay::original_connect::Kind::File(operation) = kind
        && (address != operation.syscall() as u64 || length != operation.command())
    {
        return Err(protocol("file preparation changed syscall/command"));
    }
    if !kind.valid_operands(address, length, original_count) {
        return Err(protocol("original preparation changed counted operands"));
    }
    if kind == crate::network_replay::original_connect::Kind::EpollCtl
        && (p.status.errno.is_some() || p.status.operation != "ap_prepare_original_epoll_ctl")
    {
        return Err(protocol(
            "original control preparation changed provider operation",
        ));
    }
    for envelope in [query, finish] {
        let (c, command, prior) = match serde_json::from_slice(&envelope.body)? {
            Request::AwaitOriginalSelection {
                call,
                command,
                prepared_request,
            } => (call, command, prepared_request),
            Request::CollectOriginalConnect {
                kind: submitted,
                call,
                command,
                prepared_request,
            } if submitted == kind => (call, command, prepared_request),
            Request::CancelOriginalConnect {
                call,
                command,
                prepared_request,
                selected_request,
            }
            | Request::TerminateOriginalConnect {
                call,
                command,
                prepared_request,
                selected_request,
                ..
            } if selected_request == sequences[1] => (call, command, prepared_request),
            _ => return Err(protocol("original group changed query/final request")),
        };
        if c != call || command != p.raw || prior != sequences[0] {
            return Err(protocol("original group changed ticket"));
        }
    }
    if finish.operation == Operation::TerminateOriginalConnect {
        let Reply::OriginalTerminated(terminal) = serde_json::from_slice(completed)? else {
            return Err(protocol("dead original lacks typed physical retirement"));
        };
        if terminal.status.returned != 0
            || terminal.status.errno.is_some()
            || terminal.status.operation != "ap_retire_dead_original"
            || terminal.raw.call != call
            || terminal.raw.command.command != p.raw
            || terminal.raw.command.operation != kind.provider_operation()
            || terminal.raw.command.original_count != original_count
            || terminal.raw.task_absent != 1
            || terminal.raw.fd_call_present > 1
        {
            return Err(protocol(
                "dead original retirement changed command or absence proof",
            ));
        }
        match serde_json::from_slice::<Reply>(selected)? {
            Reply::OriginalTerminated(s) if s == terminal => {}
            Reply::OriginalSelection(s)
                if kind != crate::network_replay::original_connect::Kind::EpollCtl
                    && s.status.returned == 0
                    && terminal.raw.fd_call_present == 1
                    && s.raw == terminal.raw.original.selection => {}
            Reply::OriginalControlSelection(s)
                if kind == crate::network_replay::original_connect::Kind::EpollCtl
                    && s.status.returned == 0
                    && s.status.errno.is_none()
                    && s.status.operation == "ap_read_original_epoll_ctl_selection"
                    && terminal.raw.fd_call_present == 1 =>
            {
                validate_control_terminal(
                    &super::original_epoll_ctl::Request {
                        command: p.raw,
                        call,
                        owner_mm: mm,
                        provider: s.raw.selection.provider,
                        epfd: fd,
                        operation: length,
                        target_fd: original_count as u32 as i32,
                        event_address: address,
                    },
                    &s.raw,
                    &terminal.raw.original,
                )?;
            }
            _ => {
                return Err(protocol(
                    "dead original changed its retained early observation",
                ));
            }
        }
        return Ok(());
    }
    if finish.operation == Operation::CancelOriginalConnect {
        for body in [selected, completed] {
            if !matches!(serde_json::from_slice::<Reply>(body),Ok(Reply::OriginalCanceled {command,status})
                if command==p.raw && status.returned==0 && status.errno.is_none()
                    && status.operation=="ap_cancel_uninvoked_original")
            {
                return Err(protocol(
                    "known-uninvoked original lacks exact positive disarm",
                ));
            }
        }
        return Ok(());
    }
    if kind == crate::network_replay::original_connect::Kind::EpollCtl {
        let Reply::OriginalControlSelection(s) = serde_json::from_slice(selected)? else {
            return Err(protocol("original control lost its two-selection receipt"));
        };
        let Reply::OriginalEffect(f) = serde_json::from_slice(completed)? else {
            return Err(protocol("original control final response changed"));
        };
        if s.status.returned != 0
            || s.status.errno.is_some()
            || s.status.operation != "ap_read_original_epoll_ctl_selection"
            || f.status.returned != 0
            || f.status.errno.is_some()
            || f.status.operation != "ap_collect_original_connect"
        {
            return Err(protocol(
                "original control lacks positive exact provider operations",
            ));
        }
        let request = super::original_epoll_ctl::Request {
            command: p.raw,
            call,
            owner_mm: mm,
            provider: s.raw.selection.provider,
            epfd: fd,
            operation: length,
            target_fd: original_count as u32 as i32,
            event_address: address,
        };
        let early = validate_control_prefix(&request, &s.raw, &f.raw.original)?;
        let (complete, _) = super::original_epoll_ctl::validate_completion(&request, &f.raw)?;
        if early != complete {
            return Err(protocol(
                "original control completion changed its retained pair",
            ));
        }
        return Ok(());
    }
    let Reply::OriginalSelection(s) = serde_json::from_slice(selected)? else {
        return Err(protocol("original selection response changed"));
    };
    let Reply::OriginalEffect(f) = serde_json::from_slice(completed)? else {
        return Err(protocol("original final response changed"));
    };
    if s.status.returned != 0
        || f.status.returned != 0
        || s.raw != f.raw.original.selection
        || s.raw.command != p.raw
        || s.raw.call != call
        || s.raw.owner_mm != mm
        || s.raw.requested_fd != fd
        || s.raw.user_address != address
        || s.raw.address_length != length
        || s.raw.original_count != original_count
        || s.raw.ready != 1
        || f.raw.command.command != p.raw
        || f.raw.command.operation != kind.provider_operation()
        || f.raw.command.original_count != original_count
        || f.raw.command.phase != 1
        || f.raw.original.complete != 1
        || f.raw.original.problem != 0
        || f.raw.command.returned != f.raw.original.returned
    {
        return Err(protocol(
            "original group is not its exact positive selected/final receipt",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct Incoming<T> {
    read_copy: Option<Vec<super::original_read_copy::Record>>,
    read_copy_end: Option<super::original_read_copy::End>,
    read_copy_finalized: bool,
    envelope: Envelope,
    rights: Vec<T>,
    state: IncomingState,
    command_ack: CommandAckState,
}

#[derive(Debug, PartialEq, Eq, Default)]
enum CommandAckState {
    #[default]
    Unsubmitted,
    Submitted,
    Completed(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
enum IncomingState {
    Retained,
    Submitted,
    Completed(Vec<u8>),
}

#[derive(Debug)]
struct Inbox<T> {
    wire_format: super::ProviderWireFormat,
    retired_observation: Option<ObservationReceipt>,
    retired_fd_observation: Option<ObservationReceipt>,
    entries: BTreeMap<u64, Incoming<T>>,
    next: u64,
}
impl<T> Default for Inbox<T> {
    fn default() -> Self {
        Self {
            wire_format: super::ProviderWireFormat::Abi7Copy4,
            retired_observation: None,
            retired_fd_observation: None,
            entries: BTreeMap::new(),
            next: 1,
        }
    }
}
impl<T> Inbox<T> {
    /// The primary response and all rights are owned here before the provider
    /// ACK effect. Lost/failed ACK cannot run the physical command again.
    fn acknowledge_command_completion(
        &mut self,
        sequence: u64,
        effect: impl FnOnce(&Envelope, &[u8]) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        let entry = self
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown provider command acknowledgement"))?;
        if !matches!(
            entry.envelope.operation,
            Operation::EnrollListener
                | Operation::MatchAccepted
                | Operation::FinishSetter
                | Operation::CollectAccept
                | Operation::CollectTableEnrollment
                | Operation::CollectOriginalConnect
                | Operation::CollectOriginalFileObservation
                | Operation::ObserveTerminalSocket
                | Operation::CollectNativeBirth
                | Operation::DrainFdJournal
        ) {
            return Err(protocol(
                "read-only/preparation request is not a completed provider command",
            ));
        }
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol(
                "provider ACK requires a durably retained primary response",
            ));
        };
        match &entry.command_ack {
            CommandAckState::Completed(status) => return Ok(status.clone()),
            CommandAckState::Submitted => {
                return Err(protocol(
                    "provider ACK remains submitted with unknown outcome",
                ));
            }
            CommandAckState::Unsubmitted => {}
        }
        use super::accepted_provider::Request;
        let copy_required = match serde_json::from_slice::<Request>(&entry.envelope.body) {
            Ok(Request::CollectOriginalConnect {
                kind: crate::network_replay::original_connect::Kind::Read,
                ..
            }) => {
                if entry.envelope.operation != Operation::CollectOriginalConnect {
                    return Err(protocol("original Read ACK changed request operation"));
                }
                Some("original Read ACK precedes retained native copy prefix")
            }
            Ok(Request::CollectOriginalFileObservation { role, .. }) if role.is_receive() => {
                if entry.envelope.operation != Operation::CollectOriginalFileObservation {
                    return Err(protocol("helper receive ACK changed request operation"));
                }
                Some("helper receive ACK precedes retained native copy prefix")
            }
            _ => None,
        };
        if let Some(message) = copy_required
            && !entry.read_copy_finalized
        {
            return Err(protocol(message));
        }
        entry.command_ack = CommandAckState::Submitted;
        let status = effect(&entry.envelope, body)?;
        entry.command_ack = CommandAckState::Completed(status.clone());
        Ok(status)
    }
    fn retire_observation(&mut self, receipt: &ObservationReceipt) -> io::Result<()> {
        let retired = if receipt.fd_journal {
            &mut self.retired_fd_observation
        } else {
            &mut self.retired_observation
        };
        if retired.as_ref() == Some(receipt) {
            return Ok(()); // duplicate exact ACK, never re-run the observation
        }
        if retired
            .as_ref()
            .is_some_and(|old| old.sequence >= receipt.sequence)
        {
            return Err(protocol("stale or changed observation retirement"));
        }
        let entry = self
            .entries
            .get(&receipt.sequence)
            .ok_or_else(|| protocol("retirement names an unknown observation"))?;
        if !observation_request(&entry.envelope)
            || !entry.rights.is_empty()
            || (receipt.fd_journal && !matches!(entry.command_ack, CommandAckState::Completed(_)))
            || (entry.envelope.operation == Operation::DrainFdJournal) != receipt.fd_journal
            || entry.envelope.owner != receipt.owner
            || entry.envelope.accept != receipt.accept
            || entry.state != IncomingState::Completed(receipt.body.clone())
        {
            return Err(protocol(
                "observation retirement changed its exact completed read",
            ));
        }
        self.entries.remove(&receipt.sequence);
        *retired = Some(receipt.clone());
        Ok(())
    }
    fn completed_group(&self, sequences: &[u64]) -> io::Result<Vec<(&Envelope, &[u8], usize)>> {
        sequences
            .iter()
            .map(|sequence| {
                let entry = self
                    .entries
                    .get(sequence)
                    .ok_or_else(|| protocol("command incoming custody missing"))?;
                let IncomingState::Completed(body) = &entry.state else {
                    return Err(protocol("command incoming effect unresolved"));
                };
                Ok((&entry.envelope, body.as_slice(), entry.rights.len()))
            })
            .collect()
    }
    fn retire_native_birth(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
    ) -> io::Result<()> {
        let mut sequences = vec![prepared];
        sequences.extend(observed);
        sequences.push(completed);
        let views = self.completed_group(&sequences)?;
        validate_native_birth_group(owner, call, prepared, observed, completed, &views)?;
        let finish = &self.entries[&completed];
        if finish.envelope.operation == Operation::CollectNativeBirth {
            let CommandAckState::Completed(ack) = &finish.command_ack else {
                return Err(protocol("birth provider command ACK unresolved"));
            };
            // Same positive command ACK predicate used by original connect.
            super::accepted_provider::Provider::require_original_acknowledgement(ack)?;
        }
        for sequence in sequences {
            self.entries.remove(&sequence);
        }
        Ok(())
    }
    fn retire_original(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        sequences: [u64; 3],
        failed_request: Option<u64>,
    ) -> io::Result<()> {
        let mut views = Vec::new();
        for sequence in sequences {
            let entry = self
                .entries
                .get(&sequence)
                .ok_or_else(|| protocol("original incoming custody missing"))?;
            let IncomingState::Completed(body) = &entry.state else {
                return Err(protocol("original incoming effect unresolved"));
            };
            views.push((&entry.envelope, body.as_slice(), entry.rights.len()));
        }
        validate_original_group(owner, call, sequences, &views)?;
        let copies = read_copy_incoming_for_version(
            &self.entries,
            owner,
            call,
            sequences[0],
            sequences[2],
            views[2].1,
            self.wire_format.copy_version(),
        )?;
        validate_failed_original_request(failed_request, views[2].0)?;
        if let Some(failed) = failed_request {
            let entry = self
                .entries
                .get(&failed)
                .ok_or_else(|| protocol("failed original incoming frame missing"))?;
            let IncomingState::Completed(body) = &entry.state else {
                return Err(protocol("failed original incoming frame unresolved"));
            };
            validate_failed_original_group(
                &views[0],
                sequences[1],
                sequences[2],
                failed,
                &entry.envelope,
                body,
                entry.rights.len(),
            )?;
            let CommandAckState::Completed(ack) = &entry.command_ack else {
                return Err(protocol(
                    "failed original collection ACK disposition unknown",
                ));
            };
            super::accepted_provider::Provider::require_uncollected_acknowledgement(ack)?;
        }
        let final_entry = &self.entries[&sequences[2]];
        if final_entry.envelope.operation == Operation::CollectOriginalConnect {
            let CommandAckState::Completed(ack) = &final_entry.command_ack else {
                return Err(protocol("original provider command has not retired"));
            };
            super::accepted_provider::Provider::require_original_acknowledgement(ack)?;
        } // Cancel uses its positively verified disarm, never a fabricated command ACK.
        for sequence in copies {
            self.entries.remove(&sequence);
        }
        for sequence in sequences {
            self.entries.remove(&sequence);
        }
        if let Some(failed) = failed_request {
            self.entries.remove(&failed);
        }
        Ok(())
    }
    // The original preparation already owns the target PIDFD and command.
    // Retain copy prefixes there while the original syscall is still running;
    // the final completion remains a separate member of the same group.
    fn retain_read_copy_prefix(
        &mut self,
        prepared: u64,
        records: Vec<super::original_read_copy::Record>,
        end: super::original_read_copy::End,
    ) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&prepared)
            .ok_or_else(|| protocol("Read copy preparation missing"))?;
        let prior = entry.read_copy.as_deref().unwrap_or(&[]);
        if !records.starts_with(prior)
            || entry.read_copy_end.is_some_and(|old| old != end)
            || entry.read_copy_end.is_some() && records.len() != prior.len()
        {
            return Err(protocol("Read copy final cut changed retained prefix"));
        }
        entry.read_copy = Some(records);
        entry.read_copy_end = Some(end);
        Ok(())
    }
    fn retain_read_copy(
        &mut self,
        completed: u64,
        records: Vec<super::original_read_copy::Record>,
    ) -> io::Result<()> {
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let entry = self
            .entries
            .get(&completed)
            .ok_or_else(|| protocol("Read copy completion missing"))?;
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("Read copy completion unresolved"));
        };
        let (observed, prepared_request) = match (
            serde_json::from_slice::<Reply>(body)?,
            serde_json::from_slice::<Request>(&entry.envelope.body)?,
        ) {
            (
                Reply::OriginalEffect(observed),
                Request::CollectOriginalConnect {
                    kind: crate::network_replay::original_connect::Kind::Read,
                    prepared_request,
                    ..
                },
            ) if entry.envelope.operation == Operation::CollectOriginalConnect => {
                (observed, prepared_request)
            }
            (
                Reply::OriginalFileObservation {
                    selection,
                    effect: Some(observed),
                },
                Request::CollectOriginalFileObservation {
                    role,
                    prepared_request,
                    ..
                },
            ) if entry.envelope.operation == Operation::CollectOriginalFileObservation
                && role.is_receive()
                && selection.status.returned == 0
                && selection.status.errno.is_none()
                && selection.raw == observed.raw.original.selection =>
            {
                (observed, prepared_request)
            }
            _ => {
                return Err(protocol(
                    "Read copy completion changed its prepared receive kind",
                ));
            }
        };
        let manifest = observed
            .raw
            .read_copy
            .ok_or_else(|| protocol("Read copy manifest missing"))?;
        super::original_read_copy::validate_records_for_version(
            &observed.raw,
            &records,
            self.wire_format.copy_version(),
        )?;
        if observed.status.returned != 0
            || entry.read_copy_finalized
            || records.len() as u64 != manifest.summary.records
        {
            return Err(protocol("Read copy retention changed original completion"));
        }
        self.retain_read_copy_prefix(
            prepared_request,
            records,
            super::original_read_copy::End::OriginalExit {
                protocol: manifest.present == 1,
            },
        )?;
        self.entries
            .get_mut(&completed)
            .unwrap()
            .read_copy_finalized = true;
        Ok(())
    }
    fn read_copy_chunk(
        &mut self,
        request: &Envelope,
        rights: usize,
        call: u64,
        command: u64,
        prepared: u64,
        first: u64,
        fetch: impl FnOnce() -> io::Result<Option<super::original_read_copy::Chunk>>,
    ) -> io::Result<Option<super::original_read_copy::Chunk>> {
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let entry = self
            .entries
            .get_mut(&prepared)
            .ok_or_else(|| protocol("Read copy target absent"))?;
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("Read copy preparation unresolved"));
        };
        let Reply::Prepared(observed) = serde_json::from_slice(body)? else {
            return Err(protocol("Read copy target changed preparation"));
        };
        let original = match serde_json::from_slice::<Request>(&entry.envelope.body)? {
            Request::PrepareOriginalConnect {
                kind: crate::network_replay::original_connect::Kind::Read,
                call,
                ..
            } if entry.envelope.operation == Operation::PrepareOriginalConnect => call,
            Request::PrepareOriginalFileObservation { call, role, .. }
                if entry.envelope.operation == Operation::PrepareOriginalFileObservation
                    && role.is_receive()
                    && role.valid() =>
            {
                call
            }
            _ => return Err(protocol("Read copy target is not an authenticated receive")),
        };
        if request.operation != Operation::ReadOriginalCopy
            || rights != 0
            || request.sequence <= prepared
            || request.run != entry.envelope.run
            || request.owner.is_none()
            || request.owner != entry.envelope.owner
            || request.accept.is_some()
            || entry.envelope.accept.is_some()
            || observed.status.returned != 0
            || observed.status.errno.is_some()
            || call != original
            || command == 0
            || command != observed.raw
        {
            return Err(protocol(
                "Read copy request changed retained preparation identity",
            ));
        }
        let records = entry.read_copy.get_or_insert_with(Vec::new);
        let start = usize::try_from(first).map_err(|_| protocol("Read copy index range"))?;
        if start > records.len() {
            return Err(protocol("Read copy request skipped retained prefix"));
        }
        if start < records.len() || entry.read_copy_end.is_some() {
            let end = (start + super::original_read_copy::RECORDS_PER_REPLY).min(records.len());
            return Ok(Some(super::original_read_copy::Chunk {
                prepared,
                first,
                records: records[start..end].to_vec(),
                end: if end == records.len() {
                    entry.read_copy_end
                } else {
                    None
                },
            }));
        }
        let Some(chunk) = fetch()? else {
            return Ok(None);
        };
        if chunk.prepared != prepared
            || chunk.first != first
            || chunk.records.len() > super::original_read_copy::RECORDS_PER_REPLY
            || chunk.records.is_empty() && chunk.end.is_none()
        {
            return Err(protocol(
                "Read copy producer returned another prefix or empty progress",
            ));
        }
        records.extend_from_slice(&chunk.records);
        entry.read_copy_end = chunk.end;
        Ok(Some(chunk))
    }
    // Called only after the complete zero-right retirement reply was sent. The
    // monotonically increasing Inbox sequence already rejects a replayed frame;
    // no growing per-call tombstone/payload journal is necessary.
    fn retire_sent_original_ack(&mut self, sequence: u64) -> io::Result<()> {
        let entry = self
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("sent original ACK missing"))?;
        if !matches!(
            entry.envelope.operation,
            Operation::RetireOriginalConnect
                | Operation::RetireNativeBirth
                | Operation::RetireOriginalFileObservation
                | Operation::RetireTerminalSocketObservation
        ) {
            return Ok(());
        }
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("original ACK not complete"));
        };
        let expected = match serde_json::from_slice::<super::accepted_provider::Reply>(body)? {
            super::accepted_provider::Reply::Retired => {
                entry.envelope.operation != Operation::RetireOriginalFileObservation
            }
            super::accepted_provider::Reply::OriginalFileObservationRetired(status) => {
                entry.envelope.operation == Operation::RetireOriginalFileObservation
                    && status.returned == 0
                    && status.errno.is_none()
                    && status.operation == "ap_retire_auxiliary_task"
            }
            _ => false,
        };
        if !entry.rights.is_empty() || !expected {
            return Err(protocol("original ACK has unexpected custody"));
        }
        self.entries.remove(&sequence);
        Ok(())
    }
    fn begin_observation(&mut self, sequence: u64) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown pending observation"))?;
        if !pending_read_request(&entry.envelope)
            || !entry.rights.is_empty()
            || entry.state != IncomingState::Retained
        {
            return Err(protocol(
                "pending observation is not a fresh read-only request",
            ));
        }
        entry.state = IncomingState::Submitted;
        Ok(())
    }
    fn finish_observation(&mut self, sequence: u64, body: Vec<u8>) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown observation completion"))?;
        if !pending_read_request(&entry.envelope)
            || !entry.rights.is_empty()
            || entry.state != IncomingState::Submitted
        {
            return Err(protocol("observation completion lacks its submitted read"));
        }
        entry.state = IncomingState::Completed(body);
        Ok(())
    }
    fn retain(&mut self, envelope: Envelope, rights: Vec<T>) -> Result<u64, (io::Error, Vec<T>)> {
        if envelope.sequence != self.next || envelope.operation == Operation::Reply {
            return Err((
                protocol("accepted request sequence changed or repeated"),
                rights,
            ));
        }
        let Some(next) = self.next.checked_add(1) else {
            return Err((protocol("accepted incoming sequence exhausted"), rights));
        };
        let sequence = envelope.sequence;
        self.entries.insert(
            sequence,
            Incoming {
                read_copy: None,
                read_copy_end: None,
                read_copy_finalized: false,
                envelope,
                rights,
                state: IncomingState::Retained,
                command_ack: CommandAckState::Unsubmitted,
            },
        );
        self.next = next;
        Ok(sequence)
    }
    fn dispatch(
        &mut self,
        sequence: u64,
        operation: impl FnOnce(&Envelope, &[T]) -> io::Result<Vec<u8>>,
    ) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown accepted incoming request"))?;
        if entry.state != IncomingState::Retained {
            return Err(protocol(
                "accepted provider effect cannot be submitted twice",
            ));
        }
        entry.state = IncomingState::Submitted;
        let result = operation(&entry.envelope, &entry.rights)?;
        entry.state = IncomingState::Completed(result);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Received {
    Request(u64),
    Acknowledged(u64),
}

#[derive(Debug)]
struct Unclassified {
    bytes: Vec<u8>,
    rights: Vec<OwnedFd>,
    flags: i32,
    control_valid: bool,
}

/// Both endpoint branches retain malformed/unmatched received descriptors.
/// Errors borrow this object, so cancellation cannot erase the custody owner.
#[derive(Debug)]
pub(super) struct AcceptedSession {
    endpoint: OwnedFd,
    run: [u8; 16],
    outgoing: Outbox<OwnedFd>,
    incoming: Inbox<OwnedFd>,
    quarantine: Vec<Unclassified>,
}

/// Terminal evidence counts custody without dropping any entry. Completed
/// responses are retained history; submitted effects and unclassified rights
/// remain unresolved even after the controller has exited.
#[derive(Debug, Serialize)]
pub(super) struct SessionCustody {
    pub incoming: usize,
    pub outgoing: usize,
    pub incoming_unfinished: usize,
    pub outgoing_unacknowledged: usize,
    pub command_ack_unknown: usize,
    pub retained_rights: usize,
    pub quarantined_messages: usize,
    pub quarantined_rights: usize,
}

impl AcceptedSession {
    pub(super) fn terminal_custody(&self) -> SessionCustody {
        SessionCustody {
            incoming: self.incoming.entries.len(),
            outgoing: self.outgoing.entries.len(),
            incoming_unfinished: self
                .incoming
                .entries
                .values()
                .filter(|entry| !matches!(entry.state, IncomingState::Completed(_)))
                .count(),
            outgoing_unacknowledged: self
                .outgoing
                .entries
                .values()
                .filter(|entry| !matches!(entry.state, SendState::Acknowledged(_)))
                .count(),
            command_ack_unknown: self
                .incoming
                .entries
                .values()
                .filter(|entry| entry.command_ack == CommandAckState::Submitted)
                .count(),
            retained_rights: self
                .incoming
                .entries
                .values()
                .map(|entry| entry.rights.len())
                .sum::<usize>()
                + self
                    .outgoing
                    .entries
                    .values()
                    .map(|entry| entry.rights.len())
                    .sum::<usize>(),
            quarantined_messages: self.quarantine.len(),
            quarantined_rights: self.quarantine.iter().map(|entry| entry.rights.len()).sum(),
        }
    }

    pub(super) fn acknowledge_command_completion(
        &mut self,
        sequence: u64,
        effect: impl FnOnce(&Envelope, &[u8]) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        self.incoming
            .acknowledge_command_completion(sequence, effect)
    }
    pub(super) fn retire_outgoing_observation(
        &mut self,
        sequence: u64,
        body: &[u8],
    ) -> io::Result<ObservationReceipt> {
        self.outgoing.retire_observation(sequence, body)
    }
    pub(super) fn retire_incoming_observation(
        &mut self,
        receipt: &ObservationReceipt,
    ) -> io::Result<()> {
        self.incoming.retire_observation(receipt)
    }
    pub(super) fn begin_observation(&mut self, sequence: u64) -> io::Result<()> {
        self.incoming.begin_observation(sequence)
    }
    pub(super) fn finish_observation(&mut self, sequence: u64, body: Vec<u8>) -> io::Result<()> {
        self.incoming.finish_observation(sequence, body)
    }
    pub(super) fn check_outgoing_native_birth(
        &self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
    ) -> io::Result<()> {
        let mut sequences = vec![prepared];
        sequences.extend(observed);
        sequences.push(completed);
        let views = self.outgoing.acknowledged_group(&sequences)?;
        validate_native_birth_group(owner, call, prepared, observed, completed, &views)
    }
    pub(super) fn retire_outgoing_native_birth(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
        retirement: u64,
    ) -> io::Result<()> {
        self.outgoing
            .retire_native_birth(owner, call, prepared, observed, completed, retirement)
    }
    pub(super) fn retire_incoming_native_birth(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
    ) -> io::Result<()> {
        self.incoming
            .retire_native_birth(owner, call, prepared, observed, completed)
    }
    pub(super) fn retire_incoming_terminal_socket(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        observed: u64,
    ) -> io::Result<()> {
        let entry = self
            .incoming
            .entries
            .get(&observed)
            .ok_or_else(|| protocol("terminal Socket incoming frame missing"))?;
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("terminal Socket observation unresolved"));
        };
        validate_terminal_socket_observation(
            owner,
            call,
            observed,
            &entry.envelope,
            body,
            entry.rights.len(),
        )?;
        let CommandAckState::Completed(ack) = &entry.command_ack else {
            return Err(protocol("terminal Socket auxiliary ACK remains owned"));
        };
        super::accepted_provider::Provider::require_terminal_socket_acknowledgement(ack)?;
        self.incoming.entries.remove(&observed);
        Ok(())
    }
    pub(super) fn retire_outgoing_terminal_socket(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        observed: u64,
        retired: u64,
    ) -> io::Result<()> {
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let views = self.outgoing.acknowledged_group(&[observed])?;
        let first = views[0];
        validate_terminal_socket_observation(owner, call, observed, first.0, first.1, first.2)?;
        let entry = self
            .outgoing
            .entries
            .get(&retired)
            .ok_or_else(|| protocol("terminal Socket retirement frame missing"))?;
        let SendState::Acknowledged(body) = &entry.state else {
            return Err(protocol("terminal Socket retirement unacknowledged"));
        };
        if retired <= observed
            || entry.envelope.owner != Some(owner)
            || entry.envelope.accept.is_some()
            || !entry.rights.is_empty()
            || entry.envelope.operation != Operation::RetireTerminalSocketObservation
            || !matches!(serde_json::from_slice::<Request>(&entry.envelope.body),
                Ok(Request::RetireTerminalSocketObservation { call: c, observed: o }) if (c,o) == (call,observed))
            || !matches!(serde_json::from_slice::<Reply>(body), Ok(Reply::Retired))
        {
            return Err(protocol("terminal Socket retirement changed exact group"));
        }
        self.outgoing.entries.remove(&observed);
        self.outgoing.entries.remove(&retired);
        Ok(())
    }

    pub(super) fn check_incoming_file_observation(
        &self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        completed: u64,
    ) -> io::Result<()> {
        let mut views = Vec::new();
        for sequence in [prepared, completed] {
            let entry = self
                .incoming
                .entries
                .get(&sequence)
                .ok_or_else(|| protocol("auxiliary incoming frame missing"))?;
            let IncomingState::Completed(body) = &entry.state else {
                return Err(protocol("auxiliary incoming frame unresolved"));
            };
            views.push((&entry.envelope, body.as_slice(), entry.rights.len()));
        }
        validate_file_observation_group_for_version(
            owner,
            call,
            prepared,
            completed,
            &views,
            self.incoming.wire_format.copy_version(),
        )?;
        read_copy_incoming_for_version(
            &self.incoming.entries,
            owner,
            call,
            prepared,
            completed,
            views[1].1,
            self.incoming.wire_format.copy_version(),
        )?;
        let CommandAckState::Completed(ack) = &self.incoming.entries[&completed].command_ack else {
            return Err(protocol("auxiliary command ACK remains owned"));
        };
        super::accepted_provider::Provider::require_original_acknowledgement(ack)
    }
    pub(super) fn retire_incoming_file_observation(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        completed: u64,
    ) -> io::Result<()> {
        self.check_incoming_file_observation(owner, call, prepared, completed)?;
        let IncomingState::Completed(body) = &self.incoming.entries[&completed].state else {
            unreachable!()
        };
        let copies = read_copy_incoming_for_version(
            &self.incoming.entries,
            owner,
            call,
            prepared,
            completed,
            body,
            self.incoming.wire_format.copy_version(),
        )?;
        for sequence in copies {
            self.incoming.entries.remove(&sequence);
        }
        // Only PIDFD rights are dropped here. No regular-file flush runs in
        // the synchronous service loop.
        self.incoming.entries.remove(&prepared);
        self.incoming.entries.remove(&completed);
        Ok(())
    }
    pub(super) fn retire_outgoing_file_observation(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        prepared: u64,
        completed: u64,
        retired: u64,
    ) -> io::Result<()> {
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let views = self.outgoing.acknowledged_group(&[prepared, completed])?;
        validate_file_observation_group_for_version(
            owner,
            call,
            prepared,
            completed,
            &views,
            self.outgoing.wire_format.copy_version(),
        )?;
        let copies = read_copy_outgoing_for_version(
            &self.outgoing.entries,
            owner,
            call,
            prepared,
            completed,
            views[1].1,
            self.outgoing.wire_format.copy_version(),
        )?;
        let entry = self
            .outgoing
            .entries
            .get(&retired)
            .ok_or_else(|| protocol("auxiliary retirement frame missing"))?;
        let SendState::Acknowledged(body) = &entry.state else {
            return Err(protocol("auxiliary retirement response unresolved"));
        };
        if retired <= completed
            || entry.envelope.owner != Some(owner)
            || entry.envelope.accept.is_some()
            || !entry.rights.is_empty()
            || entry.envelope.operation != Operation::RetireOriginalFileObservation
            || !matches!(serde_json::from_slice::<Request>(&entry.envelope.body),
                Ok(Request::RetireOriginalFileObservation { call: c, prepared: p, completed: d })
                    if (c,p,d) == (call,prepared,completed))
            || !matches!(serde_json::from_slice::<Reply>(body),
                Ok(Reply::OriginalFileObservationRetired(status)) if status.returned == 0
                    && status.errno.is_none() && status.operation == "ap_retire_auxiliary_task")
        {
            return Err(protocol(
                "auxiliary retirement changed exact group or idle task receipt",
            ));
        }
        for sequence in copies {
            self.outgoing.entries.remove(&sequence);
        }
        for sequence in [prepared, completed, retired] {
            self.outgoing.entries.remove(&sequence);
        }
        Ok(())
    }
    pub(super) fn retire_outgoing_original(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        sequences: [u64; 4],
        failed_request: Option<u64>,
    ) -> io::Result<()> {
        self.outgoing
            .retire_original(owner, call, sequences, failed_request)
    }
    pub(super) fn retire_incoming_original(
        &mut self,
        owner: NetworkStreamOwner,
        call: u64,
        sequences: [u64; 3],
        failed_request: Option<u64>,
    ) -> io::Result<()> {
        self.incoming
            .retire_original(owner, call, sequences, failed_request)
    }
    pub(super) fn retire_sent_original_ack(&mut self, sequence: u64) -> io::Result<()> {
        self.incoming.retire_sent_original_ack(sequence)
    }
    pub(super) fn retained_read_copy_ready(&self, prepared: u64, first: u64) -> io::Result<bool> {
        let entry = self
            .incoming
            .entries
            .get(&prepared)
            .ok_or_else(|| protocol("Read copy preparation missing"))?;
        let count = entry.read_copy.as_ref().map_or(0, Vec::len) as u64;
        if first > count {
            return Err(protocol("Read copy request skips retained prefix"));
        }
        Ok(first < count || entry.read_copy_end.is_some())
    }
    pub(super) fn read_copy_completed(&self, completed: u64) -> io::Result<bool> {
        Ok(self
            .incoming
            .entries
            .get(&completed)
            .ok_or_else(|| protocol("Read copy completion missing"))?
            .read_copy_finalized)
    }
    pub(super) fn pending_original_copy_completions(&self) -> Vec<u64> {
        self.incoming
            .entries
            .iter()
            .filter_map(|(sequence, entry)| {
                (entry.state == IncomingState::Retained
                    && matches!(
                        entry.envelope.operation,
                        Operation::CollectOriginalConnect
                            | Operation::CollectOriginalFileObservation
                            | Operation::TerminateOriginalConnect
                            | Operation::ReadOriginalCopy
                    ))
                .then_some(*sequence)
            })
            .collect()
    }
    pub(super) fn pending_original_selections(&self) -> Vec<u64> {
        self.incoming
            .entries
            .iter()
            .filter_map(|(sequence, entry)| {
                (entry.envelope.operation == Operation::AwaitOriginalSelection
                    && entry.state == IncomingState::Submitted)
                    .then_some(*sequence)
            })
            .collect()
    }
    /// Only the original outgoing request and its actual matching ACK. This
    /// never treats a retained incoming request as controller authority.
    pub(super) fn acknowledged_outgoing_request(
        &self,
        sequence: u64,
    ) -> io::Result<(&Envelope, &[OwnedFd], &[u8])> {
        let entry = self
            .outgoing
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown outgoing copy authority request"))?;
        let SendState::Acknowledged(bytes) = &entry.state else {
            return Err(protocol(
                "copy authority request has no actual acknowledgement",
            ));
        };
        Ok((&entry.envelope, &entry.rights, bytes))
    }

    pub(super) fn retained_request(
        &self,
        sequence: u64,
    ) -> io::Result<(&Envelope, &[OwnedFd], Option<&[u8]>)> {
        let entry = self
            .incoming
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown accepted retained request"))?;
        let outcome = match &entry.state {
            IncomingState::Completed(bytes) => Some(bytes.as_slice()),
            _ => None,
        };
        Ok((&entry.envelope, &entry.rights, outcome))
    }
    pub(super) fn retain_read_copy(
        &mut self,
        completed: u64,
        records: Vec<super::original_read_copy::Record>,
    ) -> io::Result<()> {
        self.incoming.retain_read_copy(completed, records)
    }
    pub(super) fn retain_terminal_read_copy(
        &mut self,
        prepared: u64,
        records: Vec<super::original_read_copy::Record>,
        end: super::original_read_copy::End,
    ) -> io::Result<()> {
        self.incoming
            .retain_read_copy_prefix(prepared, records, end)
    }
    pub(super) fn read_copy_chunk(
        &mut self,
        request: &Envelope,
        rights: usize,
        call: u64,
        command: u64,
        prepared: u64,
        first: u64,
        fetch: impl FnOnce() -> io::Result<Option<super::original_read_copy::Chunk>>,
    ) -> io::Result<Option<super::original_read_copy::Chunk>> {
        self.incoming
            .read_copy_chunk(request, rights, call, command, prepared, first, fetch)
    }
    pub(super) fn wait_transport(&self, deadline: Instant) -> io::Result<()> {
        self.wait_transport_with_copy(deadline, None)
    }
    pub(super) fn wait_transport_with_copy(
        &self,
        deadline: Instant,
        copy: Option<i32>,
    ) -> io::Result<()> {
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(10) as i32;
        let mut events = [
            libc::pollfd {
                fd: self.endpoint.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: copy.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(events.as_mut_ptr(), events.len() as libc::nfds_t, millis) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if events
            .iter()
            .any(|event| event.revents & (libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Err(protocol("accepted transport observation failed"));
        }
        Ok(())
    }
    /// Caller obtained this exact private endpoint through startup ownership;
    /// a pathname, Config integer, or channel EOF cannot grant this authority.
    pub(super) fn new(endpoint: OwnedFd, run: [u8; 16]) -> Result<Self, (io::Error, OwnedFd)> {
        // Bootstrap and historical controls have the strict legacy grammar.
        // Production run endpoints use the authenticated artifact explicitly.
        Self::from_wire(endpoint, run, super::ProviderWireFormat::Abi7Copy4)
    }
    pub(super) fn from_wire(
        endpoint: OwnedFd,
        run: [u8; 16],
        wire_format: super::ProviderWireFormat,
    ) -> Result<Self, (io::Error, OwnedFd)> {
        let mut kind = 0i32;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                endpoint.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut i32).cast(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err((io::Error::last_os_error(), endpoint));
        }
        if run == [0; 16]
            || kind != libc::SOCK_SEQPACKET
            || len != std::mem::size_of::<i32>() as u32
        {
            return Err((
                protocol("accepted startup endpoint is not the private seqpacket channel"),
                endpoint,
            ));
        }
        Ok(Self {
            endpoint,
            run,
            outgoing: Outbox {
                wire_format,
                ..Outbox::default()
            },
            incoming: Inbox {
                wire_format,
                ..Inbox::default()
            },
            quarantine: Vec::new(),
        })
    }
    pub(super) fn prepare(
        &mut self,
        envelope: Envelope,
        rights: Vec<OwnedFd>,
    ) -> Result<u64, (io::Error, Vec<OwnedFd>)> {
        if envelope.run != self.run {
            return Err((protocol("accepted request belongs to another run"), rights));
        }
        self.outgoing.prepare(envelope, rights)
    }
    pub(super) fn try_send(&mut self, sequence: u64) -> io::Result<bool> {
        let entry = self
            .outgoing
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown accepted outgoing request"))?;
        if entry.state != SendState::Prepared {
            return Err(protocol("accepted submission cannot be sent twice"));
        }
        let mut iov = libc::iovec {
            iov_base: entry.encoded.as_ptr().cast_mut().cast(),
            iov_len: entry.encoded.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !entry.rights.is_empty() {
            let bytes = entry.rights.len() * std::mem::size_of::<i32>();
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(bytes as u32) } as usize;
            let c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
            unsafe {
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(bytes as u32) as usize;
                let fds = libc::CMSG_DATA(c).cast::<i32>();
                for (i, fd) in entry.rights.iter().enumerate() {
                    fds.add(i).write(fd.as_raw_fd());
                }
            }
        }
        entry.state = SendState::Submitted; // durable before effect
        let n = unsafe {
            libc::sendmsg(
                self.endpoint.as_raw_fd(),
                &msg,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                entry.state = SendState::Prepared;
                return Ok(false);
            }
            return Err(error); // unknown/error stays submitted with all rights
        }
        if n as usize != entry.encoded.len() {
            return Err(protocol("partial seqpacket submission"));
        }
        Ok(true)
    }
    pub(super) fn try_receive(&mut self) -> io::Result<Option<Received>> {
        let Some(raw) = receive(self.endpoint.as_raw_fd())? else {
            return Ok(None);
        };
        // No accepted packet is empty. A rightless zero-length read means the
        // peer closed or called `end_send_direction`; its own report says why.
        if raw.bytes.is_empty() && raw.rights.is_empty() && raw.control_valid {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                PeerEndOfStream,
            ));
        }
        let parsed = decode(&raw.bytes);
        #[cfg(test)]
        eprintln!(
            "accepted_scm flags={} received_rights={}",
            raw.flags,
            raw.rights.len()
        );
        let valid = parsed
            .as_ref()
            .is_ok_and(|e| e.run == self.run && e.operation.rights() == raw.rights.len())
            && raw.control_valid
            && raw.flags == libc::MSG_CMSG_CLOEXEC;
        if !valid {
            self.quarantine.push(raw);
            return Err(protocol(
                "invalid accepted packet retained with its received rights",
            ));
        }
        let envelope = parsed.unwrap();
        let sequence = envelope.sequence;
        if envelope.operation == Operation::Reply {
            if let Err(error) = self.outgoing.acknowledge(&envelope) {
                self.quarantine.push(raw);
                return Err(error);
            }
            return Ok(Some(Received::Acknowledged(sequence)));
        }
        if let Err((error, rights)) = self.incoming.retain(envelope, raw.rights) {
            self.quarantine.push(Unclassified {
                bytes: raw.bytes,
                rights,
                flags: raw.flags,
                control_valid: raw.control_valid,
            });
            return Err(error);
        }
        // Only a checked ID escapes. All received capabilities already reside
        // in this run-owned inbox before a service callback or await is possible.
        Ok(Some(Received::Request(sequence)))
    }
    /// Stop sending after a retained failure so a waiting peer fails closed
    /// instead of awaiting a reply that will never come. The endpoint, inbox,
    /// outbox and every received right stay owned until process exit.
    pub(super) fn end_send_direction(&self) -> io::Result<()> {
        if unsafe { libc::shutdown(self.endpoint.as_raw_fd(), libc::SHUT_WR) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn dispatch(
        &mut self,
        sequence: u64,
        operation: impl FnOnce(&Envelope, &[OwnedFd]) -> io::Result<Vec<u8>>,
    ) -> io::Result<()> {
        self.incoming.dispatch(sequence, operation)
    }
    pub(super) fn response(&self, sequence: u64) -> io::Result<Option<&[u8]>> {
        let entry = self
            .outgoing
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown accepted outgoing request"))?;
        Ok(match &entry.state {
            SendState::Acknowledged(bytes) => Some(bytes),
            _ => None,
        })
    }
    pub(super) fn try_reply(&self, sequence: u64) -> io::Result<bool> {
        let entry = self
            .incoming
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown accepted incoming request"))?;
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("accepted response before provider completion"));
        };
        self.try_reply_body(entry, body)
    }

    /// Notify the parent of a failed bootstrap without completing its effect.
    /// The original Submitted entry and every received right stay untouched.
    /// This is unavailable for ordinary provider commands or a READY outcome.
    pub(super) fn try_bootstrap_failure_reply(
        &self,
        sequence: u64,
        failure: &super::accepted_parent::BootstrapFailure,
    ) -> io::Result<bool> {
        let entry = self
            .incoming
            .entries
            .get(&sequence)
            .ok_or_else(|| protocol("unknown failed accepted bootstrap"))?;
        if sequence != 1
            || entry.envelope.run != self.run
            || !matches!(
                (entry.envelope.operation, entry.rights.len()),
                (Operation::Bootstrap, 2) | (Operation::GroupedBootstrap, 3)
            )
            || entry.envelope.owner.is_some()
            || entry.envelope.accept.is_some()
            || entry.state != IncomingState::Submitted
        {
            return Err(protocol(
                "failure reply lacks original submitted bootstrap custody",
            ));
        }
        let body = serde_json::to_vec(&super::accepted_parent::BootstrapReply::Failed(
            failure.clone(),
        ))?;
        self.try_reply_body(entry, &body)
    }

    fn try_reply_body(&self, entry: &Incoming<OwnedFd>, body: &[u8]) -> io::Result<bool> {
        let mut reply = entry.envelope.clone();
        reply.operation = Operation::Reply;
        reply.body = body.to_vec();
        let bytes = encode(&reply)?;
        let n = unsafe {
            libc::send(
                self.endpoint.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(error)
            };
        }
        if n as usize != bytes.len() {
            return Err(protocol("partial accepted response"));
        }
        Ok(true)
    }
}

fn receive(endpoint: i32) -> io::Result<Option<Unclassified>> {
    let mut bytes = vec![0u8; MAX_MESSAGE];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let capacity =
        unsafe { libc::CMSG_SPACE((MAX_RIGHTS * std::mem::size_of::<i32>()) as u32) } as usize;
    let mut control = vec![0usize; capacity.div_ceil(std::mem::size_of::<usize>())];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = capacity;
    let n = unsafe {
        libc::recvmsg(
            endpoint,
            &mut msg,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(e)
        };
    }
    bytes.truncate(n as usize);
    let mut rights = Vec::new();
    let mut valid = true;
    let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !c.is_null() {
        let header = unsafe { &*c };
        let base = unsafe { libc::CMSG_LEN(0) } as usize;
        if header.cmsg_len < base {
            valid = false;
            break;
        }
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            let length = header.cmsg_len - base;
            if length % std::mem::size_of::<i32>() != 0 {
                valid = false;
            }
            let data = unsafe { libc::CMSG_DATA(c).cast::<i32>() };
            for i in 0..length / std::mem::size_of::<i32>() {
                let fd = unsafe { data.add(i).read_unaligned() };
                if fd < 0 {
                    valid = false;
                } else {
                    rights.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        } else {
            valid = false;
        }
        c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
    }
    Ok(Some(Unclassified {
        bytes,
        rights,
        flags: msg.msg_flags,
        control_valid: valid,
    }))
}

/// Service cleanup is allowed only by the retained controller capability, not
/// by peer EOF or a claimed PID. This zero-time check does not advance guest time.
pub(super) fn controller_exited(controller: BorrowedFd<'_>) -> io::Result<bool> {
    let mut event = libc::pollfd {
        fd: controller.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut event, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(protocol("controller pidfd observation failed"));
    }
    Ok(event.revents & libc::POLLIN != 0)
}

/// Typed payload of the zero-length rightless receive. Other UnexpectedEof
/// errors, such as truncated JSON, never carry it.
#[derive(Debug)]
struct PeerEndOfStream;
impl std::fmt::Display for PeerEndOfStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("accepted peer reached end-of-stream: it closed or ended its send direction")
    }
}
impl std::error::Error for PeerEndOfStream {}
/// Only `try_receive`'s actual end-of-stream observation matches.
pub(super) fn is_peer_end_of_stream(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::UnexpectedEof
        && error
            .get_ref()
            .is_some_and(|inner| inner.is::<PeerEndOfStream>())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_end_of_stream_is_typed_and_distinct_from_other_unexpected_eof() {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let peer = AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16]).unwrap();
        let mut local =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16]).unwrap();
        assert!(local.try_receive().unwrap().is_none());
        peer.end_send_direction().unwrap();
        let ended = local.try_receive().unwrap_err();
        assert!(is_peer_end_of_stream(&ended), "{ended}");
        assert!(ended.to_string().contains("end-of-stream"), "{ended}");
        // Same kind and text without the typed payload, a truncated JSON
        // body and an ordinary transport error are not peer end-of-stream.
        let text = io::Error::new(io::ErrorKind::UnexpectedEof, ended.to_string());
        let json = io::Error::from(serde_json::from_slice::<serde_json::Value>(b"{").unwrap_err());
        assert_eq!(json.kind(), io::ErrorKind::UnexpectedEof);
        for other in [text, json, io::Error::from_raw_os_error(libc::EPIPE)] {
            assert!(!is_peer_end_of_stream(&other), "{other}");
        }
    }
    #[test]
    fn original_auxiliary_ack_keeps_primary_and_never_retries_unknown_submission() {
        for operation in [
            Operation::CollectOriginalFileObservation,
            Operation::ObserveTerminalSocket,
        ] {
            let mut request = envelope();
            request.operation = operation;
            let mut inbox = Inbox::<u64>::default();
            let sequence = inbox.retain(request, vec![37]).unwrap();
            assert!(
                inbox
                    .acknowledge_command_completion(sequence, |_, _| panic!(
                        "ACK before primary retention"
                    ))
                    .is_err()
            );
            inbox
                .dispatch(sequence, |_, rights| {
                    assert_eq!(rights, &[37]);
                    Ok(b"retained actual observation".to_vec())
                })
                .unwrap();
            let mut calls = 0;
            assert!(
                inbox
                    .acknowledge_command_completion(sequence, |_, body| {
                        calls += 1;
                        assert_eq!(body, b"retained actual observation");
                        Err(protocol("unknown physical ACK result"))
                    })
                    .is_err()
            );
            assert_eq!(calls, 1);
            assert!(
                inbox
                    .acknowledge_command_completion(sequence, |_, _| panic!(
                        "unknown auxiliary ACK repeated"
                    ))
                    .is_err()
            );
            assert_eq!(inbox.entries[&sequence].rights, vec![37]);
            assert_eq!(
                inbox.entries[&sequence].state,
                IncomingState::Completed(b"retained actual observation".to_vec())
            );
        }
    }
    #[test]
    fn terminal_socket_retirement_requires_exact_call_capture_and_rights() {
        use super::super::accepted_provider::Reply;
        use super::super::accepted_provider::Request;
        let (owner, effect, capture) = super::super::terminal_socket_observation::fixture();
        let request = Envelope {
            run: [1; 16],
            sequence: 1,
            owner: Some(owner),
            accept: None,
            operation: Operation::ObserveTerminalSocket,
            body: serde_json::to_vec(&Request::ObserveTerminalSocket { call: 19, effect }).unwrap(),
        };
        let body = serde_json::to_vec(&Reply::TerminalSocketObservation {
            call: 19,
            capture: capture.clone(),
        })
        .unwrap();
        validate_terminal_socket_observation(owner, 19, 1, &request, &body, 1).unwrap();
        assert!(validate_terminal_socket_observation(owner, 20, 1, &request, &body, 1).is_err());
        assert!(validate_terminal_socket_observation(owner, 19, 1, &request, &body, 0).is_err());
        let mut failed = capture;
        failed.release.as_mut().unwrap().returned = -1;
        let failed = serde_json::to_vec(&Reply::TerminalSocketObservation {
            call: 19,
            capture: failed,
        })
        .unwrap();
        assert!(validate_terminal_socket_observation(owner, 19, 1, &request, &failed, 1).is_err());
    }

    fn command_ack_envelope() -> Envelope {
        let mut e = envelope();
        e.operation = Operation::FinishSetter;
        e
    }
    #[test]
    fn accepted_command_ack_requires_retained_primary_and_keeps_its_rights() {
        let mut inbox = Inbox::<u64>::default();
        let sequence = inbox.retain(command_ack_envelope(), vec![17, 19]).unwrap();
        assert!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| panic!("ACK before completion"))
                .is_err()
        );
        inbox
            .dispatch(sequence, |_, rights| {
                assert_eq!(rights, &[17, 19]);
                Ok(b"exact complete primary".to_vec())
            })
            .unwrap();
        let status = inbox
            .acknowledge_command_completion(sequence, |request, body| {
                assert_eq!(request.sequence, sequence);
                assert_eq!(body, b"exact complete primary");
                Ok(b"exact ACK status".to_vec())
            })
            .unwrap();
        assert_eq!(status, b"exact ACK status");
        assert_eq!(inbox.entries[&sequence].rights, vec![17, 19]);
        assert_eq!(
            inbox.entries[&sequence].state,
            IncomingState::Completed(b"exact complete primary".to_vec())
        );
        assert_eq!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| panic!(
                    "lost reply repeated ACK effect"
                ))
                .unwrap(),
            status
        );
    }
    #[test]
    fn accepted_command_ack_unknown_effect_stays_submitted_with_primary_owned() {
        let mut inbox = Inbox::<u64>::default();
        let sequence = inbox.retain(command_ack_envelope(), vec![23, 29]).unwrap();
        inbox
            .dispatch(sequence, |_, _| Ok(b"primary".to_vec()))
            .unwrap();
        assert!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| Err(protocol(
                    "unknown ACK outcome"
                )))
                .is_err()
        );
        assert_eq!(
            inbox.entries[&sequence].command_ack,
            CommandAckState::Submitted
        );
        assert!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| panic!("unknown ACK retried"))
                .is_err()
        );
        assert_eq!(
            inbox.entries[&sequence].state,
            IncomingState::Completed(b"primary".to_vec())
        );
        assert_eq!(inbox.entries[&sequence].rights, vec![23, 29]);
    }
    #[test]
    fn accepted_command_ack_known_failure_is_retained_without_success_inference() {
        let mut inbox = Inbox::<u64>::default();
        let sequence = inbox.retain(command_ack_envelope(), vec![31, 37]).unwrap();
        inbox
            .dispatch(sequence, |_, _| Ok(b"primary".to_vec()))
            .unwrap();
        let failed = b"returned=-1 errno=ESTALE".to_vec();
        assert_eq!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| Ok(failed.clone()))
                .unwrap(),
            failed
        );
        assert_eq!(
            inbox
                .acknowledge_command_completion(sequence, |_, _| panic!("known failure retried"))
                .unwrap(),
            failed
        );
        assert_eq!(inbox.entries[&sequence].rights, vec![31, 37]);
    }
    #[test]
    fn accepted_command_ack_never_retires_preparation_or_read_only_observation() {
        for operation in [Operation::PrepareSetter, Operation::DrainCreations] {
            let mut inbox = Inbox::<u64>::default();
            let mut request = envelope();
            request.operation = operation;
            let sequence = inbox.retain(request, vec![]).unwrap();
            inbox
                .dispatch(sequence, |_, _| Ok(b"not a complete command".to_vec()))
                .unwrap();
            assert!(
                inbox
                    .acknowledge_command_completion(sequence, |_, _| panic!("wrong effect ACK"))
                    .is_err()
            );
            assert_eq!(
                inbox.entries[&sequence].command_ack,
                CommandAckState::Unsubmitted
            );
        }
    }

    fn native_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
    fn native_memfd(name: &std::ffi::CStr) -> OwnedFd {
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw) }
    }
    fn native_send(endpoint: &OwnedFd, bytes: &[u8], rights: &[OwnedFd]) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let size = unsafe {
            libc::CMSG_SPACE(std::mem::size_of_val(
                rights
                    .iter()
                    .map(AsRawFd::as_raw_fd)
                    .collect::<Vec<_>>()
                    .as_slice(),
            ) as u32)
        } as usize;
        let mut control = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size;
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len =
                libc::CMSG_LEN((rights.len() * std::mem::size_of::<i32>()) as u32) as usize;
            let data = libc::CMSG_DATA(header).cast::<i32>();
            for (i, fd) in rights.iter().enumerate() {
                data.add(i).write(fd.as_raw_fd());
            }
        }
        assert_eq!(
            unsafe {
                libc::sendmsg(
                    endpoint.as_raw_fd(),
                    &message,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }
    fn native_identity(fd: &OwnedFd) -> (u64, u64) {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) }, 0);
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) },
            libc::FD_CLOEXEC
        );
        eprintln!(
            "accepted_scm fd={} device={} inode={} descriptor_flags={}",
            fd.as_raw_fd(),
            stat.st_dev,
            stat.st_ino,
            libc::FD_CLOEXEC
        );
        (stat.st_dev, stat.st_ino)
    }
    #[test]
    fn accepted_native_malformed_scm_keeps_actual_rights_after_sender_closes() {
        let (sender, receiver) = native_pair();
        let mut session = AcceptedSession::new(receiver, [1; 16]).unwrap();
        let rights = vec![native_memfd(c"accepted-a"), native_memfd(c"accepted-b")];
        let expected = rights.iter().map(native_identity).collect::<Vec<_>>();
        native_send(&sender, b"not a protocol message", &rights);
        drop(rights);
        assert!(session.try_receive().is_err());
        assert_eq!(session.quarantine.len(), 1);
        assert_eq!(
            session.quarantine[0]
                .rights
                .iter()
                .map(native_identity)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(session.quarantine[0].flags, libc::MSG_CMSG_CLOEXEC);
        assert!(session.incoming.entries.is_empty());
    }
    #[test]
    fn accepted_native_wrong_arity_and_truncated_payload_keep_every_delivered_right() {
        let (sender, receiver) = native_pair();
        let mut session = AcceptedSession::new(receiver, [1; 16]).unwrap();
        let rights = vec![
            native_memfd(c"accepted-c"),
            native_memfd(c"accepted-d"),
            native_memfd(c"accepted-e"),
        ];
        let expected = rights.iter().map(native_identity).collect::<Vec<_>>();
        let message = encode(&envelope()).unwrap();
        native_send(&sender, &message, &rights);
        assert!(session.try_receive().is_err());
        assert_eq!(
            session.quarantine[0]
                .rights
                .iter()
                .map(native_identity)
                .collect::<Vec<_>>(),
            expected
        );
        native_send(&sender, &vec![b'x'; MAX_MESSAGE + 1], &rights);
        drop(rights);
        assert!(session.try_receive().is_err());
        assert_eq!(session.quarantine.len(), 2);
        assert_eq!(
            session.quarantine[1]
                .rights
                .iter()
                .map(native_identity)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            session.quarantine[1].flags,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC
        );
        assert!(session.incoming.entries.is_empty());
    }
    fn envelope() -> Envelope {
        Envelope {
            run: [1; 16],
            sequence: 1,
            owner: None,
            accept: None,
            operation: Operation::Bootstrap,
            body: vec![4, 5],
        }
    }
    #[test]
    fn accepted_session_encoding_rejects_version_trailing_bytes_and_zero_identity() {
        let e = envelope();
        let raw = encode(&e).unwrap();
        assert_eq!(decode(&raw).unwrap(), e);
        let mut trailing = raw.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let wrong = serde_json::to_vec(&(VERSION + 1, &e)).unwrap();
        assert!(decode(&wrong).is_err());
        let mut zero = e;
        zero.run = [0; 16];
        assert!(encode(&zero).is_err());
    }
    #[test]
    fn accepted_session_ack_does_not_drop_escrow_and_must_match_exact_run_owner_lease() {
        let mut outbox = Outbox::default();
        let n = outbox.prepare(envelope(), vec![17u64, 18]).unwrap();
        let mut ack = envelope();
        ack.sequence = n;
        ack.operation = Operation::Reply;
        assert!(outbox.acknowledge(&ack).is_err());
        outbox.entries.get_mut(&n).unwrap().state = SendState::Submitted;
        let mut other = ack.clone();
        other.run = [2; 16];
        assert!(outbox.acknowledge(&other).is_err());
        other = ack.clone();
        other.accept = Some(NetworkAcceptLeaseId(3));
        assert!(outbox.acknowledge(&other).is_err());
        outbox.acknowledge(&ack).unwrap();
        outbox.acknowledge(&ack).unwrap();
        assert_eq!(outbox.entries[&n].rights, vec![17, 18]);
        ack.body.push(9);
        assert!(outbox.acknowledge(&ack).is_err());
        assert_eq!(outbox.entries[&n].rights, vec![17, 18]);
    }
    #[test]
    fn accepted_session_rejected_submission_returns_all_rights_and_does_not_advance_sequence() {
        let mut outbox = Outbox::default();
        let mut e = envelope();
        e.operation = Operation::MatchAccepted;
        let (_, rights) = outbox.prepare(e, vec![42u64]).unwrap_err();
        assert_eq!(rights, vec![42]);
        assert_eq!(outbox.next, 1);
        assert!(outbox.entries.is_empty());
        assert_eq!(outbox.prepare(envelope(), vec![7, 8]).unwrap(), 1);
    }

    #[test]
    fn accepted_incoming_effect_and_rights_survive_failed_dispatch_without_resubmission() {
        let mut inbox = Inbox::default();
        let sequence = inbox.retain(envelope(), vec![3u64, 4]).unwrap();
        assert!(
            inbox
                .dispatch(sequence, |_, rights| {
                    assert_eq!(rights, &[3, 4]);
                    Err(protocol("uncertain provider effect"))
                })
                .is_err()
        );
        assert_eq!(inbox.entries[&sequence].state, IncomingState::Submitted);
        assert_eq!(inbox.entries[&sequence].rights, vec![3, 4]);
        assert!(
            inbox
                .dispatch(sequence, |_, _| panic!("must not repeat provider effect"))
                .is_err()
        );
        let (_, rights) = inbox.retain(envelope(), vec![5u64, 6]).unwrap_err();
        assert_eq!(rights, vec![5, 6]);
        assert_eq!(inbox.entries[&sequence].rights, vec![3, 4]);
    }
    fn observation_envelope(sequence: u64) -> Envelope {
        let mut e = envelope();
        e.sequence = sequence;
        e.operation = Operation::DrainCreations;
        e.body = serde_json::to_vec(&super::super::accepted_provider::Request::AwaitCreation {
            sequence: sequence as u32,
            acknowledged: None,
        })
        .unwrap();
        e
    }
    #[test]
    fn accepted_observer_transport_pending_keeps_one_request_and_allows_other_effects() {
        let mut inbox = Inbox::<u64>::default();
        inbox.retain(observation_envelope(1), vec![]).unwrap();
        inbox.begin_observation(1).unwrap();
        for _ in 0..512 {
            assert_eq!(inbox.entries.len(), 1);
            assert_eq!(inbox.entries[&1].state, IncomingState::Submitted);
            assert!(inbox.begin_observation(1).is_err());
            assert!(
                inbox
                    .retire_observation(&receipt(&observation_envelope(1), b"not completed"))
                    .is_err()
            );
        }
        let mut other = envelope();
        other.sequence = 2;
        inbox.retain(other, vec![17, 18]).unwrap();
        inbox
            .dispatch(2, |_, rights| {
                assert_eq!(rights, &[17, 18]);
                Ok(b"setter completed".to_vec())
            })
            .unwrap();
        assert_eq!(inbox.entries[&1].state, IncomingState::Submitted);
        assert_eq!(
            inbox.entries[&2].state,
            IncomingState::Completed(b"setter completed".to_vec())
        );
        inbox.finish_observation(1, b"queued".to_vec()).unwrap();
        assert!(inbox.finish_observation(1, b"different".to_vec()).is_err());
        assert_eq!(inbox.entries[&2].rights, vec![17, 18]);
    }
    #[test]
    fn accepted_observer_transport_exact_retirement_bounds_both_queues_and_rejects_late_ack() {
        let mut outbox = Outbox::<u64>::default();
        let mut inbox = Inbox::<u64>::default();
        let mut previous = None;
        for n in 1..=512 {
            let e = observation_envelope(n);
            if let Some(old) = previous.take() {
                inbox.retire_observation(&old).unwrap();
            }
            let id = outbox.prepare(e.clone(), vec![]).unwrap();
            assert_eq!(id, n);
            outbox.entries.get_mut(&id).unwrap().state = SendState::Submitted;
            inbox.retain(e.clone(), vec![]).unwrap();
            inbox.begin_observation(id).unwrap();
            assert!(outbox.retire_observation(id, b"queued").is_err());
            inbox.finish_observation(id, b"queued".to_vec()).unwrap();
            let mut ack = e.clone();
            ack.operation = Operation::Reply;
            ack.body = b"queued".to_vec();
            outbox.acknowledge(&ack).unwrap();
            // Lost local waiter: the one response is recoverable, no resubmit.
            outbox.acknowledge(&ack).unwrap();
            assert!(outbox.retire_observation(id, b"changed").is_err());
            let retired = outbox.retire_observation(id, b"queued").unwrap();
            assert!(outbox.acknowledge(&ack).is_err());
            assert!(inbox.retain(e, vec![]).is_err());
            assert!(outbox.entries.is_empty());
            assert_eq!(inbox.entries.len(), 1);
            previous = Some(retired);
        }
        // Exact terminal ACK frees the last observation without a future child.
        let last = previous.unwrap();
        inbox.retire_observation(&last).unwrap();
        inbox.retire_observation(&last).unwrap();
        assert!(inbox.entries.is_empty());
        assert_eq!(inbox.next, 513);
        assert_eq!(outbox.next, 513);
        let mut changed = last.clone();
        changed.body.push(1);
        assert!(inbox.retire_observation(&changed).is_err());
        changed = last;
        changed.sequence -= 1;
        assert!(inbox.retire_observation(&changed).is_err());
    }
    #[test]
    fn accepted_observer_transport_retirement_cannot_change_owner_or_release_rights() {
        let mut inbox = Inbox::<u64>::default();
        let mut e = observation_envelope(1);
        let tid = crate::types::DetTid::from_raw(31);
        e.owner = Some(NetworkStreamOwner {
            thread: tid,
            mm: crate::types::MmId::initial(tid),
        });
        inbox.retain(e.clone(), vec![]).unwrap();
        inbox.begin_observation(1).unwrap();
        inbox.finish_observation(1, b"queued".to_vec()).unwrap();
        let exact = receipt(&e, b"queued");
        let mut changed = exact.clone();
        changed.owner = None;
        assert!(inbox.retire_observation(&changed).is_err());
        changed = exact.clone();
        changed.accept = Some(NetworkAcceptLeaseId(42));
        assert!(inbox.retire_observation(&changed).is_err());
        assert_eq!(inbox.entries.len(), 1);
        inbox.retire_observation(&exact).unwrap();
        let mut effect = envelope();
        effect.sequence = 2;
        inbox.retain(effect.clone(), vec![71, 72]).unwrap();
        inbox.dispatch(2, |_, _| Ok(b"queued".to_vec())).unwrap();
        assert!(
            inbox
                .retire_observation(&receipt(&effect, b"queued"))
                .is_err()
        );
        assert_eq!(inbox.entries[&2].rights, vec![71, 72]);
    }
}

#[cfg(test)]
mod fd_observation_tests {
    use super::*;
    fn request(sequence: u64, fd: bool) -> Envelope {
        let thread = crate::types::DetTid::from_raw(31);
        let request = if fd {
            super::super::accepted_provider::Request::AwaitFdEvent {
                sequence: 1,
                acknowledged: None,
            }
        } else {
            super::super::accepted_provider::Request::AwaitCreation {
                sequence: 1,
                acknowledged: None,
            }
        };
        Envelope {
            run: [1; 16],
            sequence,
            owner: Some(NetworkStreamOwner {
                thread,
                mm: crate::types::MmId::initial(thread),
            }),
            accept: None,
            operation: if fd {
                Operation::DrainFdJournal
            } else {
                Operation::DrainCreations
            },
            body: serde_json::to_vec(&request).unwrap(),
        }
    }
    #[test]
    fn fd_and_creation_retirements_cannot_replace_each_other() {
        let mut inbox = Inbox::<u64>::default();
        let a = request(1, true);
        let b = request(2, false);
        for e in [&a, &b] {
            inbox.retain(e.clone(), vec![]).unwrap();
            inbox.begin_observation(e.sequence).unwrap();
            inbox
                .finish_observation(e.sequence, b"raw".to_vec())
                .unwrap();
        }
        let ar = receipt(&a, b"raw");
        let br = receipt(&b, b"raw");
        assert!(inbox.retire_observation(&ar).is_err());
        inbox
            .acknowledge_command_completion(1, |_, _| Ok(b"known ACK".to_vec()))
            .unwrap();
        let mut forged = ar.clone();
        forged.fd_journal = false;
        assert!(inbox.retire_observation(&forged).is_err());
        inbox.retire_observation(&br).unwrap();
        inbox.retire_observation(&ar).unwrap();
        inbox.retire_observation(&ar).unwrap();
        inbox.retire_observation(&br).unwrap();
        assert!(inbox.entries.is_empty());
        forged.body = b"changed".to_vec();
        assert!(inbox.retire_observation(&forged).is_err());
    }
    #[test]
    fn fd_physical_ack_requires_owned_raw_reply_and_never_repeats_unknown_effect() {
        let mut inbox = Inbox::<u64>::default();
        inbox.retain(request(1, true), vec![]).unwrap();
        inbox.begin_observation(1).unwrap();
        assert!(
            inbox
                .acknowledge_command_completion(1, |_, _| panic!("ACK of incomplete row"))
                .is_err()
        );
        inbox
            .finish_observation(1, b"complete raw event".to_vec())
            .unwrap();
        assert!(
            inbox
                .acknowledge_command_completion(1, |_, body| {
                    assert_eq!(body, b"complete raw event");
                    Err(protocol("unknown C ACK"))
                })
                .is_err()
        );
        assert!(
            inbox
                .acknowledge_command_completion(1, |_, _| panic!("unknown C ACK repeated"))
                .is_err()
        );
        assert_eq!(
            inbox.entries[&1].state,
            IncomingState::Completed(b"complete raw event".to_vec())
        );
        assert!(matches!(
            inbox.entries[&1].command_ack,
            CommandAckState::Submitted
        ));
        assert!(
            inbox
                .retire_observation(&receipt(&request(1, true), b"complete raw event"))
                .is_err()
        );
    }
}

#[cfg(test)]
mod original_connect_tests {
    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::Reply;
    use crate::network_runtime::accepted_provider::Request;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn status(name: &str) -> CallStatus {
        CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        }
    }
    fn group(
        first: u64,
        call: u64,
        canceled: bool,
    ) -> (NetworkStreamOwner, Vec<(Envelope, Vec<u8>, usize)>) {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let command = call + 17;
        let prepare = Request::PrepareOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Connect,
            call,
            mm: owner.mm.generation(),
            fd: 7,
            address: 0x2000,
            length: 16,
            original_count: 0,
        };
        let selected = Request::AwaitOriginalSelection {
            call,
            command,
            prepared_request: first,
        };
        let selection = ffi::OriginalSelection {
            command,
            call,
            owner_mm: owner.mm.generation(),
            provider: 3,
            task: 61,
            task_start: 99,
            table: 5,
            file: 7,
            user_address: 0x2000,
            fdput_flags: 1,
            ready: 1,
            requested_fd: 7,
            address_length: 16,
            original_count: 0,
        };
        let final_request = if canceled {
            Request::CancelOriginalConnect {
                call,
                command,
                prepared_request: first,
                selected_request: first + 1,
            }
        } else {
            Request::CollectOriginalConnect {
                kind: crate::network_replay::original_connect::Kind::Connect,
                call,
                command,
                prepared_request: first,
            }
        };
        let disarmed = Reply::OriginalCanceled {
            command,
            status: status("ap_cancel_uninvoked_original"),
        };
        let selected_reply = if canceled {
            disarmed.clone()
        } else {
            Reply::OriginalSelection(Observation {
                status: status("ap_read_original_selection"),
                raw: selection.into(),
            })
        };
        let final_reply = if canceled {
            disarmed
        } else {
            Reply::OriginalEffect(Observation {
                status: status("ap_collect_original_connect"),
                raw: ffi::OriginalEffect {
                    command: ffi::CommandResult {
                        command,
                        operation: 7,
                        phase: 1,
                        task: 61,
                        start_boottime: 99,
                        ..Default::default()
                    },
                    original: ffi::OriginalResult {
                        selection,
                        copy_entered: 1,
                        copy_returned: 1,
                        security_entered: 1,
                        security_returned: 1,
                        complete: 1,
                        ..Default::default()
                    },
                }
                .into(),
            })
        };
        let requests = [
            (
                Operation::PrepareOriginalConnect,
                prepare,
                Reply::Prepared(Observation {
                    status: status("ap_prepare_original_connect"),
                    raw: command,
                }),
            ),
            (Operation::AwaitOriginalSelection, selected, selected_reply),
            (
                if canceled {
                    Operation::CancelOriginalConnect
                } else {
                    Operation::CollectOriginalConnect
                },
                final_request,
                final_reply,
            ),
        ];
        let rows = requests
            .into_iter()
            .enumerate()
            .map(|(i, (operation, request, reply))| {
                (
                    Envelope {
                        run: [7; 16],
                        sequence: first + i as u64,
                        owner: Some(owner),
                        accept: None,
                        operation,
                        body: serde_json::to_vec(&request).unwrap(),
                    },
                    serde_json::to_vec(&reply).unwrap(),
                    usize::from(i == 0),
                )
            })
            .collect();
        (owner, rows)
    }
    #[test]
    fn socket_held_observation_uses_bounded_retained_response_without_second_dispatch() {
        let (_, rows) = group(1, 9, false);
        let mut request = rows[2].0.clone();
        request.sequence = 1;
        request.body = serde_json::to_vec(&Request::CollectOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Socket,
            call: 9,
            command: 71,
            prepared_request: 1,
        })
        .unwrap();
        let (mut effect, capture) = crate::network_runtime::installation_observation::fixture();
        effect.socket = Some(capture);
        let body = serde_json::to_vec(&Reply::OriginalEffect(Observation {
            status: status("ap_collect_original_connect"),
            raw: effect,
        }))
        .unwrap();
        let mut reply = request.clone();
        reply.operation = Operation::Reply;
        reply.body = body.clone();
        let bytes = encode(&reply).unwrap();
        assert!(bytes.len() <= MAX_MESSAGE);
        assert_eq!(decode(&bytes).unwrap(), reply);
        let mut inbox = Inbox::<u64>::default();
        inbox.retain(request, vec![]).unwrap();
        let mut captures = 0;
        inbox
            .dispatch(1, |_, _| {
                captures += 1;
                Ok(body.clone())
            })
            .unwrap();
        assert!(
            inbox
                .dispatch(1, |_, _| {
                    captures += 1;
                    Ok(body.clone())
                })
                .is_err()
        );
        assert_eq!(captures, 1);
        assert_eq!(inbox.entries[&1].state, IncomingState::Completed(body));
        assert_eq!(inbox.entries[&1].command_ack, CommandAckState::Unsubmitted);
    }
    #[test]
    fn original_transport_rejects_changed_owner_ticket_and_cancellation_domain_without_dropping_custody()
     {
        for canceled in [false, true] {
            let (owner, rows) = group(1, 9, canceled);
            let check = |rows: &Vec<(Envelope, Vec<u8>, usize)>| {
                validate_original_group(
                    owner,
                    9,
                    [1, 2, 3],
                    &rows
                        .iter()
                        .map(|(e, b, n)| (e, b.as_slice(), *n))
                        .collect::<Vec<_>>(),
                )
            };
            check(&rows).unwrap();
            let mut wrong = rows.clone();
            wrong[1].0.owner = None;
            assert!(check(&wrong).is_err());
            wrong = rows.clone();
            wrong[1].0.run = [8; 16];
            assert!(check(&wrong).is_err());
            wrong = rows.clone();
            wrong[0].2 = 0;
            assert!(check(&wrong).is_err());
            wrong = rows.clone();
            wrong[2].2 = 1;
            assert!(check(&wrong).is_err());
            wrong = rows.clone();
            wrong[1].0.body = serde_json::to_vec(&Request::AwaitOriginalSelection {
                call: 9,
                command: 27,
                prepared_request: 1,
            })
            .unwrap();
            assert!(check(&wrong).is_err());
            wrong = rows.clone();
            wrong[2].1 = serde_json::to_vec(&Reply::OriginalCanceled {
                command: 26,
                status: status("different_cleanup"),
            })
            .unwrap();
            assert!(check(&wrong).is_err());
            assert_eq!(rows[0].2, 1); // exact preparation capability remains owned
        }
    }
    #[test]
    fn original_transport_has_finite_positive_and_uninvoked_retirement_with_no_payload_tombstones()
    {
        let mut inbox = Inbox::<u64>::default();
        let mut outbox = Outbox::<u64>::default();
        for call in 1..=256 {
            let first = (call - 1) * 4 + 1;
            let canceled = call % 2 == 0;
            let (owner, rows) = group(first, call, canceled);
            for (i, (envelope, body, rights)) in rows.iter().enumerate() {
                let pins = if *rights == 1 { vec![call] } else { vec![] };
                outbox.prepare(envelope.clone(), pins.clone()).unwrap();
                outbox.entries.get_mut(&envelope.sequence).unwrap().state = SendState::Submitted;
                inbox.retain(envelope.clone(), pins).unwrap();
                if i == 1 {
                    inbox.begin_observation(envelope.sequence).unwrap();
                    assert!(
                        inbox
                            .retire_original(owner, call, [first, first + 1, first + 2], None)
                            .is_err()
                    );
                    inbox
                        .finish_observation(envelope.sequence, body.clone())
                        .unwrap();
                } else {
                    inbox
                        .dispatch(envelope.sequence, |_, _| Ok(body.clone()))
                        .unwrap();
                }
                let mut reply = envelope.clone();
                reply.operation = Operation::Reply;
                reply.body = body.clone();
                outbox.acknowledge(&reply).unwrap();
            }
            if !canceled {
                assert!(
                    inbox
                        .retire_original(owner, call, [first, first + 1, first + 2], None)
                        .is_err()
                );
                assert_eq!(inbox.entries[&first].rights, vec![call]);
                inbox
                    .acknowledge_command_completion(first + 2, |_, _| {
                        Ok(serde_json::to_vec(
                            &serde_json::json!({"Observed":status("ap_ack_command")}),
                        )
                        .unwrap())
                    })
                    .unwrap();
            }
            let retire = Envelope {
                run: [7; 16],
                sequence: first + 3,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireOriginalConnect,
                body: serde_json::to_vec(&Request::RetireOriginalConnect {
                    failed_request: None,
                    call,
                    prepared: first,
                    selected: first + 1,
                    completed: first + 2,
                })
                .unwrap(),
            };
            outbox.prepare(retire.clone(), vec![]).unwrap();
            outbox.entries.get_mut(&(first + 3)).unwrap().state = SendState::Submitted;
            inbox.retain(retire.clone(), vec![]).unwrap();
            inbox
                .retire_original(owner, call, [first, first + 1, first + 2], None)
                .unwrap();
            let body = serde_json::to_vec(&Reply::Retired).unwrap();
            inbox.dispatch(first + 3, |_, _| Ok(body.clone())).unwrap();
            let mut reply = retire.clone();
            reply.operation = Operation::Reply;
            reply.body = body;
            outbox.acknowledge(&reply).unwrap();
            outbox
                .retire_original(owner, call, [first, first + 1, first + 2, first + 3], None)
                .unwrap();
            inbox.retire_sent_original_ack(first + 3).unwrap();
            assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
            assert!(inbox.retired_observation.is_none() && inbox.retired_fd_observation.is_none());
            assert!(inbox.retain(retire, vec![]).is_err());
        }
        assert_eq!(inbox.next, 1025);
        assert_eq!(outbox.next, 1025);
    }
    #[test]
    fn dead_original_retirement_consumes_exact_pending_query_and_optional_failed_frame() {
        for failed in [false, true] {
            let call = 9;
            let (owner, mut rows) = group(1, call, false);
            let terminal_sequence = if failed { 4 } else { 3 };
            let terminal = Reply::OriginalTerminated(Observation {
                status: status("ap_retire_dead_original"),
                raw: ffi::OriginalTerminal {
                    command: ffi::CommandResult {
                        command: 26,
                        operation: 7,
                        phase: 2,
                        ..Default::default()
                    },
                    call,
                    task_absent: 1,
                    ..Default::default()
                }
                .into(),
            });
            let terminal_body = serde_json::to_vec(&terminal).unwrap();
            rows[1].1 = terminal_body.clone();
            let mut dead = rows[2].clone();
            dead.0.sequence = terminal_sequence;
            dead.0.operation = Operation::TerminateOriginalConnect;
            dead.0.body = serde_json::to_vec(&Request::TerminateOriginalConnect {
                call,
                command: 26,
                prepared_request: 1,
                selected_request: 2,
                failed_request: failed.then_some(3),
            })
            .unwrap();
            dead.1 = terminal_body;
            if failed {
                let Reply::OriginalEffect(mut out) = serde_json::from_slice(&rows[2].1).unwrap()
                else {
                    panic!("original effect")
                };
                out.status.returned = -1;
                out.status.errno = Some(libc::ENODATA);
                rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(out)).unwrap();
                rows.push(dead);
            } else {
                rows[2] = dead;
            }
            let mut inbox = Inbox::<u64>::default();
            let mut outbox = Outbox::<u64>::default();
            for (envelope, body, rights) in &rows {
                let pins = if *rights == 1 { vec![call] } else { vec![] };
                outbox.prepare(envelope.clone(), pins.clone()).unwrap();
                outbox.entries.get_mut(&envelope.sequence).unwrap().state = SendState::Submitted;
                inbox.retain(envelope.clone(), pins).unwrap();
                if envelope.sequence == 2 {
                    inbox.begin_observation(2).unwrap();
                    continue;
                }
                inbox
                    .dispatch(envelope.sequence, |_, _| Ok(body.clone()))
                    .unwrap();
                if envelope.sequence == 3 && failed {
                    inbox
                        .acknowledge_command_completion(3, |_, _| {
                            Ok(serde_json::to_vec("NotCollected").unwrap())
                        })
                        .unwrap();
                }
                let mut reply = envelope.clone();
                reply.operation = Operation::Reply;
                reply.body = body.clone();
                outbox.acknowledge(&reply).unwrap();
            }
            // No final receipt can retire a still-unanswered early query.
            assert!(
                inbox
                    .retire_original(owner, call, [1, 2, terminal_sequence], failed.then_some(3))
                    .is_err()
            );
            assert_eq!(inbox.entries[&1].rights, vec![call]);
            inbox.finish_observation(2, rows[1].1.clone()).unwrap();
            let mut query_reply = rows[1].0.clone();
            query_reply.operation = Operation::Reply;
            query_reply.body = rows[1].1.clone();
            outbox.acknowledge(&query_reply).unwrap();
            if failed {
                assert!(
                    inbox
                        .retire_original(owner, call, [1, 2, terminal_sequence], None)
                        .is_err()
                );
                assert!(
                    inbox
                        .retire_original(owner, call, [1, 2, terminal_sequence], Some(2))
                        .is_err()
                );
                assert!(
                    matches!(serde_json::from_slice::<Reply>(match &inbox.entries[&3].state{IncomingState::Completed(body)=>body,_=>panic!("failed frame missing")}).unwrap(),Reply::OriginalEffect(out) if out.status.returned==-1)
                );
            }
            let retirement = terminal_sequence + 1;
            let envelope = Envelope {
                run: [7; 16],
                sequence: retirement,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireOriginalConnect,
                body: serde_json::to_vec(&Request::RetireOriginalConnect {
                    call,
                    prepared: 1,
                    selected: 2,
                    completed: terminal_sequence,
                    failed_request: failed.then_some(3),
                })
                .unwrap(),
            };
            outbox.prepare(envelope.clone(), vec![]).unwrap();
            outbox.entries.get_mut(&retirement).unwrap().state = SendState::Submitted;
            inbox.retain(envelope.clone(), vec![]).unwrap();
            inbox
                .retire_original(owner, call, [1, 2, terminal_sequence], failed.then_some(3))
                .unwrap();
            let body = serde_json::to_vec(&Reply::Retired).unwrap();
            inbox.dispatch(retirement, |_, _| Ok(body.clone())).unwrap();
            let mut reply = envelope.clone();
            reply.operation = Operation::Reply;
            reply.body = body;
            outbox.acknowledge(&reply).unwrap();
            outbox
                .retire_original(
                    owner,
                    call,
                    [1, 2, terminal_sequence, retirement],
                    failed.then_some(3),
                )
                .unwrap();
            inbox.retire_sent_original_ack(retirement).unwrap();
            assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
            assert!(inbox.retired_observation.is_none() && inbox.retired_fd_observation.is_none());
            assert!(inbox.retain(envelope, vec![]).is_err());
        }
    }
    #[test]
    fn close_transport_keeps_kind_bound_to_preparation_selection_and_final_effect() {
        use crate::network_replay::original_connect::Kind;
        let (owner, mut rows) = group(1, 9, false);
        // Change a complete fixture to the Close wire contract, then require
        // each exact group join. This does not claim a native observation.
        rows[0].0.body = serde_json::to_vec(&Request::PrepareOriginalConnect {
            kind: Kind::Close,
            call: 9,
            mm: owner.mm.generation(),
            fd: 7,
            address: 0,
            length: 0,
            original_count: 0,
        })
        .unwrap();
        rows[2].0.body = serde_json::to_vec(&Request::CollectOriginalConnect {
            kind: Kind::Close,
            call: 9,
            command: 26,
            prepared_request: 1,
        })
        .unwrap();
        let Reply::OriginalSelection(mut observed) = serde_json::from_slice(&rows[1].1).unwrap()
        else {
            unreachable!()
        };
        observed.raw.user_address = 0;
        observed.raw.address_length = 0;
        observed.raw.fdput_flags = 0;
        rows[1].1 = serde_json::to_vec(&Reply::OriginalSelection(observed.clone())).unwrap();
        let Reply::OriginalEffect(mut final_effect) = serde_json::from_slice(&rows[2].1).unwrap()
        else {
            unreachable!()
        };
        final_effect.raw.command.operation = 9;
        final_effect.raw.command.returned = -libc::EINTR;
        final_effect.raw.original = ffi::OriginalResult {
            selection: ffi::OriginalSelection {
                command: 26,
                call: 9,
                owner_mm: owner.mm.generation(),
                provider: 3,
                task: 61,
                task_start: 99,
                table: 5,
                file: 7,
                requested_fd: 7,
                ready: 1,
                ..Default::default()
            },
            returned: -libc::EINTR,
            complete: 1,
            ..Default::default()
        }
        .into();
        rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(final_effect)).unwrap();
        let check = |rows: &Vec<(Envelope, Vec<u8>, usize)>| {
            validate_original_group(
                owner,
                9,
                [1, 2, 3],
                &rows
                    .iter()
                    .map(|(e, b, n)| (e, b.as_slice(), *n))
                    .collect::<Vec<_>>(),
            )
        };
        check(&rows).unwrap();
        for wrong_part in 0..4 {
            let mut wrong = rows.clone();
            match wrong_part {
                0 => {
                    let mut r: Request = serde_json::from_slice(&wrong[0].0.body).unwrap();
                    if let Request::PrepareOriginalConnect { kind, .. } = &mut r {
                        *kind = Kind::Connect;
                    }
                    wrong[0].0.body = serde_json::to_vec(&r).unwrap();
                }
                1 => {
                    let mut r: Request = serde_json::from_slice(&wrong[2].0.body).unwrap();
                    if let Request::CollectOriginalConnect { kind, .. } = &mut r {
                        *kind = Kind::Connect;
                    }
                    wrong[2].0.body = serde_json::to_vec(&r).unwrap();
                }
                2 => {
                    let Reply::OriginalEffect(mut r) = serde_json::from_slice(&wrong[2].1).unwrap()
                    else {
                        unreachable!()
                    };
                    r.raw.command.operation = 7;
                    wrong[2].1 = serde_json::to_vec(&Reply::OriginalEffect(r)).unwrap();
                }
                3 => {
                    let Reply::OriginalSelection(mut r) =
                        serde_json::from_slice(&wrong[1].1).unwrap()
                    else {
                        unreachable!()
                    };
                    r.raw.requested_fd += 1;
                    wrong[1].1 = serde_json::to_vec(&Reply::OriginalSelection(r)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(check(&wrong).is_err(), "mismatched group part {wrong_part}");
            assert_eq!(wrong[0].2, 1);
        }
    }
    mod control_selection_tests {
        use super::*;
        use crate::network_replay::original_connect::Kind;
        use crate::network_runtime::accepted_provider::OriginalEffect;
        use crate::network_runtime::accepted_provider::OriginalResult;

        fn check(owner: NetworkStreamOwner, rows: &[(Envelope, Vec<u8>, usize)]) -> io::Result<()> {
            validate_original_group(
                owner,
                9,
                [1, 2, 3],
                &rows
                    .iter()
                    .map(|(e, b, n)| (e, b.as_slice(), *n))
                    .collect::<Vec<_>>(),
            )
        }
        fn set(raw: &mut OriginalResult, at: usize, value: u64) {
            raw.address[at..at + 8].copy_from_slice(&value.to_ne_bytes());
        }
        fn selected(rows: &[(Envelope, Vec<u8>, usize)]) -> Observation<OriginalResult> {
            let Reply::OriginalControlSelection(s) = serde_json::from_slice(&rows[1].1).unwrap()
            else {
                panic!("lost control selections")
            };
            s
        }
        fn completed(rows: &[(Envelope, Vec<u8>, usize)]) -> Observation<OriginalEffect> {
            let Reply::OriginalEffect(f) = serde_json::from_slice(&rows[2].1).unwrap() else {
                panic!("lost control completion")
            };
            f
        }
        fn pair() -> (NetworkStreamOwner, Vec<(Envelope, Vec<u8>, usize)>) {
            let (owner, mut rows) = group(1, 9, false);
            rows[0].0.body = serde_json::to_vec(&Request::PrepareOriginalConnect {
                kind: Kind::EpollCtl,
                call: 9,
                mm: owner.mm.generation(),
                fd: 7,
                address: 0x2000,
                length: libc::EPOLL_CTL_ADD,
                original_count: 4,
            })
            .unwrap();
            rows[0].1 = serde_json::to_vec(&Reply::Prepared(Observation {
                status: status("ap_prepare_original_epoll_ctl"),
                raw: 26,
            }))
            .unwrap();
            rows[2].0.body = serde_json::to_vec(&Request::CollectOriginalConnect {
                kind: Kind::EpollCtl,
                call: 9,
                command: 26,
                prepared_request: 1,
            })
            .unwrap();
            let mut early: OriginalResult = ffi::OriginalResult::default().into();
            early.selection = ffi::OriginalSelection {
                command: 26,
                call: 9,
                owner_mm: owner.mm.generation(),
                provider: 3,
                task: 61,
                task_start: 99,
                table: 5,
                file: 7,
                user_address: 0x2000,
                fdput_flags: 1,
                ready: 1,
                requested_fd: 7,
                address_length: libc::EPOLL_CTL_ADD,
                original_count: 4,
            }
            .into();
            for (at, value) in [
                (0, 1),
                (8, 1),
                (16, 1),
                (24, 1),
                (32, 13),
                (40, 1),
                (48, 2),
                (56, 3),
                (64, 5),
                (72, 1),
            ] {
                set(&mut early, at, value);
            }
            early.address[80..84].copy_from_slice(&(libc::EPOLLIN as u32).to_ne_bytes());
            early.address[84..92].copy_from_slice(&0xfeed_beef_1234_5678u64.to_ne_bytes());
            let mut final_effect: OriginalEffect = ffi::OriginalEffect::default().into();
            final_effect.original = early.clone();
            final_effect.original.complete = 1;
            set(&mut final_effect.original, 96, 1);
            let c = &mut final_effect.command;
            c.command = 26;
            c.operation = 20;
            c.phase = 1;
            c.task = 61;
            c.start_boottime = 99;
            c.identity.provider = 3;
            c.original_count = 4;
            rows[1].1 = serde_json::to_vec(&Reply::OriginalControlSelection(Observation {
                status: status("ap_read_original_epoll_ctl_selection"),
                raw: early,
            }))
            .unwrap();
            rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(Observation {
                status: status("ap_collect_original_connect"),
                raw: final_effect,
            }))
            .unwrap();
            (owner, rows)
        }
        #[test]
        fn original_control_transport_preserves_both_files_event_and_ack_custody() {
            let (owner, rows) = pair();
            check(owner, &rows).unwrap();
            let mut inbox = Inbox::<u64>::default();
            for (envelope, body, rights) in &rows {
                inbox
                    .retain(
                        envelope.clone(),
                        if *rights == 1 { vec![77] } else { vec![] },
                    )
                    .unwrap();
                inbox
                    .dispatch(envelope.sequence, |_, _| Ok(body.clone()))
                    .unwrap();
            }
            // The complete two-file result cannot drop the original pinned
            // actor until the existing provider command ACK is positive.
            assert!(inbox.retire_original(owner, 9, [1, 2, 3], None).is_err());
            assert_eq!(inbox.entries.len(), 3);
            assert_eq!(inbox.entries[&1].rights, [77]);
            inbox
                .acknowledge_command_completion(3, |_, _| {
                    Ok(serde_json::to_vec(&serde_json::json!({"Observed": {
                        "operation":"ap_ack_command", "returned":0, "errno":null
                    }}))
                    .unwrap())
                })
                .unwrap();
            inbox.retire_original(owner, 9, [1, 2, 3], None).unwrap();
            assert!(inbox.entries.is_empty());
        }
        #[test]
        fn original_control_transport_rejects_changed_pair_prefix_and_one_file_downgrade() {
            let (owner, rows) = pair();
            for at in 0..96 {
                let mut wrong = rows.clone();
                let mut early = selected(&wrong);
                early.raw.address[at] ^= 1;
                wrong[1].1 = serde_json::to_vec(&Reply::OriginalControlSelection(early)).unwrap();
                assert!(check(owner, &wrong).is_err(), "changed retained byte {at}");
            }
            let mut wrong = rows.clone();
            let early = selected(&wrong);
            wrong[1].1 = serde_json::to_vec(&Reply::OriginalSelection(Observation {
                status: status("ap_read_original_selection"),
                raw: early.raw.selection,
            }))
            .unwrap();
            assert!(check(owner, &wrong).is_err());
            // A complete prior observation cannot silently acquire a different
            // native errno when final collection arrives.
            let mut wrong = rows.clone();
            let mut early = selected(&wrong);
            early.raw = completed(&wrong).raw.original;
            early.raw.returned = -libc::EEXIST;
            early.raw.address[104..108].copy_from_slice(&(-libc::EEXIST).to_ne_bytes());
            wrong[1].1 = serde_json::to_vec(&Reply::OriginalControlSelection(early)).unwrap();
            assert!(check(owner, &wrong).is_err());
            // Preserve the original integer syscall operands, including a
            // negative target FD represented by its exact low 32 bits.
            let mut bad_count = rows.clone();
            let mut prepare: Request = serde_json::from_slice(&bad_count[0].0.body).unwrap();
            if let Request::PrepareOriginalConnect { original_count, .. } = &mut prepare {
                *original_count |= 1u64 << 32;
            }
            bad_count[0].0.body = serde_json::to_vec(&prepare).unwrap();
            assert!(check(owner, &bad_count).is_err());
        }
        #[test]
        fn original_control_transport_distinguishes_copy_fault_and_unreached_target() {
            let (owner, rows) = pair();
            for copy_fault in [false, true] {
                let mut fixture = rows.clone();
                let mut early = selected(&fixture);
                let mut final_effect = completed(&fixture);
                early.raw.selection.file = 0;
                early.raw.selection.fdput_flags = 0;
                for at in [24, 32, 40, 64] {
                    set(&mut early.raw, at, 0);
                }
                let returned = if copy_fault {
                    -libc::EFAULT
                } else {
                    -libc::EBADF
                };
                if copy_fault {
                    early.raw.address.fill(0);
                    for (at, value) in [(0, 1), (48, 2), (72, 1)] {
                        set(&mut early.raw, at, value);
                    }
                    early.raw.complete = 1;
                    early.raw.returned = returned;
                }
                final_effect.raw.original = early.raw.clone();
                final_effect.raw.original.complete = 1;
                final_effect.raw.original.returned = returned;
                final_effect.raw.command.returned = returned;
                if !copy_fault {
                    set(&mut final_effect.raw.original, 96, 1);
                    final_effect.raw.original.address[104..108]
                        .copy_from_slice(&returned.to_ne_bytes());
                }
                fixture[1].1 = serde_json::to_vec(&Reply::OriginalControlSelection(early)).unwrap();
                fixture[2].1 = serde_json::to_vec(&Reply::OriginalEffect(final_effect)).unwrap();
                check(owner, &fixture).unwrap();
                let mut invented = selected(&fixture);
                set(&mut invented.raw, 24, 1);
                fixture[1].1 =
                    serde_json::to_vec(&Reply::OriginalControlSelection(invented)).unwrap();
                assert!(check(owner, &fixture).is_err());
            }
        }
        #[test]
        fn original_control_terminal_retirement_preserves_partial_facts_without_completion() {
            let (owner, mut rows) = pair();
            let early = selected(&rows);
            let complete = completed(&rows);
            let mut terminal: super::super::super::accepted_provider::OriginalTerminal =
                ffi::OriginalTerminal::default().into();
            terminal.command = complete.raw.command;
            terminal.command.phase = 3;
            terminal.original = early.raw.clone();
            terminal.call = 9;
            terminal.fd_call_present = 1;
            terminal.task_absent = 1;
            rows[2].0.operation = Operation::TerminateOriginalConnect;
            rows[2].0.body = serde_json::to_vec(&Request::TerminateOriginalConnect {
                call: 9,
                command: 26,
                prepared_request: 1,
                selected_request: 2,
                failed_request: None,
            })
            .unwrap();
            rows[2].1 = serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                status: status("ap_retire_dead_original"),
                raw: terminal.clone(),
            }))
            .unwrap();
            check(owner, &rows).unwrap();
            assert_eq!(terminal.original.complete, 0);
            assert_eq!(terminal.original.returned, 0);
            for problem in [1, 2, 4, 8, 16, 32, 64, 127] {
                let mut diagnosed = rows.clone();
                let mut changed = terminal.clone();
                changed.original.problem = problem;
                diagnosed[2].1 = serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                    status: status("ap_retire_dead_original"),
                    raw: changed,
                }))
                .unwrap();
                check(owner, &diagnosed).unwrap();
                let Reply::OriginalTerminated(retained) =
                    serde_json::from_slice(&diagnosed[2].1).unwrap()
                else {
                    panic!("terminal failure became a normal completion")
                };
                assert_eq!(retained.raw.original.problem, problem);
                assert_eq!(retained.raw.original.complete, 0);
                let (_, mut ordinary) = pair();
                let mut effect = completed(&ordinary);
                effect.raw.original.problem = problem;
                ordinary[2].1 = serde_json::to_vec(&Reply::OriginalEffect(effect)).unwrap();
                assert!(check(owner, &ordinary).is_err());
            }
            for case in 0..16 {
                let mut malformed = rows.clone();
                let mut changed = terminal.clone();
                let raw = &mut changed.original;
                match case {
                    0 => raw.complete = 2,
                    1 => raw.problem = 128,
                    2 => raw.reserved = 1,
                    3 => raw.copy_entered = 1,
                    4 => raw.copy_returned = 1,
                    5 => raw.copy_remaining = 1,
                    6 => raw.audit_entered = 1,
                    7 => raw.audit_returned = 1,
                    8 => raw.audit_result = -libc::EFAULT,
                    9 => raw.security_entered = 1,
                    10 => raw.security_returned = 1,
                    11 => raw.security_result = -libc::EACCES,
                    12 => set(raw, 96, 2),
                    13 => {
                        set(raw, 96, 1);
                        raw.address[104..108].copy_from_slice(&1i32.to_ne_bytes());
                    }
                    14 => raw.complete = 1, // Cannot invent missing body return.
                    15 => raw.returned = -libc::EINVAL, // No completed sys_exit.
                    _ => unreachable!(),
                }
                malformed[2].1 = serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                    status: status("ap_retire_dead_original"),
                    raw: changed,
                }))
                .unwrap();
                assert!(
                    check(owner, &malformed).is_err(),
                    "terminal malformed field {case}"
                );
            }
            for at in [16, 24, 32, 48, 56, 64, 80, 84] {
                let mut wrong = rows.clone();
                let mut changed = terminal.clone();
                changed.original.address[at] ^= 1;
                wrong[2].1 = serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                    status: status("ap_retire_dead_original"),
                    raw: changed,
                }))
                .unwrap();
                assert!(
                    check(owner, &wrong).is_err(),
                    "terminal changed retained byte {at}"
                );
            }
            // No early pair existed: exact terminal evidence may retire the
            // physical command, but still contains no invented selections.
            terminal.original = ffi::OriginalResult::default().into();
            terminal.fd_call_present = 0;
            let body = serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                status: status("ap_retire_dead_original"),
                raw: terminal,
            }))
            .unwrap();
            rows[1].1 = body.clone();
            rows[2].1 = body;
            check(owner, &rows).unwrap();
        }
    }
    mod read_copy_tests {
        use super::*;
        use crate::network_replay::original_connect::Kind;
        use crate::network_runtime::original_read_copy::Manifest;
        use crate::network_runtime::original_read_copy::RECORD_BYTES;
        use crate::network_runtime::original_read_copy::RECORDS_PER_REPLY;
        use crate::network_runtime::original_read_copy::Record;
        use crate::network_runtime::original_read_copy::Summary;

        fn read_group() -> (
            NetworkStreamOwner,
            Vec<(Envelope, Vec<u8>, usize)>,
            Vec<Record>,
        ) {
            let (owner, mut rows) = group(1, 9, false);
            let count = 5 * RECORD_BYTES as u64;
            rows[0].0.body = serde_json::to_vec(&Request::PrepareOriginalConnect {
                kind: Kind::Read,
                call: 9,
                mm: owner.mm.generation(),
                fd: 7,
                address: 0x2000,
                length: 0,
                original_count: count,
            })
            .unwrap();
            rows[2].0.body = serde_json::to_vec(&Request::CollectOriginalConnect {
                kind: Kind::Read,
                call: 9,
                command: 26,
                prepared_request: 1,
            })
            .unwrap();
            let Reply::OriginalSelection(mut selected) =
                serde_json::from_slice(&rows[1].1).unwrap()
            else {
                unreachable!()
            };
            selected.raw.address_length = 0;
            selected.raw.original_count = count;
            rows[1].1 = serde_json::to_vec(&Reply::OriginalSelection(selected.clone())).unwrap();
            let Reply::OriginalEffect(mut effect) = serde_json::from_slice(&rows[2].1).unwrap()
            else {
                unreachable!()
            };
            effect.raw.command.operation = 11;
            effect.raw.command.returned = count as i32;
            effect.raw.command.original_count = count;
            effect.raw.command.identity.provider = 3;
            let selected_file = selected.raw.file;
            effect.raw.original.selection = selected.raw;
            effect.raw.original.copy_entered = 0;
            effect.raw.original.copy_returned = 0;
            effect.raw.original.security_entered = 0;
            effect.raw.original.security_returned = 0;
            effect.raw.original.returned = count as i32;
            effect.raw.read_copy = Some(Manifest {
                provider: 3,
                command: 26,
                call: 9,
                task: 61,
                task_start: 99,
                present: 1,
                returned: count as i64,
                summary: Summary {
                    version: 4,
                    initial_count: count,
                    attempts: 1,
                    records: 6,
                    copied: count,
                    final_count: 0,
                    protocol_returned: count,
                    protocol_complete: 1,
                },
            });
            rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(effect)).unwrap();
            let mut records: Vec<Record> = (0..5)
                .map(|index| Record {
                    provider: 3,
                    command: 26,
                    call: 9,
                    task: 61,
                    task_start: 99,
                    sequence: index + 1,
                    attempt: 1,
                    offset: index * RECORD_BYTES as u64,
                    length: RECORD_BYTES as u32,
                    kind: 1,
                    bytes: vec![255; RECORD_BYTES],
                })
                .collect();
            let fields = [selected_file, 1, 0, count, count, 0, 0, 1, 1];
            let mut bytes = vec![0; RECORD_BYTES];
            for (slot, field) in bytes[..72].chunks_exact_mut(8).zip(fields) {
                slot.copy_from_slice(&field.to_le_bytes());
            }
            records.push(Record {
                provider: 3,
                command: 26,
                call: 9,
                task: 61,
                task_start: 99,
                sequence: 6,
                attempt: 1,
                offset: 0,
                length: 72,
                kind: 3,
                bytes,
            });
            (owner, rows, records)
        }
        fn exchange(
            inbox: &mut Inbox<u64>,
            outbox: &mut Outbox<u64>,
            envelope: Envelope,
            body: Vec<u8>,
            pins: Vec<u64>,
        ) {
            let sequence = envelope.sequence;
            assert_eq!(
                outbox.prepare(envelope.clone(), pins.clone()).unwrap(),
                sequence
            );
            outbox.entries.get_mut(&sequence).unwrap().state = SendState::Submitted;
            inbox.retain(envelope.clone(), pins).unwrap();
            inbox.dispatch(sequence, |_, _| Ok(body.clone())).unwrap();
            let mut reply = envelope;
            reply.operation = Operation::Reply;
            reply.body = body;
            outbox.acknowledge(&reply).unwrap();
        }
        fn retained() -> (NetworkStreamOwner, Inbox<u64>, Outbox<u64>, Vec<Record>) {
            let (owner, rows, records) = read_group();
            let mut inbox = Inbox::default();
            let mut outbox = Outbox::default();
            for (envelope, body, rights) in rows {
                exchange(
                    &mut inbox,
                    &mut outbox,
                    envelope,
                    body,
                    if rights == 1 { vec![71] } else { vec![] },
                );
            }
            inbox.retain_read_copy(3, records.clone()).unwrap();
            inbox
                .acknowledge_command_completion(3, |_, _| {
                    Ok(serde_json::to_vec(
                        &serde_json::json!({"Observed":status("ap_ack_command")}),
                    )
                    .unwrap())
                })
                .unwrap();
            (owner, inbox, outbox, records)
        }
        fn fragment(owner: NetworkStreamOwner, first: u64) -> Envelope {
            Envelope {
                run: [7; 16],
                sequence: 4 + first / RECORDS_PER_REPLY as u64,
                owner: Some(owner),
                accept: None,
                operation: Operation::ReadOriginalCopy,
                body: serde_json::to_vec(&Request::ReadOriginalCopy {
                    call: 9,
                    command: 26,
                    prepared: 1,
                    first,
                })
                .unwrap(),
            }
        }
        fn deliver(owner: NetworkStreamOwner, inbox: &mut Inbox<u64>, outbox: &mut Outbox<u64>) {
            for first in 0..6 {
                let request = fragment(owner, first);
                let chunk = inbox
                    .read_copy_chunk(&request, 0, 9, 26, 1, first, || {
                        panic!("final retained copy performed another provider read")
                    })
                    .unwrap()
                    .unwrap();
                let body = serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap();
                let mut reply = request.clone();
                reply.operation = Operation::Reply;
                reply.body = body.clone();
                // 0xff exercises the largest byte-array JSON representation;
                // the complete existing Envelope stays under its unchanged cap.
                let encoded = encode(&reply).unwrap();
                assert!(encoded.len() <= MAX_MESSAGE);
                assert_eq!(decode(&encoded).unwrap(), reply);
                exchange(inbox, outbox, request, body, vec![]);
            }
        }
        fn retire(owner: NetworkStreamOwner, inbox: &mut Inbox<u64>, outbox: &mut Outbox<u64>) {
            let request = Envelope {
                run: [7; 16],
                sequence: 10,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireOriginalConnect,
                body: serde_json::to_vec(&Request::RetireOriginalConnect {
                    call: 9,
                    prepared: 1,
                    selected: 2,
                    completed: 3,
                    failed_request: None,
                })
                .unwrap(),
            };
            inbox.retire_original(owner, 9, [1, 2, 3], None).unwrap();
            exchange(
                inbox,
                outbox,
                request,
                serde_json::to_vec(&Reply::Retired).unwrap(),
                vec![],
            );
            outbox
                .retire_original(owner, 9, [1, 2, 3, 10], None)
                .unwrap();
            inbox.retire_sent_original_ack(10).unwrap();
        }
        /// Convert the historical controlled receipt into explicit V5 frames;
        /// this supplies no native producer evidence. All old controls stay V4.
        fn frontier_group() -> (
            NetworkStreamOwner,
            Vec<(Envelope, Vec<u8>, usize)>,
            Vec<Record>,
        ) {
            let (owner, mut rows, mut records) = read_group();
            let old_end = records.pop().unwrap();
            let mut begin = old_end.clone();
            begin.kind = 4;
            begin.length = 104;
            begin.sequence = 1;
            begin.bytes.fill(0);
            let field = |i: usize| {
                u64::from_le_bytes(old_end.bytes[i * 8..(i + 1) * 8].try_into().unwrap())
            };
            let count = field(3);
            let fields = [
                field(0),
                0,
                0,
                0,
                0,
                count,
                count,
                0,
                count,
                0,
                field(6),
                field(7),
                field(8),
            ];
            for (slot, value) in begin.bytes[..104].chunks_exact_mut(8).zip(fields) {
                slot.copy_from_slice(&value.to_le_bytes());
            }
            for record in &mut records {
                record.sequence += 1;
            }
            let mut end = old_end;
            end.kind = 5;
            end.length = 88;
            end.sequence = records.len() as u64 + 2;
            end.bytes[72..80].copy_from_slice(&0u64.to_le_bytes());
            end.bytes[80..88].copy_from_slice(&count.to_le_bytes());
            records.insert(0, begin);
            records.push(end);
            let Reply::OriginalEffect(mut observed) = serde_json::from_slice(&rows[2].1).unwrap()
            else {
                panic!("Read fixture");
            };
            let manifest = observed.raw.read_copy.as_mut().unwrap();
            manifest.summary.version = 5;
            manifest.summary.records = records.len() as u64;
            rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(observed)).unwrap();
            (owner, rows, records)
        }

        #[test]
        fn original_read_transport_retains_and_retires_only_its_constructor_grammar() {
            use crate::network_runtime::ProviderWireFormat;
            for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
                for version in [4, 5] {
                    let (owner, rows, records) = if version == 4 {
                        read_group()
                    } else {
                        frontier_group()
                    };
                    let mut inbox = Inbox {
                        wire_format: wire,
                        ..Inbox::<u64>::default()
                    };
                    let mut outbox = Outbox {
                        wire_format: wire,
                        ..Outbox::<u64>::default()
                    };
                    for (envelope, body, rights) in rows {
                        exchange(
                            &mut inbox,
                            &mut outbox,
                            envelope,
                            body,
                            if rights == 1 { vec![71] } else { vec![] },
                        );
                    }
                    let retained = inbox.retain_read_copy(3, records.clone());
                    if version != wire.copy_version() {
                        assert!(retained.is_err());
                        assert_eq!(inbox.entries[&1].rights, vec![71]);
                        assert!(!inbox.entries[&3].read_copy_finalized);
                        assert!(inbox.entries[&1].read_copy.is_none());
                        assert!(
                            inbox
                                .acknowledge_command_completion(3, |_, _| panic!(
                                    "grammar mismatch must not ACK native command"
                                ))
                                .is_err()
                        );
                        continue;
                    }
                    retained.unwrap();
                    inbox
                        .acknowledge_command_completion(3, |_, _| {
                            Ok(serde_json::to_vec(
                                &serde_json::json!({"Observed":status("ap_ack_command")}),
                            )
                            .unwrap())
                        })
                        .unwrap();
                    for first in 0..records.len() as u64 {
                        let mut request = fragment(owner, first);
                        request.sequence = 4 + first;
                        let chunk = inbox
                            .read_copy_chunk(&request, 0, 9, 26, 1, first, || {
                                panic!("retained records performed another native fetch")
                            })
                            .unwrap()
                            .unwrap();
                        assert_eq!(chunk.records, vec![records[first as usize].clone()]);
                        exchange(
                            &mut inbox,
                            &mut outbox,
                            request,
                            serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap(),
                            vec![],
                        );
                    }
                    let sequence = 4 + records.len() as u64;
                    let request = Envelope {
                        run: [7; 16],
                        sequence,
                        owner: Some(owner),
                        accept: None,
                        operation: Operation::RetireOriginalConnect,
                        body: serde_json::to_vec(&Request::RetireOriginalConnect {
                            call: 9,
                            prepared: 1,
                            selected: 2,
                            completed: 3,
                            failed_request: None,
                        })
                        .unwrap(),
                    };
                    inbox.retire_original(owner, 9, [1, 2, 3], None).unwrap();
                    exchange(
                        &mut inbox,
                        &mut outbox,
                        request,
                        serde_json::to_vec(&Reply::Retired).unwrap(),
                        vec![],
                    );
                    outbox
                        .retire_original(owner, 9, [1, 2, 3, sequence], None)
                        .unwrap();
                    inbox.retire_sent_original_ack(sequence).unwrap();
                    assert!(inbox.entries.is_empty());
                    assert!(outbox.entries.is_empty());
                }
            }
        }

        #[test]
        fn original_read_ack_requires_retained_empty_copy_before_actual_no_protocol_exit() {
            use crate::network_runtime::ProviderWireFormat;
            use crate::network_runtime::original_read_copy::End;
            for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
                let (owner, mut rows, _) = read_group();
                let Reply::OriginalSelection(mut selected) =
                    serde_json::from_slice(&rows[1].1).unwrap()
                else {
                    panic!("selected Read fixture");
                };
                selected.raw.file = 0;
                selected.raw.fdput_flags = 0;
                rows[1].1 =
                    serde_json::to_vec(&Reply::OriginalSelection(selected.clone())).unwrap();
                let Reply::OriginalEffect(mut observed) =
                    serde_json::from_slice(&rows[2].1).unwrap()
                else {
                    panic!("completed Read fixture");
                };
                observed.raw.original.selection = selected.raw;
                observed.raw.command.returned = -libc::EBADF;
                observed.raw.original.returned = -libc::EBADF;
                let manifest = observed.raw.read_copy.as_mut().unwrap();
                manifest.present = 0;
                manifest.returned = -i64::from(libc::EBADF);
                manifest.summary = Summary::default();
                rows[2].1 = serde_json::to_vec(&Reply::OriginalEffect(observed)).unwrap();
                let primary = rows[2].1.clone();
                let mut inbox = Inbox {
                    wire_format: wire,
                    ..Inbox::<u64>::default()
                };
                let mut outbox = Outbox {
                    wire_format: wire,
                    ..Outbox::<u64>::default()
                };
                for (envelope, body, rights) in rows {
                    exchange(
                        &mut inbox,
                        &mut outbox,
                        envelope,
                        body,
                        if rights == 1 { vec![71] } else { vec![] },
                    );
                }
                assert!(
                    inbox
                        .acknowledge_command_completion(3, |_, _| panic!(
                            "no-protocol Read still requires exact empty-copy retention"
                        ))
                        .is_err()
                );
                assert_eq!(inbox.entries[&3].command_ack, CommandAckState::Unsubmitted);
                assert_eq!(
                    inbox.entries[&3].state,
                    IncomingState::Completed(primary.clone())
                );
                assert_eq!(inbox.entries[&1].rights, vec![71]);
                assert!(inbox.entries[&1].read_copy.is_none());
                inbox.retain_read_copy(3, vec![]).unwrap();
                assert!(inbox.entries[&3].read_copy_finalized);
                assert_eq!(inbox.entries[&1].read_copy, Some(vec![]));
                assert_eq!(
                    inbox.entries[&1].read_copy_end,
                    Some(End::OriginalExit { protocol: false })
                );
                for wrong in [
                    Operation::CollectAccept,
                    Operation::CollectOriginalFileObservation,
                ] {
                    inbox.entries.get_mut(&3).unwrap().envelope.operation = wrong;
                    assert!(
                        inbox
                            .acknowledge_command_completion(3, |_, _| panic!(
                                "Read ACK operation mismatch cannot submit"
                            ))
                            .is_err()
                    );
                    assert_eq!(inbox.entries[&3].command_ack, CommandAckState::Unsubmitted);
                    assert_eq!(inbox.entries[&1].rights, vec![71]);
                }
                inbox.entries.get_mut(&3).unwrap().envelope.operation =
                    Operation::CollectOriginalConnect;
                let mut calls = 0;
                let ack = inbox
                    .acknowledge_command_completion(3, |_, body| {
                        calls += 1;
                        assert_eq!(body, primary);
                        Ok(serde_json::to_vec(
                            &serde_json::json!({"Observed":status("ap_ack_command")}),
                        )
                        .unwrap())
                    })
                    .unwrap();
                assert_eq!(calls, 1);
                assert_eq!(
                    inbox
                        .acknowledge_command_completion(3, |_, _| panic!(
                            "settled no-protocol ACK must not repeat"
                        ))
                        .unwrap(),
                    ack
                );
                let request = fragment(owner, 0);
                let chunk = inbox
                    .read_copy_chunk(&request, 0, 9, 26, 1, 0, || {
                        panic!("retained no-protocol copy must not fetch again")
                    })
                    .unwrap()
                    .unwrap();
                assert!(chunk.records.is_empty());
                assert_eq!(chunk.end, Some(End::OriginalExit { protocol: false }));
                exchange(
                    &mut inbox,
                    &mut outbox,
                    request,
                    serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap(),
                    vec![],
                );
                let request = Envelope {
                    run: [7; 16],
                    sequence: 5,
                    owner: Some(owner),
                    accept: None,
                    operation: Operation::RetireOriginalConnect,
                    body: serde_json::to_vec(&Request::RetireOriginalConnect {
                        call: 9,
                        prepared: 1,
                        selected: 2,
                        completed: 3,
                        failed_request: None,
                    })
                    .unwrap(),
                };
                inbox.retire_original(owner, 9, [1, 2, 3], None).unwrap();
                exchange(
                    &mut inbox,
                    &mut outbox,
                    request,
                    serde_json::to_vec(&Reply::Retired).unwrap(),
                    vec![],
                );
                outbox
                    .retire_original(owner, 9, [1, 2, 3, 5], None)
                    .unwrap();
                inbox.retire_sent_original_ack(5).unwrap();
                assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
            }
        }

        #[test]
        fn original_read_unit_prefix_progresses_before_exit_and_keeps_pending_cut_owned() {
            use crate::network_runtime::original_read_copy::Chunk;
            use crate::network_runtime::original_read_copy::End;
            let (owner, mut rows, records) = read_group();
            let mut inbox = Inbox::default();
            let mut outbox = Outbox::default();
            for (envelope, body, rights) in rows.drain(..2) {
                exchange(
                    &mut inbox,
                    &mut outbox,
                    envelope,
                    body,
                    if rights == 1 { vec![71] } else { vec![] },
                );
            }
            for first in 0..records.len() as u64 {
                let mut request = fragment(owner, first);
                request.sequence = 3 + first;
                let chunk = inbox
                    .read_copy_chunk(&request, 0, 9, 26, 1, first, || {
                        Ok(Some(Chunk {
                            prepared: 1,
                            first,
                            records: vec![records[first as usize].clone()],
                            end: None,
                        }))
                    })
                    .unwrap()
                    .unwrap();
                assert!(chunk.end.is_none());
                exchange(
                    &mut inbox,
                    &mut outbox,
                    request,
                    serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap(),
                    vec![],
                );
            }
            assert_eq!(
                inbox.entries[&1].read_copy.as_deref(),
                Some(records.as_slice())
            );
            assert!(inbox.entries[&1].read_copy_end.is_none());
            assert!(
                !inbox
                    .entries
                    .values()
                    .any(|e| e.envelope.operation == Operation::CollectOriginalConnect)
            );
            assert!(inbox.retire_original(owner, 9, [1, 2, 10], None).is_err());
            // One next-prefix request remains in the existing Inbox. An empty
            // currently available ring never completes it or creates EOF.
            let mut pending = fragment(owner, 6);
            pending.sequence = 9;
            outbox.prepare(pending.clone(), vec![]).unwrap();
            outbox.entries.get_mut(&9).unwrap().state = SendState::Submitted;
            inbox.retain(pending.clone(), vec![]).unwrap();
            assert!(
                inbox
                    .read_copy_chunk(&pending, 0, 9, 26, 1, 6, || Ok(None))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(inbox.entries[&9].state, IncomingState::Retained);
            assert_eq!(inbox.entries[&1].rights, [71]);
            // The original final collector and native ACK still run exactly
            // once. Their result closes, rather than replaces, that prefix.
            let (mut completed, body, rights) = rows.pop().unwrap();
            assert_eq!(rights, 0);
            completed.sequence = 10;
            exchange(&mut inbox, &mut outbox, completed, body, vec![]);
            inbox.retain_read_copy(10, records.clone()).unwrap();
            let mut acknowledgements = 0;
            inbox
                .acknowledge_command_completion(10, |_, _| {
                    acknowledgements += 1;
                    Ok(serde_json::to_vec(
                        &serde_json::json!({"Observed":status("ap_ack_command")}),
                    )
                    .unwrap())
                })
                .unwrap();
            assert_eq!(acknowledgements, 1);
            let chunk = inbox
                .read_copy_chunk(&pending, 0, 9, 26, 1, 6, || {
                    panic!("retained EXIT reread provider")
                })
                .unwrap()
                .unwrap();
            assert!(chunk.records.is_empty());
            assert_eq!(chunk.end, Some(End::OriginalExit { protocol: true }));
            let body = serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap();
            inbox.dispatch(9, |_, _| Ok(body.clone())).unwrap();
            assert!(matches!(outbox.entries[&9].state, SendState::Submitted));
            let mut reply = pending;
            reply.operation = Operation::Reply;
            reply.body = body;
            outbox.acknowledge(&reply).unwrap();
            inbox.retire_original(owner, 9, [1, 2, 10], None).unwrap();
            let retirement = Envelope {
                run: [7; 16],
                sequence: 11,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireOriginalConnect,
                body: serde_json::to_vec(&Request::RetireOriginalConnect {
                    call: 9,
                    prepared: 1,
                    selected: 2,
                    completed: 10,
                    failed_request: None,
                })
                .unwrap(),
            };
            exchange(
                &mut inbox,
                &mut outbox,
                retirement,
                serde_json::to_vec(&Reply::Retired).unwrap(),
                vec![],
            );
            outbox
                .retire_original(owner, 9, [1, 2, 10, 11], None)
                .unwrap();
            inbox.retire_sent_original_ack(11).unwrap();
            assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
        }
        #[test]
        fn maximum_width_copy_fragment_fits_the_unchanged_transport_cap() {
            let (owner, _, mut records) = read_group();
            let record = &mut records[0];
            record.provider = u64::MAX;
            record.command = u64::MAX;
            record.call = u64::MAX;
            record.task = u64::MAX;
            record.task_start = u64::MAX;
            record.sequence = u64::MAX;
            record.attempt = u64::MAX;
            record.offset = u64::MAX;
            let body = serde_json::to_vec(&Reply::OriginalReadCopy(
                crate::network_runtime::original_read_copy::Chunk {
                    prepared: u64::MAX,
                    first: u64::MAX,
                    records: vec![record.clone()],
                    end: Some(crate::network_runtime::original_read_copy::End::ThreadTerminal),
                },
            ))
            .unwrap();
            let reply = Envelope {
                run: [255; 16],
                sequence: u64::MAX,
                owner: Some(owner),
                accept: None,
                operation: Operation::Reply,
                body,
            };
            assert_eq!(RECORDS_PER_REPLY, 1);
            let bytes = encode(&reply).unwrap();
            assert!(bytes.len() <= MAX_MESSAGE);
            assert_eq!(decode(&bytes).unwrap(), reply);
        }
        #[test]
        fn copy_fragments_remain_owned_through_ack_and_retire_with_original_call() {
            let (owner, mut inbox, mut outbox, records) = retained();
            assert!(inbox.retire_original(owner, 9, [1, 2, 3], None).is_err());
            assert_eq!(inbox.entries[&1].rights, [71]);
            assert_eq!(inbox.entries[&1].read_copy.as_ref().unwrap(), &records);
            assert!(inbox.retain_read_copy(3, records.clone()).is_err());
            assert_eq!(inbox.entries[&1].read_copy.as_ref().unwrap(), &records);
            deliver(owner, &mut inbox, &mut outbox);
            retire(owner, &mut inbox, &mut outbox);
            assert!(inbox.entries.is_empty());
            assert!(outbox.entries.is_empty());
            assert!(inbox.retired_observation.is_none() && inbox.retired_fd_observation.is_none());
            assert!(inbox.retain(fragment(owner, 0), vec![]).is_err());
        }
        #[test]
        fn changed_missing_or_unacknowledged_copy_keeps_original_custody() {
            for bad in 0..10 {
                let (owner, mut inbox, mut outbox, records) = retained();
                deliver(owner, &mut inbox, &mut outbox);
                let request = inbox.entries[&4].envelope.clone();
                let IncomingState::Completed(body) = &inbox.entries[&4].state else {
                    unreachable!()
                };
                let body = body.clone();
                let retained = inbox.entries[&1].read_copy.clone();
                match bad {
                    0 => inbox.entries.get_mut(&4).unwrap().envelope.run = [8; 16],
                    1 => inbox.entries.get_mut(&4).unwrap().envelope.owner = None,
                    2 => inbox.entries.get_mut(&4).unwrap().rights.push(73),
                    3 => inbox.entries.get_mut(&4).unwrap().state = IncomingState::Submitted,
                    4 => inbox.entries.get_mut(&1).unwrap().read_copy = None,
                    5 => {
                        inbox
                            .entries
                            .get_mut(&1)
                            .unwrap()
                            .read_copy
                            .as_mut()
                            .unwrap()[0]
                            .bytes[0] ^= 1
                    }
                    6 => {
                        let Reply::OriginalReadCopy(mut chunk) =
                            serde_json::from_slice(&body).unwrap()
                        else {
                            unreachable!()
                        };
                        chunk.records[0].bytes[0] ^= 1;
                        inbox.entries.get_mut(&4).unwrap().state = IncomingState::Completed(
                            serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap(),
                        );
                    }
                    7 => {
                        inbox.entries.get_mut(&4).unwrap().envelope.body =
                            serde_json::to_vec(&Request::ReadOriginalCopy {
                                call: 9,
                                command: 27,
                                prepared: 1,
                                first: 0,
                            })
                            .unwrap()
                    }
                    8 => {
                        inbox.entries.get_mut(&4).unwrap().envelope.body =
                            serde_json::to_vec(&Request::ReadOriginalCopy {
                                call: 9,
                                command: 26,
                                prepared: 1,
                                first: 1,
                            })
                            .unwrap()
                    }
                    9 => {
                        inbox.entries.get_mut(&4).unwrap().envelope.body =
                            serde_json::to_vec(&Request::ReadOriginalCopy {
                                call: 9,
                                command: 26,
                                prepared: 7,
                                first: 0,
                            })
                            .unwrap()
                    }
                    _ => unreachable!(),
                }
                assert!(
                    inbox.retire_original(owner, 9, [1, 2, 3], None).is_err(),
                    "corruption {bad}"
                );
                assert_eq!(inbox.entries.len(), 9);
                assert_eq!(outbox.entries.len(), 9);
                assert_eq!(inbox.entries[&1].rights, [71]);
                let entry = inbox.entries.get_mut(&4).unwrap();
                entry.envelope = request;
                entry.state = IncomingState::Completed(body);
                entry.rights.clear();
                inbox.entries.get_mut(&1).unwrap().read_copy = retained;
                assert_eq!(inbox.entries[&1].read_copy.as_ref().unwrap(), &records);
                // A lost reply retains the submitted request and original
                // owner; only the exact previously retained reply settles it.
                let SendState::Acknowledged(ack) = &outbox.entries[&4].state else {
                    unreachable!()
                };
                let ack = ack.clone();
                outbox.entries.get_mut(&4).unwrap().state = SendState::Submitted;
                let IncomingState::Completed(completed) = &inbox.entries[&3].state else {
                    unreachable!()
                };
                assert!(read_copy_outgoing(&outbox.entries, owner, 9, 1, 3, completed).is_err());
                outbox.entries.get_mut(&4).unwrap().state = SendState::Acknowledged(ack);
                retire(owner, &mut inbox, &mut outbox);
                assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod native_birth_retirement_tests {
    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::Reply;
    use crate::network_runtime::accepted_provider::Request;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn status(name: &str) -> CallStatus {
        CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        }
    }
    pub(super) fn group(
        first: u64,
        call: u64,
        kind: u8,
    ) -> (
        NetworkStreamOwner,
        Option<u64>,
        u64,
        Vec<(Envelope, Vec<u8>, usize)>,
    ) {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let command = call + 17;
        let mut birth = ffi::NativeBirth {
            command,
            call,
            owner_mm: owner.mm.generation(),
            provider: 3,
            creator_task: (5001u64 << 32) | 5001,
            creator_start: 29,
            creator_table: 47,
            pidfd_fd: -1,
            ..Default::default()
        };
        if kind == 0 {
            birth.child_task = (5002u64 << 32) | 5002;
            birth.child_start = 31;
            birth.child_table = 53;
            birth.parent_task = birth.creator_task;
            birth.parent_start = 29;
            birth.ready = 1;
            birth.copy_begin = 59;
            birth.copy_end = 61;
            birth.exit_signal = 17;
            birth.requested_exit_signal = 17;
        }
        let mut values = vec![(
            Operation::PrepareNativeBirth,
            Request::PrepareNativeBirth {
                call,
                mm: owner.mm.generation(),
                table: 47,
                syscall: 435,
            },
            Reply::Prepared(Observation {
                status: status("ap_prepare_native_birth"),
                raw: command,
            }),
            1,
        )];
        let observed = if kind == 0 {
            values.push((
                Operation::ObserveNativeBirth,
                Request::ObserveNativeBirth {
                    call,
                    command,
                    prepared_request: first,
                    child: 62,
                    terminal: false,
                },
                Reply::NativeBirth(Observation {
                    status: status("ap_admit_native_birth_child"),
                    raw: birth.into(),
                }),
                1,
            ));
            Some(first + 1)
        } else {
            None
        };
        let completed = first + values.len() as u64;
        if kind == 2 {
            values.push((
                Operation::CancelNativeBirth,
                Request::CancelNativeBirth {
                    call,
                    command,
                    prepared_request: first,
                },
                Reply::NativeBirthCanceled {
                    command,
                    status: status("ap_cancel_uninvoked_birth"),
                },
                0,
            ));
        } else {
            let result = ffi::CommandResult {
                command,
                operation: 8,
                task: birth.creator_task,
                start_boottime: 29,
                identity: ffi::Identity {
                    provider: 3,
                    object: 0,
                    namespace: 0,
                },
                phase: 1,
                returned: if kind == 0 { 62 } else { -libc::EINVAL },
                ..Default::default()
            };
            values.push((
                Operation::CollectNativeBirth,
                Request::CollectNativeBirth {
                    call,
                    command,
                    prepared_request: first,
                },
                Reply::NativeBirthEffect(Observation {
                    status: status("ap_collect_native_birth"),
                    raw: ffi::NativeBirthEffect {
                        command: result,
                        birth,
                    }
                    .into(),
                }),
                0,
            ));
        }
        let rows = values
            .into_iter()
            .enumerate()
            .map(|(n, (operation, request, reply, rights))| {
                (
                    Envelope {
                        run: [7; 16],
                        sequence: first + n as u64,
                        owner: Some(owner),
                        accept: None,
                        operation,
                        body: serde_json::to_vec(&request).unwrap(),
                    },
                    serde_json::to_vec(&reply).unwrap(),
                    rights,
                )
            })
            .collect();
        (owner, observed, completed, rows)
    }
    fn check(
        owner: NetworkStreamOwner,
        call: u64,
        first: u64,
        observed: Option<u64>,
        completed: u64,
        rows: &[(Envelope, Vec<u8>, usize)],
    ) -> io::Result<()> {
        validate_native_birth_group(
            owner,
            call,
            first,
            observed,
            completed,
            &rows
                .iter()
                .map(|(e, b, n)| (e, b.as_slice(), *n))
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn native_birth_retirement_rejects_changed_group_without_dropping_rights() {
        for kind in 0..3 {
            let (owner, observed, completed, rows) = group(1, 9, kind);
            check(owner, 9, 1, observed, completed, &rows).unwrap();
            for index in 0..rows.len() {
                let mut wrong = rows.clone();
                wrong[index].0.owner = None;
                assert!(check(owner, 9, 1, observed, completed, &wrong).is_err());
                wrong = rows.clone();
                wrong[index].0.run = [8; 16];
                assert!(check(owner, 9, 1, observed, completed, &wrong).is_err());
                wrong = rows.clone();
                wrong[index].2 ^= 1;
                assert!(check(owner, 9, 1, observed, completed, &wrong).is_err());
            }
            let mut wrong = rows.clone();
            wrong[0].0.body = serde_json::to_vec(&Request::PrepareNativeBirth {
                call: 10,
                mm: owner.mm.generation(),
                table: 47,
                syscall: 435,
            })
            .unwrap();
            assert!(check(owner, 9, 1, observed, completed, &wrong).is_err());
            assert!(check(owner, 9, 1, Some(completed), completed, &rows).is_err());
            wrong = rows.clone();
            wrong.last_mut().unwrap().1 = serde_json::to_vec(&Reply::NativeBirthCanceled {
                command: 26,
                status: status("unproven_disappearance"),
            })
            .unwrap();
            assert!(check(owner, 9, 1, observed, completed, &wrong).is_err());
            assert_eq!(rows[0].2, 1);
            if kind == 0 {
                assert_eq!(rows[1].2, 1);
            }
        }
    }
    #[test]
    fn native_birth_groups_retire_finitely_after_semantics_and_ack() {
        let mut inbox = Inbox::<u64>::default();
        let mut outbox = Outbox::<u64>::default();
        for call in 1..=256 {
            let first = inbox.next;
            let kind = (call % 3) as u8;
            let (owner, observed, completed, rows) = group(first, call, kind);
            for (envelope, body, rights) in &rows {
                let pins = vec![call; *rights];
                outbox.prepare(envelope.clone(), pins.clone()).unwrap();
                outbox.entries.get_mut(&envelope.sequence).unwrap().state = SendState::Submitted;
                inbox.retain(envelope.clone(), pins).unwrap();
                assert!(
                    inbox
                        .retire_native_birth(owner, call, first, observed, completed)
                        .is_err()
                );
                inbox
                    .dispatch(envelope.sequence, |_, _| Ok(body.clone()))
                    .unwrap();
                let mut reply = envelope.clone();
                reply.operation = Operation::Reply;
                reply.body = body.clone();
                outbox.acknowledge(&reply).unwrap();
            }
            if kind != 2 {
                assert!(
                    inbox
                        .retire_native_birth(owner, call, first, observed, completed)
                        .is_err()
                );
                assert_eq!(inbox.entries[&first].rights, vec![call]);
                if let Some(child) = observed {
                    assert_eq!(inbox.entries[&child].rights, vec![call]);
                }
                inbox
                    .acknowledge_command_completion(completed, |_, _| {
                        Ok(serde_json::to_vec(
                            &serde_json::json!({"Observed":status("ap_ack_command")}),
                        )
                        .unwrap())
                    })
                    .unwrap();
            }
            let retirement = completed + 1;
            let request = Envelope {
                run: [7; 16],
                sequence: retirement,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireNativeBirth,
                body: serde_json::to_vec(&Request::RetireNativeBirth {
                    call,
                    prepared: first,
                    observed,
                    completed,
                })
                .unwrap(),
            };
            outbox.prepare(request.clone(), vec![]).unwrap();
            outbox.entries.get_mut(&retirement).unwrap().state = SendState::Submitted;
            inbox.retain(request.clone(), vec![]).unwrap();
            inbox
                .retire_native_birth(owner, call, first, observed, completed)
                .unwrap();
            assert!(
                outbox
                    .retire_native_birth(owner, call, first, observed, completed, retirement)
                    .is_err()
            );
            assert_eq!(outbox.entries[&first].rights, vec![call]); // lost reply retains all original rights
            let body = serde_json::to_vec(&Reply::Retired).unwrap();
            inbox.dispatch(retirement, |_, _| Ok(body.clone())).unwrap();
            let mut reply = request.clone();
            reply.operation = Operation::Reply;
            reply.body = body;
            outbox.acknowledge(&reply).unwrap();
            outbox
                .retire_native_birth(owner, call, first, observed, completed, retirement)
                .unwrap();
            inbox.retire_sent_original_ack(retirement).unwrap();
            assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
            assert!(inbox.retired_observation.is_none() && inbox.retired_fd_observation.is_none());
            assert!(inbox.retain(request, vec![]).is_err());
        }
        assert_eq!(inbox.next, outbox.next);
        assert_eq!(inbox.next, 854);
    }
    #[test]
    fn native_birth_unknown_and_failed_ack_keep_exact_creator_and_child_custody() {
        for unknown in [false, true] {
            let (owner, observed, completed, rows) = group(1, 9, 0);
            let mut inbox = Inbox::<u64>::default();
            for (envelope, body, rights) in rows {
                let seq = envelope.sequence;
                inbox.retain(envelope, vec![9; rights]).unwrap();
                inbox.dispatch(seq, |_, _| Ok(body)).unwrap();
            }
            let result=inbox.acknowledge_command_completion(completed,|_,_| {
                if unknown {Err(protocol("lost actual ACK reply"))} else {Ok(serde_json::to_vec(
                    &serde_json::json!({"Observed":CallStatus {operation:"ap_ack_command".into(),returned:-1,errno:Some(libc::EIO)}})).unwrap())}
            });
            assert_eq!(result.is_err(), unknown);
            assert!(
                inbox
                    .retire_native_birth(owner, 9, 1, observed, completed)
                    .is_err()
            );
            assert_eq!(inbox.entries[&1].rights, vec![9]);
            assert_eq!(inbox.entries[&2].rights, vec![9]);
            assert_eq!(inbox.entries.len(), 3);
            if unknown {
                assert!(
                    inbox
                        .acknowledge_command_completion(completed, |_, _| panic!(
                            "unknown physical ACK retried"
                        ))
                        .is_err()
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) fn native_birth_test_group(
    first: u64,
    call: u64,
    kind: u8,
) -> (
    NetworkStreamOwner,
    Option<u64>,
    u64,
    Vec<(Envelope, Vec<u8>, usize)>,
) {
    native_birth_retirement_tests::group(first, call, kind)
}

#[cfg(test)]
pub(super) fn native_birth_terminal_test_group(
    first: u64,
    call: u64,
) -> (
    NetworkStreamOwner,
    Option<u64>,
    u64,
    Vec<(Envelope, Vec<u8>, usize)>,
) {
    use super::accepted_provider::NativeBirthTerminal;
    use super::accepted_provider::Observation;
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    let (owner, observed, completed, mut rows) = native_birth_test_group(first, call, 0);
    let (envelope, body, _) = rows.last_mut().unwrap();
    let Reply::NativeBirthEffect(mut result) = serde_json::from_slice(body).unwrap() else {
        panic!("expected birth")
    };
    result.raw.command.phase = 3;
    result.raw.command.returned = 0;
    result.raw.command.identity.provider = 0;
    envelope.operation = Operation::TerminateNativeBirth;
    envelope.body = serde_json::to_vec(&Request::TerminateNativeBirth {
        call,
        command: result.raw.command.command,
        prepared_request: first,
    })
    .unwrap();
    result.status.operation = "ap_retire_dead_birth".into();
    *body = serde_json::to_vec(&Reply::NativeBirthTerminated(Observation {
        status: result.status,
        raw: NativeBirthTerminal {
            command: result.raw.command,
            birth: result.raw.birth,
            call,
            fd_call_present: 1,
            task_absent: 1,
        },
    }))
    .unwrap();
    (owner, observed, completed, rows)
}
#[cfg(test)]
mod native_birth_terminal_tests {
    use super::super::accepted_provider::Reply;
    use super::*;
    #[test]
    fn native_birth_terminal_requires_exact_positive_death_and_child_without_fake_return() {
        let (owner, observed, completed, rows) = native_birth_terminal_test_group(1, 9);
        let check = |rows: &[(Envelope, Vec<u8>, usize)]| {
            validate_native_birth_group(
                owner,
                9,
                1,
                observed,
                completed,
                &rows
                    .iter()
                    .map(|(e, b, n)| (e, b.as_slice(), *n))
                    .collect::<Vec<_>>(),
            )
        };
        check(&rows).unwrap();
        for change in 0..8 {
            let mut bad = rows.clone();
            let Reply::NativeBirthTerminated(mut value) =
                serde_json::from_slice(&bad[2].1).unwrap()
            else {
                panic!("not terminal")
            };
            match change {
                0 => value.raw.task_absent = 0,
                1 => value.raw.fd_call_present = 0,
                2 => value.raw.birth.child_start += 1,
                3 => value.raw.birth.creator_start += 1,
                4 => value.raw.command.phase = 1, // creator zero cannot become DONE
                5 => value.status.returned = -1,
                6 => value.raw.command.identity.provider = 4,
                7 => value.raw.birth.ready = 0,
                _ => unreachable!(),
            }
            bad[2].1 = serde_json::to_vec(&Reply::NativeBirthTerminated(value)).unwrap();
            assert!(check(&bad).is_err());
        }
        let Reply::NativeBirthTerminated(value) = serde_json::from_slice(&rows[2].1).unwrap()
        else {
            panic!("not terminal")
        };
        assert_eq!(
            (value.raw.command.phase, value.raw.command.returned),
            (3, 0)
        );
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].2, rows[1].2), (1, 1));
    }
    #[test]
    fn dead_creator_group_keeps_both_rights_until_exact_terminal_receipt_and_retirement_ack() {
        let (owner, observed, completed, rows) = native_birth_terminal_test_group(1, 9);
        let mut inbox = Inbox::<u64>::default();
        let mut outbox = Outbox::<u64>::default();
        for (envelope, body, rights) in &rows {
            outbox.prepare(envelope.clone(), vec![71; *rights]).unwrap();
            outbox.entries.get_mut(&envelope.sequence).unwrap().state = SendState::Submitted;
            inbox.retain(envelope.clone(), vec![81; *rights]).unwrap();
            inbox
                .dispatch(envelope.sequence, |_, _| Ok(body.clone()))
                .unwrap();
            let mut reply = envelope.clone();
            reply.operation = Operation::Reply;
            reply.body = body.clone();
            outbox.acknowledge(&reply).unwrap();
        }
        assert_eq!(inbox.entries[&1].rights, vec![81]);
        assert_eq!(inbox.entries[&2].rights, vec![81]);
        let IncomingState::Completed(original) = &inbox.entries[&completed].state else {
            panic!("terminal receipt not completed")
        };
        let original = original.clone();
        let Reply::NativeBirthTerminated(mut bad) = serde_json::from_slice(&rows[2].1).unwrap()
        else {
            panic!("not terminal")
        };
        bad.raw.task_absent = 0;
        inbox.entries.get_mut(&completed).unwrap().state = IncomingState::Completed(
            serde_json::to_vec(&Reply::NativeBirthTerminated(bad)).unwrap(),
        );
        assert!(
            inbox
                .retire_native_birth(owner, 9, 1, observed, completed)
                .is_err()
        );
        assert_eq!(inbox.entries[&1].rights, vec![81]);
        assert_eq!(inbox.entries[&2].rights, vec![81]);
        inbox.entries.get_mut(&completed).unwrap().state = IncomingState::Completed(original);
        // No fabricated provider CommandACK: terminal physical removal is its
        // separate positive receipt, and the immutable command stays RUNNING.
        inbox
            .retire_native_birth(owner, 9, 1, observed, completed)
            .unwrap();
        let retire = Envelope {
            run: [7; 16],
            sequence: 4,
            owner: Some(owner),
            accept: None,
            operation: Operation::RetireNativeBirth,
            body: serde_json::to_vec(
                &super::super::accepted_provider::Request::RetireNativeBirth {
                    call: 9,
                    prepared: 1,
                    observed,
                    completed,
                },
            )
            .unwrap(),
        };
        outbox.prepare(retire.clone(), vec![]).unwrap();
        outbox.entries.get_mut(&4).unwrap().state = SendState::Submitted;
        inbox.retain(retire.clone(), vec![]).unwrap();
        assert!(
            outbox
                .retire_native_birth(owner, 9, 1, observed, completed, 4)
                .is_err()
        );
        assert_eq!(outbox.entries[&1].rights, vec![71]);
        assert_eq!(outbox.entries[&2].rights, vec![71]);
        let body = serde_json::to_vec(&Reply::Retired).unwrap();
        inbox.dispatch(4, |_, _| Ok(body.clone())).unwrap();
        let mut reply = retire;
        reply.operation = Operation::Reply;
        reply.body = body;
        outbox.acknowledge(&reply).unwrap();
        outbox
            .retire_native_birth(owner, 9, 1, observed, completed, 4)
            .unwrap();
        inbox.retire_sent_original_ack(4).unwrap();
        assert!(inbox.entries.is_empty() && outbox.entries.is_empty());
    }
}

#[cfg(test)]
mod abi6_birth_receipt_tests {
    use super::*;
    #[test]
    fn retirement_rejects_a_changed_clear_tid_in_the_complete_receipt() {
        let (owner, observed, completed, mut rows) =
            super::native_birth_retirement_tests::group(1, 9, 0);
        let check = |rows: &Vec<(Envelope, Vec<u8>, usize)>| {
            validate_native_birth_group(
                owner,
                9,
                1,
                observed,
                completed,
                &rows
                    .iter()
                    .map(|(e, b, n)| (e, b.as_slice(), *n))
                    .collect::<Vec<_>>(),
            )
        };
        check(&rows).unwrap();
        let mut reply: super::super::accepted_provider::Reply =
            serde_json::from_slice(&rows.last().unwrap().1).unwrap();
        let super::super::accepted_provider::Reply::NativeBirthEffect(effect) = &mut reply else {
            panic!("Collect receipt");
        };
        effect.raw.birth.clear_child_tid = 0x5678;
        rows.last_mut().unwrap().1 = serde_json::to_vec(&reply).unwrap();
        assert!(check(&rows).is_err());
        assert_eq!(rows[0].2, 1);
        assert_eq!(rows[1].2, 1);
    }
}

// Copy frames stay in the same original preparation/selection/completion
// group. They may precede EXIT, but retirement still joins every retained frame
// to the actual normal result or the distinct positive task-terminal receipt.
fn validate_read_copy_rows(
    owner: NetworkStreamOwner,
    run: [u8; 16],
    call: u64,
    prepared: u64,
    body: &[u8],
    rows: Vec<(&Envelope, &[u8], usize)>,
    version: u64,
) -> io::Result<(
    Vec<u64>,
    Option<Vec<super::original_read_copy::Record>>,
    Option<super::original_read_copy::End>,
)> {
    use super::accepted_provider::Reply;
    use super::accepted_provider::Request;
    use super::original_read_copy::End;
    let reply: Reply = serde_json::from_slice(body)?;
    let (command, effect, terminal) = match &reply {
        Reply::OriginalEffect(out) if out.status.returned == 0 && out.raw.read_copy.is_some() => {
            (Some(out.raw.command.command), Some(&out.raw), None)
        }
        Reply::OriginalFileObservation {
            selection,
            effect: Some(out),
        } if selection.status.returned == 0
            && selection.status.errno.is_none()
            && selection.raw == out.raw.original.selection
            && out.status.returned == 0
            && out.status.errno.is_none()
            && out.raw.read_copy.is_some()
            && matches!(out.raw.command.operation, 21 | 22) =>
        {
            (Some(out.raw.command.command), Some(&out.raw), None)
        }
        Reply::OriginalTerminated(out)
            if out.status.returned == 0
                && out.raw.task_absent == 1
                && out.raw.command.operation == 11 =>
        {
            (Some(out.raw.command.command), None, Some(&out.raw))
        }
        _ => (None, None, None),
    };
    let mut next = 0u64;
    let mut records = Vec::new();
    let mut sequences = Vec::new();
    let mut end = None;
    for (envelope, body, rights) in rows {
        let Request::ReadOriginalCopy {
            call: c,
            command: k,
            prepared: p,
            first,
        } = serde_json::from_slice(&envelope.body)?
        else {
            return Err(protocol("Read copy retirement changed request kind"));
        };
        let Reply::OriginalReadCopy(chunk) = serde_json::from_slice(body)? else {
            return Err(protocol("Read copy retirement changed reply kind"));
        };
        if envelope.operation != Operation::ReadOriginalCopy
            || envelope.owner != Some(owner)
            || envelope.run != run
            || envelope.accept.is_some()
            || rights != 0
            || envelope.sequence <= prepared
            || c != call
            || p != prepared
            || Some(k) != command
            || first != next
            || chunk.prepared != prepared
            || chunk.first != next
            || end.is_some()
            || chunk.records.len() > super::original_read_copy::RECORDS_PER_REPLY
            || chunk.records.is_empty() && chunk.end.is_none()
        {
            return Err(protocol(
                "Read copy retirement changed owner/preparation/position",
            ));
        }
        next = next
            .checked_add(chunk.records.len() as u64)
            .ok_or_else(|| protocol("Read copy retirement overflow"))?;
        records.extend(chunk.records);
        end = chunk.end;
        sequences.push(envelope.sequence);
    }
    if let Some(effect) = effect {
        if end
            != Some(End::OriginalExit {
                protocol: effect.read_copy.unwrap().present == 1,
            })
        {
            return Err(protocol("Read copy retirement lacks actual EXIT cut"));
        }
        super::original_read_copy::validate_records_for_version(effect, &records, version)?;
        return Ok((sequences, Some(records), end));
    }
    if let Some(terminal) = terminal {
        if end.is_none() {
            return Err(protocol(
                "Read copy retirement lacks terminal prefix disposition",
            ));
        }
        super::original_read_copy::validate_terminal_prefix_for_version(
            terminal,
            &records,
            end.unwrap(),
            version,
        )?;
        return Ok((sequences, Some(records), end));
    }
    if !sequences.is_empty() {
        return Err(protocol("non-Read/uninvoked call has Read copy frames"));
    }
    Ok((sequences, None, None))
}
#[cfg(test)]
fn read_copy_outgoing<T>(
    entries: &BTreeMap<u64, Outgoing<T>>,
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    body: &[u8],
) -> io::Result<Vec<u64>> {
    read_copy_outgoing_for_version(entries, owner, call, prepared, completed, body, 4)
}
fn read_copy_outgoing_for_version<T>(
    entries: &BTreeMap<u64, Outgoing<T>>,
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    body: &[u8],
    version: u64,
) -> io::Result<Vec<u64>> {
    let mut rows = Vec::new();
    for entry in entries.values() {
        if entry.envelope.operation != Operation::ReadOriginalCopy {
            continue;
        }
        let super::accepted_provider::Request::ReadOriginalCopy { prepared: p, .. } =
            serde_json::from_slice(&entry.envelope.body)?
        else {
            return Err(protocol("malformed retained Read copy request"));
        };
        if p != prepared {
            continue;
        }
        let SendState::Acknowledged(body) = &entry.state else {
            return Err(protocol("Read copy outgoing reply unresolved"));
        };
        rows.push((&entry.envelope, body.as_slice(), entry.rights.len()));
    }
    let finish = entries
        .get(&completed)
        .ok_or_else(|| protocol("Read copy outgoing completion missing"))?;
    validate_read_copy_rows(
        owner,
        finish.envelope.run,
        call,
        prepared,
        body,
        rows,
        version,
    )
    .map(|(sequences, _, _)| sequences)
}
fn read_copy_incoming_for_version<T>(
    entries: &BTreeMap<u64, Incoming<T>>,
    owner: NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    body: &[u8],
    version: u64,
) -> io::Result<Vec<u64>> {
    let mut rows = Vec::new();
    for entry in entries.values() {
        if entry.envelope.operation != Operation::ReadOriginalCopy {
            continue;
        }
        let super::accepted_provider::Request::ReadOriginalCopy { prepared: p, .. } =
            serde_json::from_slice(&entry.envelope.body)?
        else {
            return Err(protocol("malformed retained Read copy request"));
        };
        if p != prepared {
            continue;
        }
        let IncomingState::Completed(body) = &entry.state else {
            return Err(protocol("Read copy incoming effect unresolved"));
        };
        rows.push((&entry.envelope, body.as_slice(), entry.rights.len()));
    }
    let finish = entries
        .get(&completed)
        .ok_or_else(|| protocol("Read copy incoming completion missing"))?;
    let (sequences, delivered, end) = validate_read_copy_rows(
        owner,
        finish.envelope.run,
        call,
        prepared,
        body,
        rows,
        version,
    )?;
    if matches!(
        finish.envelope.operation,
        Operation::CollectOriginalConnect | Operation::CollectOriginalFileObservation
    ) && delivered.is_some()
        && !finish.read_copy_finalized
    {
        return Err(protocol(
            "Read copy completion was not retained before provider ACK",
        ));
    }
    let initial = entries
        .get(&prepared)
        .ok_or_else(|| protocol("Read copy incoming preparation missing"))?;
    if initial.read_copy.as_deref() != delivered.as_deref() || initial.read_copy_end != end {
        return Err(protocol(
            "Read copy retirement changed or lost retained native prefix/end",
        ));
    }
    Ok(sequences)
}

#[cfg(test)]
mod auxiliary_file_role_tests {
    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::OriginalEffect;
    use crate::network_runtime::accepted_provider::Reply;
    use crate::network_runtime::accepted_provider::Request;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::types::DetTid;
    use crate::types::MmId;
    #[test]
    fn original_openat_auxiliary_transport_binds_worker_role_and_exact_native_operands() {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let good = |name: &str| CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        };
        let prepare = Envelope {
            run: [3; 16],
            sequence: 41,
            owner: Some(owner),
            accept: None,
            operation: Operation::PrepareOriginalFileObservation,
            body: serde_json::to_vec(&Request::PrepareOriginalFileObservation {
                call: 17,
                mm: owner.mm.generation(),
                fd: 88,
                role: crate::network_runtime::accepted_provider::AuxiliaryRole::File,
            })
            .unwrap(),
        };
        let collect = Envelope {
            run: [3; 16],
            sequence: 42,
            owner: Some(owner),
            accept: None,
            operation: Operation::CollectOriginalFileObservation,
            body: serde_json::to_vec(&Request::CollectOriginalFileObservation {
                call: 17,
                command: 91,
                prepared_request: 41,
                role: crate::network_runtime::accepted_provider::AuxiliaryRole::File,
            })
            .unwrap(),
        };
        let prepared = serde_json::to_vec(&Reply::Prepared(Observation {
            status: good("ap_prepare_auxiliary_file"),
            raw: 91,
        }))
        .unwrap();
        let mut raw: OriginalEffect = ffi::OriginalEffect::default().into();
        raw.original.selection = ffi::OriginalSelection {
            command: 91,
            call: 17,
            owner_mm: owner.mm.generation(),
            provider: 7,
            task: 41,
            task_start: 101,
            table: 13,
            file: 19,
            requested_fd: 88,
            user_address: libc::SYS_fcntl as u64,
            address_length: libc::F_GETFL,
            ready: 1,
            ..Default::default()
        }
        .into();
        raw.original.complete = 1;
        raw.command.command = 91;
        raw.command.operation = 23;
        raw.command.phase = 1;
        raw.command.task = 41;
        raw.command.start_boottime = 101;
        raw.command.identity.provider = 7;
        let reply = |raw: OriginalEffect| {
            serde_json::to_vec(&Reply::OriginalFileObservation {
                selection: Observation {
                    status: good("ap_read_original_selection"),
                    raw: raw.original.selection.clone(),
                },
                effect: Some(Observation {
                    status: good("ap_collect_original_connect"),
                    raw,
                }),
            })
            .unwrap()
        };
        let body = reply(raw.clone());
        let check = |armed: &[u8], body: &[u8]| {
            validate_file_observation_group(
                owner,
                17,
                41,
                42,
                &[(&prepare, armed, 1), (&collect, body, 0)],
            )
        };
        check(&prepared, &body).unwrap();
        for case in 0..8 {
            let mut wrong = raw.clone();
            match case {
                0 => wrong.command.operation = 10,
                1 => wrong.original.selection.user_address = libc::SYS_ioctl as u64,
                2 => wrong.original.selection.address_length = libc::F_GETFD,
                3 => wrong.original.selection.original_count = 1,
                4 => wrong.original.selection.task_start += 1,
                5 => wrong.command.identity.provider += 1,
                6 => wrong.original.problem = 1,
                7 => wrong.original.complete = 0,
                _ => unreachable!(),
            }
            assert!(
                check(&prepared, &reply(wrong)).is_err(),
                "mutated auxiliary contract {case}"
            );
        }
        let guest_prepared = serde_json::to_vec(&Reply::Prepared(Observation {
            status: good("ap_prepare_original_file"),
            raw: 91,
        }))
        .unwrap();
        assert!(check(&guest_prepared, &body).is_err());
    }
}

#[cfg(test)]
mod helper_receive_transport_tests {
    use super::*;
    use crate::network_runtime::accepted_provider::AuxiliaryRole;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::OriginalEffect;
    use crate::network_runtime::accepted_provider::ReceiveKind;
    use crate::network_runtime::accepted_provider::Reply;
    use crate::network_runtime::accepted_provider::Request;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::network_runtime::original_read_copy::End;
    use crate::network_runtime::original_read_copy::Manifest;
    use crate::network_runtime::original_read_copy::RECORD_BYTES;
    use crate::network_runtime::original_read_copy::Record;
    use crate::network_runtime::original_read_copy::Summary;
    use crate::types::DetTid;
    use crate::types::MmId;
    type Rows = Vec<(Envelope, Vec<u8>, usize)>;
    fn good(name: &str) -> CallStatus {
        CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        }
    }
    fn group(kind: ReceiveKind) -> (NetworkStreamOwner, AuxiliaryRole, Rows, Vec<Record>) {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let count = if kind == ReceiveKind::Drain { 1 } else { 1024 };
        let role = AuxiliaryRole::Receive {
            kind,
            address: 0x8000,
            count,
            provider: 7,
            file: 19,
        };
        let (address, flags, _) = role.operands();
        let mut effect: OriginalEffect = ffi::OriginalEffect::default().into();
        effect.original.selection = ffi::OriginalSelection {
            command: 91,
            call: 17,
            owner_mm: owner.mm.generation(),
            provider: 7,
            task: 41,
            task_start: 101,
            table: 13,
            file: 19,
            user_address: address,
            fdput_flags: 0,
            ready: 1,
            requested_fd: 88,
            address_length: flags,
            original_count: count,
        }
        .into();
        effect.command.command = 91;
        effect.command.operation = role.operation();
        effect.command.task = 41;
        effect.command.start_boottime = 101;
        effect.command.identity.provider = 7;
        effect.command.phase = 1;
        effect.command.original_count = count;
        effect.command.returned = 1;
        effect.original.complete = 1;
        effect.original.returned = 1;
        effect.read_copy = Some(Manifest {
            provider: 7,
            command: 91,
            call: 17,
            task: 41,
            task_start: 101,
            present: 1,
            returned: 1,
            summary: Summary {
                version: 4,
                initial_count: count,
                attempts: 1,
                records: 2,
                copied: 1,
                final_count: count - 1,
                protocol_returned: 1,
                protocol_complete: 1,
            },
        });
        let prepare = Envelope {
            run: [3; 16],
            sequence: 1,
            owner: Some(owner),
            accept: None,
            operation: Operation::PrepareOriginalFileObservation,
            body: serde_json::to_vec(&Request::PrepareOriginalFileObservation {
                call: 17,
                mm: owner.mm.generation(),
                fd: 88,
                role,
            })
            .unwrap(),
        };
        let collect = Envelope {
            sequence: 2,
            operation: Operation::CollectOriginalFileObservation,
            body: serde_json::to_vec(&Request::CollectOriginalFileObservation {
                call: 17,
                command: 91,
                prepared_request: 1,
                role,
            })
            .unwrap(),
            ..prepare.clone()
        };
        let rows = vec![
            (
                prepare,
                serde_json::to_vec(&Reply::Prepared(Observation {
                    status: good(role.prepare_name()),
                    raw: 91,
                }))
                .unwrap(),
                1,
            ),
            (
                collect,
                serde_json::to_vec(&Reply::OriginalFileObservation {
                    selection: Observation {
                        status: good("ap_read_original_selection"),
                        raw: effect.original.selection.clone(),
                    },
                    effect: Some(Observation {
                        status: good("ap_collect_original_connect"),
                        raw: effect,
                    }),
                })
                .unwrap(),
                0,
            ),
        ];
        let mut bytes = vec![0; RECORD_BYTES];
        bytes[0] = 55;
        let data = Record {
            provider: 7,
            command: 91,
            call: 17,
            task: 41,
            task_start: 101,
            sequence: 1,
            attempt: 1,
            offset: 0,
            length: 1,
            kind: 1,
            bytes,
        };
        let mut unit = data.clone();
        unit.sequence = 2;
        unit.length = 72;
        unit.kind = 3;
        unit.bytes.fill(0);
        let consume = kind == ReceiveKind::Drain;
        let fields = [
            19u64,
            if consume { 1 } else { 0 },
            0,
            1,
            1,
            0,
            8,
            1,
            if consume { 1 } else { 2 },
        ];
        for (chunk, field) in unit.bytes[..72].chunks_exact_mut(8).zip(fields) {
            chunk.copy_from_slice(&field.to_le_bytes());
        }
        (owner, role, rows, vec![data, unit])
    }
    fn exchange(
        inbox: &mut Inbox<OwnedFd>,
        outbox: &mut Outbox<OwnedFd>,
        envelope: Envelope,
        body: Vec<u8>,
        rights: usize,
    ) {
        let sequence = envelope.sequence;
        let pins: Vec<OwnedFd> = if rights == 1 {
            vec![std::fs::File::open("/dev/null").unwrap().into()]
        } else {
            vec![]
        };
        let sent = pins.iter().map(|fd| fd.try_clone().unwrap()).collect();
        assert_eq!(outbox.prepare(envelope.clone(), sent).unwrap(), sequence);
        outbox.entries.get_mut(&sequence).unwrap().state = SendState::Submitted;
        inbox.retain(envelope.clone(), pins).unwrap();
        inbox.dispatch(sequence, |_, _| Ok(body.clone())).unwrap();
        let mut response = envelope;
        response.operation = Operation::Reply;
        response.body = body;
        outbox.acknowledge(&response).unwrap();
    }
    #[test]
    fn helper_receive_transport_cannot_choose_grammar_from_a_manifest() {
        use crate::network_runtime::ProviderWireFormat;
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
                let (owner, _, rows, records) = group(kind);
                let mut sockets = [-1; 2];
                assert_eq!(
                    unsafe {
                        libc::socketpair(
                            libc::AF_UNIX,
                            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                            0,
                            sockets.as_mut_ptr(),
                        )
                    },
                    0
                );
                let endpoint = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
                let _peer = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
                let mut session = AcceptedSession::from_wire(endpoint, [3; 16], wire).unwrap();
                assert_eq!(session.incoming.wire_format, wire);
                assert_eq!(session.outgoing.wire_format, wire);
                for (envelope, body, rights) in rows {
                    exchange(
                        &mut session.incoming,
                        &mut session.outgoing,
                        envelope,
                        body,
                        rights,
                    );
                }
                let result = session.retain_read_copy(2, records);
                assert_eq!(result.is_ok(), wire == ProviderWireFormat::Abi7Copy4);
                if wire == ProviderWireFormat::Abi8Copy5 {
                    let pin = session.incoming.entries[&1].rights[0].as_raw_fd();
                    assert!(unsafe { libc::fcntl(pin, libc::F_GETFD) } >= 0);
                    assert!(!session.incoming.entries[&2].read_copy_finalized);
                    assert!(session.incoming.entries[&1].read_copy.is_none());
                    assert!(
                        session
                            .incoming
                            .acknowledge_command_completion(2, |_, _| panic!(
                                "legacy helper bytes must not retire a V5 command"
                            ))
                            .is_err()
                    );
                    let views = session.outgoing.acknowledged_group(&[1, 2]).unwrap();
                    assert!(
                        validate_file_observation_group_for_version(
                            owner,
                            17,
                            1,
                            2,
                            &views,
                            wire.copy_version()
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn helper_receive_transport_retains_actual_copy_before_ack_and_exact_group_retirement() {
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            let (owner, _, rows, records) = group(kind);
            let mut sockets = [-1; 2];
            assert_eq!(
                unsafe {
                    libc::socketpair(
                        libc::AF_UNIX,
                        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                        0,
                        sockets.as_mut_ptr(),
                    )
                },
                0
            );
            let endpoint = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
            let _peer = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
            let mut session = AcceptedSession::new(endpoint, [3; 16]).unwrap();
            // Test the exact existing Inbox/Outbox owners; no physical success is claimed.
            for (envelope, body, rights) in rows {
                exchange(
                    &mut session.incoming,
                    &mut session.outgoing,
                    envelope,
                    body,
                    rights,
                );
            }
            assert!(
                session
                    .incoming
                    .acknowledge_command_completion(2, |_, _| panic!(
                        "ACK before actual copy retention"
                    ))
                    .is_err()
            );
            assert_eq!(session.incoming.entries[&1].rights.len(), 1);
            let retained_pin = session.incoming.entries[&1].rights[0].as_raw_fd();
            assert!(unsafe { libc::fcntl(retained_pin, libc::F_GETFD) } >= 0);
            assert!(matches!(
                session.incoming.entries[&2].command_ack,
                CommandAckState::Unsubmitted
            ));
            session
                .incoming
                .retain_read_copy(2, records.clone())
                .unwrap();
            session
                .incoming
                .acknowledge_command_completion(2, |_, _| {
                    Ok(
                        serde_json::to_vec(
                            &serde_json::json!({"Observed": good("ap_ack_command")}),
                        )
                        .unwrap(),
                    )
                })
                .unwrap();
            assert!(
                session
                    .check_incoming_file_observation(owner, 17, 1, 2)
                    .is_err()
            );
            for first in 0..2 {
                let request = Envelope {
                    run: [3; 16],
                    sequence: 3 + first,
                    owner: Some(owner),
                    accept: None,
                    operation: Operation::ReadOriginalCopy,
                    body: serde_json::to_vec(&Request::ReadOriginalCopy {
                        call: 17,
                        command: 91,
                        prepared: 1,
                        first,
                    })
                    .unwrap(),
                };
                let chunk = session
                    .incoming
                    .read_copy_chunk(&request, 0, 17, 91, 1, first, || {
                        panic!("retained helper copy was physically re-read after ACK")
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(chunk.records, records[first as usize..first as usize + 1]);
                assert_eq!(
                    chunk.end,
                    if first == 1 {
                        Some(End::OriginalExit { protocol: true })
                    } else {
                        None
                    }
                );
                let body = serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).unwrap();
                exchange(
                    &mut session.incoming,
                    &mut session.outgoing,
                    request,
                    body,
                    0,
                );
            }
            session
                .check_incoming_file_observation(owner, 17, 1, 2)
                .unwrap();
            let retirement = Envelope {
                run: [3; 16],
                sequence: 5,
                owner: Some(owner),
                accept: None,
                operation: Operation::RetireOriginalFileObservation,
                body: serde_json::to_vec(&Request::RetireOriginalFileObservation {
                    call: 17,
                    prepared: 1,
                    completed: 2,
                })
                .unwrap(),
            };
            exchange(
                &mut session.incoming,
                &mut session.outgoing,
                retirement,
                serde_json::to_vec(&Reply::OriginalFileObservationRetired(good(
                    "ap_retire_auxiliary_task",
                )))
                .unwrap(),
                0,
            );
            session
                .retire_incoming_file_observation(owner, 17, 1, 2)
                .unwrap();
            session
                .retire_outgoing_file_observation(owner, 17, 1, 2, 5)
                .unwrap();
            session.incoming.retire_sent_original_ack(5).unwrap();
            assert!(session.incoming.entries.is_empty());
            assert!(session.outgoing.entries.is_empty());
            assert_eq!(unsafe { libc::fcntl(retained_pin, libc::F_GETFD) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
    }
    #[test]
    fn helper_receive_transport_refuses_changed_role_file_fdget_count_and_missing_protocol() {
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            let (owner, _, rows, _) = group(kind);
            let check = |rows: &Rows| {
                validate_file_observation_group(
                    owner,
                    17,
                    1,
                    2,
                    &rows
                        .iter()
                        .map(|(e, b, n)| (e, b.as_slice(), *n))
                        .collect::<Vec<_>>(),
                )
            };
            check(&rows).unwrap();
            for case in 0..12 {
                let mut wrong = rows.clone();
                let mut reply: Reply = serde_json::from_slice(&wrong[1].1).unwrap();
                let Reply::OriginalFileObservation {
                    selection,
                    effect: Some(effect),
                } = &mut reply
                else {
                    unreachable!()
                };
                match case {
                    0 => effect.raw.command.operation = 23,
                    1 => selection.raw.file += 1,
                    2 => selection.raw.provider += 1,
                    3 => selection.raw.fdput_flags = 1,
                    4 => selection.raw.original_count += 1,
                    5 => selection.raw.address_length ^= libc::MSG_PEEK,
                    6 => selection.raw.user_address += 1,
                    7 => {
                        effect.raw.read_copy = None;
                    }
                    8 => effect.raw.read_copy.as_mut().unwrap().present = 0,
                    9 => effect.raw.original.complete = 0,
                    10 => effect.raw.original.problem = 1,
                    11 => effect.raw.command.returned = -4096,
                    _ => unreachable!(),
                }
                effect.raw.original.selection = selection.raw.clone();
                wrong[1].1 = serde_json::to_vec(&reply).unwrap();
                assert!(check(&wrong).is_err(), "helper identity mutation {case}");
            }
        }
    }
}
