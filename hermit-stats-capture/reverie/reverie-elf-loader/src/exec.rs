/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Inactive, precommit exec preparation (launcher design LB).
//!
//! No operation in this module replaces an image. `AT_EXECVE_CHECK` supplies
//! executable-open authorization and original argument errors; the separate
//! format port below covers only checks before `begin_new_exec`. LA admission
//! remains a refusal, even when native execution would subsequently die.

use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::Error;
use crate::Invocation;
use crate::Limits;
use crate::LoaderHostFacts;
use crate::PreparedStart;
use crate::arguments::ArgumentError;
use crate::arguments::ArgumentPages;
use crate::arguments::ArgumentPlan;
use crate::arguments::MAX_ARG_STRLEN;
use crate::arguments::MAX_ARGUMENT_BUDGET;
use crate::descriptors::DescriptorError;
use crate::descriptors::DescriptorReservation;
use crate::descriptors::START_DESCRIPTOR_SLOTS;
use crate::host::CapabilityXattrEvidence;
use crate::host::HostPolicyRefusal;
use crate::host::HostQualification;
use crate::host::InterpreterAuthorizationOutcome;
use crate::host::InterpreterDenialSource;
use crate::host::classify_interpreter_denial;
use crate::host::qualify_executable_privileges;

/// Linux 6.14's authorization-only execveat flag. Never omit it in a CHECK.
pub const AT_EXECVE_CHECK: i32 = 0x10000;
const BPRM_BUFFER_SIZE: usize = 256;
const PROGRAM_READ: usize = 1;
const INTERPRETER_READ: usize = 2;
const FIRST_PATH_PIN: usize = 3;
const INTERPRETER_PIN: usize = 10;
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// An owned request, preserving the original filename, vectors and flags.
/// Invalid flags and empty paths are deliberately accepted here: the kernel
/// supplies their errno in the same order as a native execveat.
#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub dirfd: RawFd,
    pub path: CString,
    pub argv: Vec<CString>,
    pub envp: Vec<CString>,
    pub flags: i32,
}

impl ExecRequest {
    pub fn execve(
        path: impl AsRef<OsStr>,
        argv: Vec<CString>,
        envp: Vec<CString>,
    ) -> Result<Self, std::ffi::NulError> {
        Self::execveat(libc::AT_FDCWD, path, argv, envp, 0)
    }

    pub fn execveat(
        dirfd: RawFd,
        path: impl AsRef<OsStr>,
        argv: Vec<CString>,
        envp: Vec<CString>,
        flags: i32,
    ) -> Result<Self, std::ffi::NulError> {
        Ok(Self {
            dirfd,
            path: CString::new(path.as_ref().as_bytes())?,
            argv,
            envp,
            flags,
        })
    }

    fn invocation(&self) -> Result<Invocation, ExecRefusal> {
        Invocation::execveat(
            self.dirfd,
            OsStr::from_bytes(self.path.to_bytes()),
            self.flags,
        )
        .map_err(ExecRefusal::LoaderAdmission)
    }
}

/// Where a proven native error was observed. Additional launcher failures
/// never become native errors merely because they contain an errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeErrorStage {
    OriginalCheck,
    MainFormat,
    ScriptFormat,
    ScriptArguments,
    InterpreterOpen,
    InterpreterFormat,
}

/// Evidence for an E2BIG returned by CHECK, including the unclassified case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E2bigClassification {
    NativeBudget,
    BothModelsPass,
    Unclassified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeExecError {
    pub errno: i32,
    pub stage: NativeErrorStage,
    pub e2big: Option<E2bigClassification>,
}

/// Unsupported outcomes are explicitly separate from a native errno.
#[derive(Debug)]
pub enum ExecRefusal {
    ExecutableReadRequired {
        interpreter: bool,
        errno: i32,
    },
    LauncherFdCapacity {
        errno: Option<i32>,
    },
    LauncherArgumentBudget,
    PinnedAuthorizationChanged {
        errno: Option<i32>,
    },
    InterpreterDescriptorViewUnverified {
        errno: i32,
    },
    NamespaceInitUnsupported,
    SecureExecUnsupported,
    InheritedVirtualProcStateUnsupported,
    GuestExecveCheckUnsupported,
    NativeCheckUnavailable {
        errno: i32,
    },
    RequestChanged {
        operation: &'static str,
    },
    PinnedObjectChanged,
    ExecutableMetadataUnverified {
        errno: Option<i32>,
    },
    DescriptorTransaction(DescriptorError),
    Host(HostPolicyRefusal),
    LoaderAdmission(Error),
    PreparationIo {
        operation: &'static str,
        errno: Option<i32>,
    },
}

impl ExecRefusal {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ExecutableReadRequired { .. } => "ExecutableReadRequired",
            Self::LauncherFdCapacity { .. } => "LauncherFdCapacity",
            Self::LauncherArgumentBudget => "LauncherArgumentBudget",
            Self::PinnedAuthorizationChanged { .. } => "PinnedAuthorizationChanged",
            Self::InterpreterDescriptorViewUnverified { .. } => {
                "InterpreterDescriptorViewUnverified"
            }
            Self::NamespaceInitUnsupported => "NamespaceInitUnsupported",
            Self::SecureExecUnsupported => "SecureExecUnsupported",
            Self::InheritedVirtualProcStateUnsupported => "InheritedVirtualProcStateUnsupported",
            Self::GuestExecveCheckUnsupported => "GuestExecveCheckUnsupported",
            Self::NativeCheckUnavailable { .. } => "NativeCheckUnavailable",
            Self::RequestChanged { .. } => "RequestChanged",
            Self::PinnedObjectChanged => "PinnedObjectChanged",
            Self::ExecutableMetadataUnverified { .. } => "ExecutableMetadataUnverified",
            Self::DescriptorTransaction(_) => "DescriptorTransaction",
            Self::Host(policy) => policy.name(),
            Self::PreparationIo { .. } => "PreparationIo",
            Self::LoaderAdmission(error) => loader_refusal_name(error),
        }
    }
}

impl fmt::Display for ExecRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {self:?}", self.name())
    }
}

impl std::error::Error for ExecRefusal {}

impl From<HostPolicyRefusal> for ExecRefusal {
    fn from(value: HostPolicyRefusal) -> Self {
        Self::Host(value)
    }
}

/// CHECK, read-pin, and admission facts are separate, so an authorization
/// success cannot be mistaken for format acceptance.
#[derive(Clone, Debug, Default)]
pub struct PreparationEvidence {
    pub original_check: Option<i32>,
    pub pinned_checks: Vec<i32>,
    pub read_pins: Vec<i32>,
    pub script_rewrites: usize,
}

#[derive(Debug)]
pub enum ExecCheckOutcome {
    NativeErrno(NativeExecError),
    Refuse(ExecRefusal),
    Prepared(Box<PinnedStart>),
}

