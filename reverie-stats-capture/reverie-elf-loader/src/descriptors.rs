/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Inactive private descriptor transactions for exec preparation.
//!
//! Reservation does not inspect the executable pathname. It creates a memfd
//! and duplicates it into the lowest available descriptor numbers. Descriptor
//! zero in the reservation is an anchor; it is never released while a slot can
//! need restoration. Every close and flag change concerns an owned descriptor.
//! Guest descriptor numbers, open file descriptions, and flags are untouched.
//!
//! Closing a placeholder immediately before an ordinary open requires exclusive
//! access to the process descriptor table. The unsafe methods state this caller
//! obligation explicitly. A reservation is neither `Send` nor `Sync`; an
//! isolated preparation child is the intended caller. None of this module is
//! selected by a production launcher or by the existing exec refusal gate.

use std::ffi::c_char;
use std::fmt;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::rc::Rc;

/// Total reserved descriptors, including the anchor and reusable scratch slot.
pub const START_DESCRIPTOR_SLOTS: usize = 16;
/// A bound on bookkeeping, not a bound on guest descriptor numbers.
pub const MAX_DESCRIPTOR_SLOTS: usize = 64;

/// A failed attempt to restore the flags of an owned private descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorUndoFailure {
    pub fd: RawFd,
    pub errno: i32,
}

/// Private launcher failures. None of these errors is a native exec errno.
#[derive(Debug)]
pub enum DescriptorError {
    InvalidSlotCount {
        requested: usize,
    },
    InvalidFileCount {
        requested: usize,
    },
    Capacity {
        requested: usize,
        reserved: usize,
        errno: i32,
    },
    InvalidSlot {
        index: usize,
    },
    AnchorSlot,
    VacantSlot {
        index: usize,
    },
    DuplicateSlot {
        index: usize,
    },
    DuplicateFd {
        fd: RawFd,
    },
    Open {
        index: usize,
        error: io::Error,
    },
    UnexpectedFd {
        expected: RawFd,
        actual: RawFd,
    },
    FlagRead {
        fd: RawFd,
        errno: i32,
    },
    FlagChange {
        fd: RawFd,
        errno: i32,
        undo_failures: Vec<DescriptorUndoFailure>,
    },
    Restore {
        cause: Box<DescriptorError>,
        failures: Vec<DescriptorUndoFailure>,
    },
    Undo {
        failures: Vec<DescriptorUndoFailure>,
    },
}

impl DescriptorError {
    /// The underlying private operation's errno, for diagnostics only.
    ///
    /// In particular, `EMFILE` here must become a launcher refusal rather than
    /// a fabricated `NativeErrno(EMFILE)`: native exec requires no user FD.
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Capacity { errno, .. }
            | Self::FlagRead { errno, .. }
            | Self::FlagChange { errno, .. } => Some(*errno),
            Self::Open { error, .. } => error.raw_os_error(),
            Self::Restore { cause, .. } => cause.raw_os_error(),
            Self::Undo { failures } => failures.first().map(|failure| failure.errno),
            _ => None,
        }
    }
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlotCount { requested } => write!(
                formatter,
                "private descriptor reservation requires 2..={MAX_DESCRIPTOR_SLOTS} slots, got {requested}"
            ),
            Self::InvalidFileCount { requested } => write!(
                formatter,
                "private file transfer accepts at most {MAX_DESCRIPTOR_SLOTS} files, got {requested}"
            ),
            Self::Capacity {
                requested,
                reserved,
                errno,
            } => write!(
                formatter,
                "private descriptor capacity: reserved {reserved} of {requested}, errno {errno}"
            ),
            Self::InvalidSlot { index } => write!(formatter, "unknown private slot {index}"),
            Self::AnchorSlot => write!(
                formatter,
                "the private descriptor anchor cannot be released"
            ),
            Self::VacantSlot { index } => {
                write!(formatter, "private slot {index} has been transferred")
            }
            Self::DuplicateSlot { index } => {
                write!(formatter, "private slot {index} appears twice")
            }
            Self::DuplicateFd { fd } => {
                write!(formatter, "private file fd {fd} appears twice")
            }
            Self::Open { index, error } => {
                write!(formatter, "open into private slot {index}: {error}")
            }
            Self::UnexpectedFd { expected, actual } => write!(
                formatter,
                "private slot allocation changed: expected fd {expected}, got fd {actual}"
            ),
            Self::FlagRead { fd, errno } => {
                write!(
                    formatter,
                    "reading private fd {fd} flags failed with errno {errno}"
                )
            }
            Self::FlagChange {
                fd,
                errno,
                undo_failures,
            } => write!(
                formatter,
                "changing private fd {fd} flags failed with errno {errno}; {} undo failures",
                undo_failures.len()
            ),
            Self::Restore { cause, failures } => write!(
                formatter,
                "{cause}; {} private slot restoration failures",
                failures.len()
            ),
            Self::Undo { failures } => {
                write!(
                    formatter,
                    "{} private descriptor flag undo failures",
                    failures.len()
                )
            }
        }
    }
}

impl std::error::Error for DescriptorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { error, .. } => Some(error),
            Self::Restore { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

/// Native ordinary-open errors are distinct from private reservation failures.
///
/// Unlike exec, an ordinary open needs an unused caller descriptor. An
/// `EMFILE` from reserving that exact guest slot is therefore native. A private
/// exec preparation reservation still reports [`DescriptorError::Capacity`].
#[derive(Debug)]
pub enum OrdinaryOpenError {
    NativeErrno(i32),
    Private(DescriptorError),
}

impl fmt::Display for OrdinaryOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeErrno(errno) => {
                write!(formatter, "native ordinary-open errno {errno}")
            }
            Self::Private(error) => write!(formatter, "ordinary-open private transaction: {error}"),
        }
    }
}

impl std::error::Error for OrdinaryOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Private(error) => Some(error),
            Self::NativeErrno(_) => None,
        }
    }
}

/// The lowest unused guest descriptor, held before pathname classification.
///
/// This inactive building block establishes FD ordering only. Its caller must
/// copy and validate the open arguments in native order first, acknowledge and
/// bound the classification attempt, and admit the pathname/object separately.
/// Dropping the value cancels the allocation and closes only its placeholder.
#[derive(Debug)]
pub struct OrdinaryOpenSlot {
    number: RawFd,
    placeholder: Option<File>,
    _single_owner: PhantomData<Rc<()>>,
}

