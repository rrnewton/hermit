//! Bounded Herdr event-socket client for reply-marker and settled-state wakes.

use std::fmt;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

const MAX_FRAME_BYTES: usize = 2 * 1_024 * 1_024;
pub(crate) const MAX_PATTERNS: usize = 128;
pub(crate) const MAX_PATTERN_BYTES: usize = 32 * 1_024;
pub(crate) const MAX_TOTAL_PATTERN_BYTES: usize = 96 * 1_024;
const MAX_LINES: u32 = 100_000;
const MAX_EVENTS_PER_WAIT: usize = 64;
const MAX_EVENT_BYTES_PER_WAIT: usize = 8 * 1_024 * 1_024;
const COMPACT_AFTER_BYTES: usize = 64 * 1_024;

#[derive(Debug)]
pub(crate) struct ChatEventError {
    code: &'static str,
    detail: String,
}

impl ChatEventError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > 2_000 {
            let mut boundary = 2_000;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { code, detail }
    }
}

impl fmt::Display for ChatEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ChatEventError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PaneEvent {
    Output {
        matched_line: String,
        text: String,
        truncated: bool,
        revision: Option<u64>,
    },
    Settled {
        status: String,
    },
    /// Herdr reported that the agent started work, as at the start of a turn the service did not
    /// prompt.
    Working,
}

/// A cloneable, descriptor-backed interrupt for one blocking pane-event wait.
pub(crate) struct PaneEventWake {
    stream: UnixStream,
}

impl PaneEventWake {
    pub(crate) fn wake(&self) {
        signal_wake_with(|| {
            let byte = [1_u8];
            // SAFETY: `byte` is live for this call and the cloned wake descriptor remains owned
            // by `self`. Nonblocking send cannot retain the pointer after returning.
            let result = unsafe {
                libc::send(
                    self.stream.as_raw_fd(),
                    byte.as_ptr().cast(),
                    byte.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else if result == 0 {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "wake socket accepted zero bytes",
                ))
            } else {
                Ok(())
            }
        });
    }
}

fn signal_wake_with(mut send: impl FnMut() -> io::Result<()>) {
    loop {
        match send() {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::BrokenPipe
                ) =>
            {
                // A full nonblocking socket already contains a sticky wake. A closed peer no
                // longer has a waiter to interrupt. Both cases complete this best-effort signal.
                return;
            }
            Err(_) => {
                // The owning service retains a finite outer shutdown deadline for other errors.
                return;
            }
        }
    }
}

pub(crate) struct PaneEventStream {
    pane_id: String,
    patterns: Vec<String>,
    stream: UnixStream,
    wake_read: UnixStream,
    wake_write: UnixStream,
    buffer: Vec<u8>,
    buffer_offset: usize,
    deferred_error: Option<ChatEventError>,
    request_id: String,
    acknowledged: bool,
}

