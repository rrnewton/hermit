//! Bounded bytes committed by the original task's TCP Sendto. These bytes come
//! from the provider's completed skb sequence interval, never a guest reread.
use std::io;

use serde::Deserialize;
use serde::Serialize;

use super::accepted_provider::OriginalEffect;
use super::accepted_provider_ffi;

pub(super) fn classify(
    pin: &std::os::fd::OwnedFd,
) -> io::Result<crate::network_replay::original_connect::Pin> {
    use std::os::fd::AsRawFd;

    use crate::network_replay::original_connect::Pin;
    let classified = super::native_peer::classify_original(pin)?;
    let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0
        || !matches!(
            classified,
            Pin::Socket {
                domain: libc::AF_INET | libc::AF_INET6,
                kind: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP
            }
        )
    {
        return Err(io::Error::other(
            "Sendto requires actual nonblocking TCP OFD",
        ));
    }
    Ok(classified)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Capture {
    pub(super) provider: u64,
    pub(super) command: u64,
    pub(super) call: u64,
    pub(super) task: u64,
    pub(super) task_start: u64,
    pub(super) returned: i64,
    pub(super) summary: [u64; 8],
    pub(super) bytes: Vec<u8>,
}
impl From<accepted_provider_ffi::OriginalSendCapture> for Capture {
    fn from(raw: accepted_provider_ffi::OriginalSendCapture) -> Self {
        Self {
            provider: raw.provider,
            command: raw.command,
            call: raw.call,
            task: raw.task,
            task_start: raw.task_start,
            returned: raw.returned,
            summary: raw.summary,
            bytes: raw.bytes.to_vec(),
        }
    }
}
impl Capture {
    pub(super) fn validate(&self, effect: &OriginalEffect) -> io::Result<()> {
        let result = &effect.original;
        let selected = &result.selection;
        let command = &effect.command;
        let [
            version,
            file,
            requested,
            captured,
            before,
            after,
            returned,
            complete,
        ] = self.summary;
        let count = usize::try_from(captured).map_err(io::Error::other)?;
        if effect.socket.is_some()
            || effect.read_copy.is_some()
            || effect.blocking_send.is_some()
            || version != 1
            || complete != 1
            || !(1..=512).contains(&requested)
            || !(-4095..=requested as i64).contains(&self.returned)
            || captured != self.returned.max(0) as u64
            || returned != self.returned as u64
            || before > u64::from(u32::MAX)
            || after > u64::from(u32::MAX)
            || (after as u32).wrapping_sub(before as u32) as u64 != captured
            || self.bytes.len() != 512
            || count > 512
            || self
                .bytes
                .get(count..)
                .is_none_or(|tail| tail.iter().any(|b| *b != 0))
            || self.provider == 0
            || self.command == 0
            || self.call == 0
            || self.task == 0
            || self.task_start == 0
            || file == 0
            || (
                self.provider,
                self.command,
                self.call,
                self.task,
                self.task_start,
            ) != (
                selected.provider,
                selected.command,
                selected.call,
                selected.task,
                selected.task_start,
            )
            || file != selected.file
            || requested != selected.original_count
            || command.operation != 24
            || command.command != self.command
            || command.identity.provider != self.provider
            || command.task != self.task
            || command.start_boottime != self.task_start
            || command.original_count != requested
            || command.phase != 1
            || command.reserved != 0
            || i64::from(command.returned) != self.returned
            || i64::from(result.returned) != self.returned
            || result.complete != 1
            || result.problem != 0
            || result.reserved != 0
            || result.address.len() != 128
            || result.address[64..].iter().any(|b| *b != 0)
            || result.address[..64]
                .as_chunks::<8>()
                .0
                .iter()
                .zip(self.summary)
                .any(|(bytes, value)| *bytes != value.to_ne_bytes())
            || selected.ready != 1
            || selected.table == 0
            || selected.user_address == 0
            || selected.fdput_flags > 1
            || !matches!(selected.address_length, libc::MSG_NOSIGNAL | 0x4040)
            || result.copy_entered != 0
            || result.copy_returned != 0
            || result.copy_remaining != 0
            || result.audit_entered != 0
            || result.audit_returned != 0
            || result.audit_result != 0
            || result.security_entered != 0
            || result.security_returned != 0
            || result.security_result != 0
        {
            return Err(io::Error::other(
                "original Sendto capture changed exact result/sequence/bytes",
            ));
        }
        Ok(())
    }

    pub(crate) fn returned(&self) -> i64 {
        self.returned
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.summary[3] as usize]
    }
}

