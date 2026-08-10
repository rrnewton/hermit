//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Validate the centralized e2e manifests and expand their execution plan.
//!
//! Usage:
//!   cargo run -p hermit-manifest-plan -- --format text
//!   cargo run -p hermit-manifest-plan -- --format json
//!   cargo run -p hermit-manifest-plan -- --format harness-json
//!   cargo run -p hermit-manifest-plan -- --format verify-contracts

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::exit;

use serde_json::Value as JsonValue;
use serde_json::json;
use toml::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
const MATRIX_SYMMETRY_BASELINE: &str = "ci/matrix-symmetry-baseline.json";
const TEST_INVENTORY: &str = "tests/e2e/manifests/inventory/test-files.json";

#[derive(Debug)]
struct PlanRow {
    bucket: String,
    id: String,
    lane: String,
    mode: String,
    backend: String,
    ci: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Text,
    Json,
    HarnessJson,
    /// TSV of the normalized verify contract for every DECLARING cell. This is
    /// the authoritative channel for any runner outside `ci/test_harness.sh`
    /// (which consumes the same derivation embedded in `harness-json`), so no
    /// consumer has to re-read `expect_signal` / `expect_exit_code` itself.
    VerifyContracts,
}

#[cfg(not(test))]
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("manifest-plan: {msg}");
    exit(1);
}

#[cfg(test)]
fn die(msg: impl std::fmt::Display) -> ! {
    panic!("manifest-plan: {msg}");
}

fn parse_format() -> Format {
    let mut format = Format::Text;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = if arg == "--format" {
            args.next()
                .unwrap_or_else(|| die("--format requires a value"))
        } else if let Some(value) = arg.strip_prefix("--format=") {
            value.to_string()
        } else {
            die(format!("unknown argument: {arg}"));
        };
        format = match value.as_str() {
            "text" => Format::Text,
            "json" => Format::Json,
            "harness-json" => Format::HarnessJson,
            "verify-contracts" => Format::VerifyContracts,
            _ => die(format!("unknown format: {value}")),
        };
    }
    format
}

fn main() {
    let format = parse_format();
    let script_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/manifests");
    let repo_root = script_dir.join("../../..");

    let mut manifests: Vec<PathBuf> = std::fs::read_dir(&script_dir)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", script_dir.display())))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        die(format!(
            "no *.toml manifests found in {}",
            script_dir.display()
        ));
    }

    let mut rows = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut seen_programs = BTreeSet::new();
    let mut documents = Vec::new();

    for path in &manifests {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| die(format!("cannot read {}: {error}", path.display())));
        let document: Value = text
            .parse()
            .unwrap_or_else(|error| die(format!("{}: invalid TOML: {error}", path.display())));
        let location = path.file_name().unwrap().to_string_lossy().to_string();
        ensure_keys(&document, &["schema", "bucket", "test"], &location);

        if document.get("schema").and_then(Value::as_integer) != Some(2) {
            die(format!("{location}: schema must be 2"));
        }
        let bucket = required_string(&document, "bucket", &location);
        let stem = path.file_stem().unwrap().to_string_lossy();
        if bucket != stem {
            die(format!(
                "{location}: bucket `{bucket}` must equal file stem `{stem}`"
            ));
        }
        let tests = document
            .get("test")
            .and_then(Value::as_array)
            .filter(|tests| !tests.is_empty())
            .unwrap_or_else(|| die(format!("{location}: missing non-empty [[test]] array")));
        for test in tests {
            validate_and_expand(
                test,
                bucket,
                &location,
                &repo_root,
                &mut seen_ids,
                &mut seen_programs,
                &mut rows,
            );
        }
        documents.push(document);
    }

    validate_front_door(&repo_root, &documents);

    rows.sort_by(|left, right| {
        (&left.bucket, &left.id, &left.mode, &left.backend).cmp(&(
            &right.bucket,
            &right.id,
            &right.mode,
            &right.backend,
        ))
    });

    match format {
        Format::HarnessJson => {
            attach_verify_contracts(&mut documents);
            println!(
                "{}",
                serde_json::to_string(&documents)
                    .unwrap_or_else(|error| die(format!("cannot encode manifests: {error}")))
            );
        }
        Format::Json => {
            let output: Vec<_> = rows
                .iter()
                .map(|row| {
                    json!({
                        "bucket": row.bucket,
                        "test": row.id,
                        "lane": row.lane,
                        "mode": row.mode,
                        "backend": row.backend,
                        "ci": row.ci,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string(&output)
                    .unwrap_or_else(|error| die(format!("cannot encode plan: {error}")))
            );
        }
        Format::VerifyContracts => {
            for line in verify_contract_tsv(&documents) {
                println!("{line}");
            }
        }
        Format::Text => {
            println!(
                "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\tBUCKET",
                "LANE", "TEST", "MODE", "BACKEND", "CI"
            );
            for row in &rows {
                println!(
                    "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\t{}",
                    row.lane, row.id, row.mode, row.backend, row.ci, row.bucket
                );
            }
            eprintln!(
                "\nPASS: {} manifest(s), {} test(s), {} enabled plan cells validated",
                manifests.len(),
                seen_ids.len(),
                rows.len()
            );
        }
    }
}

fn json_string_set(value: &JsonValue, key: &str, location: &str) -> BTreeSet<String> {
    let values = value
        .get(key)
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| die(format!("{location}: `{key}` must be an array")));
    let result: BTreeSet<String> = values
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    die(format!(
                        "{location}: `{key}` entries must be non-empty strings"
                    ))
                })
        })
        .collect();
    if result.len() != values.len() {
        die(format!("{location}: `{key}` contains duplicate entries"));
    }
    result
}

