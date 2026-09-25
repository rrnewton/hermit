// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Cross-backend parity cells: the data model, the selection file and the
//! committed snapshot.
//!
//! A parity cell compares one manifest test's `verify` run on a candidate
//! backend (kvm, liteinst, sabre or dbt) against the same test's `verify` run
//! on ptrace, the reference. A [`ParityCellId`] is therefore a test id plus a
//! candidate backend. It has no mode, because the mode is always `verify`,
//! and ptrace cannot be a member, because it is the reference. It is distinct
//! from [`crate::runner::CellId`], which names one run rather than a
//! comparison between two.
//!
//! The matrix is derived, never hand-written: every manifest test crossed with
//! the four candidate backends. A cell is
//!
//! - APPLICABLE when `verify` is enabled on both ptrace and the candidate, and
//! - SELECTABLE when it is applicable and both of those `verify` cells are
//!   selected by full validation (`Population::Required`). A test's lane is a
//!   property of the test rather than of the backend, so the two cells are then
//!   in the same validate plan.
//!
//! Every cell that falls short carries the manifest's own reason.
//! [`PARITY_SELECTION_PATH`] names the cells the parity report measures, and
//! [`PARITY_CELLS_PATH`] is the generated snapshot of every cell's status.
//!
//! Nothing runs parity cells yet. The harness post-pass that writes
//! `parity.jsonl` from [`ParityRecord`]s is a later slice of
//! <https://github.com/rrnewton/hermit/issues/3301>.
//!
//! Equal inputs. The ptrace and candidate cells of one test run from
//! different cell directories, so today their guests see different HOME,
//! fixture and program paths, and those paths alone make ptrace diverge from
//! ptrace. Every [`ParityRecord`] therefore says whether the two runs were
//! given equal inputs, and credit from a comparison whose inputs were not
//! equalized is reported apart from clean credit (see
//! [`ParityRecord::unequalized_credit`]). A backend that cannot be given the
//! reference's inputs at all reports [`ParityVerdict::InputsNotEqualized`]
//! instead of credit.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Component;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::ci_selection::CiSelection;
use crate::logdiff_report::LogDiffReport;
use crate::logdiff_report::LogDiffVerdict;
use crate::runner::ManifestSet;
use crate::runner::ModeRecipe;
use crate::runner::Population;
use crate::runner::Selection;
use crate::runner::TestRecipe;

/// The backend every candidate is compared against.
pub const PARITY_REFERENCE_BACKEND: &str = "ptrace";
/// The only mode a parity cell compares.
pub const PARITY_MODE: &str = "verify";
/// Which parity cells the parity report measures.
pub const PARITY_SELECTION_PATH: &str = "tests/e2e/parity-selection.yaml";
/// Generated snapshot of every parity cell's status.
pub const PARITY_CELLS_PATH: &str = "ci/compat-envelope/parity-cells.json";
pub const PARITY_SELECTION_SCHEMA: u64 = 1;
pub const PARITY_CELLS_SCHEMA: u64 = 1;
/// Schema of one `parity.jsonl` line.
pub const PARITY_RECORD_SCHEMA: u64 = 1;
/// The command that rewrites [`PARITY_CELLS_PATH`].
pub const PARITY_CELLS_REGENERATE: &str =
    "cargo run -p hermit-manifest-plan --bin generate-parity-cells -- --write";

/// The selection file must not live here: this directory holds only bucket
/// manifests, and the manifest reader parses every YAML file in it.
const MANIFEST_DIR: &str = "tests/e2e/manifests";
/// Must match the wording in `manifest_metadata`, which states the same fact
/// for the same cells.
const OCCASIONAL_REASON: &str =
    "This test is marked occasional, and full validation does not select occasional tests.";
/// Bound on each message copied into a [`ParityFirstDifference`], so a
/// `parity.jsonl` line stays small whatever the guest logged.
const MESSAGE_LIMIT_BYTES: usize = 512;
const TOKEN_LIMIT_BYTES: usize = 80;
/// The largest `f64` below 1.0. A partial match must never round up to full
/// credit.
const BELOW_ONE: f64 = 1.0 - f64::EPSILON / 2.0;
/// Why a dbt guest cannot be given the ptrace cell's inputs.
const DBT_INPUTS_NOT_EQUALIZABLE: &str = "the dbt backend refuses --bind and --mount (hermit-cli/src/bin/hermit/run.rs: \
     \"its DynamoRIO adapter does not enter the guest mount namespace\"), so its guest cannot be \
     given the ptrace cell's input paths";

/// A candidate backend. ptrace is the reference and is deliberately not
/// representable. Declared in alphabetical order so the derived `Ord` matches
/// the string order.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "lowercase")]
pub enum ParityBackend {
    Dbt,
    Kvm,
    Liteinst,
    Sabre,
}

impl ParityBackend {
    pub const ALL: [Self; 4] = [Self::Dbt, Self::Kvm, Self::Liteinst, Self::Sabre];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dbt => "dbt",
            Self::Kvm => "kvm",
            Self::Liteinst => "liteinst",
            Self::Sabre => "sabre",
        }
    }

    /// Parse a candidate backend. ptrace gets its own refusal, because naming
    /// the reference as a candidate is a category error rather than a typo.
    pub fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|backend| backend.as_str() == value)
            .ok_or_else(|| {
                if value == PARITY_REFERENCE_BACKEND {
                    format!(
                        "{PARITY_REFERENCE_BACKEND} is the parity reference, not a candidate backend"
                    )
                } else {
                    format!(
                        "unknown parity backend {value:?}; expected one of dbt, kvm, liteinst, sabre"
                    )
                }
            })
    }

    /// Why this backend's guest cannot be given the same input paths as the
    /// ptrace reference, or `None` if it can. Such a backend's parity rows
    /// report [`ParityVerdict::InputsNotEqualized`] instead of credit.
    pub fn inputs_not_equalizable(self) -> Option<&'static str> {
        match self {
            Self::Dbt => Some(DBT_INPUTS_NOT_EQUALIZABLE),
            Self::Kvm | Self::Liteinst | Self::Sabre => None,
        }
    }
}

impl fmt::Display for ParityBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One comparison: `test_id` on `backend` against `test_id` on ptrace, both in
/// `verify` mode.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(deny_unknown_fields)]
pub struct ParityCellId {
    pub test_id: String,
    pub backend: ParityBackend,
}

impl fmt::Display for ParityCellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.test_id, self.backend)
    }
}

/// How far a parity cell gets, with the reason it stops short.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParityAvailability {
    /// `verify` is not enabled on ptrace, on the candidate, or on both.
    NotApplicable { reason: String },
    /// Applicable, but full validation does not select both `verify` cells.
    NotSelectable { reason: String },
    /// Both `verify` cells are selected by full validation.
    Selectable,
}

impl ParityAvailability {
    pub fn applicable(&self) -> bool {
        !matches!(self, Self::NotApplicable { .. })
    }

    pub fn selectable(&self) -> bool {
        matches!(self, Self::Selectable)
    }
}

/// Every test id crossed with every candidate backend.
#[derive(Clone, Debug)]
pub struct ParityMatrix {
    cells: BTreeMap<ParityCellId, ParityAvailability>,
    categories: BTreeMap<String, String>,
}

impl ParityMatrix {
    pub fn derive(manifests: &ManifestSet) -> Result<Self, String> {
        let selected_by_full: BTreeSet<(String, String)> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                mode: Some(PARITY_MODE.to_string()),
                ..Selection::default()
            })?
            .into_iter()
            .filter_map(|cell| cell.id.backend.map(|backend| (cell.id.test, backend)))
            .collect();
        let mut cells = BTreeMap::new();
        let mut categories = BTreeMap::new();
        for (category, _, _, test) in manifests.all_tests() {
            categories.insert(test.id.clone(), category.to_string());
            for backend in ParityBackend::ALL {
                cells.insert(
                    ParityCellId {
                        test_id: test.id.clone(),
                        backend,
                    },
                    availability(test, backend, &selected_by_full)?,
                );
            }
        }
        if cells.is_empty() {
            return Err("the manifests declare no tests, so the parity matrix is empty".into());
        }
        Ok(Self { cells, categories })
    }

    pub fn get(&self, id: &ParityCellId) -> Option<&ParityAvailability> {
        self.cells.get(id)
    }

    pub fn knows_test(&self, test_id: &str) -> bool {
        self.categories.contains_key(test_id)
    }

    /// The manifest bucket a test belongs to.
    pub fn category(&self, test_id: &str) -> Option<&str> {
        self.categories.get(test_id).map(String::as_str)
    }

    /// Every cell in sort order: test id, then backend.
    pub fn cells(&self) -> impl Iterator<Item = (&ParityCellId, &ParityAvailability)> {
        self.cells.iter()
    }
}

