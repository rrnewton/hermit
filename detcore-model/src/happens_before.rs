/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Happens-before edges: a sparse, authored partial order over dynamic events.
//!
//! Where `--replay-schedule-from` replays a *complete* total order captured from
//! a prior run, a happens-before specification pins down only the *few* events
//! that matter for a race and lets the deterministic scheduler fill in the rest.
//! An agent (or human) that already knows a target race can therefore construct
//! it deterministically instead of blind seed-search.
//!
//! # Model
//!
//! An [`Anchor`] names a precise, deterministic per-thread stop point. Following
//! the owner's refinement of RFC #1146, the *primary* addressing is a
//! [`Position`] — "after N syscalls" or "after M retired conditional branches
//! (RCBs)" on a specific thread — optionally decorated with a [`CodeLocation`]
//! (function and/or source line, resolved from debug info) for readability. The
//! RFC's richer addressing modes (Nth occurrence of a named syscall, RIP hit, or
//! function entry) remain expressible but are deliberately *not* the lead
//! addressing scheme.
//!
//! A [`HappensBeforeEdge`] states that one anchor must be observed before another
//! thread is allowed to proceed past its anchor. A [`HappensBeforeSpec`] is the
//! whole authored partial order: a table of named threads, a table of named
//! events (anchors), and the edge list connecting them.
//!
//! This module owns the *model* only: parsing (JSON and a terse DSL),
//! normalization, and static validation (name resolution, exactly-one-position,
//! and cycle detection). Resolving a [`CodeLocation`] to a concrete address via
//! debug info, and enforcing edges in the scheduler, live in higher layers.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;
use crate::schedule::SyscallPhase;

/// The schema version understood by this build.
pub const HAPPENS_BEFORE_VERSION: u32 = 1;

// ================================================================================
// Declarative on-disk / on-wire format (serde)
// ================================================================================

/// The declarative happens-before specification, as read from a JSON file or
/// desugared from the terse DSL. This mirrors the reviewed RFC #1146 file format
/// verbatim so an authored file round-trips.
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub struct HappensBeforeSpec {
    /// Schema version. Must equal [`HAPPENS_BEFORE_VERSION`].
    pub version: u32,

    /// Symbolic thread labels mapped to a resolution rule, so authors need not
    /// hard-code raw `DetTid`s.
    #[serde(default)]
    pub threads: BTreeMap<String, ThreadSpec>,

    /// Named events (anchors). Naming events separately from edges lets one event
    /// participate in several edges and keeps the edge list readable.
    #[serde(default)]
    pub events: BTreeMap<String, EventSpec>,

    /// The partial order itself.
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
}

/// How a symbolic thread label resolves to a concrete `DetTid`.
///
/// Exactly one resolution rule should be provided; if `dettid` is present it wins.
#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSpec {
    /// A human-facing label (defaults to the map key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// An explicit deterministic thread id, when the author knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dettid: Option<i32>,

    /// Resolve to the thread created by the Nth `clone`/`fork`, 1-based. Thread
    /// creation is deterministic under sequentialization, so this is stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_ordinal: Option<u32>,
}

/// A single named event (anchor) in the declarative format.
///
/// The addressing fields are flat and optional to match the RFC JSON; exactly one
/// *position* selector must be set (see [`HappensBeforeSpec::normalize`]). The
/// two owner-preferred primaries are [`syscalls`](Self::syscalls) ("after N
/// syscalls") and [`rcbs`](Self::rcbs) ("after M RBCs").
#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventSpec {
    /// The thread this event is on: a key into [`HappensBeforeSpec::threads`], or
    /// a raw integer `DetTid`.
    pub thread: String,

    // ---- primary positions (owner's refinement) ----
    /// After the thread has executed this many syscalls in total (any syscall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscalls: Option<u64>,

    /// After the thread has retired this many conditional branches (its RCB
    /// clock reaches this absolute value). Accepted as `rcbs` or `rcb`.
    #[serde(default, alias = "rcb", skip_serializing_if = "Option::is_none")]
    pub rcbs: Option<u64>,

    // ---- code location (readability; resolves to a RIP via debug info) ----
    /// A function name; with `line` this is "function+line", the preferred
    /// human-legible location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,

    /// A source file name (optional companion to `line`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,

    /// A source line number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,

    // ---- RFC richer addressing (expressible, not led with) ----
    /// A specific syscall by name (e.g. `"futex"`), the Nth occurrence of which
    /// is the anchor. Combine with `phase` and `nth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscall: Option<String>,

    /// Which phase of the named `syscall` to anchor on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseSpec>,

    /// Restrict the named `syscall` to calls whose first argument is this file
    /// descriptor, for example `{"syscall": "writev", "fd": 9, "nth": 3}`: the
    /// thread's third `writev` to fd 9. The descriptor is matched by number at
    /// the time of the call: a write to a `dup` of fd 9 under another number
    /// does not count, and after fd 9 is closed and the number reused, calls
    /// on the new fd 9 do. Only valid with `syscall`, and only for
    /// a syscall whose first argument is a file descriptor
    /// ([`syscall_takes_fd_first`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fd: Option<i32>,

    /// A raw instruction pointer, as a hex string like `"0x401f3c"` or decimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rip: Option<String>,

    /// A cooperative marker name (guest-emitted). Reserved for a future backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,

    /// Which occurrence of the addressed point (1-based). Defaults to 1. Only
    /// meaningful for the occurrence-counted modes (`syscall`, `func`, `rip`,
    /// `mark`); the absolute-count primaries (`syscalls`, `rcbs`) ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nth: Option<u64>,
}

/// Serializable mirror of [`SyscallPhase`] using lowercase author-friendly names.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseSpec {
    /// Before the syscall instruction executes.
    #[serde(alias = "pre")]
    Prehook,
    /// A nonblocking poll retry.
    Polling,
    /// After the syscall logically completes.
    #[serde(alias = "post")]
    Posthook,
}

impl From<PhaseSpec> for SyscallPhase {
    fn from(p: PhaseSpec) -> Self {
        match p {
            PhaseSpec::Prehook => SyscallPhase::Prehook,
            PhaseSpec::Polling => SyscallPhase::Polling,
            PhaseSpec::Posthook => SyscallPhase::Posthook,
        }
    }
}

/// One ordering constraint: `before` happens-before `after`.
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSpec {
    /// The name of the source event (must be observed first).
    pub before: String,
    /// The name of the sink event (blocked until the source fires).
    pub after: String,
    /// Enforcement strength; defaults to [`Strength::Hard`].
    #[serde(default)]
    pub strength: Strength,
}

