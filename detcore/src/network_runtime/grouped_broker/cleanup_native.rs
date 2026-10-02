//! Cleanup-only binding to the accepted C bridge. Every C record is descriptive;
//! only the enclosing retained controller owns terminal/peer/cursor authority.
//! There is no Drop cleanup and no conversion to SourceTerminal or a provider.
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_uint;
use std::ffi::c_void;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::rc::Rc;

const ABI: c_uint = 1;
pub(super) const SITES: usize = 17;
pub(super) const FDS: usize = 6;
pub(super) const NAME_BYTES: usize = 64;
pub(super) const LINE_BYTES: usize = 256;
pub(super) const CENSUS_BYTES: usize = 1 << 20;
const OPERATIONS: usize = 8;

// C enum ap_grouped_phase is represented as its integer ABI, never as a Rust
// enum with invalid bit patterns or as a capability-bearing Rust state.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Owner {
    pub incarnation: u64,
    pub group: [c_char; NAME_BYTES],
    pub event: [c_char; NAME_BYTES],
    pub phase: c_uint,
    pub verified_sites: u32,
    pub attempted_sites: u32,
    pub event_id: u32,
    pub write_unknown: c_uint,
    pub pending_role: c_uint,
    pub pending_remove: c_uint,
    pub pending_bytes: usize,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Write {
    pub role: c_uint,
    pub remove: c_uint,
    pub submitted: usize,
    pub raw: isize,
    pub error: c_int,
    pub started: c_int,
    pub completed: c_int,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CreationPair {
    pub intent_owner: Owner,
    pub outcome_owner: Owner,
    pub intent: Write,
    pub outcome: Write,
    pub line: [c_char; LINE_BYTES],
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RecoveryStep {
    pub pair: CreationPair,
    pub outcome_present: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Call {
    pub attempted: c_uint,
    pub returned: c_uint,
    pub raw: c_int,
    pub error: c_int,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Close {
    pub descriptor: c_int,
    pub call: Call,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Release {
    pub release_start: u64,
    pub first_failure_origin: u64,
    pub enclosing_cutoff: u64,
    pub has_first_failure: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Callback {
    pub kind: c_uint,
    pub owner: Owner,
    pub write: Write,
    pub line_bytes: usize,
    pub line: [c_char; LINE_BYTES],
    pub user_call: Call,
    pub return_to_io: Call,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Status {
    pub abi: c_uint,
    pub refused: c_uint,
    pub first_error: c_int,
    pub prepared: c_uint,
    pub adopted: c_uint,
    pub deleted: c_uint,
    pub absence_attempts: c_uint,
    pub absence_complete_mask: c_uint,
    pub aliases_attempted: c_uint,
    pub aliases_returned: c_uint,
    pub allocation_ready: c_uint,
    pub incarnation: u64,
    pub stage_cutoff: u64,
    pub effective_cutoff: u64,
    pub local_failure_origin: u64,
    pub local_failure_origin_valid: c_uint,
    pub local_failure_clock_validation_error: c_int,
    pub receipt_buffers_allocated: usize,
    pub local_failure_clock: Call,
    pub original: Release,
    pub original_retained: c_uint,
    pub supplied_steps: usize,
    pub retained_steps: usize,
    pub allocation: Call,
    pub prepare: Call,
    pub adopt: Call,
    pub deletion: Call,
    pub absence: [Call; 2],
    pub aliases: Call,
    pub io_initialize: Call,
    pub io_adopt: Call,
    pub io_delete: Call,
    pub io_absence: [Call; 2],
    pub closes: [Close; FDS],
    pub descriptors: [c_int; FDS],
    pub io_buffer_present: c_uint,
    pub io_proof_present: c_uint,
    pub owner: Owner,
    pub writes_count: c_uint,
    pub callbacks_count: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct History {
    pub owner: Owner,
    pub writes_count: c_uint,
    pub writes: [Write; SITES * 2],
    pub retained_steps: usize,
    pub steps: [RecoveryStep; SITES],
    pub callbacks_count: c_uint,
    pub callbacks: [Callback; 1 + SITES * 2],
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CompleteReadObservation {
    pub attempted: c_uint,
    pub returned: c_uint,
    pub result: isize,
    pub error: c_int,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectoryObservation {
    pub attempted: c_uint,
    pub returned: c_uint,
    pub result: c_int,
    pub error: c_int,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AbsenceObservation {
    pub cutoff: u64,
    pub started_ns: u64,
    pub finished_ns: u64,
    pub started_observed: c_uint,
    pub finished_observed: c_uint,
    pub complete: c_uint,
    pub definition_bytes: usize,
    pub profile_bytes: usize,
    pub definitions: CompleteReadObservation,
    pub profile: CompleteReadObservation,
    pub event_directory: DirectoryObservation,
    pub group_directory: DirectoryObservation,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Absence {
    pub call: Call,
    pub io_call: Call,
    pub observation: AbsenceObservation,
    pub definition_bytes_known: c_uint,
    pub profile_bytes_known: c_uint,
    pub definition_bytes: usize,
    pub profile_bytes: usize,
    pub definitions: *const u8,
    pub profile: *const u8,
}
macro_rules! zero_default {
    ($($ty:ty),+ $(,)?) => {$(impl Default for $ty {
        fn default() -> Self {
            // All fields are C integers, raw pointers, or arrays of these POD
            // records. Zero is valid storage, never evidence of returned0.
            unsafe { std::mem::zeroed() }
        }
    })+};
}
zero_default!(
    Owner,
    Write,
    CreationPair,
    RecoveryStep,
    Call,
    Close,
    Release,
    Callback,
    Status,
    History,
    CompleteReadObservation,
    DirectoryObservation,
    AbsenceObservation,
    Absence
);

// Exact accepted typedefs. In particular journal has NO length argument;
// write.submitted is the bounded extent of line, which need not be NUL-ended.
pub(super) type Journal =
    unsafe extern "C" fn(*mut c_void, *const Owner, *const Write, *const c_char) -> c_int;
pub(super) type Acknowledge = unsafe extern "C" fn(
    *mut c_void,
    *const Owner,
    *const c_int,
    *const RecoveryStep,
    usize,
) -> c_int;

#[derive(Debug)]
struct Api {
    alloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    prepare: unsafe extern "C" fn(
        *mut c_void,
        u64,
        *const c_char,
        usize,
        *const c_int,
        u64,
        Journal,
        *mut c_void,
    ) -> c_int,
    adopt: unsafe extern "C" fn(
        *mut c_void,
        *const RecoveryStep,
        usize,
        Acknowledge,
        *mut c_void,
        *const Release,
    ) -> c_int,
    delete: unsafe extern "C" fn(*mut c_void) -> c_int,
    observe: unsafe extern "C" fn(*mut c_void) -> c_int,
    status: unsafe extern "C" fn(*const c_void, *mut Status) -> c_int,
    history: unsafe extern "C" fn(*const c_void, *mut History) -> c_int,
    absence: unsafe extern "C" fn(*const c_void, c_uint, *mut Absence) -> c_int,
    release: unsafe extern "C" fn(*mut c_void) -> c_int,
    free: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Allocate,
    Prepare,
    Adopt,
    Delete,
    ObserveFirst,
    ObserveSecond,
    ReleaseAliases,
    Free,
}
impl Operation {
    fn index(self) -> usize {
        self as usize
    }
    fn observation(self) -> Option<usize> {
        match self {
            Self::ObserveFirst => Some(0),
            Self::ObserveSecond => Some(1),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Returned {
    pub raw: c_int,
    /// errno belongs only to a real returned -1; unexpected positive returns
    /// remain explicit failures and do not acquire a stale errno.
    pub errno: Option<c_int>,
}
impl Returned {
    fn capture(raw: c_int) -> Self {
        let errno = (raw == -1).then(|| unsafe { *libc::__errno_location() });
        Self { raw, errno }
    }
}
/// dlclose reports through dlerror, not errno. A failure retains the actual
/// handle and this diagnostic separately from the original bridge failure.
#[derive(Debug)]
pub(super) struct LoaderClose {
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub raw: c_int,
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub diagnostic: Option<String>,
}
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Attempt {
    pub requests: u64,
    pub entered: bool,
    pub returned: Option<Returned>,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct ReadbackIssue {
    pub part: &'static str,
    pub result: Option<Returned>,
    pub description: &'static str,
}
#[derive(Debug)]
pub(super) struct Readback {
    pub operation: Operation,
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub status_call: Returned,
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub history_call: Returned,
    pub absence_call: Option<Returned>,
    pub status: Option<Status>,
    pub history: Option<History>,
    pub issues: [Option<ReadbackIssue>; 5],
}
impl Readback {
    fn issue(&mut self, issue: ReadbackIssue) {
        // At most status, history, absence, definition span, profile span.
        *self
            .issues
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("fixed readback issue population") = Some(issue);
    }
}
#[derive(Debug)]
pub(super) struct AbsenceSnapshot {
    /// Actual returned metadata, including descriptive C pointer values. Those
    /// pointers must not be dereferenced; the owned byte accessors survive free.
    pub metadata: Option<Absence>,
    pub readback: Option<Returned>,
    definitions: Vec<u8>,
    profile: Vec<u8>,
    definition_copied: bool,
    profile_copied: bool,
}
impl AbsenceSnapshot {
    fn retained() -> Self {
        Self {
            metadata: None,
            readback: None,
            definitions: Vec::new(),
            profile: Vec::new(),
            definition_copied: false,
            profile_copied: false,
        }
    }
    fn reserve(&mut self) -> io::Result<()> {
        for bytes in [&mut self.definitions, &mut self.profile] {
            bytes
                .try_reserve_exact(CENSUS_BYTES)
                .map_err(io::Error::other)?;
            bytes.resize(CENSUS_BYTES, 0);
        }
        Ok(())
    }
    pub fn definitions(&self) -> Option<&[u8]> {
        let m = self.metadata.as_ref()?;
        (self.definition_copied && m.definition_bytes_known == 1)
            .then(|| &self.definitions[..m.definition_bytes])
    }
    pub fn profile(&self) -> Option<&[u8]> {
        let m = self.metadata.as_ref()?;
        (self.profile_copied && m.profile_bytes_known == 1)
            .then(|| &self.profile[..m.profile_bytes])
    }
    fn copied(&self) -> bool {
        self.metadata.is_some() && self.definition_copied && self.profile_copied
    }
}

#[derive(Debug)]
#[must_use = "retain actual cleanup context, callbacks and receipts through explicit retirement"]
pub(super) struct CleanupBridge {
    file: OwnedFd,
    expected: [u8; 32],
    api: Option<Api>,
    loader: Option<NonNull<c_void>>,
    context: *mut c_void,
    attempts: [Attempt; OPERATIONS],
    excess_observation_requests: u64,
    readbacks: Vec<Readback>,
    observations: [AbsenceSnapshot; 2],
    refusal: Option<String>,
    loader_close: Option<LoaderClose>,
    _single_thread: std::marker::PhantomData<Rc<()>>,
}
impl CleanupBridge {
    pub fn retain(file: OwnedFd, expected: [u8; 32]) -> Self {
        Self {
            file,
            expected,
            api: None,
            loader: None,
            context: std::ptr::null_mut(),
            attempts: [Attempt::default(); OPERATIONS],
            excess_observation_requests: 0,
            readbacks: Vec::new(),
            observations: [AbsenceSnapshot::retained(), AbsenceSnapshot::retained()],
            refusal: None,
            loader_close: None,
            _single_thread: std::marker::PhantomData,
        }
    }
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
    pub fn attempts(&self) -> &[Attempt; OPERATIONS] {
        &self.attempts
    }
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub fn excess_observation_requests(&self) -> u64 {
        self.excess_observation_requests
    }
    pub fn readbacks(&self) -> &[Readback] {
        &self.readbacks
    }
    /// Latest successful descriptive read; inspect its Readback operation for
    /// freshness. A later read failure never overwrites an earlier observation.
    pub fn status(&self) -> Option<&Status> {
        self.readbacks
            .iter()
            .rev()
            .find_map(|row| row.status.as_ref())
    }
    pub fn history(&self) -> Option<&History> {
        self.readbacks
            .iter()
            .rev()
            .find_map(|row| row.history.as_ref())
    }
    #[expect(dead_code, reason = "Retained native cleanup observations keep original failures for diagnostic consumers not yet wired")]
    pub fn absence(&self, index: usize) -> Option<&AbsenceSnapshot> {
        self.observations
            .get(index)
            .filter(|row| row.readback.is_some())
    }
    pub fn context_retained(&self) -> bool {
        !self.context.is_null()
    }
    pub fn loader_retained(&self) -> bool {
        self.loader.is_some()
    }
    pub fn loader_close(&self) -> Option<&LoaderClose> {
        self.loader_close.as_ref()
    }
    fn latch(&mut self, message: String) {
        if self.refusal.is_none() {
            self.refusal = Some(message);
        }
    }
    fn outcome(&self) -> io::Result<()> {
        match &self.refusal {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }
    fn reject<T>(&mut self, message: &str) -> io::Result<T> {
        self.latch(message.to_owned());
        Err(io::Error::other(self.refusal.as_ref().unwrap().clone()))
    }
    fn begin(&mut self, operation: Operation, after_refusal: bool) -> io::Result<()> {
        let attempt = &mut self.attempts[operation.index()];
        let Some(requests) = attempt.requests.checked_add(1) else {
            return self.reject("cleanup wrapper request counter exhausted");
        };
        attempt.requests = requests;
        if requests != 1 {
            return self.reject("cleanup wrapper cannot repeat an operation");
        }
        if !after_refusal {
            self.outcome()?;
        }
        if self.attempts[Operation::Free.index()].requests != 0 && operation != Operation::Free {
            return self.reject("cleanup wrapper cannot operate after free was requested");
        }
        Ok(())
    }
    fn native_ready(&mut self) -> io::Result<()> {
        if self.api.is_none() || self.context.is_null() {
            return self.reject("cleanup native context is absent");
        }
        Ok(())
    }
    fn entered(&mut self, operation: Operation) {
        self.attempts[operation.index()].entered = true;
    }
    fn returned(&mut self, operation: Operation, result: Returned) {
        self.attempts[operation.index()].returned = Some(result);
        if result.raw != 0 {
            self.latch(format!(
                "cleanup {operation:?} returned raw={} errno={:?}",
                result.raw, result.errno
            ));
        }
    }
    /// # Safety
    /// The artifact/dependency closure is independently authenticated by the
    /// actual controller. It must retain this owner in its failure scope before
    /// calling here. No untrusted digest authorizes dlopen constructors.
    pub unsafe fn initialize(&mut self) -> io::Result<()> {
        self.begin(Operation::Allocate, false)?;
        let result = (|| {
            let handle =
                unsafe { super::native::open_sealed_bridge(self.file.as_fd(), self.expected) }?;
            // Retain the opened DSO before ABI/symbol checks can fail. A complete
            // Api is not the constructor of loader custody; only explicit free
            // may close this handle, including a failed resolution path.
            self.loader = Some(handle);
            let resolved = (|| {
                macro_rules! symbol {
                    ($name:literal, $ty:ty) => {{
                        unsafe { libc::dlerror() };
                        let p = unsafe {
                            libc::dlsym(handle.as_ptr(), concat!($name, "\0").as_ptr().cast())
                        };
                        if p.is_null() {
                            return Err(super::native::loader_error());
                        }
                        unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
                    }};
                }
                let abi = symbol!(
                    "hermit_grouped_cleanup_abi",
                    unsafe extern "C" fn() -> c_uint
                );
                super::require(unsafe { abi() } == ABI, "cleanup bridge ABI differs")?;
                Ok(Api {
                    alloc: symbol!(
                        "hermit_grouped_cleanup_alloc",
                        unsafe extern "C" fn(*mut *mut c_void) -> c_int
                    ),
                    prepare: symbol!(
                        "hermit_grouped_cleanup_prepare",
                        unsafe extern "C" fn(
                            *mut c_void,
                            u64,
                            *const c_char,
                            usize,
                            *const c_int,
                            u64,
                            Journal,
                            *mut c_void,
                        ) -> c_int
                    ),
                    adopt: symbol!(
                        "hermit_grouped_cleanup_adopt",
                        unsafe extern "C" fn(
                            *mut c_void,
                            *const RecoveryStep,
                            usize,
                            Acknowledge,
                            *mut c_void,
                            *const Release,
                        ) -> c_int
                    ),
                    delete: symbol!(
                        "hermit_grouped_cleanup_delete",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                    observe: symbol!(
                        "hermit_grouped_cleanup_observe_absent",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                    status: symbol!(
                        "hermit_grouped_cleanup_status",
                        unsafe extern "C" fn(*const c_void, *mut Status) -> c_int
                    ),
                    history: symbol!(
                        "hermit_grouped_cleanup_history",
                        unsafe extern "C" fn(*const c_void, *mut History) -> c_int
                    ),
                    absence: symbol!(
                        "hermit_grouped_cleanup_absence",
                        unsafe extern "C" fn(*const c_void, c_uint, *mut Absence) -> c_int
                    ),
                    release: symbol!(
                        "hermit_grouped_cleanup_release_aliases",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                    free: symbol!(
                        "hermit_grouped_cleanup_free",
                        unsafe extern "C" fn(*mut *mut c_void) -> c_int
                    ),
                })
            })();
            match resolved {
                Ok(api) => self.api = Some(api),
                Err(error) => return Err(error),
            }
            // Fixed Rust observation storage is ready before native allocation
            // and before any preparation/creation ACK. No Vec growth follows a
            // global operation. On setup failure the DSO remains retained.
            self.readbacks
                .try_reserve_exact(OPERATIONS)
                .map_err(io::Error::other)?;
            for observation in &mut self.observations {
                observation.reserve()?;
            }
            self.entered(Operation::Allocate);
            // C writes directly into the externally retained slot before its
            // later allocations can fail. Do not use a temporary out pointer.
            let raw = unsafe { (self.api.as_ref().unwrap().alloc)(&mut self.context) };
            let result = Returned::capture(raw);
            self.returned(Operation::Allocate, result);
            if self.context.is_null() {
                self.latch("cleanup allocation retained no native context".to_owned());
            } else {
                self.capture_readback(Operation::Allocate);
            }
            self.outcome()
        })();
        if let Err(error) = &result {
            self.latch(error.to_string());
        }
        self.outcome()
    }
    /// # Safety
    /// The stable retained_owner and journal must outlive the C context. The
    /// callback must not unwind/reenter; it grants ACK0 only after the envelope's
    /// actual current durable peer ACK and exclusive cursor handoff. Controls,
    /// nonce/incarnation and the ORIGINAL stage cutoff are privately bound by
    /// that owner. NoProviderCreated/readiness belongs to the envelope.
    pub unsafe fn prepare(
        &mut self,
        incarnation: u64,
        nonce: &str,
        controls: [BorrowedFd<'_>; 3],
        stage_cutoff: u64,
        journal: Journal,
        retained_owner: *mut c_void,
    ) -> io::Result<()> {
        self.begin(Operation::Prepare, false)?;
        self.native_ready()?;
        let fds = controls.map(|fd| fd.as_raw_fd());
        self.entered(Operation::Prepare);
        let raw = unsafe {
            (self.api.as_ref().unwrap().prepare)(
                self.context,
                incarnation,
                nonce.as_ptr().cast(),
                nonce.len(),
                fds.as_ptr(),
                stage_cutoff,
                journal,
                retained_owner,
            )
        };
        self.finish(Operation::Prepare, Returned::capture(raw))
    }
    /// # Safety
    /// The envelope owns actual source/helper/query/Launcher terminal custody,
    /// NoProviderCreated (or separately proved full-provider terminal custody),
    /// immutable dual-intent-eligible histories and a LIVE independent cleanup
    /// peer. It owns the actual CONTROL/PROFILE OFD cursor epoch throughout this
    /// synchronous call, yielding/regaining it only at the authenticated ACK.
    /// The callback/context must remain retained, never unwind/reenter, and may
    /// return0 only after current durable ACKs. Release carries the immutable
    /// original origins/cutoffs; numeric fields here supply no such authority.
    pub unsafe fn adopt(
        &mut self,
        steps: &[RecoveryStep],
        acknowledge: Acknowledge,
        retained_owner: *mut c_void,
        original: Release,
    ) -> io::Result<()> {
        self.begin(Operation::Adopt, false)?;
        self.native_ready()?;
        self.entered(Operation::Adopt);
        let raw = unsafe {
            (self.api.as_ref().unwrap().adopt)(
                self.context,
                steps.as_ptr(),
                steps.len(),
                acknowledge,
                retained_owner,
                &original,
            )
        };
        self.finish(Operation::Adopt, Returned::capture(raw))
    }
    /// # Safety
    /// All adopt preconditions remain continuously owned; the original cursor,
    /// live peer, callbacks and cutoffs must still hold. This performs real
    /// deletion, not local alias closure or a retry after uncertainty.
    pub unsafe fn delete(&mut self) -> io::Result<()> {
        self.begin(Operation::Delete, false)?;
        self.native_ready()?;
        self.entered(Operation::Delete);
        let raw = unsafe { (self.api.as_ref().unwrap().delete)(self.context) };
        self.finish(Operation::Delete, Returned::capture(raw))
    }
    /// # Safety
    /// The same original exclusive OFD cursor and cleanup custody/cutoffs are
    /// still owned. No other process may snapshot the shared descriptions while
    /// C scans. Each call consumes one distinct slot, even when it fails.
    pub unsafe fn observe_absent(&mut self) -> io::Result<&AbsenceSnapshot> {
        let operation = if self.attempts[Operation::ObserveFirst.index()].requests == 0 {
            Operation::ObserveFirst
        } else if self.attempts[Operation::ObserveSecond.index()].requests == 0 {
            Operation::ObserveSecond
        } else {
            self.excess_observation_requests = self
                .excess_observation_requests
                .checked_add(1)
                .ok_or_else(|| io::Error::other("cleanup observation request counter exhausted"))?;
            return self.reject("cleanup has exactly two absence observation attempts");
        };
        self.begin(operation, false)?;
        self.native_ready()?;
        self.entered(operation);
        let raw = unsafe { (self.api.as_ref().unwrap().observe)(self.context) };
        self.finish(operation, Returned::capture(raw))?;
        Ok(&self.observations[operation.observation().unwrap()])
    }
    /// # Safety
    /// The envelope has explicitly entered original-deadline local alias
    /// retirement and retains all independent global custody. This cannot prove
    /// deletion/absence or clear a prior refusal. No descriptor retry is made.
    pub unsafe fn release_aliases(&mut self) -> io::Result<()> {
        self.begin(Operation::ReleaseAliases, true)?;
        self.native_ready()?;
        self.entered(Operation::ReleaseAliases);
        let raw = unsafe { (self.api.as_ref().unwrap().release)(self.context) };
        self.finish(Operation::ReleaseAliases, Returned::capture(raw))
    }
    fn finish(&mut self, operation: Operation, result: Returned) -> io::Result<()> {
        // Preserve native raw/errno and the primary BEFORE fallible readback.
        self.returned(operation, result);
        self.capture_readback(operation);
        self.outcome()
    }
    fn capture_readback(&mut self, operation: Operation) {
        let api = self.api.as_ref().unwrap();
        let mut status = Status::default();
        let raw = unsafe { (api.status)(self.context, &mut status) };
        let status_call = Returned::capture(raw);
        let mut history = History::default();
        let raw = unsafe { (api.history)(self.context, &mut history) };
        let history_call = Returned::capture(raw);
        let mut row = Readback {
            operation,
            status_call,
            history_call,
            absence_call: None,
            status: (status_call.raw == 0).then_some(status),
            history: (history_call.raw == 0).then_some(history),
            issues: [None; 5],
        };
        if status_call.raw != 0 {
            row.issue(ReadbackIssue {
                part: "status",
                result: Some(status_call),
                description: "native status readback failed",
            });
        } else if status.abi != ABI
            || status.absence_attempts > 2
            || status.retained_steps > SITES
            || status.writes_count as usize > SITES * 2
            || status.callbacks_count as usize > 1 + SITES * 2
        {
            row.issue(ReadbackIssue {
                part: "status",
                result: None,
                description: "native status shape differs",
            });
        }
        if history_call.raw != 0 {
            row.issue(ReadbackIssue {
                part: "history",
                result: Some(history_call),
                description: "native history readback failed",
            });
        } else if history.retained_steps > SITES
            || history.writes_count as usize > SITES * 2
            || history.callbacks_count as usize > 1 + SITES * 2
        {
            row.issue(ReadbackIssue {
                part: "history",
                result: None,
                description: "native history shape differs",
            });
        }
        if let Some(index) = operation.observation() {
            let mut metadata = Absence::default();
            let raw = unsafe { (api.absence)(self.context, index as c_uint, &mut metadata) };
            let result = Returned::capture(raw);
            row.absence_call = Some(result);
            let slot = &mut self.observations[index];
            slot.readback = Some(result);
            if result.raw != 0 {
                row.issue(ReadbackIssue {
                    part: "absence",
                    result: Some(result),
                    description: "native absence readback failed",
                });
            } else {
                slot.metadata = Some(metadata);
                for (name, known, count, pointer, bytes, copied) in [
                    (
                        "definitions",
                        metadata.definition_bytes_known,
                        metadata.definition_bytes,
                        metadata.definitions,
                        &mut slot.definitions,
                        &mut slot.definition_copied,
                    ),
                    (
                        "profile",
                        metadata.profile_bytes_known,
                        metadata.profile_bytes,
                        metadata.profile,
                        &mut slot.profile,
                        &mut slot.profile_copied,
                    ),
                ] {
                    if known > 1
                        || count > CENSUS_BYTES
                        || known == 0 && count != 0
                        || known == 1 && count != 0 && pointer.is_null()
                        || bytes.len() != CENSUS_BYTES
                    {
                        row.issue(ReadbackIssue {
                            part: name,
                            result: None,
                            description: "native absence span is malformed or storage absent",
                        });
                        continue;
                    }
                    if known == 1 && count != 0 {
                        // Trusted accepted C bridge owns this bounded immutable
                        // per-attempt buffer until explicit free; never shared
                        // scratch io->buffer from the next scan.
                        bytes[..count]
                            .copy_from_slice(unsafe { std::slice::from_raw_parts(pointer, count) });
                    }
                    *copied = true;
                }
            }
        }
        if let Some(issue) = row.issues.iter().flatten().next() {
            self.latch(format!(
                "cleanup {operation:?} readback {}: {} {:?}",
                issue.part, issue.description, issue.result
            ));
        }
        // Exactly one snapshot per entered fixed operation; storage was
        // reserved before C allocation. No native retry or old-frame overwrite.
        assert!(self.readbacks.len() < OPERATIONS);
        self.readbacks.push(row);
    }
    /// # Safety
    /// The actual retained controller already qualified local alias retirement
    /// and retains all unresolved external/global owners. Every required C
    /// receipt must have been copied into this still-retained Rust owner; its
    /// callbacks/context remain alive through this call. Free only releases
    /// memory/DSO aliases and supplies no global cleanup proof. A prior primary
    /// refusal remains the return value even when local free actually succeeds;
    /// consult the preserved native attempt for that distinct fact.
    pub unsafe fn free(&mut self) -> io::Result<()> {
        self.begin(Operation::Free, true)?;
        if !self.context.is_null() {
            let release = self.attempts[Operation::ReleaseAliases.index()];
            let row = self
                .readbacks
                .iter()
                .find(|r| r.operation == Operation::ReleaseAliases);
            let qualified = release.returned.is_some_and(|r| r.raw == 0)
                && row.is_some_and(|r| {
                    r.issues.iter().all(Option::is_none)
                        && r.status.is_some()
                        && r.history.is_some()
                });
            if !qualified {
                return self.reject("cleanup free requires completed explicit alias retirement and retained readback");
            }
            let status = row.unwrap().status.as_ref().unwrap();
            if status.aliases_attempted != 1
                || status.aliases_returned != 1
                || status.descriptors.iter().any(|fd| *fd >= 0)
                || status.io_buffer_present != 0
                || status.io_proof_present != 0
                || status.closes.iter().any(|close| {
                    close.call.attempted != 0
                        && (close.call.attempted != 1
                            || close.call.returned != 1
                            || close.call.raw != 0
                            || close.call.error != 0)
                })
                || (0..status.absence_attempts as usize)
                    .any(|index| !self.observations[index].copied())
            {
                return self
                    .reject("cleanup free lacks actual alias returns or copied receipt custody");
            }
            self.entered(Operation::Free);
            let raw = unsafe { (self.api.as_ref().unwrap().free)(&mut self.context) };
            let result = Returned::capture(raw);
            self.returned(Operation::Free, result);
            if result.raw != 0 {
                if !self.context.is_null() {
                    self.capture_readback(Operation::Free);
                }
                return self.outcome();
            }
            if !self.context.is_null() {
                return self
                    .reject("cleanup free returned0 without consuming the actual context slot");
            }
        } else {
            // With the exact accepted allocator, a returned -1 with a null out
            // slot means calloc never acquired a context. Before alloc entry
            // there is likewise no C owner; this explicit path only closes DSO.
            let allocation = self.attempts[Operation::Allocate.index()];
            if allocation.entered && !allocation.returned.is_some_and(|r| r.raw == -1) {
                return self.reject("cleanup has an unresolved null-context allocation result");
            }
        }
        if let Some(handle) = self.loader {
            unsafe { libc::dlerror() };
            let raw = unsafe { libc::dlclose(handle.as_ptr()) };
            // Capture the dynamic loader diagnostic immediately, before any
            // unrelated native call. errno is not its authoritative channel.
            let diagnostic = (raw != 0).then(|| super::native::loader_error().to_string());
            if let Some(message) = &diagnostic {
                self.latch(message.clone());
            }
            self.loader_close = Some(LoaderClose { raw, diagnostic });
            if raw == 0 {
                self.api = None;
                self.loader = None;
            }
        }
        self.outcome()
    }
}
// No Drop impl: unresolved C state and its loaded code are never implicitly
// freed, aliases retried, original clocks refreshed, or cleanup certified.
