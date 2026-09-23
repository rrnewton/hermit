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
        Ok::<_, io::Error>((args, artifacts))
    })();
    match prepared {
        Ok((args, artifacts)) => {
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
                    input, args.run, library, object,
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
