/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The main queue for runnable tasks.
//!
//! Tasks are selected from the queue based on 2 factors:
//! 1. Their priority
//! 2. Their round-robin order.
//!
//! The factors are compared in that order. Round-robin orders monotonically increase across
//! the entire queue; a task is assigned an order at insertion time.
//!
//! Round-robin orders can also be negative when the `push_front` method is
//! used. This "skips the line" within the priority level. This is mostly
//! relevant in non-chaos modes, where all threads have the same priority. In
//! this case, time-based events should skip the line and end-up in the front of
//! the queue.
//!
//! In addition, some priority values are reserved, such as a high priority
//! for IO eager polling.
//!
//! # Polling strategy
//!
//! We employ a polling strategy for guest threads that *would* blocked, but where we
//! don't model precisely what conditions they're waiting for.  Anywhere we have a precise
//! model of inter-thread dependencies (e.g. futexes), we can sleep a thread until we
//! encounter the matching event that will wake it.  But for Linux features that we don't
//! model 100% precisely, polling is a way to remain agnostic as to the exact
//! dependencies, but still support these blocking behaviors deterministically.
//!
//! The older DetTrace system used polling, but it would poll every time through the
//! round robin queue, which can create extremely bad performance with many threads
//! polling an unbounded number of times.  We can greatly improve the performance by
//! polling only at less-frequent, but still deterministically-defined intervals, such
//! as when we think we're out of "productive" work to do.
//!
//! Thererefore we have special handling for scheduling polling tasks. When
//! initially queued, there is exponential backoff in priority with the number
//! of attempts. After enough queueing operations are performed, however,
//! polling tasks are upgraded to their original priority to prevent complete
//! starvation. The frequency of upgrades is controlled by `POLLING_UPGRADE_INTERVAL`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Display;

use rand::RngExt as _;
use rand::SeedableRng;
use rand::distr::uniform::SampleUniform;
use rand_pcg::Pcg64Mcg;

use crate::config::SchedHeuristic;
use crate::detlog;
use crate::types::DetTid;

/// The user-accessible priority of a thread. Lowest runs first.
pub type Priority = u64;

const EAGER_IO_REPOLL_PRIORITY: Priority = Priority::MIN;

/// The lowest/highest priority a thread can have.
pub const FIRST_PRIORITY: Priority = EAGER_IO_REPOLL_PRIORITY + 1;

/// The last/lowest (numerically largest) priority a thread can have.
pub const LAST_PRIORITY: Priority = 10000;

/// A high priority for threads the replayer DOES want to run.
pub const REPLAY_FOREGROUND_PRIORITY: Priority = FIRST_PRIORITY;

/// A low priority given to threads the replayer does NOT want to run.
pub const REPLAY_DEFERRED_PRIORITY: Priority = LAST_PRIORITY - 1;

/// The default priority for a thread. If chaos mode is not enabled, all threads have this
/// priority.
pub const DEFAULT_PRIORITY: Priority = 1000;

/// Whether the priority is user-accessible. We use some values for special
/// purposes; these shouldn't be set by the user.
pub fn is_ordinary_priority(prio: Priority) -> bool {
    (FIRST_PRIORITY..=LAST_PRIORITY).contains(&prio)
}

/// Deterministically transform 64 bits of entropy into a random user-settable
/// priority.
pub fn entropy_to_priority(entropy: u64) -> Priority {
    let range = LAST_PRIORITY - FIRST_PRIORITY + 1;
    let offset = entropy % range;
    FIRST_PRIORITY + offset
}

/// The round robin turn of threads within a given priority level. Lowest runs
/// first. Both negative and positive values are used to allow insertion of a
/// thread at both the "front" and "back" of a priority level.
type RoundRobinTurn = i64;

/// The key into the priority queue that uniquely determines what to run next.
/// Priorities that compare lower run first.
#[derive(Debug, Copy, Clone)]
pub struct PrioritizedOrder {
    priority: Priority,
    turn: RoundRobinTurn,
}

// These match the derived definitions, but clearly display our intention:
impl Ord for PrioritizedOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then(self.turn.cmp(&other.turn))
    }
}
impl PartialOrd for PrioritizedOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for PrioritizedOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for PrioritizedOrder {}

impl fmt::Display for PrioritizedOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(p: {}, t: {})", self.priority, self.turn)
    }
}

/// After queueing this many new tasks, perform a "poll upgrade," in which
/// we upgrade all outstanding polling tasks their original priority levels,
/// temporarily negating backoff behavior.
const POLLING_UPGRADE_INTERVAL: u64 = 200;

#[derive(Debug, Copy, Clone)]
struct QueueValue {
    tid: DetTid,
    /// Upgrade to this priority during polling upgrades
    poll_upgrade: Option<Priority>,
}