/// How strictly an edge is enforced by the scheduler.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    /// Park the sink thread in a true gate until the source fires. The guarantee
    /// wanted for constructed repros, and the default.
    #[default]
    Hard,
    /// Merely bias scheduling (priority nudge); the sink may still run if it is
    /// the only runnable thread.
    Soft,
}

// ================================================================================
// Normalized model
// ================================================================================

/// A resolved reference to a thread.
#[derive(PartialEq, Eq, Debug, Clone, PartialOrd, Ord)]
pub struct ThreadRef {
    /// The symbolic label (map key, or the raw id as text).
    pub label: String,
    /// The concrete `DetTid`, when statically known.
    pub dettid: Option<DetTid>,
    /// Resolve to the Nth spawned thread, when that is the rule.
    pub spawn_ordinal: Option<u32>,
}

/// A source-level location, resolvable to/from an address via debug info.
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub struct CodeLocation {
    /// Function name.
    pub function: Option<String>,
    /// Source file name.
    pub file: Option<String>,
    /// Source line number.
    pub line: Option<u32>,
}

impl CodeLocation {
    /// True when this location carries no information.
    pub fn is_empty(&self) -> bool {
        self.function.is_none() && self.file.is_none() && self.line.is_none()
    }
}

impl fmt::Display for CodeLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.function, &self.file, self.line) {
            (Some(func), _, Some(line)) => write!(f, "{}:{}", func, line),
            (Some(func), _, None) => write!(f, "{}", func),
            (None, Some(file), Some(line)) => write!(f, "{}:{}", file, line),
            (None, Some(file), None) => write!(f, "{}", file),
            (None, None, Some(line)) => write!(f, "line {}", line),
            (None, None, None) => write!(f, "<unlocated>"),
        }
    }
}

/// The deterministic per-thread stop point that an anchor addresses.
///
/// All variants reduce to "a predicate over this thread's deterministic event
/// stream plus an occurrence count," but the two leading variants
/// ([`Position::SyscallCount`] and [`Position::Rcb`]) are absolute counts that
/// need no per-anchor occurrence tracking.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum Position {
    /// After the thread has executed exactly this many syscalls (any syscall).
    SyscallCount(u64),

    /// When the thread's RCB clock reaches this absolute value.
    Rcb(u64),

    /// The `nth` occurrence of a specific syscall on the thread, optionally
    /// phase-qualified and restricted to one file descriptor. The occurrence is
    /// counted per thread, over that thread's calls that match `sysno` (and
    /// `fd`), so an anchor needs no calibration run to find a syscall count.
    ///
    /// Two properties to know when writing one:
    /// - What is counted is syscall *entries*, the same unit as
    ///   [`Position::SyscallCount`]. A matching call that a signal interrupts
    ///   and the kernel restarts is entered, and counted, again.
    /// - The counters belong to the thread, like its syscall count: a new
    ///   thread starts at zero, an exec keeps them, and after a non-leader exec
    ///   the calling thread's counters continue under the leader's thread id.
    /// - Like every anchor, it fires at the prehook: an edge orders the BEFORE
    ///   thread *reaching* its syscall, not that syscall's effect. Under the
    ///   default scheduler the BEFORE syscall completes before the held thread
    ///   is re-admitted (equal priorities run in queue order); a run that
    ///   reorders runnable threads (chaos) or a BEFORE syscall that takes
    ///   several scheduler turns can let the AFTER thread's effect land first.
    Syscall {
        /// The syscall number.
        sysno: Sysno,
        /// Restrict to a phase, or match any phase when `None`.
        phase: Option<SyscallPhase>,
        /// 1-based occurrence.
        nth: u64,
        /// Restrict to calls whose first argument is this file descriptor.
        fd: Option<i32>,
    },

    /// The `nth` execution of the instruction at an absolute address. The address
    /// is resolved later when it comes from a [`CodeLocation`].
    Rip {
        /// Absolute instruction pointer, or `None` until resolved from a
        /// [`CodeLocation`].
        addr: Option<u64>,
        /// 1-based occurrence.
        nth: u64,
    },

    /// A cooperative guest marker. Reserved for a future backend.
    Marker {
        /// Marker name.
        name: String,
        /// 1-based occurrence.
        nth: u64,
    },
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Position::SyscallCount(n) => write!(f, "after {} syscalls", n),
            Position::Rcb(m) => write!(f, "at RCB {}", m),
            Position::Syscall {
                sysno,
                phase,
                nth,
                fd,
            } => {
                write!(f, "{}", sysno.name())?;
                if let Some(fd) = fd {
                    write!(f, "(fd={})", fd)?;
                }
                if let Some(p) = phase {
                    write!(f, "@{:?}", p)?;
                }
                write!(f, "#{}", nth)
            }
            Position::Rip { addr, nth } => match addr {
                Some(a) => write!(f, "@{:#x}#{}", a, nth),
                None => write!(f, "@<unresolved>#{}", nth),
            },
            Position::Marker { name, nth } => write!(f, "mark:{}#{}", name, nth),
        }
    }
}

/// A fully normalized anchor: a named, deterministic per-thread stop point.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct Anchor {
    /// The event name (map key), for diagnostics and edge references.
    pub name: String,
    /// The thread this anchor is on.
    pub thread: ThreadRef,
    /// The deterministic position selector.
    pub position: Position,
    /// Optional human-legible / debug-info-resolved code location.
    pub location: CodeLocation,
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}: {}", self.name, self.thread.label, self.position)?;
        if !self.location.is_empty() {
            write!(f, " ({})", self.location)?;
        }
        write!(f, "]")
    }
}

impl Anchor {
    /// True for an enforced syscall-occurrence anchor: a [`Position::Syscall`]
    /// with no phase or the prehook phase. The anchor fires at the prehook of
    /// the thread's `nth` matching syscall, the point where every enforced
    /// anchor fires. Other phases are not enforced.
    pub fn is_syscall_occurrence(&self) -> bool {
        matches!(
            self.position,
            Position::Syscall {
                phase: None | Some(SyscallPhase::Prehook),
                ..
            }
        )
    }

    /// The per-thread counter this anchor's occurrences are counted in: its
    /// syscall number and fd, or `None` for any other kind of anchor.
    pub fn occurrence_key(&self) -> Option<OccurrenceKey> {
        match &self.position {
            Position::Syscall { sysno, fd, .. } if self.is_syscall_occurrence() => {
                Some((*sysno as usize, *fd))
            }
            _ => None,
        }
    }

    /// True when this is an enforced syscall-occurrence anchor and a call to
    /// `sysno` with first argument `arg0` is one of the calls it counts.
    pub fn syscall_occurrence_matches(&self, sysno: Sysno, arg0: usize) -> bool {
        match &self.position {
            Position::Syscall {
                sysno: want, fd, ..
            } if self.is_syscall_occurrence() && *want == sysno => {
                // A file descriptor is an `int`: the kernel reads the low 32
                // bits of the register.
                fd.is_none_or(|fd| arg0 as u32 as i32 == fd)
            }
            _ => false,
        }
    }
}

