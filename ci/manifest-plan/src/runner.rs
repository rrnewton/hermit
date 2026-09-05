use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use detcore::logdiff::BitwiseInfoV1Comparison;
use detcore::logdiff::BitwiseInfoV1RunObservation;
use detcore::logdiff::ComparisonSideLabels;
use detcore_model::summary::PathEvidence;
use detcore_model::summary::RunSummary;
use flate2::Compression;
use flate2::GzBuilder;
use flate2::read::MultiGzDecoder;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

use crate::canonical_verdict::GuestRunDeterminism;
use crate::canonical_verdict::GuestRunResult;
use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::RuntimeStats;
use crate::canonical_verdict::Verdict;
pub use crate::canonical_verdict::VerificationReport;
pub use crate::canonical_verdict::VerificationRuntime;
use crate::ci_selection::CiDisabledReasonSpec;
use crate::ci_selection::CiSelection;
use crate::ci_selection::CiSelectionSpec;
use crate::host_capability::probe_host_capabilities;
use crate::stress_series::HostCapabilities;
use crate::stress_series::HostCapability;
#[cfg(test)]
use crate::stress_series::HostCapabilityVerdict;
#[cfg(test)]
use crate::timeouts::CALIBRATED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
use crate::timeouts::DEFAULTS_FILE;
#[cfg(test)]
use crate::timeouts::EXPLICIT_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::KVM_RUN_1709_CI_REMOVAL_COUNT;
use crate::timeouts::MANIFEST_SCHEMA;
#[cfg(test)]
use crate::timeouts::NON_CI_CELL_COUNT;
use crate::timeouts::ResolvedTestTimeouts;
use crate::timeouts::TimeoutMultipliers;
use crate::timeouts::resolve_test_timeouts;
use crate::timeouts::resolve_timeout_seconds;
use crate::timeouts::timeout_multipliers_from_env;
use crate::timeouts::validate_timeout_seconds;

const BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
const CELL_CPU_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CELL_CPU_ACCOUNTING_GRACE: Duration = Duration::from_secs(1);
pub const CELL_RESULT_SCHEMA: u64 = 4;
pub const E2E_MACHINE_SHORTNAME_ENV: &str = "E2E_MACHINE_SHORTNAME";
pub const E2E_KERNEL_VERSION_ENV: &str = "E2E_KERNEL_VERSION";
pub const E2E_RUN_INDEX_ENV: &str = "E2E_RUN_INDEX";

fn first_attempt() -> u64 {
    1
}

/// Closed vocabulary for manifest `requires` tokens.
///
/// The optional value is the host capability whose proven absence may withhold
/// a cell. A token mapped to `None` is still validated, but can never suppress
/// execution. Keeping the mapping here gives the manifest validator and the
/// executable harness one authority.
pub const REQUIRES_VOCABULARY: &[(&str, Option<HostCapability>)] = &[
    ("ar", None),
    ("bash", None),
    ("cc", None),
    ("cpuid", Some(HostCapability::CpuidFaulting)),
    ("cxx", None),
    ("date", None),
    ("du", None),
    ("find", None),
    ("gawk", None),
    ("git", None),
    ("hexdump", None),
    ("jq", None),
    ("kvm", None),
    ("linux", None),
    ("lua5.4", None),
    ("m4", None),
    ("node", None),
    ("openssl", None),
    ("perl", None),
    ("ptrace", None),
    ("python3", None),
    ("ruby", None),
    ("rustc", None),
    ("sqlite3", None),
    ("tclsh", None),
    ("userns", None),
    ("x86_64", None),
    ("zstd", None),
];

pub fn requires_capability(token: &str) -> Result<Option<HostCapability>, String> {
    REQUIRES_VOCABULARY
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, capability)| *capability)
        .ok_or_else(|| {
            format!(
                "unknown `requires` token `{token}`; the vocabulary is closed and an unrecognized \
                 name is refused rather than treated as a reason to omit a cell"
            )
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDefaults {
    pub schema: u64,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
    #[serde(default)]
    pub nextest: Vec<NextestTimeout>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextestTimeout {
    pub filter: String,
    pub timeout_seconds: u64,
    pub slow_reason: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDocument {
    pub schema: u64,
    pub bucket: String,
    pub timeout_seconds: Option<u64>,
    pub slow_reason: Option<String>,
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
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectCommand>,
    pub observation: Observation,
    pub build: Option<BuildRecipe>,
    pub modes: BTreeMap<String, ModeRecipe>,
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
    pub compare_io_buffers: Option<bool>,
    pub compare_io_buffers_disabled_reason: Option<String>,
    pub rcb_time: Option<bool>,
    pub rcb_time_disabled_reason: Option<String>,
    #[serde(default)]
    pub timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub cpu_timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub slow_reason: BTreeMap<String, String>,
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
    tests: BTreeMap<String, (String, u64, u64, TestRecipe)>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CellId {
    pub test: String,
    pub mode: String,
    pub backend: Option<String>,
}

const SCHEDULED_JOBS_ENV: &str = "HERMIT_E2E_SCHEDULED_JOBS";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const FIXED_GUEST_WORKDIR: &str = "/tmp/test";
const FIXED_WORKDIR_SOURCE_DIR: &str = "workdir";

fn fixed_workdir_source_for_attempt(cell_dir: &Path, attempt: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(attempt).components();
    let is_one_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(attempt)
    );
    if !is_one_normal_component {
        return Err(format!(
            "invalid attempt label {attempt:?}: expected exactly one normal path component"
        ));
    }
    Ok(cell_dir.join(FIXED_WORKDIR_SOURCE_DIR).join(attempt))
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
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
}

impl ManifestSet {
    pub fn load(root: &Path) -> Result<Self, String> {
        let dir = root.join("tests/e2e/manifests");
        let defaults_path = dir.join(DEFAULTS_FILE);
        let defaults_source = fs::read_to_string(&defaults_path)
            .map_err(|e| format!("cannot read {}: {e}", defaults_path.display()))?;
        let defaults: ManifestDefaults = serde_yaml::from_str(&defaults_source)
            .map_err(|e| format!("{}: invalid YAML: {e}", defaults_path.display()))?;
        if defaults.schema != MANIFEST_SCHEMA {
            return Err(format!(
                "{}: schema must be {MANIFEST_SCHEMA}",
                defaults_path.display()
            ));
        }
        let global_timeout_seconds =
            validate_timeout_seconds(defaults.timeout_seconds, "global default")?;
        let global_cpu_timeout_seconds =
            validate_timeout_seconds(defaults.cpu_timeout_seconds, "global CPU default")?;
        let mut nextest_filters = BTreeSet::new();
        for timeout in &defaults.nextest {
            if timeout.filter.trim().is_empty() {
                return Err("nextest timeout filter must not be empty".into());
            }
            validate_timeout_seconds(timeout.timeout_seconds, &timeout.filter)?;
            if timeout.timeout_seconds == global_timeout_seconds {
                return Err(format!(
                    "nextest timeout {} redundantly repeats the global default",
                    timeout.filter
                ));
            }
            if timeout.slow_reason.trim().is_empty() {
                return Err(format!(
                    "nextest timeout {} requires a non-empty slow_reason",
                    timeout.filter
                ));
            }
            if !nextest_filters.insert(timeout.filter.as_str()) {
                return Err(format!(
                    "duplicate nextest timeout filter: {}",
                    timeout.filter
                ));
            }
        }
        let mut paths = fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "yaml")
                    && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
            })
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
            validate_document_with_cpu(
                &document,
                stem,
                root,
                global_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
            let bucket_timeout_seconds =
                resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
            for test in &document.test {
                if tests
                    .insert(
                        test.id.clone(),
                        (
                            document.bucket.clone(),
                            bucket_timeout_seconds,
                            global_cpu_timeout_seconds,
                            test.clone(),
                        ),
                    )
                    .is_some()
                {
                    return Err(format!("duplicate test id: {}", test.id));
                }
            }
            documents.push(document);
        }
        Ok(Self { documents, tests })
    }

    /// Is `id` a test this manifest set has ever heard of?
    ///
    /// ⚠️ THIS EXISTS TO SEPARATE "NO CELLS" FROM "NO SUCH TEST". `select` returns an
    /// empty vector for both, and they mean opposite things: an unfiltered population
    /// that is legitimately empty ("no gaps") versus a filter naming something that
    /// does not exist (a typo). A caller that cannot tell them apart must either
    /// refuse a good answer or accept a meaningless one.
    pub fn knows_test(&self, id: &str) -> bool {
        self.tests.contains_key(id)
    }

    pub fn select(&self, selection: &Selection) -> Result<Vec<SelectedCell>, String> {
        let population = selection.population.unwrap_or(Population::Enabled);
        let mut cells = Vec::new();
        for (id, (category, bucket_timeout_seconds, default_cpu_timeout_seconds, test)) in
            &self.tests
        {
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
                        timeout_seconds: resolve_timeout_seconds(
                            *bucket_timeout_seconds,
                            None,
                            recipe.timeout_seconds.get("native").copied(),
                        ),
                        cpu_timeout_seconds: recipe
                            .cpu_timeout_seconds
                            .get("native")
                            .copied()
                            .unwrap_or(*default_cpu_timeout_seconds),
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
                    let timeout_seconds = resolve_timeout_seconds(
                        *bucket_timeout_seconds,
                        None,
                        recipe.timeout_seconds.get(&backend).copied(),
                    );
                    let cpu_timeout_seconds = recipe
                        .cpu_timeout_seconds
                        .get(&backend)
                        .copied()
                        .unwrap_or(*default_cpu_timeout_seconds);
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: Some(backend),
                        },
                        enabled,
                        timeout_seconds,
                        cpu_timeout_seconds,
                    });
                }
            }
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(cells)
    }

    pub fn all_tests(&self) -> impl Iterator<Item = (&str, u64, u64, &TestRecipe)> {
        self.tests
            .values()
            .map(|(category, timeout_seconds, cpu_timeout_seconds, test)| {
                (
                    category.as_str(),
                    *timeout_seconds,
                    *cpu_timeout_seconds,
                    test,
                )
            })
    }
}

fn population_accepts(population: Population, ci: bool, enabled: bool) -> bool {
    match population {
        Population::Required => ci && enabled,
        Population::Enabled => enabled,
        Population::Disabled => !enabled,
    }
}

#[cfg(test)]
fn validate_document(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
) -> Result<(), String> {
    validate_document_with_cpu(
        document,
        stem,
        root,
        global_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_document_with_cpu(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
    global_cpu_timeout_seconds: u64,
) -> Result<(), String> {
    if document.schema != MANIFEST_SCHEMA {
        return Err(format!("{stem}: schema must be {MANIFEST_SCHEMA}"));
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
    match (document.timeout_seconds, document.slow_reason.as_deref()) {
        (Some(timeout), Some(reason)) if !reason.trim().is_empty() => {
            validate_timeout_seconds(timeout, &format!("{stem} bucket"))?;
            if timeout == global_timeout_seconds {
                return Err(format!(
                    "{stem}: bucket timeout_seconds redundantly repeats the global default"
                ));
            }
        }
        (Some(_), _) => {
            return Err(format!(
                "{stem}: bucket timeout_seconds requires a non-empty slow_reason"
            ));
        }
        (None, Some(_)) => {
            return Err(format!("{stem}: bucket slow_reason has no timeout_seconds"));
        }
        (None, None) => {}
    }
    let bucket_timeout_seconds =
        resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
    for test in &document.test {
        if !test.id.starts_with(&format!("{stem}/")) {
            return Err(format!("{}: id must start with {stem}/", test.id));
        }
        if !matches!(test.lane.as_str(), "portable" | "privileged") {
            return Err(format!("{}: invalid lane `{}`", test.id, test.lane));
        }
        for token in &test.requires {
            requires_capability(token).map_err(|error| format!("{}.requires: {error}", test.id))?;
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
            validate_mode_with_cpu(
                &test.id,
                mode,
                recipe,
                bucket_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_mode(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
) -> Result<(), String> {
    validate_mode_with_cpu(
        id,
        mode,
        recipe,
        bucket_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_mode_with_cpu(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
    default_cpu_timeout_seconds: u64,
) -> Result<(), String> {
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
    let wall_timeout_keys: BTreeSet<_> =
        recipe.timeout_seconds.keys().map(String::as_str).collect();
    let cpu_timeout_keys: BTreeSet<_> = recipe
        .cpu_timeout_seconds
        .keys()
        .map(String::as_str)
        .collect();
    let reason_keys: BTreeSet<_> = recipe.slow_reason.keys().map(String::as_str).collect();
    if wall_timeout_keys != cpu_timeout_keys || wall_timeout_keys != reason_keys {
        return Err(format!(
            "{id}: {mode} timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        ));
    }
    for (backend, timeout) in &recipe.timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend}"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for (backend, timeout) in &recipe.cpu_timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} cpu_timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend} CPU"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} cpu_timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for backend in &wall_timeout_keys {
        if recipe.timeout_seconds[*backend] == bucket_timeout_seconds
            && recipe.cpu_timeout_seconds[*backend] == default_cpu_timeout_seconds
        {
            return Err(format!(
                "{id}: {mode}/{backend} timeout pair redundantly repeats its inherited values"
            ));
        }
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
    match (
        mode,
        recipe.compare_io_buffers,
        recipe.compare_io_buffers_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify compare_io_buffers=false requires compare_io_buffers_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify comparison reason is stale while I/O-buffer comparison is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!(
                "{id}: compare_io_buffers is supported only by verify mode"
            ));
        }
    }
    if recipe
        .compare_io_buffers_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: compare_io_buffers_disabled_reason must be substantive"
        ));
    }
    match (
        mode,
        recipe.rcb_time,
        recipe.rcb_time_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify rcb_time=false requires rcb_time_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify RCB-time reason is stale while RCB time is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!("{id}: rcb_time is supported only by verify mode"));
        }
    }
    if recipe
        .rcb_time_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: rcb_time_disabled_reason must be substantive"
        ));
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
    // The pinned DBT launcher preserves Command::current_dir, but it does not
    // enter Hermit's container and therefore cannot see a workdir supplied by
    // Hermit's mount namespace. Refuse until the requested path is established
    // in the namespace the DBT guest actually uses.
    if backends_enabled.iter().any(|backend| backend == "dbt") {
        return Err(format!(
            "{id}: {mode} workdir is unsupported when DBT is enabled because DBT does not enter the Hermit mount namespace"
        ));
    }
    Ok(())
}

fn supports_test_workdir(mode: &str, backend: &str) -> bool {
    matches!(mode, "verify" | "replay" | "chaos" | "custom") && backend != "dbt"
}

fn cell_relaxations(cell: &SelectedCell) -> Vec<String> {
    let recipe = &cell.test.modes[&cell.id.mode];
    let mut relaxations = Vec::new();
    if recipe.compare_io_buffers == Some(false) {
        relaxations.push(format!(
            "--no-detlog-io-buffers: {}",
            recipe
                .compare_io_buffers_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    if recipe.rcb_time == Some(false) {
        relaxations.push(format!(
            "--no-rcb-time: {}",
            recipe
                .rcb_time_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    relaxations
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
    #[serde(skip)]
    attempt: String,
    #[serde(skip)]
    fixed_workdir_source: PathBuf,
}

/// One side of a harness-managed verify pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyRun {
    Run1,
    Run2,
}

impl VerifyRun {
    fn directory_name(self) -> &'static str {
        match self {
            Self::Run1 => "run-1",
            Self::Run2 => "run-2",
        }
    }

    fn comparison_label(self) -> &'static str {
        match self {
            Self::Run1 => "run 1",
            Self::Run2 => "run 2",
        }
    }
}

/// Harness-owned paths for one ordinary execution in a verify pair.
///
/// Every path is below the cell artifact directory. The work directory is
/// separate for each side and is mounted at the same guest-visible `/test`
/// path when the pair is eventually enabled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifyRunPaths {
    pub workdir: PathBuf,
    pub log: PathBuf,
    pub result: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub summary: PathBuf,
}

/// One ordinary Hermit invocation that supplies one side of a verify pair.
#[derive(Clone, Debug)]
pub struct VerifyRunSpec {
    pub run: VerifyRun,
    pub execution: CellRunSpec,
    pub paths: VerifyRunPaths,
    attempt: u64,
    expected_determinism: GuestRunDeterminism,
}

impl VerifyRunSpec {
    pub fn expected_determinism(&self) -> GuestRunDeterminism {
        self.expected_determinism
    }

    fn validate_policy_binding(&self) -> Result<(), String> {
        let flag_count = |flag: &str| {
            self.execution
                .argv
                .iter()
                .filter(|argument| argument.as_str() == flag)
                .count()
        };
        let expected_no_io = usize::from(!self.expected_determinism.detlog_io_buffers);
        if flag_count("--no-detlog-io-buffers") != expected_no_io {
            return Err(format!(
                "verify run command does not match its detlog_io_buffers setting: expected {expected_no_io} --no-detlog-io-buffers flag(s)"
            ));
        }
        let expected_no_virtual_time = usize::from(!self.expected_determinism.virtualize_time);
        if flag_count("--no-virtualize-time") != expected_no_virtual_time {
            return Err(format!(
                "verify run command does not match its virtualize_time setting: expected {expected_no_virtual_time} --no-virtualize-time flag(s)"
            ));
        }
        if self.execution.argv.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--verify" | "--verify-json" | "--verify-strict" | "--keep-logs"
            ) || argument.starts_with("--verify-log-dir")
        }) {
            return Err(
                "harness-managed verify run command contains an internal verify flag".into(),
            );
        }
        Ok(())
    }
}

/// The single member of a verify pair retained after comparison.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedVerifyLogRole {
    #[serde(rename = "run-1")]
    Run1,
}

/// Durable evidence for the one compressed verify log retained by an attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLog {
    /// Path relative to the cell's `artifact_dir`.
    pub relative_path: String,
    pub role: RetainedVerifyLogRole,
    pub cell_id: CellId,
    pub attempt: u64,
    pub uncompressed_sha256: String,
    pub uncompressed_bytes: u64,
    pub compressed_sha256: String,
    pub compressed_bytes: u64,
    pub peer_uncompressed_sha256: String,
    pub peer_uncompressed_bytes: u64,
    pub compared_info_messages: u64,
}

/// A verified compressed log plus the two raw inputs that may be removed only
/// after its descriptor has been durably published with the cell result.
#[derive(Clone, Debug)]
pub struct RetainedVerifyLogPublication {
    pub retained: RetainedVerifyLog,
    run1_raw: PathBuf,
    run2_raw: PathBuf,
    run1_digest: ContentDigest,
    run2_digest: ContentDigest,
}

/// Storage policy shared by every retained verify log in one validation run.
///
/// The aggregate limit is explicit rather than inferred from free space: a
/// caller must choose it once at the scheduling boundary and share the
/// resulting [`VerifyLogRetentionBudget`] across all pair publications.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyLogRetentionPolicy {
    pub maximum_total_compressed_bytes: u64,
}

impl VerifyLogRetentionPolicy {
    pub const fn new(maximum_total_compressed_bytes: u64) -> Self {
        Self {
            maximum_total_compressed_bytes,
        }
    }
}

#[derive(Debug)]
struct VerifyLogRetentionState {
    accounted_compressed_bytes: u64,
}

/// Synchronized aggregate accounting for retained verify logs.
///
/// Clones share one counter. Each compressed write is provisionally charged
/// before touching the staging file and releases any unwritten bytes, so
/// staging may proceed concurrently without allowing physical bytes to run
/// ahead of the aggregate limit.
#[derive(Clone, Debug)]
pub struct VerifyLogRetentionBudget {
    policy: VerifyLogRetentionPolicy,
    retention_root: PathBuf,
    results_path: PathBuf,
    state: Arc<Mutex<VerifyLogRetentionState>>,
    result_publication: Arc<Mutex<()>>,
}

impl VerifyLogRetentionBudget {
    /// Reconstruct aggregate accounting from the canonical finals referenced
    /// by one prepared result file. Unreferenced finals and interrupted
    /// staging files make restart state inconsistent and are refused without
    /// deleting them.
    pub fn open(
        retention_root: impl Into<PathBuf>,
        results_path: impl Into<PathBuf>,
        policy: VerifyLogRetentionPolicy,
    ) -> Result<Self, String> {
        let retention_root = retention_root.into();
        let results_path = results_path.into();
        require_plain_directory(&retention_root, "verify-log retention root")?;
        let results = check_file_path_below(
            &retention_root,
            &results_path,
            "retained verify-log result-row destination",
        )?;
        if results.identity.is_none() {
            return Err(format!(
                "retained verify-log result-row destination {} must be prepared before opening the budget",
                results_path.display()
            ));
        }
        let accounted_compressed_bytes =
            scan_existing_verify_log_bytes(&retention_root, &results_path, policy)?;
        Ok(Self {
            policy,
            retention_root,
            results_path,
            state: Arc::new(Mutex::new(VerifyLogRetentionState {
                accounted_compressed_bytes,
            })),
            result_publication: Arc::new(Mutex::new(())),
        })
    }

    pub fn policy(&self) -> VerifyLogRetentionPolicy {
        self.policy
    }

    pub fn accounted_compressed_bytes(&self) -> Result<u64, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?
            .accounted_compressed_bytes)
    }

    fn reserve_additional(&self, compressed_bytes: u64) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?;
        let next = state
            .accounted_compressed_bytes
            .checked_add(compressed_bytes)
            .ok_or_else(|| {
                format!(
                    "retained verify-log aggregate compressed-byte accounting overflow: {} + {compressed_bytes}",
                    state.accounted_compressed_bytes
                )
            })?;
        if next > self.policy.maximum_total_compressed_bytes {
            return Err(format!(
                "retained verify logs require {next} compressed bytes, exceeding the {}-byte aggregate limit",
                self.policy.maximum_total_compressed_bytes
            ));
        }
        state.accounted_compressed_bytes = next;
        Ok(())
    }

    fn release(&self, compressed_bytes: u64) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?;
        state.accounted_compressed_bytes = state
            .accounted_compressed_bytes
            .checked_sub(compressed_bytes)
            .ok_or_else(|| {
                "retained verify-log provisional accounting underflowed during rollback".to_string()
            })?;
        Ok(())
    }

    fn require_artifact_dir(&self, artifact_dir: &Path) -> Result<(), String> {
        let relative = checked_relative_path(
            &self.retention_root,
            artifact_dir,
            "verify cell artifact directory",
        )?;
        if relative.components().count() != 1 {
            return Err(format!(
                "verify cell artifact directory {} is not an immediate child of retention root {}",
                artifact_dir.display(),
                self.retention_root.display()
            ));
        }
        require_plain_directory(artifact_dir, "verify cell artifact directory")
    }

    fn require_results_path(&self, results_path: &Path) -> Result<(), String> {
        let expected = check_file_path_below(
            &self.retention_root,
            &self.results_path,
            "configured result-row destination",
        )?;
        let actual =
            check_file_path_below(&self.retention_root, results_path, "result-row destination")?;
        if expected.normalized != actual.normalized {
            return Err(format!(
                "result-row destination {} does not match configured path {}",
                results_path.display(),
                self.results_path.display()
            ));
        }
        Ok(())
    }
}

struct VerifyLogRetentionReservation {
    budget: VerifyLogRetentionBudget,
    compressed_bytes: u64,
    resolved: bool,
}

impl VerifyLogRetentionReservation {
    fn commit(mut self) {
        self.resolved = true;
    }

    fn rollback(mut self) -> Result<(), String> {
        self.budget.release(self.compressed_bytes)?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for VerifyLogRetentionReservation {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        let _ = self.budget.release(self.compressed_bytes);
    }
}

const VERIFY_LOG_MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_LOG_MAX_COMPRESSED_BYTES: u64 = VERIFY_LOG_MAX_UNCOMPRESSED_BYTES + 16 * 1024 * 1024;
const VERIFY_LOG_STAGING_PREFIX: &str = ".run-1.log.gz.";
const VERIFY_LOG_STAGING_SUFFIX: &str = ".staging";
const VERIFY_RUN_RESULT_MAX_BYTES: u64 = 1024 * 1024;
const VERIFY_RUN_STDOUT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_RUN_STDERR_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_RUN_SUMMARY_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct VerifyRunReadLimits {
    result: u64,
    stdout: u64,
    stderr: u64,
    summary: u64,
}

const VERIFY_RUN_READ_LIMITS: VerifyRunReadLimits = VerifyRunReadLimits {
    result: VERIFY_RUN_RESULT_MAX_BYTES,
    stdout: VERIFY_RUN_STDOUT_MAX_BYTES,
    stderr: VERIFY_RUN_STDERR_MAX_BYTES,
    summary: VERIFY_RUN_SUMMARY_MAX_BYTES,
};

pub fn verify_run_paths(
    cell_dir: &Path,
    attempt: &str,
    run: VerifyRun,
) -> Result<VerifyRunPaths, String> {
    let workdir = fixed_workdir_source_for_attempt(cell_dir, attempt)?.join(run.directory_name());
    let capture_dir = cell_dir
        .join("captures")
        .join("verify")
        .join(attempt)
        .join(run.directory_name());
    Ok(VerifyRunPaths {
        workdir,
        log: capture_dir.join("detlog.log"),
        result: capture_dir.join("result.json"),
        stdout: capture_dir.join("stdout"),
        stderr: capture_dir.join("stderr"),
        summary: capture_dir.join("summary.json"),
    })
}

/// Fully checked inputs for one side of a harness-managed comparison.
#[derive(Clone, Debug)]
pub struct VerifyRunObservation {
    pub result: GuestRunResult,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub runtime: RuntimeStats,
    log_bytes: Vec<u8>,
    log_digest: ContentDigest,
    log_identity: FileIdentity,
}

impl VerifyRunObservation {
    fn as_logdiff_input(&self) -> BitwiseInfoV1RunObservation<'_> {
        BitwiseInfoV1RunObservation {
            disposition: self.result.disposition,
            stdout: &self.stdout,
            stderr: &self.stderr,
            runtime: Some(&self.runtime),
        }
    }
}

fn validate_captured_guest_stream(
    name: &str,
    expected: &crate::canonical_verdict::CapturedGuestStream,
    actual: &ContentDigest,
) -> Result<(), String> {
    if expected.bytes != actual.bytes || expected.sha256 != actual.sha256 {
        return Err(format!(
            "captured guest {name} does not match its typed result: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            expected.bytes, expected.sha256, actual.bytes, actual.sha256
        ));
    }
    Ok(())
}

/// Read one ordinary-run sidecar and bind it to the exact captured bytes.
pub fn load_verify_run(spec: &VerifyRunSpec) -> Result<VerifyRunObservation, String> {
    load_verify_run_with_limits(spec, VERIFY_RUN_READ_LIMITS)
}

fn read_verify_run_artifact(
    spec: &VerifyRunSpec,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<(Vec<u8>, ContentDigest), String> {
    let mut file = open_plain_file_below(&spec.execution.cell_dir, path, description)?;
    let mut bytes = Vec::new();
    let digest = copy_and_hash_bounded(&mut file, &mut bytes, maximum_bytes, description)?;
    Ok((bytes, digest))
}

fn load_verify_run_with_limits(
    spec: &VerifyRunSpec,
    limits: VerifyRunReadLimits,
) -> Result<VerifyRunObservation, String> {
    spec.validate_policy_binding()?;
    let paths = &spec.paths;
    let (result_bytes, _) = read_verify_run_artifact(
        spec,
        &paths.result,
        limits.result,
        "verify ordinary-run result",
    )?;
    let result = GuestRunResult::from_current_json_slice(&result_bytes)?;
    if result.determinism != spec.expected_determinism {
        return Err(format!(
            "guest run result determinism settings do not match the command: expected {:?}, got {:?}",
            spec.expected_determinism, result.determinism
        ));
    }
    let (stdout, stdout_digest) = read_verify_run_artifact(
        spec,
        &paths.stdout,
        limits.stdout,
        "verify ordinary-run stdout",
    )?;
    let (stderr, stderr_digest) = read_verify_run_artifact(
        spec,
        &paths.stderr,
        limits.stderr,
        "verify ordinary-run stderr",
    )?;
    validate_captured_guest_stream("stdout", &result.stdout, &stdout_digest)?;
    validate_captured_guest_stream("stderr", &result.stderr, &stderr_digest)?;
    let (summary_bytes, _) = read_verify_run_artifact(
        spec,
        &paths.summary,
        limits.summary,
        "verify ordinary-run summary",
    )?;
    let summary = serde_json::from_slice::<RunSummary>(&summary_bytes)
        .map_err(|error| format!("cannot parse {}: {error}", paths.summary.display()))?;
    let runtime = RuntimeStats::from(&summary);
    let mut log_file = open_plain_file_below(
        &spec.execution.cell_dir,
        &paths.log,
        "verify ordinary-run log",
    )?;
    let log_metadata = log_file
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", paths.log.display()))?;
    let mut log_bytes = Vec::new();
    let log_digest = copy_and_hash_bounded(
        &mut log_file,
        &mut log_bytes,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        "verify ordinary-run log",
    )?;
    Ok(VerifyRunObservation {
        result,
        stdout,
        stderr,
        runtime,
        log_bytes,
        log_digest,
        log_identity: FileIdentity::from_metadata(&log_metadata),
    })
}

/// One comparison whose exact input bytes and policy remain available for
/// retention. Callers cannot supply record counts or digests independently.
#[derive(Clone, Debug)]
pub struct ComparedVerifyPair {
    comparison: BitwiseInfoV1Comparison,
    artifact_dir: PathBuf,
    cell_id: CellId,
    attempt: u64,
    run1_log: PathBuf,
    run2_log: PathBuf,
    run1_log_bytes: Vec<u8>,
    run1_log_digest: ContentDigest,
    run1_log_identity: FileIdentity,
    run2_log_digest: ContentDigest,
    run2_log_identity: FileIdentity,
    fixed_result_destinations: Vec<(String, PathBuf)>,
}

impl ComparedVerifyPair {
    pub fn comparison(&self) -> &BitwiseInfoV1Comparison {
        &self.comparison
    }
}

/// Compare two checked ordinary runs using Detcore's fixed canonical policy.
pub fn compare_verify_runs(
    run1_spec: &VerifyRunSpec,
    run1: VerifyRunObservation,
    run2_spec: &VerifyRunSpec,
    run2: VerifyRunObservation,
) -> Result<ComparedVerifyPair, String> {
    run1_spec.validate_policy_binding()?;
    run2_spec.validate_policy_binding()?;
    if run1_spec.run != VerifyRun::Run1 || run2_spec.run != VerifyRun::Run2 {
        return Err("verify comparison requires run 1 on the left and run 2 on the right".into());
    }
    if run1_spec.execution.id != run2_spec.execution.id
        || run1_spec.execution.cell_dir != run2_spec.execution.cell_dir
        || run1_spec.attempt != run2_spec.attempt
    {
        return Err("verify comparison inputs do not belong to the same cell attempt".into());
    }
    if run1.result.determinism != run2.result.determinism
        || run1.result.determinism != run1_spec.expected_determinism
        || run2.result.determinism != run2_spec.expected_determinism
    {
        return Err("verify comparison inputs do not carry one matching run policy".into());
    }
    if run1.log_identity == run2.log_identity {
        return Err("verify run-1 and run-2 observations came from the same file".into());
    }
    let comparison = detcore::logdiff::try_compare_bitwise_info_v1_run_bytes(
        &run1.log_bytes,
        &run2.log_bytes,
        ComparisonSideLabels::new(
            VerifyRun::Run1.comparison_label(),
            VerifyRun::Run2.comparison_label(),
        ),
        run1.as_logdiff_input(),
        run2.as_logdiff_input(),
        run1.result.determinism.detlog_io_buffers,
        run1.result.determinism.virtualize_time,
    )
    .map_err(|error| format!("canonical verify log comparison failed: {error}"))?;
    Ok(ComparedVerifyPair {
        comparison,
        artifact_dir: run1_spec.execution.cell_dir.clone(),
        cell_id: run1_spec.execution.id.clone(),
        attempt: run1_spec.attempt,
        run1_log: run1_spec.paths.log.clone(),
        run2_log: run2_spec.paths.log.clone(),
        run1_log_bytes: run1.log_bytes,
        run1_log_digest: run1.log_digest,
        run1_log_identity: run1.log_identity,
        run2_log_digest: run2.log_digest,
        run2_log_identity: run2.log_identity,
        fixed_result_destinations: [
            ("run-1 result", &run1_spec.paths.result),
            ("run-1 stdout", &run1_spec.paths.stdout),
            ("run-1 stderr", &run1_spec.paths.stderr),
            ("run-1 summary", &run1_spec.paths.summary),
            ("run-2 result", &run2_spec.paths.result),
            ("run-2 stdout", &run2_spec.paths.stdout),
            ("run-2 stderr", &run2_spec.paths.stderr),
            ("run-2 summary", &run2_spec.paths.summary),
        ]
        .into_iter()
        .map(|(description, path)| (description.to_string(), path.clone()))
        .collect(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContentDigest {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InspectedGzip {
    identity: FileIdentity,
    compressed: ContentDigest,
    uncompressed: ContentDigest,
}

struct OpenedGzipEvidence {
    file: File,
    inspection: InspectedGzip,
}

fn checked_relative_path<'a>(
    artifact_dir: &'a Path,
    path: &'a Path,
    description: &str,
) -> Result<&'a Path, String> {
    let relative = path.strip_prefix(artifact_dir).map_err(|_| {
        format!(
            "{description} {} is outside cell artifact directory {}",
            path.display(),
            artifact_dir.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} {} is not a normal relative path below {}",
            path.display(),
            artifact_dir.display()
        ));
    }
    Ok(relative)
}

#[derive(Clone, Debug)]
struct CheckedFilePath {
    description: String,
    path: PathBuf,
    normalized: PathBuf,
    identity: Option<FileIdentity>,
}

fn check_file_path_below(
    stable_root: &Path,
    path: &Path,
    description: impl Into<String>,
) -> Result<CheckedFilePath, String> {
    let description = description.into();
    let relative = checked_relative_path(stable_root, path, &description)?;
    let canonical_root = fs::canonicalize(stable_root).map_err(|error| {
        format!(
            "cannot canonicalize stable root {} for {description}: {error}",
            stable_root.display()
        )
    })?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = stable_root.to_owned();
    let mut identity = None;
    let mut missing_parent = false;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!("checked_relative_path accepted only normal components")
        };
        current.push(component);
        if missing_parent {
            continue;
        }
        let is_file = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "{description} {} or one of its parents is a symlink",
                    current.display()
                ));
            }
            Ok(metadata) if is_file && !metadata.is_file() => {
                return Err(format!(
                    "{description} {} is not a regular file",
                    current.display()
                ));
            }
            Ok(metadata) if is_file => {
                identity = Some(FileIdentity::from_metadata(&metadata));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "parent of {description} {} is not a directory",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_parent = true;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} path {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(CheckedFilePath {
        description,
        path: path.to_owned(),
        normalized: canonical_root.join(relative),
        identity,
    })
}

