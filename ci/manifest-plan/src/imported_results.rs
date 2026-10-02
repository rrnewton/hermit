//! `test-harness run` import mode: re-publish cell rows another runner wrote.
//!
//! With `E2E_IMPORT_RESULTS=<root>` the harness executes no cell. It reads the
//! rows that a Buck run of the same plan left under
//! `<root>/<lane>/manifest_<category>/results.jsonl` (the layout
//! `ci/buck-e2e/ingest.py` writes) and emits them through the normal
//! publication path, so `results.jsonl`, JUnit, `summary.json`, the retry
//! history and the dagrun test counts are produced exactly as for an executed
//! bucket. Every selected cell must be accounted for: a cell with no row and no
//! recorded host inapplicability becomes an ERROR row. That is the
//! executed-equals-plan gate for an imported run, and nothing here may relax it.
//!
//! An imported row is rebound to this run before publication: its `run_id`
//! (and the run id its CPU observations are bound to) becomes this run's, and
//! the observations' outer attempt follows the row's attempt, which the ingest
//! assigned from Tpx's execution order. The rows' original run ids are kept
//! and reported. A row built from a different source commit is not rebound; it
//! becomes an ERROR, so a stale import root cannot pass for this commit.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::runner::CELL_RESULT_SCHEMA;
use crate::runner::CellResult;
use crate::runner::RunContext;
use crate::runner::SelectedCell;
use crate::runner::host_inapplicable_result;
use crate::runner::infrastructure_error_result;

pub const IMPORT_RESULTS_ENV: &str = "E2E_IMPORT_RESULTS";

/// The bucket directory of `(lane, category)` below an import root.
pub fn bucket_dir(root: &Path, lane: &str, category: &str) -> PathBuf {
    root.join(lane)
        .join(format!("manifest_{}", category.replace('-', "_")))
}

/// What the import root says about one selected cell, already turned into the
/// rows the harness emits: every attempt in order, or one terminal row.
#[derive(Clone, Debug)]
pub struct ImportedCell {
    pub rows: Vec<CellResult>,
}

/// The import of one `run`: one entry per selected cell, in selection order.
pub struct ImportedRun {
    pub cells: Vec<ImportedCell>,
    /// The run ids the imported rows carried before they were rebound.
    pub source_run_ids: BTreeSet<String>,
    /// Selected cells the root had nothing for.
    pub missing: usize,
}

#[derive(Deserialize)]
struct Summary {
    #[serde(default)]
    host_inapplicable_cells: Vec<HostInapplicable>,
}

#[derive(Deserialize)]
struct HostInapplicable {
    test: String,
    mode: String,
    backend: Option<String>,
    reason: Option<String>,
}

type Key = (String, String, String, String, Option<String>);

fn key(lane: &str, category: &str, test: &str, mode: &str, backend: Option<&str>) -> Key {
    (
        lane.into(),
        category.into(),
        test.into(),
        mode.into(),
        backend.map(str::to_owned),
    )
}

fn cell_key(cell: &SelectedCell) -> Key {
    key(
        &cell.test.lane,
        &cell.category,
        &cell.id.test,
        &cell.id.mode,
        cell.id.backend.as_deref(),
    )
}

fn error_row(context: &RunContext, cell: &SelectedCell, kind: &str, reason: String) -> CellResult {
    let mut row = infrastructure_error_result(context, cell, reason);
    row.error_kind = Some(kind.into());
    row
}

