//! Bounded native permission for one private anonymous event arena.
//! This does not model general VM state or certify a guest-memory copy.
use std::sync::Arc;

use reverie::InjectedSyscallEvent as Event;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::network_runtime::ForegroundRoot;

#[derive(Debug, Clone)]
pub(crate) struct OriginalArena {
    root: Arc<ForegroundRoot>,
    generation: u64,
    start: u64,
    end: u64,
}
impl OriginalArena {
    pub(crate) fn contains(&self, owner: NetworkStreamOwner, address: u64) -> bool {
        self.root.is_current(owner)
            && address >= self.start
            && address.checked_add(12).is_some_and(|end| end <= self.end)
    }
    pub(crate) fn matches_root(&self, root: &Arc<ForegroundRoot>) -> bool {
        Arc::ptr_eq(&self.root, root)
    }
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
            && self.generation == other.generation
            && self.start == other.start
            && self.end == other.end
    }
}

/// One selected nonempty destination within the exact native mmap generation.
/// This is only mapping provenance. It neither excludes concurrent native work
/// nor grants a store, certifies its result, or authorizes stream consumption.
/// In particular, an unused iovec tail is not part of this selected footprint.
#[derive(Debug)]
pub(crate) struct OriginalCopySpan {
    arena: OriginalArena,
    address: u64,
    length: u64,
}
impl OriginalCopySpan {
    pub(crate) fn address(&self) -> u64 {
        self.address
    }
    pub(crate) fn length(&self) -> u64 {
        self.length
    }
    pub(crate) fn matches_root(&self, root: &Arc<ForegroundRoot>) -> bool {
        self.arena.matches_root(root)
    }
}

#[derive(Debug)]
struct Pending {
    root: Arc<ForegroundRoot>,
    arguments: [usize; 6],
    generation: u64,
    entered: bool,
}
#[derive(Debug, Default)]
pub(super) struct State {
    generation: u64,
    exhausted: bool,
    pending: Option<Pending>,
    arena: Option<OriginalArena>,
}

/// The kernel reads mmap's descriptor as a C int, so a guest that loads -1
/// through a 32-bit register passes it zero-extended. Only that int -1 counts.
fn anonymous_fd(raw: usize) -> bool {
    raw == -1isize as usize || raw == u32::MAX as usize
}

fn arguments(args: SyscallArgs) -> [usize; 6] {
    [
        args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
    ]
}

/// Future admission of any of these operations must lose sole-root authority
/// before native effects, even if that operation ultimately returns an error.
pub(crate) fn changes_foreground_lineage(nr: Sysno) -> bool {
    matches!(
        nr,
        Sysno::clone
            | Sysno::clone3
            | Sysno::fork
            | Sysno::vfork
            | Sysno::execve
            | Sysno::execveat
            | Sysno::unshare
            | Sysno::setns
            | Sysno::userfaultfd
            | Sysno::ioctl
            | Sysno::listen
            | Sysno::accept
            | Sysno::accept4
            | Sysno::io_uring_setup
            | Sysno::bpf
            | Sysno::ptrace
            | Sysno::process_vm_writev
            | Sysno::pidfd_getfd
            | Sysno::recvmsg
            | Sysno::recvmmsg
            | Sysno::sendmsg
            | Sysno::sendmmsg
            | Sysno::shmat
            | Sysno::shmdt
    )
}
pub(crate) fn invalidates_original_arena(nr: Sysno) -> bool {
    changes_foreground_lineage(nr)
        || matches!(
            nr,
            Sysno::mmap
                | Sysno::munmap
                | Sysno::mprotect
                | Sysno::pkey_mprotect
                | Sysno::madvise
                | Sysno::brk
                | Sysno::mremap
                | Sysno::remap_file_pages
        )
}