impl OrdinaryOpenSlot {
    /// Hold the kernel's lowest available number before any target lookup.
    ///
    /// `seed` must already exist: creating a new seed could consume the only
    /// free guest slot. A live private descriptor anchor is suitable. The
    /// duplicate performs no pathname lookup and changes none of seed's flags,
    /// contents, or shared OFD position.
    ///
    /// # Safety
    ///
    /// The caller must own exclusive descriptor-table mutation rights from
    /// this reservation through classification and [`Self::open`], including
    /// signal handlers. It must already have copied/validated the ordinary-open
    /// arguments in native order. `seed` must remain a valid owned descriptor.
    pub unsafe fn reserve(seed: &File) -> Result<Self, OrdinaryOpenError> {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // alloc_fd(0, nofile) returns EMFILE when nofile is zero, before any
        // target lookup. f_dupfd rejects its minimum >= nofile with EINVAL
        // instead. Preserve ordinary-open semantics at that distinct edge;
        // a retained seed may still be valid above the lowered soft limit.
        // SAFETY: limit is the writable native structure; this query opens
        // no descriptor and touches no pathname.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } < 0 {
            return Err(OrdinaryOpenError::Private(DescriptorError::Capacity {
                requested: 1,
                reserved: 0,
                errno: errno(&io::Error::last_os_error()),
            }));
        }
        if limit.rlim_cur == 0 {
            return Err(OrdinaryOpenError::NativeErrno(libc::EMFILE));
        }
        // Native get_unused_fd_flags and this duplication share alloc_fd's
        // lowest-free search for a positive limit. A valid seed and minimum
        // zero cannot add EBADF/EINVAL before a genuine full-table EMFILE.
        let placeholder = duplicate_kernel_cloexec(seed.as_raw_fd(), 0).map_err(|error| {
            if error.raw_os_error() == Some(libc::EMFILE) {
                OrdinaryOpenError::NativeErrno(libc::EMFILE)
            } else {
                OrdinaryOpenError::Private(DescriptorError::Capacity {
                    requested: 1,
                    reserved: 0,
                    errno: errno(&error),
                })
            }
        })?;
        Ok(Self {
            number: placeholder.as_raw_fd(),
            placeholder: Some(placeholder),
            _single_owner: PhantomData,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.number
    }

    /// Release the reserved number immediately before the unchanged native open.
    ///
    /// Guest CLOEXEC/status flags are exactly those set by `open`: this method
    /// does not force private CLOEXEC flags onto the returned guest descriptor.
    /// The native result is forwarded only after the actual kernel operation;
    /// a changed allocation number remains a private transaction failure.
    ///
    /// # Safety
    ///
    /// [`Self::reserve`]'s exclusive descriptor-table contract must still hold.
    /// `open` must be the original native open operation with the already
    /// validated inputs, allocate only its returned owner, and modify no other
    /// guest descriptor. The caller must have completed its admission and
    /// bounded classification before invoking this method.
    pub unsafe fn open<F>(
        mut self,
        open: impl FnOnce() -> io::Result<F>,
    ) -> Result<File, OrdinaryOpenError>
    where
        F: Into<File>,
    {
        drop(self.placeholder.take());
        let file: File = open().map(Into::into).map_err(|error| {
            if let Some(errno) = error.raw_os_error() {
                OrdinaryOpenError::NativeErrno(errno)
            } else {
                OrdinaryOpenError::Private(DescriptorError::Open { index: 0, error })
            }
        })?;
        let actual = file.as_raw_fd();
        if actual != self.number {
            drop(file);
            return Err(OrdinaryOpenError::Private(DescriptorError::UnexpectedFd {
                expected: self.number,
                actual,
            }));
        }
        Ok(file)
    }
}

#[derive(Debug)]
struct Slot {
    number: RawFd,
    file: Option<File>,
}

/// Owned private slots reserved before target or interpreter lookup.
#[derive(Debug)]
pub struct DescriptorReservation {
    slots: Vec<Slot>,
    // This is a process FD-table transaction, not an independently movable
    // resource pool. Unsafe calls still require serialization against signals
    // and other threads that might allocate or close descriptors.
    _single_owner: PhantomData<Rc<()>>,
}

impl DescriptorReservation {
    /// Reserve the lowest available numbers without accessing any pathname.
    ///
    /// Failure drops only the resources allocated by this call. Slot 0 is the
    /// retained placeholder anchor, so the count must include it.
    pub fn reserve(count: usize) -> Result<Self, DescriptorError> {
        if !(2..=MAX_DESCRIPTOR_SLOTS).contains(&count) {
            return Err(DescriptorError::InvalidSlotCount { requested: count });
        }
        let anchor = new_anchor().map_err(|error| DescriptorError::Capacity {
            requested: count,
            reserved: 0,
            errno: errno(&error),
        })?;
        let anchor_fd = anchor.as_raw_fd();
        let mut reservation = Self {
            slots: Vec::with_capacity(count),
            _single_owner: PhantomData,
        };
        reservation.slots.push(Slot {
            number: anchor_fd,
            file: Some(anchor),
        });
        while reservation.slots.len() < count {
            let file =
                duplicate_cloexec(anchor_fd, 0).map_err(|error| DescriptorError::Capacity {
                    requested: count,
                    reserved: reservation.slots.len(),
                    errno: errno(&error),
                })?;
            reservation.slots.push(Slot {
                number: file.as_raw_fd(),
                file: Some(file),
            });
        }
        Ok(reservation)
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// The anchor is private capacity, never a program or interpreter slot.
    pub fn anchor_slot(&self) -> usize {
        0
    }

    /// The final reserved slot is the default reusable scratch slot.
    pub fn scratch_slot(&self) -> usize {
        self.slots.len() - 1
    }

    /// The current owned descriptor in a slot, if it has not been transferred.
    pub fn fd(&self, index: usize) -> Option<RawFd> {
        self.file(index).map(AsRawFd::as_raw_fd)
    }

    /// Borrow a pinned or readable file without duplicating its descriptor.
    pub fn file(&self, index: usize) -> Option<&File> {
        self.slots.get(index).and_then(|slot| slot.file.as_ref())
    }

    /// Enumerate exactly the currently owned private descriptors.
    pub fn private_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.slots
            .iter()
            .filter_map(|slot| slot.file.as_ref().map(AsRawFd::as_raw_fd))
    }

    /// Transfer ownership of an opened slot into the prepared start.
    ///
    /// The reservation never subsequently closes the transferred file. The
    /// anchor remains owned until the entire reservation is dropped.
    pub fn take_file(&mut self, index: usize) -> Result<File, DescriptorError> {
        self.validate_occupied(index)?;
        Ok(self.slots[index].file.take().expect("validated owned slot"))
    }

