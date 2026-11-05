/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An unwired x86-64 Linux ELF loader and the host side of its start protocol.
//!
//! The caller pins a readable ELF file, creates a short symlink to the prepared
//! loader image, and executes the padded symlink with the target's argv/envp.
//! The target and [`PreparedStart::metadata`] are passed on descriptors 100 and
//! 102. The loader reuses the kernel's initial stack, replaces its ELF-specific
//! auxv values, and transfers control to the target's PT_INTERP.
//!
//! The LA layout API and the LB exec preparation API are both unwired.
//! [`prepare_exec`] checks authorization and precommit format/argument errors;
//! CHECK alone never establishes that an ELF can load. The caller must use
//! `ADDR_NO_RANDOMIZE`, keep the pinned files immutable,
//! preserve descriptor numbers, arrange the native argv/envp, and authorize
//! direct ELF exec with identical resulting credentials and secureexec state.
//! No Reverie backend calls this crate. See the crate README for the admitted
//! ELF class and the exact differences still visible through procfs.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("reverie-elf-loader supports x86-64 Linux only");

pub mod arguments;
pub mod descriptors;
pub mod exec;
pub mod host;
pub mod proc_state;
pub mod protocol;
#[cfg(test)]
mod test_support;

use std::ffi::CString;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::Permissions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

pub use exec::ExecCheckOutcome;
pub use exec::ExecRefusal;
pub use exec::ExecRequest;
pub use exec::NativeExecError;
pub use exec::PinnedStart;
pub use exec::PrepareExecOptions;
pub use exec::prepare_exec;
pub use exec::prepare_exec_raw;
pub use exec::prepare_start_from_files;

/// Linux's path buffer includes its terminating NUL.
pub const PATH_MAX: usize = 4096;
/// The lower address band reserved for the loader and its scratch space.
pub const MIN_PROGRAM_ADDRESS: u64 = 0x400000;
pub const TARGET_FD: RawFd = 100;
pub const METADATA_FD: RawFd = 102;
const PAGE: u64 = 4096;
const ELF_ET_DYN_BASE: u64 = 0x555555554aaa;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_GNU_STACK: u32 = 0x6474e551;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const USER_LIMIT: u64 = 0x800000000000;
// Linux x86-64 STACK_TOP with ASLR off. Argument strings and pointers are
// capped at 6 MiB; setup_arg_pages adds at most 128 KiB. Reserving 8 MiB
// also covers page rounding, platform/random/auxv bytes and table alignment,
// independently of the caller's argv/envp and RLIMIT_STACK.
const STACK_TOP: u64 = USER_LIMIT - PAGE;
const MAP_LIMIT: u64 = STACK_TOP;
const INITIAL_STACK_LOW: u64 = STACK_TOP - 8 * 1024 * 1024;
// With at most 256 load intervals plus two first reservations, this cap
// leaves a large gap above our band below the lowest mmap base. Combined
// with the kernel's stack-guard/fallback rules, retained low mappings cannot
// change a successful admitted allocation. See KERNEL-AUDIT.md for the proof.
const MAX_MAPPING_SIZE: u64 = 16 * 1024 * 1024 * 1024;
// Keep the largest supported special-map span below loader text, entirely
// outside every admitted target/interpreter mapping and allocator search.
const TEMP_SPECIAL_START: u64 = 0x20000;
const LOADER_CODE_START: u64 = 0x100000;
const MAX_SPECIAL_SIZE: u64 = 0x80000;

/// The five scalar auxv values replaced after loading.
pub const AUXV_PATCH_TYPES: [u64; 5] = [3, 5, 7, 9, 33];