impl PaneEventStream {
    pub(crate) fn connect(
        socket_path: &Path,
        pane_id: &str,
        patterns: Vec<String>,
        lines: u32,
        connect_timeout: Duration,
    ) -> Result<Self, ChatEventError> {
        validate_configuration(socket_path, pane_id, &patterns, lines, connect_timeout)?;
        let deadline = Instant::now().checked_add(connect_timeout).ok_or_else(|| {
            ChatEventError::new("invalid_deadline", "event connection deadline is too large")
        })?;
        let stream = connect_unix(socket_path, deadline)
            .map_err(|error| ChatEventError::new("connection_failed", error.to_string()))?;
        validate_peer(&stream)
            .map_err(|error| ChatEventError::new("unsafe_peer", error.to_string()))?;
        let (wake_read, wake_write) = UnixStream::pair()
            .map_err(|error| ChatEventError::new("wake_failed", error.to_string()))?;
        wake_read
            .set_nonblocking(true)
            .map_err(|error| ChatEventError::new("wake_failed", error.to_string()))?;
        wake_write
            .set_nonblocking(true)
            .map_err(|error| ChatEventError::new("wake_failed", error.to_string()))?;
        let request_id = format!(
            "agentctl:chat-output:{}",
            crate::chat_runtime::random_operation_uuid()
                .map_err(|error| ChatEventError::new("random_failed", error.to_string()))?
        );
        let mut subscriptions = patterns
            .iter()
            .map(|pattern| {
                json!({
                    "type": "pane.output_matched",
                    "pane_id": pane_id,
                    "source": "recent_unwrapped",
                    "lines": lines,
                    "strip_ansi": true,
                    "match": {"type": "regex", "value": pattern},
                })
            })
            .collect::<Vec<_>>();
        subscriptions.extend(["idle", "done", "working"].into_iter().map(|status| {
            json!({
                "type": "pane.agent_status_changed",
                "pane_id": pane_id,
                "agent_status": status,
            })
        }));
        let mut frame = serde_json::to_vec(&json!({
            "id": request_id,
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        }))
        .map_err(|error| ChatEventError::new("invalid_request", error.to_string()))?;
        if frame.len() >= MAX_FRAME_BYTES {
            return Err(ChatEventError::new(
                "request_too_large",
                "Herdr subscription request exceeds the frame limit",
            ));
        }
        frame.push(b'\n');
        let mut client = Self {
            pane_id: pane_id.to_owned(),
            patterns,
            stream,
            wake_read,
            wake_write,
            buffer: Vec::new(),
            buffer_offset: 0,
            deferred_error: None,
            request_id,
            acknowledged: false,
        };
        write_with_deadline(&mut client.stream, &frame, deadline)
            .map_err(|error| ChatEventError::new("subscription_write_failed", error.to_string()))?;
        while !client.acknowledged {
            let frame = client.next_frame(Some(deadline))?.ok_or_else(|| {
                ChatEventError::new(
                    "subscription_timeout",
                    "Herdr did not acknowledge the subscription",
                )
            })?;
            if client.decode(&frame)?.is_some() {
                return Err(ChatEventError::new(
                    "invalid_frame",
                    "Herdr emitted an event before subscription acknowledgement",
                ));
            }
        }
        Ok(client)
    }

    pub(crate) fn wake_handle(&self) -> Result<PaneEventWake, ChatEventError> {
        self.wake_write
            .try_clone()
            .map(|stream| PaneEventWake { stream })
            .map_err(|error| ChatEventError::new("wake_failed", error.to_string()))
    }

    pub(crate) fn pane_id(&self) -> &str {
        &self.pane_id
    }

