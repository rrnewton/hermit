/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! How a schedule replay (`--replay-schedule-from`) applies the run options
//! that change thread order, slice ends, timed waits or virtual time: in the
//! part of the run its schedule covers, and past the end of that schedule.
//!
//! [`TABLE`] records what the code does today, option by option, with the
//! functions that do it. Only one row is read by code: Detcore's scheduler
//! reads the `--chaos-timed-wait-spread` row through
//! [`ReplayOption::applies`]. Every other row documents behaviour. The user
//! guide shows the table as [`markdown_table`] renders it, and a test keeps
//! the two equal.
//!
//! The rule the table measures against: the replayed part reproduces the
//! recording, by applying the option again or by following the schedule
//! that already carries its effect; past the schedule no new chaos decision
//! is made, and a semantic option (a rule rather than a draw) still
//! applies. A row whose behaviour differs from the rule is marked
//! [`Status::KnownGap`]; changing it is a separate change.
//!
//! "Past the schedule" begins at the consumption of the schedule's last
//! record. A legacy replay then goes on unguided. A slice-boundary replay
//! (`--strict-schedule-boundaries`) refuses the next event it observes, so
//! for it the interval is short: what the replayed thread does between
//! consuming its last record and its next event, such as filing the timed
//! wait of the syscall whose prehook was that record.
//!
//! Not classified here: the seeds (`--seed`, `--sched-seed`, `--fuzz-seed`
//! act through the decisions they seed, listed below; `--rng-seed` is the
//! guest's random data, not a scheduling option), `--imprecise-timers` (the
//! precision of the PMU timer, meant for recording),
//! `--no-sequentialize-threads` (no schedule to replay), and the replay
//! controls themselves (`--replay-preemptions-from`, another kind of
//! replay; `--strict-schedule-boundaries`, the format).

/// A run option, or a group of options that act together, as a schedule
/// replay applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayOption {
    /// `--chaos`.
    Chaos,
    /// `--chaos-timed-wait-spread`.
    ChaosTimedWaitSpread,
    /// `--chaos-per-thread-slowdown`, `--chaos-slowdown-max-factor`,
    /// `--chaos-epoch-length-ns`.
    ChaosPerThreadSlowdown,
    /// `--chaos-target-races`.
    ChaosTargetRaces,
    /// `--fuzz-futexes`.
    FuzzFutexes,
    /// `--sched-heuristic`, `--sched-sticky-random-param`.
    SchedHeuristic,
    /// `--runs-post-fork`.
    RunsPostFork,
    /// `--target-timeslice`, `--target-timeslice-syscalls-only`.
    TargetTimeslice,
    /// `--max-timeslice` (and its alias `--preemption-timeout`).
    MaxTimeslice,
    /// `--no-rcb-time`.
    NoRcbTime,
    /// `--clock-multiplier`.
    ClockMultiplier,
    /// `--scheduler-turn-cost`.
    SchedulerTurnCost,
    /// `--epoch`.
    Epoch,
    /// `--sleep-registration`.
    SleepRegistration,
    /// `--pipe-wake-latency`.
    PipeWakeLatency,
    /// `--futex-wake-yields`.
    FutexWakeYields,
}

/// What kind of option it is, which decides what the rule asks of it past
/// the schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A decision drawn from a seed: past the schedule the rule asks for
    /// none.
    ChaosDecision,
    /// A rule of the scheduler or of virtual time: past the schedule the
    /// rule asks that it still apply.
    Semantic,
}

/// Where in a schedule replay an option takes effect today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// The replay applies the option.
    Yes,
    /// The replay follows the schedule, which carries the option's effect
    /// (its slice ends and switches), and does not apply the option itself.
    FromTheSchedule,
    /// Only a replay given `--chaos` applies the option.
    OnlyUnderChaos,
    /// The replay does not apply the option.
    No,
}

impl Applies {
    fn text(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::FromTheSchedule => "from the schedule",
            Self::OnlyUnderChaos => "only under `--chaos`",
            Self::No => "no",
        }
    }
}

/// Whether today's behaviour follows the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    FollowsRule,
    /// Today's behaviour differs from the rule, as the row's reason says.
    KnownGap,
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Self::ChaosDecision => "chaos decision",
            Self::Semantic => "semantic",
        }
    }
}

