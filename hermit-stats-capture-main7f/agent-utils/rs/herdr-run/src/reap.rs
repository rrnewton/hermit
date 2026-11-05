//! Reaping policy for stale herdr tabs.
//!
//! Agents are coined and destroyed continuously, and every one that ever ran a command through
//! `herdr-run` leaves a tab behind. This module decides which of those tabs are *provably* finished
//! with, so they can be closed, and -- much more importantly -- which ones must be left alone.
//!
//! # Why the obvious signals do not work
//!
//! Herdr exposes no tab creation time and no last-activity time. `PaneInfo.revision` looks like an
//! activity counter and is not: it counts pane *metadata* revisions and stays put while the
//! terminal produces visible output. So there is no "last used" field to threshold on.
//!
//! Worse, the tempting substitute -- "no output for N minutes" -- cannot work even in principle
//! here. A *finished* agent and a *dead* agent are identical on the silence axis. Any
//! absence-of-output detector conflates them, and the expensive direction of that mistake is
//! killing an agent that is merely thinking.
//!
//! # Why the run-directory PID is the wrong anchor (measured, not assumed)
//!
//! The run spool is `.herdr-run/runs/<utc>-<agent>-<pid>/`, and it is tempting to read that
//! trailing PID as "the process that owns this tab" and treat its death as evidence of staleness.
//! **That PID is the short-lived `herdr-run` CLI invocation, not the agent and not the pane.** It
//! exits the moment the command finishes, so it is dead by construction for every completed run.
//!
//! Measured on the live spool (15 run directories, 2026-08-06): the run-directory PID was dead for
//! **15 of 15**, while the pane shell recorded in the same `meta.json` was alive for **15 of 15**.
//! A policy anchored on the run-directory PID would therefore have classified every tab whose
//! commands had all finished -- which is every healthy idle agent -- as stale.
//!
//! The correct anchor is the **pane's shell process**, which is what actually owns the tab.
//!
//! # The predicate
//!
//! A tab is STALE only when all of these hold, each positive evidence rather than absence:
//!
//! * **R1 no in-flight work.** Every run naming this pane recorded an `exit_code`. A run with no
//!   recorded exit code is IN FLIGHT -- the "agent is thinking" case, positively distinguishable
//!   from a finished one rather than inferred from silence. The state is written by exactly one
//!   place: [`crate::runner::execute`] records the run with a null exit code when collection times
//!   out, because the command is then still running in a pane this process does not own. That is
//!   the whole reason the rule exists, and until the timeout path recorded anything the rule was
//!   unreachable -- documented as a protection while no writer could produce the state it tests
//!   for. [`crate::sweep::load_run_records`] re-reads the run's `exit_code` file afterwards, so a
//!   command that finished after we stopped waiting stops looking in flight.
//! * **R2 the pane's shell is gone.** Not the run-directory PID; the shell PID.
//! * **R3 reboot and PID reuse are excluded.** `kill(pid, 0)` alone is not sufficient on a busy
//!   host. Identity is bound as `(pid, boot_id, start_ticks)` -- field 22 of `/proc/<pid>/stat` --
//!   and the recorded boot must match the current boot before absence of the old PID can authorize
//!   anything.
//!
//! Anything else -- unreadable `/proc`, missing `meta.json`, absent `pane_id`, a boot-id mismatch,
//! a workspace herdr could not list -- is UNKNOWN, and unknown is never reaped. The cost asymmetry
//! is the whole reason: killing an agent mid-work is far worse than clutter, and a reaper that is
//! wrong once in the expensive direction gets switched off permanently.
//!
//! # GONE: a record whose tab is already closed
//!
//! A run record outlives its tab: a tab closed by hand keeps its records until retention prunes
//! them. When herdr answered with a listing of the workspace and the recorded pane is not in it,
//! the tab is GONE. There is nothing to close, and it holds none of `max_panes`, because the cap
//! counts herdr's live pane list rather than run records. GONE needs a listing that succeeded: a
//! listing that failed is UNKNOWN, since "herdr did not answer" must never read as "every pane is
//! gone". Measured 2026-10-02 on devbig030: 15 of the 32 records `reap` reported were such closed
//! tabs, reported as UNKNOWN, and read as capacity nobody could reclaim while the cap was in fact
//! held by live panes with no surviving record.
//!
//! # Scope is enforced, not trusted
//!
//! Only panes belonging to tabs in *our* workspace, whose labels match the configured tab schema,
//! are even considered. A tab a human opened by hand, or another tool's workspace, is not a
//! candidate no matter how idle it looks.
//!
//! # Report-only by default
//!
//! [`plan_reap`] decides; it never closes anything. Closing is a separate, explicit step. A reaper
//! that is trusted before it has been checked against a known-good population is how the whole
//! workspace gets deleted once.
//!
//! This module holds no I/O. Evidence is passed in (see [`crate::sweep`]), so the policy is
//! testable without a live herdr and without waiting for real processes to die -- and so the
//! planted-stale and planted-live cases can both be exercised deterministically.