/// Immutable object binding, including its metadata change witnesses.
/// Inode pinning alone is not protection against changes to file contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub mount_id: u64,
    pub size: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl FileIdentity {
    pub fn of(file: &File) -> Result<Self, ExecRefusal> {
        let metadata = file.metadata().map_err(|e| preparation_io("fstat", e))?;
        let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: the pinned descriptor is live; empty-path statx performs no
        // pathname traversal and initializes the output on success.
        if unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } != 0
        {
            return Err(preparation_io(
                "pinned mount identity",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: statx succeeded.
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(ExecRefusal::ExecutableMetadataUnverified { errno: None });
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mount_id: stat.stx_mnt_id,
            size: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    pub fn verify(&self, file: &File) -> Result<(), ExecRefusal> {
        if &Self::of(file)? != self {
            return Err(ExecRefusal::PinnedObjectChanged);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ScriptImage {
    pub name: CString,
    pub identity: FileIdentity,
    pub interpreter: CString,
    pub optional_argument: Option<CString>,
}

/// Owned T/I and the inactive start plan. The freestanding LA consumer still
/// has its old fixed-FD protocol; consuming the LB manifest is LC work.
/// This value grants no authority to bypass Reverie's production exec gate.
#[derive(Debug)]
pub struct PinnedStart {
    pub program: File,
    pub interpreter: File,
    pub program_identity: FileIdentity,
    pub interpreter_identity: FileIdentity,
    pub original_invocation: Invocation,
    pub scripts: Vec<ScriptImage>,
    pub arguments: ArgumentPlan,
    pub start: PreparedStart,
    pub evidence: PreparationEvidence,
    pub reservation: Option<DescriptorReservation>,
}

impl PinnedStart {
    pub fn verify_objects(&self) -> Result<(), ExecRefusal> {
        self.program_identity.verify(&self.program)?;
        self.interpreter_identity.verify(&self.interpreter)?;
        for (index, script) in self.scripts.iter().enumerate() {
            let pin = self
                .reservation
                .as_ref()
                .and_then(|reservation| reservation.file(FIRST_PATH_PIN + index))
                .ok_or(ExecRefusal::PinnedObjectChanged)?;
            script.identity.verify(pin)?;
        }
        Ok(())
    }

    /// Temporarily clear CLOEXEC on the retained private T/I for a future
    /// manifest consumer. Explicit rollback reports every undo failure.
    /// This does not execute an image or alter the production exec gate.
    ///
    /// # Safety
    ///
    /// These files must be private launcher descriptors, including when this
    /// value came from [`prepare_start_from_files`]. The caller must serialize
    /// descriptor-table mutation for the entire transaction and treat an undo
    /// failure as a terminal preparation failure, never an acknowledged cancel.
    pub unsafe fn transfer_files(
        &self,
    ) -> Result<crate::descriptors::PrivateFileTransfer<'_>, ExecRefusal> {
        self.verify_objects()?;
        unsafe { crate::descriptors::transfer_private_files(&[&self.program, &self.interpreter]) }
            .map_err(ExecRefusal::DescriptorTransaction)
    }
}

/// Context and resource facts supplied before any target traversal.
pub struct PrepareExecOptions<'a> {
    pub launcher_link: &'a Path,
    pub host: &'a HostQualification,
    pub limits: Limits,
    pub loader_host: &'a LoaderHostFacts,
    /// Later exec with carried virtual proc/OFD or clock/counter state is not
    /// supported by this root-initial preparation building block.
    pub inherited_virtual_proc_state: bool,
    /// Optional live writable FDs proving a native interpreter ETXTBSY source.
    /// Merely observing ETXTBSY from an extra bprm CHECK is insufficient.
    pub interpreter_writer_fds: &'a [RawFd],
}

/// Prepare an owned request without executing either the target or loader.
///
/// # Safety
///
/// Run in an isolated, ordinary preparation process with serialized FD-table
/// allocation. Namespace, credentials, limits, paths and executable contents
/// must satisfy `host`'s frozen contract until the future consumer commits.
/// A real launcher caller must also preserve the design's context exclusions:
/// it must be the thread-group leader, have no installed SIGALRM handler and
/// have no foreign seccomp filters. This library does not qualify those facts.
/// Do not call from a signal handler or a post-fork multi-threaded Rust child.
/// Explicitly modeled ordinary tests exercise preparation only; a `Prepared`
/// result under modeled evidence is not qualified for runtime activation.
pub unsafe fn prepare_exec(
    request: &ExecRequest,
    options: &PrepareExecOptions<'_>,
) -> ExecCheckOutcome {
    let argv = pointer_vector(&request.argv);
    let envp = pointer_vector(&request.envp);
    // SAFETY: owned strings/vectors live through this call; the caller supplies
    // the same FD/context serialization contract required by the raw API.
    outcome(unsafe {
        prepare_raw(
            request.dirfd,
            request.path.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            request.flags,
            options,
            Some(request),
        )
    })
}

/// Raw syscall inputs, including invalid pointers used to qualify EFAULT.
/// Arbitrary user addresses are copied with process_vm_readv, never Rust
/// references. Filename lookup is qualified before CHECK. An unavailable
/// filename copy requires a kernel user-copy certificate that cannot traverse
/// a pathname; other copy failures are refused. CHECK uses original pointers.
///
/// # Safety
///
/// All readable input memory must remain unchanged for the call. The process
/// and frozen-host requirements of [`prepare_exec`] also apply. A bad address
/// is permitted and returns a typed kernel error; concurrent memory mutation
/// is outside this interface's contract.
pub unsafe fn prepare_exec_raw(
    dirfd: RawFd,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    flags: i32,
    options: &PrepareExecOptions<'_>,
) -> ExecCheckOutcome {
    outcome(unsafe { prepare_raw(dirfd, path, argv, envp, flags, options, None) })
}

fn outcome(result: Result<PinnedStart, CheckFailure>) -> ExecCheckOutcome {
    match result {
        Ok(prepared) => ExecCheckOutcome::Prepared(Box::new(prepared)),
        Err(CheckFailure::Native(error)) => ExecCheckOutcome::NativeErrno(error),
        Err(CheckFailure::Refuse(refusal)) => ExecCheckOutcome::Refuse(refusal),
    }
}

enum CheckFailure {
    Native(NativeExecError),
    Refuse(ExecRefusal),
}

impl From<ExecRefusal> for CheckFailure {
    fn from(value: ExecRefusal) -> Self {
        Self::Refuse(value)
    }
}

impl From<HostPolicyRefusal> for CheckFailure {
    fn from(value: HostPolicyRefusal) -> Self {
        Self::Refuse(value.into())
    }
}

fn native(errno: i32, stage: NativeErrorStage) -> CheckFailure {
    CheckFailure::Native(NativeExecError {
        errno,
        stage,
        e2big: (errno == libc::E2BIG).then_some(E2bigClassification::NativeBudget),
    })
}

unsafe fn prepare_raw(
    dirfd: RawFd,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    flags: i32,
    options: &PrepareExecOptions<'_>,
    owned: Option<&ExecRequest>,
) -> Result<PinnedStart, CheckFailure> {
    // Reserve before pinning or pathname classification. Native exec itself
    // needs no free user FD; EMFILE here is a launcher capacity refusal.
    let reservation =
        DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).map_err(capacity_error)?;
    let reserved_numbers: Vec<_> = reservation.private_fds().collect();
    if options.inherited_virtual_proc_state {
        return Err(ExecRefusal::InheritedVirtualProcStateUnsupported.into());
    }
    // These checks use the actual caller's PID/user namespace and IDs.
    // SAFETY: these queries take no user pointers.
    if unsafe { libc::getpid() } == 1 {
        return Err(ExecRefusal::NamespaceInitUnsupported.into());
    }
    if unsafe { libc::getuid() != libc::geteuid() || libc::getgid() != libc::getegid() } {
        return Err(ExecRefusal::SecureExecUnsupported.into());
    }
    if flags & AT_EXECVE_CHECK != 0 {
        return Err(ExecRefusal::GuestExecveCheckUnsupported.into());
    }
    // Qualification is complete before this first target lookup. It includes
    // descriptor-reachable/detached mounts and authoritative binfmt policy.
    // HostQualification's constructor has already checked its frozen evidence.
    let stack_limit = current_stack_limit()?;
    // Private FDs must not change original CHECK's lookup namespace: an
    // originally invalid low dirfd or a literal/symlinked procfd could otherwise
    // refer to a placeholder. Establish capacity first, release only our pool,
    // CHECK in the original caller context, then reacquire the same numbers
    // before the first pin. CHECK allocates no user FDs. Exclusive FD-table
    // ownership makes reacquisition deterministic; a violation is refused.
    drop(reservation);
    let path_hint = if let Some(request) = owned {
        Some(request.path.clone())
    } else {
        match read_user_string(path as usize, crate::PATH_MAX) {
            Ok(path) => Some(path),
            Err(error) if error.raw_os_error() == Some(libc::E2BIG) => {
                // The complete PATH_MAX bytes were copied without a NUL.
                // getname cannot traverse this filename. Original CHECK
                // preserves actual flag/getname precedence on this kernel.
                None
            }
            Err(error) => {
                // process_vm_readv permissions and getname's user copy differ.
                // A failed remote copy does not prove the kernel cannot walk
                // this pathname. fgetxattr imports its NAME with the same
                // strncpy_from_user primitive as getname. FD -1 and size 0
                // ensure this probe cannot access a real file or pathname.
                // Only EFAULT certifies a fault before a NUL within the name
                // bound; any other result leaves the filename unqualified.
                let probe = unsafe {
                    libc::syscall(libc::SYS_fgetxattr, -1, path, std::ptr::null_mut::<u8>(), 0)
                };
                if probe < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EFAULT) {
                    None
                } else {
                    return Err(preparation_io("qualify raw filename copy", error).into());
                }
            }
        }
    };
    if let Some(path_hint) = &path_hint
        && !path_hint.to_bytes().starts_with(b"/")
    {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let directory = dirfd == libc::AT_FDCWD
            || (unsafe { libc::fstat(dirfd, stat.as_mut_ptr()) } == 0
                && unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR);
        if directory || path_hint.to_bytes().is_empty() {
            options.host.check_lookup_fd(dirfd)?;
        }
    }
    if let Some(path_hint) = &path_hint {
        qualify_original_lookup(dirfd, path_hint, flags, options.host)?;
    }
    ensure_check_available(options.host)?;
    let check = unsafe { check_exec(dirfd, path, argv, envp, flags) };
    if check != 0 {
        let e2big = if check == libc::E2BIG {
            if let Some(request) = owned {
                Some(classify_owned_e2big(request, stack_limit))
            } else {
                classify_check_e2big(dirfd, path, argv, envp, flags, stack_limit)
            }
        } else {
            None
        };
        return Err(CheckFailure::Native(NativeExecError {
            errno: check,
            stage: NativeErrorStage::OriginalCheck,
            e2big,
        }));
    }
    // Stable owned Rust buffers need no process_vm_readv permission. Raw
    // callers still use fault-contained copies after the original CHECK.
    let request = if let Some(request) = owned {
        request.clone()
    } else {
        ExecRequest {
            dirfd,
            path: read_user_string(path as usize, crate::PATH_MAX)
                .map_err(|e| request_copy_error("copy filename", e))?,
            argv: read_user_vector(argv).map_err(|e| request_copy_error("copy argv", e))?,
            envp: read_user_vector(envp).map_err(|e| request_copy_error("copy envp", e))?,
            flags,
        }
    };
    let invocation = request.invocation()?;
    let original_f = invocation.native_execfn();
    let mut arguments = ArgumentPlan::new(&request.argv, &request.envp, &original_f, stack_limit)
        .map_err(|_| ExecRefusal::RequestChanged {
        operation: "argument model after CHECK",
    })?;
    let mut argument_pages =
        ArgumentPages::new(&request.argv, &request.envp, &original_f, stack_limit).map_err(
            |_| ExecRefusal::RequestChanged {
                operation: "argument pages after CHECK",
            },
        )?;
    let mut reservation =
        DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).map_err(capacity_error)?;
    if reservation.private_fds().collect::<Vec<_>>() != reserved_numbers {
        return Err(ExecRefusal::RequestChanged {
            operation: "private descriptor reacquisition",
        }
        .into());
    }
    let mut evidence = PreparationEvidence {
        original_check: Some(0),
        ..Default::default()
    };
    let mut scripts = Vec::new();
    let mut current_name = original_f;
    let mut current_path = request.path.clone();
    let mut current_dirfd = request.dirfd;
    let mut current_flags = request.flags;
    let mut program_pin = FIRST_PATH_PIN;
    let inaccessible = request.dirfd != libc::AT_FDCWD
        && !request.path.to_bytes().starts_with(b"/")
        && descriptor_cloexec(request.dirfd)?;
    let mut final_image = None;

    for depth in 0..=6 {
        if depth > 0 {
            qualify_interpreter_lookup(&current_path, options.host)?;
            qualify_interpreter_descriptor_view(
                &mut reservation,
                program_pin,
                &current_path,
                &request.path,
                options.host,
            )?;
        }
        if let Err(failure) = pin(
            &mut reservation,
            program_pin,
            current_dirfd,
            &current_path,
            current_flags,
            options.host,
        ) {
            return Err(match failure {
                CheckFailure::Native(error) if depth == 0 => {
                    ExecRefusal::PinnedAuthorizationChanged {
                        errno: Some(error.errno),
                    }
                    .into()
                }
                other => other,
            });
        }
        let path_file = reservation.file(program_pin).expect("owned path pin");
        let identity = FileIdentity::of(path_file)?;
        if depth > 0
            && !path_file
                .metadata()
                .map_err(|e| preparation_io("script interpreter type", e))?
                .is_file()
        {
            return Err(interpreter_denial(
                libc::EACCES,
                InterpreterDenialSource::Regularity,
            ));
        }
        classify_pinned(path_file, options.host)?;
        let empty = [std::ptr::null::<libc::c_char>()];
        let (check_argv, check_envp) = if depth == 0 {
            (argv, envp)
        } else {
            (empty.as_ptr(), empty.as_ptr())
        };
        let fd_check = unsafe {
            check_exec(
                path_file.as_raw_fd(),
                c"".as_ptr(),
                check_argv,
                check_envp,
                libc::AT_EMPTY_PATH,
            )
        };
        evidence.pinned_checks.push(fd_check);
        if fd_check != 0 {
            if depth == 0 {
                return Err(ExecRefusal::PinnedAuthorizationChanged {
                    errno: Some(fd_check),
                }
                .into());
            }
            return Err(interpreter_open_error(
                path_file,
                fd_check,
                options.interpreter_writer_fds,
                options.host,
            )?);
        }
        // The sixth script's open_exec is allowed to fail before the next
        // depth test; only the following handler iteration returns ELOOP.
        if depth > 5 {
            return Err(native(libc::ELOOP, NativeErrorStage::ScriptFormat));
        }
        reopen(
            &mut reservation,
            program_pin,
            PROGRAM_READ,
            depth != 0,
            &mut evidence,
            options.host,
        )?;
        identity.verify(reservation.file(PROGRAM_READ).expect("read pin"))?;
        verify_read_endpoint(
            reservation.file(PROGRAM_READ).expect("read pin"),
            options.host,
        )?;
        check_program_credentials(reservation.file(PROGRAM_READ).expect("read pin"))?;
        let buffer = bprm_header(reservation.file(PROGRAM_READ).expect("read pin"))?;
        options
            .host
            .check_interpreter_stage(&buffer, current_name.to_bytes())?;
        if buffer.starts_with(b"#!") {
            let (name, optional) = parse_script(&buffer)?;
            if inaccessible {
                return Err(native(libc::ENOENT, NativeErrorStage::ScriptFormat));
            }
            argument_pages
                .rewrite_script(
                    arguments.argv.first().map(|argument| argument.as_c_str()),
                    &name,
                    optional.as_deref(),
                    &current_name,
                )
                .map_err(|_| native(libc::E2BIG, NativeErrorStage::ScriptArguments))?;
            arguments
                .rewrite_script(&name, optional.as_deref(), &current_name)
                .map_err(|error| match error {
                    ArgumentError::NativeE2big => {
                        native(libc::E2BIG, NativeErrorStage::ScriptArguments)
                    }
                    ArgumentError::LauncherArgumentBudget => {
                        ExecRefusal::LauncherArgumentBudget.into()
                    }
                })?;
            scripts.push(ScriptImage {
                name: current_name.clone(),
                identity,
                interpreter: name.clone(),
                optional_argument: optional,
            });
            current_name = name.clone();
            current_path = name;
            current_dirfd = libc::AT_FDCWD;
            current_flags = 0;
            program_pin += 1;
            evidence.script_rewrites += 1;
            continue;
        }
        let interpreter_path =
            native_main_interpreter(reservation.file(PROGRAM_READ).expect("read pin"), &buffer)?;
        final_image = Some((identity, interpreter_path));
        break;
    }
    let (final_identity, interpreter_path) = final_image.expect("depth six always terminates");
    // A first PT_INTERP open precedes all LA geometry restrictions.
    let interpreter_path =
        interpreter_path.ok_or(ExecRefusal::LoaderAdmission(Error::MissingInterpreter))?;
    qualify_interpreter_lookup(&interpreter_path, options.host)?;
    qualify_interpreter_descriptor_view(
        &mut reservation,
        INTERPRETER_PIN,
        &interpreter_path,
        &request.path,
        options.host,
    )?;
    pin(
        &mut reservation,
        INTERPRETER_PIN,
        libc::AT_FDCWD,
        &interpreter_path,
        0,
        options.host,
    )
    .map_err(|failure| match failure {
        CheckFailure::Native(mut e) => {
            e.stage = NativeErrorStage::InterpreterOpen;
            CheckFailure::Native(e)
        }
        other => other,
    })?;
    let interp_pin = reservation
        .file(INTERPRETER_PIN)
        .expect("interpreter path pin");
    let interpreter_identity = FileIdentity::of(interp_pin)?;
    if !interp_pin
        .metadata()
        .map_err(|e| preparation_io("interpreter fstat", e))?
        .is_file()
    {
        return Err(interpreter_denial(
            libc::EACCES,
            InterpreterDenialSource::Regularity,
        ));
    }
    classify_pinned(interp_pin, options.host)?;
    let empty = [std::ptr::null::<libc::c_char>()];
    let interp_check = unsafe {
        check_exec(
            interp_pin.as_raw_fd(),
            c"".as_ptr(),
            empty.as_ptr(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    evidence.pinned_checks.push(interp_check);
    if interp_check != 0 {
        return Err(interpreter_open_error(
            interp_pin,
            interp_check,
            options.interpreter_writer_fds,
            options.host,
        )?);
    }
    reopen(
        &mut reservation,
        INTERPRETER_PIN,
        INTERPRETER_READ,
        true,
        &mut evidence,
        options.host,
    )?;
    interpreter_identity.verify(
        reservation
            .file(INTERPRETER_READ)
            .expect("interpreter read pin"),
    )?;
    verify_read_endpoint(
        reservation
            .file(INTERPRETER_READ)
            .expect("interpreter read pin"),
        options.host,
    )?;
    native_interpreter_format(
        reservation
            .file(INTERPRETER_READ)
            .expect("interpreter read pin"),
    )?;
    final_identity.verify(reservation.file(PROGRAM_READ).expect("program read pin"))?;
    arguments
        .launcher_budget(&[], &[])
        .map_err(|_| ExecRefusal::LauncherArgumentBudget)?;
    let program = reservation
        .take_file(PROGRAM_READ)
        .map_err(ExecRefusal::DescriptorTransaction)?;
    let interpreter = reservation
        .take_file(INTERPRETER_READ)
        .map_err(ExecRefusal::DescriptorTransaction)?;
    let start = crate::prepare_start_with_files(
        &program,
        Some(&interpreter),
        &invocation,
        options.launcher_link,
        options.limits,
        Some(options.loader_host),
    )
    .map_err(ExecRefusal::LoaderAdmission)?;
    final_identity.verify(&program)?;
    interpreter_identity.verify(&interpreter)?;
    let prepared = PinnedStart {
        program,
        interpreter,
        program_identity: final_identity,
        interpreter_identity,
        original_invocation: invocation,
        scripts,
        arguments,
        start,
        evidence,
        reservation: Some(reservation),
    };
    prepared.verify_objects()?;
    Ok(prepared)
}

/// Use files already pinned, read-authorized, and bound to the request by the
/// caller. This API performs no PT_INTERP pathname resolution or read-open.
///
/// # Safety
///
/// The caller must have established native authorization and interpreter
/// binding, immutable contents and the frozen host/context contract. Passing
/// an unrelated interpreter is not a substitute for that proof. No descriptor
/// flags are changed here and the existing runtime gate remains in force.
/// The real-launcher context exclusions of [`prepare_exec`] also apply.
pub unsafe fn prepare_start_from_files(
    target: File,
    interpreter: File,
    invocation: &Invocation,
    arguments: ArgumentPlan,
    options: &PrepareExecOptions<'_>,
) -> Result<PinnedStart, ExecRefusal> {
    if options.inherited_virtual_proc_state {
        return Err(ExecRefusal::InheritedVirtualProcStateUnsupported);
    }
    if arguments.execfn != invocation.native_execfn() {
        return Err(ExecRefusal::RequestChanged {
            operation: "pinned-file invocation",
        });
    }
    classify_pinned(&target, options.host)?;
    classify_pinned(&interpreter, options.host)?;
    let program_identity = FileIdentity::of(&target)?;
    let interpreter_identity = FileIdentity::of(&interpreter)?;
    let buffer = bprm_header(&target).map_err(check_failure_to_refusal)?;
    options
        .host
        .check_interpreter_stage(&buffer, arguments.execfn.to_bytes())?;
    native_main_interpreter(&target, &buffer).map_err(check_failure_to_refusal)?;
    native_interpreter_format(&interpreter).map_err(check_failure_to_refusal)?;
    arguments
        .launcher_budget(&[], &[])
        .map_err(|_| ExecRefusal::LauncherArgumentBudget)?;
    let start = crate::prepare_start_with_files(
        &target,
        Some(&interpreter),
        invocation,
        options.launcher_link,
        options.limits,
        Some(options.loader_host),
    )
    .map_err(ExecRefusal::LoaderAdmission)?;
    program_identity.verify(&target)?;
    interpreter_identity.verify(&interpreter)?;
    Ok(PinnedStart {
        program: target,
        interpreter,
        program_identity,
        interpreter_identity,
        original_invocation: invocation.clone(),
        scripts: Vec::new(),
        arguments,
        start,
        evidence: PreparationEvidence::default(),
        reservation: None,
    })
}

fn check_failure_to_refusal(error: CheckFailure) -> ExecRefusal {
    match error {
        CheckFailure::Refuse(refusal) => refusal,
        CheckFailure::Native(error) => {
            ExecRefusal::LoaderAdmission(Error::InvalidElf(match error.stage {
                NativeErrorStage::InterpreterFormat => "pinned interpreter precommit format",
                _ => "pinned main precommit format",
            }))
        }
    }
}

fn pin(
    reservation: &mut DescriptorReservation,
    slot: usize,
    dirfd: RawFd,
    path: &CStr,
    flags: i32,
    host: &HostQualification,
) -> Result<(), CheckFailure> {
    let open_flags = libc::O_PATH
        | libc::O_CLOEXEC
        | if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
            libc::O_NOFOLLOW
        } else {
            0
        };
    let proc_directory = if path.to_bytes().is_empty() && flags & libc::AT_EMPTY_PATH != 0 {
        Some(host.proc_fd_directory()?)
    } else {
        None
    };
    let result = unsafe {
        reservation.open_into(slot, || {
            let fd = if path.to_bytes().is_empty() && flags & libc::AT_EMPTY_PATH != 0 {
                // Reopen only this existing descriptor with O_PATH. A dup of
                // an ordinary readable fd is not an O_PATH-first pin. Flags
                // for the guest's empty pathname do not apply to this audited
                // bootstrap magic link, whose source fd stays live.
                let endpoint = CString::new(dirfd.to_string()).expect("numeric fd");
                libc::openat(
                    proc_directory.expect("qualified proc descriptor directory"),
                    endpoint.as_ptr(),
                    libc::O_PATH | libc::O_CLOEXEC,
                )
            } else {
                libc::openat(dirfd, path.as_ptr(), open_flags)
            };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(File::from_raw_fd(fd))
            }
        })
    };
    // open_into preserves the original I/O error separately from transaction
    // failures; only path lookup errors that native open_exec shares qualify.
    result.map(|_| ()).map_err(descriptor_open_failure)
}

fn reopen(
    reservation: &mut DescriptorReservation,
    path_slot: usize,
    read_slot: usize,
    interpreter: bool,
    evidence: &mut PreparationEvidence,
    host: &HostQualification,
) -> Result<(), CheckFailure> {
    let fd = reservation.file(path_slot).expect("path pin").as_raw_fd();
    let path = CString::new(fd.to_string()).expect("numeric descriptor");
    let directory = host.proc_fd_directory()?;
    let result = unsafe {
        reservation.open_into(read_slot, || {
            let opened = libc::openat(directory, path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if opened < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(File::from_raw_fd(opened))
            }
        })
    };
    match result {
        Ok(_) => {
            evidence.read_pins.push(0);
            Ok(())
        }
        Err(error) => {
            let errno = descriptor_errno(&error);
            evidence.read_pins.push(errno.unwrap_or(0));
            if matches!(error, DescriptorError::Open { ref error, .. } if error.raw_os_error() == Some(libc::EACCES))
            {
                Err(ExecRefusal::ExecutableReadRequired {
                    interpreter,
                    errno: libc::EACCES,
                }
                .into())
            } else {
                Err(ExecRefusal::DescriptorTransaction(error).into())
            }
        }
    }
}

fn classify_pinned(file: &File, host: &HostQualification) -> Result<(), ExecRefusal> {
    if !file
        .metadata()
        .map_err(|e| preparation_io("pinned regularity", e))?
        .is_file()
    {
        // Original CHECK already succeeded, so a different object/class here
        // is a binding change, rather than a borrowed native errno.
        return Err(ExecRefusal::PinnedAuthorizationChanged {
            errno: Some(libc::EACCES),
        });
    }
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: fstatfs queries an owned descriptor without pathname traversal.
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(preparation_io(
            "pinned filesystem",
            io::Error::last_os_error(),
        ));
    }
    let identity = FileIdentity::of(file)?;
    let filesystem = unsafe { filesystem.assume_init() };
    if let Err(error) = host.check_pinned_mount(identity.mount_id, filesystem.f_type) {
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 {
            // O_PATH cannot query F_GET_SEALS. An individually attested,
            // immutable memfd endpoint may classify by its exact mount/inode;
            // the readable reopen must subsequently verify the saved seals.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags >= 0 && flags & libc::O_PATH != 0 {
                return host
                    .check_pinned_memfd_path(identity.mount_id, filesystem.f_type, identity.inode)
                    .map_err(Into::into);
            }
            return Err(error.into());
        }
        host.check_pinned_memfd(identity.mount_id, filesystem.f_type, identity.inode, seals)?;
    }
    Ok(())
}

