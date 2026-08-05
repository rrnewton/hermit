/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Ptrace safety net for syscall instructions missed by SaBRe rewriting.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use nix::sys::ptrace;
use nix::sys::signal::Signal;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;
use serde::Serialize;

const SYSCALL_INSN: [u8; 2] = [0x0f, 0x05];
// SaBRe's SIGILL handler recognizes this reserved two-byte instruction as a
// syscall site that could not be expanded to an out-of-line jump.
const SABRE_SYSCALL_MARKER: [u8; 2] = [0x0f, 0xff];

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub path_evidence: PathEvidence,
}

#[derive(Debug, Serialize)]
pub struct PathEvidence {
    pub schema: u8,
    pub guest_rpc_observed: bool,
    pub ptrace_fallback_sites: usize,
    pub trusted_shared_object_sites: usize,
    pub trusted_shared_objects: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MappingClassification {
    trusted: bool,
    trusted_shared_object: Option<PathBuf>,
}

#[derive(Default)]
struct TraceeState {
    pending_patch: Option<PendingPatch>,
}

struct PendingPatch {
    site: usize,
    syscall: u64,
}

struct SignalDiagnostic {
    signal: Signal,
    si_code: Option<i32>,
    si_errno: Option<i32>,
    fault_address: Option<usize>,
    mapping: Option<MappingDiagnostic>,
    instruction_bytes: Option<[u8; 16]>,
    registers: libc::user_regs_struct,
}

struct MappingDiagnostic {
    line: String,
    relative_offset: usize,
    file_offset: usize,
}

fn should_replace_signal_diagnostic(
    existing: Option<(Signal, Option<i32>, bool)>,
    signal: Signal,
    si_code: Option<i32>,
) -> bool {
    // SaBRe handles its reserved SIGILL markers in-process. For an unknown hardware SIGILL it
    // restores SIG_DFL and raises SIGILL again, producing a second ptrace stop from SI_TKILL.
    // Preserve the original kernel fault context across that re-raise, but let a later hardware
    // fault replace it so a successfully handled marker cannot leave stale diagnostics behind.
    if let Some((Signal::SIGILL, Some(existing_code), existing_is_marker)) = existing
        && signal == Signal::SIGILL
        && existing_code > 0
        && !existing_is_marker
        && !si_code.is_some_and(|code| code > 0)
    {
        return false;
    }
    true
}

fn is_sabre_sigill_marker(instruction_bytes: Option<&[u8; 16]>) -> bool {
    instruction_bytes
        .is_some_and(|bytes| matches!(&bytes[..2], [0x0f, 0xff] | [0x0f, 0x0b] | [0x0f, 0x0c]))
}

fn final_physical_exit(status: &WaitStatus) -> Option<(Pid, ExitStatus)> {
    match *status {
        WaitStatus::Exited(pid, code) => Some((pid, ExitStatus::from_raw(code << 8))),
        WaitStatus::Signaled(pid, signal, core_dumped) => {
            let raw = signal as i32 | if core_dumped { 0x80 } else { 0 };
            Some((pid, ExitStatus::from_raw(raw)))
        }
        _ => None,
    }
}

struct Supervisor {
    root: Pid,
    tracees: HashSet<Pid>,
    states: HashMap<Pid, TraceeState>,
    mapping_cache: HashMap<(Pid, usize), MappingClassification>,
    /// Identity (dev+inode) of the launched SaBRe loader and plugin. Resolved
    /// once at construction so the exemption binds to the objects this
    /// supervisor actually started, not to whatever a mapping calls itself.
    sabre_id: Option<FileId>,
    plugin_id: Option<FileId>,
    readiness: Arc<AtomicBool>,
    ready_observed: bool,
    patched_sites: HashSet<(Pid, usize)>,
    trusted_shared_object_sites: HashSet<(Pid, usize)>,
    trusted_shared_objects: HashSet<PathBuf>,
    signal_diagnostics: HashMap<Pid, SignalDiagnostic>,
    physical_exit_observer: Arc<detcore::GlobalState>,
}

impl Supervisor {
    fn new(
        root: Pid,
        sabre: PathBuf,
        plugin: PathBuf,
        readiness: Arc<AtomicBool>,
        physical_exit_observer: Arc<detcore::GlobalState>,
    ) -> Self {
        Self {
            root,
            tracees: HashSet::from([root]),
            states: HashMap::from([(root, TraceeState::default())]),
            mapping_cache: HashMap::new(),
            sabre_id: launched_file_id(&sabre),
            plugin_id: launched_file_id(&plugin),
            readiness,
            ready_observed: false,
            patched_sites: HashSet::new(),
            trusted_shared_object_sites: HashSet::new(),
            trusted_shared_objects: HashSet::new(),
            signal_diagnostics: HashMap::new(),
            physical_exit_observer,
        }
    }