    pub(crate) fn wait(&mut self, timeout: Duration) -> Result<Vec<PaneEvent>, ChatEventError> {
        if let Some(error) = self.deferred_error.take() {
            return Err(error);
        }
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            ChatEventError::new("invalid_deadline", "event wait deadline is too large")
        })?;
        let Some(frame) = self.next_frame(Some(deadline))? else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        let event = self.decode(&frame)?.ok_or_else(|| {
            ChatEventError::new(
                "invalid_frame",
                "unexpected acknowledgement during event wait",
            )
        })?;
        let mut decoded_bytes = frame.len();
        events.push(event);
        while events.len() < MAX_EVENTS_PER_WAIT && decoded_bytes < MAX_EVENT_BYTES_PER_WAIT {
            let frame = match self.next_frame(None) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(error) if !events.is_empty() => {
                    self.deferred_error = Some(error);
                    break;
                }
                Err(error) => return Err(error),
            };
            let event = self.decode(&frame)?.ok_or_else(|| {
                ChatEventError::new(
                    "invalid_frame",
                    "unexpected acknowledgement during event wait",
                )
            })?;
            decoded_bytes = decoded_bytes.saturating_add(frame.len());
            events.push(event);
        }
        Ok(events)
    }

    fn next_frame(&mut self, deadline: Option<Instant>) -> Result<Option<Vec<u8>>, ChatEventError> {
        loop {
            let unread = &self.buffer[self.buffer_offset..];
            if let Some(newline) = unread.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_FRAME_BYTES {
                    return Err(ChatEventError::new(
                        "frame_limit_exceeded",
                        "Herdr event frame exceeds 2 MiB",
                    ));
                }
                let start = self.buffer_offset;
                let end = start + newline;
                let frame = self.buffer[start..end].to_vec();
                self.buffer_offset = end + 1;
                self.compact_buffer();
                return Ok(Some(frame));
            }
            if unread.len() > MAX_FRAME_BYTES {
                return Err(ChatEventError::new(
                    "frame_limit_exceeded",
                    "Herdr event frame exceeds 2 MiB",
                ));
            }
            let ready = poll_readable(
                self.stream.as_raw_fd(),
                self.wake_read.as_raw_fd(),
                deadline,
            )
            .map_err(|error| ChatEventError::new("connection_failed", error.to_string()))?;
            match ready {
                Readiness::TimedOut => return Ok(None),
                Readiness::Wake => {
                    drain_wake(&mut self.wake_read)
                        .map_err(|error| ChatEventError::new("wake_failed", error.to_string()))?;
                    return Ok(None);
                }
                Readiness::Socket => {}
            }
            let mut chunk = [0_u8; 64 * 1_024];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    let detail = if self.buffer.is_empty() {
                        "Herdr closed the event subscription"
                    } else {
                        "Herdr closed the event subscription mid-frame"
                    };
                    return Err(ChatEventError::new("stream_eof", detail));
                }
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(ChatEventError::new("connection_failed", error.to_string()))
                }
            }
        }
    }

    fn compact_buffer(&mut self) {
        if self.buffer_offset == self.buffer.len() {
            self.buffer.clear();
            self.buffer_offset = 0;
        } else if self.buffer_offset >= COMPACT_AFTER_BYTES
            && self.buffer_offset >= self.buffer.len() / 2
        {
            self.buffer.copy_within(self.buffer_offset.., 0);
            self.buffer.truncate(self.buffer.len() - self.buffer_offset);
            self.buffer_offset = 0;
        }
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Option<PaneEvent>, ChatEventError> {
        let document: Value = chat_subscription_plugin::decode_strict_json(frame)
            .map_err(|error| ChatEventError::new("invalid_frame", error.to_string()))?;
        let object = document
            .as_object()
            .ok_or_else(|| ChatEventError::new("invalid_frame", "event frame is not an object"))?;
        if let Some(error) = object.get("error") {
            if self.acknowledged
                || object.get("id").and_then(Value::as_str) != Some(&self.request_id)
            {
                return Err(ChatEventError::new(
                    "invalid_frame",
                    "subscription rejection is not bound to the pending request",
                ));
            }
            let error = object_value(error, "subscription error")?;
            return Err(ChatEventError::new(
                "subscription_rejected",
                format!(
                    "{}: {}",
                    string_value(error, "code", "subscription error")?,
                    string_value(error, "message", "subscription error")?
                ),
            ));
        }
        if let Some(result) = object.get("result") {
            let result = object_value(result, "subscription response")?;
            if self.acknowledged
                || object.get("id").and_then(Value::as_str) != Some(&self.request_id)
                || result.get("type").and_then(Value::as_str) != Some("subscription_started")
            {
                return Err(ChatEventError::new(
                    "invalid_frame",
                    "unexpected subscription acknowledgement",
                ));
            }
            self.acknowledged = true;
            return Ok(None);
        }
        if !self.acknowledged {
            return Err(ChatEventError::new(
                "invalid_frame",
                "event arrived before subscription acknowledgement",
            ));
        }
        match object.get("event").and_then(Value::as_str) {
            Some("pane.agent_status_changed") => {
                let data = object_value(
                    object.get("data").ok_or_else(|| {
                        ChatEventError::new("invalid_frame", "status event has no data")
                    })?,
                    "status event",
                )?;
                if data.get("pane_id").and_then(Value::as_str) != Some(&self.pane_id) {
                    return Err(ChatEventError::new(
                        "invalid_frame",
                        "status event belongs to a different pane",
                    ));
                }
                match string_value(data, "agent_status", "status event")? {
                    status @ ("idle" | "done") => Ok(Some(PaneEvent::Settled {
                        status: status.to_owned(),
                    })),
                    "working" => Ok(Some(PaneEvent::Working)),
                    _ => Err(ChatEventError::new(
                        "invalid_frame",
                        "status event is not idle, done, or working",
                    )),
                }
            }
            Some("pane.output_matched") if !self.patterns.is_empty() => {
                let data = object_value(
                    object.get("data").ok_or_else(|| {
                        ChatEventError::new("invalid_frame", "output event has no data")
                    })?,
                    "output event",
                )?;
                let read = object_value(
                    data.get("read").ok_or_else(|| {
                        ChatEventError::new("invalid_frame", "output event has no read snapshot")
                    })?,
                    "output snapshot",
                )?;
                if data.get("pane_id").and_then(Value::as_str) != Some(&self.pane_id)
                    || read.get("pane_id").and_then(Value::as_str) != Some(&self.pane_id)
                    || read.get("source").and_then(Value::as_str) != Some("recent_unwrapped")
                    || read.get("format").and_then(Value::as_str) != Some("text")
                {
                    return Err(ChatEventError::new(
                        "invalid_frame",
                        "output snapshot has mismatched pane, source, or format",
                    ));
                }
                let matched_line = string_value(data, "matched_line", "output event")?;
                if matched_line.len() > 1_024 || matched_line.contains(['\0', '\n', '\r']) {
                    return Err(ChatEventError::new(
                        "invalid_frame",
                        "output matched line is invalid",
                    ));
                }
                let text = string_value(read, "text", "output snapshot")?.to_owned();
                let truncated =
                    read.get("truncated")
                        .and_then(Value::as_bool)
                        .ok_or_else(|| {
                            ChatEventError::new(
                                "invalid_frame",
                                "snapshot truncation flag is invalid",
                            )
                        })?;
                let revision = match read.get("revision") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(value.as_u64().ok_or_else(|| {
                        ChatEventError::new("invalid_frame", "snapshot revision is invalid")
                    })?),
                };
                Ok(Some(PaneEvent::Output {
                    matched_line: matched_line.to_owned(),
                    text,
                    truncated,
                    revision,
                }))
            }
            _ => Err(ChatEventError::new(
                "invalid_frame",
                "unexpected Herdr subscription event",
            )),
        }
    }
}