fn verify_read_endpoint(file: &File, host: &HostQualification) -> Result<(), ExecRefusal> {
    let identity = FileIdentity::of(file)?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(preparation_io(
            "readable filesystem",
            io::Error::last_os_error(),
        ));
    }
    let filesystem = unsafe { filesystem.assume_init() };
    if host
        .check_pinned_mount(identity.mount_id, filesystem.f_type)
        .is_err()
    {
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        host.check_pinned_memfd(identity.mount_id, filesystem.f_type, identity.inode, seals)?;
    }
    Ok(())
}

fn qualify_interpreter_lookup(path: &CStr, host: &HostQualification) -> Result<(), ExecRefusal> {
    let bytes = path.to_bytes();
    // Linux resolves every relative PT_INTERP and script interpreter from
    // fs->pwd, independently of the original executable's lookup base. A CWD
    // can retain a detached mount absent from the namespace snapshot.
    if !bytes.starts_with(b"/") {
        host.check_lookup_fd(libc::AT_FDCWD)?;
    }
    // Only helper-owned proc-identity endpoints have been audited. Guest
    // interpreter lookup through generic proc/sys/dev may name a new private
    // descriptor, which cannot be treated as a native guest object. Denying
    // these spellings grants no authority to any other lookup: the complete
    // frozen namespace qualification still precedes all traversal.
    if [b"/proc".as_slice(), b"/sys", b"/dev"]
        .iter()
        .any(|prefix| {
            bytes == *prefix
                || bytes
                    .strip_prefix(*prefix)
                    .is_some_and(|tail| tail.starts_with(b"/"))
        })
    {
        return Err(HostPolicyRefusal::LookupMountUnverified {
            fd: None,
            mount_id: None,
        }
        .into());
    }
    Ok(())
}