/// Why a start cannot be prepared. These are loader refusals, not guest errno.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidElf(&'static str),
    UnsupportedElf(&'static str),
    PaddedPathTooLong {
        length_with_nul: usize,
        maximum: usize,
    },
    LauncherPathTooLong {
        filename_length: usize,
        launcher_length: usize,
    },
    LowLoadSegment {
        address: u64,
        interpreter: bool,
    },
    ReservedTopPage {
        address: u64,
        interpreter: bool,
    },
    MappingTooLarge {
        address: u64,
        size: u64,
        interpreter: bool,
    },
    InterpreterHintOverlapsLoader {
        address: u64,
        span: u64,
    },
    MdweExecutableBss {
        address: u64,
        interpreter: bool,
    },
    BssRightMerge {
        address: u64,
        interpreter: bool,
    },
    InitialStackOverlap {
        address: u64,
        size: u64,
        interpreter: bool,
    },
    InterpreterEntry {
        address: u64,
    },
    PrivilegedExecutable {
        reason: &'static str,
    },
    HugetlbElf {
        interpreter: bool,
    },
    StackGuardGapOverride,
    MmapMinAddrTooHigh {
        minimum: u64,
    },
    MissingInterpreter,
    ZeroInterpreterLoadSpan,
    ExecutableStack,
    InvalidInvocation(&'static str),
    FiniteAddressSpaceLimit {
        limit: u64,
    },
    FiniteDataLimitBelowStartupFootprint {
        limit: u64,
        minimum: u64,
    },
    InvalidLoaderTemplate(&'static str),
    AmbiguousDescriptorComm,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ELF loader I/O: {e}"),
            Self::InvalidElf(reason) => write!(f, "invalid ELF: {reason}"),
            Self::UnsupportedElf(reason) => {
                write!(f, "ELF loader refuses unsupported ELF: {reason}")
            }
            Self::PaddedPathTooLong {
                length_with_nul,
                maximum,
            } => write!(
                f,
                "ELF loader refuses padded path of {length_with_nul} bytes including NUL; PATH_MAX is {maximum}"
            ),
            Self::LauncherPathTooLong {
                filename_length,
                launcher_length,
            } => write!(
                f,
                "ELF loader refuses filename length {filename_length}; launcher path requires {launcher_length} bytes"
            ),
            Self::LowLoadSegment {
                address,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} PT_LOAD at {address:#x}, below {MIN_PROGRAM_ADDRESS:#x}",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::InterpreterHintOverlapsLoader { address, span } => write!(
                f,
                "ELF loader refuses interpreter PT_LOAD hint {address:#x} with span {span:#x} in its reserved range below {MIN_PROGRAM_ADDRESS:#x}"
            ),
            Self::ReservedTopPage {
                address,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} PT_LOAD at {address:#x} reaching the x86-64 reserved top page",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::MappingTooLarge {
                address,
                size,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} mapping at {address:#x} of size {size:#x}, above its {MAX_MAPPING_SIZE:#x} allocator bound",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::MdweExecutableBss {
                address,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} executable PT_LOAD BSS at {address:#x} under inherited MDWE PR_MDWE_REFUSE_EXEC_GAIN",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::BssRightMerge {
                address,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} anonymous BSS ending at {address:#x}: mmap would merge with the right VMA unlike vm_brk_flags",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::InitialStackOverlap {
                address,
                size,
                interpreter,
            } => write!(
                f,
                "ELF loader refuses {} PT_LOAD range {address:#x}+{size:#x} overlapping its initial stack reservation",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::InterpreterEntry { address } => write!(
                f,
                "ELF loader refuses interpreter entry {address:#x} outside its admitted address range"
            ),
            Self::PrivilegedExecutable { reason } => write!(
                f,
                "ELF loader refuses privileged executable: {reason} would change exec credentials"
            ),
            Self::HugetlbElf { interpreter } => write!(
                f,
                "ELF loader refuses hugetlb-backed {}: mmap would round ELF file mappings differently",
                if *interpreter {
                    "interpreter"
                } else {
                    "program"
                }
            ),
            Self::StackGuardGapOverride => write!(
                f,
                "ELF loader refuses a stack_guard_gap boot override: initial stack table growth could change allocator placement"
            ),
            Self::MmapMinAddrTooHigh { minimum } => write!(
                f,
                "ELF loader refuses vm.mmap_min_addr {minimum:#x} above {:#x}: temporary special mappings must fit below loader text",
                LOADER_CODE_START - MAX_SPECIAL_SIZE
            ),
            Self::MissingInterpreter => write!(f, "ELF loader refuses program without PT_INTERP"),
            Self::ZeroInterpreterLoadSpan => {
                write!(
                    f,
                    "ELF loader refuses interpreter with zero PT_LOAD mapping span"
                )
            }
            Self::ExecutableStack => write!(
                f,
                "ELF loader refuses executable, missing or repeated PT_GNU_STACK"
            ),
            Self::InvalidInvocation(reason) => write!(f, "ELF loader refuses invocation: {reason}"),
            Self::FiniteAddressSpaceLimit { limit } => write!(
                f,
                "ELF loader refuses finite RLIMIT_AS ({limit} bytes); loader mappings consume additional address space"
            ),
            Self::InvalidLoaderTemplate(reason) => write!(f, "invalid loader template: {reason}"),
            Self::FiniteDataLimitBelowStartupFootprint { limit, minimum } => write!(
                f,
                "ELF loader refuses RLIMIT_DATA {limit} bytes below its {minimum}-byte ELF startup footprint boundary"
            ),
            Self::AmbiguousDescriptorComm => write!(
                f,
                "ELF loader refuses AT_EMPTY_PATH comm: a procfd name ending in ' (deleted)' with live hardlinks is ambiguous"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// A literal exec call. `dirfd` is the descriptor number used by the new child.
#[derive(Clone, Debug)]
pub struct Invocation {
    dirfd: RawFd,
    path: CString,
    flags: i32,
}

impl Invocation {
    pub fn execve(path: impl AsRef<OsStr>) -> Result<Self, Error> {
        Self::execveat(libc::AT_FDCWD, path, 0)
    }

    pub fn execveat(dirfd: RawFd, path: impl AsRef<OsStr>, flags: i32) -> Result<Self, Error> {
        if flags & !(libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW) != 0 {
            return Err(Error::InvalidInvocation("unsupported execveat flags"));
        }
        let path = CString::new(path.as_ref().as_bytes())
            .map_err(|_| Error::InvalidInvocation("path contains NUL"))?;
        if path.as_bytes().is_empty() && flags & libc::AT_EMPTY_PATH == 0 {
            return Err(Error::InvalidInvocation("empty path without AT_EMPTY_PATH"));
        }
        Ok(Self { dirfd, path, flags })
    }

    /// The kernel's `bprm->filename`, including execveat's `/dev/fd/` prefix.
    pub fn native_execfn(&self) -> CString {
        if self.dirfd == libc::AT_FDCWD || self.path.as_bytes().starts_with(b"/") {
            return self.path.clone();
        }
        let mut filename = format!("/dev/fd/{}", self.dirfd).into_bytes();
        if !self.path.as_bytes().is_empty() {
            filename.push(b'/');
            filename.extend_from_slice(self.path.as_bytes());
        }
        CString::new(filename).expect("numeric descriptor and validated path have no NUL")
    }

    pub fn dirfd(&self) -> RawFd {
        self.dirfd
    }
    pub fn path(&self) -> &std::ffi::CStr {
        &self.path
    }
    pub fn flags(&self) -> i32 {
        self.flags
    }
}

/// Limits that will be installed in the child before exec.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub data: u64,
    pub address_space: u64,
}

/// Loader host facts captured before descriptor transfer. They avoid new
/// pathname opens when all remaining descriptor slots are occupied.
/// The caller must keep these facts and the execution context stable.
#[derive(Clone, Debug)]
pub struct LoaderHostFacts {
    pub cmdline: Vec<u8>,
    pub mmap_min_addr: u64,
    pub mdwe_inherited: bool,
}

impl LoaderHostFacts {
    pub fn current() -> Result<Self, Error> {
        Ok(Self {
            cmdline: std::fs::read("/proc/cmdline")?,
            mmap_min_addr: parse_mmap_min_addr(&std::fs::read("/proc/sys/vm/mmap_min_addr")?)?,
            mdwe_inherited: mdwe_inherited_on_exec()?,
        })
    }
}

impl Limits {
    pub fn current() -> Result<Self, Error> {
        fn limit(resource: libc::__rlimit_resource_t) -> io::Result<u64> {
            let mut value = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: value is writable for the exact structure size.
            if unsafe { libc::getrlimit(resource, &mut value) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(value.rlim_cur)
        }
        Ok(Self {
            data: limit(libc::RLIMIT_DATA)?,
            address_space: limit(libc::RLIMIT_AS)?,
        })
    }
}

/// Native ELF layout with randomization disabled.
#[derive(Clone, Debug)]
pub struct ProgramLayout {
    pub load_bias: u64,
    pub program_base: u64,
    pub entry: u64,
    pub phdr: u64,
    pub phnum: u16,
    pub start_data: u64,
    pub end_data: u64,
    /// Raw maximum biased PT_LOAD memory end, before page rounding for brk.
    pub memory_end: u64,
    pub start_brk: u64,
    pub interpreter: PathBuf,
    /// Conservative bound covering private writable target/interpreter loads,
    /// including a possible transient first-mapping reservation.
    pub minimum_data_limit: u64,
}

/// Values supplied at preparation or determined by the freestanding loader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxvValue {
    Fixed(u64),
    InterpreterBase,
    RelocatedVdso,
    ExecFnString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuxvPatch {
    pub kind: u64,
    pub value: AuxvValue,
}

/// A start plan. It holds no live descriptors and performs no exec itself.
#[derive(Clone, Debug)]
pub struct PreparedStart {
    pub native_execfn: CString,
    pub padded_path: CString,
    pub comm: CString,
    pub layout: ProgramLayout,
    pub auxv_patches: [AuxvPatch; 6],
}

impl PreparedStart {
    /// Bytes for the private, non-CLOEXEC metadata descriptor 102.
    pub fn metadata(&self) -> Vec<u8> {
        let mut bytes = self.native_execfn.as_bytes_with_nul().to_vec();
        bytes.extend_from_slice(self.comm.as_bytes_with_nul());
        bytes.push(0); // Production loader rejects nonzero mutation flags.
        bytes
    }

    /// Generate a sparse per-start loader ELF from the bundled static program.
    pub fn write_image(&self, output: &File) -> Result<(), Error> {
        self.write_image_from_template(output, loader_template())
    }

    /// Generate from another loader built with the same protocol/linker script.
    /// This also lets the integration tests exercise separately compiled mutations.
    pub fn write_image_from_template(&self, output: &File, template: &[u8]) -> Result<(), Error> {
        let elf = Elf::from_bytes(template)
            .map_err(|_| Error::InvalidLoaderTemplate("ELF header or program headers"))?;
        if elf.kind != ET_EXEC {
            return Err(Error::InvalidLoaderTemplate("expected fixed ET_EXEC"));
        }
        let index = elf
            .headers
            .iter()
            .position(|p| p.kind == PT_NOTE)
            .ok_or(Error::InvalidLoaderTemplate("missing replaceable PT_NOTE"))?;
        let mut image = template.to_vec();
        // Leave a sparse zero tail, with the file offset congruent to
        // start_data. Its R-only file pages do not add data_vm; its memsz
        // retains the raw memory end so the kernel alone rounds brk.
        let offset = page_start(checked_add(template.len() as u64, 0x10000)?)
            + self.layout.start_data % PAGE;
        let filesize = self
            .layout
            .end_data
            .checked_sub(self.layout.start_data)
            .ok_or(Error::InvalidElf("end_data precedes start_data"))?;
        let memsize = self
            .layout
            .memory_end
            .checked_sub(self.layout.start_data)
            .ok_or(Error::InvalidElf("memory_end precedes start_data"))?;
        if memsize < filesize {
            return Err(Error::InvalidElf("shadow file size exceeds memory size"));
        }
        if self.layout.start_brk != page_align(self.layout.memory_end)? {
            return Err(Error::InvalidElf("start_brk is not the rounded memory end"));
        }
        // ProgramLayout is public: repeat the reserved-band check before the
        // kernel can install a caller-modified shadow over the loader itself.
        let address = page_start(self.layout.start_data);
        if address < MIN_PROGRAM_ADDRESS {
            return Err(Error::LowLoadSegment {
                address,
                interpreter: false,
            });
        }
        check_load_range(self.layout.start_data, memsize, false)?;
        let shadow = ProgramHeader {
            kind: PT_LOAD,
            flags: 4,
            offset,
            vaddr: self.layout.start_data,
            paddr: self.layout.start_data,
            filesz: filesize,
            memsz: memsize,
            align: PAGE,
        };
        let header_offset = elf.phoff as usize + index * 56;
        shadow.write(&mut image[header_offset..header_offset + 56]);
        output.set_len(0)?;
        output.write_all_at(&image, 0)?;
        output.set_len(checked_add(offset, filesize)?)?;
        output.set_permissions(Permissions::from_mode(0o755))?;
        Ok(())
    }
}

/// The build.rs-generated static loader. This byte slice contains no libc.
pub fn loader_template() -> &'static [u8] {
    include_bytes!(env!("ELF_LOADER_TEMPLATE"))
}

pub fn prepare_start(
    target: &File,
    invocation: &Invocation,
    launcher_link: &Path,
) -> Result<PreparedStart, Error> {
    prepare_start_with_limits(target, invocation, launcher_link, Limits::current()?)
}

/// Prepare with the limits that the caller plans to install in the child.
/// Finite DATA at or above the ELF startup footprint is admitted: scratch is
/// shared, and shadow and data segments replace each other. Smaller DATA and
/// all finite AS limits are refused explicitly. Preparation reads the calling
/// process's inheritable MDWE setting; preserve that setting through exec, or
/// repeat preparation after enabling MDWE in the child. Child preparation
/// requires a process context safe for Rust allocation and I/O.
pub fn prepare_start_with_limits(
    target: &File,
    invocation: &Invocation,
    launcher_link: &Path,
    limits: Limits,
) -> Result<PreparedStart, Error> {
    prepare_start_with_files(target, None, invocation, launcher_link, limits, None)
}

// Both APIs use exactly the same LA admission and layout computations. The
// supplied-interpreter path never resolves or reopens its PT_INTERP pathname.
pub(crate) fn prepare_start_with_files(
    target: &File,
    interpreter: Option<&File>,
    invocation: &Invocation,
    launcher_link: &Path,
    limits: Limits,
    host: Option<&LoaderHostFacts>,
) -> Result<PreparedStart, Error> {
    let native_execfn = invocation.native_execfn();
    let padded_path = pad_launcher_path(launcher_link, native_execfn.as_bytes().len())?;
    if limits.address_space != libc::RLIM_INFINITY {
        return Err(Error::FiniteAddressSpaceLimit {
            limit: limits.address_space,
        });
    }
    check_exec_credentials(target)?;
    let current_host;
    let host = if let Some(host) = host {
        host
    } else {
        current_host = LoaderHostFacts::current()?;
        &current_host
    };
    check_stack_guard_cmdline(&host.cmdline)?;
    temporary_special_start(host.mmap_min_addr)?;
    let elf = Elf::read(target, false)?;
    let mut layout = elf.layout()?;
    // Classify without running a device open method or waiting on a FIFO,
    // then read the pinned object rather than repeating its path lookup.
    let opened_interpreter;
    let interp = if let Some(interpreter) = interpreter {
        interpreter
    } else {
        let interp_path = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&layout.interpreter)?;
        if !interp_path.metadata()?.is_file() {
            return Err(Error::UnsupportedElf("interpreter is not a regular file"));
        }
        opened_interpreter = File::open(format!("/proc/self/fd/{}", interp_path.as_raw_fd()))?;
        &opened_interpreter
    };
    let interp_elf = Elf::read(interp, true)?;
    interp_elf.check_raw_top_page(true)?;
    let interp_span = interp_elf.total_mapping_size()?;
    if interp_span == 0 {
        return Err(Error::ZeroInterpreterLoadSpan);
    }
    interp_elf.check_interpreter_ranges(layout.load_bias, interp_span)?;
    interp_elf.check_interpreter_entry(layout.load_bias)?;
    let mut bss = Vec::new();
    elf.check_bss_merges(layout.load_bias, elf.kind == ET_DYN, false, &mut bss)?;
    let interp_first = interp_elf
        .headers
        .iter()
        .find(|p| p.kind == PT_LOAD)
        .expect("validated interpreter has a PT_LOAD");
    if interp_elf.kind == ET_EXEC || interp_first.filesz == 0 {
        let bias = if interp_elf.kind == ET_DYN && layout.load_bias != 0 {
            0_u64.wrapping_sub(page_start(interp_first.vaddr))
        } else {
            0
        };
        interp_elf.check_bss_merges(bias, true, true, &mut bss)?;
    } else {
        // mmap chooses this interpreter's actual base at execution. Relative
        // self-overlap is already decidable here; the loader also checks actual
        // cross-image contacts against the BSS it has mapped.
        interp_elf.check_bss_merges(0, true, true, &mut Vec::new())?;
    }
    if host.mdwe_inherited {
        elf.check_mdwe_bss(false)?;
        interp_elf.check_mdwe_bss(true)?;
    }
    let (target_data, target_peak) = elf.data_footprint(elf.kind == ET_DYN)?;
    let (_, interpreter_peak) = interp_elf.data_footprint(true)?;
    layout.minimum_data_limit = target_peak.max(checked_add(target_data, interpreter_peak)?);
    if limits.data < layout.minimum_data_limit {
        return Err(Error::FiniteDataLimitBelowStartupFootprint {
            limit: limits.data,
            minimum: layout.minimum_data_limit,
        });
    }
    let comm_bytes = if invocation.path.as_bytes().is_empty() {
        // With AT_EMPTY_PATH Linux uses the executable dentry, not /dev/fd/N.
        // An unlinked file with no surviving hardlinks has an unambiguous
        // kernel-added suffix; the remaining ambiguous class is refused.
        let link = std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(target)
        ))?;
        let bytes = link.as_os_str().as_bytes();
        let bytes = if let Some(original) = bytes.strip_suffix(b" (deleted)") {
            if target.metadata()?.nlink() != 0 {
                return Err(Error::AmbiguousDescriptorComm);
            }
            original
        } else {
            bytes
        };
        bytes
            .rsplit(|b| *b == b'/')
            .next()
            .unwrap_or(bytes)
            .to_vec()
    } else {
        native_execfn
            .as_bytes()
            .rsplit(|b| *b == b'/')
            .next()
            .unwrap_or_default()
            .to_vec()
    };
    let comm = CString::new(&comm_bytes[..comm_bytes.len().min(15)])
        .map_err(|_| Error::InvalidInvocation("comm contains NUL"))?;
    let auxv_patches = [
        AuxvPatch {
            kind: 3,
            value: AuxvValue::Fixed(layout.phdr),
        },
        AuxvPatch {
            kind: 5,
            value: AuxvValue::Fixed(u64::from(layout.phnum)),
        },
        AuxvPatch {
            kind: 7,
            value: AuxvValue::InterpreterBase,
        },
        AuxvPatch {
            kind: 9,
            value: AuxvValue::Fixed(layout.entry),
        },
        AuxvPatch {
            kind: 33,
            value: AuxvValue::RelocatedVdso,
        },
        AuxvPatch {
            kind: 31,
            value: AuxvValue::ExecFnString,
        },
    ];
    Ok(PreparedStart {
        native_execfn,
        padded_path,
        comm,
        layout,
        auxv_patches,
    })
}

/// Pad redundant separators, preserving lookup and the exact filename length.
pub fn pad_launcher_path(link: &Path, filename_length: usize) -> Result<CString, Error> {
    let length_with_nul = filename_length.saturating_add(1);
    if length_with_nul > PATH_MAX {
        return Err(Error::PaddedPathTooLong {
            length_with_nul,
            maximum: PATH_MAX,
        });
    }
    let mut path = link.as_os_str().as_bytes().to_vec();
    if path.contains(&0) || path.is_empty() {
        return Err(Error::InvalidInvocation(
            "launcher link is empty or contains NUL",
        ));
    }
    if path.len() < filename_length && !path.contains(&b'/') {
        let mut relative = b"./".to_vec();
        relative.extend_from_slice(&path);
        path = relative;
    }
    if path.len() > filename_length {
        return Err(Error::LauncherPathTooLong {
            filename_length,
            launcher_length: path.len(),
        });
    }
    if let Some(position) = path.iter().position(|b| *b == b'/') {
        let padding = filename_length - path.len();
        path.splice(position..position, std::iter::repeat_n(b'/', padding));
    }
    CString::new(path).map_err(|_| Error::InvalidInvocation("launcher link contains NUL"))
}

#[derive(Clone, Debug)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl ProgramHeader {
    fn read(bytes: &[u8]) -> Self {
        Self {
            kind: u32_at(bytes, 0),
            flags: u32_at(bytes, 4),
            offset: u64_at(bytes, 8),
            vaddr: u64_at(bytes, 16),
            paddr: u64_at(bytes, 24),
            filesz: u64_at(bytes, 32),
            memsz: u64_at(bytes, 40),
            align: u64_at(bytes, 48),
        }
    }
    fn write(&self, bytes: &mut [u8]) {
        bytes[..4].copy_from_slice(&self.kind.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.flags.to_le_bytes());
        for (index, value) in [
            self.offset,
            self.vaddr,
            self.paddr,
            self.filesz,
            self.memsz,
            self.align,
        ]
        .iter()
        .enumerate()
        {
            bytes[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_le_bytes());
        }
    }
}