    fn run(mut self) -> Result<(ExitStatus, PathEvidence), Error> {
        ptrace::attach(self.root).context("failed to attach SaBRe supervisor worker")?;
        match waitpid(self.root, Some(WaitPidFlag::__WALL))? {
            WaitStatus::Stopped(pid, Signal::SIGSTOP) if pid == self.root => {}
            status => {
                return Err(anyhow!(
                    "unexpected SaBRe supervisor attach stop: {status:?}"
                ));
            }
        }
        tracing::trace!(
            target: "hermit::sabre::fallback",
            tid = self.root.as_raw(),
            "received supervisor attach stop",
        );
        self.set_options(self.root)
            .context("failed to set options on the initial SaBRe tracee")?;
        ptrace::syscall(self.root, None)
            .context("failed to resume the initial SaBRe tracee with PTRACE_SYSCALL")?;

        let mut root_status = None;
        while !self.tracees.is_empty() {
            let status = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::__WALL)) {
                Ok(status) => status,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            };
            tracing::trace!(
                target: "hermit::sabre::fallback",
                ?status,
                "received ptrace wait status",
            );
            if let Some((pid, exit_status)) = final_physical_exit(&status) {
                if let WaitStatus::Signaled(_, signal, _) = status
                    && let Some(diagnostic) = self
                        .signal_diagnostics
                        .get(&pid)
                        .filter(|diagnostic| diagnostic.signal == signal)
                {
                    let instruction_bytes = diagnostic.instruction_bytes.as_ref().map(|bytes| {
                        bytes
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    });
                    tracing::error!(
                        target: "hermit::sabre::fallback",
                        tid = pid.as_raw(),
                        ?signal,
                        si_code = diagnostic.si_code,
                        si_errno = diagnostic.si_errno,
                        rip = format!("{:#x}", diagnostic.registers.rip),
                        rsp = format!("{:#x}", diagnostic.registers.rsp),
                        fault_address = diagnostic
                            .fault_address
                            .map(|address| format!("{address:#x}")),
                        mapping = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| mapping.line.as_str()),
                        file_offset = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| format!("{:#x}", mapping.file_offset)),
                        relative_offset = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| format!("{:#x}", mapping.relative_offset)),
                        instruction_bytes,
                        rax = format!("{:#x}", diagnostic.registers.rax),
                        rbx = format!("{:#x}", diagnostic.registers.rbx),
                        rcx = format!("{:#x}", diagnostic.registers.rcx),
                        rdx = format!("{:#x}", diagnostic.registers.rdx),
                        rsi = format!("{:#x}", diagnostic.registers.rsi),
                        rdi = format!("{:#x}", diagnostic.registers.rdi),
                        rbp = format!("{:#x}", diagnostic.registers.rbp),
                        r8 = format!("{:#x}", diagnostic.registers.r8),
                        r9 = format!("{:#x}", diagnostic.registers.r9),
                        r10 = format!("{:#x}", diagnostic.registers.r10),
                        r11 = format!("{:#x}", diagnostic.registers.r11),
                        r12 = format!("{:#x}", diagnostic.registers.r12),
                        r13 = format!("{:#x}", diagnostic.registers.r13),
                        r14 = format!("{:#x}", diagnostic.registers.r14),
                        r15 = format!("{:#x}", diagnostic.registers.r15),
                        orig_rax = format!("{:#x}", diagnostic.registers.orig_rax),
                        eflags = format!("{:#x}", diagnostic.registers.eflags),
                        cs = format!("{:#x}", diagnostic.registers.cs),
                        ss = format!("{:#x}", diagnostic.registers.ss),
                        fs_base = format!("{:#x}", diagnostic.registers.fs_base),
                        gs_base = format!("{:#x}", diagnostic.registers.gs_base),
                        "SaBRe tracee terminated by a fatal signal",
                    );
                }
                self.remove_tracee(pid);
                self.physical_exit_observer
                    .complete_physical_process_exit(pid.as_raw());
                if pid == self.root {
                    root_status = Some(exit_status);
                }
                continue;
            }
            match status {
                WaitStatus::PtraceSyscall(pid) => self.handle_syscall_stop(pid)?,
                WaitStatus::PtraceEvent(pid, _, event) => self.handle_ptrace_event(pid, event)?,
                WaitStatus::Stopped(pid, signal) => {
                    if signal == Signal::SIGSTOP && pid != self.root {
                        self.states.entry(pid).or_default();
                        self.tracees.insert(pid);
                        self.set_options(pid)?;
                        self.resume(pid, None)?;
                    } else {
                        if !matches!(signal, Signal::SIGSTOP | Signal::SIGCHLD) {
                            let registers = ptrace::getregs(pid).ok();
                            let rip = registers.as_ref().map_or(0, |registers| registers.rip);
                            let captures_fault_context = matches!(
                                signal,
                                Signal::SIGSEGV
                                    | Signal::SIGILL
                                    | Signal::SIGBUS
                                    | Signal::SIGFPE
                                    | Signal::SIGABRT
                            );
                            if captures_fault_context {
                                let siginfo = ptrace::getsiginfo(pid).ok();
                                let si_code = siginfo.as_ref().map(|info| info.si_code);
                                let existing =
                                    self.signal_diagnostics.get(&pid).map(|diagnostic| {
                                        (
                                            diagnostic.signal,
                                            diagnostic.si_code,
                                            is_sabre_sigill_marker(
                                                diagnostic.instruction_bytes.as_ref(),
                                            ),
                                        )
                                    });
                                if should_replace_signal_diagnostic(existing, signal, si_code) {
                                    if let Some(registers) = registers {
                                        let fault_address = siginfo
                                            .as_ref()
                                            .filter(|info| info.si_code > 0)
                                            .map(|info| unsafe { info.si_addr() as usize });
                                        let mapping = fs::read_to_string(format!(
                                            "/proc/{}/maps",
                                            pid.as_raw()
                                        ))
                                        .ok()
                                        .and_then(|maps| mapping_diagnostic(&maps, rip as usize));
                                        let instruction_bytes =
                                            read_diagnostic_bytes(pid, rip as usize).ok();
                                        self.signal_diagnostics.insert(
                                            pid,
                                            SignalDiagnostic {
                                                signal,
                                                si_code,
                                                si_errno: siginfo
                                                    .as_ref()
                                                    .map(|info| info.si_errno),
                                                fault_address,
                                                mapping,
                                                instruction_bytes,
                                                registers,
                                            },
                                        );
                                    } else {
                                        self.signal_diagnostics.remove(&pid);
                                    }
                                }
                            }
                            tracing::debug!(
                                target: "hermit::sabre::fallback",
                                tid = pid.as_raw(),
                                ?signal,
                                rip = format!("{rip:#x}"),
                                "forwarding signal to tracee",
                            );
                        }
                        self.resume(pid, Some(signal))?;
                    }
                }
                WaitStatus::Exited(..) | WaitStatus::Signaled(..) => unreachable!(),
                WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            }
        }

        let status = root_status.ok_or_else(|| anyhow!("SaBRe root tracee disappeared"))?;
        let mut trusted_shared_objects = self
            .trusted_shared_objects
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        trusted_shared_objects.sort();
        Ok((
            status,
            PathEvidence {
                schema: 1,
                guest_rpc_observed: self.readiness.load(Ordering::Acquire),
                ptrace_fallback_sites: self.patched_sites.len(),
                trusted_shared_object_sites: self.trusted_shared_object_sites.len(),
                trusted_shared_objects,
            },
        ))
    }

    fn set_options(&self, pid: Pid) -> Result<(), Error> {
        ptrace::setoptions(
            pid,
            ptrace::Options::PTRACE_O_EXITKILL
                | ptrace::Options::PTRACE_O_TRACESYSGOOD
                | ptrace::Options::PTRACE_O_TRACECLONE
                | ptrace::Options::PTRACE_O_TRACEFORK
                | ptrace::Options::PTRACE_O_TRACEVFORK
                | ptrace::Options::PTRACE_O_TRACEEXEC
                | ptrace::Options::PTRACE_O_TRACEEXIT,
        )?;
        Ok(())
    }

    fn handle_syscall_stop(&mut self, pid: Pid) -> Result<(), Error> {
        let syscall_info = get_syscall_info(pid)?;
        tracing::trace!(
            target: "hermit::sabre::fallback",
            tid = pid.as_raw(),
            op = syscall_info.op,
            "decoded ptrace syscall stop",
        );
        match syscall_info.op {
            libc::PTRACE_SYSCALL_INFO_ENTRY => {
                let mut regs = ptrace::getregs(pid)?;
                let site = regs
                    .rip
                    .checked_sub(SYSCALL_INSN.len() as u64)
                    .ok_or_else(|| anyhow!("invalid syscall RIP {:#x} in tracee {pid}", regs.rip))?
                    as usize;
                let bytes = read_two_bytes(pid, site)?;
                let fallback_ready = self.fallback_ready()?;
                if pid != self.root {
                    tracing::trace!(
                        target: "hermit::sabre::fallback",
                        tid = pid.as_raw(),
                        nr = regs.orig_rax,
                        site = format!("{site:#x}"),
                        raw = bytes == SYSCALL_INSN,
                        "child syscall entry",
                    );
                }
                if bytes == SYSCALL_INSN && fallback_ready {
                    let mapping = self.classify_mapping(pid, site)?;
                    if let Some(path) = mapping.trusted_shared_object {
                        self.trusted_shared_object_sites.insert((pid, site));
                        self.trusted_shared_objects.insert(path);
                    }
                    if !mapping.trusted {
                        write_two_bytes(pid, site, SABRE_SYSCALL_MARKER)?;
                        let syscall = regs.orig_rax;
                        regs.orig_rax = u64::MAX;
                        ptrace::setregs(pid, regs)?;
                        self.states.entry(pid).or_default().pending_patch =
                            Some(PendingPatch { site, syscall });
                        self.patched_sites.insert((pid, site));
                        tracing::debug!(
                            target: "hermit::sabre::fallback",
                            tid = pid.as_raw(),
                            address = site,
                            "redirecting raw syscall instruction through the SaBRe handler",
                        );
                    }
                }
            }
            libc::PTRACE_SYSCALL_INFO_EXIT => {
                // A cached (pid, page) verdict describes the mapping that
                // occupied that page when it was classified. mmap/munmap/
                // mremap/mprotect/brk can replace or re-permission that page
                // in-process, so a page previously classified as trusted can
                // come to host a completely different raw-syscall site. Drop
                // this tracee's cache whenever it mutates its address space;
                // the next site there is reclassified against live
                // /proc/<pid>/maps. Per-pid is coarse but correct -- per-page
                // would be an optimisation, not a correctness requirement.
                let exit_regs = ptrace::getregs(pid)?;
                if mutates_address_space(exit_regs.orig_rax) {
                    self.mapping_cache
                        .retain(|(cached_pid, _), _| *cached_pid != pid);
                }
                if let Some(pending) = self.states.entry(pid).or_default().pending_patch.take() {
                    let mut regs = exit_regs;
                    regs.rax = pending.syscall;
                    regs.orig_rax = pending.syscall;
                    regs.rip = pending.site as u64;
                    ptrace::setregs(pid, regs)?;
                }
            }
            _ => {}
        }
        ptrace::syscall(pid, None)?;
        Ok(())
    }

    fn handle_ptrace_event(&mut self, pid: Pid, event: libc::c_int) -> Result<(), Error> {
        if matches!(
            event,
            libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK
        ) {
            let child = Pid::from_raw(ptrace::getevent(pid)? as i32);
            self.tracees.insert(child);
            self.states.entry(child).or_default();
        } else if event == libc::PTRACE_EVENT_EXEC {
            self.mapping_cache
                .retain(|(cached_pid, _), _| *cached_pid != pid);
            self.states.insert(pid, TraceeState::default());
            self.signal_diagnostics.remove(&pid);
        }
        self.resume(pid, None)
    }

    fn resume(&self, pid: Pid, signal: Option<Signal>) -> Result<(), Error> {
        ptrace::syscall(pid, signal)?;
        Ok(())
    }
    fn fallback_ready(&mut self) -> Result<bool, Error> {
        let ready = self.readiness.load(Ordering::Acquire);
        if ready && !self.ready_observed {
            self.ready_observed = true;
            tracing::debug!(
                target: "hermit::sabre::fallback",
                "SaBRe fallback readiness observed",
            );
        }
        Ok(ready)
    }

    fn classify_mapping(
        &mut self,
        pid: Pid,
        address: usize,
    ) -> Result<MappingClassification, Error> {
        let page = address & !4095usize;
        if let Some(classification) = self.mapping_cache.get(&(pid, page)) {
            return Ok(classification.clone());
        }
        let maps = fs::read_to_string(format!("/proc/{}/maps", pid.as_raw()))?;
        let classification = mapping_entry(&maps, address).map_or(
            MappingClassification {
                trusted: false,
                trusted_shared_object: None,
            },
            |entry| classify_mapping(&entry, self.sabre_id.as_ref(), self.plugin_id.as_ref()),
        );
        self.mapping_cache
            .insert((pid, page), classification.clone());
        Ok(classification)
    }

    fn remove_tracee(&mut self, pid: Pid) {
        self.tracees.remove(&pid);
        self.states.remove(&pid);
        self.mapping_cache
            .retain(|(cached_pid, _), _| *cached_pid != pid);
        self.signal_diagnostics.remove(&pid);
    }
}

