//! Source/successor bridge owner. The early helper retains this entire object
//! before calling C; a failed call never relinquishes its partial native FDs.
//! The private provider lease borrows an actually adopted LEAVES context.
//! It does not construct runtime readiness or terminal-cleanup authority.
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::c_int;
use std::ffi::c_void;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::rc::Rc;

use sha2::Digest;
use sha2::Sha256;

use super::require;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::network_runtime) struct Status {
    pub abi: u32,
    pub attempted: u32,
    pub refused: u32,
    pub error: u32,
    pub source_ready: u32,
    pub source_created: u32,
    pub successor_adopted: u32,
    pub leaves_bound: u32,
    pub aliases_released: u32,
    pub owner_phase: u32,
    pub attempted_sites: u32,
    pub verified_sites: u32,
    pub incarnation: u64,
    pub deadline: u64,
    pub creator_cutoff: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RuntimeHandoffStatus {
    pub installed: u32,
    pub aliases_released: u32,
    pub attempted: u32,
    pub returned: u32,
    pub fd: [i32; 9],
    pub raw: [i32; 9],
    pub error: [i32; 9],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::network_runtime) struct NativeCall {
    pub returned: c_int,
    pub errno: Option<c_int>,
}
type DigestFn = unsafe extern "C" fn(*const u8, usize, *mut u8) -> c_int;
type Start = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    u64,
    *const i8,
    u64,
    u64,
    *const i8,
    *const c_int,
) -> c_int;
type Successor = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    u64,
    *const i8,
    u64,
    u64,
    *const i8,
    *const c_int,
    DigestFn,
) -> c_int;
#[derive(Debug)]
struct Api {
    handle: NonNull<c_void>,
    alloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    source: Start,
    create: unsafe extern "C" fn(*mut c_void) -> c_int,
    successor: Successor,
    runtime_controls: unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int,
    runtime_journal:
        unsafe extern "C" fn(*mut c_void, super::cleanup_native::Journal, *mut c_void) -> c_int,
    runtime_status: unsafe extern "C" fn(*const c_void, *mut RuntimeHandoffStatus) -> c_int,
    retire_unopened: unsafe extern "C" fn(*mut c_void, u64, u64) -> c_int,
    provider_lease:
        unsafe extern "C" fn(*mut c_void, u64, *mut *mut c_void, *mut *mut c_void) -> c_int,
    status: unsafe extern "C" fn(*const c_void, *mut Status) -> c_int,
    release: unsafe extern "C" fn(*mut c_void) -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
}
// Neither the retained native context nor its TLS hash callback may cross
// threads. Rc additionally makes the outer owner !Send and !Sync.
#[derive(Debug)]
#[must_use = "retain native context through explicit alias release"]
pub(in crate::network_runtime) struct Bridge {
    file: OwnedFd,
    expected: [u8; 32],
    api: Option<Api>,
    context: *mut c_void,
    last: Option<Status>,
    refusal: Option<String>,
    lease_attempt: Option<(c_int, Option<c_int>)>,
    unopened_attempt: Option<NativeCall>,
    runtime_attempt: Option<(c_int, Option<c_int>)>,
    last_runtime: Option<RuntimeHandoffStatus>,
    controls_observation: Option<(NativeCall, [c_int; 3])>,
    pre_open_alias_retirement: Option<PreOpenAliasRetirement>,
    lease_io: *mut c_void,
    lease_owner: *mut c_void,
    _single_thread: std::marker::PhantomData<Rc<()>>,
}
pub(super) fn loader_error() -> io::Error {
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        io::Error::other("grouped bridge dynamic loader failed without diagnostic")
    } else {
        io::Error::other(
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}
/// Open only the exact authenticated sealed memfd. This is the existing
/// source-bridge loader policy, shared without a second cleanup policy.
///
/// # Safety
/// The caller has authenticated this exact artifact and its executable
/// dependency closure; a digest supplied by an untrusted peer is not authority.
pub(super) unsafe fn open_sealed_bridge(
    file: BorrowedFd<'_>,
    expected: [u8; 32],
) -> io::Result<NonNull<c_void>> {
    require(expected != [0; 32], "bridge digest is absent")?;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    require(
        seals == libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        "bridge requires exact executable memfd seals",
    )?;
    let held = super::owner::stat(file.as_raw_fd())?;
    require(
        held.mode & libc::S_IFMT == libc::S_IFREG
            && held.mode & 0o7777 == 0o500
            && held.size >= 64
            && held.size <= 1_048_576,
        "bridge file shape or original artifact bound differs",
    )?;
    let mut bytes = vec![0; held.size as usize];
    let mut done = 0;
    while done < bytes.len() {
        let n = unsafe {
            libc::pread(
                file.as_raw_fd(),
                bytes[done..].as_mut_ptr().cast(),
                bytes.len() - done,
                done as libc::off_t,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        require(n > 0, "bridge sealed read made no progress")?;
        done += n as usize;
    }
    require(
        <[u8; 32]>::from(Sha256::digest(&bytes)) == expected,
        "sealed bridge bytes differ from authenticated package",
    )?;
    require(
        bytes[..6] == *b"\x7fELF\x02\x01"
            && u16::from_le_bytes([bytes[16], bytes[17]]) == 3
            && u16::from_le_bytes([bytes[18], bytes[19]]) == 62,
        "bridge ELF type or architecture differs",
    )?;
    let path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    let raw = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    let handle = NonNull::new(raw).ok_or_else(loader_error)?;
    Ok(handle)
}

unsafe extern "C" fn digest(bytes: *const u8, count: usize, out: *mut u8) -> c_int {
    if bytes.is_null() || out.is_null() || count > 65_536 {
        return -1;
    }
    // Maintained C codec bounds both spans and invokes this synchronously. This
    // fixed sha2 call neither allocates authority nor unwinds across the ABI.
    let value = Sha256::digest(unsafe { std::slice::from_raw_parts(bytes, count) });
    unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), out, 32) };
    0
}
/// Descriptive local retirement only. No definition is deleted and no
/// provider/session/terminal authority is created from these native receipts.
#[derive(Debug)]
pub(in crate::network_runtime) struct PreOpenAliasRetirement {
    #[expect(dead_code, reason = "Original native alias-retirement status is retained for diagnostic consumers not yet integrated")]
    pub allocation_retained: bool,
    pub attempted: bool,
    pub returned: Option<NativeCall>,
    pub observed: Option<Status>,
    pub readback_error: Option<String>,
}
impl Bridge {
    /// Infallibly retain the actual sealed artifact before any loader call.
    pub fn retain(file: OwnedFd, expected: [u8; 32]) -> Self {
        Self {
            file,
            expected,
            api: None,
            context: std::ptr::null_mut(),
            last: None,
            refusal: None,
            lease_attempt: None,
            unopened_attempt: None,
            runtime_attempt: None,
            last_runtime: None,
            controls_observation: None,
            pre_open_alias_retirement: None,
            lease_io: std::ptr::null_mut(),
            lease_owner: std::ptr::null_mut(),
            _single_thread: std::marker::PhantomData,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refusal.get_or_insert_with(|| error.to_string());
        }
        result
    }
    /// # Safety
    /// The caller has authenticated this exact bridge/package/dependency byte
    /// closure. dlopen executes code; a digest supplied by an untrusted peer is
    /// not authorization. The supplied descriptor must be the sealed snapshot.
    pub unsafe fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            require(
                self.api.is_none() && self.context.is_null() && self.refusal.is_none(),
                "native bridge cannot initialize twice",
            )?;
            let handle = unsafe { open_sealed_bridge(self.file.as_fd(), self.expected) }?;
            // Resolve all exact public symbols before native allocation. On a
            // resolution/ABI failure no grouped resource exists yet.
            let resolved = (|| {
                macro_rules! symbol {
                    ($name:literal,$ty:ty) => {{
                        unsafe { libc::dlerror() };
                        let pointer = unsafe {
                            libc::dlsym(handle.as_ptr(), concat!($name, "\0").as_ptr().cast())
                        };
                        if pointer.is_null() {
                            return Err(loader_error());
                        }
                        unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
                    }};
                }
                let abi = symbol!("hermit_grouped_broker_abi", unsafe extern "C" fn() -> u32);
                require(unsafe { abi() } == 1, "grouped bridge ABI differs")?;
                Ok(Api {
                    handle,
                    alloc: symbol!(
                        "hermit_grouped_broker_alloc",
                        unsafe extern "C" fn(*mut *mut c_void) -> c_int
                    ),
                    source: symbol!("hermit_grouped_broker_source", Start),
                    create: symbol!(
                        "hermit_grouped_broker_create",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                    successor: symbol!("hermit_grouped_broker_successor", Successor),
                    runtime_controls: symbol!(
                        "hermit_grouped_broker_runtime_controls",
                        unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int
                    ),
                    runtime_journal: symbol!(
                        "hermit_grouped_broker_runtime_journal",
                        unsafe extern "C" fn(
                            *mut c_void,
                            super::cleanup_native::Journal,
                            *mut c_void,
                        ) -> c_int
                    ),
                    runtime_status: symbol!(
                        "hermit_grouped_broker_runtime_status",
                        unsafe extern "C" fn(*const c_void, *mut RuntimeHandoffStatus) -> c_int
                    ),
                    retire_unopened: symbol!(
                        "hermit_grouped_broker_retire_unopened",
                        unsafe extern "C" fn(*mut c_void, u64, u64) -> c_int
                    ),
                    provider_lease: symbol!(
                        "hermit_grouped_broker_provider_lease",
                        unsafe extern "C" fn(
                            *mut c_void,
                            u64,
                            *mut *mut c_void,
                            *mut *mut c_void,
                        ) -> c_int
                    ),
                    status: symbol!(
                        "hermit_grouped_broker_status",
                        unsafe extern "C" fn(*const c_void, *mut Status) -> c_int
                    ),
                    release: symbol!(
                        "hermit_grouped_broker_release_aliases",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                    free: symbol!(
                        "hermit_grouped_broker_free",
                        unsafe extern "C" fn(*mut c_void) -> c_int
                    ),
                })
            })();
            match resolved {
                Ok(api) => self.api = Some(api),
                Err(error) => {
                    unsafe { libc::dlclose(handle.as_ptr()) };
                    return Err(error);
                }
            }
            // C stores directly in the externally retained slot. No local
            // successful-return constructor can lose partial native custody.
            let raw = unsafe { (self.api.as_ref().unwrap().alloc)(&mut self.context) };
            if raw != 0 {
                return Err(io::Error::last_os_error());
            }
            require(!self.context.is_null(), "bridge allocated no context")?;
            self.status()?;
            Ok(())
        })();
        self.remember(result)
    }
    fn ready(&self) -> io::Result<&Api> {
        require(
            self.refusal.is_none() && !self.context.is_null(),
            "native bridge is absent or refused",
        )?;
        self.api
            .as_ref()
            .ok_or_else(|| io::Error::other("bridge API is absent"))
    }
    pub fn status(&mut self) -> io::Result<Status> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| io::Error::other("bridge API is absent"))?;
        require(!self.context.is_null(), "native context is absent")?;
        let mut status = Status::default();
        let raw = unsafe { (api.status)(self.context, &mut status) };
        if raw != 0 {
            return Err(io::Error::last_os_error());
        }
        self.last = Some(status);
        require(status.abi == 1, "native status ABI differs")?;
        Ok(status)
    }
    pub fn source(
        &mut self,
        channel: BorrowedFd<'_>,
        incarnation: u64,
        nonce: &CStr,
        bounds: (u64, u64),
        unit: &CStr,
        controls: [BorrowedFd<'_>; 3],
    ) -> io::Result<()> {
        let (deadline, cutoff) = bounds;
        let result = (|| {
            let call = self.ready()?.source;
            let fds = controls.map(|fd| fd.as_raw_fd());
            let raw = unsafe {
                call(
                    self.context,
                    channel.as_raw_fd(),
                    incarnation,
                    nonce.as_ptr(),
                    deadline,
                    cutoff,
                    unit.as_ptr(),
                    fds.as_ptr(),
                )
            };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.status()?;
            if let Some(error) = error {
                return Err(error);
            }
            Ok(())
        })();
        self.remember(result)
    }
    pub fn create(&mut self) -> io::Result<()> {
        let result = (|| {
            let call = self.ready()?.create;
            let raw = unsafe { call(self.context) };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.status()?;
            if let Some(error) = error {
                return Err(error);
            }
            Ok(())
        })();
        self.remember(result)
    }
    pub fn successor(
        &mut self,
        channel: BorrowedFd<'_>,
        incarnation: u64,
        nonce: &CStr,
        bounds: (u64, u64),
        unit: &CStr,
        leaves: [BorrowedFd<'_>; 3],
    ) -> io::Result<()> {
        let (deadline, cutoff) = bounds;
        let result = (|| {
            let call = self.ready()?.successor;
            let fds = leaves.map(|fd| fd.as_raw_fd());
            let raw = unsafe {
                call(
                    self.context,
                    channel.as_raw_fd(),
                    incarnation,
                    nonce.as_ptr(),
                    deadline,
                    cutoff,
                    unit.as_ptr(),
                    fds.as_ptr(),
                    digest,
                )
            };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.status()?;
            if let Some(error) = error {
                return Err(error);
            }
            Ok(())
        })();
        self.remember(result)
    }
    /// Borrow the three actual native control descriptions for SCM/KCMP
    /// comparison. The exclusive borrow prevents alias retirement while these
    /// descriptors are in use. This creates no provider or cursor authority.
    pub(super) fn runtime_control_descriptors(&mut self) -> io::Result<[BorrowedFd<'_>; 3]> {
        let result = (|| {
            let call = self.ready()?.runtime_controls;
            let mut fds = [-1; 3];
            let raw = unsafe { call(self.context, fds.as_mut_ptr()) };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.controls_observation = Some((
                NativeCall {
                    returned: raw,
                    errno: error.as_ref().and_then(io::Error::raw_os_error),
                },
                fds,
            ));
            if let Some(error) = error {
                return Err(error);
            }
            require(
                fds.iter().all(|fd| *fd >= 0),
                "native runtime control slot is absent",
            )?;
            Ok(fds)
        })();
        let fds = self.remember(result)?;
        // SAFETY: authenticated C returned its continuously owned slots, and
        // this exclusive Bridge borrow pins their owner for the returned view.
        Ok(fds.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) }))
    }
    /// # Safety
    /// Only RuntimeCleanup's actual linear handoff may call this. It has
    /// already retained both durable histories, original controls, exclusive
    /// cursor and live replacement peer. context is stable and lives inside
    /// that retained owner, including on Err. Natural startup joins still must
    /// complete before that owner exposes provider_ready.
    pub(super) unsafe fn install_runtime_journal(
        &mut self,
        journal: super::cleanup_native::Journal,
        context: *mut c_void,
    ) -> io::Result<()> {
        let result = (|| {
            require(
                self.runtime_attempt.is_none() && self.lease_attempt.is_none(),
                "runtime journal transfer is one-use before provider lease",
            )?;
            let call = self.ready()?.runtime_journal;
            let raw = unsafe { call(self.context, journal, context) };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.runtime_attempt = Some((raw, error.as_ref().and_then(io::Error::raw_os_error)));
            let observed = self.runtime_status();
            if let Some(error) = error {
                return Err(error);
            }
            let status = observed?;
            require(
                status.installed == 1
                    && status.aliases_released == 1
                    && status.attempted == 0x1ff
                    && status.returned == 0x1ff
                    && (0..9).all(|row| {
                        status.attempted & (1 << row) == 0
                            || (status.raw[row] == 0 && status.error[row] == 0)
                    }),
                "native runtime handoff retained an incomplete local close",
            )
        })();
        self.remember(result)
    }
    pub(super) fn runtime_status(&mut self) -> io::Result<RuntimeHandoffStatus> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| io::Error::other("bridge API is absent"))?;
        require(!self.context.is_null(), "native context is absent")?;
        let mut status = RuntimeHandoffStatus::default();
        let raw = unsafe { (api.runtime_status)(self.context, &mut status) };
        if raw != 0 {
            return Err(io::Error::last_os_error());
        }
        self.last_runtime = Some(status);
        Ok(status)
    }
    /// Only the actual C successor can expose these pointers. Keeping this
    /// exclusive borrow prevents alias release, context movement and another
    /// operation while the open call borrows them. LEAVES is not runtime Ready.
    pub(in crate::network_runtime) fn provider_lease(
        &mut self,
        incarnation: u64,
    ) -> io::Result<ProviderLease<'_>> {
        let result = (|| {
            require(self.lease_attempt.is_none(), "provider lease is one-use")?;
            let call = self.ready()?.provider_lease;
            let raw = unsafe {
                call(
                    self.context,
                    incarnation,
                    &mut self.lease_io,
                    &mut self.lease_owner,
                )
            };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.lease_attempt = Some((raw, error.as_ref().and_then(io::Error::raw_os_error)));
            let observed = self.status();
            if let Some(error) = error {
                return Err(error);
            }
            observed?;
            require(
                !self.lease_io.is_null() && !self.lease_owner.is_null(),
                "native lease returned no original context",
            )
        })();
        self.remember(result)?;
        Ok(ProviderLease { bridge: self })
    }
    pub(in crate::network_runtime) fn lease_attempt(&self) -> Option<(c_int, Option<c_int>)> {
        self.lease_attempt
    }
    /// # Safety
    /// The joint owner holds actual controller terminal custody, the real live
    /// runtime cleanup peer, exclusive cursor and the immutable original bound.
    /// C independently refuses if the original provider lease was ever issued.
    pub(in crate::network_runtime) unsafe fn retire_unopened(
        &mut self,
        release_start: u64,
        cutoff: u64,
    ) -> io::Result<NativeCall> {
        let result = (|| {
            require(
                self.unopened_attempt.is_none() && self.lease_attempt.is_none(),
                "unopened retirement cannot follow a pointer lease or retry",
            )?;
            let call = self.ready()?.retire_unopened;
            let raw = unsafe { call(self.context, release_start, cutoff) };
            let error = (raw != 0).then(io::Error::last_os_error);
            let call = NativeCall {
                returned: raw,
                errno: error.as_ref().and_then(io::Error::raw_os_error),
            };
            self.unopened_attempt = Some(call);
            let observed = self.status();
            if let Some(error) = error {
                return Err(error);
            }
            observed?;
            Ok(call)
        })();
        self.remember(result)
    }
    pub(in crate::network_runtime) fn unopened_attempt(&self) -> Option<NativeCall> {
        self.unopened_attempt
    }
    /// Retire only this retained successor's local aliases before any provider
    /// lease. The real independent Keeper owns deletion; this method never calls
    /// retire_unopened and deliberately retains the native allocation/DSO.
    pub(in crate::network_runtime) fn retire_pre_open_aliases(&mut self) -> io::Result<()> {
        require(
            self.pre_open_alias_retirement.is_none()
                && self.lease_attempt.is_none()
                && self.unopened_attempt.is_none()
                && self.lease_io.is_null()
                && self.lease_owner.is_null(),
            "pre-open local retirement cannot follow a lease, deletion or repeat",
        )?;
        self.pre_open_alias_retirement = Some(PreOpenAliasRetirement {
            allocation_retained: !self.context.is_null(),
            attempted: false,
            returned: None,
            observed: None,
            readback_error: None,
        });
        // Allocation never succeeded: retained loader state remains owned. No
        // native close or invented successful C result is recorded.
        if self.context.is_null() {
            return Ok(());
        }
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| io::Error::other("allocated bridge API missing"))?;
        self.pre_open_alias_retirement.as_mut().unwrap().attempted = true;
        let raw = unsafe { (api.release)(self.context) };
        let error = (raw != 0).then(io::Error::last_os_error);
        self.pre_open_alias_retirement.as_mut().unwrap().returned = Some(NativeCall {
            returned: raw,
            errno: error.as_ref().and_then(io::Error::raw_os_error),
        });
        // Capture the primary return before the separate descriptive readback.
        let observed = self.status();
        match &observed {
            Ok(status) => self.pre_open_alias_retirement.as_mut().unwrap().observed = Some(*status),
            Err(error) => {
                self.pre_open_alias_retirement
                    .as_mut()
                    .unwrap()
                    .readback_error = Some(error.to_string())
            }
        }
        if let Some(error) = error {
            return self.remember(Err(error));
        }
        let status = observed?;
        require(
            status.aliases_released == 1,
            "pre-open aliases remain retained",
        )
    }
    pub(in crate::network_runtime) fn pre_open_alias_retirement(
        &self,
    ) -> Option<&PreOpenAliasRetirement> {
        self.pre_open_alias_retirement.as_ref()
    }
    /// Explicit local alias closure is allowed after refusal. It does not clear
    /// the original error, delete a definition or imply external terminality.
    pub fn release_aliases(&mut self) -> io::Result<()> {
        let result = (|| {
            let api = self
                .api
                .as_ref()
                .ok_or_else(|| io::Error::other("bridge API is absent"))?;
            require(!self.context.is_null(), "native context is absent")?;
            let raw = unsafe { (api.release)(self.context) };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.status()?;
            if let Some(error) = error {
                return Err(error);
            }
            Ok(())
        })();
        self.remember(result)
    }
    pub fn finish(&mut self) -> io::Result<()> {
        let status = self.status()?;
        require(
            status.aliases_released == 1,
            "native aliases have not been released",
        )?;
        let api = self.api.as_ref().unwrap();
        if unsafe { (api.free)(self.context) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.context = std::ptr::null_mut();
        let api = self.api.take().unwrap();
        if unsafe { libc::dlclose(api.handle.as_ptr()) } != 0 {
            return Err(loader_error());
        }
        Ok(())
    }
}
// No destructor invents release completion. The early helper's recovery scope
// explicitly releases aliases and retains original errors. If it is killed,
// kernel process exit closes its aliases while both independent holders retain
// the originals and the journal that precedes every possible global write.

/// Private borrowing authority for one grouped open, never serializable or
/// constructible from Status. The outer joint owner must retain Bridge and the
/// authenticated provider library through every returned native pointer.
pub(in crate::network_runtime) struct ProviderLease<'a> {
    bridge: &'a mut Bridge,
}
impl ProviderLease<'_> {
    /// # Safety
    /// Only the retained joint provider owner may use these in the exact
    /// authenticated grouped ABI. It may retain the pair only inside the same
    /// owner as this Bridge, and must not release aliases/free/unload Bridge
    /// while a native session or original cleanup obligation remains.
    pub(in crate::network_runtime) unsafe fn parts(&mut self) -> (*mut c_void, *mut c_void) {
        (self.bridge.lease_io, self.bridge.lease_owner)
    }
}