fn qualify_interpreter_descriptor_view(
    reservation: &mut DescriptorReservation,
    slot: usize,
    path: &CStr,
    original_path: &CStr,
    host: &HostQualification,
) -> Result<(), CheckFailure> {
    let (dirfd, guarded_path) = qualified_lookup_base(host, libc::AT_FDCWD, path)?;
    let result = probe_interpreter_lookup(
        reservation,
        slot,
        dirfd,
        &guarded_path,
        guarded_path.to_bytes().is_empty() && path.to_bytes().starts_with(b"/"),
    );
    if matches!(&result, Err(CheckFailure::Native(_))) {
        // A failed mount-relative probe may have skipped an earlier original
        // pathname error. Prove those prefixes before borrowing its errno.
        // Successful probes still go through pin()'s full original pathname.
        qualify_interpreter_absolute_prefix(reservation, slot, path, original_path, dirfd, host)?;
    }
    result
}

fn qualify_interpreter_absolute_prefix(
    reservation: &mut DescriptorReservation,
    slot: usize,
    path: &CStr,
    original_path: &CStr,
    selected_fd: RawFd,
    host: &HostQualification,
) -> Result<(), CheckFailure> {
    if !path.to_bytes().starts_with(b"/") {
        return Ok(()); // Linux starts relative interpreters at the actual CWD.
    }
    let unverified = || HostPolicyRefusal::LookupMountUnverified {
        fd: Some(selected_fd),
        mount_id: None,
    };
    let roots = host.retained_lookup_roots().ok_or_else(unverified)?;
    let selected = roots
        .iter()
        .find(|root| root.directory.as_raw_fd() == selected_fd)
        .ok_or_else(unverified)?;
    let mut ancestors: Vec<_> = roots
        .iter()
        .filter(|root| {
            root.mount_point == b"/"
                || root.mount_point == selected.mount_point
                || selected
                    .mount_point
                    .strip_prefix(root.mount_point.as_slice())
                    .is_some_and(|tail| tail.starts_with(b"/"))
        })
        .collect();
    ancestors.sort_by_key(|root| root.mount_point.len());
    if ancestors
        .first()
        .is_none_or(|root| root.mount_point != b"/")
    {
        return Err(unverified().into());
    }
    // A successful original CHECK already searched shared absolute prefixes.
    // The call's frozen credentials/namespace contract keeps that proof valid.
    // Relative and AT_EMPTY_PATH originals prove nothing about those ancestors.
    let original = collapse_slashes(original_path);
    let searched = if original.to_bytes().starts_with(b"/") {
        ancestors
            .iter()
            .rposition(|root| {
                root.mount_point == b"/"
                    || original
                        .to_bytes()
                        .strip_prefix(root.mount_point.as_slice())
                        .is_some_and(|tail| tail.starts_with(b"/"))
            })
            .unwrap_or(0)
    } else {
        0
    };
    for pair in ancestors[searched..].windows(2) {
        let [previous, next] = pair else {
            unreachable!("two retained mount roots")
        };
        let parent_end = next
            .mount_point
            .iter()
            .rposition(|byte| *byte == b'/')
            .expect("absolute retained mount point");
        let parent = if parent_end == 0 {
            b"/".as_slice()
        } else {
            &next.mount_point[..parent_end]
        };
        let mut relative = parent[previous.mount_point.len()..]
            .iter()
            .skip_while(|byte| **byte == b'/')
            .copied()
            .collect::<Vec<_>>();
        if !relative.is_empty() {
            relative.push(b'/');
        }
        relative.push(b'.');
        let relative = CString::new(relative).expect("retained mount point contains no NUL");
        // A retained child mount skips the original pathname's ancestors.
        // Walk each prefix first, in root-to-leaf order, without crossing a
        // mount or magic link. '/.' requires search on the terminal parent;
        // O_PATH on the parent alone would omit that check. Bootstrap's frozen
        // exact-mountpoint contract supplies the binding at each boundary.
        // An unretained intermediate mount remains a named refusal.
        probe_interpreter_lookup(
            reservation,
            slot,
            previous.directory.as_raw_fd(),
            &relative,
            false,
        )?;
    }
    Ok(())
}