// nix 0.30.1 passes a null `addr` to PTRACE_GET_SYSCALL_INFO, but Linux
// defines that argument as the size of the output buffer. Use the kernel ABI
// directly until the upstream wrapper supplies the required size.
fn get_syscall_info(pid: Pid) -> Result<libc::ptrace_syscall_info, Error> {
    let mut info = std::mem::MaybeUninit::<libc::ptrace_syscall_info>::zeroed();
    let size = std::mem::size_of::<libc::ptrace_syscall_info>();
    let written = unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid.as_raw(),
            size as *mut libc::c_void,
            info.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if written < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if written == 0 {
        return Err(anyhow!(
            "PTRACE_GET_SYSCALL_INFO returned no data for {pid}"
        ));
    }
    Ok(unsafe { info.assume_init() })
}

fn mapping_line(maps: &str, address: usize) -> Option<&str> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let mut limits = range.split('-');
        let start = usize::from_str_radix(limits.next()?, 16).ok()?;
        let end = usize::from_str_radix(limits.next()?, 16).ok()?;
        (start <= address && address < end).then_some(line)
    })
}

/// Identity of a mapped file: the `dev` and `inode` columns of
/// `/proc/<pid>/maps`, which name the object itself rather than a path that
/// anyone can reproduce with the same basename. `dev` is kept as the literal
/// `major:minor` text the kernel prints so no format assumption is needed on
/// the maps side; the launched-object side is rendered into the same form.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileId {
    dev: String,
    inode: u64,
}

