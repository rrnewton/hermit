use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

pub use crate::canonical_verdict::VerificationReport;
use crate::ci_selection::CiDisabledReasonSpec;
use crate::ci_selection::CiSelection;
use crate::ci_selection::CiSelectionSpec;

const BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
pub const CELL_RESULT_SCHEMA: u64 = 4;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDocument {
    pub schema: u64,
    pub bucket: String,
    pub test: Vec<TestRecipe>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRecipe {
    pub id: String,
    pub description: String,
    pub lane: String,
    #[serde(default)]
    pub requires: Vec<String>,
    pub timeout_seconds: u64,
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectCommand>,
    pub observation: Observation,
    pub build: Option<BuildRecipe>,
    pub modes: BTreeMap<String, ModeRecipe>,
    pub slow_reason: Option<String>,
    #[serde(default)]
    pub preprocessors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DirectCommand {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecipe {
    #[serde(default)]
    pub cflags: Vec<String>,
    #[serde(default)]
    pub rustflags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub status: bool,
    pub stdout: bool,
    pub stderr: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeRecipe {
    pub ci: CiSelectionSpec,
    pub ci_disabled_reason: Option<CiDisabledReasonSpec>,
    #[serde(default)]
    pub backends_enabled: Vec<String>,
    #[serde(default)]
    pub backends_disabled: BTreeMap<String, String>,
    #[serde(default)]
    pub guest_args: BTreeMap<String, Vec<String>>,
    pub workdir: Option<String>,
    pub runs: Option<u64>,
    pub seeds: Option<Vec<i64>>,
    pub assert: Option<Assertions>,
    pub outcome_classes: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertions {
    pub bitwise_parity: Option<bool>,
    pub min_distinct: Option<u64>,
    pub min_passes: Option<u64>,
    pub min_failures: Option<u64>,
    pub min_normalized_entropy: Option<f64>,
    pub runs: Option<u64>,
    pub repeat_identical: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ManifestSet {
    pub documents: Vec<ManifestDocument>,
    tests: BTreeMap<String, (String, TestRecipe)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CellId {
    pub test: String,
    pub mode: String,
    pub backend: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Population {
    Required,
    Enabled,
    Disabled,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub population: Option<Population>,
    pub lane: Option<String>,
    pub category: Option<String>,
    pub test: Option<String>,
    pub mode: Option<String>,
    pub backend: Option<String>,
    pub include_occasional: bool,
    pub include_manual: bool,
}

#[derive(Clone, Debug)]
pub struct SelectedCell {
    pub category: String,
    pub test: TestRecipe,
    pub id: CellId,
    pub enabled: bool,
}

impl ManifestSet {
    pub fn load(root: &Path) -> Result<Self, String> {
        let dir = root.join("tests/e2e/manifests");
        let mut paths = fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .collect::<Vec<_>>();
        paths.sort();
        if paths.is_empty() {
            return Err(format!("no YAML manifests found in {}", dir.display()));
        }
        let mut documents = Vec::new();
        let mut tests = BTreeMap::new();
        for path in paths {
            let source = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let document: ManifestDocument = serde_yaml::from_str(&source)
                .map_err(|e| format!("{}: invalid YAML: {e}", path.display()))?;
            let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
            validate_document(&document, stem, root)?;
            for test in &document.test {
                if tests
                    .insert(test.id.clone(), (document.bucket.clone(), test.clone()))
                    .is_some()
                {
                    return Err(format!("duplicate test id: {}", test.id));
                }
            }
            documents.push(document);
        }
        Ok(Self { documents, tests })
    }

    pub fn select(&self, selection: &Selection) -> Result<Vec<SelectedCell>, String> {
        let population = selection.population.unwrap_or(Population::Enabled);
        let mut cells = Vec::new();
        for (id, (category, test)) in &self.tests {
            if selection
                .lane
                .as_deref()
                .is_some_and(|lane| lane != test.lane)
                || selection
                    .category
                    .as_deref()
                    .is_some_and(|value| value != category)
                || selection.test.as_deref().is_some_and(|value| value != id)
                || (!selection.include_occasional && test.occasional)
            {
                continue;
            }
            for (mode, recipe) in &test.modes {
                if selection.mode.as_deref().is_some_and(|value| value != mode) {
                    continue;
                }
                if mode == "naked" {
                    let enabled = recipe
                        .backends_enabled
                        .iter()
                        .any(|value| value == "native");
                    let ci = ci_selection(recipe)?.selected("native");
                    let accepted = match population {
                        Population::Required => {
                            enabled
                                && (ci
                                    || selection.mode.as_deref() == Some("naked")
                                    || selection.include_manual)
                        }
                        other => population_accepts(other, ci, enabled),
                    };
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|backend| backend != "native")
                        || !accepted
                    {
                        continue;
                    }
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: None,
                        },
                        enabled,
                    });
                    continue;
                }
                let backends: Vec<_> = match population {
                    Population::Disabled => recipe.backends_disabled.keys().cloned().collect(),
                    _ => recipe.backends_enabled.clone(),
                };
                let ci = ci_selection(recipe)?;
                for backend in backends {
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|value| value != backend)
                    {
                        continue;
                    }
                    let enabled = recipe.backends_enabled.contains(&backend);
                    if population == Population::Required
                        && !selection.include_manual
                        && !ci.selected(&backend)
                    {
                        continue;
                    }
                    if !population_accepts(population, ci.selected(&backend), enabled) {
                        continue;
                    }
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: Some(backend),
                        },
                        enabled,
                    });
                }
            }
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(cells)
    }

    pub fn all_tests(&self) -> impl Iterator<Item = (&str, &TestRecipe)> {
        self.tests
            .values()
            .map(|(category, test)| (category.as_str(), test))
    }
}

fn population_accepts(population: Population, ci: bool, enabled: bool) -> bool {
    match population {
        Population::Required => ci && enabled,
        Population::Enabled => enabled,
        Population::Disabled => !enabled,
    }
}

fn validate_document(document: &ManifestDocument, stem: &str, root: &Path) -> Result<(), String> {
    if document.schema != 2 {
        return Err(format!("{stem}: schema must be 2"));
    }
    if document.bucket != stem {
        return Err(format!(
            "{stem}: bucket `{}` must equal file stem",
            document.bucket
        ));
    }
    if document.test.is_empty() {
        return Err(format!("{stem}: test list must not be empty"));
    }
    for test in &document.test {
        if !test.id.starts_with(&format!("{stem}/")) {
            return Err(format!("{}: id must start with {stem}/", test.id));
        }
        if !matches!(test.lane.as_str(), "portable" | "privileged") {
            return Err(format!("{}: invalid lane `{}`", test.id, test.lane));
        }
        if !(1..=1800).contains(&test.timeout_seconds) {
            return Err(format!("{}: timeout_seconds must be 1..=1800", test.id));
        }
        match (&test.program, &test.direct) {
            (Some(program), None) => {
                if !program.starts_with("tests/") || !root.join(program).exists() {
                    return Err(format!("{}: missing program {program}", test.id));
                }
            }
            (None, Some(DirectCommand::Shell(command))) if !command.trim().is_empty() => {}
            (None, Some(DirectCommand::Argv(argv))) if !argv.is_empty() => {}
            (Some(_), Some(_)) => return Err(format!("{}: set only program or direct", test.id)),
            _ => return Err(format!("{}: missing executable program/direct", test.id)),
        }
        let actual: BTreeSet<_> = test.modes.keys().map(String::as_str).collect();
        let expected: BTreeSet<_> = MODES.into_iter().collect();
        if actual != expected {
            return Err(format!("{}: modes must be exactly {expected:?}", test.id));
        }
        for (mode, recipe) in &test.modes {
            validate_mode(&test.id, mode, recipe)?;
        }
    }
    Ok(())
}

fn validate_mode(id: &str, mode: &str, recipe: &ModeRecipe) -> Result<(), String> {
    validate_mode_workdir(
        id,
        mode,
        recipe.workdir.as_deref(),
        &recipe.backends_enabled,
    )?;
    let expected: BTreeSet<&str> = if mode == "naked" {
        ["native"].into_iter().collect()
    } else {
        BACKENDS.into_iter().collect()
    };
    let enabled: BTreeSet<_> = recipe.backends_enabled.iter().map(String::as_str).collect();
    let disabled: BTreeSet<_> = recipe
        .backends_disabled
        .keys()
        .map(String::as_str)
        .collect();
    if enabled.len() != recipe.backends_enabled.len()
        || !enabled.is_disjoint(&disabled)
        || enabled.union(&disabled).copied().collect::<BTreeSet<_>>() != expected
    {
        return Err(format!("{id}: {mode} does not partition {expected:?}"));
    }
    if recipe
        .backends_disabled
        .values()
        .any(|reason| reason.trim().is_empty())
    {
        return Err(format!("{id}: {mode} has an empty backend-disabled reason"));
    }
    let ci = CiSelection::validate(
        &enabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &disabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
    .map_err(|error| format!("{id}: {mode} {error}"))?;
    if mode == "naked" && ci.any_selected() {
        return Err(format!(
            "{id}: naked is opt-in meta-CI and must set ci=false"
        ));
    }
    for backend in recipe.guest_args.keys() {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} guest_args names disabled backend {backend}"
            ));
        }
    }
    Ok(())
}