struct Elf {
    kind: u16,
    entry: u64,
    phoff: u64,
    phnum: u16,
    headers: Vec<ProgramHeader>,
    interpreter: Option<PathBuf>,
}

struct BssRange {
    start: u64,
    end: u64,
    executable: bool,
}

impl Elf {
    fn header(bytes: &[u8]) -> Result<(u16, u64, u64, u16), Error> {
        if bytes.len() < 64 || bytes[..4] != *b"\x7fELF" {
            return Err(Error::InvalidElf("truncated or missing ELF header"));
        }
        if bytes[4..7] != [2, 1, 1] || u16_at(bytes, 18) != 62 || u32_at(bytes, 20) != 1 {
            return Err(Error::UnsupportedElf("expected little-endian ELF64 x86-64"));
        }
        let kind = u16_at(bytes, 16);
        if kind != ET_EXEC && kind != ET_DYN {
            return Err(Error::UnsupportedElf("expected ET_EXEC or ET_DYN"));
        }
        let phnum = u16_at(bytes, 56);
        if u16_at(bytes, 52) != 64 || u16_at(bytes, 54) != 56 || phnum == 0 || phnum > 128 {
            return Err(Error::UnsupportedElf(
                "program-header size or count (maximum 128)",
            ));
        }
        Ok((kind, u64_at(bytes, 24), u64_at(bytes, 32), phnum))
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (kind, entry, phoff, phnum) = Self::header(bytes)?;
        let end = checked_add(phoff, u64::from(phnum) * 56)?;
        let header_bytes = bytes
            .get(
                phoff as usize
                    ..usize::try_from(end)
                        .map_err(|_| Error::InvalidElf("program-header offset overflow"))?,
            )
            .ok_or(Error::InvalidElf("truncated program headers"))?;
        Ok(Self {
            kind,
            entry,
            phoff,
            phnum,
            headers: header_bytes
                .as_chunks::<56>()
                .0
                .iter()
                .map(|bytes| ProgramHeader::read(bytes))
                .collect(),
            interpreter: None,
        })
    }

