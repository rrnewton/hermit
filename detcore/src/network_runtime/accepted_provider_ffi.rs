//! Owned, single-thread adapter for the separately reviewed accepted provider.
//!
//! This module does not launch a privileged service, enroll a production
//! capability, or own any TCP/socket/pidfd descriptor. Every descriptor is
//! borrowed from the service's durable custody map. Dropping a request must not
//! drop that map. Missing observations remain failures with their raw evidence.
use std::ffi::CStr;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;


#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub provider: u64,
    pub object: u64,
    pub namespace: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawState {
    pub receive_timeout_ticks: i64,
    pub send_timeout_ticks: i64,
    pub lowat: i32,
    pub receive_buffer: i32,
    pub peek_offset: i32,
    pub socket_option_memory: i32,
    pub window_clamp: u32,
    pub userlocks: u8,
    pub scaling_ratio: u8,
    pub tcp_state: u8,
    pub child_spin_locked: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Endpoint4 {
    /// Preserve network byte order; do not serialize native-endian integers.
    pub address_be: u32,
    pub port_be: u16,
    pub family: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Creation {
    pub sequence: u64,
    pub listener: Identity,
    pub child: Identity,
    pub listener_generation: u64,
    pub mutation_epoch_enter: u64,
    pub mutation_epoch_exit: u64,
    pub overlap: u64,
    pub listener_before: RawState,
    pub listener_after: RawState,
    pub child_created: RawState,
    pub local: Endpoint4,
    pub peer: Endpoint4,
    pub cookie_at_creation: u64,
    pub phase: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandResult {
    pub command: u64,
    pub operation: u64,
    pub task: u64,
    pub start_boottime: u64,
    pub identity: Identity,
    pub creation: u64,
    pub cookie: u64,
    pub state: RawState,
    pub returned: i32,
    pub reserved: u32,
    pub phase: u64,
    /// Exact full-width original I/O count; zero for every other operation.
    pub original_count: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub fatal: u64,
    pub next_object: u64,
    pub next_creation: u64,
    pub clone_entries: u64,
    pub clone_null_returns: u64,
    pub created: u64,
    pub queued: u64,
    pub retired: u64,
    pub matched: u64,
    pub setters_entered: u64,
    pub setters_exited: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdAccept {
    pub command: u64,
    pub accept_lease: u64,
    pub owner_mm: u64,
    pub task: u64,
    pub task_start: u64,
    pub table: u64,
    pub file: u64,
    pub install_begin: u64,
    pub install_end: u64,
    pub listener: Identity,
    pub child: Identity,
    pub creation: u64,
    pub cookie: u64,
    pub phases: u64,
    pub problem: u64,
    pub requested_fd: i32,
    pub flags: i32,
    pub returned_fd: i32,
    pub do_accept_errno: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBirth {
    pub command: u64,
    pub call: u64,
    pub owner_mm: u64,
    pub provider: u64,
    pub creator_task: u64,
    pub creator_start: u64,
    pub creator_table: u64,
    pub child_task: u64,
    pub child_start: u64,
    pub child_table: u64,
    pub parent_task: u64,
    pub parent_start: u64,
    pub copy_begin: u64,
    pub copy_end: u64,
    pub pidfd_install_begin: u64,
    pub pidfd_install_end: u64,
    pub pidfd_file: u64,
    pub kernel_flags: u64,
    pub ready: u64,
    pub problem: u64,
    pub shared_mm: u32,
    pub shared_files: u32,
    pub same_thread_group: u32,
    pub exit_signal: i32,
    pub requested_exit_signal: i32,
    pub pidfd_fd: i32,
    pub clear_child_tid: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBirthEffect {
    pub command: CommandResult,
    pub birth: NativeBirth,
}

/// Finally dead creator cleanup; the original phase and return stay raw.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBirthTerminal {
    pub command: CommandResult,
    pub birth: NativeBirth,
    pub call: u64,
    pub fd_call_present: u64,
    pub task_absent: u64,
}

/// Immutable early fdget publication from this exact original connect.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OriginalSelection {
    pub command: u64,
    pub call: u64,
    pub owner_mm: u64,
    pub provider: u64,
    pub task: u64,
    pub task_start: u64,
    pub table: u64,
    pub file: u64,
    pub user_address: u64,
    pub fdput_flags: u64,
    pub ready: u64,
    pub requested_fd: i32,
    pub address_length: i32,
    /// Original byte count, without kernel clamping or signed narrowing.
    pub original_count: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OriginalResult {
    pub selection: OriginalSelection,
    pub address: [u8; 128],
    pub copy_entered: u64,
    pub copy_returned: u64,
    pub copy_remaining: u64,
    pub audit_entered: u64,
    pub audit_returned: u64,
    pub security_entered: u64,
    pub security_returned: u64,
    pub complete: u64,
    pub problem: u64,
    pub audit_result: i32,
    pub security_result: i32,
    pub returned: i32,
    pub reserved: u32,
}
impl Default for OriginalResult {
    fn default() -> Self {
        // Every field is an integer/byte array; zero is raw unknown evidence.
        unsafe { std::mem::zeroed() }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OriginalEffect {
    pub command: CommandResult,
    pub original: OriginalResult,
}
/// Exact op24 provider output, never a guest-memory snapshot.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OriginalSendCapture {
    pub provider: u64,
    pub command: u64,
    pub call: u64,
    pub task: u64,
    pub task_start: u64,
    pub returned: i64,
    pub summary: [u64; 8],
    pub bytes: [u8; 512],
}
impl Default for OriginalSendCapture {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}
const _: () = assert!(std::mem::size_of::<OriginalSendCapture>() == 624);
/// Exact physical cleanup evidence, not an observed syscall result.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OriginalTerminal {
    pub command: CommandResult,
    pub original: OriginalResult,
    pub call: u64,
    pub fd_call_present: u64,
    pub task_absent: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdEnrollment {
    pub command: u64,
    pub registration: u64,
    pub owner_mm: u64,
    pub task: u64,
    pub task_start: u64,
    pub table: u64,
    pub begin: u64,
    pub end: u64,
    pub expected_table: u64,
    pub phases: u64,
    pub problem: u64,
    pub slots: u32,
    pub files: u32,
    pub references: u32,
    pub mode: u32,
    pub ptrace_return: i32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableEnrollmentEffect {
    pub command: CommandResult,
    pub enrollment: FdEnrollment,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdEvent {
    pub sequence: u64,
    pub kind: u64,
    pub task: u64,
    pub task_start: u64,
    pub table: u64,
    pub file: u64,
    pub previous_file: u64,
    pub dependency: u64,
    pub accept_command: u64,
    pub fd: i32,
    pub returned: i32,
    pub complete: u64,
    pub mode: u32,
    pub status_flags: u32,
    pub device_major: u32,
    pub device_minor: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdStatus {
    pub problem: u64,
    pub next_table: u64,
    pub next_file: u64,
    pub next_event: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcceptedEffect {
    pub command: CommandResult,
    pub installation: FdAccept,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceId {
    /// 0 = map, 1 = program, 2 = link. Not the native runner's 1-based kind.
    pub kind: u32,
    pub id: u32,
}

// Exact Linux x86_64 ABI, also asserted by adapter-abi.c against provider.h.
const _: () = {
    use std::mem::align_of;
    use std::mem::offset_of;
    use std::mem::size_of;
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<Identity>() == 24 && align_of::<Identity>() == 8);
    assert!(size_of::<RawState>() == 40 && align_of::<RawState>() == 8);
    assert!(offset_of!(RawState, lowat) == 16);
    assert!(offset_of!(RawState, window_clamp) == 32);
    assert!(offset_of!(RawState, userlocks) == 36);
    assert!(size_of::<Endpoint4>() == 8);
    assert!(size_of::<Creation>() == 240 && align_of::<Creation>() == 8);
    assert!(offset_of!(Creation, listener) == 8);
    assert!(offset_of!(Creation, child) == 32);
    assert!(offset_of!(Creation, listener_generation) == 56);
    assert!(offset_of!(Creation, overlap) == 80);
    assert!(offset_of!(Creation, listener_before) == 88);
    assert!(offset_of!(Creation, listener_after) == 128);
    assert!(offset_of!(Creation, child_created) == 168);
    assert!(offset_of!(Creation, local) == 208);
    assert!(offset_of!(Creation, peer) == 216);
    assert!(offset_of!(Creation, cookie_at_creation) == 224);
    assert!(offset_of!(Creation, phase) == 232);
    assert!(size_of::<CommandResult>() == 136);
    assert!(offset_of!(CommandResult, identity) == 32);
    assert!(offset_of!(CommandResult, creation) == 56);
    assert!(offset_of!(CommandResult, state) == 72);
    assert!(offset_of!(CommandResult, returned) == 112);
    assert!(offset_of!(CommandResult, phase) == 120);
    assert!(offset_of!(CommandResult, original_count) == 128);
    assert!(size_of::<Status>() == 88);
    assert!(size_of::<ResourceId>() == 8);
    assert!(size_of::<FdAccept>() == 168);
    assert!(offset_of!(FdAccept, listener) == 72);
    assert!(offset_of!(FdAccept, phases) == 136);
    assert!(offset_of!(FdAccept, requested_fd) == 152);
    assert!(
        size_of::<FdEvent>() == 104
            && offset_of!(FdEvent, complete) == 80
            && offset_of!(FdEvent, mode) == 88
            && offset_of!(FdEvent, status_flags) == 92
            && offset_of!(FdEvent, device_major) == 96
            && offset_of!(FdEvent, device_minor) == 100
    );
    assert!(size_of::<FdStatus>() == 32);
    assert!(size_of::<OriginalTerminal>() == 480);
    assert!(size_of::<NativeBirthTerminal>() == 352);
    assert!(size_of::<NativeBirth>() == 192);
    assert!(offset_of!(NativeBirth, ready) == 144);
    assert!(offset_of!(NativeBirth, clear_child_tid) == 184);
    assert!(size_of::<NativeBirthEffect>() == 328);
    assert!(size_of::<OriginalSelection>() == 104);
    assert!(offset_of!(OriginalSelection, ready) == 80);
    assert!(offset_of!(OriginalSelection, original_count) == 96);
    assert!(size_of::<OriginalResult>() == 320);
    assert!(offset_of!(OriginalResult, address) == 104);
    assert!(offset_of!(OriginalResult, complete) == 288);
    assert!(size_of::<FdEnrollment>() == 112 && align_of::<FdEnrollment>() == 8);
    assert!(offset_of!(FdEnrollment, phases) == 72);
    assert!(offset_of!(FdEnrollment, slots) == 88);
    assert!(offset_of!(FdEnrollment, ptrace_return) == 104);
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallStatus {
    pub operation: &'static str,
    pub returned: i32,
    /// Captured immediately after a failed C call. Never synthesized as ENOENT.
    pub errno: Option<i32>,
}
impl CallStatus {
    fn capture(operation: &'static str, returned: i32) -> Self {
        let errno = if returned == 0 {
            None
        } else {
            io::Error::last_os_error().raw_os_error()
        };
        Self {
            operation,
            returned,
            errno,
        }
    }
    pub fn succeeded(self) -> bool {
        self.returned == 0
    }
}
#[derive(Clone, Debug)]
pub struct Observation<T> {
    pub status: CallStatus,
    /// May be zero, partial or complete on error; never a certificate by itself.
    pub raw: T,
}
#[derive(Clone, Debug)]
pub struct Inventory {
    pub status: CallStatus,
    pub ids: Vec<ResourceId>,
    pub count_invalid: bool,
}
impl Inventory {
    pub fn complete(&self) -> bool {
        self.status.succeeded()
            && !self.count_invalid
            && self
                .ids
                .iter()
                .enumerate()
                .all(|(i, id)| id.kind <= 2 && id.id != 0 && !self.ids[..i].contains(id))
    }
}
#[derive(Clone, Debug)]
pub struct CloseReceipt {
    pub incarnation: u64,
    pub inventory: Inventory,
    pub close: CallStatus,
    pub unexpected_drop: bool,
    /// Always true. A separate authorized helper must prove exact ID absence.
    pub requires_external_absence: bool,
}
#[derive(Clone, Debug, Default)]
pub struct AuditState {
    pub invalidated: bool,
    pub closed: Option<CloseReceipt>,
}
#[derive(Clone, Debug, Default)]
pub struct AuditHandle(Arc<Mutex<AuditState>>);
impl AuditHandle {
    fn record(&self, receipt: CloseReceipt) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.invalidated |=
            receipt.unexpected_drop || !receipt.inventory.complete() || !receipt.close.succeeded();
        state.closed = Some(receipt);
    }
}

// The shared object is built from the authenticated reviewed driver.c + ABI assertion
// shim, linked against libbpf.so.1. RTLD_NOW|RTLD_LOCAL, no ambient symbol search.
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn dlclose(handle: *mut c_void) -> c_int;
}
#[derive(Debug)]
pub struct LoadError(pub String);
fn loader_error() -> LoadError {
    // SAFETY: dlerror returns a thread-local, NUL-terminated diagnostic or NULL.
    let p = unsafe { dlerror() };
    if p.is_null() {
        return LoadError("dynamic loader returned no diagnostic".into());
    }
    let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
    LoadError(String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned())
}
// Read only declared version exports before any operational API is resolved.
// The same ordering is exercised by the bounded pure resolver controls below.
fn authenticate_provider_declarations(
    wire: super::ProviderWireFormat,
    topology: &super::ProviderTopology,
    mut read: impl FnMut(&CStr) -> Result<u64, LoadError>,
) -> Result<super::ProviderTopology, LoadError> {
    topology
        .validate()
        .map_err(|error| LoadError(error.to_string()))?;
    if read(c"ap_adapter_abi_version")? != wire.abi_version() {
        return Err(LoadError("provider adapter ABI version mismatch".into()));
    }
    if wire == super::ProviderWireFormat::Abi8Copy5
        && read(c"ap_adapter_copy_version")? != wire.copy_version()
    {
        return Err(LoadError("provider copy grammar version mismatch".into()));
    }
    topology
        .observe_driver(read(c"ap_provider_topology_version")?)
        .map_err(|error| LoadError(error.to_string()))
}

unsafe fn read_version_export(handle: NonNull<c_void>, name: &CStr) -> Result<u64, LoadError> {
    unsafe {
        dlerror();
    }
    let pointer = unsafe { dlsym(handle.as_ptr(), name.as_ptr()) };
    if pointer.is_null() {
        return Err(loader_error());
    }
    // These exact exports all have the reviewed u64(void) signature.
    let function =
        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> u64>(pointer) };
    Ok(unsafe { function() })
}

struct LibraryHandle(NonNull<c_void>);
impl Drop for LibraryHandle {
    fn drop(&mut self) {
        // No BPF session can outlive the Rc owning this library. This only
        // unloads code; it never confirms BPF ID absence or socket release.
        unsafe {
            dlclose(self.0.as_ptr());
        }
    }
}
type SessionPtr = *mut c_void;
struct GroupedApi {
    open: unsafe extern "C" fn(
        *const c_char,
        u64,
        *mut c_void,
        *mut c_void,
        u64,
        *mut SessionPtr,
    ) -> c_int,
    close: unsafe extern "C" fn(*mut SessionPtr, u64) -> c_int,
    close_until: unsafe extern "C" fn(*mut SessionPtr, u64, u64) -> c_int,
    close_startup:
        unsafe extern "C" fn(*mut SessionPtr, *mut c_void, *mut c_void, u64, u64) -> c_int,
}
struct Api {
    grouped: Option<GroupedApi>,
    original_read_copy_ready: unsafe extern "C" fn(SessionPtr, c_int, u64, u32) -> c_int,
    original_copy_poll_fd: unsafe extern "C" fn(SessionPtr) -> c_int,
    drain_original_copy: unsafe extern "C" fn(SessionPtr) -> c_int,
    original_read_copy_progress: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        *mut super::original_read_copy::Progress,
    ) -> c_int,
    original_read_copy_manifest:
        unsafe extern "C" fn(SessionPtr, u64, *mut super::original_read_copy::Manifest) -> c_int,
    original_read_copy_record: unsafe extern "C" fn(
        SessionPtr,
        u64,
        u64,
        *mut super::original_read_copy::RawRecord,
    ) -> c_int,
    prepare_native_birth:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, u64, c_int, *mut u64) -> c_int,
    admit_native_birth_child:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut NativeBirth) -> c_int,
    admit_native_birth_terminal: unsafe extern "C" fn(SessionPtr, u64, *mut NativeBirth) -> c_int,
    retire_dead_birth:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut NativeBirthTerminal) -> c_int,
    collect_native_birth:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult, *mut NativeBirth) -> c_int,

    prepare_original_close:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, *mut u64) -> c_int,
    prepare_original_socket:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, c_int, c_int, *mut u64) -> c_int,
    prepare_original_openat: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        u64,
        c_int,
        u64,
        c_int,
        u64,
        *mut u64,
    ) -> c_int,
    prepare_original_epoll_ctl: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        u64,
        c_int,
        c_int,
        c_int,
        u64,
        *mut u64,
    ) -> c_int,
    read_original_epoll_ctl_selection:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalResult) -> c_int,
    prepare_original_epoll:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, c_int, *mut u64) -> c_int,
    prepare_original_file:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, c_int, c_int, *mut u64) -> c_int,
    prepare_auxiliary_file:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, *mut u64) -> c_int,
    prepare_original_recvfrom: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        u64,
        c_int,
        u64,
        u64,
        c_int,
        *mut u64,
    ) -> c_int,
    prepare_original_recvmsg: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        u64,
        c_int,
        u64,
        u64,
        c_int,
        *mut u64,
    ) -> c_int,
    prepare_original_read:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, u64, u64, *mut u64) -> c_int,
    prepare_original_sendto:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, u64, u64, c_int, *mut u64) -> c_int,
    original_sendto_capture:
        unsafe extern "C" fn(SessionPtr, u64, *mut OriginalSendCapture) -> c_int,
    prepare_original_connect:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, u64, c_int, *mut u64) -> c_int,
    read_original_selection:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalSelection) -> c_int,
    collect_original_connect: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        *mut CommandResult,
        *mut OriginalResult,
    ) -> c_int,
    cancel_uninvoked_original: unsafe extern "C" fn(SessionPtr, c_int, u64) -> c_int,
    retire_dead_original:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalTerminal) -> c_int,
    cancel_uninvoked_birth: unsafe extern "C" fn(SessionPtr, c_int, u64) -> c_int,

    prepare_table_enrollment:
        unsafe extern "C" fn(SessionPtr, c_int, u64, u64, u64, *mut u64) -> c_int,
    collect_table_enrollment: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        u64,
        *mut CommandResult,
        *mut FdEnrollment,
    ) -> c_int,
    prepare_accept: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        Identity,
        u64,
        u64,
        c_int,
        c_int,
        *mut u64,
    ) -> c_int,
    collect_accept:
        unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult, *mut FdAccept) -> c_int,
    read_fd_status: unsafe extern "C" fn(SessionPtr, *mut FdStatus) -> c_int,
    read_fd_event: unsafe extern "C" fn(SessionPtr, u64, *mut FdEvent) -> c_int,
    ack_fd_event: unsafe extern "C" fn(SessionPtr, *const FdEvent) -> c_int,

    open: unsafe extern "C" fn(*const c_char, u64, *mut SessionPtr) -> c_int,
    register_task: unsafe extern "C" fn(SessionPtr, c_int) -> c_int,
    retire_auxiliary_task: unsafe extern "C" fn(SessionPtr, c_int) -> c_int,
    enroll_listener:
        unsafe extern "C" fn(SessionPtr, c_int, c_int, u64, *mut CommandResult) -> c_int,
    observe_socket_file:
        unsafe extern "C" fn(SessionPtr, c_int, c_int, *mut CommandResult) -> c_int,
    prepare_setter: unsafe extern "C" fn(
        SessionPtr,
        c_int,
        Identity,
        u64,
        u64,
        c_int,
        c_int,
        *mut u64,
    ) -> c_int,
    finish_setter: unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult) -> c_int,
    resolve_accepted: unsafe extern "C" fn(SessionPtr, c_int, c_int, *mut CommandResult) -> c_int,
    // Resolve and retain the complete required ABI even when this client uses
    // the original-command route rather than these legacy convenience calls.
    _match_accepted:
        unsafe extern "C" fn(SessionPtr, c_int, c_int, Identity, *mut CommandResult) -> c_int,
    ack_command: unsafe extern "C" fn(SessionPtr, *const CommandResult) -> c_int,
    read_creation: unsafe extern "C" fn(SessionPtr, u32, *mut Creation) -> c_int,
    read_status: unsafe extern "C" fn(SessionPtr, *mut Status) -> c_int,
    _validate_creation: unsafe extern "C" fn(*const Creation, *const Status) -> c_int,
    identifiers: unsafe extern "C" fn(SessionPtr, *mut ResourceId, u32, *mut u32) -> c_int,
    close: unsafe extern "C" fn(SessionPtr) -> c_int,
}
/// Non-Send/non-Sync by construction. Keep on the dedicated service thread.
pub struct Library {
    topology: super::ProviderTopology,
    wire_format: super::ProviderWireFormat,
    _handle: LibraryHandle,
    api: Api,
    _single_thread: std::marker::PhantomData<Rc<()>>,
}
impl Library {
    /// # Safety
    /// The caller must authenticate a trusted, immutable shared-object artifact
    /// built from the reviewed C driver/ABI shim. dlopen executes constructors;
    /// an arbitrary path is code execution, not merely data parsing. This API
    /// supplies no privilege or experiment authorization.
    pub unsafe fn load(
        path: &CStr,
        wire_format: super::ProviderWireFormat,
        expected_topology: &super::ProviderTopology,
    ) -> Result<Rc<Self>, LoadError> {
        if !path.to_bytes().starts_with(b"/") {
            return Err(LoadError(
                "absolute authenticated library path required".into(),
            ));
        }
        let raw = unsafe { dlopen(path.as_ptr(), 2) }; // RTLD_NOW; LOCAL is zero.
        let handle = LibraryHandle(NonNull::new(raw).ok_or_else(loader_error)?);
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                unsafe {
                    dlerror();
                }
                let ptr = unsafe { dlsym(handle.0.as_ptr(), concat!($name, "\0").as_ptr().cast()) };
                if ptr.is_null() {
                    return Err(loader_error());
                }
                // POSIX dlsym permits converting this exact named C symbol to
                // its declared function pointer; the caller authenticates ABI.
                unsafe { std::mem::transmute::<*mut c_void, $ty>(ptr) }
            }};
        }
        let topology =
            authenticate_provider_declarations(wire_format, expected_topology, |name| unsafe {
                read_version_export(handle.0, name)
            })?;
        // Resolve the distinct grouped ABI only after authenticated topology.
        // The legacy symbols remain unchanged for the classic owner below.
        let grouped = if matches!(&topology, super::ProviderTopology::GroupedV1 { .. }) {
            Some(GroupedApi {
                open: symbol!(
                    "ap_open_grouped",
                    unsafe extern "C" fn(
                        *const c_char,
                        u64,
                        *mut c_void,
                        *mut c_void,
                        u64,
                        *mut SessionPtr,
                    ) -> c_int
                ),
                close: symbol!(
                    "ap_close_grouped_terminal",
                    unsafe extern "C" fn(*mut SessionPtr, u64) -> c_int
                ),
                close_until: symbol!(
                    "ap_close_grouped_terminal_until",
                    unsafe extern "C" fn(*mut SessionPtr, u64, u64) -> c_int
                ),
                close_startup: symbol!(
                    "ap_close_grouped_startup_terminal",
                    unsafe extern "C" fn(
                        *mut SessionPtr,
                        *mut c_void,
                        *mut c_void,
                        u64,
                        u64,
                    ) -> c_int
                ),
            })
        } else {
            None
        };
        let api = Api {
            grouped,
            observe_socket_file: symbol!(
                "ap_observe_socket_file",
                unsafe extern "C" fn(SessionPtr, c_int, c_int, *mut CommandResult) -> c_int
            ),
            prepare_native_birth: symbol!(
                "ap_prepare_native_birth",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, u64, c_int, *mut u64) -> c_int
            ),
            admit_native_birth_child: symbol!(
                "ap_admit_native_birth_child",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut NativeBirth) -> c_int
            ),
            admit_native_birth_terminal: symbol!(
                "ap_admit_native_birth_terminal",
                unsafe extern "C" fn(SessionPtr, u64, *mut NativeBirth) -> c_int
            ),
            retire_dead_birth: symbol!(
                "ap_retire_dead_birth",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut NativeBirthTerminal) -> c_int
            ),
            collect_native_birth: symbol!(
                "ap_collect_native_birth",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    *mut CommandResult,
                    *mut NativeBirth,
                ) -> c_int
            ),

            prepare_original_socket: symbol!(
                "ap_prepare_original_socket",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    c_int,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            prepare_original_epoll_ctl: symbol!(
                "ap_prepare_original_epoll_ctl",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    c_int,
                    c_int,
                    u64,
                    *mut u64,
                ) -> c_int
            ),
            read_original_epoll_ctl_selection: symbol!(
                "ap_read_original_epoll_ctl_selection",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalResult) -> c_int
            ),
            prepare_original_epoll: symbol!(
                "ap_prepare_original_epoll",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, c_int, *mut u64) -> c_int
            ),
            prepare_original_openat: symbol!(
                "ap_prepare_original_openat",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    u64,
                    c_int,
                    u64,
                    *mut u64,
                ) -> c_int
            ),
            prepare_original_file: symbol!(
                "ap_prepare_original_file",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    c_int,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            prepare_auxiliary_file: symbol!(
                "ap_prepare_auxiliary_file",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, *mut u64) -> c_int
            ),
            prepare_original_recvfrom: symbol!(
                "ap_prepare_original_recvfrom",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            prepare_original_recvmsg: symbol!(
                "ap_prepare_original_recvmsg",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            prepare_original_read: symbol!(
                "ap_prepare_original_read",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    u64,
                    u64,
                    *mut u64,
                ) -> c_int
            ),
            prepare_original_sendto: symbol!(
                "ap_prepare_original_sendto",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, u64, u64, c_int, *mut u64) -> c_int
            ),
            original_sendto_capture: symbol!(
                "ap_original_sendto_capture",
                unsafe extern "C" fn(SessionPtr, u64, *mut OriginalSendCapture) -> c_int
            ),
            prepare_original_close: symbol!(
                "ap_prepare_original_close",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, c_int, *mut u64) -> c_int
            ),
            prepare_original_connect: symbol!(
                "ap_prepare_original_connect",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    u64,
                    c_int,
                    u64,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            read_original_selection: symbol!(
                "ap_read_original_selection",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalSelection) -> c_int
            ),
            original_read_copy_ready: symbol!(
                "ap_original_read_copy_ready",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u32) -> c_int
            ),
            original_copy_poll_fd: symbol!(
                "ap_original_copy_poll_fd",
                unsafe extern "C" fn(SessionPtr) -> c_int
            ),
            drain_original_copy: symbol!(
                "ap_drain_original_copy",
                unsafe extern "C" fn(SessionPtr) -> c_int
            ),
            original_read_copy_progress: symbol!(
                "ap_original_read_copy_progress",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    *mut super::original_read_copy::Progress,
                ) -> c_int
            ),
            original_read_copy_manifest: symbol!(
                "ap_original_read_copy_manifest",
                unsafe extern "C" fn(
                    SessionPtr,
                    u64,
                    *mut super::original_read_copy::Manifest,
                ) -> c_int
            ),
            original_read_copy_record: symbol!(
                "ap_original_read_copy_record",
                unsafe extern "C" fn(
                    SessionPtr,
                    u64,
                    u64,
                    *mut super::original_read_copy::RawRecord,
                ) -> c_int
            ),
            collect_original_connect: symbol!(
                "ap_collect_original_connect",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    *mut CommandResult,
                    *mut OriginalResult,
                ) -> c_int
            ),
            retire_dead_original: symbol!(
                "ap_retire_dead_original",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut OriginalTerminal) -> c_int
            ),
            cancel_uninvoked_birth: symbol!(
                "ap_cancel_uninvoked_birth",
                unsafe extern "C" fn(SessionPtr, c_int, u64) -> c_int
            ),
            cancel_uninvoked_original: symbol!(
                "ap_cancel_uninvoked_original",
                unsafe extern "C" fn(SessionPtr, c_int, u64) -> c_int
            ),

            prepare_table_enrollment: symbol!(
                "ap_prepare_table_enrollment",
                unsafe extern "C" fn(SessionPtr, c_int, u64, u64, u64, *mut u64) -> c_int
            ),
            collect_table_enrollment: symbol!(
                "ap_collect_table_enrollment",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    *mut CommandResult,
                    *mut FdEnrollment,
                ) -> c_int
            ),
            prepare_accept: symbol!(
                "ap_prepare_accept",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    Identity,
                    u64,
                    u64,
                    c_int,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            collect_accept: symbol!(
                "ap_collect_accept",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    u64,
                    *mut CommandResult,
                    *mut FdAccept,
                ) -> c_int
            ),
            read_fd_status: symbol!(
                "ap_read_fd_status",
                unsafe extern "C" fn(SessionPtr, *mut FdStatus) -> c_int
            ),
            read_fd_event: symbol!(
                "ap_read_fd_event",
                unsafe extern "C" fn(SessionPtr, u64, *mut FdEvent) -> c_int
            ),
            ack_fd_event: symbol!(
                "ap_ack_fd_event",
                unsafe extern "C" fn(SessionPtr, *const FdEvent) -> c_int
            ),

            open: symbol!(
                "ap_open",
                unsafe extern "C" fn(*const c_char, u64, *mut SessionPtr) -> c_int
            ),
            register_task: symbol!(
                "ap_register_task",
                unsafe extern "C" fn(SessionPtr, c_int) -> c_int
            ),
            retire_auxiliary_task: symbol!(
                "ap_retire_auxiliary_task",
                unsafe extern "C" fn(SessionPtr, c_int) -> c_int
            ),
            enroll_listener: symbol!(
                "ap_enroll_listener",
                unsafe extern "C" fn(SessionPtr, c_int, c_int, u64, *mut CommandResult) -> c_int
            ),
            prepare_setter: symbol!(
                "ap_prepare_setter",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    Identity,
                    u64,
                    u64,
                    c_int,
                    c_int,
                    *mut u64,
                ) -> c_int
            ),
            finish_setter: symbol!(
                "ap_finish_setter",
                unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult) -> c_int
            ),
            resolve_accepted: symbol!(
                "ap_resolve_accepted",
                unsafe extern "C" fn(SessionPtr, c_int, c_int, *mut CommandResult) -> c_int
            ),
            _match_accepted: symbol!(
                "ap_match_accepted",
                unsafe extern "C" fn(
                    SessionPtr,
                    c_int,
                    c_int,
                    Identity,
                    *mut CommandResult,
                ) -> c_int
            ),
            ack_command: symbol!(
                "ap_ack_command",
                unsafe extern "C" fn(SessionPtr, *const CommandResult) -> c_int
            ),
            read_creation: symbol!(
                "ap_read_creation",
                unsafe extern "C" fn(SessionPtr, u32, *mut Creation) -> c_int
            ),
            read_status: symbol!(
                "ap_read_status",
                unsafe extern "C" fn(SessionPtr, *mut Status) -> c_int
            ),
            _validate_creation: symbol!(
                "ap_validate_creation",
                unsafe extern "C" fn(*const Creation, *const Status) -> c_int
            ),
            identifiers: symbol!(
                "ap_identifiers",
                unsafe extern "C" fn(SessionPtr, *mut ResourceId, u32, *mut u32) -> c_int
            ),
            close: symbol!("ap_close", unsafe extern "C" fn(SessionPtr) -> c_int),
        };
        Ok(Rc::new(Self {
            topology,
            wire_format,
            _handle: handle,
            api,
            _single_thread: std::marker::PhantomData,
        }))
    }
    pub fn topology(&self) -> &super::ProviderTopology {
        &self.topology
    }
    pub fn wire_format(&self) -> super::ProviderWireFormat {
        self.wire_format
    }
}

