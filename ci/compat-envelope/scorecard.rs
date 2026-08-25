#!/usr/bin/env -S rust-script --force
//! Keep Hermit's compatibility scorecard derived from the E2E manifest and
//! verify that a validate run produced a fresh passing row for every selected
//! regression cell.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../manifest-plan/src/canonical_verdict.rs"]
mod canonical_verdict;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

const SCORECARD: &str = "SCORECARD.md";
const CELLS: &str = "ci/compat-envelope/cells.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SCHEMA: u64 = 6;
const PRESSURE_SUMMARY_SCHEMA: u64 = 4;
const CELL_RESULT_SCHEMA: u64 = 4;

const USAGE: &str = r#"Usage: ci/compat-envelope/scorecard.rs COMMAND [OPTIONS]

Commands:
  show
      Print the derived compatibility table.
  check
      Refuse if SCORECARD.md or ci/compat-envelope/cells.json is stale.
  update [--allow-green-removal] [--allow-cell-removal]
      Rewrite the two tracked files. Green regressions and cell deletion are
      refused unless the matching explicit flag is present.
  update-observations --summary FILE
      Merge one completed clean pressure-test summary into the red cells'
      checked-in observations. This never changes which cells are green.
  observe-results --results DIR
      Merge the divergence positions from ONE validate result directory into
      the cells' checked-in observations, under the `validate` provenance so
      they never mix with pressure-test bounds. Explicit and opt-in: ordinary
      validation does not run this and changes no tracked file. Never changes
      which cells are green.
  project-observations --series-root DIR --refreshed-at STAMP
      Re-derive the divergence-position projection from the series store, which
      is the authority for it. REFUSES to drop measured evidence when the source
      supplied no rows: reading nothing is what an unpopulated series looks
      like, not a finding that a cell has no evidence. An unreachable root is
      refused outright rather than read as empty.
  verify-results --results DIR [--lanes portable,privileged]
      Check the tracked files, then require a fresh PASS row at HEAD for every
      selected regression cell in the named lanes. The default is both lanes.
  self-test
      Exercise accepting and refusing result sets without running a guest.
  self-test-and-check
      Run the self-test and exact tracked-file check in one forced compilation.
  --help
      Show this text.

Green means that the cell is selected by ci/expected-e2e-plan.json. Everything
else in the manifest is red until it is measured, promoted into the selected
plan, and passes validate.
"#;

#[derive(Clone, Debug, Deserialize)]
struct ManifestRow {
    backend: String,
    bucket: String,
    ci: bool,
    #[serde(default)]
    ci_disabled_reason: Option<CiDisabledReasonData>,
    enabled: bool,
    lane: String,
    mode: String,
    test: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedPlan {
    cells: Vec<CellId>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrackedCells {
    schema: u64,
    /// How the `observations` below relate to the series store. See
    /// [`ObservationProjection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection: Option<ObservationProjection>,
    cells: Vec<TrackedCell>,
}

/// Declares that `observations` are a DERIVED PROJECTION, not the source of
/// truth, and records how stale that projection is.
///
/// ⚠️ WHY THE DEMOTION IS RECORDED IN THE FILE RATHER THAN JUST BELIEVED. Once
/// the series store is the authority, `cells.json` is a cache -- and a cache
/// that does not say when it was refreshed is indistinguishable from current
/// data. `last_tested` remains the per-cell staleness marker; this is the
/// per-file one, and it names the source so a reader can go and check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservationProjection {
    /// Where the authoritative rows live. Recorded so a stale projection can be
    /// re-derived without anyone having to remember.
    source: String,
    /// When this projection was last refreshed from that source.
    refreshed_at: String,
    /// How many series rows the refresh actually read.
    ///
    /// ⚠️ ZERO IS A REAL AND DIFFERENT ANSWER FROM ABSENT. A refresh that read
    /// zero rows has not established that a cell has no evidence -- it has
    /// established that the source had nothing to say, which is what happens
    /// when the producer has not run yet. Recording the count is what lets a
    /// reader tell "projected from 800 rows" from "projected from nothing".
    rows_read: u64,
    /// True when observations below predate the series store and are therefore
    /// still authoritative rather than derived.
    ///
    /// This is the honest state today: step 4 has not emitted a row yet, so the
    /// series is empty and every observation here is pre-series evidence.
    #[serde(default)]
    pre_series_corpus: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    #[serde(default)]
    enabled: bool,
    status: CellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ci_disabled_reason: Option<CiDisabledReasonData>,
    /// Last recorded exercise of this cell. See [`LastTested`] -- absence means
    /// no writer recorded one, NOT that the cell was never run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tested: Option<LastTested>,
    /// Divergence evidence, keyed by `(detcore_tree, provenance)`.
    ///
    /// ORDINARY VALIDATE STILL NEVER CHANGES THIS ARRAY. Two commands write it,
    /// both explicit and opt-in: `update-observations` from a pressure-test
    /// summary, and `observe-results` from a validate result directory. Neither
    /// runs as part of a normal validate, so the tracked file stays untouched
    /// by routine runs.
    ///
    /// The two provenances answer different questions and are never merged --
    /// pressure test repeats a cell at one tree and measures flakiness, while
    /// validate runs it once per commit and supplies the regression signal.
    observations: Vec<Observation>,
    /// What the evidence above says, in one word, readable without opening
    /// another file. DERIVED -- see [`MeasurementState`] and
    /// [`derive_measurement`]. Defaulted for the pre-field corpus; every write
    /// recomputes it and the writer boundary refuses a stored value that
    /// disagrees with the derivation.
    #[serde(default = "default_measurement")]
    measurement: MeasurementState,
}

fn default_measurement() -> MeasurementState {
    MeasurementState::NeverMeasured
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CiDisabledReasonData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CellStatus {
    Green,
    Red,
}

/// WHICH MECHANISM produced an observation. Two mechanisms answer two different
/// questions and their bounds must never be merged into one range.
///
/// PRESSURE TEST repeats a cell at ONE fixed tree, so its bounds isolate
/// run-to-run flakiness -- that is the distribution a yellow-cell floor should
/// be derived from.
///
/// VALIDATE runs a cell ONCE per commit, so a single validate observation is a
/// point, not a distribution. Its value is as the regression signal a floor is
/// CHECKED against.
///
/// Merging them would produce one number moving for two unrelated causes -- "the
/// code changed" and "this varies run to run" -- which is the measurement trap
/// this project has repeatedly been bitten by. Observations are therefore keyed
/// by `(detcore_tree, provenance)`, not by tree alone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ObservationProvenance {
    PressureTest,
    Validate,
}

impl ObservationProvenance {
    fn as_str(self) -> &'static str {
        match self {
            Self::PressureTest => "pressure-test",
            Self::Validate => "validate",
        }
    }
}

/// What the row's evidence actually says, readable WITHOUT opening another file.
///
/// ⚠️ THIS EXISTS BECAUSE AN EMPTY FIELD HAD FIVE MEANINGS. `last_tested`'s own
/// doc already warns that absence "means NO WRITER RECORDED ONE, not that the
/// cell was never exercised", and `render_evidence_coverage` was added to "state
/// the coverage out loud every time the table is printed". That is a mitigation
/// at the PRINTER: a consumer reading `cells.json` directly still could not tell
/// a never-measured cell from a measured-and-passed one, because both have an
/// empty `observations` and both may lack `last_tested`. Anything downstream
/// then had to guess, and a guess that reads "never measured" as "passed" turns
/// absence of evidence into evidence.
///
/// DERIVED, NEVER ASSERTED. `derive_measurement` computes this from the
/// evidence already on the row, and `enforce_writer_boundary` refuses any write
/// where the stored value disagrees. So it is a cache that cannot lie rather
/// than a second source of truth that can drift from the first.
///
/// The vocabulary is deliberately IDENTICAL to `ci-hub/series/series.py`'s
/// `measurement_state`, so the scorecard and the series store cannot describe
/// the same cell with different words.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MeasurementState {
    /// No observation of any kind. NOT the same as passing.
    NeverMeasured,
    /// Measured, and every recorded result was a pass.
    MeasuredAndPassed,
    /// Measured, nothing passed, but nothing diverged either -- every result was
    /// a crash, timeout or OOM. ⚠️ A NON-VERDICT IS NOT A DIVERGENCE: reading
    /// one as a product failure is how an infrastructure hiccup becomes a false
    /// regression.
    MeasuredNoVerdict,
    /// A real divergence, whose position could not be established. A LEGITIMATE
    /// answer, not an error: refusing it would force a writer to invent a
    /// coordinate it does not have.
    DivergedUnlocated,
    /// A real divergence with at least one located coordinate.
    Diverged,
}

impl MeasurementState {
    fn as_str(self) -> &'static str {
        match self {
            Self::NeverMeasured => "never-measured",
            Self::MeasuredAndPassed => "measured-and-passed",
            Self::MeasuredNoVerdict => "measured-no-verdict",
            Self::DivergedUnlocated => "diverged-unlocated",
            Self::Diverged => "diverged",
        }
    }
}

/// Compute the one correct [`MeasurementState`] for a row from its own evidence.
///
/// A divergence is a determinism/parity/replay failure. Crash, timeout and OOM
/// are measured NON-VERDICTS and deliberately do not count as divergence.
fn derive_measurement(cell: &TrackedCell) -> MeasurementState {
    if cell.observations.is_empty() {
        return MeasurementState::NeverMeasured;
    }
    let mut diverged = false;
    let mut passed = false;
    let mut located = false;
    for observation in &cell.observations {
        for result in &observation.results {
            match result {
                ObservedResult::Pass => passed = true,
                ObservedResult::DeterminismFailure
                | ObservedResult::ParityFailure
                | ObservedResult::ReplayFailure => diverged = true,
                ObservedResult::CrashError | ObservedResult::Timeout | ObservedResult::Oom => {}
            }
        }
        located |= !observation.first_divergent_record.is_empty()
            || !observation.first_divergent_syscall.is_empty()
            || !observation.first_divergent_scheduler_turn.is_empty()
            || !observation.first_divergent_virtual_nanoseconds.is_empty();
    }
    if diverged {
        return if located {
            MeasurementState::Diverged
        } else {
            MeasurementState::DivergedUnlocated
        };
    }
    if passed {
        MeasurementState::MeasuredAndPassed
    } else {
        MeasurementState::MeasuredNoVerdict
    }
}