/// The service-lead budget cost charged to a thread for one completed
/// scheduling opportunity. Phase-1 uses a flat unit cost with no escalation, so
/// the liveness bound depends only on the budget and the population.
const FAIR_BASE_COST: u64 = 1;

/// Maximum stale credit a just-woken thread may keep relative to the runnable
/// floor. A short sleep cannot erase over-service debt (the `max` never lowers a
/// counter), and a long sleeper cannot bank unlimited credit (it is clamped to
/// at most this far below the floor). One base slice, per the design.
const FAIR_WAKE_CREDIT: u64 = FAIR_BASE_COST;

/// Bounded service-lead fairness overlay (research-only; see
/// `Config::sched_fairness_budget`).
///
/// Each thread carries a deterministic unsigned integer "fair service" counter
/// `S`. For the current runnable set `R`, let `F = min(S[j] for j in R)`. A
/// thread `i` is *eligible* iff `S[i] - F < budget`. Selection then applies the
/// unchanged priority/FIFO (or chaos) policy, but only among eligible threads.
/// A minimum-service thread always has lead `0 < budget`, so at least one thread
/// is always eligible: the overlay is work-conserving. Every committed turn
/// charges the selected thread `FAIR_BASE_COST`, so a cheap poll/yield loop
/// burns its lead budget and self-deprioritizes *by behavior*, with no poller
/// classification and without ever touching guest-visible virtual time.
#[derive(Debug, Clone)]
struct FairService {
    /// Service-lead budget `B > 0`, in scheduling opportunities.
    budget: u64,
    /// Per-thread service counter. Retained while a thread is blocked (absent
    /// from the queue) so its accounting survives a block/wake round trip.
    service: BTreeMap<DetTid, u64>,
    /// Threads that left the runnable set via `remove_tid` (a block), pending a
    /// wake-credit clamp when they are next admitted. A thread that merely took
    /// its turn (committed pop then requeue) is NOT in this set and keeps its
    /// counter unchanged on requeue.
    blocked: BTreeSet<DetTid>,
    /// Monotonic remembered floor so a thread cannot reset its service simply by
    /// draining the queue to empty and re-entering at zero.
    remembered_floor: u64,
}

impl FairService {
    fn new(budget: u64) -> Self {
        Self {
            budget,
            service: BTreeMap::new(),
            blocked: BTreeSet::new(),
            remembered_floor: 0,
        }
    }

    /// The runnable floor: the least service among threads currently queued,
    /// falling back to the monotonic remembered floor when the queue is empty.
    fn floor(&self, queue: &BTreeMap<PrioritizedOrder, QueueValue>) -> u64 {
        queue
            .values()
            .filter_map(|v| self.service.get(&v.tid).copied())
            .min()
            .unwrap_or(self.remembered_floor)
            .max(self.remembered_floor)
    }

    /// Whether `tid` is within the eligibility band given a precomputed `floor`.
    fn eligible(&self, tid: DetTid, floor: u64) -> bool {
        let s = self.service.get(&tid).copied().unwrap_or(floor);
        s.saturating_sub(floor) < self.budget
    }

    /// Admission: place a thread's counter as it (re)enters the runnable set.
    /// A brand-new thread is placed neutrally at the floor; a waking thread
    /// keeps its counter but has stale credit clamped; a thread requeued after
    /// taking a turn is left exactly as charged.
    fn on_admit(&mut self, tid: DetTid, queue: &BTreeMap<PrioritizedOrder, QueueValue>) {
        let floor = self.floor(queue);
        self.remembered_floor = self.remembered_floor.max(floor);
        match self.service.entry(tid) {
            std::collections::btree_map::Entry::Vacant(e) => {
                // New thread: neutral placement, not minimum preference.
                e.insert(floor);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                if self.blocked.remove(&tid) {
                    // Wake: retain over-service debt, clamp stale sleeper credit
                    // to at most one slice below the floor.
                    let clamped = floor.saturating_sub(FAIR_WAKE_CREDIT);
                    let s = e.get_mut();
                    *s = (*s).max(clamped);
                }
                // else: ordinary requeue after a turn -> counter unchanged.
            }
        }
    }

    /// A thread left the runnable set to block. Retain its counter and mark it
    /// for a wake clamp on readmission.
    fn on_block(&mut self, tid: DetTid) {
        if self.service.contains_key(&tid) {
            self.blocked.insert(tid);
        }
    }

    /// Charge one committed scheduling opportunity to the selected thread.
    fn on_commit(&mut self, tid: DetTid) {
        let s = self.service.entry(tid).or_insert(self.remembered_floor);
        *s = s.saturating_add(FAIR_BASE_COST);
    }
}