fn validate_configuration(
    socket_path: &Path,
    pane_id: &str,
    patterns: &[String],
    lines: u32,
    timeout: Duration,
) -> Result<(), ChatEventError> {
    let pattern_bytes = patterns.iter().fold(0_usize, |total, pattern| {
        total.saturating_add(pattern.len())
    });
    if !socket_path.is_absolute()
        || socket_path.as_os_str().as_bytes().contains(&0)
        || pane_id.is_empty()
        || pane_id.len() > 512
        || pane_id.contains(['\0', '\n', '\r'])
        || patterns.len() > MAX_PATTERNS
        || pattern_bytes > MAX_TOTAL_PATTERN_BYTES
        || patterns
            .iter()
            .any(|pattern| pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES)
        || lines == 0
        || lines > MAX_LINES
        || timeout.is_zero()
        || timeout > Duration::from_secs(300)
    {
        return Err(ChatEventError::new(
            "invalid_configuration",
            "invalid socket, pane, patterns, line count, or connection timeout",
        ));
    }
    Ok(())
}

fn connect_unix(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= 108 || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "event socket path does not fit sockaddr_un",
        ));
    }
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *destination = source as libc::c_char;
    }
    let length = (size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    let result = unsafe {
        libc::connect(
            owned.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            length,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error);
        }
        if !poll_fd(owned.as_raw_fd(), libc::POLLOUT, Some(deadline))? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "event socket connect timed out",
            ));
        }
        let mut socket_error: libc::c_int = 0;
        let mut option_length = size_of::<libc::c_int>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                owned.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &raw mut option_length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if socket_error != 0 {
            return Err(io::Error::from_raw_os_error(socket_error));
        }
    }
    Ok(UnixStream::from(owned))
}

fn validate_peer(stream: &UnixStream) -> io::Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let uid = std::fs::metadata("/proc/self")?.uid();
    if credentials.uid != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Herdr event peer belongs to a different UID",
        ));
    }
    Ok(())
}

fn write_with_deadline(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        match stream.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "event socket wrote zero bytes",
                ))
            }
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !poll_fd(stream.as_raw_fd(), libc::POLLOUT, Some(deadline))? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "event subscription write timed out",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Readiness {
    Socket,
    Wake,
    TimedOut,
}