/// True for a syscall that never reaches Hermit's tracer on the ptrace
/// backend, whatever Detcore subscribes to: Reverie lets `rt_sigreturn` run
/// untraced from its private page so a signal frame is restored safely. A
/// syscall anchor on one could never fire, so the run refuses it.
pub fn syscall_never_reaches_tracer(sysno: Sysno) -> bool {
    sysno == Sysno::rt_sigreturn
}

/// The key of one per-thread occurrence counter: a syscall number and an
/// optional file descriptor ([`Anchor::occurrence_key`]).
pub type OccurrenceKey = (usize, Option<i32>);

/// One thread's syscall-occurrence counters
/// ([`HappensBeforeProgram::count_syscall_occurrences`]).
pub type OccurrenceCounters = BTreeMap<OccurrenceKey, u64>;

/// True when `sysno`'s first argument is a file descriptor, so an anchor may
/// restrict it with `fd`. The list is the fd-first syscalls a guest is likely to
/// anchor on; an `fd` on any other syscall is refused when the spec is loaded
/// rather than matched against an argument that is not a descriptor.
pub fn syscall_takes_fd_first(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::read
            | Sysno::write
            | Sysno::readv
            | Sysno::writev
            | Sysno::pread64
            | Sysno::pwrite64
            | Sysno::preadv
            | Sysno::pwritev
            | Sysno::preadv2
            | Sysno::pwritev2
            | Sysno::close
            | Sysno::fsync
            | Sysno::fdatasync
            | Sysno::ioctl
            | Sysno::fcntl
            | Sysno::lseek
            | Sysno::fstat
            | Sysno::ftruncate
            | Sysno::fallocate
            | Sysno::flock
            | Sysno::getdents64
            | Sysno::sendto
            | Sysno::recvfrom
            | Sysno::sendmsg
            | Sysno::recvmsg
            | Sysno::sendmmsg
            | Sysno::recvmmsg
            | Sysno::accept
            | Sysno::accept4
            | Sysno::connect
            | Sysno::bind
            | Sysno::listen
            | Sysno::shutdown
            | Sysno::epoll_wait
            | Sysno::epoll_pwait
            | Sysno::epoll_ctl
            | Sysno::dup
            | Sysno::dup2
            | Sysno::dup3
    )
}

/// A normalized happens-before edge between two anchors.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct HappensBeforeEdge {
    /// The source anchor name (observed first).
    pub before: String,
    /// The sink anchor name (gated until the source fires).
    pub after: String,
    /// Enforcement strength.
    pub strength: Strength,
}

/// A validated, normalized happens-before program: anchors indexed by name plus
/// the edge list, guaranteed acyclic with all references resolved.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct HappensBeforeProgram {
    /// Normalized anchors, keyed by event name.
    pub anchors: BTreeMap<String, Anchor>,
    /// The validated, acyclic edge list.
    pub edges: Vec<HappensBeforeEdge>,
}

impl HappensBeforeProgram {
    /// Anchors that still require debug-info resolution (an unresolved RIP from a
    /// code location, i.e. a `func`/`line` that has not been turned into an
    /// address yet).
    pub fn unresolved_locations(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.values().filter(|a| {
            matches!(a.position, Position::Rip { addr: None, .. }) && !a.location.is_empty()
        })
    }

    /// True when thread `dettid` must check in with the scheduler at its
    /// syscall number `count`: some anchor sits at exactly
    /// [`Position::SyscallCount`]`(count)` and names `dettid`, or names its
    /// thread by spawn ordinal (which only the scheduler can resolve, so the
    /// guest side must ask). For any other (thread, count) the scheduler's
    /// checkpoint finds no anchor and does nothing, so skipping it changes no
    /// gate. It costs one scheduler round trip per anchor its thread reaches,
    /// not one per syscall (<https://github.com/rrnewton/hermit/issues/3877>),
    /// and threads a spec does not name keep their schedule: a count another
    /// thread passes early (QEMU's main loop makes about a million syscalls a
    /// run) no longer adds a scheduler turn there.
    pub fn may_have_syscall_count_anchor_at(&self, dettid: DetTid, count: u64) -> bool {
        self.anchors.values().any(|a| {
            matches!(a.position, Position::SyscallCount(n) if n == count)
                && a.thread.dettid.is_none_or(|d| d == dettid)
        })
    }

    /// True when the program has any enforced syscall-occurrence anchor
    /// ([`Anchor::is_syscall_occurrence`]), so the guest must count its calls.
    pub fn has_syscall_occurrence_anchors(&self) -> bool {
        self.anchors.values().any(Anchor::is_syscall_occurrence)
    }

    /// Count one syscall entry of thread `dettid` (`sysno`, first argument
    /// `arg0`) in that thread's `counters`, and return the names of the
    /// syscall-occurrence anchors this entry is the `nth` occurrence of.
    ///
    /// `counters` belongs to one thread and is kept with its other per-thread
    /// counts (the syscall count that count anchors use), so it starts at zero
    /// for every new thread. It counts, per distinct (syscall, fd) an anchor
    /// names, every entry of that thread that matches; an entry that matches
    /// both an fd-specific and an fd-agnostic anchor counts toward each. An
    /// entry is counted exactly once, when the thread enters the syscall, so a
    /// thread held at an anchor and re-admitted does not count again. The
    /// result names anchors on `dettid` or on a spawn ordinal; the scheduler
    /// resolves spawn ordinals and fires only the anchors on this thread.
    pub fn count_syscall_occurrences(
        &self,
        counters: &mut OccurrenceCounters,
        dettid: DetTid,
        sysno: Sysno,
        arg0: usize,
    ) -> Vec<String> {
        let keys: BTreeSet<OccurrenceKey> = self
            .anchors
            .values()
            .filter(|a| a.syscall_occurrence_matches(sysno, arg0))
            .filter_map(Anchor::occurrence_key)
            .collect();
        if keys.is_empty() {
            return Vec::new();
        }
        for key in &keys {
            *counters.entry(*key).or_insert(0) += 1;
        }
        self.anchors
            .values()
            .filter(|a| {
                a.syscall_occurrence_matches(sysno, arg0)
                    && a.thread.dettid.is_none_or(|d| d == dettid)
            })
            .filter(|a| match (&a.position, a.occurrence_key()) {
                (Position::Syscall { nth, .. }, Some(key)) => counters.get(&key) == Some(nth),
                _ => false,
            })
            .map(|a| a.name.clone())
            .collect()
    }