fn reject_file_path_aliases(paths: &[CheckedFilePath]) -> Result<(), String> {
    for (index, left) in paths.iter().enumerate() {
        for right in &paths[index + 1..] {
            if left.normalized == right.normalized
                || left
                    .identity
                    .zip(right.identity)
                    .is_some_and(|(left, right)| left == right)
            {
                return Err(format!(
                    "{} {} aliases {} {}",
                    left.description,
                    left.path.display(),
                    right.description,
                    right.path.display()
                ));
            }
        }
    }
    Ok(())
}

fn require_path_identity(
    path: &Path,
    expected: FileIdentity,
    description: &str,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot recheck {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{description} {} is no longer a regular non-symlink file",
            path.display()
        ));
    }
    let actual = FileIdentity::from_metadata(&metadata);
    if actual != expected {
        return Err(format!(
            "{description} {} changed identity before publication",
            path.display()
        ));
    }
    Ok(())
}

fn require_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} is not a non-symlink directory",
            path.display()
        ));
    }
    Ok(())
}

fn require_plain_parent_chain(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    let relative = checked_relative_path(artifact_dir, path, description)?;
    require_plain_directory(artifact_dir, "cell artifact directory")?;
    let mut current = artifact_dir.to_owned();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(component) = component else {
                unreachable!("checked_relative_path accepted only normal components")
            };
            current.push(component);
            require_plain_directory(&current, description)?;
        }
    }
    Ok(())
}

fn create_plain_relative_directory(
    artifact_dir: &Path,
    relative: &Path,
    description: &str,
) -> Result<PathBuf, String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{description} is not a normal relative path"));
    }
    require_plain_directory(artifact_dir, "cell artifact directory")?;
    let mut current = artifact_dir.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            unreachable!("relative path was checked above")
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "{description} {} is not a non-symlink directory",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| {
                    format!("cannot create {description} {}: {error}", current.display())
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

fn sync_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("cannot sync {description} {}: {error}", path.display()))
}

fn sync_relative_directory_chain_with_failure(
    root: &Path,
    final_directory: &Path,
    description: &str,
    fail_at: Option<usize>,
) -> Result<(), String> {
    let relative = final_directory.strip_prefix(root).map_err(|_| {
        format!(
            "{description} {} is outside stable root {}",
            final_directory.display(),
            root.display()
        )
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} {} is not a normal path below stable root {}",
            final_directory.display(),
            root.display()
        ));
    }
    if fail_at == Some(0) {
        return Err(format!(
            "injected failure syncing {description} ancestor {}",
            root.display()
        ));
    }
    sync_plain_directory(root, description)?;
    let mut current = root.to_owned();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!("checked_relative_path accepted only normal components")
        };
        current.push(component);
        require_plain_directory(&current, description)?;
        if fail_at == Some(index + 1) {
            return Err(format!(
                "injected failure syncing {description} ancestor {}",
                current.display()
            ));
        }
        sync_plain_directory(&current, description)?;
    }
    Ok(())
}

fn remove_empty_retained_verify_directories(
    retention_root: &Path,
    retained_directory: &Path,
) -> Result<(), String> {
    let relative = checked_relative_path(
        retention_root,
        retained_directory,
        "retained verify-log rollback directory",
    )?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 4
        || components[1].as_os_str() != OsStr::new("retained")
        || components[2].as_os_str() != OsStr::new("verify")
    {
        return Err(format!(
            "retained verify-log rollback directory {} does not have the expected artifact/retained/verify/attempt layout below {}",
            retained_directory.display(),
            retention_root.display()
        ));
    }
    let artifact_dir = retention_root.join(components[0].as_os_str());
    let verify_dir = retained_directory
        .parent()
        .expect("validated retained verify-log attempt directory has a parent");
    let retained_dir = verify_dir
        .parent()
        .expect("validated retained verify-log verify directory has a parent");
    for (index, directory) in [retained_directory, verify_dir, retained_dir]
        .into_iter()
        .enumerate()
    {
        match fs::remove_dir(directory) {
            Ok(()) => {
                let parent = directory
                    .parent()
                    .expect("a retained verify-log rollback directory has a parent");
                sync_plain_directory(parent, "retained verify-log rollback parent")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty && index == 1 => {
                break;
            }
            Err(error) => {
                return Err(format!(
                    "cannot remove empty retained verify-log rollback directory {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    sync_plain_directory(retention_root, "retained verify-log rollback stable root")?;
    for directory in [&artifact_dir, retained_dir, verify_dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
                sync_plain_directory(directory, "retained verify-log rollback ancestor")?;
            }
            Ok(_) => {
                return Err(format!(
                    "retained verify-log rollback ancestor {} is not a non-symlink directory",
                    directory.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "cannot inspect retained verify-log rollback ancestor {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    Ok(())
}

fn open_plain_file_below(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<File, String> {
    require_plain_parent_chain(artifact_dir, path, description)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} {} is not a regular file",
            path.display()
        ));
    }
    Ok(file)
}

fn copy_and_hash_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum_bytes: u64,
    description: &str,
) -> Result<ContentDigest, String> {
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {description}: {error}"))?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(read).expect("read buffer length fits u64"))
            .ok_or_else(|| format!("{description} size overflowed u64"))?;
        if bytes > maximum_bytes {
            return Err(format!(
                "{description} exceeds the {maximum_bytes}-byte limit"
            ));
        }
        digest.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|error| format!("cannot write {description}: {error}"))?;
    }
    Ok(ContentDigest {
        sha256: format!("{:x}", digest.finalize()),
        bytes,
    })
}

struct OpenedPlainFileEvidence {
    _file: File,
    identity: FileIdentity,
    digest: ContentDigest,
}

fn open_and_inspect_plain_file_bounded(
    root: &Path,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<OpenedPlainFileEvidence, String> {
    let mut file = open_plain_file_below(root, path, description)?;
    let identity =
        FileIdentity::from_metadata(&file.metadata().map_err(|error| {
            format!("cannot inspect {description} {}: {error}", path.display())
        })?);
    let digest =
        copy_and_hash_bounded(&mut file, &mut std::io::sink(), maximum_bytes, description)?;
    require_path_identity(path, identity, description)?;
    Ok(OpenedPlainFileEvidence {
        _file: file,
        identity,
        digest,
    })
}

fn inspect_plain_file_bounded(
    root: &Path,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<(FileIdentity, ContentDigest), String> {
    let evidence = open_and_inspect_plain_file_bounded(root, path, maximum_bytes, description)?;
    Ok((evidence.identity, evidence.digest))
}

fn inspect_gzip_file(
    root: &Path,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<InspectedGzip, String> {
    Ok(open_and_inspect_gzip_file(
        root,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?
    .inspection)
}

fn open_and_inspect_gzip_file(
    root: &Path,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<OpenedGzipEvidence, String> {
    let mut file = open_plain_file_below(root, path, description)?;
    let inspection = inspect_open_gzip_file(
        &mut file,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?;
    Ok(OpenedGzipEvidence { file, inspection })
}

fn inspect_open_gzip_file(
    file: &mut File,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<InspectedGzip, String> {
    let identity =
        FileIdentity::from_metadata(&file.metadata().map_err(|error| {
            format!("cannot inspect {description} {}: {error}", path.display())
        })?);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let compressed = copy_and_hash_bounded(
        file,
        &mut std::io::sink(),
        maximum_compressed_bytes,
        description,
    )?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).map_err(|error| {
        format!(
            "cannot read deterministic gzip header from {description} {}: {error}",
            path.display()
        )
    })?;
    if header[..3] != [0x1f, 0x8b, 8] || header[3] != 0 || header[4..8] != [0, 0, 0, 0] {
        return Err(format!(
            "{description} {} does not have the canonical deterministic gzip header",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let uncompressed = {
        let mut decoder = MultiGzDecoder::new(file);
        copy_and_hash_bounded(
            &mut decoder,
            &mut std::io::sink(),
            maximum_uncompressed_bytes,
            description,
        )?
    };
    Ok(InspectedGzip {
        identity,
        compressed,
        uncompressed,
    })
}

fn retained_verify_log_relative_path(attempt: u64) -> Result<PathBuf, String> {
    if attempt == 0 {
        return Err("retained verify-log attempt must be positive".into());
    }
    Ok(PathBuf::from("retained")
        .join("verify")
        .join(attempt.to_string())
        .join("run-1.log.gz"))
}

fn is_verify_log_staging_name(name: &str) -> bool {
    name.starts_with(VERIFY_LOG_STAGING_PREFIX) && name.ends_with(VERIFY_LOG_STAGING_SUFFIX)
}

fn scan_existing_verify_log_bytes(
    retention_root: &Path,
    results_path: &Path,
    policy: VerifyLogRetentionPolicy,
) -> Result<u64, String> {
    scan_existing_verify_log_bytes_with_hook(retention_root, results_path, policy, None)
}

#[derive(Debug)]
struct ScannedRetainedVerifyLog {
    path: PathBuf,
    retained: RetainedVerifyLog,
    inspection: InspectedGzip,
}

fn scan_existing_verify_log_bytes_with_hook(
    retention_root: &Path,
    results_path: &Path,
    policy: VerifyLogRetentionPolicy,
    before_final_recheck: Option<&dyn Fn(&Path)>,
) -> Result<u64, String> {
    let results_bytes = read_existing_result(results_path)?;
    let mut descriptors = BTreeMap::<(PathBuf, u64), RetainedVerifyLog>::new();
    if let Some(results_bytes) = results_bytes {
        for (line_index, line) in results_bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let result = serde_json::from_slice::<CellResult>(line).map_err(|error| {
                format!(
                    "cannot parse result row {} in {} while reconciling retained verify logs: {error}",
                    line_index + 1,
                    results_path.display()
                )
            })?;
            let Some(retained) = result.validate_retained_verify_log_binding()? else {
                continue;
            };
            let artifact_dir = PathBuf::from(&result.artifact_dir);
            checked_relative_path(
                retention_root,
                &artifact_dir,
                "retained verify-log artifact directory from result row",
            )?;
            let key = (artifact_dir, retained.attempt);
            if descriptors.insert(key.clone(), retained.clone()).is_some() {
                return Err(format!(
                    "duplicate retained verify-log descriptors for {} attempt {}",
                    key.0.display(),
                    key.1
                ));
            }
        }
    }

    let configured_result = check_file_path_below(
        retention_root,
        results_path,
        "configured result-row destination",
    )?;
    let mut artifact_dirs = Vec::new();
    for entry in fs::read_dir(retention_root).map_err(|error| {
        format!(
            "cannot read verify-log retention root {}: {error}",
            retention_root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "cannot read entry below verify-log retention root {}: {error}",
                retention_root.display()
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            format!(
                "cannot inspect verify-log retention-root entry {}: {error}",
                entry.path().display()
            )
        })?;
        let entry_path = entry.path();
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "verify-log retention-root entry {} is a symlink",
                entry_path.display()
            ));
        }
        if metadata.is_dir() {
            if entry.file_name() == OsStr::new("retained") {
                return Err(format!(
                    "unexpected root-level retained verify-log layout: {}",
                    entry_path.display()
                ));
            }
            artifact_dirs.push(entry_path);
            continue;
        }
        let root_file = check_file_path_below(
            retention_root,
            &entry_path,
            "verify-log retention-root file",
        )?;
        if root_file.normalized != configured_result.normalized {
            return Err(format!(
                "unexpected root-level file in verify-log retention layout: {}",
                entry_path.display()
            ));
        }
    }

    let mut accounted = 0u64;
    let mut final_inodes = BTreeSet::new();
    let mut matched_descriptors = BTreeSet::new();
    let mut scanned_finals = Vec::new();
    for artifact_dir in artifact_dirs {
        let retained_dir = artifact_dir.join("retained");
        match fs::symlink_metadata(&retained_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect retained verify-log directory {}: {error}",
                    retained_dir.display()
                ));
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "retained verify-log path {} is not a non-symlink directory",
                    retained_dir.display()
                ));
            }
            Ok(_) => {}
        }
        let mut found_verify_dir = false;
        for retained_entry in fs::read_dir(&retained_dir).map_err(|error| {
            format!(
                "cannot read retained verify-log directory {}: {error}",
                retained_dir.display()
            )
        })? {
            let retained_entry = retained_entry.map_err(|error| {
                format!(
                    "cannot read entry below retained verify-log directory {}: {error}",
                    retained_dir.display()
                )
            })?;
            let retained_path = retained_entry.path();
            if retained_entry.file_name() != OsStr::new("verify") {
                return Err(format!(
                    "unreferenced bytes in retained verify-log layout: {}",
                    retained_path.display()
                ));
            }
            require_plain_directory(&retained_path, "retained verify-log directory")?;
            found_verify_dir = true;
        }
        if !found_verify_dir {
            continue;
        }
        let verify_dir = retained_dir.join("verify");
        require_plain_parent_chain(
            retention_root,
            &verify_dir.join("entry"),
            "retained verify-log directory",
        )?;
        for attempt_entry in fs::read_dir(&verify_dir).map_err(|error| {
            format!(
                "cannot read retained verify-log directory {}: {error}",
                verify_dir.display()
            )
        })? {
            let attempt_entry = attempt_entry.map_err(|error| {
                format!(
                    "cannot read entry below retained verify-log directory {}: {error}",
                    verify_dir.display()
                )
            })?;
            let attempt_path = attempt_entry.path();
            let attempt_name = attempt_entry.file_name();
            let attempt_name = attempt_name.to_str().ok_or_else(|| {
                format!(
                    "retained verify-log attempt directory {} is not UTF-8",
                    attempt_path.display()
                )
            })?;
            let attempt = attempt_name.parse::<u64>().ok().filter(|value| *value > 0);
            let Some(attempt) = attempt else {
                return Err(format!(
                    "retained verify-log attempt directory {} is not a canonical positive integer",
                    attempt_path.display()
                ));
            };
            if attempt.to_string() != attempt_name {
                return Err(format!(
                    "retained verify-log attempt directory {} is not canonical (expected {})",
                    attempt_path.display(),
                    attempt
                ));
            }
            require_plain_directory(&attempt_path, "retained verify-log attempt directory")?;
            let mut final_path = None;
            let mut staging_paths = Vec::new();
            for file_entry in fs::read_dir(&attempt_path).map_err(|error| {
                format!(
                    "cannot read retained verify-log attempt directory {}: {error}",
                    attempt_path.display()
                )
            })? {
                let file_entry = file_entry.map_err(|error| {
                    format!(
                        "cannot read entry below retained verify-log attempt directory {}: {error}",
                        attempt_path.display()
                    )
                })?;
                let path = file_entry.path();
                let name = file_entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    format!("retained verify-log file {} is not UTF-8", path.display())
                })?;
                if name == "run-1.log.gz" {
                    if final_path.replace(path.clone()).is_some() {
                        return Err(format!(
                            "duplicate retained verify-log finals in {}",
                            attempt_path.display()
                        ));
                    }
                } else if is_verify_log_staging_name(name) {
                    staging_paths.push(path);
                } else {
                    return Err(format!(
                        "unexpected file in retained verify-log directory: {}",
                        path.display()
                    ));
                }
            }
            if !staging_paths.is_empty() {
                for staging_path in &staging_paths {
                    inspect_gzip_file(
                        retention_root,
                        staging_path,
                        VERIFY_LOG_MAX_COMPRESSED_BYTES,
                        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                        "pre-existing retained verify-log staging file",
                    )?;
                }
                let detail = if final_path.is_some() {
                    "a final and staging file coexist"
                } else if staging_paths.len() > 1 {
                    "multiple staging files coexist"
                } else {
                    "an unreferenced staging file remains"
                };
                return Err(format!(
                    "restart-inconsistent retained verify-log layout in {}: {detail}; refusing to discard or silently charge {}",
                    attempt_path.display(),
                    staging_paths[0].display()
                ));
            }
            let final_path = final_path.ok_or_else(|| {
                format!(
                    "retained verify-log attempt directory {} contains no canonical final",
                    attempt_path.display()
                )
            })?;
            let key = (artifact_dir.clone(), attempt);
            let retained = descriptors.get(&key).ok_or_else(|| {
                format!(
                    "retained verify log {} has no result-row descriptor",
                    final_path.display()
                )
            })?;
            let inspected = inspect_gzip_file(
                retention_root,
                &final_path,
                VERIFY_LOG_MAX_COMPRESSED_BYTES,
                VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                "pre-existing retained verify log",
            )?;
            validate_retained_verify_log_inspection(retained, &inspected)?;
            let attempt_label = attempt.to_string();
            let run1_paths = verify_run_paths(&artifact_dir, &attempt_label, VerifyRun::Run1)?;
            let run2_paths = verify_run_paths(&artifact_dir, &attempt_label, VerifyRun::Run2)?;
            let mut bound_paths = vec![
                configured_result.clone(),
                check_file_path_below(
                    retention_root,
                    &final_path,
                    "pre-existing retained verify log",
                )?,
                check_file_path_below(retention_root, &run1_paths.log, "verify run-1 raw log")?,
                check_file_path_below(retention_root, &run2_paths.log, "verify run-2 raw log")?,
            ];
            for (description, path) in [
                ("run-1 result", run1_paths.result),
                ("run-1 stdout", run1_paths.stdout),
                ("run-1 stderr", run1_paths.stderr),
                ("run-1 summary", run1_paths.summary),
                ("run-2 result", run2_paths.result),
                ("run-2 stdout", run2_paths.stdout),
                ("run-2 stderr", run2_paths.stderr),
                ("run-2 summary", run2_paths.summary),
            ] {
                bound_paths.push(check_file_path_below(retention_root, &path, description)?);
            }
            reject_file_path_aliases(&bound_paths)?;
            for (description, path, expected) in [
                (
                    "pre-existing verify run-1 raw log",
                    &run1_paths.log,
                    ContentDigest {
                        sha256: retained.uncompressed_sha256.clone(),
                        bytes: retained.uncompressed_bytes,
                    },
                ),
                (
                    "pre-existing verify run-2 raw log",
                    &run2_paths.log,
                    ContentDigest {
                        sha256: retained.peer_uncompressed_sha256.clone(),
                        bytes: retained.peer_uncompressed_bytes,
                    },
                ),
            ] {
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "cannot inspect {description} {}: {error}",
                            path.display()
                        ));
                    }
                    Ok(_) => {
                        let (_, actual) = inspect_plain_file_bounded(
                            retention_root,
                            path,
                            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                            description,
                        )?;
                        if actual != expected {
                            return Err(format!(
                                "{description} {} disagrees with its result-row descriptor",
                                path.display()
                            ));
                        }
                    }
                }
            }
            if !final_inodes.insert(inspected.identity) {
                return Err(format!(
                    "retained verify-log finals reuse one inode at {}",
                    final_path.display()
                ));
            }
            scanned_finals.push(ScannedRetainedVerifyLog {
                path: final_path,
                retained: retained.clone(),
                inspection: inspected.clone(),
            });
            matched_descriptors.insert(key);
            accounted = accounted
                .checked_add(inspected.compressed.bytes)
                .ok_or_else(|| {
                    "pre-existing retained verify-log byte accounting overflowed u64".to_string()
                })?;
            if accounted > policy.maximum_total_compressed_bytes {
                return Err(format!(
                    "pre-existing retained verify logs require {accounted} compressed bytes, exceeding the {}-byte aggregate limit",
                    policy.maximum_total_compressed_bytes
                ));
            }
        }
    }
    if let Some(((artifact_dir, attempt), _)) = descriptors
        .iter()
        .find(|(key, _)| !matched_descriptors.contains(*key))
    {
        return Err(format!(
            "result-row descriptor for {} attempt {} has no retained verify-log final",
            artifact_dir.display(),
            attempt
        ));
    }
    for scanned in scanned_finals {
        if let Some(before_final_recheck) = before_final_recheck {
            before_final_recheck(&scanned.path);
        }
        let rechecked = inspect_gzip_file(
            retention_root,
            &scanned.path,
            VERIFY_LOG_MAX_COMPRESSED_BYTES,
            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
            "pre-existing retained verify log at end of restart scan",
        )?;
        validate_retained_verify_log_inspection(&scanned.retained, &rechecked)?;
        if rechecked != scanned.inspection {
            return Err(format!(
                "pre-existing retained verify log {} changed during restart scan",
                scanned.path.display()
            ));
        }
        require_path_identity(
            &scanned.path,
            rechecked.identity,
            "pre-existing retained verify log at end of restart scan",
        )?;
    }
    Ok(accounted)
}

// The variants are production fault boundaries constructed only by tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagingWriteFault {
    ZeroFirst,
    ShortFirst,
    ErrorAfterFirstSuccessfulWrite,
}

struct VerifyLogStagingWriter<'a> {
    file: &'a mut File,
    budget: VerifyLogRetentionBudget,
    /// Bytes still charged to `budget`, including a pessimistic whole write
    /// when the underlying writer reports an error after it may have changed
    /// the staging file.
    charged: &'a mut u64,
    on_first_write: Option<&'a (dyn Fn() + Sync)>,
    first_write_observed: bool,
    fault: Option<StagingWriteFault>,
    write_calls: usize,
}

impl Write for VerifyLogStagingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let requested = u64::try_from(buffer.len()).map_err(|_| {
            std::io::Error::other("retained verify-log write length does not fit u64")
        })?;
        let staged = self.charged.checked_add(requested).ok_or_else(|| {
            std::io::Error::other("retained verify-log staging byte count overflowed u64")
        })?;
        if staged > VERIFY_LOG_MAX_COMPRESSED_BYTES {
            return Err(std::io::Error::other(format!(
                "retained compressed verify log exceeds the {VERIFY_LOG_MAX_COMPRESSED_BYTES}-byte per-log limit"
            )));
        }
        self.budget
            .reserve_additional(requested)
            .map_err(std::io::Error::other)?;
        if !self.first_write_observed {
            self.first_write_observed = true;
            if let Some(on_first_write) = self.on_first_write {
                // The reservation lock has been released, but the requested
                // bytes remain charged while the callback and actual write
                // overlap other staging writers.
                on_first_write();
            }
        }
        let call = self.write_calls;
        self.write_calls += 1;
        let write_result = match self.fault {
            Some(StagingWriteFault::ZeroFirst) if call == 0 => Ok(0),
            Some(StagingWriteFault::ShortFirst) if call == 0 => {
                self.file.write(&buffer[..buffer.len().min(1)])
            }
            Some(StagingWriteFault::ErrorAfterFirstSuccessfulWrite) if call > 0 => Err(
                std::io::Error::other("injected retained verify-log staging write failure"),
            ),
            _ => self.file.write(buffer),
        };
        let written = match write_result {
            Ok(written) => written,
            Err(error) => {
                // `Write::write` may have changed the file before returning an
                // error. Keep the whole request charged until the staging file
                // is removed and its parent directory is synced.
                *self.charged = staged;
                return Err(error);
            }
        };
        if written == 0 && !buffer.is_empty() {
            if let Err(error) = self.budget.release(requested) {
                *self.charged = staged;
                return Err(std::io::Error::other(error));
            }
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let unused = requested
            .checked_sub(u64::try_from(written).expect("write result fits input length"))
            .expect("write cannot report more bytes than requested");
        if unused == 0 {
            *self.charged = staged;
        } else if let Err(error) = self.budget.release(unused) {
            // The full request remains charged if releasing the unused tail
            // fails. This deliberately overcounts instead of allowing bytes
            // on disk to escape the aggregate limit.
            *self.charged = staged;
            return Err(std::io::Error::other(error));
        } else {
            *self.charged = self
                .charged
                .checked_add(u64::try_from(written).expect("write result fits input length"))
                .expect("the requested staging-byte addition was already checked");
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn verify_retained_verify_log_with_limit(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    maximum_uncompressed_bytes: u64,
) -> Result<(), String> {
    if retained.role != RetainedVerifyLogRole::Run1 {
        return Err("retained verify log has an unsupported role".into());
    }
    if &retained.cell_id != expected_cell_id || retained.attempt != expected_attempt {
        return Err("retained verify log does not match its cell id and attempt".into());
    }
    let expected_relative = retained_verify_log_relative_path(expected_attempt)?;
    if Path::new(&retained.relative_path) != expected_relative {
        return Err(format!(
            "retained verify log path must be exactly {}, got {}",
            expected_relative.display(),
            retained.relative_path
        ));
    }
    let path = artifact_dir.join(&expected_relative);
    let inspected = inspect_gzip_file(
        artifact_dir,
        &path,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "retained compressed verify log",
    )?;
    validate_retained_verify_log_inspection(retained, &inspected)
}

fn validate_retained_verify_log_inspection(
    retained: &RetainedVerifyLog,
    inspected: &InspectedGzip,
) -> Result<(), String> {
    if inspected.compressed.sha256 != retained.compressed_sha256
        || inspected.compressed.bytes != retained.compressed_bytes
    {
        return Err(format!(
            "retained compressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.compressed_bytes,
            retained.compressed_sha256,
            inspected.compressed.bytes,
            inspected.compressed.sha256
        ));
    }
    if inspected.uncompressed.sha256 != retained.uncompressed_sha256
        || inspected.uncompressed.bytes != retained.uncompressed_bytes
    {
        return Err(format!(
            "retained uncompressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.uncompressed_bytes,
            retained.uncompressed_sha256,
            inspected.uncompressed.bytes,
            inspected.uncompressed.sha256
        ));
    }
    Ok(())
}

/// Re-read and authenticate one retained verify log against its typed binding.
pub fn verify_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
) -> Result<(), String> {
    verify_retained_verify_log_with_limit(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )
}

/// Reconstruct cleanup authority from one durable result row after restart.
///
/// Raw paths are derived from the fixed verify layout; the serialized
/// descriptor supplies both expected digests and remains bound to the outer
/// cell id and attempt by [`CellResult::validate_retained_verify_log_binding`].
pub fn reopen_retained_verify_log_publication(
    result: &CellResult,
) -> Result<RetainedVerifyLogPublication, String> {
    let retained = result
        .validate_retained_verify_log_binding()?
        .ok_or_else(|| "result row has no retained verify-log descriptor".to_string())?;
    let artifact_dir = PathBuf::from(&result.artifact_dir);
    let expected_id = CellId {
        test: result.test.clone(),
        mode: result.mode.clone(),
        backend: result.backend.clone(),
    };
    verify_retained_verify_log(&artifact_dir, retained, &expected_id, result.attempt)?;
    let attempt = result.attempt.to_string();
    let run1_raw = verify_run_paths(&artifact_dir, &attempt, VerifyRun::Run1)?.log;
    let run2_raw = verify_run_paths(&artifact_dir, &attempt, VerifyRun::Run2)?.log;
    Ok(RetainedVerifyLogPublication {
        retained: retained.clone(),
        run1_raw,
        run2_raw,
        run1_digest: ContentDigest {
            sha256: retained.uncompressed_sha256.clone(),
            bytes: retained.uncompressed_bytes,
        },
        run2_digest: ContentDigest {
            sha256: retained.peer_uncompressed_sha256.clone(),
            bytes: retained.peer_uncompressed_bytes,
        },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerifyLogTransactionFailurePoint {
    BeforeStagingFileCreation,
    AfterFinalRename,
    AfterDirectorySync,
    AfterFinalVerification,
    BeforeResultPublication,
}

#[derive(Clone, Copy, Default)]
struct VerifyLogTransactionHooks<'a> {
    failure: Option<VerifyLogTransactionFailurePoint>,
    result_publication_failure: Option<ResultPublicationFailurePoint>,
    fail_final_removal: bool,
    on_first_staging_write: Option<&'a (dyn Fn() + Sync)>,
    before_descriptor_publication: Option<&'a (dyn Fn(&Path) + Sync)>,
    staging_write_fault: Option<StagingWriteFault>,
    directory_sync_failure_at: Option<usize>,
}

fn retained_verify_log_attempt_index(
    result: &CellResult,
    cell_id: &CellId,
    attempt: u64,
    artifact_dir: &Path,
) -> Result<usize, String> {
    result.require_current_classification()?;
    result.require_current_timeout_policy()?;
    if result.test != cell_id.test
        || result.mode != cell_id.mode
        || result.backend != cell_id.backend
        || result.attempt != attempt
    {
        return Err("retained verify log does not match its result-row identity".into());
    }
    if Path::new(&result.artifact_dir) != artifact_dir {
        return Err("retained verify log does not match its result-row artifact directory".into());
    }
    if result.attempts.len() != 1 {
        return Err("a harness-managed verify result must contain exactly one attempt row".into());
    }
    if result.attempts[0].retained_verify_log.is_some() {
        return Err("verify attempt already carries a retained-log descriptor".into());
    }
    if result.outcome == "HOST-INAPPLICABLE" {
        return Err("a host-inapplicable cell cannot publish a retained verify log".into());
    }
    Ok(0)
}

fn verify_retention_paths_do_not_alias(
    pair: &ComparedVerifyPair,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    retained_path: &Path,
) -> Result<(), String> {
    let mut paths = vec![
        check_file_path_below(
            &retention_budget.retention_root,
            results_path,
            "result-row destination",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            &pair.run1_log,
            "verify run-1 raw log",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            &pair.run2_log,
            "verify run-2 raw log",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            retained_path,
            "retained verify-log destination",
        )?,
    ];
    for (description, path) in &pair.fixed_result_destinations {
        paths.push(check_file_path_below(
            &retention_budget.retention_root,
            path,
            description.clone(),
        )?);
    }
    let run1_identity = paths[1]
        .identity
        .ok_or_else(|| "verify run-1 raw log is missing before retention".to_string())?;
    let run2_identity = paths[2]
        .identity
        .ok_or_else(|| "verify run-2 raw log is missing before retention".to_string())?;
    if run1_identity != pair.run1_log_identity || run2_identity != pair.run2_log_identity {
        return Err("verify raw-log identity changed after comparison".into());
    }
    reject_file_path_aliases(&paths)
}

fn abort_staged_verify_log(
    temporary: tempfile::NamedTempFile,
    retained_directory: &Path,
    reservation: VerifyLogRetentionReservation,
    primary_error: String,
) -> Result<RetainedVerifyLogPublication, String> {
    let staging_path = temporary.path().to_owned();
    let retention_root = reservation.budget.retention_root.clone();
    match temporary.close() {
        Ok(()) => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; removed the retained verify-log staging file but could not roll back its accounting: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; removed the retained verify-log staging file but could not durably clean up its directories, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(removal_error) => {
            reservation.commit();
            Err(format!(
                "{primary_error}; could not remove retained verify-log staging file {}, so its bytes remain charged: {removal_error}",
                staging_path.display()
            ))
        }
    }
}

fn abort_empty_retained_verify_log_layout(
    retention_root: &Path,
    retained_directory: &Path,
    primary_error: String,
) -> Result<RetainedVerifyLogPublication, String> {
    match remove_empty_retained_verify_directories(retention_root, retained_directory) {
        Ok(()) => Err(primary_error),
        Err(cleanup_error) => Err(format!(
            "{primary_error}; could not durably clean up the empty retained verify-log directories: {cleanup_error}"
        )),
    }
}

fn abort_retained_verify_log(
    retained_path: &Path,
    retained_directory: &Path,
    reservation: VerifyLogRetentionReservation,
    primary_error: String,
    fail_final_removal: bool,
) -> Result<RetainedVerifyLogPublication, String> {
    let retention_root = reservation.budget.retention_root.clone();
    let removal = if fail_final_removal {
        Err(std::io::Error::other(
            "injected retained verify-log final-removal failure",
        ))
    } else {
        fs::remove_file(retained_path)
    };
    match removal {
        Ok(()) => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; removed the unreferenced retained verify log but could not roll back its accounting: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; removed the unreferenced retained verify log but could not durably clean up its directories, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; retained verify log was absent but its accounting could not be rolled back: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; retained verify log was absent but its directories could not be durably cleaned up, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(removal_error) => {
            reservation.commit();
            Err(format!(
                "{primary_error}; could not remove unreferenced retained verify log {}, so its bytes remain charged: {removal_error}",
                retained_path.display()
            ))
        }
    }
}

fn retain_verify_log_with_limit(
    pair: ComparedVerifyPair,
    maximum_uncompressed_bytes: u64,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    result: &mut CellResult,
    hooks: VerifyLogTransactionHooks<'_>,
) -> Result<RetainedVerifyLogPublication, String> {
    retention_budget.require_artifact_dir(&pair.artifact_dir)?;
    let retained_relative = retained_verify_log_relative_path(pair.attempt)?;
    let retained_path = pair.artifact_dir.join(&retained_relative);
    let attempt_index =
        retained_verify_log_attempt_index(result, &pair.cell_id, pair.attempt, &pair.artifact_dir)?;
    verify_retention_paths_do_not_alias(&pair, retention_budget, results_path, &retained_path)?;
    retention_budget.require_results_path(results_path)?;
    let (current_run1_identity, current_run1) = inspect_plain_file_bounded(
        &pair.artifact_dir,
        &pair.run1_log,
        maximum_uncompressed_bytes,
        "verify run-1 log",
    )?;
    let (current_run2_identity, current_run2) = inspect_plain_file_bounded(
        &pair.artifact_dir,
        &pair.run2_log,
        maximum_uncompressed_bytes,
        "verify run-2 log",
    )?;
    if current_run1_identity != pair.run1_log_identity
        || current_run2_identity != pair.run2_log_identity
    {
        return Err("verify raw-log identity changed after comparison".into());
    }
    if current_run1 != pair.run1_log_digest || current_run2 != pair.run2_log_digest {
        return Err(
            "verify logs changed after comparison; refusing to retain bytes that were not compared"
                .into(),
        );
    }

    let ComparedVerifyPair {
        comparison,
        artifact_dir,
        cell_id,
        attempt,
        run1_log,
        run2_log,
        run1_log_bytes,
        run1_log_digest,
        run1_log_identity: _,
        run2_log_digest,
        run2_log_identity: _,
        fixed_result_destinations: _,
    } = pair;
    let compared_info_messages = comparison
        .report
        .compared_log_messages
        .as_ref()
        .ok_or_else(|| "canonical verify comparison omitted compared log counts".to_string())?
        .left;
    let retained_directory = create_plain_relative_directory(
        &artifact_dir,
        retained_relative
            .parent()
            .expect("retained verify log has a parent"),
        "retained verify log directory",
    )?;
    match fs::symlink_metadata(&retained_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(format!(
                "retained verify log {} already exists",
                retained_path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify log {}: {error}",
                retained_path.display()
            ));
        }
    }

    if hooks.failure == Some(VerifyLogTransactionFailurePoint::BeforeStagingFileCreation) {
        return abort_empty_retained_verify_log_layout(
            &retention_budget.retention_root,
            &retained_directory,
            "injected failure before retained verify-log staging-file creation".into(),
        );
    }

    let mut temporary = match tempfile::Builder::new()
        .prefix(VERIFY_LOG_STAGING_PREFIX)
        .suffix(VERIFY_LOG_STAGING_SUFFIX)
        .tempfile_in(&retained_directory)
    {
        Ok(temporary) => temporary,
        Err(error) => {
            return abort_empty_retained_verify_log_layout(
                &retention_budget.retention_root,
                &retained_directory,
                format!(
                    "cannot create temporary retained verify log in {}: {error}",
                    retained_directory.display()
                ),
            );
        }
    };
    let created_staging_identity =
        FileIdentity::from_metadata(&temporary.as_file().metadata().map_err(|error| {
            format!(
                "cannot inspect temporary retained verify log {}: {error}",
                temporary.path().display()
            )
        })?);
    let mut charged = 0u64;
    let compression = {
        let mut run1_bytes = run1_log_bytes.as_slice();
        let staging_writer = VerifyLogStagingWriter {
            file: temporary.as_file_mut(),
            budget: retention_budget.clone(),
            charged: &mut charged,
            on_first_write: hooks.on_first_staging_write,
            first_write_observed: false,
            fault: hooks.staging_write_fault,
            write_calls: 0,
        };
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(staging_writer, Compression::new(6));
        let digest = copy_and_hash_bounded(
            &mut run1_bytes,
            &mut encoder,
            maximum_uncompressed_bytes,
            "verify run-1 log",
        );
        match digest {
            Ok(digest) => encoder
                .finish()
                .map(|_| digest)
                .map_err(|error| format!("cannot finish retained verify gzip stream: {error}")),
            Err(error) => {
                // Dropping the encoder can attempt final writes. `charged` is
                // borrowed by its writer, so create the reservation only after
                // the encoder has finished dropping.
                drop(encoder);
                Err(error)
            }
        }
    };
    let reservation = VerifyLogRetentionReservation {
        budget: retention_budget.clone(),
        compressed_bytes: charged,
        resolved: false,
    };
    let uncompressed = match compression {
        Ok(uncompressed) => uncompressed,
        Err(error) => {
            return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
        }
    };
    if uncompressed != run1_log_digest {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "captured verify run-1 bytes changed after comparison".into(),
        );
    }
    if let Err(error) = temporary
        .as_file_mut()
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
    {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            format!("cannot flush retained verify gzip stream: {error}"),
        );
    }
    let staging = match inspect_gzip_file(
        &artifact_dir,
        temporary.path(),
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "temporary compressed verify log",
    ) {
        Ok(staging) => staging,
        Err(error) => {
            return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
        }
    };
    if staging.identity != created_staging_identity {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "temporary retained verify log changed identity before inspection".into(),
        );
    }
    if staging.uncompressed != run1_log_digest {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "temporary compressed verify log does not decode to the compared run-1 bytes".into(),
        );
    }
    if staging.compressed.bytes != reservation.compressed_bytes {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            format!(
                "retained verify-log staging count changed: wrote {} bytes but read back {}",
                charged, staging.compressed.bytes
            ),
        );
    }
    if let Err(error) = require_path_identity(
        temporary.path(),
        staging.identity,
        "temporary compressed verify log",
    ) {
        return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
    }
    match temporary.persist_noclobber(&retained_path) {
        Ok(file) => drop(file),
        Err(error) => {
            let primary_error = format!(
                "cannot atomically publish retained verify log {}: {}",
                retained_path.display(),
                error.error
            );
            return abort_staged_verify_log(
                error.file,
                &retained_directory,
                reservation,
                primary_error,
            );
        }
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterFinalRename) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log rename".into(),
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = sync_relative_directory_chain_with_failure(
        &retention_budget.retention_root,
        &retained_directory,
        "retained verify-log directory",
        hooks.directory_sync_failure_at,
    ) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterDirectorySync) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log directory sync".into(),
            hooks.fail_final_removal,
        );
    }

    let mut final_evidence = match open_and_inspect_gzip_file(
        &artifact_dir,
        &retained_path,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "retained compressed verify log",
    ) {
        Ok(inspection) => inspection,
        Err(error) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error,
                hooks.fail_final_removal,
            );
        }
    };
    let final_inspection = final_evidence.inspection.clone();
    if final_inspection.compressed != staging.compressed
        || final_inspection.uncompressed != staging.uncompressed
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained verify log changed during atomic rename".into(),
            hooks.fail_final_removal,
        );
    }
    let retained = RetainedVerifyLog {
        relative_path: retained_relative.to_string_lossy().into_owned(),
        role: RetainedVerifyLogRole::Run1,
        cell_id: cell_id.clone(),
        attempt,
        uncompressed_sha256: final_inspection.uncompressed.sha256.clone(),
        uncompressed_bytes: final_inspection.uncompressed.bytes,
        compressed_sha256: final_inspection.compressed.sha256.clone(),
        compressed_bytes: final_inspection.compressed.bytes,
        peer_uncompressed_sha256: run2_log_digest.sha256.clone(),
        peer_uncompressed_bytes: run2_log_digest.bytes,
        compared_info_messages,
    };
    if let Err(error) = validate_retained_verify_log_inspection(&retained, &final_inspection) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterFinalVerification) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log final verification".into(),
            hooks.fail_final_removal,
        );
    }
    if let Some(before_descriptor_publication) = hooks.before_descriptor_publication {
        before_descriptor_publication(&retained_path);
    }
    let publication_inspection = match inspect_open_gzip_file(
        &mut final_evidence.file,
        &retained_path,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "retained compressed verify log immediately before descriptor publication",
    ) {
        Ok(inspection) => inspection,
        Err(error) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error,
                hooks.fail_final_removal,
            );
        }
    };
    if publication_inspection.identity != final_inspection.identity {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained compressed verify log changed identity before publication".into(),
            hooks.fail_final_removal,
        );
    }
    if publication_inspection.compressed != final_inspection.compressed
        || publication_inspection.uncompressed != final_inspection.uncompressed
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained verify log changed before descriptor publication".into(),
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = validate_retained_verify_log_inspection(&retained, &publication_inspection)
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = require_path_identity(
        &retained_path,
        publication_inspection.identity,
        "retained compressed verify log immediately before descriptor publication",
    ) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::BeforeResultPublication) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure at retained verify-log descriptor-publication boundary".into(),
            hooks.fail_final_removal,
        );
    }
    let result_publication = match retention_budget.result_publication.lock() {
        Ok(publication) => publication,
        Err(_) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                "retained verify-log result-publication lock is poisoned".into(),
                hooks.fail_final_removal,
            );
        }
    };
    result.attempts[attempt_index].retained_verify_log = Some(retained.clone());
    if let Err(error) =
        append_result_with_failure(results_path, result, hooks.result_publication_failure, true)
    {
        drop(result_publication);
        if !error.descriptor_visible {
            result.attempts[attempt_index].retained_verify_log = None;
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error.message,
                hooks.fail_final_removal,
            );
        }
        reservation.commit();
        return Err(format!(
            "{}; retained verify log remains published and charged because its result row may be visible",
            error.message
        ));
    }
    drop(result_publication);
    reservation.commit();

    Ok(RetainedVerifyLogPublication {
        retained,
        run1_raw: run1_log,
        run2_raw: run2_log,
        run1_digest: run1_log_digest,
        run2_digest: run2_log_digest,
    })
}