fn names_backend(value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            let token = token.to_ascii_lowercase();
            matches!(
                token.as_str(),
                "ptrace" | "dbt" | "dynamorio" | "kvm" | "sabre" | "e9patch"
            ) || token.starts_with("liteinst")
        })
}

fn backend_private_guest_files(inventory: &JsonValue) -> BTreeSet<String> {
    inventory
        .get("files")
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| die(format!("{TEST_INVENTORY}: `files` must be an array")))
        .iter()
        .filter(|entry| {
            entry.get("disposition").and_then(JsonValue::as_str) == Some("guest-fixture")
        })
        .filter_map(|entry| {
            let path = entry.get("path").and_then(JsonValue::as_str)?;
            let runner = entry
                .get("runner")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            let parity_private = path.starts_with("tests/backend-parity/")
                || runner.contains("tests/backend-parity/");
            (parity_private || names_backend(path) || names_backend(runner))
                .then(|| path.to_string())
        })
        .collect()
}

fn asymmetric_manifest_tests(documents: &[Value]) -> BTreeSet<String> {
    let mut asymmetric = BTreeSet::new();
    for document in documents {
        for test in document
            .get("test")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = test.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(modes) = test.get("modes").and_then(Value::as_table) else {
                continue;
            };
            let mut has_ptrace_front_door = false;
            let mut has_backend_without_ptrace = false;
            for mode in MODES.into_iter().filter(|mode| *mode != "naked") {
                let enabled = modes
                    .get(mode)
                    .and_then(Value::as_table)
                    .and_then(|spec| spec.get("backends_enabled"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let has_ptrace = enabled.contains(&"ptrace");
                has_ptrace_front_door |= has_ptrace;
                has_backend_without_ptrace |= !enabled.is_empty() && !has_ptrace;
            }
            if !has_ptrace_front_door || has_backend_without_ptrace {
                asymmetric.insert(id.to_string());
            }
        }
    }
    asymmetric
}

fn enforce_exact_ratchet(label: &str, actual: &BTreeSet<String>, baseline: &BTreeSet<String>) {
    let unexpected: Vec<_> = actual.difference(baseline).cloned().collect();
    let stale: Vec<_> = baseline.difference(actual).cloned().collect();
    if !unexpected.is_empty() || !stale.is_empty() {
        die(format!(
            "matrix symmetry {label} changed; unexpected={unexpected:?}, stale_baseline={stale:?}. New compatibility coverage must enter a shared schema-v2 TOML manifest, establish ptrace first, and declare every backend/mode cell; remove migrated debt from {MATRIX_SYMMETRY_BASELINE}"
        ));
    }
}

fn validate_front_door(repo_root: &Path, documents: &[Value]) {
    let baseline_path = repo_root.join(MATRIX_SYMMETRY_BASELINE);
    let baseline_text = std::fs::read_to_string(&baseline_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", baseline_path.display())));
    let baseline: JsonValue = serde_json::from_str(&baseline_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            baseline_path.display()
        ))
    });
    let baseline_keys: BTreeSet<_> = baseline
        .as_object()
        .unwrap_or_else(|| die(format!("{MATRIX_SYMMETRY_BASELINE}: expected an object")))
        .keys()
        .map(String::as_str)
        .collect();
    let expected_keys: BTreeSet<_> = [
        "schema",
        "asymmetric_manifest_tests",
        "backend_private_guest_files",
    ]
    .into_iter()
    .collect();
    if baseline_keys != expected_keys {
        die(format!(
            "{MATRIX_SYMMETRY_BASELINE}: keys must be exactly {expected_keys:?}, got {baseline_keys:?}"
        ));
    }
    if baseline.get("schema").and_then(JsonValue::as_u64) != Some(1) {
        die(format!("{MATRIX_SYMMETRY_BASELINE}: schema must be 1"));
    }
    let expected_asymmetric = json_string_set(
        &baseline,
        "asymmetric_manifest_tests",
        MATRIX_SYMMETRY_BASELINE,
    );
    let expected_private = json_string_set(
        &baseline,
        "backend_private_guest_files",
        MATRIX_SYMMETRY_BASELINE,
    );

    let inventory_path = repo_root.join(TEST_INVENTORY);
    let inventory_text = std::fs::read_to_string(&inventory_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", inventory_path.display())));
    let inventory: JsonValue = serde_json::from_str(&inventory_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            inventory_path.display()
        ))
    });

    enforce_exact_ratchet(
        "manifest ptrace-front-door debt",
        &asymmetric_manifest_tests(documents),
        &expected_asymmetric,
    );
    enforce_exact_ratchet(
        "backend-private guest debt",
        &backend_private_guest_files(&inventory),
        &expected_private,
    );
}

fn required_string<'a>(value: &'a Value, key: &str, location: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| die(format!("{location}: missing non-empty string `{key}`")))
}

fn string_array(value: Option<&Value>, location: &str) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .unwrap_or_else(|| die(format!("{location}: expected an array")))
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| die(format!("{location}: array values must be strings")))
        })
        .collect()
}