    /// Anchors whose position kind the current scheduler does not yet enforce:
    /// everything other than [`Position::SyscallCount`] and the syscall
    /// occurrence anchors ([`Anchor::is_syscall_occurrence`]). Reported so a run
    /// does not silently ignore an authored ordering constraint it cannot honor.
    pub fn unenforced_positions(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.values().filter(|a| {
            !matches!(a.position, Position::SyscallCount(_)) && !a.is_syscall_occurrence()
        })
    }
}

// ================================================================================
// Errors
// ================================================================================

/// An error produced while parsing or validating a happens-before specification.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HappensBeforeError {
    /// The schema `version` is not understood by this build.
    UnsupportedVersion(u32),
    /// An event named more than one position selector, or none.
    AmbiguousPosition {
        /// The offending event name.
        event: String,
        /// The selectors that were set.
        found: Vec<String>,
    },
    /// A syscall name could not be parsed.
    UnknownSyscall {
        /// The offending event name.
        event: String,
        /// The unparseable name.
        name: String,
    },
    /// An occurrence-counted event (`syscall`, `rip`, `mark` or a code
    /// location) set `nth` to 0; occurrences are 1-based.
    NthZero {
        /// The offending event name.
        event: String,
    },
    /// An event set `fd` without `syscall`.
    FdWithoutSyscall {
        /// The offending event name.
        event: String,
    },
    /// An event set `fd` on a syscall whose first argument is not a file
    /// descriptor ([`syscall_takes_fd_first`]).
    FdNotFirstArgument {
        /// The offending event name.
        event: String,
        /// The syscall name.
        syscall: String,
    },
    /// A RIP string could not be parsed as an address.
    BadRip {
        /// The offending event name.
        event: String,
        /// The unparseable text.
        text: String,
    },
    /// An edge referenced an event that does not exist.
    UnknownEvent {
        /// `before` or `after`.
        which: String,
        /// The dangling name.
        name: String,
    },
    /// An event referenced a thread label that is not in the `threads` table and
    /// is not a raw integer id.
    UnknownThread {
        /// The offending event name.
        event: String,
        /// The dangling thread label.
        thread: String,
    },
    /// The edge graph contains a cycle (listed in discovery order).
    Cycle(Vec<String>),
    /// A DSL line could not be parsed.
    DslSyntax {
        /// 1-based line number.
        line: usize,
        /// What went wrong.
        message: String,
    },
}

impl fmt::Display for HappensBeforeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HappensBeforeError::UnsupportedVersion(v) => write!(
                f,
                "unsupported happens-before schema version {} (this build understands {})",
                v, HAPPENS_BEFORE_VERSION
            ),
            HappensBeforeError::AmbiguousPosition { event, found } => {
                if found.is_empty() {
                    write!(
                        f,
                        "event '{}' must specify a position: a count (syscalls/rcbs), a syscall, a \
                         rip, a mark, or a code location (func/file/line)",
                        event
                    )
                } else {
                    write!(
                        f,
                        "event '{}' names conflicting positions {:?}; use at most one explicit \
                         position selector (a code location may accompany it)",
                        event, found
                    )
                }
            }
            HappensBeforeError::UnknownSyscall { event, name } => {
                write!(f, "event '{}' names unknown syscall '{}'", event, name)
            }
            HappensBeforeError::NthZero { event } => write!(
                f,
                "event '{}' sets 'nth' to 0; occurrences are counted from 1",
                event
            ),
            HappensBeforeError::FdWithoutSyscall { event } => write!(
                f,
                "event '{}' sets 'fd' without 'syscall'; 'fd' restricts a named syscall",
                event
            ),
            HappensBeforeError::FdNotFirstArgument { event, syscall } => write!(
                f,
                "event '{}' sets 'fd' on syscall '{}', whose first argument is not a file \
                 descriptor",
                event, syscall
            ),
            HappensBeforeError::BadRip { event, text } => {
                write!(f, "event '{}' has unparseable rip '{}'", event, text)
            }
            HappensBeforeError::UnknownEvent { which, name } => {
                write!(f, "edge '{}' references unknown event '{}'", which, name)
            }
            HappensBeforeError::UnknownThread { event, thread } => write!(
                f,
                "event '{}' references unknown thread '{}'",
                event, thread
            ),
            HappensBeforeError::Cycle(names) => {
                write!(
                    f,
                    "happens-before edges contain a cycle: {}",
                    names.join(" -> ")
                )
            }
            HappensBeforeError::DslSyntax { line, message } => {
                write!(f, "DSL parse error on line {}: {}", line, message)
            }
        }
    }
}

impl std::error::Error for HappensBeforeError {}

// ================================================================================
// Parsing & normalization
// ================================================================================