fn poll_readable(
    descriptor: libc::c_int,
    wake_descriptor: libc::c_int,
    deadline: Option<Instant>,
) -> io::Result<Readiness> {
    loop {
        let timeout = poll_timeout(deadline)?;
        let mut polls = [
            libc::pollfd {
                fd: descriptor,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_descriptor,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result =
            unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
        if result > 0 {
            if polls.iter().any(|poll| poll.revents & libc::POLLNVAL != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "event or wake socket became invalid",
                ));
            }
            if polls[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                return Ok(Readiness::Wake);
            }
            if polls[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                return Ok(Readiness::Socket);
            }
            continue;
        }
        if result == 0 {
            return Ok(Readiness::TimedOut);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn poll_fd(descriptor: libc::c_int, events: i16, deadline: Option<Instant>) -> io::Result<bool> {
    loop {
        let timeout = poll_timeout(deadline)?;
        let mut poll = libc::pollfd {
            fd: descriptor,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, timeout) };
        if result > 0 {
            if poll.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "event socket became invalid",
                ));
            }
            return Ok(poll.revents & (events | libc::POLLHUP | libc::POLLERR) != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn poll_timeout(deadline: Option<Instant>) -> io::Result<i32> {
    match deadline {
        None => Ok(0),
        Some(deadline) => {
            let now = Instant::now();
            if now >= deadline {
                return Ok(0);
            }
            Ok(deadline
                .saturating_duration_since(now)
                .as_millis()
                .clamp(1, i32::MAX as u128) as i32)
        }
    }
}

fn drain_wake(stream: &mut UnixStream) -> io::Result<()> {
    let mut buffer = [0_u8; 256];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn object_value<'a>(
    value: &'a Value,
    label: &str,
) -> Result<&'a Map<String, Value>, ChatEventError> {
    value
        .as_object()
        .ok_or_else(|| ChatEventError::new("invalid_frame", format!("{label} is not an object")))
}

fn string_value<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a str, ChatEventError> {
    object.get(key).and_then(Value::as_str).ok_or_else(|| {
        ChatEventError::new("invalid_frame", format!("{label}.{key} is not a string"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_socket() -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "agentctl-chat-events-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("create event fixture directory");
        (directory.join("events.sock"), directory)
    }

    fn wire(value: Value) -> Vec<u8> {
        let mut encoded = serde_json::to_vec(&value).expect("encode fixture frame");
        encoded.push(b'\n');
        encoded
    }

    fn output_event(text: &str, truncated: bool, revision: Option<u64>) -> Value {
        json!({
            "event": "pane.output_matched",
            "data": {
                "pane_id": "w1:p2",
                "matched_line": "</CHAT_REPLY_nonce_1>",
                "read": {
                    "pane_id": "w1:p2",
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "source": "recent_unwrapped",
                    "format": "text",
                    "text": text,
                    "revision": revision,
                    "truncated": truncated,
                },
            },
        })
    }

    fn serve(
        listener: UnixListener,
        response: impl FnOnce(Value) -> Vec<u8> + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept event client");
            let mut request = String::new();
            BufReader::new(connection.try_clone().expect("clone fixture socket"))
                .read_line(&mut request)
                .expect("read subscription request");
            let request: Value = serde_json::from_str(&request).expect("decode request");
            connection
                .write_all(&response(request))
                .expect("write fixture response");
        })
    }

    #[test]
    fn output_pattern_configuration_bounds_individual_total_and_count() {
        let validate = |patterns: Vec<String>| {
            validate_configuration(
                Path::new("/tmp/events.sock"),
                "workspace:pane",
                &patterns,
                4_000,
                Duration::from_secs(2),
            )
        };
        assert!(validate(vec!["x".repeat(MAX_PATTERN_BYTES); 3]).is_ok());
        assert!(validate(vec!["x".repeat(MAX_PATTERN_BYTES + 1)]).is_err());
        let mut over_total = vec!["x".repeat(MAX_PATTERN_BYTES); 3];
        over_total.push("x".to_owned());
        assert!(validate(over_total).is_err());
        assert!(validate(vec!["x".to_owned(); MAX_PATTERNS]).is_ok());
        assert!(validate(vec!["x".to_owned(); MAX_PATTERNS + 1]).is_err());
        assert!(validate(vec![String::new()]).is_err());
    }

    #[test]
    fn subscribes_and_drains_multiple_events_with_snapshot_metadata() {
        let (socket, directory) = temporary_socket();
        let listener = UnixListener::bind(&socket).expect("bind event fixture");
        let server = serve(listener, |request| {
            assert_eq!(request["method"], "events.subscribe");
            let subscriptions = request["params"]["subscriptions"]
                .as_array()
                .expect("subscription array");
            assert_eq!(subscriptions.len(), 5);
            assert_eq!(subscriptions[0]["type"], "pane.output_matched");
            assert_eq!(subscriptions[2]["agent_status"], "idle");
            assert_eq!(subscriptions[3]["agent_status"], "done");
            assert_eq!(subscriptions[4]["agent_status"], "working");
            let mut frames = wire(json!({
                "id": request["id"],
                "result": {"type": "subscription_started"},
            }));
            frames.extend(wire(output_event("first", true, None)));
            frames.extend(wire(json!({
                "event": "pane.agent_status_changed",
                "data": {"pane_id": "w1:p2", "agent_status": "idle"},
            })));
            frames.extend(wire(json!({
                "event": "pane.agent_status_changed",
                "data": {"pane_id": "w1:p2", "agent_status": "working"},
            })));
            frames
        });

        let mut stream = PaneEventStream::connect(
            &socket,
            "w1:p2",
            vec!["^first$".to_owned(), "^second$".to_owned()],
            4_000,
            Duration::from_secs(2),
        )
        .expect("connect event stream");
        assert_eq!(
            stream.wait(Duration::from_secs(1)).expect("drain events"),
            vec![
                PaneEvent::Output {
                    matched_line: "</CHAT_REPLY_nonce_1>".to_owned(),
                    text: "first".to_owned(),
                    truncated: true,
                    revision: None,
                },
                PaneEvent::Settled {
                    status: "idle".to_owned(),
                },
                PaneEvent::Working,
            ]
        );
        server.join().expect("join event fixture");
        fs::remove_dir_all(directory).expect("remove event fixture");
    }

    #[test]
    fn refuses_mismatched_snapshot_identity() {
        let (socket, directory) = temporary_socket();
        let listener = UnixListener::bind(&socket).expect("bind event fixture");
        let server = serve(listener, |request| {
            let mut frames = wire(json!({
                "id": request["id"],
                "result": {"type": "subscription_started"},
            }));
            let mut event = output_event("reply", false, Some(7));
            event["data"]["read"]["pane_id"] = json!("w1:p9");
            frames.extend(wire(event));
            frames
        });
        let mut stream = PaneEventStream::connect(
            &socket,
            "w1:p2",
            vec!["^reply$".to_owned()],
            4_000,
            Duration::from_secs(2),
        )
        .expect("connect event stream");
        let error = stream
            .wait(Duration::from_secs(1))
            .expect_err("mismatched pane must fail");
        assert_eq!(error.code, "invalid_frame");
        server.join().expect("join event fixture");
        fs::remove_dir_all(directory).expect("remove event fixture");
    }

    #[test]
    fn refuses_a_status_event_for_a_status_it_did_not_subscribe_to() {
        // The stream subscribes to `idle`, `done`, and `working` only, so a status event for
        // another status, such as Herdr's `blocked` or `unknown`, is a malformed frame.
        for status in ["blocked", "unknown"] {
            let (socket, directory) = temporary_socket();
            let listener = UnixListener::bind(&socket).expect("bind event fixture");
            let server = serve(listener, move |request| {
                let mut frames = wire(json!({
                    "id": request["id"],
                    "result": {"type": "subscription_started"},
                }));
                frames.extend(wire(json!({
                    "event": "pane.agent_status_changed",
                    "data": {"pane_id": "w1:p2", "agent_status": status},
                })));
                frames
            });
            let mut stream = PaneEventStream::connect(
                &socket,
                "w1:p2",
                Vec::new(),
                4_000,
                Duration::from_secs(2),
            )
            .expect("connect event stream");
            let error = stream
                .wait(Duration::from_secs(1))
                .expect_err("a status it did not subscribe to must fail");
            assert_eq!(error.code, "invalid_frame", "{status}");
            assert_eq!(
                error.detail, "status event is not idle, done, or working",
                "{status}"
            );
            server.join().expect("join event fixture");
            fs::remove_dir_all(directory).expect("remove event fixture");
        }
    }

    #[test]
    fn wake_interrupts_a_blocking_event_wait_without_polling() {
        let (socket, directory) = temporary_socket();
        let listener = UnixListener::bind(&socket).expect("bind event fixture");
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept event client");
            let mut request = String::new();
            BufReader::new(connection.try_clone().expect("clone fixture socket"))
                .read_line(&mut request)
                .expect("read subscription request");
            let request: Value = serde_json::from_str(&request).expect("decode request");
            connection
                .write_all(&wire(json!({
                    "id": request["id"],
                    "result": {"type": "subscription_started"},
                })))
                .expect("write acknowledgement");
            released.recv().expect("release fixture connection");
        });
        let mut stream =
            PaneEventStream::connect(&socket, "w1:p2", Vec::new(), 4_000, Duration::from_secs(2))
                .expect("connect event stream");
        let wake = stream.wake_handle().expect("clone wake handle");
        let waiter = thread::spawn(move || stream.wait(Duration::from_secs(30)));
        wake.wake();
        assert!(waiter
            .join()
            .expect("join event waiter")
            .expect("wake event wait")
            .is_empty());
        release.send(()).expect("release fixture connection");
        server.join().expect("join event fixture");
        fs::remove_dir_all(directory).expect("remove event fixture");
    }

    #[test]
    fn wake_before_wait_is_sticky() {
        let (socket, directory) = temporary_socket();
        let listener = UnixListener::bind(&socket).expect("bind event fixture");
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept event client");
            let mut request = String::new();
            BufReader::new(connection.try_clone().expect("clone fixture socket"))
                .read_line(&mut request)
                .expect("read subscription request");
            let request: Value = serde_json::from_str(&request).expect("decode request");
            connection
                .write_all(&wire(json!({
                    "id": request["id"],
                    "result": {"type": "subscription_started"},
                })))
                .expect("write acknowledgement");
            released.recv().expect("release fixture connection");
        });
        let mut stream =
            PaneEventStream::connect(&socket, "w1:p2", Vec::new(), 4_000, Duration::from_secs(2))
                .expect("connect event stream");
        let wake = stream.wake_handle().expect("clone wake handle");

        wake.wake();
        let started = Instant::now();
        assert!(stream
            .wait(Duration::from_secs(30))
            .expect("consume pre-armed wake")
            .is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "wake queued before wait was not sticky: {:?}",
            started.elapsed()
        );

        release.send(()).expect("release fixture connection");
        server.join().expect("join event fixture");
        fs::remove_dir_all(directory).expect("remove event fixture");
    }

    #[test]
    fn wake_retries_interrupted_send_until_success() {
        let mut attempts = 0_u8;
        signal_wake_with(|| {
            attempts += 1;
            if attempts == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        });
        assert_eq!(attempts, 2);
    }

    #[test]
    fn wake_retries_interrupted_send_then_accepts_existing_sticky_byte() {
        let mut attempts = 0_u8;
        signal_wake_with(|| {
            attempts += 1;
            if attempts == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        });
        assert_eq!(attempts, 2);
    }

    #[test]
    fn one_wait_has_a_bounded_event_population_and_preserves_remainder() {
        let (socket, directory) = temporary_socket();
        let listener = UnixListener::bind(&socket).expect("bind event fixture");
        let server = serve(listener, |request| {
            let mut frames = wire(json!({
                "id": request["id"],
                "result": {"type": "subscription_started"},
            }));
            for index in 0..(MAX_EVENTS_PER_WAIT + 1) {
                frames.extend(wire(output_event(
                    &format!("reply-{index}"),
                    false,
                    Some(index as u64),
                )));
            }
            frames
        });
        let mut stream = PaneEventStream::connect(
            &socket,
            "w1:p2",
            vec!["^reply".to_owned()],
            4_000,
            Duration::from_secs(2),
        )
        .expect("connect event stream");
        assert_eq!(
            stream
                .wait(Duration::from_secs(1))
                .expect("first bounded drain")
                .len(),
            MAX_EVENTS_PER_WAIT
        );
        assert_eq!(
            stream
                .wait(Duration::from_secs(1))
                .expect("drain preserved remainder")
                .len(),
            1
        );
        server.join().expect("join event fixture");
        fs::remove_dir_all(directory).expect("remove event fixture");
    }
}