pub fn validate_mode_workdir(
    id: &str,
    mode: &str,
    workdir: Option<&str>,
    backends_enabled: &[String],
) -> Result<(), String> {
    let Some(workdir) = workdir else {
        return Ok(());
    };
    if !matches!(mode, "verify" | "chaos" | "custom") {
        return Err(format!(
            "{id}: {mode} workdir is supported only by Hermit run modes"
        ));
    }
    if !Path::new(workdir).is_absolute() {
        return Err(format!("{id}: {mode} workdir must be an absolute path"));
    }
    // The DBT dispatcher currently rebuilds the guest launch from program,
    // arguments, and environment only; it does not preserve Command::current_dir.
    if backends_enabled.iter().any(|backend| backend == "dbt") {
        return Err(format!(
            "{id}: {mode} workdir is unsupported when DBT is enabled"
        ));
    }
    Ok(())
}

fn ci_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct CellRunSpec {
    pub id: CellId,
    pub lane: String,
    pub category: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub timeout_seconds: u64,
    pub verdict_path: Option<PathBuf>,
    pub verification_log_dir: Option<PathBuf>,
    pub sabre_path_evidence: Option<PathBuf>,
    pub cell_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptResult {
    pub index: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub observation_sha256: Option<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    pub stdout: String,
    pub stderr: String,
    pub verification_report: Option<String>,
    pub verification_report_sha256: Option<String>,
    pub sabre_path_evidence: Option<String>,
    pub sabre_path_evidence_sha256: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SabrePathRecord {
    schema: u64,
    guest_rpc_observed: bool,
    ptrace_fallback_sites: u64,
    trusted_shared_object_sites: u64,
    trusted_shared_objects: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CellResult {
    pub schema: u64,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub binary_sha256: Option<String>,
    pub test_sha256: String,
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: Option<String>,
    pub classification: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub duration_ms: u128,
    pub log_level: Option<String>,
    pub effective_args: Vec<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    pub relaxations: Vec<String>,
    pub execution_path: Option<JsonValue>,
    pub diversity: Option<JsonValue>,
    pub attempts: Vec<AttemptResult>,
    pub reason: Option<String>,
}

pub struct RunContext {
    pub root: PathBuf,
    pub hermit_bin: PathBuf,
    pub result_root: PathBuf,
    pub build_root: PathBuf,
    pub run_id: String,
    pub source_sha: String,
    pub source_dirty: bool,
    pub prebuilt: bool,
    pub keep_logs: bool,
    pub run_verify_strict: bool,
    pub record_verify_strict: bool,
}

impl RunContext {
    pub fn from_env(root: PathBuf, prebuilt: bool) -> Result<Self, String> {
        let source_sha = git(&root, &["rev-parse", "HEAD"])?;
        let source_dirty =
            !git(&root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
        let result_root = std::env::var_os("E2E_RESULT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("ignored/e2e"));
        let run_id = std::env::var("E2E_RUN_ID").unwrap_or_else(|_| {
            format!(
                "local-{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                std::process::id()
            )
        });
        let build_root = std::env::var_os("E2E_BUILD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| result_root.join("build").join(&source_sha));
        let hermit_bin = std::env::var_os("HERMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/debug/hermit"));
        // Published main still exposes the legacy `--verify-strict` spelling;
        // the canonical-only cutover removes it and makes bare `--verify`
        // canonical.  Detect the running binary rather than keying behavior to
        // a source SHA.  Whichever spelling executes is retained verbatim in
        // the result row, so this bridge cannot hide the comparison policy.
        let run_verify_strict =
            command_help_contains(&hermit_bin, &["run", "--help"], "--verify-strict");
        let record_verify_strict = command_help_contains(
            &hermit_bin,
            &["record", "start", "--help"],
            "--verify-strict",
        );
        Ok(Self {
            root,
            hermit_bin,
            result_root,
            build_root,
            run_id,
            source_sha,
            source_dirty,
            prebuilt,
            keep_logs: std::env::var("E2E_KEEP_VERIFY_LOGS").as_deref() == Ok("1"),
            run_verify_strict,
            record_verify_strict,
        })
    }
}

pub fn prepare_test(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
) -> Result<Vec<String>, String> {
    prepare_dirs(&context.root, dir)?;
    if context.prebuilt && cell.test.program.is_some() {
        let source = context
            .build_root
            .join(cell.test.id.replace('/', "-"))
            .join("fixtures");
        if !source.is_dir() {
            return Err(format!("prebuilt fixture is missing: {}", source.display()));
        }
        let destination = dir.join("fixtures");
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("cannot clear {}: {e}", destination.display()))?;
        }
        copy_tree(&source, &destination)?;
    }
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    let guest_args = mode.guest_args.get(backend).cloned().unwrap_or_default();
    let mut guest = match (&cell.test.program, &cell.test.direct) {
        (Some(program), None) if program.ends_with(".c") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"]
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.cflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                run_preparation(context, dir, "cc", &args, cell.test.timeout_seconds)?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".rs") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-O".to_string()];
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.rustflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                run_preparation(context, dir, "rustc", &args, cell.test.timeout_seconds)?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".sh") => {
            let path = context.root.join(program).to_string_lossy().into_owned();
            if !context.prebuilt {
                run_preparation(
                    context,
                    dir,
                    &path,
                    &["--prepare".into()],
                    cell.test.timeout_seconds,
                )?;
            }
            vec![path, "--run".into()]
        }
        (None, Some(DirectCommand::Shell(command))) => {
            let mut argv = vec!["bash".into(), "-c".into(), command.clone()];
            if !guest_args.is_empty() {
                argv.push("--".into());
            }
            argv
        }
        (None, Some(DirectCommand::Argv(argv))) => argv.clone(),
        _ => return Err(format!("{} has unsupported program kind", cell.test.id)),
    };
    guest.extend(guest_args);
    Ok(guest)
}

fn require_executable_program(path: &Path, captures: &Path) -> Result<(), String> {
    let executable = path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if executable {
        return Ok(());
    }
    let diagnostic = fs::read_to_string(captures.join("prepare.stderr"))
        .ok()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(captures.join("prepare.stdout"))
                .ok()
                .filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default();
    Err(format!(
        "compiled guest is missing or not executable: {}{}",
        path.display(),
        if diagnostic.is_empty() {
            String::new()
        } else {
            format!("\n{diagnostic}")
        }
    ))
}

fn run_preparation(
    context: &RunContext,
    dir: &Path,
    program: &str,
    args: &[String],
    timeout: u64,
) -> Result<(), String> {
    let captures = dir.join("captures");
    let output = execute_process(
        &context.root,
        program,
        args,
        &preparation_env(dir),
        &captures.join("prepare.stdout"),
        &captures.join("prepare.stderr"),
        timeout,
    )?;
    if output.timed_out || !output.status.success() {
        return Err(format!("fixture preparation failed for {program}"));
    }
    Ok(())
}

pub fn build_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: &str,
    seed: Option<i64>,
) -> Result<CellRunSpec, String> {
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode_recipe = &cell.test.modes[&cell.id.mode];
    let mut env = cell_env(&dir, cell.id.mode != "naked");
    let sabre_path_evidence = (backend == "sabre").then(|| {
        dir.join("captures")
            .join(format!("{}-{attempt}.sabre-path.jsonl", cell.id.mode))
    });
    if let Some(path) = &sabre_path_evidence {
        fs::create_dir_all(path.parent().expect("evidence path has a parent"))
            .map_err(|e| e.to_string())?;
        fs::write(path, b"").map_err(|e| e.to_string())?;
        env.insert(
            "HERMIT_SABRE_PATH_EVIDENCE".into(),
            path.to_string_lossy().into_owned(),
        );
    }
    let verdict = dir.join(format!("verify-{attempt}.json"));
    let mut verification_log_dir = None;
    let (argv, verdict_path) = match cell.id.mode.as_str() {
        "naked" => (guest_argv.clone(), None),
        "verify" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--backend".into(),
                backend.into(),
                "--strict".into(),
            ];
            if context.run_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
            ]);
            if context.keep_logs {
                let logs = dir.join(format!("verify-logs/verify-{attempt}"));
                fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
                argv.extend([
                    "--keep-logs".into(),
                    "--verify-log-dir".into(),
                    logs.to_string_lossy().into_owned(),
                ]);
                verification_log_dir = Some(logs);
            }
            append_workdir_arg(&mut argv, mode_recipe.workdir.as_deref());
            append_guest_env_args(&mut argv, &env);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "replay" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "record".into(),
                "start".into(),
                "--strict".into(),
            ];
            if context.record_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--data-dir".into(),
                dir.join("recording").to_string_lossy().into_owned(),
                "--record-timeout".into(),
                cell.test.timeout_seconds.to_string(),
            ]);
            append_guest_env_args(&mut argv, &env);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "chaos" => {
            let seed = seed.ok_or_else(|| "chaos attempt requires a seed".to_string())?;
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--backend".into(),
                backend.into(),
                "--strict".into(),
            ];
            if context.run_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-allow=both".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--chaos".into(),
                "--sched-heuristic=random".into(),
                format!("--seed={seed}"),
            ]);
            append_workdir_arg(&mut argv, mode_recipe.workdir.as_deref());
            append_guest_env_args(&mut argv, &env);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "custom" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--backend".into(),
                backend.into(),
            ];
            argv.extend(cell.test.modes["custom"].args.clone());
            append_workdir_arg(&mut argv, mode_recipe.workdir.as_deref());
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, None)
        }
        mode => return Err(format!("unsupported mode {mode}")),
    };
    Ok(CellRunSpec {
        id: cell.id.clone(),
        lane: cell.test.lane.clone(),
        category: cell.category.clone(),
        cwd: context.root.clone(),
        env,
        argv,
        guest_argv,
        timeout_seconds: cell.test.timeout_seconds,
        verdict_path,
        verification_log_dir,
        sabre_path_evidence,
        cell_dir: dir,
    })
}

