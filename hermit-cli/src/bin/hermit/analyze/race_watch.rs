/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The watch stage of the "Racing accesses" section. When the syscall finder
//! ([`races`](super::races)) names no pair, the race may be on plain memory,
//! which leaves nothing in a syscall trace. The stage replays both
//! continuations again, each a deterministic replay of the schedule it
//! realized, with `hermit run --watch` on chosen guest bytes, and compares
//! the loads and stores the two runs make on them.
//!
//! The bytes come from `--race-watch` and from what the analysis already
//! has: the addresses a sanitizer report in the target's stderr names, the
//! fault address of a SIGSEGV or SIGBUS outcome, and the futex word of a
//! futex call at the deciding event ([`candidates`]).
//!
//! An x86-64 thread has four debug registers. Each watched piece (at most
//! eight aligned bytes) takes two of them, a write watch and a read-write
//! watch on the same bytes, so a hit on the read-write slot alone is a load
//! and a hit on both is a store: two pieces per pass, each pass one replay
//! of each side, at most [`MAX_PASSES`] passes ([`plan_passes`]).
//!
//! A hit lies between the schedule events `after - 1` and `after`, in the
//! turn of the thread that made it, so program order places it after that
//! thread's last event before `after` and before its next event. Whether an
//! edge orders two hits is the finder's happens-before over those events
//! ([`memory_pairs`]).
//!
//! Detcore writes a thread's first [`WATCH_HITS_WITH_STACKS`] hits as they
//! happen and keeps only its last ones after those, written when the run
//! ends. Hits arrive one at a time under the scheduler's lock, one guest
//! thread running, so the hits written as they happen are in execution
//! order across threads, and each thread's are a contiguous prefix of its
//! hits in the window. The ones written at the end are neither: the stage
//! uses only the first kind ([`hits`]), and says how many it left out.
//!
//! The same address in two processes need not be the same memory (a
//! private page after fork, or an image after execve), so accesses are
//! paired within one process, between its execs.
//!
//! The section's wording holds here too: two accesses are "unordered by the
//! edges Hermit observes", which uncontended locks and atomics do not show.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use detcore::types::Op;
use detcore::types::SchedEvent;
use detcore::types::SyscallPhase;
use detcore_model::access_trace::AccessTrace;
use detcore_model::access_trace::EventIndex;
use detcore_model::access_trace::EventRef;
use detcore_model::access_trace::StackFrame;
use detcore_model::access_trace::TraceRecord;
use detcore_model::access_trace::WATCH_HITS_WITH_STACKS;
use detcore_model::access_trace::WatchUnarmed;
use detcore_model::pid::DetPid;
use detcore_model::pid::DetTid;
use detcore_model::watch::WatchAccess;
use detcore_model::watch::WatchArg;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::analyze::races::HbEdge;
use crate::analyze::races::HbEdgeKind;
use crate::analyze::races::HbVerdict;
use crate::analyze::races::happens_before;

/// At most this many passes, each one replay of each side.
pub const MAX_PASSES: usize = 8;

/// A fault address below this (the first page) is a null dereference's,
/// not an object's: it is not watched.
pub const NULL_PAGE: u64 = 4096;

/// At most this many unwatched pieces are listed by name; the rest are
/// counted.
pub const MAX_UNWATCHED_LISTED: usize = 16;

/// Pieces a pass watches: each takes a write slot and a read-write slot.
pub const PIECES_PER_PASS: usize = 2;

/// The longest `--race-watch` range.
pub const MAX_RACE_WATCH_LEN: u64 = 64;

/// At most this many pairs are listed.
pub const MAX_PAIRS: usize = 4;

/// One `--race-watch=ADDR[:LEN]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaceWatchArg {
    pub at: RaceWatchAt,
    /// The bytes watched; `None` takes a symbol's size (at most
    /// [`MAX_RACE_WATCH_LEN`]), or 8 for an address.
    pub len: Option<u64>,
}

/// Where a `--race-watch` range starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaceWatchAt {
    /// A guest address.
    Addr(u64),
    /// A symbol of the program's symbol table, as it is spelled there
    /// (mangled for C++), and an offset into it.
    Symbol { name: String, offset: u64 },
}

impl FromStr for RaceWatchArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (at, len) = match s.rsplit_once(':') {
            Some((at, len)) => (at, Some(len)),
            None => (s, None),
        };
        let len = match len {
            None => None,
            Some(len) => Some(
                parse_number(len)
                    .filter(|len| (1..=MAX_RACE_WATCH_LEN).contains(len))
                    .ok_or_else(|| {
                        format!(
                            "--race-watch={s}: the length must be 1 to {MAX_RACE_WATCH_LEN} bytes"
                        )
                    })?,
            ),
        };
        let at = match parse_number(at) {
            Some(addr) => RaceWatchAt::Addr(addr),
            None => {
                let (name, offset) = match at.rsplit_once('+') {
                    Some((name, offset)) => (
                        name,
                        parse_number(offset).ok_or_else(|| {
                            format!("--race-watch={s}: `{offset}` is not an offset")
                        })?,
                    ),
                    None => (at, 0),
                };
                if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                    return Err(format!(
                        "--race-watch={s}: give an address in hex (0x...) or decimal, or a symbol \
                         as the program's symbol table spells it"
                    ));
                }
                RaceWatchAt::Symbol {
                    name: name.to_owned(),
                    offset,
                }
            }
        };
        Ok(RaceWatchArg { at, len })
    }
}

/// The ranges `args` name in `program`: an address as given, a symbol at
/// its address in the program's symbol table plus its offset, where the
/// program is not position-independent (its file addresses are its run
/// addresses). A symbol that cannot be placed is a note, not a range.
pub fn race_watch_candidates(
    program: &std::path::Path,
    args: &[RaceWatchArg],
) -> (Vec<Candidate>, Vec<String>) {
    use object::Object;
    use object::ObjectKind;
    use object::ObjectSymbol;

    let mut out = Vec::new();
    let mut notes = Vec::new();
    let data = args
        .iter()
        .any(|arg| matches!(arg.at, RaceWatchAt::Symbol { .. }))
        .then(|| std::fs::read(program));
    let object = match &data {
        Some(Ok(bytes)) => match object::File::parse(&bytes[..]) {
            Ok(object) => Some(Ok(object)),
            Err(error) => Some(Err(format!(
                "{} is not an object file ({error})",
                program.display()
            ))),
        },
        Some(Err(error)) => Some(Err(format!(
            "{} cannot be read ({error})",
            program.display()
        ))),
        None => None,
    };
    for arg in args {
        match &arg.at {
            RaceWatchAt::Addr(addr) => {
                match Candidate::new(*addr, arg.len.unwrap_or(8), None, WatchSource::RaceWatch) {
                    Some(candidate) => out.push(candidate),
                    None => notes.push(format!(
                        "--race-watch={addr:#x}: not watched: the range runs past the end of the \
                         address space."
                    )),
                }
            }
            RaceWatchAt::Symbol { name, offset } => {
                let found = match &object {
                    Some(Ok(object)) if object.kind() != ObjectKind::Executable => Err(format!(
                        "{} is position-independent, so its symbols' file addresses are not the \
                         run's; give the address",
                        program.display()
                    )),
                    Some(Ok(object)) => object
                        .symbols()
                        .filter(|sym| sym.name() == Ok(name.as_str()))
                        .max_by_key(|sym| sym.is_definition())
                        .ok_or_else(|| format!("{} has no symbol {name}", program.display()))
                        .and_then(|sym| {
                            place_symbol(
                                sym.kind(),
                                sym.is_definition(),
                                (sym.address(), sym.size()),
                                *offset,
                                arg.len,
                            )
                        }),
                    Some(Err(error)) => Err(error.clone()),
                    None => unreachable!("the program is read when a symbol is given"),
                };
                let name = match offset {
                    0 => name.clone(),
                    offset => format!("{name}+{offset:#x}"),
                };
                match found.and_then(|(addr, len)| {
                    Candidate::new(addr, len, Some(name.clone()), WatchSource::RaceWatch)
                        .ok_or_else(|| {
                            "the range runs past the end of the address space".to_owned()
                        })
                }) {
                    Ok(candidate) => out.push(candidate),
                    Err(why) => notes.push(format!("--race-watch={name}: not watched: {why}.")),
                }
            }
        }
    }
    (out, notes)
}

/// The bytes a symbol of a non-PIE program names: its address plus
/// `offset`, `len` bytes or the rest of the symbol (at least one byte, at
/// most [`MAX_RACE_WATCH_LEN`]). A thread-local symbol's value is an offset
/// into each thread's TLS block, not an address, and an undefined symbol
/// has none: both are refused, as is a range past the end of the address
/// space.
fn place_symbol(
    kind: object::SymbolKind,
    defined: bool,
    (addr, size): (u64, u64),
    offset: u64,
    len: Option<u64>,
) -> Result<(u64, u64), String> {
    if kind == object::SymbolKind::Tls {
        return Err(
            "it is thread-local: its value is an offset into each thread's TLS block, \
                    not an address; give the address one thread uses"
                .to_owned(),
        );
    }
    if !defined || addr == 0 {
        return Err("the program does not define it".to_owned());
    }
    let start = addr
        .checked_add(offset)
        .ok_or_else(|| "the offset runs past the end of the address space".to_owned())?;
    let len = len.unwrap_or_else(|| size.saturating_sub(offset).clamp(1, MAX_RACE_WATCH_LEN));
    start
        .checked_add(len)
        .ok_or_else(|| "the range runs past the end of the address space".to_owned())?;
    Ok((start, len))
}

fn parse_number(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Where a watched range came from.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum WatchSource {
    /// `--race-watch`.
    RaceWatch,
    /// A sanitizer report in the target's stderr.
    SanitizerReport,
    /// The `si_addr` of a SIGSEGV or SIGBUS in the target.
    FaultAddress,
    /// The futex word of a futex call at the deciding event.
    FutexWord,
    /// The bytes next to that futex word: the rest of its lock or condition
    /// variable, or data beside it.
    NearFutexWord,
}

impl fmt::Display for WatchSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WatchSource::RaceWatch => "--race-watch",
            WatchSource::SanitizerReport => "the sanitizer report",
            WatchSource::FaultAddress => "the fault address",
            WatchSource::FutexWord => "the futex word at the deciding event",
            WatchSource::NearFutexWord => "the bytes next to the futex word at the deciding event",
        })
    }
}

/// A range of guest bytes to watch, before it is cut into pieces. Its end,
/// `addr + len`, never passes the end of the address space ([`Self::new`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub addr: u64,
    pub len: u64,
    /// `symbol+offset`, or what the bytes are, where known.
    pub name: Option<String>,
    pub source: WatchSource,
}

impl Candidate {
    /// The range of `len` bytes at `addr`, or `None` when it is empty or
    /// runs past the end of the address space.
    pub fn new(addr: u64, len: u64, name: Option<String>, source: WatchSource) -> Option<Self> {
        (len > 0 && addr.checked_add(len).is_some()).then_some(Candidate {
            addr,
            len,
            name,
            source,
        })
    }
}

/// One watched piece: 1, 2, 4 or 8 bytes aligned to their length.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WatchedBytes {
    pub addr: u64,
    pub len: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source: WatchSource,
}

impl fmt::Display for WatchedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} byte(s) at {:#x}", self.len, self.addr)?;
        if let Some(name) = &self.name {
            write!(f, " ({name})")?;
        }
        write!(f, ", from {}", self.source)
    }
}