pub struct OpenFailure {
    pub status: CallStatus,
    /// Partial BPF resources remain owned here, including load/attach failure.
    pub partial: Option<Session>,
    pub audit: AuditHandle,
    pub success_without_session: bool,
}
/// Owned by the outside service, independent of individual request futures.
/// C borrows every socket and task pidfd; this object owns only BPF resources.
pub struct Session {
    raw: Option<NonNull<c_void>>,
    library: Rc<Library>,
    incarnation: u64,
    ready: bool,
    audit: AuditHandle,
    inventory_capacity: std::num::NonZeroU32,
    grouped: bool,
    grouped_parts: Option<(*mut c_void, *mut c_void)>,
}
impl Session {
    pub fn open(
        library: Rc<Library>,
        object_path: &CStr,
        incarnation: u64,
        inventory_capacity: std::num::NonZeroU32,
    ) -> Result<Self, OpenFailure> {
        let mut raw = std::ptr::null_mut();
        let rc = unsafe { (library.api.open)(object_path.as_ptr(), incarnation, &mut raw) };
        let status = CallStatus::capture("ap_open", rc);
        let audit = AuditHandle::default();
        let session = NonNull::new(raw).map(|raw| Self {
            raw: Some(raw),
            library,
            incarnation,
            ready: rc == 0,
            audit: audit.clone(),
            inventory_capacity,
            grouped: false,
            grouped_parts: None,
        });
        match (rc, session) {
            (0, Some(session)) => Ok(session),
            (_, partial) => Err(OpenFailure {
                status,
                partial,
                audit,
                success_without_session: rc == 0,
            }),
        }
    }
    pub fn audit(&self) -> AuditHandle {
        self.audit.clone()
    }
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }
    pub fn is_ready(&self) -> bool {
        self.ready
    }
    fn pointer(&self) -> SessionPtr {
        self.raw.expect("session already consumed").as_ptr()
    }
    /// Authenticates the actual task referenced by the held pidfd. Duplicate
    /// registration remains the original C error; no implicit retry/reset.
    pub fn register_task(&mut self, exact_pidfd: BorrowedFd<'_>) -> CallStatus {
        CallStatus::capture("ap_register_task", unsafe {
            (self.library.api.register_task)(self.pointer(), exact_pidfd.as_raw_fd())
        })
    }
    pub fn enroll_listener(
        &mut self,
        helper_pidfd: BorrowedFd<'_>,
        socket: BorrowedFd<'_>,
        generation: u64,
    ) -> Observation<CommandResult> {
        let mut raw = CommandResult::default();
        let rc = unsafe {
            (self.library.api.enroll_listener)(
                self.pointer(),
                helper_pidfd.as_raw_fd(),
                socket.as_raw_fd(),
                generation,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_enroll_listener", rc),
            raw,
        }
    }
    /// Retire the exact idle auxiliary registration after its original command ACK.
    pub fn retire_auxiliary_task(&mut self, worker: BorrowedFd<'_>) -> CallStatus {
        let rc =
            unsafe { (self.library.api.retire_auxiliary_task)(self.pointer(), worker.as_raw_fd()) };
        CallStatus::capture("ap_retire_auxiliary_task", rc)
    }
    pub fn observe_socket_file(
        &mut self,
        helper: BorrowedFd<'_>,
        socket: BorrowedFd<'_>,
    ) -> Observation<CommandResult> {
        let mut raw = CommandResult::default();
        let rc = unsafe {
            (self.library.api.observe_socket_file)(
                self.pointer(),
                helper.as_raw_fd(),
                socket.as_raw_fd(),
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_observe_socket_file", rc),
            raw,
        }
    }
    pub fn prepare_setter(
        &mut self,
        guest_task_pidfd: BorrowedFd<'_>,
        identity: Identity,
        before: u64,
        after: u64,
        level: i32,
        option: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_setter)(
                self.pointer(),
                guest_task_pidfd.as_raw_fd(),
                identity,
                before,
                after,
                level,
                option,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_setter", rc),
            raw,
        }
    }
    pub fn finish_setter(
        &mut self,
        guest_task_pidfd: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<CommandResult> {
        let mut raw = CommandResult::default();
        let rc = unsafe {
            (self.library.api.finish_setter)(
                self.pointer(),
                guest_task_pidfd.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_finish_setter", rc),
            raw,
        }
    }
    pub fn prepare_native_birth(
        &mut self,
        pidfd: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        table: u64,
        syscall: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_native_birth)(
                self.pointer(),
                pidfd.as_raw_fd(),
                call,
                mm,
                table,
                syscall,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_native_birth", rc),
            raw,
        }
    }
    pub fn admit_native_birth_terminal(&mut self, command: u64) -> Observation<NativeBirth> {
        let mut raw = NativeBirth::default();
        let rc = unsafe {
            (self.library.api.admit_native_birth_terminal)(self.pointer(), command, &mut raw)
        };
        Observation {
            status: CallStatus::capture("ap_admit_native_birth_terminal", rc),
            raw,
        }
    }
    pub fn admit_native_birth_child(
        &mut self,
        pidfd: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<NativeBirth> {
        let mut raw = NativeBirth::default();
        let rc = unsafe {
            (self.library.api.admit_native_birth_child)(
                self.pointer(),
                pidfd.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_admit_native_birth_child", rc),
            raw,
        }
    }
    /// Borrows the original preparation's held PIDFD_THREAD. A successful
    /// result is physical cleanup, never a fabricated syscall completion.
    pub fn retire_dead_birth(
        &mut self,
        pidfd: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<NativeBirthTerminal> {
        let mut raw = NativeBirthTerminal::default();
        let rc = unsafe {
            (self.library.api.retire_dead_birth)(
                self.pointer(),
                pidfd.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_retire_dead_birth", rc),
            raw,
        }
    }
    pub fn collect_native_birth(
        &mut self,
        pidfd: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<NativeBirthEffect> {
        let mut raw = NativeBirthEffect::default();
        let rc = unsafe {
            (self.library.api.collect_native_birth)(
                self.pointer(),
                pidfd.as_raw_fd(),
                command,
                &mut raw.command,
                &mut raw.birth,
            )
        };
        Observation {
            status: CallStatus::capture("ap_collect_native_birth", rc),
            raw,
        }
    }

    pub fn prepare_original_connect(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        fd: i32,
        address: u64,
        length: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_connect)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                address,
                length,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_connect", rc),
            raw,
        }
    }
    pub fn prepare_original_close(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        fd: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_close)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_close", rc),
            raw,
        }
    }
    /// Arm the existing original Call for Socket's actual install/no-install result.
    pub fn prepare_original_socket(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        domain: i32,
        socket_type: i32,
        protocol: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_socket)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                domain,
                socket_type,
                protocol,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_socket", rc),
            raw,
        }
    }
    /// Select the exact original legacy/create1 syscall. Invalid size/flags
    /// still execute in Linux and require its actual negative completion.
    /// Native prerequisite only: admission still needs the same Call's
    /// unresolved logical selection reservation. No guest event reread.
    pub fn prepare_original_epoll_ctl(
        &mut self,
        target: BorrowedFd<'_>,
        (call, mm): (u64, u64),
        epfd: i32,
        operation: i32,
        target_fd: i32,
        event_address: u64,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_epoll_ctl)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                epfd,
                operation,
                target_fd,
                event_address,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_epoll_ctl", rc),
            raw,
        }
    }
    pub fn read_original_epoll_ctl_selection(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<OriginalResult> {
        let mut raw = OriginalResult::default();
        let rc = unsafe {
            (self.library.api.read_original_epoll_ctl_selection)(
                self.pointer(),
                target.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_read_original_epoll_ctl_selection", rc),
            raw,
        }
    }
    pub fn prepare_original_epoll(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        syscall_nr: i32,
        argument: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_epoll)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                syscall_nr,
                argument,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_epoll", rc),
            raw,
        }
    }
    /// Original Openat keeps its pathname and mode registers intact. Linux
    /// interprets the low int bits of dirfd/flags; no private syscall is issued.
    pub fn prepare_original_openat(
        &mut self,
        target: BorrowedFd<'_>,
        (call, mm): (u64, u64),
        dirfd: i32,
        pathname: u64,
        flags: i32,
        mode: u64,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_openat)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                dirfd,
                pathname,
                flags,
                mode,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_openat", rc),
            raw,
        }
    }
    pub fn prepare_original_file(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        fd: i32,
        syscall: i32,
        command: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_file)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                syscall,
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_file", rc),
            raw,
        }
    }
    /// Prepare the held Openat file's F_GETFL on the actual auxiliary worker.
    /// This does not enroll the worker's descriptor table as a guest table.
    pub fn prepare_auxiliary_file(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        fd: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_auxiliary_file)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_auxiliary_file", rc),
            raw,
        }
    }
    /// Prepare exactly the worker's native receive. Its PIDFD is not the guest PIDFD.
    pub fn prepare_helper_receive(
        &mut self,
        target: BorrowedFd<'_>,
        (call, mm): (u64, u64),
        fd: i32,
        address: u64,
        count: u64,
        peek: bool,
    ) -> Observation<u64> {
        let mut raw = 0;
        let (function, name, flags) = if peek {
            (
                self.library.api.prepare_original_recvmsg,
                "ap_prepare_original_recvmsg",
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        } else {
            (
                self.library.api.prepare_original_recvfrom,
                "ap_prepare_original_recvfrom",
                libc::MSG_DONTWAIT,
            )
        };
        let rc = unsafe {
            function(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                address,
                count,
                flags,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture(name, rc),
            raw,
        }
    }

    /// Prepare one original scalar Read; neither buffer nor count is reread or clipped.
    pub fn prepare_original_read(
        &mut self,
        target: BorrowedFd<'_>,
        call: u64,
        mm: u64,
        fd: i32,
        buffer: u64,
        count: u64,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_original_read)(
                self.pointer(),
                target.as_raw_fd(),
                call,
                mm,
                fd,
                buffer,
                count,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_original_read", rc),
            raw,
        }
    }

    pub fn prepare_original_sendto(
        &mut self, target: BorrowedFd<'_>, call: u64, mm: u64,
        operands: (i32, u64, u64, i32),
    ) -> Observation<u64> {
        let (fd, buffer, count, flags) = operands;
        let mut raw = 0;
        let rc = unsafe { (self.library.api.prepare_original_sendto)(
            self.pointer(), target.as_raw_fd(), call, mm, fd, buffer, count, flags, &mut raw
        ) };
        Observation { status: CallStatus::capture("ap_prepare_original_sendto", rc), raw }
    }

    pub(super) fn original_sendto_capture(&mut self, command: u64) -> io::Result<OriginalSendCapture> {
        let mut raw = OriginalSendCapture::default();
        let rc = unsafe { (self.library.api.original_sendto_capture)(self.pointer(), command, &mut raw) };
        if rc != 0 { return Err(io::Error::last_os_error()); }
        Ok(raw)
    }

    pub fn cancel_uninvoked_birth(&mut self, pin: BorrowedFd<'_>, command: u64) -> CallStatus {
        let rc = unsafe {
            (self.library.api.cancel_uninvoked_birth)(self.pointer(), pin.as_raw_fd(), command)
        };
        CallStatus::capture("ap_cancel_uninvoked_birth", rc)
    }
    pub fn cancel_uninvoked_original(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> CallStatus {
        let rc = unsafe {
            (self.library.api.cancel_uninvoked_original)(
                self.pointer(),
                target.as_raw_fd(),
                command,
            )
        };
        CallStatus::capture("ap_cancel_uninvoked_original", rc)
    }
    pub fn retire_dead_original(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<OriginalTerminal> {
        let mut raw = OriginalTerminal::default();
        let rc = unsafe {
            (self.library.api.retire_dead_original)(
                self.pointer(),
                target.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_retire_dead_original", rc),
            raw,
        }
    }
    pub fn read_original_selection(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<OriginalSelection> {
        let mut raw = OriginalSelection::default();
        let rc = unsafe {
            (self.library.api.read_original_selection)(
                self.pointer(),
                target.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_read_original_selection", rc),
            raw,
        }
    }
    pub(super) fn original_copy_poll_fd(&self) -> io::Result<c_int> {
        let fd = unsafe { (self.library.api.original_copy_poll_fd)(self.pointer()) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }
    pub(super) fn drain_original_copy(&mut self) -> io::Result<()> {
        let rc = unsafe { (self.library.api.drain_original_copy)(self.pointer()) };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(super) fn original_read_copy_ready(
        &mut self,
        pin: BorrowedFd<'_>,
        command: u64,
        terminal: bool,
    ) -> io::Result<bool> {
        match unsafe {
            (self.library.api.original_read_copy_ready)(
                self.pointer(),
                pin.as_raw_fd(),
                command,
                u32::from(terminal),
            )
        } {
            0 => Ok(false),
            1 => Ok(true),
            -1 => Err(io::Error::last_os_error()),
            _ => Err(io::Error::other("invalid Read copy readiness result")),
        }
    }
    pub(super) fn original_read_copy_progress(
        &mut self,
        pin: BorrowedFd<'_>,
        command: u64,
    ) -> io::Result<super::original_read_copy::Progress> {
        let mut raw = super::original_read_copy::Progress::default();
        let rc = unsafe {
            (self.library.api.original_read_copy_progress)(
                self.pointer(),
                pin.as_raw_fd(),
                command,
                &mut raw,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if raw.exited > 1
            || raw.terminal > 1
            || raw.protocol > 1
            || raw.protocol == 1 && raw.exited != 1
        {
            return Err(io::Error::other(
                "invalid original Read EXIT progress marker",
            ));
        }
        Ok(raw)
    }
    pub(super) fn original_read_copy_manifest(
        &mut self,
        command: u64,
    ) -> io::Result<super::original_read_copy::Manifest> {
        let mut raw = super::original_read_copy::Manifest::default();
        let rc = unsafe {
            (self.library.api.original_read_copy_manifest)(self.pointer(), command, &mut raw)
        };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(raw)
        }
    }
    pub(super) fn original_read_copy_record(
        &mut self,
        command: u64,
        index: u64,
    ) -> io::Result<super::original_read_copy::Record> {
        let mut raw = super::original_read_copy::RawRecord::default();
        let rc = unsafe {
            (self.library.api.original_read_copy_record)(self.pointer(), command, index, &mut raw)
        };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(raw.into())
        }
    }
    pub fn collect_original_connect(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<OriginalEffect> {
        let mut raw = OriginalEffect::default();
        let rc = unsafe {
            (self.library.api.collect_original_connect)(
                self.pointer(),
                target.as_raw_fd(),
                command,
                &mut raw.command,
                &mut raw.original,
            )
        };
        Observation {
            status: CallStatus::capture("ap_collect_original_connect", rc),
            raw,
        }
    }
    pub fn prepare_accept(
        &mut self,
        guest_task_pidfd: BorrowedFd<'_>,
        listener: Identity,
        lease: u64,
        mm: u64,
        fd: i32,
        flags: i32,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_accept)(
                self.pointer(),
                guest_task_pidfd.as_raw_fd(),
                listener,
                lease,
                mm,
                fd,
                flags,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_accept", rc),
            raw,
        }
    }
    /// Reads the originally submitted effect. Failure preserves both partial
    /// outputs and never submits another command or infers an installation.
    pub fn collect_accept(
        &mut self,
        guest_task_pidfd: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<AcceptedEffect> {
        let mut raw = AcceptedEffect::default();
        let rc = unsafe {
            (self.library.api.collect_accept)(
                self.pointer(),
                guest_task_pidfd.as_raw_fd(),
                command,
                &mut raw.command,
                &mut raw.installation,
            )
        };
        Observation {
            status: CallStatus::capture("ap_collect_accept", rc),
            raw,
        }
    }
    pub fn prepare_table_enrollment(
        &mut self,
        target: BorrowedFd<'_>,
        registration: u64,
        mm: u64,
        expected_table: u64,
    ) -> Observation<u64> {
        let mut raw = 0;
        let rc = unsafe {
            (self.library.api.prepare_table_enrollment)(
                self.pointer(),
                target.as_raw_fd(),
                registration,
                mm,
                expected_table,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_prepare_table_enrollment", rc),
            raw,
        }
    }
    pub fn collect_table_enrollment(
        &mut self,
        target: BorrowedFd<'_>,
        command: u64,
    ) -> Observation<TableEnrollmentEffect> {
        let mut raw = TableEnrollmentEffect::default();
        let rc = unsafe {
            (self.library.api.collect_table_enrollment)(
                self.pointer(),
                target.as_raw_fd(),
                command,
                &mut raw.command,
                &mut raw.enrollment,
            )
        };
        Observation {
            status: CallStatus::capture("ap_collect_table_enrollment", rc),
            raw,
        }
    }
    pub fn read_fd_status(&mut self) -> Observation<FdStatus> {
        let mut raw = FdStatus::default();
        let rc = unsafe { (self.library.api.read_fd_status)(self.pointer(), &mut raw) };
        Observation {
            status: CallStatus::capture("ap_read_fd_status", rc),
            raw,
        }
    }
    pub fn read_fd_event(&mut self, sequence: u64) -> Observation<FdEvent> {
        let mut raw = FdEvent::default();
        let rc = unsafe { (self.library.api.read_fd_event)(self.pointer(), sequence, &mut raw) };
        Observation {
            status: CallStatus::capture("ap_read_fd_event", rc),
            raw,
        }
    }
    pub fn ack_fd_event(&mut self, receipt: &FdEvent) -> CallStatus {
        CallStatus::capture("ap_ack_fd_event", unsafe {
            (self.library.api.ack_fd_event)(self.pointer(), receipt)
        })
    }
    pub fn resolve_accepted(
        &mut self,
        helper_pidfd: BorrowedFd<'_>,
        socket: BorrowedFd<'_>,
    ) -> Observation<CommandResult> {
        let mut raw = CommandResult::default();
        let rc = unsafe {
            (self.library.api.resolve_accepted)(
                self.pointer(),
                helper_pidfd.as_raw_fd(),
                socket.as_raw_fd(),
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_resolve_accepted", rc),
            raw,
        }
    }
    /// The durable service inbox must already own this complete observation.
    /// A failed/repeated ACK is retained as its actual status, never inferred to
    /// mean a prior attempt succeeded or permission to submit a new command.
    pub fn ack_command(&mut self, receipt: &CommandResult) -> CallStatus {
        CallStatus::capture("ap_ack_command", unsafe {
            (self.library.api.ack_command)(self.pointer(), receipt)
        })
    }
    pub fn read_creation(&mut self, sequence: u32) -> Observation<Creation> {
        let mut raw = Creation::default();
        let rc = unsafe { (self.library.api.read_creation)(self.pointer(), sequence, &mut raw) };
        Observation {
            status: CallStatus::capture("ap_read_creation", rc),
            raw,
        }
    }
    pub fn read_status(&mut self) -> Observation<Status> {
        let mut raw = Status::default();
        let rc = unsafe { (self.library.api.read_status)(self.pointer(), &mut raw) };
        Observation {
            status: CallStatus::capture("ap_read_status", rc),
            raw,
        }
    }
    pub fn wire_format(&self) -> super::ProviderWireFormat {
        self.library.wire_format()
    }

    pub fn identifiers(&mut self) -> Inventory {
        let capacity = self.inventory_capacity.get() as usize;
        let mut ids = vec![ResourceId::default(); capacity];
        let mut written = 0;
        let rc = unsafe {
            (self.library.api.identifiers)(
                self.pointer(),
                ids.as_mut_ptr(),
                self.inventory_capacity.get(),
                &mut written,
            )
        };
        let status = CallStatus::capture("ap_identifiers", rc);
        Inventory {
            status,
            ids: ids[..(written as usize).min(capacity)].to_vec(),
            count_invalid: written as usize > capacity,
        }
    }
    fn close_inner(&mut self, unexpected_drop: bool) -> CloseReceipt {
        assert!(!self.grouped, "grouped Session cannot use classic close");
        let inventory = self.identifiers();
        let raw = self.raw.take().expect("close exactly once");
        let rc = unsafe { (self.library.api.close)(raw.as_ptr()) };
        let close = CallStatus::capture("ap_close", rc);
        self.ready = false;
        CloseReceipt {
            incarnation: self.incarnation,
            inventory,
            close,
            unexpected_drop,
            requires_external_absence: true,
        }
    }
    pub fn close(mut self) -> CloseReceipt {
        let receipt = self.close_inner(false);
        self.audit.record(receipt.clone());
        receipt
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.raw.is_some() && self.grouped {
            // Grouped resources require the retained controller/runtime owner.
            // Never invoke classic close or synthesize a native CallStatus in
            // Drop. Keep the code mapping alive through eventual process exit.
            self.audit
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .invalidated = true;
            std::mem::forget(self.library.clone());
            return;
        }
        if self.raw.is_some() {
            // A discarded service session is invalidation, never completion.
            // This owns only BPF links/maps, not possibly-final TCP references.
            // The service must retain audit() outside its cancelable waiter.
            let receipt = self.close_inner(true);
            self.audit.record(receipt);
        }
    }
}

#[cfg(test)]
mod topology_tests {
    use super::*;

    #[test]
    fn loader_authenticates_topology_before_any_operational_symbol() {
        use crate::network_runtime::ProviderTopology;
        use crate::network_runtime::ProviderWireFormat;
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for expected in [
                ProviderTopology::ClassicV40,
                ProviderTopology::GroupedV1 {
                    contract_sha256: [9; 32],
                },
                ProviderTopology::FtraceV1 {
                    contract_sha256: [8; 32],
                },
            ] {
                for actual in [Some(0), Some(1), Some(2), Some(u64::MAX), None] {
                    let mut names = Vec::new();
                    let result = authenticate_provider_declarations(wire, &expected, |name| {
                        names.push(name.to_owned());
                        match name.to_bytes() {
                            b"ap_adapter_abi_version" => Ok(wire.abi_version()),
                            b"ap_adapter_copy_version" => Ok(wire.copy_version()),
                            b"ap_provider_topology_version" => {
                                actual.ok_or_else(|| LoadError("missing topology export".into()))
                            }
                            _ => panic!(
                                "operational symbol resolved during declaration authentication"
                            ),
                        }
                    });
                    assert_eq!(result.is_ok(), actual == Some(expected.driver_version()));
                    if let Ok(observed) = result {
                        assert_eq!(observed, expected);
                    }
                    let mut expected_names = vec![c"ap_adapter_abi_version".to_owned()];
                    if wire == ProviderWireFormat::Abi8Copy5 {
                        expected_names.push(c"ap_adapter_copy_version".to_owned());
                    }
                    expected_names.push(c"ap_provider_topology_version".to_owned());
                    assert_eq!(names, expected_names);
                }
            }
        }
        let grouped = ProviderTopology::GroupedV1 {
            contract_sha256: [0; 32],
        };
        assert!(
            authenticate_provider_declarations(
                ProviderWireFormat::Abi7Copy4,
                &grouped,
                |_| panic!("invalid expected metadata must fail before exports")
            )
            .is_err()
        );
        let mut queried = Vec::new();
        assert!(
            authenticate_provider_declarations(
                ProviderWireFormat::Abi7Copy4,
                &ProviderTopology::ClassicV40,
                |name| {
                    queried.push(name.to_owned());
                    Ok(0)
                }
            )
            .is_err()
        );
        assert_eq!(queried, vec![c"ap_adapter_abi_version".to_owned()]);
    }
}

/// The native close result is distinct from global absence and process/FD
/// restoration. A NULL open produced no inventory call; it is not empty proof.
#[derive(Clone, Debug)]
pub(super) struct GroupedCloseReceipt {
    pub incarnation: u64,
    pub inventory: Option<Inventory>,
    pub close: CallStatus,
    pub original_release_start: u64,
    pub cutoff: u64,
    pub native_pointer_retained: bool,
    pub runtime_completed: bool,
    pub requires_external_absence: bool,
}

struct GroupedCustody {
    bridge: super::grouped_broker::Bridge,
    library: Option<Rc<Library>>,
    run_controller_pidfd: OwnedFd,
    broker_endpoint: OwnedFd,
    runtime: Option<super::grouped_broker::RuntimeCleanup>,
    incarnation: Option<u64>,
    startup_deadline: Option<u64>,
    runtime_install_attempted: bool,
    open_attempted: bool,
    open_status: Option<CallStatus>,
    parts: Option<(*mut c_void, *mut c_void)>,
    session: Option<Session>,
    failed_open: Option<OpenFailure>,
    audit: AuditHandle,
    first_failure: Option<String>,
    failure_origin: Option<u64>,
    terminal_started: bool,
    terminal_origin: Option<(u64, u64)>,
    closed: Option<GroupedCloseReceipt>,
}

/// Installed before adoption or open. No Err carries a by-value owner that a
/// caller could accidentally drop. The retained object includes raw-NULL open
/// failure and both original channels, not just successful Session pointers.
///
/// RuntimeCleanup is the private actual handoff owner, not a marker declared
/// here. Until that module implements and completes its native transfer/join,
/// there is no constructor/boolean/status route to install or use this owner.
pub(super) struct GroupedSessionOwner {
    custody: std::mem::ManuallyDrop<GroupedCustody>,
}
pub(super) type GroupedBootstrapOwner = GroupedSessionOwner;
impl GroupedSessionOwner {
    pub(super) fn retain_pending(
        bridge: super::grouped_broker::Bridge,
        run_controller_pidfd: OwnedFd,
        broker_endpoint: OwnedFd,
    ) -> Self {
        Self {
            custody: std::mem::ManuallyDrop::new(GroupedCustody {
                bridge,
                library: None,
                run_controller_pidfd,
                broker_endpoint,
                runtime: None,
                incarnation: None,
                startup_deadline: None,
                runtime_install_attempted: false,
                open_attempted: false,
                open_status: None,
                parts: None,
                session: None,
                failed_open: None,
                audit: AuditHandle::default(),
                first_failure: None,
                failure_origin: None,
                terminal_started: false,
                terminal_origin: None,
                closed: None,
            }),
        }
    }
    pub(super) fn retain_bootstrap(
        bridge: super::grouped_broker::Bridge,
        run_controller_pidfd: OwnedFd,
        broker_endpoint: OwnedFd,
        runtime: super::grouped_broker::RuntimeCleanup,
    ) -> Self {
        let mut owner = Self::retain_pending(bridge, run_controller_pidfd, broker_endpoint);
        owner.custody.runtime = Some(runtime);
        owner
    }
    pub(super) fn attach_library(&mut self, library: Rc<Library>) -> io::Result<()> {
        let result = (|| {
            let c = &mut *self.custody;
            if c.library.is_some() || c.open_attempted || c.first_failure.is_some() {
                return Err(io::Error::other(
                    "grouped library attachment is one-use before open",
                ));
            }
            // Retain the mapping before the fallible authenticated-ABI check.
            c.library = Some(library);
            if c.library.as_ref().unwrap().api.grouped.is_none() {
                return Err(io::Error::other(
                    "actual grouped owner requires the grouped library API",
                ));
            }
            Ok(())
        })();
        self.remember(result)
    }
    pub(super) fn retain_failure(&mut self, error: &io::Error) {
        let c = &mut *self.custody;
        if c.first_failure.is_none() {
            // Capture the first observed refusal here, never later at cleanup.
            // A clock failure remains an unknown origin, not a fresh budget.
            if c.failure_origin.is_none() {
                c.failure_origin = grouped_monotonic_ns().ok();
            }
            c.first_failure = Some(error.to_string());
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.retain_failure(error);
        }
        result
    }
    /// # Safety
    /// The existing package owner authenticated the sealed bridge and complete
    /// loader closure. Custody has already been installed, including on Err.
    pub(super) unsafe fn initialize_bridge(&mut self) -> io::Result<()> {
        let result = unsafe { self.custody.bridge.initialize() };
        self.remember(result)
    }
    pub(super) fn adopt(
        &mut self,
        incarnation: u64,
        nonce: &CStr,
        deadline: u64,
        creator_cutoff: u64,
        unit: &CStr,
        leaves: [BorrowedFd<'_>; 3],
    ) -> io::Result<()> {
        let result = (|| {
            let c = &mut *self.custody;
            if c.incarnation.is_some() || c.first_failure.is_some() {
                return Err(io::Error::other(
                    "grouped adoption owner is absent, used or refused",
                ));
            }
            c.incarnation = Some(incarnation);
            c.startup_deadline = Some(deadline);
            c.bridge.successor(
                c.broker_endpoint.as_fd(),
                incarnation,
                nonce,
                (deadline, creator_cutoff),
                unit,
                leaves,
            )
        })();
        self.remember(result)
    }
    /// Move and retain the actual run-lifetime cleanup owner BEFORE opening
    /// any BPF resource. Its implementation must linearly transfer the live
    /// journal/cursor context and join startup actors; a LEAVES status cannot
    /// satisfy this call. Precondition refusal leaves the input slot untouched;
    /// after transfer starts the owner stays here on every partial failure.
    pub(super) fn install_retained_runtime_cleanup(&mut self) -> io::Result<()> {
        let result = (|| {
            let c = &mut *self.custody;
            if c.runtime_install_attempted || c.open_attempted || c.first_failure.is_some() {
                return Err(io::Error::other(
                    "runtime cleanup transfer is one-use before open",
                ));
            }
            c.runtime_install_attempted = true;
            c.runtime
                .as_mut()
                .ok_or_else(|| io::Error::other("original runtime cleanup owner is absent"))?
                .install_provider(&mut c.bridge, c.run_controller_pidfd.as_fd())
        })();
        self.remember(result)
    }
    /// Recover the actual pre-open owner in place. This never constructs a
    /// Session or turns incomplete adoption into an unopened terminal lease.
    pub(super) fn recover_pre_open(
        &mut self,
        error: &io::Error,
        leaf_aliases: &mut Vec<OwnedFd>,
    ) -> io::Result<String> {
        self.retain_failure(error);
        let c = &mut *self.custody;
        if c.open_attempted
            || c.open_status.is_some()
            || c.parts.is_some()
            || c.session.is_some()
            || c.failed_open.is_some()
            || c.bridge.lease_attempt().is_some()
            || c.terminal_started
            || c.closed.is_some()
        {
            return Err(io::Error::other(
                "pre-open cleanup cannot follow any provider/open/terminal attempt",
            ));
        }
        let outcome = c
            .runtime
            .as_mut()
            .ok_or_else(|| io::Error::other("retained pre-open runtime is absent"))?
            .recover_pre_open(&mut c.bridge, error, c.failure_origin, leaf_aliases)?;
        // Descriptive reporting only; this string is never parsed as authority.
        Ok(format!("{outcome:?}"))
    }
    pub(super) fn open_retained(
        &mut self,
        object_path: &CStr,
        incarnation: u64,
        inventory_capacity: std::num::NonZeroU32,
    ) -> io::Result<()> {
        let deadline = self
            .custody
            .startup_deadline
            .ok_or_else(|| io::Error::other("original adoption deadline is absent"))?;
        self.open(object_path, incarnation, deadline, inventory_capacity)
    }
    pub(super) fn open(
        &mut self,
        object_path: &CStr,
        incarnation: u64,
        deadline: u64,
        inventory_capacity: std::num::NonZeroU32,
    ) -> io::Result<()> {
        let result = (|| {
            let c = &mut *self.custody;
            if c.open_attempted
                || c.first_failure.is_some()
                || c.incarnation != Some(incarnation)
                || c.startup_deadline != Some(deadline)
            {
                return Err(io::Error::other(
                    "grouped open changed or reused its retained owner",
                ));
            }
            c.open_attempted = true;
            c.runtime
                .as_mut()
                .ok_or_else(|| io::Error::other("actual runtime cleanup handoff is absent"))?
                .provider_ready()?;
            let started = grouped_monotonic_ns()?;
            if started >= deadline {
                return Err(io::Error::other("original grouped open deadline expired"));
            }
            let library = c
                .library
                .as_ref()
                .ok_or_else(|| io::Error::other("authenticated grouped library is absent"))?;
            let open = library
                .api
                .grouped
                .as_ref()
                .ok_or_else(|| io::Error::other("authenticated grouped API is absent"))?
                .open;
            let (io, owner) = {
                let mut lease = c.bridge.provider_lease(incarnation)?;
                // The same retained container owns Bridge for every session and
                // close path. Neither pointer escapes this private container.
                unsafe { lease.parts() }
            };
            c.parts = Some((io, owner));
            let mut raw = std::ptr::null_mut();
            let returned = unsafe {
                open(
                    object_path.as_ptr(),
                    incarnation,
                    io,
                    owner,
                    deadline,
                    &mut raw,
                )
            };
            let status = CallStatus::capture("ap_open_grouped", returned);
            c.open_status = Some(status);
            let session = NonNull::new(raw).map(|raw| Session {
                raw: Some(raw),
                library: library.clone(),
                incarnation,
                ready: returned == 0,
                audit: c.audit.clone(),
                inventory_capacity,
                grouped: true,
                grouped_parts: Some((io, owner)),
            });
            if returned == 0 && session.is_some() {
                c.session = session;
                return Ok(());
            }
            // The before-call origin is conservative even if a failing C call
            // loses its clock/errno later. It never grants an extra second.
            c.failure_origin.get_or_insert(started);
            c.failed_open = Some(OpenFailure {
                status,
                partial: session,
                audit: c.audit.clone(),
                success_without_session: returned == 0,
            });
            Err(status
                .errno
                .map(io::Error::from_raw_os_error)
                .unwrap_or_else(|| io::Error::other("grouped open returned failure or no session")))
        })();
        self.remember(result)
    }
    /// Lend operations only. The original private pointer/library binding is
    /// checked again before close; replacing a borrowed Session cannot make a
    /// foreign native object part of this owner.
    pub(super) fn session_mut(&mut self) -> io::Result<&mut Session> {
        let c = &mut *self.custody;
        if c.first_failure.is_some() || c.terminal_started {
            return Err(io::Error::other("grouped owner is refused or terminal"));
        }
        let origin = match grouped_monotonic_ns() {
            Ok(origin) => origin,
            Err(error) => {
                c.first_failure.get_or_insert_with(|| error.to_string());
                return Err(error);
            }
        };
        let ready = c
            .runtime
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime cleanup owner is absent"))
            .and_then(|runtime| runtime.provider_ready());
        if let Err(error) = ready {
            c.first_failure.get_or_insert_with(|| error.to_string());
            c.failure_origin.get_or_insert(origin);
            return Err(error);
        }
        let parts = c.parts;
        let incarnation = c.incarnation;
        let library = c
            .library
            .as_ref()
            .ok_or_else(|| io::Error::other("grouped library custody is absent"))?;
        if !c.session.as_ref().is_some_and(|session| {
            session.ready
                && session.grouped
                && session.grouped_parts == parts
                && Some(session.incarnation) == incarnation
                && Rc::ptr_eq(&session.library, library)
        }) {
            let error = io::Error::other("grouped session changed or is not ready");
            c.first_failure.get_or_insert_with(|| error.to_string());
            c.failure_origin.get_or_insert(origin);
            return Err(error);
        }
        Ok(c.session.as_mut().expect("checked original Session"))
    }
    pub(super) fn copy_poll_fd(&self) -> io::Result<Option<i32>> {
        let c = &*self.custody;
        if c.first_failure.is_some() || c.terminal_started {
            return Ok(None);
        }
        // Descriptive readiness fd only: this does not admit a new operation or
        // replace the actual runtime handoff check in session_mut.
        c.session
            .as_ref()
            .filter(|session| {
                session.ready
                    && session.grouped
                    && session.grouped_parts == c.parts
                    && Some(session.incarnation) == c.incarnation
                    && c.library
                        .as_ref()
                        .is_some_and(|library| Rc::ptr_eq(&session.library, library))
            })
            .map(Session::original_copy_poll_fd)
            .transpose()
    }
    pub(super) fn has_session(&self) -> bool {
        self.custody
            .session
            .as_ref()
            .is_some_and(|session| session.raw.is_some())
    }
    pub(super) fn open_status(&self) -> Option<CallStatus> {
        self.custody.open_status
    }
    pub(super) fn terminal_inventory(&mut self) -> Vec<Inventory> {
        let c = &mut *self.custody;
        c.session
            .iter_mut()
            .chain(
                c.failed_open
                    .iter_mut()
                    .filter_map(|failure| failure.partial.as_mut()),
            )
            .filter(|session| session.raw.is_some())
            .map(Session::identifiers)
            .collect()
    }
    pub(super) fn failed_open(&self) -> Option<&OpenFailure> {
        self.custody.failed_open.as_ref()
    }
    pub(super) fn close_receipt(&self) -> Option<&GroupedCloseReceipt> {
        self.custody.closed.as_ref()
    }
    pub(super) fn first_failure(&self) -> Option<&str> {
        self.custody.first_failure.as_deref()
    }
    pub(super) fn audit(&self) -> AuditHandle {
        self.custody.audit.clone()
    }

    /// Actual retained original pidfd, not EOF/JSON or a supplied boolean, is
    /// the terminal issuer. No repeated call can move its original origin.
    /// The private runtime handoff must supply a live peer and exclusive full
    /// read cursor before any close/reconcile/delete effect.
    pub(super) fn close_terminal(
        &mut self,
        release_start: u64,
        enclosing_cutoff: u64,
    ) -> io::Result<&GroupedCloseReceipt> {
        let result = (|| {
            let c = &mut *self.custody;
            if c.terminal_started {
                return Err(io::Error::other("grouped terminal close is one-use"));
            }
            c.terminal_started = true;
            if c.first_failure.is_some() && c.failure_origin.is_none() {
                return Err(io::Error::other(
                    "original grouped failure origin is unknown",
                ));
            }
            let mut cutoff = release_start
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("original grouped release origin overflow"))?
                .min(enclosing_cutoff);
            if let Some(origin) = c.failure_origin {
                cutoff =
                    cutoff.min(origin.checked_add(1_000_000_000).ok_or_else(|| {
                        io::Error::other("original grouped failure origin overflow")
                    })?);
            }
            c.terminal_origin = Some((release_start, cutoff));
            grouped_check_cutoff(release_start, cutoff)?;
            if !super::accepted_transport::controller_exited(c.run_controller_pidfd.as_fd())? {
                return Err(io::Error::other("original run controller remains live"));
            }
            let incarnation = c
                .incarnation
                .ok_or_else(|| io::Error::other("original incarnation is absent"))?;
            c.runtime
                .as_mut()
                .ok_or_else(|| io::Error::other("retained runtime cleanup owner is absent"))?
                .begin_terminal(c.run_controller_pidfd.as_fd(), release_start, cutoff)?;
            grouped_check_cutoff(release_start, cutoff)?;
            if c.open_status.is_none() {
                if c.parts.is_some() {
                    return Err(io::Error::other(
                        "issued provider lease has an unknown open result",
                    ));
                }
                // SAFETY: actual controller terminal and real runtime owner
                // authorization were obtained above under this pinned cutoff.
                let result = unsafe { c.bridge.retire_unopened(release_start, cutoff) };
                if let Some(call) = c.bridge.unopened_attempt() {
                    let close = CallStatus {
                        operation: "hermit_grouped_broker_retire_unopened",
                        returned: call.returned,
                        errno: call.errno,
                    };
                    c.closed = Some(GroupedCloseReceipt {
                        incarnation,
                        inventory: None,
                        close,
                        original_release_start: release_start,
                        cutoff,
                        native_pointer_retained: false,
                        runtime_completed: false,
                        requires_external_absence: true,
                    });
                }
                result?;
                grouped_check_cutoff(release_start, cutoff)?;
                c.runtime.as_mut().unwrap().finish_terminal(
                    &mut c.bridge,
                    release_start,
                    cutoff,
                )?;
                grouped_check_cutoff(release_start, cutoff)?;
                c.closed.as_mut().unwrap().runtime_completed = true;
                return Ok(());
            }
            let (io, owner) = c
                .parts
                .ok_or_else(|| io::Error::other("no original provider lease to retire"))?;
            let library = c
                .library
                .as_ref()
                .ok_or_else(|| io::Error::other("grouped library custody is absent"))?;
            let failed = c.failed_open.is_some();
            let mut session = if failed {
                c.failed_open
                    .as_mut()
                    .and_then(|failure| failure.partial.as_mut())
            } else {
                c.session.as_mut()
            };
            if session.as_ref().is_some_and(|session| {
                !session.grouped
                    || session.grouped_parts != Some((io, owner))
                    || session.incarnation != incarnation
                    || !Rc::ptr_eq(&session.library, library)
            }) {
                return Err(io::Error::other(
                    "grouped Session changed original pointer/library ownership",
                ));
            }
            let inventory = session.as_mut().map(|session| session.identifiers());
            let mut raw = session
                .as_ref()
                .and_then(|session| session.raw)
                .map_or(std::ptr::null_mut(), NonNull::as_ptr);
            let api = library
                .api
                .grouped
                .as_ref()
                .ok_or_else(|| io::Error::other("grouped close API is absent"))?;
            grouped_check_cutoff(release_start, cutoff)?;
            let (returned, operation) = if failed {
                (
                    unsafe { (api.close_startup)(&mut raw, io, owner, release_start, cutoff) },
                    "ap_close_grouped_startup_terminal",
                )
            } else if cutoff == release_start + 1_000_000_000 {
                // The old exact ACTIVE path is sufficient only at its full
                // original bound. Any shorter caller bound uses the additive API.
                (
                    unsafe { (api.close)(&mut raw, release_start) },
                    "ap_close_grouped_terminal",
                )
            } else {
                (
                    unsafe { (api.close_until)(&mut raw, release_start, cutoff) },
                    "ap_close_grouped_terminal_until",
                )
            };
            let close = CallStatus::capture(operation, returned);
            if let Some(session) = session {
                session.raw = NonNull::new(raw);
                session.ready = false;
            }
            c.closed = Some(GroupedCloseReceipt {
                incarnation,
                inventory,
                close,
                original_release_start: release_start,
                cutoff,
                native_pointer_retained: !raw.is_null(),
                runtime_completed: false,
                requires_external_absence: true,
            });
            if !close.succeeded() {
                return Err(close
                    .errno
                    .map(io::Error::from_raw_os_error)
                    .unwrap_or_else(|| io::Error::other("grouped terminal close failed")));
            }
            if !raw.is_null() {
                return Err(io::Error::other(
                    "successful grouped close retained native pointer",
                ));
            }
            grouped_check_cutoff(release_start, cutoff)?;
            c.runtime
                .as_mut()
                .unwrap()
                .finish_terminal(&mut c.bridge, release_start, cutoff)?;
            grouped_check_cutoff(release_start, cutoff)?;
            c.closed.as_mut().unwrap().runtime_completed = true;
            Ok(())
        })();
        self.remember(result)?;
        Ok(self
            .custody
            .closed
            .as_ref()
            .expect("native receipt retained before success"))
    }
}
impl Drop for GroupedSessionOwner {
    fn drop(&mut self) {
        // Deliberately retain every FD, context, DSO and Session on an accidental
        // drop. The outside service's nonreturning process exit owns final local
        // release; neither Drop nor C close is authoritative global absence.
        self.custody
            .audit
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidated = true;
    }
}
fn grouped_monotonic_ns() -> io::Result<u64> {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if time.tv_sec < 0 || time.tv_nsec < 0 || time.tv_nsec >= 1_000_000_000 {
        return Err(io::Error::other("invalid original grouped clock"));
    }
    (time.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(time.tv_nsec as u64))
        .ok_or_else(|| io::Error::other("original grouped clock overflow"))
}
fn grouped_check_cutoff(origin: u64, cutoff: u64) -> io::Result<()> {
    let now = grouped_monotonic_ns()?;
    if origin == 0 || origin > now || now >= cutoff {
        return Err(io::Error::other(
            "original grouped terminal cutoff expired or invalid",
        ));
    }
    Ok(())
}