    fn read(file: &File, is_interpreter: bool) -> Result<Self, Error> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::UnsupportedElf("ELF is not a regular file"));
        }
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the descriptor is pinned and filesystem is writable for
        // sizeof(statfs). A successful fstatfs initializes the structure.
        if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: fstatfs succeeded and initialized the structure above.
        if unsafe { filesystem.assume_init() }.f_type == libc::HUGETLBFS_MAGIC {
            return Err(Error::HugetlbElf {
                interpreter: is_interpreter,
            });
        }
        let mut header = [0; 64];
        file.read_exact_at(&mut header, 0)?;
        let (kind, entry, phoff, phnum) = Self::header(&header)?;
        let mut bytes = vec![0; usize::from(phnum) * 56];
        file.read_exact_at(&mut bytes, phoff)?;
        let headers: Vec<_> = bytes
            .as_chunks::<56>()
            .0
            .iter()
            .map(|bytes| ProgramHeader::read(bytes))
            .collect();
        let file_length = metadata.len();
        let mut interpreter = None;
        let mut last_load = None;
        for p in &headers {
            if p.kind == PT_LOAD {
                if p.filesz > p.memsz || checked_add(p.offset, p.filesz)? > file_length {
                    return Err(Error::InvalidElf("PT_LOAD file size"));
                }
                let memory_end = checked_add(p.vaddr, p.memsz)?;
                if p.vaddr >= USER_LIMIT || memory_end > USER_LIMIT {
                    return Err(Error::UnsupportedElf(
                        "PT_LOAD exceeds x86-64 default user address range",
                    ));
                }
                if p.offset % PAGE != p.vaddr % PAGE || (p.align > 1 && !p.align.is_power_of_two())
                {
                    return Err(Error::UnsupportedElf("PT_LOAD alignment"));
                }
                if last_load.is_some_and(|last| p.vaddr < last) {
                    return Err(Error::UnsupportedElf("unordered PT_LOAD headers"));
                }
                last_load = Some(p.vaddr);
            }
            if p.kind == PT_INTERP {
                if interpreter.is_some() || p.filesz < 2 || p.filesz > PATH_MAX as u64 {
                    return Err(Error::InvalidElf("PT_INTERP count or length"));
                }
                let mut path = vec![0; p.filesz as usize];
                file.read_exact_at(&mut path, p.offset)?;
                if path.last() != Some(&0) || path[..path.len() - 1].contains(&0) {
                    return Err(Error::InvalidElf(
                        "PT_INTERP is not a single terminated string",
                    ));
                }
                path.pop();
                interpreter = Some(PathBuf::from(OsStr::from_bytes(&path)));
            }
        }
        if last_load.is_none() {
            return Err(Error::InvalidElf("no PT_LOAD"));
        }
        Ok(Self {
            kind,
            entry,
            phoff,
            phnum,
            headers,
            interpreter,
        })
    }

    fn check_low(&self, bias: u64, interpreter: bool) -> Result<(), Error> {
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            let address = checked_add(bias, page_start(p.vaddr))?;
            if address < MIN_PROGRAM_ADDRESS {
                return Err(Error::LowLoadSegment {
                    address,
                    interpreter,
                });
            }
            check_load_range(checked_add(bias, p.vaddr)?, p.memsz, interpreter)?;
        }
        Ok(())
    }

    fn check_raw_top_page(&self, interpreter: bool) -> Result<(), Error> {
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            if p.vaddr >= MAP_LIMIT || checked_add(p.vaddr, p.memsz)? > MAP_LIMIT {
                return Err(Error::ReservedTopPage {
                    address: p.vaddr,
                    interpreter,
                });
            }
        }
        Ok(())
    }

    fn check_interpreter_ranges(&self, main_bias: u64, span: u64) -> Result<(), Error> {
        let first = self
            .headers
            .iter()
            .find(|p| p.kind == PT_LOAD)
            .expect("validated interpreter has a PT_LOAD");
        let first_page = page_start(first.vaddr);
        if first.filesz != 0 {
            check_mapping_size(first_page, page_align(span)?, true)?;
        }
        if self.kind == ET_EXEC {
            for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
                check_load_range(p.vaddr, p.memsz, true)?;
            }
            if first.filesz != 0 {
                check_initial_stack_range(first_page, page_align(span)?, true)?;
            }
            return Ok(());
        }

        let base = if first.filesz == 0 {
            // elf_load does not call mmap for a zero-filesz first load, even
            // when it has BSS. load_elf_interp still establishes its bias:
            // zero for an unbiased main, -page_start(first.vaddr) for PIE.
            // Subsequent loads are fixed at base + (vaddr - first_page).
            if main_bias == 0 { first_page } else { 0 }
        } else {
            // A file-backed first load reserves the complete span at an
            // allocator-selected base. Refuse a preserved low hint, then
            // check every relative range at the lowest admitted base. The
            // full reservation protects later loads from existing mappings;
            // the loader checks its actual returned base before zeroing BSS
            // or making any later fixed mapping.
            if main_bias == 0 && first_page != 0 && first_page < MIN_PROGRAM_ADDRESS {
                return Err(Error::InterpreterHintOverlapsLoader {
                    address: first_page,
                    span,
                });
            }
            MIN_PROGRAM_ADDRESS
        };
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            let offset = p.vaddr.checked_sub(first_page).ok_or(Error::InvalidElf(
                "interpreter load precedes its first page",
            ))?;
            check_load_range(checked_add(base, offset)?, p.memsz, true)?;
        }
        Ok(())
    }

    fn check_interpreter_entry(&self, main_bias: u64) -> Result<(), Error> {
        let first = self.headers.iter().find(|p| p.kind == PT_LOAD).unwrap();
        if self.kind == ET_DYN && first.filesz != 0 {
            // The actual mmap-selected bias is unknown until loading. Refuse
            // raw entries outside the scoped window, even if modular addition
            // could bring one back into it. Loading checks the resolved entry.
            if self.entry >= MAP_LIMIT {
                return Err(Error::InterpreterEntry {
                    address: self.entry,
                });
            }
            return Ok(());
        }
        let bias = if self.kind == ET_DYN && main_bias != 0 {
            0_u64.wrapping_sub(page_start(first.vaddr))
        } else {
            0
        };
        let address = bias.wrapping_add(self.entry);
        if !(MIN_PROGRAM_ADDRESS..MAP_LIMIT).contains(&address) {
            return Err(Error::InterpreterEntry { address });
        }
        Ok(())
    }

    fn check_bss_merges(
        &self,
        bias: u64,
        reserves_span: bool,
        interpreter: bool,
        bss: &mut Vec<BssRange>,
    ) -> Result<(), Error> {
        let mut first = true;
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            let address = bias.wrapping_add(p.vaddr);
            if p.filesz != 0 {
                let size = if first && reserves_span {
                    page_align(self.total_mapping_size()?)?
                } else {
                    page_align(checked_add(p.filesz, address % PAGE)?)?
                };
                remove_bss_range(bss, page_start(address), size)?;
            }
            first = false;
            if p.memsz <= p.filesz {
                continue;
            }
            let start = if p.filesz == 0 {
                page_start(address)
            } else {
                page_align(checked_add(address, p.filesz)?)?
            };
            let end = page_align(checked_add(address, p.memsz)?)?;
            let executable = p.flags & 1 != 0;
            if end > start {
                if bss.iter().any(|range| {
                    range.start <= end && end < range.end && range.executable == executable
                }) {
                    return Err(Error::BssRightMerge {
                        address: end,
                        interpreter,
                    });
                }
                remove_bss_range(bss, start, end - start)?;
                bss.push(BssRange {
                    start,
                    end,
                    executable,
                });
            }
        }
        Ok(())
    }

    fn layout(&self) -> Result<ProgramLayout, Error> {
        let interpreter = self.interpreter.clone().ok_or(Error::MissingInterpreter)?;
        let stacks: Vec<_> = self
            .headers
            .iter()
            .filter(|p| p.kind == PT_GNU_STACK)
            .collect();
        if stacks.len() != 1 || stacks[0].flags & 1 != 0 {
            return Err(Error::ExecutableStack);
        }
        self.check_raw_top_page(false)?;
        let loads: Vec<_> = self.headers.iter().filter(|p| p.kind == PT_LOAD).collect();
        let first = loads[0];
        let mut load_bias = 0;
        if self.kind == ET_DYN {
            // maximum_alignment page-rounds its maximum from zero. All-zero
            // p_align values leave ELF_ET_DYN_BASE unmasked before the first
            // vaddr is subtracted and the result is rounded down to a page.
            let alignment = page_align(loads.iter().map(|p| p.align).max().unwrap_or(0))?;
            let base = if alignment == 0 {
                ELF_ET_DYN_BASE
            } else {
                ELF_ET_DYN_BASE & !(alignment - 1)
            };
            load_bias = page_start(base.checked_sub(first.vaddr).ok_or(Error::UnsupportedElf(
                "PIE first segment exceeds native load base",
            ))?);
        }
        self.check_low(load_bias, false)?;
        if self.kind == ET_DYN && first.filesz != 0 {
            check_mapping_size(
                checked_add(page_start(first.vaddr), load_bias)?,
                page_align(self.total_mapping_size()?)?,
                false,
            )?;
            check_initial_stack_range(
                checked_add(page_start(first.vaddr), load_bias)?,
                page_align(self.total_mapping_size()?)?,
                false,
            )?;
        }
        let start_data = checked_add(loads.iter().map(|p| p.vaddr).max().unwrap(), load_bias)?;
        let end_data = checked_add(
            loads
                .iter()
                .map(|p| checked_add(p.vaddr, p.filesz))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .max()
                .unwrap(),
            load_bias,
        )?;
        let memory_end = checked_add(
            loads
                .iter()
                .map(|p| checked_add(p.vaddr, p.memsz))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .max()
                .unwrap(),
            load_bias,
        )?;
        if end_data < start_data {
            return Err(Error::UnsupportedElf("end_data precedes start_data"));
        }
        let start_brk = page_align(memory_end)?;
        // Preserve the raw memory end: rounding it into the shadow's memsz
        // would allocate a BSS page for an unaligned empty final load.
        check_load_range(start_data, memory_end - start_data, false)?;
        let phdr = loads
            .iter()
            .rfind(|p| p.offset <= self.phoff && self.phoff < p.offset + p.filesz)
            .ok_or(Error::UnsupportedElf("program headers are not mapped"))?;
        Ok(ProgramLayout {
            load_bias,
            program_base: checked_add(page_start(first.vaddr), load_bias)?,
            entry: checked_add(self.entry, load_bias)?,
            phdr: checked_add(
                checked_add(phdr.vaddr, self.phoff - phdr.offset)?,
                load_bias,
            )?,
            phnum: self.phnum,
            start_data,
            end_data,
            memory_end,
            start_brk,
            interpreter,
            minimum_data_limit: 0, // Filled after the pinned interpreter is read.
        })
    }

    fn total_mapping_size(&self) -> Result<u64, Error> {
        // Linux 6.17 binfmt_elf.c rounds only the minimum load address here;
        // the maximum is the actual vaddr + memsz, not its page-aligned end.
        let mut low = u64::MAX;
        let mut high = 0;
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            low = low.min(page_start(p.vaddr));
            high = high.max(checked_add(p.vaddr, p.memsz)?);
        }
        Ok(if low == u64::MAX { 0 } else { high - low })
    }

    fn check_mdwe_bss(&self, interpreter: bool) -> Result<(), Error> {
        for p in self.headers.iter().filter(|p| p.kind == PT_LOAD) {
            if p.flags & 1 == 0 || p.memsz <= p.filesz {
                continue;
            }
            // File-backed BSS in the last partial page requires no anonymous
            // mapping. A load with no file bytes includes its first page.
            let zero_start = if p.filesz == 0 {
                page_start(p.vaddr)
            } else {
                page_align(checked_add(p.vaddr, p.filesz)?)?
            };
            let zero_end = page_align(checked_add(p.vaddr, p.memsz)?)?;
            if zero_end > zero_start {
                return Err(Error::MdweExecutableBss {
                    address: p.vaddr,
                    interpreter,
                });
            }
        }
        Ok(())
    }

    fn data_footprint(&self, reserves_span: bool) -> Result<(u64, u64), Error> {
        let loads: Vec<_> = self.headers.iter().filter(|p| p.kind == PT_LOAD).collect();
        let mut data = 0;
        let mut peak = 0;
        for (index, p) in loads.iter().enumerate() {
            let file_pages = if p.filesz == 0 {
                0
            } else {
                page_align(checked_add(p.filesz, p.vaddr % PAGE)?)?
            };
            if p.flags & 2 != 0 {
                let span = if index == 0 && reserves_span && p.filesz != 0 {
                    page_align(self.total_mapping_size()?)?
                } else {
                    file_pages
                };
                peak = peak.max(checked_add(data, span)?);
                data = checked_add(data, file_pages)?;
            }
            if p.memsz > p.filesz {
                // binfmt_elf supplies RW anonymous BSS even for an R-only
                // segment. A zero-file-size load includes its first partial
                // page, whereas file-backed BSS starts at the next page.
                let zero_start = if p.filesz == 0 {
                    page_start(p.vaddr)
                } else {
                    page_align(checked_add(p.vaddr, p.filesz)?)?
                };
                let zero_end = page_align(checked_add(p.vaddr, p.memsz)?)?;
                data = checked_add(data, zero_end - zero_start)?;
            }
            peak = peak.max(data);
        }
        Ok((data, peak))
    }
}