fn probe_interpreter_lookup(
    reservation: &mut DescriptorReservation,
    slot: usize,
    dirfd: RawFd,
    guarded_path: &CStr,
    exact_retained_leaf: bool,
) -> Result<(), CheckFailure> {
    // nd_jump_link rejects a magic link before following it, including links
    // reached through ordinary symlinks and redundant slashes. Our private
    // reservations therefore cannot turn a guest's absent fd into a regular
    // file or directory during this lookup. An ELOOP here has ambiguous
    // provenance (a magic link or an ordinary symlink loop), so it is refused.
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS,
    };
    let result = unsafe {
        reservation.open_into(slot, || {
            let fd = if exact_retained_leaf {
                // Exact retained leaf mount roots have already-qualified
                // pathname binding. Duplicating them performs no lookup.
                libc::fcntl(dirfd, libc::F_DUPFD_CLOEXEC, 0) as libc::c_long
            } else {
                libc::syscall(
                    libc::SYS_openat2,
                    dirfd,
                    guarded_path.as_ptr(),
                    &how,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(File::from_raw_fd(fd as RawFd))
            }
        })
    };
    match result {
        Ok(_) => Ok(()),
        Err(error) if matches!(&error, DescriptorError::Open { error, .. } if error.raw_os_error() == Some(libc::EXDEV)) => {
            Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(dirfd),
                mount_id: None,
            }
            .into())
        }
        Err(error)
            if matches!(
                &error,
                DescriptorError::Open { error, .. }
                    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOSYS | libc::EINVAL))
            ) =>
        {
            Err(ExecRefusal::InterpreterDescriptorViewUnverified {
                errno: error.raw_os_error().expect("matched errno"),
            }
            .into())
        }
        Err(error) => Err(descriptor_open_failure(error)),
    }
}