fn validate_direct(value: &Value, id: &str) {
    match value {
        Value::String(command) if !command.trim().is_empty() => {}
        Value::String(_) => die(format!("{id}: direct command must not be empty")),
        Value::Array(_) => {
            if string_array(Some(value), &format!("{id}.direct")).is_empty() {
                die(format!("{id}: direct argv must not be empty"));
            }
        }
        _ => die(format!(
            "{id}: direct must be a shell command string or an argv array"
        )),
    }
}

fn ensure_keys(value: &Value, allowed: &[&str], location: &str) {
    let table = value
        .as_table()
        .unwrap_or_else(|| die(format!("{location}: expected a table")));
    let allowed: BTreeSet<_> = allowed.iter().copied().collect();
    let actual: BTreeSet<_> = table.keys().map(String::as_str).collect();
    let unknown: Vec<_> = actual.difference(&allowed).copied().collect();
    if !unknown.is_empty() {
        die(format!("{location}: unknown keys: {unknown:?}"));
    }
}

fn is_file_or_symlink(path: &Path) -> bool {
    path.is_file()
        || std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[allow(clippy::too_many_arguments)]
fn validate_and_expand(
    test: &Value,
    bucket: &str,
    location: &str,
    repo_root: &Path,
    seen_ids: &mut BTreeSet<String>,
    seen_programs: &mut BTreeSet<String>,
    rows: &mut Vec<PlanRow>,
) {
    let id = required_string(test, "id", location).to_string();
    ensure_keys(
        test,
        &[
            "id",
            "description",
            "lane",
            "requires",
            "timeout_seconds",
            "occasional",
            "program",
            "direct",
            "observation",
            "build",
            "modes",
            "slow_reason",
            "preprocessors",
        ],
        &id,
    );
    if !id.starts_with(&format!("{bucket}/"))
        || !id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "-/".contains(character)
        })
        || id
            .strip_prefix(&format!("{bucket}/"))
            .is_none_or(|suffix| suffix.is_empty() || suffix.starts_with('-'))
    {
        die(format!(
            "{location}: id `{id}` must be lowercase and start with `{bucket}/`"
        ));
    }
    if !seen_ids.insert(id.clone()) {
        die(format!("duplicate test id across manifests: {id}"));
    }
    required_string(test, "description", &id);

    let lane = required_string(test, "lane", &id);
    if lane != "portable" && lane != "privileged" {
        die(format!(
            "{id}: lane must be portable|privileged, got `{lane}`"
        ));
    }
    match test.get("timeout_seconds").and_then(Value::as_integer) {
        Some(timeout) if (1..=1800).contains(&timeout) => {}
        other => die(format!(
            "{id}: timeout_seconds must be 1..=1800, got {other:?}"
        )),
    }
    if test.get("occasional").and_then(Value::as_bool).is_none() {
        die(format!("{id}: occasional must be a boolean"));
    }
    let _requires = string_array(test.get("requires"), &format!("{id}.requires"));

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct");
    let mut program_path = None;
    match (program, direct) {
        (Some(_), Some(_)) => die(format!("{id}: set only one of `program`/`direct`")),
        (None, None) => die(format!("{id}: must set `program` or `direct`")),
        (Some(program), None) => {
            let extension = Path::new(program)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("");
            if !["sh", "c", "rs"].contains(&extension) {
                die(format!("{id}: program `{program}` must end in .sh/.c/.rs"));
            }
            if !program.starts_with("tests/") || program.split('/').any(|part| part == "..") {
                die(format!(
                    "{id}: program must be a repo-relative path below tests/: {program}"
                ));
            }
            let path = repo_root.join(program);
            if !is_file_or_symlink(&path) {
                die(format!("{id}: program path does not exist: {program}"));
            }
            program_path = Some(path);
            if !seen_programs.insert(program.to_string()) {
                die(format!(
                    "program appears in multiple manifest tests: {program}"
                ));
            }
        }
        (None, Some(direct)) => validate_direct(direct, &id),
    }

    if let Some(build) = test.get("build") {
        ensure_keys(build, &["cflags", "rustflags"], &format!("{id}.build"));
        for key in ["cflags", "rustflags"] {
            if build.get(key).is_some() {
                let _flags = string_array(build.get(key), &format!("{id}.build.{key}"));
            }
        }
    }
    if let Some(reason) = test.get("slow_reason") {
        if reason.as_str().is_none_or(str::is_empty) {
            die(format!("{id}: slow_reason must be a non-empty string"));
        }
    }
    if let Some(preprocessors) = test.get("preprocessors") {
        let preprocessors = string_array(Some(preprocessors), &format!("{id}.preprocessors"));
        if preprocessors.iter().any(|value| value != "e9patch") {
            die(format!("{id}: the only supported preprocessor is e9patch"));
        }
    }

    validate_observation(test, &id);

    let modes = test
        .get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| die(format!("{id}: missing [test.modes]")));
    let actual_modes: BTreeSet<_> = modes.keys().map(String::as_str).collect();
    let expected_modes: BTreeSet<_> = MODES.into_iter().collect();
    if actual_modes != expected_modes {
        die(format!(
            "{id}: modes must be exactly {:?}, got {:?}",
            expected_modes, actual_modes
        ));
    }

    let row_start = rows.len();
    for mode in MODES {
        validate_mode(&id, bucket, lane, mode, modes.get(mode).unwrap(), rows);
    }
    if rows[row_start..].iter().any(|row| row.ci)
        && program_path.as_ref().is_some_and(|path| !path.is_file())
    {
        die(format!(
            "{id}: CI-enabled program symlink target is unavailable: {}",
            program.unwrap()
        ));
    }
}