pub fn execute_spec(spec: &CellRunSpec, index: &str) -> Result<AttemptResult, String> {
    if spec.argv.is_empty() {
        return Err("empty cell argv".into());
    }
    if let Some(verdict) = &spec.verdict_path {
        let _ = fs::remove_file(verdict);
    }
    let tmp = spec.cell_dir.join("tmp");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| format!("cannot reset {}: {e}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let captures = spec.cell_dir.join("captures");
    fs::create_dir_all(&captures).map_err(|e| e.to_string())?;
    let stdout_path = captures.join(format!("{}-{index}.stdout", spec.id.mode));
    let stderr_path = captures.join(format!("{}-{index}.stderr", spec.id.mode));
    let started = Instant::now();
    let output = execute_process(
        &spec.cwd,
        &spec.argv[0],
        &spec.argv[1..],
        &spec.env,
        &stdout_path,
        &stderr_path,
        spec.timeout_seconds,
    )?;
    if spec.id.mode == "verify" && spec.id.backend.as_deref() == Some("ptrace") {
        if let Some(directory) = &spec.verification_log_dir {
            normalize_ptrace_golden(&spec.argv[0], directory)?;
        }
    }
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let mut outcome = if output.timed_out || !output.status.success() {
        "FAIL"
    } else {
        "PASS"
    }
    .to_string();
    let mut reason = output
        .timed_out
        .then(|| format!("cell exceeded {} s", spec.timeout_seconds));
    let mut error_kind = None;
    let launch_refusal = spec.id.mode != "naked"
        && !output.status.success()
        && stdout.is_empty()
        && stderr.lines().next().is_some_and(|line| {
            line.starts_with("Error: Program ")
                || line.starts_with("Error: Could not resolve program ")
        });
    if launch_refusal {
        outcome = "ERROR".into();
        error_kind = Some("guest-launch-refused".into());
        reason = Some(format!(
            "guest launch refused before execution: {}",
            stderr
                .lines()
                .next()
                .unwrap_or("Error: unknown launch refusal")
                .trim_start_matches("Error: ")
        ));
    }
    let mut report_json = None;
    let mut report_sha = None;
    let (sabre_path_evidence, sabre_path_evidence_sha256) = spec
        .sabre_path_evidence
        .as_ref()
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| {
            (
                Some(String::from_utf8_lossy(&bytes).into_owned()),
                Some(hex_digest(&bytes)),
            )
        })
        .unwrap_or((None, None));
    if let Some(path) = &spec.verdict_path {
        match fs::read(path) {
            Ok(bytes) => {
                report_sha = Some(hex_digest(&bytes));
                report_json = Some(String::from_utf8_lossy(&bytes).into_owned());
                match VerificationReport::from_json_slice(&bytes) {
                    Ok(report) => {
                        if launch_refusal {
                            // The process never created a guest.  A report at
                            // this point is stale or otherwise unrelated and
                            // cannot supersede the refusal classification.
                        } else if report.verdict == "no_result"
                            && !output.timed_out
                            && output.status.code().is_some_and(|code| code != 0)
                        {
                            // The producer correctly has no comparison to
                            // report, but an ordinary nonzero process exit is
                            // still a completed failure rather than unknown
                            // infrastructure state.
                            outcome = "FAIL".into();
                            error_kind = None;
                            reason = Some(format!(
                                "{} exited with status {} before producing a terminal comparison",
                                spec.id.mode,
                                output.status.code().unwrap()
                            ));
                        } else if let Err(error) = report.require_canonical_comparison() {
                            outcome = "ERROR".into();
                            error_kind = Some("incomplete-verification-evidence".into());
                            reason = Some(error);
                        } else if let Err(error) = report.require_canonical_match() {
                            outcome = "FAIL".into();
                            reason = Some(error);
                        } else if !output.timed_out
                            && (output.status.success() || spec.id.mode == "chaos")
                        {
                            // Chaos deliberately admits a reproduced nonzero
                            // guest class. Verify and replay still require the
                            // Hermit process itself to succeed, and no receipt
                            // may erase a timeout.
                            outcome = "PASS".into();
                            reason = None;
                        } else if reason.is_none() {
                            reason = Some(format!(
                                "{} exited with status {}",
                                spec.id.mode,
                                output.status.code().unwrap_or(128)
                            ));
                        }
                    }
                    Err(error) => {
                        outcome = "ERROR".into();
                        error_kind = Some("incomplete-verification-evidence".into());
                        reason = Some(format!("verification report is unreadable: {error}"));
                    }
                }
            }
            Err(_error) if launch_refusal => {}
            Err(error) => {
                outcome = "ERROR".into();
                error_kind = Some("incomplete-verification-evidence".into());
                reason = Some(format!("verification report is missing: {error}"));
            }
        }
    }
    Ok(AttemptResult {
        index: index.into(),
        outcome,
        error_kind,
        status: output.status.code(),
        signal: std::os::unix::process::ExitStatusExt::signal(&output.status),
        timed_out: output.timed_out,
        duration_ms: started.elapsed().as_millis(),
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout,
        stderr,
        verification_report: report_json,
        verification_report_sha256: report_sha,
        sabre_path_evidence,
        sabre_path_evidence_sha256,
        reason,
    })
}