impl HappensBeforeSpec {
    /// Parse a JSON specification.
    pub fn from_json(s: &str) -> anyhow::Result<HappensBeforeSpec> {
        Ok(serde_json::from_str(s)?)
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Normalize and statically validate: check the version, resolve thread and
    /// event references, require exactly one position per event, parse syscalls
    /// and RIPs, and confirm the edge graph is acyclic.
    pub fn normalize(&self) -> Result<HappensBeforeProgram, HappensBeforeError> {
        if self.version != HAPPENS_BEFORE_VERSION {
            return Err(HappensBeforeError::UnsupportedVersion(self.version));
        }

        let mut anchors = BTreeMap::new();
        for (name, ev) in &self.events {
            anchors.insert(name.clone(), self.normalize_event(name, ev)?);
        }

        // Resolve edges against the anchor table.
        let mut edges = Vec::with_capacity(self.edges.len());
        for e in &self.edges {
            if !anchors.contains_key(&e.before) {
                return Err(HappensBeforeError::UnknownEvent {
                    which: "before".to_string(),
                    name: e.before.clone(),
                });
            }
            if !anchors.contains_key(&e.after) {
                return Err(HappensBeforeError::UnknownEvent {
                    which: "after".to_string(),
                    name: e.after.clone(),
                });
            }
            edges.push(HappensBeforeEdge {
                before: e.before.clone(),
                after: e.after.clone(),
                strength: e.strength,
            });
        }

        detect_cycle(&anchors, &edges)?;

        Ok(HappensBeforeProgram { anchors, edges })
    }

    /// Resolve one event into a normalized [`Anchor`].
    fn normalize_event(&self, name: &str, ev: &EventSpec) -> Result<Anchor, HappensBeforeError> {
        let thread = self.resolve_thread(name, &ev.thread)?;

        // A code location can accompany any position; it also *supplies* a RIP
        // position when no explicit position selector is present.
        let location = CodeLocation {
            function: ev.func.clone(),
            file: ev.file.clone(),
            line: ev.line,
        };

        // Determine which *explicit* position selectors are present. A code
        // location (`func`/`file`/`line`) is descriptive and may accompany any
        // one of these — the owner's primary anchor is "function foo on thread T
        // after N syscalls / M RBCs", i.e. a code location *and* a count. The
        // code location only *becomes* the (deferred RIP) position when no
        // explicit selector is present at all.
        let mut found: Vec<&str> = Vec::new();
        if ev.syscalls.is_some() {
            found.push("syscalls");
        }
        if ev.rcbs.is_some() {
            found.push("rcbs");
        }
        if ev.syscall.is_some() {
            found.push("syscall");
        }
        if ev.rip.is_some() {
            found.push("rip");
        }
        if ev.mark.is_some() {
            found.push("mark");
        }
        let has_code_location = !location.is_empty();

        // Reject multiple explicit selectors outright. A single explicit
        // selector wins as the position (code location stays descriptive). Zero
        // explicit selectors is only valid when a code location supplies a RIP.
        if found.len() > 1 {
            return Err(HappensBeforeError::AmbiguousPosition {
                event: name.to_string(),
                found: found.iter().map(|s| s.to_string()).collect(),
            });
        }
        if found.is_empty() && !has_code_location {
            return Err(HappensBeforeError::AmbiguousPosition {
                event: name.to_string(),
                found: Vec::new(),
            });
        }

        if ev.fd.is_some() && ev.syscall.is_none() {
            return Err(HappensBeforeError::FdWithoutSyscall {
                event: name.to_string(),
            });
        }

        if ev.nth == Some(0) && ev.syscalls.is_none() && ev.rcbs.is_none() {
            return Err(HappensBeforeError::NthZero {
                event: name.to_string(),
            });
        }

        let nth = ev.nth.unwrap_or(1);
        let position = if let Some(n) = ev.syscalls {
            Position::SyscallCount(n)
        } else if let Some(m) = ev.rcbs {
            Position::Rcb(m)
        } else if let Some(sc) = &ev.syscall {
            let sysno = Sysno::from_str(sc).map_err(|_| HappensBeforeError::UnknownSyscall {
                event: name.to_string(),
                name: sc.clone(),
            })?;
            if ev.fd.is_some() && !syscall_takes_fd_first(sysno) {
                return Err(HappensBeforeError::FdNotFirstArgument {
                    event: name.to_string(),
                    syscall: sc.clone(),
                });
            }
            Position::Syscall {
                sysno,
                phase: ev.phase.map(Into::into),
                nth,
                fd: ev.fd,
            }
        } else if let Some(rip) = &ev.rip {
            let addr = parse_rip(rip).ok_or_else(|| HappensBeforeError::BadRip {
                event: name.to_string(),
                text: rip.clone(),
            })?;
            Position::Rip {
                addr: Some(addr),
                nth,
            }
        } else if let Some(mark) = &ev.mark {
            Position::Marker {
                name: mark.clone(),
                nth,
            }
        } else {
            // Code-location-only: a RIP to be resolved later from debug info.
            debug_assert!(has_code_location);
            Position::Rip { addr: None, nth }
        };

        Ok(Anchor {
            name: name.to_string(),
            thread,
            position,
            location,
        })
    }

    /// Resolve an event's `thread` field to a [`ThreadRef`], consulting the
    /// `threads` table and falling back to a raw integer id.
    fn resolve_thread(&self, event: &str, thread: &str) -> Result<ThreadRef, HappensBeforeError> {
        if let Some(spec) = self.threads.get(thread) {
            Ok(ThreadRef {
                label: spec.label.clone().unwrap_or_else(|| thread.to_string()),
                dettid: spec.dettid.map(DetTid::from_raw),
                spawn_ordinal: spec.spawn_ordinal,
            })
        } else if let Ok(raw) = thread.parse::<i32>() {
            Ok(ThreadRef {
                label: thread.to_string(),
                dettid: Some(DetTid::from_raw(raw)),
                spawn_ordinal: None,
            })
        } else {
            Err(HappensBeforeError::UnknownThread {
                event: event.to_string(),
                thread: thread.to_string(),
            })
        }
    }
}

/// Parse a RIP string: hex (`0x...`) or plain decimal.
fn parse_rip(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Detect a cycle in the edge graph via depth-first search, returning the cycle
/// path if one exists. Anchors are visited in name order for determinism.
fn detect_cycle(
    anchors: &BTreeMap<String, Anchor>,
    edges: &[HappensBeforeEdge],
) -> Result<(), HappensBeforeError> {
    // Adjacency: before -> [after...]
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in anchors.keys() {
        adj.entry(name.as_str()).or_default();
    }
    for e in edges {
        adj.entry(e.before.as_str())
            .or_default()
            .push(e.after.as_str());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let mut state: BTreeMap<&str, Mark> = BTreeMap::new();

    // Iterative DFS to avoid stack overflow on deep chains, tracking the current
    // path so we can report a concrete cycle.
    for root in adj.keys().copied() {
        if state.contains_key(root) {
            continue;
        }
        // Stack of (node, index of next neighbor to visit).
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        let mut path: Vec<&str> = vec![root];
        state.insert(root, Mark::Visiting);

        while let Some(&mut (node, ref mut idx)) = stack.last_mut() {
            let neighbors = &adj[node];
            if *idx < neighbors.len() {
                let next = neighbors[*idx];
                *idx += 1;
                match state.get(next) {
                    Some(Mark::Visiting) => {
                        // Found a back-edge: assemble the cycle from `path`.
                        let start = path.iter().position(|&n| n == next).unwrap_or(0);
                        let mut cycle: Vec<String> =
                            path[start..].iter().map(|s| s.to_string()).collect();
                        cycle.push(next.to_string());
                        return Err(HappensBeforeError::Cycle(cycle));
                    }
                    Some(Mark::Done) => {}
                    None => {
                        state.insert(next, Mark::Visiting);
                        path.push(next);
                        stack.push((next, 0));
                    }
                }
            } else {
                state.insert(node, Mark::Done);
                stack.pop();
                path.pop();
            }
        }
    }
    Ok(())
}

// ================================================================================
// Terse DSL
// ================================================================================
//
// One edge per non-empty, non-comment line:
//
//     writer:free_buffer#342  <  reader:read_buffer#97
//     writer:futex@post#5     <  reader:@0x401f3c#1
//     A:rcb=123456            <  B:sc=97
//
// Each side is `thread:anchor[#ordinal]`. The anchor token is one of:
//   * `name`            -> function name (code location)
//   * `@0xADDR`         -> raw RIP
//   * `syscall@phase`   -> a named syscall, optional `@pre`/`@post`/`@polling`
//   * `rcb=M`           -> after M RBCs (owner primary)
//   * `sc=N`            -> after N syscalls (owner primary)
// A trailing `#N` sets the occurrence ordinal (ignored by `rcb=`/`sc=`).
// A `!soft` suffix on the line marks the edge soft; default is hard.

impl HappensBeforeSpec {
    /// Parse the terse line-oriented DSL into a specification. Symbolic threads
    /// mentioned by name become entries in the `threads` table.
    pub fn from_dsl(input: &str) -> Result<HappensBeforeSpec, HappensBeforeError> {
        let mut spec = HappensBeforeSpec {
            version: HAPPENS_BEFORE_VERSION,
            threads: BTreeMap::new(),
            events: BTreeMap::new(),
            edges: Vec::new(),
        };
        let mut seen_names: BTreeSet<String> = BTreeSet::new();

        for (i, raw_line) in input.lines().enumerate() {
            let lineno = i + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Optional trailing "!soft" / "!hard".
            let (body, strength) = if let Some(b) = line.strip_suffix("!soft") {
                (b.trim(), Strength::Soft)
            } else if let Some(b) = line.strip_suffix("!hard") {
                (b.trim(), Strength::Hard)
            } else {
                (line, Strength::Hard)
            };

            let (lhs, rhs) = body
                .split_once('<')
                .ok_or_else(|| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: "expected '<' separating two events".to_string(),
                })?;

            let before = parse_dsl_side(lhs.trim(), lineno, &mut spec, &mut seen_names)?;
            let after = parse_dsl_side(rhs.trim(), lineno, &mut spec, &mut seen_names)?;
            spec.edges.push(EdgeSpec {
                before,
                after,
                strength,
            });
        }
        Ok(spec)
    }
}

/// Strip a `#`-or-`//` comment, but not a `#ordinal` that is part of a token.
/// We treat `//` as the only comment marker to avoid clashing with `#ordinal`.
fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Parse one side of a DSL edge, registering the event and thread in `spec`, and
/// returning the generated event name.
fn parse_dsl_side(
    token: &str,
    lineno: usize,
    spec: &mut HappensBeforeSpec,
    seen_names: &mut BTreeSet<String>,
) -> Result<String, HappensBeforeError> {
    let (thread, anchor) = token
        .split_once(':')
        .ok_or_else(|| HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("expected 'thread:anchor' in '{}'", token),
        })?;
    let thread = thread.trim();
    let anchor = anchor.trim();
    if thread.is_empty() || anchor.is_empty() {
        return Err(HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("empty thread or anchor in '{}'", token),
        });
    }