fn availability(
    test: &TestRecipe,
    backend: ParityBackend,
    selected_by_full: &BTreeSet<(String, String)>,
) -> Result<ParityAvailability, String> {
    let Some(recipe) = test.modes.get(PARITY_MODE) else {
        return Ok(ParityAvailability::NotApplicable {
            reason: format!("{} has no {PARITY_MODE} mode", test.id),
        });
    };
    let sides = [PARITY_REFERENCE_BACKEND, backend.as_str()];

    let mut not_applicable = Vec::new();
    for side in sides {
        if recipe.backends_enabled.iter().any(|value| value == side) {
            continue;
        }
        // The manifest loader requires every mode to partition the backends,
        // so a backend that is not enabled has a stated disabled reason.
        let why = recipe.backends_disabled.get(side).ok_or_else(|| {
            format!(
                "{}: {side} {PARITY_MODE} is neither enabled nor disabled",
                test.id
            )
        })?;
        not_applicable.push(format!("{side} {PARITY_MODE} is disabled: {why}"));
    }
    if !not_applicable.is_empty() {
        return Ok(ParityAvailability::NotApplicable {
            reason: not_applicable.join("; "),
        });
    }

    let selection = configured_selection(recipe)
        .map_err(|error| format!("{}: {PARITY_MODE} {error}", test.id))?;
    let mut not_selected = Vec::new();
    for side in sides {
        if selected_by_full.contains(&(test.id.clone(), side.to_string())) {
            continue;
        }
        let why = if let Some(reason) = selection.reason(side) {
            reason.reason.clone()
        } else if test.occasional {
            OCCASIONAL_REASON.to_string()
        } else {
            return Err(format!(
                "{}/{PARITY_MODE}@{side} is not selected by full validation without a reason",
                test.id
            ));
        };
        not_selected.push(format!(
            "{side} {PARITY_MODE} is not selected by full validation: {why}"
        ));
    }
    Ok(if not_selected.is_empty() {
        ParityAvailability::Selectable
    } else {
        ParityAvailability::NotSelectable {
            reason: not_selected.join("; "),
        }
    })
}

fn configured_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

/// The parity credit of one comparison.
///
/// `credit = matched_prefix / max(left_len, right_len)`, clamped to
/// `[0.0, 1.0]`, where `matched_prefix` counts the leading compared messages
/// that are equal on both sides. In compared-message units the first
/// difference is at 1-based position `matched_prefix + 1`.
///
/// That position is not [`ParityRecord::first_divergent_record`]. That field
/// is the log-diff report's raw log-record index, which also counts records
/// the comparison did not select, so `(first_divergent_record - 1) /
/// max(left_len, right_len)` is not this credit and can exceed 1.
///
/// - 1.0 only for a full-length match, meaning both sides have the same
///   length and every message matched. A partial match never rounds up to 1.0.
/// - Unequal lengths with an equal prefix give `min / max`, because the extra
///   messages on the longer side are the divergence.
/// - A difference at the first message gives `Some(0.0)`. That is a measured
///   zero.
/// - `None` when nothing was measured, meaning both lengths are zero. An
///   unmeasured comparison is never 0.
///
/// A prefix longer than the shorter side is clamped to it: no side can match
/// more messages than it has.
pub fn credit(matched_prefix: usize, left_len: usize, right_len: usize) -> Option<f64> {
    let longer = left_len.max(right_len);
    if longer == 0 {
        return None;
    }
    let matched = matched_prefix.min(left_len.min(right_len));
    if matched == longer {
        return Some(1.0);
    }
    Some((matched as f64 / longer as f64).clamp(0.0, BELOW_ONE))
}

/// The parity credit a log-diff report supports, or `None` if it measured
/// nothing.
///
/// A match is full credit. A divergence needs the schema-2 matched prefix. A
/// schema-1 divergence carries no prefix, so it is unmeasured rather than
/// guessed. Refusals, empty comparisons and no-result reports are unmeasured.
pub fn credit_from_report(report: &LogDiffReport) -> Option<f64> {
    let selected = &report.selected_messages;
    match report.verdict {
        LogDiffVerdict::Matched => credit(
            report
                .matched_prefix_records
                .unwrap_or_else(|| selected.left.min(selected.right)),
            selected.left,
            selected.right,
        ),
        LogDiffVerdict::Diverged => report
            .matched_prefix_records
            .and_then(|prefix| credit(prefix, selected.left, selected.right)),
        _ => None,
    }
}

/// The outcome of one parity cell in one run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParityVerdict {
    Matched,
    Diverged,
    /// The ptrace `verify` run left no retained log to compare.
    ReferenceMissing,
    /// The candidate `verify` run left no retained log to compare.
    CandidateMissing,
    /// Both logs exist but no comparison verdict could be reached (the
    /// log-diff refused, found nothing comparable, or produced a report that is
    /// not cross-backend evidence).
    Unavailable,
    /// The candidate backend cannot be given the reference's inputs at all
    /// (see [`ParityBackend::inputs_not_equalizable`]), so any comparison would
    /// measure the path difference rather than the backend.
    InputsNotEqualized,
}

impl ParityVerdict {
    pub fn is_measured(self) -> bool {
        matches!(self, Self::Matched | Self::Diverged)
    }
}

/// Where the two logs first part, from the log-diff report. Left is the ptrace
/// reference and right is the candidate, the same orientation as
/// [`LogDiffReport`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityFirstDifference {
    /// Which whitespace-separated token of the message differs first, and how.
    /// For example, ``token 3: `nr=0` vs `nr=1` ``.
    pub field: Option<String>,
    pub syscall: Option<u64>,
    pub scheduler_turn: Option<u64>,
    pub virtual_nanoseconds: Option<u64>,
    /// At most 512 bytes of each message.
    pub reference_message: Option<String>,
    pub candidate_message: Option<String>,
}

/// One line of `parity.jsonl`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityRecord {
    pub schema: u64,
    pub test_id: String,
    pub backend: ParityBackend,
    pub verdict: ParityVerdict,
    /// Whether the ptrace and candidate runs were given the same input paths
    /// (HOME, XDG_CONFIG_HOME, the fixture directory and the program path).
    /// False for every row until the runner equalizes them (slice S8 of
    /// <https://github.com/rrnewton/hermit/issues/3301>).
    pub inputs_equalized: bool,
    /// Why an unmeasured verdict was reached. Absent for measured verdicts.
    pub reason: Option<String>,
    /// Clean credit (see [`credit`]): set only for a measured comparison whose
    /// inputs were equalized. `None` when unmeasured, never 0.
    pub credit: Option<f64>,
    /// The same measurement when the inputs were NOT equalized. It is kept out
    /// of `credit` so that a reader summing `credit` never counts a guest path
    /// difference as backend parity. Exactly one of the two is set for a
    /// measured comparison with a matched prefix.
    pub unequalized_credit: Option<f64>,
    /// Raw log-record index of the first difference, from the log-diff report.
    pub first_divergent_record: Option<usize>,
    /// Compared messages on the ptrace reference side.
    pub left_len: Option<usize>,
    /// Compared messages on the candidate side.
    pub right_len: Option<usize>,
    /// Leading compared messages equal on both sides.
    pub matched_prefix: Option<usize>,
    pub first_difference: Option<ParityFirstDifference>,
    pub reference_log: Option<String>,
    pub candidate_log: Option<String>,
    pub run_id: String,
    pub hermit_sha: String,
}

