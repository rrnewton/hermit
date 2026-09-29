// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the repository LICENSE file.

//! Private original-publication observation for the declared network fixtures.
//! This source is shared by the CLI parent and its outside fixture receiver.
//! No packet grants cleanup authority or changes the CLI's primary result.
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Instant;

use hermit::accepted_terminal::AcceptedPublication;
use hermit::unix_guard_package::RecoveryDirectoryIdentity;

const ENV: &str = "HERMIT_PRIVATE_ACCEPTED_COMPLETION";
const MAGIC: &[u8; 8] = b"HAGP211\0";
const BYTES: usize = 128;
const MAX_RUNS: usize = 2; // exact largest declared public call: one verify pair

fn refuse(message: &str) -> io::Error {
    io::Error::other(message)
}
fn within(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(refuse("original accepted completion deadline expired"));
    }
    Ok(())
}
fn socket_option<T: Copy>(fd: RawFd, option: i32) -> io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut length = std::mem::size_of::<T>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<T>() {
        return Err(refuse("completion socket option size differs"));
    }
    Ok(unsafe { value.assume_init() })
}

struct Publisher {
    socket: OwnedFd,
    token: [u8; 16],
    pid: i32,
    // Outer None is pending; Some(None) is a completed guard-only aggregate.
    attempts: Vec<Option<Option<AcceptedPublication>>>,
    failed: bool,
}
impl Publisher {
    fn new(socket: OwnedFd, token: [u8; 16]) -> Self {
        Self {
            socket,
            token,
            pid: unsafe { libc::getpid() },
            attempts: Vec::with_capacity(MAX_RUNS),
            failed: false,
        }
    }
    fn frame(&self) -> io::Result<[u8; BYTES]> {
        if self.pid != unsafe { libc::getpid() }
            || self.failed
            || self.attempts.len() > MAX_RUNS
            || self.attempts.iter().any(Option::is_none)
        {
            return Err(refuse("original aggregate publication remains unconfirmed"));
        }
        let mut frame = [0; BYTES];
        frame[..8].copy_from_slice(MAGIC);
        frame[8..24].copy_from_slice(&self.token);
        frame[24] = self.attempts.iter().flatten().flatten().count() as u8;
        frame[25] = self.attempts.len() as u8;
        for (index, publication) in self.attempts.iter().flatten().flatten().enumerate() {
            let start = 32 + index * 48;
            frame[start..start + 16].copy_from_slice(&publication.run());
            frame[start + 16..start + 24].copy_from_slice(&publication.root().device.to_le_bytes());
            frame[start + 24..start + 32].copy_from_slice(&publication.root().inode.to_le_bytes());
            frame[start + 32..start + 36].copy_from_slice(&publication.root().uid.to_le_bytes());
            frame[start + 36..start + 40].copy_from_slice(&publication.root().mode.to_le_bytes());
        }
        Ok(frame)
    }
    fn send(self) -> io::Result<()> {
        let frame = self.frame()?;
        // One atomic packet, no retries or renewed timer, and no fallible file
        // publication afterward. Failed/partial send means no acknowledgment.
        let sent = unsafe {
            libc::send(
                self.socket.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if sent != BYTES as isize {
            return Err(refuse("original accepted completion packet not sent"));
        }
        Ok(())
    }
}
thread_local! {
    static PUBLISHER: RefCell<Option<Publisher>> = const { RefCell::new(None) };
}

/// Adopt only a directly inherited private socket, before normal CLI preflight.
/// The environment locates an endpoint; it cannot authorize the receiver.
pub(crate) fn initialize() -> io::Result<()> {
    let Some(value) = std::env::var_os(ENV) else {
        return Ok(());
    };
    let value = value
        .to_str()
        .ok_or_else(|| refuse("invalid completion address"))?;
    let (fd, token) = value
        .split_once(':')
        .ok_or_else(|| refuse("invalid completion address"))?;
    let fd: i32 = fd
        .parse()
        .map_err(|_| refuse("invalid completion descriptor"))?;
    if fd < 3 || token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refuse("invalid completion descriptor/token"));
    }
    if socket_option::<i32>(fd, libc::SO_TYPE)? != libc::SOCK_SEQPACKET
        || socket_option::<i32>(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
    {
        return Err(refuse(
            "completion descriptor is not a private packet socket",
        ));
    }
    let peer = socket_option::<libc::ucred>(fd, libc::SO_PEERCRED)?;
    if peer.pid != unsafe { libc::getppid() }
        || peer.uid != unsafe { libc::getuid() }
        || peer.gid != unsafe { libc::getgid() }
    {
        return Err(refuse(
            "completion socket does not belong to original fixture parent",
        ));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut identity = [0; 16];
    for (index, byte) in identity.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&token[index * 2..index * 2 + 2], 16)
            .map_err(|_| refuse("invalid completion token"))?;
    }
    if identity == [0; 16] {
        return Err(refuse("zero completion token"));
    }
    // Ownership is taken exactly once, only after authenticating the inherited
    // socket. Helper re-execs lose it to CLOEXEC, even if the address remains.
    PUBLISHER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(refuse("completion endpoint already adopted"));
        }
        *slot = Some(Publisher::new(
            unsafe { OwnedFd::from_raw_fd(fd) },
            identity,
        ));
        Ok(())
    })
}