    // Split a trailing "#ordinal".
    let (anchor_body, nth) = match anchor.split_once('#') {
        Some((a, n)) => {
            let parsed = n
                .parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad ordinal '#{}'", n),
                })?;
            (a.trim(), Some(parsed))
        }
        None => (anchor, None),
    };

    let mut ev = EventSpec {
        thread: thread.to_string(),
        nth,
        ..Default::default()
    };

    if let Some(rest) = anchor_body.strip_prefix('@') {
        // raw rip: @0x...
        ev.rip = Some(rest.to_string());
    } else if let Some(m) = anchor_body.strip_prefix("rcb=") {
        ev.rcbs = Some(
            m.parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad rcb count '{}'", m),
                })?,
        );
        ev.nth = None;
    } else if let Some(n) = anchor_body.strip_prefix("sc=") {
        ev.syscalls = Some(
            n.parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad syscall count '{}'", n),
                })?,
        );
        ev.nth = None;
    } else if let Some((sc, phase)) = anchor_body.split_once('@') {
        // syscall@phase
        ev.syscall = Some(sc.to_string());
        ev.phase = Some(parse_dsl_phase(phase, lineno)?);
    } else if is_syscall_name(anchor_body) {
        // bare syscall name
        ev.syscall = Some(anchor_body.to_string());
    } else {
        // function name (code location)
        ev.func = Some(anchor_body.to_string());
    }

    // Generate a stable, unique event name from the token.
    let base = sanitize_name(token);
    let mut ev_name = base.clone();
    let mut suffix = 1;
    while seen_names.contains(&ev_name) && spec.events.get(&ev_name) != Some(&ev) {
        suffix += 1;
        ev_name = format!("{}_{}", base, suffix);
    }
    seen_names.insert(ev_name.clone());
    spec.events.entry(ev_name.clone()).or_insert(ev);

    // Register the thread label if not already present and not a raw id.
    if thread.parse::<i32>().is_err() {
        spec.threads
            .entry(thread.to_string())
            .or_insert(ThreadSpec {
                label: Some(thread.to_string()),
                dettid: None,
                spawn_ordinal: None,
            });
    }

    Ok(ev_name)
}

fn parse_dsl_phase(phase: &str, lineno: usize) -> Result<PhaseSpec, HappensBeforeError> {
    match phase.trim().to_ascii_lowercase().as_str() {
        "pre" | "prehook" => Ok(PhaseSpec::Prehook),
        "post" | "posthook" => Ok(PhaseSpec::Posthook),
        "poll" | "polling" => Ok(PhaseSpec::Polling),
        other => Err(HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("unknown syscall phase '{}'", other),
        }),
    }
}

/// True when the token parses as a known syscall name.
fn is_syscall_name(s: &str) -> bool {
    Sysno::from_str(s).is_ok()
}