#[derive(Debug, Clone)]
pub struct RunQueue {
    /// We use a "flattened" queue (rather than a Priority -> Vec<DetTid> map)
    /// to simplify peek/pop logic: there's no need to ignore clear empty
    /// from unused priority levels vectors. This could also reduce allocator
    /// pressure. Also, each thread having a clear global key makes it easier to
    /// change their priorities after they are in the queue.
    ///
    /// Additionally, we use a TreeMap rather than a Heap to ease removing /
    /// inserting random values for poll upgrades. std::BinaryHeap would require
    /// destroying/re-allocating the entire structure to do this.
    queue: BTreeMap<PrioritizedOrder, QueueValue>,

    // We use global turn counters across all priority levels. This foregoes
    // the need for an extra data structure to track them, while also ensuring
    // unique keys for every insertion. Because of this, we never need to alter
    // a turn value when altering the priority level of a thread.
    last_back_turn: RoundRobinTurn,
    last_front_turn: RoundRobinTurn,

    /// Used to lock the queue from other changes while we are tentatively popping from it, and also
    /// cache the result.
    tentative_selection: Option<DetTid>,
    tentative_selection_is_exact: bool,

    /// A thread that explicitly yielded must not be selected again until some
    /// other runnable thread receives a turn.
    yielded_skip: Option<DetTid>,

    // TODO: The following fields need to be properly abstracted into separate types of run queues.
    /// Which scheduling strategy shall we use.
    sched_strategy: SchedHeuristic,
    prng: Pcg64Mcg,

    sticky_random_param: f64,
    sticky_random_selection: Option<DetTid>,

    /// Optional bounded service-lead fairness overlay. `None` (the default)
    /// preserves the exact legacy priority/FIFO selection with zero behavior
    /// change; `Some` enables deterministic service accounting and eligibility.
    fair: Option<FairService>,
}

/// A multi-line print of the runqueue.
impl fmt::Display for RunQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Run queue, size={}, last_back_turn={}, last_front_turn={}:",
            self.queue.len(),
            self.last_back_turn,
            self.last_front_turn,
        )?;
        for x in self.queue.iter() {
            writeln!(f, "    {:.500?}", x)?;
        }
        Ok(())
    }
}

impl RunQueue {
    /// Create a new RunQueue. `fairness_budget` enables the bounded service-lead
    /// fairness overlay when `Some`; `None` preserves exact legacy behavior.
    pub fn new(ss: SchedHeuristic, seed: u64, srp: f64, fairness_budget: Option<u64>) -> Self {
        detlog!("SCHEDRAND: seeding scheduler runqueue with seed {}", seed);
        if let Some(b) = fairness_budget {
            detlog!(
                "SCHEDFAIR: bounded service-lead fairness enabled, budget {}",
                b
            );
        }
        Self {
            queue: BTreeMap::new(),
            // For clarity, 0 is unused so that positive/negative == back/front:
            last_back_turn: 0,
            last_front_turn: 0,
            sched_strategy: ss,
            tentative_selection: None,
            tentative_selection_is_exact: false,
            yielded_skip: None,
            prng: Pcg64Mcg::seed_from_u64(seed),
            sticky_random_param: srp,
            sticky_random_selection: None,
            fair: fairness_budget.map(FairService::new),
        }
    }

    fn push_safety_check(&self, tid: DetTid) {
        if cfg!(debug_assertions) {
            // Expensive.
            for qv in self.queue.values() {
                if qv.tid == tid {
                    panic!(
                        "Invariant violation! Tried to add {} to runqueue, but it's already present:\n {:?}",
                        tid, self
                    );
                }
            }
        }
    }

    // Return the numerically least Priority value in the run_queue, or None if the queue is empty.
    pub fn first_priority(&self) -> Option<Priority> {
        let (k, _) = self.queue.first_key_value()?;
        Some(k.priority)
    }

    /// True if any thread other than `exclude` is runnable at ordinary
    /// (non-poller) priority. This is the "deterministic work still runnable"
    /// test used to decide whether an asynchronous signal delivery must defer to
    /// guest work that was already scheduled. Read-only, so it is safe to call
    /// while a tentative_pop selection is in progress.
    pub fn has_runnable_besides(&self, exclude: DetTid) -> bool {
        self.queue
            .iter()
            .any(|(k, v)| v.tid != exclude && k.priority < LAST_PRIORITY)
    }

