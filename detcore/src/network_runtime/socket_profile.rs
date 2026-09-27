// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.

//! Shared physical receive-profile observation under an already-held network
//! namespace. The retained namespace can outlive its original guest task.

use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;

use detcore_model::network_trace::LinuxReceiveHzV3;
use detcore_model::network_trace::LinuxReceiveNormalizationV3;
use detcore_model::network_trace::StreamSocketKeyV3;
use reverie::Error;

fn profile_error(error: impl std::fmt::Display) -> Error {
    Error::Tool(anyhow::anyhow!(
        "shared network engine refused operation: {error}"
    ))
}

fn namespace_identity(fd: BorrowedFd<'_>) -> Result<(u64, u64), Error> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstat initialized the complete stat object.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

// Record-only namespace bootstrap. The scratch descriptor is in the recorder's
// table, never the guest's. It is never connected, bound, or exposed to the guest.
// A different namespace is an explicit integration failure, not host fallback.
pub(crate) fn record_receive_normalization_in_namespace(
    original_namespace: BorrowedFd<'_>,
    key: StreamSocketKeyV3,
) -> Result<(LinuxReceiveNormalizationV3, bool), Error> {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    let namespace = |path: &str| -> Result<std::fs::File, Error> {
        std::fs::File::open(path).map_err(Error::Io)
    };
    let original_identity = namespace_identity(original_namespace)?;
    let recorder_ns = namespace("/proc/thread-self/ns/net")?;
    if original_identity != namespace_identity(recorder_ns.as_fd())? {
        return Err(profile_error(
            "V3 profile bootstrap needs a namespace-owned helper; host defaults are not authoritative",
        ));
    }
    // No await between pinning the current thread namespace and these syscalls.
    let raw = unsafe {
        libc::socket(
            key.domain,
            key.socket_type | libc::SOCK_CLOEXEC,
            key.protocol,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful socket returned one uniquely owned descriptor.
    let scratch = unsafe { OwnedFd::from_raw_fd(raw) };
    let fd = scratch.as_raw_fd();
    let scalar_get = |name: i32| -> Result<i32, Error> {
        let mut value = 0i32;
        let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if length != std::mem::size_of::<i32>() as libc::socklen_t {
            return Err(profile_error("profile scratch scalar size mismatch"));
        }
        Ok(value)
    };
    let scalar_set = |name: i32, value: i32| -> std::io::Result<()> {
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw const value).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    let timeout_set = |seconds: i64, microseconds: i64| -> Result<(), Error> {
        let value = libc::timeval {
            tv_sec: seconds,
            tv_usec: microseconds,
        };
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const value).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    };
    let timeout_get = || -> Result<(i64, i64), Error> {
        let mut value = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut length = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if length != std::mem::size_of::<libc::timeval>() as libc::socklen_t {
            return Err(profile_error("profile scratch timeout size mismatch"));
        }
        Ok((value.tv_sec, value.tv_usec))
    };
    let read_limits = || -> Result<(u32, u32), Error> {
        fn values(path: &str) -> Result<Vec<u32>, Error> {
            let text = std::fs::read_to_string(path).map_err(Error::Io)?;
            text.split_whitespace()
                .map(|part| {
                    part.parse::<u32>()
                        .map_err(|error| profile_error(format!("invalid {path}: {error}")))
                })
                .collect()
        }
        let system = values("/proc/sys/net/core/rmem_max")?;
        let namespace = values("/proc/sys/net/ipv4/tcp_rmem")?;
        if system.len() != 1 || namespace.len() != 3 {
            return Err(profile_error("receive buffer sysctl shape mismatch"));
        }
        Ok((system[0], namespace[2]))
    };
    let before_limits = read_limits()?;
    timeout_set(0, 1)?;
    let (seconds, microseconds) = timeout_get()?;
    let hz = LinuxReceiveHzV3::from_one_microsecond_probe(seconds, microseconds)
        .ok_or_else(|| profile_error("kernel timeout rounding is outside the audited profile"))?;
    // This socket is private, so irreversible RCVBUF_LOCK is harmless here.
    scalar_set(libc::SO_RCVBUF, 0).map_err(Error::Io)?;
    let minimum_receive_buffer = u32::try_from(scalar_get(libc::SO_RCVBUF)?)
        .map_err(|_| profile_error("negative minimum receive buffer"))?;
    let mut normalization = LinuxReceiveNormalizationV3 {
        hz,
        system_rmem_max: before_limits.0,
        namespace_tcp_rmem_max: before_limits.1,
        minimum_receive_buffer,
        peek_offset_set_supported: false,
    };
    normalization
        .validate()
        .map_err(|error| profile_error(format!("invalid receive profile: {error:?}")))?;
    // Check the HZ300 integer-division boundary rather than infer a whole
    // normalization law from only the one-microsecond witness.
    for (seconds, microseconds) in [(0, 999_999), (-1, 0), (0, 0)] {
        timeout_set(seconds, microseconds)?;
        let expected = normalization
            .normalize_timeout(seconds, microseconds)
            .map_err(|error| profile_error(format!("timeout normalization: {error:?}")))?
            .exposed_timeval(hz);
        if timeout_get()? != expected {
            return Err(profile_error("kernel timeout profile witness mismatch"));
        }
    }
    let peek_offset_set_supported = match scalar_set(libc::SO_PEEK_OFF, 0) {
        Ok(()) => {
            if scalar_get(libc::SO_PEEK_OFF)? != 0 {
                return Err(profile_error("scratch peek cursor mismatch"));
            }
            true
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOPROTOOPT)
            ) =>
        {
            false
        }
        Err(error) => return Err(Error::Io(error)),
    };
    normalization.peek_offset_set_supported = peek_offset_set_supported;
    if before_limits != read_limits()?
        || original_identity != namespace_identity(original_namespace)?
        || namespace_identity(recorder_ns.as_fd())?
            != namespace_identity(namespace("/proc/thread-self/ns/net")?.as_fd())?
    {
        return Err(profile_error(
            "receive profile namespace or sysctl changed during bootstrap",
        ));
    }
    // OwnedFd drops on every success/error path. No socket network operation ran.
    Ok((normalization, peek_offset_set_supported))
}

#[cfg(test)]
mod tests {
    use detcore_model::network_trace::NetworkTransportV2;

    use super::*;

    fn tcp_key() -> StreamSocketKeyV3 {
        StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        }
    }

    #[test]
    fn retained_namespace_profile_survives_its_observing_thread() {
        let namespace =
            std::thread::spawn(|| std::fs::File::open("/proc/thread-self/ns/net").unwrap())
                .join()
                .unwrap();
        let (profile, peek_supported) =
            record_receive_normalization_in_namespace(namespace.as_fd(), tcp_key()).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.peek_offset_set_supported, peek_supported);
    }

    #[test]
    fn profile_refuses_an_fd_that_is_not_the_current_network_namespace() {
        let other = std::fs::File::open("/dev/null").unwrap();
        let error =
            record_receive_normalization_in_namespace(other.as_fd(), tcp_key()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("host defaults are not authoritative")
        );
    }
}
