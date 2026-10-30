/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Persistent native references, classified before their actual import. A
//! mutable nested-pointer snapshot is not a certificate for its later use.

use reverie::Guest;
use reverie::reserved_input::InputDescriptorFacts;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::Span;
use super::capability;
use super::effective_spans;
use super::expose_global;
use super::expose_local;
use super::expose_local_overlap;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const PR_SET_MEMORY_MERGE: usize = 67;

/// A negative is established from the actual retained OFD. A guessed target
/// PID is never converted into a RegionLifetime: proc mem keeps its original
/// MM across exec and numeric PID reuse.
fn may_be_process_memory(facts: &InputDescriptorFacts, mountinfo: Option<&[u8]>) -> bool {
    match facts.filesystem {
        Ok(fs) if fs != PROC_SUPER_MAGIC => return false,
        Err(_) => return true,
        _ => {}
    }
    let Some(path) = mountinfo.and_then(|mounts| super::objects::internal_path(facts, mounts))
    else {
        return true;
    };
    let parts: Vec<_> = path
        .split(|byte| *byte == b'/')
        .filter(|s| !s.is_empty())
        .collect();
    let pid = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    match parts.as_slice() {
        [id, leaf] => pid(id) && *leaf == b"mem",
        [id, task, tid, leaf] => pid(id) && *task == b"task" && pid(tid) && *leaf == b"mem",
        _ => false,
    }
}

fn is_known_uffd_import(request: usize) -> bool {
    // Linux UAPI userfaultfd.h: _IOWR(0xaa, 0, uffdio_register[32])
    // and _IOWR(0xaa, 5, uffdio_move[40]), on both admitted native ABIs.
    // REGISTER retains a range; MOVE changes its mapping. Do not classify
    // another direction/size or an unrelated ioctl by its low command byte.
    matches!(request, 0xc020_aa00 | 0xc028_aa05)
}

/// Whether a future background read/write has an unbound file effect.
///
/// Neither a private table nor a sampled ordinary OFD certifies the later
/// native lookup: ordinary injection preparation can forward a caught signal
/// and single-step a handler instruction before restoring the syscall inputs.
/// The current backend exposes no final stopped binding for ordinary fdget.
/// A possibly shared table also permits a peer replacement after turn release.
///
/// The caller therefore adds UnknownTarget transient access for any possible
/// descriptor read/write, alongside its ordinary local buffers, and retains
/// it through actual native completion, decoder/copyout and deterministic
/// readmission. This is not a permanent taint on every read or write.
pub(crate) fn background_descriptor_effect_is_unverified<
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
>(
    guest: &G,
    call: Syscall,
) -> bool {
    capability(guest).is_some() && descriptor_io_may_access(call)
}

fn descriptor_io_may_access(call: Syscall) -> bool {
    if !matches!(
        call.name(),
        "read"
            | "pread64"
            | "readv"
            | "preadv"
            | "preadv2"
            | "write"
            | "pwrite64"
            | "writev"
            | "pwritev"
            | "pwritev2"
    ) {
        return false;
    }
    let (_, args) = call.into_parts();
    // These are immutable syscall-register negatives, not sampled OFD facts.
    if (args.arg0 as i32) < 0 || args.arg2 == 0 {
        return false;
    }
    !matches!(call.name(), "pread64" | "pwrite64") || (args.arg3 as i64) >= 0
}

fn ancillary_import_may_export(call: Syscall) -> bool {
    let (_, args) = call.into_parts();
    (args.arg0 as i32) >= 0
        && match call.name() {
            "sendmsg" => true,
            "sendmmsg" => args.arg2 != 0,
            _ => false,
        }
}