/// One parsed `/proc/<pid>/maps` row: `range perms offset dev inode path`.
struct MappingEntry<'a> {
    dev: &'a str,
    inode: Option<u64>,
    path: &'a str,
}

impl MappingEntry<'_> {
    /// `None` for anonymous or pseudo mappings (inode 0), which have no file
    /// identity to bind to and therefore can never match a launched object.
    fn file_id(&self) -> Option<FileId> {
        match self.inode {
            Some(inode) if inode != 0 => Some(FileId {
                dev: self.dev.to_owned(),
                inode,
            }),
            _ => None,
        }
    }
}

/// Render a `stat(2)` device number in the `major:minor` hex form used by
/// `/proc/<pid>/maps`, so a launched object can be compared to a mapping row.
fn maps_dev_string(dev: u64) -> String {
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    format!("{major:02x}:{minor:02x}")
}

/// Identity of a path this supervisor launched. `None` when the file cannot be
/// stat'ed, which fails CLOSED: an unknown identity matches no mapping, so the
/// exemption is withheld rather than granted.
fn launched_file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).ok()?;
    Some(FileId {
        dev: maps_dev_string(metadata.dev()),
        inode: metadata.ino(),
    })
}

fn mapping_entry(maps: &str, address: usize) -> Option<MappingEntry<'_>> {
    let mut fields = mapping_line(maps, address)?.split_whitespace();
    fields.next()?; // range
    fields.next()?; // perms
    fields.next()?; // offset
    let dev = fields.next()?;
    let inode = fields.next()?;
    Some(MappingEntry {
        dev,
        inode: inode.parse::<u64>().ok(),
        path: fields.next().unwrap_or(""),
    })
}

