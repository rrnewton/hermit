#!/usr/bin/env rust-script
//! Keep Hermit's compatibility scorecard derived from the E2E manifest and
//! verify that a validate run produced a fresh passing row for every selected
//! regression cell.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;

const SCORECARD: &str = "SCORECARD.md";
const CELLS: &str = "ci/compat-envelope/cells.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SCHEMA: u64 = 3;

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
  verify-results --results DIR [--lanes portable,privileged]
      Check the tracked files, then require a fresh PASS row at HEAD for every
      selected regression cell in the named lanes. The default is both lanes.
  self-test
      Exercise accepting and refusing result sets without running a guest.
  --help
      Show this text.

Green means that the cell is selected by ci/expected-e2e-plan.json and is not a
chaos-mode race-exposure check. Everything else in the manifest is red until it
is measured, promoted into the selected plan, and passes validate.
"#;

#[derive(Clone, Debug, Deserialize)]
struct ManifestRow {
    backend: String,
    bucket: String,
    ci: bool,
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
    cells: Vec<TrackedCell>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    #[serde(default)]
    enabled: bool,
    status: CellStatus,
    /// Filled only by the periodic all-red pressure test. Ordinary validate
    /// never changes this array.
    observations: Vec<Observation>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CellStatus {
    Green,
    Red,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Observation {
    detcore_tree: String,
    hermit_shas: BTreeSet<String>,
    results: BTreeSet<ObservedResult>,
    first_divergent_scheduler_turn: Option<ObservedRange>,
    first_divergent_virtual_nanoseconds: Option<ObservedRange>,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedRange {
    earliest: u64,
    latest: u64,
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
    result: String,
    #[serde(default)]
    verification: Option<PressureVerification>,
    #[serde(default)]
    evidence_errors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct PressureVerification {
    #[serde(default)]
    first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    first_divergent_virtual_nanoseconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct ResultRow {
    schema: u64,
    hermit_sha: String,
    source_tree_dirty: bool,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    classification: String,
    outcome: String,
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
}

struct Derived {
    population: BTreeSet<CellId>,
    enabled: BTreeSet<CellId>,
    selected: BTreeSet<CellId>,
    green: BTreeSet<CellId>,
}

struct ResultCandidate {
    modified: SystemTime,
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
        }
        "check" => {
            no_more(&mut args)?;
            check_tracked(&root)?;
            println!(
                "compatibility scorecard: tracked table and {} cells are current",
                derive(&root)?.population.len()
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
        "self-test" => {
            no_more(&mut args)?;
            self_test()?;
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
                ci_enabled.insert(id);
            }
        }
    }
    let selected: BTreeSet<CellId> = expected.cells.into_iter().collect();
    if selected.len() == 0 {
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
    let green = selected
        .iter()
        .filter(|id| id.mode != "chaos" && population.contains(*id))
        .cloned()
        .collect();
    Ok(Derived {
        population,
        enabled,
        selected,
        green,
    })
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
**Green** means the cell is in `ci/expected-e2e-plan.json`, is not a chaos-mode \
race-exposure check, and is therefore required to pass by ordinary validation. **Red** is every \
other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until \
the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, \
not omitted: a cell that cannot run is not green.\n\n\
These are the current Basic Sanity Milestone 1 contracts. Every `verify` cell runs the same backend \
twice. Bare `--verify` uses the Stripped comparator, so these counts measure legacy \
same-backend repeatability; they do not establish strict INFO-log determinism or cross-backend \
parity.\n\n\
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
compatibility cells above, {chaos} chaos-mode race-exposure checks, and {custom} explicit custom \
commands outside the comparable denominator. A passing validate must produce a fresh result for \
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
            let status = if derived.green.contains(&id) {
                CellStatus::Green
            } else {
                CellStatus::Red
            };
            let enabled = derived.enabled.contains(&id);
            TrackedCell {
                id,
                enabled,
                status,
                observations,
            }
        })
        .collect();
    Ok(TrackedCells {
        schema: SCHEMA,
        cells,
    })
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

fn check_tracked(root: &Path) -> Result<(), String> {
    let derived = derive(root)?;
    let expected_scorecard = render_scorecard(&derived);
    compare_file(&root.join(SCORECARD), &expected_scorecard)?;
    let cells = tracked_from(&derived, load_existing(root)?, false, false)?;
    compare_file(&root.join(CELLS), &encoded_cells(&cells)?)?;
    Ok(())
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
    let cells = tracked_from(
        &derived,
        load_existing(root)?,
        allow_green_removal,
        allow_cell_removal,
    )?;
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

fn merge_range(range: &mut Option<ObservedRange>, value: Option<u64>) {
    let Some(value) = value else {
        return;
    };
    match range {
        Some(range) => {
            range.earliest = range.earliest.min(value);
            range.latest = range.latest.max(value);
        }
        None => {
            *range = Some(ObservedRange {
                earliest: value,
                latest: value,
            });
        }
    }
}

fn apply_pressure_summary(
    tracked: &mut TrackedCells,
    summary: &PressureSummary,
    head: &str,
    detcore_tree: &str,
) -> Result<usize, String> {
    if summary.schema != 3 {
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
    let mut prepared = Vec::new();
    for row in &summary.rows {
        if !seen.insert(row.cell.clone()) {
            return Err(format!(
                "pressure summary contains duplicate cell {}",
                display_id(&row.cell)
            ));
        }
        if !row.evidence_errors.is_empty() {
            return Err(format!(
                "pressure summary contains untrustworthy evidence for {}: {}",
                display_id(&row.cell),
                row.evidence_errors.join("; ")
            ));
        }
        let Some(index) = positions.get(&row.cell).copied() else {
            return Err(format!(
                "pressure summary contains unknown cell {}",
                display_id(&row.cell)
            ));
        };
        if tracked.cells[index].status != CellStatus::Red {
            return Err(format!(
                "pressure summary contains green regression cell {}; ordinary validate owns green evidence",
                display_id(&row.cell)
            ));
        }
        let result = ObservedResult::parse(&row.result)?;
        let turn = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_scheduler_turn);
        let virtual_nanoseconds = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_virtual_nanoseconds);
        if !result.carries_divergence_position()
            && (turn.is_some() || virtual_nanoseconds.is_some())
        {
            return Err(format!(
                "non-divergence result for {} carries a divergence position",
                display_id(&row.cell)
            ));
        }
        prepared.push((index, result, turn, virtual_nanoseconds));
    }

    for (index, result, turn, virtual_nanoseconds) in prepared {
        let observations = &mut tracked.cells[index].observations;
        let position = observations
            .iter()
            .position(|observation| observation.detcore_tree == summary.detcore_tree);
        let observation = match position {
            Some(position) => &mut observations[position],
            None => {
                observations.push(Observation {
                    detcore_tree: summary.detcore_tree.clone(),
                    hermit_shas: BTreeSet::new(),
                    results: BTreeSet::new(),
                    first_divergent_scheduler_turn: None,
                    first_divergent_virtual_nanoseconds: None,
                });
                observations.last_mut().expect("observation was appended")
            }
        };
        observation.hermit_shas.insert(summary.hermit_sha.clone());
        observation.results.insert(result);
        merge_range(&mut observation.first_divergent_scheduler_turn, turn);
        merge_range(
            &mut observation.first_divergent_virtual_nanoseconds,
            virtual_nanoseconds,
        );
        observations.sort_by(|left, right| left.detcore_tree.cmp(&right.detcore_tree));
    }
    Ok(seen.len())
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
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let updated = apply_pressure_summary(&mut tracked, &summary, &head, &detcore_tree)?;
    fs::write(root.join(CELLS), encoded_cells(&tracked)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    println!(
        "compatibility scorecard: merged pressure observations for {updated} red cell(s) at {head}"
    );
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
    verify_candidate_set(&expected, candidates)?;

    print!("{}", render_scorecard(&derived));
    let green_checked = expected
        .iter()
        .filter(|id| derived.green.contains(*id))
        .count();
    let chaos_checked = expected.iter().filter(|id| id.mode == "chaos").count();
    let custom_checked = expected.iter().filter(|id| id.mode == "custom").count();
    println!();
    println!(
        "Fresh result check: {}/{} selected cells passed at {} ({} compatibility green, {} chaos, {} custom).",
        expected.len(),
        expected.len(),
        head,
        green_checked,
        chaos_checked,
        custom_checked
    );
    println!("Result directory: {}", result_root.display());
    Ok(())
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
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .map_err(|e| format!("cannot read timestamp for {}: {e}", path.display()))?;
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: ResultRow = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), index + 1))?;
            if row.schema != 3 {
                return Err(format!(
                    "{}:{} has result schema {}, expected 3",
                    path.display(),
                    index + 1,
                    row.schema
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
            let id = row
                .id()
                .ok_or_else(|| format!("{}:{} has no backend", path.display(), index + 1))?;
            out.entry(id).or_default().push(ResultCandidate {
                modified,
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
    mut candidates: BTreeMap<CellId, Vec<ResultCandidate>>,
) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    for id in expected {
        let Some(rows) = candidates.get_mut(id) else {
            missing.push(display_id(id));
            continue;
        };
        rows.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        let latest = rows.last().expect("nonempty candidate list");
        if rows.len() >= 2 && rows[rows.len() - 2].modified == latest.modified {
            return Err(format!(
                "ambiguous equally-new results for {} in {} and {}",
                display_id(id),
                rows[rows.len() - 2].path.display(),
                latest.path.display()
            ));
        }
        if latest.row.outcome != "PASS" {
            failed.push(format!(
                "{}={} ({})",
                display_id(id),
                latest.row.outcome,
                latest.path.display()
            ));
        }
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
    Ok(())
}

fn display_id(id: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        id.lane, id.category, id.test, id.mode, id.backend
    )
}

fn self_test() -> Result<(), String> {
    let id = CellId {
        lane: "portable".into(),
        category: "fixture".into(),
        test: "fixture/pass".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let expected = BTreeSet::from([id.clone()]);
    let candidate = |outcome: &str| ResultCandidate {
        modified: SystemTime::UNIX_EPOCH,
        path: PathBuf::from("fixture/results.jsonl"),
        row: ResultRow {
            schema: 3,
            hermit_sha: "fixture".into(),
            source_tree_dirty: false,
            test: id.test.clone(),
            category: id.category.clone(),
            lane: id.lane.clone(),
            mode: id.mode.clone(),
            backend: Some(id.backend.clone()),
            classification: "required".into(),
            outcome: outcome.into(),
        },
    };
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("PASS")])]),
    )
    .map_err(|e| format!("positive result bracket failed: {e}"))?;
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
    let old_green = TrackedCells {
        schema: SCHEMA,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            observations: Vec::new(),
        }],
    };
    let regressed = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
    };
    if tracked_from(&regressed, Some(old_green), false, false).is_ok() {
        return Err("negative ratchet bracket accepted green-to-red movement".into());
    }
    let intentional = TrackedCells {
        schema: SCHEMA,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            observations: Vec::new(),
        }],
    };
    tracked_from(&regressed, Some(intentional), true, false)
        .map_err(|e| format!("explicit compatibility-transition bracket failed: {e}"))?;

    let mut observed = TrackedCells {
        schema: SCHEMA,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: false,
            status: CellStatus::Red,
            observations: Vec::new(),
        }],
    };
    let pressure_row = |result: &str, turn, virtual_nanoseconds| PressureSummaryRow {
        cell: id.clone(),
        result: result.into(),
        verification: Some(PressureVerification {
            first_divergent_scheduler_turn: turn,
            first_divergent_virtual_nanoseconds: virtual_nanoseconds,
        }),
        evidence_errors: Vec::new(),
    };
    let pressure_summary = |sha: &str, tree: &str, rows| PressureSummary {
        schema: 3,
        hermit_sha: sha.into(),
        detcore_tree: tree.into(),
        source_tree_dirty: false,
        rows,
    };
    let first = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("determinism-failure", Some(20), Some(500))],
    );
    apply_pressure_summary(&mut observed, &first, "sha-1", "tree-1")
        .map_err(|e| format!("positive pressure-observation bracket failed: {e}"))?;
    let later = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("determinism-failure", Some(10), Some(900))],
    );
    apply_pressure_summary(&mut observed, &later, "sha-1", "tree-1")
        .map_err(|e| format!("pressure-observation range bracket failed: {e}"))?;
    let timeout = pressure_summary("sha-1", "tree-1", vec![pressure_row("timeout", None, None)]);
    apply_pressure_summary(&mut observed, &timeout, "sha-1", "tree-1")
        .map_err(|e| format!("pressure-observation result-set bracket failed: {e}"))?;
    let same_engine = pressure_summary("sha-doc", "tree-1", vec![pressure_row("pass", None, None)]);
    apply_pressure_summary(&mut observed, &same_engine, "sha-doc", "tree-1")
        .map_err(|e| format!("same-Detcore-tree pressure-observation bracket failed: {e}"))?;
    let replay = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("replay-failure", Some(30), Some(1000))],
    );
    apply_pressure_summary(&mut observed, &replay, "sha-1", "tree-1")
        .map_err(|e| format!("replay divergence-position bracket failed: {e}"))?;
    let observation = &observed.cells[0].observations[0];
    if observation.first_divergent_scheduler_turn
        != Some(ObservedRange {
            earliest: 10,
            latest: 30,
        })
        || observation.first_divergent_virtual_nanoseconds
            != Some(ObservedRange {
                earliest: 500,
                latest: 1000,
            })
        || observation.results
            != BTreeSet::from([
                ObservedResult::Pass,
                ObservedResult::DeterminismFailure,
                ObservedResult::ReplayFailure,
                ObservedResult::Timeout,
            ])
        || observation.hermit_shas != BTreeSet::from(["sha-1".into(), "sha-doc".into()])
    {
        return Err(
            "pressure observations did not preserve min/max ranges and all outcomes".into(),
        );
    }
    let next_source = pressure_summary(
        "sha-2",
        "tree-2",
        vec![pressure_row("crash-error", None, None)],
    );
    apply_pressure_summary(&mut observed, &next_source, "sha-2", "tree-2")
        .map_err(|e| format!("new-source pressure-observation bracket failed: {e}"))?;
    if observed.cells[0].observations.len() != 2 {
        return Err("a new Detcore tree was blended into an old observation".into());
    }
    let preserved = tracked_from(&regressed, Some(observed.clone()), true, false)?;
    if preserved.cells[0].observations != observed.cells[0].observations {
        return Err("ordinary scorecard derivation changed pressure observations".into());
    }

    let mut dirty = first.clone();
    dirty.source_tree_dirty = true;
    let mut refusal_target = observed.clone();
    if apply_pressure_summary(&mut refusal_target, &dirty, "sha-1", "tree-1").is_ok() {
        return Err("dirty pressure observations were accepted".into());
    }
    if apply_pressure_summary(&mut refusal_target, &first, "wrong-sha", "tree-1").is_ok()
        || apply_pressure_summary(&mut refusal_target, &first, "sha-1", "wrong-tree").is_ok()
    {
        return Err("pressure observations with wrong source identity were accepted".into());
    }
    let infrastructure = pressure_summary(
        "sha-1",
        "tree-1",
        vec![PressureSummaryRow {
            cell: id.clone(),
            result: "infrastructure-error".into(),
            verification: None,
            evidence_errors: vec!["fixture missing".into()],
        }],
    );
    if apply_pressure_summary(&mut refusal_target, &infrastructure, "sha-1", "tree-1").is_ok() {
        return Err("infrastructure failure was stored as product behavior".into());
    }
    let mut green_target = observed.clone();
    green_target.cells[0].status = CellStatus::Green;
    if apply_pressure_summary(&mut green_target, &first, "sha-1", "tree-1").is_ok() {
        return Err("periodic pressure evidence was allowed to rewrite a green cell".into());
    }

    let native = ResultRow {
        schema: 3,
        hermit_sha: "fixture".into(),
        source_tree_dirty: false,
        test: "fixture/native".into(),
        category: "fixture".into(),
        lane: "portable".into(),
        mode: "naked".into(),
        backend: None,
        classification: "required".into(),
        outcome: "PASS".into(),
    };
    if native.id().map(|id| id.backend) != Some("native".into()) {
        return Err("native result identity did not map a null backend to `native`".into());
    }
    let mut malformed = native;
    malformed.mode = "verify".into();
    if malformed.id().is_some() {
        return Err("non-native result without a backend was accepted".into());
    }
    println!(
        "compatibility scorecard self-test: result, ratchet, observation-range, source-identity, and infrastructure-refusal brackets pass"
    );
    Ok(())
}