    /// Release an owned placeholder immediately before a single ordinary open.
    ///
    /// The open must return the released number. On failure, the slot is
    /// re-reserved by duplicating the retained anchor with that number as the
    /// allocation minimum. This never uses `dup2` to overwrite another FD.
    /// Newly opened private resources are kept CLOEXEC until explicit transfer.
    ///
    /// # Safety
    ///
    /// The caller must serialize the descriptor table against every other
    /// allocation, close, and exec, including signal handlers, throughout this
    /// call. `open` must perform exactly one FD allocation and return its owner;
    /// it must not close or modify any guest descriptor. A single-threaded
    /// preparation helper with nonallocating signal handlers satisfies this
    /// contract. The released slot must be the lowest available FD for `open`.
    pub unsafe fn open_into<F>(
        &mut self,
        index: usize,
        open: impl FnOnce() -> io::Result<F>,
    ) -> Result<RawFd, DescriptorError>
    where
        F: Into<File>,
    {
        self.validate_occupied(index)?;
        let expected = self.slots[index].number;
        drop(self.slots[index].file.take());
        let opened = match open() {
            Ok(file) => file.into(),
            Err(error) => {
                return Err(
                    self.restore_after_failure(&[index], DescriptorError::Open { index, error })
                );
            }
        };
        let actual = opened.as_raw_fd();
        if actual != expected {
            drop(opened);
            return Err(self.restore_after_failure(
                &[index],
                DescriptorError::UnexpectedFd { expected, actual },
            ));
        }
        if let Err(error) = keep_cloexec(actual) {
            drop(opened);
            return Err(self.restore_after_failure(
                &[index],
                DescriptorError::FlagChange {
                    fd: actual,
                    errno: errno(&error),
                    undo_failures: Vec::new(),
                },
            ));
        }
        self.slots[index].file = Some(opened);
        Ok(actual)
    }

    /// Allocate a socket pair using two already reserved private slots.
    ///
    /// This supports connection and failure-channel creation with an otherwise
    /// full table, without allocating an unreserved descriptor.
    ///
    /// # Safety
    ///
    /// The descriptor-table serialization contract of [`Self::open_into`]
    /// applies. The closure must allocate exactly the two returned descriptors
    /// and leave guest descriptors unchanged. The two released slots must be
    /// the lowest available numbers at the allocation.
    pub unsafe fn open_pair_into<F>(
        &mut self,
        first: usize,
        second: usize,
        open: impl FnOnce() -> io::Result<(F, F)>,
    ) -> Result<(RawFd, RawFd), DescriptorError>
    where
        F: Into<File>,
    {
        self.validate_occupied(first)?;
        self.validate_occupied(second)?;
        if first == second {
            return Err(DescriptorError::DuplicateSlot { index: first });
        }
        let first_number = self.slots[first].number;
        let second_number = self.slots[second].number;
        drop(self.slots[first].file.take());
        drop(self.slots[second].file.take());
        let (one, two) = match open() {
            Ok((one, two)) => (one.into(), two.into()),
            Err(error) => {
                return Err(self.restore_after_failure(
                    &[first, second],
                    DescriptorError::Open {
                        index: first,
                        error,
                    },
                ));
            }
        };
        let (first_file, second_file) =
            if one.as_raw_fd() == first_number && two.as_raw_fd() == second_number {
                (one, two)
            } else if two.as_raw_fd() == first_number && one.as_raw_fd() == second_number {
                (two, one)
            } else {
                let (expected, actual) =
                    if one.as_raw_fd() != first_number && two.as_raw_fd() != first_number {
                        (first_number, one.as_raw_fd())
                    } else {
                        (
                            second_number,
                            if one.as_raw_fd() == first_number {
                                two.as_raw_fd()
                            } else {
                                one.as_raw_fd()
                            },
                        )
                    };
                drop((one, two));
                return Err(self.restore_after_failure(
                    &[first, second],
                    DescriptorError::UnexpectedFd { expected, actual },
                ));
            };
        for fd in [first_number, second_number] {
            if let Err(error) = keep_cloexec(fd) {
                drop((first_file, second_file));
                return Err(self.restore_after_failure(
                    &[first, second],
                    DescriptorError::FlagChange {
                        fd,
                        errno: errno(&error),
                        undo_failures: Vec::new(),
                    },
                ));
            }
        }
        self.slots[first].file = Some(first_file);
        self.slots[second].file = Some(second_file);
        Ok((first_number, second_number))
    }

    /// Close an owned scratch resource and restore its reserved placeholder.
    ///
    /// # Safety
    ///
    /// The caller must serialize descriptor-table mutations as specified by
    /// [`Self::open_into`]. It must not retain uses of the closed scratch OFD.
    pub unsafe fn restore_placeholder(&mut self, index: usize) -> Result<RawFd, DescriptorError> {
        self.validate_occupied(index)?;
        drop(self.slots[index].file.take());
        self.restore_slot(index)
    }

    /// Reuse a scratch slot by restoring its private placeholder.
    ///
    /// # Safety
    ///
    /// The serialization and borrower requirements of
    /// [`Self::restore_placeholder`] apply.
    pub unsafe fn reset(&mut self, index: usize) -> Result<RawFd, DescriptorError> {
        // SAFETY: the caller promises restore_placeholder's same contract.
        unsafe { self.restore_placeholder(index) }
    }

    /// Reversibly clear CLOEXEC for the specified owned private resources.
    ///
    /// Every original descriptor flag word is recorded before the first change.
    /// An error undoes completed changes and reports any undo failures explicitly.
    pub fn transfer(
        &mut self,
        indices: &[usize],
    ) -> Result<PrivateFdTransfer<'_>, DescriptorError> {
        let mut original = Vec::with_capacity(indices.len());
        for (position, &index) in indices.iter().enumerate() {
            self.validate_occupied(index)?;
            if indices[..position].contains(&index) {
                return Err(DescriptorError::DuplicateSlot { index });
            }
            let fd = self.slots[index].number;
            let flags = get_fd_flags(fd).map_err(|error| DescriptorError::FlagRead {
                fd,
                errno: errno(&error),
            })?;
            original.push((fd, flags));
        }
        let changed = clear_cloexec(&original)?;
        Ok(PrivateFdTransfer {
            reservation: self,
            original: changed,
            active: true,
        })
    }

    fn validate_occupied(&self, index: usize) -> Result<(), DescriptorError> {
        if index == self.anchor_slot() {
            return Err(DescriptorError::AnchorSlot);
        }
        let slot = self
            .slots
            .get(index)
            .ok_or(DescriptorError::InvalidSlot { index })?;
        if slot.file.is_none() {
            return Err(DescriptorError::VacantSlot { index });
        }
        Ok(())
    }

    fn restore_slot(&mut self, index: usize) -> Result<RawFd, DescriptorError> {
        let expected = self.slots[index].number;
        let anchor = self.slots[0]
            .file
            .as_ref()
            .expect("the anchor cannot be transferred")
            .as_raw_fd();
        let file =
            duplicate_cloexec(anchor, expected).map_err(|error| DescriptorError::Capacity {
                requested: self.slots.len(),
                reserved: self.private_fds().count(),
                errno: errno(&error),
            })?;
        let actual = file.as_raw_fd();
        if actual != expected {
            drop(file);
            return Err(DescriptorError::UnexpectedFd { expected, actual });
        }
        self.slots[index].file = Some(file);
        Ok(actual)
    }