fn normalize_ptrace_golden(hermit: &str, directory: &Path) -> Result<(), String> {
    let mut run1 = fs::read_dir(directory)
        .map_err(|e| format!("cannot read {}: {e}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect::<Vec<_>>();
    run1.sort();
    let normalized = directory.join("normalized-ptrace-golden.log");
    let status_path = directory.join("normalized-ptrace-golden.status");
    let status = if run1.len() == 1 && run1[0].metadata().is_ok_and(|metadata| metadata.len() > 0) {
        let output = Command::new(hermit)
            .arg("log-diff")
            .arg(&run1[0])
            .output()
            .map_err(|e| format!("cannot normalize {}: {e}", run1[0].display()))?;
        if output.status.success() {
            fs::write(&normalized, output.stdout).map_err(|e| e.to_string())?;
        } else {
            fs::write(&normalized, b"").map_err(|e| e.to_string())?;
        }
        output.status.code().unwrap_or(1)
    } else {
        fs::write(&normalized, b"").map_err(|e| e.to_string())?;
        2
    };
    fs::write(status_path, format!("{status}\n")).map_err(|e| e.to_string())
}

struct ProcessOutput {
    status: ExitStatus,
    timed_out: bool,
}

fn execute_process(
    cwd: &Path,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdout: &Path,
    stderr: &Path,
    timeout_seconds: u64,
) -> Result<ProcessOutput, String> {
    let stdout_file = File::create(stdout).map_err(|e| format!("{}: {e}", stdout.display()))?;
    let stderr_file = File::create(stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    command.envs(env.iter());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot execute {program}: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(ProcessOutput {
                status,
                timed_out: false,
            });
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            let grace = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    return Ok(ProcessOutput {
                        status,
                        timed_out: true,
                    });
                }
                if Instant::now() >= grace {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let status = child.wait().map_err(|e| e.to_string())?;
            return Ok(ProcessOutput {
                status,
                timed_out: true,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn run_cell(context: &RunContext, cell: &SelectedCell) -> Result<CellResult, String> {
    let slug = format!(
        "{}-{}-{}",
        cell.id.test.replace('/', "-"),
        cell.id.mode,
        cell.id.backend.as_deref().unwrap_or("none")
    );
    let dir = context
        .result_root
        .join("runs")
        .join(&context.run_id)
        .join(slug);
    let started = Instant::now();
    let binary_before = fs::read(&context.hermit_bin)
        .ok()
        .map(|bytes| hex_digest(&bytes));
    let guest = prepare_test(context, cell, &dir)?;
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    let mut attempts = Vec::new();
    match cell.id.mode.as_str() {
        "naked" => {
            for index in 1..=mode.runs.unwrap_or(3) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                )?;
                attempts.push(execute_observed(
                    &spec,
                    &index.to_string(),
                    &cell.test.observation,
                    &dir,
                )?);
            }
        }
        "chaos" => {
            let seeds = mode
                .seeds
                .as_ref()
                .ok_or_else(|| format!("{} chaos has no seeds", cell.id.test))?;
            if seeds.is_empty() {
                return Err(format!("{} chaos has no seeds", cell.id.test));
            }
            for seed in seeds {
                let index = format!("seed-{seed}");
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index,
                    Some(*seed),
                )?;
                attempts.push(execute_observed(
                    &spec,
                    &index,
                    &cell.test.observation,
                    &dir,
                )?);
            }
        }
        "custom" => {
            for index in 1..=mode.assert.as_ref().and_then(|a| a.runs).unwrap_or(1) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                )?;
                attempts.push(execute_observed(
                    &spec,
                    &index.to_string(),
                    &cell.test.observation,
                    &dir,
                )?);
            }
        }
        _ => {
            let spec = build_spec(context, cell, dir.clone(), guest.clone(), "1", None)?;
            attempts.push(execute_observed(&spec, "1", &cell.test.observation, &dir)?);
        }
    }
    let hashes = attempts
        .iter()
        .filter_map(|attempt| attempt.observation_sha256.clone())
        .collect::<Vec<_>>();
    let distinct = hashes.iter().collect::<BTreeSet<_>>().len() as u64;
    let mut outcome = if attempts.iter().all(|attempt| attempt.outcome == "PASS") {
        "PASS"
    } else if attempts.iter().any(|attempt| attempt.outcome == "ERROR") {
        "ERROR"
    } else {
        "FAIL"
    }
    .to_string();
    let mut reason = attempts.iter().find_map(|attempt| attempt.reason.clone());
    let mut error_kind = attempts
        .iter()
        .find_map(|attempt| attempt.error_kind.clone());
    if cell.id.mode == "naked" {
        let minimum = mode
            .assert
            .as_ref()
            .and_then(|a| a.min_distinct)
            .unwrap_or(2);
        if distinct < minimum {
            outcome = "FAIL".into();
            reason = Some(format!(
                "naked observed {distinct} distinct outcomes, need {minimum}"
            ));
        }
    }
    if cell.id.mode == "custom"
        && mode.assert.as_ref().and_then(|a| a.repeat_identical) == Some(true)
        && distinct != 1
    {
        outcome = "FAIL".into();
        reason = Some(format!("custom observed {distinct} distinct outcomes"));
    }
    if cell.id.mode == "chaos" {
        let assert = mode.assert.as_ref().cloned().unwrap_or_default();
        let pass_count = attempts
            .iter()
            .filter(|attempt| attempt.status == Some(0))
            .count() as u64;
        let failure_count = attempts.len() as u64 - pass_count;
        let diversity = diversity_evidence(
            &hashes,
            mode.outcome_classes,
            assert.min_distinct.unwrap_or(2),
            assert.min_normalized_entropy,
        );
        let normalized_entropy = diversity["normalized_entropy"].as_f64().unwrap_or(0.0);
        if distinct < assert.min_distinct.unwrap_or(2)
            || pass_count < assert.min_passes.unwrap_or(0)
            || failure_count < assert.min_failures.unwrap_or(0)
            || assert
                .min_normalized_entropy
                .is_some_and(|minimum| normalized_entropy < minimum)
        {
            outcome = "FAIL".into();
            reason = Some(format!(
                "chaos distinct={distinct} passes={pass_count} failures={failure_count} normalized_entropy={normalized_entropy:.4}"
            ));
        }
    }
    let execution_path = match summarize_sabre_path_evidence(&attempts) {
        Ok(value) => value,
        Err(error) => {
            outcome = "ERROR".into();
            error_kind = Some("invalid-backend-evidence".into());
            reason = Some(error);
            None
        }
    };
    if outcome == "PASS"
        && execution_path
            .as_ref()
            .is_some_and(|evidence| evidence["eligible"] != true)
    {
        outcome = "FAIL".into();
        reason = Some("SaBRe execution path is incomplete or used fallback/native sites".into());
    }
    let literal_argv = attempts
        .first()
        .map(|attempt| attempt.argv.clone())
        .unwrap_or_default();
    let literal_guest_argv = attempts
        .first()
        .map(|attempt| attempt.guest_argv.clone())
        .unwrap_or_else(|| guest.clone());
    let literal_env = attempts
        .first()
        .map(|attempt| attempt.env.clone())
        .unwrap_or_else(|| cell_env(&dir, cell.id.mode != "naked"));
    let literal_cwd = attempts
        .first()
        .map(|attempt| attempt.cwd.clone())
        .unwrap_or_else(|| context.root.to_string_lossy().into_owned());
    let literal_shell_command = attempts
        .first()
        .map(|attempt| attempt.shell_command.clone())
        .unwrap_or_default();
    let test_sha = test_digest(&context.root, &cell.test)?;
    let binary_sha = fs::read(&context.hermit_bin)
        .ok()
        .map(|bytes| hex_digest(&bytes));
    if binary_before.is_some() && binary_before != binary_sha {
        outcome = "ERROR".into();
        reason = Some("Hermit binary changed while the cell was executing".into());
    }
    Ok(CellResult {
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        hermit_sha: context.source_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: binary_sha,
        test_sha256: test_sha,
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        // `required` records that the selected backend is an executable cell.
        // `ci` controls ordinary validate selection, not whether a manual red
        // measurement is real. Pressure re-runs enabled ci=false cells and
        // must be able to admit their evidence under the same identity.
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome,
        error_kind,
        duration_ms: started.elapsed().as_millis(),
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: literal_argv.iter().skip(1).cloned().collect(),
        argv: literal_argv,
        guest_argv: literal_guest_argv,
        env: literal_env,
        cwd: literal_cwd,
        shell_command: literal_shell_command,
        relaxations: Vec::new(),
        execution_path,
        diversity: (cell.id.mode == "chaos").then(|| {
            diversity_evidence(
                &hashes,
                mode.outcome_classes,
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_distinct)
                    .unwrap_or(2),
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_normalized_entropy),
            )
        }),
        attempts,
        reason,
    })
}