use std::collections::BTreeMap;

use crate::textfmt::{quote_optional, strip_space};

/// Why a pane was or was not selected. Rendered as strings so they survive into JSON and logs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Verdict {
    /// Provably finished with: closable.
    Stale,
    /// A run for this pane has no recorded exit code -- the agent may be thinking.
    InFlight,
    /// The recorded pane shell is still the original process.
    ShellAlive,
    /// Herdr's listing of the workspace does not include the pane: the tab is already closed.
    Gone,
    /// We could not tell. Never reaped.
    Unknown,
    /// Not one of ours.
    OutOfScope,
}

impl Verdict {
    /// Stable wire name. Fixed, because it is what reaches JSON output and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Stale => "STALE",
            Verdict::InFlight => "IN_FLIGHT",
            Verdict::ShellAlive => "SHELL_ALIVE",
            Verdict::Gone => "GONE",
            Verdict::Unknown => "UNKNOWN",
            Verdict::OutOfScope => "OUT_OF_SCOPE",
        }
    }

    /// Every verdict, so a report can print the zeros as well as the hits.
    #[must_use]
    pub fn all() -> [Verdict; 6] {
        [
            Verdict::Stale,
            Verdict::InFlight,
            Verdict::ShellAlive,
            Verdict::Gone,
            Verdict::Unknown,
            Verdict::OutOfScope,
        ]
    }
}

/// A PID bound to the boot and start tick that make it *that* process.
///
/// `boot_id` and `start_ticks` are what stop a recycled PID from being mistaken for the original.
/// Either being `None` means we could not establish identity, which is UNKNOWN -- never a licence
/// to reap.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessIdentity {
    /// Process id as recorded or observed.
    pub pid: Option<i64>,
    /// Boot the pid belongs to.
    pub boot_id: Option<String>,
    /// Start time in clock ticks since boot.
    pub start_ticks: Option<u64>,
}

impl ProcessIdentity {
    /// True when pid, `boot_id` and `start_ticks` are all known, so reuse can be excluded.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.pid
            .is_some_and(|pid| (1..=2_147_483_647).contains(&pid))
            && self
                .boot_id
                .as_deref()
                .is_some_and(|boot| !strip_space(boot).is_empty())
            && self.start_ticks.is_some()
    }

    /// True only if both identities are bound AND agree on pid, boot and start tick.
    #[must_use]
    pub fn same_process_as(&self, other: &ProcessIdentity) -> bool {
        self.is_bound()
            && other.is_bound()
            && self.pid == other.pid
            && self.boot_id == other.boot_id
            && self.start_ticks == other.start_ticks
    }
}

/// Everything the policy is allowed to look at for one pane.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaneEvidence {
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Herdr tab identifier, when known.
    pub tab_id: Option<String>,
    /// Tab label, when known.
    pub tab_label: Option<String>,
    /// Workspace label the pane was recorded under.
    pub workspace_label: Option<String>,
    /// Whether this pane belongs to a tab this tool is authorised over.
    pub in_scope: bool,
    /// One entry per run naming this pane; `true` means an exit code was recorded.
    pub run_exit_codes_recorded: Vec<bool>,
    /// Identity captured in `meta.json` when the run happened.
    pub recorded_shell: Option<ProcessIdentity>,
    /// Identity of the pane's shell right now, or `None` when the shell is gone.
    pub live_shell: Option<ProcessIdentity>,
    /// Current kernel boot id, gathered independently of the recorded pid.
    pub current_boot_id: Option<String>,
    /// Whether herdr's listing of the workspace includes the pane. Read as GONE only when
    /// `evidence_error` is `None`, so a failed listing must set `evidence_error`.
    pub pane_known_to_herdr: bool,
    /// Set when evidence could not be gathered, including a failed workspace listing; forces
    /// UNKNOWN with this reason.
    pub evidence_error: Option<String>,
}