    fn restore_after_failure(
        &mut self,
        indices: &[usize],
        cause: DescriptorError,
    ) -> DescriptorError {
        let mut failures = Vec::new();
        for &index in indices {
            if let Err(error) = self.restore_slot(index) {
                failures.push(DescriptorUndoFailure {
                    fd: self.slots[index].number,
                    errno: error.raw_os_error().unwrap_or(libc::EIO),
                });
            }
        }
        if failures.is_empty() {
            cause
        } else {
            DescriptorError::Restore {
                cause: Box::new(cause),
                failures,
            }
        }
    }
}

/// An explicit undo record for the private non-CLOEXEC transfer flags.
#[derive(Debug)]
pub struct PrivateFdTransfer<'a> {
    reservation: &'a mut DescriptorReservation,
    original: Vec<(RawFd, i32)>,
    active: bool,
}

impl PrivateFdTransfer<'_> {
    /// The complete owned private set, including resources not transferred.
    pub fn private_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.reservation.private_fds()
    }

    /// Restore flags and expose every failure to the caller before cancellation.
    ///
    /// This consumes the undo record. Drop will not silently retry a reported
    /// failure or obscure whether explicit cancellation completed.
    pub fn rollback(mut self) -> Result<(), DescriptorError> {
        self.active = false;
        let failures = restore_flags(&self.original);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DescriptorError::Undo { failures })
        }
    }

    /// Retain the private non-CLOEXEC flags for a subsequent exec.
    ///
    /// This does not execute a program or transfer ownership of any descriptor.
    pub fn commit(mut self) {
        self.active = false;
    }
}

impl Drop for PrivateFdTransfer<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_flags(&self.original);
            self.active = false;
        }
    }
}

/// Undoable non-CLOEXEC flags for private files retained outside a reservation.
///
/// In particular, a prepared start owns its readable T/I files after taking
/// their slots. Borrowing those actual owners keeps them alive throughout this
/// transaction. No descriptor number is opened, duplicated, closed or replaced.
#[derive(Debug)]
pub struct PrivateFileTransfer<'a> {
    files: Vec<&'a File>,
    original: Vec<(RawFd, i32)>,
    active: bool,
    _single_owner: PhantomData<Rc<()>>,
}

/// Reversibly clear CLOEXEC on borrowed, caller-owned private files.
///
/// All flag words and FD uniqueness are checked before the first change. A
/// failure restores completed changes and returns explicit undo failures. This
/// creates no FD and works with a full user descriptor table. Errors are private
/// transaction failures, never native exec errors. The manifest/consumer remain
/// inactive; this operation changes flags only.
///
/// # Safety
///
/// Every supplied file must own a private launcher descriptor, never a guest
/// descriptor. The caller must serialize the process FD table and these flags
/// against all other threads and signal handlers until commit/rollback or guard
/// drop. No owner may be closed or replaced through raw FD operations. File
/// borrows keep the Rust owners live; this guard is neither `Send` nor `Sync`.
pub unsafe fn transfer_private_files<'a>(
    files: &[&'a File],
) -> Result<PrivateFileTransfer<'a>, DescriptorError> {
    if files.len() > MAX_DESCRIPTOR_SLOTS {
        return Err(DescriptorError::InvalidFileCount {
            requested: files.len(),
        });
    }
    let mut original = Vec::with_capacity(files.len());
    for file in files {
        let fd = file.as_raw_fd();
        if original.iter().any(|&(previous, _)| previous == fd) {
            return Err(DescriptorError::DuplicateFd { fd });
        }
        let flags = get_fd_flags(fd).map_err(|error| DescriptorError::FlagRead {
            fd,
            errno: errno(&error),
        })?;
        original.push((fd, flags));
    }
    let borrowed_files = files.to_vec();
    let changed = clear_cloexec(&original)?;
    Ok(PrivateFileTransfer {
        files: borrowed_files,
        original: changed,
        active: true,
        _single_owner: PhantomData,
    })
}

impl PrivateFileTransfer<'_> {
    /// The exact borrowed private set, including already non-CLOEXEC files.
    pub fn private_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.files.iter().map(|file| file.as_raw_fd())
    }

    /// Restore original flags, reporting each failure without a Drop retry.
    ///
    /// A failed explicit undo cannot be treated as an acknowledged cancellation.
    pub fn rollback(mut self) -> Result<(), DescriptorError> {
        self.active = false;
        let failures = restore_flags(&self.original);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DescriptorError::Undo { failures })
        }
    }

    /// Leave private files non-CLOEXEC for a subsequent consumer.
    ///
    /// No exec or ownership handoff occurs here. The caller still owns the files.
    pub fn commit(mut self) {
        self.active = false;
    }
}

impl Drop for PrivateFileTransfer<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_flags(&self.original);
            self.active = false;
        }
    }
}