/// Durably publish run 1 and the result row that retains its descriptor.
///
/// The compressed file is staged, provisionally accounted, renamed, synced,
/// and re-read before the descriptor is added to the attempt and the complete
/// result row is atomically replaced and synced. Accounting becomes permanent
/// only after that row is durable.
pub fn publish_retained_verify_log(
    pair: ComparedVerifyPair,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    result: &mut CellResult,
) -> Result<RetainedVerifyLogPublication, String> {
    retain_verify_log_with_limit(
        pair,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        retention_budget,
        results_path,
        result,
        VerifyLogTransactionHooks::default(),
    )
}

fn cleanup_verify_log_sources_with(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
    maximum_uncompressed_bytes: u64,
    remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    cleanup_verify_log_sources_with_hook(
        artifact_dir,
        publication,
        maximum_uncompressed_bytes,
        None,
        remove,
    )
}

fn cleanup_verify_log_sources_with_hook(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
    maximum_uncompressed_bytes: u64,
    before_raw_removal: Option<&dyn Fn()>,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let retained_path = artifact_dir.join(&publication.retained.relative_path);
    let cleanup_paths = [
        check_file_path_below(
            artifact_dir,
            &retained_path,
            "retained verify log during raw cleanup",
        )?,
        check_file_path_below(
            artifact_dir,
            &publication.run1_raw,
            "verify run-1 raw log during cleanup",
        )?,
        check_file_path_below(
            artifact_dir,
            &publication.run2_raw,
            "verify run-2 raw log during cleanup",
        )?,
    ];
    reject_file_path_aliases(&cleanup_paths)?;
    verify_retained_verify_log_with_limit(
        artifact_dir,
        &publication.retained,
        &publication.retained.cell_id,
        publication.retained.attempt,
        maximum_uncompressed_bytes,
    )?;
    let raw_inputs = [
        (
            "verify run-1 log before cleanup",
            &publication.run1_raw,
            &publication.run1_digest,
        ),
        (
            "verify run-2 log before cleanup",
            &publication.run2_raw,
            &publication.run2_digest,
        ),
    ];
    let mut raw_evidence = Vec::with_capacity(raw_inputs.len());
    for (description, path, expected) in raw_inputs {
        let evidence = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} {}: {error}",
                    path.display()
                ));
            }
            Ok(_) => {
                let evidence = open_and_inspect_plain_file_bounded(
                    artifact_dir,
                    path,
                    maximum_uncompressed_bytes,
                    description,
                )?;
                if &evidence.digest != expected {
                    return Err(
                        "verify logs changed after retention; refusing to remove the recovery inputs"
                            .into(),
                    );
                }
                Some(evidence)
            }
        };
        raw_evidence.push((description, path, evidence));
    }
    if let Some(before_raw_removal) = before_raw_removal {
        before_raw_removal();
    }
    for (description, path, evidence) in &raw_evidence {
        match evidence {
            Some(evidence) => require_path_identity(path, evidence.identity, description)?,
            None => match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot recheck absent {description} {} before cleanup: {error}",
                        path.display()
                    ));
                }
                Ok(_) => {
                    return Err(format!(
                        "{description} {} appeared after cleanup evidence was collected",
                        path.display()
                    ));
                }
            },
        }
    }

    let mut errors = Vec::new();
    let mut parents_to_sync = BTreeSet::new();
    for index in [1usize, 0] {
        let (description, path, evidence) = &raw_evidence[index];
        let Some(evidence) = evidence else {
            continue;
        };
        if let Err(error) = require_path_identity(path, evidence.identity, description) {
            errors.push(error);
            break;
        }
        match remove(path) {
            Ok(()) => match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(parent) = path.parent() {
                        parents_to_sync.insert(parent.to_owned());
                    }
                }
                Err(error) => errors.push(format!(
                    "cannot inspect {description} {} after removal: {error}",
                    path.display()
                )),
                Ok(_) => errors.push(format!(
                    "removal of {description} {} reported success but the path still exists",
                    path.display()
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(path) {
                    Err(check) if check.kind() == std::io::ErrorKind::NotFound => {}
                    Err(check) => errors.push(format!(
                        "cannot inspect {description} {} after a not-found removal: {check}",
                        path.display()
                    )),
                    Ok(_) => errors.push(format!(
                        "removal of {description} {} reported not found but the path still exists",
                        path.display()
                    )),
                }
            }
            Err(error) => errors.push(format!(
                "cannot remove {description} {}: {error}",
                path.display()
            )),
        }
    }
    for parent in parents_to_sync {
        if let Err(error) = sync_plain_directory(&parent, "verify raw-log parent after cleanup") {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Idempotently remove the two raw inputs after the caller has durably
/// published [`RetainedVerifyLogPublication::retained`]. A cleanup error does
/// not erase that descriptor or the already-verified gzip, and a later call
/// resumes with whichever raw input remains.
pub fn cleanup_verify_log_sources(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
) -> Result<(), String> {
    cleanup_verify_log_sources_with(
        artifact_dir,
        publication,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        |path| fs::remove_file(path),
    )
}

/// Build one ordinary `hermit run` command for a harness-managed verify pair.
///
/// This is intentionally separate from [`build_spec`]'s still-live internal
/// `--verify` path. Callers may construct and test the replacement without
/// changing production scheduling until every selected backend can provide the
/// same authoritative artifacts and `/test` bind.
pub fn build_verify_run_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: u64,
    run: VerifyRun,
    timeout_seconds: u64,
) -> Result<VerifyRunSpec, String> {
    if attempt == 0 {
        return Err("harness-managed verify attempt must be positive".into());
    }
    if cell.id.mode != "verify" {
        return Err(format!(
            "harness-managed verify run requested for {} mode",
            cell.id.mode
        ));
    }
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    match backend {
        "dbt" => {
            return Err(
                "harness-managed DBT verify cannot bind its isolated work directory at /test"
                    .into(),
            );
        }
        "sabre" => {
            return Err(
                "harness-managed SaBRe verify has no authoritative ordinary-run log sink".into(),
            );
        }
        "ptrace" | "kvm" | "liteinst" => {}
        other => {
            return Err(format!(
                "harness-managed verify is unsupported on backend {other}"
            ));
        }
    }

    let mode_recipe = &cell.test.modes["verify"];
    if mode_recipe
        .workdir
        .as_deref()
        .is_some_and(|workdir| workdir != HERMETIC_TEST_WORKDIR)
    {
        return Err(format!(
            "harness-managed verify requires the fixed guest workdir {HERMETIC_TEST_WORKDIR}; {} requests {}",
            cell.id.test,
            mode_recipe.workdir.as_deref().expect("checked above")
        ));
    }

    let attempt_label = attempt.to_string();
    let paths = verify_run_paths(&dir, &attempt_label, run)?;
    let capture_parent = paths
        .log
        .parent()
        .expect("a verify log path always has a capture directory");
    let capture_parent_relative = capture_parent.strip_prefix(&dir).map_err(|_| {
        format!(
            "verify capture directory {} escaped cell artifact directory {}",
            capture_parent.display(),
            dir.display()
        )
    })?;
    create_plain_relative_directory(&dir, capture_parent_relative, "verify capture directory")?;
    let env = execution_cell_env(context, &dir, true);
    let expected_determinism = GuestRunDeterminism {
        detlog_io_buffers: mode_recipe.compare_io_buffers != Some(false),
        virtualize_time: true,
    };
    let mut argv = vec![
        context.hermit_bin.to_string_lossy().into_owned(),
        "--log".into(),
        "info".into(),
        "--log-file".into(),
        paths.log.to_string_lossy().into_owned(),
        "run".into(),
        "--base-env=minimal".into(),
        "--backend".into(),
        backend.into(),
        "--strict".into(),
    ];
    if mode_recipe.compare_io_buffers == Some(false) {
        argv.push("--no-detlog-io-buffers".into());
    }
    if mode_recipe.rcb_time == Some(false) {
        argv.push("--no-rcb-time".into());
    }
    argv.extend([
        "--summary-json".into(),
        paths.summary.to_string_lossy().into_owned(),
        "--run-result-json".into(),
        paths.result.to_string_lossy().into_owned(),
        "--guest-stdout".into(),
        paths.stdout.to_string_lossy().into_owned(),
        "--guest-stderr".into(),
        paths.stderr.to_string_lossy().into_owned(),
        format!(
            "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
            paths.workdir.to_string_lossy()
        ),
        "--workdir".into(),
        HERMETIC_TEST_WORKDIR.into(),
    ]);
    append_guest_env_args(&mut argv, &env, true);
    argv.push("--".into());
    argv.extend(guest_argv.clone());

    let execution = CellRunSpec {
        id: cell.id.clone(),
        lane: cell.test.lane.clone(),
        category: cell.category.clone(),
        cwd: context.root.clone(),
        env,
        argv,
        guest_argv,
        timeout_seconds,
        verdict_path: None,
        verification_log_dir: None,
        sabre_path_evidence: None,
        cell_dir: dir,
        attempt: format!("{attempt_label}-{}", run.directory_name()),
        fixed_workdir_source: paths.workdir.clone(),
    };
    Ok(VerifyRunSpec {
        run,
        execution,
        paths,
        attempt,
        expected_determinism,
    })
}

/// The existing pressure-test result vocabulary for one executed cell.
///
/// This is separate from [`FailureClass`]: two product failures can have
/// different results (`determinism-failure` and `crash-error`), while both are
/// still product failures. Keeping both values on the framework-written row
/// preserves that per-attempt distinction without asking pressure-test to
/// reconstruct it from process status and retained report files.
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
pub enum ObservedResult {
    Pass,
    DeterminismFailure,
    ParityFailure,
    ReplayFailure,
    CrashError,
    Timeout,
    Oom,
}

impl ObservedResult {
    pub fn parse(value: &str) -> Result<Self, String> {
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

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::DeterminismFailure => "determinism-failure",
            Self::ParityFailure => "parity-failure",
            Self::ReplayFailure => "replay-failure",
            Self::CrashError => "crash-error",
            Self::Timeout => "timeout",
            Self::Oom => "oom",
        }
    }

    pub fn carries_divergence_position(self) -> bool {
        matches!(
            self,
            Self::DeterminismFailure | Self::ParityFailure | Self::ReplayFailure
        )
    }

    pub fn failure_class(self) -> Option<FailureClass> {
        match self {
            Self::Pass => None,
            Self::DeterminismFailure
            | Self::ParityFailure
            | Self::ReplayFailure
            | Self::CrashError => Some(FailureClass::ProductFailure),
            Self::Timeout | Self::Oom => Some(FailureClass::NoResult),
        }
    }
}

/// Attribution for a non-passing cell result.
///
/// These are the owner's four existing failure classes. `None` is reserved for
/// a pass or a retained row written before this field existed; every current
/// non-pass written by the framework carries one of these values.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ProductFailure,
    UnderstoodInfrastructureFailure,
    UnderstoodPrerequisiteFailure,
    NoResult,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProductFailure => "product_failure",
            Self::UnderstoodInfrastructureFailure => "understood_infrastructure_failure",
            Self::UnderstoodPrerequisiteFailure => "understood_prerequisite_failure",
            Self::NoResult => "no_result",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "product_failure" => Ok(Self::ProductFailure),
            "understood_infrastructure_failure" => Ok(Self::UnderstoodInfrastructureFailure),
            "understood_prerequisite_failure" => Ok(Self::UnderstoodPrerequisiteFailure),
            "no_result" => Ok(Self::NoResult),
            other => Err(format!("unknown failure_class `{other}`")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptResult {
    pub index: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub duration_ms: u128,
    /// CPU consumed by the launched process group.
    ///
    /// Completed commands use `wait4`; a CPU timeout retains the last live
    /// process-group observation when that is larger. It is not inferred from
    /// wall time and remains attributable when cells execute concurrently or
    /// move their work outside the enclosing DAG cgroup.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    pub observation_sha256: Option<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub verification_report: Option<String>,
    pub verification_report_sha256: Option<String>,
    /// The retained run-1 log and its binding to the compared run-2 log.
    ///
    /// Older result rows predate retained harness-managed logs and therefore
    /// deserialize this field as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_verify_log: Option<RetainedVerifyLog>,
    /// Runtime totals for the two executions compared by this attempt.
    pub runtime: Option<VerificationRuntime>,
    /// The divergence position, LIFTED OUT of `verification_report` so it is a
    /// field rather than a substring.
    ///
    /// `verification_report` already contained these numbers, but as a
    /// JSON-ENCODED STRING, so every consumer had to parse the row and then
    /// parse a string inside it. That is why no cell has ever reported where
    /// its divergence began: the value was present and unreachable. One
    /// attempt is one observation, so this is the level the position belongs
    /// at -- a cell with three attempts has three of them.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, in virtual-nanosecond units.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence rather than bounding it.
    pub first_divergent_record: Option<u64>,
    /// Syscalls the guest completed before diverging. A different keyspace from
    /// every other coordinate here; see the note in canonical_verdict.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the left execution, with only the
    /// separately recorded syscall number, scheduler turn, and committed time
    /// removed.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from the right execution.
    pub first_divergent_right_message: Option<String>,
    pub sabre_path_evidence: Option<String>,
    pub sabre_path_evidence_sha256: Option<String>,
    pub reason: Option<String>,
}

/// One test-harness cell observation written to `results.jsonl`.
///
/// This is not [`crate::ledger::CellResult`]. That type is the validation
/// ledger's compact cell verdict; this type is the test framework's complete
/// execution result, including invocation, attempts, timing, and retained
/// evidence.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CellResult {
    pub schema: u64,
    pub run_id: String,
    /// Short machine name measured with this row. A hostname alone is not a
    /// stable host class, but it is the existing per-machine key used by the
    /// parent store.
    #[serde(default)]
    pub machine_shortname: String,
    /// `uname -r` for the kernel under which this cell ran.
    #[serde(default)]
    pub kernel_version: String,
    /// The complete closed set of capability verdicts that determined which
    /// cells could execute on this host.
    #[serde(default)]
    pub host_capabilities: HostCapabilities,
    /// The cell attempt that produced this observation. A retry is a second
    /// observation, so it receives the next positive ordinal instead of
    /// replacing the earlier row.
    #[serde(default = "first_attempt")]
    pub attempt: u64,
    /// Pressure-test repetition number supplied to the test framework.
    /// Ordinary validate rows leave this absent and use `attempt` as their
    /// series run index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_index: Option<u64>,
    /// HEAD of the CHECKOUT the harness ran in. NOT the provenance of the
    /// binary that produced this measurement, despite the name.
    ///
    /// The harness cannot learn a binary's provenance from a path, and
    /// `hermit_bin` defaults to `target/debug/hermit` -- whatever file happens
    /// to be there. During any rebase-and-rerun loop the checkout moves and the
    /// binary does not, so this names a commit that never built the artifact.
    /// Read [`CellResult::binary_build_sha`] for that; read this only as "where
    /// the harness was standing".
    pub hermit_sha: String,
    /// Dirtiness of that same CHECKOUT at run time -- again not a statement
    /// about the binary, which may have been built from a different tree in a
    /// different state.
    pub source_tree_dirty: bool,
    pub binary_sha256: Option<String>,
    /// What the BINARY says about its own origin: the commit it was compiled
    /// from, with a `-dirty` suffix when that tree was dirty.
    ///
    /// `binary_sha256` proves WHICH file ran; this says WHERE THAT FILE CAME
    /// FROM. Together they are a complete attribution, and neither substitutes
    /// for the other. `None` means the binary could not be asked or did not
    /// answer recognisably -- deliberately distinct from a value, so "not
    /// established" never reads as agreement.
    pub binary_build_sha: Option<String>,
    #[serde(default)]
    pub test_sha256: String,
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: Option<String>,
    pub classification: String,
    pub outcome: String,
    /// What this cell attempt observed, using the same closed vocabulary that
    /// pressure summaries and the scorecard consume. `None` means either an
    /// infrastructure result or a retained row written before this field.
    #[serde(default)]
    pub result: Option<ObservedResult>,
    /// Who the non-pass is attributed to. This is deliberately separate from
    /// `error_kind`, which retains the more specific mechanism such as
    /// `backend-unavailable` or `incomplete-verification-evidence`.
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    pub error_kind: Option<String>,
    /// The cell wall-clock bound recorded by schema 4. It remains the fixture
    /// preparation and inner-invocation wall bound; the additive fields below
    /// describe the post-preparation policy without changing this field's
    /// meaning for existing readers.
    #[serde(default)]
    pub timeout_seconds: u64,
    /// Aggregate post-preparation CPU budget across attempts and seeds.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_cpu_timeout_seconds: Option<u64>,
    /// Outer post-preparation wall backstop for a near-zero-CPU wedge.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_wall_timeout_seconds: Option<u64>,
    /// Measured wall time for a cell that reached execution. Absent when the
    /// cell never ran; a measured zero remains a valid sub-millisecond result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u128>,
    /// CPU consumed by this cell's preparation and launched attempts.
    ///
    /// Null when the cell never reached execution or the runner could not
    /// obtain a complete measurement. A measured zero remains a real value.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    /// Runtime totals from the first attempt that produced them.
    pub runtime: Option<VerificationRuntime>,
    pub log_level: Option<String>,
    #[serde(default)]
    pub effective_args: Vec<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub relaxations: Vec<String>,
    pub execution_path: Option<JsonValue>,
    pub diversity: Option<JsonValue>,
    pub attempts: Vec<AttemptResult>,
    /// Cell-level divergence position: the FIRST attempt that located one.
    ///
    /// This mirrors `reason` exactly, which is also
    /// `attempts.iter().find_map(...)`. Two other rules were available and both
    /// are worse here: the LAST attempt would let a passing retry erase the
    /// position a failing attempt found, and a min across attempts would be a
    /// one-run aggregate that reads like the cross-run `ObservedRange`
    /// earliest/latest without having a sample behind it. The per-attempt
    /// fields are still present, so a consumer that wants a different rule can
    /// apply it rather than being stuck with this one.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, selected by the same first-attempt rule.
    /// Selected INDEPENDENTLY, so a report carrying only some of the three
    /// coordinates still contributes the ones it has.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence. Same first-attempt rule.
    pub first_divergent_record: Option<u64>,
    /// Syscalls completed before diverging. Same first-attempt rule.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the first attempt that recorded
    /// message content.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from that same attempt.
    pub first_divergent_right_message: Option<String>,
    pub reason: Option<String>,
    /// Where this cell's retained evidence lives.
    ///
    /// Carried on the result rather than recomputed by readers: the directory
    /// is `<result_root>/runs/<run_id>/<slug>` for attempt 1 and uses an
    /// `-attempt-N` suffix for a retry. The slug is built in exactly one place
    /// (`cell_artifact_dir`). A consumer that rebuilds it from `test`, `mode`,
    /// `backend`, and `attempt` becomes a second definition that goes stale
    /// silently, and a path that points at nothing is worse than no path at all.
    pub artifact_dir: String,
}

impl CellResult {
    fn validate_retained_verify_log_binding(&self) -> Result<Option<&RetainedVerifyLog>, String> {
        let expected_id = CellId {
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
        };
        let retained_attempts = self
            .attempts
            .iter()
            .filter(|attempt| attempt.retained_verify_log.is_some())
            .collect::<Vec<_>>();
        if retained_attempts.len() > 1 {
            return Err("one result row carries more than one retained verify log".into());
        }
        let Some(attempt_row) = retained_attempts.first() else {
            return Ok(None);
        };
        let retained = attempt_row
            .retained_verify_log
            .as_ref()
            .expect("the attempt was selected for carrying a descriptor");
        if self.mode != "verify" {
            return Err("a non-verify result row carries a retained verify log".into());
        }
        if retained.cell_id != expected_id || retained.attempt != self.attempt {
            return Err("retained verify log does not match its result-row identity".into());
        }
        let expected_relative = retained_verify_log_relative_path(self.attempt)?;
        if Path::new(&retained.relative_path) != expected_relative {
            return Err(format!(
                "retained verify log path must be exactly {}, got {}",
                expected_relative.display(),
                retained.relative_path
            ));
        }
        Ok(Some(retained))
    }

    fn validate_retained_verify_logs(&self) -> Result<(), String> {
        let Some(retained) = self.validate_retained_verify_log_binding()? else {
            return Ok(());
        };
        let expected_id = CellId {
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
        };
        verify_retained_verify_log(
            Path::new(&self.artifact_dir),
            retained,
            &expected_id,
            self.attempt,
        )
    }

    /// Validate the additive timeout fields while keeping earlier schema-4 rows
    /// readable. `timeout_seconds` retains its original wall-bound meaning;
    /// only rows carrying both new fields claim the execution CPU/backstop
    /// policy.
    pub fn validate_timeout_policy(&self) -> Result<(), String> {
        match (
            self.execution_cpu_timeout_seconds,
            self.execution_wall_timeout_seconds,
        ) {
            (None, None) => Ok(()),
            (Some(cpu), Some(wall)) if cpu > 0 && wall == self.timeout_seconds && wall > cpu => {
                Ok(())
            }
            (cpu, wall) => Err(format!(
                "timeout policy disagrees: timeout_seconds={} execution_cpu_timeout_seconds={cpu:?} execution_wall_timeout_seconds={wall:?}",
                self.timeout_seconds
            )),
        }
    }

    /// Require evidence emitted by the current runner rather than a readable
    /// schema-4 row written before the additive timeout fields existed.
    pub fn require_current_timeout_policy(&self) -> Result<(), String> {
        self.validate_timeout_policy()?;
        if self.execution_cpu_timeout_seconds.is_none()
            || self.execution_wall_timeout_seconds.is_none()
        {
            return Err("current result omitted explicit execution timeout bounds".into());
        }
        Ok(())
    }

    /// Check the classification fields written by the current framework.
    ///
    /// Both fields remain optional in the deserializer because schema 4 also
    /// names retained rows written before they existed. Current publication is
    /// stricter: every result is recorded, and every non-pass is attributed.
    pub fn require_current_classification(&self) -> Result<(), String> {
        match self.outcome.as_str() {
            "PASS" => {
                if self.result != Some(ObservedResult::Pass) || self.failure_class.is_some() {
                    return Err(format!(
                        "PASS result must carry result=pass and no failure_class, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "HOST-INAPPLICABLE" => {
                if self.result.is_some()
                    || self.failure_class != Some(FailureClass::UnderstoodPrerequisiteFailure)
                {
                    return Err(format!(
                        "HOST-INAPPLICABLE result must carry understood_prerequisite_failure and no observed result, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "FAIL" => {
                let failure_class = self.failure_class.ok_or_else(|| {
                    format!(
                        "{} result has no failure_class; current non-passes must be attributed",
                        self.outcome
                    )
                })?;
                let result = self.result.ok_or_else(|| {
                    "FAIL result has no observed result; the failure kind was lost".to_string()
                })?;
                let expected = result.failure_class().ok_or_else(|| {
                    format!("FAIL result cannot carry the passing result {result:?}")
                })?;
                if failure_class != expected {
                    return Err(format!(
                        "observed result {} requires failure_class {:?}, got {:?}",
                        result.as_str(),
                        expected,
                        failure_class
                    ));
                }
            }
            "ERROR" => match (self.result, self.failure_class) {
                (
                    None,
                    Some(
                        FailureClass::UnderstoodInfrastructureFailure
                        | FailureClass::UnderstoodPrerequisiteFailure
                        | FailureClass::NoResult,
                    ),
                )
                | (
                    Some(ObservedResult::Timeout | ObservedResult::Oom),
                    Some(FailureClass::NoResult),
                ) => {}
                other => {
                    return Err(format!(
                        "ERROR result must carry a non-product observation/classification, got {other:?}"
                    ));
                }
            },
            other => return Err(format!("unknown cell outcome {other:?}")),
        }
        Ok(())
    }

    /// Retained schema-4 rows predate these fields. Accept that exact legacy
    /// absence, but validate any row that carries either half of the contract.
    pub fn validate_recorded_classification(&self) -> Result<(), String> {
        if self.result.is_none() && self.failure_class.is_none() {
            return Ok(());
        }
        self.require_current_classification()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledWorkerCapacity(usize);

impl ScheduledWorkerCapacity {
    pub fn new(configured: usize) -> Self {
        assert!(configured > 0, "scheduled worker capacity must be positive");
        Self(configured)
    }

    pub fn configured(self) -> usize {
        self.0
    }

    pub fn workers_for(self, count: usize) -> usize {
        self.0.min(count)
    }
}

#[derive(Clone)]
pub struct RunContext {
    pub root: PathBuf,
    pub hermit_bin: PathBuf,
    pub result_root: PathBuf,
    pub build_root: PathBuf,
    pub run_id: String,
    pub machine_shortname: String,
    pub kernel_version: String,
    pub host_capabilities: HostCapabilities,
    pub attempt: u64,
    pub run_index: Option<u64>,
    pub source_sha: String,
    pub source_dirty: bool,
    /// Provenance the hermit binary reports about itself, probed once per run.
    /// See [`CellResult::binary_build_sha`].
    pub binary_build_sha: Option<String>,
    pub prebuilt: bool,
    pub keep_logs: bool,
    pub run_verify_strict: bool,
    pub record_verify_strict: bool,
    /// Per-machine scaling for individual test CPU and wall bounds. This is execution policy,
    /// not part of the canonical manifest.
    pub timeout_multipliers: TimeoutMultipliers,
    pub scheduled_worker_capacity: ScheduledWorkerCapacity,
    pub isolated_workdir: Option<PathBuf>,
}

impl RunContext {
    /// A copy of this context whose result row and retained artifacts use the
    /// given cell-attempt ordinal.
    pub fn with_attempt(&self, attempt: u64) -> Self {
        Self {
            attempt,
            ..self.clone()
        }
    }

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
        let run_index = std::env::var(E2E_RUN_INDEX_ENV)
            .ok()
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    format!("{E2E_RUN_INDEX_ENV} must be a non-negative integer, got {value:?}")
                })
            })
            .transpose()?;
        let machine_name = match std::env::var(E2E_MACHINE_SHORTNAME_ENV) {
            Ok(value) if !value.is_empty() => value,
            _ => command_text("hostname", &["-s"])
                .or_else(|_| command_text("hostname", &[]))
                .map_err(|error| format!("cannot establish machine_shortname: {error}"))?,
        };
        let machine_shortname = machine_name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        if machine_shortname.is_empty() || machine_shortname.contains('/') {
            return Err(format!(
                "machine_shortname must be one nonempty path segment, got {machine_shortname:?}"
            ));
        }
        let kernel_version = match std::env::var(E2E_KERNEL_VERSION_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => command_text("uname", &["-r"])
                .map_err(|error| format!("cannot establish kernel_version: {error}"))?,
        };
        let host_capabilities = probe_host_capabilities();
        let build_root = std::env::var_os("E2E_BUILD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| result_root.join("build").join(&source_sha));
        let hermit_bin = std::env::var_os("HERMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/debug/hermit"));
        // Ask the binary where it came from, the same way this function already
        // asks it what flags it supports. `source_sha` above describes the
        // checkout; only the binary can describe the binary.
        let binary_build_sha = probe_binary_build_sha(&hermit_bin);
        // Published main still exposes the legacy `--verify-strict` spelling;
        // the canonical-only cutover removes it and makes bare `--verify`
        // canonical.  Detect the running binary rather than keying behavior to
        // a source SHA.  Whichever spelling executes is retained verbatim in
        // the result row, so this bridge cannot hide the comparison policy.
        let run_verify_strict =
            command_help_contains(&hermit_bin, &["run", "--help"], "--verify-strict");
        let isolated_workdir = match std::env::var_os(ISOLATED_WORKDIR_ENV) {
            None => None,
            Some(value) if value == HERMETIC_TEST_WORKDIR => {
                Some(PathBuf::from(HERMETIC_TEST_WORKDIR))
            }
            Some(value) => {
                return Err(format!(
                    "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {}",
                    PathBuf::from(value).display()
                ));
            }
        };
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
            machine_shortname,
            kernel_version,
            host_capabilities,
            attempt: 1,
            run_index,
            source_sha,
            source_dirty,
            binary_build_sha,
            prebuilt,
            keep_logs: std::env::var("E2E_KEEP_VERIFY_LOGS").as_deref() == Ok("1"),
            run_verify_strict,
            record_verify_strict,
            timeout_multipliers: timeout_multipliers_from_env()?,
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir,
        })
    }

    pub fn with_scheduled_worker_capacity(
        mut self,
        scheduled_worker_capacity: ScheduledWorkerCapacity,
    ) -> Self {
        self.scheduled_worker_capacity = scheduled_worker_capacity;
        self
    }
}

/// One initial execution plus one retry, scoped to one selected cell.
pub const MAX_ATTEMPTS_PER_CELL: u64 = 2;

/// Select the outcome that the framework reports for one cell's complete
/// attempt history.
///
/// A passing retry makes the cell pass, while the appended rows still retain
/// the failure it recovered from. If no attempt passes, any product failure
/// keeps the cell failed even when a later attempt ends in an infrastructure
/// error. A history containing only infrastructure errors remains an error.
/// Host-inapplicable is terminal and cannot be mixed with executed attempts.
pub fn outcome_after_retries<'a>(
    attempts: impl IntoIterator<Item = (u64, &'a str)>,
) -> Result<&'static str, String> {
    let mut expected_attempt = 1;
    let mut saw_pass = false;
    let mut saw_failure = false;
    let mut saw_error = false;
    let mut saw_host_inapplicable = false;