fn mapping_diagnostic(maps: &str, address: usize) -> Option<MappingDiagnostic> {
    let line = mapping_line(maps, address)?;
    let mut fields = line.split_whitespace();
    let mut limits = fields.next()?.split('-');
    let start = usize::from_str_radix(limits.next()?, 16).ok()?;
    fields.next()?;
    let mapping_offset = usize::from_str_radix(fields.next()?, 16).ok()?;
    let relative_offset = address.checked_sub(start)?;
    Some(MappingDiagnostic {
        line: line.to_owned(),
        relative_offset,
        file_offset: mapping_offset.checked_add(relative_offset)?,
    })
}

/// Syscalls that can replace, move, unmap or re-permission a page in the
/// tracee's address space. A cached page classification is only valid until one
/// of these runs, so observing any of them invalidates that tracee's cache.
const ADDRESS_SPACE_MUTATORS: [u64; 6] = [
    libc::SYS_mmap as u64,
    libc::SYS_munmap as u64,
    libc::SYS_mremap as u64,
    libc::SYS_mprotect as u64,
    libc::SYS_brk as u64,
    libc::SYS_shmat as u64,
];

/// True when `nr` is an address-space mutator. On x86-64 the syscall number
/// remains in `orig_rax` at the syscall-exit stop, which is where the cache is
/// invalidated -- after the mapping change has actually taken effect.
fn mutates_address_space(nr: u64) -> bool {
    ADDRESS_SPACE_MUTATORS.contains(&nr)
}

/// Kernel-supplied mappings whose raw syscall sites are causally identified as
/// infrastructure the guest cannot have rewritten. `[vdso]` and `[vsyscall]`
/// are mapped by the kernel and are not writable, so SaBRe cannot expand a
/// syscall site inside them and the supervisor must not try to patch one.
///
/// They are exempt from REDIRECTION only — never from ACCOUNTING. A raw syscall
/// executed here did not traverse the measured in-guest SaBRe handler, so it is
/// still recorded as a trusted-native site and still makes the cell ineligible.
/// Exempting a mapping from both redirection and counting is the one
/// combination that makes a real raw syscall disappear from the evidence.
const CAUSAL_KERNEL_MAPPINGS: [&str; 2] = ["[vdso]", "[vsyscall]"];

fn classify_mapping(
    entry: &MappingEntry<'_>,
    sabre: Option<&FileId>,
    plugin: Option<&FileId>,
) -> MappingClassification {
    let path = entry.path;
    if path.starts_with('[') {
        // Only the causally identified kernel mappings above are exempt from
        // redirection, and even those are counted. Every other bracket-named
        // mapping -- `[stack]`, `[heap]`, or any future kernel name -- is NOT
        // infrastructure: fall through to `trusted: false` so the site is
        // redirected through the SaBRe handler and counted as a fallback site.
        if CAUSAL_KERNEL_MAPPINGS.contains(&path) {
            return MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from(path)),
            };
        }
        return MappingClassification {
            trusted: false,
            trusted_shared_object: None,
        };
    }
    let path = Path::new(path.strip_suffix(" (deleted)").unwrap_or(path));
    // Bind the infrastructure exemption to the identity of the objects this
    // supervisor actually launched (device + inode from the same
    // `/proc/<pid>/maps` line), not to their basenames. A basename match would
    // exempt any unrelated mapping that happens to be called `sabre` or
    // `libdetcore_sabre.so`.
    let mapped = entry.file_id();
    let infrastructure = matches!((mapped.as_ref(), sabre), (Some(seen), Some(known)) if seen == known)
        || matches!((mapped.as_ref(), plugin), (Some(seen), Some(known)) if seen == known);
    if infrastructure {
        return MappingClassification {
            trusted: true,
            trusted_shared_object: None,
        };
    }
    // SaBRe owns shared-library rewriting. A raw syscall that still reaches the
    // supervisor from another shared object ran outside the measured in-guest
    // interception path. Keep trusting it for runtime safety, but report it so
    // compatibility accounting cannot credit the cell to SaBRe.
    let trusted_shared_object = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(".so"))
        .then(|| path.to_path_buf());
    MappingClassification {
        trusted: trusted_shared_object.is_some(),
        trusted_shared_object,
    }
}

fn read_two_bytes(pid: Pid, address: usize) -> Result<[u8; 2], Error> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let aligned = address & !(word_size - 1);
    let offset = address - aligned;
    let first = ptrace::read(pid, aligned as ptrace::AddressType)?.to_ne_bytes();
    if offset + 1 < word_size {
        Ok([first[offset], first[offset + 1]])
    } else {
        let second = ptrace::read(pid, (aligned + word_size) as ptrace::AddressType)?.to_ne_bytes();
        Ok([first[offset], second[0]])
    }
}