impl ParityRecord {
    /// Record the comparison of two retained logs.
    ///
    /// A report whose verdict is not matched or diverged, or which fails the
    /// cross-backend evidence policy, becomes `unavailable` with the reason.
    /// It never becomes a zero-credit divergence. A backend that cannot be
    /// given the reference's inputs becomes `inputs-not-equalized` whatever the
    /// report says, and claiming equal inputs for it is an error.
    ///
    /// A matched or diverged report that passes the evidence policy but still
    /// contradicts its verdict, for example a match that names a first
    /// divergent record, is an error rather than an `unavailable` row: the
    /// built record must pass [`ParityRecord::validate`].
    ///
    /// `inputs_equalized` must come from how the two cells were launched, not
    /// from the comparison.
    #[allow(clippy::too_many_arguments)]
    pub fn from_comparison(
        cell: &ParityCellId,
        report: &LogDiffReport,
        inputs_equalized: bool,
        reference_log: &str,
        candidate_log: &str,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<Self, String> {
        if let Some(why) = cell.backend.inputs_not_equalizable() {
            if inputs_equalized {
                return Err(format!(
                    "parity cell {cell}: inputs cannot be equalized: {why}"
                ));
            }
            return Self::unmeasured(
                cell,
                ParityVerdict::InputsNotEqualized,
                false,
                why,
                Some(reference_log),
                Some(candidate_log),
                run_id,
                hermit_sha,
            );
        }
        let unavailable = |reason: String| {
            Self::unmeasured(
                cell,
                ParityVerdict::Unavailable,
                inputs_equalized,
                &reason,
                Some(reference_log),
                Some(candidate_log),
                run_id,
                hermit_sha,
            )
        };
        let verdict = match report.verdict {
            LogDiffVerdict::Matched => ParityVerdict::Matched,
            LogDiffVerdict::Diverged => ParityVerdict::Diverged,
            other => {
                let detail = report
                    .refusal
                    .as_deref()
                    .map(|refusal| format!(": {refusal}"))
                    .unwrap_or_default();
                return unavailable(format!("log-diff verdict was {other:?}{detail}"));
            }
        };
        if let Err(error) = report.require_cross_backend_evidence() {
            return unavailable(format!(
                "log-diff report is not cross-backend evidence: {error}"
            ));
        }
        let selected = &report.selected_messages;
        let first_difference =
            (verdict == ParityVerdict::Diverged).then(|| ParityFirstDifference {
                field: first_differing_field(
                    report.first_divergent_left_message.as_deref(),
                    report.first_divergent_right_message.as_deref(),
                ),
                syscall: report.first_divergent_syscall,
                scheduler_turn: report.first_divergent_scheduler_turn,
                virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
                reference_message: report
                    .first_divergent_left_message
                    .as_deref()
                    .map(|message| bounded(message, MESSAGE_LIMIT_BYTES)),
                candidate_message: report
                    .first_divergent_right_message
                    .as_deref()
                    .map(|message| bounded(message, MESSAGE_LIMIT_BYTES)),
            });
        let matched_prefix = match verdict {
            ParityVerdict::Matched => Some(
                report
                    .matched_prefix_records
                    .unwrap_or_else(|| selected.left.min(selected.right)),
            ),
            _ => report.matched_prefix_records,
        };
        let measured = credit_from_report(report);
        let record = Self {
            schema: PARITY_RECORD_SCHEMA,
            test_id: cell.test_id.clone(),
            backend: cell.backend,
            verdict,
            inputs_equalized,
            reason: None,
            credit: measured.filter(|_| inputs_equalized),
            unequalized_credit: measured.filter(|_| !inputs_equalized),
            first_divergent_record: report.first_divergent_record,
            left_len: Some(selected.left),
            right_len: Some(selected.right),
            matched_prefix,
            first_difference,
            reference_log: Some(reference_log.to_string()),
            candidate_log: Some(candidate_log.to_string()),
            run_id: run_id.to_string(),
            hermit_sha: hermit_sha.to_string(),
        };
        record.validate()?;
        Ok(record)
    }

    /// Record a cell that could not be measured.
    #[allow(clippy::too_many_arguments)]
    pub fn unmeasured(
        cell: &ParityCellId,
        verdict: ParityVerdict,
        inputs_equalized: bool,
        reason: &str,
        reference_log: Option<&str>,
        candidate_log: Option<&str>,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<Self, String> {
        let record = Self {
            schema: PARITY_RECORD_SCHEMA,
            test_id: cell.test_id.clone(),
            backend: cell.backend,
            verdict,
            inputs_equalized,
            reason: Some(reason.to_string()),
            credit: None,
            unequalized_credit: None,
            first_divergent_record: None,
            left_len: None,
            right_len: None,
            matched_prefix: None,
            first_difference: None,
            reference_log: reference_log.map(str::to_string),
            candidate_log: candidate_log.map(str::to_string),
            run_id: run_id.to_string(),
            hermit_sha: hermit_sha.to_string(),
        };
        record.validate()?;
        Ok(record)
    }

    /// The measured credit whether or not the inputs were equalized. Report
    /// it next to [`ParityRecord::inputs_equalized`]; only `credit` is clean.
    pub fn measured_credit(&self) -> Option<f64> {
        self.credit.or(self.unequalized_credit)
    }

    /// Refuse a record whose fields contradict its verdict.
    pub fn validate(&self) -> Result<(), String> {
        let at = format!("parity record {}@{}", self.test_id, self.backend);
        if self.schema != PARITY_RECORD_SCHEMA {
            return Err(format!(
                "{at}: schema must be {PARITY_RECORD_SCHEMA}, got {}",
                self.schema
            ));
        }
        if self.test_id.trim().is_empty() || self.run_id.trim().is_empty() {
            return Err(format!("{at}: test_id and run_id must be non-empty"));
        }
        if self.hermit_sha.len() != 40
            || !self
                .hermit_sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "{at}: hermit_sha must be 40 lowercase hex digits, got {:?}",
                self.hermit_sha
            ));
        }
        for credit in [self.credit, self.unequalized_credit].into_iter().flatten() {
            if !(0.0..=1.0).contains(&credit) {
                return Err(format!("{at}: credit {credit} is outside [0, 1]"));
            }
        }
        if self.credit.is_some() && self.unequalized_credit.is_some() {
            return Err(format!("{at}: credit and unequalized_credit are exclusive"));
        }
        if self.credit.is_some() && !self.inputs_equalized {
            return Err(format!(
                "{at}: credit is clean credit and needs equalized inputs; this measurement \
                 belongs in unequalized_credit"
            ));
        }
        if self.unequalized_credit.is_some() && self.inputs_equalized {
            return Err(format!(
                "{at}: unequalized_credit is set but the inputs were equalized"
            ));
        }
        if let Some(why) = self.backend.inputs_not_equalizable() {
            if self.inputs_equalized || self.verdict.is_measured() {
                return Err(format!(
                    "{at}: {} inputs cannot be equalized, so it reports no measured \
                     comparison and never equal inputs: {why}",
                    self.backend
                ));
            }
        } else if self.verdict == ParityVerdict::InputsNotEqualized {
            return Err(format!(
                "{at}: {} inputs can be equalized; a comparison with unequal inputs is \
                 measured and reports unequalized_credit",
                self.backend
            ));
        }
        if self.verdict.is_measured() {
            if self.reason.is_some() {
                return Err(format!("{at}: a measured verdict carries no reason"));
            }
            let (Some(left), Some(right)) = (self.left_len, self.right_len) else {
                return Err(format!("{at}: a measured verdict needs both lengths"));
            };
            if self.reference_log.is_none() || self.candidate_log.is_none() {
                return Err(format!("{at}: a measured verdict needs both log paths"));
            }
            // `credit` clamps the prefix to the shorter side, so without this a
            // prefix no side could have matched would still agree with it.
            if let Some(prefix) = self.matched_prefix.filter(|&p| p > left.min(right)) {
                return Err(format!(
                    "{at}: matched prefix {prefix} exceeds the shorter compared stream of \
                     {left} | {right}"
                ));
            }
            if self.verdict == ParityVerdict::Matched
                && (self.first_divergent_record.is_some() || self.first_difference.is_some())
            {
                return Err(format!("{at}: a match carries no divergence position"));
            }
            let expected = self
                .matched_prefix
                .and_then(|prefix| credit(prefix, left, right));
            let measured = self.measured_credit();
            if measured != expected {
                return Err(format!(
                    "{at}: credit {measured:?} disagrees with matched prefix {:?} of {left} | {right}",
                    self.matched_prefix
                ));
            }
            match self.verdict {
                ParityVerdict::Matched if measured != Some(1.0) => {
                    return Err(format!(
                        "{at}: a match must be full credit, got {measured:?}"
                    ));
                }
                ParityVerdict::Diverged if measured == Some(1.0) => {
                    return Err(format!("{at}: a divergence cannot be full credit"));
                }
                _ => {}
            }
        } else {
            if self
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(format!("{at}: {:?} needs a reason", self.verdict));
            }
            if self.measured_credit().is_some()
                || self.matched_prefix.is_some()
                || self.first_divergent_record.is_some()
                || self.first_difference.is_some()
            {
                return Err(format!(
                    "{at}: {:?} is unmeasured and carries no credit or divergence",
                    self.verdict
                ));
            }
            if self.left_len.is_some() || self.right_len.is_some() {
                return Err(format!(
                    "{at}: {:?} is unmeasured and carries no compared lengths",
                    self.verdict
                ));
            }
            let missing_log = match self.verdict {
                ParityVerdict::ReferenceMissing => self.reference_log.is_some(),
                ParityVerdict::CandidateMissing => self.candidate_log.is_some(),
                _ => false,
            };
            if missing_log {
                return Err(format!(
                    "{at}: {:?} names the log it says is missing",
                    self.verdict
                ));
            }
        }
        Ok(())
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Summarize the first whitespace-separated token at which two messages
/// differ.
fn first_differing_field(reference: Option<&str>, candidate: Option<&str>) -> Option<String> {
    let (reference, candidate) = (reference?, candidate?);
    let mut left = reference.split_whitespace();
    let mut right = candidate.split_whitespace();
    let mut index = 1usize;
    loop {
        match (left.next(), right.next()) {
            (Some(l), Some(r)) if l == r => index += 1,
            (None, None) => {
                return Some("messages differ only in whitespace".to_string());
            }
            (l, r) => {
                let show = |token: Option<&str>| {
                    token.map_or_else(
                        || "end of message".to_string(),
                        |token| format!("`{}`", bounded(token, TOKEN_LIMIT_BYTES)),
                    )
                };
                return Some(format!("token {index}: {} vs {}", show(l), show(r)));
            }
        }
    }
}

