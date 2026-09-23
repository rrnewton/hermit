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
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;

pub const CREATED: u64 = 1;
pub const QUEUED: u64 = 2;
pub const RETIRED: u64 = 4;
pub const MATCHED: u64 = 8;
const ABI_V1: u64 = 0x4150_5255_5354_0001;

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
    assert!(size_of::<CommandResult>() == 128);
    assert!(offset_of!(CommandResult, identity) == 32);
    assert!(offset_of!(CommandResult, creation) == 56);
    assert!(offset_of!(CommandResult, state) == 72);
    assert!(offset_of!(CommandResult, returned) == 112);
    assert!(offset_of!(CommandResult, phase) == 120);
    assert!(size_of::<Status>() == 88);
    assert!(size_of::<ResourceId>() == 8);
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
impl<T> Observation<T> {
    pub fn into_result(self) -> Result<T, Self> {
        if self.status.succeeded() {
            Ok(self.raw)
        } else {
            Err(self)
        }
    }
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
    pub fn snapshot(&self) -> AuditState {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
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
struct Api {
    open: unsafe extern "C" fn(*const c_char, u64, *mut SessionPtr) -> c_int,
    register_task: unsafe extern "C" fn(SessionPtr, c_int) -> c_int,
    enroll_listener:
        unsafe extern "C" fn(SessionPtr, c_int, c_int, u64, *mut CommandResult) -> c_int,
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
    match_accepted:
        unsafe extern "C" fn(SessionPtr, c_int, c_int, Identity, *mut CommandResult) -> c_int,
    ack_command: unsafe extern "C" fn(SessionPtr, *const CommandResult) -> c_int,
    read_creation: unsafe extern "C" fn(SessionPtr, u32, *mut Creation) -> c_int,
    read_status: unsafe extern "C" fn(SessionPtr, *mut Status) -> c_int,
    validate_creation: unsafe extern "C" fn(*const Creation, *const Status) -> c_int,
    identifiers: unsafe extern "C" fn(SessionPtr, *mut ResourceId, u32, *mut u32) -> c_int,
    close: unsafe extern "C" fn(SessionPtr) -> c_int,
}
/// Non-Send/non-Sync by construction. Keep on the dedicated service thread.
pub struct Library {
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
    pub unsafe fn load(path: &CStr) -> Result<Rc<Self>, LoadError> {
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
        let version = symbol!("ap_adapter_abi_version", unsafe extern "C" fn() -> u64);
        if unsafe { version() } != ABI_V1 {
            return Err(LoadError("provider adapter ABI version mismatch".into()));
        }
        let api = Api {
            open: symbol!(
                "ap_open",
                unsafe extern "C" fn(*const c_char, u64, *mut SessionPtr) -> c_int
            ),
            register_task: symbol!(
                "ap_register_task",
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
            match_accepted: symbol!(
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
            validate_creation: symbol!(
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
            _handle: handle,
            api,
            _single_thread: std::marker::PhantomData,
        }))
    }
    pub fn validate_creation(&self, creation: &Creation, status: &Status) -> CallStatus {
        CallStatus::capture("ap_validate_creation", unsafe {
            (self.api.validate_creation)(creation, status)
        })
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
        });
        if rc == 0 && session.is_some() {
            return Ok(session.expect("checked Some"));
        }
        Err(OpenFailure {
            status,
            partial: session,
            audit,
            success_without_session: rc == 0,
        })
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
    pub fn match_accepted(
        &mut self,
        helper_pidfd: BorrowedFd<'_>,
        socket: BorrowedFd<'_>,
        expected: Identity,
    ) -> Observation<CommandResult> {
        let mut raw = CommandResult::default();
        let rc = unsafe {
            (self.library.api.match_accepted)(
                self.pointer(),
                helper_pidfd.as_raw_fd(),
                socket.as_raw_fd(),
                expected,
                &mut raw,
            )
        };
        Observation {
            status: CallStatus::capture("ap_match_accepted", rc),
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
    pub fn validate_creation(&self, creation: &Creation, status: &Status) -> CallStatus {
        self.library.validate_creation(creation, status)
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
        if self.raw.is_some() {
            // A discarded service session is invalidation, never completion.
            // This owns only BPF links/maps, not possibly-final TCP references.
            // The service must retain audit() outside its cancelable waiter.
            let receipt = self.close_inner(true);
            self.audit.record(receipt);
        }
    }
}
