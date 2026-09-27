/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

//! Private capability-unit entry. No normal CLI, backend, or Tokio runtime is
//! initialized here. Its protocol remains the actual owned service protocol.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

const FLAG: &str = "--accepted-private-stdin-v1";

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    object: PathBuf,
    library: PathBuf,
    run: [u8; 16],
}

impl Arguments {
    fn parse(args: &[OsString]) -> io::Result<Self> {
        if args.len() != 7
            || args[0] != FLAG
            || args[1] != "--object"
            || args[3] != "--library"
            || args[5] != "--run"
        {
            return Err(io::Error::other(
                "malformed private accepted-provider arguments",
            ));
        }
        let text = args[6]
            .to_str()
            .ok_or_else(|| io::Error::other("run identity is not UTF-8"))?;
        if text.len() != 32
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::other(
                "run identity must be 32 lowercase hexadecimal digits",
            ));
        }
        let mut run = [0; 16];
        for (index, value) in run.iter_mut().enumerate() {
            *value = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(io::Error::other)?;
        }
        if u64::from_le_bytes(run[..8].try_into().unwrap()) == 0 {
            return Err(io::Error::other("zero provider incarnation"));
        }
        let object = PathBuf::from(&args[2]);
        let library = PathBuf::from(&args[4]);
        if !object.is_absolute() || !library.is_absolute() {
            return Err(io::Error::other("private artifact paths must be absolute"));
        }
        Ok(Self {
            object,
            library,
            run,
        })
    }
}

pub(super) fn requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == FLAG)
}

const GROUPED_STARTUP: &str = "--grouped-startup-controller-private-stdin-v1";
const GROUPED_RUNTIME_KEEPER: &str = "--grouped-runtime-keeper-private-stdin-v1";
const GROUPED_SOURCE_OWNER: &str = "--grouped-source-owner-private-stdin-v1";
const GROUPED_SOURCE: &str = "--grouped-source-private-stdin-v1";
const GROUPED_LEAVES: &str = "--grouped-leaves-private-stdin-v1";

pub(super) fn grouped_requested() -> bool {
    std::env::args_os().nth(1).is_some_and(|argument| {
        argument == GROUPED_STARTUP
            || argument == GROUPED_RUNTIME_KEEPER
            || argument == GROUPED_SOURCE_OWNER
            || argument == GROUPED_SOURCE
            || argument == GROUPED_LEAVES
    })
}

/// These entries precede ordinary CLI/runtime initialization. The endpoint is
/// retained even when argv or descriptor validation fails; parsing never grants
/// authority to create probes or to construct a completed startup state.
pub(super) fn run_grouped(input: io::Result<Option<File>>) -> ! {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == GROUPED_SOURCE || argument == GROUPED_LEAVES)
    {
        run_grouped_delegated(input)
    }
    let input = ManuallyDrop::new(input);
    let checked = (|| {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if args.len() != 5
            || (args[0] != GROUPED_STARTUP
                && args[0] != GROUPED_RUNTIME_KEEPER
                && args[0] != GROUPED_SOURCE_OWNER)
            || args[1] != "--run"
            || args[3] != "--deadline-ns"
        {
            return Err(io::Error::other("malformed private grouped arguments"));
        }
        let text = args[2]
            .to_str()
            .ok_or_else(|| io::Error::other("grouped run identity is not UTF-8"))?;
        if text.len() != 32
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::other(
                "grouped run requires32 lowercase hex digits",
            ));
        }
        let mut run = [0u8; 16];
        for (index, byte) in run.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(io::Error::other)?;
        }
        if u64::from_le_bytes(run[..8].try_into().unwrap()) == 0 {
            return Err(io::Error::other("zero grouped provider incarnation"));
        }
        let deadline = args[4]
            .to_str()
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::other("missing original grouped deadline"))?;
        let file = match &*input {
            Ok(Some(file)) => file,
            _ => return Err(io::Error::other("private grouped stdin missing")),
        };
        validate_private_stdin(file)?;
        validate_inherited_fds(file)?;
        Ok((args[0].clone(), run, deadline))
    })();
    let (entry, run, deadline) = match checked {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("private grouped entry admission failed: {error}");
            unsafe { libc::_exit(125) }
        }
    };
    let input: OwnedFd = match ManuallyDrop::into_inner(input) {
        Ok(Some(file)) => file.into(),
        _ => unreachable!("validated grouped endpoint changed without mutation"),
    };
    // SAFETY: the exact early endpoint/census has been checked before any
    // runtime creation. These never-returning entries retain their input before
    // authenticating the SCM configuration, original actors and native bounds.
    unsafe {
        if entry == GROUPED_STARTUP {
            detcore::network_runtime::run_grouped_startup_controller_process(input, run, deadline)
        } else if entry == GROUPED_SOURCE_OWNER {
            detcore::network_runtime::run_grouped_source_owner_process(input, run, deadline)
        } else {
            detcore::network_runtime::run_grouped_runtime_keeper_process(input, run, deadline)
        }
    }
}