/// Publish a durable third-outcome row when a cell could not reach execution.
///
/// Infrastructure failures are neither product failures nor admissible green
/// evidence.  Keeping an explicit row prevents preparation/spawn failures from
/// silently disappearing while leaving literal argv empty rather than
/// inventing a command that was never executed.
pub fn infrastructure_error_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let slug = format!(
        "{}-{}-{}",
        cell.id.test.replace('/', "-"),
        cell.id.mode,
        cell.id.backend.as_deref().unwrap_or("none")
    );
    let dir = context
        .result_root
        .join("runs")
        .join(&context.run_id)
        .join(slug);
    CellResult {
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        hermit_sha: context.source_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: fs::read(&context.hermit_bin)
            .ok()
            .map(|bytes| hex_digest(&bytes)),
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome: "ERROR".into(),
        error_kind: Some("infrastructure".into()),
        duration_ms: 0,
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: cell_env(&dir, cell.id.mode != "naked"),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: Vec::new(),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        reason: Some(reason),
    }
}

fn summarize_sabre_path_evidence(attempts: &[AttemptResult]) -> Result<Option<JsonValue>, String> {
    let is_sabre = attempts.iter().any(|attempt| {
        attempt
            .argv
            .windows(2)
            .any(|window| window[0] == "--backend" && window[1] == "sabre")
    });
    if !is_sabre {
        return Ok(None);
    }
    let mut executions = Vec::<SabrePathRecord>::new();
    for line in attempts
        .iter()
        .flat_map(|attempt| attempt.sabre_path_evidence.as_deref().unwrap_or("").lines())
    {
        executions.push(
            serde_json::from_str::<SabrePathRecord>(line)
                .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?,
        );
    }
    if executions.iter().any(|row| row.schema != 1) {
        return Err("invalid SaBRe path-evidence record: schema must be 1".into());
    }
    let expected = attempts
        .iter()
        .map(|attempt| {
            if attempt.argv.iter().any(|arg| arg == "--verify") {
                2
            } else {
                1
            }
        })
        .sum::<usize>();
    let complete = executions.len() == expected;
    let guest_rpc_observed = complete && executions.iter().all(|row| row.guest_rpc_observed);
    let ptrace_fallback_sites = executions
        .iter()
        .map(|row| row.ptrace_fallback_sites)
        .sum::<u64>();
    let trusted_shared_object_sites = executions
        .iter()
        .map(|row| row.trusted_shared_object_sites)
        .sum::<u64>();
    let trusted_shared_objects = executions
        .iter()
        .flat_map(|row| row.trusted_shared_objects.iter().cloned())
        .collect::<BTreeSet<_>>();
    let eligible = complete
        && guest_rpc_observed
        && ptrace_fallback_sites == 0
        && trusted_shared_object_sites == 0;
    Ok(Some(serde_json::json!({
        "schema": 1,
        "expected_execution_count": expected,
        "complete": complete,
        "execution_count": executions.len(),
        "guest_rpc_observed": guest_rpc_observed,
        "ptrace_fallback_sites": ptrace_fallback_sites,
        "trusted_shared_object_sites": trusted_shared_object_sites,
        "trusted_shared_objects": trusted_shared_objects,
        "eligible": eligible,
        "executions": executions
    })))
}