/// The ranges a sanitizer report in `stderr` names: every access line
/// (`Write of size 8 at 0x... by thread T1`, ThreadSanitizer's; `READ of
/// size 4 at 0x... thread T1`, AddressSanitizer's), named by the report's
/// `Location is global 'NAME' of size N at 0x...` line when that covers it.
pub fn sanitizer_candidates(stderr: &str) -> Vec<Candidate> {
    let mut globals: Vec<(u64, u64, String)> = Vec::new();
    let mut accesses: Vec<(u64, u64)> = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Location is global '") {
            let Some((name, rest)) = rest.split_once('\'') else {
                continue;
            };
            if let Some((size, addr)) = size_and_address(rest) {
                globals.push((addr, size, name.to_owned()));
            }
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let is_access = ["read of size", "write of size"]
            .iter()
            .any(|kind| lower.contains(kind));
        if is_access && let Some((size, addr)) = size_and_address(line) {
            accesses.push((addr, size));
        }
    }
    let mut out: Vec<Candidate> = Vec::new();
    for (addr, len) in accesses {
        if out.iter().any(|c| c.addr == addr && c.len == len) {
            continue;
        }
        let name = globals
            .iter()
            .find(|(start, size, _)| {
                addr >= *start && start.checked_add(*size).is_none_or(|end| addr < end)
            })
            .map(|(start, _, name)| match addr - start {
                0 => name.clone(),
                offset => format!("{name}+{offset:#x}"),
            });
        out.extend(Candidate::new(
            addr,
            len.clamp(1, MAX_RACE_WATCH_LEN),
            name,
            WatchSource::SanitizerReport,
        ));
    }
    out
}

/// `of size N at 0xADDR` in `text`.
fn size_and_address(text: &str) -> Option<(u64, u64)> {
    let (_, rest) = text.split_once("of size ")?;
    let (size, rest) = rest.split_once(' ')?;
    let (_, rest) = rest.split_once("at 0x")?;
    let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    Some((size.parse().ok()?, u64::from_str_radix(&hex, 16).ok()?))
}

/// The ranges the target trace names: the fault address of a SIGSEGV or
/// SIGBUS at or after `decision`, and the futex word of each futex call that
/// enters or returns at `decision`.
pub fn trace_candidates(target: &AccessTrace, decision: EventIndex) -> Vec<Candidate> {
    let mut out = Vec::new();
    for record in &target.records {
        match record {
            TraceRecord::Signal(signal)
                if matches!(signal.signo, libc::SIGSEGV | libc::SIGBUS)
                    && signal.event >= decision =>
            {
                // A null or near-null fault address names no object.
                if let Some(addr) = signal.addr.filter(|addr| *addr >= NULL_PAGE) {
                    out.extend(Candidate::new(
                        addr,
                        8,
                        Some(format!(
                            "si_addr of thread {}'s signal {} at event {}",
                            signal.tid, signal.signo, signal.event
                        )),
                        WatchSource::FaultAddress,
                    ));
                }
            }
            TraceRecord::Syscall(sys)
                if sys.sysno == Sysno::futex
                    && (sys.pre == decision || sys.post == Some(decision)) =>
            {
                // The word, then the 40 bytes before and after its aligned
                // word: a lost wakeup races on the lock or condition variable
                // the word belongs to (a glibc pthread_cond_t's fields come
                // before its __g_signals, a pthread_mutex_t's after its lock
                // word), not only on the word the waiter sleeps on.
                let word = sys.args[0];
                let name = format!(
                    "thread {}'s futex call {} at event {}",
                    sys.tid, sys.nth, decision
                );
                out.extend(Candidate::new(
                    word,
                    4,
                    Some(format!("futex word of {name}")),
                    WatchSource::FutexWord,
                ));
                let aligned = word - word % 8;
                let before = aligned.min(40);
                let after = aligned.checked_add(8).map(|addr| (addr, 40));
                for (addr, len, place) in [(aligned - before, before, "before")]
                    .into_iter()
                    .chain(after.map(|(addr, len)| (addr, len, "after")))
                {
                    out.extend(Candidate::new(
                        addr,
                        len,
                        Some(format!("{len} bytes {place} the futex word of {name}")),
                        WatchSource::NearFutexWord,
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Cut `candidate` into pieces of 1, 2, 4 or 8 bytes, each aligned to its
/// length, covering exactly its bytes.
pub fn pieces(candidate: &Candidate) -> Vec<WatchedBytes> {
    let mut out = Vec::new();
    let mut addr = candidate.addr;
    // A range that would run past the end of the address space is refused
    // when it is made; this one keeps the pieces from wrapping anyway.
    let Some(end) = candidate.addr.checked_add(candidate.len) else {
        return out;
    };
    while addr < end {
        let len = [8u64, 4, 2, 1]
            .into_iter()
            .find(|len| {
                addr.is_multiple_of(*len) && addr.checked_add(*len).is_some_and(|e| e <= end)
            })
            .unwrap_or(1);
        let name = candidate
            .name
            .as_ref()
            .map(|name| match addr - candidate.addr {
                0 => name.clone(),
                offset => format!("{name}, byte {offset}"),
            });
        out.push(WatchedBytes {
            addr,
            len: len as u8,
            name,
            source: candidate.source,
        });
        addr += len;
    }
    out
}

/// The passes for `candidates`, in order, [`PIECES_PER_PASS`] pieces each,
/// at most [`MAX_PASSES`]; the first [`MAX_UNWATCHED_LISTED`] pieces left
/// unwatched; and how many ranges past those were not cut at all. A piece
/// already watched (the same bytes from another source) is not watched
/// twice.
pub fn plan_passes(candidates: &[Candidate]) -> (Vec<Vec<WatchedBytes>>, Vec<WatchedBytes>, usize) {
    let keep = MAX_PASSES * PIECES_PER_PASS;
    let mut all: Vec<WatchedBytes> = Vec::new();
    let mut more = 0;
    for candidate in candidates {
        if all.len() >= keep + MAX_UNWATCHED_LISTED {
            more += 1;
            continue;
        }
        for piece in pieces(candidate) {
            if all.len() < keep + MAX_UNWATCHED_LISTED
                && !all
                    .iter()
                    .any(|p| p.addr == piece.addr && p.len == piece.len)
            {
                all.push(piece);
            }
        }
    }
    let unwatched = all.split_off(keep.min(all.len()));
    let passes = all.chunks(PIECES_PER_PASS).map(<[_]>::to_vec).collect();
    (passes, unwatched, more)
}

/// The `--watch` arguments of a pass: for each piece, a write watch and then
/// a read-write watch, so piece `i` holds slots `2i` and `2i + 1`.
pub fn pass_watches(pass: &[WatchedBytes]) -> Vec<WatchArg> {
    pass.iter()
        .flat_map(|piece| {
            [WatchAccess::Write, WatchAccess::ReadWrite].map(|access| WatchArg {
                addr: piece.addr,
                len: piece.len,
                access,
            })
        })
        .collect()
}

/// Whether an access read or wrote the watched bytes.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    Load,
    Store,
}

impl fmt::Display for AccessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AccessKind::Load => "load",
            AccessKind::Store => "store",
        })
    }
}

/// A thread's process and how many of that process's execs came before an
/// access: the image the access ran in. `None` when the thread made no
/// syscall in the trace.
pub type Space = Option<(DetPid, usize)>;

/// One access a pass saw, from one or two hits of one stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index of the piece in its pass.
    pub piece: usize,
    pub thread: DetTid,
    pub kind: AccessKind,
    /// The instruction after the access.
    pub ip: u64,
    /// Events recorded before it.
    pub after: EventIndex,
    /// Its thread's process and how many of that process's execs came
    /// before it (the image it ran in); `None` when the thread made no
    /// syscall in the trace.
    pub space: Space,
    /// How many accesses of this kind at this instruction to this piece
    /// the thread made in this image in the window before it: with the
    /// thread, image, piece, kind and instruction, its identity across runs.
    pub ordinal: usize,
    pub stack: Vec<StackFrame>,
}

/// What a pass's trace says about one thread's hits, from its watch
/// summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// The first event it recorded with its watches armed; `None` when it
    /// never armed.
    pub armed_before: Option<EventIndex>,
    /// Every hit it had, before the window too.
    pub hits: u64,
    /// Its hits written as they happened: in execution order, and a
    /// contiguous prefix of its hits in the window.
    pub in_order: u64,
    /// Its later hits, written when the run ended and out of execution
    /// order: left out.
    pub deferred: u64,
}

impl Coverage {
    /// From a watch summary: Detcore writes a thread's first
    /// [`WATCH_HITS_WITH_STACKS`] hits as they happen (those in the window)
    /// and the last ones after those when the run ends, so the deferred
    /// rows are the `recorded` beyond what the first part can hold.
    fn of(hits: u64, recorded: u64, armed_before: Option<EventIndex>) -> Self {
        let deferred = if hits <= WATCH_HITS_WITH_STACKS {
            0
        } else {
            recorded.min(WATCH_HITS_WITH_STACKS.min(hits - WATCH_HITS_WITH_STACKS))
        };
        Coverage {
            armed_before,
            hits,
            in_order: recorded - deferred,
            deferred,
        }
    }

    /// Every hit the thread made in the window was written in order.
    pub fn complete(&self) -> bool {
        self.hits <= WATCH_HITS_WITH_STACKS
    }
}

/// One side of a pass: its accesses in execution order, each thread's
/// coverage, and the process of each thread that made a syscall in the
/// trace, with each process's execs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassHits {
    pub hits: Vec<Hit>,
    pub coverage: BTreeMap<DetTid, Coverage>,
    /// The event from which the pass ran on its own, past the deciding
    /// event: its hits from there are another execution's and are dropped
    /// ([`Self::cut_at`]).
    pub ran_on_own_from: Option<EventIndex>,
    pub process: BTreeMap<DetTid, DetPid>,
    /// The prehook event of each execve or execveat that did not return,
    /// by process: memory before it is not memory after it.
    pub execs: BTreeMap<DetPid, Vec<EventIndex>>,
}

impl PassHits {
    /// Drop the hits from the turn that realized event `at` on, where the
    /// pass left its side's schedule: a hit with `after` = `at` lies in the
    /// branch batch event `at` summarizes, which is not the side's, so only
    /// hits with `after` < `at` are kept.
    pub fn cut_at(&mut self, at: EventIndex) {
        self.hits.retain(|hit| hit.after < at);
        self.ran_on_own_from = Some(at);
    }

    /// The address space an access at `after` by `thread` lies in: its
    /// process and how many of that process's execs came before it. `None`
    /// when the thread made no syscall in the trace.
    fn space(&self, thread: DetTid, after: EventIndex) -> Space {
        let pid = *self.process.get(&thread)?;
        let execs = self
            .execs
            .get(&pid)
            .map_or(0, |execs| execs.iter().filter(|&&pre| pre < after).count());
        Some((pid, execs))
    }
}