fn read_diagnostic_bytes(pid: Pid, address: usize) -> Result<[u8; 16], Error> {
    let mut bytes = [0; 16];
    let word_size = std::mem::size_of::<libc::c_long>();
    for (index, chunk) in bytes.chunks_mut(word_size).enumerate() {
        let word = ptrace::read(pid, (address + index * word_size) as ptrace::AddressType)?;
        chunk.copy_from_slice(&word.to_ne_bytes()[..chunk.len()]);
    }
    Ok(bytes)
}

fn write_two_bytes(pid: Pid, address: usize, bytes: [u8; 2]) -> Result<(), Error> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let aligned = address & !(word_size - 1);
    let offset = address - aligned;
    let mut first = ptrace::read(pid, aligned as ptrace::AddressType)?.to_ne_bytes();
    first[offset] = bytes[0];
    if offset + 1 < word_size {
        first[offset + 1] = bytes[1];
        ptrace::write(
            pid,
            aligned as ptrace::AddressType,
            libc::c_long::from_ne_bytes(first),
        )?;
    } else {
        ptrace::write(
            pid,
            aligned as ptrace::AddressType,
            libc::c_long::from_ne_bytes(first),
        )?;
        let second_address = aligned + word_size;
        let mut second = ptrace::read(pid, second_address as ptrace::AddressType)?.to_ne_bytes();
        second[0] = bytes[1];
        ptrace::write(
            pid,
            second_address as ptrace::AddressType,
            libc::c_long::from_ne_bytes(second),
        )?;
    }
    Ok(())
}

pub async fn run(
    mut command: std::process::Command,
    sabre: PathBuf,
    plugin: PathBuf,
    readiness: Arc<AtomicBool>,
    physical_exit_observer: Arc<detcore::GlobalState>,
    capture_output: bool,
) -> Result<Output, Error> {
    if capture_output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    // Spawn before creating the blocking supervisor worker. A worker thread consumes a task ID
    // in the guest PID namespace; creating it first shifts the root guest from PID 3 to PID 4 and
    // makes otherwise identical ptrace and SaBRe programs observe different process identities.
    let child = spawn_tracee(command)?;
    let root = Pid::from_raw(child.id() as i32);
    match waitpid(root, Some(WaitPidFlag::__WALL))? {
        WaitStatus::Stopped(pid, Signal::SIGTRAP) if pid == root => {}
        status => return Err(anyhow!("unexpected initial SaBRe ptrace stop: {status:?}")),
    }
    // Ptrace ownership belongs to the individual tracer task, not its thread group. Leave the
    // tracee stopped while handing ownership from this async caller to the blocking supervisor.
    // Injecting SIGSTOP as part of detach prevents any guest instruction from running in between.
    ptrace::detach(root, Some(Signal::SIGSTOP))
        .context("failed to hand SaBRe tracee to supervisor worker")?;
    tokio::task::spawn_blocking(move || {
        run_blocking(child, sabre, plugin, readiness, physical_exit_observer)
    })
    .await
    .context("SaBRe ptrace supervisor task panicked")?
}

fn spawn_tracee(mut command: std::process::Command) -> Result<std::process::Child, Error> {
    // TODO-HUMAN-REVIEW(PR-845): Review SaBRe launch-time ASLR disabling.
    // PTRACE_TRACEME makes exec stop with SIGTRAP. A pre-exec SIGSTOP would
    // deadlock std::process::Command on its exec error pipe. personality(2)
    // is async-signal-safe and survives the SaBRe and guest execs.
    unsafe {
        command.pre_exec(|| {
            let current = libc::personality(0xffff_ffff);
            if current == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let deterministic = current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong;
            if libc::personality(deterministic) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            ptrace::traceme().map_err(std::io::Error::from)
        });
    }

    command
        .spawn()
        .context("failed to spawn ptraced SaBRe guest")
}