/// A pending original aggregate cannot disappear on an early return.
pub(crate) struct Attempt {
    index: Option<usize>,
    pid: i32,
}
pub(crate) fn begin_aggregate() -> io::Result<Attempt> {
    PUBLISHER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let pid = unsafe { libc::getpid() };
        let Some(publisher) = slot.as_mut() else {
            return Ok(Attempt { index: None, pid });
        };
        if publisher.pid != pid || publisher.failed || publisher.attempts.len() == MAX_RUNS {
            publisher.failed = true;
            return Err(refuse(
                "accepted completion attempt is foreign/extra/failed",
            ));
        }
        let index = publisher.attempts.len();
        publisher.attempts.push(None);
        Ok(Attempt {
            index: Some(index),
            pid,
        })
    })
}
impl Attempt {
    /// Called only after original aggregate publication and certification.
    pub(crate) fn confirm(&mut self, publication: Option<AcceptedPublication>) -> io::Result<()> {
        let Some(index) = self.index else {
            return Ok(());
        };
        PUBLISHER.with(|slot| {
            let mut slot = slot.borrow_mut();
            let publisher = slot
                .as_mut()
                .ok_or_else(|| refuse("completion owner missing"))?;
            if self.pid != unsafe { libc::getpid() }
                || publisher.pid != self.pid
                || publisher.failed
                || publisher.attempts.get(index).is_none_or(Option::is_some)
                || publication.as_ref().is_some_and(|publication| {
                    publisher
                        .attempts
                        .iter()
                        .flatten()
                        .flatten()
                        .any(|p| p.run() == publication.run())
                })
            {
                publisher.failed = true;
                return Err(refuse("accepted completion is repeated/foreign"));
            }
            publisher.attempts[index] = Some(publication);
            Ok(())
        })
    }
}
/// Called only from the normal original main return, never Drop/unwind.
pub(crate) fn finish_invocation() -> io::Result<()> {
    PUBLISHER.with(|slot| slot.borrow_mut().take().map_or(Ok(()), Publisher::send))
}
/// Called ONLY in the actual cloned network controller, before guest work.
/// Do not infer clone identity from getpid: parent and child can both be PID1
/// in different PID namespaces. The real child callback supplies that identity.
pub(crate) fn close_in_child() {
    PUBLISHER.with(|slot| drop(slot.borrow_mut().take()));
}