/// A pre-entry observation of the retained OFD's finite send timeout. Actual
/// native authorization additionally joins the provider's saved stack local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockingTimeout(u64);
impl BlockingTimeout {
    pub(crate) fn from_ticks(ticks: u64) -> io::Result<Self> {
        if !(1..=i64::MAX as u64 - 1).contains(&ticks) {
            return Err(io::Error::other("blocking send timeout is not finite"));
        }
        Ok(Self(ticks))
    }
    fn from_timeval(value: libc::timeval) -> io::Result<Self> {
        // The exact supported image authenticates HZ1000 and both timeout
        // converters. SO_SNDTIMEO readback must be in whole1000-usec ticks.
        if value.tv_sec < 0 || !(0..1_000_000).contains(&value.tv_usec) || value.tv_usec % 1000 != 0
        {
            return Err(io::Error::other(
                "unsupported blocking send timeout readback",
            ));
        }
        let ticks = (value.tv_sec as u64)
            .checked_mul(1000)
            .and_then(|secs| secs.checked_add(value.tv_usec as u64 / 1000))
            .filter(|ticks| (1..=i64::MAX as u64 - 1).contains(ticks))
            .ok_or_else(|| io::Error::other("blocking send timeout is not finite"))?;
        Ok(Self(ticks))
    }
    pub(crate) fn ticks(self) -> u64 {
        self.0
    }
}

/// Read-only classification of the actual retained file. This observes intent,
/// not a native-operation completion or protection from concurrent alias edits.
pub(super) fn classify_blocking(pin: &std::os::fd::OwnedFd) -> io::Result<BlockingTimeout> {
    use std::os::fd::AsRawFd;

    use crate::network_replay::original_connect::Pin;
    let classified = super::native_peer::classify_original(pin)?;
    let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0
        || !matches!(
            classified,
            Pin::Socket {
                domain: libc::AF_INET,
                kind: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            }
        )
    {
        return Err(io::Error::other(
            "blocking Sendto requires actual blocking IPv4 TCP OFD",
        ));
    }
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            pin.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            std::ptr::from_mut(&mut value).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of_val(&value) {
        return Err(io::Error::other("blocking send timeout length changed"));
    }
    BlockingTimeout::from_timeval(value)
}

/// Version2 positive accepted prefix, obtained only through its632-byte API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlockingCapture {
    provider: u64,
    command: u64,
    call: u64,
    task: u64,
    task_start: u64,
    returned: i64,
    summary: [u64; 9],
    bytes: Vec<u8>,
}
impl From<accepted_provider_ffi::OriginalBlockingSendCapture> for BlockingCapture {
    fn from(raw: accepted_provider_ffi::OriginalBlockingSendCapture) -> Self {
        Self {
            provider: raw.provider,
            command: raw.command,
            call: raw.call,
            task: raw.task,
            task_start: raw.task_start,
            returned: raw.returned,
            summary: raw.summary,
            bytes: raw.bytes.to_vec(),
        }
    }
}
impl BlockingCapture {
    pub(super) fn validate(
        &self,
        effect: &OriginalEffect,
        expected_timeout: BlockingTimeout,
    ) -> io::Result<()> {
        let result = &effect.original;
        let selected = &result.selection;
        let command = &effect.command;
        let [
            version,
            file,
            requested,
            captured,
            before,
            after,
            returned,
            complete,
            saved_timeout,
        ] = self.summary;
        let count = usize::try_from(captured).map_err(io::Error::other)?;
        if effect.socket.is_some()
            || effect.read_copy.is_some()
            || effect.send.is_some()
            || version != 2
            || saved_timeout != expected_timeout.0
            || !(1..=i64::MAX as u64 - 1).contains(&saved_timeout)
            || complete != 1
            || !(1..=512).contains(&requested)
            || !(1..=requested as i64).contains(&self.returned)
            || captured != self.returned as u64
            || returned != self.returned as u64
            || before > u64::from(u32::MAX)
            || after > u64::from(u32::MAX)
            || (after as u32).wrapping_sub(before as u32) as u64 != captured
            || self.bytes.len() != 512
            || count > 512
            || self
                .bytes
                .get(count..)
                .is_none_or(|tail| tail.iter().any(|b| *b != 0))
            || self.provider == 0
            || self.command == 0
            || self.call == 0
            || self.task == 0
            || self.task_start == 0
            || file == 0
            || (
                self.provider,
                self.command,
                self.call,
                self.task,
                self.task_start,
            ) != (
                selected.provider,
                selected.command,
                selected.call,
                selected.task,
                selected.task_start,
            )
            || file != selected.file
            || requested != selected.original_count
            || command.operation != 25
            || command.command != self.command
            || command.identity.provider != self.provider
            || command.task != self.task
            || command.start_boottime != self.task_start
            || command.original_count != requested
            || command.phase != 1
            || command.reserved != 0
            || i64::from(command.returned) != self.returned
            || i64::from(result.returned) != self.returned
            || result.complete != 1
            || result.problem != 0
            || result.reserved != 0
            || result.address.len() != 128
            || result.address[72..].iter().any(|b| *b != 0)
            || result.address[..72]
                .as_chunks::<8>()
                .0
                .iter()
                .zip(self.summary)
                .any(|(bytes, value)| *bytes != value.to_ne_bytes())
            || selected.ready != 1
            || selected.table == 0
            || selected.user_address == 0
            || selected.fdput_flags > 1
            || selected.address_length != libc::MSG_NOSIGNAL
            || selected.requested_fd < 0
            || result.copy_entered != 0
            || result.copy_returned != 0
            || result.copy_remaining != 0
            || result.audit_entered != 0
            || result.audit_returned != 0
            || result.audit_result != 0
            || result.security_entered != 0
            || result.security_returned != 0
            || result.security_result != 0
        {
            return Err(io::Error::other(
                "blocking Sendto capture changed exact result/timeout/sequence/bytes",
            ));
        }
        Ok(())
    }