fn diversity_evidence(
    hashes: &[String],
    outcome_classes: Option<u64>,
    min_distinct: u64,
    min_normalized_entropy: Option<f64>,
) -> JsonValue {
    let mut counts = BTreeMap::<&str, u64>::new();
    for hash in hashes {
        *counts.entry(hash).or_default() += 1;
    }
    let total = hashes.len() as f64;
    let entropy_bits = counts.values().fold(0.0, |entropy, count| {
        let share = *count as f64 / total.max(1.0);
        if share > 0.0 {
            entropy - share * share.log2()
        } else {
            entropy
        }
    });
    let normalized_entropy = outcome_classes
        .filter(|classes| *classes >= 2)
        .map(|classes| entropy_bits / (classes as f64).log2())
        .unwrap_or(0.0);
    let minority_share = counts
        .values()
        .min()
        .map(|count| *count as f64 / total.max(1.0))
        .unwrap_or(0.0);
    let histogram = counts
        .iter()
        .map(|(hash, count)| format!("{}:{count}", &hash[..hash.len().min(12)]))
        .collect::<Vec<_>>();
    serde_json::json!({
        "distinct": counts.len(), "outcome_classes": outcome_classes, "seeds": hashes.len(),
        "entropy_bits": entropy_bits, "normalized_entropy": normalized_entropy,
        "minority_share": minority_share,
        "oracle_saturated": outcome_classes.is_some_and(|classes| min_distinct >= classes),
        "class_histogram": histogram,
        "min_normalized_entropy": min_normalized_entropy,
    })
}

pub fn append_result(path: &Path, result: &CellResult) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, result).map_err(|e| e.to_string())?;
    file.write_all(b"\n").map_err(|e| e.to_string())?;
    // The bucket runner publishes each completed cell before printing its
    // PASS/FAIL/ERROR line. Flush the row now rather than waiting for the
    // bucket's JUnit/summary epilogue, which an outer node timeout may kill.
    file.flush().map_err(|e| e.to_string())
}

pub fn write_junit(path: &Path, results: &[CellResult]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let failures = results
        .iter()
        .filter(|result| result.outcome == "FAIL")
        .count();
    let errors = results
        .iter()
        .filter(|result| result.outcome == "ERROR")
        .count();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"hermit-e2e\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\">\n",
        results.len()
    );
    for result in results {
        out.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}/{}/{}\" time=\"{:.3}\">",
            xml(&result.category),
            xml(&result.test),
            xml(&result.mode),
            xml(result.backend.as_deref().unwrap_or("none")),
            result.duration_ms as f64 / 1000.0
        ));
        if result.outcome == "FAIL" {
            out.push_str(&format!(
                "<failure>{}</failure>",
                xml(result.reason.as_deref().unwrap_or("failed"))
            ));
        }
        if result.outcome == "ERROR" {
            out.push_str(&format!(
                "<error>{}</error>",
                xml(result.reason.as_deref().unwrap_or("error"))
            ));
        }
        out.push_str("</testcase>\n");
    }
    out.push_str("</testsuite>\n");
    fs::write(path, out).map_err(|e| e.to_string())
}

fn prepare_dirs(root: &Path, dir: &Path) -> Result<(), String> {
    for child in [
        "home",
        "xdg-config",
        "tmp",
        "fixtures",
        "recording",
        "captures",
    ] {
        fs::create_dir_all(dir.join(child)).map_err(|e| e.to_string())?;
    }
    let xdg = root.join("tests/e2e/xdg-config");
    if xdg.is_dir() {
        copy_tree(&xdg, &dir.join("xdg-config"))?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = destination.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn cell_env(dir: &Path, verified: bool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        (
            "HOME".into(),
            dir.join("home").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            dir.join("xdg-config").to_string_lossy().into_owned(),
        ),
        (
            "E2E_TMPDIR".into(),
            if verified {
                "/tmp/hermit-e2e".into()
            } else {
                dir.join("tmp").to_string_lossy().into_owned()
            },
        ),
        (
            "E2E_FIXTURE_DIR".into(),
            dir.join("fixtures").to_string_lossy().into_owned(),
        ),
    ])
}

fn preparation_env(dir: &Path) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, false);
    let ambient_home = std::env::var_os("HOME");
    let rustup_home = std::env::var_os("RUSTUP_HOME");
    let cargo_home = std::env::var_os("CARGO_HOME");
    add_toolchain_homes(
        &mut env,
        rustup_home.as_deref(),
        cargo_home.as_deref(),
        ambient_home.as_deref(),
    );
    env
}

fn add_toolchain_homes(
    env: &mut BTreeMap<String, String>,
    rustup_home: Option<&OsStr>,
    cargo_home: Option<&OsStr>,
    ambient_home: Option<&OsStr>,
) {
    // Guest preparation is a build-time context, not guest execution. Preserve
    // the invoking toolchain state even though the cell's HOME is isolated;
    // these values are deliberately absent from `cell_env` and never become
    // Hermit `--env` arguments.
    for (name, explicit, fallback) in [
        ("RUSTUP_HOME", rustup_home, ".rustup"),
        ("CARGO_HOME", cargo_home, ".cargo"),
    ] {
        let value = explicit
            .map(PathBuf::from)
            .or_else(|| ambient_home.map(|home| PathBuf::from(home).join(fallback)));
        if let Some(value) = value {
            env.insert(name.into(), value.to_string_lossy().into_owned());
        }
    }
}