    for (attempt, outcome) in attempts {
        if attempt > MAX_ATTEMPTS_PER_CELL {
            return Err(format!(
                "cell result attempt {attempt} exceeds the shared maximum of {MAX_ATTEMPTS_PER_CELL}"
            ));
        }
        if attempt != expected_attempt {
            return Err(format!(
                "cell result attempt {attempt} does not follow the preceding attempts; expected {expected_attempt}"
            ));
        }
        if saw_pass || saw_host_inapplicable {
            return Err(format!(
                "cell result attempt {attempt} follows terminal outcome {}",
                if saw_pass {
                    "PASS"
                } else {
                    "HOST-INAPPLICABLE"
                }
            ));
        }
        match outcome {
            "PASS" => saw_pass = true,
            "FAIL" => saw_failure = true,
            "ERROR" => saw_error = true,
            "HOST-INAPPLICABLE" => saw_host_inapplicable = true,
            other => {
                return Err(format!(
                    "cell result attempt {attempt} has unknown outcome {other:?}"
                ));
            }
        }
        expected_attempt += 1;
    }

    if expected_attempt == 1 {
        return Err("cell result has no attempts".into());
    }
    if saw_host_inapplicable && (saw_failure || saw_error) {
        return Err("HOST-INAPPLICABLE cannot be mixed with executed cell attempts".into());
    }
    Ok(if saw_pass {
        "PASS"
    } else if saw_failure {
        "FAIL"
    } else if saw_error {
        "ERROR"
    } else {
        "HOST-INAPPLICABLE"
    })
}

/// Return the framework result row that carries the outcome selected by
/// [`outcome_after_retries`]. All attempt rows remain in `results.jsonl`; this
/// row is the one used for the cell's JUnit and summary entry.
pub fn cell_result_after_retries(results: &[CellResult]) -> Result<&CellResult, String> {
    let outcome = outcome_after_retries(
        results
            .iter()
            .map(|result| (result.attempt, result.outcome.as_str())),
    )?;
    results
        .iter()
        .rev()
        .find(|result| result.outcome == outcome)
        .ok_or_else(|| format!("cell result history selected {outcome} without a matching row"))
}

/// Return both the selected result row and the number of attempts in its
/// validated history. The selected row keeps its own ordinal and artifact;
/// the separate count is what structured summaries report.
pub fn cell_result_and_attempts_after_retries(
    results: &[CellResult],
) -> Result<(&CellResult, u64), String> {
    let selected = cell_result_after_retries(results)?;
    let attempts = results
        .last()
        .map(|result| result.attempt)
        .ok_or_else(|| "cell result has no attempts".to_string())?;
    Ok((selected, attempts))
}

fn command_text(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot execute {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited {}", output.status));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} output is not UTF-8: {error}"))?
        .trim()
        .to_string();
    if value.is_empty() {
        return Err(format!("{program} returned an empty value"));
    }
    Ok(value)
}

pub fn prepare_test(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
) -> Result<Vec<String>, String> {
    let timeouts = cell_timeouts(context, cell)?;
    prepare_test_until(
        context,
        cell,
        dir,
        execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?,
        timeouts.wall_seconds,
    )
    .map(|(guest, _cpu_usage_usec)| guest)
}

fn prepare_test_until(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
    deadline: Instant,
    wall_timeout_seconds: u64,
) -> Result<(Vec<String>, u64), String> {
    prepare_dirs(&context.root, dir)?;
    let mut cpu_usage_usec = 0u64;
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
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "cc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
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
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "rustc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".sh") => {
            let path = context.root.join(program).to_string_lossy().into_owned();
            if !context.prebuilt {
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        &path,
                        &["--prepare".into()],
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
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
    if context.isolated_workdir.is_some() || supports_test_workdir(&cell.id.mode, backend) {
        resolve_repo_guest_args(&context.root, &mut guest);
    }
    Ok((guest, cpu_usage_usec))
}

fn resolve_repo_guest_args(root: &Path, argv: &mut [String]) {
    for arg in argv {
        let path = Path::new(arg);
        if path.is_absolute() || arg == "." || arg == ".." {
            continue;
        }
        let resolved = root.join(path);
        let looks_like_repo_path = arg.starts_with("./") || arg.contains('/') || resolved.is_file();
        if looks_like_repo_path && resolved.exists() {
            *arg = resolved.to_string_lossy().into_owned();
        }
    }
}

/// What the preparation child actually said, stderr first.
///
/// The child's output is already captured to `prepare.stderr` / `prepare.stdout`;
/// this is the only thing that carries it back to the caller. It matters beyond
/// readability: validate classifies an environmentally-blocked gate by reading
/// the FAILING NODE's own output region and nothing else, deliberately, so that
/// an unrelated concurrent host denial cannot excuse a real red. A build step
/// that swallows its compiler's stderr therefore hides the very evidence the
/// classifier is looking for, and a host denial gets recorded as a product
/// failure. Measured 2026-08-17 in one cold run: `build.manifest_guests`
/// reported three guests as "missing or not executable" with no diagnostic while
/// the jailer had denied their `cc`/`ld`, and `build.runtime_release` in the SAME
/// run propagated its banner, was classified `bpfjailer-banner`, and was retried.
fn preparation_diagnostic(captures: &Path) -> String {
    fs::read_to_string(captures.join("prepare.stderr"))
        .ok()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(captures.join("prepare.stdout"))
                .ok()
                .filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default()
}

/// Append a captured diagnostic to `message`, or say plainly that there was none.
///
/// The empty case is spelled out rather than left blank: "no output was captured"
/// is a different and much more suspicious fact than a compiler error, and a
/// reader who cannot tell them apart cannot tell a denial from a broken guest.
fn with_diagnostic(message: String, captures: &Path) -> String {
    let diagnostic = preparation_diagnostic(captures);
    if diagnostic.is_empty() {
        format!(
            "{message}\n  (no output was captured in {}; the preparation command produced none)",
            captures.display()
        )
    } else {
        format!("{message}\n{diagnostic}")
    }
}

fn remaining_cell_time(deadline: Instant) -> Duration {
    remaining_cell_time_at(deadline, Instant::now())
}

fn remaining_cell_seconds(deadline: Instant) -> u64 {
    let remaining = remaining_cell_time(deadline);
    if remaining.is_zero() {
        return 1;
    }
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() != 0))
}

fn remaining_cell_time_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

fn cell_timeouts(
    context: &RunContext,
    cell: &SelectedCell,
) -> Result<ResolvedTestTimeouts, String> {
    resolve_test_timeouts(
        cell.cpu_timeout_seconds,
        cell.timeout_seconds,
        context.timeout_multipliers,
    )
}

fn execution_deadline_after_preparation(
    prepared_at: Instant,
    wall_timeout_seconds: u64,
) -> Result<Instant, String> {
    prepared_at
        .checked_add(Duration::from_secs(wall_timeout_seconds))
        .ok_or_else(|| format!("wall timeout {wall_timeout_seconds}s exceeds clock range"))
}

fn require_executable_program(path: &Path, captures: &Path) -> Result<(), String> {
    let executable = path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if executable {
        return Ok(());
    }
    Err(with_diagnostic(
        format!(
            "compiled guest is missing or not executable: {}",
            path.display()
        ),
        captures,
    ))
}

fn run_preparation(
    context: &RunContext,
    dir: &Path,
    program: &str,
    args: &[String],
    deadline: Instant,
    cell_timeout_seconds: u64,
) -> Result<u64, String> {
    let captures = dir.join("captures");
    if remaining_cell_time(deadline).is_zero() {
        return Err(with_diagnostic(
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation"),
            &captures,
        ));
    }
    let output = execute_process(
        &context.root,
        program,
        args,
        &preparation_env(dir),
        &captures.join("prepare.stdout"),
        &captures.join("prepare.stderr"),
        (deadline, None),
    )?;
    if output.timeout.is_some() || !output.status.success() {
        // Carry the child's own words back. This used to return the bare sentence
        // and drop `prepare.stderr` on the floor, which turned every denied or
        // broken compile into the same uninformative line.
        let how = if output.timeout.is_some() {
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation")
        } else {
            match output.status.code() {
                Some(code) => format!("exited {code}"),
                None => "was killed by a signal".to_string(),
            }
        };
        return Err(with_diagnostic(
            format!("fixture preparation failed for {program}: {how}"),
            &captures,
        ));
    }
    Ok(output.cpu_usage_usec)
}

pub fn build_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: &str,
    seed: Option<i64>,
    timeout_seconds: u64,
) -> Result<CellRunSpec, String> {
    // The label is used in verdict, log, evidence, and capture paths. Reject
    // traversal and multi-component values before any of those paths are
    // created. The ordinary host-bound workdir uses the same guard; the
    // hermetic tmpfs path does not remove this path-safety property.
    let fixed_workdir_source = fixed_workdir_source_for_attempt(&dir, attempt)?;
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode_recipe = &cell.test.modes[&cell.id.mode];
    let mut env = execution_cell_env(context, &dir, cell.id.mode != "naked");
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
    // The explicit hermetic path is available only where the launched backend
    // can honour a guest working directory. Replay is included there because
    // `record start` accepts the same mount and workdir arguments as `run`.
    let supports_test_workdir = supports_test_workdir(&cell.id.mode, backend);
    if context.isolated_workdir.is_some() && !supports_test_workdir {
        return Err(format!(
            "hermetic validation cannot isolate a {} cell on the {backend} backend at /test",
            cell.id.mode
        ));
    }
    let bound_workdir_source = (matches!(cell.id.mode.as_str(), "verify" | "chaos" | "custom")
        && backend != "dbt")
        .then_some(fixed_workdir_source.as_path());
    let isolated = context.isolated_workdir.is_some();
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
            if mode_recipe.compare_io_buffers == Some(false) {
                argv.push("--no-detlog-io-buffers".into());
            }
            if mode_recipe.rcb_time == Some(false) {
                argv.push("--no-rcb-time".into());
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
            append_execution_root_args(
                &mut argv,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
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
            if isolated {
                argv.push("--base-env=minimal".into());
            }
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
                timeout_seconds.to_string(),
            ]);
            append_execution_root_args(
                &mut argv,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
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
            append_execution_root_args(
                &mut argv,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
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
            if isolated {
                require_minimal_base_env(&mut argv)?;
                append_guest_env_args(&mut argv, &env, true);
            } else {
                append_scheduled_jobs_env_arg(&mut argv, &env);
            }
            append_execution_root_args(
                &mut argv,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
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
        timeout_seconds,
        verdict_path,
        verification_log_dir,
        sabre_path_evidence,
        cell_dir: dir,
        attempt: attempt.into(),
        fixed_workdir_source,
    })
}

pub fn execute_spec(spec: &CellRunSpec) -> Result<AttemptResult, String> {
    execute_spec_until(
        spec,
        execution_deadline_after_preparation(Instant::now(), spec.timeout_seconds)?,
        spec.timeout_seconds,
        spec.timeout_seconds,
        None,
    )
}

fn execute_spec_until(
    spec: &CellRunSpec,
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
) -> Result<AttemptResult, String> {
    // The attempt label comes from the spec rather than a parallel parameter.
    // `build_spec` already stored it, and every caller passed the same value to
    // both, so a separate argument was a second definition that could go stale
    // against the workdir and capture paths built from the first.
    let index = spec.attempt.as_str();
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
    // The ordinary-host fallback binds this exact per-attempt directory at
    // /tmp/test. The explicit hermetic path mounts a fresh tmpfs at /test and
    // ignores the directory, but keeping the reset unconditional preserves the
    // fallback and makes switching paths a pure precedence decision.
    let workdir = &spec.fixed_workdir_source;
    if workdir.exists() {
        fs::remove_dir_all(workdir)
            .map_err(|e| format!("cannot reset {}: {e}", workdir.display()))?;
    }
    fs::create_dir_all(workdir).map_err(|e| format!("cannot create {}: {e}", workdir.display()))?;
    let captures = spec.cell_dir.join("captures");
    fs::create_dir_all(&captures).map_err(|e| e.to_string())?;
    let stdout_path = captures.join(format!("{}-{index}.stdout", spec.id.mode));
    let stderr_path = captures.join(format!("{}-{index}.stderr", spec.id.mode));
    let started = Instant::now();
    let remaining = remaining_cell_time(deadline);
    let exhausted = if remaining.is_zero() {
        Some(ProcessTimeout::Wall)
    } else if remaining_cpu_usec == Some(0) {
        Some(ProcessTimeout::Cpu)
    } else {
        None
    };
    if let Some(timeout) = exhausted {
        fs::write(&stdout_path, b"").map_err(|e| e.to_string())?;
        fs::write(&stderr_path, b"").map_err(|e| e.to_string())?;
        return Ok(cell_timeout_attempt(
            spec,
            cpu_timeout_seconds,
            wall_timeout_seconds,
            started.elapsed(),
            timeout,
        ));
    }
    let output = execute_process(
        &spec.cwd,
        &spec.argv[0],
        &spec.argv[1..],
        &spec.env,
        &stdout_path,
        &stderr_path,
        (deadline, remaining_cpu_usec),
    )?;
    if spec.id.mode == "verify" && spec.id.backend.as_deref() == Some("ptrace") {
        if let Some(directory) = &spec.verification_log_dir {
            normalize_ptrace_golden(&spec.argv[0], directory)?;
        }
    }
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let mut outcome = if output.timeout.is_some() || !output.status.success() {
        "FAIL"
    } else {
        "PASS"
    }
    .to_string();
    let mut reason = output
        .timeout
        .map(|kind| kind.reason(cpu_timeout_seconds, wall_timeout_seconds));
    let mut error_kind = None;
    let failure_class_line = stderr.lines().next().unwrap_or_default();
    let launch_refusal = spec.id.mode != "naked"
        && !output.status.success()
        && stdout.is_empty()
        && matches!(
            failure_class_line,
            "HERMIT_INTERNAL_FAILURE class=guest-program-not-found"
                | "HERMIT_INTERNAL_FAILURE class=guest-program-not-executable"
        );
    if launch_refusal {
        outcome = "ERROR".into();
        error_kind = Some("guest-launch-refused".into());
        reason = Some(format!(
            "guest launch refused before execution: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown launch refusal")
        ));
    }
    // A runner that cannot start the requested backend and a backend that ran
    // but produced no canonical comparison are different failures. Keep both
    // visible as ERROR, but give the pre-guest availability refusal its own
    // machine-readable kind so sweeps cannot count it as a product failure.
    // The first line is the producer-owned class emitted before human prose.
    // Matching a broader nonzero exit would hide real regressions.
    // KVM currently bypasses ensure_available, so its availability failures do
    // not enter this class.
    let unavailable_class = spec.id.backend.as_deref().map(|backend| {
        format!("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend={backend}")
    });
    let backend_unavailable = spec.id.mode != "naked"
        && !launch_refusal
        && output.timeout.is_none()
        && !output.status.success()
        && stdout.is_empty()
        && unavailable_class
            .as_deref()
            .is_some_and(|class| failure_class_line == class);
    if backend_unavailable {
        outcome = "ERROR".into();
        error_kind = Some("backend-unavailable".into());
        reason = Some(format!(
            "backend unavailable on this runner, so nothing was measured: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown backend unavailability")
        ));
    }
    // A producer class that cannot satisfy the requested backend or execution
    // shape is unavailable evidence, not a product crash. In particular, the
    // human error line must never override a mismatched class into FAIL.
    let invalid_backend_evidence = output.timeout.is_none()
        && !output.status.success()
        && !backend_unavailable
        && failure_class_line
            .starts_with("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=");
    if invalid_backend_evidence {
        outcome = "ERROR".into();
        error_kind = Some("invalid-backend-evidence".into());
        reason = Some(format!(
            "backend availability evidence does not match this attempt: {failure_class_line}"
        ));
    }
    // The producer did write a class, but it did not establish a more specific
    // result. Keep that absence as no-result instead of letting the following
    // English line manufacture a product failure.
    let unclassified_internal_failure = output.timeout.is_none()
        && !output.status.success()
        && !launch_refusal
        && !backend_unavailable
        && !invalid_backend_evidence
        && failure_class_line == "HERMIT_INTERNAL_FAILURE class=cli-error";
    if unclassified_internal_failure {
        outcome = "ERROR".into();
        error_kind = Some("incomplete-verification-evidence".into());
        reason = Some("Hermit reported cli-error without a more specific result".into());
    }
    let producer_failure_classified = launch_refusal
        || backend_unavailable
        || invalid_backend_evidence
        || unclassified_internal_failure;
    let mut report_json = None;
    let mut report_sha = None;
    let mut runtime = None;
    let mut first_divergent_scheduler_turn = None;
    let mut first_divergent_virtual_nanoseconds = None;
    let mut first_divergent_record = None;
    let mut first_divergent_syscall = None;
    let mut first_divergent_left_message = None;
    let mut first_divergent_right_message = None;
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
                        runtime = report.runtime.clone();
                        // Recorded BEFORE the classification chain below,
                        // because where the divergence began is a fact about
                        // the report rather than a consequence of how the
                        // attempt is classified -- a FAIL and an ERROR can both
                        // carry a located divergence.
                        //
                        // Guarded on `launch_refusal` for the same reason the
                        // arm below refuses to reclassify: no guest was ever
                        // created, so any report present cannot describe an
                        // executed guest's divergence position.
                        if !launch_refusal && !backend_unavailable {
                            first_divergent_scheduler_turn = report.first_divergent_scheduler_turn;
                            first_divergent_virtual_nanoseconds =
                                report.first_divergent_virtual_nanoseconds;
                            first_divergent_record = report.first_divergent_record;
                            first_divergent_syscall = report.first_divergent_syscall;
                            first_divergent_left_message =
                                report.first_divergent_left_message.clone();
                            first_divergent_right_message =
                                report.first_divergent_right_message.clone();
                        }
                        if producer_failure_classified {
                            // The producer's classified failure, or a
                            // contradiction in that evidence, cannot be
                            // superseded by a pre-stamped or unrelated report.
                        } else if report.verdict == Verdict::InfrastructureError {
                            let comparison_error = report
                                .comparison
                                .as_ref()
                                .and_then(|_| report.require_canonical_comparison().err());
                            if let Some(error) = comparison_error {
                                outcome = "ERROR".into();
                                error_kind = Some("incomplete-verification-evidence".into());
                                reason = Some(error);
                            } else {
                                outcome = "ERROR".into();
                                error_kind = Some("infrastructure".into());
                                reason = Some(match report.infrastructure_error.as_ref() {
                                    Some(InfrastructureError::SkidOvershoot { count }) => format!(
                                        "verification recorded {count} HERMIT_SKID_OVERSHOOT report(s)"
                                    ),
                                    None => unreachable!(
                                        "typed report parser requires an infrastructure error"
                                    ),
                                });
                            }
                        } else if report.verdict == Verdict::NoResult
                            && matches!(
                                report.no_result_reason,
                                Some(
                                    crate::canonical_verdict::NoResultReason::FirstRunRejected { .. }
                                ) | None
                            )
                            && output.timeout.is_none()
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
                        } else if output.timeout.is_none()
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
                    Err(error) if !producer_failure_classified => {
                        outcome = "ERROR".into();
                        error_kind = Some("incomplete-verification-evidence".into());
                        reason = Some(format!("verification report is unreadable: {error}"));
                    }
                    Err(_) => {}
                }
            }
            Err(_error) if producer_failure_classified => {}
            Err(error) => {
                outcome = "ERROR".into();
                error_kind = Some("incomplete-verification-evidence".into());
                reason = Some(format!("verification report is missing: {error}"));
            }
        }
    }
    if let Some(timeout) = output.timeout {
        error_kind = Some(timeout.error_kind().into());
        reason = Some(timeout.reason(cpu_timeout_seconds, wall_timeout_seconds));
    }
    Ok(AttemptResult {
        index: index.into(),
        outcome,
        error_kind,
        status: output.status.code(),
        signal: std::os::unix::process::ExitStatusExt::signal(&output.status),
        timed_out: output.timeout.is_some(),
        duration_ms: started.elapsed().as_millis(),
        cpu_usage_usec: Some(output.cpu_usage_usec),
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
        retained_verify_log: None,
        runtime,
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessTimeout {
    Wall,
    Cpu,
}

impl ProcessTimeout {
    fn error_kind(self) -> &'static str {
        match self {
            Self::Wall => "wall-timeout",
            Self::Cpu => "cpu-timeout",
        }
    }

    fn reason(self, cpu_seconds: u64, wall_seconds: u64) -> String {
        match self {
            Self::Wall => {
                format!("cell exceeded {wall_seconds} wall s backstop ({cpu_seconds} s CPU budget)")
            }
            Self::Cpu => format!("cell exceeded {cpu_seconds} CPU s"),
        }
    }
}

struct ProcessOutput {
    status: ExitStatus,
    timeout: Option<ProcessTimeout>,
    cpu_usage_usec: u64,
}

struct ProcessLimits {
    deadline: Instant,
    cpu_budget_usec: Option<u64>,
    cpu_poll_interval: Duration,
}

/// Add two complete CPU measurements, refusing missing or overflowing input.
pub fn checked_add_cpu_usage(total: Option<u64>, usage: Option<u64>) -> Option<u64> {
    total.and_then(|total| usage.and_then(|usage| total.checked_add(usage)))
}

fn rusage_cpu_usage_usec(usage: &libc::rusage) -> Result<u64, String> {
    fn timeval_usec(value: libc::timeval) -> Option<u64> {
        let seconds = u64::try_from(value.tv_sec).ok()?;
        let microseconds = u64::try_from(value.tv_usec).ok()?;
        (microseconds < 1_000_000)
            .then_some(seconds.checked_mul(1_000_000)?.checked_add(microseconds)?)
    }

    let user = timeval_usec(usage.ru_utime)
        .ok_or_else(|| "wait4 returned an invalid user CPU duration".to_string())?;
    let system = timeval_usec(usage.ru_stime)
        .ok_or_else(|| "wait4 returned an invalid system CPU duration".to_string())?;
    user.checked_add(system)
        .ok_or_else(|| "wait4 CPU usage overflowed u64".to_string())
}

fn wait4_process(pid: u32, options: libc::c_int) -> Result<Option<(ExitStatus, u64)>, String> {
    loop {
        let mut status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited = unsafe { libc::wait4(pid as libc::pid_t, &mut status, options, &mut usage) };
        if waited == 0 {
            return Ok(None);
        }
        if waited == pid as libc::pid_t {
            return Ok(Some((
                ExitStatus::from_raw(status),
                rusage_cpu_usage_usec(&usage)?,
            )));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!("wait4({pid}) failed: {error}"));
    }
}

/// Live user-plus-system CPU consumed by one process group, including CPU from
/// children already reaped by a still-live member.
///
/// `wait4` is the authoritative final measurement, but it becomes available
/// only after the process exits and therefore cannot enforce a budget. Reuse
/// dagrun's process-group reader: its one short-lived snapshot is shared by all
/// concurrent cells, rather than making every cell enumerate the host's entire
/// `/proc` tree on every poll.
fn process_group_cpu_usage_usec(pgid: u32) -> Result<Option<u64>, String> {
    dagrun::proccpu::subtree_cpu_seconds(pgid)
        .map(|seconds| {
            let usec = seconds * 1_000_000.0;
            if !usec.is_finite() || usec.is_sign_negative() || usec > u64::MAX as f64 {
                return Err(format!(
                    "process group {pgid} returned invalid live CPU seconds {seconds}"
                ));
            }
            Ok(usec as u64)
        })
        .transpose()
}

fn stop_process_group(pid: u32) -> Result<(ExitStatus, u64), String> {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = wait4_process(pid, libc::WNOHANG)? {
            return Ok(result);
        }
        if Instant::now() >= grace {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    wait4_process(pid, 0)?.ok_or_else(|| format!("blocking wait4({pid}) returned no child"))
}

fn execute_process(
    cwd: &Path,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdout: &Path,
    stderr: &Path,
    limits: (Instant, Option<u64>),
) -> Result<ProcessOutput, String> {
    execute_process_with_cpu_poll_interval(
        cwd,
        program,
        args,
        env,
        stdout,
        stderr,
        ProcessLimits {
            deadline: limits.0,
            cpu_budget_usec: limits.1,
            cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
        },
    )
}

fn execute_process_with_cpu_poll_interval(
    cwd: &Path,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdout: &Path,
    stderr: &Path,
    limits: ProcessLimits,
) -> Result<ProcessOutput, String> {
    let ProcessLimits {
        deadline,
        cpu_budget_usec,
        cpu_poll_interval,
    } = limits;
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
    let child = command
        .spawn()
        .map_err(|e| format!("cannot execute {program}: {e}"))?;
    let pid = child.id();
    let mut next_cpu_poll = Instant::now() + cpu_poll_interval;
    let mut cpu_accounting_missing_since = None;
    loop {
        if let Some((status, cpu_usage_usec)) = wait4_process(pid, libc::WNOHANG)? {
            let timeout = cpu_budget_usec
                .filter(|limit| cpu_usage_usec >= *limit)
                .map(|_| ProcessTimeout::Cpu);
            return Ok(ProcessOutput {
                status,
                timeout,
                cpu_usage_usec,
            });
        }
        let now = Instant::now();
        let (timeout, observed_cpu_usec) = if now >= deadline {
            (Some(ProcessTimeout::Wall), None)
        } else if let Some(limit) = cpu_budget_usec.filter(|_| now >= next_cpu_poll) {
            next_cpu_poll = now + cpu_poll_interval;
            match process_group_cpu_usage_usec(pid)? {
                Some(used) => {
                    cpu_accounting_missing_since = None;
                    ((used >= limit).then_some(ProcessTimeout::Cpu), Some(used))
                }
                None => {
                    let missing_since = cpu_accounting_missing_since.get_or_insert(now);
                    if now.duration_since(*missing_since) >= CELL_CPU_ACCOUNTING_GRACE {
                        let _ = stop_process_group(pid);
                        return Err(format!(
                            "cannot measure live CPU for process group {pid}; stopped it rather than silently disabling its CPU budget"
                        ));
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        if let Some(timeout) = timeout {
            let (status, final_cpu_usage_usec) = stop_process_group(pid)?;
            let cpu_usage_usec = observed_cpu_usec.map_or(final_cpu_usage_usec, |observed| {
                observed.max(final_cpu_usage_usec)
            });
            return Ok(ProcessOutput {
                status,
                timeout: Some(timeout),
                cpu_usage_usec,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn cell_timeout_attempt(
    spec: &CellRunSpec,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    duration: Duration,
    timeout: ProcessTimeout,
) -> AttemptResult {
    let index = spec.attempt.as_str();
    // A post-preparation bound was already exhausted before this attempt could
    // start, so no process exists from which to recover an exit status or signal.
    // Record that fact with the
    // same typed NotRun report Hermit uses before its first guest run.  A
    // synthetic exit code would make a scheduler decision look like a process
    // result, while a bare FAIL makes the retained attempt unreadable to every
    // typed consumer.
    let report = serde_json::to_string(&VerificationReport::no_result())
        .expect("the typed NotRun report must serialize");
    let report_sha256 = hex_digest(report.as_bytes());
    AttemptResult {
        index: index.into(),
        outcome: "ERROR".into(),
        error_kind: Some(timeout.error_kind().into()),
        status: None,
        signal: None,
        timed_out: true,
        duration_ms: duration.as_millis(),
        cpu_usage_usec: None,
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout: String::new(),
        stderr: String::new(),
        verification_report: Some(report),
        verification_report_sha256: Some(report_sha256),
        retained_verify_log: None,
        runtime: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        sabre_path_evidence: None,
        sabre_path_evidence_sha256: None,
        reason: Some(format!(
            "{} before attempt {index} started",
            timeout.reason(cpu_timeout_seconds, wall_timeout_seconds)
        )),
    }
}

/// The one definition of where a cell's retained evidence lives.
///
/// Was duplicated verbatim in `run_cell` and `infrastructure_error_result`.
/// Two copies of a path formula is how a log line ends up pointing at a
/// directory that does not exist, so surfacing the path made collapsing them a
/// prerequisite rather than a tidy-up.
fn cell_artifact_dir(context: &RunContext, cell: &SelectedCell) -> PathBuf {
    let base_slug = format!(
        "{}-{}-{}",
        cell.id.test.replace('/', "-"),
        cell.id.mode,
        cell.id.backend.as_deref().unwrap_or("none")
    );
    let slug = if context.attempt == 1 {
        base_slug
    } else {
        format!("{base_slug}-attempt-{}", context.attempt)
    };
    context
        .result_root
        .join("runs")
        .join(&context.run_id)
        .join(slug)
}

fn verification_verdict(attempt: &AttemptResult) -> Option<Verdict> {
    let report = attempt.verification_report.as_deref()?;
    VerificationReport::from_json_slice(report.as_bytes())
        .ok()
        .map(|report| report.verdict)
}

fn observed_result(
    mode: &str,
    outcome: &str,
    attempts: &[AttemptResult],
    error_kind: Option<&str>,
) -> Option<ObservedResult> {
    if outcome == "PASS" {
        return Some(ObservedResult::Pass);
    }
    // A later framework or evidence failure decides whether this cell produced
    // a usable product result. An earlier attempt can still retain a located
    // divergence in `attempts`, but it must not make a terminal infrastructure
    // or no-result outcome look product-attributed.
    if non_product_failure_class(error_kind).is_some() {
        return None;
    }
    if attempts.iter().any(|attempt| attempt.timed_out) {
        return Some(ObservedResult::Timeout);
    }
    if mode == "verify"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::DeterminismFailure);
    }
    if mode == "replay"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::ReplayFailure);
    }
    (outcome == "FAIL").then_some(ObservedResult::CrashError)
}

fn non_product_failure_class(error_kind: Option<&str>) -> Option<FailureClass> {
    match error_kind {
        Some("guest-launch-refused" | "backend-unavailable") => {
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        }
        Some("infrastructure" | "result-publication") => {
            Some(FailureClass::UnderstoodInfrastructureFailure)
        }
        Some("incomplete-verification-evidence" | "invalid-backend-evidence") => {
            Some(FailureClass::NoResult)
        }
        _ => None,
    }
}

fn failure_class(
    outcome: &str,
    result: Option<ObservedResult>,
    error_kind: Option<&str>,
) -> Option<FailureClass> {
    if outcome == "PASS" {
        return None;
    }
    if let Some(failure_class) = non_product_failure_class(error_kind) {
        return Some(failure_class);
    }
    if let Some(result) = result {
        if let Some(failure_class) = result.failure_class() {
            return Some(failure_class);
        }
    }
    if outcome == "HOST-INAPPLICABLE" {
        Some(FailureClass::UnderstoodPrerequisiteFailure)
    } else {
        Some(FailureClass::NoResult)
    }
}

/// Apply a failed chaos population assertion without erasing an attempt-level
/// infrastructure result. A timeout or unavailable verification report means
/// the population was not fully measured; it is not a product failure merely
/// because the incomplete sample also missed `min_passes`.
fn apply_failed_chaos_assertion(
    outcome: &mut String,
    reason: &mut Option<String>,
    assertion_reason: String,
) {
    if outcome != "ERROR" {
        *outcome = "FAIL".into();
        *reason = Some(assertion_reason);
    }
}

pub fn run_cell(context: &RunContext, cell: &SelectedCell) -> Result<CellResult, String> {
    let dir = cell_artifact_dir(context, cell);
    let started = Instant::now();
    let timeouts = cell_timeouts(context, cell)?;
    let preparation_deadline =
        execution_deadline_after_preparation(started, timeouts.wall_seconds)?;
    let binary_before = fs::read(&context.hermit_bin)
        .ok()
        .map(|bytes| hex_digest(&bytes));
    let (guest, preparation_cpu_usage_usec) = prepare_test_until(
        context,
        cell,
        &dir,
        preparation_deadline,
        timeouts.wall_seconds,
    )?;
    // Fixture preparation keeps its scaled wall-clock guard above. Post-preparation
    // execution uses its separately measured bound as an aggregate CPU-second budget
    // across all attempts/seeds, so time spent descheduled by a busy host cannot
    // turn a valid run into no_result. The wider wall deadline is retained only
    // for a wedged process that consumes no CPU and therefore cannot reach the
    // primary bound.
    let deadline = execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?;
    let execution_cpu_budget_usec = timeouts.cpu_seconds.saturating_mul(1_000_000);
    let mut execution_cpu_usage_usec = 0u64;
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
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
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
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
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
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        _ => {
            let spec = build_spec(
                context,
                cell,
                dir.clone(),
                guest.clone(),
                "1",
                None,
                remaining_cell_seconds(deadline),
            )?;
            attempts.push(execute_observed_until(
                &spec,
                &cell.test.observation,
                &dir,
                deadline,
                timeouts.cpu_seconds,
                timeouts.wall_seconds,
                Some(execution_cpu_budget_usec),
            )?);
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
    let position = cell_divergence_position(&attempts);
    let first_divergent_scheduler_turn = position.scheduler_turn;
    let first_divergent_virtual_nanoseconds = position.virtual_nanoseconds;
    let first_divergent_record = position.record;
    let first_divergent_syscall = position.syscall;
    let first_divergent_left_message = position.left_message;
    let first_divergent_right_message = position.right_message;
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
            apply_failed_chaos_assertion(
                &mut outcome,
                &mut reason,
                format!(
                    "chaos distinct={distinct} passes={pass_count} failures={failure_count} normalized_entropy={normalized_entropy:.4}"
                ),
            );
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
        .unwrap_or_else(|| execution_cell_env(context, &dir, cell.id.mode != "naked"));
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
        error_kind = Some("infrastructure".into());
        reason = Some("Hermit binary changed while the cell was executing".into());
    }
    let result = observed_result(&cell.id.mode, &outcome, &attempts, error_kind.as_deref());
    let failure_class = failure_class(&outcome, result, error_kind.as_deref());
    let cpu_usage_usec = attempts
        .iter()
        .try_fold(preparation_cpu_usage_usec, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
    Ok(CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
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
        result,
        failure_class,
        error_kind,
        timeout_seconds: timeouts.wall_seconds,
        execution_cpu_timeout_seconds: Some(timeouts.cpu_seconds),
        execution_wall_timeout_seconds: Some(timeouts.wall_seconds),
        duration_ms: Some(started.elapsed().as_millis()),
        cpu_usage_usec,
        runtime: attempts.iter().find_map(|attempt| attempt.runtime.clone()),
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: literal_argv.iter().skip(1).cloned().collect(),
        argv: literal_argv,
        guest_argv: literal_guest_argv,
        env: literal_env,
        cwd: literal_cwd,
        shell_command: literal_shell_command,
        relaxations: cell_relaxations(cell),
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
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
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
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
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
        result: None,
        failure_class: Some(FailureClass::UnderstoodInfrastructureFailure),
        error_kind: Some("infrastructure".into()),
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: execution_cell_env(context, &dir, cell.id.mode != "naked"),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        // No attempt ran, so there is no divergence position to report. `None`
        // here means "never measured", which is the same value a clean run
        // produces -- see the note in the observation fold about those two
        // being indistinguishable in this field alone. The surrounding row is
        // what distinguishes them: this one carries an infrastructure error
        // kind and an empty `attempts`.
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Publish a typed result for a cell that provably cannot execute on this host.
///
/// No argv, environment, binary identity, or attempt is invented: the cell did
/// not run. Its normal identity and source digest remain present so the row
/// stays in the selected denominator and downstream completeness checks can
/// distinguish explicit inapplicability from missing output.
pub fn host_inapplicable_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: None,
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome: "HOST-INAPPLICABLE".into(),
        result: None,
        failure_class: Some(FailureClass::UnderstoodPrerequisiteFailure),
        error_kind: None,
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: None,
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: BTreeMap::new(),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Reduce per-attempt divergence evidence to the cell-level fields.
///
/// Same first-attempt rule as `reason`, and resolved PER COORDINATE rather than
/// per attempt: a report that located a turn but no virtual nanosecond still
/// contributes the turn. Picking one attempt and taking both of its values
/// would silently drop the other coordinate.
///
/// Deliberately NOT a min across attempts. A min over the attempts of one run
/// is a one-run aggregate that would read like the cross-run `ObservedRange`
/// earliest/latest in ci/compat-envelope/scorecard.rs without having a sample
/// behind it, and this project has repeatedly been bitten by numbers that look
/// like a measurement and are not.
/// The divergence coordinates and compared messages of one cell.
///
/// A named struct rather than a tuple, deliberately: the four numeric values
/// are otherwise indistinguishable, and this project has already had a bare
/// ordinal read against the wrong axis. Each numeric field is a different
/// keyspace -- measured on one real divergence, the same event was record 98,
/// syscall 37 and scheduler turn 4. The messages retain the compared event
/// content without treating record position as identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DivergencePosition {
    pub scheduler_turn: Option<u64>,
    pub virtual_nanoseconds: Option<u64>,
    pub record: Option<u64>,
    pub syscall: Option<u64>,
    pub left_message: Option<String>,
    pub right_message: Option<String>,
}

fn cell_divergence_position(attempts: &[AttemptResult]) -> DivergencePosition {
    let messages = attempts.iter().find(|attempt| {
        attempt.first_divergent_left_message.is_some()
            || attempt.first_divergent_right_message.is_some()
    });
    DivergencePosition {
        scheduler_turn: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_scheduler_turn),
        virtual_nanoseconds: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_virtual_nanoseconds),
        record: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_record),
        syscall: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_syscall),
        left_message: messages.and_then(|attempt| attempt.first_divergent_left_message.clone()),
        right_message: messages.and_then(|attempt| attempt.first_divergent_right_message.clone()),
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
    let mut executions = Vec::<PathEvidence>::new();
    for line in attempts
        .iter()
        .flat_map(|attempt| attempt.sabre_path_evidence.as_deref().unwrap_or("").lines())
    {
        let value = serde_json::from_str::<JsonValue>(line)
            .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?;
        if value.get("schema").and_then(JsonValue::as_u64) != Some(u64::from(PathEvidence::SCHEMA))
        {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        executions.push(
            serde_json::from_value::<PathEvidence>(value)
                .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?,
        );
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
    let mut guest_rpc_observed = complete;
    let mut ptrace_fallback_sites = 0usize;
    let mut trusted_shared_object_sites = 0usize;
    let mut trusted_shared_objects = BTreeSet::new();
    for row in &executions {
        // Keep this pattern exhaustive. Adding a producer field must stop this
        // consumer at compile time until its meaning is handled here.
        let PathEvidence {
            schema,
            guest_rpc_observed: observed,
            ptrace_fallback_sites: fallback_sites,
            trusted_shared_object_sites: shared_object_sites,
            trusted_shared_objects: shared_objects,
        } = row;
        if *schema != PathEvidence::SCHEMA {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        guest_rpc_observed &= *observed;
        ptrace_fallback_sites += *fallback_sites;
        trusted_shared_object_sites += *shared_object_sites;
        trusted_shared_objects.extend(shared_objects.iter().cloned());
    }
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

fn nearest_existing_directory(path: &Path) -> Result<PathBuf, String> {
    let mut candidate = path.to_owned();
    loop {
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "stable result root {} is not a non-symlink directory",
                    candidate.display()
                ));
            }
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidate = candidate
                    .parent()
                    .ok_or_else(|| {
                        format!(
                            "cannot find an existing directory above result path {}",
                            path.display()
                        )
                    })?
                    .to_owned();
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect prospective result directory {}: {error}",
                    candidate.display()
                ));
            }
        }
    }
}

fn prepare_result_path_from_root_with_failure(
    stable_root: &Path,
    path: &Path,
    directory_sync_failure_at: Option<usize>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require_plain_directory(stable_root, "stable result root")?;
    let relative_parent = parent.strip_prefix(stable_root).map_err(|_| {
        format!(
            "result path {} is outside stable result root {}",
            path.display(),
            stable_root.display()
        )
    })?;
    if relative_parent
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "result directory {} is not a normal path below stable root {}",
            parent.display(),
            stable_root.display()
        ));
    }
    let mut current = stable_root.to_owned();
    for component in relative_parent.components() {
        let Component::Normal(component) = component else {
            unreachable!("result parent components were checked above")
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "result directory {} is not a non-symlink directory",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| {
                    format!(
                        "cannot create result directory {}: {error}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect result directory {}: {error}",
                    current.display()
                ));
            }
        }
    }
    sync_relative_directory_chain_with_failure(
        stable_root,
        parent,
        "result directory",
        directory_sync_failure_at,
    )?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!(
            "result path {} is not a regular file",
            path.display()
        ));
    }
    file.sync_all().map_err(|e| e.to_string())?;
    sync_plain_directory(parent, "result directory after result-file creation")
}