/// Turn a DSL token into a valid, readable event-name slug.
fn sanitize_name(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for ch in token.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('e');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json() -> &'static str {
        r#"{
          "version": 1,
          "threads": { "writer": {"label": "writer"}, "reader": {"label": "reader"} },
          "events": {
            "X_342": {"thread": "writer", "func": "free_buffer", "line": 120, "nth": 342},
            "Y_97":  {"thread": "reader", "func": "read_buffer", "nth": 97},
            "lockA":  {"thread": "writer", "syscall": "futex", "phase": "posthook", "nth": 5},
            "storeB": {"thread": "reader", "rip": "0x401f3c", "nth": 1},
            "scA":    {"thread": "writer", "syscalls": 10},
            "rcbB":   {"thread": "reader", "rcbs": 123456}
          },
          "edges": [
            {"before": "X_342", "after": "Y_97", "strength": "hard"},
            {"before": "lockA", "after": "storeB"},
            {"before": "scA", "after": "rcbB", "strength": "soft"}
          ]
        }"#
    }

    /// The guest-side checkpoint filter is exact per thread: true for the named
    /// dettid at exactly its anchored count, false for another dettid or for
    /// count +/- 1 or at count 0, true for every thread at a spawn-ordinal
    /// anchor's count (the guest side cannot resolve ordinals), and false at
    /// numbers that belong to other position kinds (an RCB value, a
    /// syscall-by-name occurrence).
    #[test]
    fn may_have_syscall_count_anchor_at_is_exact_per_thread() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "threads": {"T7": {"dettid": 7}, "S2": {"spawn_ordinal": 2}},
                "events": {"a": {"thread": "T7", "syscalls": 10},
                           "b": {"thread": "S2", "syscalls": 20},
                           "c": {"thread": "T7", "rcbs": 30},
                           "d": {"thread": "T7", "syscall": "futex", "phase": "pre", "nth": 40}},
                "edges": [{"before": "a", "after": "b"}]}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 10));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 10));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 9));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 11));
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 20));
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 20));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 21));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 30));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 0));
        assert!(matches!(
            prog.anchors["d"].position,
            Position::Syscall { nth: 40, .. }
        ));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 40));
    }

    /// `{"syscall": "writev", "fd": 9, "nth": 3}` is the thread's third
    /// `writev` to fd 9: a syscall-occurrence anchor that needs no calibration.
    #[test]
    fn syscall_fd_anchor_normalizes_and_displays() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "events": {"m": {"thread": "7", "syscall": "writev", "fd": 9, "nth": 3}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        let anchor = &prog.anchors["m"];
        assert_eq!(
            anchor.position,
            Position::Syscall {
                sysno: Sysno::writev,
                phase: None,
                nth: 3,
                fd: Some(9),
            }
        );
        assert_eq!(anchor.position.to_string(), "writev(fd=9)#3");
        assert!(anchor.is_syscall_occurrence());
        assert_eq!(prog.unenforced_positions().count(), 0);
    }

    /// `fd` restricts a named syscall whose first argument is a descriptor; set
    /// anywhere else it is refused by name when the spec is loaded, never
    /// matched against an argument that is not a descriptor.
    #[test]
    fn syscall_fd_anchor_is_refused_where_it_cannot_apply() {
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"x": {"thread": "7", "syscalls": 5, "fd": 9}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::FdWithoutSyscall {
                event: "x".to_string()
            }
        );
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"y": {"thread": "7", "syscall": "getpid", "fd": 9}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::FdNotFirstArgument {
                event: "y".to_string(),
                syscall: "getpid".to_string()
            }
        );
        assert!(err.to_string().contains("'y'"), "{err}");
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"z": {"thread": "7", "syscall": "write", "fd": 1, "nth": 0}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::NthZero {
                event: "z".to_string()
            }
        );
    }

    /// Occurrences are counted per thread over the calls that match: the
    /// anchor's syscall and, when it has one, its fd (the register's low 32
    /// bits). The second matching `writev` to fd 9 reaches an `nth: 2` anchor
    /// even with non-matching calls in between, an fd-agnostic anchor counts
    /// every fd, a spawn-ordinal anchor is returned on any thread (the
    /// scheduler resolves it), and a posthook anchor is never counted and is
    /// reported as unenforced.
    #[test]
    fn count_syscall_occurrences_is_exact_per_thread() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "threads": {"S1": {"spawn_ordinal": 1}},
                "events": {"fd9": {"thread": "7", "syscall": "writev", "fd": 9, "nth": 2},
                           "anyfd": {"thread": "7", "syscall": "writev", "nth": 3},
                           "spawned": {"thread": "S1", "syscall": "write", "fd": 1},
                           "post": {"thread": "7", "syscall": "close", "phase": "post"}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(prog.has_syscall_occurrence_anchors());
        let t7 = DetTid::from_raw(7);
        let t8 = DetTid::from_raw(8);
        let mut c7 = OccurrenceCounters::new();
        let count = |c: &mut OccurrenceCounters, t, sysno, arg0| {
            prog.count_syscall_occurrences(c, t, sysno, arg0)
        };
        assert!(count(&mut c7, t7, Sysno::writev, 9).is_empty());
        // Another fd and another syscall do not count toward fd9.
        assert!(count(&mut c7, t7, Sysno::writev, 8).is_empty());
        assert!(count(&mut c7, t7, Sysno::write, 9).is_empty());
        // Low 32 bits: this is fd 9, the second, and the third writev of any fd.
        assert_eq!(
            count(&mut c7, t7, Sysno::writev, 0x1_0000_0009),
            ["anyfd", "fd9"]
        );
        assert!(count(&mut c7, t7, Sysno::writev, 9).is_empty());
        // Thread 8 has its own counters; anchors on thread 7 are never its.
        let mut c8 = OccurrenceCounters::new();
        assert!(count(&mut c8, t8, Sysno::writev, 9).is_empty());
        assert!(count(&mut c8, t8, Sysno::writev, 9).is_empty());
        // A spawn-ordinal anchor is returned on any thread at its nth.
        assert_eq!(count(&mut c8, t8, Sysno::write, 1), ["spawned"]);
        assert!(count(&mut c8, t8, Sysno::write, 2).is_empty());
        // A posthook anchor is not enforced, never counted.
        assert!(count(&mut c7, t7, Sysno::close, 3).is_empty());
        assert!(!c7.contains_key(&(Sysno::close as usize, None)));
        let unenforced: Vec<&str> = prog
            .unenforced_positions()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(unenforced, ["post"]);
        // A count anchor is not a syscall-occurrence anchor and is unaffected.
        assert!(!prog.may_have_syscall_count_anchor_at(t7, 2));
        let count_only = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"a": {"thread": "7", "syscalls": 2}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(!count_only.has_syscall_occurrence_anchors());
    }

    #[test]
    fn parse_and_normalize_rfc_example() {
        let spec = HappensBeforeSpec::from_json(spec_json()).unwrap();
        let prog = spec.normalize().unwrap();
        assert_eq!(prog.anchors.len(), 6);
        assert_eq!(prog.edges.len(), 3);

        // Owner-primary positions.
        assert_eq!(prog.anchors["scA"].position, Position::SyscallCount(10));
        assert_eq!(prog.anchors["rcbB"].position, Position::Rcb(123456));

        // Function+line becomes an unresolved RIP with a code location attached.
        match &prog.anchors["X_342"].position {
            Position::Rip { addr: None, nth } => assert_eq!(*nth, 342),
            other => panic!("expected unresolved RIP, got {:?}", other),
        }
        assert_eq!(
            prog.anchors["X_342"].location.function.as_deref(),
            Some("free_buffer")
        );
        assert_eq!(prog.anchors["X_342"].location.line, Some(120));

        // Syscall anchor parses the name and phase.
        match &prog.anchors["lockA"].position {
            Position::Syscall {
                sysno,
                phase,
                nth,
                fd: None,
            } => {
                assert_eq!(*sysno, Sysno::futex);
                assert_eq!(*phase, Some(SyscallPhase::Posthook));
                assert_eq!(*nth, 5);
            }
            other => panic!("expected syscall, got {:?}", other),
        }

        // RIP anchor.
        assert_eq!(
            prog.anchors["storeB"].position,
            Position::Rip {
                addr: Some(0x401f3c),
                nth: 1
            }
        );

        // Soft strength preserved.
        assert_eq!(prog.edges[2].strength, Strength::Soft);
        // Default strength is hard.
        assert_eq!(prog.edges[1].strength, Strength::Hard);

        // One anchor needs debug-info resolution (X_342, Y_97).
        assert_eq!(prog.unresolved_locations().count(), 2);
    }

    #[test]
    fn round_trip_json() {
        let spec = HappensBeforeSpec::from_json(spec_json()).unwrap();
        let json = spec.to_json().unwrap();
        let spec2 = HappensBeforeSpec::from_json(&json).unwrap();
        assert_eq!(spec, spec2);
    }

    #[test]
    fn rejects_wrong_version() {
        let spec = HappensBeforeSpec {
            version: 999,
            ..HappensBeforeSpec::from_json(spec_json()).unwrap()
        };
        assert_eq!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnsupportedVersion(999)
        );
    }

    #[test]
    fn rejects_ambiguous_position() {
        let json = r#"{
          "version": 1,
          "events": { "bad": {"thread": "1", "syscalls": 3, "rcbs": 5} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        match spec.normalize().unwrap_err() {
            HappensBeforeError::AmbiguousPosition { event, found } => {
                assert_eq!(event, "bad");
                assert_eq!(found.len(), 2);
            }
            other => panic!("expected AmbiguousPosition, got {:?}", other),
        }
    }

    #[test]
    fn rejects_no_position() {
        let json = r#"{
          "version": 1,
          "events": { "bad": {"thread": "1"} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::AmbiguousPosition { .. }
        ));
    }

    #[test]
    fn code_location_accompanies_count() {
        // The owner's primary anchor: "function foo (line L) on thread T after
        // N syscalls / M RBCs". The code location is descriptive and the count
        // is the enforced position; the two must coexist, not conflict.
        let json = r#"{
          "version": 1,
          "events": {
            "w": {"thread": "1", "func": "free_buffer", "line": 342, "syscalls": 7},
            "r": {"thread": "1", "func": "read_buffer", "rcbs": 900}
          },
          "edges": [ {"before": "w", "after": "r"} ]
        }"#;
        let prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();

        // The count wins as the position; the code location is retained.
        assert_eq!(prog.anchors["w"].position, Position::SyscallCount(7));
        assert_eq!(
            prog.anchors["w"].location.function.as_deref(),
            Some("free_buffer")
        );
        assert_eq!(prog.anchors["w"].location.line, Some(342));

        assert_eq!(prog.anchors["r"].position, Position::Rcb(900));
        assert_eq!(
            prog.anchors["r"].location.function.as_deref(),
            Some("read_buffer")
        );

        // A descriptive-only code location is not an unresolved RIP position.
        assert_eq!(prog.unresolved_locations().count(), 0);
    }

    #[test]
    fn raw_dettid_thread() {
        let json = r#"{
          "version": 1,
          "events": { "e": {"thread": "42", "rcbs": 7} },
          "edges": []
        }"#;
        let prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();
        assert_eq!(prog.anchors["e"].thread.dettid, Some(DetTid::from_raw(42)));
    }

    #[test]
    fn rejects_unknown_thread() {
        let json = r#"{
          "version": 1,
          "events": { "e": {"thread": "ghost", "rcbs": 7} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownThread { .. }
        ));
    }

    #[test]
    fn rejects_unknown_event_in_edge() {
        let json = r#"{
          "version": 1,
          "events": { "a": {"thread": "1", "rcbs": 7} },
          "edges": [ {"before": "a", "after": "missing"} ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownEvent { .. }
        ));
    }

    #[test]
    fn rejects_unknown_syscall() {
        let json = r#"{
          "version": 1,
          "events": { "a": {"thread": "1", "syscall": "not_a_syscall"} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownSyscall { .. }
        ));
    }

    #[test]
    fn detects_cycle() {
        let json = r#"{
          "version": 1,
          "events": {
            "a": {"thread": "1", "rcbs": 1},
            "b": {"thread": "1", "rcbs": 2},
            "c": {"thread": "1", "rcbs": 3}
          },
          "edges": [
            {"before": "a", "after": "b"},
            {"before": "b", "after": "c"},
            {"before": "c", "after": "a"}
          ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        match spec.normalize().unwrap_err() {
            HappensBeforeError::Cycle(path) => {
                // Path forms a closed loop.
                assert_eq!(path.first(), path.last());
                assert!(path.len() >= 4);
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    #[test]
    fn accepts_dag() {
        let json = r#"{
          "version": 1,
          "events": {
            "a": {"thread": "1", "rcbs": 1},
            "b": {"thread": "1", "rcbs": 2},
            "c": {"thread": "1", "rcbs": 3}
          },
          "edges": [
            {"before": "a", "after": "c"},
            {"before": "b", "after": "c"}
          ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(spec.normalize().is_ok());
    }

    #[test]
    fn dsl_desugars() {
        let dsl = "\
            // btrfs race: erase-by-key must precede the re-insert
            writer:free_buffer#342  <  reader:read_buffer#97
            writer:futex@post#5     <  reader:@0x401f3c#1
            A:rcb=123456            <  B:sc=97   !soft
        ";
        let spec = HappensBeforeSpec::from_dsl(dsl).unwrap();
        let prog = spec.normalize().unwrap();
        assert_eq!(prog.edges.len(), 3);
        assert_eq!(prog.anchors.len(), 6);

        // The rcb/sc line desugars to owner-primary positions and is soft.
        let soft = &prog.edges[2];
        assert_eq!(soft.strength, Strength::Soft);
        assert_eq!(prog.anchors[&soft.before].position, Position::Rcb(123456));
        assert_eq!(
            prog.anchors[&soft.after].position,
            Position::SyscallCount(97)
        );

        // futex@post#5 desugars to a phase-qualified syscall.
        let futex = prog
            .anchors
            .values()
            .find(
                |a| matches!(a.position, Position::Syscall { sysno, .. } if sysno == Sysno::futex),
            )
            .unwrap();
        match &futex.position {
            Position::Syscall { phase, nth, .. } => {
                assert_eq!(*phase, Some(SyscallPhase::Posthook));
                assert_eq!(*nth, 5);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn dsl_rejects_missing_arrow() {
        let err = HappensBeforeSpec::from_dsl("writer:foo reader:bar").unwrap_err();
        assert!(matches!(err, HappensBeforeError::DslSyntax { line: 1, .. }));
    }

    #[test]
    fn parse_rip_forms() {
        assert_eq!(parse_rip("0x401f3c"), Some(0x401f3c));
        assert_eq!(parse_rip("4201276"), Some(4201276));
        assert_eq!(parse_rip("nonsense"), None);
    }
}