fn validate_observation(test: &Value, id: &str) {
    let observation_value = test
        .get("observation")
        .unwrap_or_else(|| die(format!("{id}: observation must be a table")));
    ensure_keys(
        observation_value,
        &["status", "stdout", "stderr", "artifacts"],
        &format!("{id}.observation"),
    );
    let observation = observation_value.as_table().unwrap();
    for key in ["status", "stdout", "stderr"] {
        if observation.get(key).and_then(Value::as_bool).is_none() {
            die(format!("{id}: observation.{key} must be a boolean"));
        }
    }
    for artifact in string_array(
        observation.get("artifacts"),
        &format!("{id}.observation.artifacts"),
    ) {
        if artifact.starts_with('/') || artifact.split('/').any(|part| part == "..") {
            die(format!(
                "{id}: observation artifact must stay below E2E_TMPDIR: {artifact}"
            ));
        }
    }
}

/// How a verify cell's guest is declared to terminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestTermination {
    /// The default: the guest ran to completion and exited 0.
    Success,
    /// The guest was killed by this signal (`modes.verify.expect_signal`).
    Signal(i64),
    /// The guest exited with this non-zero code (`modes.verify.expect_exit_code`).
    ExitCode(i64),
}

/// The normalized verify-mode contract for one cell.
///
/// THIS FUNCTION IS THE ONLY PLACE THE POLICY IS DERIVED. It is emitted verbatim
/// into `--format harness-json` at `modes.verify.verify_contract`, and both
/// `ci/test_harness.sh` and `scripts/manifest-to-commands.rs` consume that
/// emission rather than re-reading the raw keys. Three independent
/// re-derivations of the same policy is a drift hazard even while they agree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyContract {
    termination: GuestTermination,
    /// Hermit flags the cell must be run with, beyond the harness defaults.
    extra_args: Vec<&'static str>,
    /// Whether the runner must pass `--verify-json` and check the record.
    verify_json: bool,
    /// The whole-invocation status the runner must observe.
    shell_status: i64,
    /// Required `guest_signal` in the verification record; `None` means the
    /// record must carry JSON `null` there.
    require_guest_signal: Option<i64>,
    /// Required `guest_exit_code`; `None` means the record must carry `null`.
    require_guest_exit_code: Option<i64>,
}

/// Derive (and validate) the verify contract from `modes.verify`.
///
/// A guest whose deterministic outcome is a crash or a deliberate non-zero exit
/// could not be expressed before these keys existed. `hermit run --verify`
/// defaults to `--verify-allow success`: when the first run's status is not
/// success it refuses the second run, so the determinism comparison never happens
/// and the record says `verdict: "no_result"`. Such guests had to stay at
/// `ci = false`, outside the measured envelope, even when they reproduce
/// bitwise. This is the same class of gap `guest_args` closed for guests that
/// need arguments (rrnewton/hermit#1815).
///
/// The declaration names the guest's TERMINATION, never a raw wait status.
/// A single 8-bit number cannot be a safety boundary here: `139` is
/// indistinguishable between a guest `exit(139)`, a guest killed by SIGSEGV, and
/// HERMIT ITSELF dying of SIGSEGV — and the last is a Hermit defect that must
/// never be able to satisfy a cell. So the contract additionally requires the
/// `--verify-json` record's own provenance fields (`guest_signal` /
/// `guest_exit_code`, which Hermit derives from the GUEST's wait status) plus
/// `verified` and `bitwise_parity`. A Hermit-side abort cannot forge those: the
/// record is stamped `no_result` before any fallible work, so a run that dies
/// before a verdict leaves a refusal behind, not a stale pass.
fn verify_contract(id: &str, spec: &toml::value::Table) -> VerifyContract {
    let signal = spec.get("expect_signal");
    let exit_code = spec.get("expect_exit_code");
    let termination = match (signal, exit_code) {
        (Some(_), Some(_)) => die(format!(
            "{id}: modes.verify must set at most one of expect_signal / expect_exit_code"
        )),
        (None, None) => GuestTermination::Success,
        (Some(value), None) => match value.as_integer() {
            // 1..=64 covers the standard and real-time signals on Linux/x86_64.
            Some(number) if (1..=64).contains(&number) => GuestTermination::Signal(number),
            other => die(format!(
                "{id}: modes.verify.expect_signal must be a signal number 1..=64 \
                 (11 = SIGSEGV), got {other:?}"
            )),
        },
        (None, Some(value)) => match value.as_integer() {
            Some(number) if (1..=255).contains(&number) => GuestTermination::ExitCode(number),
            Some(0) => die(format!(
                "{id}: modes.verify.expect_exit_code=0 is the default; omit it rather \
                 than restating the success contract"
            )),
            other => die(format!(
                "{id}: modes.verify.expect_exit_code must be 1..=255, got {other:?}"
            )),
        },
    };

    match termination {
        GuestTermination::Success => VerifyContract {
            termination,
            extra_args: Vec::new(),
            // An exit-0 cell needs no record: `--verify` already returns non-zero
            // on divergence, so its status IS causally bound to the verdict. The
            // record becomes load-bearing exactly when that binding breaks.
            verify_json: false,
            shell_status: 0,
            require_guest_signal: None,
            require_guest_exit_code: Some(0),
        },
        // `--verify-strict` is mandatory for a declaring cell, not optional: the
        // justification for accepting a non-zero exit is that the run reproduces
        // BITWISE, and only the canonical comparator can certify that. The
        // default stripped comparator cannot, so a declaring cell that ran
        // without it would assert less than its own rationale claims.
        GuestTermination::Signal(number) => VerifyContract {
            termination,
            extra_args: vec!["--verify-allow", "both", "--verify-strict"],
            verify_json: true,
            shell_status: 128 + number,
            require_guest_signal: Some(number),
            require_guest_exit_code: None,
        },
        GuestTermination::ExitCode(number) => VerifyContract {
            termination,
            extra_args: vec!["--verify-allow", "both", "--verify-strict"],
            verify_json: true,
            shell_status: number,
            require_guest_signal: None,
            require_guest_exit_code: Some(number),
        },
    }
}