fn remove_bss_range(bss: &mut Vec<BssRange>, start: u64, size: u64) -> Result<(), Error> {
    let end = checked_add(start, size)?;
    let mut remaining = Vec::new();
    for range in bss.drain(..) {
        if range.start >= end || range.end <= start {
            remaining.push(range);
            continue;
        }
        if range.start < start {
            remaining.push(BssRange {
                start: range.start,
                end: start,
                executable: range.executable,
            });
        }
        if range.end > end {
            remaining.push(BssRange {
                start: end,
                end: range.end,
                executable: range.executable,
            });
        }
    }
    *bss = remaining;
    Ok(())
}

fn check_exec_credentials(target: &File) -> Result<(), Error> {
    if target.metadata()?.mode() & (libc::S_ISUID | libc::S_ISGID) != 0 {
        return Err(Error::PrivilegedExecutable {
            reason: "setid mode",
        });
    }
    // SAFETY: the constant name is NUL terminated; a null buffer with size 0
    // queries only the size of the xattr on the pinned descriptor.
    let size = unsafe {
        libc::fgetxattr(
            target.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size >= 0 {
        return Err(Error::PrivilegedExecutable {
            reason: "security.capability",
        });
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)) {
        return Ok(());
    }
    Err(error.into())
}

fn check_stack_guard_cmdline(cmdline: &[u8]) -> Result<(), Error> {
    // A default 1 MiB stack guard and our 8 MiB initial-stack bound stay
    // above the minimum 128 MiB mmap gap. Refuse every visible override,
    // including invalid/small values, rather than approximate kernel parsing.
    let key = b"stack_guard_gap=";
    if cmdline.windows(key.len()).any(|bytes| bytes == key) {
        return Err(Error::StackGuardGapOverride);
    }
    Ok(())
}

fn parse_mmap_min_addr(bytes: &[u8]) -> Result<u64, Error> {
    let bytes = bytes.trim_ascii();
    if !bytes.is_empty()
        && bytes.iter().all(u8::is_ascii_digit)
        && let Some(value) = std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
    {
        return Ok(value);
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "invalid vm.mmap_min_addr").into())
}