/// The cells the parity report measures, as loaded from
/// [`PARITY_SELECTION_PATH`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParitySelection {
    /// The written rule the cells were chosen by.
    pub rule: String,
    pub cells: BTreeSet<ParityCellId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionDocument {
    schema: u64,
    rule: String,
    cells: Vec<SelectionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionEntry {
    test: String,
    backends: Vec<String>,
}

impl ParitySelection {
    /// Load [`PARITY_SELECTION_PATH`] below `root`.
    pub fn load(root: &Path, matrix: &ParityMatrix) -> Result<Self, String> {
        Self::load_path(root, Path::new(PARITY_SELECTION_PATH), matrix)
    }

    /// Load a selection file at `relative` below `root`. Refuses one placed
    /// under `tests/e2e/manifests`.
    pub fn load_path(root: &Path, relative: &Path, matrix: &ParityMatrix) -> Result<Self, String> {
        refuse_manifest_directory(root, relative)?;
        let path = root.join(relative);
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text, matrix).map_err(|error| format!("{}: {error}", relative.display()))
    }

    /// Parse selection text against the matrix.
    ///
    /// Refuses an unknown or retired test id, ptrace or an unknown backend, a
    /// cell that is not applicable (printing why), duplicates or unsorted
    /// entries, an empty rule and an empty selection. A cell that is
    /// applicable but not selectable is accepted. The snapshot marks it, and
    /// it stays unmeasured until both of its `verify` cells are selected.
    pub fn parse(text: &str, matrix: &ParityMatrix) -> Result<Self, String> {
        let document: SelectionDocument =
            serde_yaml::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
        if document.schema != PARITY_SELECTION_SCHEMA {
            return Err(format!(
                "schema must be {PARITY_SELECTION_SCHEMA}, got {}",
                document.schema
            ));
        }
        let rule = document.rule.trim().to_string();
        if rule.is_empty() {
            return Err("`rule` must state how the cells were chosen".into());
        }
        if document.cells.is_empty() {
            return Err("selects no cells; a selection that measures nothing is refused".into());
        }
        let mut cells = BTreeSet::new();
        let mut previous_test: Option<&str> = None;
        for entry in &document.cells {
            if previous_test.is_some_and(|previous| previous >= entry.test.as_str()) {
                return Err(format!(
                    "tests must be sorted and unique; {:?} follows {:?}",
                    entry.test,
                    previous_test.unwrap_or_default()
                ));
            }
            previous_test = Some(&entry.test);
            if !matrix.knows_test(&entry.test) {
                return Err(format!(
                    "unknown test id {:?}: no manifest declares it (retired or misspelled)",
                    entry.test
                ));
            }
            if entry.backends.is_empty() {
                return Err(format!("{} lists no backends", entry.test));
            }
            let mut previous_backend = None;
            for name in &entry.backends {
                let backend = ParityBackend::parse(name)
                    .map_err(|error| format!("{}: {error}", entry.test))?;
                if previous_backend.is_some_and(|previous| previous >= backend) {
                    return Err(format!(
                        "{}: backends must be sorted and unique; {backend} is out of order",
                        entry.test
                    ));
                }
                previous_backend = Some(backend);
                let id = ParityCellId {
                    test_id: entry.test.clone(),
                    backend,
                };
                match matrix.get(&id) {
                    Some(ParityAvailability::NotApplicable { reason }) => {
                        return Err(format!("{id} is not applicable: {reason}"));
                    }
                    Some(_) => {}
                    None => return Err(format!("{id} is not a parity cell")),
                }
                cells.insert(id);
            }
        }
        Ok(Self { rule, cells })
    }
}

fn refuse_manifest_directory(root: &Path, relative: &Path) -> Result<(), String> {
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "parity selection path {} must be relative with plain components only",
            relative.display()
        ));
    }
    let refuse = || {
        Err(format!(
            "parity selection {} is under {MANIFEST_DIR}; that directory holds only bucket \
             manifests, and every YAML file in it is parsed as one. Keep it at \
             {PARITY_SELECTION_PATH}",
            relative.display()
        ))
    };
    if relative.starts_with(MANIFEST_DIR) {
        return refuse();
    }
    // A symlinked directory could still alias the manifest directory.
    if let (Ok(target), Ok(manifests)) = (
        fs::canonicalize(root.join(relative)),
        fs::canonicalize(root.join(MANIFEST_DIR)),
    ) {
        if target.starts_with(manifests) {
            return refuse();
        }
    }
    Ok(())
}

