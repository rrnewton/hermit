//! Auxiliary observations of the original installed file. The service performs
//! no guest read/copy and returns every capture, observation and close result in
//! the existing Call's collection response. A numeric duplicate is a candidate;
//! only the existing provider file generation authenticates it.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;

use detcore_model::network_trace::FreshStreamSocketProfileV3;
use detcore_model::network_trace::LinuxReceiveNormalizationV3;
use detcore_model::network_trace::ReceiveBufferStateV3;
use detcore_model::network_trace::ReceiveTimeoutV3;
use detcore_model::network_trace::StreamSocketKeyV3;
use detcore_model::network_trace::StreamSocketOptionsV3;
use serde::Deserialize;
use serde::Serialize;

use super::accepted_provider::CallStatus;
use super::accepted_provider::CommandResult;
use super::accepted_provider::Observation;
use super::accepted_provider::OriginalEffect;
use super::accepted_provider::RawState;
use super::accepted_provider_ffi as ffi;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TcpMetadata {
    pub low_water: i32,
    pub receive_buffer: i32,
    pub peek_offset: Option<i32>,
    pub receive_timeout: (i64, i64),
    pub send_timeout: (i64, i64),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeldMetadata {
    pub stat: crate::stat::DetStat,
    pub status_flags: i32,
    pub domain: i32,
    pub socket_type: i32,
    pub protocol: i32,
    pub tcp: Option<TcpMetadata>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Capture {
    pub file: u64,
    pub returned_fd: i32,
    pub capture: CallStatus,
    pub candidate_stat: Option<crate::stat::DetStat>,
    pub observation: Option<Observation<CommandResult>>,
    pub metadata: Option<HeldMetadata>,
    pub error: Option<String>,
    pub release: Option<CallStatus>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checked {
    pub provider: u64,
    pub file: u64,
    pub namespace: u64,
    pub cookie: u64,
    raw: RawState,
    pub metadata: HeldMetadata,
}
fn status(operation: &str, returned: i32) -> CallStatus {
    let errno = (returned < 0).then(|| {
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    });
    CallStatus {
        operation: operation.into(),
        returned,
        errno,
    }
}
pub(super) fn held_stat(fd: BorrowedFd<'_>) -> io::Result<crate::stat::DetStat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.into())
}
/// Observe the held file through the ordinary kernel interfaces. In particular,
/// stat still executes the filesystem's getattr and namespace/idmap conversion;
/// raw inode fields from a census are not a substitute for that operation.
pub(super) fn held_file_profile(
    fd: BorrowedFd<'_>,
) -> io::Result<(crate::stat::DetStat, i32, Option<i32>)> {
    let stat = held_stat(fd)?;
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let domain = if stat.mode & libc::S_IFMT == libc::S_IFSOCK {
        Some(get(fd, libc::SO_DOMAIN)?)
    } else {
        None
    };
    Ok((stat, flags, domain))
}
fn get<T: Default>(fd: BorrowedFd<'_>, option: i32) -> io::Result<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw mut value).cast(),
            &raw mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<T>() {
        return Err(io::Error::other("held socket getter changed output size"));
    }
    Ok(value)
}
fn timeout(fd: BorrowedFd<'_>, option: i32) -> io::Result<(i64, i64)> {
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw mut value).cast(),
            &raw mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::timeval>() {
        return Err(io::Error::other("held timeout getter changed output size"));
    }
    Ok((value.tv_sec, value.tv_usec))
}
fn metadata(fd: BorrowedFd<'_>, stat: crate::stat::DetStat) -> io::Result<HeldMetadata> {
    let status_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if status_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let domain = get(fd, libc::SO_DOMAIN)?;
    let socket_type = get(fd, libc::SO_TYPE)?;
    let protocol = get(fd, libc::SO_PROTOCOL)?;
    let tcp = if matches!(domain, libc::AF_INET | libc::AF_INET6)
        && socket_type == libc::SOCK_STREAM
        && protocol == libc::IPPROTO_TCP
    {
        Some(TcpMetadata {
            low_water: get(fd, libc::SO_RCVLOWAT)?,
            receive_buffer: get(fd, libc::SO_RCVBUF)?,
            peek_offset: match get(fd, libc::SO_PEEK_OFF) {
                Ok(value) => Some(value),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOPROTOOPT | libc::EOPNOTSUPP)
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error),
            },
            receive_timeout: timeout(fd, libc::SO_RCVTIMEO)?,
            send_timeout: timeout(fd, libc::SO_SNDTIMEO)?,
        })
    } else {
        None
    };
    Ok(HeldMetadata {
        stat,
        status_flags,
        domain,
        socket_type,
        protocol,
        tcp,
    })
}