fn temporary_special_start(minimum: u64) -> Result<u64, Error> {
    // Refuse before rounding, including overflowing inputs. Preparation uses
    // the worst-case span; C repeats this policy with its post-exec sysctl.
    if minimum > LOADER_CODE_START - MAX_SPECIAL_SIZE {
        return Err(Error::MmapMinAddrTooHigh { minimum });
    }
    Ok(page_align(minimum)?.max(TEMP_SPECIAL_START))
}

fn mdwe_inherited_on_exec() -> Result<bool, Error> {
    // SAFETY: PR_GET_MDWE takes no pointers; unused arguments must be zero.
    let flags = unsafe { libc::prctl(libc::PR_GET_MDWE, 0_u64, 0_u64, 0_u64, 0_u64) };
    if flags < 0 {
        let error = io::Error::last_os_error();
        // Kernels predating MDWE return EINVAL for the unrecognized option.
        if error.raw_os_error() == Some(libc::EINVAL) {
            return Ok(false);
        }
        return Err(error.into());
    }
    // Linux clears MDWE at exec when NO_INHERIT is set (mmf_init_flags).
    Ok(
        flags as libc::c_uint & (libc::PR_MDWE_REFUSE_EXEC_GAIN | libc::PR_MDWE_NO_INHERIT)
            == libc::PR_MDWE_REFUSE_EXEC_GAIN,
    )
}