/// One pane's verdict, with the reason recorded so a refusal can be audited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReapDecision {
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Herdr tab identifier, when known.
    pub tab_id: Option<String>,
    /// Tab label, when known.
    pub tab_label: Option<String>,
    /// The verdict reached.
    pub verdict: Verdict,
    /// Why, in one sentence.
    pub reason: String,
}

impl ReapDecision {
    /// Only a STALE verdict authorises closing the tab.
    #[must_use]
    pub fn reapable(&self) -> bool {
        self.verdict == Verdict::Stale
    }
}

/// The full set of verdicts for one sweep. Decides only; closes nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReapPlan {
    /// Every pane considered, in the order evidence was supplied.
    pub decisions: Vec<ReapDecision>,
}

impl ReapPlan {
    /// Panes proven stale. Everything else is spared.
    #[must_use]
    pub fn reapable(&self) -> Vec<&ReapDecision> {
        self.decisions
            .iter()
            .filter(|decision| decision.reapable())
            .collect()
    }

    /// Panes left alone, each with the reason -- so "reaped nothing" is never silent.
    #[must_use]
    pub fn declined(&self) -> Vec<&ReapDecision> {
        self.decisions
            .iter()
            .filter(|decision| !decision.reapable())
            .collect()
    }

    /// Counts for EVERY verdict, including the zeros, plus `considered`.
    ///
    /// Reported unconditionally because "reaped 0 because nothing was stale" and "reaped 0 because
    /// the detector is inert" are indistinguishable from a bare total.
    #[must_use]
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut out: BTreeMap<&'static str, usize> = Verdict::all()
            .into_iter()
            .map(|verdict| (verdict.as_str(), 0))
            .collect();
        for decision in &self.decisions {
            *out.entry(decision.verdict.as_str()).or_insert(0) += 1;
        }
        out.insert("considered", self.decisions.len());
        out
    }
}

fn decide(evidence: &PaneEvidence) -> ReapDecision {
    let build = |verdict: Verdict, reason: String| ReapDecision {
        pane_id: evidence.pane_id.clone(),
        tab_id: evidence.tab_id.clone(),
        tab_label: evidence.tab_label.clone(),
        verdict,
        reason,
    };

    // Scope first: a tab outside our workspace/schema is never a candidate, however idle it looks.
    if !evidence.in_scope {
        return build(
            Verdict::OutOfScope,
            format!(
                "tab {} in workspace {} is not one of ours",
                quote_optional(evidence.tab_label.as_deref()),
                quote_optional(evidence.workspace_label.as_deref())
            ),
        );
    }

    if let Some(error) = &evidence.evidence_error {
        return build(Verdict::Unknown, format!("evidence unavailable: {error}"));
    }

    if !evidence.pane_known_to_herdr {
        // The listing succeeded (a failed one set evidence_error above) and does not include the
        // pane, so its tab is already closed: nothing to close, and no share of max_panes.
        return build(
            Verdict::Gone,
            "herdr's listing of the workspace does not include this pane: its tab is already closed and holds none of max_panes".to_owned(),
        );
    }

    // R1 -- in-flight work beats every other signal. This is the "agent is thinking" case.
    let unfinished = evidence
        .run_exit_codes_recorded
        .iter()
        .filter(|recorded| !**recorded)
        .count();
    if unfinished > 0 {
        return build(
            Verdict::InFlight,
            format!("{unfinished} run(s) for this pane have no recorded exit_code"),
        );
    }

    // R3 -- we must have a bound identity from when the run happened, or we cannot reason at all.
    let Some(recorded) = evidence
        .recorded_shell
        .as_ref()
        .filter(|identity| identity.is_bound())
    else {
        return build(
            Verdict::Unknown,
            "no identity-bound shell recorded for this pane (need pid+boot_id+start_ticks)"
                .to_owned(),
        );
    };

    // A missing recorded PID is only evidence of death within the SAME boot. Across a reboot every
    // old pid is absent by construction, which cannot authorize closing a newly-created live tab.
    let Some(current_boot) = evidence
        .current_boot_id
        .as_deref()
        .filter(|boot| !strip_space(boot).is_empty())
    else {
        return build(
            Verdict::Unknown,
            "current boot_id is unavailable".to_owned(),
        );
    };
    if recorded.boot_id.as_deref() != Some(current_boot) {
        return build(
            Verdict::Unknown,
            format!(
                "recorded shell belongs to boot {}, current boot is {}",
                quote_optional(recorded.boot_id.as_deref()),
                quote_optional(Some(current_boot))
            ),
        );
    }

    // R2 -- the shell is gone. Note this is the PANE shell, not the run-directory PID, which is the
    // short-lived herdr-run CLI and is dead for every completed run.
    let Some(live) = evidence.live_shell.as_ref() else {
        return build(
            Verdict::Stale,
            format!(
                "all runs finished and pane shell {} is gone (identity {}:{})",
                display_pid(recorded.pid),
                recorded.boot_id.as_deref().unwrap_or("?"),
                display_ticks(recorded.start_ticks),
            ),
        );
    };

    if !live.is_bound() {
        return build(
            Verdict::Unknown,
            format!(
                "could not bind identity of live pid {}",
                display_pid(live.pid)
            ),
        );
    }

    if recorded.same_process_as(live) {
        return build(
            Verdict::ShellAlive,
            format!(
                "pane shell {} is still the original process",
                display_pid(live.pid)
            ),
        );
    }

    // A different identity may be PID reuse or a new pane incarnation. Deliberately UNKNOWN, not
    // STALE: the original is probably gone, but the currently-listed pane is not proven to be it.
    build(
        Verdict::Unknown,
        format!(
            "recorded pane shell {} is now a DIFFERENT process identity (recorded {}:{}, live {}/{}:{}) -- PID reuse or new pane incarnation, refusing to guess",
            display_pid(recorded.pid),
            recorded.boot_id.as_deref().unwrap_or("?"),
            display_ticks(recorded.start_ticks),
            display_pid(live.pid),
            live.boot_id.as_deref().unwrap_or("?"),
            display_ticks(live.start_ticks),
        ),
    )
}