/// The manager's OpenFiles are the only additional inherited descriptions.
/// Keep the ordinary accepted entry's stricter stdio-only census unchanged.
fn validate_delegated_census(input: &File, source: bool) -> io::Result<()> {
    let roles = if source {
        "hermit_kprobe_control:hermit_kprobe_profile:hermit_trace_events"
    } else {
        "hermit_group_id:hermit_group_format:hermit_group_enable"
    };
    if std::env::var("LISTEN_PID").ok().as_deref() != Some(std::process::id().to_string().as_str())
        || std::env::var("LISTEN_FDS").ok().as_deref() != Some("3")
        || std::env::var("LISTEN_FDNAMES").ok().as_deref() != Some(roles)
        || input.as_raw_fd() <= 5
    {
        return Err(io::Error::other("grouped manager OpenFiles roles differ"));
    }
    let directory = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error());
    }
    let own_directory = unsafe { libc::dirfd(directory) };
    let checked = (|| {
        if own_directory < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut mask = 0u8;
        let mut input_seen = false;
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error);
                }
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            let fd: i32 = name
                .to_str()
                .map_err(io::Error::other)?
                .parse()
                .map_err(io::Error::other)?;
            if fd == own_directory {
                continue;
            }
            if fd == input.as_raw_fd() {
                if input_seen {
                    return Err(io::Error::other("duplicate captured grouped stdin"));
                }
                input_seen = true;
            } else if (0..=5).contains(&fd) && mask & (1 << fd) == 0 {
                mask |= 1 << fd;
            } else {
                return Err(io::Error::other("unexpected grouped inherited descriptor"));
            }
        }
        if mask != 63 || !input_seen {
            return Err(io::Error::other(
                "grouped inherited descriptor population differs",
            ));
        }
        for role in 0..3 {
            let fd = 3 + role;
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0
                || unsafe { libc::fstatfs(fd, filesystem.as_mut_ptr()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let stat = unsafe { stat.assume_init() };
            let filesystem = unsafe { filesystem.assume_init() };
            let kind = if source && role == 2 {
                libc::S_IFDIR
            } else {
                libc::S_IFREG
            };
            let access = if source && role == 0 {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            };
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            // Linux exposes __O_LARGEFILE on x86_64 even though glibc's
            // source-level O_LARGEFILE constant is zero on this ABI.
            let allowed =
                libc::O_ACCMODE | 0o100000 | libc::O_NOCTTY | libc::O_NOFOLLOW | libc::O_DIRECTORY;
            if filesystem.f_type != 0x74726163
                || stat.st_uid != 0
                || stat.st_gid != 0
                || stat.st_mode & libc::S_IFMT != kind
                || flags < 0
                || flags & libc::O_ACCMODE != access
                || flags & !allowed != 0
                || unsafe { libc::fcntl(fd, libc::F_GETFD) } != libc::FD_CLOEXEC
            {
                return Err(io::Error::other(
                    "grouped delegated tracefs description differs",
                ));
            }
        }
        Ok(())
    })();
    let closed = unsafe { libc::closedir(directory) };
    checked?;
    if closed != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_grouped_delegated(input: io::Result<Option<File>>) -> ! {
    let input = ManuallyDrop::new(input);
    let checked = (|| {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if args.len() != 9
            || (args[0] != GROUPED_SOURCE && args[0] != GROUPED_LEAVES)
            || args[1] != "--unit"
            || args[3] != "--run"
            || args[5] != "--incarnation"
            || args[7] != "--deadline-ns"
        {
            return Err(io::Error::other("malformed grouped delegated arguments"));
        }
        let hex = |text: &str| {
            text.len() == 32
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && text.bytes().any(|byte| byte != b'0')
        };
        let unit = args[2]
            .to_str()
            .ok_or_else(|| io::Error::other("grouped unit is not UTF-8"))?;
        if !unit
            .strip_prefix("hermit-accepted-")
            .and_then(|name| name.strip_suffix(".service"))
            .is_some_and(hex)
        {
            return Err(io::Error::other(
                "grouped unit is not the exact accepted purpose",
            ));
        }
        let text = args[4]
            .to_str()
            .filter(|text| hex(text))
            .ok_or_else(|| io::Error::other("invalid grouped delegated run"))?;
        let mut run = [0u8; 16];
        for (index, byte) in run.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(io::Error::other)?;
        }
        let number = |value: &OsString| -> io::Result<u64> {
            value
                .to_str()
                .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|text| text.parse::<u64>().ok())
                .filter(|value| *value != 0)
                .ok_or_else(|| io::Error::other("invalid original grouped numeric argument"))
        };
        let incarnation = number(&args[6])?;
        let deadline = number(&args[8])?;
        if incarnation != u64::from_le_bytes(run[..8].try_into().unwrap()) {
            return Err(io::Error::other("grouped incarnation differs from run"));
        }
        let file = match &*input {
            Ok(Some(file)) => file,
            _ => return Err(io::Error::other("grouped delegated input missing")),
        };
        let source = args[0] == GROUPED_SOURCE;
        validate_private_stdin(file)?;
        validate_delegated_census(file, source)?;
        Ok((source, unit.to_owned(), run, incarnation, deadline))
    })();
    let (source, unit, run, incarnation, deadline) = match checked {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("grouped delegated entry admission failed: {error}");
            // Raw manager descriptors and captured input remain until PF_EXITING.
            unsafe { libc::_exit(125) }
        }
    };
    let input: OwnedFd = match ManuallyDrop::into_inner(input) {
        Ok(Some(file)) => file.into(),
        _ => unreachable!("validated delegated endpoint changed without mutation"),
    };
    use std::os::fd::FromRawFd;
    // SAFETY: the exact three live inherited descriptions were checked above,
    // no Rust owner exists, and this early process has no competing thread.
    let files = unsafe {
        [
            OwnedFd::from_raw_fd(3),
            OwnedFd::from_raw_fd(4),
            OwnedFd::from_raw_fd(5),
        ]
    };
    // SAFETY: native entries retain every input before further validation; the
    // actual package, peers and original cutoff are still authenticated there.
    unsafe {
        if source {
            detcore::network_runtime::run_grouped_source_process(
                input,
                files,
                unit,
                run,
                incarnation,
                deadline,
            )
        } else {
            detcore::network_runtime::run_grouped_leaf_delegate_process(
                input,
                files,
                unit,
                run,
                incarnation,
                deadline,
            )
        }
    }
}