    pub(crate) fn returned(&self) -> i64 {
        self.returned
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.summary[3] as usize]
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub(in crate::network_runtime) fn fixture(returned: i64) -> (OriginalEffect, Capture) {
        let summary = [
            1,
            41,
            8,
            returned.max(0) as u64,
            u32::MAX as u64 - 1,
            (u32::MAX - 1).wrapping_add(returned.max(0) as u32) as u64,
            returned as u64,
            1,
        ];
        let mut raw = accepted_provider_ffi::OriginalEffect {
            command: accepted_provider_ffi::CommandResult {
                command: 17,
                operation: 24,
                task: 61,
                start_boottime: 99,
                identity: accepted_provider_ffi::Identity {
                    provider: 5,
                    ..Default::default()
                },
                returned: returned as i32,
                phase: 1,
                original_count: 8,
                ..Default::default()
            },
            original: accepted_provider_ffi::OriginalResult {
                selection: accepted_provider_ffi::OriginalSelection {
                    command: 17,
                    call: 19,
                    owner_mm: crate::types::MmId::initial(crate::types::DetTid::from_raw(61))
                        .generation(),
                    provider: 5,
                    task: 61,
                    task_start: 99,
                    table: 7,
                    file: 41,
                    user_address: 0x2000,
                    requested_fd: 8,
                    address_length: libc::MSG_NOSIGNAL,
                    original_count: 8,
                    ready: 1,
                    ..Default::default()
                },
                returned: returned as i32,
                complete: 1,
                ..Default::default()
            },
        };
        for (chunk, value) in raw
            .original
            .address
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(summary)
        {
            chunk.copy_from_slice(&value.to_ne_bytes());
        }
        let mut bytes = vec![0; 512];
        bytes[..returned.max(0) as usize].fill(b'x');
        let capture = Capture {
            provider: 5,
            command: 17,
            call: 19,
            task: 61,
            task_start: 99,
            returned,
            summary,
            bytes,
        };
        (raw.into(), capture)
    }

    #[test]
    fn original_send_capture_joins_partial_return_and_wrapping_sequence() {
        for returned in [3, 8, -i64::from(libc::EAGAIN), -i64::from(libc::EPIPE)] {
            let (effect, capture) = fixture(returned);
            capture.validate(&effect).unwrap();
            assert_eq!(capture.returned(), returned);
            assert_eq!(capture.bytes(), vec![b'x'; returned.max(0) as usize]);
        }
    }

    #[test]
    fn original_send_capture_refuses_identity_count_result_and_padding_changes() {
        let (effect, capture) = fixture(3);
        for variant in 0..15 {
            let mut changed = capture.clone();
            match variant {
                0 => changed.provider += 1,
                1 => changed.command += 1,
                2 => changed.call += 1,
                3 => changed.task += 1,
                4 => changed.task_start += 1,
                5 => changed.returned = -1,
                6 => changed.summary[2] += 1,
                7 => changed.summary[3] += 1,
                8 => changed.summary[4] += 1,
                9 => changed.summary[5] += 1,
                10 => changed.summary[6] += 1,
                11 => changed.summary[7] = 0,
                12 => {
                    changed.bytes.pop();
                }
                13 => changed.bytes[3] = 1,
                14 => changed.summary[1] += 1,
                _ => unreachable!(),
            }
            assert!(changed.validate(&effect).is_err(), "variant {variant}");
        }
        let mut changed = effect.clone();
        changed.original.address[127] = 1;
        assert!(capture.validate(&changed).is_err());
        let mut changed = effect;
        changed.command.returned = 8;
        assert!(capture.validate(&changed).is_err());
    }