/// The exact subset of `--verify-json` fields a declaring cell's record must
/// carry, as a JSON object literal.
///
/// Emitted as a STRING because TOML has no `null`, and `null` is load-bearing
/// here: "the guest was killed by a signal" is `guest_signal: 11` **together
/// with** `guest_exit_code: null`, and a record that reports both would not be
/// the thing we claim. The consumer's whole job is then one generic operation —
/// assert the observed record is a superset of this object — so it holds no
/// policy of its own and cannot drift from this function.
fn require_record_json(contract: &VerifyContract) -> String {
    let mut required = serde_json::Map::new();
    // `verified` is the determinism verdict; `bitwise_parity` is the L2 claim
    // this whole feature's rationale rests on; `matched` refuses `no_result`
    // explicitly rather than by implication.
    required.insert("verdict".to_owned(), JsonValue::from("matched"));
    required.insert("verified".to_owned(), JsonValue::Bool(true));
    required.insert("bitwise_parity".to_owned(), JsonValue::Bool(true));
    required.insert(
        "guest_signal".to_owned(),
        match contract.require_guest_signal {
            Some(number) => JsonValue::from(number),
            None => JsonValue::Null,
        },
    );
    required.insert(
        "guest_exit_code".to_owned(),
        match contract.require_guest_exit_code {
            Some(number) => JsonValue::from(number),
            None => JsonValue::Null,
        },
    );
    serde_json::to_string(&JsonValue::Object(required))
        .unwrap_or_else(|error| die(format!("cannot encode verify contract: {error}")))
}

/// Render the contract for `--format harness-json`.
fn verify_contract_value(contract: &VerifyContract) -> Value {
    let mut table = toml::value::Table::new();
    let declared = contract.termination != GuestTermination::Success;
    table.insert("declared".to_owned(), Value::Boolean(declared));
    table.insert(
        "extra_args".to_owned(),
        Value::Array(
            contract
                .extra_args
                .iter()
                .map(|argument| Value::String((*argument).to_owned()))
                .collect(),
        ),
    );
    table.insert(
        "verify_json".to_owned(),
        Value::Boolean(contract.verify_json),
    );
    table.insert(
        "shell_status".to_owned(),
        Value::Integer(contract.shell_status),
    );
    if contract.verify_json {
        table.insert(
            "require_record".to_owned(),
            Value::String(require_record_json(contract)),
        );
    }
    Value::Table(table)
}

/// One TSV line per DECLARING verify cell, sorted by test id:
/// `<test-id>\t<shell-status>\t<extra-args, space-joined>\t<require-record JSON>`.
///
/// This is the authoritative channel for runners that do not read
/// `harness-json`: `scripts/manifest-to-commands.rs` and any out-of-tree
/// harness (the compat scorecard). Same derivation, different envelope, so a
/// consumer never re-reads `expect_signal` / `expect_exit_code` itself. Cells
/// with the default exit-0 contract are omitted rather than emitted, so a
/// consumer can distinguish "declared nothing" from "declared success".
fn verify_contract_tsv(documents: &[Value]) -> Vec<String> {
    let mut lines = Vec::new();
    for document in documents {
        let Some(tests) = document.get("test").and_then(Value::as_array) else {
            continue;
        };
        for test in tests {
            let id = test
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let Some(verify) = test
                .get("modes")
                .and_then(Value::as_table)
                .and_then(|modes| modes.get("verify"))
                .and_then(Value::as_table)
            else {
                continue;
            };
            let contract = verify_contract(id, verify);
            if contract.termination == GuestTermination::Success {
                continue;
            }
            lines.push(format!(
                "{id}\t{}\t{}\t{}",
                contract.shell_status,
                contract.extra_args.join(" "),
                require_record_json(&contract)
            ));
        }
    }
    lines.sort();
    lines
}