/// The committed snapshot at [`PARITY_CELLS_PATH`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCells {
    pub schema: u64,
    pub regenerate: String,
    pub reference: ParityReference,
    pub selection_path: String,
    pub selection_rule: String,
    /// Backends whose guest cannot be given the reference's inputs, with the
    /// reason. Their parity rows report `inputs-not-equalized` rather than
    /// credit, even for a selected, selectable cell.
    pub inputs_not_equalizable: BTreeMap<ParityBackend, String>,
    pub counts: ParityCounts,
    pub cells: Vec<ParityCellStatus>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityReference {
    pub backend: String,
    pub mode: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCounts {
    pub all: ParityCount,
    pub by_backend: BTreeMap<ParityBackend, ParityCount>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCount {
    pub cells: usize,
    pub applicable: usize,
    pub selectable: usize,
    pub selected: usize,
    /// Selected and selectable: the cells a full validation can measure.
    pub selected_selectable: usize,
}

impl ParityCount {
    fn add(&mut self, cell: &ParityCellStatus) {
        self.cells += 1;
        self.applicable += usize::from(cell.applicable);
        self.selectable += usize::from(cell.selectable);
        self.selected += usize::from(cell.selected);
        self.selected_selectable += usize::from(cell.selected && cell.selectable);
    }
}

/// One cell of the snapshot. `reason` explains the furthest level the cell
/// does not reach. For a selected cell, it says so.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCellStatus {
    pub test_id: String,
    pub backend: ParityBackend,
    pub category: String,
    pub applicable: bool,
    pub selectable: bool,
    pub selected: bool,
    pub reason: String,
}

/// Derive the snapshot from the manifests and the selection file under `root`.
pub fn generate(root: &Path) -> Result<ParityCells, String> {
    let manifests = ManifestSet::load(root)?;
    let matrix = ParityMatrix::derive(&manifests)?;
    let selection = ParitySelection::load(root, &matrix)?;
    snapshot(&matrix, &selection)
}

/// Build the snapshot from an already-derived matrix and selection.
pub fn snapshot(matrix: &ParityMatrix, selection: &ParitySelection) -> Result<ParityCells, String> {
    let mut cells = Vec::new();
    let mut all = ParityCount::default();
    let mut by_backend: BTreeMap<ParityBackend, ParityCount> = ParityBackend::ALL
        .into_iter()
        .map(|backend| (backend, ParityCount::default()))
        .collect();
    for (id, availability) in matrix.cells() {
        let selected = selection.cells.contains(id);
        let reason = match (availability, selected) {
            (ParityAvailability::NotApplicable { reason }, false) => {
                format!("not applicable: {reason}")
            }
            (ParityAvailability::NotApplicable { .. }, true) => {
                return Err(format!("{id} is selected but not applicable"));
            }
            (ParityAvailability::NotSelectable { reason }, false) => {
                format!("not selectable: {reason}")
            }
            (ParityAvailability::NotSelectable { reason }, true) => {
                format!("selected by {PARITY_SELECTION_PATH}, but not selectable: {reason}")
            }
            (ParityAvailability::Selectable, false) => {
                format!("selectable, not selected: not listed in {PARITY_SELECTION_PATH}")
            }
            (ParityAvailability::Selectable, true) => {
                format!("selected by {PARITY_SELECTION_PATH}")
            }
        };
        let cell = ParityCellStatus {
            test_id: id.test_id.clone(),
            backend: id.backend,
            category: matrix
                .category(&id.test_id)
                .ok_or_else(|| format!("{id} has no manifest category"))?
                .to_string(),
            applicable: availability.applicable(),
            selectable: availability.selectable(),
            selected,
            reason,
        };
        all.add(&cell);
        by_backend
            .get_mut(&id.backend)
            .expect("every backend is counted")
            .add(&cell);
        cells.push(cell);
    }
    // A selection may name a cell that is not selectable; the snapshot keeps
    // it and says why. A selection made ONLY of such cells has nothing a run
    // could measure, yet would still be written as a parity population.
    if all.cells == 0 || all.selected == 0 || all.selected_selectable == 0 {
        return Err(format!(
            "parity snapshot would be vacuous: {} cells, {} selected, {} of them selectable",
            all.cells, all.selected, all.selected_selectable
        ));
    }
    Ok(ParityCells {
        schema: PARITY_CELLS_SCHEMA,
        regenerate: PARITY_CELLS_REGENERATE.to_string(),
        reference: ParityReference {
            backend: PARITY_REFERENCE_BACKEND.to_string(),
            mode: PARITY_MODE.to_string(),
        },
        selection_path: PARITY_SELECTION_PATH.to_string(),
        selection_rule: selection.rule.clone(),
        inputs_not_equalizable: ParityBackend::ALL
            .into_iter()
            .filter_map(|backend| {
                backend
                    .inputs_not_equalizable()
                    .map(|why| (backend, why.to_string()))
            })
            .collect(),
        counts: ParityCounts { all, by_backend },
        cells,
    })
}

/// Canonical text of the snapshot: the header pretty-printed, and one cell per
/// line so a manifest change shows up as a small line diff.
pub fn canonical_text(snapshot: &ParityCells) -> Result<String, String> {
    // Serializing the struct itself keeps the header in declaration order.
    // The empty cells array is then the one slot the cell lines replace. It
    // cannot occur inside a string value, where its quotes would be escaped.
    const SLOT: &str = "\"cells\": []";
    let header = ParityCells {
        cells: Vec::new(),
        ..snapshot.clone()
    };
    let header = serde_json::to_string_pretty(&header).map_err(|error| error.to_string())?;
    if header.matches(SLOT).count() != 1 {
        return Err("parity snapshot header does not contain exactly one cells slot".into());
    }
    let mut cells = String::from("\"cells\": [\n");
    for (index, cell) in snapshot.cells.iter().enumerate() {
        cells.push_str("    ");
        cells.push_str(&serde_json::to_string(cell).map_err(|error| error.to_string())?);
        cells.push_str(if index + 1 == snapshot.cells.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    cells.push_str("  ]");
    Ok(format!("{}\n", header.replace(SLOT, &cells)))
}

/// Refuse a stale snapshot, naming the first differing line and the fix.
pub fn require_fresh(committed: &str, generated: &str) -> Result<(), String> {
    if committed == generated {
        return Ok(());
    }
    let first = committed
        .lines()
        .zip(generated.lines())
        .position(|(left, right)| left != right)
        .map(|line| line + 1)
        .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()) + 1);
    Err(format!(
        "{PARITY_CELLS_PATH} is stale (first differing line {first}); regenerate with: \
         {PARITY_CELLS_REGENERATE}"
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::logdiff_report::LOG_DIFF_REPORT_SCHEMA;
    use crate::logdiff_report::LogDiffComparison;
    use crate::logdiff_report::LogDiffInput;
    use crate::logdiff_report::LogDiffInputs;
    use crate::logdiff_report::LogDiffMessageCounts;
    use crate::logdiff_report::LogDiffRecords;
    use crate::logdiff_report::RecordEnvelopePolicy;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn shipped_matrix() -> ParityMatrix {
        ParityMatrix::derive(&ManifestSet::load(&repo_root()).unwrap()).unwrap()
    }

    /// The committed snapshot must be byte-for-byte what the generator
    /// produces from the committed manifests and selection. This is the same
    /// shape as the validation DAG's freshness test, and it runs wherever
    /// that test runs.
    #[test]
    fn committed_parity_cells_snapshot_is_fresh() {
        let generated = canonical_text(&generate(&repo_root()).unwrap()).unwrap();
        let committed = include_str!("../../compat-envelope/parity-cells.json");
        if let Err(error) = require_fresh(committed, &generated) {
            panic!("{error}");
        }
        let parsed: ParityCells = serde_json::from_str(committed).unwrap();
        assert_eq!(parsed.cells.len(), 1456);
        assert_eq!(
            parsed.inputs_not_equalizable.keys().collect::<Vec<_>>(),
            [&ParityBackend::Dbt]
        );
        assert!(
            committed.len() < 1024 * 1024,
            "{PARITY_CELLS_PATH} is {} bytes; it must stay well under 2 MiB",
            committed.len()
        );
    }

    /// Counts recorded in https://github.com/rrnewton/hermit/issues/3301.
    /// If a manifest change moves them on purpose, update these numbers in
    /// the same commit and regenerate the snapshot.
    #[test]
    fn the_shipped_manifests_give_the_recorded_parity_counts() {
        let counts = generate(&repo_root()).unwrap().counts;
        let row = |count: &ParityCount| {
            (
                count.cells,
                count.applicable,
                count.selectable,
                count.selected,
                count.selected_selectable,
            )
        };
        assert_eq!(row(&counts.all), (1456, 604, 501, 192, 175));
        let by_backend: Vec<_> = counts
            .by_backend
            .iter()
            .map(|(backend, count)| (backend.as_str(), row(count)))
            .collect();
        assert_eq!(
            by_backend,
            [
                ("dbt", (364, 61, 0, 14, 0)),
                ("kvm", (364, 250, 243, 77, 76)),
                ("liteinst", (364, 149, 146, 99, 98)),
                ("sabre", (364, 144, 112, 2, 1)),
            ]
        );
    }

    /// The committed selection is exactly what its written rule derives, so
    /// the rule cannot silently drift from the list.
    #[test]
    fn the_initial_selection_is_exactly_its_rule() {
        let root = repo_root();
        let manifests = ManifestSet::load(&root).unwrap();
        let matrix = ParityMatrix::derive(&manifests).unwrap();
        let selection = ParitySelection::load(&root, &matrix).unwrap();
        let reference_selected: BTreeSet<String> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                category: Some("backend-parity-c".to_string()),
                mode: Some(PARITY_MODE.to_string()),
                backend: Some(PARITY_REFERENCE_BACKEND.to_string()),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .map(|cell| cell.id.test)
            .collect();
        assert!(!reference_selected.is_empty());
        let derived: BTreeSet<ParityCellId> = matrix
            .cells()
            .filter(|(id, availability)| {
                availability.applicable() && reference_selected.contains(&id.test_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(selection.cells, derived);
        assert!(selection.rule.contains("backend-parity-c"));
    }

    #[test]
    fn every_cell_carries_a_reason_matching_its_status() {
        let snapshot = generate(&repo_root()).unwrap();
        // `Exact` reasons carry no per-cell detail, so they must be the
        // whole text. A `Detail` reason must name why after its prefix.
        enum Expected {
            Exact(&'static str),
            Detail(&'static str),
        }
        for cell in &snapshot.cells {
            let expected = match (cell.applicable, cell.selectable, cell.selected) {
                (false, false, false) => Expected::Detail("not applicable: "),
                (true, false, false) => Expected::Detail("not selectable: "),
                (true, false, true) => Expected::Detail(
                    "selected by tests/e2e/parity-selection.yaml, but not selectable: ",
                ),
                (true, true, false) => Expected::Exact(
                    "selectable, not selected: not listed in tests/e2e/parity-selection.yaml",
                ),
                (true, true, true) => {
                    Expected::Exact("selected by tests/e2e/parity-selection.yaml")
                }
                other => panic!("impossible status {other:?} for {}", cell.test_id),
            };
            let matches = match expected {
                Expected::Exact(text) => cell.reason == text,
                Expected::Detail(prefix) => cell
                    .reason
                    .strip_prefix(prefix)
                    .is_some_and(|detail| !detail.trim().is_empty()),
            };
            assert!(
                matches,
                "{}@{}: {}",
                cell.test_id, cell.backend, cell.reason
            );
        }
    }

    /// A selection may keep a cell that is not selectable, but a selection of
    /// ONLY such cells gives a population no run can measure. The shipped
    /// selection's not-selectable members (the 14 dbt cells among them) are
    /// that selection.
    #[test]
    fn a_selection_with_nothing_selectable_is_a_vacuous_snapshot() {
        let root = repo_root();
        let matrix = shipped_matrix();
        let shipped = ParitySelection::load(&root, &matrix).unwrap();
        let availability: BTreeMap<&ParityCellId, &ParityAvailability> = matrix.cells().collect();
        let (selectable, not_selectable): (BTreeSet<_>, BTreeSet<_>) = shipped
            .cells
            .iter()
            .cloned()
            .partition(|id| availability[id].selectable());
        assert!(!not_selectable.is_empty());
        assert!(
            not_selectable
                .iter()
                .any(|id| id.backend == ParityBackend::Dbt)
        );
        let unmeasurable = ParitySelection {
            rule: "fixture rule".into(),
            cells: not_selectable.clone(),
        };
        assert_eq!(
            snapshot(&matrix, &unmeasurable).unwrap_err(),
            format!(
                "parity snapshot would be vacuous: {} cells, {} selected, 0 of them selectable",
                matrix.cells().count(),
                not_selectable.len()
            )
        );

        // One selectable cell is enough to make it a population.
        let mut one_measurable = unmeasurable;
        one_measurable
            .cells
            .insert(selectable.first().unwrap().clone());
        let counts = snapshot(&matrix, &one_measurable).unwrap().counts;
        assert_eq!(counts.all.selected, not_selectable.len() + 1);
        assert_eq!(counts.all.selected_selectable, 1);
    }

    #[test]
    fn backends_parse_and_ptrace_is_refused_as_the_reference() {
        for backend in ParityBackend::ALL {
            assert_eq!(ParityBackend::parse(backend.as_str()), Ok(backend));
        }
        let ptrace = ParityBackend::parse("ptrace").unwrap_err();
        assert!(ptrace.contains("parity reference"), "{ptrace}");
        let unknown = ParityBackend::parse("qemu").unwrap_err();
        assert!(unknown.contains("unknown parity backend"), "{unknown}");
        let mut sorted = ParityBackend::ALL.map(ParityBackend::as_str);
        sorted.sort_unstable();
        assert_eq!(sorted, ParityBackend::ALL.map(ParityBackend::as_str));
    }

    fn first_cell(matrix: &ParityMatrix, wanted: fn(&ParityAvailability) -> bool) -> ParityCellId {
        matrix
            .cells()
            .find(|(_, availability)| wanted(availability))
            .map(|(id, _)| id.clone())
            .expect("the shipped matrix has a cell of this kind")
    }

    fn selection_text(test: &str, backends: &str) -> String {
        format!(
            "schema: 1\nrule: fixture rule\ncells:\n  - test: {test}\n    backends: [{backends}]\n"
        )
    }

    fn refusal(matrix: &ParityMatrix, text: &str) -> String {
        ParitySelection::parse(text, matrix).unwrap_err()
    }

    #[test]
    fn the_selection_loader_refuses_what_it_cannot_measure() {
        let matrix = shipped_matrix();
        let applicable = first_cell(&matrix, ParityAvailability::applicable);
        let not_applicable = first_cell(&matrix, |availability| !availability.applicable());
        let not_selectable = first_cell(&matrix, |availability| {
            availability.applicable() && !availability.selectable()
        });

        // Accepted: an applicable cell, including one not yet selectable.
        let accepted = ParitySelection::parse(
            &selection_text(&applicable.test_id, applicable.backend.as_str()),
            &matrix,
        )
        .unwrap();
        assert_eq!(accepted.cells, BTreeSet::from([applicable.clone()]));
        ParitySelection::parse(
            &selection_text(&not_selectable.test_id, not_selectable.backend.as_str()),
            &matrix,
        )
        .unwrap();

        let error = refusal(
            &matrix,
            &selection_text(&not_applicable.test_id, not_applicable.backend.as_str()),
        );
        let ParityAvailability::NotApplicable { reason } = matrix.get(&not_applicable).unwrap()
        else {
            unreachable!()
        };
        assert!(
            error.contains("is not applicable") && error.contains(reason.as_str()),
            "{error}"
        );

        let error = refusal(&matrix, &selection_text("retired/no-such-test", "kvm"));
        assert!(error.contains("unknown test id"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, "ptrace"));
        assert!(error.contains("parity reference"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, "qemu"));
        assert!(error.contains("unknown parity backend"), "{error}");
        let backend = applicable.backend.as_str();
        let error = refusal(
            &matrix,
            &selection_text(&applicable.test_id, &format!("{backend}, {backend}")),
        );
        assert!(error.contains("sorted and unique"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, ""));
        assert!(error.contains("lists no backends"), "{error}");

        let twice = format!(
            "schema: 1\nrule: r\ncells:\n  - test: {t}\n    backends: [{backend}]\n  - test: {t}\n    backends: [{backend}]\n",
            t = applicable.test_id
        );
        assert!(refusal(&matrix, &twice).contains("sorted and unique"));
        assert!(refusal(&matrix, "schema: 1\nrule: r\ncells: []\n").contains("selects no cells"));
        assert!(refusal(&matrix, "schema: 1\nrule: '  '\ncells: []\n").contains("`rule`"));
        assert!(refusal(&matrix, "schema: 2\nrule: r\ncells: []\n").contains("schema must be 1"));
        assert!(
            refusal(&matrix, "schema: 1\nrule: r\nextra: 1\ncells: []\n").contains("invalid YAML")
        );
    }

    #[test]
    fn a_selection_under_the_manifest_directory_is_refused() {
        let matrix = shipped_matrix();
        let root = repo_root();
        for relative in [
            "tests/e2e/manifests/parity-selection.yaml",
            "tests/e2e/manifests/nested/parity-selection.yaml",
        ] {
            let error =
                ParitySelection::load_path(&root, Path::new(relative), &matrix).unwrap_err();
            assert!(error.contains("holds only bucket manifests"), "{error}");
        }
        for relative in [
            "tests/e2e/../e2e/manifests/parity-selection.yaml",
            "./tests/e2e/manifests/parity-selection.yaml",
            "/tmp/parity-selection.yaml",
        ] {
            let error =
                ParitySelection::load_path(&root, Path::new(relative), &matrix).unwrap_err();
            assert!(error.contains("plain components only"), "{error}");
        }
        // The real file loads from its real place.
        ParitySelection::load(&root, &matrix).unwrap();
    }

    #[test]
    fn credit_is_full_only_for_a_full_length_match_and_none_when_unmeasured() {
        // Identical logs.
        assert_eq!(credit(4, 4, 4), Some(1.0));
        // Divergence at compared message 1: a measured zero, not "unmeasured".
        assert_eq!(credit(0, 4, 4), Some(0.0));
        // Divergence at compared message k = 3 of 4: (k - 1) / 4.
        assert_eq!(credit(2, 4, 4), Some(0.5));
        // Unequal lengths, equal prefix: min / max.
        assert_eq!(credit(2, 2, 5), Some(0.4));
        assert_eq!(credit(5, 5, 2), Some(0.4));
        // Nothing compared.
        assert_eq!(credit(0, 0, 0), None);
        // A prefix cannot exceed the shorter side, so clamping cannot fabricate
        // full credit for unequal lengths.
        assert_eq!(credit(9, 4, 4), Some(1.0));
        assert_eq!(credit(9, 2, 5), Some(0.4));
        // A partial match never rounds up to 1.0, however long the logs are.
        let huge = 1usize << 60;
        let almost = credit(huge - 1, huge, huge).unwrap();
        assert!(almost < 1.0, "{almost}");
        // Always in range.
        for (prefix, left, right) in [(0, 1, 0), (1, 1, 3), (3, 3, 3), (7, 9, 8)] {
            let value = credit(prefix, left, right).unwrap();
            assert!((0.0..=1.0).contains(&value));
            assert_eq!(value == 1.0, prefix >= left && left == right);
        }
    }

    fn report(
        schema: u64,
        verdict: LogDiffVerdict,
        left: usize,
        right: usize,
        prefix: Option<usize>,
    ) -> LogDiffReport {
        let mut parsed = LogDiffReport {
            schema,
            verdict,
            refusal: None,
            selected_messages: LogDiffMessageCounts { left, right },
            // Raw records bound the selected messages on each side.
            records: LogDiffRecords {
                compared: left.max(9).min(right.max(9)),
                available_left: left.max(9),
                available_right: right.max(9),
                withheld_incomplete_tail: false,
            },
            inputs: Some(LogDiffInputs {
                left: LogDiffInput {
                    sha256: "a".repeat(64),
                    bytes: 100,
                },
                right: LogDiffInput {
                    sha256: "b".repeat(64),
                    bytes: 100,
                },
            }),
            comparison: LogDiffComparison {
                stream: "info".into(),
                record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
                unsafe_strip_lines: false,
                canonicalize_host_addresses: true,
                require_structured_events: true,
                ignored_line_substrings: Vec::new(),
                skip_commit: false,
                skip_detlog: false,
                included_detlog_kinds: vec![
                    "syscall".into(),
                    "syscall_result".into(),
                    "other".into(),
                ],
                git_diff: false,
            },
            follow_stopped_because: None,
            first_divergent_record: None,
            matched_prefix_records: None,
            first_divergent_syscall: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        };
        parsed.matched_prefix_records = prefix;
        if verdict == LogDiffVerdict::Diverged {
            // A raw log-record index, as the producer writes it: it also counts
            // records the comparison did not select, so it is NOT
            // `matched_prefix + 1`. Keeping them apart here is what lets the
            // tests below tell a credit derived from the prefix from one
            // derived from this index.
            parsed.first_divergent_record =
                Some(prefix.unwrap_or(0) + 1 + UNSELECTED_RECORDS_BEFORE_DIVERGENCE);
            parsed.first_divergent_syscall = Some(1);
            parsed.first_divergent_left_message = Some("DETLOG syscall nr=0 ret=3".into());
            parsed.first_divergent_right_message = Some("DETLOG syscall nr=0 ret=4".into());
        }
        parsed
    }

    /// Unselected raw records the [`report`] fixture places before the first
    /// difference. Nonzero, so `first_divergent_record != matched_prefix + 1`.
    const UNSELECTED_RECORDS_BEFORE_DIVERGENCE: usize = 4;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn cell() -> ParityCellId {
        ParityCellId {
            test_id: "fixture/parity".into(),
            backend: ParityBackend::Kvm,
        }
    }

    fn record_on(backend: ParityBackend, report: &LogDiffReport, equal: bool) -> ParityRecord {
        let cell = ParityCellId { backend, ..cell() };
        ParityRecord::from_comparison(&cell, report, equal, "ref.log", "cand.log", "run-1", SHA)
            .unwrap()
    }

    /// A kvm record from a run whose inputs were equalized.
    fn record_for(report: &LogDiffReport) -> ParityRecord {
        record_on(ParityBackend::Kvm, report, true)
    }

    #[test]
    fn credit_from_a_report_follows_its_verdict_and_schema() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        assert_eq!(
            credit_from_report(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3))),
            Some(1.0)
        );
        assert_eq!(
            credit_from_report(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(0))),
            Some(0.0)
        );
        // Unequal lengths with an equal prefix: 2 of 5.
        let shorter_side = report(current, LogDiffVerdict::Diverged, 2, 5, Some(2));
        assert_eq!(credit_from_report(&shorter_side), Some(0.4));
        // Credit comes from the prefix (1 of 8), not from the raw record index
        // 6: `6 - 1` is below the shorter side, so credit() would not clamp it
        // and a credit taken from the index would be 5 / 8.
        let early = report(current, LogDiffVerdict::Diverged, 6, 8, Some(1));
        assert_eq!(early.first_divergent_record, Some(6));
        assert_eq!(credit_from_report(&early), Some(0.125));
        // Schema 1: a match is still full credit; a divergence is unmeasured.
        assert_eq!(
            credit_from_report(&report(1, LogDiffVerdict::Matched, 3, 3, None)),
            Some(1.0)
        );
        assert_eq!(
            credit_from_report(&report(1, LogDiffVerdict::Diverged, 4, 4, None)),
            None
        );
        for verdict in [
            LogDiffVerdict::Refused,
            LogDiffVerdict::NoResult,
            LogDiffVerdict::NoComparableMessages,
        ] {
            assert_eq!(
                credit_from_report(&report(current, verdict, 0, 0, None)),
                None
            );
        }
    }

    #[test]
    fn parity_records_carry_credit_only_when_measured() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        assert_eq!(matched.verdict, ParityVerdict::Matched);
        assert_eq!(matched.credit, Some(1.0));
        assert_eq!(matched.first_difference, None);

        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(2)));
        assert_eq!(diverged.verdict, ParityVerdict::Diverged);
        // The record carries the report's raw index (2 matched + 1 + 4
        // unselected) and its credit follows the matched prefix, 2 of 4.
        // Derived from the raw index it would be (7 - 1) / 4 = 1.5.
        assert_eq!(diverged.first_divergent_record, Some(7));
        assert_eq!(diverged.matched_prefix, Some(2));
        assert_eq!(diverged.credit, Some(0.5));
        let difference = diverged.first_difference.as_ref().unwrap();
        assert_eq!(
            difference.field.as_deref(),
            Some("token 4: `ret=3` vs `ret=4`")
        );
        assert_eq!(difference.syscall, Some(1));

        // One JSONL line round-trips exactly.
        let line = serde_json::to_string(&diverged).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""verdict":"diverged""#));
        let parsed: ParityRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, diverged);
        parsed.validate().unwrap();

        // A refused comparison is unavailable with a reason, never a zero.
        let refused = record_for(&report(current, LogDiffVerdict::Refused, 0, 0, None));
        assert_eq!(refused.verdict, ParityVerdict::Unavailable);
        assert_eq!(refused.credit, None);
        assert!(refused.reason.as_deref().unwrap().contains("Refused"));

        // A report that breaks the evidence policy is unavailable too.
        let inconsistent = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(2)));
        assert_eq!(inconsistent.verdict, ParityVerdict::Unavailable);
        assert!(
            inconsistent
                .reason
                .as_deref()
                .unwrap()
                .contains("not cross-backend evidence")
        );

        let missing = ParityRecord::unmeasured(
            &cell(),
            ParityVerdict::ReferenceMissing,
            false,
            "ptrace verify retained no log",
            None,
            Some("cand.log"),
            "run-1",
            SHA,
        )
        .unwrap();
        assert_eq!(missing.credit, None);
        assert!(
            serde_json::to_string(&missing)
                .unwrap()
                .contains(r#""verdict":"reference-missing""#)
        );
    }

    #[test]
    fn a_parity_record_contradicting_its_verdict_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(2)));
        let cases: Vec<(&str, ParityRecord)> = vec![
            (
                "a partial-credit match",
                ParityRecord {
                    credit: Some(0.5),
                    ..matched.clone()
                },
            ),
            (
                "a zero-credit match",
                ParityRecord {
                    credit: Some(0.0),
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured match",
                ParityRecord {
                    credit: None,
                    ..matched.clone()
                },
            ),
            (
                "a full-credit divergence",
                ParityRecord {
                    credit: Some(1.0),
                    ..diverged.clone()
                },
            ),
            (
                "credit disagreeing with the prefix",
                ParityRecord {
                    credit: Some(0.25),
                    ..diverged.clone()
                },
            ),
            (
                "a measured verdict with a reason",
                ParityRecord {
                    reason: Some("why".into()),
                    ..diverged.clone()
                },
            ),
            (
                "a short hermit sha",
                ParityRecord {
                    hermit_sha: "abc".into(),
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured verdict with credit",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    reason: Some("no log".into()),
                    candidate_log: None,
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured verdict without a reason",
                ParityRecord {
                    verdict: ParityVerdict::Unavailable,
                    reason: None,
                    credit: None,
                    matched_prefix: None,
                    ..matched.clone()
                },
            ),
            (
                "a missing log that is named",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    reason: Some("no log".into()),
                    credit: None,
                    matched_prefix: None,
                    left_len: None,
                    right_len: None,
                    ..matched.clone()
                },
            ),
        ];
        for (label, record) in cases {
            assert!(record.validate().is_err(), "{label} must be refused");
        }
    }

    /// Contradictions the credit check alone cannot see: `credit` clamps a
    /// prefix to the shorter side, a match has no divergence to locate, and an
    /// unmeasured verdict compared nothing.
    #[test]
    fn a_parity_record_with_fields_its_verdict_cannot_have_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 2, 5, Some(2)));
        let unavailable = record_for(&report(current, LogDiffVerdict::Refused, 0, 0, None));
        assert_eq!(unavailable.verdict, ParityVerdict::Unavailable);
        let at = "parity record fixture/parity@kvm";
        let cases: Vec<(&str, ParityRecord, String)> = vec![
            (
                "a match whose prefix exceeds both sides",
                ParityRecord {
                    matched_prefix: Some(4),
                    ..matched.clone()
                },
                format!("{at}: matched prefix 4 exceeds the shorter compared stream of 3 | 3"),
            ),
            (
                "a divergence whose prefix exceeds the shorter side",
                ParityRecord {
                    matched_prefix: Some(3),
                    ..diverged.clone()
                },
                format!("{at}: matched prefix 3 exceeds the shorter compared stream of 2 | 5"),
            ),
            (
                "a match with a divergent record",
                ParityRecord {
                    first_divergent_record: Some(1),
                    ..matched.clone()
                },
                format!("{at}: a match carries no divergence position"),
            ),
            (
                "a match with a first difference",
                ParityRecord {
                    first_difference: diverged.first_difference.clone(),
                    ..matched.clone()
                },
                format!("{at}: a match carries no divergence position"),
            ),
            (
                "an unmeasured verdict with a reference length",
                ParityRecord {
                    left_len: Some(3),
                    ..unavailable.clone()
                },
                format!("{at}: Unavailable is unmeasured and carries no compared lengths"),
            ),
            (
                "an unmeasured verdict with a candidate length",
                ParityRecord {
                    right_len: Some(0),
                    ..unavailable.clone()
                },
                format!("{at}: Unavailable is unmeasured and carries no compared lengths"),
            ),
        ];
        for (label, record, expected) in cases {
            assert_eq!(record.validate().unwrap_err(), expected, "{label}");
        }
        for record in [&matched, &diverged, &unavailable] {
            record.validate().unwrap();
        }

        // A report that contradicts itself this way is refused when recorded,
        // not turned into a row.
        let mut located_match = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        located_match.first_divergent_record = Some(2);
        assert_eq!(
            ParityRecord::from_comparison(
                &cell(),
                &located_match,
                true,
                "ref.log",
                "cand.log",
                "run-1",
                SHA
            )
            .unwrap_err(),
            format!("{at}: a match carries no divergence position")
        );
    }

    /// The first divergences S0 measured on c-programs/add-key-enosys
    /// (https://github.com/rrnewton/hermit/issues/3301#issuecomment-5874842696):
    /// kvm and liteinst at compared message 14, sabre at 3, of 115 ptrace
    /// messages. Until the runner equalizes inputs, that credit is reported
    /// apart from clean credit, and it is small, not near 1.
    #[test]
    fn credit_from_unequal_inputs_is_kept_apart_from_clean_credit() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let measured = [
            (ParityBackend::Kvm, 115, 13, 13.0 / 115.0),
            (ParityBackend::Liteinst, 1013, 13, 13.0 / 1013.0),
            (ParityBackend::Sabre, 31, 2, 2.0 / 115.0),
        ];
        for (backend, right, prefix, expected) in measured {
            let diverged = report(current, LogDiffVerdict::Diverged, 115, right, Some(prefix));
            let row = record_on(backend, &diverged, false);
            assert_eq!(row.verdict, ParityVerdict::Diverged, "{backend}");
            assert!(!row.inputs_equalized);
            assert_eq!(
                row.credit, None,
                "{backend}: unequal inputs give no clean credit"
            );
            assert_eq!(row.unequalized_credit, Some(expected), "{backend}");
            assert_eq!(row.measured_credit(), Some(expected));
            assert!(expected < 0.12, "{backend}: {expected}");
            let line = serde_json::to_string(&row).unwrap();
            assert!(line.contains(r#""inputs_equalized":false"#), "{line}");
            assert!(line.contains(r#""credit":null"#), "{line}");
            let parsed: ParityRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed, row);

            // The same comparison with equalized inputs is clean credit.
            let clean = record_on(backend, &diverged, true);
            assert_eq!(clean.credit, Some(expected));
            assert_eq!(clean.unequalized_credit, None);
        }

        // Even a full match is not clean when the inputs differed.
        let matched = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        let unequal_match = record_on(ParityBackend::Kvm, &matched, false);
        assert_eq!(unequal_match.credit, None);
        assert_eq!(unequal_match.unequalized_credit, Some(1.0));

        // A row without the flag is refused rather than defaulted.
        let mut value = serde_json::to_value(&unequal_match).unwrap();
        value.as_object_mut().unwrap().remove("inputs_equalized");
        assert!(serde_json::from_value::<ParityRecord>(value).is_err());
    }

    #[test]
    fn a_backend_that_cannot_be_equalized_reports_inputs_not_equalized() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        let dbt = record_on(ParityBackend::Dbt, &matched, false);
        assert_eq!(dbt.verdict, ParityVerdict::InputsNotEqualized);
        assert!(!dbt.verdict.is_measured());
        assert_eq!(dbt.measured_credit(), None);
        assert_eq!(dbt.matched_prefix, None);
        assert!(dbt.reason.as_deref().unwrap().contains("--bind"));
        assert_eq!(dbt.candidate_log.as_deref(), Some("cand.log"));
        assert!(
            serde_json::to_string(&dbt)
                .unwrap()
                .contains(r#""verdict":"inputs-not-equalized""#)
        );

        let dbt_cell = ParityCellId {
            backend: ParityBackend::Dbt,
            ..cell()
        };
        let error = ParityRecord::from_comparison(
            &dbt_cell, &matched, true, "ref.log", "cand.log", "run-1", SHA,
        )
        .unwrap_err();
        assert!(error.contains("cannot be equalized"), "{error}");
        for backend in [
            ParityBackend::Kvm,
            ParityBackend::Liteinst,
            ParityBackend::Sabre,
        ] {
            assert_eq!(backend.inputs_not_equalizable(), None);
        }
    }

    #[test]
    fn a_parity_record_contradicting_its_inputs_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let diverged = report(current, LogDiffVerdict::Diverged, 4, 4, Some(2));
        let clean = record_for(&diverged);
        let unequal = record_on(ParityBackend::Kvm, &diverged, false);
        let dbt = record_on(ParityBackend::Dbt, &diverged, false);
        let cases: Vec<(&str, ParityRecord)> = vec![
            (
                "clean credit from unequal inputs",
                ParityRecord {
                    credit: Some(0.5),
                    unequalized_credit: None,
                    ..unequal.clone()
                },
            ),
            (
                "unequalized credit from equal inputs",
                ParityRecord {
                    credit: None,
                    unequalized_credit: Some(0.5),
                    ..clean.clone()
                },
            ),
            (
                "both credits",
                ParityRecord {
                    credit: Some(0.5),
                    ..unequal.clone()
                },
            ),
            (
                "unequalized credit disagreeing with the prefix",
                ParityRecord {
                    unequalized_credit: Some(0.25),
                    ..unequal.clone()
                },
            ),
            (
                "a measured dbt comparison",
                ParityRecord {
                    backend: ParityBackend::Dbt,
                    ..unequal.clone()
                },
            ),
            (
                "dbt with equal inputs",
                ParityRecord {
                    inputs_equalized: true,
                    ..dbt.clone()
                },
            ),
            (
                "inputs-not-equalized on a backend that can be equalized",
                ParityRecord {
                    backend: ParityBackend::Kvm,
                    ..dbt.clone()
                },
            ),
            (
                "an unmeasured verdict with unequalized credit",
                ParityRecord {
                    unequalized_credit: Some(0.5),
                    ..dbt.clone()
                },
            ),
        ];
        for (label, record) in cases {
            assert!(record.validate().is_err(), "{label} must be refused");
        }
        for record in [clean, unequal, dbt] {
            record.validate().unwrap();
        }
    }

    #[test]
    fn canonical_text_puts_one_cell_per_line() {
        let snapshot = generate(&repo_root()).unwrap();
        let text = canonical_text(&snapshot).unwrap();
        let cell_lines = text
            .lines()
            .filter(|line| line.starts_with("    {\"test_id\""))
            .count();
        assert_eq!(cell_lines, snapshot.cells.len());
        let reparsed: ParityCells = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, snapshot);
    }
}