/// Read the import root for `cells`. `results_path` is where this run
/// publishes; an import file that is the same file is refused, because the
/// harness appends to it.
pub fn load(
    root: &Path,
    cells: &[SelectedCell],
    context: &RunContext,
    results_path: &Path,
) -> Result<ImportedRun, String> {
    let wanted = cells.iter().map(cell_key).collect::<BTreeSet<_>>();
    let buckets = cells
        .iter()
        .map(|cell| (cell.test.lane.clone(), cell.category.clone()))
        .collect::<BTreeSet<_>>();
    let publish = fs::canonicalize(results_path).ok();
    let mut rows = BTreeMap::<Key, Vec<CellResult>>::new();
    let mut inapplicable = BTreeMap::<Key, String>::new();
    for (lane, category) in &buckets {
        let dir = bucket_dir(root, lane, category);
        let results = dir.join("results.jsonl");
        if publish.is_some() && fs::canonicalize(&results).ok() == publish {
            return Err(format!(
                "{IMPORT_RESULTS_ENV} file {} is this run's own results path",
                results.display()
            ));
        }
        match fs::read_to_string(&results) {
            Ok(text) => {
                for (number, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let row: CellResult = serde_json::from_str(line).map_err(|error| {
                        format!("{}:{}: {error}", results.display(), number + 1)
                    })?;
                    if row.lane != *lane || row.category != *category {
                        return Err(format!(
                            "{}:{}: row of {}/{} in the {lane}/{category} bucket",
                            results.display(),
                            number + 1,
                            row.lane,
                            row.category
                        ));
                    }
                    let id = key(lane, category, &row.test, &row.mode, row.backend.as_deref());
                    if wanted.contains(&id) {
                        rows.entry(id).or_default().push(row);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", results.display())),
        }
        let summary = dir.join("summary.json");
        match fs::read(&summary) {
            Ok(bytes) => {
                let summary: Summary = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("{}: {error}", summary.display()))?;
                for cell in summary.host_inapplicable_cells {
                    let id = key(
                        lane,
                        category,
                        &cell.test,
                        &cell.mode,
                        cell.backend.as_deref(),
                    );
                    if wanted.contains(&id) {
                        inapplicable.insert(id, cell.reason.unwrap_or_default());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", summary.display())),
        }
    }

    let mut source_run_ids = BTreeSet::new();
    let mut missing = 0;
    let cells = cells
        .iter()
        .map(|cell| {
            let id = cell_key(cell);
            let found = rows.remove(&id);
            let reason = inapplicable.remove(&id);
            let rows = match (found, reason) {
                (Some(_), Some(_)) => vec![error_row(
                    context,
                    cell,
                    "import-conflict",
                    format!(
                        "{IMPORT_RESULTS_ENV} has both result rows and a host-inapplicable entry for this cell"
                    ),
                )],
                (None, Some(reason)) => vec![host_inapplicable_result(context, cell, reason)],
                (None, None) => {
                    missing += 1;
                    vec![error_row(
                        context,
                        cell,
                        "import-missing",
                        format!(
                            "{IMPORT_RESULTS_ENV}={} has no result for this selected cell",
                            root.display()
                        ),
                    )]
                }
                (Some(mut found), None) => {
                    found.sort_by_key(|row| row.attempt);
                    match found.iter().find_map(|row| {
                        (row.schema != CELL_RESULT_SCHEMA).then(|| {
                            format!("imported row has schema {}, expected {CELL_RESULT_SCHEMA}", row.schema)
                        }).or_else(|| (row.hermit_sha != context.source_sha).then(|| {
                            format!(
                                "imported row was built from {}, this run is {}",
                                row.hermit_sha, context.source_sha
                            )
                        }))
                    }) {
                        Some(reason) => vec![error_row(context, cell, "import-stale", reason)],
                        None => found
                            .into_iter()
                            .map(|mut row| {
                                source_run_ids.insert(std::mem::replace(
                                    &mut row.run_id,
                                    context.run_id.clone(),
                                ));
                                if let Some(observations) = row.cpu_observations.as_mut() {
                                    observations.binding.run_id = context.run_id.clone();
                                    observations.binding.outer_attempt = row.attempt;
                                }
                                row
                            })
                            .collect(),
                    }
                }
            };
            ImportedCell { rows }
        })
        .collect();
    Ok(ImportedRun {
        cells,
        source_run_ids,
        missing,
    })
}
