/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A seam through which one installed policy may refuse a guest syscall, by
//! name, before Detcore handles it.
//!
//! A process installs at most one [`CallGate`]. A plain run installs none,
//! and its only cost is the load of an empty `OnceLock` once per syscall;
//! the check itself is boxed where Detcore awaits it, so it adds nothing to
//! the per-syscall future (`hot_path_futures_stay_small`). A gate decides
//! from the call's number and arguments and from what [`GateCall`] lets it
//! read about the caller, and changes nothing. A refused call has none of its
//! effects: the run ends with `HERMIT_POLICY_REFUSAL_EXIT`, after
//! `hermit: <message>` on standard error. A gate also learns when each call
//! it let run has had its effects ([`CallGate::completed`]), so it can know
//! which calls are in flight.
//!
//! A run that may be copied as multiverse timelines installs one
//! (`crate::timeline_sharing`), so that a copy refuses what it would
//! otherwise share with its template and the other copies.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use reverie::FactError;
use reverie::Guest;
use reverie::PathLink;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// Reads the caller's memory for a gate: `fill(address, buffer)` fills
/// `buffer` from `address` and says whether it could.
pub type ReadMemory<'a> = &'a dyn Fn(usize, &mut [u8]) -> bool;

/// Reads one of the caller's `/proc` links for a gate, as the backend reports
/// it (`Guest::link_path`): the text `readlink(2)` would give.
pub type ReadLink<'a> = &'a dyn Fn(PathLink) -> Result<Vec<u8>, FactError>;

/// One guest syscall as a gate sees it.
pub struct GateCall<'a> {
    /// The call's number.
    pub number: Sysno,
    /// Its raw arguments.
    pub args: SyscallArgs,
    /// The calling thread's ID, as the backend reports it.
    pub thread: i32,
    read_path: &'a dyn Fn(usize) -> Option<PathBuf>,
    read_memory: ReadMemory<'a>,
    read_link: ReadLink<'a>,
}

impl<'a> GateCall<'a> {
    /// A call of `thread` whose path arguments `read_path` reads, whose other
    /// in-memory arguments `read_memory` reads, from the caller's memory, and
    /// whose descriptors and working directory `read_link` names.
    pub fn new(
        number: Sysno,
        args: SyscallArgs,
        thread: i32,
        read_path: &'a dyn Fn(usize) -> Option<PathBuf>,
        read_memory: ReadMemory<'a>,
        read_link: ReadLink<'a>,
    ) -> Self {
        GateCall {
            number,
            args,
            thread,
            read_path,
            read_memory,
            read_link,
        }
    }

    /// The text of the caller's link `link`, with the backend's errors:
    /// `EBADF` for a descriptor the caller's table does not hold, and
    /// `NotProvided` for a link the backend does not name.
    pub fn link(&self, link: PathLink) -> Result<Vec<u8>, FactError> {
        (self.read_link)(link)
    }

    /// The NUL-terminated path at `address` in the caller's memory (`None`
    /// for a null or unreadable address).
    pub fn path(&self, address: usize) -> Option<PathBuf> {
        (self.read_path)(address)
    }

    /// The native-endian `u64` at `address` in the caller's memory.
    pub fn u64_at(&self, address: usize) -> Option<u64> {
        let mut bytes = [0u8; 8];
        (self.read_memory)(address, &mut bytes).then(|| u64::from_ne_bytes(bytes))
    }
}

/// A policy that may refuse a guest syscall before Detcore handles it.
pub trait CallGate: Send + Sync {
    /// `Some(message)` refuses `call`, naming what it would have reached;
    /// `None` lets Detcore handle it.
    fn refusal(&self, call: &GateCall<'_>) -> Option<String>;

    /// The call of `thread` that [`CallGate::refusal`] last let run has had
    /// its effects: Detcore has finished it, before any scheduler turn its
    /// post-handler hook takes.
    fn completed(&self, _thread: i32) {}
}

static GATE: OnceLock<&'static dyn CallGate> = OnceLock::new();

/// Installs `gate` for every later syscall of this process. Returns false,
/// leaving the installed gate in place, when one is already installed.
pub fn install(gate: &'static dyn CallGate) -> bool {
    GATE.set(gate).is_ok()
}

/// The installed gate, if any.
pub(crate) fn installed() -> Option<&'static dyn CallGate> {
    GATE.get().copied()
}

/// A call the gate let run, in flight until this is dropped, once Detcore
/// has finished it.
pub(crate) struct InFlight {
    gate: &'static dyn CallGate,
    thread: i32,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.gate.completed(self.thread);
    }
}

/// Asks `gate` about `call`, and ends the run by name when it refuses;
/// otherwise the call is in flight while the returned value lives.
pub(crate) async fn check<G, T>(guest: &G, gate: &'static dyn CallGate, call: Syscall) -> InFlight
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let thread = guest.tid().as_raw();
    // The caller's memory is read here, and not held across the await below.
    let refusal = {
        let (number, args) = call.into_parts();
        let memory = guest.memory();
        let read_path = |address: usize| {
            PathPtr::from_ptr(address as *const libc::c_char)
                .and_then(|path| path.read(&memory).ok())
        };
        let read_memory = |address: usize, buffer: &mut [u8]| {
            Addr::<u8>::from_raw(address).is_some_and(|at| memory.read_exact(at, buffer).is_ok())
        };
        let read_link = |link: PathLink| {
            guest
                .link_path(guest.tid(), link)
                .map(|text| text.as_bytes().to_vec())
        };
        gate.refusal(&GateCall::new(
            number,
            args,
            thread,
            &read_path,
            &read_memory,
            &read_link,
        ))
    };
    let Some(message) = refusal else {
        return InFlight { gate, thread };
    };
    tracing::error!("{message}");
    let _ = writeln!(crate::util::RetryingStderr, "hermit: {message}");
    crate::tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
        .await
}