/// Inject the normalized contract into every test's `modes.verify` so
/// `--format harness-json` carries the single derivation to its consumers.
///
/// Runs after full validation, so the keys it reads are already known-good and
/// `ensure_keys` has already run against the un-injected document.
fn attach_verify_contracts(documents: &mut [Value]) {
    for document in documents.iter_mut() {
        let Some(tests) = document.get_mut("test").and_then(Value::as_array_mut) else {
            continue;
        };
        for test in tests.iter_mut() {
            let id = test
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_owned();
            let verify = test
                .get_mut("modes")
                .and_then(Value::as_table_mut)
                .and_then(|modes| modes.get_mut("verify"))
                .and_then(Value::as_table_mut)
                .unwrap_or_else(|| die(format!("{id}: modes.verify must be a table")));
            let contract = verify_contract(&id, verify);
            verify.insert(
                "verify_contract".to_owned(),
                verify_contract_value(&contract),
            );
        }
    }
}

fn validate_mode(
    id: &str,
    bucket: &str,
    lane: &str,
    mode: &str,
    spec: &Value,
    rows: &mut Vec<PlanRow>,
) {
    let spec_value = spec;
    let spec = spec_value
        .as_table()
        .unwrap_or_else(|| die(format!("{id}: modes.{mode} must be a table")));
    let mut allowed = vec!["ci", "backends_enabled", "backends_disabled", "guest_args"];
    match mode {
        "naked" => allowed.extend(["runs", "assert"]),
        "chaos" => allowed.extend(["seeds", "assert"]),
        "custom" => allowed.extend(["args", "assert"]),
        "verify" => allowed.extend(["expect_signal", "expect_exit_code"]),
        _ => {}
    }
    ensure_keys(spec_value, &allowed, &format!("{id}.modes.{mode}"));
    if mode == "verify" {
        // Derive here purely to VALIDATE: the same function is what later
        // normalizes the contract into `--format harness-json`, so a manifest
        // the plan accepts is exactly a manifest the runners can execute.
        let _ = verify_contract(id, spec);
    }
    let ci = spec
        .get("ci")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| die(format!("{id}: modes.{mode}.ci must be a boolean")));
    let enabled = string_array(
        spec.get("backends_enabled"),
        &format!("{id}.modes.{mode}.backends_enabled"),
    );
    let disabled = spec
        .get("backends_disabled")
        .and_then(Value::as_table)
        .unwrap_or_else(|| {
            die(format!(
                "{id}: modes.{mode}.backends_disabled must be a table"
            ))
        });
    for (backend, reason) in disabled {
        if reason.as_str().is_none_or(str::is_empty) {
            die(format!(
                "{id}: modes.{mode}.backends_disabled.{backend} needs a reason"
            ));
        }
    }

    let expected: BTreeSet<&str> = if mode == "naked" {
        ["native"].into_iter().collect()
    } else {
        KNOWN_BACKENDS.into_iter().collect()
    };
    let enabled_set: BTreeSet<&str> = enabled.iter().map(String::as_str).collect();
    let disabled_set: BTreeSet<&str> = disabled.keys().map(String::as_str).collect();
    if enabled_set.len() != enabled.len()
        || !enabled_set.is_disjoint(&disabled_set)
        || enabled_set
            .union(&disabled_set)
            .copied()
            .collect::<BTreeSet<_>>()
            != expected
    {
        die(format!(
            "{id}: modes.{mode} must partition {:?}; enabled={enabled_set:?}, disabled={disabled_set:?}",
            expected
        ));
    }
    if let Some(guest_args) = spec.get("guest_args") {
        let guest_args = guest_args
            .as_table()
            .unwrap_or_else(|| die(format!("{id}: modes.{mode}.guest_args must be a table")));
        for (backend, args) in guest_args {
            if !enabled_set.contains(backend.as_str()) {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} names a backend that is not enabled"
                ));
            }
            if string_array(
                Some(args),
                &format!("{id}.modes.{mode}.guest_args.{backend}"),
            )
            .is_empty()
            {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} must contain at least one argument"
                ));
            }
        }
    }
    if ci && enabled.is_empty() {
        die(format!(
            "{id}: modes.{mode} is CI-enabled but has no backend"
        ));
    }
    if mode == "naked" && ci {
        die(format!(
            "{id}: naked is opt-in meta-CI and must set ci=false"
        ));
    }
    if mode == "replay" && enabled.iter().any(|backend| backend != "ptrace") {
        die(format!("{id}: replay is ptrace-only, got {enabled:?}"));
    }

    if mode == "naked" && !enabled.is_empty() {
        let runs = spec
            .get("runs")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: enabled naked mode requires runs")));
        if !(3..=5).contains(&runs) {
            die(format!("{id}: naked.runs must be 3..=5, got {runs}"));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: naked.assert must be a table")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["min_distinct"],
            &format!("{id}.modes.naked.assert"),
        );
        let min_distinct = assertions
            .get("min_distinct")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: naked.assert.min_distinct is required")));
        if !(2..=runs).contains(&min_distinct) {
            die(format!(
                "{id}: naked.assert.min_distinct must be 2..={runs}, got {min_distinct}"
            ));
        }
    }
    if mode == "chaos" && !enabled.is_empty() {
        let seeds = spec
            .get("seeds")
            .and_then(Value::as_array)
            .unwrap_or_else(|| die(format!("{id}: enabled chaos mode requires seeds")));
        let unique: BTreeSet<_> = seeds.iter().filter_map(Value::as_integer).collect();
        if seeds.len() < 2 || unique.len() != seeds.len() {
            die(format!(
                "{id}: chaos seeds must contain at least two unique integers"
            ));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: enabled chaos mode requires assert")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["min_distinct", "min_passes", "min_failures"],
            &format!("{id}.modes.chaos.assert"),
        );
        for key in ["min_distinct", "min_passes", "min_failures"] {
            match assertions.get(key).and_then(Value::as_integer) {
                Some(value) if value >= 0 && (key != "min_distinct" || value >= 2) => {}
                other => die(format!(
                    "{id}: chaos.assert.{key} has invalid value {other:?}"
                )),
            }
        }
    }
    if mode == "custom" && !enabled.is_empty() {
        let args = string_array(spec.get("args"), &format!("{id}.modes.custom.args"));
        if args.is_empty() {
            die(format!("{id}: enabled custom mode requires args"));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: enabled custom mode requires assert")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["runs", "repeat_identical"],
            &format!("{id}.modes.custom.assert"),
        );
        let runs = assertions
            .get("runs")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: custom.assert.runs is required")));
        if !(3..=5).contains(&runs)
            || assertions.get("repeat_identical").and_then(Value::as_bool) != Some(true)
        {
            die(format!(
                "{id}: custom must require 3..=5 runs with repeat_identical=true"
            ));
        }
    }

    for backend in enabled {
        rows.push(PlanRow {
            bucket: bucket.to_string(),
            id: id.to_string(),
            lane: lane.to_string(),
            mode: mode.to_string(),
            backend,
            ci,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_mode(text: &str) -> Value {
        text.parse::<Value>().expect("test mode must be valid TOML")
    }

    #[test]
    #[should_panic(expected = "unknown keys")]
    fn rejects_unknown_schema_keys() {
        let value = parse_mode("known = true\nunknown = false\n");
        ensure_keys(&value, &["known"], "test");
    }

    #[test]
    fn accepts_structured_direct_argv() {
        let value = parse_mode("direct = [\"./example\", \"argument with spaces\"]\n");
        validate_direct(value.get("direct").unwrap(), "bucket/test");
    }

    #[test]
    #[should_panic(expected = "direct argv must not be empty")]
    fn rejects_empty_direct_argv() {
        let value = parse_mode("direct = []\n");
        validate_direct(value.get("direct").unwrap(), "bucket/test");
    }

    #[test]
    #[should_panic(expected = "must partition")]
    fn rejects_incomplete_backend_partition() {
        let spec = parse_mode(
            r#"
ci = false
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "naked is opt-in meta-CI")]
    fn rejects_naked_mode_in_regular_ci() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["native"]
runs = 3

[backends_disabled]

[assert]
min_distinct = 2
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "naked",
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn accepts_complete_verify_partition() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
guest_args = { ptrace = ["multi"] }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            &spec,
            &mut rows,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].backend, "ptrace");
        assert!(rows[0].ci);
    }

    /// A verify cell whose guest deterministically dies must be able to say so,
    /// and the derived contract must demand the evidence that makes the claim
    /// checkable: the canonical comparator plus the record's own provenance.
    #[test]
    fn signal_declaration_requires_canonical_parity_and_provenance() {
        let spec = parse_mode("expect_signal = 11\n");
        let contract = verify_contract("bucket/test", spec.as_table().unwrap());
        assert_eq!(contract.termination, GuestTermination::Signal(11));
        assert_eq!(contract.shell_status, 139);
        assert!(contract.verify_json);
        assert_eq!(
            contract.extra_args,
            vec!["--verify-allow", "both", "--verify-strict"]
        );
        let required: JsonValue =
            serde_json::from_str(&require_record_json(&contract)).expect("required record is JSON");
        assert_eq!(required["verified"], JsonValue::Bool(true));
        assert_eq!(required["bitwise_parity"], JsonValue::Bool(true));
        assert_eq!(required["verdict"], JsonValue::from("matched"));
        assert_eq!(required["guest_signal"], JsonValue::from(11));
        // `null`, not absent: a record reporting BOTH a signal and an exit code
        // is not the thing the cell claims, so the requirement must say so.
        assert_eq!(required["guest_exit_code"], JsonValue::Null);
    }

    /// The mirror case, so a deliberate non-zero exit cannot be satisfied by a
    /// guest that was KILLED with the same number.
    #[test]
    fn exit_code_declaration_requires_a_null_signal() {
        let spec = parse_mode("expect_exit_code = 7\n");
        let contract = verify_contract("bucket/test", spec.as_table().unwrap());
        assert_eq!(contract.termination, GuestTermination::ExitCode(7));
        assert_eq!(contract.shell_status, 7);
        let required: JsonValue =
            serde_json::from_str(&require_record_json(&contract)).expect("required record is JSON");
        assert_eq!(required["guest_exit_code"], JsonValue::from(7));
        assert_eq!(required["guest_signal"], JsonValue::Null);
    }

    /// The default must stay exactly as it was: no extra flags, no record. An
    /// exit-0 cell's status is already causally bound to the verdict because
    /// `--verify` returns non-zero on divergence.
    #[test]
    fn undeclared_verify_cell_keeps_the_exit_zero_contract() {
        let spec = parse_mode("ci = false\n");
        let contract = verify_contract("bucket/test", spec.as_table().unwrap());
        assert_eq!(contract.termination, GuestTermination::Success);
        assert_eq!(contract.shell_status, 0);
        assert!(!contract.verify_json);
        assert!(contract.extra_args.is_empty());
    }

    /// NEGATIVE: only `verify` runs a single Hermit invocation whose status and
    /// verification record are the cell's whole observation. Accepting the key
    /// elsewhere would create a declaration that silently changes nothing.
    #[test]
    #[should_panic(expected = "unknown keys")]
    fn rejects_declared_termination_outside_verify() {
        let spec = parse_mode(
            r#"
ci = false
expect_signal = 11
backends_enabled = []
seeds = [1, 2]

[backends_disabled]
ptrace = "unsupported"
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "chaos",
            &spec,
            &mut Vec::new(),
        );
    }

    /// NEGATIVE: the two keys mean different things and a cell that set both
    /// would have no single checkable termination.
    #[test]
    #[should_panic(expected = "at most one of expect_signal")]
    fn rejects_both_termination_keys() {
        let spec = parse_mode("expect_signal = 11\nexpect_exit_code = 7\n");
        verify_contract("bucket/test", spec.as_table().unwrap());
    }

    /// NEGATIVE: `expect_exit_code = 0` restates the default. Allowing it would
    /// let a reader believe a cell had been examined and declared successful
    /// when nothing distinguishes it from every undeclared cell.
    #[test]
    #[should_panic(expected = "omit it rather than restating")]
    fn rejects_redundant_zero_exit_code() {
        let spec = parse_mode("expect_exit_code = 0\n");
        verify_contract("bucket/test", spec.as_table().unwrap());
    }

    /// NEGATIVE: a raw wait status in the signal slot is the exact confusion
    /// this schema exists to prevent — 139 is `128 + SIGSEGV`, not a signal.
    #[test]
    #[should_panic(expected = "must be a signal number 1..=64")]
    fn rejects_a_wait_status_in_the_signal_slot() {
        let spec = parse_mode("expect_signal = 139\n");
        verify_contract("bucket/test", spec.as_table().unwrap());
    }

    #[test]
    #[should_panic(expected = "must be 1..=255")]
    fn rejects_out_of_range_exit_code() {
        let spec = parse_mode("expect_exit_code = 256\n");
        verify_contract("bucket/test", spec.as_table().unwrap());
    }

    #[test]
    fn identifies_backend_private_guest_fixtures() {
        let inventory = json!({
            "files": [
                {
                    "path": "tests/backend-parity/fixtures/new_contract.c",
                    "disposition": "guest-fixture",
                    "runner": "tests/backend-parity/run_matrix.py"
                },
                {
                    "path": "tests/c/liteinst_only.c",
                    "disposition": "guest-fixture",
                    "runner": "hermit-cli/tests/liteinst.rs"
                },
                {
                    "path": "tests/c/shared.c",
                    "disposition": "manifest-test",
                    "runner": "ci/test_harness.sh"
                },
                {
                    "path": "tests/c/cargo_fixture.c",
                    "disposition": "guest-fixture",
                    "runner": "detcore integration tests"
                }
            ]
        });
        assert_eq!(
            backend_private_guest_files(&inventory),
            BTreeSet::from([
                "tests/backend-parity/fixtures/new_contract.c".to_string(),
                "tests/c/liteinst_only.c".to_string(),
            ])
        );
    }

    #[test]
    fn identifies_manifest_mode_without_ptrace_front_door() {
        let document = r#"
[[test]]
id = "applications/kvm-only"

[test.modes.verify]
backends_enabled = ["kvm"]

[test.modes.chaos]
backends_enabled = []

[test.modes.replay]
backends_enabled = []

[test.modes.naked]
backends_enabled = []

[test.modes.custom]
backends_enabled = []
"#
        .parse::<Value>()
        .expect("test manifest must be valid TOML");
        assert_eq!(
            asymmetric_manifest_tests(&[document]),
            BTreeSet::from(["applications/kvm-only".to_string()])
        );
    }

    #[test]
    fn accepts_ptrace_established_shared_manifest_row() {
        let document = r#"
[[test]]
id = "applications/shared"

[test.modes.verify]
backends_enabled = ["ptrace", "kvm"]

[test.modes.chaos]
backends_enabled = []

[test.modes.replay]
backends_enabled = ["ptrace"]

[test.modes.naked]
backends_enabled = []

[test.modes.custom]
backends_enabled = []
"#
        .parse::<Value>()
        .expect("test manifest must be valid TOML");
        assert!(asymmetric_manifest_tests(&[document]).is_empty());
    }

    #[test]
    #[should_panic(expected = "unexpected=[\"tests/backend-parity/private.c\"]")]
    fn rejects_backend_private_guest_growth() {
        enforce_exact_ratchet(
            "backend-private guest debt",
            &BTreeSet::from(["tests/backend-parity/private.c".to_string()]),
            &BTreeSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "names a backend that is not enabled")]
    fn rejects_guest_args_for_disabled_backend() {
        let spec = parse_mode(
            r#"
ci = false
backends_enabled = ["ptrace"]
guest_args = { kvm = ["--kvm"] }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            &spec,
            &mut Vec::new(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn recognizes_broken_symlink_as_manual_program_entry() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "hermit-manifest-plan-symlink-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let link = directory.join("external.c");
        symlink("missing-external-target.c", &link).expect("create broken symlink");
        assert!(is_file_or_symlink(&link));
        assert!(!link.is_file());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}