/// The accesses in `trace`'s hit records, in execution order: each thread's
/// rows written as they happened (its first [`Coverage::in_order`] rows in
/// file order), never the ones written at the end of the run. A store hits
/// both of its piece's slots at one stop, the write slot first (Reverie
/// reports a stop's slots in order), so a write-slot row followed by the
/// same thread's read-write row of that piece, instruction and `after` is
/// one store; a write-slot row alone is a store too, a read-write row alone
/// a load. Refuses a trace without its end line, or whose hit rows have no
/// watch summary to bound them.
pub fn hits(trace: &AccessTrace, pieces_in_pass: usize) -> Result<PassHits, String> {
    if trace.end.is_none() {
        return Err("its trace has no end line, so which hits it kept is unknown".to_owned());
    }
    let mut out = PassHits::default();
    for record in &trace.records {
        match record {
            TraceRecord::WatchSummary(summary) => {
                out.coverage.insert(
                    summary.tid,
                    Coverage::of(summary.hits, summary.recorded, summary.armed_before),
                );
            }
            TraceRecord::Syscall(sys) => {
                out.process.entry(sys.tid).or_insert(sys.pid);
                if matches!(sys.sysno, Sysno::execve | Sysno::execveat) && sys.post.is_none() {
                    out.execs.entry(sys.pid).or_default().push(sys.pre);
                }
            }
            _ => {}
        }
    }
    let mut rows: BTreeMap<DetTid, u64> = BTreeMap::new();
    // Per thread: the index of its last access, and whether it is a store
    // seen on its write slot alone so far, which its read-write row joins.
    let mut open: BTreeMap<DetTid, (usize, bool)> = BTreeMap::new();
    let mut seen: BTreeMap<(DetTid, Space, usize, AccessKind, u64), usize> = BTreeMap::new();
    for record in &trace.records {
        let TraceRecord::WatchHit(hit) = record else {
            continue;
        };
        let Some(coverage) = out.coverage.get(&hit.tid) else {
            return Err(format!(
                "thread {}'s hits have no watch summary, so which of them are in order is \
                 unknown",
                hit.tid
            ));
        };
        let row = rows.entry(hit.tid).or_default();
        *row += 1;
        if *row > coverage.in_order {
            continue;
        }
        let piece = usize::from(hit.slot / 2);
        if piece >= pieces_in_pass {
            continue;
        }
        let write_slot = hit.slot % 2 == 0;
        let joins = !write_slot
            && open.get(&hit.tid).is_some_and(|&(index, open_store)| {
                let last = &out.hits[index];
                open_store && last.piece == piece && last.ip == hit.ip && last.after == hit.after
            });
        if joins {
            let (index, open_store) = open.get_mut(&hit.tid).unwrap();
            *open_store = false;
            let last = &mut out.hits[*index];
            if last.stack.is_empty() {
                last.stack = hit.stack.clone();
            }
            continue;
        }
        let kind = if write_slot {
            AccessKind::Store
        } else {
            AccessKind::Load
        };
        let space = out.space(hit.tid, hit.after);
        let n = seen
            .entry((hit.tid, space, piece, kind, hit.ip))
            .or_default();
        out.hits.push(Hit {
            piece,
            thread: hit.tid,
            kind,
            ip: hit.ip,
            after: hit.after,
            space,
            ordinal: *n,
            stack: hit.stack.clone(),
        });
        *n += 1;
        open.insert(hit.tid, (out.hits.len() - 1, write_slot));
    }
    for (thread, coverage) in &out.coverage {
        let written = rows.get(thread).copied().unwrap_or(0);
        if written != coverage.in_order + coverage.deferred {
            return Err(format!(
                "thread {thread}'s watch summary says {} of its hits were written, and the trace \
                 has {written}",
                coverage.in_order + coverage.deferred
            ));
        }
    }
    Ok(out)
}

/// Threads whose accesses a pass could not see: they never armed.
pub fn unobserved(trace: &AccessTrace) -> Vec<(DetTid, WatchUnarmed)> {
    trace
        .records
        .iter()
        .filter_map(|record| match record {
            TraceRecord::WatchSummary(summary) if summary.armed_before.is_none() => {
                Some((summary.tid, summary.unarmed.unwrap_or(WatchUnarmed::NoStop)))
            }
            _ => None,
        })
        .collect()
}

/// What the baseline says about one access of a target pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineAccess {
    /// The same access (the thread's same-numbered access of this kind at
    /// this instruction to this piece), after this many events.
    At(EventIndex),
    /// Not made: the thread armed at the same event in both runs and the
    /// baseline kept every hit it had in the window, in order.
    Absent,
    /// Not seen there, which proves nothing about it; why.
    Unobserved(String),
}

/// One side of a reported memory pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryAccess {
    pub thread: DetTid,
    pub kind: AccessKind,
    /// The instruction after the access.
    pub ip: u64,
    /// In the target, events recorded before it: it lies between event
    /// `after - 1` and event `after`.
    pub after: EventIndex,
    /// An exec came before it in its run (past the one that launched the
    /// program), so the image it ran in may not be the program: its frames
    /// are not named from the program's symbols, and no access of the other
    /// run is claimed to be it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub other_image: bool,
    pub baseline: BaselineAccess,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackFrame>,
}

/// The execs a run made before event `before`: the events of its realized
/// schedule that enter an execve or execveat. The first is the launch of the
/// program itself, so a run is known to be in the program's image only while
/// it has made at most one; which image a later exec loaded is not recorded
/// (a failed exec counts too, which only makes the answer more cautious).
pub fn execs_before(schedule: &[SchedEvent], before: EventIndex) -> usize {
    schedule
        .iter()
        .take(usize::try_from(before).unwrap_or(usize::MAX))
        .filter(|event| {
            matches!(
                event.op,
                Op::Syscall(Sysno::execve | Sysno::execveat, SyscallPhase::Prehook)
            )
        })
        .count()
}

/// Whether an access after `after` events of a run with `schedule` ran in
/// the program's image ([`execs_before`]).
fn in_program_image(schedule: &[SchedEvent], after: EventIndex) -> bool {
    execs_before(schedule, after) <= 1
}

/// Two threads' accesses to one piece, at least one a store: an access, and
/// the other thread's last access before it that conflicts with it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryPair {
    pub bytes: WatchedBytes,
    /// In target order.
    pub accesses: [MemoryAccess; 2],
    /// Their order in the baseline, by index into `accesses`; `None` unless
    /// both are found there.
    pub baseline_order: Option<[usize; 2]>,
    /// Whether an edge orders the first before the second in the target.
    pub target_hb: HbVerdict,
    /// The same in the baseline, from the access that comes first there;
    /// `None` unless both are found there.
    pub baseline_hb: Option<HbVerdict>,
}

impl MemoryPair {
    /// The baseline differs: the two accesses come in the other order there,
    /// or one of them is not made there.
    pub fn differs(&self) -> bool {
        self.baseline_order == Some([1, 0])
            || self
                .accesses
                .iter()
                .any(|access| access.baseline == BaselineAccess::Absent)
    }

    /// Both accesses are found in the baseline, in the same order.
    pub fn same_order(&self) -> bool {
        self.baseline_order == Some([0, 1])
    }
}

/// One side's view for [`memory_pairs`]: its accesses, the schedule it
/// realized and its edges.
pub struct SideView<'a> {
    pub hits: &'a PassHits,
    pub schedule: &'a [SchedEvent],
    pub edges: &'a [HbEdge],
}

/// A hit's identity across runs: its thread, its process and image, piece,
/// kind, instruction, and ordinal. Two runs' accesses in different images
/// (one exec'd and the other did not) never correspond, whatever their
/// addresses.
type Identity = (DetTid, Space, usize, AccessKind, u64, usize);

fn identity(hit: &Hit) -> Identity {
    (
        hit.thread,
        hit.space,
        hit.piece,
        hit.kind,
        hit.ip,
        hit.ordinal,
    )
}

/// Whether an edge orders hit `first` before hit `second` of `side`: from
/// `first`'s thread's next event at or after it to `second`'s thread's last
/// event before it. When `second` lies in its thread's first slice, before
/// any event of its own, the edge that created the thread stands for that
/// last event: whatever reaches the creating call is before it.
fn hb_between(side: &SideView<'_>, first: &Hit, second: &Hit) -> HbVerdict {
    let unordered = HbVerdict {
        ordered: false,
        edges: Vec::new(),
    };
    let next = side
        .schedule
        .iter()
        .enumerate()
        .skip(first.after as usize)
        .find(|(_, event)| event.dettid == first.thread)
        .map(|(index, _)| index as EventIndex);
    let previous = side.schedule[..(second.after as usize).min(side.schedule.len())]
        .iter()
        .rposition(|event| event.dettid == second.thread)
        .map(|index| index as EventIndex);
    let Some(next) = next else {
        return unordered;
    };
    let from = EventRef {
        tid: first.thread,
        event: next,
    };
    match previous {
        Some(to) => happens_before(
            side.edges,
            from,
            EventRef {
                tid: second.thread,
                event: to,
            },
        ),
        None => side
            .edges
            .iter()
            .find(|edge| edge.kind == HbEdgeKind::Create && edge.to.tid == second.thread)
            .map(|create| {
                let mut hb = happens_before(side.edges, from, create.from);
                if hb.ordered {
                    hb.edges.push(*create);
                }
                hb
            })
            .unwrap_or(unordered),
    }
}

/// What the baseline says about target access `hit`, given its index there
/// when found. A same-numbered access is the same one only when the thread
/// armed at the same event in both runs (its count starts at the same
/// point); an access not found there is absent only when the baseline kept
/// every hit the thread had and its pass did not run on its own.
fn baseline_access(
    hit: &Hit,
    target: &SideView<'_>,
    baseline: &SideView<'_>,
    found: Option<usize>,
) -> BaselineAccess {
    let thread = hit.thread;
    let armed = target
        .hits
        .coverage
        .get(&thread)
        .and_then(|coverage| coverage.armed_before);
    let Some(coverage) = baseline.hits.coverage.get(&thread) else {
        return BaselineAccess::Unobserved(format!(
            "thread {thread} recorded no event there once the watches were due"
        ));
    };
    if coverage.armed_before != armed {
        return BaselineAccess::Unobserved(format!(
            "thread {thread} armed its watches at another event there, so its accesses are \
             counted from another point"
        ));
    }
    // Which image an access after an exec ran is not recorded: two runs'
    // accesses are claimed to be one only in the program's image, by the
    // whole realized schedule, which also holds the program's launch (the
    // trace's own exec count includes that launch when its window starts
    // at event 0, so it is no test of the image).
    if !in_program_image(target.schedule, hit.after) {
        return BaselineAccess::Unobserved(
            "an exec came before it, and which image it ran in is not recorded, so no access of \
             the baseline is claimed to be it"
                .to_owned(),
        );
    }
    if let Some(index) = found {
        let matched = &baseline.hits.hits[index];
        if !in_program_image(baseline.schedule, matched.after) {
            return BaselineAccess::Unobserved(
                "the baseline's same-looking access came after an exec, and which image it ran \
                 in is not recorded"
                    .to_owned(),
            );
        }
        return BaselineAccess::At(matched.after);
    }
    if let Some(at) = baseline.hits.ran_on_own_from {
        return BaselineAccess::Unobserved(format!(
            "the baseline pass ran on its own from event {at}, and its hits from there are left \
             out"
        ));
    }
    if coverage.complete() && execs_before(baseline.schedule, EventIndex::MAX) > 1 {
        return BaselineAccess::Unobserved(
            "the baseline run exec'd, so which image its accesses ran in is not recorded"
                .to_owned(),
        );
    }
    if coverage.complete() {
        BaselineAccess::Absent
    } else {
        BaselineAccess::Unobserved(format!(
            "it is past the first {} hits of thread {thread} the baseline kept in order",
            coverage.in_order
        ))
    }
}