fn display_pid(pid: Option<i64>) -> String {
    pid.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

fn display_ticks(ticks: Option<u64>) -> String {
    ticks.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

/// Decide, for each pane, whether it is provably stale. Closes nothing.
#[must_use]
pub fn plan_reap(evidence: &[PaneEvidence]) -> ReapPlan {
    ReapPlan {
        decisions: evidence.iter().map(decide).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "boot-aaaa";

    fn ident(pid: i64, boot: &str, ticks: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid: Some(pid),
            boot_id: Some(boot.to_owned()),
            start_ticks: Some(ticks),
        }
    }

    fn scoped() -> PaneEvidence {
        PaneEvidence {
            pane_id: "wE:p1".to_owned(),
            tab_id: Some("wE:t1".to_owned()),
            tab_label: Some("widget-w1".to_owned()),
            workspace_label: Some("agent-cmds".to_owned()),
            in_scope: true,
            run_exit_codes_recorded: vec![true],
            recorded_shell: Some(ident(4242, BOOT, 1000)),
            live_shell: None,
            current_boot_id: Some(BOOT.to_owned()),
            pane_known_to_herdr: true,
            evidence_error: None,
        }
    }

    #[test]
    fn planted_stale_tab_is_reaped() {
        let plan = plan_reap(&[scoped()]);
        assert_eq!(plan.counts()["STALE"], 1);
        assert_eq!(plan.reapable().len(), 1);
        assert!(plan.reapable()[0].reason.contains("is gone"));
    }

    #[test]
    fn agent_thinking_is_not_reaped() {
        // The shell is ALSO gone here, so only R1 stands between this tab and the axe.
        let plan = plan_reap(&[PaneEvidence {
            run_exit_codes_recorded: vec![true, false],
            ..scoped()
        }]);
        assert_eq!(plan.counts()["IN_FLIGHT"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        assert!(plan.reapable().is_empty());
    }

    #[test]
    fn live_shell_is_not_reaped() {
        let plan = plan_reap(&[PaneEvidence {
            live_shell: Some(ident(4242, BOOT, 1000)),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["SHELL_ALIVE"], 1);
        assert!(plan.reapable().is_empty());
    }

    #[test]
    fn recycled_pid_is_unknown_not_stale() {
        let plan = plan_reap(&[PaneEvidence {
            live_shell: Some(ident(4242, BOOT, 999_999)),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        assert!(plan.declined()[0].reason.contains("PID reuse"));
    }

    #[test]
    fn rebooted_host_is_unknown_even_when_the_recorded_pid_is_gone() {
        let plan = plan_reap(&[PaneEvidence {
            recorded_shell: Some(ident(4242, "old-boot", 1000)),
            current_boot_id: Some("new-boot".to_owned()),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(plan.declined()[0].reason.contains("current boot"));
    }

    #[test]
    fn missing_current_boot_is_unknown() {
        let plan = plan_reap(&[PaneEvidence {
            current_boot_id: None,
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(plan.reapable().is_empty());
    }

    #[test]
    fn new_pane_incarnation_with_a_different_shell_pid_is_unknown() {
        let plan = plan_reap(&[PaneEvidence {
            live_shell: Some(ident(9001, BOOT, 2000)),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(plan.declined()[0].reason.contains("new pane incarnation"));
    }

    #[test]
    fn unbound_recorded_identity_is_unknown() {
        let plan = plan_reap(&[PaneEvidence {
            recorded_shell: Some(ProcessIdentity {
                pid: Some(4242),
                ..ProcessIdentity::default()
            }),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(plan.reapable().is_empty());
    }

    #[test]
    fn pane_missing_from_a_successful_listing_is_gone_not_stale() {
        let plan = plan_reap(&[PaneEvidence {
            pane_known_to_herdr: false,
            ..scoped()
        }]);
        assert_eq!(plan.counts()["GONE"], 1);
        assert_eq!(plan.counts()["UNKNOWN"], 0);
        assert!(plan.reapable().is_empty());
        assert!(plan.declined()[0].reason.contains("already closed"));
    }

    #[test]
    fn pane_missing_because_the_listing_failed_is_unknown_not_gone() {
        let plan = plan_reap(&[PaneEvidence {
            pane_known_to_herdr: false,
            evidence_error: Some("pane list: timed out".to_owned()),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert_eq!(plan.counts()["GONE"], 0);
    }

    #[test]
    fn out_of_scope_beats_gone() {
        let plan = plan_reap(&[PaneEvidence {
            pane_known_to_herdr: false,
            in_scope: false,
            ..scoped()
        }]);
        assert_eq!(plan.counts()["OUT_OF_SCOPE"], 1);
    }

    #[test]
    fn evidence_error_is_unknown() {
        let plan = plan_reap(&[PaneEvidence {
            evidence_error: Some("/proc unreadable".to_owned()),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(plan.declined()[0].reason.contains("/proc unreadable"));
    }

    #[test]
    fn out_of_scope_tab_is_never_considered() {
        let plan = plan_reap(&[PaneEvidence {
            in_scope: false,
            workspace_label: Some("someone-else".to_owned()),
            ..scoped()
        }]);
        assert_eq!(plan.counts()["OUT_OF_SCOPE"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
    }

    #[test]
    fn mixed_population_reports_counts_on_both_sides() {
        let plan = plan_reap(&[
            PaneEvidence {
                pane_id: "wE:p1".to_owned(),
                ..scoped()
            },
            PaneEvidence {
                pane_id: "wE:p2".to_owned(),
                live_shell: Some(ident(4242, BOOT, 1000)),
                ..scoped()
            },
            PaneEvidence {
                pane_id: "wE:p3".to_owned(),
                run_exit_codes_recorded: vec![false],
                ..scoped()
            },
            PaneEvidence {
                pane_id: "wE:p4".to_owned(),
                live_shell: Some(ident(4242, BOOT, 7)),
                ..scoped()
            },
            PaneEvidence {
                pane_id: "wE:p5".to_owned(),
                in_scope: false,
                ..scoped()
            },
            PaneEvidence {
                pane_id: "wE:p6".to_owned(),
                pane_known_to_herdr: false,
                ..scoped()
            },
        ]);
        let counts = plan.counts();
        assert_eq!(counts["considered"], 6);
        assert_eq!(counts["GONE"], 1);
        assert_eq!(counts["STALE"], 1);
        assert_eq!(counts["SHELL_ALIVE"], 1);
        assert_eq!(counts["IN_FLIGHT"], 1);
        assert_eq!(counts["UNKNOWN"], 1);
        assert_eq!(counts["OUT_OF_SCOPE"], 1);
        assert_eq!(plan.reapable()[0].pane_id, "wE:p1");
        // Every declined pane carries a reason; "declined silently" is the failure to prevent.
        assert!(plan.declined().iter().all(|d| !d.reason.is_empty()));
    }

    #[test]
    fn an_empty_population_still_reports_every_verdict() {
        let counts = plan_reap(&[]).counts();
        assert_eq!(counts["considered"], 0);
        for verdict in Verdict::all() {
            assert_eq!(counts[verdict.as_str()], 0);
        }
    }
}