fn clear_cloexec(original: &[(RawFd, i32)]) -> Result<Vec<(RawFd, i32)>, DescriptorError> {
    let mut changed = Vec::with_capacity(original.len());
    for &(fd, flags) in original {
        if flags & libc::FD_CLOEXEC == 0 {
            continue;
        }
        if let Err(error) = set_fd_flags(fd, flags & !libc::FD_CLOEXEC, FlagOperation::Change) {
            let undo_failures = restore_flags(&changed);
            return Err(DescriptorError::FlagChange {
                fd,
                errno: errno(&error),
                undo_failures,
            });
        }
        changed.push((fd, flags));
    }
    Ok(changed)
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn new_anchor() -> io::Result<File> {
    allocation_fault()?;
    // SAFETY: this static name is terminated and the flags have no pointers.
    let fd = unsafe {
        libc::memfd_create(
            c"reverie-private-reservation".as_ptr().cast::<c_char>(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    owned_result(fd)
}

fn duplicate_cloexec(source: RawFd, minimum: RawFd) -> io::Result<File> {
    allocation_fault()?;
    duplicate_kernel_cloexec(source, minimum)
}

fn duplicate_kernel_cloexec(source: RawFd, minimum: RawFd) -> io::Result<File> {
    // SAFETY: fcntl duplicates a borrowed live descriptor into a new owned one.
    let fd = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, minimum) };
    owned_result(fd)
}

fn owned_result(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a successful creating syscall returned unique FD ownership.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn get_fd_flags(fd: RawFd) -> io::Result<i32> {
    // SAFETY: F_GETFD borrows the live owned descriptor and has no pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

#[derive(Clone, Copy)]
enum FlagOperation {
    Change,
    Undo,
}

fn set_fd_flags(fd: RawFd, flags: i32, operation: FlagOperation) -> io::Result<()> {
    flag_fault(operation)?;
    // SAFETY: F_SETFD changes only this owned private descriptor's flag word.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn keep_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = get_fd_flags(fd)?;
    if flags & libc::FD_CLOEXEC == 0 {
        set_fd_flags(fd, flags | libc::FD_CLOEXEC, FlagOperation::Change)?;
    }
    Ok(())
}

fn restore_flags(original: &[(RawFd, i32)]) -> Vec<DescriptorUndoFailure> {
    let mut failures = Vec::new();
    for &(fd, flags) in original.iter().rev() {
        if let Err(error) = set_fd_flags(fd, flags, FlagOperation::Undo) {
            failures.push(DescriptorUndoFailure {
                fd,
                errno: errno(&error),
            });
        }
    }
    failures
}

#[cfg(not(test))]
fn allocation_fault() -> io::Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn flag_fault(_operation: FlagOperation) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct FaultPlan {
    allocation_at: Option<usize>,
    change_at: Option<usize>,
    undo_at: Option<usize>,
    allocations: usize,
    changes: usize,
    undos: usize,
}

#[cfg(test)]
thread_local! {
    static FAULTS: std::cell::RefCell<FaultPlan> = const {
        std::cell::RefCell::new(FaultPlan {
            allocation_at: None,
            change_at: None,
            undo_at: None,
            allocations: 0,
            changes: 0,
            undos: 0,
        })
    };
}

#[cfg(test)]
fn allocation_fault() -> io::Result<()> {
    FAULTS.with_borrow_mut(|faults| {
        faults.allocations += 1;
        if faults.allocation_at == Some(faults.allocations) {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn flag_fault(operation: FlagOperation) -> io::Result<()> {
    FAULTS.with_borrow_mut(|faults| {
        let fails = match operation {
            FlagOperation::Change => {
                faults.changes += 1;
                faults.change_at == Some(faults.changes)
            }
            FlagOperation::Undo => {
                faults.undos += 1;
                faults.undo_at == Some(faults.undos)
            }
        };
        if fails {
            Err(io::Error::from_raw_os_error(libc::EIO))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    const CHILD_ENV: &str = "REVERIE_LB4_DESCRIPTOR_CHILD";

    #[test]
    fn lb4_guest_fds_numbers_flags_and_shared_ofds() {
        child("guest_fds");
    }

    #[test]
    fn lb4_full_table_before_reservation_native_exec_succeeds() {
        child("full_before");
    }

    #[test]
    fn lb4_full_table_after_reservation_scratch_and_connection() {
        child("full_after");
    }

    #[test]
    fn lb4_allocation_flag_and_undo_failures_are_private() {
        child("faults");
    }

    #[test]
    fn lb4_failed_open_restores_slot_and_rejects_allocation_mutation() {
        child("open_failure");
    }

    #[test]
    fn lb4_ordinary_open_capacity_precedes_missing_and_fifo_lookup() {
        child("ordinary_open");
    }

    #[test]
    fn lb4_zero_nofile_retained_seed_has_native_emfile_before_lookup() {
        child("zero_nofile");
    }

    #[test]
    fn lb4_taken_private_files_transfer_flag_and_undo_failures() {
        child("file_transfer");
    }

    // An independently exec'd libtest process contains every FD-table mutation.
    // This entry does nothing in the ordinary parent test invocation.
    #[test]
    fn descriptor_child() {
        let Ok(case) = std::env::var(CHILD_ENV) else {
            return;
        };
        match case.as_str() {
            "guest_fds" => guest_fds(),
            "full_before" => full_before(),
            "full_after" => full_after(),
            "faults" => faults(),
            "open_failure" => open_failure(),
            "ordinary_open" => ordinary_open(),
            "zero_nofile" => zero_nofile(),
            "file_transfer" => file_transfer(),
            _ => panic!("unknown descriptor child case {case}"),
        }
    }

    fn artifact_dir(name: &str) -> std::path::PathBuf {
        crate::test_support::fixture_dir_in(
            std::path::Path::new(
                option_env!("ELF_LOADER_ARTIFACT_DIR").unwrap_or("target/lb4-artifacts"),
            ),
            name,
        )
    }

    fn child(case: &str) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "descriptors::tests::descriptor_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD_ENV, case)
            .stdin(Stdio::null());
        // Expiry kills the child's process group and fails immediately; reaping
        // is bounded and asynchronous, never a blocking wait after SIGKILL.
        let directory = artifact_dir(&format!("lb4-descriptor-child-{case}"));
        let output = crate::test_support::run_monitored_output(
            command,
            &format!("descriptor child {case}, output under {directory:?}"),
            Duration::from_secs(10),
            &directory,
        );
        assert!(
            output.status.success(),
            "descriptor child {case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn limit(value: libc::rlim_t) {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: current is writable and is the exact native structure.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) },
            0
        );
        assert!(
            current.rlim_max >= value,
            "descriptor fixture needs {value} FDs"
        );
        current.rlim_cur = value;
        // SAFETY: only this isolated child's soft limit changes.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &current) }, 0);
    }

    fn marker() -> File {
        let mut file = new_anchor().unwrap();
        file.write_all(b"guest-ofd-marker").unwrap();
        // SAFETY: the marker is a live owned seekable memfd.
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 4, libc::SEEK_SET) },
            4
        );
        file
    }

    fn open_count(maximum: RawFd) -> usize {
        (0..maximum)
            .filter(|&fd| {
                // SAFETY: F_GETFD is a query; even an invalid fd has no effects.
                (unsafe { libc::fcntl(fd, libc::F_GETFD) }) >= 0
            })
            .count()
    }

    fn flags(fd: RawFd) -> i32 {
        get_fd_flags(fd).unwrap()
    }

    fn position(fd: RawFd) -> libc::off_t {
        // SAFETY: querying a live marker OFD's current seek position.
        let position = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        assert!(position >= 0);
        position
    }

    fn guest_fds() {
        limit(2048);
        let source = marker();
        let mut guests = Vec::new();
        for (minimum, desired_flags) in [(100, 0), (102, libc::FD_CLOEXEC), (1024, 0)] {
            let file = duplicate_cloexec(source.as_raw_fd(), minimum).unwrap();
            assert_eq!(file.as_raw_fd(), minimum);
            set_fd_flags(minimum, desired_flags, FlagOperation::Change).unwrap();
            guests.push(file);
        }
        let state: Vec<_> = guests
            .iter()
            .map(|file| {
                let metadata = file.metadata().unwrap();
                (
                    file.as_raw_fd(),
                    flags(file.as_raw_fd()),
                    metadata.dev(),
                    metadata.ino(),
                )
            })
            .collect();
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        assert!(
            reservation
                .private_fds()
                .all(|fd| ![100, 102, 1024].contains(&fd))
        );
        // SAFETY: this isolated child is the sole descriptor-table mutator.
        unsafe { reservation.open_into(1, new_anchor) }.unwrap();
        let transfer = reservation
            .transfer(&[1, 2, reservation.scratch_slot()])
            .unwrap();
        transfer.rollback().unwrap();
        drop(reservation);
        for (file, &(number, original_flags, device, inode)) in guests.iter().zip(&state) {
            let metadata = file.metadata().unwrap();
            assert_eq!(file.as_raw_fd(), number);
            assert_eq!(flags(number), original_flags);
            assert_eq!((metadata.dev(), metadata.ino()), (device, inode));
            assert_eq!(position(number), 4);
        }
        let mut one = [0];
        (&guests[0]).read_exact(&mut one).unwrap();
        assert_eq!(one, [b't']);
        for file in &guests {
            assert_eq!(
                position(file.as_raw_fd()),
                5,
                "dup must still share the guest OFD"
            );
        }
        assert_eq!(position(source.as_raw_fd()), 5);
    }

    fn fill_table(source: RawFd) -> Vec<File> {
        let mut fillers = Vec::new();
        loop {
            match duplicate_cloexec(source, 0) {
                Ok(file) => fillers.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    return fillers;
                }
            }
        }
    }

    fn full_before() {
        limit(96);
        let executable = std::fs::read_link("/proc/self/exe").unwrap();
        // This is a real ordinary caller's libc PRNG, not an assertion about
        // Hermit's root_prng (which this inactive crate cannot access). Record
        // the expected next draw, then recreate that exact live state.
        let seed = 0x5eed;
        // SAFETY: this isolated child is the only libc rand/srand user.
        let (first, next, after_next) = unsafe {
            libc::srand(seed);
            (libc::rand(), libc::rand(), libc::rand())
        };
        assert_ne!(next, after_next, "PRNG mutation must discriminate");
        // SAFETY: the same sole caller restores its real PRNG state.
        unsafe { libc::srand(seed) };
        assert_eq!(unsafe { libc::rand() }, first);
        let source = marker();
        let fillers = fill_table(source.as_raw_fd());
        let marker_flags = flags(source.as_raw_fd());
        let old_state = [17_u64, 23, 29, 31];
        let mut state = old_state;
        let refusal = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap_err();
        assert!(matches!(
            refusal,
            DescriptorError::Capacity { reserved: 0, .. }
        ));
        assert_eq!(refusal.raw_os_error(), Some(libc::EMFILE));
        assert_eq!(flags(source.as_raw_fd()), marker_flags);
        assert_eq!(position(source.as_raw_fd()), 4);
        assert_eq!(state, old_state, "caller memory canary remains untouched");
        assert_eq!(
            std::fs::read_link("/proc/self/exe").unwrap(),
            executable,
            "capacity refusal preserves the running image"
        );
        // SAFETY: the serialized caller observes its next actual PRNG draw.
        assert_eq!(unsafe { libc::rand() }, next);
        // Mutation: consuming one additional draw must change that witness.
        assert_eq!(unsafe { libc::rand() }, after_next);
        state[0] += 1;
        assert_eq!(state[0], 18);
        assert!(!fillers.is_empty());
        let argv = [c"true".as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null::<c_char>()];
        // SAFETY: all pointers are valid, terminated strings/arrays. This is
        // native exec with a full table, in the ordinary test child. CLOEXEC
        // fillers close only after the kernel commits the successful exec.
        unsafe { libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        panic!(
            "native exec with a full user table failed: {}",
            io::Error::last_os_error()
        );
    }

    fn full_after() {
        limit(96);
        let source = marker();
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        let fillers = fill_table(source.as_raw_fd());
        let before = open_count(96);
        assert_eq!(before, 96);
        // SAFETY: no other code in this isolated child changes the FD table.
        unsafe { reservation.open_into(1, || File::open("/bin/true")) }.unwrap();
        let target = reservation.fd(1).unwrap();
        let pinned_path = format!("/proc/self/fd/{target}");
        let scratch = reservation.scratch_slot();
        let number = reservation.fd(scratch).unwrap();
        for _ in 0..3 {
            // SAFETY: one serialized ordinary read-open uses the one vacancy.
            assert_eq!(
                unsafe { reservation.open_into(scratch, || File::open(&pinned_path)) }.unwrap(),
                number
            );
            assert_eq!(open_count(96), before);
            // SAFETY: the scratch file has no surviving borrower.
            assert_eq!(
                unsafe { reservation.restore_placeholder(scratch) }.unwrap(),
                number
            );
        }
        // SAFETY: this closure allocates only the two reserved socket FDs.
        let (one, two) = unsafe { reservation.open_pair_into(2, 3, socket_pair) }.unwrap();
        assert_eq!(open_count(96), before);
        let payload = b"reserved connection";
        let mut writer = reservation.file(2).unwrap();
        writer.write_all(payload).unwrap();
        let mut reader = reservation.file(3).unwrap();
        let mut received = [0_u8; 19];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(&received, payload);
        let transfer = reservation.transfer(&[1, 2, 3, scratch]).unwrap();
        for fd in [target, one, two, number] {
            assert_eq!(flags(fd), 0);
        }
        transfer.rollback().unwrap();
        for fd in [target, one, two, number] {
            assert_eq!(flags(fd), libc::FD_CLOEXEC);
        }
        drop(fillers);
    }

    fn socket_pair() -> io::Result<(File, File)> {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is writable for two ints; syscall creates owners.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                descriptors.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both unique descriptors were just returned by socketpair.
        Ok(unsafe {
            (
                File::from_raw_fd(descriptors[0]),
                File::from_raw_fd(descriptors[1]),
            )
        })
    }

    fn faults() {
        limit(96);
        let source = marker();
        let before = open_count(96);
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                allocation_at: Some(4),
                ..FaultPlan::default()
            };
        });
        let allocation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap_err();
        assert!(matches!(
            allocation,
            DescriptorError::Capacity { reserved: 3, .. }
        ));
        assert_eq!(
            open_count(96),
            before,
            "partial reserve must close only its owned resources"
        );
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        let one = reservation.fd(1).unwrap();
        let two = reservation.fd(2).unwrap();
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                change_at: Some(2),
                ..FaultPlan::default()
            };
        });
        let flag_error = reservation.transfer(&[1, 2]).unwrap_err();
        assert!(matches!(
            flag_error,
            DescriptorError::FlagChange { errno: libc::EIO, ref undo_failures, .. }
                if undo_failures.is_empty()
        ));
        assert_eq!(flags(one), libc::FD_CLOEXEC);
        assert_eq!(flags(two), libc::FD_CLOEXEC);
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        let transfer = reservation.transfer(&[1, 2]).unwrap();
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                undo_at: Some(1),
                ..FaultPlan::default()
            };
        });
        let undo_error = transfer.rollback().unwrap_err();
        assert!(
            matches!(undo_error, DescriptorError::Undo { ref failures } if failures.len() == 1)
        );
        assert_eq!(
            FAULTS.with_borrow(|faults| faults.undos),
            2,
            "explicit undo must not run again in Drop"
        );
        assert_eq!(flags(one), libc::FD_CLOEXEC);
        assert_eq!(
            flags(two),
            0,
            "failed undo is visible, not a claimed rollback"
        );
        assert_eq!(flags(source.as_raw_fd()), libc::FD_CLOEXEC);
        assert_eq!(position(source.as_raw_fd()), 4);
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        drop(reservation);
        assert_eq!(open_count(96), before);
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                allocation_at: Some(1),
                ..FaultPlan::default()
            };
        });
        // SAFETY: the failed open and injected restoration affect only slot 1.
        let failed_restore = unsafe {
            reservation.open_into(1, || -> io::Result<File> {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            })
        }
        .unwrap_err();
        assert!(matches!(
            failed_restore,
            DescriptorError::Restore { ref failures, .. }
                if failures == &[DescriptorUndoFailure {
                    fd: reservation.slots[1].number,
                    errno: libc::EMFILE,
                }]
        ));
        assert_eq!(
            reservation.fd(1),
            None,
            "failed restoration cannot claim a reserved slot"
        );
        assert_eq!(position(source.as_raw_fd()), 4);
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        drop(reservation);
        assert_eq!(open_count(96), before);
    }

    fn open_failure() {
        limit(96);
        let source = marker();
        let before = open_count(96);
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        let number = reservation.fd(1).unwrap();
        // SAFETY: closure creates no FD on failure and no guest FD is changed.
        let failure = unsafe {
            reservation.open_into(1, || -> io::Result<File> {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            })
        }
        .unwrap_err();
        assert!(matches!(failure, DescriptorError::Open { .. }));
        assert_eq!(failure.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(reservation.fd(1), Some(number));
        assert_eq!(flags(number), libc::FD_CLOEXEC);
        // A discriminator for an allocator that silently accepts a different
        // number: deliberately return a duplicate at an unrelated free number.
        // SAFETY: the mutation still allocates only its returned owner and
        // changes no guest FD; violating the expected slot is detected.
        let changed =
            unsafe { reservation.open_into(1, || duplicate_cloexec(source.as_raw_fd(), 80)) }
                .unwrap_err();
        assert!(
            matches!(changed, DescriptorError::UnexpectedFd { expected, actual: 80 } if expected == number)
        );
        assert_eq!(reservation.fd(1), Some(number));
        assert_eq!(position(source.as_raw_fd()), 4);
        drop(reservation);
        assert_eq!(open_count(96), before);
    }

    fn zero_nofile() {
        limit(96);
        let directory = artifact_dir("lb4-zero-nofile");
        let fifo = directory.join("fifo");
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let missing_name =
            std::ffi::CString::new(directory.join("missing").as_os_str().as_bytes()).unwrap();
        let source = marker();
        let retained_fd = source.as_raw_fd();
        let retained_flags = flags(retained_fd);
        let retained_position = position(retained_fd);
        limit(0);
        assert_eq!(flags(retained_fd), retained_flags);
        for name in [c"/bin/true", missing_name.as_c_str(), fifo_name.as_c_str()] {
            let begin = Instant::now();
            // The retained source and standard I/O remain valid above a zero
            // soft limit. Native open must fail before missing/FIFO lookup.
            let native = unsafe { libc::openat(libc::AT_FDCWD, name.as_ptr(), libc::O_RDONLY) };
            assert_eq!(native, -1);
            let native_errno = io::Error::last_os_error().raw_os_error().unwrap();
            assert_eq!(native_errno, libc::EMFILE);
            assert!(begin.elapsed() < Duration::from_secs(1));
            let mut lookups = 0;
            let prepared = unsafe { OrdinaryOpenSlot::reserve(&source) };
            if prepared.is_ok() {
                lookups += 1;
            }
            assert!(
                matches!(prepared, Err(OrdinaryOpenError::NativeErrno(errno)) if errno == native_errno)
            );
            assert_eq!(lookups, 0);
        }
        // Mutation: forwarding f_dupfd's errno would disagree with every
        // native companion above, even with this still-valid retained seed.
        assert_eq!(
            unsafe { libc::fcntl(retained_fd, libc::F_DUPFD_CLOEXEC, 0) },
            -1
        );
        let wrong_errno = io::Error::last_os_error().raw_os_error().unwrap();
        assert_eq!(wrong_errno, libc::EINVAL);
        assert_ne!(wrong_errno, libc::EMFILE);
        assert_eq!(flags(retained_fd), retained_flags);
        assert_eq!(position(retained_fd), retained_position);
        limit(96);
        std::fs::remove_file(fifo).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    fn ordinary_open() {
        limit(96);
        let directory = artifact_dir("lb4-ordinary-open");
        let fifo = directory.join("fifo");
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is valid/terminated. FIFO creation does not open it.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let missing = directory.join("missing");
        let missing_name = std::ffi::CString::new(missing.as_os_str().as_bytes()).unwrap();
        let source = marker();
        let mut fillers = fill_table(source.as_raw_fd());
        assert_eq!(open_count(96), 96);
        for (path, name) in [(&missing, &missing_name), (&fifo, &fifo_name)] {
            let begin = Instant::now();
            // SAFETY: the ordinary open gets copied valid arguments. Its full
            // FD table prevents both pathname traversal and FIFO ->open.
            let native = unsafe { libc::openat(libc::AT_FDCWD, name.as_ptr(), libc::O_RDONLY) };
            assert_eq!(native, -1);
            let native_errno = io::Error::last_os_error().raw_os_error().unwrap();
            assert_eq!(native_errno, libc::EMFILE);
            assert!(
                begin.elapsed() < Duration::from_secs(1),
                "full-table FIFO must not block"
            );

            let mut lookups = 0;
            // SAFETY: this isolated child is the only FD-table mutator; copied
            // O_RDONLY/path arguments are valid before the capacity decision.
            let prepared = unsafe { OrdinaryOpenSlot::reserve(&source) };
            if prepared.is_ok() {
                lookups += 1;
                let _ = std::fs::symlink_metadata(path);
            }
            assert!(
                matches!(prepared, Err(OrdinaryOpenError::NativeErrno(errno)) if errno == native_errno)
            );
            assert_eq!(lookups, 0, "capacity must precede target classification");

            // Mutation: getattr before capacity really traverses the missing
            // path/FIFO and either gets ENOENT or sees the unsupported FIFO.
            // The same ordering witness rejects that changed operation order.
            let mut wrong_lookups = 0;
            wrong_lookups += 1;
            let early_classification = std::fs::symlink_metadata(path);
            assert_ne!(wrong_lookups, lookups);
            match early_classification {
                Ok(metadata) => assert!(!metadata.is_file() && !metadata.is_dir()),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
                    assert_ne!(error.raw_os_error(), Some(native_errno));
                }
            }
        }
        let released = fillers.swap_remove(7);
        let lowest = released.as_raw_fd();
        drop(released);
        // SAFETY: one vacancy now exists, so the unchanged native open must
        // allocate exactly that lowest number. No classification helper runs.
        let native = unsafe { libc::openat(libc::AT_FDCWD, c"/bin/true".as_ptr(), libc::O_RDONLY) };
        assert_eq!(native, lowest);
        let native_flags = flags(native);
        assert_eq!(native_flags, 0);
        // SAFETY: the successful syscall returned a unique owner.
        drop(unsafe { File::from_raw_fd(native) });
        // SAFETY: same sole vacancy/serialized table, valid copied arguments.
        let slot = unsafe { OrdinaryOpenSlot::reserve(&source) }.unwrap();
        assert_eq!(slot.fd(), lowest);
        assert_eq!(
            open_count(96),
            96,
            "classification holds its reserved number"
        );
        let metadata = std::fs::metadata("/bin/true").unwrap();
        assert!(metadata.is_file());
        // SAFETY: classification admitted a regular file; the original read
        // open runs while the table is serialized and allocates only its owner.
        let opened = unsafe {
            slot.open(|| {
                owned_result(libc::openat(
                    libc::AT_FDCWD,
                    c"/bin/true".as_ptr(),
                    libc::O_RDONLY,
                ))
            })
        }
        .unwrap();
        assert_eq!(opened.as_raw_fd(), lowest);
        assert_eq!(flags(opened.as_raw_fd()), native_flags);
        assert_eq!(position(source.as_raw_fd()), 4);
        let mut header = [0_u8; 4];
        std::os::unix::fs::FileExt::read_exact_at(&opened, &mut header, 0).unwrap();
        assert_eq!(header, *b"\x7fELF");
        drop(opened);
        drop(fillers);
        std::fs::remove_file(fifo).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    fn file_transfer() {
        limit(96);
        let source = marker();
        let before = open_count(96);
        let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
        let one = reservation.take_file(1).unwrap();
        let two = reservation.take_file(2).unwrap();
        let fds = [one.as_raw_fd(), two.as_raw_fd()];
        let identities = [
            (one.metadata().unwrap().dev(), one.metadata().unwrap().ino()),
            (two.metadata().unwrap().dev(), two.metadata().unwrap().ino()),
        ];
        assert_eq!(reservation.fd(1), None);
        assert_eq!(reservation.fd(2), None);
        // SAFETY: these are private files just taken from the owned pool; this
        // fresh child serializes their flags and descriptor lifetimes.
        let duplicate = unsafe { transfer_private_files(&[&one, &one]) }.unwrap_err();
        assert!(matches!(duplicate, DescriptorError::DuplicateFd { fd } if fd == fds[0]));
        for fd in fds {
            assert_eq!(flags(fd), libc::FD_CLOEXEC);
        }

        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                change_at: Some(2),
                ..FaultPlan::default()
            };
        });
        // SAFETY: the injected private flag error preserves owner lifetimes.
        let flag_error = unsafe { transfer_private_files(&[&one, &two]) }.unwrap_err();
        assert!(
            matches!(flag_error, DescriptorError::FlagChange { errno: libc::EIO, ref undo_failures, .. } if undo_failures.is_empty())
        );
        for fd in fds {
            assert_eq!(flags(fd), libc::FD_CLOEXEC);
        }

        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                change_at: Some(2),
                undo_at: Some(1),
                ..FaultPlan::default()
            };
        });
        // SAFETY: only these borrowed private files are changed/undone.
        let failed_change_undo = unsafe { transfer_private_files(&[&one, &two]) }.unwrap_err();
        assert!(matches!(failed_change_undo,
            DescriptorError::FlagChange { errno: libc::EIO, ref undo_failures, .. }
            if undo_failures == &[DescriptorUndoFailure { fd: fds[0], errno: libc::EIO }]));
        assert_eq!(flags(fds[0]), 0);
        assert_eq!(flags(fds[1]), libc::FD_CLOEXEC);
        assert_eq!(FAULTS.with_borrow(|faults| faults.undos), 1);
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        set_fd_flags(fds[0], libc::FD_CLOEXEC, FlagOperation::Change).unwrap();

        // This API does not allocate an FD, even when an allocation failure is
        // armed. An EMFILE injection belongs only to private FD reservation.
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                allocation_at: Some(1),
                ..FaultPlan::default()
            };
        });
        // SAFETY: both owners and their private category remain unchanged.
        let transfer = unsafe { transfer_private_files(&[&one, &two]) }.unwrap();
        assert_eq!(transfer.private_fds().collect::<Vec<_>>(), fds);
        assert_eq!(FAULTS.with_borrow(|faults| faults.allocations), 0);
        for fd in fds {
            assert_eq!(flags(fd), 0);
        }
        FAULTS.with_borrow_mut(|faults| {
            *faults = FaultPlan {
                undo_at: Some(1),
                ..FaultPlan::default()
            };
        });
        let undo_error = transfer.rollback().unwrap_err();
        assert!(matches!(undo_error, DescriptorError::Undo { ref failures }
            if failures == &[DescriptorUndoFailure { fd: fds[1], errno: libc::EIO }]));
        assert_eq!(
            FAULTS.with_borrow(|faults| faults.undos),
            2,
            "explicit rollback must not run again in Drop"
        );
        assert_eq!(flags(fds[0]), libc::FD_CLOEXEC);
        assert_eq!(flags(fds[1]), 0);
        assert_eq!(flags(source.as_raw_fd()), libc::FD_CLOEXEC);
        assert_eq!(position(source.as_raw_fd()), 4);
        FAULTS.with_borrow_mut(|faults| *faults = FaultPlan::default());
        set_fd_flags(fds[1], libc::FD_CLOEXEC, FlagOperation::Change).unwrap();
        // SAFETY: automatic undo still borrows the same live private owners.
        drop(unsafe { transfer_private_files(&[&one, &two]) }.unwrap());
        for fd in fds {
            assert_eq!(flags(fd), libc::FD_CLOEXEC);
        }
        // SAFETY: committing flags is a private operation, with no exec/consumer.
        unsafe { transfer_private_files(&[&one, &two]) }
            .unwrap()
            .commit();
        for fd in fds {
            assert_eq!(flags(fd), 0);
        }
        for (file, identity) in [&one, &two].into_iter().zip(identities) {
            let metadata = file.metadata().unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), identity);
        }
        drop(reservation);
        drop((one, two));
        assert_eq!(open_count(96), before, "only private resources are closed");
    }
}