/// The pairs of `pass`: each access, with each other thread's last access
/// before it to the same piece that conflicts with it (a store before a
/// load, anything before a store), among their process's accesses between
/// two of its execs; each with its order and happens-before verdict in both
/// runs where the baseline shows both; and notes on the accesses left out.
/// Pairs whose baseline differs come first, then the rest, each group in
/// target order.
pub fn memory_pairs(
    pass: &[WatchedBytes],
    target: &SideView<'_>,
    baseline: &SideView<'_>,
) -> (Vec<MemoryPair>, Vec<String>) {
    let mut notes = Vec::new();
    let baseline_index: BTreeMap<Identity, usize> = baseline
        .hits
        .hits
        .iter()
        .enumerate()
        .map(|(index, hit)| (identity(hit), index))
        .collect();
    let mut unknown_space: Vec<DetTid> = Vec::new();
    let mut pairs = Vec::new();
    let mut repeated = 0usize;
    for (piece, bytes) in pass.iter().enumerate() {
        // The piece's accesses by address space, in execution order.
        let mut by_space: BTreeMap<(DetPid, usize), Vec<usize>> = BTreeMap::new();
        for (index, hit) in target.hits.hits.iter().enumerate() {
            if hit.piece != piece {
                continue;
            }
            match hit.space {
                Some(space) => by_space.entry(space).or_default().push(index),
                None if !unknown_space.contains(&hit.thread) => unknown_space.push(hit.thread),
                None => {}
            }
        }
        for indices in by_space.values() {
            // Each thread's last access and last store so far, and the
            // accesses already paired with a later access of each thread.
            let mut last: BTreeMap<DetTid, (usize, Option<usize>)> = BTreeMap::new();
            let mut paired: BTreeSet<(usize, DetTid)> = BTreeSet::new();
            for &b in indices {
                let second = &target.hits.hits[b];
                for (&thread, &(any, store)) in &last {
                    if thread == second.thread {
                        continue;
                    }
                    let first = match second.kind {
                        AccessKind::Store => Some(any),
                        AccessKind::Load => store,
                    };
                    let Some(a) = first else {
                        continue;
                    };
                    // One store against a thread's hundred loads is one
                    // pair: the first; the rest are counted.
                    if paired.insert((a, second.thread)) {
                        pairs.push(pair(bytes, (a, b), target, baseline, &baseline_index));
                    } else {
                        repeated += 1;
                    }
                }
                let entry = last.entry(second.thread).or_insert((b, None));
                entry.0 = b;
                if second.kind == AccessKind::Store {
                    entry.1 = Some(b);
                }
            }
        }
    }
    if repeated > 0 {
        notes.push(format!(
            "{repeated} later access(es) conflicting with an access already paired with an access \
             of their thread are not paired again."
        ));
    }
    for thread in unknown_space {
        notes.push(format!(
            "Thread {thread}'s accesses are not paired: it made no syscall in the trace, so its \
             process, and whether its address is the others' memory, are unknown."
        ));
    }
    for (role, side) in [("target", target), ("baseline", baseline)] {
        for (thread, coverage) in &side.hits.coverage {
            if !coverage.complete() {
                notes.push(format!(
                    "The {role} pass kept only the first {} of thread {thread}'s hits in order; \
                     its later ones are left out, since Detcore writes them when the run ends, \
                     out of execution order.",
                    coverage.in_order
                ));
            }
        }
    }
    pairs.sort_by_key(|pair| !(pair.differs() && !pair.target_hb.ordered));
    (pairs, notes)
}

/// The pair of target accesses `a` before `b`, with what the baseline says
/// about each.
fn pair(
    bytes: &WatchedBytes,
    (a, b): (usize, usize),
    target: &SideView<'_>,
    baseline: &SideView<'_>,
    baseline_index: &BTreeMap<Identity, usize>,
) -> MemoryPair {
    let (first, second) = (&target.hits.hits[a], &target.hits.hits[b]);
    let found = [first, second].map(|hit| baseline_index.get(&identity(hit)).copied());
    let views = [
        baseline_access(first, target, baseline, found[0]),
        baseline_access(second, target, baseline, found[1]),
    ];
    let both = match (&views, found) {
        ([BaselineAccess::At(_), BaselineAccess::At(_)], [Some(x), Some(y)]) => Some((x, y)),
        _ => None,
    };
    let (baseline_order, baseline_hb) = match both {
        Some((x, y)) if x < y => (
            Some([0, 1]),
            Some(hb_between(
                baseline,
                &baseline.hits.hits[x],
                &baseline.hits.hits[y],
            )),
        ),
        Some((x, y)) => (
            Some([1, 0]),
            Some(hb_between(
                baseline,
                &baseline.hits.hits[y],
                &baseline.hits.hits[x],
            )),
        ),
        None => (None, None),
    };
    let [first_view, second_view] = views;
    let access = |hit: &Hit, view: BaselineAccess| MemoryAccess {
        thread: hit.thread,
        kind: hit.kind,
        ip: hit.ip,
        after: hit.after,
        other_image: !in_program_image(target.schedule, hit.after),
        baseline: view,
        stack: hit.stack.clone(),
    };
    MemoryPair {
        bytes: bytes.clone(),
        accesses: [access(first, first_view), access(second, second_view)],
        baseline_order,
        target_hb: hb_between(target, first, second),
        baseline_hb,
    }
}

/// What the watch stage concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchStatus {
    /// Two threads' accesses to watched bytes, at least one a store, that
    /// no edge Hermit observes orders in the target.
    Found,
    /// The passes ran and found no such pair.
    NoneFound,
    /// Nothing to watch, or the passes could not run.
    Unavailable,
}

/// The watch stage's part of the section.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WatchStage {
    pub status: WatchStatus,
    /// Why, in a sentence.
    pub reason: String,
    /// The first event the passes watched from.
    pub from: EventIndex,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched: Vec<WatchedBytes>,
    /// Candidates past the pass cap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unwatched: Vec<WatchedBytes>,
    /// Ranges past those, not listed.
    #[serde(default)]
    pub more_unwatched: u64,
    /// Pieces whose pass was left out (it left its side's schedule before
    /// the deciding event, its outcome differed, or its trace could not be
    /// used): not watched after all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<WatchedBytes>,
    /// The syscall finder named a pair, and only the bytes --race-watch names
    /// were watched, beside it.
    #[serde(default)]
    pub beside_finder: bool,
    /// Some accesses were not seen for reasons the other lists do not show:
    /// a thread's later hits not kept in order, a thread whose process is
    /// unknown, or a pass cut where it ran on its own (the notes say which).
    #[serde(default)]
    pub partial: bool,
    /// Threads that armed their watches only after their first event in the
    /// window, and before which event: their accesses before then are not
    /// seen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub late: Vec<(DetTid, EventIndex)>,
    /// The reported pair first, then up to [`MAX_PAIRS`] - 1 others.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairs: Vec<MemoryPair>,
    /// Pairs past those.
    #[serde(default)]
    pub more_pairs: u64,
    /// Pairs an edge Hermit observes orders in the target: not reported.
    #[serde(default)]
    pub ordered_pairs: u64,
    /// Threads that never armed in some pass, and why: their accesses are
    /// not seen, so a missing hit is no missing access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unobserved: Vec<(DetTid, WatchUnarmed)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The passes' traces, target and baseline of each, as file names in the
    /// analysis workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trace_files: Vec<String>,
}

impl WatchStage {
    pub fn unavailable(from: EventIndex, reason: String) -> Self {
        WatchStage {
            status: WatchStatus::Unavailable,
            reason,
            from,
            watched: Vec::new(),
            unwatched: Vec::new(),
            more_unwatched: 0,
            dropped: Vec::new(),
            beside_finder: false,
            partial: false,
            late: Vec::new(),
            pairs: Vec::new(),
            more_pairs: 0,
            ordered_pairs: 0,
            unobserved: Vec::new(),
            notes: Vec::new(),
            trace_files: Vec::new(),
        }
    }

    /// Set the status and reason from the pairs, keeping at most
    /// [`MAX_PAIRS`] of them. A pair no edge orders is reported whether or not
    /// the baseline differs: the deciding event can move one access far from
    /// the other without reordering them, and an unordered pair is what a
    /// sanitizer reports either way. Pairs whose baseline differs come
    /// first, then those in the same order there, then those the baseline
    /// does not show.
    pub fn conclude(&mut self) {
        let before = self.pairs.len();
        self.pairs.retain(|pair| !pair.target_hb.ordered);
        self.ordered_pairs += (before - self.pairs.len()) as u64;
        self.pairs
            .sort_by_key(|pair| (!pair.differs(), !pair.same_order()));
        let unordered = "two threads' accesses to watched bytes are unordered by the edges Hermit \
                         observes";
        let (status, reason) = match self.pairs.first() {
            Some(pair) if pair.baseline_order == Some([1, 0]) => (
                WatchStatus::Found,
                format!("{unordered}, and their order differs between the two runs"),
            ),
            Some(pair) if pair.differs() => (
                WatchStatus::Found,
                format!(
                    "{unordered}, and the baseline does not make one of them (it kept every hit \
                     of that thread)"
                ),
            ),
            Some(pair) if pair.same_order() => (
                WatchStatus::Found,
                format!("{unordered}; they keep their order in both runs"),
            ),
            Some(_) => (
                WatchStatus::Found,
                format!("{unordered}; the baseline does not show both of them"),
            ),
            _ => self.no_unordered_pair(),
        };
        self.status = status;
        self.reason = reason;
        if self.pairs.len() > MAX_PAIRS {
            self.more_pairs = (self.pairs.len() - MAX_PAIRS) as u64;
            self.pairs.truncate(MAX_PAIRS);
        }
    }

    /// The status and reason when no pair is unordered: among the accesses
    /// the passes saw, and saying so when some were not seen.
    fn no_unordered_pair(&self) -> (WatchStatus, String) {
        let found = if self.ordered_pairs == 0 {
            "no eligible pair (two threads of one process in one exec image, one of them storing) \
             accessed the same watched bytes, among the threads that armed their watches"
        } else {
            "an edge Hermit observes orders every eligible pair (two threads of one process in one \
             exec image, one of them storing) of accesses to watched bytes, among the threads that \
             armed their watches"
        };
        let partial = self.partial
            || !self.unobserved.is_empty()
            || !self.late.is_empty()
            || !self.dropped.is_empty()
            || !self.unwatched.is_empty();
        let reason = if partial {
            format!("{found}; some accesses were not seen (listed below)")
        } else {
            found.to_owned()
        };
        (WatchStatus::NoneFound, reason)
    }
}

/// Name the frames of the stage's accesses from `program`'s symbol table,
/// as `symbol+offset`, where the program is not position-independent (its
/// file addresses are its run addresses); frames elsewhere keep their
/// address alone. Each frame's address is the instruction after the access
/// or the call, so the byte before it is looked up. Returns a note when the
/// program cannot name them.
pub fn name_frames(program: &std::path::Path, stage: &mut WatchStage) -> Option<String> {
    use object::Object;
    use object::ObjectKind;
    use object::ObjectSymbol;
    use object::SymbolKind;

    if stage.pairs.is_empty() {
        return None;
    }
    let data = match std::fs::read(program) {
        Ok(data) => data,
        Err(error) => {
            return Some(format!(
                "The frames are addresses: {} cannot be read ({error}).",
                program.display()
            ));
        }
    };
    let object = match object::File::parse(&*data) {
        Ok(object) => object,
        Err(error) => {
            return Some(format!(
                "The frames are addresses: {} is not an object file ({error}).",
                program.display()
            ));
        }
    };
    if object.kind() != ObjectKind::Executable {
        return Some(format!(
            "The frames are addresses: {} is position-independent, so its file addresses are \
             not the run's.",
            program.display()
        ));
    }
    use object::ObjectSection;
    let mut symbols: Vec<(u64, u64, String, Option<u64>)> = object
        .symbols()
        .filter(|sym| sym.kind() == SymbolKind::Text && sym.address() != 0)
        .filter_map(|sym| {
            let section_end = sym
                .section_index()
                .and_then(|index| object.section_by_index(index).ok())
                .and_then(|section| section.address().checked_add(section.size()));
            Some((
                sym.address(),
                sym.size(),
                sym.name().ok()?.to_owned(),
                section_end,
            ))
        })
        .filter(|(_, _, name, _)| !name.is_empty())
        .collect();
    symbols.sort();
    let name = |ip: u64| {
        let at = ip.checked_sub(1)?;
        let index = symbols
            .partition_point(|(addr, _, _, _)| *addr <= at)
            .checked_sub(1)?;
        let (addr, size, name, section_end) = &symbols[index];
        let next = symbols[index + 1..]
            .iter()
            .map(|(next, ..)| *next)
            .find(|next| next > addr);
        (at < symbol_end(*addr, *size, next, *section_end))
            .then(|| format!("{name}+{:#x}", ip - addr))
    };
    apply_frame_names(stage, name)
}

/// Name the frames of the stage's accesses with `name`, except those of an
/// access that may have run in another image (an exec came before it, inside
/// the window or before it): its frames keep their addresses. Returns a note
/// when any did.
fn apply_frame_names(
    stage: &mut WatchStage,
    name: impl Fn(u64) -> Option<String>,
) -> Option<String> {
    let mut other_image = false;
    for pair in &mut stage.pairs {
        for access in &mut pair.accesses {
            if access.other_image {
                other_image |= !access.stack.is_empty();
                continue;
            }
            for frame in &mut access.stack {
                if frame.function.is_none() {
                    frame.function = name(frame.ip);
                }
            }
        }
    }
    other_image.then(|| {
        "The frames of accesses made after an exec are addresses: the image they ran in may not \
         be the program whose symbols name the others."
            .to_owned()
    })
}