    /// Push a thread to the back of the specified priority. Return the
    /// resulting overall position in the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_back(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        if !is_ordinary_priority(priority) {
            panic!("This is not an acceptable priority value: {}", priority);
        }
        self.push_back_inner(tid, priority, None)
    }

    /// Requeue an explicitly yielding thread at its persistent priority while
    /// excluding it from the next selection. The exclusion, rather than the
    /// queue key, makes this a one-turn operation under every heuristic.
    pub fn push_yielded(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.yielded_skip.is_none());
        self.yielded_skip = Some(tid);
        self.push_back(tid, priority)
    }

    /// Push a polling thread. The priority level is an exponential backoff from
    /// the given `normal_priority` value. The pushed thread will also
    /// participatein "poll upgrades" in which periodically polling threads are
    /// re-boosted to their original `normal_priority` values.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_poller(
        &mut self,
        tid: DetTid,
        normal_priority: Priority,
        poll_attempt: u32,
    ) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        // Exponential backoff in priority, up to LAST_PRIORITY:
        let priority = 1u64
            .checked_shl(poll_attempt)
            .and_then(|f| f.checked_mul(normal_priority))
            .unwrap_or(Priority::MAX)
            .min(LAST_PRIORITY);
        // Upgrade back to original priority:
        self.push_back_inner(tid, priority, Some(normal_priority))
    }

    fn push_back_inner(
        &mut self,
        tid: DetTid,
        priority: Priority,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        self.last_back_turn += 1;
        let turn = self.last_back_turn;
        let prio = PrioritizedOrder { priority, turn };
        self.push_inner(tid, prio, poll_upgrade)
    }

    /// Push a thread to the front of the specified priority. `push_back` should
    /// be used unless special circumstances call for `push_front`. Return the
    /// resulting overall position in the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_front(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        assert!(is_ordinary_priority(priority));
        self.push_front_inner(tid, priority, None)
    }

    /// Workaround for eager io repolling: this will send the thread to the
    /// absolute front of the queue. Return the resulting overall position in
    /// the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_eager_io_repoll(&mut self, tid: DetTid) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        let priority = EAGER_IO_REPOLL_PRIORITY;
        self.push_front_inner(tid, priority, None)
    }

    fn push_front_inner(
        &mut self,
        tid: DetTid,
        priority: Priority,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        self.last_front_turn -= 1;
        let turn = self.last_front_turn;
        let prio = PrioritizedOrder { priority, turn };
        self.push_inner(tid, prio, poll_upgrade)
    }

    fn push_inner(
        &mut self,
        tid: DetTid,
        prio: PrioritizedOrder,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        // Admit into the fairness overlay before the insert so a brand-new
        // thread is placed at the floor of the *other* runnable threads rather
        // than counting itself.
        if let Some(fair) = self.fair.as_mut() {
            fair.on_admit(tid, &self.queue);
        }
        let qval = QueueValue { tid, poll_upgrade };
        let old = self.queue.insert(prio, qval);
        assert!(old.is_none()); // last_*_turn should be monotonic
        self.check_poll_upgrade();
        prio
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn tids(&self) -> impl Iterator<Item = &DetTid> {
        self.queue.values().map(|v| &v.tid)
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn contains_tid(&self, tid: DetTid) -> bool {
        self.tids().any(|t| t == &tid)
    }

    /// Remove `tid` from the queue, returning true if removal ocurred.
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn remove_tid(&mut self, tid: DetTid) -> bool {
        assert!(self.tentative_selection.is_none());

        // This is O(N), but could be faster if we also stored a thread -> priority mapping.
        let mut kept_all = true;
        self.queue.retain(|_k, v| {
            let ret = v.tid != tid;
            kept_all = kept_all && ret;
            ret
        });
        if self.yielded_skip == Some(tid) {
            self.yielded_skip = None;
        }
        // A thread leaving the runnable set is treated as blocking: retain its
        // service counter and mark it for a wake-credit clamp on readmission.
        if !kept_all && let Some(fair) = self.fair.as_mut() {
            fair.on_block(tid);
        }
        !kept_all
    }

    // Helper function for logging purposes.
    fn random_range<T>(&mut self, start: T, end: T) -> T
    where
        T: SampleUniform + Display + PartialOrd + Copy,
    {
        let r = self.prng.random_range(start..end);
        detlog!("SCHEDRAND: [{},{}) => {}", start, end, r);
        r
    }

    /// Begin, but do not complete, a pop_transaction.  This can be committed or undone later.  But
    /// one of those must happen before other modification operations can occur on the RunQueue.
    ///
    /// Postcondition: if return a `Some` value, the RunQueue enters a *locked* state where
    /// commit or undo must happen before any other mutations to the structure.
    pub fn tentative_pop_next(&mut self) -> Option<DetTid> {
        assert!(!self.tentative_selection_is_exact);
        let skip = self
            .yielded_skip
            .filter(|tid| self.queue.len() > 1 && self.contains_tid(*tid));
        // Precompute the runnable floor once so every eligibility check in this
        // selection sees the same value. `None` when the overlay is disabled.
        let floor = self.fair.as_ref().map(|f| f.floor(&self.queue));
        self.tentative_selection = match self.sched_strategy {
            SchedHeuristic::None | SchedHeuristic::ConnectBind => self
                .queue
                .values()
                .find(|value| {
                    Some(value.tid) != skip && Self::fair_eligible(&self.fair, value.tid, floor)
                })
                // Work-conserving fallback: if the eligibility band excluded
                // every non-skipped thread (only possible when the sole eligible
                // thread is the yielded one), ignore eligibility this turn.
                .or_else(|| self.queue.values().find(|value| Some(value.tid) != skip))
                .map(|value| value.tid),
            SchedHeuristic::Random => {
                if self.queue.is_empty() {
                    return None;
                }

                // If there is not Tid picked from a previous operation, let's pick one now.
                if self.tentative_selection.is_none() {
                    let candidates = self.eligible_candidates(skip, floor);
                    let random_idx = self.random_range(0, candidates.len());
                    self.tentative_selection = candidates.get(random_idx).copied();
                };

                self.tentative_selection
            }
            SchedHeuristic::StickyRandom => {
                if self.queue.is_empty() {
                    return None;
                }

                if self.sticky_random_selection == skip {
                    self.sticky_random_selection = None;
                }
                // Drop a sticky selection that has fallen out of the eligibility
                // band so fairness can force a switch.
                if let Some(sel) = self.sticky_random_selection
                    && !Self::fair_eligible(&self.fair, sel, floor)
                {
                    self.sticky_random_selection = None;
                }
                if self.sticky_random_selection.is_none()
                    || !self.contains_tid(self.sticky_random_selection.unwrap())
                {
                    let candidates = self.eligible_candidates(skip, floor);
                    let random_idx = self.random_range(0, candidates.len());
                    self.sticky_random_selection = candidates.get(random_idx).copied();
                }

                self.sticky_random_selection
            }
        };

        self.tentative_selection
    }

    /// Whether the fairness overlay (if enabled) admits `tid` given a
    /// precomputed `floor`. Always true when the overlay is disabled.
    fn fair_eligible(fair: &Option<FairService>, tid: DetTid, floor: Option<u64>) -> bool {
        match (fair, floor) {
            (Some(f), Some(fl)) => f.eligible(tid, fl),
            _ => true,
        }
    }

    /// The randomly-selectable candidate tids for chaos heuristics: non-skipped
    /// and (when the overlay is on) within the eligibility band, in deterministic
    /// queue order. Falls back to all non-skipped tids if the band would leave
    /// no candidate, preserving work-conservation.
    fn eligible_candidates(&self, skip: Option<DetTid>, floor: Option<u64>) -> Vec<DetTid> {
        let eligible: Vec<DetTid> = self
            .queue
            .values()
            .filter(|v| Some(v.tid) != skip && Self::fair_eligible(&self.fair, v.tid, floor))
            .map(|v| v.tid)
            .collect();
        if eligible.is_empty() {
            self.queue
                .values()
                .filter(|v| Some(v.tid) != skip)
                .map(|v| v.tid)
                .collect()
        } else {
            eligible
        }
    }

    // TODO-HUMAN-REVIEW(PR-868): Review exact run-queue selection for vfork barriers.
    /// Begin a pop transaction for one specific queued thread, bypassing the
    /// configured scheduling heuristic without changing the thread's priority.
    pub fn tentative_pop_tid(&mut self, tid: DetTid) -> Option<DetTid> {
        assert!(self.tentative_selection.is_none());
        if self.contains_tid(tid) {
            self.tentative_selection = Some(tid);
            self.tentative_selection_is_exact = true;
        }
        self.tentative_selection
    }

    /// Complete the tentative pop operation, readying the RunQueue for future operations.  This
    /// operation is only permissible when the queue is locked, i.e. the tentative_pop has
    /// previously returned `Some`.
    pub fn commit_tentative_pop(&mut self) -> DetTid {
        self.commit_tentative_pop_inner(true)
    }

    /// Like [`commit_tentative_pop`] but does NOT charge the fairness overlay.
    ///
    /// Used only for internal I/O poll-retry requeues (the `poll_attempt > 0`
    /// NONCOMMIT path in the scheduler). The *count* of those retries is
    /// host-timing nondeterministic — the scheduler already keeps their
    /// time-advance out of the determinism log for exactly this reason — so
    /// charging them would leak host nondeterminism into the selection-gating
    /// service counter and make `--verify` diverge on poll-heavy, external-actor
    /// workloads (#140). A poll retry is not guest progress and must not consume
    /// fairness budget.
    pub fn commit_tentative_pop_uncharged(&mut self) -> DetTid {
        self.commit_tentative_pop_inner(false)
    }

    fn commit_tentative_pop_inner(&mut self, charge: bool) -> DetTid {
        // Check that queue is locked and unlock it.
        let tentative_selection = self
            .tentative_selection
            .take()
            .expect("tentative_pop to already returned a `Some`");
        let exact = std::mem::take(&mut self.tentative_selection_is_exact);

        let ret = if exact {
            let key = *self
                .queue
                .iter()
                .find(|(_key, value)| value.tid == tentative_selection)
                .map(|(key, _value)| key)
                .unwrap();
            self.queue.remove(&key).map(|value| value.tid)
        } else {
            match self.sched_strategy {
                SchedHeuristic::None | SchedHeuristic::ConnectBind | SchedHeuristic::Random => {
                    let key = *self
                        .queue
                        .iter()
                        .find(|(_k, v)| v.tid == tentative_selection)
                        .map(|(k, _v)| k)
                        .unwrap();
                    self.queue.remove(&key).map(|v| v.tid)
                }
                SchedHeuristic::StickyRandom => {
                    let tid = self.sticky_random_selection.unwrap();
                    // Probability of staying to our current thread on the next round.
                    // If the generated random number is smaller than what we set, we switch threads.
                    if self.random_range(0f64, 1f64) <= 1.0 - self.sticky_random_param {
                        self.sticky_random_selection = None;
                    }

                    let key = *self
                        .queue
                        .iter()
                        .find(|(_k, v)| v.tid == tid)
                        .map(|(k, _v)| k)
                        .unwrap();

                    self.queue.remove(&key).map(|v| v.tid)
                }
            }
        }
        .expect("to always return a DetTid");
        // The above should always return a DetTid as we peeked right before.
        // If this invariant is violated, then it's a bug, or the queue is modified
        // between the peek and tentative_pop and commit_tentative_pop.
        debug_assert!(ret == tentative_selection);
        // Charge the committed scheduling opportunity. This is the single choke
        // point for every real turn (chaos reprioritization and completed guest
        // turns route through here with `charge = true`); `undo_tentative_pop`
        // deliberately bypasses it so a rolled-back selection is never charged,
        // and the internal I/O poll-retry requeue routes through
        // `commit_tentative_pop_uncharged` (`charge = false`) so a
        // host-nondeterministic retry count never gates fairness (#140).
        if charge && let Some(fair) = self.fair.as_mut() {
            fair.on_commit(ret);
        }
        ret
    }

    /// Commit a tentative pop for a guest turn that will actually run. A
    /// yielded thread's one-turn exclusion is consumed only here, not by
    /// scheduler bookkeeping turns that never unblock a guest.
    pub fn commit_tentative_pop_completed_turn(&mut self) -> DetTid {
        let tid = self.commit_tentative_pop();
        self.consume_yield_exclusion();
        tid
    }

    /// Mark that a different guest received execution after an explicit yield.
    pub fn consume_yield_exclusion(&mut self) {
        self.yielded_skip = None;
    }

    /// Forget the tentative pop as though it never happened.
    pub fn undo_tentative_pop(&mut self) {
        assert!(self.tentative_selection.is_some());
        self.tentative_selection = None;
        self.tentative_selection_is_exact = false;
    }

    /// Return how many things have been queued.
    fn turn_counter(&self) -> u64 {
        debug_assert!(self.last_back_turn >= 0);
        debug_assert!(self.last_front_turn <= 0);
        self.last_back_turn as u64 + self.last_front_turn.unsigned_abs()
    }

    fn check_poll_upgrade(&mut self) {
        if self.turn_counter().is_multiple_of(POLLING_UPGRADE_INTERVAL) {
            self.do_poll_upgrade()
        }
    }

    /// Upgrade polled tasks to their specified normal priority.
    #[cold]
    fn do_poll_upgrade(&mut self) {
        let upgrades = self
            .queue
            .iter()
            .filter_map(|(k, v)| v.poll_upgrade.map(|upgd| (*k, upgd)))
            .collect::<Vec<(PrioritizedOrder, Priority)>>();
        // TODO(T100400409): if all polling threads are below a certain priority, this
        // can use a range query rather than iterating over all threads in the
        // run queue:
        for (key, upgrade_prio) in upgrades {
            let mut new_key = key;
            new_key.priority = upgrade_prio;
            let mut qval = self.queue.remove(&key).unwrap();
            qval.poll_upgrade = None; // there's no need to upgrade to the same priority twice
            let old = self.queue.insert(new_key, qval);
            assert!(old.is_none()); // round robin turns should ensure uniqueness
        }
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new(SchedHeuristic::None, 0, 0.0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yielded_thread_cedes_exactly_one_turn_under_every_heuristic() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::ConnectBind,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let yielding = DetTid::from_raw(1);
            let peer = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 0, 1.0, None);

            queue.push_back(yielding, DEFAULT_PRIORITY - 1);
            assert_eq!(queue.tentative_pop_next(), Some(yielding));
            assert_eq!(queue.commit_tentative_pop(), yielding);

            queue.push_yielded(yielding, DEFAULT_PRIORITY - 1);
            queue.push_back(peer, LAST_PRIORITY);
            assert_eq!(queue.tentative_pop_next(), Some(peer), "{strategy:?}");
            assert_eq!(
                queue.commit_tentative_pop_completed_turn(),
                peer,
                "{strategy:?}"
            );

            assert_eq!(queue.yielded_skip, None, "{strategy:?}");
            let restored_priority = queue
                .queue
                .iter()
                .find(|(_key, value)| value.tid == yielding)
                .map(|(key, _value)| key.priority);
            assert_eq!(
                restored_priority,
                Some(DEFAULT_PRIORITY - 1),
                "{strategy:?}"
            );

            if matches!(strategy, SchedHeuristic::None | SchedHeuristic::ConnectBind) {
                queue.push_back(peer, LAST_PRIORITY);
                assert_eq!(queue.tentative_pop_next(), Some(yielding), "{strategy:?}");
            }
        }
    }

    #[test]
    fn exact_selection_bypasses_priority_and_heuristic() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::ConnectBind,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let higher_priority = DetTid::from_raw(1);
            let selected = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 0, 1.0, None);
            queue.push_back(higher_priority, FIRST_PRIORITY);
            queue.push_back(selected, LAST_PRIORITY);

            assert_eq!(queue.tentative_pop_tid(selected), Some(selected));
            assert_eq!(queue.commit_tentative_pop(), selected);
            assert!(queue.contains_tid(higher_priority));
            assert!(!queue.contains_tid(selected));
        }
    }

    #[test]
    fn scheduler_only_commit_does_not_consume_yield_exclusion() {
        let yielding = DetTid::from_raw(1);
        let peer = DetTid::from_raw(2);
        let mut queue = RunQueue::default();

        queue.push_back(yielding, DEFAULT_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(yielding));
        assert_eq!(queue.commit_tentative_pop(), yielding);

        queue.push_yielded(yielding, DEFAULT_PRIORITY);
        queue.push_back(peer, DEFAULT_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(peer));
        assert_eq!(queue.commit_tentative_pop(), peer);

        queue.push_back(peer, DEFAULT_PRIORITY);
        assert_eq!(queue.yielded_skip, Some(yielding));
        assert_eq!(queue.tentative_pop_next(), Some(peer));
        assert_eq!(queue.commit_tentative_pop_completed_turn(), peer);
        assert_eq!(queue.yielded_skip, None);
    }

    /// The overlay must be entirely inert when disabled: no `fair` state exists
    /// and selection is exactly the legacy priority/FIFO order.
    #[test]
    fn overlay_disabled_is_exact_legacy_behavior() {
        let a = DetTid::from_raw(1);
        let b = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, None);
        assert!(queue.fair.is_none());
        // A monopolist that always re-inserts at the front would run forever
        // without the overlay; confirm that is exactly what happens with None.
        queue.push_back(a, DEFAULT_PRIORITY);
        queue.push_back(b, DEFAULT_PRIORITY);
        for _ in 0..10 {
            assert_eq!(queue.tentative_pop_next(), Some(a));
            assert_eq!(queue.commit_tentative_pop(), a);
            queue.push_front(a, DEFAULT_PRIORITY);
        }
        // b never ran.
        assert!(queue.contains_tid(b));
    }

    /// A thread that keeps winning selection (here, by re-inserting at the front
    /// every turn) burns its service lead and, once it is `budget` turns ahead of
    /// the floor, becomes ineligible so the starved peer is forced to run. This
    /// is the missing burn-out mechanism (H8) demonstrated in miniature.
    #[test]
    fn hot_thread_self_deprioritizes_within_budget() {
        let budget = 3u64;
        let hot = DetTid::from_raw(1);
        let peer = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, Some(budget));

        queue.push_back(hot, DEFAULT_PRIORITY);
        queue.push_back(peer, DEFAULT_PRIORITY);

        // The hot thread can win at most `budget` turns before its lead over the
        // (never-charged) peer reaches the budget and it is excluded.
        let mut hot_turns = 0;
        for _ in 0..budget {
            let sel = queue.tentative_pop_next().unwrap();
            assert_eq!(sel, hot, "hot thread should keep winning while eligible");
            assert_eq!(queue.commit_tentative_pop(), hot);
            hot_turns += 1;
            // Re-insert at the front: absent fairness this would monopolize.
            queue.push_front(hot, DEFAULT_PRIORITY);
        }
        assert_eq!(hot_turns, budget);

        // Now hot's lead == budget => ineligible. The peer must be selected even
        // though hot sits at the front of the queue.
        assert_eq!(
            queue.tentative_pop_next(),
            Some(peer),
            "starved peer must run once the hot thread exhausts its budget"
        );
    }

    /// A rolled-back tentative selection must not be charged: `undo_tentative_pop`
    /// bypasses `on_commit`, so eligibility is unchanged afterward.
    #[test]
    fn undo_tentative_pop_does_not_charge() {
        let budget = 2u64;
        let a = DetTid::from_raw(1);
        let b = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, Some(budget));
        queue.push_back(a, DEFAULT_PRIORITY);
        queue.push_back(b, DEFAULT_PRIORITY);

        // Peek-and-undo many times: no charge accrues, so `a` stays eligible and
        // keeps being the FIFO-first selection every time.
        for _ in 0..10 {
            assert_eq!(queue.tentative_pop_next(), Some(a));
            queue.undo_tentative_pop();
        }
        let fair = queue.fair.as_ref().unwrap();
        assert_eq!(fair.service.get(&a).copied(), Some(0));
        assert_eq!(fair.service.get(&b).copied(), Some(0));
    }

    /// An internal I/O poll-retry requeue commits through the UNCHARGED path, so
    /// it never advances the service counter. The count of such retries is
    /// host-timing nondeterministic, so charging them would make fairness (and
    /// therefore `--verify`) host-dependent (#140). A thread that only ever
    /// poll-retries keeps service 0 and never becomes ineligible from polling.
    #[test]
    fn poll_retry_commit_is_uncharged() {
        let budget = 2u64;
        let poller = DetTid::from_raw(1);
        let peer = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, Some(budget));
        queue.push_back(poller, DEFAULT_PRIORITY);
        queue.push_back(peer, DEFAULT_PRIORITY);

        // The poller wins and "poll-retries" many times via the uncharged commit.
        // Absent the exemption, `budget` charged turns would exclude it; with the
        // exemption its service stays 0 forever and it remains eligible.
        for _ in 0..50 {
            assert_eq!(queue.tentative_pop_next(), Some(poller));
            assert_eq!(queue.commit_tentative_pop_uncharged(), poller);
            queue.push_front(poller, DEFAULT_PRIORITY);
        }
        let fair = queue.fair.as_ref().unwrap();
        assert_eq!(
            fair.service.get(&poller).copied(),
            Some(0),
            "uncharged poll-retry commits must not advance the service counter"
        );
        assert_eq!(fair.service.get(&peer).copied(), Some(0));
    }

    /// A blocked thread's service counter is retained across a block/wake round
    /// trip, and a short sleeper cannot bank unbounded credit: on wake its
    /// counter is clamped to at most one slice below the runnable floor.
    #[test]
    fn wake_clamp_bounds_sleeper_credit() {
        let budget = 100u64;
        let sleeper = DetTid::from_raw(1);
        let worker = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, Some(budget));

        queue.push_back(sleeper, DEFAULT_PRIORITY);
        queue.push_back(worker, DEFAULT_PRIORITY);

        // Sleeper takes one turn (charged to service 1), is requeued, then
        // blocks (leaves the runnable set via remove_tid).
        assert_eq!(queue.tentative_pop_next(), Some(sleeper));
        assert_eq!(queue.commit_tentative_pop(), sleeper);
        queue.push_back(sleeper, DEFAULT_PRIORITY);
        assert!(queue.remove_tid(sleeper));

        // Worker runs many turns, advancing the floor far ahead.
        for _ in 0..20 {
            assert_eq!(queue.tentative_pop_next(), Some(worker));
            assert_eq!(queue.commit_tentative_pop(), worker);
            queue.push_front(worker, DEFAULT_PRIORITY);
        }

        // Sleeper wakes: its stale counter (1) is clamped up to floor-credit so
        // it cannot preempt the worker for more than a bounded burst.
        queue.push_back(sleeper, DEFAULT_PRIORITY);
        let fair = queue.fair.as_ref().unwrap();
        let floor_before = *fair.service.get(&worker).unwrap();
        let sleeper_service = *fair.service.get(&sleeper).unwrap();
        assert!(
            sleeper_service + FAIR_WAKE_CREDIT >= floor_before,
            "woken sleeper credit must be clamped to <= one slice below floor \
             (sleeper={sleeper_service}, floor={floor_before})"
        );
    }

    /// A brand-new thread joins at the runnable floor (neutral), neither given
    /// minimum-service preference nor penalized, so it competes fairly at once.
    #[test]
    fn new_thread_admitted_at_floor() {
        let budget = 100u64;
        let old = DetTid::from_raw(1);
        let fresh = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 1.0, Some(budget));

        queue.push_back(old, DEFAULT_PRIORITY);
        for _ in 0..5 {
            assert_eq!(queue.tentative_pop_next(), Some(old));
            assert_eq!(queue.commit_tentative_pop(), old);
            queue.push_front(old, DEFAULT_PRIORITY);
        }
        // Now `old` has service 5. A newcomer joins.
        queue.push_back(fresh, DEFAULT_PRIORITY);
        let fair = queue.fair.as_ref().unwrap();
        let floor = fair.floor(&queue.queue);
        assert_eq!(
            fair.service.get(&fresh).copied(),
            Some(floor),
            "new thread must be admitted at the current floor"
        );
    }
}