/// Synchronous service dispatch owns the candidate before any subsequent
/// syscall. Its one explicit close is retained before this response can await
/// an ACK. No new worker, descriptor registry, guest buffer or helper drain.
pub(super) fn capture(
    session: &mut ffi::Session,
    helper: BorrowedFd<'_>,
    target: BorrowedFd<'_>,
    effect: &OriginalEffect,
) -> Capture {
    let file = effect.original.selection.file;
    let returned_fd = effect.original.returned;
    let raw =
        unsafe { libc::syscall(libc::SYS_pidfd_getfd, target.as_raw_fd(), returned_fd, 0) } as i32;
    let capture = status("pidfd_getfd original installation", raw);
    let mut result = Capture {
        file,
        returned_fd,
        capture,
        candidate_stat: None,
        observation: None,
        metadata: None,
        error: None,
        release: None,
    };
    if raw < 0 {
        return result;
    }
    let held = unsafe { OwnedFd::from_raw_fd(raw) };
    let observed = (|| -> io::Result<()> {
        let stat = held_stat(held.as_fd())?;
        result.candidate_stat = Some(stat);
        if stat.mode & libc::S_IFMT != libc::S_IFSOCK {
            return Ok(());
        }
        let observation: Observation<CommandResult> =
            session.observe_socket_file(helper, held.as_fd()).into();
        let matches = observation.status.returned == 0
            && observation.status.errno.is_none()
            && observation.raw.returned == 0
            && observation.raw.identity.object == file;
        result.observation = Some(observation);
        // An unknown/reused file does not supply metadata for this Call. The
        // later exact journal cut, never this mismatch, can prove retirement.
        if matches {
            result.metadata = Some(metadata(held.as_fd(), stat)?);
        }
        Ok(())
    })();
    if let Err(error) = observed {
        result.error = Some(error.to_string());
    }
    let rc = unsafe { libc::close(held.into_raw_fd()) };
    result.release = Some(status("close installation observation", rc));
    result
}