/// Where a text symbol ends: at its size, or for a symbol of size zero (an
/// assembler label) at the next symbol or the end of its section, whichever
/// comes first, and nowhere past `addr` when neither is known, so a frame
/// in another module never takes its name.
fn symbol_end(addr: u64, size: u64, next: Option<u64>, section_end: Option<u64>) -> u64 {
    if size > 0 {
        return addr.saturating_add(size);
    }
    [next, section_end]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(addr)
}

/// The report lines of a watch stage.
pub fn render_watch_stage(stage: &WatchStage) -> Vec<String> {
    let mut lines = Vec::new();
    let status = match stage.status {
        WatchStatus::Found => "found",
        WatchStatus::NoneFound => "none found",
        WatchStatus::Unavailable => "unavailable",
    };
    let scope = if stage.beside_finder {
        " (only the bytes --race-watch names, beside the syscall finder's pair above; a \
         synchronization pair can decide when a race is exposed without being its memory root \
         cause, and a watched pair need not be what decides the outcome)"
    } else {
        ""
    };
    lines.push(format!(
        "Watched memory{scope}: {status}: {}.",
        stage.reason
    ));
    if !stage.watched.is_empty() {
        lines.push(format!(
            "  Watched from event {} on, in both runs:",
            stage.from
        ));
        for bytes in &stage.watched {
            lines.push(format!("    {bytes}"));
        }
    }
    for bytes in &stage.unwatched {
        lines.push(format!("  Not watched (past {MAX_PASSES} passes): {bytes}"));
    }
    for bytes in &stage.dropped {
        lines.push(format!("  Not watched (its pass was left out): {bytes}"));
    }
    if stage.more_unwatched > 0 {
        lines.push(format!(
            "  {} more range(s) not watched.",
            stage.more_unwatched
        ));
    }
    for (index, pair) in stage.pairs.iter().enumerate() {
        let title = if index == 0 { "Pair" } else { "Also" };
        lines.push(format!("  {title} on {}:", pair.bytes));
        match pair.bytes.source {
            WatchSource::FutexWord => lines.push(
                "    These bytes are the futex word of the lock or condition variable the \
                 deciding futex call waits on: the accesses are its own operations (taking, \
                 releasing, signalling), not accesses to the data it guards, such as a flag an \
                 assertion reads."
                    .to_owned(),
            ),
            WatchSource::NearFutexWord => lines.push(
                "    These bytes lie next to the futex word of the deciding futex call: they may \
                 be the rest of its lock or condition variable, or data beside it."
                    .to_owned(),
            ),
            _ => {}
        }
        for (n, access) in pair.accesses.iter().enumerate() {
            let baseline = match &access.baseline {
                BaselineAccess::At(after) => {
                    format!("after event {} in the baseline", after.saturating_sub(1))
                }
                BaselineAccess::Absent => "not made in the baseline".to_owned(),
                BaselineAccess::Unobserved(_) => "not seen in the baseline".to_owned(),
            };
            lines.push(format!(
                "    {}. thread {} {}s at ip {:#x}, after event {} in the target, {baseline}",
                n + 1,
                access.thread,
                access.kind,
                access.ip,
                access.after.saturating_sub(1)
            ));
            for frame in access.stack.iter().take(8) {
                let mut text = format!("         {:#x}", frame.ip);
                if let Some(function) = &frame.function {
                    text.push_str(&format!(" {function}"));
                }
                if let (Some(file), Some(line)) = (&frame.file, frame.line) {
                    text.push_str(&format!(" {file}:{line}"));
                }
                lines.push(text);
            }
        }
        lines.push(format!(
            "    Target: {}.",
            hb_sentence(&pair.target_hb, "1", "2")
        ));
        let [a, b] = &pair.accesses;
        let at = |access: &MemoryAccess| match access.baseline {
            BaselineAccess::At(after) => after,
            _ => 0,
        };
        lines.push(match (pair.baseline_order, &pair.baseline_hb) {
            (Some([0, 1]), Some(hb)) => {
                // Same order: say how far apart they are in each run, and
                // never that the order decides.
                let gap = |first: EventIndex, second: EventIndex| second.saturating_sub(first);
                format!(
                    "    Baseline: the same order in both runs ({}, then {}); in the target the \
                     {} comes {} events after the {}, in the baseline {}; {}.",
                    a.kind,
                    b.kind,
                    b.kind,
                    gap(a.after, b.after),
                    a.kind,
                    gap(at(a), at(b)),
                    hb_sentence(hb, "1", "2")
                )
            }
            (Some(_), Some(hb)) => format!(
                "    Baseline: the opposite order, {}.",
                hb_sentence(hb, "2", "1")
            ),
            _ => {
                let said: Vec<String> = pair
                    .accesses
                    .iter()
                    .enumerate()
                    .filter_map(|(n, access)| match &access.baseline {
                        BaselineAccess::At(_) => None,
                        BaselineAccess::Absent => Some(format!(
                            "{} is not made there (it kept every hit of thread {})",
                            n + 1,
                            access.thread
                        )),
                        BaselineAccess::Unobserved(why) => {
                            Some(format!("{} is not seen there ({why})", n + 1))
                        }
                    })
                    .collect();
                format!(
                    "    Baseline: {}; no order there is claimed.",
                    said.join("; ")
                )
            }
        });
    }
    if stage.more_pairs > 0 {
        lines.push(format!(
            "  {} more unordered pair(s) not listed.",
            stage.more_pairs
        ));
    }
    if stage.ordered_pairs > 0 {
        lines.push(format!(
            "  {} pair(s) that an edge Hermit observes orders are not listed.",
            stage.ordered_pairs
        ));
    }
    for (thread, why) in &stage.unobserved {
        let why = match why {
            WatchUnarmed::SigtrapBlocked => "it blocked SIGTRAP at every stop it could arm at",
            WatchUnarmed::SigtrapIgnored => "its process ignored SIGTRAP at every such stop",
            WatchUnarmed::NoStop => "it made no stop at which it could arm",
        };
        lines.push(format!(
            "  Thread {thread} never armed its watches ({why}): it was not observed, so that the \
             passes show no access of it is no evidence it made none."
        ));
    }
    for (thread, armed_before) in &stage.late {
        lines.push(format!(
            "  Thread {thread} armed its watches only before event {armed_before}, after its \
             first event in the window: its accesses before then are not seen."
        ));
    }
    for note in &stage.notes {
        lines.push(format!("  Note: {note}"));
    }
    lines
}