fn qualified_lookup_base(
    host: &HostQualification,
    dirfd: RawFd,
    path: &CStr,
) -> Result<(RawFd, CString), ExecRefusal> {
    if path.to_bytes().starts_with(b"/") {
        // Linux skips repeated slashes. Preserve every component, especially
        // '..': lexical parent removal would be unsound across symlinks.
        let path = collapse_slashes(path);
        host.absolute_lookup_root(&path).map_err(Into::into)
    } else {
        Ok((dirfd, path.to_owned()))
    }
}

fn collapse_slashes(path: &CStr) -> CString {
    let mut result = Vec::with_capacity(path.to_bytes().len());
    for &byte in path.to_bytes() {
        if byte != b'/' || result.last() != Some(&b'/') {
            result.push(byte);
        }
    }
    CString::new(result).expect("existing CString contains no NUL")
}

fn qualify_original_lookup(
    dirfd: RawFd,
    path: &CStr,
    flags: i32,
    host: &HostQualification,
) -> Result<(), ExecRefusal> {
    if path.to_bytes().is_empty() || flags & !(libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW) != 0
    {
        // Invalid flags and empty paths are rejected by the original CHECK
        // before traversal (or select an already-existing AT_EMPTY_PATH fd).
        return Ok(());
    }
    if path.to_bytes().starts_with(b"/") && qualify_named_proc_lookup(host, path)? {
        return Ok(());
    }
    if !path.to_bytes().starts_with(b"/") && dirfd != libc::AT_FDCWD {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe { libc::fstat(dirfd, metadata.as_mut_ptr()) } != 0
            || unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFDIR
        {
            // Native lookup returns EBADF/ENOTDIR without walking a child.
            return Ok(());
        }
    }
    let (base, guarded_path) = qualified_lookup_base(host, dirfd, path)?;
    if guarded_path.to_bytes().is_empty() {
        return Ok(()); // exact retained leaf endpoint, no relative traversal
    }
    let how = OpenHow {
        flags: (libc::O_PATH
            | libc::O_CLOEXEC
            | if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
                libc::O_NOFOLLOW
            } else {
                0
            }) as u64,
        mode: 0,
        resolve: RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            base,
            guarded_path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd >= 0 {
        // The original CHECK must see the guest's unmodified descriptor view.
        drop(unsafe { File::from_raw_fd(fd as RawFd) });
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::EACCES | libc::ENAMETOOLONG | libc::EBADF) => {
            // These errors happened entirely on qualified lookup bases. Let
            // CHECK preserve its original flags/path/pointer error ordering.
            Ok(())
        }
        Some(libc::EXDEV | libc::ELOOP | libc::ENOSYS | libc::EINVAL) => {
            Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(base),
                mount_id: None,
            }
            .into())
        }
        _ => Err(preparation_io("original constrained lookup", error)),
    }
}

fn qualify_named_proc_lookup(host: &HostQualification, path: &CStr) -> Result<bool, ExecRefusal> {
    let path = collapse_slashes(path);
    let bytes = path.to_bytes();
    if bytes != b"/proc" && !bytes.starts_with(b"/proc/") {
        return Ok(false);
    }
    // Only the certified genuine /proc mounting and generated self endpoints
    // are exceptions. No lexical '..' or symlink alias earns this authority.
    let authority = host
        .evidence()
        .mounts
        .proc_namespaces()
        .iter()
        .find(|authority| {
            host.evidence().mounts.records().iter().any(|record| {
                record.mount_id == authority.mount_id && record.mount_point == b"/proc"
            })
        })
        .ok_or(HostPolicyRefusal::LookupMountUnverified {
            fd: None,
            mount_id: None,
        })?;
    if bytes == b"/proc/self/exe" || bytes.starts_with(b"/proc/self/exe/") {
        host.proc_executable()?; // certified regular leaf; suffix fails natively
        return Ok(true);
    }
    if let Some(tail) = bytes.strip_prefix(b"/proc/self/fd/") {
        let (number, suffix) = match tail.iter().position(|byte| *byte == b'/') {
            Some(index) => (&tail[..index], &tail[index + 1..]),
            None => (tail, b"".as_slice()),
        };
        if let Some(fd) = std::str::from_utf8(number)
            .ok()
            .and_then(|value| value.parse::<RawFd>().ok())
            && fd >= 0
            && fd.to_string().as_bytes() == number
        {
            if host.owns_bootstrap_fd(fd) {
                return Err(HostPolicyRefusal::LookupMountUnverified {
                    fd: Some(fd),
                    mount_id: None,
                }
                .into());
            }
            host.proc_fd_directory()?;
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                // The exact genuine proc child is absent in the original FD
                // view. CHECK returns its actual ENOENT before any allocation.
                return Ok(true);
            }
            host.check_lookup_fd(fd)?;
            if !suffix.is_empty() {
                let suffix = CString::new(suffix).expect("existing CString tail");
                qualify_original_lookup(fd, &suffix, 0, host)?;
            }
            return Ok(true);
        }
    }
    Err(HostPolicyRefusal::LookupMountUnverified {
        fd: None,
        mount_id: Some(authority.mount_id),
    }
    .into())
}

fn interpreter_open_error(
    file: &File,
    errno: i32,
    writer_fds: &[RawFd],
    host: &HostQualification,
) -> Result<CheckFailure, ExecRefusal> {
    let mut source = InterpreterDenialSource::BprmOrUnknown;
    // PT_INTERP gets native open_exec, not its own bprm hook sequence. CHECK's
    // extra policy denials are refused unless their native source is proved.
    if errno == libc::ETXTBSY
        && matches!(
            host.evidence().watches,
            crate::host::PreContentWatchEvidence::AbsentForLifetime(_)
        )
        && has_native_writer(file, writer_fds)?
    {
        source = InterpreterDenialSource::WriterHeld;
    }
    if errno == libc::EACCES {
        let mut fs = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if unsafe { libc::fstatvfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(preparation_io(
                "interpreter noexec",
                io::Error::last_os_error(),
            ));
        }
        let noexec = unsafe { fs.assume_init() }.f_flag & libc::ST_NOEXEC != 0;
        // No execute bits is a conclusive regular-file DAC denial, including
        // root's CAP_DAC_OVERRIDE case. faccessat2's MAY_ACCESS result alone is
        // not proof of native open_exec authorization or its denial source.
        let no_execute_bits = file
            .metadata()
            .map_err(|e| preparation_io("interpreter execute bits", e))?
            .mode()
            & 0o111
            == 0;
        if noexec {
            source = InterpreterDenialSource::NoexecMount;
        } else if no_execute_bits {
            source = InterpreterDenialSource::ExecuteDac;
        }
    }
    Ok(interpreter_denial(errno, source))
}

