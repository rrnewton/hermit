//! Authoritative native startup configuration over a consumed temporary FD.
//!
//! This is configuration ownership only. It does not publish a Tool, readiness,
//! a native thread, a private phase, a continuation or a descriptor fallback.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;

use detcore::Config;

/// The native launch owner supplies this explicit selection. A missing
/// coordinator never changes a Coordinated launch into a local one.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u32)]
pub(super) enum Mode {
    Standalone = 1,
    Coordinated = 2,
}

#[derive(Clone, Copy)]
pub(super) enum Source {
    Coordinator,
    StandaloneJson,
    StandaloneParseFallback,
    StandaloneDefault,
}

pub(super) struct Configuration {
    pub config: Config,
    pub coordinator: Option<PathBuf>,
    pub source: Source,
}

/// Preserve the maintained native launch selection, including its strict
/// standalone JSON fallback. `Some(empty)` is a present, invalid control and
/// must reach the socket validator; it is never treated as absent.
pub(super) fn select(
    mode: Mode,
    control: Option<&OsStr>,
    fingerprint: Option<&OsStr>,
    standalone_json: Option<&str>,
) -> io::Result<Configuration> {
    let (mut config, coordinator, source) = match control {
        Some(path) => {
            let fingerprint = fingerprint.ok_or_else(|| io::Error::other(
                "native coordinator announced no Detcore configuration fingerprint"))?;
            (receive(Path::new(path), fingerprint)?, Some(PathBuf::from(path)), Source::Coordinator)
        }
        None if mode == Mode::Coordinated => {
            return Err(io::Error::other("coordinated native startup requires a coordinator socket"));
        }
        None => {
            let (config, source) = match standalone_json {
                Some(value) if !value.is_empty() => match detcore::decode_backend_config_json(value) {
                    Ok(config) => (config, Source::StandaloneJson),
                    Err(_) => (strict_default(), Source::StandaloneParseFallback),
                },
                _ => (strict_default(), Source::StandaloneDefault),
            };
            (config, None, source)
        }
    };
    // The existing native executor owns the external sequential scheduler.
    // These are its original physical facts, not an alternate syscall policy.
    config.max_timeslice = None;
    config.sequentialize_threads = true;
    config.backend.supports_parked_write_signal_interruption = false;
    config.backend.promotes_open_files_before_native_lifecycle = true;
    Ok(Configuration { config, coordinator, source })
}

fn strict_default() -> Config {
    Config {
        sequentialize_threads: true,
        deterministic_io: true,
        max_timeslice: None,
        ..Config::default()
    }
}

/// `fingerprint` is the actual coordinator's announcement. Check it BEFORE
/// decoding its positional Config or any Global RPC. A present control never
/// falls back to JSON, an ambient socket, empty config or another runtime.
fn receive(path: &Path, fingerprint: &OsStr) -> io::Result<Config> {
    if let Some(mismatch) = detcore::config_fingerprint_mismatch(fingerprint) {
        return Err(io::Error::other(mismatch));
    }
    read_initial(path)
}

/// The maintained initial native config protocol: validate before socket;
/// receive exactly the full bounded frame; consume the FD with ONE close
/// attempt before either result. EINTR does not grant a second close attempt.
fn read_initial(path: &Path) -> io::Result<Config> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // Immediately adopt ownership so every exit consumes this exact FD.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut stream = UnixStream::from(owned);
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    let result = if unsafe { libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        reverie_rpc_transport::read_config_frame(&mut stream)
            .and_then(|payload| reverie_rpc_transport::codec::decode_exact(&payload))
            .map_err(io::Error::other)
    };
    let fd = stream.into_raw_fd();
    let close = if unsafe { libc::close(fd) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
    // Preserve the receive failure even if close also failed. A successful
    // receive with failed close does not return a usable configuration.
    match (result, close) {
        (Ok(config), Ok(())) => Ok(config),
        (Err(receive), Ok(())) => Err(receive),
        (Ok(_), Err(close)) => Err(close),
        (Err(receive), Err(close)) => Err(io::Error::new(receive.kind(), format!("{receive}; temporary config close also failed: {close}"))),
    }
}