fn hb_sentence(hb: &HbVerdict, first: &str, second: &str) -> String {
    if hb.ordered {
        format!(
            "{first} before {second}, ordered by {} edge(s) Hermit observes",
            hb.edges.len()
        )
    } else {
        format!("{first} before {second}, unordered by the edges Hermit observes")
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::access_trace::SyscallRecord;
    use detcore_model::access_trace::TraceEnd;
    use detcore_model::access_trace::TraceHeader;
    use detcore_model::access_trace::WatchHitRecord;
    use detcore_model::access_trace::WatchSummaryRecord;

    use super::*;

    fn tid(n: u32) -> DetTid {
        DetTid::from_raw(n as i32)
    }

    const TSAN: &str = "\
WARNING: ThreadSanitizer: data race (pid=3)
  Read of size 8 at 0x000002cb4a70 by thread T1:
    #0 getInstance() Instance.cpp:21:10 (test_binary+0x1073910)
  Previous write of size 8 at 0x000002cb4a70 by main thread:
    #0 setInstance() Instance.cpp:25:17 (test_binary+0x1073a1f)
  Location is global 'getInstance()::sInstance' of size 8 at 0x000002cb4a70 (test_binary+0x2cb4a70)
WARNING: ThreadSanitizer: data race on vptr (ctor/dtor vs virtual call) (pid=3)
  Read of size 8 at 0x726800000c00 by thread T1:
  Previous write of size 8 at 0x726800000c00 by main thread:
  Location is heap block of size 1520 at 0x726800000c00 allocated by main thread:
";

    /// A ThreadSanitizer report gives each racing address once, named by
    /// its global where the report names one.
    #[test]
    fn a_sanitizer_report_names_the_racing_bytes() {
        let found = sanitizer_candidates(TSAN);
        assert_eq!(
            found,
            vec![
                Candidate {
                    addr: 0x2cb4a70,
                    len: 8,
                    name: Some("getInstance()::sInstance".to_owned()),
                    source: WatchSource::SanitizerReport,
                },
                Candidate {
                    addr: 0x726800000c00,
                    len: 8,
                    name: None,
                    source: WatchSource::SanitizerReport,
                },
            ]
        );
        let asan = "WRITE of size 4 at 0x602000000010 thread T1\n";
        assert_eq!(sanitizer_candidates(asan)[0].addr, 0x602000000010);
    }

    /// A range is cut into aligned pieces that cover exactly its bytes, and
    /// passes take two pieces each up to the cap.
    #[test]
    fn ranges_become_aligned_pieces_and_capped_passes() {
        let c = |addr, len| Candidate {
            addr,
            len,
            name: None,
            source: WatchSource::RaceWatch,
        };
        let cut: Vec<(u64, u8)> = pieces(&c(0x1003, 13))
            .iter()
            .map(|p| (p.addr, p.len))
            .collect();
        assert_eq!(
            cut,
            vec![(0x1003, 1), (0x1004, 4), (0x1008, 8)],
            "13 bytes from 0x1003"
        );
        let (passes, unwatched, _) = plan_passes(&[c(0x2000, 64), c(0x3000, 64), c(0x2000, 8)]);
        assert_eq!(passes.len(), MAX_PASSES);
        assert!(passes.iter().all(|p| p.len() == PIECES_PER_PASS));
        assert_eq!(unwatched.len(), 0);
        let (passes, unwatched, _) = plan_passes(&[c(0x2000, 64), c(0x3000, 64), c(0x4000, 16)]);
        assert_eq!(passes.len(), MAX_PASSES);
        assert_eq!(unwatched.len(), 2, "the third range is past the cap");
        assert_eq!(unwatched[0].addr, 0x4000);
        let watches = pass_watches(&passes[0]);
        assert_eq!(
            watches
                .iter()
                .map(|w| (w.addr, w.access))
                .collect::<Vec<_>>(),
            vec![
                (0x2000, WatchAccess::Write),
                (0x2000, WatchAccess::ReadWrite),
                (0x2008, WatchAccess::Write),
                (0x2008, WatchAccess::ReadWrite),
            ]
        );
    }

    fn hit(after: EventIndex, t: u32, slot: u8, ip: u64) -> TraceRecord {
        TraceRecord::WatchHit(WatchHitRecord {
            after,
            tid: tid(t),
            slot,
            addr: 0x2cb4a70,
            ip,
            stack: Vec::new(),
        })
    }

    /// A store's two rows, write slot first, and a load's one row, on
    /// piece 0.
    fn store(after: EventIndex, t: u32, ip: u64) -> [TraceRecord; 2] {
        [hit(after, t, 0, ip), hit(after, t, 1, ip)]
    }

    fn load(after: EventIndex, t: u32, ip: u64) -> TraceRecord {
        hit(after, t, 1, ip)
    }

    fn summary(t: u32, armed_before: EventIndex, hits: u64, recorded: u64) -> TraceRecord {
        TraceRecord::WatchSummary(WatchSummaryRecord {
            tid: tid(t),
            armed_before: Some(armed_before),
            unarmed: None,
            hits,
            recorded,
        })
    }

    /// A call by thread `t` of process `pid` at event `pre`; an execve that
    /// never returns when `exec`.
    fn syscall(t: u32, pid: i32, pre: EventIndex, exec: bool) -> TraceRecord {
        TraceRecord::Syscall(SyscallRecord {
            pre,
            post: (!exec).then_some(pre + 1),
            tid: tid(t),
            pid: DetPid::from_raw(pid),
            nth: 1,
            sysno: if exec { Sysno::execve } else { Sysno::getpid },
            args: [0; 6],
            result: (!exec).then_some(pid as i64),
            accesses: Vec::new(),
            returned: None,
            out_bytes: None,
        })
    }

    fn trace(records: Vec<TraceRecord>) -> AccessTrace {
        AccessTrace {
            header: TraceHeader {
                format: "hermit-access-trace".to_owned(),
                version: 1,
                from: 0,
            },
            end: Some(TraceEnd {
                events: 0,
                records: records.len() as u64,
            }),
            records,
        }
    }

    /// A pass trace as Detcore writes one with nothing left out: a call of
    /// process 3 for each thread with rows, the rows, then each thread's
    /// summary (armed before event 0, every hit written).
    fn complete(rows: Vec<TraceRecord>) -> AccessTrace {
        let mut counts: BTreeMap<DetTid, u64> = BTreeMap::new();
        for row in &rows {
            if let TraceRecord::WatchHit(hit) = row {
                *counts.entry(hit.tid).or_default() += 1;
            }
        }
        let mut records: Vec<TraceRecord> = counts
            .keys()
            .map(|t| syscall(t.as_raw() as u32, 3, 0, false))
            .collect();
        records.extend(rows);
        records.extend(
            counts
                .iter()
                .map(|(t, n)| summary(t.as_raw() as u32, 0, *n, *n)),
        );
        trace(records)
    }

    fn kinds(hits: &PassHits) -> Vec<(u32, AccessKind, u64)> {
        hits.hits
            .iter()
            .map(|h| (h.thread.as_raw() as u32, h.kind, h.ip))
            .collect()
    }

    /// A write-slot row and the read-write row after it, at one instruction
    /// and one `after`, are one store; a read-write row alone is a load,
    /// also when a store at the same instruction follows it; a loop of
    /// stores with no event between them stays one store an iteration.
    #[test]
    fn hits_on_both_slots_are_a_store_and_the_read_write_slot_alone_a_load() {
        let mut rows = store(10, 3, 0x100).to_vec();
        rows.push(load(12, 5, 0x200));
        rows.push(load(14, 3, 0x300));
        rows.extend(store(14, 3, 0x300));
        rows.extend(store(14, 3, 0x300));
        let got = hits(&complete(rows), 1).unwrap();
        assert_eq!(
            kinds(&got),
            vec![
                (3, AccessKind::Store, 0x100),
                (5, AccessKind::Load, 0x200),
                (3, AccessKind::Load, 0x300),
                (3, AccessKind::Store, 0x300),
                (3, AccessKind::Store, 0x300),
            ]
        );
        let unarmed = TraceRecord::WatchSummary(WatchSummaryRecord {
            tid: tid(7),
            armed_before: None,
            unarmed: Some(WatchUnarmed::SigtrapBlocked),
            hits: 0,
            recorded: 0,
        });
        assert_eq!(
            unobserved(&trace(vec![unarmed])),
            vec![(tid(7), WatchUnarmed::SigtrapBlocked)]
        );
        let mut open = complete(store(10, 3, 0x100).to_vec());
        open.end = None;
        assert!(hits(&open, 1).is_err(), "a trace without its end line");
        let unsummarized = trace(store(10, 3, 0x100).to_vec());
        assert!(hits(&unsummarized, 1).is_err(), "rows without a summary");
    }

    /// 100 stores and then a load by one thread make 201 rows: Detcore
    /// writes the first 64 as they happen and the last 64 when the run ends,
    /// and the first of those is a store's read-write row whose write row it
    /// dropped. Only the 32 stores written in order are accesses, and none
    /// of them reads as a load.
    #[test]
    fn a_store_cut_from_its_write_row_is_never_a_load() {
        let mut rows = Vec::new();
        for i in 0..100 {
            rows.extend(store(10 + i, 3, 0x100));
        }
        rows.push(load(200, 3, 0x200));
        let tail = rows.split_off(64);
        let kept_tail = tail[tail.len() - 64..].to_vec();
        assert_eq!(kept_tail[0], hit(10 + 68, 3, 1, 0x100), "the cut store");
        let mut records = vec![syscall(3, 3, 0, false)];
        records.extend(rows);
        records.extend(kept_tail);
        records.push(summary(3, 0, 201, 128));
        let got = hits(&trace(records), 1).unwrap();
        assert_eq!(got.hits.len(), 32);
        assert!(got.hits.iter().all(|h| h.kind == AccessKind::Store));
        assert_eq!(
            got.coverage[&tid(3)],
            Coverage {
                armed_before: Some(0),
                hits: 201,
                in_order: 64,
                deferred: 64,
            }
        );
        assert!(!got.coverage[&tid(3)].complete());
    }

    /// The order Detcore writes hits in: thread 5's first 64 as they happen,
    /// thread 3's store after event 900 as it happens, then thread 5's later
    /// load after event 800, written when the run ends. That load comes
    /// before the store in execution order, but after it in the file; it is
    /// left out rather than paired in the file's order, and the store pairs
    /// only with thread 5's stores, which an edge orders before it.
    #[test]
    fn hits_written_when_the_run_ends_are_never_paired() {
        let mut records = vec![syscall(3, 3, 0, false), syscall(5, 3, 0, false)];
        for i in 0..32 {
            records.extend(store(10 + i, 5, 0x100));
        }
        records.extend(store(900, 3, 0x300));
        records.push(load(800, 5, 0x200));
        records.push(summary(3, 0, 2, 2));
        records.push(summary(5, 0, 65, 65));
        let got = hits(&trace(records), 1).unwrap();
        assert!(
            got.hits
                .iter()
                .all(|h| h.kind == AccessKind::Store && h.after != 800),
            "{:?}",
            kinds(&got)
        );
        let schedule = sched(&[5; 1000]);
        let edge = HbEdge {
            from: EventRef {
                tid: tid(5),
                event: 42,
            },
            to: EventRef {
                tid: tid(3),
                event: 899,
            },
            kind: crate::analyze::races::HbEdgeKind::FutexWake,
        };
        let mut schedule = schedule;
        schedule[899] = SchedEvent::branches(tid(3), 1);
        let edges = [edge];
        let view = side(&got, &schedule, &edges);
        let (pairs, notes) = memory_pairs(&singleton(), &view, &view);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].target_hb.ordered, "{:?}", pairs[0]);
        assert!(
            pairs[0]
                .accesses
                .iter()
                .all(|a| a.kind == AccessKind::Store),
            "{:?}",
            pairs[0]
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("kept only the first 64 of thread 5's hits")),
            "{notes:?}"
        );
        let stage = concluded(pairs);
        assert_eq!(stage.status, WatchStatus::NoneFound);
    }

    fn sched(threads: &[u32]) -> Vec<SchedEvent> {
        threads
            .iter()
            .map(|&t| SchedEvent::branches(tid(t), 1))
            .collect()
    }

    /// `--race-watch` takes an address or a symbol, each with an optional
    /// length; a symbol takes an offset. A symbol in a position-independent
    /// program is not placed, and says why.
    #[test]
    fn race_watch_takes_an_address_or_a_symbol() {
        let parse = |s: &str| s.parse::<RaceWatchArg>();
        assert_eq!(
            parse("0x2cb4a70"),
            Ok(RaceWatchArg {
                at: RaceWatchAt::Addr(0x2cb4a70),
                len: None
            })
        );
        assert_eq!(
            parse("0xc897d8:1"),
            Ok(RaceWatchArg {
                at: RaceWatchAt::Addr(0xc897d8),
                len: Some(1)
            })
        );
        let symbol = |name: &str, offset| RaceWatchAt::Symbol {
            name: name.to_owned(),
            offset,
        };
        assert_eq!(
            parse("_ZZN3app12_GLOBAL__N_111getInstanceEvE9sInstance"),
            Ok(RaceWatchArg {
                at: symbol("_ZZN3app12_GLOBAL__N_111getInstanceEvE9sInstance", 0),
                len: None
            })
        );
        assert_eq!(
            parse("g_flag+0x8:4"),
            Ok(RaceWatchArg {
                at: symbol("g_flag", 8),
                len: Some(4)
            })
        );
        assert!(parse("0x1000:65").is_err());
        assert!(parse("g_flag+x").is_err());
        assert!(parse("1abc").is_err());

        let (candidates, notes) = race_watch_candidates(
            &std::env::current_exe().unwrap(),
            &[parse("main").unwrap(), parse("0x1000:2").unwrap()],
        );
        assert_eq!(
            candidates,
            vec![Candidate {
                addr: 0x1000,
                len: 2,
                name: None,
                source: WatchSource::RaceWatch,
            }]
        );
        assert!(
            notes.len() == 1 && notes[0].contains("position-independent"),
            "{notes:?}"
        );
    }

    /// A range that would run past the end of the address space is refused,
    /// never cut, whether it is given, comes from a symbol, or lies around a
    /// futex word; so is a thread-local symbol, whose value is no address.
    #[test]
    fn ranges_past_the_end_of_the_address_space_are_refused() {
        assert_eq!(
            Candidate::new(0xffff_ffff_ffff_fffe, 8, None, WatchSource::RaceWatch),
            None
        );
        let (candidates, notes) = race_watch_candidates(
            std::path::Path::new("/nonexistent"),
            &["0xfffffffffffffffe:8".parse::<RaceWatchArg>().unwrap()],
        );
        assert!(candidates.is_empty());
        assert!(notes[0].contains("past the end"), "{notes:?}");
        let last = Candidate::new(u64::MAX - 7, 7, None, WatchSource::RaceWatch).unwrap();
        let cut: Vec<(u64, u8)> = pieces(&last).iter().map(|p| (p.addr, p.len)).collect();
        assert_eq!(
            cut,
            vec![(u64::MAX - 7, 4), (u64::MAX - 3, 2), (u64::MAX - 1, 1)]
        );
        use object::SymbolKind;
        assert_eq!(
            place_symbol(SymbolKind::Data, true, (0x1000, 16), 4, None),
            Ok((0x1004, 12))
        );
        assert!(place_symbol(SymbolKind::Data, true, (0x1000, 16), u64::MAX, None).is_err());
        assert!(place_symbol(SymbolKind::Data, true, (u64::MAX - 4, 8), 0, None).is_err());
        assert!(
            place_symbol(SymbolKind::Tls, true, (0x4, 8), 0, None)
                .unwrap_err()
                .contains("thread-local")
        );
        assert!(place_symbol(SymbolKind::Data, false, (0, 0), 0, None).is_err());

        let futex_at = |word: u64| {
            let mut sys = match syscall(3, 3, 5, false) {
                TraceRecord::Syscall(sys) => sys,
                _ => unreachable!(),
            };
            sys.sysno = Sysno::futex;
            sys.args[0] = word;
            trace_candidates(&trace(vec![TraceRecord::Syscall(sys)]), 5)
        };
        let low: Vec<(u64, u64)> = futex_at(4).iter().map(|c| (c.addr, c.len)).collect();
        assert_eq!(low, vec![(4, 4), (8, 40)], "nothing before address 0");
        let high: Vec<(u64, u64)> = futex_at(u64::MAX - 7)
            .iter()
            .map(|c| (c.addr, c.len))
            .collect();
        assert_eq!(
            high,
            vec![(u64::MAX - 7, 4), (u64::MAX - 47, 40)],
            "nothing after the last word"
        );

        let many: Vec<Candidate> = (0..100)
            .map(|i| Candidate::new(0x10000 + i * 0x100, 8, None, WatchSource::SanitizerReport))
            .collect::<Option<_>>()
            .unwrap();
        let (passes, unwatched, more) = plan_passes(&many);
        assert_eq!(passes.len(), MAX_PASSES);
        assert_eq!(unwatched.len(), MAX_UNWATCHED_LISTED);
        assert_eq!(
            more,
            100 - MAX_PASSES * PIECES_PER_PASS - MAX_UNWATCHED_LISTED
        );
    }

    /// A text symbol of size zero ends at the next symbol or the end of its
    /// section, and at itself when neither is known, so a frame in another
    /// module keeps its address.
    #[test]
    fn a_zero_size_symbol_ends_at_the_next_symbol_or_its_section() {
        assert_eq!(symbol_end(0x1000, 0x20, Some(0x1100), None), 0x1020);
        assert_eq!(symbol_end(0x1000, 0, Some(0x1100), Some(0x2000)), 0x1100);
        assert_eq!(symbol_end(0x1f00, 0, None, Some(0x2000)), 0x2000);
        assert_eq!(symbol_end(0x1f00, 0, None, None), 0x1f00);
    }

    fn singleton() -> Vec<WatchedBytes> {
        vec![WatchedBytes {
            addr: 0x2cb4a70,
            len: 8,
            name: Some("sInstance".to_owned()),
            source: WatchSource::SanitizerReport,
        }]
    }

    fn side<'a>(
        hits: &'a PassHits,
        schedule: &'a [SchedEvent],
        edges: &'a [HbEdge],
    ) -> SideView<'a> {
        SideView {
            hits,
            schedule,
            edges,
        }
    }

    fn concluded(pairs: Vec<MemoryPair>) -> WatchStage {
        let mut stage = WatchStage::unavailable(0, String::new());
        stage.pairs = pairs;
        stage.conclude();
        stage
    }

    fn pass(rows: Vec<TraceRecord>) -> PassHits {
        hits(&complete(rows), 1).unwrap()
    }

    /// Main (3) stores the singleton and thread 7 loads it; the target runs
    /// the store first, the baseline the load, and no edge orders them: the
    /// pair is found, and the report says its order flips.
    #[test]
    fn an_unordered_pair_whose_order_flips_is_found_first() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7]);
        let baseline_schedule = sched(&[3, 3, 7, 7, 3, 3]);
        let mut target_rows = store(2, 3, 0x1a1f).to_vec();
        target_rows.push(load(4, 7, 0x3910));
        let target = pass(target_rows);
        let mut baseline_rows = vec![load(3, 7, 0x3910)];
        baseline_rows.extend(store(5, 3, 0x1a1f));
        let baseline = pass(baseline_rows);
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&target, &schedule, &[]),
            &side(&baseline, &baseline_schedule, &[]),
        );
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0].accesses.each_ref().map(|a| (a.thread, a.kind)),
            [(tid(3), AccessKind::Store), (tid(7), AccessKind::Load)]
        );
        assert_eq!(pairs[0].baseline_order, Some([1, 0]));
        let stage = concluded(pairs);
        assert_eq!(stage.status, WatchStatus::Found);
        assert!(stage.reason.contains("order differs"), "{}", stage.reason);
        let text = render_watch_stage(&stage).join("\n");
        assert!(
            text.contains("unordered by the edges Hermit observes"),
            "{text}"
        );
        assert!(text.contains("Baseline: the opposite order"), "{text}");
        assert!(!text.contains("data race"), "{text}");
    }

    /// A pair as a sanitizer reports one: the store comes first in both runs,
    /// the load 4 events after it in the target and 2 in the baseline, and no
    /// edge orders them. The pair is found; the report gives both distances and
    /// says the order is the same, never that it decides.
    #[test]
    fn an_unordered_pair_in_the_same_order_is_found_with_both_distances() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7, 7]);
        let mut target_rows = store(2, 3, 0x1a1f).to_vec();
        target_rows.push(load(6, 7, 0x3910));
        let mut baseline_rows = store(2, 3, 0x1a1f).to_vec();
        baseline_rows.push(load(4, 7, 0x3910));
        let (target, baseline) = (pass(target_rows), pass(baseline_rows));
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&target, &schedule, &[]),
            &side(&baseline, &schedule, &[]),
        );
        assert_eq!(pairs[0].baseline_order, Some([0, 1]));
        let stage = concluded(pairs);
        assert_eq!(stage.status, WatchStatus::Found);
        assert!(
            stage.reason.contains("keep their order"),
            "{}",
            stage.reason
        );
        let text = render_watch_stage(&stage).join("\n");
        assert!(
            text.contains(
                "Baseline: the same order in both runs (store, then load); in the target the \
                 load comes 4 events after the store, in the baseline 2"
            ),
            "{text}"
        );
        assert!(!text.contains("opposite"), "{text}");
    }

    /// A pair an edge orders is never reported: main's next event after its
    /// store is 4, a futex wake from it reaches thread 7 at event 5, and the
    /// load comes after that. The stage counts it and says none found.
    #[test]
    fn a_pair_an_edge_orders_is_not_reported() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7]);
        let baseline_schedule = sched(&[3, 3, 7, 7, 3, 3]);
        let edge = HbEdge {
            from: EventRef {
                tid: tid(3),
                event: 4,
            },
            to: EventRef {
                tid: tid(7),
                event: 5,
            },
            kind: crate::analyze::races::HbEdgeKind::FutexWake,
        };
        let mut target_rows = store(2, 3, 0x1a1f).to_vec();
        target_rows.push(load(6, 7, 0x3910));
        let mut baseline_rows = vec![load(3, 7, 0x3910)];
        baseline_rows.extend(store(5, 3, 0x1a1f));
        let (target, baseline) = (pass(target_rows), pass(baseline_rows));
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&target, &schedule, &[edge]),
            &side(&baseline, &baseline_schedule, &[]),
        );
        assert!(pairs[0].target_hb.ordered, "{:?}", pairs[0]);
        let stage = concluded(pairs);
        assert_eq!(stage.status, WatchStatus::NoneFound);
        assert!(stage.pairs.is_empty());
        assert_eq!(stage.ordered_pairs, 1);
        let text = render_watch_stage(&stage).join("\n");
        assert!(
            text.contains("1 pair(s) that an edge Hermit observes orders are not listed"),
            "{text}"
        );
        assert!(!text.contains("Pair on"), "{text}");
    }

    /// The target's load by thread 7 has no match in the baseline. It is
    /// "not made" there only when the baseline kept every hit thread 7 had
    /// and the thread armed at the same event in both runs; past the hits
    /// the baseline kept, or counted from another arming point, it is not
    /// seen, and no order is claimed.
    #[test]
    fn an_access_the_baseline_does_not_show_is_absent_only_when_its_hits_are_complete() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7, 7]);
        let mut target_rows = store(2, 3, 0x1a1f).to_vec();
        target_rows.push(load(6, 7, 0x3910));
        let target = pass(target_rows);
        let baseline_with = |seven: TraceRecord| {
            let mut records = vec![syscall(3, 3, 0, false), syscall(7, 3, 0, false)];
            records.extend(store(2, 3, 0x1a1f));
            records.push(summary(3, 0, 2, 2));
            records.push(seven);
            hits(&trace(records), 1).unwrap()
        };
        let view = |baseline: &PassHits| {
            let (pairs, _) = memory_pairs(
                &singleton(),
                &side(&target, &schedule, &[]),
                &side(baseline, &schedule, &[]),
            );
            pairs[0].accesses[1].baseline.clone()
        };
        assert_eq!(
            view(&baseline_with(summary(7, 0, 0, 0))),
            BaselineAccess::Absent
        );
        let truncated = view(&baseline_with(summary(7, 0, 200, 0)));
        assert!(
            matches!(&truncated, BaselineAccess::Unobserved(why) if why.contains("past the first 0 hits")),
            "{truncated:?}"
        );
        let late = view(&baseline_with(summary(7, 4, 0, 0)));
        assert!(
            matches!(&late, BaselineAccess::Unobserved(why) if why.contains("another event")),
            "{late:?}"
        );
        let mut records = vec![syscall(3, 3, 0, false)];
        records.extend(store(2, 3, 0x1a1f));
        records.push(summary(3, 0, 2, 2));
        let missing = view(&hits(&trace(records), 1).unwrap());
        assert!(
            matches!(missing, BaselineAccess::Unobserved(_)),
            "{missing:?}"
        );

        let baseline = baseline_with(summary(7, 0, 200, 0));
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&target, &schedule, &[]),
            &side(&baseline, &schedule, &[]),
        );
        let stage = concluded(pairs);
        assert_eq!(stage.status, WatchStatus::Found);
        assert!(stage.reason.contains("does not show"), "{}", stage.reason);
        let text = render_watch_stage(&stage).join("\n");
        assert!(text.contains("no order there is claimed"), "{text}");
        assert!(!text.contains("does not happen"), "{text}");
    }

    /// After fork, a private global sits at the same address in two
    /// processes, and after execve one process's address is new memory:
    /// stores by threads of processes 3 and 4, or on either side of process
    /// 3's exec, are not paired; a thread that made no syscall is noted, not
    /// paired; two threads of one process between its execs are.
    #[test]
    fn accesses_pair_only_within_one_process_between_its_execs() {
        let schedule = sched(&[3; 20]);
        let pairs_of = |records: Vec<TraceRecord>| {
            let got = hits(&trace(records), 1).unwrap();
            let view = side(&got, &schedule, &[]);
            memory_pairs(&singleton(), &view, &view)
        };
        let mut forked = vec![syscall(3, 3, 0, false), syscall(4, 4, 1, false)];
        forked.extend(store(5, 3, 0x100));
        forked.extend(store(6, 4, 0x100));
        forked.extend([summary(3, 0, 2, 2), summary(4, 0, 2, 2)]);
        assert!(pairs_of(forked).0.is_empty(), "two processes");

        let mut exec = vec![
            syscall(3, 3, 0, false),
            syscall(5, 3, 1, false),
            syscall(5, 3, 7, true),
        ];
        exec.extend(store(5, 3, 0x100));
        exec.extend(store(9, 5, 0x100));
        exec.extend([summary(3, 0, 2, 2), summary(5, 0, 2, 2)]);
        assert!(pairs_of(exec).0.is_empty(), "an exec between them");

        let mut silent = vec![syscall(3, 3, 0, false)];
        silent.extend(store(5, 3, 0x100));
        silent.extend(store(6, 8, 0x100));
        silent.extend([summary(3, 0, 2, 2), summary(8, 0, 2, 2)]);
        let (pairs, notes) = pairs_of(silent);
        assert!(pairs.is_empty());
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("Thread 8's accesses are not paired")),
            "{notes:?}"
        );

        let mut threads = vec![syscall(3, 3, 0, false), syscall(5, 3, 1, false)];
        threads.extend(store(5, 3, 0x100));
        threads.extend(store(6, 5, 0x100));
        threads.extend([summary(3, 0, 2, 2), summary(5, 0, 2, 2)]);
        assert_eq!(pairs_of(threads).0.len(), 1, "one process");
    }

    /// Thread 3 stores and then loads, and thread 7 loads: the load of
    /// thread 7 pairs with thread 3's store, the last access of thread 3 that
    /// conflicts with it, though thread 3's load comes between.
    #[test]
    fn an_access_pairs_with_each_other_threads_last_conflicting_access() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7, 7]);
        let mut rows = store(2, 3, 0x1a1f).to_vec();
        rows.push(load(3, 3, 0x1a40));
        rows.push(load(6, 7, 0x3910));
        let got = pass(rows);
        let view = side(&got, &schedule, &[]);
        let (pairs, _) = memory_pairs(&singleton(), &view, &view);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(
            pairs[0]
                .accesses
                .each_ref()
                .map(|a| (a.thread, a.kind, a.ip)),
            [
                (tid(3), AccessKind::Store, 0x1a1f),
                (tid(7), AccessKind::Load, 0x3910)
            ]
        );
    }

    /// Thread 7's load lies in its first slice, before any event of its own;
    /// the clone that created it comes after thread 3's store, and the
    /// creation edge orders the two.
    #[test]
    fn the_creating_edge_orders_an_access_in_a_new_threads_first_slice() {
        let schedule = sched(&[3, 3, 3, 3, 7, 7]);
        let mut rows = store(1, 3, 0x1a1f).to_vec();
        rows.push(load(4, 7, 0x3910));
        let got = pass(rows);
        let create = HbEdge {
            from: EventRef {
                tid: tid(3),
                event: 3,
            },
            to: EventRef {
                tid: tid(7),
                event: 4,
            },
            kind: HbEdgeKind::Create,
        };
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&got, &schedule, &[create]),
            &side(&got, &schedule, &[]),
        );
        assert!(pairs[0].target_hb.ordered, "{:?}", pairs[0]);
        assert_eq!(pairs[0].target_hb.edges, vec![create]);
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&got, &schedule, &[]),
            &side(&got, &schedule, &[]),
        );
        assert!(!pairs[0].target_hb.ordered, "no edge, no order");
    }

    /// A pass that ran on its own from event 5 keeps only the hits before
    /// it; in the baseline, an access it no longer shows is not seen there,
    /// never absent.
    #[test]
    fn hits_after_a_pass_ran_on_its_own_are_dropped() {
        let schedule = sched(&[3, 3, 7, 7, 3, 7, 7]);
        let mut rows = store(2, 3, 0x1a1f).to_vec();
        rows.push(load(6, 7, 0x3910));
        let target = pass(rows.clone());
        let mut baseline = pass(store(2, 3, 0x1a1f).to_vec());
        baseline.coverage.insert(
            tid(7),
            Coverage {
                armed_before: Some(0),
                hits: 0,
                in_order: 0,
                deferred: 0,
            },
        );
        baseline.cut_at(5);
        let (pairs, _) = memory_pairs(
            &singleton(),
            &side(&target, &schedule, &[]),
            &side(&baseline, &schedule, &[]),
        );
        assert!(
            matches!(&pairs[0].accesses[1].baseline, BaselineAccess::Unobserved(why) if why.contains("ran on its own from event 5")),
            "{:?}",
            pairs[0]
        );
        let mut cut = pass(rows);
        cut.cut_at(5);
        assert_eq!(cut.hits.len(), 1, "the load after event 5 is dropped");
        // A hit stamped with the first differing event lies in the branch
        // batch that event summarizes: it is dropped too.
        let mut at_the_event = pass(vec![load(4, 7, 0x10), load(5, 3, 0x20), load(6, 7, 0x30)]);
        at_the_event.cut_at(5);
        assert_eq!(
            kinds(&at_the_event),
            vec![(7, AccessKind::Load, 0x10)],
            "only hits before the differing event's batch"
        );
    }

    fn exec_at(t: u32) -> SchedEvent {
        SchedEvent::syscall(tid(t), Sysno::execve, SyscallPhase::Prehook)
    }

    /// Two runs' same-looking accesses after an exec are never claimed to be
    /// one: not when only the target exec'd, not when both exec'd once
    /// (possibly into different programs), and not when the exec came before
    /// the window, so the trace does not show it but the schedule does. The
    /// first exec of a run is the program's launch and changes nothing.
    #[test]
    fn accesses_in_another_image_never_correspond() {
        let rows = |exec_in_trace: Option<EventIndex>| {
            let mut records = vec![syscall(3, 3, 0, false), syscall(5, 3, 7, false)];
            if let Some(at) = exec_in_trace {
                records.push(syscall(3, 3, at, true));
            }
            records.extend(store(8, 3, 0x1a1f));
            records.push(load(9, 5, 0x3910));
            records.extend([summary(3, 0, 2, 2), summary(5, 0, 1, 1)]);
            hits(&trace(records), 1).unwrap()
        };
        let launched = |exec: Option<usize>| {
            let mut schedule = sched(&[3; 12]);
            schedule[1] = exec_at(3);
            if let Some(at) = exec {
                schedule[at] = exec_at(3);
            }
            schedule
        };
        let views =
            |target: &PassHits, ts: &[SchedEvent], baseline: &PassHits, bs: &[SchedEvent]| {
                let (pairs, _) = memory_pairs(
                    &singleton(),
                    &side(target, ts, &[]),
                    &side(baseline, bs, &[]),
                );
                assert_eq!(pairs.len(), 1);
                pairs.into_iter().next().unwrap()
            };
        let no_claim = |pair: &MemoryPair| {
            pair.baseline_order.is_none()
                && pair
                    .accesses
                    .iter()
                    .all(|a| matches!(&a.baseline, BaselineAccess::Unobserved(why) if why.contains("exec")))
        };

        // Only the program's launch: the same access in both runs.
        let plain = views(&rows(None), &launched(None), &rows(None), &launched(None));
        assert_eq!(plain.baseline_order, Some([0, 1]), "{plain:?}");
        assert!(plain.accesses.iter().all(|a| !a.other_image));

        // A window from event 0: the trace holds the launch too, and the
        // accesses are still the program's, in both runs.
        let whole = views(
            &rows(Some(1)),
            &launched(None),
            &rows(Some(1)),
            &launched(None),
        );
        assert_eq!(whole.baseline_order, Some([0, 1]), "{whole:?}");
        assert!(whole.accesses.iter().all(|a| !a.other_image));
        assert!(
            whole
                .accesses
                .iter()
                .all(|a| matches!(a.baseline, BaselineAccess::At(_))),
            "{whole:?}"
        );

        // The target exec'd in the window, the baseline did not.
        let one = views(
            &rows(Some(5)),
            &launched(Some(5)),
            &rows(None),
            &launched(None),
        );
        assert!(no_claim(&one), "{one:?}");
        assert!(one.accesses.iter().all(|a| a.other_image));

        // Both exec'd once, possibly into different programs.
        let both = views(
            &rows(Some(5)),
            &launched(Some(5)),
            &rows(Some(5)),
            &launched(Some(5)),
        );
        assert!(no_claim(&both), "{both:?}");

        // The exec came before the window: the trace has no record of it.
        let early = views(
            &rows(None),
            &launched(Some(3)),
            &rows(None),
            &launched(Some(3)),
        );
        assert!(no_claim(&early), "{early:?}");
        assert!(early.accesses.iter().all(|a| a.other_image));
        assert_eq!(execs_before(&launched(Some(3)), 3), 1);
        assert_eq!(execs_before(&launched(Some(3)), 4), 2);
    }

    /// Frames of an access that may have run in another image keep their
    /// addresses: the program's symbols may be another image's.
    #[test]
    fn frames_after_an_exec_are_not_named_from_the_program() {
        let frame = |ip| StackFrame {
            ip,
            function: None,
            file: None,
            line: None,
        };
        let access = |other_image| MemoryAccess {
            thread: tid(3),
            kind: AccessKind::Store,
            ip: 0x1000,
            after: 4,
            other_image,
            baseline: BaselineAccess::Absent,
            stack: vec![frame(0x1000)],
        };
        let mut stage = WatchStage::unavailable(0, String::new());
        stage.pairs.push(MemoryPair {
            bytes: singleton().remove(0),
            accesses: [access(false), access(true)],
            baseline_order: None,
            target_hb: HbVerdict {
                ordered: false,
                edges: Vec::new(),
            },
            baseline_hb: None,
        });
        let note = apply_frame_names(&mut stage, |_| Some("main+0x10".to_owned()));
        let [old, new] = &stage.pairs[0].accesses;
        assert_eq!(old.stack[0].function.as_deref(), Some("main+0x10"));
        assert_eq!(new.stack[0].function, None);
        assert!(note.unwrap().contains("after an exec"));
    }

    /// A summary that disagrees with the rows the trace holds refuses the
    /// pass rather than guessing which rows are in order.
    #[test]
    fn a_summary_that_disagrees_with_the_rows_refuses_the_pass() {
        let mut records = vec![syscall(3, 3, 0, false)];
        records.extend(store(2, 3, 0x1a1f));
        records.push(summary(3, 0, 3, 3));
        let error = hits(&trace(records), 1).unwrap_err();
        assert!(error.contains("says 3 of its hits"), "{error}");
    }

    /// Only the futex word reads as the lock's own operations; the bytes
    /// beside it are next to the lock, which a guarded flag can be.
    #[test]
    fn only_the_futex_word_is_labelled_the_lock() {
        let pair_on = |source| {
            let mut stage = WatchStage::unavailable(0, String::new());
            let schedule = sched(&[3, 3, 7, 7, 3, 7, 7]);
            let mut rows = store(2, 3, 0x1a1f).to_vec();
            rows.push(load(6, 7, 0x3910));
            let got = pass(rows);
            let view = side(&got, &schedule, &[]);
            let bytes = vec![WatchedBytes {
                addr: 0x1000,
                len: 8,
                name: None,
                source,
            }];
            stage.pairs = memory_pairs(&bytes, &view, &view).0;
            stage.conclude();
            render_watch_stage(&stage).join("\n")
        };
        let word = pair_on(WatchSource::FutexWord);
        assert!(word.contains("its own operations"), "{word}");
        let near = pair_on(WatchSource::NearFutexWord);
        assert!(!near.contains("its own operations"), "{near}");
        assert!(near.contains("lie next to the futex word"), "{near}");
    }

    /// "None found" says it holds among the threads that armed, and that
    /// some accesses were not seen when a thread armed late or a pass was
    /// left out, listing both.
    #[test]
    fn none_found_says_what_was_not_seen() {
        let mut stage = WatchStage::unavailable(10, String::new());
        stage.conclude();
        assert_eq!(stage.status, WatchStatus::NoneFound);
        assert!(
            stage
                .reason
                .ends_with("among the threads that armed their watches"),
            "{}",
            stage.reason
        );
        let mut partial = stage.clone();
        partial.partial = true;
        partial.conclude();
        assert!(partial.reason.contains("not seen"), "{}", partial.reason);
        stage.late.push((tid(7), 40));
        stage.dropped.push(singleton().remove(0));
        stage.conclude();
        assert!(stage.reason.contains("not seen"), "{}", stage.reason);
        let text = render_watch_stage(&stage).join("\n");
        assert!(
            text.contains("Thread 7 armed its watches only before event 40"),
            "{text}"
        );
        assert!(
            text.contains("Not watched (its pass was left out)"),
            "{text}"
        );
    }

    /// One store against three loads of another thread is one pair, the
    /// first; the rest are counted, not paired again.
    #[test]
    fn a_store_pairs_once_with_each_other_thread() {
        let schedule = sched(&[3, 3, 7, 7, 7, 7, 7]);
        let mut rows = store(2, 3, 0x1a1f).to_vec();
        rows.extend([load(4, 7, 0x3910), load(5, 7, 0x3910), load(6, 7, 0x3910)]);
        let got = pass(rows);
        let view = side(&got, &schedule, &[]);
        let (pairs, notes) = memory_pairs(&singleton(), &view, &view);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(pairs[0].accesses[1].after, 4);
        assert!(
            notes.iter().any(|n| n.starts_with("2 later access(es)")),
            "{notes:?}"
        );
    }

    /// A SIGSEGV at a null or near-null address names no object to watch;
    /// one at a real address does.
    #[test]
    fn a_null_fault_address_is_not_watched() {
        let fault = |addr| {
            TraceRecord::Signal(detcore_model::access_trace::SignalRecord {
                event: 10,
                tid: tid(5),
                signo: libc::SIGSEGV,
                code: 1,
                addr: Some(addr),
                sender: None,
                delivery: detcore_model::access_trace::SignalDelivery::Fatal,
            })
        };
        assert!(trace_candidates(&trace(vec![fault(0)]), 10).is_empty());
        assert!(trace_candidates(&trace(vec![fault(0x10)]), 10).is_empty());
        let real = trace_candidates(&trace(vec![fault(0x2cb4a70)]), 10);
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].addr, 0x2cb4a70);
    }
}