fn interpreter_denial(errno: i32, source: InterpreterDenialSource) -> CheckFailure {
    match classify_interpreter_denial(errno, source) {
        InterpreterAuthorizationOutcome::NativeErrno(errno) => {
            native(errno, NativeErrorStage::InterpreterOpen)
        }
        InterpreterAuthorizationOutcome::Refuse(refusal) => ExecRefusal::from(refusal).into(),
    }
}

fn has_native_writer(file: &File, writer_fds: &[RawFd]) -> Result<bool, ExecRefusal> {
    let target = file
        .metadata()
        .map_err(|e| preparation_io("interpreter writer identity", e))?;
    for fd in writer_fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
        if flags < 0 || flags & libc::O_PATH != 0 || flags & libc::O_ACCMODE == libc::O_RDONLY {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe { libc::fstat(*fd, stat.as_mut_ptr()) } != 0 {
            continue;
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_dev == target.dev()
            && stat.st_ino == target.ino()
            && stat.st_mode & libc::S_IFMT == libc::S_IFREG
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_program_credentials(file: &File) -> Result<(), ExecRefusal> {
    let mode = file
        .metadata()
        .map_err(|error| preparation_io("executable privilege metadata", error))?
        .mode();
    // This only queries metadata on the classified readable pin. Presence,
    // including an empty capability xattr, is enough to require refusal.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    let capabilities = if size >= 0 {
        let mut bytes = vec![0; size as usize];
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"security.capability".as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read != size {
            CapabilityXattrEvidence::Unreadable
        } else {
            let namespaced = bytes.get(..4).is_some_and(|magic| {
                u32::from_le_bytes(magic.try_into().expect("four-byte capability magic"))
                    & 0xff00_0000
                    == 0x0300_0000
            });
            CapabilityXattrEvidence::Present { bytes, namespaced }
        }
    } else if matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ENODATA | libc::EOPNOTSUPP)
    ) {
        CapabilityXattrEvidence::Absent
    } else {
        CapabilityXattrEvidence::Unreadable
    };
    qualify_executable_privileges(mode, &capabilities).map_err(Into::into)
}

fn bprm_header(file: &File) -> Result<[u8; BPRM_BUFFER_SIZE], CheckFailure> {
    let mut header = [0; BPRM_BUFFER_SIZE];
    native_single_read(file, &mut header, 0).map_err(|e| preparation_io("main header read", e))?;
    Ok(header)
}

fn native_single_read(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    // Native elf_read/prepare_binprm each make one kernel_read call. Retrying a
    // successful short read could admit an image the native path would reject.
    file.read_at(bytes, offset)
}

fn native_main_interpreter(
    file: &File,
    header: &[u8; 256],
) -> Result<Option<CString>, CheckFailure> {
    if !header.starts_with(b"\x7fELF") {
        return Err(native(libc::ENOEXEC, NativeErrorStage::MainFormat));
    }
    let kind = crate::u16_at(header, 16);
    if kind != crate::ET_EXEC && kind != crate::ET_DYN {
        return Err(native(libc::ENOEXEC, NativeErrorStage::MainFormat));
    }
    // Compat/foreign handlers require a different format port, not guesses
    // about whether this kernel has another native binfmt implementation.
    if header[4] != 2 || header[5] != 1 || crate::u16_at(header, 18) != 62 {
        return Err(ExecRefusal::LoaderAdmission(Error::UnsupportedElf(
            "native format port requires ELF64 x86-64",
        ))
        .into());
    }
    let phdrs = native_program_headers(file, header, libc::ENOEXEC, NativeErrorStage::MainFormat)?;
    if let Some(phdr) = phdrs.iter().find(|p| p.kind == crate::PT_INTERP) {
        if !(2..=crate::PATH_MAX as u64).contains(&phdr.filesz) {
            return Err(native(libc::ENOEXEC, NativeErrorStage::MainFormat));
        }
        let mut bytes = vec![0; phdr.filesz as usize];
        match native_single_read(file, &mut bytes, phdr.offset) {
            Ok(count) if count == bytes.len() => {}
            Ok(_) => return Err(native(libc::EIO, NativeErrorStage::MainFormat)),
            Err(error) => return Err(preparation_io("PT_INTERP read", error).into()),
        }
        if bytes.last() != Some(&0) {
            return Err(native(libc::ENOEXEC, NativeErrorStage::MainFormat));
        }
        // Native uses the first embedded NUL; LA later refuses this shape.
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .expect("terminated interpreter");
        return Ok(Some(CString::new(&bytes[..end]).expect("first NUL")));
    }
    Ok(None)
}

fn native_program_headers(
    file: &File,
    header: &[u8],
    errno: i32,
    stage: NativeErrorStage,
) -> Result<Vec<crate::ProgramHeader>, CheckFailure> {
    let size = usize::from(crate::u16_at(header, 56)) * 56;
    if crate::u16_at(header, 54) != 56 || size == 0 || size > 65536 {
        return Err(native(errno, stage));
    }
    let offset = crate::u64_at(header, 32);
    let mut bytes = vec![0; size];
    // Native load_elf_phdrs converts a short read to the format errno. Extra
    // preflight read errors can have a stronger policy/resource source than
    // native kernel_read(FMODE_EXEC); those remain preparation refusals.
    match native_single_read(file, &mut bytes, offset) {
        Ok(count) if count == size => Ok(bytes
            .as_chunks::<56>()
            .0
            .iter()
            .map(|p| crate::ProgramHeader::read(p))
            .collect()),
        Ok(_) => Err(native(errno, stage)),
        Err(error) => Err(preparation_io("program header read", error).into()),
    }
}

fn native_interpreter_format(file: &File) -> Result<(), CheckFailure> {
    let mut header = [0; 64];
    match native_single_read(file, &mut header, 0) {
        Ok(64) => {}
        Ok(_) => return Err(native(libc::EIO, NativeErrorStage::InterpreterFormat)),
        Err(error) => return Err(preparation_io("interpreter header read", error).into()),
    }
    if !header.starts_with(b"\x7fELF") || crate::u16_at(&header, 18) != 62 {
        return Err(native(libc::ELIBBAD, NativeErrorStage::InterpreterFormat));
    }
    // Interpreter e_type and segment geometry are checked after native commit.
    native_program_headers(
        file,
        &header,
        libc::ELIBBAD,
        NativeErrorStage::InterpreterFormat,
    )?;
    Ok(())
}

fn parse_script(buffer: &[u8; 256]) -> Result<(CString, Option<CString>), CheckFailure> {
    let space = |byte: u8| byte == b' ' || byte == b'\t';
    let end = if let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
        end
    } else {
        let start = (2..=255)
            .find(|i| !space(buffer[*i]))
            .ok_or_else(|| native(libc::ENOEXEC, NativeErrorStage::ScriptFormat))?;
        if !(start..=255).any(|i| space(buffer[i]) || buffer[i] == 0) {
            return Err(native(libc::ENOEXEC, NativeErrorStage::ScriptFormat));
        }
        255
    };
    let mut end = end;
    while end > 2 && space(buffer[end - 1]) {
        end -= 1;
    }
    let start = (2..end)
        .find(|i| !space(buffer[*i]))
        .ok_or_else(|| native(libc::ENOEXEC, NativeErrorStage::ScriptFormat))?;
    let sep = (start..end).find(|i| space(buffer[*i]) || buffer[*i] == 0);
    let name_end = sep.unwrap_or(end);
    let optional = sep
        .filter(|i| buffer[*i] != 0)
        .and_then(|sep| (sep..end).find(|i| !space(buffer[*i])))
        .map(|start| {
            let end = (start..end).find(|i| buffer[*i] == 0).unwrap_or(end);
            CString::new(&buffer[start..end]).expect("truncated at NUL")
        });
    Ok((
        CString::new(&buffer[start..name_end]).expect("terminated script name"),
        optional,
    ))
}