impl Capture {
    /// Exact auxiliary command is ACKed even when its actual file differs.
    /// That observation is retained, not promoted to an installation match.
    pub(super) fn command(
        &self,
        effect: &OriginalEffect,
    ) -> io::Result<Option<ffi::CommandResult>> {
        if effect.command.operation != 12
            || effect.original.returned < 0
            || self.file == 0
            || self.file != effect.original.selection.file
            || self.returned_fd != effect.original.returned
        {
            return Err(io::Error::other(
                "held observation changed original Socket association",
            ));
        }
        let Some(observed) = &self.observation else {
            return Ok(None);
        };
        if observed.status.returned != 0 {
            return Ok(None);
        }
        let raw = &observed.raw;
        if observed.status.operation != "ap_observe_socket_file"
            || observed.status.errno.is_some()
            || raw.operation != 14
            || raw.command == 0
            || raw.command == effect.command.command
            || raw.phase != 1
            || raw.task == 0
            || raw.start_boottime == 0
            || raw.identity.provider != effect.command.identity.provider
            || raw.identity.namespace == 0
            || raw.cookie == 0
            || raw.creation != 0
            || raw.original_count != 0
            || raw.reserved != 0
            || raw.returned != 0
        {
            return Err(io::Error::other(
                "held observation changed complete command identity",
            ));
        }
        Ok(Some(raw.clone().into()))
    }
    /// None denotes the finite capture race only. It never certifies removal;
    /// publication must independently prove the complete ordered retirement.
    pub(super) fn checked(&self, effect: &OriginalEffect) -> io::Result<Option<Checked>> {
        let command = self.command(effect)?;
        if self.capture.operation != "pidfd_getfd original installation" {
            return Err(io::Error::other("installation capture changed operation"));
        }
        if self.capture.returned < 0 {
            if self.capture.errno == Some(libc::EBADF)
                && self.candidate_stat.is_none()
                && self.observation.is_none()
                && self.metadata.is_none()
                && self.error.is_none()
                && self.release.is_none()
            {
                return Ok(None);
            }
            return Err(io::Error::other("installation candidate capture failed"));
        }
        if self.capture.errno.is_some()
            || self.release.as_ref().is_none_or(|s| {
                s.operation != "close installation observation"
                    || s.returned != 0
                    || s.errno.is_some()
            })
            || self.error.is_some()
        {
            return Err(io::Error::other(
                "held installation observation/close remains unresolved",
            ));
        }
        let stat = self
            .candidate_stat
            .ok_or_else(|| io::Error::other("held observation lacks actual fstat"))?;
        if stat.mode & libc::S_IFMT != libc::S_IFSOCK {
            if self.observation.is_none() && self.metadata.is_none() {
                return Ok(None);
            }
            return Err(io::Error::other(
                "non-socket candidate carries socket authority",
            ));
        }
        let command =
            command.ok_or_else(|| io::Error::other("held socket observation did not complete"))?;
        if command.identity.object != self.file {
            if self.metadata.is_none() {
                return Ok(None);
            }
            return Err(io::Error::other(
                "wrong held file carries original metadata",
            ));
        }
        let metadata = self
            .metadata
            .clone()
            .ok_or_else(|| io::Error::other("matched held file lacks metadata"))?;
        if metadata.stat != stat {
            return Err(io::Error::other("held metadata changed fstat"));
        }
        Ok(Some(Checked {
            provider: command.identity.provider,
            file: self.file,
            namespace: command.identity.namespace,
            cookie: command.cookie,
            raw: command.state.into(),
            metadata,
        }))
    }
}
impl Checked {
    pub(crate) fn fresh_profile(
        &self,
        key: StreamSocketKeyV3,
        normalization: LinuxReceiveNormalizationV3,
    ) -> io::Result<FreshStreamSocketProfileV3> {
        let m = &self.metadata;
        let t = m
            .tcp
            .as_ref()
            .ok_or_else(|| io::Error::other("original TCP profile lacks held state"))?;
        if m.domain != key.domain
            || m.socket_type != key.socket_type
            || m.protocol != key.protocol
            || t.low_water != self.raw.lowat
            || t.receive_buffer != self.raw.receive_buffer
            || t.peek_offset
                .is_some_and(|peek| peek != self.raw.peek_offset)
            || (t.peek_offset.is_none() && self.raw.peek_offset != -1)
            || t.receive_timeout != (0, 0)
            || t.send_timeout != (0, 0)
            || self.raw.receive_timeout_ticks != i64::MAX
            || self.raw.send_timeout_ticks != i64::MAX
            || self.raw.userlocks != 0
            || self.raw.scaling_ratio != 128
            || self.raw.tcp_state != 7
            || self.raw.socket_option_memory != 0
            || self.raw.child_spin_locked != 0
        {
            return Err(io::Error::other(
                "original fresh TCP getters/raw state disagree",
            ));
        }
        normalization
            .validate()
            .map_err(|error| io::Error::other(format!("invalid normalization: {error:?}")))?;
        Ok(FreshStreamSocketProfileV3 {
            key,
            normalization,
            initial: StreamSocketOptionsV3 {
                peek_offset: t.peek_offset,
                receive_low_water: u32::try_from(t.low_water)
                    .ok()
                    .filter(|value| *value != 0)
                    .ok_or_else(|| io::Error::other("invalid held TCP low-water"))?,
                receive_timeout: ReceiveTimeoutV3::Infinite,
                receive_buffer: ReceiveBufferStateV3 {
                    bytes: u32::try_from(t.receive_buffer).map_err(io::Error::other)?,
                    user_locked: false,
                    tcp_scaling_ratio: 128,
                },
            },
        })
    }
}