/// Set every row's `measurement` to its derived value. Called before the writer
/// boundary so the guard checks a value the writer actually intended.
fn refresh_measurement(cells: &mut TrackedCells) {
    for cell in &mut cells.cells {
        cell.measurement = derive_measurement(cell);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Observation {
    detcore_tree: String,
    /// Defaulted to `pressure-test` so the pre-provenance corpus keeps its
    /// meaning: before this field existed, the pressure test was the ONLY
    /// writer of observations, so that is what any older entry must have been.
    /// (In practice no such entry exists -- every cell's array was empty when
    /// this landed -- but a default that quietly relabelled old data as
    /// validate-sourced would be wrong even with nothing to relabel.)
    #[serde(default = "default_provenance")]
    provenance: ObservationProvenance,
    /// Depth at which this observation was taken, KEYED BY REPOSITORY.
    ///
    /// Per-repo because hermit depth and reverie depth are DIFFERENT KEYSPACES
    /// and a bare integer would be read against whichever one the reader had in
    /// mind. Measured at the time of writing: hermit 1927, reverie 931. Those
    /// numbers are not comparable to each other in any way.
    ///
    /// A repository whose depth could not be resolved is ABSENT from the map
    /// rather than present with a zero or a guess. Hermit is always resolvable
    /// because the tool runs inside it; reverie is only resolvable when a
    /// checkout is reachable, which is a property of the workspace layout
    /// rather than of the measurement.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    depth: BTreeMap<String, SourceDepth>,
    hermit_shas: BTreeSet<String>,
    results: BTreeSet<ObservedResult>,
    invocations: BTreeSet<ObservedInvocation>,
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_scheduler_turn: ObservedPositions,
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_virtual_nanoseconds: ObservedPositions,
    /// The prefix of the log that was deterministic, as a compared-record
    /// index. Shares a unit with the report's compared counts, unlike the two
    /// above, which are positions in scheduler and virtual time.
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_record: ObservedPositions,
    /// Syscalls the guest completed before diverging. A DIFFERENT KEYSPACE from
    /// the three above -- one real divergence was record 98, syscall 37,
    /// scheduler turn 4 -- so these bounds must never be read against another
    /// coordinate's axis.
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_syscall: ObservedPositions,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ObservedInvocation {
    hermit_sha: String,
    run_id: String,
    result: ObservedResult,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    attempts: Vec<ObservedAttemptInvocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ObservedAttemptInvocation {
    index: String,
    outcome: String,
    status: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
enum ObservedResult {
    Pass,
    DeterminismFailure,
    ParityFailure,
    ReplayFailure,
    CrashError,
    Timeout,
    Oom,
}

impl ObservedResult {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pass" => Ok(Self::Pass),
            "determinism-failure" => Ok(Self::DeterminismFailure),
            "parity-failure" => Ok(Self::ParityFailure),
            "replay-failure" => Ok(Self::ReplayFailure),
            "crash-error" => Ok(Self::CrashError),
            "timeout" => Ok(Self::Timeout),
            "oom" => Ok(Self::Oom),
            "infrastructure-error" => Err(
                "pressure summary contains an infrastructure error; refusing to store it as product behavior"
                    .into(),
            ),
            other => Err(format!("unknown pressure result `{other}`")),
        }
    }

    fn carries_divergence_position(self) -> bool {
        matches!(
            self,
            Self::DeterminismFailure | Self::ParityFailure | Self::ReplayFailure
        )
    }
}

/// How deep in a repository's history an observation was taken, so staleness is
/// SUBTRACTABLE -- "measured 47 commits ago" rather than a stare at two opaque
/// forty-hex shas.
///
/// BOTH COUNTS ARE RECORDED BECAUSE THEY DISAGREE, and by a lot. Measured on
/// this repository at the time of writing: `git rev-list --count HEAD` is 1927
/// while `--first-parent` is 1852, a 75-commit gap, because hermit's history
/// contains merges. The plain count is TOTAL REACHABLE ANCESTRY -- every commit
/// that was ever merged in, including whole side branches -- and is NOT a
/// distance along the mainline. `first_parent` is the mainline distance and is
/// the one that matches what "N commits ago" means to a reader. Recording only
/// one would guarantee it eventually gets read as the other.
///
/// SUBTRACTION IS ONLY MEANINGFUL ALONG ANCESTRY. Two commits on divergent
/// branches can carry identical depths while being unrelated, so a difference
/// is a distance only once one commit is known to be an ancestor of the other.
/// Depth complements the recorded tree and sha; it does not replace them.
/// When a cell was last ACTUALLY EXERCISED, by sha and depth. No timestamp: a
/// date cannot tell you whether the code under test changed, and the tree hash
/// and depth can.
///
/// ⚠️ ABSENCE MEANS "NO WRITER HAS RECORDED ONE", NOT "NEVER TESTED". The two
/// are easy to confuse and the difference matters. Only the two explicit fold
/// commands write this field, and validate only ever runs the ~282 selected
/// cells, so every red cell will carry NOTHING here until a pressure-test
/// campaign covers it -- not because it was never exercised, but because
/// nothing was ever asked to record that it was. `scorecard.rs show` prints how
/// many cells carry the field precisely so this emptiness stays visible instead
/// of being read as evidence.
///
/// This is recorded for EVERY tested cell, including passing ones, which is why
/// it is on the cell row rather than inside `observations`: an observation only
/// exists where a divergence was located, so a green cell that stays green
/// would otherwise leave no trace of having been checked at all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LastTested {
    hermit_sha: String,
    /// The staleness key. Content-addressed, so comparing it to `HEAD:detcore`
    /// says whether the result still describes the current code regardless of
    /// how old or recent the run was.
    detcore_tree: String,
    /// Keyed by repository: hermit depth and reverie depth are different
    /// keyspaces and a bare number would be read against the wrong one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    depth: BTreeMap<String, SourceDepth>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SourceDepth {
    /// `git rev-list --count HEAD`: total reachable ancestry, merges included.
    commits: u64,
    /// `git rev-list --count --first-parent HEAD`: mainline distance.
    first_parent: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedRange {
    earliest: u64,
    latest: u64,
    /// How many LOCATED positions produced these bounds.
    ///
    /// Without it the bounds invite exactly the over-reading this repository
    /// keeps catching: "earliest 80, latest 500" is a different claim over two
    /// runs than over fifty, and the pair alone cannot tell them apart.
    ///
    /// This is NOT the number of runs, and the difference matters. A run that
    /// passed located nothing and contributes no sample, so `samples` counts
    /// only the runs that actually diverged. It is also per COORDINATE rather
    /// than per observation, because a report can locate a scheduler turn
    /// without locating a virtual nanosecond, which would leave one coordinate
    /// with fewer samples behind it than its sibling.
    ///
    /// `samples == 1` is the honest way to say "one observation, so these
    /// bounds are a point, not a range".
    samples: u64,
}

/// Refuse a projection refresh that would erase evidence it did not replace.
///
/// ⚠️ THIS IS THE WHOLE POINT OF LANDING THE GUARD BEFORE THE DATA. Once
/// `cells.json` is a projection, the natural implementation is "read the series,
/// write what it says". With an empty or unreachable series that writes NOTHING,
/// and the three real divergence coordinates currently on main -- the only
/// located evidence in the file -- disappear. The resulting row is
/// indistinguishable from a cell nobody ever measured, which is precisely the
/// failure `deny_unknown_fields` was load-bearing against one step earlier: a
/// silent default that reads as absence of evidence.
///
/// So a refresh may only DROP an observation when the source actually supplied
/// rows to replace it. Zero rows read is not a statement that a cell has no
/// evidence; it is a statement that the source had nothing to say.
fn enforce_projection_preserves_evidence(
    before: &TrackedCells,
    after: &TrackedCells,
    rows_read: u64,
) -> Result<(), String> {
    if rows_read > 0 {
        return Ok(());
    }
    let index = |cells: &TrackedCells| -> BTreeMap<CellId, TrackedCell> {
        cells
            .cells
            .iter()
            .map(|cell| (cell.id.clone(), cell.clone()))
            .collect()
    };
    let (old, new) = (index(before), index(after));
    for (id, old_cell) in &old {
        let had = located_evidence(old_cell);
        if had == 0 {
            continue;
        }
        let has = new.get(id).map_or(0, located_evidence);
        if has < had {
            return Err(format!(
                "projection refused: it read 0 series rows yet would drop measured \
                 evidence from {}/{}/{} ({had} located coordinate(s) down to {has}). \
                 Reading nothing is not the same as establishing that a cell has no \
                 evidence -- it is what an unpopulated or not-yet-written series \
                 looks like. Populate the series (plan step 4) or leave the \
                 pre-series corpus alone.",
                id.test, id.mode, id.backend
            ));
        }
    }
    Ok(())
}

/// How many located divergence positions a cell holds, across every
/// observation and all four coordinates.
///
/// ⚠️ COUNTED IN POSITIONS, NOT OBSERVATIONS. The first version of the guard
/// above compared `observations.len()`, and an end-to-end run against the real
/// `cells.json` walked straight through it: the naive projection does not
/// delete observations, it BLANKS THE COORDINATES INSIDE THEM. The vector
/// length never moves, so a count-based guard sees a clean refresh while all
/// three located coordinates on main disappear. The quantity that can vanish is
/// the quantity that has to be counted.
fn located_evidence(cell: &TrackedCell) -> usize {
    cell.observations
        .iter()
        .map(|observation| {
            [
                &observation.first_divergent_scheduler_turn,
                &observation.first_divergent_virtual_nanoseconds,
                &observation.first_divergent_record,
                &observation.first_divergent_syscall,
            ]
            .into_iter()
            .filter(|positions| !positions.is_empty())
            .count()
        })
        .sum()
}

/// Every located position for one coordinate, ONE ENTRY PER RUN THAT LOCATED IT.
///
/// ⚠️ THE STORED FORM IS THE POSITIONS; THE RANGE IS DERIVED. Storing only
/// `{earliest, latest, samples}` discards the distribution the pressure test
/// exists to measure: `{earliest 93, latest 94, samples 2}` and fifty runs
/// clustered at 93 with one outlier at 94 are the same triple and completely
/// different findings. Keeping the positions makes the bound recomputable and
/// everything else -- median, clustering, whether a "range" is really a point
/// with one stray -- answerable later without re-running anything.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ObservedPositions {
    /// One entry per run that located this coordinate, in insertion order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    positions: Vec<u64>,
    /// Bounds inherited from before positions were stored.
    ///
    /// ⚠️ DELIBERATELY NOT EXPANDED INTO POSITIONS, and this is the whole
    /// honesty of the migration. `{earliest 93, latest 94, samples 2}` records
    /// that two runs diverged somewhere in [93, 94]; it does NOT record which
    /// run was which, and no rule recovers that. Synthesising two positions
    /// would invent measurements nobody took -- the exact fabrication this
    /// change exists to prevent -- so legacy evidence is carried forward as the
    /// weaker claim it always was, and marked as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_bounds: Option<ObservedRange>,
}

impl ObservedPositions {
    /// The DERIVED `{earliest, latest, samples}` view. `None` when nothing
    /// located this coordinate.
    fn range(&self) -> Option<ObservedRange> {
        let from_positions = if self.positions.is_empty() {
            None
        } else {
            Some(ObservedRange {
                earliest: *self.positions.iter().min().expect("non-empty"),
                latest: *self.positions.iter().max().expect("non-empty"),
                samples: self.positions.len() as u64,
            })
        };
        match (from_positions, self.legacy_bounds) {
            (None, legacy) => legacy,
            (Some(range), None) => Some(range),
            // Both present: widen the bounds and ADD the sample counts, because
            // the legacy triple stands for runs that really happened even though
            // their individual positions are gone.
            (Some(range), Some(legacy)) => Some(ObservedRange {
                earliest: range.earliest.min(legacy.earliest),
                latest: range.latest.max(legacy.latest),
                samples: range.samples + legacy.samples,
            }),
        }
    }

    fn is_empty(&self) -> bool {
        self.positions.is_empty() && self.legacy_bounds.is_none()
    }

    /// Record one run's located position. A run that located nothing is not a
    /// sample of WHERE the divergence was and contributes no entry.
    fn record(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.positions.push(value);
        }
    }
}

/// Accept both the current form and the pre-step-5 bare range, without letting
/// a legacy object silently deserialize as "no evidence".
impl<'de> Deserialize<'de> for ObservedPositions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Current {
            #[serde(default)]
            positions: Vec<u64>,
            #[serde(default)]
            legacy_bounds: Option<ObservedRange>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            // ⚠️ `null` IS A REAL FORM IN THE EXISTING FILE and must be handled
            // explicitly. Before this change the four coordinates were
            // `Option<ObservedRange>` and serialized as `null` when nothing had
            // located them, so the tracked corpus is full of them. Omitting this
            // variant made `update` abort with "data did not match any variant
            // of untagged enum Either" -- caught by running it, not by reading.
            Absent(()),
            // ⚠️ `Current` IS TRIED FIRST AND USES `deny_unknown_fields`, which is
            // load-bearing: without it a legacy `{earliest, latest, samples}`
            // object would match `Current` with every field defaulted and the
            // bounds would be SILENTLY DISCARDED -- evidence loss that looks
            // exactly like a cell that was never measured.
            Current(Current),
            Legacy(ObservedRange),
        }
        Ok(match Either::deserialize(deserializer)? {
            Either::Absent(()) => ObservedPositions::default(),
            Either::Current(current) => ObservedPositions {
                positions: current.positions,
                legacy_bounds: current.legacy_bounds,
            },
            Either::Legacy(range) => ObservedPositions {
                positions: Vec::new(),
                legacy_bounds: Some(range),
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct PressureSummary {
    schema: u64,
    hermit_sha: String,
    detcore_tree: String,
    source_tree_dirty: bool,
    rows: Vec<PressureSummaryRow>,
}

#[derive(Clone, Debug, Deserialize)]
struct PressureSummaryRow {
    cell: CellId,
    /// Which repeat of this cell the row describes.
    ///
    /// ⚠️ THIS FIELD IS WHY THE RANGES WERE UNFILLABLE. `pressure-test.rs run
    /// --repetitions N` emits ONE ROW PER REPETITION, each stamped with its
    /// repetition and carrying its own verification report. This consumer had
    /// no such field and treated the second row for a cell as a DUPLICATE,
    /// refusing the whole summary -- so the one workflow that can produce a
    /// distribution could never reach the scorecard, and `samples` could never
    /// exceed one.
    ///
    /// Repeats are now distinguished, and the duplicate guard is keyed on
    /// `(cell, repetition)` so a genuinely repeated row is still refused.
    #[serde(default)]
    repetition: Option<u64>,
    result: String,
    #[serde(default)]
    verification: Option<PressureVerification>,
    #[serde(default)]
    evidence_errors: Vec<String>,
    invocation: Option<PressureInvocation>,
}

#[derive(Clone, Debug, Deserialize)]
struct PressureInvocation {
    run_id: String,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    attempts: Vec<ObservedAttemptInvocation>,
}

#[derive(Clone, Debug, Deserialize)]
struct PressureVerification {
    #[serde(default)]
    first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    first_divergent_virtual_nanoseconds: Option<u64>,
    /// `#[serde(default)]` like its siblings, so every verify report written
    /// before this field existed still parses and simply reports None.
    #[serde(default)]
    first_divergent_record: Option<u64>,
    #[serde(default)]
    first_divergent_syscall: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ResultRow {
    schema: u64,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    binary_sha256: Option<String>,
    test_sha256: String,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    classification: String,
    outcome: String,
    log_level: Option<String>,
    effective_args: Vec<String>,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    relaxations: Vec<String>,
    attempts: Vec<JsonValue>,
    /// WHERE the cell diverged, as emitted by the harness.
    ///
    /// `#[serde(default)]` for the same reason the sibling copy in
    /// ci/manifest-plan/src/canonical_verdict.rs is tolerant: this reader
    /// aggregates RETAINED result rows, including ones written before the field
    /// existed, so absence has to mean "older row" rather than "broken
    /// producer". The pressure test's copy is deliberately REQUIRED-nullable
    /// instead, because it only ever reads rows it just produced.
    #[serde(default)]
    first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    first_divergent_virtual_nanoseconds: Option<u64>,
    /// The third coordinate, added by hermit#2386. Unlike its two siblings this
    /// one LOCATES the divergence rather than bounding it: they are the
    /// position of the preceding scheduler COMMIT, while this is the index of
    /// the differing record itself.
    #[serde(default)]
    first_divergent_record: Option<u64>,
    #[serde(default)]
    first_divergent_syscall: Option<u64>,
}

impl ResultRow {
    fn id(&self) -> Option<CellId> {
        let backend = match self.backend.as_deref() {
            Some(backend) => backend.to_string(),
            None if self.mode == "naked" => "native".to_string(),
            None => return None,
        };
        Some(CellId {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend,
        })
    }

    fn require_literal_invocation(&self) -> Result<(), String> {
        if self.run_id.trim().is_empty()
            || self.argv.is_empty()
            || self.guest_argv.is_empty()
            || self.env.is_empty()
            || self.cwd.trim().is_empty()
            || self.shell_command.trim().is_empty()
            || self.shell_command != literal_shell_command(&self.cwd, &self.env, &self.argv)
            || self.attempts.is_empty()
            || self.attempts.iter().any(|attempt| {
                attempt
                    .get("argv")
                    .and_then(JsonValue::as_array)
                    .is_none_or(Vec::is_empty)
                    || attempt
                        .get("guest_argv")
                        .and_then(JsonValue::as_array)
                        .is_none_or(Vec::is_empty)
                    || attempt.get("env").and_then(JsonValue::as_object).is_none()
                    || attempt
                        .get("cwd")
                        .and_then(JsonValue::as_str)
                        .is_none_or(str::is_empty)
                    || attempt_shell_command_is_invalid(attempt)
            })
            || self.attempts.first().and_then(|attempt| attempt.get("argv"))
                != Some(&serde_json::to_value(&self.argv).unwrap())
            || self
                .attempts
                .first()
                .and_then(|attempt| attempt.get("guest_argv"))
                != Some(&serde_json::to_value(&self.guest_argv).unwrap())
            || self.attempts.first().and_then(|attempt| attempt.get("env"))
                != Some(&serde_json::to_value(&self.env).unwrap())
            || self.attempts.first().and_then(|attempt| attempt.get("cwd"))
                != Some(&JsonValue::String(self.cwd.clone()))
            || self
                .attempts
                .first()
                .and_then(|attempt| attempt.get("shell_command"))
                != Some(&JsonValue::String(self.shell_command.clone()))
        {
            return Err("does not bind its PASS/FAIL claim to a literal invocation".into());
        }
        Ok(())
    }

    fn require_provenance(&self) -> Result<(), String> {
        self.require_literal_invocation()?;
        let binary_sha = self
            .binary_sha256
            .as_deref()
            .ok_or("has no Hermit binary identity")?;
        require_sha256("Hermit binary", binary_sha)?;
        require_sha256("test", &self.test_sha256)?;
        if self.effective_args != self.argv.iter().skip(1).cloned().collect::<Vec<_>>() {
            return Err("effective_args does not match literal argv".into());
        }
        if self.mode == "naked" {
            if self.log_level.is_some() {
                return Err("naked result unexpectedly records a Hermit log level".into());
            }
        } else if self.log_level.as_deref().is_none_or(str::is_empty) {
            return Err("Hermit result has no log-level identity".into());
        }
        if self
            .relaxations
            .iter()
            .any(|relaxation| relaxation.trim().is_empty())
            || self.relaxations.iter().collect::<BTreeSet<_>>().len()
                != self.relaxations.len()
        {
            return Err("relaxations contain an empty or duplicate identity".into());
        }
        Ok(())
    }

    fn evidence_identity(&self) -> Result<String, String> {
        let evidence = serde_json::json!({
            "run_id": self.run_id,
            "hermit_sha": self.hermit_sha,
            "binary_sha256": self.binary_sha256,
            "test_sha256": self.test_sha256,
            "test": self.test,
            "category": self.category,
            "lane": self.lane,
            "mode": self.mode,
            "backend": self.backend,
            "classification": self.classification,
            "outcome": self.outcome,
            "log_level": self.log_level,
            "effective_args": self.effective_args,
            "argv": self.argv,
            "guest_argv": self.guest_argv,
            "env": self.env,
            "cwd": self.cwd,
            "shell_command": self.shell_command,
            "relaxations": self.relaxations,
            "attempts": self.attempts,
        });
        let encoded = serde_json::to_vec(&evidence)
            .map_err(|error| format!("cannot encode result evidence: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    fn require_canonical_pass_evidence(&self) -> Result<(), String> {
        if self.outcome != "PASS" || !matches!(self.mode.as_str(), "verify" | "replay" | "chaos") {
            return Ok(());
        }
        for (index, attempt) in self.attempts.iter().enumerate() {
            let report = attempt
                .get("verification_report")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| format!("attempt {} has no embedded verification report", index + 1))?;
            let recorded_sha = attempt
                .get("verification_report_sha256")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| format!("attempt {} has no verification-report identity", index + 1))?;
            let actual_sha = format!("{:x}", Sha256::digest(report.as_bytes()));
            if recorded_sha != actual_sha {
                return Err(format!("attempt {} verification-report identity does not match its embedded report", index + 1));
            }
            let report: canonical_verdict::VerificationReport = serde_json::from_str(report)
                .map_err(|error| format!("attempt {} has an incomplete verification report: {error}", index + 1))?;
            report.require_canonical_match().map_err(|error| {
                format!("attempt {} cannot support a green result: {error}", index + 1)
            })?;
        }
        Ok(())
    }
}

struct Derived {
    population: BTreeSet<CellId>,
    enabled: BTreeSet<CellId>,
    ci_disabled_reasons: BTreeMap<CellId, CiDisabledReasonData>,
    selected: BTreeSet<CellId>,
    green: BTreeSet<CellId>,
}

struct ResultCandidate {
    evidence_identity: String,
    path: PathBuf,
    row: ResultRow,
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility scorecard: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "show" => {
            no_more(&mut args)?;
            let derived = derive(&root)?;
            print!("{}", render_scorecard(&derived));
            print!("{}", render_evidence_coverage(&root)?);
        }
        "check" => {
            no_more(&mut args)?;
            let derived = check_tracked(&root)?;
            println!(
                "compatibility scorecard: tracked table and {} cells are current",
                derived.population.len()
            );
        }
        "update" => {
            let mut allow_green_removal = false;
            let mut allow_cell_removal = false;
            for arg in args {
                match arg.as_str() {
                    "--allow-green-removal" => allow_green_removal = true,
                    "--allow-cell-removal" => allow_cell_removal = true,
                    _ => return Err(format!("unknown update option `{arg}`\n\n{USAGE}")),
                }
            }
            update_tracked(&root, allow_green_removal, allow_cell_removal)?;
        }
        "update-observations" => {
            let mut summary = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--summary" => {
                        summary = Some(PathBuf::from(
                            args.next().ok_or("--summary requires a file")?,
                        ));
                    }
                    _ => {
                        return Err(format!(
                            "unknown update-observations option `{arg}`\n\n{USAGE}"
                        ));
                    }
                }
            }
            update_observations(
                &root,
                &summary.ok_or("update-observations requires --summary FILE")?,
            )?;
        }
        "project-observations" => {
            let mut series_root = None;
            let mut refreshed_at = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--series-root" => {
                        series_root = Some(PathBuf::from(
                            args.next().ok_or("--series-root requires a directory")?,
                        ));
                    }
                    "--refreshed-at" => {
                        refreshed_at =
                            Some(args.next().ok_or("--refreshed-at requires a timestamp")?);
                    }
                    _ => {
                        return Err(format!(
                            "unknown project-observations option `{arg}`\n\n{USAGE}"
                        ));
                    }
                }
            }
            project_observations(
                &root,
                &series_root.ok_or("project-observations requires --series-root DIR")?,
                // Supplied rather than read from the clock, so the same inputs
                // produce the same file. A refresh timestamp the tool invents is
                // a diff on every run that says nothing changed.
                &refreshed_at.ok_or("project-observations requires --refreshed-at STAMP")?,
            )?;
        }
        "verify-results" => {
            let mut result_root = None;
            let mut lanes = BTreeSet::from(["portable".to_string(), "privileged".to_string()]);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    "--lanes" => {
                        lanes = args
                            .next()
                            .ok_or("--lanes requires a comma-separated value")?
                            .split(',')
                            .filter(|lane| !lane.is_empty())
                            .map(str::to_string)
                            .collect();
                        if lanes.is_empty()
                            || lanes
                                .iter()
                                .any(|lane| lane != "portable" && lane != "privileged")
                        {
                            return Err("--lanes accepts portable, privileged, or both".into());
                        }
                    }
                    _ => return Err(format!("unknown verify-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("verify-results requires --results DIR")?;
            check_tracked(&root)?;
            verify_results(&root, &result_root, &lanes)?;
        }
        "observe-results" => {
            let mut result_root = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    _ => return Err(format!("unknown observe-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("observe-results requires --results DIR")?;
            observe_results(&root, &result_root)?;
        }
        "self-test" => {
            no_more(&mut args)?;
            self_test()?;
        }
        "self-test-and-check" => {
            no_more(&mut args)?;
            self_test()?;
            let derived = check_tracked(&root)?;
            println!(
                "compatibility scorecard: tracked table and {} cells are current",
                derived.population.len()
            );
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn no_more(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    match args.next() {
        Some(arg) => Err(format!("unexpected argument `{arg}`\n\n{USAGE}")),
        None => Ok(()),
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(EXPECTED_PLAN).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn derive(root: &Path) -> Result<Derived, String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "matrix-json",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run hermit-manifest-plan: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hermit-manifest-plan failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<ManifestRow> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("manifest-plan emitted invalid JSON: {e}"))?;
    let expected: ExpectedPlan = read_json(&root.join(EXPECTED_PLAN))?;

    let mut manifest_cells = BTreeSet::new();
    let mut population = BTreeSet::new();
    let mut enabled = BTreeSet::new();
    let mut ci_enabled = BTreeSet::new();
    let mut ci_disabled_reasons = BTreeMap::new();
    for row in rows {
        let comparable = row.mode != "custom";
        let id = CellId {
            lane: row.lane,
            category: row.bucket,
            test: row.test,
            mode: row.mode,
            backend: row.backend,
        };
        if !manifest_cells.insert(id.clone()) {
            return Err(format!(
                "manifest-plan emitted duplicate cell {}",
                display_id(&id)
            ));
        }
        // `custom` is an explicit per-test command, not a mode which applies
        // uniformly to every test/backend pair. Keep selected custom commands
        // in ordinary validation, but do not manufacture 1,680 scorecard cells
        // from combinations that have no product meaning.
        if comparable {
            population.insert(id.clone());
        }
        if row.enabled {
            if comparable {
                enabled.insert(id.clone());
            }
            if row.ci {
                if row.ci_disabled_reason.is_some() {
                    return Err(format!(
                        "CI-enabled cell carries ci_disabled_reason: {}",
                        display_id(&id)
                    ));
                }
                ci_enabled.insert(id);
            } else {
                let reason = row.ci_disabled_reason.ok_or_else(|| {
                    format!(
                        "enabled cell omitted from ordinary CI has no reason: {}",
                        display_id(&id)
                    )
                })?;
                ci_disabled_reasons.insert(id, reason);
            }
        } else if row.ci_disabled_reason.is_some() {
            return Err(format!(
                "disabled cell carries ci_disabled_reason: {}",
                display_id(&id)
            ));
        }
    }
    let selected: BTreeSet<CellId> = expected.cells.into_iter().collect();
    if selected.is_empty() {
        return Err("expected E2E plan is empty".into());
    }
    for id in &selected {
        if !manifest_cells.contains(id) {
            return Err(format!(
                "expected plan names absent cell {}",
                display_id(id)
            ));
        }
        if !ci_enabled.contains(id) {
            return Err(format!(
                "expected plan names a cell not enabled for ordinary CI: {}",
                display_id(id)
            ));
        }
    }
    let green = selected_green(&selected, &population);
    Ok(Derived {
        population,
        enabled,
        ci_disabled_reasons,
        selected,
        green,
    })
}

fn selected_green(selected: &BTreeSet<CellId>, population: &BTreeSet<CellId>) -> BTreeSet<CellId> {
    selected
        .iter()
        .filter(|id| population.contains(*id))
        .cloned()
        .collect()
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
}

fn render_scorecard(derived: &Derived) -> String {
    let mut backends: BTreeSet<&str> = derived
        .population
        .iter()
        .map(|id| id.backend.as_str())
        .collect();
    let preferred = ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"];
    let mut ordered = Vec::new();
    for backend in preferred {
        if backends.remove(backend) {
            ordered.push(backend);
        }
    }
    ordered.extend(backends);

    let mut out = String::from(
        "# Compatibility scorecard\n\n\
This table is derived from the manifest, not from a separately maintained parent-workspace CSV. \
`./ci/compat-envelope/scorecard.rs check` verifies it.\n\n\
**Green** means the cell is SELECTED: it is listed in `ci/expected-e2e-plan.json` and is therefore \
required to pass by ordinary validation. **Red** is every \
other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until \
the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, \
not omitted: a cell that cannot run is not green.\n\n\
**Green does not mean measured, and it does not mean passing.** Selection, measurement, and result \
are three separate facts, and the Green column below reports only the first of them. A green cell \
that has never been executed once is the ordinary case, not an anomaly: green is a statement about \
what the plan REQUIRES, not about what has been OBSERVED. Whether a result was ever seen is a \
per-cell `measurement` field in `ci/compat-envelope/cells.json`, independent of colour and reading \
`never-measured`, `measured-and-passed`, or `diverged`; a cell can be green and `never-measured`, \
or red and `measured-and-passed`, and both combinations are present in the tracked file today. To \
count what has actually run, count that field -- do not count this table. Conflating the three has \
repeatedly produced project-status reports that quoted the Green total as a number of passing \
tests, which it has never been.\n\n\
Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend \
twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and \
accepts a result only when the typed report says `verified=true`, `verdict=matched`, \
`bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical \
`record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped \
comparison when invoked directly and does not satisfy \
this regression plan. These same-backend results do not establish cross-backend parity.\n\n\
| Backend | Green | Red | Total |\n\
| --- | ---: | ---: | ---: |\n",
    );
    let mut green_total = 0usize;
    let mut total = 0usize;
    for backend in &ordered {
        let backend_total = derived
            .population
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        let backend_green = derived
            .green
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        green_total += backend_green;
        total += backend_total;
        out.push_str(&format!(
            "| `{backend}` | {backend_green} | {} | {backend_total} |\n",
            backend_total - backend_green
        ));
    }
    out.push_str(&format!(
        "| **Total** | **{green_total}** | **{}** | **{total}** |\n\n",
        total - green_total
    ));
    // DENOMINATOR PROVENANCE. Emitted from the derived population, never
    // hand-written, so it cannot go stale and cannot be forgotten.
    //
    // ⚠️ WHY THIS EXISTS. A percentage is meaningless without the population it
    // was taken over, and this table's population CHANGES: adding a backend or
    // a mode to the manifest grows it, removing one shrinks it. Both move the
    // percentage while nothing about the product moves. Worked example with
    // real numbers at the time of writing: dropping `dbt` would remove 1035
    // cells, ALL OF THEM RED, taking the total from 5520 to 4485 and RAISING
    // reported green from 5.07% to 6.24% -- a 23% relative improvement with
    // nothing improved. Restoring it later would move the number back DOWN,
    // which reads as a regression when it is a restoration of honesty.
    //
    // Recording the composition beside the number makes any such change show up
    // as a diff hunk in this generated file, so a new percentage cannot be
    // quoted without the reason it moved sitting next to it.
    let percent = if total == 0 {
        0.0
    } else {
        100.0 * green_total as f64 / total as f64
    };
    let modes: BTreeSet<&str> = derived
        .population
        .iter()
        .map(|id| id.mode.as_str())
        .collect();
    out.push_str(&format!(
        "## Denominator, and why the percentage is not comparable across changes to it\n\n\
Green is **{green_total} of {total}**, which is **{percent:.2}%** — over THIS population and no \
other. The population is every combination the manifest declares, and it is composed of:\n\n\
- backends: {}\n\
- modes: {}\n\n\
⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, \
without anything about the product changing.** Removing a backend whose cells are mostly red \
RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before \
comparing this percentage against an earlier one, diff the two lists above: if they differ, the \
numbers are not comparable and the difference is not a result.\n\n",
        ordered
            .iter()
            .map(|b| format!("`{b}`"))
            .collect::<Vec<_>>()
            .join(", "),
        modes
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    out.push_str(
        "The mode view makes the current order of work explicit: expand `verify` first, then \
`replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does \
not exist for that backend.\n\n| Mode",
    );
    for backend in &ordered {
        out.push_str(&format!(" | `{backend}`"));
    }
    out.push_str(" | Green | Red | Total |\n| ---");
    for _ in &ordered {
        out.push_str(" | ---:");
    }
    out.push_str(" | ---: | ---: | ---: |\n");
    for mode in ["verify", "replay", "chaos", "naked"] {
        let mode_total = derived
            .population
            .iter()
            .filter(|id| id.mode == mode)
            .count();
        let mode_green = derived.green.iter().filter(|id| id.mode == mode).count();
        out.push_str(&format!("| `{mode}`"));
        for backend in &ordered {
            let cell_total = derived
                .population
                .iter()
                .filter(|id| id.mode == mode && id.backend == *backend)
                .count();
            if cell_total == 0 {
                out.push_str(" | —");
            } else {
                let cell_green = derived
                    .green
                    .iter()
                    .filter(|id| id.mode == mode && id.backend == *backend)
                    .count();
                out.push_str(&format!(" | {cell_green} / {cell_total}"));
            }
        }
        out.push_str(&format!(
            " | {mode_green} | {} | {mode_total} |\n",
            mode_total - mode_green
        ));
    }
    out.push_str(&format!(
        "| **Total** | | | | | | | **{green_total}** | **{}** | **{total}** |\n\n",
        total - green_total
    ));
    out.push_str(
        "## Cross-backend parity\n\n\
The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, \
a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. \
Standalone backend gates exercise selected comparisons, but their results are not counted here. \
Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table \
reports no cross-backend parity number.\n\n\
## Ptrace by manifest category\n\n\
This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace \
workload mix visible. Each entry is `green / total`; `custom` commands are not part of this \
denominator.\n\n\
| Manifest category | Verify | Replay | Chaos | Green | Total |\n\
| --- | ---: | ---: | ---: | ---: | ---: |\n",
    );
    let categories: BTreeSet<&str> = derived
        .population
        .iter()
        .filter(|id| id.backend == "ptrace")
        .map(|id| id.category.as_str())
        .collect();
    for category in categories {
        let category_cells: Vec<_> = derived
            .population
            .iter()
            .filter(|id| id.backend == "ptrace" && id.category == category)
            .collect();
        let category_green = category_cells
            .iter()
            .filter(|id| derived.green.contains(**id))
            .count();
        out.push_str(&format!("| `{category}`"));
        for mode in ["verify", "replay", "chaos"] {
            let mode_total = category_cells.iter().filter(|id| id.mode == mode).count();
            let mode_green = category_cells
                .iter()
                .filter(|id| id.mode == mode && derived.green.contains(**id))
                .count();
            out.push_str(&format!(" | {mode_green} / {mode_total}"));
        }
        out.push_str(&format!(
            " | {category_green} | {} |\n",
            category_cells.len()
        ));
    }
    out.push('\n');
    let chaos = derived
        .selected
        .iter()
        .filter(|id| id.mode == "chaos")
        .count();
    let custom = derived
        .selected
        .iter()
        .filter(|id| id.mode == "custom")
        .count();
    out.push_str(&format!(
        "Ordinary full validation executes {} selected regression cells: the {green_total} green \
compatibility cells above (including {chaos} chaos-mode race-exposure checks), and {custom} \
explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for \
all of them; a failing green cell is a regression, not permission to move it to red.\n",
        derived.selected.len()
    ));
    out
}

fn tracked_from(
    derived: &Derived,
    existing: Option<TrackedCells>,
    allow_green_removal: bool,
    allow_cell_removal: bool,
) -> Result<TrackedCells, String> {
    let mut previous = BTreeMap::new();
    let previous_projection = existing.as_ref().and_then(|file| file.projection.clone());
    if let Some(existing) = existing {
        if !(1..=SCHEMA).contains(&existing.schema) {
            return Err(format!(
                "unsupported tracked cell schema {}",
                existing.schema
            ));
        }
        for cell in existing.cells {
            if previous.insert(cell.id.clone(), cell).is_some() {
                return Err("tracked cell file contains a duplicate identity".into());
            }
        }
    }

    let removed: Vec<_> = previous
        .keys()
        .filter(|id| !derived.population.contains(*id))
        .cloned()
        .collect();
    if !removed.is_empty() && !allow_cell_removal {
        return Err(format!(
            "refusing to delete {} tracked cell(s); first is {}. Re-run update with \
             --allow-cell-removal only for an intentional reviewed denominator change",
            removed.len(),
            display_id(&removed[0])
        ));
    }
    let regressed: Vec<_> = previous
        .values()
        .filter(|cell| {
            cell.status == CellStatus::Green
                && derived.population.contains(&cell.id)
                && !derived.green.contains(&cell.id)
        })
        .map(|cell| cell.id.clone())
        .collect();
    if !regressed.is_empty() && !allow_green_removal {
        return Err(format!(
            "refusing to move {} green cell(s) to red; first is {}. Fix the regression, or use \
             --allow-green-removal only at an explicit compatibility-standard transition",
            regressed.len(),
            display_id(&regressed[0])
        ));
    }


    let cells = derived
        .population
        .iter()
        .cloned()
        .map(|id| {
            let observations = previous
                .get(&id)
                .map(|cell| cell.observations.clone())
                .unwrap_or_default();
            // Preserved, like observations, for the same reason: ordinary
            // derivation recomputes STATUS from the manifest and the plan, and
            // must not discard measured evidence while doing so.
            let last_tested = previous.get(&id).and_then(|cell| cell.last_tested.clone());
            let status = if derived.green.contains(&id) {
                CellStatus::Green
            } else {
                CellStatus::Red
            };
            let enabled = derived.enabled.contains(&id);
            let ci_disabled_reason = derived.ci_disabled_reasons.get(&id).cloned();
            TrackedCell {
                id,
                enabled,
                status,
                ci_disabled_reason,
                last_tested,
                observations,
                measurement: MeasurementState::NeverMeasured,
            }
        })
        .collect();
    Ok(TrackedCells {
        schema: SCHEMA,
        // ⚠️ CARRIED FORWARD, NOT RESET. `update` is the ratchet authority; the
        // projection block belongs to the observation writers. Rebuilding it as
        // `None` here silently deleted it on every ordinary derivation, so the
        // file stopped saying when it was last projected and a stale projection
        // became indistinguishable from a fresh one -- the same failure the
        // block exists to prevent. Found by running `update` after
        // `project-observations`, not by reading this function.
        projection: previous_projection,
        cells,
    })
}

/// Which command is writing, for `enforce_writer_boundary`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Writer {
    /// `update`: owns the manifest-derived ratchet fields.
    Update,
    /// `update-observations` and `observe-results`: own `observations` only.
    Observations,
}

/// REFUSE A WRITE THAT CROSSED THE WRITER BOUNDARY.
///
/// `cells.json` has two authorities living in one tool, and until this existed
/// the split was correct only by convention. `update` derives the ratchet from
/// the manifest and the expected plan; the observation writers merge measured
/// evidence. Neither may touch the other's fields, and a future edit that
/// quietly crossed over would corrupt either the ratchet or the evidence with
/// nothing to catch it.
///
/// This compares the file BEFORE and AFTER rather than auditing each mutation
/// site, so it keeps holding as the code moves. It is deliberately a refusal,
/// not a warning: a boundary that can be crossed with a printed complaint is
/// not a boundary.
fn enforce_writer_boundary(
    before: &TrackedCells,
    after: &TrackedCells,
    writer: Writer,
) -> Result<(), String> {
    let index = |cells: &TrackedCells| -> BTreeMap<CellId, TrackedCell> {
        cells
            .cells
            .iter()
            .map(|cell| (cell.id.clone(), cell.clone()))
            .collect()
    };
    let (old, new) = (index(before), index(after));

    // ⚠️ THE PROJECTION BLOCK IS OBSERVATION-AUTHORITY STATE, so `update` may
    // not silently drop it. It did exactly that before this check existed:
    // `tracked_from` rebuilt the file with `projection: None`, so every
    // ordinary derivation deleted the record of when the observations were last
    // projected, and a stale projection became indistinguishable from a fresh
    // one. Found by running `update` after `project-observations` and looking
    // at the file, which is the only way it shows up.
    if writer == Writer::Update && before.projection.is_some() && after.projection.is_none() {
        return Err(
            "writer boundary violated: `update` dropped the observation projection block.              That block records when the observations were last derived from the series;              deleting it makes a stale projection read as a current one"
                .into(),
        );
    }

    // ⚠️ THE DERIVED FIELD MUST NEVER DISAGREE WITH THE EVIDENCE IT SUMMARISES,
    // and this is checked for EVERY writer rather than split between them.
    // `measurement` is a cache of `derive_measurement`, so a row whose stored
    // value differs from its own observations is a row that lies to any consumer
    // reading it instead of recomputing -- which is the entire reason the field
    // exists. Checking it here, at the one boundary both writers already pass
    // through, is what makes it impossible to write a disagreeing value at all
    // rather than merely discouraged.
    for (id, cell) in &new {
        let derived = derive_measurement(cell);
        if cell.measurement != derived {
            return Err(format!(
                "writer boundary violated: {}/{}/{} stores measurement `{}` but its \
                 own evidence derives `{}`. `measurement` is derived from \
                 `observations`; it is never set independently.",
                id.test,
                id.mode,
                id.backend,
                cell.measurement.as_str(),
                derived.as_str()
            ));
        }
    }

    match writer {
        Writer::Update => {
            // `update` legitimately adds and removes cells when the manifest
            // changes, so only cells present on BOTH sides are compared. What it
            // must never do is alter measured evidence.
            for (id, old_cell) in &old {
                let Some(new_cell) = new.get(id) else { continue };
                if old_cell.observations != new_cell.observations {
                    return Err(format!(
                        "writer boundary violated: `update` changed observations on                          {}/{}/{}. Observations are owned by `update-observations`                          and `observe-results`; `update` may only carry them forward                          verbatim.",
                        id.test, id.mode, id.backend
                    ));
                }
            }
            // A cell the manifest has just introduced cannot already carry
            // evidence; that would mean evidence was invented rather than measured.
            for (id, new_cell) in &new {
                if !old.contains_key(id) && !new_cell.observations.is_empty() {
                    return Err(format!(
                        "writer boundary violated: `update` created {}/{}/{} already                          carrying observations. A new cell has never been measured.",
                        id.test, id.mode, id.backend
                    ));
                }
            }
        }
        Writer::Observations => {
            // An observation writer merges evidence into cells that already
            // exist. It may not change the population, and it may not touch a
            // single ratchet field.
            if old.len() != new.len() {
                return Err(format!(
                    "writer boundary violated: an observation writer changed the cell                      population from {} to {}. Only `update` may add or remove cells.",
                    old.len(),
                    new.len()
                ));
            }
            for (id, old_cell) in &old {
                let Some(new_cell) = new.get(id) else {
                    return Err(format!(
                        "writer boundary violated: an observation writer removed                          {}/{}/{}.",
                        id.test, id.mode, id.backend
                    ));
                };
                let changed = |field: &str| {
                    format!(
                        "writer boundary violated: an observation writer changed `{field}`                          on {}/{}/{}. That field is owned by `update`, which derives it                          from the manifest and the expected plan.",
                        id.test, id.mode, id.backend
                    )
                };
                if old_cell.enabled != new_cell.enabled {
                    return Err(changed("enabled"));
                }
                if old_cell.status != new_cell.status {
                    return Err(changed("status"));
                }
                if old_cell.ci_disabled_reason != new_cell.ci_disabled_reason {
                    return Err(changed("ci_disabled_reason"));
                }
            }
        }
    }
    if before.schema != after.schema && writer == Writer::Observations {
        return Err(
            "writer boundary violated: an observation writer changed the schema version"
                .into(),
        );
    }
    Ok(())
}

fn load_existing(root: &Path) -> Result<Option<TrackedCells>, String> {
    let path = root.join(CELLS);
    if !path.exists() {
        return Ok(None);
    }
    read_json(&path).map(Some)
}

fn encoded_cells(cells: &TrackedCells) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(cells)
        .map_err(|e| format!("cannot serialize tracked cells: {e}"))?;
    text.push('\n');
    Ok(text)
}

fn check_tracked(root: &Path) -> Result<Derived, String> {
    let derived = derive(root)?;
    // ORDER MATTERS, AND IT IS THE FIX FOR A MISDIRECTING FAILURE. `tracked_from` runs FIRST
    // because it is the only step that can name a SEMANTIC cause -- a green cell regressing to
    // red, or a tracked cell disappearing. `compare_file(SCORECARD)` can only ever say "stale;
    // run `update`". Both files derive from the plan, so a plan edit makes BOTH fail at once;
    // with the comparison first, the operator was told to run `update`, and `update` then
    // refused the green regression. Measured 2026-08-25: dropping one green cell from
    // ci/expected-e2e-plan.json produced `check` -> "SCORECARD.md is stale; run update" and
    // `update` -> "refusing to move 1 green cell(s) to red". Following the instruction the tool
    // itself printed could not clear it; the real remedy, --allow-green-removal, was named
    // nowhere in the message the operator actually saw. Deleting a cell already reported its own
    // cause correctly, because that path has no SCORECARD.md difference to mask it -- which is
    // why this looked intermittent rather than ordered.
    let mut cells = tracked_from(&derived, load_existing(root)?, false, false)?;
    let expected_scorecard = render_scorecard(&derived);
    compare_file(&root.join(SCORECARD), &expected_scorecard)?;
    // The WRITE path applies this before serialising (see `update_tracked`), so the READ path
    // must too or the two derive different bytes from the same inputs and `check` reports a
    // staleness that `update` cannot clear. Measured 2026-08-25: `update` was a fixed point --
    // three consecutive runs left cells.json byte-identical and exited 0 -- while `check`
    // exited 2 every time telling the operator to run `update`. That made gate.manifest
    // UNSATISFIABLE, and because the gate truncates the lane it took 58 nodes with it on every
    // run. The asymmetry only became reachable once observations were non-empty for the first
    // time, which is why it appeared tonight rather than when it was introduced.
    refresh_measurement(&mut cells);
    compare_file(&root.join(CELLS), &encoded_cells(&cells)?)?;
    Ok(derived)
}

fn compare_file(path: &Path, expected: &str) -> Result<(), String> {
    let actual = fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read tracked {}: {e}; run `./ci/compat-envelope/scorecard.rs update`",
            path.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{} is stale; run `./ci/compat-envelope/scorecard.rs update` and review the diff",
            path.display()
        ));
    }
    Ok(())
}

fn update_tracked(
    root: &Path,
    allow_green_removal: bool,
    allow_cell_removal: bool,
) -> Result<(), String> {
    let derived = derive(root)?;
    let existing = load_existing(root)?;
    let mut cells = tracked_from(
        &derived,
        existing.clone(),
        allow_green_removal,
        allow_cell_removal,
    )?;
    refresh_measurement(&mut cells);
    if let Some(before) = existing.as_ref() {
        enforce_writer_boundary(before, &cells, Writer::Update)?;
    }
    fs::write(root.join(SCORECARD), render_scorecard(&derived))
        .map_err(|e| format!("cannot write {SCORECARD}: {e}"))?;
    fs::write(root.join(CELLS), encoded_cells(&cells)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    println!(
        "compatibility scorecard: wrote {} green / {} red / {} total",
        derived.green.len(),
        derived.population.len() - derived.green.len(),
        derived.population.len()
    );
    Ok(())
}

/// Resolve `git rev-list` depths for one repository, or `None` if it is not a
/// resolvable git checkout.
fn repo_depth(root: &Path) -> Option<SourceDepth> {
    let count = |args: &[&str]| -> Option<u64> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()?.trim().parse::<u64>().ok()
    };
    Some(SourceDepth {
        commits: count(&["rev-list", "--count", "HEAD"])?,
        first_parent: count(&["rev-list", "--count", "--first-parent", "HEAD"])?,
    })
}

/// Depths for every repository whose history is part of what was measured.
///
/// Hermit is mandatory -- the tool runs inside it, so a failure there is a
/// genuine fault rather than a layout difference. Reverie is best-effort: it is
/// a pinned git dependency, not a checkout hermit owns, so whether a clone is
/// reachable depends on the surrounding workspace. When it is not, the key is
/// OMITTED and the caller says so, rather than a zero being recorded as if it
/// were a measurement.
fn source_depths(root: &Path) -> Result<BTreeMap<String, SourceDepth>, String> {
    let mut depths = BTreeMap::new();
    let hermit = repo_depth(root)
        .ok_or("cannot read hermit git depth; this tool runs inside that repository")?;
    depths.insert("hermit".to_string(), hermit);
    // Sibling first, then the dev-hermit parent layout where hermit checkouts
    // live under worktrees/<slot>/hermit and reverie sits at the top level.
    // Best-effort by design: the absence of a reverie clone is a property of
    // the workspace, not a fault, so it is reported and omitted rather than
    // guessed at.
    for candidate in [
        "../reverie",
        "../../reverie",
        "../../../reverie",
        "../../../../reverie",
    ] {
        let path = root.join(candidate);
        if path.join(".git").exists() {
            if let Some(depth) = repo_depth(&path) {
                depths.insert("reverie".to_string(), depth);
                break;
            }
        }
    }
    Ok(depths)
}

/// Report how much of the grid carries recorded evidence, and how much of that
/// evidence still describes the CURRENT code.
///
/// This exists so an empty field is never mistaken for a measurement. Absence of
/// `last_tested` means NO WRITER RECORDED ONE, not that the cell was never
/// exercised, and the only way to keep that honest is to state the coverage out
/// loud every time the table is printed.
fn render_evidence_coverage(root: &Path) -> Result<String, String> {
    let Some(tracked) = load_existing(root)? else {
        return Ok(String::new());
    };
    let head_tree = git_rev_parse(root, "HEAD:detcore").ok();
    let total = tracked.cells.len();
    let stamped = tracked
        .cells
        .iter()
        .filter(|cell| cell.last_tested.is_some())
        .count();
    let (mut current, mut stale) = (0usize, 0usize);
    for cell in &tracked.cells {
        let Some(last) = &cell.last_tested else {
            continue;
        };
        match head_tree.as_deref() {
            Some(head) if head == last.detcore_tree => current += 1,
            Some(_) => stale += 1,
            None => {}
        }
    }
    let observed = tracked
        .cells
        .iter()
        .filter(|cell| !cell.observations.is_empty())
        .count();
    let stale_observations = tracked
        .cells
        .iter()
        .flat_map(|cell| cell.observations.iter())
        .filter(|observation| {
            head_tree
                .as_deref()
                .is_some_and(|head| head != observation.detcore_tree)
        })
        .count();

    let mut out = String::new();
    out.push_str("\nRecorded evidence (not part of the green/red verdict)\n");
    out.push_str(&format!(
        "  cells with a recorded last test : {stamped} of {total}\n"
    ));
    if stamped < total {
        out.push_str(concat!(
            "      the remainder have NO RECORD, which is not the same as never tested:\n",
            "      only the two explicit fold commands write it, and validate runs only\n",
            "      the selected plan, so red cells stay blank until a pressure-test\n",
            "      campaign covers them.\n",
        ));
    }
    if head_tree.is_some() && stamped > 0 {
        out.push_str(&format!(
            "      of those, {current} still match HEAD:detcore and {stale} are STALE\n"
        ));
        if stale > 0 {
            out.push_str(concat!(
                "      a stale record describes code that has since changed; do not act\n",
                "      on its divergence point without re-measuring.\n",
            ));
        }
    }
    out.push_str(&format!(
        "  cells with a divergence range   : {observed} of {total}\n"
    ));

    // ⚠️ DISAGREEMENT IS INFORMATION, NOT A COLLISION. Owner ruling. A pressure
    // test that stresses a cell harder than validate did and reaches a
    // different result is the system WORKING, so the two provenances are kept
    // in one array and compared here rather than partitioned into places they
    // can never meet. Both results and both sample counts are printed, because
    // "pressure says determinism-failure" means something different at N=1 and
    // at N=40.
    let mut conflicts = Vec::new();
    for cell in &tracked.cells {
        for tree in cell
            .observations
            .iter()
            .map(|o| o.detcore_tree.clone())
            .collect::<BTreeSet<_>>()
        {
            let at = |p: ObservationProvenance| {
                cell.observations
                    .iter()
                    .find(|o| o.detcore_tree == tree && o.provenance == p)
            };
            let (Some(pressure), Some(validate)) = (
                at(ObservationProvenance::PressureTest),
                at(ObservationProvenance::Validate),
            ) else {
                continue;
            };
            if pressure.results != validate.results {
                conflicts.push((display_id(&cell.id), tree.clone(), pressure, validate));
            }
        }
    }
    if !conflicts.is_empty() {
        out.push_str(&format!(
            concat!(
                "\n  !! {} cell(s) where PRESSURE AND VALIDATE DISAGREE at the same tree.\n",
                "      This is a finding, not an error: the pressure test may simply have\n",
                "      stressed the cell harder. Read both results with their sample counts.\n",
            ),
            conflicts.len()
        ));
        for (id, tree, pressure, validate) in conflicts {
            let n = |o: &Observation| {
                o.first_divergent_record
                    .range()
                    .or_else(|| o.first_divergent_scheduler_turn.range())
                    .map(|r| r.samples)
                    .unwrap_or(0)
            };
            out.push_str(&format!(
                "      {id} @ tree {}\n         pressure: {:?} (positions from {} run(s), {} invocation(s))\n         validate: {:?} (positions from {} run(s), {} invocation(s))\n",
                &tree[..12.min(tree.len())],
                pressure.results,
                n(pressure),
                pressure.invocations.len(),
                validate.results,
                n(validate),
                validate.invocations.len(),
            ));
        }
    }
    if stale_observations > 0 {
        out.push_str(&format!(
            "      {stale_observations} observation(s) were taken against a DIFFERENT detcore tree\n"
        ));
    }
    Ok(out)
}

fn default_provenance() -> ObservationProvenance {
    ObservationProvenance::PressureTest
}

// `merge_range` was removed by step 5: a stored range is no longer the
// form being merged into. `ObservedPositions::record` appends the run's
// position and `ObservedPositions::range()` derives the bound on demand.

/// What a fold admitted and what it refused, so the caller can report both.
///
/// `skipped` is carried out rather than logged in place because a fold that
/// drops rows silently is strictly worse than one that refuses everything: the
/// caller cannot then tell a thin batch from a broken one.
struct FoldOutcome {
    /// Distinct cells that received an observation.
    cells: usize,
    /// Rows admitted, which exceeds `cells` when a campaign repeated a cell.
    rows: usize,
    /// (cell identity, joined reasons) for every row that marked itself
    /// untrustworthy.
    skipped: Vec<(String, String)>,
}

fn apply_pressure_summary(
    tracked: &mut TrackedCells,
    summary: &PressureSummary,
    head: &str,
    detcore_tree: &str,
    depth: &BTreeMap<String, SourceDepth>,
) -> Result<FoldOutcome, String> {
    if summary.schema != PRESSURE_SUMMARY_SCHEMA {
        return Err(format!(
            "unsupported pressure summary schema {}",
            summary.schema
        ));
    }
    if summary.source_tree_dirty {
        return Err("dirty pressure results cannot update checked-in observations".into());
    }
    if summary.hermit_sha != head {
        return Err(format!(
            "pressure summary belongs to {}, but HEAD is {head}",
            summary.hermit_sha
        ));
    }
    if summary.detcore_tree != detcore_tree {
        return Err(format!(
            "pressure summary names detcore tree {}, but HEAD contains {detcore_tree}",
            summary.detcore_tree
        ));
    }

    let positions: BTreeMap<_, _> = tracked
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| (cell.id.clone(), index))
        .collect();
    if positions.len() != tracked.cells.len() {
        return Err("tracked cell file contains a duplicate identity".into());
    }

    let mut seen = BTreeSet::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut prepared = Vec::new();
    for row in &summary.rows {
        // Keyed on (cell, repetition): repeats of one cell are the POINT of a
        // stress campaign, while two rows claiming the same repetition are
        // still a malformed summary that would double-count.
        if !seen.insert((row.cell.clone(), row.repetition)) {
            skipped.push((
                display_id(&row.cell),
                format!("duplicate row for repetition {:?}", row.repetition),
            ));
            continue;
        }
        // OPTION B: a row that marks ITSELF untrustworthy is SKIPPED AND NAMED
        // rather than vetoing the whole summary.
        //
        // WHAT THIS GIVES UP, stated plainly because it is a real trade. The
        // old behaviour incidentally protected against a SYSTEMIC fault that
        // damaged some rows detectably and others subtly: the detectable ones
        // vetoed everything, including the quietly-wrong survivors. That
        // protection was never targeted -- it fired by coincidence and it
        // equally voided perfectly good batches, measured at 0 rows admitted
        // from 20 on a real campaign of which 3 were sound.
        //
        // WHAT IT DOES NOT GIVE UP: a row whose artifacts are present but WRONG
        // was admitted under the old behaviour too, because it sets no
        // evidence_errors. B does not widen that surface at all.
        //
        // The systemic case is handled by LOUDNESS instead of by veto: the
        // caller prints every skipped cell with its reasons and both counts, so
        // a batch that skipped 17 of 20 is impossible to mistake for a thin one
        // and an operator can discard it. Silence here would be strictly worse
        // than the old veto, which is why `skipped` is reported and never
        // swallowed.
        //
        // Deliberately placed BEFORE the structural checks below: a row with no
        // readable result row cannot also be expected to carry a well-formed
        // invocation, and reporting the downstream complaint would bury the
        // actual cause.
        if !row.evidence_errors.is_empty() {
            skipped.push((display_id(&row.cell), row.evidence_errors.join("; ")));
            continue;
        }
        let Some(index) = positions.get(&row.cell).copied() else {
            skipped.push((
                display_id(&row.cell),
                "not a cell in the tracked manifest".to_string(),
            ));
            continue;
        };
        // REFUSAL REMOVED BY OWNER RULING. This used to refuse any row whose
        // cell was green, on the grounds that "ordinary validate owns green
        // evidence". Two things were wrong with it. It made the producer and
        // consumer contradict each other outright -- `--repetitions` only
        // repeats GREEN cells, so the one command that can produce a
        // multi-sample range emitted a summary this function rejected on its
        // first row. And it treated a pressure result that disagrees with
        // validate as a collision to be prevented, when it is the most
        // interesting thing the pressure test can produce: stressing a cell
        // harder and finding it red where validate called it green is the
        // system working. Disagreement is surfaced by `show` instead of being
        // refused here.
        let result = match ObservedResult::parse(&row.result) {
            Ok(result) => result,
            Err(why) => {
                skipped.push((display_id(&row.cell), why));
                continue;
            }
        };
        let Some(invocation) = row.invocation.clone() else {
            skipped.push((
                display_id(&row.cell),
                "no literal invocation recorded".to_string(),
            ));
            continue;
        };
        if invocation.run_id.trim().is_empty()
            || invocation.argv.is_empty()
            || invocation.guest_argv.is_empty()
            || invocation.env.is_empty()
            || invocation.cwd.trim().is_empty()
            || invocation.shell_command.trim().is_empty()
            || invocation.shell_command
                != literal_shell_command(&invocation.cwd, &invocation.env, &invocation.argv)
            || invocation.attempts.is_empty()
            || invocation.attempts.iter().any(|attempt| {
                attempt.index.trim().is_empty()
                    || attempt.outcome.trim().is_empty()
                    || attempt.argv.is_empty()
                    || attempt.guest_argv.is_empty()
                    || attempt.env.is_empty()
                    || attempt.cwd.trim().is_empty()
                    || attempt.shell_command.trim().is_empty()
                    || attempt.shell_command
                        != literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv)
            })
            || invocation.attempts.first().is_some_and(|attempt| {
                attempt.argv != invocation.argv
                    || attempt.guest_argv != invocation.guest_argv
                    || attempt.env != invocation.env
                    || attempt.cwd != invocation.cwd
                    || attempt.shell_command != invocation.shell_command
            })
        {
            skipped.push((
                display_id(&row.cell),
                "incomplete invocation: a field is empty, or shell_command does not \
                 reconstruct from cwd, env and argv"
                    .to_string(),
            ));
            continue;
        }
        let turn = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_scheduler_turn);
        let virtual_nanoseconds = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_virtual_nanoseconds);
        let divergent_record = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_record);
        let divergent_syscall = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_syscall);
        if !result.carries_divergence_position()
            && (turn.is_some()
                || virtual_nanoseconds.is_some()
                || divergent_record.is_some()
                || divergent_syscall.is_some())
        {
            skipped.push((
                display_id(&row.cell),
                format!("result {} carries a divergence position", row.result),
            ));
            continue;
        }
        prepared.push((
            index,
            result,
            turn,
            virtual_nanoseconds,
            divergent_record,
            divergent_syscall,
            invocation,
        ));
    }

    // ALL-SKIPPED IS A FAILURE, NOT A QUIET SUCCESS. A batch where every row
    // was untrustworthy tells you nothing about any cell, and returning Ok(0)
    // would let a caller print a cheerful "merged 0" that reads as "nothing to
    // do". The error names every skipped cell, so the refusal carries the same
    // detail the success path would have.
    if prepared.is_empty() && !skipped.is_empty() {
        return Err(format!(
            "every one of the {} offered row(s) was untrustworthy, so nothing was merged:\n{}",
            skipped.len(),
            skipped
                .iter()
                .map(|(cell, why)| format!("  {cell}: {why}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    // Distinct from the case above: a summary that OFFERED nothing is a
    // different fault from one whose every row failed, and conflating them
    // would hide an empty campaign behind an evidence complaint.
    if prepared.is_empty() && skipped.is_empty() {
        return Err("pressure summary contains no rows to merge".into());
    }

    let prepared_len = prepared.len();
    let prepared_cells = prepared
        .iter()
        .map(|(index, _, _, _, _, _, _)| *index)
        .collect::<BTreeSet<_>>()
        .len();
    for (index, result, turn, virtual_nanoseconds, divergent_record, divergent_syscall, invocation) in
        prepared
    {
        // Every row here is a cell the pressure test actually exercised,
        // whatever its result, so it gets the same stamp the validate fold
        // applies. This is the ONLY writer that reaches red cells.
        tracked.cells[index].last_tested = Some(LastTested {
            hermit_sha: summary.hermit_sha.clone(),
            detcore_tree: summary.detcore_tree.clone(),
            depth: depth.clone(),
        });
        let observations = &mut tracked.cells[index].observations;
        // Keyed by tree AND provenance. Keying by tree alone would let a
        // validate point and a pressure-test distribution fall into one range.
        let position = observations.iter().position(|observation| {
            observation.detcore_tree == summary.detcore_tree
                && observation.provenance == ObservationProvenance::PressureTest
        });
        let observation = match position {
            Some(position) => &mut observations[position],
            None => {
                observations.push(Observation {
                    detcore_tree: summary.detcore_tree.clone(),
                    provenance: ObservationProvenance::PressureTest,
                    depth: depth.clone(),
                    hermit_shas: BTreeSet::new(),
                    results: BTreeSet::new(),
                    invocations: BTreeSet::new(),
                    first_divergent_scheduler_turn: ObservedPositions::default(),
                    first_divergent_virtual_nanoseconds: ObservedPositions::default(),
                    first_divergent_record: ObservedPositions::default(),
                    first_divergent_syscall: ObservedPositions::default(),
                });
                observations.last_mut().expect("observation was appended")
            }
        };
        observation.hermit_shas.insert(summary.hermit_sha.clone());
        observation.results.insert(result);
        observation.invocations.insert(ObservedInvocation {
            hermit_sha: summary.hermit_sha.clone(),
            run_id: invocation.run_id,
            result,
            argv: invocation.argv,
            guest_argv: invocation.guest_argv,
            env: invocation.env,
            cwd: invocation.cwd,
            shell_command: invocation.shell_command,
            attempts: invocation.attempts,
        });
        observation.first_divergent_scheduler_turn.record(turn);
        observation.first_divergent_virtual_nanoseconds.record(virtual_nanoseconds);
        observation.first_divergent_record.record(divergent_record);
        observation.first_divergent_syscall.record(divergent_syscall);
        // Sort by the full key, so a tree carrying both a pressure-test and a
        // validate observation still has a stable tracked-file order.
        observations.sort_by(|left, right| {
            left.detcore_tree
                .cmp(&right.detcore_tree)
                .then(left.provenance.cmp(&right.provenance))
        });
    }
    // DISTINCT ADMITTED CELLS, not rows. Skipped rows must not inflate this
    // count; the caller reports admitted rows, admitted cells, and skips separately.
    Ok(FoldOutcome {
        cells: prepared_cells,
        rows: prepared_len,
        skipped,
    })
}

/// Classify one validate row into the observed-result taxonomy.
///
/// Validate rows carry PASS/FAIL/ERROR, which is a coarser vocabulary than the
/// pressure summary's. The mapping is by MODE, because a divergence in `verify`
/// and a divergence in `replay` are different findings and collapsing them
/// would make the observed result set say less than the row already knew.
///
/// ERROR is refused rather than mapped. It is an infrastructure state, and the
/// pressure path already refuses `infrastructure-error` with the same
/// reasoning: storing it as product behaviour would record a fault against the
/// cell.
///
/// There is deliberately no arm producing `ParityFailure`. No parity MODE
/// exists in the cell grid today -- the modes are verify, replay, chaos and
/// naked -- so validate cannot observe one, and inventing a mapping would
/// manufacture a result the run never established.
fn validate_row_result(row: &ResultRow) -> Result<ObservedResult, String> {
    match (row.outcome.as_str(), row.mode.as_str()) {
        ("PASS", _) => Ok(ObservedResult::Pass),
        ("FAIL", "replay") => Ok(ObservedResult::ReplayFailure),
        ("FAIL", _) => Ok(ObservedResult::DeterminismFailure),
        ("ERROR", _) => Err(format!(
            "{} is an infrastructure ERROR; refusing to store it as product behavior",
            row.test
        )),
        (other, _) => Err(format!("unknown validate outcome `{other}` for {}", row.test)),
    }
}

/// Fold VALIDATE rows into the tracked observations under the `validate`
/// provenance.
///
/// This is a SEPARATE ENTRY POINT and not something ordinary validation does.
/// `ci/compat-envelope/README.md` states that normal validation changes no
/// tracked scorecard file, and that invariant is preserved: a run only reaches
/// here when someone explicitly asks it to.
///
/// WHAT THESE BOUNDS MEAN, AND WHAT THEY DO NOT. Validate runs a cell once per
/// commit, so a validate observation at one tree is a POINT, not a
/// distribution. Its `samples` will read 1 until the same tree is validated
/// again. Only the pressure test repeats a cell at a fixed tree, so only its
/// bounds describe flakiness. They are stored under different provenance keys
/// precisely so nobody reads one as the other.
fn apply_validate_results(
    tracked: &mut TrackedCells,
    rows: &BTreeMap<CellId, Vec<ResultCandidate>>,
    hermit_sha: &str,
    detcore_tree: &str,
    depth: &BTreeMap<String, SourceDepth>,
) -> Result<usize, String> {
    let mut updated = 0usize;
    for (id, candidates) in rows {
        let Some(index) = tracked.cells.iter().position(|cell| &cell.id == id) else {
            continue;
        };
        for candidate in candidates {
            let row = &candidate.row;
            // Stamped BEFORE the divergence check below, deliberately. A cell
            // that PASSED was still exercised, and if this only ran for
            // diverging cells then "green and checked" would be indistinguishable
            // from "never checked" -- which is the exact confusion this field
            // exists to remove.
            tracked.cells[index].last_tested = Some(LastTested {
                hermit_sha: hermit_sha.to_string(),
                detcore_tree: detcore_tree.to_string(),
                depth: depth.clone(),
            });
            // Nothing to record. Skipping rather than folding a no-op keeps a
            // cell that has only ever passed free of an empty observation,
            // which would otherwise be indistinguishable from one that was
            // measured and located nothing.
            if row.first_divergent_scheduler_turn.is_none()
                && row.first_divergent_virtual_nanoseconds.is_none()
                && row.first_divergent_record.is_none()
                && row.first_divergent_syscall.is_none()
            {
                continue;
            }
            let result = validate_row_result(row)?;
            // Same integrity check the pressure path applies to its
            // invocations. A shell_command that does not reconstruct from cwd,
            // env and argv is not a pasteable reproduction, and recording it as
            // one would be worse than recording nothing.
            if row.shell_command != literal_shell_command(&row.cwd, &row.env, &row.argv) {
                return Err(format!(
                    "{}: shell_command does not reconstruct from cwd, env and argv",
                    display_id(id)
                ));
            }
            let attempt_invocations = row
                .attempts
                .iter()
                .map(|attempt| {
                    serde_json::from_value::<ObservedAttemptInvocation>(attempt.clone()).map_err(
                        |e| format!("{}: unreadable attempt record: {e}", display_id(id)),
                    )
                })
                .collect::<Result<Vec<_>, String>>()?;
            // A row that located a divergence but recorded no attempt is
            // self-contradictory: something ran to produce that position. The
            // pressure path refuses the same shape.
            if attempt_invocations.is_empty() {
                return Err(format!(
                    "{} located a divergence but recorded no attempt",
                    display_id(id)
                ));
            }
            if !result.carries_divergence_position() {
                return Err(format!(
                    "{} reports {} yet carries a divergence position",
                    display_id(id),
                    row.outcome
                ));
            }
            let observations = &mut tracked.cells[index].observations;
            let position = observations.iter().position(|observation| {
                observation.detcore_tree == detcore_tree
                    && observation.provenance == ObservationProvenance::Validate
            });
            let observation = match position {
                Some(position) => &mut observations[position],
                None => {
                    observations.push(Observation {
                        detcore_tree: detcore_tree.to_string(),
                        provenance: ObservationProvenance::Validate,
                        depth: depth.clone(),
                        hermit_shas: BTreeSet::new(),
                        results: BTreeSet::new(),
                        invocations: BTreeSet::new(),
                        first_divergent_scheduler_turn: ObservedPositions::default(),
                        first_divergent_virtual_nanoseconds: ObservedPositions::default(),
                        first_divergent_record: ObservedPositions::default(),
                        first_divergent_syscall: ObservedPositions::default(),
                    });
                    observations.last_mut().expect("observation was appended")
                }
            };
            observation.hermit_shas.insert(hermit_sha.to_string());
            observation.results.insert(result);
            // Record the invocation, exactly as the pressure path does. Without
            // it a validate-sourced bound would have strictly WORSE provenance
            // than a pressure-sourced one: no per-run record, no run_id, and no
            // pasteable command to reproduce the divergence it reports.
            observation.invocations.insert(ObservedInvocation {
                hermit_sha: row.hermit_sha.clone(),
                run_id: row.run_id.clone(),
                result,
                argv: row.argv.clone(),
                guest_argv: row.guest_argv.clone(),
                env: row.env.clone(),
                cwd: row.cwd.clone(),
                shell_command: row.shell_command.clone(),
                attempts: attempt_invocations,
            });
            observation.first_divergent_scheduler_turn.record(row.first_divergent_scheduler_turn);
            observation.first_divergent_virtual_nanoseconds.record(row.first_divergent_virtual_nanoseconds);
            observation.first_divergent_record.record(row.first_divergent_record);
            observation.first_divergent_syscall.record(row.first_divergent_syscall);
            observations.sort_by(|left, right| {
                left.detcore_tree
                    .cmp(&right.detcore_tree)
                    .then(left.provenance.cmp(&right.provenance))
            });
            updated += 1;
        }
    }
    Ok(updated)
}

fn observe_results(root: &Path, results: &Path) -> Result<(), String> {
    check_tracked(root)?;
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect working tree: {e}"))?;
    if !status.status.success() {
        return Err("git status failed while checking the working tree".into());
    }
    if !status.stdout.is_empty() {
        return Err("observe-results requires a clean tracked working tree".into());
    }
    let head = git_head(root)?;
    let detcore_tree = git_rev_parse(root, "HEAD:detcore")?;
    let rows = read_result_candidates(results, &head)?;
    let depth = source_depths(root)?;
    if !depth.contains_key("reverie") {
        println!(
            "  note: no reverie checkout reachable, so reverie depth is OMITTED \
             rather than guessed. Hermit depth is recorded."
        );
    }
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let before = tracked.clone();
    let updated = apply_validate_results(&mut tracked, &rows, &head, &detcore_tree, &depth)?;
    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    fs::write(root.join(CELLS), encoded_cells(&tracked)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    println!(
        "compatibility scorecard: merged {} {} divergence position(s) at {head}",
        updated,
        ObservationProvenance::Validate.as_str()
    );
    if updated == 0 {
        println!(
            "  no row located a divergence. That is the expected result for an \
             all-green run and is NOT evidence that the field is unpopulated."
        );
    }
    Ok(())
}

fn update_observations(root: &Path, summary_path: &Path) -> Result<(), String> {
    check_tracked(root)?;
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect working tree: {e}"))?;
    if !status.status.success() {
        return Err("git status failed while checking the working tree".into());
    }
    if !status.stdout.is_empty() {
        return Err("update-observations requires a clean tracked working tree".into());
    }

    let summary: PressureSummary = read_json(summary_path)?;
    let head = git_head(root)?;
    let detcore_tree = git_rev_parse(root, "HEAD:detcore")?;
    let depth = source_depths(root)?;
    if !depth.contains_key("reverie") {
        println!(
            "  note: no reverie checkout reachable, so reverie depth is OMITTED \
             rather than guessed. Hermit depth is recorded."
        );
    }
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let before = tracked.clone();
    let outcome = apply_pressure_summary(&mut tracked, &summary, &head, &detcore_tree, &depth)?;
    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    fs::write(root.join(CELLS), encoded_cells(&tracked)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    println!(
        "compatibility scorecard: merged {} row(s) across {} cell(s) at {head}; {} row(s) skipped",
        outcome.rows,
        outcome.cells,
        outcome.skipped.len()
    );
    // NAMED INDIVIDUALLY, NEVER JUST COUNTED. A fold that drops rows silently
    // is worse than one that refuses everything, because the caller cannot then
    // tell a thin batch from a broken one. A high skip ratio here is the signal
    // that something systemic went wrong with the campaign.
    for (cell, why) in &outcome.skipped {
        println!("  skipped {cell}: {why}");
    }
    Ok(())
}

/// Re-derive the observation projection from the series store.
///
/// ⚠️ WHAT THIS DOES AND DOES NOT CLAIM TODAY. The series store is the
/// authority for divergence positions; `cells.json` holds a projection of it so
/// readers and the ratchet do not need the parent repository. Plan step 4 --
/// the producer that writes series rows -- HAS NOT LANDED, so on current main
/// every invocation of this reads zero rows and every observation in the file
/// is still pre-series evidence. That is why the refusal below is the part that
/// matters right now: it is the thing standing between an empty source and the
/// only located divergence coordinates the repository has.
fn project_observations(
    root: &Path,
    series_root: &Path,
    refreshed_at: &str,
) -> Result<(), String> {
    check_tracked(root)?;
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let before = tracked.clone();

    let (rows, skipped) = read_series_rows(series_root)?;
    let mut by_cell: BTreeMap<String, Vec<&SeriesRow>> = BTreeMap::new();
    for row in &rows {
        by_cell.entry(row.cell.clone()).or_default().push(row);
    }

    let mut projected = 0usize;
    for cell in &mut tracked.cells {
        let Some(cell_rows) = by_cell.get(&display_id(&cell.id)) else {
            continue;
        };
        for observation in &mut cell.observations {
            // ⚠️ ALL FOUR COORDINATES OR NONE. Projecting a subset leaves one
            // observation holding series-derived bounds beside pre-series ones
            // with nothing in the row saying which is which -- two authorities
            // in one record, which is the arrangement this whole step exists to
            // end. Caught by inventorying the checked-in file: it carries
            // `first_divergent_record` bounds that a two-coordinate projection
            // would have silently left behind as stale.
            observation.first_divergent_scheduler_turn = ObservedPositions::default();
            observation.first_divergent_virtual_nanoseconds = ObservedPositions::default();
            observation.first_divergent_record = ObservedPositions::default();
            observation.first_divergent_syscall = ObservedPositions::default();
            for row in cell_rows {
                observation
                    .first_divergent_scheduler_turn
                    .record(row.first_divergent_scheduler_turn);
                observation
                    .first_divergent_virtual_nanoseconds
                    .record(row.first_divergent_virtual_nanoseconds);
                observation
                    .first_divergent_record
                    .record(row.first_divergent_record);
                observation
                    .first_divergent_syscall
                    .record(row.first_divergent_syscall);
            }
        }
        projected += 1;
    }

    let rows_read = rows.len() as u64;
    tracked.projection = Some(ObservationProjection {
        source: series_root.display().to_string(),
        refreshed_at: refreshed_at.to_string(),
        rows_read,
        pre_series_corpus: rows_read == 0,
    });
    refresh_measurement(&mut tracked);

    // The guard runs BEFORE the write, not after. A projection that has already
    // hit the disk is a projection someone has to notice and revert.
    enforce_projection_preserves_evidence(&before, &tracked, rows_read)?;
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    fs::write(root.join(CELLS), encoded_cells(&tracked)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;

    println!(
        "compatibility scorecard: projected {projected} cell(s) from {rows_read} series row(s) \
         under {}",
        series_root.display()
    );
    if rows_read == 0 {
        println!(
            "  note: the series is EMPTY, so every observation here remains PRE-SERIES \
             evidence rather than a projection. This is the expected state until plan step 4 \
             lands a producer."
        );
    }
    for line in &skipped {
        println!("  skipped {line}");
    }
    Ok(())
}

/// One divergence-position row as the series store carries it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeriesRow {
    cell: String,
    #[serde(default)]
    first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    first_divergent_virtual_nanoseconds: Option<u64>,
    #[serde(default)]
    first_divergent_record: Option<u64>,
    #[serde(default)]
    first_divergent_syscall: Option<u64>,
}

/// Read every series shard under `series_root`.
///
/// A line that does not parse is NAMED AND SKIPPED rather than aborting the
/// refresh or vanishing: one malformed row should not deny the projection every
/// other row supports, and a row that disappears without a word is how a thin
/// projection gets mistaken for a complete one.
fn read_series_rows(series_root: &Path) -> Result<(Vec<SeriesRow>, Vec<String>), String> {
    if !series_root.exists() {
        return Err(format!(
            "series root {} does not exist. An unreachable source is REFUSED rather than \
             treated as an empty one -- those are different facts, and only one of them is \
             a statement about the cells.",
            series_root.display()
        ));
    }
    let mut shards: Vec<PathBuf> = Vec::new();
    collect_shards(series_root, &mut shards)?;
    shards.sort();
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for shard in shards {
        let text = fs::read_to_string(&shard)
            .map_err(|e| format!("cannot read series shard {}: {e}", shard.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SeriesRow>(line) {
                Ok(row) => rows.push(row),
                Err(why) => {
                    skipped.push(format!("{}:{}: {why}", shard.display(), index + 1));
                }
            }
        }
    }
    Ok((rows, skipped))
}

fn collect_shards(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_shards(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_results(root: &Path, result_root: &Path, lanes: &BTreeSet<String>) -> Result<(), String> {
    let derived = derive(root)?;
    let head = git_head(root)?;
    let expected: BTreeSet<_> = derived
        .selected
        .iter()
        .filter(|id| lanes.contains(&id.lane))
        .cloned()
        .collect();
    if expected.is_empty() {
        return Err("selected lanes contain no regression cells".into());
    }
    let candidates = read_result_candidates(result_root, &head)?;
    let admitted = verify_candidate_set(&expected, candidates)?;
    if admitted != expected.len() {
        return Err(format!(
            "result admission counted {admitted} cells, expected {}",
            expected.len()
        ));
    }

    print!("{}", render_scorecard(&derived));
    let green_checked = expected
        .iter()
        .filter(|id| derived.green.contains(*id))
        .count();
    let chaos_checked = expected.iter().filter(|id| id.mode == "chaos").count();
    let custom_checked = expected.iter().filter(|id| id.mode == "custom").count();
    println!();
    println!(
        "{}",
        fresh_result_summary(
            expected.len(),
            &head,
            green_checked,
            chaos_checked,
            custom_checked,
        )
    );
    println!("Result directory: {}", result_root.display());
    Ok(())
}

fn fresh_result_summary(
    selected: usize,
    head: &str,
    green: usize,
    chaos: usize,
    custom: usize,
) -> String {
    format!(
        "Fresh result check: {selected}/{selected} selected cells passed at {head} \
({green} compatibility green, including {chaos} chaos; {custom} custom outside the comparable denominator)."
    )
}

fn git_head(root: &Path) -> Result<String, String> {
    git_rev_parse(root, "HEAD")
}

fn git_rev_parse(root: &Path, revision: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", revision])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot read HEAD: {e}"))?;
    if !output.status.success() {
        return Err(format!("git rev-parse {revision} failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_result_candidates(
    root: &Path,
    head: &str,
) -> Result<BTreeMap<CellId, Vec<ResultCandidate>>, String> {
    if !root.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            root.display()
        ));
    }
    let mut files = Vec::new();
    find_result_files(root, &mut files)?;
    if files.is_empty() {
        return Err(format!("no results.jsonl files under {}", root.display()));
    }
    let mut out: BTreeMap<CellId, Vec<ResultCandidate>> = BTreeMap::new();
    for path in files {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: ResultRow = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), index + 1))?;
            if row.schema != CELL_RESULT_SCHEMA {
                return Err(format!(
                    "{}:{} has result schema {}, expected {}",
                    path.display(),
                    index + 1,
                    row.schema,
                    CELL_RESULT_SCHEMA
                ));
            }
            if row.hermit_sha != head || row.source_tree_dirty {
                return Err(format!(
                    "{}:{} is not a clean result for HEAD {} (sha={}, dirty={})",
                    path.display(),
                    index + 1,
                    head,
                    row.hermit_sha,
                    row.source_tree_dirty
                ));
            }
            if row.classification != "required" {
                continue;
            }
            row.require_provenance()
                .map_err(|error| format!("{}:{} {error}", path.display(), index + 1))?;
            row.require_canonical_pass_evidence().map_err(|error| {
                format!("{}:{} {error}", path.display(), index + 1)
            })?;
            let evidence_identity = row.evidence_identity().map_err(|error| {
                format!("{}:{} {error}", path.display(), index + 1)
            })?;
            let id = row
                .id()
                .ok_or_else(|| format!("{}:{} has no backend", path.display(), index + 1))?;
            out.entry(id).or_default().push(ResultCandidate {
                evidence_identity,
                path: path.clone(),
                row,
            });
        }
    }
    Ok(out)
}

fn find_result_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("cannot read entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            find_result_files(&path, out)?;
        } else if kind.is_file() && entry.file_name() == "results.jsonl" {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_candidate_set(
    expected: &BTreeSet<CellId>,
    candidates: BTreeMap<CellId, Vec<ResultCandidate>>,
) -> Result<usize, String> {
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    let mut admitted = 0usize;
    let mut binary_identities = BTreeMap::<String, Vec<String>>::new();
    for id in expected {
        let Some(rows) = candidates.get(id) else {
            missing.push(display_id(id));
            continue;
        };
        let mut distinct = BTreeMap::new();
        for candidate in rows {
            distinct
                .entry(candidate.evidence_identity.as_str())
                .or_insert(candidate);
        }
        if distinct.len() != 1 {
            let descriptions = distinct
                .values()
                .take(4)
                .map(|candidate| {
                    format!(
                        "{} run={} evidence={}",
                        candidate.path.display(),
                        candidate.row.run_id,
                        &candidate.evidence_identity[..12]
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "ambiguous distinct evidence for {}: {}",
                display_id(id),
                descriptions
            ));
        }
        let candidate = distinct
            .into_values()
            .next()
            .expect("candidate evidence is nonempty");
        candidate.row.require_provenance().map_err(|error| {
            format!(
                "fresh result for {} in {} {error}",
                display_id(id),
                candidate.path.display()
            )
        })?;
        candidate.row.require_canonical_pass_evidence().map_err(|error| {
            format!(
                "fresh result for {} in {} {error}",
                display_id(id),
                candidate.path.display()
            )
        })?;
        binary_identities
            .entry(
                candidate
                    .row
                    .binary_sha256
                    .clone()
                    .expect("provenance requires a binary identity"),
            )
            .or_default()
            .push(display_id(id));
        if candidate.row.outcome != "PASS" {
            failed.push(format!(
                "{}={} ({})",
                display_id(id),
                candidate.row.outcome,
                candidate.path.display()
            ));
        } else {
            admitted += 1;
        }
    }
    if binary_identities.len() > 1 {
        let details = binary_identities
            .iter()
            .take(4)
            .map(|(sha, ids)| format!("{}: {}", &sha[..12], ids.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "fresh result set mixes {} Hermit binary identities at one source SHA: {details}",
            binary_identities.len()
        ));
    }
    if !missing.is_empty() || !failed.is_empty() {
        let mut message = format!(
            "fresh result set refused: {} missing, {} non-passing",
            missing.len(),
            failed.len()
        );
        for item in missing.iter().take(8) {
            message.push_str(&format!("\n  missing: {item}"));
        }
        for item in failed.iter().take(8) {
            message.push_str(&format!("\n  non-passing: {item}"));
        }
        return Err(message);
    }
    Ok(admitted)
}

fn require_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("has no valid {label} SHA-256 identity"));
    }
    Ok(())
}

fn display_id(id: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        id.lane, id.category, id.test, id.mode, id.backend
    )
}

fn attempt_shell_command_is_invalid(attempt: &JsonValue) -> bool {
    let Some(cwd) = attempt.get("cwd").and_then(JsonValue::as_str) else {
        return true;
    };
    let Some(command) = attempt.get("shell_command").and_then(JsonValue::as_str) else {
        return true;
    };
    let Ok(argv) = serde_json::from_value::<Vec<String>>(
        attempt.get("argv").cloned().unwrap_or(JsonValue::Null),
    ) else {
        return true;
    };
    let Ok(env) = serde_json::from_value::<BTreeMap<String, String>>(
        attempt.get("env").cloned().unwrap_or(JsonValue::Null),
    ) else {
        return true;
    };
    command != literal_shell_command(cwd, &env, &argv)
}

fn literal_shell_command(
    cwd: &str,
    env: &BTreeMap<String, String>,
    argv: &[String],
) -> String {
    let mut words = vec![
        "cd".into(),
        recorded_shell_quote(cwd),
        "&&".into(),
        "env".into(),
    ];
    words.extend(
        env.iter()
            .map(|(name, value)| recorded_shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| recorded_shell_quote(arg)));
    words.join(" ")
}

fn recorded_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn self_test() -> Result<(), String> {
    let summary = fresh_result_summary(172, "fixture-sha", 170, 2, 2);
    let expected_summary = "Fresh result check: 172/172 selected cells passed at fixture-sha \
(170 compatibility green, including 2 chaos; 2 custom outside the comparable denominator).";
    if summary != expected_summary {
        return Err(format!(
            "fresh-result summary obscures overlapping compatibility/chaos counts: {summary}"
        ));
    }

    let id = CellId {
        lane: "portable".into(),
        category: "fixture".into(),
        test: "fixture/pass".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let expected = BTreeSet::from([id.clone()]);
    let candidate = |outcome: &str| {
        let row = ResultRow {
            schema: CELL_RESULT_SCHEMA,
            run_id: "fixture-run".into(),
            hermit_sha: "fixture".into(),
            source_tree_dirty: false,
            binary_sha256: Some("b".repeat(64)),
            test_sha256: "c".repeat(64),
            test: id.test.clone(),
            category: id.category.clone(),
            lane: id.lane.clone(),
            mode: id.mode.clone(),
            backend: Some(id.backend.clone()),
            classification: "required".into(),
            outcome: outcome.into(),
            log_level: Some("info".into()),
            effective_args: vec!["run".into()],
            argv: vec!["hermit".into(), "run".into()],
            guest_argv: vec!["fixture".into()],
            env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
            relaxations: Vec::new(),
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            attempts: vec![{
                let report = serde_json::to_string(&canonical_verdict::VerificationReport {
                    verified: true,
                    bitwise_parity: true,
                    verdict: "matched".into(),
                    // `Some`: this fixture is a REACHED verdict (verified,
                    // bitwise_parity, "matched"). `None` is reserved for the
                    // documented producer no-result state.
                    comparison: Some(canonical_verdict::ComparisonReport {
                        strictness: "canonical".into(),
                        compare_logs: true,
                        record_envelope:
                            canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                    }),
                    compared_log_messages: Some(canonical_verdict::ComparedLogMessages { left: 1, right: 1 }),
                    // A matched verdict located no divergence, so both
                    // positions are absent -- the same value a pre-field
                    // report carries.
                    first_divergent_scheduler_turn: None,
                    first_divergent_virtual_nanoseconds: None,
                    first_divergent_record: None,
                    first_divergent_syscall: None,
                }).unwrap();
                serde_json::json!({
                "argv":["hermit","run"],
                "guest_argv":["fixture"],
                "env":{"LC_ALL":"C"},
                "cwd":"/repo",
                "shell_command":"cd /repo && env LC_ALL=C hermit run",
                "verification_report": report,
                "verification_report_sha256": format!("{:x}", Sha256::digest(report.as_bytes()))
            })}],
        };
        let evidence_identity = row.evidence_identity().unwrap();
        ResultCandidate {
            evidence_identity,
            path: PathBuf::from("fixture/results.jsonl"),
            row,
        }
    };
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("PASS")])]),
    )
    .map_err(|e| format!("positive result bracket failed: {e}"))?;
    let mut false_command = candidate("PASS");
    false_command.row.shell_command = "true".into();
    false_command.row.attempts[0]["shell_command"] = serde_json::json!("true");
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![false_command])]),
    )
    .is_ok()
    {
        return Err("result evidence accepted a shell command unrelated to argv/env".into());
    }
    let duplicate = candidate("PASS");
    let mut duplicate_copy = candidate("PASS");
    duplicate_copy.path = PathBuf::from("fixture/copied-results.jsonl");
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![duplicate, duplicate_copy])]),
    )
    .map_err(|e| format!("identical duplicate evidence was not deduplicated: {e}"))?;
    let first = candidate("PASS");
    let mut distinct = candidate("PASS");
    distinct.row.run_id = "different-run".into();
    distinct.evidence_identity = distinct.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![first, distinct])]),
    )
    .is_ok()
    {
        return Err("distinct same-SHA evidence was selected by file ordering".into());
    }
    let first = candidate("PASS");
    let mut distinct_relaxation = candidate("PASS");
    distinct_relaxation.row.relaxations = vec!["fixture-relaxation".into()];
    distinct_relaxation.evidence_identity = distinct_relaxation.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![first, distinct_relaxation])]),
    )
    .is_ok()
    {
        return Err("distinct relaxation evidence was treated as the same receipt".into());
    }
    let second_id = CellId {
        test: "fixture/second".into(),
        ..id.clone()
    };
    let mut second_binary = candidate("PASS");
    second_binary.row.test = second_id.test.clone();
    second_binary.row.binary_sha256 = Some("d".repeat(64));
    second_binary.evidence_identity = second_binary.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &BTreeSet::from([id.clone(), second_id.clone()]),
        BTreeMap::from([
            (id.clone(), vec![candidate("PASS")]),
            (second_id, vec![second_binary]),
        ]),
    )
    .is_ok()
    {
        return Err("one source SHA admitted multiple Hermit binary identities".into());
    }
    if verify_candidate_set(&expected, BTreeMap::new()).is_ok() {
        return Err("negative result bracket accepted a missing row".into());
    }
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("FAIL")])]),
    )
    .is_ok()
    {
        return Err("negative result bracket accepted a failing row".into());
    }
    let mut weak = candidate("PASS").row;
    let mut report: JsonValue = serde_json::from_str(
        weak.attempts[0]["verification_report"].as_str().unwrap(),
    )
    .unwrap();
    report["comparison"]["strictness"] = JsonValue::String("stripped".into());
    let report = serde_json::to_string(&report).unwrap();
    weak.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
    weak.attempts[0]["verification_report"] = JsonValue::String(report);
    if weak.require_canonical_pass_evidence().is_ok() {
        return Err("negative result bracket accepted a stripped PASS receipt".into());
    }
    let mut missing_report = candidate("PASS").row;
    missing_report.attempts[0]
        .as_object_mut()
        .unwrap()
        .remove("verification_report");
    if missing_report.require_canonical_pass_evidence().is_ok() {
        return Err("result evidence without a canonical report was accepted".into());
    }
    let mut missing_hash = candidate("PASS").row;
    missing_hash.binary_sha256 = None;
    if missing_hash.require_provenance().is_ok() {
        return Err("result evidence without an artifact hash was accepted".into());
    }
    let mut missing_relaxations = serde_json::to_value(&candidate("PASS").row).unwrap();
    missing_relaxations
        .as_object_mut()
        .unwrap()
        .remove("relaxations");
    if serde_json::from_value::<ResultRow>(missing_relaxations).is_ok() {
        return Err("result row without explicit relaxations was admitted by the schema".into());
    }
    let chaos_id = CellId {
        mode: "chaos".into(),
        ..id.clone()
    };
    let population = BTreeSet::from([chaos_id.clone()]);
    if !selected_green(&BTreeSet::from([chaos_id.clone()]), &population).contains(&chaos_id) {
        return Err("a selected chaos cell was structurally excluded from green".into());
    }
    if selected_green(&BTreeSet::new(), &population).contains(&chaos_id) {
        return Err("an unselected chaos cell was accepted as green".into());
    }
    let visible_reason = CiDisabledReasonData {
        result: Some("determinism-failure".into()),
        evidence: Some("ignored/results/liteinst.jsonl".into()),
        reason: "canonical comparison diverged at scheduler turn 10".into(),
    };
    let visible_red = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        ci_disabled_reasons: BTreeMap::from([(id.clone(), visible_reason.clone())]),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
    };
    let visible_tracked = tracked_from(&visible_red, None, true, false)?;
    if visible_tracked.cells[0].ci_disabled_reason.as_ref() != Some(&visible_reason)
        || !encoded_cells(&visible_tracked)?.contains("ci_disabled_reason")
    {
        return Err("per-backend CI reason was not emitted into tracked scorecard data".into());
    }
    let old_green = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            ci_disabled_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
        }],
    };
    let regressed = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
    };
    if tracked_from(&regressed, Some(old_green), false, false).is_ok() {
        return Err("negative ratchet bracket accepted green-to-red movement".into());
    }
    let intentional = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            ci_disabled_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
        }],
    };
    tracked_from(&regressed, Some(intentional), true, false)
        .map_err(|e| format!("explicit compatibility-transition bracket failed: {e}"))?;

    let mut observed = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: false,
            status: CellStatus::Red,
            ci_disabled_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
        }],
    };
    let pressure_row = |result: &str, turn, virtual_nanoseconds| PressureSummaryRow {
        cell: id.clone(),
        repetition: None,
        result: result.into(),
        verification: Some(PressureVerification {
            first_divergent_scheduler_turn: turn,
            first_divergent_virtual_nanoseconds: virtual_nanoseconds,
            first_divergent_record: None,
            first_divergent_syscall: None,
        }),
        evidence_errors: Vec::new(),
        invocation: Some(PressureInvocation {
            run_id: format!("fixture-{result}"),
            argv: vec!["hermit".into(), "run".into()],
            guest_argv: vec!["fixture".into()],
            env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
            attempts: vec![ObservedAttemptInvocation {
                index: "1".into(),
                outcome: if result == "pass" { "PASS" } else { "FAIL" }.into(),
                status: Some(if result == "pass" { 0 } else { 1 }),
                signal: None,
                timed_out: result == "timeout",
                argv: vec!["hermit".into(), "run".into()],
                guest_argv: vec!["fixture".into()],
                env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
                cwd: "/repo".into(),
                shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
            }],
        }),
    };
    // A fixture depth, so the brackets exercise the recorded-depth path rather
    // than skipping it. Two repos with deliberately different magnitudes,
    // because they are different keyspaces and must never be compared.
    let depth_fixture: BTreeMap<String, SourceDepth> = BTreeMap::from([
        (
            "hermit".to_string(),
            SourceDepth {
                commits: 1927,
                first_parent: 1852,
            },
        ),
        (
            "reverie".to_string(),
            SourceDepth {
                commits: 931,
                first_parent: 916,
            },
        ),
    ]);
    let pressure_summary = |sha: &str, tree: &str, rows| PressureSummary {
        schema: PRESSURE_SUMMARY_SCHEMA,
        hermit_sha: sha.into(),
        detcore_tree: tree.into(),
        source_tree_dirty: false,
        rows,
    };
    // ONE CAMPAIGN, MANY ROWS. This is the shape a real pressure run produces:
    // `summarize --results DIR` writes a single summary covering every repeat,
    // so within-campaign rows are what a distribution is built from and they
    // MERGE. The separate `first` summary below then proves the other half of
    // the rule -- a LATER campaign at the same tree RESETS rather than widening.
    // Each repeat is its OWN run with its own run_id, which is what a real
    // campaign produces and what makes the invocations distinguishable.
    let pressure_repeat = |repetition: u64, result: &str, turn, virtual_nanoseconds| {
        let mut row = pressure_row(result, turn, virtual_nanoseconds);
        row.repetition = Some(repetition);
        if let Some(invocation) = row.invocation.as_mut() {
            invocation.run_id = format!("fixture-{result}-rep{repetition}");
        }
        row
    };
    // Kept as a single-row summary for the refusal brackets below, which need a
    // valid summary to corrupt.
    let first = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("determinism-failure", Some(20), Some(500))],
    );
    let campaign = pressure_summary(
        "sha-1",
        "tree-1",
        vec![
            pressure_repeat(1, "determinism-failure", Some(20), Some(500)),
            pressure_repeat(2, "determinism-failure", Some(10), Some(900)),
            pressure_repeat(3, "timeout", None, None),
            pressure_repeat(4, "replay-failure", Some(30), Some(1000)),
        ],
    );
    apply_pressure_summary(&mut observed, &campaign, "sha-1", "tree-1", &depth_fixture)
        .map_err(|e| format!("pressure-observation campaign bracket failed: {e}"))?;
    let same_engine = pressure_summary("sha-doc", "tree-1", vec![pressure_row("pass", None, None)]);
    apply_pressure_summary(&mut observed, &same_engine, "sha-doc", "tree-1", &depth_fixture)
        .map_err(|e| format!("same-Detcore-tree pressure-observation bracket failed: {e}"))?;
    let observation = &observed.cells[0].observations[0];
    // SAMPLES IS NOT THE RUN COUNT, which is exactly why it has to be stored
    // rather than derived. This bracket folds FIVE runs -- a four-repetition
    // campaign plus one run at a second hermit sha on the same detcore tree --
    // giving FIVE distinct invocations. Only THREE of them LOCATED a divergence
    // position: the two determinism failures and the replay failure. The pass
    // and the timeout contributed nothing to these bounds.
    //
    // Reporting "earliest 10, latest 30" against an implied five runs would
    // overstate the evidence by two thirds.
    if observation.first_divergent_scheduler_turn.range()