/// One option's classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub option: ReplayOption,
    /// The `hermit run` flags, comma-separated.
    pub flags: &'static str,
    pub kind: Kind,
    /// In the part the schedule covers.
    pub replayed: Applies,
    /// Past the end of the schedule.
    pub past: Applies,
    pub status: Status,
    /// Whether code reads this row, rather than the row documenting what
    /// code does. Set by hand; the test
    /// `read_by_code_names_exactly_the_rows_code_reads` checks it against
    /// every caller of [`ReplayOption::applies`] and [`row`] in the Hermit
    /// crates.
    pub read_by_code: bool,
    /// What happens and why, with the gap if there is one.
    pub why: &'static str,
    /// The functions that do it.
    pub code: &'static str,
}

impl Row {
    /// Whether the replayed part satisfies the rule: the option is applied
    /// again, or the schedule carries its effect.
    pub fn replayed_follows_rule(&self) -> bool {
        matches!(self.replayed, Applies::Yes | Applies::FromTheSchedule)
    }

    /// Whether the part past the schedule satisfies the rule.
    pub fn past_follows_rule(&self) -> bool {
        match self.kind {
            Kind::ChaosDecision => self.past == Applies::No,
            Kind::Semantic => self.past == Applies::Yes,
        }
    }
}

/// The classification, in the order the user guide lists it.
pub const TABLE: &[Row] = &[
    Row {
        option: ReplayOption::Chaos,
        flags: "--chaos",
        kind: Kind::ChaosDecision,
        replayed: Applies::FromTheSchedule,
        past: Applies::OnlyUnderChaos,
        status: Status::KnownGap,
        read_by_code: false,
        why: "Random slice ends and priorities. The replayed part follows the schedule's \
              recorded switches; a replay given `--chaos` also keeps drawing slice ends and \
              priorities, during the schedule and after it (gap). A slice-boundary replay \
              refuses `--chaos`.",
        code: "tool_local.rs ThreadState::next_timeslice (slice end draw); lib.rs \
               Detcore::end_timeslice_with_checkpoint (random priority change point); \
               scheduler.rs validate_strict_boundary_config",
    },
    Row {
        option: ReplayOption::ChaosTimedWaitSpread,
        flags: "--chaos-timed-wait-spread",
        kind: Kind::ChaosDecision,
        replayed: Applies::Yes,
        past: Applies::No,
        status: Status::FollowsRule,
        read_by_code: true,
        why: "A seeded draw of each timed wait's wake time. The replay draws again from the \
              recorded spread and seed, and refuses others; past the schedule a wait is \
              queued at its deadline.",
        code: "scheduler.rs Scheduler::file_timed_wait; preemptions.rs \
               timed_wait_spread_mismatch",
    },
    Row {
        option: ReplayOption::ChaosPerThreadSlowdown,
        flags: "--chaos-per-thread-slowdown, --chaos-slowdown-max-factor, \
                --chaos-epoch-length-ns",
        kind: Kind::ChaosDecision,
        replayed: Applies::OnlyUnderChaos,
        past: Applies::OnlyUnderChaos,
        status: Status::KnownGap,
        read_by_code: false,
        why: "A seeded factor that scales each thread's virtual time per RCB, drawn only when \
              the replay has `--chaos`; the recorded factors are read only by \
              `--replay-preemptions-from`. A replay without `--chaos` does not reproduce a \
              recording made with it, and one with `--chaos` keeps drawing past the schedule \
              (gaps).",
        code: "tool_local.rs ThreadState::next_timeslice",
    },
    Row {
        option: ReplayOption::ChaosTargetRaces,
        flags: "--chaos-target-races",
        kind: Kind::ChaosDecision,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::KnownGap,
        read_by_code: false,
        why: "Draws a process-directed signal's target thread and a woken thread's queue \
              position from the fuzz seed whenever given, during the schedule and after it \
              (gap). Its child priorities are not drawn in a schedule replay, whose children \
              take the replay priorities.",
        code: "scheduler.rs Scheduler::chaos_signal_target, \
               Scheduler::force_unblock_thread_at; tool_global.rs create_child_thread (after \
               its schedule replay branch)",
    },
    Row {
        option: ReplayOption::FuzzFutexes,
        flags: "--fuzz-futexes",
        kind: Kind::ChaosDecision,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::KnownGap,
        read_by_code: false,
        why: "Draws which futex waiters a wake takes, from the fuzz seed, during the schedule \
              and after it (gap).",
        code: "scheduler.rs Scheduler::take_futex_wakees, \
               Scheduler::choose_fuzzed_futex_wakees",
    },
    Row {
        option: ReplayOption::SchedHeuristic,
        flags: "--sched-heuristic, --sched-sticky-random-param",
        kind: Kind::ChaosDecision,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::KnownGap,
        read_by_code: false,
        why: "`random` and `stickyrandom` draw the next thread from the scheduler seed at \
              every turn, during the schedule and after it (gap); `connectbind` and `none` \
              draw nothing.",
        code: "runqueue.rs RunQueue::tentative_pop_next",
    },
    Row {
        option: ReplayOption::RunsPostFork,
        flags: "--runs-post-fork",
        kind: Kind::ChaosDecision,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::KnownGap,
        read_by_code: false,
        why: "`random` draws whether a new thread runs before its parent at every clone, \
              during the schedule and after it (gap); `parent` and `child` draw nothing.",
        code: "tool_global.rs recv_create_child_thread; scheduler.rs \
               Scheduler::child_runs_first_post_fork",
    },
    Row {
        option: ReplayOption::TargetTimeslice,
        flags: "--target-timeslice, --target-timeslice-syscalls-only",
        kind: Kind::Semantic,
        replayed: Applies::FromTheSchedule,
        past: Applies::OnlyUnderChaos,
        status: Status::KnownGap,
        read_by_code: false,
        why: "A replay ignores `--target-timeslice-syscalls-only`, and one without `--chaos` \
              ignores `--target-timeslice` too, during the schedule and after it: its slice \
              end stays the replay's (gap past the schedule). A replay given `--chaos` uses the \
              target as the mean of its random slice ends.",
        code: "tool_local.rs ThreadState::next_timeslice and \
               ThreadState::instruction_trap_ends_timeslice",
    },
    Row {
        option: ReplayOption::MaxTimeslice,
        flags: "--max-timeslice, --preemption-timeout",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::KnownGap,
        read_by_code: false,
        why: "The fallback PMU stop of every slice in both parts: a replay programs the PMU \
              at the slice's selected end when that end is finite, which takes precedence and \
              may lie beyond the configured maximum (a replay given `--chaos` draws such ends), \
              and at the configured maximum when the selected end is the maximum time; it stops \
              a thread that makes no syscalls. Past the schedule a replay without `--chaos` \
              keeps its software slice end at the maximum time, so only this PMU stop on the \
              thread's own execution ends a slice, not the plain run's end on logical time, \
              which counts expired waits (gap). Under `--no-rcb-time` it also sets the software \
              slice end.",
        code: "tool_local.rs ThreadState::next_timeslice",
    },
    Row {
        option: ReplayOption::NoRcbTime,
        flags: "--no-rcb-time",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "Virtual time without branch counts: slice ends at the maximum timeslice of \
              logical time, in both parts.",
        code: "tool_local.rs ThreadState::next_timeslice; lib.rs Detcore::handle_timer_event",
    },
    Row {
        option: ReplayOption::ClockMultiplier,
        flags: "--clock-multiplier",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "Scales virtual time per RCB and per scheduler turn, in both parts.",
        code: "detcore-model time.rs DetTime::new; config.rs \
               Config::scheduler_turn_charge; tool_local.rs ThreadState::next_timeslice",
    },
    Row {
        option: ReplayOption::SchedulerTurnCost,
        flags: "--scheduler-turn-cost",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "The virtual time each committed scheduler turn adds, in both parts.",
        code: "config.rs Config::scheduler_turn_charge; detcore-model time.rs \
               GlobalTime::add_scheduler_time",
    },
    Row {
        option: ReplayOption::Epoch,
        flags: "--epoch",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "Where virtual time starts. The recording stores it, and its slice ends are \
              absolute virtual times.",
        code: "detcore-model time.rs GlobalTime::new; scheduler.rs Scheduler::new (stamps \
               the record)",
    },
    Row {
        option: ReplayOption::SleepRegistration,
        flags: "--sleep-registration",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "When a queued sleep becomes a timed waiter. A replay that leaves it unset takes \
              the recording's value, and another value is refused.",
        code: "config.rs Config::queued_sleeps_registered_at_first_pass; scheduler.rs \
               Scheduler::new",
    },
    Row {
        option: ReplayOption::PipeWakeLatency,
        flags: "--pipe-wake-latency",
        kind: Kind::Semantic,
        replayed: Applies::FromTheSchedule,
        past: Applies::No,
        status: Status::KnownGap,
        read_by_code: false,
        why: "When a pipe write readies its reader. A schedule replay makes the hand-off \
              inert in both parts: while records remain the schedule reproduces the recorded \
              switch to the reader, and past the schedule nothing hands off (gap). The \
              recording stores the value, and another value is refused.",
        code: "scheduler/pipe_wake.rs Scheduler::ready_channel; config.rs \
               Config::pipe_wake_latency_in_effect",
    },
    Row {
        option: ReplayOption::FutexWakeYields,
        flags: "--futex-wake-yields",
        kind: Kind::Semantic,
        replayed: Applies::Yes,
        past: Applies::Yes,
        status: Status::FollowsRule,
        read_by_code: false,
        why: "A thread whose futex wake woke a waiter yields its turn. The record holds no \
              queue order, so the replay yields again, in both parts.",
        code: "config.rs Config::yields_after_futex_wake; lib.rs \
               Detcore::post_handler_hook_observed",
    },
];