/// Per-call outside custody. The send alias survives only through Command::spawn.
pub(crate) struct FixtureChannel {
    receive: OwnedFd,
    send: Option<OwnedFd>,
    token: [u8; 16],
    pid: Option<i32>,
    uid: u32,
    gid: u32,
}
impl FixtureChannel {
    pub(crate) fn new() -> io::Result<Self> {
        let mut fds = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let receive = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let send = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let enabled = 1i32;
        if unsafe {
            libc::setsockopt(
                receive.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&enabled as *const i32).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            receive,
            send: Some(send),
            token: *uuid::Uuid::new_v4().as_bytes(),
            pid: None,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        })
    }
    pub(crate) fn prepare(&self, command: &mut Command) -> io::Result<()> {
        let fd = self
            .send
            .as_ref()
            .ok_or_else(|| refuse("completion launch repeated"))?
            .as_raw_fd();
        command.env(
            ENV,
            format!("{fd}:{}", uuid::Uuid::from_bytes(self.token).simple()),
        );
        // The parent alias remains CLOEXEC. Only this exact Command's child
        // inherits it; no other concurrent fixture spawn receives authority.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
    pub(crate) fn launched(&mut self, pid: u32) -> io::Result<()> {
        if self.pid.is_some() || pid == 0 || pid > i32::MAX as u32 {
            return Err(refuse("invalid/repeated completion child identity"));
        }
        self.pid = Some(pid as i32);
        drop(self.send.take());
        Ok(())
    }
    pub(crate) fn confirm(
        &mut self,
        attempts: usize,
        runs: &BTreeSet<[u8; 16]>,
        root: Option<&RecoveryDirectoryIdentity>,
        deadline: Instant,
    ) -> io::Result<()> {
        within(deadline)?;
        let pid = self
            .pid
            .ok_or_else(|| refuse("completion child was never launched"))?;
        let packet = receive_packet(self.receive.as_raw_fd())?
            .ok_or_else(|| refuse("original accepted publication acknowledgment missing"))?;
        let (frame, sender) = packet;
        if sender.pid != pid || sender.uid != self.uid || sender.gid != self.gid {
            return Err(refuse("accepted completion sender identity differs"));
        }
        if &frame[..8] != MAGIC
            || frame[8..24] != self.token
            || frame[26..32] != [0; 6]
            || frame[24] as usize != runs.len()
            || frame[25] as usize != attempts
            || attempts > MAX_RUNS
            || runs.len() > attempts
            || runs.len() > MAX_RUNS
        {
            return Err(refuse("accepted completion call/token/population differs"));
        }
        let mut acknowledged = BTreeSet::new();
        for index in 0..MAX_RUNS {
            let start = 32 + index * 48;
            let record = &frame[start..start + 48];
            if index >= runs.len() {
                if record != [0; 48] {
                    return Err(refuse("accepted completion extra record"));
                }
                continue;
            }
            let run: [u8; 16] = record[..16].try_into().unwrap();
            let observed = RecoveryDirectoryIdentity {
                device: u64::from_le_bytes(record[16..24].try_into().unwrap()),
                inode: u64::from_le_bytes(record[24..32].try_into().unwrap()),
                uid: u32::from_le_bytes(record[32..36].try_into().unwrap()),
                mode: u32::from_le_bytes(record[36..40].try_into().unwrap()),
            };
            if root != Some(&observed) || record[40..] != [0; 8] || !acknowledged.insert(run) {
                return Err(refuse("accepted completion root/run identity differs"));
            }
        }
        if acknowledged != *runs {
            return Err(refuse(
                "accepted completion original run population differs",
            ));
        }
        // After original Child reap, require exactly one packet and actual peer
        // closure. EAGAIN is not EOF: inherited/still-live aliases refuse.
        if receive_packet(self.receive.as_raw_fd())?.is_some() {
            return Err(refuse("accepted completion packet repeated"));
        }
        within(deadline)
    }
    #[cfg(test)]
    pub(crate) fn install_publisher_premise(&mut self) {
        let publisher = Publisher::new(self.send.take().unwrap(), self.token);
        self.launched(unsafe { libc::getpid() } as u32).unwrap();
        PUBLISHER.with(|slot| *slot.borrow_mut() = Some(publisher));
    }
}