/// The readback helper shares the same private pre-runtime descriptor boundary.
/// Its only operation is querying the original typed IDs after provider close.
pub(super) fn readback_requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--accepted-readback-private-stdin-v1")
}
pub(super) fn run_readback(input: io::Result<Option<File>>) -> ! {
    let input = ManuallyDrop::new(input);
    let checked = (|| {
        if std::env::args_os().count() != 3 {
            return Err(io::Error::other("unexpected private readback arguments"));
        }
        let deadline = std::env::args_os()
            .nth(2)
            .and_then(|arg| arg.into_string().ok())
            .filter(|arg| !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|arg| arg.parse::<u64>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::other("missing original accepted bootstrap deadline"))?;
        let file = match &*input {
            Ok(Some(file)) => file,
            _ => return Err(io::Error::other("private readback stdin missing")),
        };
        validate_private_stdin(file)?;
        validate_inherited_fds(file)?;
        Ok(deadline)
    })();
    let deadline = match checked {
        Ok(deadline) => deadline,
        Err(error) => {
            eprintln!("accepted private readback admission failed: {error}");
            unsafe { libc::_exit(125) }
        }
    };
    hermit::accepted_terminal::run_private_readback(ManuallyDrop::into_inner(input), deadline)
}

fn validate_private_stdin(input: &File) -> io::Result<()> {
    for (option, expected) in [
        (libc::SO_TYPE, libc::SOCK_SEQPACKET),
        (libc::SO_DOMAIN, libc::AF_UNIX),
    ] {
        let mut actual = 0i32;
        let mut size = std::mem::size_of_val(&actual) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                input.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&mut actual as *mut i32).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of_val(&actual) || actual != expected {
            return Err(io::Error::other(
                "private stdin is not the owned Unix seqpacket endpoint",
            ));
        }
    }
    if unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFD) } != libc::FD_CLOEXEC {
        return Err(io::Error::other(
            "captured private stdin is not exactly CLOEXEC",
        ));
    }
    Ok(())
}