/// The row of `option`.
pub fn row(option: ReplayOption) -> &'static Row {
    TABLE
        .iter()
        .find(|row| row.option == option)
        .expect("every ReplayOption has a row in TABLE")
}

impl ReplayOption {
    /// Whether a schedule replay, with or without `--chaos`, applies this
    /// option at a point of the run: in the part its schedule covers, or
    /// once the schedule has run out (`past_the_schedule`). An option whose
    /// effect the schedule carries is not applied again.
    pub fn applies(self, past_the_schedule: bool, chaos: bool) -> bool {
        let row = row(self);
        match if past_the_schedule {
            row.past
        } else {
            row.replayed
        } {
            Applies::Yes => true,
            Applies::OnlyUnderChaos => chaos,
            Applies::FromTheSchedule | Applies::No => false,
        }
    }
}

/// The first line of the table's block in docs/USER_GUIDE.md.
pub const GUIDE_BEGIN: &str = "<!-- BEGIN schedule replay options: rendered from detcore-model's schedule_replay_options::TABLE -->";
/// The last line of that block.
pub const GUIDE_END: &str = "<!-- END schedule replay options -->";

/// [`TABLE`] as the user guide shows it, without the marker lines.
pub fn markdown_table() -> String {
    let mut out = String::from(
        "| Option | Kind | Replayed part | Past the schedule | Status | Read by code | Why | Code |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for row in TABLE {
        let flags: Vec<String> = row.flags.split(", ").map(|f| format!("`{f}`")).collect();
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            flags.join(", "),
            row.kind.text(),
            row.replayed.text(),
            row.past.text(),
            match row.status {
                Status::FollowsRule => "follows the rule",
                Status::KnownGap => "known gap",
            },
            if row.read_by_code { "yes" } else { "no" },
            row.why,
            row.code
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row, written out independently of [`TABLE`]: a row changed in
    /// the table, or in the guide with it, fails here until this list says
    /// the same. (option, kind, replayed part, past the schedule, status,
    /// read by code)
    #[rustfmt::skip]
    const EXPECTED: &[(ReplayOption, Kind, Applies, Applies, Status, bool)] = {
        use Applies::*;
        use Kind::*;
        use ReplayOption::*;
        use Status::*;
        &[
            (Chaos, ChaosDecision, FromTheSchedule, OnlyUnderChaos, KnownGap, false),
            (ChaosTimedWaitSpread, ChaosDecision, Yes, No, FollowsRule, true),
            (ChaosPerThreadSlowdown, ChaosDecision, OnlyUnderChaos, OnlyUnderChaos, KnownGap, false),
            (ChaosTargetRaces, ChaosDecision, Yes, Yes, KnownGap, false),
            (FuzzFutexes, ChaosDecision, Yes, Yes, KnownGap, false),
            (SchedHeuristic, ChaosDecision, Yes, Yes, KnownGap, false),
            (RunsPostFork, ChaosDecision, Yes, Yes, KnownGap, false),
            (TargetTimeslice, Semantic, FromTheSchedule, OnlyUnderChaos, KnownGap, false),
            (MaxTimeslice, Semantic, Yes, Yes, KnownGap, false),
            (NoRcbTime, Semantic, Yes, Yes, FollowsRule, false),
            (ClockMultiplier, Semantic, Yes, Yes, FollowsRule, false),
            (SchedulerTurnCost, Semantic, Yes, Yes, FollowsRule, false),
            (Epoch, Semantic, Yes, Yes, FollowsRule, false),
            (SleepRegistration, Semantic, Yes, Yes, FollowsRule, false),
            (PipeWakeLatency, Semantic, FromTheSchedule, No, KnownGap, false),
            (FutexWakeYields, Semantic, Yes, Yes, FollowsRule, false),
        ]
    };

    #[test]
    fn the_table_says_what_the_expected_list_says() {
        let actual: Vec<_> = TABLE
            .iter()
            .map(|r| {
                (
                    r.option,
                    r.kind,
                    r.replayed,
                    r.past,
                    r.status,
                    r.read_by_code,
                )
            })
            .collect();
        assert_eq!(actual, EXPECTED);
    }

    /// Each option has one row with flags and a reason that render in a
    /// table cell; a row whose behaviour contradicts the rule is marked a
    /// known gap; only the spread row is read by code; and `applies` reads
    /// the row: the spread applies in the replayed part and not past it,
    /// with or without `--chaos`.
    #[test]
    fn rows_are_consistent_and_only_the_spread_row_is_read() {
        for row in TABLE {
            assert_eq!(TABLE.iter().filter(|r| r.option == row.option).count(), 1);
            for flag in row.flags.split(", ") {
                assert!(flag.starts_with("--") && !flag.contains(' '), "{flag}");
            }
            for text in [row.why, row.code] {
                assert!(!text.contains('|') && !text.contains('\n'), "{text}");
            }
            if !row.replayed_follows_rule() || !row.past_follows_rule() {
                assert_eq!(row.status, Status::KnownGap, "{:?}", row.option);
            }
        }
        let read: Vec<_> = TABLE
            .iter()
            .filter(|r| r.read_by_code)
            .map(|r| r.option)
            .collect();
        assert_eq!(read, [ReplayOption::ChaosTimedWaitSpread]);
        for chaos in [false, true] {
            assert!(ReplayOption::ChaosTimedWaitSpread.applies(false, chaos));
            assert!(!ReplayOption::ChaosTimedWaitSpread.applies(true, chaos));
        }
        assert!(!ReplayOption::ChaosPerThreadSlowdown.applies(false, false));
        assert!(ReplayOption::ChaosPerThreadSlowdown.applies(true, true));
        assert!(!ReplayOption::TargetTimeslice.applies(false, true));
    }

    /// `read_by_code` is set by hand, so this finds the rows code reads:
    /// every `ReplayOption::<Variant>.applies(` and `row(ReplayOption::<Variant>)`
    /// in the sources of detcore, detcore-model and hermit-cli, with
    /// whitespace removed. A new reader whose row does not say so fails
    /// here, and so does an `.applies(` call this scan cannot attribute to
    /// one variant.
    #[test]
    fn read_by_code_names_exactly_the_rows_code_reads() {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        fn is_ident(c: char) -> bool {
            c.is_alphanumeric() || c == '_'
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for krate in ["detcore/src", "detcore-model/src", "hermit-cli/src"] {
            sources(&root.join(krate), &mut files);
        }
        let mut read = std::collections::BTreeSet::new();
        for file in files
            .iter()
            .filter(|f| !f.ends_with("schedule_replay_options.rs"))
        {
            let text: String = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()))
                .split_whitespace()
                .collect();
            for (at, _) in text.match_indices(".applies(") {
                let before = &text[..at];
                let ident = before.trim_end_matches(is_ident);
                assert!(
                    ident.ends_with("ReplayOption::"),
                    "{}: an `.applies(` call on something other than a ReplayOption \
                     variant; mark the rows it reads read_by_code and extend this test",
                    file.display()
                );
                read.insert(before[ident.len()..].to_string());
            }
            for (at, marker) in text.match_indices("row(ReplayOption::") {
                let rest = &text[at + marker.len()..];
                read.insert(rest.chars().take_while(|c| is_ident(*c)).collect());
            }
        }
        let marked: std::collections::BTreeSet<String> = TABLE
            .iter()
            .filter(|row| row.read_by_code)
            .map(|row| format!("{:?}", row.option))
            .collect();
        assert_eq!(read, marked);
    }

    /// docs/USER_GUIDE.md shows exactly [`markdown_table`] between its
    /// markers, so the guide cannot drift from the table the scheduler reads.
    #[test]
    fn the_user_guide_shows_the_table_the_scheduler_reads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/USER_GUIDE.md");
        let guide = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let shown = guide
            .split_once(&format!("{GUIDE_BEGIN}\n"))
            .and_then(|(_, rest)| rest.split_once(GUIDE_END))
            .map(|(block, _)| block)
            .unwrap_or_else(|| panic!("{} has no {GUIDE_BEGIN} block", path.display()));
        assert_eq!(
            shown,
            markdown_table(),
            "the schedule replay options table in {} differs from \
             schedule_replay_options::TABLE; replace the block with:\n{GUIDE_BEGIN}\n{}{GUIDE_END}",
            path.display(),
            markdown_table()
        );
    }
}