fn u16_at(bytes: &[u8], index: usize) -> u16 {
    u16::from_le_bytes(bytes[index..index + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(bytes[index..index + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], index: usize) -> u64 {
    u64::from_le_bytes(bytes[index..index + 8].try_into().unwrap())
}
fn checked_add(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_add(b)
        .ok_or(Error::InvalidElf("address or file-offset overflow"))
}
fn check_load_range(address: u64, memsize: u64, interpreter: bool) -> Result<(), Error> {
    // Empty loads still affect ELF bias and metadata, but map no pages, even
    // at an unaligned address. Nonempty ranges include the complete last file
    // page, padzero and every anonymous BSS page after page rounding.
    if memsize != 0 && page_start(address) < MIN_PROGRAM_ADDRESS {
        return Err(Error::LowLoadSegment {
            address: page_start(address),
            interpreter,
        });
    }
    let end = page_align(checked_add(address, memsize)?)?;
    if address >= USER_LIMIT || end > USER_LIMIT {
        return Err(Error::UnsupportedElf(
            "biased PT_LOAD exceeds x86-64 default user address range",
        ));
    }
    if address >= MAP_LIMIT || end > MAP_LIMIT {
        return Err(Error::ReservedTopPage {
            address,
            interpreter,
        });
    }
    if memsize != 0 {
        check_mapping_size(page_start(address), end - page_start(address), interpreter)?;
        check_initial_stack_range(page_start(address), end - page_start(address), interpreter)?;
    }
    Ok(())
}
fn check_mapping_size(address: u64, size: u64, interpreter: bool) -> Result<(), Error> {
    if size > MAX_MAPPING_SIZE {
        return Err(Error::MappingTooLarge {
            address,
            size,
            interpreter,
        });
    }
    Ok(())
}
fn check_initial_stack_range(address: u64, size: u64, interpreter: bool) -> Result<(), Error> {
    if size != 0 && address < STACK_TOP && checked_add(address, size)? > INITIAL_STACK_LOW {
        return Err(Error::InitialStackOverlap {
            address,
            size,
            interpreter,
        });
    }
    Ok(())
}
fn page_start(value: u64) -> u64 {
    value & !(PAGE - 1)
}
fn page_align(value: u64) -> Result<u64, Error> {
    Ok(page_start(checked_add(value, PAGE - 1)?))
}

#[cfg(test)]
mod tests {
    use super::Error;
    use super::check_stack_guard_cmdline;
    use super::parse_mmap_min_addr;
    use super::temporary_special_start;

    #[test]
    fn boot_stack_guard_refusal_control() {
        for (cmdline, refused) in [
            ("", false),
            ("quiet root=/dev/example", false),
            ("quiet stack_guard_gap=256", true),
            ("stack_guard_gap=536870912 quiet", true),
            ("\"stack_guard_gap=536870912\"", true),
            ("stack_guard_gap=invalid", true),
        ] {
            let rust = check_stack_guard_cmdline(cmdline.as_bytes());
            assert_eq!(matches!(rust, Err(Error::StackGuardGapOverride)), refused);
            let c = std::process::Command::new(env!("ELF_LOADER_STACK_GUARD_CONTROL"))
                .arg(cmdline)
                .output()
                .unwrap();
            assert!(c.stdout.is_empty());
            if refused {
                assert_eq!(c.status.code(), Some(127));
                assert_eq!(
                    c.stderr,
                    b"reverie-elf-loader: stack_guard_gap boot override unsupported -22\n"
                );
            } else {
                assert!(rust.is_ok());
                assert_eq!(c.status.code(), Some(0));
                assert!(c.stderr.is_empty());
            }
            println!("boot stack guard cmdline={cmdline:?}: Rust/C named refusal={refused}");
        }
    }

    #[test]
    fn temporary_special_mapping_min_addr_controls() {
        for (minimum, expected) in [
            (0, Some(0x20000)),
            (0x10000, Some(0x20000)),
            (0x1ffff, Some(0x20000)),
            (0x20000, Some(0x20000)),
            (0x20001, Some(0x21000)),
            (0x3ffff, Some(0x40000)),
            (0x40000, Some(0x40000)),
            (0x7ffff, Some(0x80000)),
            (0x80000, Some(0x80000)),
            (0x80001, None),
            (0xfffff, None),
            (0x100000, None),
            (u64::MAX, None),
        ] {
            let input = format!("{minimum}\n");
            let parsed = parse_mmap_min_addr(input.as_bytes()).unwrap();
            assert_eq!(parsed, minimum);
            let rust = temporary_special_start(parsed);
            let c = std::process::Command::new(env!("ELF_LOADER_STACK_GUARD_CONTROL"))
                .args(["--mmap-min-addr", &input])
                .output()
                .unwrap();
            if let Some(address) = expected {
                assert_eq!(rust.unwrap(), address);
                assert!(address >= minimum);
                assert_eq!(address % super::PAGE, 0);
                assert!(address + super::MAX_SPECIAL_SIZE <= super::LOADER_CODE_START);
                assert_eq!(c.status.code(), Some(0));
                assert_eq!(c.stdout, format!("{address}\n").as_bytes());
                assert!(c.stderr.is_empty());
            } else {
                assert!(matches!(
                    rust,
                    Err(Error::MmapMinAddrTooHigh { minimum: value }) if value == minimum
                ));
                assert_eq!(c.status.code(), Some(127));
                assert!(c.stdout.is_empty());
                assert_eq!(
                    c.stderr,
                    b"reverie-elf-loader: vm.mmap_min_addr exceeds temporary special mapping band -22\n"
                );
            }
            println!("temporary specials: minimum={minimum:#x}, expected={expected:x?}");
        }
    }

    #[test]
    fn mmap_min_addr_sysctl_parsing_controls() {
        for (input, valid) in [
            ("0", true),
            (" \t262144\r\n", true),
            ("18446744073709551615\n", true),
            ("", false),
            (" \n", false),
            ("-1", false),
            ("+1", false),
            ("0x40000", false),
            ("262144 1", false),
            ("18446744073709551616", false),
            ("\u{b}262144", false),
            ("262144\u{b}", false),
        ] {
            let rust = parse_mmap_min_addr(input.as_bytes());
            let c = std::process::Command::new(env!("ELF_LOADER_STACK_GUARD_CONTROL"))
                .args(["--mmap-min-addr", input])
                .output()
                .unwrap();
            assert_eq!(rust.is_ok(), valid, "{input:?}");
            if valid {
                let minimum = rust.unwrap();
                if let Ok(address) = temporary_special_start(minimum) {
                    assert_eq!(c.status.code(), Some(0));
                    assert_eq!(c.stdout, format!("{address}\n").as_bytes());
                    assert!(c.stderr.is_empty());
                } else {
                    assert_eq!(c.status.code(), Some(127));
                    assert!(c.stdout.is_empty());
                    assert_eq!(
                        c.stderr,
                        b"reverie-elf-loader: vm.mmap_min_addr exceeds temporary special mapping band -22\n"
                    );
                }
            } else {
                assert!(matches!(
                    rust,
                    Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
                ));
                assert_eq!(c.status.code(), Some(127));
                assert!(c.stdout.is_empty());
                assert_eq!(
                    c.stderr,
                    b"reverie-elf-loader: invalid vm.mmap_min_addr -22\n"
                );
            }
        }
    }
}