#[cfg(test)]
pub(super) fn fixture() -> (OriginalEffect, Capture) {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    stat.st_mode = libc::S_IFSOCK | 0o600;
    stat.st_ino = 91;
    stat.st_nlink = 1;
    let stat: crate::stat::DetStat = stat.into();
    let mut effect: OriginalEffect = ffi::OriginalEffect::default().into();
    effect.command = ffi::CommandResult {
        command: 71,
        operation: 12,
        task: 31,
        start_boottime: 101,
        identity: ffi::Identity {
            provider: 7,
            ..Default::default()
        },
        returned: 17,
        phase: 1,
        ..Default::default()
    }
    .into();
    effect.original.selection.file = 19;
    effect.original.returned = 17;
    let raw = ffi::CommandResult {
        command: 72,
        operation: 14,
        task: 41,
        start_boottime: 102,
        identity: ffi::Identity {
            provider: 7,
            object: 19,
            namespace: 100,
        },
        cookie: 51,
        phase: 1,
        state: ffi::RawState {
            receive_timeout_ticks: i64::MAX,
            send_timeout_ticks: i64::MAX,
            lowat: 1,
            receive_buffer: 262144,
            peek_offset: -1,
            scaling_ratio: 128,
            tcp_state: 7,
            ..Default::default()
        },
        ..Default::default()
    };
    let capture = Capture {
        file: 19,
        returned_fd: 17,
        capture: CallStatus {
            operation: "pidfd_getfd original installation".into(),
            returned: 20,
            errno: None,
        },
        candidate_stat: Some(stat),
        observation: Some(Observation {
            status: CallStatus {
                operation: "ap_observe_socket_file".into(),
                returned: 0,
                errno: None,
            },
            raw: raw.into(),
        }),
        metadata: Some(HeldMetadata {
            stat,
            status_flags: libc::O_RDWR,
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
            tcp: Some(TcpMetadata {
                low_water: 1,
                receive_buffer: 262144,
                peek_offset: Some(-1),
                receive_timeout: (0, 0),
                send_timeout: (0, 0),
            }),
        }),
        error: None,
        release: Some(CallStatus {
            operation: "close installation observation".into(),
            returned: 0,
            errno: None,
        }),
    };
    (effect, capture)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> (StreamSocketKeyV3, LinuxReceiveNormalizationV3) {
        use detcore_model::network_trace::LinuxReceiveHzV3;
        use detcore_model::network_trace::NetworkTransportV2;
        (
            StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain: libc::AF_INET,
                socket_type: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            },
            LinuxReceiveNormalizationV3 {
                hz: LinuxReceiveHzV3::Hz1000,
                system_rmem_max: 20971520,
                namespace_tcp_rmem_max: 6291456,
                minimum_receive_buffer: 2304,
                peek_offset_set_supported: true,
            },
        )
    }
    #[test]
    fn held_installation_joins_exact_original_file_and_retains_observed_profile() {
        let (effect, capture) = fixture();
        let observed = capture.checked(&effect).unwrap().unwrap();
        assert_eq!(
            (
                observed.provider,
                observed.file,
                observed.namespace,
                observed.cookie
            ),
            (7, 19, 100, 51)
        );
        let (key, normalization) = profile();
        let fresh = observed.fresh_profile(key, normalization).unwrap();
        assert_eq!(fresh.initial.receive_low_water, 1);
        assert_eq!(fresh.initial.receive_buffer.bytes, 262144);
        assert_eq!(fresh.initial.receive_timeout, ReceiveTimeoutV3::Infinite);
        assert_eq!(fresh.initial.peek_offset, Some(-1));
        assert_eq!(observed.metadata.stat, capture.candidate_stat.unwrap());
    }
    #[test]
    fn held_installation_refuses_changed_command_original_owner_and_close_receipts() {
        let (effect, original) = fixture();
        for changed in 0..24 {
            let mut capture = original.clone();
            let mut effect = effect.clone();
            match changed {
                0 => effect.command.operation = 11,
                1 => effect.original.returned = -libc::EBADF,
                2 => capture.file += 1,
                3 => capture.returned_fd += 1,
                4 => capture.observation.as_mut().unwrap().raw.operation = 2,
                5 => capture.observation.as_mut().unwrap().raw.command = 0,
                6 => capture.observation.as_mut().unwrap().raw.command = 71,
                7 => capture.observation.as_mut().unwrap().raw.phase = 2,
                8 => capture.observation.as_mut().unwrap().raw.task = 0,
                9 => capture.observation.as_mut().unwrap().raw.start_boottime = 0,
                10 => capture.observation.as_mut().unwrap().raw.identity.provider += 1,
                11 => capture.observation.as_mut().unwrap().raw.identity.namespace = 0,
                12 => capture.observation.as_mut().unwrap().raw.cookie = 0,
                13 => capture.observation.as_mut().unwrap().raw.creation = 1,
                14 => capture.observation.as_mut().unwrap().raw.original_count = 1,
                15 => capture.observation.as_mut().unwrap().raw.reserved = 1,
                16 => capture.observation.as_mut().unwrap().raw.returned = -1,
                17 => capture.observation.as_mut().unwrap().status.errno = Some(libc::EIO),
                18 => {
                    capture.observation.as_mut().unwrap().status.operation = "another getter".into()
                }
                19 => capture.capture.errno = Some(libc::EIO),
                20 => capture.release = None,
                21 => capture.release.as_mut().unwrap().returned = -1,
                22 => capture.metadata.as_mut().unwrap().stat.inode += 1,
                23 => capture.error = Some("unresolved metadata read".into()),
                _ => unreachable!(),
            }
            assert!(capture.checked(&effect).is_err(), "changed {changed}");
        }
    }
    #[test]
    fn late_slot_races_return_no_match_and_never_manufacture_a_retirement() {
        let (effect, original) = fixture();
        let mut absent = original.clone();
        absent.capture.returned = -1;
        absent.capture.errno = Some(libc::EBADF);
        absent.candidate_stat = None;
        absent.observation = None;
        absent.metadata = None;
        absent.release = None;
        assert_eq!(absent.checked(&effect).unwrap(), None);
        let mut failed = absent.clone();
        failed.capture.errno = Some(libc::EPERM);
        assert!(failed.checked(&effect).is_err());
        for file in [0, 20] {
            let mut reused = original.clone();
            reused.observation.as_mut().unwrap().raw.identity.object = file;
            // A complete other-file observation can be ACKed, but its metadata
            // cannot qualify the original installation under any slot number.
            assert!(reused.command(&effect).unwrap().is_some());
            assert!(reused.checked(&effect).is_err());
            reused.metadata = None;
            assert_eq!(reused.checked(&effect).unwrap(), None);
        }
        let mut other = original;
        other.candidate_stat.as_mut().unwrap().mode = libc::S_IFREG;
        other.observation = None;
        other.metadata = None;
        assert_eq!(other.checked(&effect).unwrap(), None);
    }
    #[test]
    fn fresh_held_profile_requires_actual_raw_state_and_all_original_getter_guards() {
        let (effect, capture) = fixture();
        let original = capture.checked(&effect).unwrap().unwrap();
        for changed in 0..17 {
            let mut value = original.clone();
            let (mut key, normalization) = profile();
            match changed {
                0 => key.domain = libc::AF_INET6,
                1 => value.metadata.tcp = None,
                2 => value.metadata.tcp.as_mut().unwrap().low_water = 2,
                3 => value.metadata.tcp.as_mut().unwrap().receive_buffer += 1,
                4 => value.metadata.tcp.as_mut().unwrap().peek_offset = Some(0),
                5 => value.metadata.tcp.as_mut().unwrap().receive_timeout = (0, 1),
                6 => value.metadata.tcp.as_mut().unwrap().send_timeout = (0, 1),
                7 => value.raw.receive_timeout_ticks = 0,
                8 => value.raw.send_timeout_ticks = 0,
                9 => value.raw.userlocks = 1,
                10 => value.raw.scaling_ratio = 127,
                11 => value.raw.tcp_state = 1,
                12 => value.raw.socket_option_memory = 1,
                13 => value.raw.child_spin_locked = 1,
                14 => {
                    value.raw.lowat = 0;
                    value.metadata.tcp.as_mut().unwrap().low_water = 0;
                }
                15 => {
                    value.raw.receive_buffer = -1;
                    value.metadata.tcp.as_mut().unwrap().receive_buffer = -1;
                }
                16 => {
                    value.raw.peek_offset = 0;
                    value.metadata.tcp.as_mut().unwrap().peek_offset = None;
                }
                _ => unreachable!(),
            }
            assert!(
                value.fresh_profile(key, normalization).is_err(),
                "changed {changed}"
            );
        }
    }
}