/// Create and durably sync a result file from a caller-established directory.
pub fn prepare_result_path_from_root(stable_root: &Path, path: &Path) -> Result<(), String> {
    prepare_result_path_from_root_with_failure(stable_root, path, None)
}

pub fn prepare_result_path(path: &Path) -> Result<(), String> {
    let absolute_path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve relative result path: {error}"))?
            .join(path)
    };
    let parent = absolute_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stable_root = nearest_existing_directory(parent)?;
    prepare_result_path_from_root(&stable_root, &absolute_path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultPublicationFailurePoint {
    TemporaryWrite,
    TemporaryFileSync,
    BeforeRename,
    ParentDirectorySync,
}

#[derive(Debug)]
struct ResultPublicationFailure {
    message: String,
    descriptor_visible: bool,
}

fn read_existing_result(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot open result file {}: {error}",
                path.display()
            ));
        }
    };
    if !file
        .metadata()
        .map_err(|error| format!("cannot inspect result file {}: {error}", path.display()))?
        .is_file()
    {
        return Err(format!(
            "result path {} is not a regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read result file {}: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn write_atomic_file(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".result-publication.")
        .suffix(".staging")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "cannot create temporary {description} in {}: {error}",
                parent.display()
            )
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("cannot write temporary {description}: {error}"))?;
    temporary.persist(path).map_err(|error| {
        format!(
            "cannot atomically publish {description} {}: {}",
            path.display(),
            error.error
        )
    })?;
    sync_plain_directory(parent, &format!("{description} parent directory"))
}

fn restore_previous_result(path: &Path, previous: Option<&[u8]>) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match previous {
        Some(bytes) => write_atomic_file(path, bytes, "previous result file"),
        None => match fs::remove_file(path) {
            Ok(()) => sync_plain_directory(parent, "result directory after rollback"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "cannot remove new result file {} during rollback: {error}",
                path.display()
            )),
        },
    }
}