/// A direct positional offset is immutable syscall input; a file cursor or
/// indirect vector length is not. Never read lseek/fdinfo and call the sampled
/// shared OFD cursor a bound on the later native write. A proc-mem description
/// targets the MM retained in file->private_data, including across holder exec,
/// so an unproved target remains global even when the offset is known.
fn proc_write_may_reach(call: Syscall, reserved: Span) -> bool {
    let (_, args) = call.into_parts();
    if args.arg2 == 0 {
        return false;
    }
    if call.name() == "pwrite64" {
        if (args.arg3 as i64) < 0 {
            return false; // ksys_pwrite64 rejects before fdget/vfs_write.
        }
        return effective_spans(args.arg3 as u64, args.arg2 as u64)
            .into_iter()
            .any(|span| span.overlaps(reserved));
    }
    true // Shared cursor or mutable iovec extent is unproved.
}

/// Run immediately before native issue, and also before a covered background
/// operation releases its grant. Repetition is monotone, not a fresh proof.
pub(crate) fn before_native_call<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    call: Syscall,
) -> Result<(), reverie::Error> {
    if capability(guest).is_none() {
        return Ok(());
    }
    let (_, args) = call.into_parts();
    match call.name() {
        "madvise" if args.arg2 == libc::MADV_MERGEABLE as usize => {
            expose_local_overlap(
                guest,
                args.arg0 as u64,
                args.arg1 as u64,
                "mergeable-reserved-page",
            )?;
        }
        "prctl" if args.arg0 == PR_SET_MEMORY_MERGE && args.arg1 != 0 => {
            expose_local(guest, "mergeable-reserved-page")?;
        }
        "vmsplice" => {
            // The actual iovec may be changed by an already-running native
            // writer. No owned immutable vector-import proof exists here.
            expose_local(guest, "unverified-persistent-import")?;
        }
        "sendto" if args.arg3 & libc::MSG_ZEROCOPY as usize != 0 => {
            expose_local_overlap(
                guest,
                args.arg1 as u64,
                args.arg2 as u64,
                "pinned-reserved-source",
            )?;
        }
        "sendmsg" | "sendmmsg" => {
            let flags = if call.name() == "sendmsg" {
                args.arg2
            } else {
                args.arg3
            };
            if flags & libc::MSG_ZEROCOPY as usize != 0 {
                expose_local(guest, "unverified-persistent-import")?;
            }
            if ancillary_import_may_export(call) {
                // Rechecked after retries and placeholder/export-report awaits.
                // Neither an empty ancillary snapshot nor a matching modeled
                // guest peer binds the later native import. The backend's
                // ordinary syscall-skip prologue can execute a caught-signal
                // handler instruction after this hook. No internal-peer
                // exemption is certified by this pre-prologue snapshot.
                before_descriptor_export(guest, &[], true)?;
            }
        }
        "ioctl" => {
            if (args.arg0 as i32) >= 0 && is_known_uffd_import(args.arg1) {
                // The exact known command can retain a foreign-MM reference.
                // A sampled non-UFFD descriptor, even under a current grant
                // or private table, does not bind the later native lookup
                // across ordinary injection preparation. Mutable request
                // bytes likewise do not certify what the kernel imports.
                expose_global(guest, "unverified-persistent-import")?;
            }
            // S5 deliberately covers only the unknown direct argument. Do
            // not invent protection for an opaque driver's nested pointers.
            expose_local_overlap(
                guest,
                args.arg2 as u64,
                1,
                "unknown-ioctl-reserved-argument",
            )?;
        }
        "write" | "pwrite64" | "writev" | "pwritev" | "pwritev2" => {
            let Some((control, task)) = capability(guest) else {
                return Ok(());
            };
            // No permanent all-write taint after a background release. Its
            // unbound destination is instead held by BackgroundAccess's
            // UnknownTarget lease. The earlier in-grant classification still
            // publishes any identified process-memory force-write cause.
            if control.current_grant(task).is_none() {
                return Ok(());
            }
            let reserved = control.layout(task).map_or(Span::Unknown, |layout| {
                Span::bytes(layout.base, layout.page_bytes as u64)
            });
            if !proc_write_may_reach(call, reserved) || (args.arg0 as i32) < 0 {
                return Ok(());
            }
            let snapshot = guest.reserved_input_descriptors();
            let possible = snapshot.as_ref().map_or(true, |snapshot| {
                snapshot
                    .descriptors
                    .iter()
                    .find(|fd| fd.fd == args.arg0 as i32)
                    .is_none_or(|fd| {
                        let cannot_write = fd.status_flags.as_ref().is_ok_and(|flags| {
                            *flags & libc::O_PATH != 0 || *flags & libc::O_ACCMODE == libc::O_RDONLY
                        });
                        !cannot_write
                            && may_be_process_memory(fd.facts(), snapshot.mountinfo.as_deref().ok())
                    })
            });
            if possible {
                expose_global(guest, "forced-reserved-write")?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Called for the actual ancillary view even after old scheduler export
/// accounting was reported. A descriptor number is not retained identity.
pub(crate) fn before_descriptor_export<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    _descriptors: &[i32],
    _unread: bool,
) -> Result<(), reverie::Error> {
    if capability(guest).is_none() {
        return Ok(());
    }
    // These callers classify a potentially external ancillary import. Their
    // entry bytes are reinstalled in guest memory, not an immutable kernel
    // binding; a background kernel writer can still change the header/control
    // or a shared-table slot before native import. A readable empty list, or
    // a snapshot containing only O_PATH, cannot certify an absent export.
    // Keep the actual snapshots for their existing scheduler bookkeeping;
    // this stronger persistence policy grants them no new import authority.
    expose_global(guest, "unverified-persistent-import")?;
    expose_global(guest, "exported-process-memory-authority")?;
    expose_global(guest, "exported-description-state-unverified")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use reverie::reserved_input::InputNativeError;
    use reverie::reserved_input::InputPathContext;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;

    fn call(number: Sysno, args: [usize; 6]) -> Syscall {
        Syscall::from_raw(
            number,
            SyscallArgs {
                arg0: args[0],
                arg1: args[1],
                arg2: args[2],
                arg3: args[3],
                arg4: args[4],
                arg5: args[5],
            },
        )
    }

    fn facts(filesystem: u64, path: &[u8]) -> InputDescriptorFacts {
        InputDescriptorFacts {
            fd: 3,
            status_flags: Ok(libc::O_RDWR),
            async_signal: Ok(0),
            async_owner: Ok((libc::F_OWNER_PID, 0)),
            identity: Ok((libc::makedev(0, 23), 7, libc::S_IFREG | 0o600)),
            filesystem: Ok(filesystem),
            mount_id: Ok(27),
            proc_path: Ok(path.to_vec()),
            path_context: Ok(InputPathContext {
                reader_mount_namespace: (1, 2),
                target_mount_namespace: (1, 2),
                reader_root: (3, 4),
                target_root: (3, 4),
                reader_cgroup_namespace: (5, 6),
                target_cgroup_namespace: (5, 6),
            }),
        }
    }

    #[test]
    fn process_memory_classification_uses_native_filesystem_and_bind_root() {
        let mounts = b"27 1 0:23 / /proc rw - proc proc rw\n";
        for path in [b"/proc/31/mem".as_slice(), b"/proc/31/task/32/mem"] {
            assert!(may_be_process_memory(
                &facts(PROC_SUPER_MAGIC, path),
                Some(mounts)
            ));
        }
        let alias = facts(PROC_SUPER_MAGIC, b"/ordinary-name");
        let bound = b"27 1 0:23 /31/mem /ordinary-name rw - proc proc rw\n";
        assert!(may_be_process_memory(&alias, Some(bound)));
        for path in [
            b"/proc/31/maps".as_slice(),
            b"/proc/sys/kernel/pid_max",
            b"/proc/31/memory",
        ] {
            assert!(!may_be_process_memory(
                &facts(PROC_SUPER_MAGIC, path),
                Some(mounts)
            ));
        }
        // A pathname alone neither grants nor removes process-memory authority.
        assert!(!may_be_process_memory(
            &facts(0xef53, b"/proc/31/mem"),
            None
        ));
        assert!(may_be_process_memory(&alias, None));
        let mut hidden = facts(PROC_SUPER_MAGIC, b"/proc/31/maps");
        hidden.path_context.as_mut().unwrap().target_root = (9, 10);
        assert!(may_be_process_memory(&hidden, Some(mounts)));
        hidden.filesystem = Err(InputNativeError {
            operation: "fstatfs",
            errno: Some(libc::EIO),
        });
        assert!(may_be_process_memory(&hidden, Some(mounts)));
    }

    #[test]
    fn background_descriptor_effects_use_only_immutable_no_access_negatives() {
        for number in [Sysno::read, Sysno::write, Sysno::readv, Sysno::writev] {
            assert!(descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, 0, 0, 0]
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [usize::MAX, 0x1000, 1, 0, 0, 0],
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [3, 0x1000, 0, 0, 0, 0]
            )));
        }
        for number in [Sysno::pread64, Sysno::pwrite64] {
            assert!(descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, 0, 0, 0]
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, usize::MAX, 0, 0],
            )));
        }
        // pwritev2's -1 offset selects the shared file cursor; it is not the
        // native EINVAL negative of pwrite64. It still requires the lease.
        assert!(descriptor_io_may_access(call(
            Sysno::pwritev2,
            [3, 0x1000, 1, usize::MAX, usize::MAX, 0],
        )));
        assert!(!descriptor_io_may_access(call(Sysno::getpid, [0; 6])));
    }

    #[test]
    fn ancillary_import_never_uses_a_pre_prologue_peer_or_empty_buffer_snapshot() {
        // The payload/header address does not prove immutable native import;
        // NULL or an apparently empty msghdr must not manufacture an exemption.
        for pointer in [0, 0x1000] {
            assert!(ancillary_import_may_export(call(
                Sysno::sendmsg,
                [3, pointer, 0, 0, 0, 0],
            )));
            assert!(ancillary_import_may_export(call(
                Sysno::sendmmsg,
                [3, pointer, 1, 0, 0, 0],
            )));
        }
        assert!(!ancillary_import_may_export(call(
            Sysno::sendmsg,
            [usize::MAX, 0x1000, 0, 0, 0, 0],
        )));
        assert!(!ancillary_import_may_export(call(
            Sysno::sendmmsg,
            [3, 0x1000, 0, 0, 0, 0],
        )));
        assert!(!ancillary_import_may_export(call(Sysno::getpid, [0; 6])));
    }

    #[test]
    fn positional_proc_writes_bound_only_immutable_scalar_extent() {
        let base = 0x7101_0000;
        let page = Span::bytes(base, 4096);
        for (offset, length, expected) in [
            (base - 1, 1, false),
            (base - 1, 2, true),
            (base, 4096, true),
            (base + 4096, 1, false),
            (base, 0, false),
            (u64::MAX, 1, false),
            ((1u64 << 62) | base, 1, true),
        ] {
            assert_eq!(
                proc_write_may_reach(
                    call(Sysno::pwrite64, [3, 0x1000, length, offset as usize, 0, 0]),
                    page
                ),
                expected,
                "offset={offset:#x} length={length}"
            );
        }
        assert!(proc_write_may_reach(
            call(Sysno::write, [3, 0x1000, 1, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwritev, [3, 0x1000, 1, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwritev2, [3, 0x1000, 1, usize::MAX, usize::MAX, 0]),
            page
        ));
        assert!(!proc_write_may_reach(
            call(Sysno::writev, [3, 0x1000, 0, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwrite64, [3, 0x1000, 1, base as usize, 0, 0]),
            Span::Unknown
        ));
    }

    #[test]
    fn persistent_uffd_commands_do_not_swallow_unrelated_ioctl_space() {
        assert!(is_known_uffd_import(0xc020_aa00));
        assert!(is_known_uffd_import(0xc028_aa05));
        for request in [
            0x8010_aa01,
            0xc018_aa3f,
            0xc028_aa03,
            0x8020_aa00,
            0xc018_aa00,
            libc::FIONREAD as usize,
        ] {
            assert!(!is_known_uffd_import(request), "{request:#x}");
        }
    }
}