fn validate_inherited_fds(input: &File) -> io::Result<()> {
    // This private entry precedes thread/runtime creation. The only inherited
    // descriptors are stdio plus main's exact preinit stdin duplicate. Do not
    // confuse RLIMIT_NOFILE with closing inherited high-numbered descriptors.
    let directory = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error());
    }
    let own_directory = unsafe { libc::dirfd(directory) };
    let result = (|| {
        if own_directory < 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(())
                } else {
                    Err(error)
                };
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            let fd: i32 = name
                .to_str()
                .map_err(io::Error::other)?
                .parse()
                .map_err(io::Error::other)?;
            if ![0, 1, 2, input.as_raw_fd(), own_directory].contains(&fd) {
                return Err(io::Error::other("unexpected inherited helper descriptor"));
            }
        }
    })();
    let closed = unsafe { libc::closedir(directory) };
    result?;
    if closed != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn run(input: io::Result<Option<File>>) -> ! {
    // Keep even an unrecognized queued endpoint through PF_EXITING on an early
    // error; returning Err must not normally close a possibly final SCM right.
    let input = ManuallyDrop::new(input);
    let prepared = (|| {
        let args = Arguments::parse(&std::env::args_os().skip(1).collect::<Vec<_>>())?;
        let file = match &*input {
            Ok(Some(file)) => file,
            Ok(None) => return Err(io::Error::other("private stdin missing")),
            Err(error) => return Err(io::Error::other(error.to_string())),
        };
        validate_private_stdin(file)?;
        validate_inherited_fds(file)?;
        let artifacts = hermit::network_provider_package::SealedAcceptedArtifacts::snapshot(
            &args.object,
            &args.library,
        )?;
        let library_file = artifacts.library_fd().try_clone_to_owned()?;
        Ok::<_, io::Error>((args, artifacts, library_file))
    })();
    match prepared {
        Ok((args, artifacts, library_file)) => {
            let input: OwnedFd = match ManuallyDrop::into_inner(input) {
                Ok(Some(file)) => file.into(),
                _ => unreachable!("validated owned stdin changed without mutation"),
            };
            let (object, library) = artifacts.paths();
            // SAFETY: the reviewed parent capability unit supplied this exact
            // private endpoint, and only immutable local memfds reach dlopen.
            // The service separately authenticates the parent's artifact hashes
            // and BTF before loading; actual bootstrap pidfd gates terminal work.
            // `artifacts` stays owned on this stack across this never-returning
            // call. All final socket rights close only in service PF_EXITING.
            unsafe {
                detcore::network_runtime::run_accepted_provider_process(
                    input,
                    args.run,
                    library,
                    object,
                    library_file,
                )
            }
        }
        Err(error) => {
            eprintln!("accepted provider startup unavailable: {error}");
            // No service/BPF was constructed. The still-owned input and any
            // queued SCM references are released by process exit, not Drop.
            unsafe { libc::_exit(125) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> Vec<OsString> {
        [
            FLAG,
            "--object",
            "/package/object",
            "--library",
            "/package/library",
            "--run",
            "01000000000000000000000000000000",
        ]
        .iter()
        .map(OsString::from)
        .collect()
    }
    #[test]
    fn private_arguments_require_exact_run_and_artifact_paths() {
        let parsed = Arguments::parse(&valid()).unwrap();
        assert_eq!(parsed.run[0], 1);
        assert_eq!(parsed.object, PathBuf::from("/package/object"));
        for case in 0..7 {
            let mut args = valid();
            match case {
                0 => {
                    args.push("extra".into());
                }
                1 => {
                    args[1] = "--library".into();
                }
                2 => {
                    args[2] = "relative".into();
                }
                3 => {
                    args[4] = "relative".into();
                }
                4 => {
                    args[6] = "00000000000000000000000000000000".into();
                }
                5 => {
                    args[6] = "0100000000000000000000000000000A".into();
                }
                _ => {
                    args[6] = "01".into();
                }
            }
            assert!(Arguments::parse(&args).is_err(), "case {case}");
        }
    }
}