fn run_blocking(
    mut child: std::process::Child,
    sabre: PathBuf,
    plugin: PathBuf,
    readiness: Arc<AtomicBool>,
    physical_exit_observer: Arc<detcore::GlobalState>,
) -> Result<Output, Error> {
    let root = Pid::from_raw(child.id() as i32);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    drop(child);

    let stdout_thread = std::thread::spawn(move || read_pipe(stdout));
    let stderr_thread = std::thread::spawn(move || read_pipe(stderr));
    let supervised = Supervisor::new(root, sabre, plugin, readiness, physical_exit_observer).run();
    if supervised.is_err() {
        let _ = nix::sys::signal::kill(root, Signal::SIGKILL);
    }
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("SaBRe stdout reader panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("SaBRe stderr reader panicked"))??;
    let (status, path_evidence) = supervised?;
    Ok(Output {
        status,
        stdout,
        stderr,
        path_evidence,
    })
}

fn read_pipe<R: Read>(pipe: Option<R>) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledges_only_final_physical_exit_statuses() {
        let child = Pid::from_raw(17);
        let parent = Pid::from_raw(11);

        assert_eq!(
            final_physical_exit(&WaitStatus::Exited(child, 0)).map(|(pid, _)| pid),
            Some(child)
        );
        assert_eq!(
            final_physical_exit(&WaitStatus::Signaled(child, Signal::SIGKILL, false))
                .map(|(pid, _)| pid),
            Some(child)
        );
        assert!(
            final_physical_exit(&WaitStatus::PtraceEvent(
                child,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_EXIT,
            ))
            .is_none()
        );
        assert!(final_physical_exit(&WaitStatus::Stopped(parent, Signal::SIGCHLD)).is_none());
    }

    #[test]
    fn preserves_hardware_sigill_across_userspace_reraise() {
        const KERNEL_FAULT: i32 = 2;
        const USER_RERAISE: i32 = -6;
        let hardware = Some((Signal::SIGILL, Some(KERNEL_FAULT), false));

        assert!(!should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
        assert!(!should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            None,
        ));
        assert!(should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            Some(KERNEL_FAULT),
        ));
        assert!(should_replace_signal_diagnostic(
            hardware,
            Signal::SIGSEGV,
            Some(KERNEL_FAULT),
        ));
        assert!(should_replace_signal_diagnostic(
            Some((Signal::SIGILL, Some(KERNEL_FAULT), true)),
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
        assert!(should_replace_signal_diagnostic(
            Some((Signal::SIGILL, Some(USER_RERAISE), false)),
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
    }

    #[test]
    fn recognizes_sabre_sigill_markers() {
        for marker in [[0x0f, 0xff], [0x0f, 0x0b], [0x0f, 0x0c]] {
            let mut bytes = [0; 16];
            bytes[..2].copy_from_slice(&marker);
            assert!(is_sabre_sigill_marker(Some(&bytes)));
        }

        let mut unknown = [0; 16];
        unknown[..2].copy_from_slice(&[0x62, 0xf1]);
        assert!(!is_sabre_sigill_marker(Some(&unknown)));
        assert!(!is_sabre_sigill_marker(None));
    }

    #[test]
    fn finds_mapping_path() {
        let maps = concat!(
            "1000-2000 r-xp 00002000 00:00 0 /tmp/sabre\n",
            "3000-4000 rwxp 00000000 00:00 0 \n",
        );
        assert_eq!(
            mapping_entry(maps, 0x1234).map(|entry| entry.path),
            Some("/tmp/sabre")
        );
        assert_eq!(
            mapping_entry(maps, 0x3456).map(|entry| entry.path),
            Some("")
        );
        assert!(mapping_entry(maps, 0x2500).is_none());
        let diagnostic = mapping_diagnostic(maps, 0x1234).unwrap();
        assert_eq!(diagnostic.relative_offset, 0x234);
        assert_eq!(diagnostic.file_offset, 0x2234);
        assert_eq!(
            diagnostic.line,
            "1000-2000 r-xp 00002000 00:00 0 /tmp/sabre"
        );
    }

    // Rewritten for the identity-based contract. The previous version of this
    // test asserted the two defects codex found: it required a different-root
    // `libdetcore_sabre.so` and `[vdso]` to be `trusted` with NO counted site --
    // the one combination that makes a real raw syscall vanish from evidence.
    #[test]
    fn classifies_runtime_mapping_attribution() {
        let sabre = file_id("fd:01", 1001);
        let plugin = file_id("fd:01", 1002);

        // The launched objects themselves: exempt and uncounted, because they
        // ARE the measured interception path.
        for (inode, path) in [(1001, "/opt/sabre/bin/sabre"), (1002, "/opt/hermit/x.so")] {
            let maps = maps_row("1000-2000", "fd:01", inode, path);
            let entry = mapping_entry(&maps, 0x1234).unwrap();
            assert_eq!(
                classify_mapping(&entry, Some(&sabre), Some(&plugin)),
                MappingClassification {
                    trusted: true,
                    trusted_shared_object: None
                },
                "{path} is a launched object and must be exempt"
            );
        }

        // A DIFFERENT object wearing the plugin's basename is refused, even
        // with the ` (deleted)` suffix the old test used.
        let maps = maps_row(
            "1000-2000",
            "fd:01",
            7777,
            "/different/root/libdetcore_sabre.so (deleted)",
        );
        let entry = mapping_entry(&maps, 0x1234).unwrap();
        assert_eq!(
            classify_mapping(&entry, Some(&sabre), Some(&plugin)),
            MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from("/different/root/libdetcore_sabre.so")),
            },
            "a basename impostor must be counted as a trusted-native site, not exempted"
        );

        // Ordinary shared object: trusted for runtime safety, counted for
        // accounting. Unchanged contract.
        let maps = maps_row("1000-2000", "fd:01", 55, "/usr/lib/libc.so.6");
        let entry = mapping_entry(&maps, 0x1234).unwrap();
        assert_eq!(
            classify_mapping(&entry, Some(&sabre), Some(&plugin)),
            MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from("/usr/lib/libc.so.6")),
            }
        );

        // Non-library and anonymous mappings are redirected and counted as
        // fallback sites. Unchanged contract.
        for (inode, path) in [(66, "/usr/bin/echo"), (0, "")] {
            let maps = maps_row("1000-2000", "fd:01", inode, path);
            let entry = mapping_entry(&maps, 0x1234).unwrap();
            assert_eq!(
                classify_mapping(&entry, Some(&sabre), Some(&plugin)),
                MappingClassification {
                    trusted: false,
                    trusted_shared_object: None
                }
            );
        }
    }

    /// One synthetic `/proc/<pid>/maps` row. Columns are exactly what the
    /// kernel prints: `range perms offset dev inode path`.
    fn maps_row(range: &str, dev: &str, inode: u64, path: &str) -> String {
        format!("{range} r-xp 00000000 {dev} {inode} {path}\n")
    }

    fn classify(maps: &str, address: usize, sabre: Option<&FileId>) -> MappingClassification {
        let entry = mapping_entry(maps, address).expect("mapping row must parse");
        classify_mapping(&entry, sabre, None)
    }

    fn file_id(dev: &str, inode: u64) -> FileId {
        FileId {
            dev: dev.to_owned(),
            inode,
        }
    }

    // FINDING 1 -- bracketed mappings.
    //
    // POSITIVE: a raw syscall in the kernel-supplied [vdso] is exempt from
    // REDIRECTION (it is not writable, so it cannot be patched) but is still
    // COUNTED, so the cell cannot be silently credited to SaBRe.
    #[test]
    fn vdso_raw_syscall_is_counted_not_silently_trusted() {
        let maps = maps_row("7ffff7fc9000-7ffff7fcb000", "00:00", 0, "[vdso]");
        let classification = classify(&maps, 0x7ffff7fc9010, None);
        assert!(
            classification.trusted,
            "[vdso] is not writable, so it must not be redirected"
        );
        assert_eq!(
            classification.trusted_shared_object,
            Some(PathBuf::from("[vdso]")),
            "a [vdso] raw syscall did not traverse the SaBRe handler and must be counted"
        );
    }

    // NEGATIVE: any other bracket-named mapping is NOT causally identified
    // infrastructure. An executable [stack] hosting a raw syscall must be
    // redirected and counted as a fallback site, never silently exempted.
    #[test]
    fn non_infrastructure_bracket_mapping_is_refused() {
        for name in ["[stack]", "[heap]", "[anon:jit]"] {
            let maps = maps_row("7ffffffde000-7ffffffff000", "00:00", 0, name);
            let classification = classify(&maps, 0x7ffffffde010, None);
            assert!(
                !classification.trusted,
                "{name} must be redirected through the SaBRe handler, not exempted"
            );
        }
    }

    // FINDING 2 -- infrastructure identity.
    //
    // POSITIVE: the launched loader is recognised by its own dev+inode and is
    // exempt without being counted, because it IS the measured path.
    #[test]
    fn launched_sabre_object_is_recognised_by_identity() {
        let maps = maps_row("400000-401000", "fd:01", 4242, "/opt/hermit/rsrcs/sabre");
        let classification = classify(&maps, 0x400010, Some(&file_id("fd:01", 4242)));
        assert!(classification.trusted);
        assert_eq!(classification.trusted_shared_object, None);
    }

    // NEGATIVE: an impostor with the SAME BASENAME but a different object
    // identity must NOT inherit the exemption. This is the planted bypass.
    #[test]
    fn same_basename_different_object_is_refused() {
        let maps = maps_row("500000-501000", "fd:01", 9999, "/tmp/attacker/sabre");
        let classification = classify(&maps, 0x500010, Some(&file_id("fd:01", 4242)));
        assert!(
            !classification.trusted,
            "basename `sabre` must not exempt a different object"
        );
        assert_eq!(classification.trusted_shared_object, None);
    }

    // NEGATIVE: an unstat-able launched path yields no identity, and a missing
    // identity must withhold the exemption (fail closed) rather than grant it.
    #[test]
    fn unknown_launched_identity_grants_no_exemption() {
        let maps = maps_row("400000-401000", "fd:01", 4242, "/opt/hermit/rsrcs/sabre");
        let classification = classify(&maps, 0x400010, None);
        assert!(
            !classification.trusted,
            "an unresolved launched identity must not exempt anything"
        );
    }

    // A shared object that is neither infrastructure nor kernel mapping stays
    // trusted-for-safety but counted, which is the pre-existing contract this
    // change must not weaken.
    #[test]
    fn other_shared_object_remains_counted() {
        let maps = maps_row(
            "7f0000000000-7f0000001000",
            "fd:01",
            77,
            "/usr/lib64/libc.so.6",
        );
        let classification = classify(&maps, 0x7f0000000010, Some(&file_id("fd:01", 4242)));
        assert!(classification.trusted);
        assert_eq!(
            classification.trusted_shared_object,
            Some(PathBuf::from("/usr/lib64/libc.so.6"))
        );
    }

    // FINDING 3 -- cache invalidation. Both sides of the predicate that decides
    // whether a tracee's cached page verdicts survive a syscall.
    #[test]
    fn address_space_mutators_invalidate_and_others_do_not() {
        for nr in [
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mremap,
            libc::SYS_mprotect,
            libc::SYS_brk,
            libc::SYS_shmat,
        ] {
            assert!(
                mutates_address_space(nr as u64),
                "syscall {nr} changes the address space and must invalidate the cache"
            );
        }
        for nr in [libc::SYS_read, libc::SYS_write, libc::SYS_getpid] {
            assert!(
                !mutates_address_space(nr as u64),
                "syscall {nr} cannot change a mapping and must not invalidate the cache"
            );
        }
    }
}