    #[test]
    fn original_send_pin_requires_actual_nonblocking_tcp_not_logical_flags() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let pin: std::os::fd::OwnedFd = stream.try_clone().unwrap().into();
        assert!(classify(&pin).is_err());
        stream.set_nonblocking(true).unwrap();
        assert!(classify(&pin).is_ok());
        stream.set_nonblocking(false).unwrap();
        assert!(classify(&pin).is_err());
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        assert!(classify(&udp.into()).is_err());
        drop((pin, stream, peer, listener));
    }

    fn blocking_fixture() -> (OriginalEffect, BlockingCapture, BlockingTimeout) {
        let (mut effect, old) = fixture(3);
        effect.command.operation = 25;
        let mut summary = [0; 9];
        summary[..8].copy_from_slice(&old.summary);
        summary[0] = 2;
        summary[8] = 5000;
        effect.original.address.fill(0);
        for (chunk, value) in effect.original.address[..72]
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(summary)
        {
            chunk.copy_from_slice(&value.to_ne_bytes());
        }
        let capture = BlockingCapture {
            provider: old.provider,
            command: old.command,
            call: old.call,
            task: old.task,
            task_start: old.task_start,
            returned: 3,
            summary,
            bytes: old.bytes,
        };
        (effect, capture, BlockingTimeout(5000))
    }

    #[test]
    fn blocking_send_capture_requires_saved_timeout_positive_prefix_and_exact_result() {
        let (effect, capture, timeout) = blocking_fixture();
        // Initial MM generation0 is valid; exact equality belongs to the
        // retained native command/selection join, not a positivity predicate.
        assert_eq!(effect.original.selection.owner_mm, 0);
        capture.validate(&effect, timeout).unwrap();
        assert_eq!(capture.returned(), 3);
        assert_eq!(capture.bytes(), b"xxx");
        assert!(capture.validate(&effect, BlockingTimeout(4999)).is_err());
        assert!(capture.validate(&effect, BlockingTimeout(0)).is_err());
        // Mutate each actual identity, every summary word, and the byte tail.
        for variant in 0..17 {
            let mut changed = capture.clone();
            match variant {
                0 => changed.provider += 1,
                1 => changed.command += 1,
                2 => changed.call += 1,
                3 => changed.task += 1,
                4 => changed.task_start += 1,
                5 => changed.returned = 0,
                6 => changed.returned = -i64::from(libc::EAGAIN),
                7..=15 => changed.summary[variant - 7] += 1,
                16 => changed.bytes[511] = 1,
                _ => unreachable!(),
            }
            assert!(
                changed.validate(&effect, timeout).is_err(),
                "capture variant{variant}"
            );
        }
        for variant in 0..15 {
            let mut changed = effect.clone();
            match variant {
                0 => changed.command.operation = 24,
                1 => changed.original.address[64] ^= 1,
                2 => changed.original.address[72] = 1,
                3 => changed.original.address[127] = 1,
                4 => changed.original.selection.address_length |= libc::MSG_DONTWAIT,
                5 => changed.original.selection.ready = 0,
                6 => changed.original.selection.requested_fd = -1,
                7 => changed.original.selection.file += 1,
                8 => changed.original.selection.user_address = 0,
                9 => changed.command.returned = 8,
                10 => changed.original.returned = 8,
                11 => changed.original.problem = 1,
                12 => changed.original.complete = 0,
                13 => changed.original.copy_entered = 1,
                14 => changed.send = Some(fixture(3).1),
                _ => unreachable!(),
            }
            assert!(
                capture.validate(&changed, timeout).is_err(),
                "effect variant{variant}"
            );
        }
        let (old_effect, old_capture) = fixture(3);
        assert!(capture.validate(&old_effect, timeout).is_err());
        assert!(old_capture.validate(&effect).is_err());
    }

    #[test]
    fn blocking_send_classifies_actual_timeout_without_changing_nonblocking_policy() {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0);
        let pin = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(classify_blocking(&pin).is_err()); // Actual default infinite timeout.
        let value = libc::timeval {
            tv_sec: 5,
            tv_usec: 1000,
        };
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    pin.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDTIMEO,
                    std::ptr::from_ref(&value).cast(),
                    std::mem::size_of_val(&value) as libc::socklen_t,
                )
            },
            0
        );
        assert_eq!(classify_blocking(&pin).unwrap(), BlockingTimeout(5001));
        assert!(classify(&pin).is_err());
        assert_eq!(
            unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        assert!(classify_blocking(&pin).is_err());
        assert!(classify(&pin).is_ok());
        for (sec, usec) in [
            (0, 0),
            (-1, 0),
            (1, -1),
            (1, 1_000_000),
            (1, 1),
            (i64::MAX, 0),
        ] {
            assert!(
                BlockingTimeout::from_timeval(libc::timeval {
                    tv_sec: sec,
                    tv_usec: usec
                })
                .is_err()
            );
        }
    }
}