fn receive_packet(fd: RawFd) -> io::Result<Option<([u8; BYTES], libc::ucred)>> {
    let mut frame = [0; BYTES];
    let mut iov = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: BYTES,
    };
    // Large enough to receive and close all unexpected SCM_RIGHTS descriptors.
    let mut control = [0usize; 136];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let length = unsafe {
        libc::recvmsg(
            fd,
            &mut message,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC,
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut credential = None;
    let mut foreign = false;
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let base = libc::CMSG_LEN(0) as usize;
            if (*header).cmsg_len < base {
                foreign = true;
                break;
            }
            let bytes = (*header).cmsg_len - base;
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                for index in 0..bytes / std::mem::size_of::<i32>() {
                    let raw =
                        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>().add(index));
                    drop(OwnedFd::from_raw_fd(raw));
                }
                foreign = true;
            } else if (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_CREDENTIALS
                && bytes == std::mem::size_of::<libc::ucred>()
                && credential.is_none()
            {
                credential = Some(std::ptr::read_unaligned(
                    libc::CMSG_DATA(header).cast::<libc::ucred>(),
                ));
            } else {
                foreign = true;
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if foreign || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(refuse(
            "accepted completion ancillary data/truncation differs",
        ));
    }
    // A zero-length data packet still carries credentials. Only a clean EOF
    // with no ancillary message denotes closure of the original peer.
    if length == 0 && credential.is_none() {
        return Ok(None);
    }
    if length != BYTES as isize {
        return Err(refuse("accepted completion packet length differs"));
    }
    Ok(Some((
        frame,
        credential.ok_or_else(|| refuse("completion credentials absent"))?,
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn pair() -> (FixtureChannel, Publisher) {
        let mut channel = FixtureChannel::new().unwrap();
        let publisher = Publisher::new(channel.send.take().unwrap(), channel.token);
        channel.launched(unsafe { libc::getpid() } as u32).unwrap();
        (channel, publisher)
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }
    fn send(socket: RawFd, bytes: &[u8]) {
        assert_eq!(
            unsafe {
                libc::send(
                    socket,
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }

    #[test]
    fn empty_acknowledgment_requires_one_original_packet_and_closed_peer() {
        let (mut channel, publisher) = pair();
        publisher.send().unwrap();
        channel
            .confirm(0, &BTreeSet::new(), None, deadline())
            .unwrap();
        for variant in 0..4 {
            let (mut channel, publisher) = pair();
            let bytes = publisher.frame().unwrap();
            match variant {
                0 => drop(publisher), // no packet, actual EOF
                1 => {
                    send(publisher.socket.as_raw_fd(), &bytes);
                    // Valid packet with a live original alias is not complete.
                    assert!(
                        channel
                            .confirm(0, &BTreeSet::new(), None, deadline())
                            .is_err()
                    );
                    continue;
                }
                2 => {
                    send(publisher.socket.as_raw_fd(), &bytes);
                    send(publisher.socket.as_raw_fd(), &bytes);
                    drop(publisher);
                }
                3 => {
                    send(publisher.socket.as_raw_fd(), &[]);
                    drop(publisher);
                }
                _ => unreachable!(),
            }
            assert!(
                channel
                    .confirm(0, &BTreeSet::new(), None, deadline())
                    .is_err(),
                "variant {variant}"
            );
        }
    }

    #[test]
    fn acknowledgment_rejects_malformed_token_length_reserved_and_extra_records() {
        for variant in 0..7 {
            let (mut channel, publisher) = pair();
            let mut bytes = publisher.frame().unwrap().to_vec();
            match variant {
                0 => bytes[0] ^= 1,
                1 => bytes[8] ^= 1,
                2 => bytes[26] = 1,
                3 => {
                    bytes.pop();
                }
                4 => bytes.push(0),
                5 => bytes[24] = 1,
                6 => bytes[127] = 1,
                _ => unreachable!(),
            }
            send(publisher.socket.as_raw_fd(), &bytes);
            drop(publisher);
            assert!(
                channel
                    .confirm(0, &BTreeSet::new(), None, deadline())
                    .is_err(),
                "variant {variant}"
            );
        }
    }

    #[test]
    fn acknowledgment_requires_kernel_sender_pid_uid_and_gid() {
        for variant in 0..3 {
            let (mut channel, publisher) = pair();
            match variant {
                0 => channel.pid = Some(channel.pid.unwrap() + 1),
                1 => channel.uid = channel.uid.wrapping_add(1),
                2 => channel.gid = channel.gid.wrapping_add(1),
                _ => unreachable!(),
            }
            publisher.send().unwrap();
            let error = channel
                .confirm(0, &BTreeSet::new(), None, deadline())
                .unwrap_err();
            assert!(error.to_string().contains("sender identity"));
        }
    }

    #[test]
    fn acknowledgment_parser_binds_exact_run_population_and_root_identity_premises() {
        // Wire-parser premises only: these frames do not claim that a provider
        // or the original writer ran. The custody regression uses real finish.
        let root = RecoveryDirectoryIdentity {
            device: 27,
            inode: 31,
            uid: unsafe { libc::getuid() },
            mode: 0o40700,
        };
        let runs = BTreeSet::from([[7; 16]]);
        for variant in 0..9 {
            let (mut channel, publisher) = pair();
            let mut bytes = publisher.frame().unwrap();
            bytes[24] = 1;
            bytes[25] = 1;
            bytes[32..48].copy_from_slice(&[7; 16]);
            bytes[48..56].copy_from_slice(&root.device.to_le_bytes());
            bytes[56..64].copy_from_slice(&root.inode.to_le_bytes());
            bytes[64..68].copy_from_slice(&root.uid.to_le_bytes());
            bytes[68..72].copy_from_slice(&root.mode.to_le_bytes());
            let mut expected = runs.clone();
            match variant {
                0 => (),
                1 => bytes[32] ^= 1, // foreign/stale run
                2 => bytes[48] ^= 1,
                3 => bytes[56] ^= 1,
                4 => bytes[64] ^= 1,
                5 => bytes[68] ^= 1,
                6 => bytes[72] = 1,
                7 => {
                    expected.insert([8; 16]);
                } // verify pair needs two
                8 => {
                    bytes[24] = 2;
                    bytes[25] = 2;
                    let record: [u8; 48] = bytes[32..80].try_into().unwrap();
                    bytes[80..128].copy_from_slice(&record); // duplicate original run
                    expected.insert([8; 16]);
                }
                _ => unreachable!(),
            }
            send(publisher.socket.as_raw_fd(), &bytes);
            drop(publisher);
            assert_eq!(
                channel
                    .confirm(expected.len(), &expected, Some(&root), deadline())
                    .is_ok(),
                variant == 0,
                "variant {variant}"
            );
        }
    }

    #[test]
    fn guard_only_aggregate_count_is_required_even_without_accepted_runs() {
        for completed in 0..=2 {
            for expected in 0..=2 {
                let mut channel = FixtureChannel::new().unwrap();
                channel.install_publisher_premise();
                for _ in 0..completed {
                    begin_aggregate().unwrap().confirm(None).unwrap();
                }
                finish_invocation().unwrap();
                assert_eq!(
                    channel
                        .confirm(expected, &BTreeSet::new(), None, deadline())
                        .is_ok(),
                    completed == expected,
                    "empty accepted population still binds original aggregates",
                );
            }
        }
    }

    #[test]
    fn unresolved_attempts_cannot_acknowledge_zero_or_a_verify_pair() {
        for attempted in 1..=3 {
            let mut channel = FixtureChannel::new().unwrap();
            channel.install_publisher_premise();
            for index in 0..attempted {
                assert_eq!(begin_aggregate().is_ok(), index < MAX_RUNS);
            }
            assert!(finish_invocation().is_err());
            assert!(
                channel
                    .confirm(0, &BTreeSet::new(), None, deadline())
                    .is_err()
            );
        }
    }

    #[test]
    fn cancellation_and_original_expired_deadline_never_acknowledge() {
        let (channel, publisher) = pair();
        drop(channel);
        assert!(publisher.send().is_err()); // MSG_NOSIGNAL, no unbounded wait
        let (mut channel, publisher) = pair();
        publisher.send().unwrap();
        let expired = Instant::now();
        let error = channel
            .confirm(0, &BTreeSet::new(), None, expired)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original accepted completion deadline expired")
        );
    }

    #[test]
    fn sender_descriptor_is_cloexec_and_cloned_child_state_is_closed_premise() {
        let mut channel = FixtureChannel::new().unwrap();
        let fd = channel.send.as_ref().unwrap().as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        channel.install_publisher_premise();
        // Source premise for the actual post-clone hook, without forking or
        // running a guest. Numeric PID equality across namespaces must not
        // keep an alias alive once the real child callback invokes this hook.
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        close_in_child();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert!(
            channel
                .confirm(0, &BTreeSet::new(), None, deadline())
                .is_err()
        );
    }
}