impl MemoryMetadata {
    /// The synchronous backend mmap observer is the sole arena issuer. The
    /// range supplied here describes only selected bytes, never a whole read's
    /// otherwise-unused destination or an inferred permission from VM metadata.
    pub(crate) fn original_copy_span(
        &self,
        owner: NetworkStreamOwner,
        address: u64,
        length: u64,
    ) -> Result<OriginalCopySpan, &'static str> {
        let arena = self
            .original_arena
            .arena
            .as_ref()
            .filter(|arena| {
                !self.original_arena.exhausted
                    && self.original_arena.pending.is_none()
                    && arena.root.is_current(owner)
                    && length != 0
                    && address >= arena.start
                    && address
                        .checked_add(length)
                        .is_some_and(|end| end <= arena.end)
            })
            .ok_or("receive destination lacks an authenticated private anonymous span")?;
        Ok(OriginalCopySpan {
            arena: arena.clone(),
            address,
            length,
        })
    }
    pub(crate) fn validate_original_copy_span(
        &self,
        owner: NetworkStreamOwner,
        span: &OriginalCopySpan,
    ) -> Result<(), &'static str> {
        let current = self.original_copy_span(owner, span.address, span.length)?;
        if current.arena.same(&span.arena) {
            Ok(())
        } else {
            Err("receive destination arena generation changed")
        }
    }

    pub(crate) fn invalidate_original_arena(&mut self) {
        self.original_arena.pending = None;
        self.original_arena.arena = None;
        match self.original_arena.generation.checked_add(1) {
            Some(next) => self.original_arena.generation = next,
            None => self.original_arena.exhausted = true,
        }
    }

    /// Called only from the synchronous, exact backend observation, with the
    /// existing current root/MM capability. No ordinary RPC issues this proof.
    pub(crate) fn observe_original_arena(
        &mut self,
        root: &Arc<ForegroundRoot>,
        nr: Sysno,
        args: SyscallArgs,
        event: Event,
    ) -> Result<(), &'static str> {
        let raw = arguments(args);
        if event == Event::Prepared && invalidates_original_arena(nr) {
            self.invalidate_original_arena();
            let qualifies = nr == Sysno::mmap
                && raw[0] == 0
                && raw[1] != 0
                && raw[2] == (libc::PROT_READ | libc::PROT_WRITE) as usize
                && raw[3] == (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize
                && anonymous_fd(raw[4])
                && raw[5] == 0;
            if qualifies && !self.original_arena.exhausted && root.is_current(root.owner()) {
                self.original_arena.pending = Some(Pending {
                    root: root.clone(),
                    arguments: raw,
                    generation: self.original_arena.generation,
                    entered: false,
                });
            }
            return Ok(());
        }
        if nr != Sysno::mmap {
            return Ok(());
        }
        let Some(pending) = self.original_arena.pending.as_mut() else {
            return Ok(());
        };
        if !Arc::ptr_eq(&pending.root, root)
            || pending.arguments != raw
            || !root.is_current(root.owner())
        {
            self.invalidate_original_arena();
            return Err("native mmap changed exact root/MM/arguments");
        }
        match event {
            Event::Entered if !pending.entered => pending.entered = true,
            Event::InterruptedBeforeEntry if !pending.entered => {
                self.invalidate_original_arena();
            }
            Event::Returned(returned) => {
                let pending = self.original_arena.pending.take().unwrap();
                // Generic injection supplies Prepared and an actual native
                // Returned boundary. Unlike specialized Read, it emits no
                // separate Entered event; an RPC/delegate result cannot call
                // this issuer. The native return itself proves entry.
                if returned < 0 {
                    return Ok(());
                }
                let start = returned as u64;
                let size = (pending.arguments[1] as u64)
                    .checked_add(PAGE_SIZE as u64 - 1)
                    .map(|size| size & !(PAGE_SIZE as u64 - 1));
                let end = size.and_then(|size| start.checked_add(size));
                if start == 0 || !start.is_multiple_of(PAGE_SIZE as u64) || end.is_none() {
                    self.invalidate_original_arena();
                    return Err("native mmap arena span is not canonical");
                }
                self.original_arena.arena = Some(OriginalArena {
                    root: pending.root,
                    generation: pending.generation,
                    start,
                    end: end.unwrap(),
                });
            }
            _ => {
                self.invalidate_original_arena();
                return Err("native mmap changed preparation/entry/return progression");
            }
        }
        Ok(())
    }

    pub(crate) fn original_event_arena(
        &self,
        owner: NetworkStreamOwner,
        address: u64,
    ) -> Result<OriginalArena, &'static str> {
        let arena = self
            .original_arena
            .arena
            .as_ref()
            .filter(|arena| {
                !self.original_arena.exhausted
                    && self.original_arena.pending.is_none()
                    && arena.contains(owner, address)
            })
            .ok_or("epoll event lacks an authenticated private anonymous arena")?;
        Ok(arena.clone())
    }
    pub(crate) fn validate_original_event_arena(
        &self,
        owner: NetworkStreamOwner,
        address: u64,
        arena: &OriginalArena,
    ) -> Result<(), &'static str> {
        let current = self.original_event_arena(owner, address)?;
        if current.same(arena) {
            Ok(())
        } else {
            Err("epoll event arena generation changed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mmap() -> SyscallArgs {
        SyscallArgs {
            arg0: 0,
            arg1: 4096,
            arg2: (libc::PROT_READ | libc::PROT_WRITE) as usize,
            arg3: (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
            arg4: -1isize as usize,
            arg5: 0,
        }
    }
    fn issue(memory: &mut MemoryMetadata, root: &Arc<ForegroundRoot>) {
        memory
            .observe_original_arena(root, Sysno::mmap, mmap(), Event::Prepared)
            .unwrap();
        assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
        // Actual generic backend path has no separate Entered callback.
        memory
            .observe_original_arena(root, Sysno::mmap, mmap(), Event::Returned(0x8000))
            .unwrap();
    }
    #[test]
    fn event_arena_requires_real_prepare_return_and_never_survives_clone_or_serde() {
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        memory
            .observe_original_arena(&root, Sysno::mmap, mmap(), Event::Returned(0x8000))
            .unwrap();
        assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
        issue(&mut memory, &root);
        let arena = memory.original_event_arena(root.owner(), 0x8000).unwrap();
        memory
            .validate_original_event_arena(root.owner(), 0x8ff4, &arena)
            .unwrap();
        for address in [0, 0x7fff, 0x8ff5, u64::MAX - 2] {
            assert!(memory.original_event_arena(root.owner(), address).is_err());
        }
        assert!(
            memory
                .clone()
                .original_event_arena(root.owner(), 0x8000)
                .is_err()
        );
        let encoded = serde_json::to_vec(&*memory).unwrap();
        let decoded: MemoryMetadata = serde_json::from_slice(&encoded).unwrap();
        assert!(decoded.original_event_arena(root.owner(), 0x8000).is_err());
        issue(&mut memory, &root);
        assert!(
            memory
                .validate_original_event_arena(root.owner(), 0x8000, &arena)
                .is_err()
        );
    }
    #[test]
    fn event_arena_every_native_invalidating_family_loses_permission_before_return() {
        let invalidators = [
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
            Sysno::execve,
            Sysno::execveat,
            Sysno::unshare,
            Sysno::setns,
            Sysno::userfaultfd,
            Sysno::ioctl,
            Sysno::listen,
            Sysno::accept,
            Sysno::accept4,
            Sysno::io_uring_setup,
            Sysno::bpf,
            Sysno::ptrace,
            Sysno::process_vm_writev,
            Sysno::pidfd_getfd,
            Sysno::recvmsg,
            Sysno::recvmmsg,
            Sysno::sendmsg,
            Sysno::sendmmsg,
            Sysno::shmat,
            Sysno::shmdt,
            Sysno::mmap,
            Sysno::munmap,
            Sysno::mprotect,
            Sysno::pkey_mprotect,
            Sysno::madvise,
            Sysno::brk,
            Sysno::mremap,
            Sysno::remap_file_pages,
        ];
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        for nr in invalidators {
            issue(&mut memory, &root);
            memory
                .observe_original_arena(&root, nr, mmap(), Event::Prepared)
                .unwrap();
            assert!(
                memory.original_event_arena(root.owner(), 0x8000).is_err(),
                "{nr:?}"
            );
            if nr == Sysno::mmap {
                memory
                    .observe_original_arena(
                        &root,
                        nr,
                        mmap(),
                        Event::Returned(-i64::from(libc::ENOMEM)),
                    )
                    .unwrap();
                assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
            }
        }
    }
    #[test]
    fn event_arena_rejects_nonanonymous_shared_hint_fd_and_changed_actual_operands() {
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        for change in 0..6 {
            let mut args = mmap();
            match change {
                0 => args.arg0 = 0x8000,
                1 => args.arg1 = 0,
                2 => args.arg2 = libc::PROT_READ as usize,
                3 => args.arg3 = (libc::MAP_SHARED | libc::MAP_ANONYMOUS) as usize,
                4 => args.arg4 = 3,
                5 => args.arg5 = 4096,
                _ => unreachable!(),
            }
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Returned(0x8000))
                .unwrap();
            assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
        }
        memory
            .observe_original_arena(&root, Sysno::mmap, mmap(), Event::Prepared)
            .unwrap();
        let mut changed = mmap();
        changed.arg1 = 8192;
        assert!(
            memory
                .observe_original_arena(&root, Sysno::mmap, changed, Event::Returned(0x8000))
                .is_err()
        );
        assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
        memory.original_arena.generation = u64::MAX;
        issue(&mut memory, &root);
        assert!(memory.original_event_arena(root.owner(), 0x8000).is_err());
    }

    #[test]
    fn event_arena_accepts_zero_extended_int_minus_one_fd_only() {
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        // A static guest's `mov $-1,%r8d` arrives as 0xffffffff (attempt-22).
        let mut args = mmap();
        args.arg4 = u32::MAX as usize;
        memory
            .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
            .unwrap();
        memory
            .observe_original_arena(&root, Sysno::mmap, args, Event::Returned(0x8000))
            .unwrap();
        memory.original_event_arena(root.owner(), 0x8000).unwrap();
        memory.original_copy_span(root.owner(), 0x8040, 4).unwrap();
        for fd in [
            0x1_ffff_ffffusize,
            u32::MAX as usize - 1,
            0,
            i32::MAX as usize,
        ] {
            let mut args = mmap();
            args.arg4 = fd;
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Prepared)
                .unwrap();
            memory
                .observe_original_arena(&root, Sysno::mmap, args, Event::Returned(0x8000))
                .unwrap();
            assert!(
                memory.original_event_arena(root.owner(), 0x8000).is_err(),
                "{fd:#x}"
            );
            assert!(
                memory.original_copy_span(root.owner(), 0x8040, 4).is_err(),
                "{fd:#x}"
            );
        }
    }

    #[test]
    fn receive_span_proves_only_selected_bytes_and_preserves_event_width() {
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
        issue(&mut memory, &root);
        for (address, length) in [
            (0x8000, 1),
            (0x8000, 4096),
            (0x8ff4, 12),
            (0x8fff, 1),
            (0x8103, 512),
        ] {
            let span = memory
                .original_copy_span(root.owner(), address, length)
                .unwrap();
            assert_eq!((span.address(), span.length()), (address, length));
            assert!(span.matches_root(&root));
            memory
                .validate_original_copy_span(root.owner(), &span)
                .unwrap();
        }
        // The selected byte may end at the arena edge even though an unused
        // tail would be invalid. The old epoll proof still requires all12 bytes.
        assert!(memory.original_copy_span(root.owner(), 0x8fff, 2).is_err());
        assert!(memory.original_event_arena(root.owner(), 0x8fff).is_err());
        for (address, length) in [
            (0, 1),
            (0x7fff, 1),
            (0x9000, 1),
            (0x8000, 0),
            (0x8000, 4097),
            (0x8000, u64::MAX),
            (u64::MAX, 1),
            (u64::MAX - 2, 4),
        ] {
            assert!(
                memory
                    .original_copy_span(root.owner(), address, length)
                    .is_err(),
                "{address:#x}+{length:#x}"
            );
        }
        memory
            .original_copy_span(root.owner(), 0x8000, 4096)
            .unwrap();
    }

    #[test]
    fn receive_span_cannot_cross_root_mm_clone_serde_or_new_mmap_generation() {
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let (other_root, _other_metadata, other_memory, _) =
            crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        let mut other_memory = other_memory.lock().unwrap();
        issue(&mut memory, &root);
        issue(&mut other_memory, &other_root);
        let span = memory
            .original_copy_span(root.owner(), 0x8000, 512)
            .unwrap();
        assert_eq!(root.owner(), other_root.owner());
        assert!(!span.matches_root(&other_root));
        assert!(
            other_memory
                .validate_original_copy_span(other_root.owner(), &span)
                .is_err()
        );
        let wrong_mm = NetworkStreamOwner {
            mm: root.owner().mm.for_exec(root.owner().thread),
            ..root.owner()
        };
        assert!(memory.original_copy_span(wrong_mm, 0x8000, 512).is_err());
        assert!(memory.validate_original_copy_span(wrong_mm, &span).is_err());
        assert!(
            memory
                .clone()
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        let encoded = serde_json::to_vec(&*memory).unwrap();
        let decoded: MemoryMetadata = serde_json::from_slice(&encoded).unwrap();
        assert!(
            decoded
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        memory
            .observe_original_arena(&root, Sysno::mmap, mmap(), Event::Prepared)
            .unwrap();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        memory
            .observe_original_arena(&root, Sysno::mmap, mmap(), Event::Returned(0x8000))
            .unwrap();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        memory
            .original_copy_span(root.owner(), 0x8000, 512)
            .unwrap();
    }

    #[test]
    fn receive_span_loses_permission_before_each_native_invalidation() {
        let invalidators = [
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
            Sysno::execve,
            Sysno::execveat,
            Sysno::unshare,
            Sysno::setns,
            Sysno::userfaultfd,
            Sysno::ioctl,
            Sysno::listen,
            Sysno::accept,
            Sysno::accept4,
            Sysno::io_uring_setup,
            Sysno::bpf,
            Sysno::ptrace,
            Sysno::process_vm_writev,
            Sysno::pidfd_getfd,
            Sysno::recvmsg,
            Sysno::recvmmsg,
            Sysno::sendmsg,
            Sysno::sendmmsg,
            Sysno::shmat,
            Sysno::shmdt,
            Sysno::mmap,
            Sysno::munmap,
            Sysno::mprotect,
            Sysno::pkey_mprotect,
            Sysno::madvise,
            Sysno::brk,
            Sysno::mremap,
            Sysno::remap_file_pages,
        ];
        let (root, _metadata, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        for nr in invalidators {
            issue(&mut memory, &root);
            let span = memory.original_copy_span(root.owner(), 0x8000, 1).unwrap();
            memory
                .observe_original_arena(&root, nr, mmap(), Event::Prepared)
                .unwrap();
            assert!(
                memory
                    .validate_original_copy_span(root.owner(), &span)
                    .is_err(),
                "{nr:?}"
            );
            assert!(
                memory.original_copy_span(root.owner(), 0x8000, 1).is_err(),
                "{nr:?}"
            );
        }
        issue(&mut memory, &root);
        let span = memory.original_copy_span(root.owner(), 0x8000, 1).unwrap();
        memory.original_arena.generation = u64::MAX;
        memory.invalidate_original_arena();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        issue(&mut memory, &root);
        assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    }

    #[test]
    fn receive_span_requires_current_native_root_even_without_arena_invalidation() {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (runtime, root, _metadata, memory, _) =
            crate::network_runtime::controlled_foreground_runtime(tid);
        let mut memory = memory.lock().unwrap();
        issue(&mut memory, &root);
        let span = memory.original_copy_span(root.owner(), 0x8000, 1).unwrap();
        runtime.revoke_foreground_lineage();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &span)
                .is_err()
        );
        assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    }
}