fn shell_command(cwd: &str, env: &BTreeMap<String, String>, argv: &[String]) -> String {
    let mut words = vec!["cd".into(), shell_quote(cwd), "&&".into(), "env".into()];
    words.extend(
        env.iter()
            .map(|(name, value)| shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

fn shell_quote(value: &str) -> String {
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

fn append_guest_env_args(argv: &mut Vec<String>, env: &BTreeMap<String, String>) {
    // `--base-env=minimal` intentionally removes the invoking host's ambient
    // environment. These six values are the manifest runner's explicit guest
    // contract, so forward them literally instead of relying on inheritance.
    for name in [
        "LC_ALL",
        "TZ",
        "HOME",
        "XDG_CONFIG_HOME",
        "E2E_TMPDIR",
        "E2E_FIXTURE_DIR",
    ] {
        let value = env
            .get(name)
            .expect("cell environment contains every forwarded guest value");
        argv.push("--env".into());
        argv.push(format!("{name}={value}"));
    }
}

fn append_workdir_arg(argv: &mut Vec<String>, workdir: Option<&str>) {
    if let Some(workdir) = workdir {
        argv.extend(["--workdir".into(), workdir.into()]);
    }
}

fn observation_hash(observation: &Observation, attempt: &AttemptResult, dir: &Path) -> String {
    let mut digest = Sha256::new();
    if observation.status {
        digest.update(attempt.status.unwrap_or(-1).to_le_bytes());
        digest.update(attempt.signal.unwrap_or(0).to_le_bytes());
    }
    if observation.stdout {
        digest.update(attempt.stdout.as_bytes());
    }
    if observation.stderr {
        digest.update(attempt.stderr.as_bytes());
    }
    for artifact in &observation.artifacts {
        if let Ok(bytes) = fs::read(dir.join("tmp").join(artifact)) {
            digest.update(bytes);
        }
    }
    format!("{:x}", digest.finalize())
}

fn test_digest(root: &Path, test: &TestRecipe) -> Result<String, String> {
    let bytes = if let Some(program) = &test.program {
        fs::read(root.join(program)).map_err(|e| e.to_string())?
    } else {
        serde_json::to_vec(&test.direct.as_ref().map(|direct| match direct {
            DirectCommand::Shell(s) => vec![s.clone()],
            DirectCommand::Argv(v) => v.clone(),
        }))
        .map_err(|e| e.to_string())?
    };
    Ok(hex_digest(&bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_help_contains(program: &Path, args: &[&str], needle: &str) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).contains(needle)
                || String::from_utf8_lossy(&output.stderr).contains(needle)
        })
}

fn execute_observed(
    spec: &CellRunSpec,
    index: &str,
    observation: &Observation,
    dir: &Path,
) -> Result<AttemptResult, String> {
    let mut attempt = execute_spec(spec, index)?;
    attempt.observation_sha256 = Some(observation_hash(observation, &attempt, dir));
    Ok(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_selection::BackendCiDisabledReason;
    use crate::ci_selection::CiDisabledResult;

    fn recipe(ci: bool) -> TestRecipe {
        let disabled = BTreeMap::from([
            ("dbt".into(), "not qualified".into()),
            ("kvm".into(), "not qualified".into()),
            ("sabre".into(), "not qualified".into()),
            ("liteinst".into(), "not qualified".into()),
        ]);
        let mode = ModeRecipe {
            ci: CiSelectionSpec::Uniform(ci),
            ci_disabled_reason: (!ci)
                // A real sentence, because the shared-string form now carries the
                // same substance rule as a per-backend entry. "not selected yet"
                // is one of the placeholder phrases that rule bans outright.
                .then(|| {
                    CiDisabledReasonSpec::Uniform(
                        "fixture cell: ptrace only, other backends unmeasured here".into(),
                    )
                }),
            backends_enabled: vec!["ptrace".into()],
            backends_disabled: disabled,
            ..ModeRecipe::default()
        };
        TestRecipe {
            id: "fixture/test".into(),
            description: "fixture".into(),
            lane: "portable".into(),
            requires: Vec::new(),
            timeout_seconds: 10,
            occasional: false,
            program: None,
            direct: Some(DirectCommand::Argv(vec!["/bin/true".into()])),
            observation: Observation {
                status: true,
                stdout: true,
                stderr: true,
                artifacts: Vec::new(),
            },
            build: None,
            modes: BTreeMap::from([("verify".into(), mode)]),
            slow_reason: None,
            preprocessors: Vec::new(),
        }
    }

    #[test]
    fn required_and_enabled_are_distinct_populations() {
        let test = recipe(false);
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(test.id.clone(), ("fixture".into(), test))]),
        };
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            set.select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .len(),
            1
        );
        let cell = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-classification-bracket-{}",
            std::process::id()
        ));
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
        };
        assert_eq!(
            infrastructure_error_result(&context, &cell, "fixture".into()).classification,
            "required"
        );
    }

    #[test]
    fn completed_rows_are_readable_after_each_append() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-durable-row-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
        };
        let path = root.join("results.jsonl");
        for expected in 1..=2 {
            let result = infrastructure_error_result(&context, &cell, "fixture".into());
            append_result(&path, &result).unwrap();
            let rows = fs::read_to_string(&path).unwrap();
            assert_eq!(rows.lines().count(), expected);
            assert!(
                rows.lines()
                    .all(|line| serde_json::from_str::<JsonValue>(line).is_ok())
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_selection_is_per_backend_while_enabled_selection_keeps_the_failure_visible() {
        let mut test = recipe(false);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.ci = CiSelectionSpec::PerBackend(BTreeMap::from([
            ("ptrace".into(), true),
            ("liteinst".into(), false),
        ]));
        mode.ci_disabled_reason = Some(CiDisabledReasonSpec::PerBackend(BTreeMap::from([(
            "liteinst".into(),
            BackendCiDisabledReason {
                result: CiDisabledResult::DeterminismFailure,
                evidence: "ignored/results/liteinst.jsonl".into(),
                reason: "canonical comparison diverged at scheduler turn 10".into(),
            },
        )])));
        validate_mode("fixture/test", "verify", mode).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(test.id.clone(), ("fixture".into(), test))]),
        };
        let required = set
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].id.backend.as_deref(), Some("ptrace"));
        let enabled = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(enabled.len(), 2);
        assert!(
            enabled
                .iter()
                .any(|cell| cell.id.backend.as_deref() == Some("liteinst"))
        );
    }

    #[test]
    fn yaml_parses_structured_per_backend_selection() {
        let mode: ModeRecipe = serde_yaml::from_str(
            r#"
ci:
  ptrace: true
  liteinst: false
workdir: /tmp
ci_disabled_reason:
  liteinst:
    result: determinism-failure
    evidence: ignored/results/liteinst.jsonl
    reason: canonical comparison diverged at scheduler turn 10
backends_enabled: [ptrace, liteinst]
backends_disabled:
  dbt: unsupported
  kvm: unsupported
  sabre: unsupported
"#,
        )
        .unwrap();
        validate_mode("fixture/test", "verify", &mode).unwrap();
        assert_eq!(mode.workdir.as_deref(), Some("/tmp"));
        let policy = ci_selection(&mode).unwrap();
        assert!(policy.selected("ptrace"));
        assert!(!policy.selected("liteinst"));
        assert_eq!(
            policy.reason("liteinst").unwrap().result,
            Some(CiDisabledResult::DeterminismFailure)
        );
    }

    #[test]
    fn rejects_relative_run_workdir() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.workdir = Some("tmp".into());
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode).unwrap_err(),
            "fixture/test: verify workdir must be an absolute path"
        );
    }

    #[test]
    fn rejects_workdir_for_non_run_modes() {
        for mode in ["replay", "naked"] {
            assert_eq!(
                validate_mode_workdir("fixture/test", mode, Some("/tmp"), &[]).unwrap_err(),
                format!("fixture/test: {mode} workdir is supported only by Hermit run modes")
            );
        }
    }

    #[test]
    fn workdir_accepts_ptrace_and_rejects_dbt_or_mixed_modes() {
        let ptrace = vec!["ptrace".into()];
        assert_eq!(
            validate_mode_workdir("fixture/test", "verify", Some("/tmp"), &ptrace),
            Ok(())
        );

        for mode in ["verify", "chaos", "custom"] {
            for backends in [vec!["dbt".into()], vec!["ptrace".into(), "dbt".into()]] {
                assert_eq!(
                    validate_mode_workdir("fixture/test", mode, Some("/tmp"), &backends)
                        .unwrap_err(),
                    format!("fixture/test: {mode} workdir is unsupported when DBT is enabled")
                );
            }
        }
    }

    #[test]
    fn run_workdir_precedes_the_guest_separator() {
        let mut test = recipe(true);
        test.modes.get_mut("verify").unwrap().workdir = Some("/tmp".into());
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/tmp"])
            .unwrap();
        assert!(workdir < separator);
    }

    #[test]
    fn run_spec_records_correct_policy_without_hidden_portable_flags() {
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
        )
        .unwrap();
        assert!(spec.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(spec.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert!(!spec.argv.iter().any(|arg| arg == "--no-virtualize-cpuid"));
        assert!(
            !spec
                .argv
                .iter()
                .any(|arg| arg == "--max-timeslice=disabled")
        );
        for name in [
            "LC_ALL",
            "TZ",
            "HOME",
            "XDG_CONFIG_HOME",
            "E2E_TMPDIR",
            "E2E_FIXTURE_DIR",
        ] {
            assert!(spec.argv.windows(2).any(|window| {
                window[0] == "--env" && window[1].starts_with(&format!("{name}="))
            }));
        }
        for name in ["RUSTUP_HOME", "CARGO_HOME"] {
            assert!(!spec.env.contains_key(name));
            assert!(
                !spec
                    .argv
                    .iter()
                    .any(|arg| arg.starts_with(&format!("{name}=")))
            );
        }

        let mut replay_test = recipe(true);
        let replay_mode = replay_test.modes.remove("verify").unwrap();
        replay_test.modes.insert("replay".into(), replay_mode);
        let replay_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: replay_test.id.clone(),
                mode: "replay".into(),
                backend: Some("ptrace".into()),
            },
            test: replay_test,
            enabled: true,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
        )
        .unwrap();
        assert!(replay.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(replay.argv.windows(2).any(|window| {
            window[0] == "--verify-json" && window[1] == "/repo/results/replay-cell/verify-1.json"
        }));
        for name in [
            "LC_ALL",
            "TZ",
            "HOME",
            "XDG_CONFIG_HOME",
            "E2E_TMPDIR",
            "E2E_FIXTURE_DIR",
        ] {
            assert!(replay.argv.windows(2).any(|window| {
                window[0] == "--env" && window[1].starts_with(&format!("{name}="))
            }));
        }
    }

    #[test]
    fn preparation_preserves_toolchain_homes_outside_the_guest_environment() {
        let mut derived = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut derived,
            None,
            None,
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(derived["RUSTUP_HOME"], "/example/toolchain-root/.rustup");
        assert_eq!(derived["CARGO_HOME"], "/example/toolchain-root/.cargo");

        let mut explicit = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut explicit,
            Some(OsStr::new("/toolchains/rustup")),
            Some(OsStr::new("/toolchains/cargo")),
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(explicit["RUSTUP_HOME"], "/toolchains/rustup");
        assert_eq!(explicit["CARGO_HOME"], "/toolchains/cargo");

        let guest = cell_env(Path::new("/cell"), false);
        assert!(!guest.contains_key("RUSTUP_HOME"));
        assert!(!guest.contains_key("CARGO_HOME"));
    }

    #[test]
    fn recorded_shell_command_replays_literal_environment_and_argv() {
        let root = std::env::temp_dir().join(format!(
            "hermit-recorded-command-bracket-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let env = BTreeMap::from([("E2E_VALUE".into(), "space and ' quote".into())]);
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$E2E_VALUE\" = \"space and ' quote\" && test \"$1\" = \"arg with spaces\""
                .into(),
            "recorded-command".into(),
            "arg with spaces".into(),
        ];
        let literal = shell_command(&root.to_string_lossy(), &env, &argv);
        let status = Command::new("/bin/sh")
            .args(["-c", &literal])
            .status()
            .unwrap();
        assert!(status.success(), "recorded command failed: {literal}");
        fs::remove_dir_all(root).unwrap();
    }

    fn attempt_with_sabre_evidence(evidence: &str) -> AttemptResult {
        AttemptResult {
            index: "1".into(),
            outcome: "PASS".into(),
            error_kind: None,
            status: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            observation_sha256: Some("a".into()),
            argv: vec![
                "hermit".into(),
                "--backend".into(),
                "sabre".into(),
                "--verify".into(),
            ],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env hermit --backend sabre --verify".into(),
            stdout: String::new(),
            stderr: String::new(),
            verification_report: None,
            verification_report_sha256: None,
            sabre_path_evidence: Some(evidence.into()),
            sabre_path_evidence_sha256: Some("b".into()),
            reason: None,
        }
    }

    #[test]
    fn sabre_path_evidence_requires_every_execution_and_no_fallback() {
        let clean = r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}"#;
        let complete = attempt_with_sabre_evidence(&format!("{clean}\n{clean}\n"));
        assert_eq!(
            summarize_sabre_path_evidence(&[complete]).unwrap().unwrap()["eligible"],
            true
        );

        let short = attempt_with_sabre_evidence(&format!("{clean}\n"));
        let summary = summarize_sabre_path_evidence(&[short]).unwrap().unwrap();
        assert_eq!(summary["complete"], false);
        assert_eq!(summary["eligible"], false);

        let malformed = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
"#,
        );
        assert!(summarize_sabre_path_evidence(&[malformed]).is_err());
    }

    #[test]
    fn normalized_entropy_detects_a_narrowed_two_class_sweep() {
        let balanced = diversity_evidence(
            &["a".into(), "a".into(), "b".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        let narrowed = diversity_evidence(
            &["a".into(), "a".into(), "a".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        assert!(balanced["normalized_entropy"].as_f64().unwrap() > 0.99);
        assert!(narrowed["normalized_entropy"].as_f64().unwrap() < 0.82);
    }

    fn nonzero_with_canonical_receipt(mode: &str) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-status-bracket-{}-{mode}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let report = serde_json::to_string(&VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: "matched".into(),
            comparison: Some(crate::canonical_verdict::ComparisonReport {
                strictness: "canonical".into(),
                compare_logs: true,
                record_envelope: crate::canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
            }),
            compared_log_messages: Some(crate::canonical_verdict::ComparedLogMessages {
                left: 1,
                right: 1,
            }),
        })
        .unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/status".into(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$1\" > \"$2\"; exit 7".into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
        };
        let result = execute_spec(&spec, "1").unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn canonical_receipt_does_not_erase_process_failure_except_for_chaos() {
        let verify = nonzero_with_canonical_receipt("verify");
        assert_eq!(verify.outcome, "FAIL");
        assert_eq!(verify.status, Some(7));

        let replay = nonzero_with_canonical_receipt("replay");
        assert_eq!(replay.outcome, "FAIL");
        assert_eq!(replay.status, Some(7));

        let chaos = nonzero_with_canonical_receipt("chaos");
        assert_eq!(chaos.outcome, "PASS");
        assert_eq!(chaos.status, Some(7));
    }

    fn no_result_with_exit_status(status: i32) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-no-result-bracket-{}-{status}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let report = serde_json::to_string(&VerificationReport {
            verified: false,
            bitwise_parity: false,
            verdict: "no_result".into(),
            comparison: None,
            compared_log_messages: None,
        })
        .unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/no-result".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"printf %s "$1" > "$2"; exit "$3""#.into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
                status.to_string(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
        };
        let result = execute_spec(&spec, "1").unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn no_result_preserves_the_process_outcome_distinction() {
        let failed = no_result_with_exit_status(7);
        assert_eq!(failed.outcome, "FAIL");
        assert_eq!(failed.error_kind, None);
        assert_eq!(failed.status, Some(7));
        assert!(
            failed
                .reason
                .as_deref()
                .unwrap()
                .contains("before producing a terminal comparison")
        );

        let unknown = no_result_with_exit_status(0);
        assert_eq!(unknown.outcome, "ERROR");
        assert_eq!(
            unknown.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(unknown.status, Some(0));
    }

    #[test]
    fn prebuilt_copy_cannot_inherit_or_accept_a_nonexecutable_guest() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prebuilt-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let build_root = root.join("build");
        let source = build_root.join("fixture-test/fixtures");
        let cell_dir = root.join("cell");
        fs::create_dir_all(&source).unwrap();

        let mut test = recipe(true);
        test.program = Some("tests/fixture.c".into());
        test.direct = None;
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root,
            run_id: "fixture".into(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            prebuilt: true,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
        };

        fs::create_dir_all(cell_dir.join("fixtures")).unwrap();
        fs::write(cell_dir.join("fixtures/program"), b"stale").unwrap();
        fs::set_permissions(
            cell_dir.join("fixtures/program"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());
        assert!(!cell_dir.join("fixtures/program").exists());

        fs::write(source.join("program"), b"new").unwrap();
        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());

        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