fn pointer_vector(strings: &[CString]) -> Vec<*const libc::c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

unsafe fn check_exec(
    dirfd: RawFd,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    flags: i32,
) -> i32 {
    // Even malformed input ALWAYS retains CHECK. On kernels lacking the flag
    // this syscall fails EINVAL; it can never accidentally execute the image.
    if unsafe {
        libc::syscall(
            libc::SYS_execveat,
            dirfd,
            path,
            argv,
            envp,
            flags | AT_EXECVE_CHECK,
        )
    } < 0
    {
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    } else {
        0
    }
}

fn ensure_check_available(host: &HostQualification) -> Result<(), ExecRefusal> {
    let empty = [std::ptr::null::<libc::c_char>()];
    let errno = unsafe {
        check_exec(
            host.proc_executable()?,
            c"".as_ptr(),
            empty.as_ptr(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if errno != 0 {
        return Err(ExecRefusal::NativeCheckUnavailable { errno });
    }
    Ok(())
}

fn current_stack_limit() -> Result<u64, ExecRefusal> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::zeroed();
    if unsafe { libc::getrlimit(libc::RLIMIT_STACK, limit.as_mut_ptr()) } != 0 {
        return Err(preparation_io("RLIMIT_STACK", io::Error::last_os_error()));
    }
    Ok(unsafe { limit.assume_init() }.rlim_cur)
}

fn descriptor_cloexec(fd: RawFd) -> Result<bool, ExecRefusal> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(ExecRefusal::PinnedAuthorizationChanged {
            errno: io::Error::last_os_error().raw_os_error(),
        });
    }
    Ok(flags & libc::FD_CLOEXEC != 0)
}

fn read_user_bytes(address: usize, bytes: &mut [u8]) -> io::Result<usize> {
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let count = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

fn read_user_string(address: usize, maximum: usize) -> io::Result<CString> {
    let mut bytes = Vec::new();
    while bytes.len() < maximum {
        let position = address
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        // Never probe the next page before discovering a NUL in this one.
        let size = (4096 - position % 4096).min(maximum - bytes.len());
        let mut chunk = vec![0; size];
        let count = read_user_bytes(position, &mut chunk)?;
        if let Some(end) = chunk[..count].iter().position(|byte| *byte == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(CString::new(bytes).expect("NUL excluded"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if count == 0 {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
    }
    Err(io::Error::from_raw_os_error(libc::E2BIG))
}

fn read_user_vector(vector: *const *const libc::c_char) -> io::Result<Vec<CString>> {
    if vector.is_null() {
        return Ok(Vec::new());
    }
    let mut strings = Vec::new();
    let mut total = 0;
    // Six MiB bounds both the number of meaningful pointers and strings.
    for index in 0..=MAX_ARGUMENT_BUDGET as usize / 8 {
        let address = (vector as usize)
            .checked_add(index * 8)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let mut pointer = [0; 8];
        if read_user_bytes(address, &mut pointer)? != 8 {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        let address = usize::from_ne_bytes(pointer);
        if address == 0 {
            return Ok(strings);
        }
        let string = read_user_string(address, MAX_ARG_STRLEN)?;
        total += string.as_bytes_with_nul().len();
        if total > MAX_ARGUMENT_BUDGET as usize {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        strings.push(string);
    }
    Err(io::Error::from_raw_os_error(libc::E2BIG))
}

fn classify_check_e2big(
    dirfd: RawFd,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    flags: i32,
    stack_limit: u64,
) -> Option<E2bigClassification> {
    let classify = || {
        let path = read_user_string(path as usize, crate::PATH_MAX).ok()?;
        let argv = read_user_vector(argv).ok()?;
        let envp = read_user_vector(envp).ok()?;
        let invocation =
            Invocation::execveat(dirfd, OsStr::from_bytes(path.to_bytes()), flags).ok()?;
        match ArgumentPlan::new(&argv, &envp, &invocation.native_execfn(), stack_limit) {
            Err(_) => Some(E2bigClassification::NativeBudget),
            Ok(plan) if plan.launcher_budget(&[], &[]).is_ok() => {
                Some(E2bigClassification::BothModelsPass)
            }
            Ok(_) => Some(E2bigClassification::Unclassified),
        }
    };
    Some(classify().unwrap_or(E2bigClassification::Unclassified))
}

fn preparation_io(operation: &'static str, error: io::Error) -> ExecRefusal {
    ExecRefusal::PreparationIo {
        operation,
        errno: error.raw_os_error(),
    }
}

fn classify_owned_e2big(request: &ExecRequest, stack_limit: u64) -> E2bigClassification {
    let Ok(invocation) = request.invocation() else {
        return E2bigClassification::Unclassified;
    };
    match ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &invocation.native_execfn(),
        stack_limit,
    ) {
        Err(_) => E2bigClassification::NativeBudget,
        Ok(plan) if plan.launcher_budget(&[], &[]).is_ok() => E2bigClassification::BothModelsPass,
        Ok(_) => E2bigClassification::Unclassified,
    }
}

fn request_copy_error(operation: &'static str, error: io::Error) -> ExecRefusal {
    if matches!(error.raw_os_error(), Some(libc::EFAULT | libc::E2BIG)) {
        ExecRefusal::RequestChanged { operation }
    } else {
        preparation_io(operation, error)
    }
}

fn capacity_error(error: DescriptorError) -> CheckFailure {
    ExecRefusal::LauncherFdCapacity {
        errno: descriptor_errno(&error),
    }
    .into()
}

fn descriptor_errno(error: &DescriptorError) -> Option<i32> {
    error.raw_os_error()
}

fn descriptor_open_failure(error: DescriptorError) -> CheckFailure {
    if let DescriptorError::Open { ref error, .. } = error
        && let Some(errno) = error.raw_os_error()
        && matches!(
            errno,
            libc::ENOENT
                | libc::ENOTDIR
                | libc::ELOOP
                | libc::EACCES
                | libc::ENAMETOOLONG
                | libc::EBADF
        )
    {
        return native(errno, NativeErrorStage::InterpreterOpen);
    }
    ExecRefusal::DescriptorTransaction(error).into()
}

fn loader_refusal_name(error: &Error) -> &'static str {
    match error {
        Error::Io(_) => "LoaderPreparationIo",
        Error::InvalidElf(_) => "InvalidElf",
        Error::UnsupportedElf(_) => "UnsupportedElf",
        Error::PaddedPathTooLong { .. } => "PaddedPathTooLong",
        Error::LauncherPathTooLong { .. } => "LauncherPathTooLong",
        Error::LowLoadSegment { .. } => "LowLoadSegment",
        Error::ReservedTopPage { .. } => "ReservedTopPage",
        Error::MappingTooLarge { .. } => "MappingTooLarge",
        Error::InterpreterHintOverlapsLoader { .. } => "InterpreterHintOverlapsLoader",
        Error::MdweExecutableBss { .. } => "MdweExecutableBss",
        Error::BssRightMerge { .. } => "BssRightMerge",
        Error::InitialStackOverlap { .. } => "InitialStackOverlap",
        Error::InterpreterEntry { .. } => "InterpreterEntry",
        Error::PrivilegedExecutable { .. } => "PrivilegedExecutable",
        Error::HugetlbElf { .. } => "HugetlbElf",
        Error::StackGuardGapOverride => "StackGuardGapOverride",
        Error::MmapMinAddrTooHigh { .. } => "MmapMinAddrTooHigh",
        Error::MissingInterpreter => "MissingInterpreter",
        Error::ZeroInterpreterLoadSpan => "ZeroInterpreterLoadSpan",
        Error::ExecutableStack => "ExecutableStack",
        Error::InvalidInvocation(_) => "InvalidInvocation",
        Error::FiniteAddressSpaceLimit { .. } => "FiniteAddressSpaceLimit",
        Error::FiniteDataLimitBelowStartupFootprint { .. } => {
            "FiniteDataLimitBelowStartupFootprint"
        }
        Error::InvalidLoaderTemplate(_) => "InvalidLoaderTemplate",
        Error::AmbiguousDescriptorComm => "AmbiguousDescriptorComm",
    }
}
