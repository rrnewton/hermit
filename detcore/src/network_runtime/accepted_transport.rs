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
    Bootstrap,
    EnrollListener,
    DrainCreations,
    PrepareSetter,
    FinishSetter,
    MatchAccepted,
    ReleaseAccepted,
    Reply,
}

impl Operation {
    fn rights(self) -> usize {
        match self {
            Self::Bootstrap => 2, // actual controller pidfd + private controller endpoint
            Self::EnrollListener | Self::PrepareSetter | Self::MatchAccepted => 2, // socket + task pidfd
            Self::DrainCreations | Self::FinishSetter | Self::ReleaseAccepted | Self::Reply => 0,
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
    pub sequence: u64,
    pub owner: Option<NetworkStreamOwner>,
    pub accept: Option<NetworkAcceptLeaseId>,
    pub body: Vec<u8>,
}
fn observation_request(envelope: &Envelope) -> bool {
    use super::accepted_provider::Request;
    envelope.operation == Operation::DrainCreations
        && matches!(
            serde_json::from_slice::<Request>(&envelope.body),
            Ok(Request::ReadStatus | Request::ReadCreation { .. } | Request::AwaitCreation { .. })
        )
}
fn receipt(envelope: &Envelope, body: &[u8]) -> ObservationReceipt {
    ObservationReceipt {
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
    entries: BTreeMap<u64, Outgoing<T>>,
    next: u64,
}
impl<T> Default for Outbox<T> {
    fn default() -> Self {
        Self {
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

#[derive(Debug)]
struct Incoming<T> {
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
    retired_observation: Option<ObservationReceipt>,
    entries: BTreeMap<u64, Incoming<T>>,
    next: u64,
}
impl<T> Default for Inbox<T> {
    fn default() -> Self {
        Self {
            retired_observation: None,
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
            Operation::EnrollListener | Operation::MatchAccepted | Operation::FinishSetter
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
        entry.command_ack = CommandAckState::Submitted;
        let status = effect(&entry.envelope, body)?;
        entry.command_ack = CommandAckState::Completed(status.clone());
        Ok(status)
    }
    fn retire_observation(&mut self, receipt: &ObservationReceipt) -> io::Result<()> {
        if self.retired_observation.as_ref() == Some(receipt) {
            return Ok(()); // duplicate exact ACK, never re-run the observation
        }
        if self
            .retired_observation
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
            || entry.envelope.owner != receipt.owner
            || entry.envelope.accept != receipt.accept
            || entry.state != IncomingState::Completed(receipt.body.clone())
        {
            return Err(protocol(
                "observation retirement changed its exact completed read",
            ));
        }
        self.entries.remove(&receipt.sequence);
        self.retired_observation = Some(receipt.clone());
        Ok(())
    }
    fn begin_observation(&mut self, sequence: u64) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&sequence)
            .ok_or_else(|| protocol("unknown pending observation"))?;
        if !observation_request(&entry.envelope)
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
        if !observation_request(&entry.envelope)
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
    pub(super) fn wait_transport(&self, deadline: Instant) -> io::Result<()> {
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(10) as i32;
        let mut event = libc::pollfd {
            fd: self.endpoint.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut event, 1, millis) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(protocol("accepted transport observation failed"));
        }
        Ok(())
    }
    /// Caller obtained this exact private endpoint through startup ownership;
    /// a pathname, Config integer, or channel EOF cannot grant this authority.
    pub(super) fn new(endpoint: OwnedFd, run: [u8; 16]) -> Result<Self, (io::Error, OwnedFd)> {
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
            outgoing: Outbox::default(),
            incoming: Inbox::default(),
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
        let mut reply = entry.envelope.clone();
        reply.operation = Operation::Reply;
        reply.body = body.clone();
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

#[cfg(test)]
mod tests {
    use super::*;
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