!= Some(ObservedRange {
            earliest: 10,
            latest: 30,
            samples: 3,
        })
        || observation.first_divergent_virtual_nanoseconds.range()
!= Some(ObservedRange {
                earliest: 500,
                latest: 1000,
                samples: 3,
            })
        || observation.provenance != ObservationProvenance::PressureTest
        || observation.results
            != BTreeSet::from([
                ObservedResult::Pass,
                ObservedResult::DeterminismFailure,
                ObservedResult::ReplayFailure,
                ObservedResult::Timeout,
            ])
        || observation.hermit_shas != BTreeSet::from(["sha-1".into(), "sha-doc".into()])
        || observation.invocations.len() != 5
        || !observation.invocations.iter().any(|invocation| {
            invocation.hermit_sha == "sha-doc"
                && invocation.result == ObservedResult::Pass
                && invocation.run_id == "fixture-pass"
                && invocation.argv == ["hermit", "run"]
                && invocation.guest_argv == ["fixture"]
                && invocation.env == BTreeMap::from([("LC_ALL".into(), "C".into())])
                && invocation.cwd == "/repo"
                && invocation.shell_command == "cd /repo && env LC_ALL=C hermit run"
                && invocation.attempts.len() == 1
                && invocation.attempts[0].index == "1"
                && invocation.attempts[0].outcome == "PASS"
                && invocation.attempts[0].status == Some(0)
        })
    {
        return Err(
            "pressure observations did not preserve ranges, outcomes, and literal invocations"
                .into(),
        );
    }

    // VALIDATE BRACKET. A validate fold at the SAME detcore tree must land in
    // its OWN observation and must not touch the pressure-test bounds above.
    // Merging them would produce one range moving for two unrelated causes --
    // "the code changed" and "this varies run to run".
    let validate_id = observed.cells[0].id.clone();
    let validate_row = ResultRow {
        schema: CELL_RESULT_SCHEMA,
        run_id: "validate-bracket".into(),
        hermit_sha: "sha-1".into(),
        source_tree_dirty: false,
        binary_sha256: Some("b".repeat(64)),
        test_sha256: "c".repeat(64),
        test: validate_id.test.clone(),
        category: validate_id.category.clone(),
        lane: validate_id.lane.clone(),
        mode: validate_id.mode.clone(),
        backend: Some(validate_id.backend.clone()),
        classification: "required".into(),
        outcome: "FAIL".into(),
        log_level: Some("info".into()),
        effective_args: vec!["run".into()],
        argv: vec!["hermit".into(), "run".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
        relaxations: Vec::new(),
        first_divergent_scheduler_turn: Some(7),
        first_divergent_virtual_nanoseconds: Some(70),
        first_divergent_record: Some(12),
        first_divergent_syscall: Some(9),
        attempts: vec![serde_json::json!({
            "index": "1",
            "outcome": "FAIL",
            "status": 1,
            "signal": null,
            "timed_out": false,
            "argv": ["hermit", "run"],
            "guest_argv": ["fixture"],
            "env": {"LC_ALL": "C"},
            "cwd": "/repo",
            "shell_command": "cd /repo && env LC_ALL=C hermit run",
        })],
    };
    let rows = BTreeMap::from([(
        validate_id.clone(),
        vec![ResultCandidate {
            evidence_identity: "validate-bracket".into(),
            path: PathBuf::from("fixture/results.jsonl"),
            row: validate_row,
        }],
    )]);
    apply_validate_results(&mut observed, &rows, "sha-1", "tree-1", &depth_fixture)
        .map_err(|e| format!("validate-observation bracket failed: {e}"))?;
    let pressure = observed.cells[0]
        .observations
        .iter()
        .find(|o| o.provenance == ObservationProvenance::PressureTest)
        .ok_or("validate fold destroyed the pressure-test observation")?;
    if pressure.first_divergent_scheduler_turn.range()
!= Some(ObservedRange {
            earliest: 10,
            latest: 30,
            samples: 3,
        })
    {
        return Err(
            "a validate fold at the same tree contaminated the pressure-test range".into(),
        );
    }
    let from_validate = observed.cells[0]
        .observations
        .iter()
        .find(|o| o.provenance == ObservationProvenance::Validate)
        .ok_or("validate fold recorded no validate-provenance observation")?;
    // `samples: 1` is the honest reading of a single validate run: a POINT, not
    // a distribution. Validate runs a cell once per commit, so it cannot
    // produce a range at one tree by itself.
    if from_validate.first_divergent_scheduler_turn.range()
!= Some(ObservedRange {
            earliest: 7,
            latest: 7,
            samples: 1,
        })
        || from_validate.first_divergent_virtual_nanoseconds.range()
!= Some(ObservedRange {
                earliest: 70,
                latest: 70,
                samples: 1,
            })
        // The third coordinate, from hermit#2386. Unlike the two above it
        // LOCATES the divergence rather than bounding it, so it is the one a
        // reader should trust for "how far did the run get".
        || from_validate.first_divergent_record.range()
!= Some(ObservedRange {
                earliest: 12,
                latest: 12,
                samples: 1,
            })
        // The fourth unit, a different keyspace again: 12 records in but only
        // 9 syscalls completed.
        || from_validate.first_divergent_syscall.range()
!= Some(ObservedRange {
                earliest: 9,
                latest: 9,
                samples: 1,
            })
        || from_validate.results != BTreeSet::from([ObservedResult::DeterminismFailure])
    {
        return Err("validate observation did not record its own bounds and result".into());
    }

    let next_source = pressure_summary(
        "sha-2",
        "tree-2",
        vec![pressure_row("crash-error", None, None)],
    );
    apply_pressure_summary(&mut observed, &next_source, "sha-2", "tree-2", &depth_fixture)
        .map_err(|e| format!("new-source pressure-observation bracket failed: {e}"))?;
    // Assert the exact KEY SET rather than a count. Observations are keyed by
    // (detcore_tree, provenance), so three distinct keys are expected here: the
    // tree-1 pressure bounds, the tree-1 VALIDATE bounds folded by the bracket
    // above, and the new tree-2 pressure entry. A bare length check would pass
    // for the wrong reason if two of these ever collapsed while a third split.
    let keys: Vec<(String, ObservationProvenance)> = observed.cells[0]
        .observations
        .iter()
        .map(|o| (o.detcore_tree.clone(), o.provenance))
        .collect();
    if keys
        != vec![
            ("tree-1".to_string(), ObservationProvenance::PressureTest),
            ("tree-1".to_string(), ObservationProvenance::Validate),
            ("tree-2".to_string(), ObservationProvenance::PressureTest),
        ]
    {
        return Err(format!(
            "observations are not keyed by (tree, provenance) in sorted order: {keys:?}"
        ));
    }
    let preserved = tracked_from(&regressed, Some(observed.clone()), true, false)?;
    if preserved.cells[0].observations != observed.cells[0].observations {
        return Err("ordinary scorecard derivation changed pressure observations".into());
    }

    let mut dirty = first.clone();
    dirty.source_tree_dirty = true;
    let mut refusal_target = observed.clone();
    if apply_pressure_summary(&mut refusal_target, &dirty, "sha-1", "tree-1", &depth_fixture).is_ok() {
        return Err("dirty pressure observations were accepted".into());
    }
    if apply_pressure_summary(&mut refusal_target, &first, "wrong-sha", "tree-1", &depth_fixture).is_ok()
        || apply_pressure_summary(&mut refusal_target, &first, "sha-1", "wrong-tree", &depth_fixture).is_ok()
    {
        return Err("pressure observations with wrong source identity were accepted".into());
    }
    let mut missing_invocation = first.clone();
    missing_invocation.rows[0].invocation = None;
    if apply_pressure_summary(
        &mut refusal_target,
        &missing_invocation,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
    {
        return Err("pressure observation without a literal invocation was accepted".into());
    }
    let infrastructure = pressure_summary(
        "sha-1",
        "tree-1",
        vec![PressureSummaryRow {
            cell: id.clone(),
            repetition: None,
            result: "infrastructure-error".into(),
            verification: None,
            evidence_errors: vec!["fixture missing".into()],
            invocation: None,
        }],
    );
    if apply_pressure_summary(&mut refusal_target, &infrastructure, "sha-1", "tree-1", &depth_fixture).is_ok() {
        return Err("infrastructure failure was stored as product behavior".into());
    }
    // ⚠️ THE HEADLINE PROPERTY, OWNER RULING: A BATCH OF N CELLS HAS EXACTLY THE
    // SAME EFFECT AS N SEPARATE SINGLE-CELL RUNS. Cells have nothing to do with
    // each other, so one bad row must affect its own cell and nothing else.
    // Asserted by CONSTRUCTION rather than by inspection: fold one three-row
    // batch containing a poisoned row, fold the same rows as three separate
    // single-row summaries, and require the two tracked files to be EQUAL.
    // Anything that entangles rows -- a whole-fold veto, shared mutable state,
    // order dependence -- makes these diverge.
    let mut poisoned = PressureSummaryRow {
        evidence_errors: vec!["fixture missing".into()],
        ..pressure_row("determinism-failure", Some(11), Some(110))
    };
    poisoned.cell.test = "fixture/poisoned".into();
    let batch_rows = vec![
        pressure_repeat(1, "determinism-failure", Some(20), Some(500)),
        poisoned.clone(),
        pressure_repeat(2, "replay-failure", Some(30), Some(1000)),
    ];
    let mut batched = observed.clone();
    let batch_outcome = apply_pressure_summary(
        &mut batched,
        &pressure_summary("sha-1", "tree-1", batch_rows.clone()),
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .map_err(|e| format!("batch-equivalence bracket failed on the batch: {e}"))?;
    let mut singly = observed.clone();
    let mut singly_skipped = 0usize;
    for row in batch_rows {
        match apply_pressure_summary(
            &mut singly,
            &pressure_summary("sha-1", "tree-1", vec![row]),
            "sha-1",
            "tree-1",
            &depth_fixture,
        ) {
            Ok(one) => {
                if !one.skipped.is_empty() {
                    singly_skipped += one.skipped.len();
                }
            }
            // A single-row summary whose only row is poisoned is all-skipped,
            // which is correctly an error. It must still leave the tracked file
            // untouched, which is what the equality below proves.
            Err(_) => singly_skipped += 1,
        }
    }
    if batched.cells != singly.cells {
        return Err(
            "a batch of rows did not have the same effect as the same rows folded singly".into(),
        );
    }
    if batch_outcome.skipped.len() != singly_skipped || batch_outcome.skipped.len() != 1 {
        return Err(format!(
            "batch and single folds disagreed on skipped rows ({} vs {})",
            batch_outcome.skipped.len(),
            singly_skipped
        ));
    }
    if batch_outcome.rows != 2 {
        return Err(format!(
            "the two sound rows were not both admitted (rows={})",
            batch_outcome.rows
        ));
    }
    if batch_outcome.cells != 1 {
        return Err(format!(
            "the skipped cell was counted as merged (cells={})",
            batch_outcome.cells
        ));
    }

    // Every divergence coordinate is guarded by the result class. The record
    // and syscall fields were added after the original turn/nanosecond check;
    // omitting either here would let a PASS carry a contradictory divergence.
    for (label, record, syscall) in
        [("record", Some(1), None), ("syscall", None, Some(1))]
    {
        let mut contradictory = pressure_row("pass", None, None);
        let verification = contradictory
            .verification
            .as_mut()
            .expect("pressure fixture carries a verification report");
        verification.first_divergent_record = record;
        verification.first_divergent_syscall = syscall;
        if apply_pressure_summary(
            &mut refusal_target,
            &pressure_summary("sha-1", "tree-1", vec![contradictory]),
            "sha-1",
            "tree-1",
            &depth_fixture,
        )
        .is_ok()
        {
            return Err(format!(
                "a pass carrying a divergent {label} was accepted"
            ));
        }
    }

    // OWNER RULING: A GREEN CELL IS ADMISSIBLE. This bracket used to assert the
    // opposite. The refusal made the producer and consumer contradict each
    // other -- `--repetitions` only repeats GREEN cells -- and it suppressed
    // the most informative thing a pressure test can report: stressing a cell
    // harder than validate did and finding it red.
    let mut green_target = observed.clone();
    green_target.cells[0].status = CellStatus::Green;
    let green_outcome =
        apply_pressure_summary(&mut green_target, &first, "sha-1", "tree-1", &depth_fixture)
            .map_err(|e| format!("green-cell admission bracket failed: {e}"))?;
    if green_outcome.rows != 1 || !green_outcome.skipped.is_empty() {
        return Err("a green cell was not admitted on equal terms with a red one".into());
    }
    if green_target.cells[0].status != CellStatus::Green {
        return Err("folding pressure evidence changed a cell's status".into());
    }

    let native = ResultRow {
        schema: CELL_RESULT_SCHEMA,
        run_id: "fixture-run".into(),
        hermit_sha: "fixture".into(),
        source_tree_dirty: false,
        binary_sha256: Some("b".repeat(64)),
        test_sha256: "c".repeat(64),
        test: "fixture/native".into(),
        category: "fixture".into(),
        lane: "portable".into(),
        mode: "naked".into(),
        backend: None,
        classification: "required".into(),
        outcome: "PASS".into(),
        log_level: None,
        effective_args: Vec::new(),
        argv: vec!["fixture".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C fixture".into(),
        relaxations: Vec::new(),
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        attempts: vec![serde_json::json!({
            "argv":["fixture"],
            "guest_argv":["fixture"],
            "env":{"LC_ALL":"C"},
            "cwd":"/repo",
            "shell_command":"cd /repo && env LC_ALL=C fixture"
        })],
    };
    if native.id().map(|id| id.backend) != Some("native".into()) {
        return Err("native result identity did not map a null backend to `native`".into());
    }
    let mut malformed = native;
    malformed.mode = "verify".into();
    if malformed.id().is_some() {
        return Err("non-native result without a backend was accepted".into());
    }
    // --- writer-boundary bracket -------------------------------------------
    // The two authorities in this file must not touch each other's fields. A
    // guard that is never exercised is a guard nobody knows is broken, so each
    // direction is asserted to REFUSE, and the legal no-op is asserted to pass.
    let boundary_cell = |observations: Vec<Observation>, status: CellStatus| {
        let mut cell = TrackedCell {
            id: CellId {
                lane: "portable".into(),
                category: "fixture".into(),
                test: "fixture/boundary".into(),
                mode: "verify".into(),
                backend: "ptrace".into(),
            },
            enabled: true,
            status,
            ci_disabled_reason: None,
            last_tested: None,
            measurement: MeasurementState::NeverMeasured,
            observations,
        };
        // ⚠️ DERIVE IT HERE RATHER THAN HARDCODING, or this helper builds rows
        // that contradict themselves and every boundary case below fails for
        // the wrong reason -- a refusal about `measurement` while the test is
        // asserting something about `status`. Caught exactly that way.
        cell.measurement = derive_measurement(&cell);
        cell
    };
    let sample = Observation {
        detcore_tree: "tree".into(),
        provenance: ObservationProvenance::PressureTest,
        depth: BTreeMap::new(),
        hermit_shas: BTreeSet::new(),
        results: BTreeSet::new(),
        invocations: BTreeSet::new(),
        first_divergent_scheduler_turn: ObservedPositions::default(),
        first_divergent_virtual_nanoseconds: ObservedPositions::default(),
        first_divergent_record: ObservedPositions::default(),
        first_divergent_syscall: ObservedPositions::default(),
    };
    let with_evidence = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    let evidence_dropped = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Red)],
    };
    let ratchet_moved = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Green)],
    };
    if enforce_writer_boundary(&with_evidence, &evidence_dropped, Writer::Update).is_ok() {
        return Err("writer boundary allowed `update` to drop measured observations".into());
    }
    if enforce_writer_boundary(&with_evidence, &ratchet_moved, Writer::Observations).is_ok() {
        return Err("writer boundary allowed an observation writer to move `status`".into());
    }
    if enforce_writer_boundary(&with_evidence, &evidence_dropped, Writer::Observations).is_err() {
        return Err(
            "writer boundary refused an observation writer for changing observations, \
             which is the field it owns"
                .into(),
        );
    }
    // ---- step 5: positions are stored, the range is derived ----
    {
        // The range is a VIEW. Three runs at 93, 97, 94 must derive
        // {93, 97, 3} -- and, unlike a stored bound, the individual positions
        // survive so a later reader can ask whether that is a cluster or a
        // spread.
        let mut positions = ObservedPositions::default();
        for value in [93u64, 97, 94] {
            positions.record(Some(value));
        }
        if positions.range()
            != Some(ObservedRange {
                earliest: 93,
                latest: 97,
                samples: 3,
            })
        {
            return Err("derived range did not reproduce the bounds of its positions".into());
        }
        if positions.positions != vec![93, 97, 94] {
            return Err("individual positions were not retained in run order".into());
        }

        // A run that located nothing contributes no sample. Counting it would
        // inflate the denominator with runs that produced no bound.
        let mut sparse = ObservedPositions::default();
        sparse.record(Some(5));
        sparse.record(None);
        if sparse.range().map(|r| r.samples) != Some(1) {
            return Err("a run that located nothing was counted as a sample".into());
        }

        // Nothing located at all is None, not a zero-width range at zero.
        if ObservedPositions::default().range().is_some() {
            return Err("an empty coordinate derived a range".into());
        }

        // ⚠️ THE LEGACY MIGRATION, AND THE POINT IS WHAT IT REFUSES TO DO.
        // `{earliest 93, latest 94, samples 2}` records that two runs diverged
        // somewhere in [93, 94]; it does NOT record which run was which, and no
        // rule recovers that. The bounds must survive as bounds, and NO
        // positions may be invented from them.
        let legacy: ObservedPositions = serde_json::from_str(
            r#"{"earliest":93,"latest":94,"samples":2}"#,
        )
        .map_err(|e| format!("legacy range failed to migrate: {e}"))?;
        if !legacy.positions.is_empty() {
            return Err(
                "legacy bounds were expanded into fabricated positions; they cannot be \
                 recovered and must not be invented"
                    .into(),
            );
        }
        if legacy.range()
            != Some(ObservedRange {
                earliest: 93,
                latest: 94,
                samples: 2,
            })
        {
            return Err("legacy bounds were lost during migration".into());
        }

        // ⚠️ AND THE SILENT-LOSS CASE THAT `deny_unknown_fields` EXISTS FOR.
        // Without it the legacy object would match the current shape with every
        // field defaulted, and the bounds would vanish -- evidence loss that
        // looks exactly like a cell nobody measured. Asserting the round trip
        // is what pins that.
        let reserialised = serde_json::to_string(&legacy)
            .map_err(|e| format!("cannot reserialise migrated legacy bounds: {e}"))?;
        let round_tripped: ObservedPositions = serde_json::from_str(&reserialised)
            .map_err(|e| format!("migrated legacy bounds do not round trip: {e}"))?;
        if round_tripped != legacy {
            return Err("migrated legacy bounds changed across a round trip".into());
        }

        // Positions and legacy bounds coexisting: widen, and ADD the counts,
        // because the legacy triple stands for runs that really happened.
        let mut mixed: ObservedPositions = serde_json::from_str(
            r#"{"earliest":10,"latest":20,"samples":2}"#,
        )
        .map_err(|e| format!("legacy range failed to migrate: {e}"))?;
        mixed.record(Some(30));
        if mixed.range()
            != Some(ObservedRange {
                earliest: 10,
                latest: 30,
                samples: 3,
            })
        {
            return Err("positions and legacy bounds did not combine correctly".into());
        }
    }

    // ---- step 6: `measurement` is derived, and cannot be written to disagree ----
    //
    // ⚠️ EVERY ONE OF THESE IS A REFUSAL OR A DERIVATION, DEMONSTRATED. A field
    // whose guard is never seen refusing is indistinguishable from a field
    // nobody checks, which is the exact failure this scorecard keeps finding
    // elsewhere.
    let diverged_observation = |located: bool| {
        let mut observation = sample.clone();
        observation
            .results
            .insert(ObservedResult::DeterminismFailure);
        if located {
            observation.first_divergent_record.record(Some(98));
        }
        observation
    };
    let passed_observation = || {
        let mut observation = sample.clone();
        observation.results.insert(ObservedResult::Pass);
        observation
    };
    let crashed_observation = || {
        let mut observation = sample.clone();
        observation.results.insert(ObservedResult::CrashError);
        observation
    };

    // The five states are distinguishable FROM THE ROW, which is the whole point.
    for (label, observations, expected) in [
        ("never-measured", Vec::new(), MeasurementState::NeverMeasured),
        (
            "measured-and-passed",
            vec![passed_observation()],
            MeasurementState::MeasuredAndPassed,
        ),
        (
            "measured-no-verdict",
            vec![crashed_observation()],
            MeasurementState::MeasuredNoVerdict,
        ),
        (
            "diverged-unlocated",
            vec![diverged_observation(false)],
            MeasurementState::DivergedUnlocated,
        ),
        (
            "diverged",
            vec![diverged_observation(true)],
            MeasurementState::Diverged,
        ),
    ] {
        let cell = boundary_cell(observations, CellStatus::Red);
        if cell.measurement != expected {
            return Err(format!(
                "measurement derivation: expected `{}` for the {label} case, got `{}`",
                expected.as_str(),
                cell.measurement.as_str()
            ));
        }
    }

    // ⚠️ A CRASH IS NOT A DIVERGENCE. Reading a non-verdict as a product failure
    // is how an infrastructure hiccup becomes a false regression, so this is
    // asserted separately rather than left implicit in the table above.
    let crashed = boundary_cell(vec![crashed_observation()], CellStatus::Red);
    if crashed.measurement == MeasurementState::Diverged
        || crashed.measurement == MeasurementState::DivergedUnlocated
    {
        return Err("measurement counted a crash as a divergence".into());
    }

    // THE REFUSAL: a stored value that disagrees with the row's own evidence.
    let mut lying = boundary_cell(vec![diverged_observation(true)], CellStatus::Red);
    lying.measurement = MeasurementState::MeasuredAndPassed;
    let lying_cells = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![lying],
    };
    for writer in [Writer::Update, Writer::Observations] {
        if enforce_writer_boundary(&lying_cells, &lying_cells, writer).is_ok() {
            return Err(
                "writer boundary allowed a `measurement` that contradicts the row's own \
                 observations"
                    .into(),
            );
        }
    }

    // And the honest row still passes, so the check above discriminates rather
    // than refusing everything.
    let honest = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(
            vec![diverged_observation(true)],
            CellStatus::Red,
        )],
    };
    if enforce_writer_boundary(&honest, &honest, Writer::Observations).is_err() {
        return Err("writer boundary refused a row whose measurement matches its evidence".into());
    }

    if enforce_writer_boundary(&with_evidence, &ratchet_moved, Writer::Update).is_err() {
        return Err(
            "writer boundary refused `update` for changing `status`, which is the field \
             it owns"
                .into(),
        );
    }
    let population_grown = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![
            boundary_cell(vec![sample.clone()], CellStatus::Red),
            TrackedCell {
                id: CellId {
                    lane: "portable".into(),
                    category: "fixture".into(),
                    test: "fixture/second".into(),
                    mode: "verify".into(),
                    backend: "ptrace".into(),
                },
                enabled: true,
                status: CellStatus::Red,
                ci_disabled_reason: None,
                last_tested: None,
                observations: Vec::new(),
                measurement: MeasurementState::NeverMeasured,
            },
        ],
    };
    if enforce_writer_boundary(&with_evidence, &population_grown, Writer::Observations).is_ok() {
        return Err("writer boundary allowed an observation writer to add a cell".into());
    }

    // --- projection bracket -------------------------------------------------
    // Observations are a DERIVED PROJECTION of the series store as of plan step
    // 8. The failure that demotion invites is specific: read the series, write
    // what it says, and when the series is empty write nothing -- silently
    // erasing the only located divergence coordinates in the file and leaving
    // rows indistinguishable from cells nobody ever measured.
    //
    // ⚠️ ASSERTED BY BEHAVIOUR, NOT BY READING THE TYPE. The guard is called
    // with the same before/after pair the real path builds, so a future edit
    // that reorders the write ahead of the check fails here.
    // ⚠️ THE FIXTURE MUST CARRY A LOCATED POSITION, not merely an observation.
    // The guard counts coordinates, so a sample with empty `ObservedPositions`
    // gives it nothing to protect and every case below passes vacuously.
    let mut located = sample.clone();
    located.first_divergent_scheduler_turn.record(Some(68));
    let with_located = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![located.clone()], CellStatus::Red)],
    };
    // The real erasure mode: the observation SURVIVES and its coordinates are
    // blanked. A guard comparing observation counts cannot see this, which is
    // how the first version of it passed the unit case and then walked straight
    // through an end-to-end run against the checked-in file.
    let coordinates_blanked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    match enforce_projection_preserves_evidence(&with_located, &coordinates_blanked, 0) {
        Ok(()) => {
            return Err(
                "projection blanked located coordinates while keeping the observation, and the                  guard did not see it. This is the erasure mode the naive implementation                  actually produces"
                    .into(),
            );
        }
        Err(why) => {
            if !why.contains("fixture/boundary") {
                return Err(format!(
                    "coordinate-blanking refusal does not name the cell: {why}"
                ));
            }
        }
    }
    match enforce_projection_preserves_evidence(&with_located, &evidence_dropped, 0) {
        Ok(()) => {
            return Err(
                "projection dropped measured evidence after reading ZERO series rows. An empty \
                 source is not a finding that a cell has no evidence"
                    .into(),
            );
        }
        Err(why) => {
            // The refusal must NAME the cell it protected. "projection refused"
            // with no subject sends the reader back to diff the whole file.
            if !why.contains("fixture/boundary") {
                return Err(format!(
                    "projection refusal does not name the cell whose evidence it protected: {why}"
                ));
            }
        }
    }
    // Rows actually read means the source spoke, so replacement is legitimate --
    // otherwise the guard would freeze the projection permanently and step 4
    // could never correct a stale bound.
    if enforce_projection_preserves_evidence(&with_located, &evidence_dropped, 1).is_err() {
        return Err(
            "projection guard refused a replacement the series actually supplied rows for; it \
             would freeze the projection against every future correction"
                .into(),
        );
    }
    // Adding evidence is never the erasure case, whatever the row count.
    if enforce_projection_preserves_evidence(&evidence_dropped, &with_located, 0).is_err() {
        return Err("projection guard refused a refresh that ADDED evidence".into());
    }
    // `update` is the ratchet authority and must leave the projection block
    // alone. Asserted in BOTH directions: the boundary refuses the drop, and
    // the derivation actually carries it forward -- a refusal alone would just
    // make every ordinary `update` fail.
    let stamped = TrackedCells {
        schema: SCHEMA,
        projection: Some(ObservationProjection {
            source: "fixture-series".into(),
            refreshed_at: "fixture-stamp".into(),
            rows_read: 7,
            pre_series_corpus: false,
        }),
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    let projection_dropped = TrackedCells {
        projection: None,
        ..stamped.clone()
    };
    if enforce_writer_boundary(&stamped, &projection_dropped, Writer::Update).is_ok() {
        return Err(
            "writer boundary allowed `update` to delete the projection block, which is what              turns a stale projection into one nobody can tell from a fresh one"
                .into(),
        );
    }
    if enforce_writer_boundary(&stamped, &stamped, Writer::Update).is_err() {
        return Err("writer boundary refused an `update` that preserved the projection".into());
    }
    // ⚠️ AND THE DERIVATION ITSELF, not only the guard that polices it. The
    // boundary check above passes even when `tracked_from` rebuilds the file
    // with `projection: None`, because it is never handed the two versions to
    // compare unless a projection already exists in the checked-in file -- and
    // none does until plan step 4 lands. So the reintroduced bug would sit
    // undetected in CI for as long as the series stays empty. This calls the
    // derivation directly, which is the only version of this assertion that can
    // fail today.
    let boundary_id = stamped.cells[0].id.clone();
    let derived_fixture = Derived {
        population: BTreeSet::from([boundary_id.clone()]),
        enabled: BTreeSet::from([boundary_id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        selected: BTreeSet::from([boundary_id.clone()]),
        green: BTreeSet::new(),
    };
    let rederived = tracked_from(&derived_fixture, Some(stamped.clone()), false, false)?;
    if rederived.projection != stamped.projection {
        return Err(format!(
            "`update` did not carry the projection block forward: {:?} became {:?}. Every              ordinary derivation would then delete the record of when the observations were              last projected",
            stamped.projection, rederived.projection
        ));
    }
    // An unreachable source and an empty one are different facts. Only the
    // second is a statement about the cells, so the first is refused outright
    // rather than folded into "zero rows".
    if read_series_rows(Path::new("/nonexistent/series/root")).is_ok() {
        return Err("an unreachable series root was read as an empty series".into());
    }
    // A legacy file with no `projection` key must still load -- the demotion is
    // additive, and a hard requirement would strand every checked-in scorecard.
    let legacy: TrackedCells = serde_json::from_str(r#"{"schema":6,"cells":[]}"#)
        .map_err(|e| format!("legacy scorecard without a projection block no longer loads: {e}"))?;
    if legacy.projection.is_some() {
        return Err("absent projection block deserialized as present".into());
    }
    // ⚠️ THE SAME LOAD-BEARING REFUSAL AS THE OBSERVED-POSITIONS SCHEMA. Without
    // `deny_unknown_fields` a projection block carrying a misspelled or future
    // key matches with every field defaulted: rows_read 0, pre_series_corpus
    // false, refreshed_at empty -- a projection that claims to be current while
    // having read nothing. That is tonight's defect class living in the schema.
    if serde_json::from_str::<ObservationProjection>(
        r#"{"source":"s","refreshed_at":"t","rows_read":3,"row_count":9}"#,
    )
    .is_ok()
    {
        return Err(
            "ObservationProjection accepted an unknown field; a typo would silently default \
             rows_read to 0 and read as a projection nobody can distinguish from a fresh one"
                .into(),
        );
    }

    println!(
        "compatibility scorecard self-test: provenance, distinct-evidence, result, selected-chaos, ratchet, observation-range, batch-equivalence, green-admission, validate-observation, source-identity, writer-boundary, projection, and infrastructure-refusal brackets pass"
    );
    Ok(())
}