fn append_result_with_failure(
    path: &Path,
    result: &CellResult,
    failure: Option<ResultPublicationFailurePoint>,
    retained_file_already_verified: bool,
) -> Result<(), ResultPublicationFailure> {
    let unpublished = |message| ResultPublicationFailure {
        message,
        descriptor_visible: false,
    };
    result
        .require_current_classification()
        .map_err(unpublished)?;
    result
        .require_current_timeout_policy()
        .map_err(unpublished)?;
    if retained_file_already_verified {
        result
            .validate_retained_verify_log_binding()
            .map_err(unpublished)?;
    } else {
        result
            .validate_retained_verify_logs()
            .map_err(unpublished)?;
    }
    // A missing prerequisite means the cell did not execute. Keep the typed
    // value for the harness summary and JUnit skip, but do not publish a cell
    // row that downstream readers could count as an observation. The validate
    // record names the withheld node and prerequisite separately.
    if result.outcome == "HOST-INAPPLICABLE" {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require_plain_directory(parent, "prepared result directory").map_err(|message| {
        ResultPublicationFailure {
            message,
            descriptor_visible: false,
        }
    })?;
    let previous = read_existing_result(path).map_err(|message| ResultPublicationFailure {
        message,
        descriptor_visible: false,
    })?;
    let mut next = previous.clone().unwrap_or_default();
    serde_json::to_writer(&mut next, result).map_err(|error| ResultPublicationFailure {
        message: error.to_string(),
        descriptor_visible: false,
    })?;
    next.push(b'\n');
    let mut temporary = tempfile::Builder::new()
        .prefix(".results.jsonl.")
        .suffix(".staging")
        .tempfile_in(parent)
        .map_err(|error| ResultPublicationFailure {
            message: format!(
                "cannot create temporary result file in {}: {error}",
                parent.display()
            ),
            descriptor_visible: false,
        })?;
    if failure == Some(ResultPublicationFailurePoint::TemporaryWrite) {
        return Err(ResultPublicationFailure {
            message: "injected failure writing the temporary result file".into(),
            descriptor_visible: false,
        });
    }
    temporary
        .write_all(&next)
        .and_then(|()| temporary.flush())
        .map_err(|error| ResultPublicationFailure {
            message: format!("cannot write temporary result file: {error}"),
            descriptor_visible: false,
        })?;
    if failure == Some(ResultPublicationFailurePoint::TemporaryFileSync) {
        return Err(ResultPublicationFailure {
            message: "injected failure syncing the temporary result file".into(),
            descriptor_visible: false,
        });
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ResultPublicationFailure {
            message: format!("cannot sync temporary result file: {error}"),
            descriptor_visible: false,
        })?;
    if failure == Some(ResultPublicationFailurePoint::BeforeRename) {
        return Err(ResultPublicationFailure {
            message: "injected failure at result-row publication boundary".into(),
            descriptor_visible: false,
        });
    }
    temporary
        .persist(path)
        .map_err(|error| ResultPublicationFailure {
            message: format!(
                "cannot atomically publish result file {}: {}",
                path.display(),
                error.error
            ),
            descriptor_visible: false,
        })?;
    let durable = if failure == Some(ResultPublicationFailurePoint::ParentDirectorySync) {
        Err("injected failure syncing the result directory after result-row rename".to_string())
    } else {
        sync_plain_directory(parent, "result directory")
    };
    if let Err(error) = durable {
        return match restore_previous_result(path, previous.as_deref()) {
            Ok(()) => Err(ResultPublicationFailure {
                message: error,
                descriptor_visible: false,
            }),
            Err(rollback_error) => Err(ResultPublicationFailure {
                message: format!(
                    "{error}; result-row rollback also failed and the new row may remain visible: {rollback_error}"
                ),
                descriptor_visible: true,
            }),
        };
    }
    Ok(())
}

pub fn append_result(path: &Path, result: &CellResult) -> Result<(), String> {
    append_result_with_failure(path, result, None, false).map_err(|error| error.message)
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
    let skipped = results
        .iter()
        .filter(|result| result.outcome == "HOST-INAPPLICABLE")
        .count();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"hermit-e2e\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\">\n",
        results.len()
    );
    for result in results {
        let time = result
            .duration_ms
            .map(|duration_ms| format!(" time=\"{:.3}\"", duration_ms as f64 / 1000.0))
            .unwrap_or_default();
        out.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}/{}/{}\"{}>",
            xml(&result.category),
            xml(&result.test),
            xml(&result.mode),
            xml(result.backend.as_deref().unwrap_or("none")),
            time,
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
        if result.outcome == "HOST-INAPPLICABLE" {
            out.push_str(&format!(
                "<skipped message=\"{}\"/>",
                xml(result.reason.as_deref().unwrap_or("host-inapplicable"))
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

fn execution_cell_env(
    context: &RunContext,
    dir: &Path,
    verified: bool,
) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, verified);
    env.insert(
        SCHEDULED_JOBS_ENV.into(),
        context.scheduled_worker_capacity.configured().to_string(),
    );
    env
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

fn append_guest_env_args(
    argv: &mut Vec<String>,
    env: &BTreeMap<String, String>,
    uses_test_workdir: bool,
) {
    // Every forwarded value is harness-authored, never inherited ambient state.
    // PWD and OLDPWD remain absent; E2E_TMPDIR joins the fresh /test mount while
    // HOME/XDG retain their unique per-cell directories and seeded config.
    for name in [
        "LC_ALL",
        "TZ",
        "HOME",
        "XDG_CONFIG_HOME",
        "E2E_TMPDIR",
        "E2E_FIXTURE_DIR",
        SCHEDULED_JOBS_ENV,
    ] {
        let value = if uses_test_workdir && name == "E2E_TMPDIR" {
            HERMETIC_TEST_WORKDIR
        } else {
            env.get(name)
                .expect("cell environment contains every forwarded guest value")
        };
        argv.push("--env".into());
        argv.push(format!("{name}={value}"));
    }
}

fn append_scheduled_jobs_env_arg(argv: &mut Vec<String>, env: &BTreeMap<String, String>) {
    let value = env
        .get(SCHEDULED_JOBS_ENV)
        .expect("cell environment contains scheduled worker capacity");
    argv.push("--env".into());
    argv.push(format!("{SCHEDULED_JOBS_ENV}={value}"));
}

fn require_minimal_base_env(argv: &mut Vec<String>) -> Result<(), String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        if let Some(value) = argv[index].strip_prefix("--base-env=") {
            values.push(value.to_owned());
        } else if argv[index] == "--base-env" {
            index += 1;
            let value = argv.get(index).ok_or_else(|| {
                "hermetic validation received --base-env without a value".to_string()
            })?;
            values.push(value.clone());
        }
        index += 1;
    }
    match values.as_slice() {
        [] => argv.push("--base-env=minimal".into()),
        [value] if value == "minimal" => {}
        [value] => {
            return Err(format!(
                "hermetic validation requires --base-env=minimal, got {value}"
            ));
        }
        _ => return Err("hermetic validation received repeated --base-env flags".into()),
    }
    Ok(())
}

/// Choose the guest's working directory. Three sources, in this order, and the
/// ORDER IS THE POINT.
///
///   1. `isolated_workdir` — the hermetic lane's fresh tmpfs at `/test`, enabled by
///      `HERMIT_E2E_EMPTY_WORKDIR` from `ci/hermetic/run-split-validate.sh`.
///   2. `requested_workdir` — a workdir the manifest names for itself.
///   3. `fixed_workdir_source` — the ordinary-host fallback binds a fresh
///      per-attempt directory at `/tmp/test`.
fn append_execution_root_args(
    argv: &mut Vec<String>,
    isolated_workdir: Option<&Path>,
    requested_workdir: Option<&str>,
    fixed_workdir_source: Option<&Path>,
) {
    if let Some(workdir) = isolated_workdir {
        argv.push(format!("--mount=type=tmpfs,target={}", workdir.display()));
        argv.extend(["--workdir".into(), workdir.to_string_lossy().into_owned()]);
    } else if let Some(workdir) = requested_workdir {
        // Outside the explicit hermetic path, a manifest that names its own
        // workdir keeps it and outranks the ordinary-host fallback.
        argv.extend(["--workdir".into(), workdir.into()]);
    } else if let Some(source) = fixed_workdir_source {
        argv.push(format!(
            "--bind={}:{FIXED_GUEST_WORKDIR}",
            source.to_string_lossy()
        ));
        argv.extend(["--workdir".into(), FIXED_GUEST_WORKDIR.into()]);
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

/// Ask the hermit binary which commit it was built from.
///
/// The producer-owned `version --json` record is the authority. Human-readable
/// `--version` output is presentation and must not be parsed into provenance.
/// `None` means the binary could not be run, refused the current schema, or
/// returned an invalid revision; it is deliberately distinct from any value.
fn probe_binary_build_sha(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(["version", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_binary_build_info(&output.stdout)
}

fn parse_binary_build_info(bytes: &[u8]) -> Option<String> {
    let report = serde_json::from_slice::<detcore_model::build_info::BuildInfo>(bytes).ok()?;
    if report.schema != detcore_model::build_info::BuildInfo::SCHEMA {
        return None;
    }
    let sha = report.git_sha;
    let hex = sha.strip_suffix("-dirty").unwrap_or(&sha);
    let recognised =
        hex == "unknown" || (hex.len() >= 7 && hex.chars().all(|c| c.is_ascii_hexdigit()));
    recognised.then_some(sha)
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

fn execute_observed_until(
    spec: &CellRunSpec,
    observation: &Observation,
    dir: &Path,
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
) -> Result<AttemptResult, String> {
    let mut attempt = execute_spec_until(
        spec,
        deadline,
        cpu_timeout_seconds,
        wall_timeout_seconds,
        remaining_cpu_usec,
    )?;
    attempt.observation_sha256 = Some(observation_hash(observation, &attempt, dir));
    Ok(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_selection::BackendCiDisabledReason;

    #[test]
    fn failure_class_schema_matches_serialized_enum() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../failure-class-schema.json")).unwrap();
        assert_eq!(schema["schema"], 1);
        let classes = [
            FailureClass::ProductFailure,
            FailureClass::UnderstoodInfrastructureFailure,
            FailureClass::UnderstoodPrerequisiteFailure,
            FailureClass::NoResult,
        ];
        let serialized = classes
            .iter()
            .map(|class| serde_json::to_value(class).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(schema["values"], serde_json::Value::Array(serialized));
        for class in classes {
            assert_eq!(FailureClass::parse(class.as_str()), Ok(class));
        }
        assert_eq!(
            FailureClass::parse("future_failure"),
            Err("unknown failure_class `future_failure`".into())
        );
    }
    use crate::ci_selection::CiDisabledResult;

    #[test]
    fn retry_outcome_preserves_recovery_and_product_failure() {
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "ERROR")]).unwrap(),
            "FAIL"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "ERROR")]).unwrap(),
            "ERROR"
        );
        assert_eq!(
            outcome_after_retries([(1, "HOST-INAPPLICABLE")]).unwrap(),
            "HOST-INAPPLICABLE"
        );
    }

    #[test]
    fn retry_outcome_refuses_malformed_history_by_name() {
        for (attempts, named) in [
            (vec![], "no attempts"),
            (vec![(2, "FAIL")], "expected 1"),
            (
                vec![(1, "PASS"), (2, "FAIL")],
                "follows terminal outcome PASS",
            ),
            (
                vec![(1, "HOST-INAPPLICABLE"), (2, "ERROR")],
                "follows terminal outcome HOST-INAPPLICABLE",
            ),
            (vec![(1, "UNKNOWN")], "unknown outcome \"UNKNOWN\""),
            (
                vec![(1, "FAIL"), (2, "ERROR"), (3, "PASS")],
                "exceeds the shared maximum of 2",
            ),
        ] {
            let error = outcome_after_retries(attempts).unwrap_err();
            assert!(error.contains(named), "{error:?} did not name {named:?}");
        }
    }

    #[test]
    fn selected_retry_row_keeps_its_ordinal_while_summary_keeps_history_count() {
        let mut first = cell_result_that_located_nothing();
        first.outcome = "FAIL".into();
        first.attempt = 1;

        let mut infrastructure = first.clone();
        infrastructure.outcome = "ERROR".into();
        infrastructure.attempt = 2;
        let product_then_infrastructure = [first.clone(), infrastructure];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&product_then_infrastructure).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("FAIL", 1, 2)
        );

        let mut recovered = first.clone();
        recovered.outcome = "PASS".into();
        recovered.attempt = 2;
        let recovered_history = [first, recovered];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&recovered_history).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 2, 2)
        );

        let peer = cell_result_that_located_nothing();
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 1, 1)
        );
    }

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
            preprocessors: Vec::new(),
        }
    }

    /// ⚠️ "NO CELLS" AND "NO SUCH TEST" ARE DIFFERENT ANSWERS, AND `select` GIVES
    /// THE SAME EMPTY VECTOR FOR BOTH. Measured 2026-08-26 before this existed:
    /// `test-harness plan --lane portable --test no-such-test-xyz` printed nothing and
    /// exited 0 -- and so did the same command with a REAL id, so the exit code
    /// carried no information either way and anything bisecting off `plan` read a typo
    /// as "nothing failed here".
    ///
    /// The fix could NOT be `cells.is_empty()`, which is why this predicate exists:
    /// `print_plan` also serves `audit-gaps`, where empty legitimately means NO GAPS.
    /// Measured on the same head, a real id filtered to a lane it has no cells in
    /// (`--lane privileged --test applications/c-toolchain-workflow`) prints `[]` and
    /// exits 0 -- a correct, well-formed query that an emptiness guard would refuse.
    #[test]
    fn knows_test_separates_an_absent_id_from_an_empty_population() {
        let test = recipe(false);
        let id = test.id.clone();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        assert!(set.knows_test(&id), "a declared test must be known");
        assert!(
            !set.knows_test("no-such-test-xyz"),
            "an id in no manifest must NOT be known -- this is the typo a bisect would \
             otherwise read as a pass"
        );
        // The control that makes the pair meaningful: the KNOWN id still selects zero
        // cells in the Required population, so emptiness and unknown-ness genuinely
        // come apart here rather than only in principle.
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty(),
            "fixture must be empty in Required, or this test proves nothing"
        );
    }

    fn guest_env_args(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|window| window[0] == "--env")
            .map(|window| window[1].clone())
            .collect()
    }

    fn assert_minimal_guest_env(argv: &[String], dir: &str, tmp: &str, jobs: &str) {
        let mut base_env = Vec::new();
        let mut index = 0;
        while index < argv.len() {
            if let Some(value) = argv[index].strip_prefix("--base-env=") {
                base_env.push(value.to_string());
            } else if argv[index] == "--base-env" {
                index += 1;
                base_env.push(argv[index].clone());
            }
            index += 1;
        }
        assert_eq!(
            base_env,
            ["minimal"],
            "every hermetic Hermit cell must select the minimal base environment exactly once: {argv:?}"
        );
        assert_eq!(
            guest_env_args(argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                format!("HOME={dir}/home"),
                format!("XDG_CONFIG_HOME={dir}/xdg-config"),
                format!("E2E_TMPDIR={tmp}"),
                format!("E2E_FIXTURE_DIR={dir}/fixtures"),
                format!("HERMIT_E2E_SCHEDULED_JOBS={jobs}"),
            ],
            "the guest environment is an exact allowlist; an added, removed, or inherited name is a regression"
        );
    }

    fn ptrace_cell(mode: &str) -> SelectedCell {
        let mut test = recipe(true);
        if mode != "verify" {
            let mode_recipe = test.modes.remove("verify").unwrap();
            test.modes.insert(mode.into(), mode_recipe);
        }
        SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 600,
            cpu_timeout_seconds: 22,
        }
    }

    fn fixture_host_capabilities() -> HostCapabilities {
        BTreeMap::from([
            (
                HostCapability::CpuidFaulting,
                HostCapabilityVerdict {
                    present: true,
                    evidence: "fixture cpuid probe".into(),
                },
            ),
            (
                HostCapability::Kvm,
                HostCapabilityVerdict {
                    present: false,
                    evidence: "fixture kvm probe".into(),
                },
            ),
        ])
    }

    fn run_context(root: &Path) -> RunContext {
        RunContext {
            root: root.into(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        }
    }

    #[test]
    fn cell_retry_attempts_use_distinct_artifact_directories() {
        let root = PathBuf::from("/tmp/hermit-cell-retry-artifact-fixture");
        let cell = ptrace_cell("verify");
        let first = run_context(&root);
        let second = first.with_attempt(2);

        let first_dir = cell_artifact_dir(&first, &cell);
        let second_dir = cell_artifact_dir(&second, &cell);
        assert_ne!(first_dir, second_dir);
        assert!(
            second_dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-attempt-2"))
        );
    }

    fn bound_workdir_source(spec: &CellRunSpec) -> PathBuf {
        let bind = spec
            .argv
            .iter()
            .find_map(|arg| {
                arg.strip_prefix("--bind=")
                    .and_then(|arg| arg.strip_suffix(":/tmp/test"))
            })
            .expect("default workdir bind is present");
        PathBuf::from(bind)
    }

    #[test]
    fn required_and_enabled_are_distinct_populations() {
        let test = recipe(false);
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
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
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        assert_eq!(
            infrastructure_error_result(&context, &cell, "fixture".into()).classification,
            "required"
        );
    }

    #[test]
    fn exact_cell_timeout_overrides_the_bucket_and_global_defaults() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.timeout_seconds.insert("ptrace".into(), 30);
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason.insert(
            "ptrace".into(),
            "three complete validation runs measured this cell above the inherited limit".into(),
        );
        validate_mode("fixture/test", "verify", mode, 20).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 20, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let cells = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("ptrace"))
                .unwrap()
                .timeout_seconds,
            30
        );
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("liteinst"))
                .unwrap()
                .timeout_seconds,
            20
        );
    }

    #[test]
    fn enabled_kvm_python_examples_keeps_its_measured_timeout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let defaults: ManifestDefaults = serde_yaml::from_str(
            &fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(defaults.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(
            defaults.cpu_timeout_seconds,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS
        );

        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Enabled),
                test: Some("applications/kvm-python-examples".into()),
                mode: Some("verify".into()),
                backend: Some("kvm".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].timeout_seconds, 74);
        assert_eq!(cells[0].cpu_timeout_seconds, 25);
    }

    #[test]
    fn shipped_readdir_order_identity_uses_its_measured_timeout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Required),
                test: Some("backend-parity-c/readdir-order-identity".into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(
            cells[0].cpu_timeout_seconds,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn shipped_plan_matches_the_frozen_timeout_calibration_partition() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let required = manifests
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let enabled = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(
            required.len(),
            CALIBRATED_CI_CELL_COUNT + KVM_RATCHET_CI_CELL_COUNT - KVM_RUN_1709_CI_REMOVAL_COUNT
                + KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT
        );
        assert_eq!(
            enabled.len() - required.len(),
            NON_CI_CELL_COUNT,
            "the current manifest census records every enabled ci:false cell"
        );

        let observed = enabled
            .iter()
            .filter(|cell| {
                cell.cpu_timeout_seconds != DEFAULT_TEST_CPU_TIMEOUT_SECONDS
                    || cell.timeout_seconds != DEFAULT_TEST_WALL_TIMEOUT_SECONDS
            })
            .map(|cell| {
                (
                    (
                        cell.id.test.as_str(),
                        cell.id.mode.as_str(),
                        cell.id.backend.as_deref().unwrap_or("native"),
                    ),
                    (cell.cpu_timeout_seconds, cell.timeout_seconds),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let expected = EXPLICIT_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&KVM_RATCHET_TIMEOUT_CALIBRATIONS)
            .map(|calibration| {
                (
                    (calibration.test, calibration.mode, calibration.backend),
                    (
                        calibration.configured_cpu_seconds,
                        calibration.configured_wall_seconds,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(observed, expected);
    }

    #[test]
    fn bucket_timeout_requires_a_reason_and_must_change_the_global_default() {
        let mut document = ManifestDocument {
            schema: MANIFEST_SCHEMA,
            bucket: "fixture".into(),
            timeout_seconds: Some(20),
            slow_reason: Some("measured bucket-wide work needs more than 15 seconds".into()),
            test: vec![recipe(true)],
        };
        document.slow_reason = None;
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds requires a non-empty slow_reason"
        );
        document.slow_reason = Some("measured bucket-wide work needs more than 15 seconds".into());
        document.timeout_seconds = Some(15);
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds redundantly repeats the global default"
        );
    }

    #[test]
    fn cell_timeout_requires_the_same_named_reason() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.timeout_seconds.insert("ptrace".into(), 30);
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
            "fixture/test: verify timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        );
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason
            .insert("ptrace".into(), "measured above the inherited limit".into());
        assert!(validate_mode("fixture/test", "verify", &mode, 15).is_ok());
    }

    #[test]
    fn execution_wall_backstop_is_wider_than_the_cpu_budget() {
        let started = Instant::now();
        let deadline = execution_deadline_after_preparation(started, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(deadline, started),
            Duration::from_secs(57)
        );
        assert_eq!(
            remaining_cell_time_at(deadline, started + Duration::from_secs(57)),
            Duration::ZERO
        );
    }

    #[test]
    fn slow_preparation_does_not_reduce_the_execution_backstop() {
        let preparation_started = Instant::now();
        let prepared_at = preparation_started + Duration::from_millis(47_770);
        let execution_deadline = execution_deadline_after_preparation(prepared_at, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(execution_deadline, prepared_at),
            Duration::from_secs(57),
            "preparation must not consume the fresh explicit wall backstop"
        );
    }

    #[test]
    fn an_expired_wall_backstop_emits_typed_not_run_without_a_process() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/prelaunch-timeout".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.clone(),
            env: BTreeMap::new(),
            argv: vec!["/bin/false".into()],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 15,
            verdict_path: Some(root.join("verdict.json")),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.clone(),
            attempt: "1".into(),
            fixed_workdir_source: root.join("workdir/1"),
        };

        // This is the exact boundary reached when the aggregate execution
        // backstop is gone before a later attempt starts: no child process can
        // exist or have an exit status.
        let attempt = execute_spec_until(&spec, Instant::now(), 15, 57, None).unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        let report = attempt
            .verification_report
            .as_deref()
            .expect("pre-launch timeout must carry typed NotRun evidence");
        let report_sha256 = hex_digest(report.as_bytes());
        assert_eq!(
            attempt.verification_report_sha256.as_deref(),
            Some(report_sha256.as_str())
        );
        let report = VerificationReport::from_json_slice(report.as_bytes()).unwrap();
        assert_eq!(report.verdict, Verdict::NoResult);
        assert_eq!(
            report.no_result_reason,
            Some(crate::canonical_verdict::NoResultReason::NotRun)
        );
        assert_eq!(
            failure_class(
                &attempt.outcome,
                observed_result(
                    "verify",
                    &attempt.outcome,
                    std::slice::from_ref(&attempt),
                    attempt.error_kind.as_deref(),
                ),
                attempt.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_exhausted_cpu_budget_is_typed_before_the_next_process_starts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-cpu-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "prelaunch-cpu-timeout", "/bin/false");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(0),
        )
        .unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wait4_cpu_usage_moves_with_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cpu-usage-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let measure = |label: &str, script: &str| {
            execute_process(
                &root,
                "/bin/sh",
                &["-c".into(), script.into()],
                &BTreeMap::new(),
                &root.join(format!("{label}.stdout")),
                &root.join(format!("{label}.stderr")),
                (Instant::now() + Duration::from_secs(10), None),
            )
            .unwrap()
        };
        let low = measure("low", ":");
        let high = measure("high", "head -c 134217728 /dev/zero | sha256sum >/dev/null");
        assert!(low.status.success());
        assert!(high.status.success());
        assert!(
            high.cpu_usage_usec > low.cpu_usage_usec.saturating_add(10_000),
            "adding 128 MiB of descendant hashing did not move CPU usage: low={} high={}",
            low.cpu_usage_usec,
            high.cpu_usage_usec
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn bounded_process(
        root: &Path,
        label: &str,
        script: &str,
        wall: Duration,
        cpu_budget_usec: u64,
    ) -> ProcessOutput {
        execute_process(
            root,
            "/bin/sh",
            &["-c".into(), script.into()],
            &BTreeMap::new(),
            &root.join(format!("{label}.stdout")),
            &root.join(format!("{label}.stderr")),
            (Instant::now() + wall, Some(cpu_budget_usec)),
        )
        .unwrap()
    }

    #[test]
    fn live_cpu_accounting_excludes_an_unrelated_process_group() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-owned-cpu-accounting-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("3");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        // `stop_process_group` below owns the matching wait4; discard only the
        // std handle so the test exercises the production reaper.
        drop(child);
        let unrelated = bounded_process(
            &root,
            "unrelated",
            "while :; do :; done",
            Duration::from_millis(650),
            5_000_000,
        );
        assert_eq!(unrelated.timeout, Some(ProcessTimeout::Wall));
        let used = process_group_cpu_usage_usec(pid)
            .unwrap()
            .expect("the launched sleeper must remain measurable");
        assert!(
            used < 100_000,
            "an idle process group unexpectedly included unrelated CPU: {used} usec"
        );
        stop_process_group(pid).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn bounded_spec(root: &Path, label: &str, script: &str) -> CellRunSpec {
        CellRunSpec {
            id: CellId {
                test: format!("fixture/{label}"),
                mode: "naked".into(),
                backend: None,
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.to_owned(),
            env: BTreeMap::new(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            guest_argv: vec![script.into()],
            timeout_seconds: 1,
            verdict_path: None,
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.join(label),
            attempt: "1".into(),
            fixed_workdir_source: root.join(label).join("workdir/1"),
        }
    }

    #[test]
    fn cpu_burner_is_stopped_by_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "burner", "while :; do :; done");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(1_000_000),
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert_eq!(attempt.reason.as_deref(), Some("cell exceeded 1 CPU s"));
        assert!(attempt.cpu_usage_usec.unwrap() >= 1_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "cpu-timeout",
            "the retained evidence must distinguish a CPU timeout from wall timeout"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_process_is_checked_against_cpu_budget_between_polls() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-completed-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = execute_process_with_cpu_poll_interval(
            &root,
            "/bin/sh",
            &[
                "-c".into(),
                "head -c 8388608 /dev/zero | sha256sum >/dev/null".into(),
            ],
            &BTreeMap::new(),
            &root.join("completed.stdout"),
            &root.join("completed.stderr"),
            ProcessLimits {
                deadline: Instant::now() + Duration::from_secs(5),
                cpu_budget_usec: Some(1),
                cpu_poll_interval: Duration::from_secs(5),
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleeping_process_is_stopped_by_the_wall_backstop() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-wall-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "sleeper", "sleep 5");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(1),
            1,
            1,
            Some(5_000_000),
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert_eq!(
            attempt.reason.as_deref(),
            Some("cell exceeded 1 wall s backstop (1 s CPU budget)")
        );
        assert!(attempt.cpu_usage_usec.unwrap() < 5_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "wall-timeout",
            "the retained evidence must distinguish a wall timeout from CPU timeout"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descheduled_process_may_exceed_the_old_wall_bound_and_pass() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descheduled-process-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let output = bounded_process(
            &root,
            "stopped",
            "(sleep 0.3; kill -CONT $$) & kill -STOP $$; printf resumed",
            Duration::from_secs(2),
            200_000,
        );
        assert!(output.status.success());
        assert_eq!(output.timeout, None);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "the process did not remain descheduled long enough to exercise wall-vs-CPU"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_cpu_budget_counts_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descendant-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = bounded_process(
            &root,
            "descendant",
            "sh -c 'while :; do :; done' & wait",
            Duration::from_secs(5),
            100_000,
        );
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 100_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_processes_share_one_aggregate_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-aggregate-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let limit = 300_000u64;
        let first = bounded_process(
            &root,
            "first",
            "head -c 8388608 /dev/zero | sha256sum >/dev/null",
            Duration::from_secs(5),
            limit,
        );
        assert!(first.status.success());
        assert_eq!(first.timeout, None);
        assert!(first.cpu_usage_usec < limit);
        let second = bounded_process(
            &root,
            "second",
            "while :; do :; done",
            Duration::from_secs(5),
            limit - first.cpu_usage_usec,
        );
        assert_eq!(second.timeout, Some(ProcessTimeout::Cpu));
        assert!(
            first.cpu_usage_usec.saturating_add(second.cpu_usage_usec) >= limit,
            "second process did not consume the remainder of the shared CPU budget"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_usage_aggregation_refuses_missing_or_overflowing_measurements() {
        assert_eq!(checked_add_cpu_usage(Some(2), Some(3)), Some(5));
        assert_eq!(checked_add_cpu_usage(Some(2), None), None);
        assert_eq!(checked_add_cpu_usage(None, Some(3)), None);
        assert_eq!(checked_add_cpu_usage(Some(u64::MAX), Some(1)), None);
    }

    #[test]
    fn sleeping_repeated_invocations_do_not_consume_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cell-deadline-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mut test = recipe(true);
        let counter = root.join("counter");
        test.direct = Some(DirectCommand::Argv(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "count=$(cat '{}' 2>/dev/null || printf 0); count=$((count + 1)); printf '%s' \"$count\" > '{}'; sleep 0.7; printf '%s\\n' \"$count\"",
                counter.display(),
                counter.display()
            ),
        ]));
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(3);
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 3,
            cpu_timeout_seconds: 1,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("missing-hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.attempts.len(), 3);
        assert!(result.attempts.iter().all(|attempt| !attempt.timed_out));
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.failure_class, None);
        let duration_ms = result
            .duration_ms
            .expect("a cell that executed must report measured wall time");
        assert!(
            (2_000..3_000).contains(&duration_ms),
            "three sleeping attempts should pass despite exceeding the old one-second wall cap: {duration_ms}ms"
        );
        let attempt_cpu_usage_usec = result.attempts.iter().try_fold(0u64, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
        assert_eq!(
            result.cpu_usage_usec, attempt_cpu_usage_usec,
            "the cell CPU figure must sum every process-owned attempt measurement"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_inapplicable_is_a_typed_nonpass_and_a_junit_skip() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-host-inapplicable-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut test = recipe(true);
        test.requires = vec!["cpuid".into()];
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let result = host_inapplicable_result(
            &context,
            &cell,
            "NOT RUN, NOT a pass, no coverage: planted absence".into(),
        );
        assert_eq!(result.outcome, "HOST-INAPPLICABLE");
        assert_eq!(result.result, None);
        assert_eq!(
            result.failure_class,
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert!(result.attempts.is_empty());
        assert!(result.binary_sha256.is_none());
        assert_eq!(result.error_kind, None);
        assert_eq!(result.duration_ms, None);
        assert_eq!(result.cpu_usage_usec, None);
        let results = root.join("results.jsonl");
        append_result(&results, &result).unwrap();
        assert!(
            !results.exists(),
            "a cell withheld for an unmet prerequisite must not publish a cell row"
        );
        let row = serde_json::to_value(&result).unwrap();
        assert!(
            row.get("duration_ms").is_none(),
            "a cell that never executed must not publish a measured zero wall time"
        );
        assert!(
            row["cpu_usage_usec"].is_null(),
            "a cell that never executed must publish null rather than a measured zero CPU time"
        );
        let mut measured_zero = result.clone();
        measured_zero.duration_ms = Some(0);
        assert_eq!(
            serde_json::to_value(measured_zero).unwrap()["duration_ms"],
            0,
            "a measured sub-millisecond duration must remain zero"
        );

        let junit = root.join("junit.xml");
        write_junit(&junit, &[result]).unwrap();
        let xml = fs::read_to_string(&junit).unwrap();
        assert!(xml.contains("tests=\"1\" failures=\"0\" errors=\"0\" skipped=\"1\""));
        assert!(
            xml.contains(
                "<skipped message=\"NOT RUN, NOT a pass, no coverage: planted absence\"/>"
            )
        );
        assert!(!xml.contains(" time=\"0.000\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retry_preserves_a_divergence_when_the_later_row_passes() {
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let mut context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();
        let mut first = infrastructure_error_result(&context, &cell, "forced failure".into());
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        assert_eq!(first.duration_ms, None);
        first.duration_ms = Some(111);
        first.first_divergent_record = Some(93);
        first.first_divergent_syscall = Some(37);
        first.first_divergent_scheduler_turn = Some(68);
        first.first_divergent_virtual_nanoseconds = Some(7);
        first.first_divergent_left_message = Some("INFO detcore: left event".into());
        first.first_divergent_right_message = Some("INFO detcore: right event".into());
        append_result(&path, &first).unwrap();

        // A validate retry starts a fresh harness process and prepares the same
        // path again. This call is the negative-control boundary: replacing it
        // with the former fs::write(path, b"") drops the first observation.
        prepare_result_path(&path).unwrap();
        context.attempt = 2;
        let mut second = infrastructure_error_result(&context, &cell, "forced retry".into());
        second.outcome = "PASS".into();
        second.result = Some(ObservedResult::Pass);
        second.failure_class = None;
        second.error_kind = None;
        second.duration_ms = Some(222);
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path).unwrap();
        let rows = rows
            .lines()
            .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["attempt"], 1);
        assert_eq!(rows[0]["duration_ms"], 111);
        assert_eq!(rows[0]["timeout_seconds"], 15);
        assert_eq!(rows[0]["outcome"], "FAIL");
        assert_eq!(rows[0]["result"], "determinism-failure");
        assert_eq!(rows[0]["failure_class"], "product_failure");
        assert_eq!(rows[0]["first_divergent_record"], 93);
        assert_eq!(rows[0]["first_divergent_syscall"], 37);
        assert_eq!(rows[0]["first_divergent_scheduler_turn"], 68);
        assert_eq!(rows[0]["first_divergent_virtual_nanoseconds"], 7);
        assert_eq!(
            rows[0]["first_divergent_left_message"],
            "INFO detcore: left event"
        );
        assert_eq!(
            rows[0]["first_divergent_right_message"],
            "INFO detcore: right event"
        );
        assert_eq!(rows[1]["attempt"], 2);
        assert_eq!(rows[1]["duration_ms"], 222);
        assert_eq!(rows[1]["timeout_seconds"], 15);
        assert_eq!(rows[1]["outcome"], "PASS");
        assert_eq!(rows[1]["result"], "pass");
        assert_eq!(rows[1]["failure_class"], JsonValue::Null);
        assert!(rows[1]["first_divergent_left_message"].is_null());
        assert!(rows[1]["first_divergent_right_message"].is_null());
        assert_ne!(rows[0]["artifact_dir"], rows[1]["artifact_dir"]);
        assert!(
            rows[1]["artifact_dir"]
                .as_str()
                .unwrap()
                .ends_with("-attempt-2")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_preserves_changed_product_result_and_attribution() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-retry-result-class-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();

        let mut first = cell_result_that_located_nothing();
        first.attempt = 1;
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        first.artifact_dir = "/repo/artifacts-attempt-1".into();
        append_result(&path, &first).unwrap();

        let mut second = first.clone();
        second.attempt = 2;
        second.result = Some(ObservedResult::CrashError);
        second.artifact_dir = "/repo/artifacts-attempt-2".into();
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(rows[1].result, Some(ObservedResult::CrashError));
        assert_eq!(rows[0].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[1].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[0].error_kind, None);
        assert_eq!(rows[1].error_kind, None);
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
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
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
        validate_mode("fixture/test", "verify", &mode, 15).unwrap();
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
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
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
                    format!(
                        "fixture/test: {mode} workdir is unsupported when DBT is enabled because DBT does not enter the Hermit mount namespace"
                    )
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
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
    fn hermetic_spec_mounts_a_fresh_test_root_and_limits_guest_environment() {
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: Some(PathBuf::from("/test")),
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let mount = spec
            .argv
            .iter()
            .position(|arg| arg == "--mount=type=tmpfs,target=/test")
            .unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/test"])
            .unwrap();
        assert!(mount < separator && workdir < separator);
        assert_minimal_guest_env(&spec.argv, "/repo/results/cell", "/test", "7");

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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay"),
            vec!["/bin/true".into()],
            "1",
            None,
            7,
        )
        .unwrap();
        assert!(
            replay
                .argv
                .windows(2)
                .any(|window| { window[0] == "--record-timeout" && window[1] == "7" })
        );
        assert_eq!(replay.timeout_seconds, 7);
        assert!(replay.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert!(
            replay
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&replay.argv, "/repo/results/replay", "/test", "7");

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env".into(), "minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert!(
            custom
                .argv
                .windows(2)
                .any(|args| args == ["--base-env", "minimal"])
        );
        assert!(
            custom
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&custom.argv, "/repo/results/custom", "/test", "7");
        let mut naked_test = recipe(true);
        let naked_mode = naked_test.modes.remove("verify").unwrap();
        naked_test.modes.insert("naked".into(), naked_mode);
        let naked_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: naked_test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test: naked_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let naked_error = build_spec(
            &context,
            &naked_cell,
            PathBuf::from("/repo/results/naked"),
            vec!["/bin/true".into()],
            "1",
            None,
            naked_cell.timeout_seconds,
        )
        .unwrap_err();
        assert_eq!(
            naked_error,
            "hermetic validation cannot isolate a naked cell on the native backend at /test"
        );
    }

    #[test]
    fn hermetic_guest_arguments_resolve_repo_inputs_but_keep_dot_local() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("tests/e2e")).unwrap();
        fs::write(root.join("README.md"), b"fixture").unwrap();
        let mut argv = vec![
            "tool".into(),
            "tests/e2e".into(),
            "README.md".into(),
            ".".into(),
            "missing/path".into(),
        ];
        resolve_repo_guest_args(&root, &mut argv);
        assert_eq!(argv[0], "tool");
        assert_eq!(argv[1], root.join("tests/e2e").to_string_lossy());
        assert_eq!(argv[2], root.join("README.md").to_string_lossy());
        assert_eq!(argv[3], ".");
        assert_eq!(argv[4], "missing/path");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_fixed_workdir_resolves_repo_inputs_before_chdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-ordinary-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::write(root.join("fixtures/input.txt"), b"fixture").unwrap();

        let mut cell = ptrace_cell("verify");
        cell.test.direct = Some(DirectCommand::Argv(vec![
            "/bin/true".into(),
            "fixtures/input.txt".into(),
            ".".into(),
        ]));
        let context = run_context(&root);
        let cell_dir = root.join("results/cell");
        let guest = prepare_test(&context, &cell, &cell_dir).unwrap();
        assert_eq!(guest[1], root.join("fixtures/input.txt").to_string_lossy());
        assert_eq!(guest[2], ".");

        let spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            guest,
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            bound_workdir_source(&spec),
            cell_dir.join("workdir/1"),
            "the ordinary supported path must retain hermit-132's per-attempt bind"
        );
        assert_eq!(
            spec.guest_argv[1],
            root.join("fixtures/input.txt").to_string_lossy(),
            "repo input must be absolute before the guest changes to /tmp/test"
        );
        assert_eq!(spec.guest_argv[2], ".");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_relaxations_are_explicit_and_recorded() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.compare_io_buffers = Some(false);
        mode.compare_io_buffers_disabled_reason = Some(
            "guest assertions validate sanitizer-specific invariants while whole files may vary"
                .into(),
        );
        mode.rcb_time = Some(false);
        mode.rcb_time_disabled_reason =
            Some("data-dependent assertion work must not perturb virtual time".into());
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let relaxation = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-detlog-io-buffers")
            .unwrap();
        assert!(relaxation < separator);
        let no_rcb_time = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-rcb-time")
            .unwrap();
        assert!(no_rcb_time < separator);
        assert_eq!(
            cell_relaxations(&cell),
            vec![
                String::from(
                    "--no-detlog-io-buffers: guest assertions validate sanitizer-specific invariants while whole files may vary",
                ),
                String::from(
                    "--no-rcb-time: data-dependent assertion work must not perturb virtual time",
                ),
            ]
        );

        let mut missing_reason = recipe(true).modes.remove("verify").unwrap();
        missing_reason.compare_io_buffers = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_reason, 15)
                .unwrap_err()
                .contains("requires compare_io_buffers_disabled_reason")
        );

        let mut missing_rcb_reason = recipe(true).modes.remove("verify").unwrap();
        missing_rcb_reason.rcb_time = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_rcb_reason, 15)
                .unwrap_err()
                .contains("requires rcb_time_disabled_reason")
        );
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert!(spec.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(spec.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert_eq!(
            spec.env.get(SCHEDULED_JOBS_ENV).map(String::as_str),
            Some("7")
        );
        assert_minimal_guest_env(&spec.argv, "/repo/results/cell", "/tmp/hermit-e2e", "7");
        assert!(!spec.argv.iter().any(|arg| arg == "--no-virtualize-cpuid"));
        assert!(
            !spec
                .argv
                .iter()
                .any(|arg| arg == "--max-timeslice=disabled")
        );
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            replay_cell.timeout_seconds,
        )
        .unwrap();
        assert!(replay.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(replay.argv.windows(2).any(|window| {
            window[0] == "--verify-json" && window[1] == "/repo/results/replay-cell/verify-1.json"
        }));
        assert!(!replay.argv.iter().any(|arg| arg.starts_with("--base-env")));
        assert_eq!(
            guest_env_args(&replay.argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                "HOME=/repo/results/replay-cell/home".to_string(),
                "XDG_CONFIG_HOME=/repo/results/replay-cell/xdg-config".to_string(),
                "E2E_TMPDIR=/tmp/hermit-e2e".to_string(),
                "E2E_FIXTURE_DIR=/repo/results/replay-cell/fixtures".to_string(),
                "HERMIT_E2E_SCHEDULED_JOBS=7".to_string(),
            ]
        );

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env=minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            guest_env_args(&custom.argv),
            vec!["HERMIT_E2E_SCHEDULED_JOBS=7".to_string()]
        );
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

    fn attempt_with_divergence(
        turn: Option<u64>,
        nanos: Option<u64>,
        record: Option<u64>,
        syscall: Option<u64>,
    ) -> AttemptResult {
        let mut attempt = attempt_with_sabre_evidence("");
        attempt.first_divergent_scheduler_turn = turn;
        attempt.first_divergent_virtual_nanoseconds = nanos;
        attempt.first_divergent_record = record;
        attempt.first_divergent_syscall = syscall;
        attempt
    }

    /// The cell-level position is the FIRST attempt that located one, matching
    /// how `reason` is chosen, and NOT the last -- otherwise a passing retry
    /// would erase the position the failing attempt found.
    #[test]
    fn cell_divergence_takes_the_first_attempt_that_located_one() {
        let mut attempts = vec![
            attempt_with_divergence(None, None, None, None),
            attempt_with_divergence(Some(4), Some(400), Some(40), Some(14)),
            attempt_with_divergence(Some(1), Some(100), Some(10), Some(2)),
        ];
        attempts[1].first_divergent_left_message = Some("left event A".into());
        attempts[1].first_divergent_right_message = Some("right event A".into());
        attempts[2].first_divergent_left_message = Some("left event B".into());
        attempts[2].first_divergent_right_message = Some("right event B".into());
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(4),
                virtual_nanoseconds: Some(400),
                record: Some(40),
                syscall: Some(14),
                left_message: Some("left event A".into()),
                right_message: Some("right event A".into()),
            },
            "the first located position wins; a later, earlier-diverging attempt \
             must not silently replace it, and this is NOT a min across attempts"
        );
    }

    /// Resolved per coordinate, so a report that located only some of the four
    /// still contributes the ones it has. Each coordinate here comes from a
    /// DIFFERENT attempt, which a per-attempt rule could not produce.
    #[test]
    fn cell_divergence_resolves_each_coordinate_independently() {
        let attempts = vec![
            attempt_with_divergence(None, Some(900), None, None),
            attempt_with_divergence(Some(7), None, None, None),
            attempt_with_divergence(None, None, Some(3), None),
            attempt_with_divergence(None, None, None, Some(5)),
        ];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(7),
                virtual_nanoseconds: Some(900),
                record: Some(3),
                syscall: Some(5),
                left_message: None,
                right_message: None,
            }
        );
    }

    /// A located record with NO syscall is a real state, not a gap: a run can
    /// diverge before any syscall has completed. Measured on a real log --
    /// diverging at record 12 reported syscall `None`, because no
    /// `finish syscall #N` had been written yet.
    #[test]
    fn a_divergence_before_any_syscall_completes_reports_no_syscall() {
        let attempts = vec![attempt_with_divergence(Some(1), None, Some(12), None)];
        let position = cell_divergence_position(&attempts);
        assert_eq!(position.record, Some(12));
        assert_eq!(position.syscall, None);
    }

    /// A clean cell reports no position at all.
    #[test]
    fn cell_divergence_is_absent_when_no_attempt_located_one() {
        let attempts = vec![attempt_with_divergence(None, None, None, None)];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition::default()
        );
        assert_eq!(cell_divergence_position(&[]), DivergencePosition::default());
    }

    /// ⚠️ ABSENCE OF THIS KEY IS RESERVED FOR "WRITTEN BEFORE THE FIELD
    /// EXISTED", AND NOTHING ELSE. A row that ran, compared, and located no
    /// divergence must carry the key with an explicit null.
    ///
    /// WHY IT NEEDS A TEST RATHER THAN BEING OBVIOUS. `CELL_RESULT_SCHEMA` was
    /// NOT bumped when these coordinates were added, so the schema number
    /// cannot separate the two meanings. Within schema 4 the only thing that
    /// distinguishes "this predates the field" from "this ran and found nothing"
    /// is whether the key is present at all.
    ///
    /// ⚠️ THE STRUCTURAL CLAIM IS THE DURABLE ONE; THE COUNTS ARE NOT. Schema 4
    /// contains BOTH rows with the key and rows without it, while schemas 1, 2
    /// and 3 never carry it -- that is what makes the schema number useless as a
    /// discriminator, and agent(hermit-106) reproduced it independently across
    /// 1478 rows. The row COUNTS behind it move: this file measured 28899 without
    /// and 17540 with over 6215 retained `results.jsonl`, and a sample of 1200 of
    /// those files gave a different ratio. Both are honest. The corpus lives in
    /// gitignored run directories that agents create and delete, so it is not a
    /// fixed population and a count taken from it is true of one moment. Do not
    /// treat these numbers as a baseline to compare against.
    ///
    /// That distinction currently holds only because no
    /// `skip_serializing_if = "Option::is_none"` is attached to these
    /// fields -- an entirely natural tidy-up for a field that is null on the
    /// large majority of rows. Adding one would silently convert every future
    /// "ran and found nothing" row into a row indistinguishable from a
    /// pre-field one, and no existing test would notice. This is that test.
    ///
    /// It asserts PRESENCE and NULLNESS separately from any value, so it fails
    /// on the skip attribute rather than on a changed position.
    /// ⚠️ THE ROW, NOT ONLY THE ATTEMPT. The measurement in the docstring above is
    /// about `results.jsonl` ROWS, and a row is a `CellResult` with its attempts
    /// nested inside it. An earlier version of this test guarded `AttemptResult`
    /// alone, which left the level the evidence actually describes uncovered:
    /// agent(hermit-106) put `skip_serializing_if = "Option::is_none"` on all four
    /// `CellResult` coordinates and the whole package stayed GREEN, 126 tests, 0
    /// failures, INCLUDING this test. A check that stays green while the hazard it
    /// names is open is the failure this file is full of warnings about.
    fn cell_result_that_located_nothing() -> CellResult {
        CellResult {
            schema: CELL_RESULT_SCHEMA,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            hermit_sha: "sha".into(),
            binary_build_sha: None,
            source_tree_dirty: false,
            binary_sha256: None,
            test_sha256: "digest".into(),
            test: "fixture/test".into(),
            category: "fixture".into(),
            lane: "portable".into(),
            mode: "verify".into(),
            backend: Some("ptrace".into()),
            classification: "required".into(),
            outcome: "PASS".into(),
            result: Some(ObservedResult::Pass),
            failure_class: None,
            error_kind: None,
            timeout_seconds: 3,
            execution_cpu_timeout_seconds: Some(1),
            execution_wall_timeout_seconds: Some(3),
            duration_ms: Some(1),
            cpu_usage_usec: Some(1),
            runtime: None,
            log_level: None,
            effective_args: Vec::new(),
            argv: vec!["hermit".into()],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && hermit".into(),
            relaxations: Vec::new(),
            execution_path: None,
            diversity: None,
            attempts: vec![attempt_with_sabre_evidence("evidence")],
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            reason: None,
            artifact_dir: "/repo/artifacts".into(),
        }
    }

    #[test]
    fn a_located_nothing_row_still_carries_all_divergence_fields() {
        let attempt = attempt_with_sabre_evidence("evidence");
        let rendered = serde_json::to_value(&attempt).expect("attempt serializes");
        let object = rendered.as_object().expect("attempt is a JSON object");
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized attempt that located nothing. \
                     Absence is how a consumer recognises a row written before this \
                     field existed; emitting it for a row that simply found nothing \
                     merges two different facts that no later reader can separate."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null when nothing was located, got {found}"
            );
        }

        // THE ROW LEVEL, which is what a `results.jsonl` line actually is and what
        // every count in the docstring above was measured over.
        let row = cell_result_that_located_nothing();
        let rendered = serde_json::to_value(&row).expect("row serializes");
        let object = rendered.as_object().expect("row is a JSON object");
        assert!(
            !object.contains_key("run_index"),
            "ordinary validate rows must not invent a pressure repetition"
        );
        assert_eq!(object["machine_shortname"], "fixture-host");
        assert_eq!(object["kernel_version"], "7.1.3-fixture");
        assert_eq!(
            object["host_capabilities"]["cpuid-faulting"]["present"],
            true
        );
        assert_eq!(object["host_capabilities"]["kvm"]["present"], false);
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized RESULT ROW that located nothing. \
                     A results.jsonl line is a CellResult, so this is the level every \
                     count in this test's docstring was measured over; absence here is \
                     how a reader recognises a row written before the field existed."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null on the row when nothing was located, got {found}"
            );
        }

        let decoded: CellResult =
            serde_json::from_value(rendered).expect("the producer-owned result type reads its row");
        assert_eq!(decoded.attempt, 1);
        assert_eq!(decoded.run_index, None);
        assert_eq!(decoded.attempts.len(), 1);

        let mut pressure_row = row;
        pressure_row.run_index = Some(4);
        let rendered = serde_json::to_value(&pressure_row).expect("pressure row serializes");
        assert_eq!(rendered["run_index"], 4);
        let decoded: CellResult = serde_json::from_value(rendered)
            .expect("the producer-owned result type reads a pressure row");
        assert_eq!(decoded.run_index, Some(4));
    }

    #[test]
    fn retained_verify_log_is_additive_attempt_evidence() {
        let mut attempt = attempt_with_sabre_evidence("");
        let historical = serde_json::to_value(&attempt).unwrap();
        assert!(historical.get("retained_verify_log").is_none());
        let decoded: AttemptResult = serde_json::from_value(historical).unwrap();
        assert!(decoded.retained_verify_log.is_none());

        attempt.retained_verify_log = Some(RetainedVerifyLog {
            relative_path: "retained/verify/1/run-1.log.gz".into(),
            role: RetainedVerifyLogRole::Run1,
            cell_id: CellId {
                test: "fixture/test".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            attempt: 1,
            uncompressed_sha256: "a".repeat(64),
            uncompressed_bytes: 11,
            compressed_sha256: "b".repeat(64),
            compressed_bytes: 12,
            peer_uncompressed_sha256: "c".repeat(64),
            peer_uncompressed_bytes: 13,
            compared_info_messages: 14,
        });
        let rendered = serde_json::to_value(&attempt).unwrap();
        let descriptor = rendered["retained_verify_log"].as_object().unwrap();
        assert_eq!(
            descriptor
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "attempt",
                "cell_id",
                "compared_info_messages",
                "compressed_bytes",
                "compressed_sha256",
                "peer_uncompressed_bytes",
                "peer_uncompressed_sha256",
                "relative_path",
                "role",
                "uncompressed_bytes",
                "uncompressed_sha256",
            ])
        );
        let decoded: AttemptResult = serde_json::from_value(rendered).unwrap();
        assert_eq!(decoded.retained_verify_log, attempt.retained_verify_log);
    }

    #[test]
    fn schema4_keeps_its_wall_field_and_adds_explicit_execution_bounds() {
        let row = cell_result_that_located_nothing();
        let mut rendered = serde_json::to_value(&row).expect("cell result serializes");
        assert_eq!(rendered["timeout_seconds"], 3);
        assert_eq!(rendered["execution_cpu_timeout_seconds"], 1);
        assert_eq!(rendered["execution_wall_timeout_seconds"], 3);
        row.require_current_timeout_policy().unwrap();

        let mut half_present = row.clone();
        half_present.execution_wall_timeout_seconds = None;
        assert!(half_present.validate_timeout_policy().is_err());
        let publication = std::env::temp_dir().join(format!(
            "hermit-runner-timeout-policy-publication-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&publication);
        assert!(append_result(&publication, &half_present).is_err());
        assert!(
            !publication.exists(),
            "a malformed current timeout policy reached the results file"
        );
        let mut wrong_cpu = row.clone();
        wrong_cpu.execution_cpu_timeout_seconds = Some(3);
        assert!(wrong_cpu.validate_timeout_policy().is_err());
        let mut wrong_wall = row.clone();
        wrong_wall.execution_wall_timeout_seconds = Some(2);
        assert!(wrong_wall.validate_timeout_policy().is_err());

        let object = rendered.as_object_mut().unwrap();
        object.remove("execution_cpu_timeout_seconds");
        object.remove("execution_wall_timeout_seconds");
        let retained: CellResult = serde_json::from_value(rendered)
            .expect("a schema-4 row written before the additive fields remains readable");
        assert_eq!(retained.timeout_seconds, 3);
        assert_eq!(retained.execution_cpu_timeout_seconds, None);
        assert_eq!(retained.execution_wall_timeout_seconds, None);
        retained.validate_timeout_policy().unwrap();
        assert!(retained.require_current_timeout_policy().is_err());
    }

    fn attempt_with_sabre_evidence(evidence: &str) -> AttemptResult {
        AttemptResult {
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            index: "1".into(),
            outcome: "PASS".into(),
            error_kind: None,
            status: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            cpu_usage_usec: Some(1),
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
            retained_verify_log: None,
            runtime: None,
            sabre_path_evidence: Some(evidence.into()),
            sabre_path_evidence_sha256: Some("b".into()),
            reason: None,
        }
    }

    #[test]
    fn current_cell_result_carries_result_and_failure_class() {
        let passed = serde_json::to_value(cell_result_that_located_nothing()).unwrap();
        assert_eq!(passed["result"], "pass");
        assert_eq!(passed["failure_class"], JsonValue::Null);

        let mut failed = cell_result_that_located_nothing();
        failed.outcome = "FAIL".into();
        failed.result = Some(ObservedResult::DeterminismFailure);
        failed.failure_class = Some(FailureClass::ProductFailure);
        let failed = serde_json::to_value(failed).unwrap();
        assert_eq!(failed["result"], "determinism-failure");
        assert_eq!(failed["failure_class"], "product_failure");

        let mut missing = cell_result_that_located_nothing();
        missing.outcome = "FAIL".into();
        missing.result = Some(ObservedResult::DeterminismFailure);
        missing.failure_class = None;
        assert_eq!(
            missing.require_current_classification().unwrap_err(),
            "FAIL result has no failure_class; current non-passes must be attributed"
        );

        let mut legacy = cell_result_that_located_nothing();
        legacy.outcome = "FAIL".into();
        legacy.result = None;
        legacy.failure_class = None;
        legacy
            .validate_recorded_classification()
            .expect("retained pre-field schema-4 row remains readable");
        assert!(legacy.require_current_classification().is_err());

        let mut error_with_product_result = cell_result_that_located_nothing();
        error_with_product_result.outcome = "ERROR".into();
        error_with_product_result.result = Some(ObservedResult::CrashError);
        error_with_product_result.failure_class = Some(FailureClass::ProductFailure);
        assert_eq!(
            error_with_product_result
                .require_current_classification()
                .unwrap_err(),
            "ERROR result must carry a non-product observation/classification, got (Some(CrashError), Some(ProductFailure))"
        );

        let mut timeout = cell_result_that_located_nothing();
        timeout.outcome = "ERROR".into();
        timeout.result = Some(ObservedResult::Timeout);
        timeout.failure_class = Some(FailureClass::NoResult);
        timeout.require_current_classification().unwrap();
    }

    #[test]
    fn framework_classifies_divergence_and_crash_before_pressure_reads_them() {
        let mut divergence = attempt_with_sabre_evidence("");
        divergence.outcome = "FAIL".into();
        divergence.verification_report = Some(
            r#"{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":1,"right":1},"first_divergent_scheduler_turn":4,"first_divergent_virtual_nanoseconds":7,"first_divergent_record":9,"first_divergent_syscall":2,"first_divergent_left_message":"left","first_divergent_right_message":"right"}"#
                .into(),
        );
        let divergence_result = observed_result(
            "verify",
            &divergence.outcome,
            std::slice::from_ref(&divergence),
            divergence.error_kind.as_deref(),
        );
        assert_eq!(divergence_result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(
            failure_class(
                &divergence.outcome,
                divergence_result,
                divergence.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );

        let mut crash = attempt_with_sabre_evidence("");
        crash.outcome = "FAIL".into();
        crash.status = Some(1);
        let crash_result = observed_result(
            "verify",
            &crash.outcome,
            std::slice::from_ref(&crash),
            crash.error_kind.as_deref(),
        );
        assert_eq!(crash_result, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(&crash.outcome, crash_result, crash.error_kind.as_deref()),
            Some(FailureClass::ProductFailure)
        );

        let mut invalidated = divergence.clone();
        invalidated.outcome = "ERROR".into();
        invalidated.error_kind = Some("infrastructure".into());
        let invalidated_result = observed_result(
            "verify",
            &invalidated.outcome,
            std::slice::from_ref(&invalidated),
            invalidated.error_kind.as_deref(),
        );
        assert_eq!(
            invalidated_result, None,
            "a later infrastructure failure must outrank an earlier product observation"
        );
        assert_eq!(
            failure_class(
                &invalidated.outcome,
                invalidated_result,
                invalidated.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut invalid_evidence = divergence;
        invalid_evidence.outcome = "ERROR".into();
        invalid_evidence.error_kind = Some("invalid-backend-evidence".into());
        let invalid_evidence_result = observed_result(
            "verify",
            &invalid_evidence.outcome,
            std::slice::from_ref(&invalid_evidence),
            invalid_evidence.error_kind.as_deref(),
        );
        assert_eq!(invalid_evidence_result, None);
        assert_eq!(
            failure_class(
                &invalid_evidence.outcome,
                invalid_evidence_result,
                invalid_evidence.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
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

        let unknown = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"unexpected":0}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[unknown]).unwrap_err();
        assert!(error.contains("unknown field `unexpected`"), "{error}");

        let missing = attempt_with_sabre_evidence(
            r#"{"schema":1,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[missing]).unwrap_err();
        assert!(
            error.contains("missing field `guest_rpc_observed`"),
            "{error}"
        );

        let future = attempt_with_sabre_evidence(
            r#"{"schema":2,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"future_field":"value"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        assert_eq!(
            summarize_sabre_path_evidence(&[future]).unwrap_err(),
            "invalid SaBRe path-evidence record: schema must be 1"
        );
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

    fn canonical_verification_report() -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: Verdict::Matched,
            no_result_reason: None,
            infrastructure_error: None,
            comparison: Some(crate::canonical_verdict::ComparisonReport {
                strictness: crate::canonical_verdict::LogCompareStrictness::Canonical,
                display_name: Some("BitwiseInfoV1".into()),
                compare_logs: true,
                compare_io_buffers: Some(true),
                log_scope: Some(crate::canonical_verdict::ComparedLogScope::Info),
                record_envelope: crate::canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                virtualize_time: Some(true),
                strip_lines: Some(false),
                canonicalize_addresses: Some(true),
                full_trace: Some(true),
                exact_remainder: Some(true),
                stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                canonicalizations: Some(vec!["host-address-to-first-appearance-ordinal/v1".into()]),
                ignore_lines: Some(false),
                skip_commit: Some(false),
                skip_detlog: Some(false),
            }),
            compared_log_messages: Some(crate::canonical_verdict::ComparedLogMessages {
                left: 1,
                right: 1,
            }),
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: Some(7),
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    fn nonzero_with_canonical_receipt(mode: &str) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-status-bracket-{}-{mode}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let report = serde_json::to_string(&canonical_verification_report()).unwrap();
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
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    /// Build one attempt from a real subprocess, so this exercises the same
    /// classification path used by an actual manifest sweep.
    fn attempt_from_script(backend: &str, script: &str, report: Option<&str>) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-backend-availability-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
        ];
        argv.push(report.unwrap_or("").to_string());
        argv.push(verdict.to_string_lossy().into_owned());
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/backend-availability".into(),
                mode: "verify".into(),
                backend: Some(backend.into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv,
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 10,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn skid_overshoot_receipt_is_an_infrastructure_error_with_comparison_evidence() {
        let mut report = canonical_verification_report();
        report.verified = false;
        report.bitwise_parity = false;
        report.verdict = Verdict::InfrastructureError;
        report.infrastructure_error = Some(InfrastructureError::SkidOvershoot { count: 2 });
        let report = serde_json::to_string(&report).unwrap();

        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&report),
        );

        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert_eq!(result.status, Some(122));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("2 HERMIT_SKID_OVERSHOOT")),
            "infrastructure error must retain the recorded count: {result:?}"
        );
        assert!(
            result
                .verification_report
                .as_deref()
                .is_some_and(|json| json.contains("\"comparison\":{")),
            "infrastructure error must retain the completed comparison: {result:?}"
        );

        let mut before_comparison = canonical_verification_report();
        before_comparison.verified = false;
        before_comparison.bitwise_parity = false;
        before_comparison.verdict = Verdict::InfrastructureError;
        before_comparison.infrastructure_error =
            Some(InfrastructureError::SkidOvershoot { count: 1 });
        before_comparison.comparison = None;
        before_comparison.compared_log_messages = None;
        let before_comparison = serde_json::to_string(&before_comparison).unwrap();
        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&before_comparison),
        );
        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("1 HERMIT_SKID_OVERSHOOT")),
            "comparison-null infrastructure error must retain its cause: {result:?}"
        );
    }

    /// A backend this runner could not start and a backend that ran but recorded
    /// no comparison must stay visible while carrying different error kinds.
    ///
    /// The old shared incomplete-verification-evidence kind turned an unstaged
    /// backend into an apparent product defect. Classifying every nonzero exit
    /// as unavailable would be the same bug with the sign flipped, so this
    /// bracket exercises both directions through real subprocesses.
    #[test]
    fn an_unavailable_backend_is_not_reported_as_a_silent_one() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: HERMIT_SABRE_BINARY=/nonexistent/sabre is not an executable file' >&2; exit 1",
            Some(no_result),
        );
        let silent = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; exit 0",
            Some(no_result),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(silent.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &unavailable.outcome,
                observed_result(
                    "verify",
                    &unavailable.outcome,
                    std::slice::from_ref(&unavailable),
                    unavailable.error_kind.as_deref(),
                ),
                unavailable.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert_eq!(
            failure_class(
                &silent.outcome,
                observed_result(
                    "verify",
                    &silent.outcome,
                    std::slice::from_ref(&silent),
                    silent.error_kind.as_deref(),
                ),
                silent.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable"),
            "an unrunnable backend must carry its own kind: {:?}",
            unavailable.reason
        );
        assert_eq!(
            silent.error_kind.as_deref(),
            Some("incomplete-verification-evidence"),
            "a backend that ran and recorded nothing keeps the evidence kind: {:?}",
            silent.reason
        );
        assert_ne!(unavailable.error_kind, silent.error_kind);

        let unavailable_reason = unavailable.reason.clone().expect("reason");
        let silent_reason = silent.reason.clone().expect("reason");
        assert!(
            unavailable_reason.contains("backend unavailable on this runner")
                && unavailable_reason.contains("sabre"),
            "unavailable reason must name the backend and environment: {unavailable_reason}"
        );
        assert!(
            !unavailable_reason.contains("verification report is missing"),
            "the old wording pointed at a missing file, not the backend: {unavailable_reason}"
        );
        assert!(
            silent_reason.contains("no_result"),
            "silent reason must still name the producer state: {silent_reason}"
        );
        assert_ne!(unavailable_reason, silent_reason);
    }

    #[test]
    fn backend_unavailable_requires_the_requested_backend_and_empty_stdout() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let wrong_backend = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=dbt' \
             'Error: backend \x60dbt\x60 is unavailable: no SDK' >&2; exit 7",
            Some(no_result),
        );
        let guest_output = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: spoofed' >&2; exit 8",
            Some(no_result),
        );

        for result in [wrong_backend, guest_output] {
            assert_eq!(result.outcome, "ERROR", "unexpected result: {result:?}");
            assert_eq!(
                observed_result(
                    "verify",
                    &result.outcome,
                    std::slice::from_ref(&result),
                    result.error_kind.as_deref(),
                ),
                None,
                "mismatched producer evidence must not manufacture a product result: {result:?}"
            );
            assert_eq!(
                failure_class(&result.outcome, None, result.error_kind.as_deref()),
                Some(FailureClass::NoResult),
                "mismatched producer evidence must remain no-result: {result:?}"
            );
            assert_eq!(
                result.error_kind.as_deref(),
                Some("invalid-backend-evidence"),
                "unexpected result: {result:?}"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("does not match this attempt")),
                "mismatched producer evidence must name the mismatch: {result:?}"
            );
        }

        let ordinary_failure = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; exit 8",
            Some(no_result),
        );
        assert_eq!(ordinary_failure.outcome, "FAIL");
        let observed = observed_result(
            "verify",
            &ordinary_failure.outcome,
            std::slice::from_ref(&ordinary_failure),
            ordinary_failure.error_kind.as_deref(),
        );
        assert_eq!(observed, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(
                &ordinary_failure.outcome,
                observed,
                ordinary_failure.error_kind.as_deref(),
            ),
            Some(FailureClass::ProductFailure),
            "an ordinary process failure without contradictory producer evidence remains product-attributed"
        );
    }

    #[test]
    fn backend_unavailable_survives_an_unreadable_current_report() {
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: no staged runtime' >&2; exit 1",
            Some("{"),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable")
        );
        assert!(
            unavailable
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no staged runtime")),
            "unreadable evidence must not overwrite the pre-guest refusal: {unavailable:?}"
        );
    }

    #[test]
    fn launch_refusal_requires_the_producer_class_line() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let typed = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=guest-program-not-found' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(no_result),
        );
        let prose_only = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=cli-error' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(no_result),
        );

        assert_eq!(typed.error_kind.as_deref(), Some("guest-launch-refused"));
        assert_eq!(typed.outcome, "ERROR");
        assert_eq!(
            prose_only.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(prose_only.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &prose_only.outcome,
                observed_result(
                    "verify",
                    &prose_only.outcome,
                    std::slice::from_ref(&prose_only),
                    prose_only.error_kind.as_deref(),
                ),
                prose_only.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult),
            "English launch prose without the producer class must remain no-result"
        );
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

    #[test]
    fn chaos_assertion_does_not_relabel_an_incomplete_population_as_a_product_failure() {
        let mut outcome = "ERROR".to_string();
        let mut reason = Some("seed-31 timed out before producing a comparison".to_string());
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            "chaos distinct=8 passes=31 failures=1 normalized_entropy=0.9180".into(),
        );
        assert_eq!(outcome, "ERROR");
        assert_eq!(
            reason.as_deref(),
            Some("seed-31 timed out before producing a comparison")
        );

        let mut outcome = "PASS".to_string();
        let mut reason = None;
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            "chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000".into(),
        );
        assert_eq!(outcome, "FAIL");
        assert_eq!(
            reason.as_deref(),
            Some("chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000")
        );
    }

    fn no_result_with_exit_status(
        status: i32,
        reason: crate::canonical_verdict::NoResultReason,
    ) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-no-result-bracket-{}-{status}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let mut report = VerificationReport::no_result();
        report.no_result_reason = Some(reason);
        let report = serde_json::to_string(&report).unwrap();
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
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn no_result_preserves_the_process_outcome_distinction() {
        let failed = no_result_with_exit_status(
            7,
            crate::canonical_verdict::NoResultReason::FirstRunRejected {
                exit_code: Some(7),
                signal: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
        );
        assert_eq!(failed.outcome, "FAIL");
        assert_eq!(
            observed_result(
                "verify",
                &failed.outcome,
                std::slice::from_ref(&failed),
                failed.error_kind.as_deref(),
            ),
            Some(ObservedResult::CrashError)
        );
        assert_eq!(
            failure_class(
                &failed.outcome,
                Some(ObservedResult::CrashError),
                failed.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );
        assert_eq!(failed.error_kind, None);
        assert_eq!(failed.status, Some(7));
        assert!(
            failed
                .reason
                .as_deref()
                .unwrap()
                .contains("before producing a terminal comparison")
        );

        let not_run =
            no_result_with_exit_status(127, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(not_run.outcome, "ERROR");
        assert_eq!(
            failure_class(&not_run.outcome, None, not_run.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            not_run.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(not_run.status, Some(127));

        let unknown =
            no_result_with_exit_status(0, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(unknown.outcome, "ERROR");
        assert_eq!(
            failure_class(&unknown.outcome, None, unknown.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unknown.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(unknown.status, Some(0));
    }

    #[test]
    fn failed_preparation_surfaces_its_own_diagnostic() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-diagnostic-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &[
                "-c".into(),
                "printf 'error: failed to write fixture: Permission denied\\n' >&2; exit 17".into(),
            ],
            Instant::now() + Duration::from_secs(5),
            5,
        )
        .unwrap_err();
        assert!(error.contains("fixture preparation failed for /bin/sh: exited 17"));
        assert!(error.contains("error: failed to write fixture: Permission denied"));
        assert!(!error.contains("no output was captured"));

        fs::remove_file(cell_dir.join("captures/prepare.stderr")).unwrap();
        let empty = with_diagnostic(
            "fixture preparation failed".into(),
            &cell_dir.join("captures"),
        );
        assert!(empty.contains("no output was captured"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_uses_the_named_cells_wall_deadline() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
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
            timeout_seconds: 1,
            cpu_timeout_seconds: 1,
        };

        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "sleep 60".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
        )
        .unwrap_err();
        assert!(
            error.contains("cell exceeded 1 s during fixture preparation"),
            "{error}"
        );
        let result = infrastructure_error_result(&context, &cell, error);
        assert_eq!(result.test, "fixture/test");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .unwrap()
                .contains("cell exceeded 1 s during fixture preparation")
        );

        run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "true".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
        )
        .expect("healthy fixture preparation must finish silently under the same bound");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_does_not_consume_the_guest_execution_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-separate-preparation-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let program = root.join("fixture.sh");
        fs::write(
            &program,
            "#!/bin/sh\ncase \"$1\" in\n  --prepare) sleep 1.1 ;;\n  --run) sleep 1.1; printf 'complete\\n' ;;\n  *) exit 64 ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();

        let mut test = recipe(true);
        test.id = "fixture/separate-preparation-budget".into();
        test.program = Some("fixture.sh".into());
        test.direct = None;
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(1);
        mode.assert = Some(Assertions {
            min_distinct: Some(1),
            ..Assertions::default()
        });
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 2,
            cpu_timeout_seconds: 1,
        };
        let context = run_context(&root);

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.outcome, "PASS", "unexpected result: {result:?}");
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.attempts.len(), 1);
        assert!(!result.attempts[0].timed_out);
        assert_eq!(result.attempts[0].stdout, "complete\n");
        assert!(
            result.duration_ms.is_some_and(|duration| duration >= 2_000),
            "the fixture did not exercise separate preparation and execution time: {result:?}"
        );
        fs::remove_dir_all(root).unwrap();
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
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: true,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
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

    fn build_info_json(git_sha: &str) -> Vec<u8> {
        serde_json::to_vec(&detcore_model::build_info::BuildInfo {
            schema: detcore_model::build_info::BuildInfo::SCHEMA,
            version: "0.2.0".into(),
            build_date: Some("2026-08-25".into()),
            git_sha: git_sha.into(),
            features: detcore_model::build_info::BuildFeatures {
                dbt: false,
                e9patch: false,
                sabre: false,
            },
        })
        .unwrap()
    }

    /// The binary's own revision is read from its typed record, including the
    /// dirty marker, which the checkout HEAD cannot supply because it describes
    /// a different tree at a different time.
    #[test]
    fn binary_build_sha_is_read_from_the_typed_build_record() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e-dirty")).as_deref(),
            Some("351cd3603f7e-dirty"),
            "a dirty build must keep saying so; that is the fact the checkout cannot supply"
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("3d85028b3bca")).as_deref(),
            Some("3d85028b3bca")
        );
        // 40-hex is equally acceptable; the width is not the contract.
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e537297067e07a20c5ccf7a23c0e0"))
                .as_deref(),
            Some("351cd3603f7e537297067e07a20c5ccf7a23c0e0")
        );
    }

    /// "I do not know" is a value the binary can state, and it must survive as
    /// one rather than collapsing into "I could not ask".
    #[test]
    fn an_unknown_revision_is_preserved_not_dropped() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("unknown")).as_deref(),
            Some("unknown")
        );
    }

    /// Nothing recognisable must yield `None`, never a guess. A provenance that
    /// could not be established must not be reported as one that matched.
    #[test]
    fn malformed_or_unsupported_build_records_establish_nothing() {
        for bytes in [
            b"".as_slice(),
            br#"{"schema":1}"#,
            br#"{"schema":2,"version":"0.2.0","build_date":null,"git_sha":"351cd3603f7e","features":{"dbt":false,"e9patch":false,"sabre":false}}"#,
            &build_info_json("zzzz"),
        ] {
            assert_eq!(
                parse_binary_build_info(bytes),
                None,
                "must not invent provenance from {}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Mutating the producer's typed value must move the consumer's value.
    #[test]
    fn binary_build_sha_follows_the_typed_field() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e")),
            Some("351cd3603f7e".into())
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("aaaaaaaaaaaa")),
            Some("aaaaaaaaaaaa".into())
        );
    }

    /// The two fields answer different questions and must be independently
    /// settable, because in practice they disagree: the checkout moves during a
    /// rebase-and-rerun loop while the binary on disk does not.
    #[test]
    fn checkout_sha_and_binary_provenance_are_separate_facts() {
        let checkout = "affda5d9840baeb60c5f5aa9c7b0ff5560e81ef3";
        let built_from = parse_binary_build_info(&build_info_json("351cd3603f7e-dirty"))
            .expect("typed build record parses");
        assert_ne!(
            checkout,
            built_from.trim_end_matches("-dirty"),
            "the case this field exists for: the harness stood at one commit and ran a \
             binary built from another"
        );
        assert!(
            !checkout.contains("-dirty") && built_from.ends_with("-dirty"),
            "and the checkout cannot express the build tree's dirtiness at all"
        );
    }

    /// REGRESSION CHECK FOR THE ORDINARY-HOST FALLBACK.
    ///
    /// Without the default the guest inherits the HOST's cwd verbatim. Measured on
    /// 2026-08-26 at main f4de43461a, 200 runs rotating across four checkouts:
    ///   default off -> 4 distinct guest `pwd` values
    ///   default on  -> 1, `/tmp/test`, and the directory empty on all 200
    /// The explicit hermetic path below supersedes this with a tmpfs at `/test`.
    /// This fallback remains for direct non-hermetic harness runs.
    #[test]
    fn fixed_workdir_is_bound_by_default_and_withheld_where_it_would_lie() {
        let source = Path::new("/cells/x/workdir/attempt-7");
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, None, None, Some(source));
        assert_eq!(
            argv,
            vec![
                "--bind=/cells/x/workdir/attempt-7:/tmp/test".to_string(),
                "--workdir".to_string(),
                "/tmp/test".to_string(),
            ],
            "the fallback must bind a fresh per-attempt directory AND chdir into it"
        );

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, None, Some("/tmp"), Some(source));
        assert_eq!(argv, vec!["--workdir".to_string(), "/tmp".to_string()]);

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, None, None, None);
        assert!(
            argv.is_empty(),
            "a cell that cannot honour a workdir must be given none, got {argv:?}"
        );
    }

    #[test]
    fn the_hermetic_lane_outranks_manifest_and_fallback_workdirs() {
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(
            &mut argv,
            Some(Path::new(HERMETIC_TEST_WORKDIR)),
            Some("/tmp"),
            Some(Path::new("/cells/x/workdir/attempt-7")),
        );
        assert_eq!(
            argv,
            vec![
                format!("--mount=type=tmpfs,target={HERMETIC_TEST_WORKDIR}"),
                "--workdir".to_string(),
                HERMETIC_TEST_WORKDIR.to_string(),
            ],
            "the explicit hermetic request must suppress both the manifest override and fallback"
        );
        assert!(
            !argv.iter().any(|arg| arg.starts_with("--bind")),
            "the ordinary-host bind leaked into the /test path: {argv:?}"
        );
    }

    #[test]
    fn test_workdir_support_matches_the_modes_that_can_honour_it() {
        for mode in ["verify", "replay", "chaos", "custom"] {
            assert!(supports_test_workdir(mode, "ptrace"), "mode={mode}");
        }
        assert!(!supports_test_workdir("naked", "native"));
        assert!(!supports_test_workdir("verify", "dbt"));
    }

    #[test]
    fn selected_supported_cells_do_not_override_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let mut overrides = Vec::new();
        for document in &manifests.documents {
            for test in &document.test {
                for (mode, recipe) in &test.modes {
                    let selection = ci_selection(recipe).unwrap();
                    for backend in &recipe.backends_enabled {
                        if selection.selected(backend)
                            && supports_test_workdir(mode, backend)
                            && recipe.workdir.is_some()
                        {
                            overrides.push(format!("{}/{mode}/{backend}", test.id));
                        }
                    }
                }
            }
        }
        assert!(
            overrides.is_empty(),
            "scheduled supported cells must all use the hermetic /test workdir: {overrides:?}"
        );
    }

    #[test]
    fn every_selected_cell_can_honour_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let expected: JsonValue =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        let unsupported = expected["cells"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|cell| {
                let mode = cell["mode"].as_str().unwrap();
                let backend = cell["backend"].as_str().unwrap();
                (!supports_test_workdir(mode, backend))
                    .then(|| format!("{}/{mode}/{backend}", cell["test"].as_str().unwrap()))
            })
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "selected cells that cannot honour the hermetic /test gate: {unsupported:?}"
        );
    }

    #[test]
    fn attempt_labels_must_be_one_normal_path_component() {
        for attempt in [
            "",
            ".",
            "..",
            "/absolute",
            "../parent",
            "parent/child",
            "normal/",
        ] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Err(format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                ))
            );
        }
        for attempt in ["1", "attempt-7", "seed--42"] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Ok(PathBuf::from(format!("/cell/workdir/{attempt}")))
            );
        }
    }

    #[test]
    fn verify_pair_paths_are_distinct_and_stay_below_the_cell_directory() {
        let cell = Path::new("/result/run/cell");
        let run1 = verify_run_paths(cell, "2", VerifyRun::Run1).unwrap();
        let run2 = verify_run_paths(cell, "2", VerifyRun::Run2).unwrap();

        assert_eq!(run1.workdir, cell.join("workdir/2/run-1"));
        assert_eq!(run2.workdir, cell.join("workdir/2/run-2"));
        assert_eq!(run1.log, cell.join("captures/verify/2/run-1/detlog.log"));
        assert_eq!(run2.log, cell.join("captures/verify/2/run-2/detlog.log"));
        assert_ne!(run1, run2);
        for path in [
            &run1.workdir,
            &run1.log,
            &run1.result,
            &run1.stdout,
            &run1.stderr,
            &run1.summary,
            &run2.workdir,
            &run2.log,
            &run2.result,
            &run2.stdout,
            &run2.stderr,
            &run2.summary,
        ] {
            assert!(
                path.starts_with(cell),
                "{} escaped the cell",
                path.display()
            );
            assert!(
                !path.starts_with("/tmp"),
                "{} used host /tmp",
                path.display()
            );
        }
        assert!(verify_run_paths(cell, "../escape", VerifyRun::Run1).is_err());
    }

    #[test]
    fn harness_managed_verify_specs_are_two_ordinary_runs_with_fixed_test_binds() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let cell_dir = directory.path().join("results/cell");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&cell_dir).unwrap();
        let context =
            run_context(&root).with_scheduled_worker_capacity(ScheduledWorkerCapacity::new(8));
        let cell = ptrace_cell("verify");
        let run1 = build_verify_run_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run1,
            15,
        )
        .unwrap();
        let run2 = build_verify_run_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run2,
            15,
        )
        .unwrap();

        assert_eq!(run1.run, VerifyRun::Run1);
        assert_eq!(run2.run, VerifyRun::Run2);
        assert_ne!(run1.paths, run2.paths);
        assert_eq!(run1.execution.fixed_workdir_source, run1.paths.workdir);
        assert_eq!(run2.execution.fixed_workdir_source, run2.paths.workdir);
        for spec in [&run1, &run2] {
            let argv = &spec.execution.argv;
            assert!(argv.windows(2).any(|args| {
                args[0] == "--log-file" && args[1] == spec.paths.log.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--summary-json" && args[1] == spec.paths.summary.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--run-result-json" && args[1] == spec.paths.result.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--guest-stdout" && args[1] == spec.paths.stdout.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--guest-stderr" && args[1] == spec.paths.stderr.to_string_lossy()
            }));
            assert!(argv.iter().any(|arg| {
                arg == &format!(
                    "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
                    spec.paths.workdir.display()
                )
            }));
            assert!(
                argv.windows(2)
                    .any(|args| { args[0] == "--workdir" && args[1] == HERMETIC_TEST_WORKDIR })
            );
            assert!(!argv.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--verify" | "--verify-json" | "--verify-strict" | "--keep-logs"
                ) || arg.starts_with("--verify-log-dir")
            }));
            assert!(spec.paths.workdir.starts_with(&cell_dir));
            assert!(spec.paths.log.parent().unwrap().is_dir());
            File::create(&spec.paths.log)
                .expect("Hermit's pre-namespace host-opened log sink must be creatable");
            fs::remove_file(&spec.paths.log).unwrap();
            assert_minimal_guest_env(argv, &cell_dir.to_string_lossy(), "/test", "8");
        }
    }

    #[test]
    fn harness_managed_verify_executes_two_ptrace_runs_and_retains_run_1() {
        static REAL_HERMIT_RUN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = REAL_HERMIT_RUN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root must be readable");
        let hermit_bin = std::env::var_os("HERMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/debug/hermit"));
        assert!(
            hermit_bin.is_file(),
            "real Hermit binary {} is missing; build it or set HERMIT_BIN",
            hermit_bin.display()
        );

        let directory = tempfile::tempdir_in(root.join("target")).unwrap();
        let artifact_dir = directory.path().join("cell");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut context = run_context(&root);
        context.hermit_bin = hermit_bin;
        let cell = ptrace_cell("verify");
        let guest = vec![
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "test \"$PWD\" = /test || exit 91; ",
                "test ! -e verify-pair-marker || exit 92; ",
                "printf same > verify-pair-marker; ",
                "printf '/test\\nexact-stdout'; ",
                "printf exact-stderr >&2"
            )
            .into(),
        ];
        let run1_spec = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            guest.clone(),
            1,
            VerifyRun::Run1,
            30,
        )
        .unwrap();
        let run2_spec = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            guest,
            1,
            VerifyRun::Run2,
            30,
        )
        .unwrap();

        assert_ne!(run1_spec.paths.workdir, run2_spec.paths.workdir);
        for spec in [&run1_spec, &run2_spec] {
            assert!(spec.execution.argv.iter().any(|argument| {
                argument
                    == &format!(
                        "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
                        spec.paths.workdir.display()
                    )
            }));
            let attempt = execute_spec(&spec.execution).unwrap();
            assert_eq!(attempt.outcome, "PASS", "{}", attempt.stderr);
            assert_eq!(attempt.status, Some(0), "{}", attempt.stderr);
        }
        for workdir in [&run1_spec.paths.workdir, &run2_spec.paths.workdir] {
            assert_eq!(
                fs::read(workdir.join("verify-pair-marker")).unwrap(),
                b"same"
            );
        }

        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        for (spec, observation) in [(&run1_spec, &run1), (&run2_spec, &run2)] {
            assert_eq!(
                observation.result.disposition,
                crate::canonical_verdict::GuestDisposition::Exited { code: 0 }
            );
            assert_eq!(observation.stdout, b"/test\nexact-stdout");
            assert_eq!(observation.stderr, b"exact-stderr");
            assert_eq!(fs::read(&spec.paths.stdout).unwrap(), observation.stdout);
            assert_eq!(fs::read(&spec.paths.stderr).unwrap(), observation.stderr);
            let summary: RunSummary =
                serde_json::from_slice(&fs::read(&spec.paths.summary).unwrap()).unwrap();
            assert_eq!(RuntimeStats::from(&summary), observation.runtime);
        }

        let run1_raw = fs::read(&run1_spec.paths.log).unwrap();
        let run2_raw = fs::read(&run2_spec.paths.log).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let comparison = compared.comparison();
        assert_eq!(comparison.report.verdict, Verdict::Matched);
        assert!(comparison.report.bitwise_parity);
        assert!(comparison.stdout_equal);
        assert!(comparison.stderr_equal);
        assert!(comparison.disposition_equal);
        let counts = comparison
            .report
            .compared_log_messages
            .as_ref()
            .expect("canonical comparison must record both INFO counts");
        assert!(counts.left > 0 && counts.right > 0, "{counts:?}");

        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let results_path = directory.path().join("results.jsonl");
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(compared, &retention_budget, &results_path, &mut result)
                .unwrap();
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();

        let retained_path = artifact_dir.join(&publication.retained.relative_path);
        let compressed = fs::read(&retained_path).unwrap();
        assert!(compressed.len() >= 10);
        assert_eq!(&compressed[4..8], &[0, 0, 0, 0], "gzip mtime must be zero");
        assert_eq!(compressed[3] & 0x08, 0, "gzip must not carry a filename");
        assert_eq!(
            publication.retained.compressed_sha256,
            hex_digest(&compressed)
        );
        assert_eq!(
            publication.retained.compressed_bytes,
            u64::try_from(compressed.len()).unwrap()
        );
        assert_eq!(
            publication.retained.uncompressed_sha256,
            hex_digest(&run1_raw)
        );
        assert_eq!(
            publication.retained.peer_uncompressed_sha256,
            hex_digest(&run2_raw)
        );
        let mut decoded = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, run1_raw);
        let mut expected = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::new(6));
        expected.write_all(&run1_raw).unwrap();
        assert_eq!(compressed, expected.finish().unwrap());

        let published_result: CellResult =
            serde_json::from_str(fs::read_to_string(&results_path).unwrap().trim_end()).unwrap();
        assert_eq!(
            published_result.attempts[0].retained_verify_log,
            Some(publication.retained.clone())
        );
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());

        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        assert!(retained_path.is_file());
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();
    }

    #[test]
    fn harness_managed_verify_refuses_backends_without_both_required_channels() {
        let context = run_context(Path::new("/repo"));
        for (backend, expected) in [
            (
                "dbt",
                "harness-managed DBT verify cannot bind its isolated work directory at /test",
            ),
            (
                "sabre",
                "harness-managed SaBRe verify has no authoritative ordinary-run log sink",
            ),
        ] {
            let mut cell = ptrace_cell("verify");
            cell.id.backend = Some(backend.into());
            let error = build_verify_run_spec(
                &context,
                &cell,
                PathBuf::from("/results/cell"),
                vec!["/bin/true".into()],
                1,
                VerifyRun::Run1,
                15,
            )
            .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    fn write_verify_run_fixture(
        spec: &VerifyRunSpec,
        disposition: crate::canonical_verdict::GuestDisposition,
        stdout: &[u8],
        stderr: &[u8],
        log_body: &str,
    ) {
        let paths = &spec.paths;
        fs::create_dir_all(paths.log.parent().unwrap()).unwrap();
        fs::create_dir_all(&paths.workdir).unwrap();
        fs::write(&paths.stdout, stdout).unwrap();
        fs::write(&paths.stderr, stderr).unwrap();
        fs::write(&paths.log, log_body).unwrap();
        let result = GuestRunResult {
            schema: GuestRunResult::SCHEMA,
            disposition,
            determinism: spec.expected_determinism,
            stdout: crate::canonical_verdict::CapturedGuestStream {
                bytes: u64::try_from(stdout.len()).unwrap(),
                sha256: hex_digest(stdout),
            },
            stderr: crate::canonical_verdict::CapturedGuestStream {
                bytes: u64::try_from(stderr.len()).unwrap(),
                sha256: hex_digest(stderr),
            },
        };
        fs::write(&paths.result, serde_json::to_vec(&result).unwrap()).unwrap();
        fs::write(
            &paths.summary,
            serde_json::to_vec(&RunSummary::default()).unwrap(),
        )
        .unwrap();
    }

    fn verify_pair_fixture(
        artifact_dir: &Path,
        run1_log: &str,
        run2_log: &str,
        detlog_io_buffers: bool,
    ) -> (VerifyRunSpec, VerifyRunSpec) {
        fs::create_dir_all(artifact_dir).unwrap();
        let root = artifact_dir.parent().unwrap();
        let context = run_context(root);
        let mut cell = ptrace_cell("verify");
        if !detlog_io_buffers {
            let mode = cell.test.modes.get_mut("verify").unwrap();
            mode.compare_io_buffers = Some(false);
            mode.compare_io_buffers_disabled_reason = Some("fixture opt-out".into());
        }
        let run1 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.to_owned(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run1,
            15,
        )
        .unwrap();
        let run2 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.to_owned(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run2,
            15,
        )
        .unwrap();
        let disposition = crate::canonical_verdict::GuestDisposition::Exited { code: 0 };
        write_verify_run_fixture(&run1, disposition, b"out", b"err", run1_log);
        write_verify_run_fixture(&run2, disposition, b"out", b"err", run2_log);
        (run1, run2)
    }

    fn structured_info_record(body: &str) -> String {
        format!(
            "Apr 09 06:08:01.100  INFO detcore: {body}{}\n",
            detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Other)
        )
    }

    fn verify_log_retention_budget(
        retention_root: &Path,
        maximum_total_compressed_bytes: u64,
    ) -> VerifyLogRetentionBudget {
        let results_path = retention_root.join("results.jsonl");
        prepare_result_path(&results_path).unwrap();
        VerifyLogRetentionBudget::open(
            retention_root,
            results_path,
            VerifyLogRetentionPolicy::new(maximum_total_compressed_bytes),
        )
        .unwrap()
    }

    fn verify_cell_result(spec: &VerifyRunSpec) -> CellResult {
        let mut result = cell_result_that_located_nothing();
        result.test = spec.execution.id.test.clone();
        result.mode = spec.execution.id.mode.clone();
        result.backend = spec.execution.id.backend.clone();
        result.attempt = spec.attempt;
        result.artifact_dir = spec.execution.cell_dir.to_string_lossy().into_owned();
        result.argv = spec.execution.argv.clone();
        result.guest_argv = spec.execution.guest_argv.clone();
        result.env = spec.execution.env.clone();
        result.cwd = spec.execution.cwd.to_string_lossy().into_owned();
        result.shell_command = shell_command(
            &spec.execution.cwd.to_string_lossy(),
            &spec.execution.env,
            &spec.execution.argv,
        );
        let attempt = &mut result.attempts[0];
        attempt.index = "1".into();
        attempt.argv = result.argv.clone();
        attempt.guest_argv = result.guest_argv.clone();
        attempt.env = result.env.clone();
        attempt.cwd = result.cwd.clone();
        attempt.shell_command = result.shell_command.clone();
        attempt.sabre_path_evidence = None;
        attempt.sabre_path_evidence_sha256 = None;
        attempt.retained_verify_log = None;
        result
    }

    fn deterministic_gzip_size(bytes: &[u8]) -> u64 {
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::new(6));
        encoder.write_all(bytes).unwrap();
        u64::try_from(encoder.finish().unwrap().len()).unwrap()
    }

    #[test]
    fn harness_pair_loader_binds_sidecars_to_exact_bytes_and_shared_comparison() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, run2_spec) = verify_pair_fixture(&root, &log, &log, true);

        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::Matched);
        assert!(compared.comparison.report.bitwise_parity);
        assert_eq!(
            compared.comparison.report.compared_log_messages,
            Some(crate::canonical_verdict::ComparedLogMessages { left: 1, right: 1 })
        );

        fs::write(&run2_spec.paths.stdout, b"forged after sidecar").unwrap();
        let error = load_verify_run(&run2_spec)
            .expect_err("captured bytes changed after the sidecar must be refused");
        assert!(
            error.contains("stdout does not match its typed result"),
            "{error}"
        );
    }

    #[test]
    fn harness_pair_policy_comes_from_both_typed_run_results() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, run2_spec) = verify_pair_fixture(&root, &log, &log, false);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::Matched);
        assert!(!compared.comparison.report.bitwise_parity);
        assert_eq!(
            compared
                .comparison
                .report
                .comparison
                .as_ref()
                .unwrap()
                .compare_io_buffers,
            Some(false)
        );

        let mut forged_spec = run1_spec;
        forged_spec
            .execution
            .argv
            .retain(|argument| argument != "--no-detlog-io-buffers");
        let error = load_verify_run(&forged_spec)
            .expect_err("a command that omits its recorded opt-out must be refused");
        assert!(
            error.contains("does not match its detlog_io_buffers"),
            "{error}"
        );
    }

    #[test]
    fn harness_pair_requires_a_complete_run_summary() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&root, &log, &log, true);
        fs::remove_file(&run1_spec.paths.summary).unwrap();
        let missing = load_verify_run(&run1_spec).unwrap_err();
        assert!(missing.contains("summary.json"), "{missing}");

        fs::write(&run1_spec.paths.summary, b"not json").unwrap();
        let malformed = load_verify_run(&run1_spec).unwrap_err();
        assert!(malformed.contains("cannot parse"), "{malformed}");
    }

    #[test]
    fn harness_pair_loader_refuses_symlink_and_non_regular_sidecars() {
        for selected in ["result", "stdout", "stderr", "summary"] {
            let directory = tempfile::tempdir().unwrap();
            let artifact_dir = directory.path().join(selected);
            let log = structured_info_record("DETLOG stable");
            let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);
            let selected_path = match selected {
                "result" => &run1_spec.paths.result,
                "stdout" => &run1_spec.paths.stdout,
                "stderr" => &run1_spec.paths.stderr,
                "summary" => &run1_spec.paths.summary,
                _ => unreachable!(),
            };
            let target = artifact_dir.join("preserved-target");
            fs::write(&target, b"preserve me").unwrap();
            fs::remove_file(selected_path).unwrap();
            std::os::unix::fs::symlink(&target, selected_path).unwrap();

            let error = load_verify_run(&run1_spec).unwrap_err();
            assert!(error.contains("cannot open verify ordinary-run"), "{error}");
            assert_eq!(fs::read(&target).unwrap(), b"preserve me");
            assert!(
                fs::symlink_metadata(selected_path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("non-regular");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);
        fs::remove_file(&run1_spec.paths.summary).unwrap();
        fs::create_dir(&run1_spec.paths.summary).unwrap();
        let error = load_verify_run(&run1_spec).unwrap_err();
        assert!(error.contains("is not a regular file"), "{error}");
    }

    #[test]
    fn harness_pair_loader_refuses_each_oversize_sidecar_before_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);

        for (name, limits) in [
            (
                "result",
                VerifyRunReadLimits {
                    result: 1,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "stdout",
                VerifyRunReadLimits {
                    stdout: 2,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "stderr",
                VerifyRunReadLimits {
                    stderr: 2,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "summary",
                VerifyRunReadLimits {
                    summary: 1,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
        ] {
            let error = load_verify_run_with_limits(&run1_spec, limits).unwrap_err();
            assert!(
                error.contains("exceeds the") && error.contains(name),
                "{error}"
            );
        }
    }

    #[test]
    fn retained_verify_log_is_deterministic_bound_and_verified_before_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG retained");
        let mut compressed = Vec::new();

        for name in ["first", "second"] {
            let artifact_dir = directory.path().join(name);
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let run1 = load_verify_run(&run1_spec).unwrap();
            let run2 = load_verify_run(&run2_spec).unwrap();
            let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
            let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
            let results_path = retention_budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            let publication = retain_verify_log_with_limit(
                compared,
                4096,
                &retention_budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks::default(),
            )
            .unwrap();
            let retained = &publication.retained;

            assert_eq!(retained.role, RetainedVerifyLogRole::Run1);
            assert_eq!(retained.cell_id, run1_spec.execution.id);
            assert_eq!(retained.attempt, 1);
            assert_eq!(retained.compared_info_messages, 1);
            assert_eq!(retained.uncompressed_bytes, body.len() as u64);
            assert_eq!(retained.peer_uncompressed_bytes, body.len() as u64);
            assert_eq!(
                retained.uncompressed_sha256,
                retained.peer_uncompressed_sha256
            );
            assert!(run1_spec.paths.log.exists());
            assert!(run2_spec.paths.log.exists());
            verify_retained_verify_log_with_limit(
                &artifact_dir,
                retained,
                &run1_spec.execution.id,
                1,
                4096,
            )
            .unwrap();

            let bytes = fs::read(artifact_dir.join(&retained.relative_path)).unwrap();
            assert_eq!(&bytes[4..8], &[0, 0, 0, 0], "gzip mtime must be zero");
            assert_eq!(bytes[3] & 0x08, 0, "gzip must not carry a filename");
            compressed.push(bytes);
            cleanup_verify_log_sources_with(&artifact_dir, &publication, 4096, |path| {
                fs::remove_file(path)
            })
            .unwrap();
            assert!(!run1_spec.paths.log.exists());
            assert!(!run2_spec.paths.log.exists());
        }
        assert_eq!(compressed[0], compressed[1]);
    }

    #[test]
    fn aggregate_retention_budget_accepts_exact_bound_and_refuses_one_byte_over() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG aggregate retention bound");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());

        let exact_root = directory.path().join("exact-root");
        fs::create_dir(&exact_root).unwrap();
        let exact_artifact_dir = exact_root.join("exact");
        let (exact_run1, exact_run2) = verify_pair_fixture(&exact_artifact_dir, &body, &body, true);
        let exact_pair = compare_verify_runs(
            &exact_run1,
            load_verify_run(&exact_run1).unwrap(),
            &exact_run2,
            load_verify_run(&exact_run2).unwrap(),
        )
        .unwrap();
        let exact_budget = verify_log_retention_budget(&exact_root, compressed_bytes);
        let mut exact_result = verify_cell_result(&exact_run1);
        let exact_results_path = exact_budget.results_path.clone();
        let exact_publication = publish_retained_verify_log(
            exact_pair,
            &exact_budget,
            &exact_results_path,
            &mut exact_result,
        )
        .unwrap();
        assert_eq!(
            exact_publication.retained.compressed_bytes,
            compressed_bytes
        );
        assert!(
            exact_artifact_dir
                .join(&exact_publication.retained.relative_path)
                .is_file()
        );
        assert_eq!(
            exact_budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );

        let maximum = compressed_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_sub(1))
            .unwrap();
        let shared_root = directory.path().join("shared-root");
        fs::create_dir(&shared_root).unwrap();
        let first_artifact_dir = shared_root.join("first");
        let (first_run1, first_run2) = verify_pair_fixture(&first_artifact_dir, &body, &body, true);
        let first_pair = compare_verify_runs(
            &first_run1,
            load_verify_run(&first_run1).unwrap(),
            &first_run2,
            load_verify_run(&first_run2).unwrap(),
        )
        .unwrap();
        let shared_budget = verify_log_retention_budget(&shared_root, maximum);
        let mut first_result = verify_cell_result(&first_run1);
        let first_results_path = shared_budget.results_path.clone();
        let first_publication = publish_retained_verify_log(
            first_pair,
            &shared_budget,
            &first_results_path,
            &mut first_result,
        )
        .unwrap();
        let first_retained = first_artifact_dir.join(&first_publication.retained.relative_path);
        let first_results_bytes = fs::read(&first_results_path).unwrap();

        let refused_artifact_dir = shared_root.join("refused");
        let (refused_run1, refused_run2) =
            verify_pair_fixture(&refused_artifact_dir, &body, &body, true);
        let refused_pair = compare_verify_runs(
            &refused_run1,
            load_verify_run(&refused_run1).unwrap(),
            &refused_run2,
            load_verify_run(&refused_run2).unwrap(),
        )
        .unwrap();
        let required = compressed_bytes.checked_mul(2).unwrap();
        assert_eq!(required, maximum.checked_add(1).unwrap());
        let mut refused_result = verify_cell_result(&refused_run1);
        let refused_results_path = shared_budget.results_path.clone();
        let error = publish_retained_verify_log(
            refused_pair,
            &shared_budget,
            &refused_results_path,
            &mut refused_result,
        )
        .unwrap_err();
        assert!(error.contains("exceeding the"), "{error}");
        assert!(error.contains(&format!("require {required} compressed bytes")));
        assert!(error.contains(&format!("{}-byte aggregate limit", maximum)));
        assert_eq!(
            shared_budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );
        assert!(
            first_retained.is_file(),
            "an earlier log must not be evicted"
        );
        assert!(refused_run1.paths.log.is_file());
        assert!(refused_run2.paths.log.is_file());
        let refused_attempt_dir = refused_artifact_dir.join("retained/verify/1");
        assert!(
            !refused_attempt_dir.join("run-1.log.gz").exists(),
            "an over-limit pair must not publish a retained log"
        );
        assert!(
            !refused_artifact_dir.join("retained").exists(),
            "an over-limit staging file and its empty directory chain must be removed before its charge is released"
        );
        assert!(refused_result.attempts[0].retained_verify_log.is_none());
        assert_eq!(
            fs::read(&refused_results_path).unwrap(),
            first_results_bytes
        );
    }

    #[test]
    fn retained_verify_log_transaction_rolls_back_before_descriptor_publication() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG transaction rollback");
        for (name, failure) in [
            (
                "before-staging-file-creation",
                VerifyLogTransactionFailurePoint::BeforeStagingFileCreation,
            ),
            (
                "after-rename",
                VerifyLogTransactionFailurePoint::AfterFinalRename,
            ),
            (
                "after-directory-sync",
                VerifyLogTransactionFailurePoint::AfterDirectorySync,
            ),
            (
                "after-final-verification",
                VerifyLogTransactionFailurePoint::AfterFinalVerification,
            ),
            (
                "descriptor-publication-boundary",
                VerifyLogTransactionFailurePoint::BeforeResultPublication,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = retention_root.join("results.jsonl");
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    failure: Some(failure),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(error.contains("injected failure"), "{error}");
            assert!(run1_spec.paths.log.is_file());
            assert!(run2_spec.paths.log.is_file());
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&results_path).unwrap(), b"");
            let retained_attempt_dir = artifact_dir.join("retained/verify/1");
            assert!(!retained_attempt_dir.join("run-1.log.gz").exists());
            assert!(!artifact_dir.join("retained").exists());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }

        for (name, publication_failure) in [
            (
                "result-temporary-write",
                ResultPublicationFailurePoint::TemporaryWrite,
            ),
            (
                "result-temporary-file-sync",
                ResultPublicationFailurePoint::TemporaryFileSync,
            ),
            (
                "result-before-rename",
                ResultPublicationFailurePoint::BeforeRename,
            ),
            (
                "result-parent-directory-sync",
                ResultPublicationFailurePoint::ParentDirectorySync,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = retention_root.join("results.jsonl");
            let previous = cell_result_that_located_nothing();
            append_result(&results_path, &previous).unwrap();
            let previous_bytes = fs::read(&results_path).unwrap();
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    result_publication_failure: Some(publication_failure),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(error.contains("injected failure"), "{error}");
            assert_eq!(fs::read(&results_path).unwrap(), previous_bytes);
            assert!(result.attempts[0].retained_verify_log.is_none());
            let retained_attempt_dir = artifact_dir.join("retained/verify/1");
            assert!(!retained_attempt_dir.join("run-1.log.gz").exists());
            assert!(!artifact_dir.join("retained").exists());
            assert!(!fs::read_dir(&retention_root).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".results.jsonl.")
            }));
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }
    }

    #[test]
    fn retained_verify_log_refuses_destination_aliases_before_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG alias preflight");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);

        let retained_path = artifact_dir.join("retained/verify/1/run-1.log.gz");
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &retained_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");
        assert!(!artifact_dir.join("retained").exists());

        let symlink_result = directory.path().join("symlink-results.jsonl");
        std::os::unix::fs::symlink(&run1_spec.paths.log, &symlink_result).unwrap();
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &symlink_result,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("is a symlink"), "{error}");
        assert_eq!(fs::read(&run1_spec.paths.log).unwrap(), body.as_bytes());

        let hardlink_result = directory.path().join("hardlink-results.jsonl");
        fs::hard_link(&run1_spec.paths.log, &hardlink_result).unwrap();
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &hardlink_result,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");

        fs::remove_file(&run2_spec.paths.summary).unwrap();
        fs::hard_link(&run1_spec.paths.log, &run2_spec.paths.summary).unwrap();
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_keeps_the_final_fd_across_a_path_swap() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG final path swap");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let swap = |path: &Path| {
            let original = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
            let replacement = path.with_extension("replacement");
            fs::write(&replacement, b"replacement is intentionally not gzip").unwrap();
            let replacement_identity =
                FileIdentity::from_metadata(&fs::metadata(&replacement).unwrap());
            assert_ne!(original, replacement_identity);
            fs::rename(replacement, path).unwrap();
        };
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                before_descriptor_publication: Some(&swap),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(
            error.contains("changed identity before publication"),
            "{error}"
        );
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
        assert!(!artifact_dir.join("retained/verify/1/run-1.log.gz").exists());
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
        let results_path = budget.results_path.clone();
        drop(budget);
        let restarted = VerifyLogRetentionBudget::open(
            directory.path(),
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_detects_in_place_mutation_before_descriptor_publication() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG final in-place mutation");
        for (name, same_size) in [("same-size", true), ("different-size", false)] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            let mutate = |path: &Path| {
                let before = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                if same_size {
                    let mut bytes = fs::read(path).unwrap();
                    let last = bytes.last_mut().expect("gzip output is not empty");
                    *last ^= 1;
                    fs::write(path, bytes).unwrap();
                } else {
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap();
                }
                let after = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                assert_eq!(before, after, "the mutation must preserve the inode");
            };
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    before_descriptor_publication: Some(&mutate),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(
                error.contains("descriptor publication")
                    || error.contains("gzip")
                    || error.contains("retained compressed verify log"),
                "{error}"
            );
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&results_path).unwrap(), b"");
            assert!(!artifact_dir.join("retained").exists());
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }
    }

    #[test]
    fn retained_and_result_paths_refuse_injected_ancestor_sync_failures() {
        let directory = tempfile::tempdir().unwrap();
        let result_path = directory.path().join("new/parents/results.jsonl");
        let error =
            prepare_result_path_from_root_with_failure(directory.path(), &result_path, Some(1))
                .unwrap_err();
        assert!(error.contains("injected failure syncing result directory ancestor"));
        assert!(!result_path.exists());

        let retention_root = directory.path().join("retention");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell");
        let body = structured_info_record("DETLOG ancestor sync failure");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                directory_sync_failure_at: Some(1),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(
            error.contains("injected failure syncing retained verify-log directory ancestor"),
            "{error}"
        );
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
        assert!(!artifact_dir.join("retained/verify/1/run-1.log.gz").exists());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
        let results_path = budget.results_path.clone();
        drop(budget);
        let restarted = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_write_faults_preserve_inputs_and_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG staging write faults");
        for (name, fault) in [
            ("zero", StagingWriteFault::ZeroFirst),
            (
                "mid-stream-error",
                StagingWriteFault::ErrorAfterFirstSuccessfulWrite,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let observed_charge = std::sync::atomic::AtomicU64::new(0);
            let on_write = || {
                observed_charge.store(
                    budget.accounted_compressed_bytes().unwrap(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            };
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &budget.results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    on_first_staging_write: Some(&on_write),
                    staging_write_fault: Some(fault),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(observed_charge.load(std::sync::atomic::Ordering::SeqCst) > 0);
            assert!(error.contains("write") || error.contains("gzip"), "{error}");
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
            assert!(!artifact_dir.join("retained").exists());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            let results_path = budget.results_path.clone();
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }

        let retention_root = directory.path().join("short");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                staging_write_fault: Some(StagingWriteFault::ShortFirst),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap();
        assert_eq!(
            budget.accounted_compressed_bytes().unwrap(),
            publication.retained.compressed_bytes
        );
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();
    }

    #[test]
    fn failed_final_removal_keeps_the_unreferenced_file_charged() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG retained cleanup failure");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), compressed_bytes);
        let results_path = directory.path().join("results.jsonl");
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &results_path,
            &mut result,
            VerifyLogTransactionHooks {
                failure: Some(VerifyLogTransactionFailurePoint::AfterFinalRename),
                fail_final_removal: true,
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("remain charged"), "{error}");
        assert_eq!(
            budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );
        assert!(
            artifact_dir
                .join("retained/verify/1/run-1.log.gz")
                .is_file()
        );
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&results_path).unwrap(), b"");
        drop(budget);
        let error = VerifyLogRetentionBudget::open(
            directory.path(),
            &results_path,
            VerifyLogRetentionPolicy::new(compressed_bytes),
        )
        .unwrap_err();
        assert!(error.contains("no result-row descriptor"), "{error}");
    }

    #[test]
    fn restart_accounting_reconciles_descriptors_and_refuses_impossible_layouts() {
        let directory = tempfile::tempdir().unwrap();
        let retention_root = directory.path().join("run");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell-a");
        let body = structured_info_record("DETLOG restart accounting");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let results_path = budget.results_path.clone();
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &results_path, &mut result).unwrap();
        let expected = publication.retained.compressed_bytes;
        let final_path = artifact_dir.join(&publication.retained.relative_path);
        let final_bytes = fs::read(&final_path).unwrap();
        drop(budget);

        let reopened = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(expected),
        )
        .unwrap();
        assert_eq!(reopened.accounted_compressed_bytes().unwrap(), expected);
        drop(reopened);
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(expected - 1),
        )
        .unwrap_err();
        assert!(error.contains("exceeding the"), "{error}");
        assert!(final_path.is_file());

        let second_artifact = retention_root.join("cell-b");
        let (second_run1, second_run2) = verify_pair_fixture(&second_artifact, &body, &body, true);
        let second_pair = compare_verify_runs(
            &second_run1,
            load_verify_run(&second_run1).unwrap(),
            &second_run2,
            load_verify_run(&second_run2).unwrap(),
        )
        .unwrap();
        let second_budget = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        let mut second_result = verify_cell_result(&second_run1);
        let second_publication = publish_retained_verify_log(
            second_pair,
            &second_budget,
            &results_path,
            &mut second_result,
        )
        .unwrap();
        let second_final = second_artifact.join(&second_publication.retained.relative_path);
        let second_final_bytes = fs::read(&second_final).unwrap();
        drop(second_budget);
        fs::remove_file(&second_final).unwrap();
        fs::hard_link(&final_path, &second_final).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("finals reuse one inode"), "{error}");
        fs::remove_file(&second_final).unwrap();
        fs::write(&second_final, &second_final_bytes).unwrap();

        let descriptor_without_final = fs::read(&results_path).unwrap();
        fs::remove_file(&final_path).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("contains no canonical final"), "{error}");
        fs::write(&final_path, &final_bytes).unwrap();

        let mut duplicate_descriptor = descriptor_without_final.clone();
        duplicate_descriptor.extend_from_slice(&descriptor_without_final);
        fs::write(&results_path, &duplicate_descriptor).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate retained verify-log descriptors"),
            "{error}"
        );
        fs::write(&results_path, &descriptor_without_final).unwrap();

        let mut wrong_path_result = result.clone();
        wrong_path_result.attempts[0]
            .retained_verify_log
            .as_mut()
            .unwrap()
            .relative_path = "retained/verify/1/other.log.gz".into();
        let mut wrong_path_bytes = serde_json::to_vec(&wrong_path_result).unwrap();
        wrong_path_bytes.push(b'\n');
        fs::write(&results_path, wrong_path_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");
        fs::write(&results_path, &descriptor_without_final).unwrap();

        let staging_path = final_path.parent().unwrap().join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}crash{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&staging_path, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(
            error.contains("a final and staging file coexist"),
            "{error}"
        );
        assert!(staging_path.is_file());
        fs::remove_file(&staging_path).unwrap();

        fs::write(&final_path, b"not gzip").unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("deterministic gzip header"), "{error}");

        let bad_root = directory.path().join("bad-run");
        fs::create_dir(&bad_root).unwrap();
        let bad_results = bad_root.join("results.jsonl");
        prepare_result_path(&bad_results).unwrap();
        fs::write(bad_root.join("unexpected"), b"bytes").unwrap();
        let error = VerifyLogRetentionBudget::open(
            &bad_root,
            &bad_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unexpected root-level file"), "{error}");

        let root_level_retained = directory.path().join("root-level-retained-run");
        let root_level_results = root_level_retained.join("results.jsonl");
        prepare_result_path(&root_level_results).unwrap();
        let root_level_attempt = root_level_retained.join("retained/verify/1");
        fs::create_dir_all(&root_level_attempt).unwrap();
        fs::write(root_level_attempt.join("run-1.log.gz"), &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &root_level_retained,
            &root_level_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("root-level retained"), "{error}");

        let unreferenced_root = directory.path().join("unreferenced-retained-run");
        let unreferenced_results = unreferenced_root.join("results.jsonl");
        prepare_result_path(&unreferenced_results).unwrap();
        let unreferenced_retained = unreferenced_root.join("cell/retained");
        fs::create_dir_all(&unreferenced_retained).unwrap();
        fs::write(
            unreferenced_retained.join("unreferenced.log.gz"),
            &final_bytes,
        )
        .unwrap();
        let error = VerifyLogRetentionBudget::open(
            &unreferenced_root,
            &unreferenced_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unreferenced bytes"), "{error}");

        let noncanonical_root = directory.path().join("noncanonical-run");
        let noncanonical_directory = noncanonical_root.join("cell/retained/verify/01");
        fs::create_dir_all(&noncanonical_directory).unwrap();
        let noncanonical_results = noncanonical_root.join("results.jsonl");
        prepare_result_path(&noncanonical_results).unwrap();
        fs::write(noncanonical_directory.join("run-1.log.gz"), &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &noncanonical_root,
            &noncanonical_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("is not canonical"), "{error}");

        let staging_root = directory.path().join("staging-run");
        let staging_directory = staging_root.join("cell/retained/verify/1");
        fs::create_dir_all(&staging_directory).unwrap();
        let staging_results = staging_root.join("results.jsonl");
        prepare_result_path(&staging_results).unwrap();
        let first_staging = staging_directory.join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}one{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&first_staging, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &staging_root,
            &staging_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unreferenced staging file"), "{error}");
        let second_staging = staging_directory.join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}two{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&second_staging, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &staging_root,
            &staging_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("multiple staging files coexist"), "{error}");

        let symlink_root = directory.path().join("symlink-run");
        let symlink_artifact = symlink_root.join("cell");
        let symlink_target = directory.path().join("retained-target");
        fs::create_dir_all(&symlink_artifact).unwrap();
        fs::create_dir_all(symlink_target.join("verify")).unwrap();
        let symlink_results = symlink_root.join("results.jsonl");
        prepare_result_path(&symlink_results).unwrap();
        std::os::unix::fs::symlink(&symlink_target, symlink_artifact.join("retained")).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &symlink_root,
            &symlink_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("non-symlink directory"), "{error}");
    }

    #[test]
    fn restart_scan_detects_in_place_final_mutation_during_reconciliation() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG restart in-place mutation");
        for (name, same_size) in [("same-size", true), ("different-size", false)] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            publish_retained_verify_log(pair, &budget, &results_path, &mut result).unwrap();
            drop(budget);

            let mutate = |path: &Path| {
                let before = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                if same_size {
                    let mut bytes = fs::read(path).unwrap();
                    let last = bytes.last_mut().expect("gzip output is not empty");
                    *last ^= 1;
                    fs::write(path, bytes).unwrap();
                } else {
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap();
                }
                let after = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                assert_eq!(before, after, "the mutation must preserve the inode");
            };
            let error = scan_existing_verify_log_bytes_with_hook(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
                Some(&mutate),
            )
            .unwrap_err();
            assert!(
                error.contains("restart scan")
                    || error.contains("gzip")
                    || error.contains("digest/size mismatch"),
                "{error}"
            );
        }
    }

    #[test]
    fn aggregate_retention_budget_refuses_checked_add_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        budget.reserve_additional(u64::MAX).unwrap();
        let error = budget.reserve_additional(1).unwrap_err();
        assert!(error.contains("accounting overflow"), "{error}");
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), u64::MAX);
    }

    #[test]
    fn concurrent_retention_publications_cannot_race_past_the_aggregate_limit() {
        const PAIRS: usize = 12;
        const ALLOWED: u64 = 3;

        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG concurrent retention bound");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());
        let results = Mutex::new(Vec::new());
        let mut pairs = Vec::new();
        for index in 0..PAIRS {
            let artifact_dir = directory.path().join(format!("pair-{index}"));
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            pairs.push((artifact_dir, run1_spec, run2_spec, pair));
        }
        let budget = verify_log_retention_budget(
            directory.path(),
            compressed_bytes.checked_mul(ALLOWED).unwrap(),
        );
        let barrier = std::sync::Barrier::new(PAIRS);
        let active_staging_writers = std::sync::atomic::AtomicUsize::new(0);
        let maximum_active_staging_writers = std::sync::atomic::AtomicUsize::new(0);

        thread::scope(|scope| {
            for (artifact_dir, run1_spec, run2_spec, pair) in pairs {
                let budget = budget.clone();
                let barrier = &barrier;
                let results = &results;
                let active_staging_writers = &active_staging_writers;
                let maximum_active_staging_writers = &maximum_active_staging_writers;
                scope.spawn(move || {
                    let on_first_staging_write = || {
                        assert!(
                            budget.accounted_compressed_bytes().unwrap() > 0,
                            "the requested staging write must be charged before I/O begins"
                        );
                        let active = active_staging_writers
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            + 1;
                        maximum_active_staging_writers
                            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
                        barrier.wait();
                        active_staging_writers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    };
                    let results_path = budget.results_path.clone();
                    let mut cell_result = verify_cell_result(&run1_spec);
                    let result = retain_verify_log_with_limit(
                        pair,
                        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                        &budget,
                        &results_path,
                        &mut cell_result,
                        VerifyLogTransactionHooks {
                            on_first_staging_write: Some(&on_first_staging_write),
                            ..VerifyLogTransactionHooks::default()
                        },
                    )
                    .map(|publication| publication.retained.compressed_bytes);
                    results.lock().unwrap().push((
                        artifact_dir,
                        run1_spec.paths.log,
                        run2_spec.paths.log,
                        result,
                    ));
                });
            }
        });

        let results = results.into_inner().unwrap();
        assert_eq!(results.len(), PAIRS);
        assert_eq!(
            maximum_active_staging_writers.load(std::sync::atomic::Ordering::SeqCst),
            PAIRS,
            "all staging writers must be able to overlap without holding the accounting lock"
        );
        let successes = results
            .iter()
            .filter(|(_, _, _, result)| result.is_ok())
            .count();
        assert!(successes > 0);
        assert!(successes <= usize::try_from(ALLOWED).unwrap());
        assert!(
            budget.accounted_compressed_bytes().unwrap()
                <= compressed_bytes.checked_mul(ALLOWED).unwrap()
        );
        let published_rows = fs::read_to_string(&budget.results_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(published_rows.len(), successes);
        assert!(
            published_rows
                .iter()
                .all(|result| result.attempts[0].retained_verify_log.is_some())
        );
        for (artifact_dir, run1_log, run2_log, result) in results {
            assert!(run1_log.is_file(), "retention must not remove run 1");
            assert!(run2_log.is_file(), "retention must not remove run 2");
            let retained = artifact_dir.join("retained/verify/1/run-1.log.gz");
            match result {
                Ok(bytes) => {
                    assert_eq!(bytes, compressed_bytes);
                    assert!(retained.is_file());
                }
                Err(error) => {
                    assert!(error.contains("aggregate limit"), "{error}");
                    assert!(!retained.exists());
                }
            }
        }
    }

    #[test]
    fn retained_verify_log_refuses_mutation_oversize_symlink_alias_and_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG retained");

        let mutated = directory.path().join("mutated");
        let (run1_spec, run2_spec) = verify_pair_fixture(&mutated, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        fs::write(
            &run2_spec.paths.log,
            structured_info_record("DETLOG changed"),
        )
        .unwrap();
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("changed after comparison"), "{error}");
        assert!(!mutated.join("retained").exists());

        let oversized = directory.path().join("oversized");
        let (run1_spec, run2_spec) = verify_pair_fixture(&oversized, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            compared,
            4,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("exceeds the 4-byte limit"), "{error}");
        assert!(run1_spec.paths.log.exists() && run2_spec.paths.log.exists());
        assert!(!oversized.join("retained").exists());

        let linked = directory.path().join("linked");
        let (run1_spec, run2_spec) = verify_pair_fixture(&linked, &body, &body, true);
        fs::remove_file(&run2_spec.paths.log).unwrap();
        std::fs::hard_link(&run1_spec.paths.log, &run2_spec.paths.log).unwrap();
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let error = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap_err();
        assert!(error.contains("same file"), "{error}");

        let symlinked = directory.path().join("symlinked");
        let (run1_spec, _) = verify_pair_fixture(&symlinked, &body, &body, true);
        let target = symlinked.join("target.log");
        fs::write(&target, b"do not change").unwrap();
        fs::remove_file(&run1_spec.paths.log).unwrap();
        std::os::unix::fs::symlink(&target, &run1_spec.paths.log).unwrap();
        let error = load_verify_run(&run1_spec).unwrap_err();
        assert!(
            error.contains("cannot open verify ordinary-run log"),
            "{error}"
        );
        assert_eq!(fs::read(target).unwrap(), b"do not change");

        let corrupted = directory.path().join("corrupted");
        let (run1_spec, run2_spec) = verify_pair_fixture(&corrupted, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap();
        fs::write(
            corrupted.join(&publication.retained.relative_path),
            b"not gzip",
        )
        .unwrap();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &publication.retained,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("deterministic gzip header"), "{error}");

        let mut traversal = publication.retained;
        traversal.relative_path = "../escape.log.gz".into();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &traversal,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");

        traversal.relative_path = "retained/verify/1/other.log.gz".into();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &traversal,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");
    }

    #[test]
    fn refused_empty_pair_is_still_retained_and_cleanup_failure_keeps_its_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("empty");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, "", "", true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::NoResult);
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap();
        assert_eq!(publication.retained.compared_info_messages, 0);
        assert_eq!(publication.retained.uncompressed_bytes, 0);
        verify_retained_verify_log_with_limit(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap();

        let mut calls = 0;
        let error = cleanup_verify_log_sources_with(&artifact_dir, &publication, 4096, |path| {
            calls += 1;
            if calls == 2 {
                Err(std::io::Error::other("injected second unlink failure"))
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap_err();
        assert!(error.contains("injected second unlink failure"), "{error}");
        assert!(
            artifact_dir
                .join(&publication.retained.relative_path)
                .is_file()
        );
        assert_eq!(publication.retained.role, RetainedVerifyLogRole::Run1);
        assert!(!run2_spec.paths.log.exists());
        assert!(run1_spec.paths.log.exists());
        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        cleanup_verify_log_sources(&artifact_dir, &publication)
            .expect("repeating completed raw-log cleanup must be harmless");
    }

    #[test]
    fn raw_log_cleanup_refuses_replacement_before_removing_either_input() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG raw cleanup replacement");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        let replacement = || {
            let replacement_path = run1_spec.paths.log.with_extension("replacement");
            fs::write(&replacement_path, fs::read(&run1_spec.paths.log).unwrap()).unwrap();
            fs::rename(&replacement_path, &run1_spec.paths.log).unwrap();
        };
        let error = cleanup_verify_log_sources_with_hook(
            &artifact_dir,
            &publication,
            4096,
            Some(&replacement),
            |path| fs::remove_file(path),
        )
        .unwrap_err();
        assert!(error.contains("changed identity"), "{error}");
        assert!(
            run1_spec.paths.log.is_file(),
            "the replacement must remain for recovery"
        );
        assert!(
            run2_spec.paths.log.is_file(),
            "cleanup must not remove the other raw log after detecting replacement"
        );
        assert_eq!(fs::read(&run1_spec.paths.log).unwrap(), body.as_bytes());
        assert_eq!(fs::read(&run2_spec.paths.log).unwrap(), body.as_bytes());
        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
    }

    #[test]
    fn raw_log_cleanup_is_reconstructed_from_the_durable_result_row() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG restart cleanup");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        drop(publication);
        drop(budget);

        let persisted: CellResult = serde_json::from_str(
            fs::read_to_string(directory.path().join("results.jsonl"))
                .unwrap()
                .trim_end(),
        )
        .unwrap();
        let reopened = reopen_retained_verify_log_publication(&persisted).unwrap();
        assert_eq!(
            reopened.run1_raw,
            verify_run_paths(&artifact_dir, "1", VerifyRun::Run1)
                .unwrap()
                .log
        );
        assert_eq!(
            reopened.run2_raw,
            verify_run_paths(&artifact_dir, "1", VerifyRun::Run2)
                .unwrap()
                .log
        );
        cleanup_verify_log_sources(&artifact_dir, &reopened).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        assert!(
            artifact_dir
                .join(&reopened.retained.relative_path)
                .is_file()
        );
        let restarted = VerifyLogRetentionBudget::open(
            directory.path(),
            directory.path().join("results.jsonl"),
            VerifyLogRetentionPolicy::new(reopened.retained.compressed_bytes),
        )
        .unwrap();
        assert_eq!(
            restarted.accounted_compressed_bytes().unwrap(),
            reopened.retained.compressed_bytes
        );
    }

    /// The label is rejected BEFORE anything is created on disk.
    ///
    /// `attempt_labels_must_be_one_normal_path_component` covers the return value
    /// against a path that is never touched, so it cannot see an ordering defect.
    /// If the guard ever moves below the first `create_dir_all`, a traversal label
    /// gets to create a directory outside the cell before being refused, and the
    /// return value alone still looks correct.
    #[test]
    fn an_invalid_attempt_label_is_refused_before_any_filesystem_side_effect() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-attempt-label-ordering-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let cell_dir = root.join("cell");

        for attempt in ["..", "/absolute", "parent/child"] {
            let error = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap_err();
            assert_eq!(
                error,
                format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                )
            );
            assert!(
                !cell_dir.exists(),
                "an invalid attempt label must be rejected before filesystem operations"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fixed_workdir_bind_is_distinct_for_concurrent_attempts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-concurrent-workdir-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(&cell_dir).unwrap();
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let source_for = |attempt: &str| {
            let spec = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap();
            bound_workdir_source(&spec)
        };
        let sources = [source_for("1"), source_for("2")];
        assert_eq!(sources[0], cell_dir.join("workdir/1"));
        assert_eq!(sources[1], cell_dir.join("workdir/2"));
        assert_ne!(sources[0], sources[1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_spec_resets_the_exact_bound_workdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-workdir-reset-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cell_dir = root.join("cell");
        let cell = ptrace_cell("custom");
        let context = run_context(&root);
        let mut spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            "attempt-7",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let bound = bound_workdir_source(&spec);
        assert_eq!(bound, cell_dir.join("workdir/attempt-7"));
        assert_eq!(bound, spec.fixed_workdir_source);

        fs::create_dir_all(&bound).unwrap();
        fs::write(bound.join("stale"), b"left by an earlier run").unwrap();
        spec.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"test -d "$1" && test ! -e "$1/stale" && : > "$1/myfile.txt""#.into(),
            "sh".into(),
            bound.to_string_lossy().into_owned(),
        ];

        let result = execute_spec(&spec).unwrap();
        assert_eq!(result.index, "attempt-7");
        assert_eq!(result.outcome, "PASS", "{}", result.stderr);
        assert!(bound.join("myfile.txt").is_file());
        assert!(!bound.join("stale").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
