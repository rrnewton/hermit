//! Export the current manifest policy as deterministic, typed JSON.
//!
//! This is a read-only projection. It deliberately contains no observation or
//! measurement state: those facts belong to the canonical ledgers. Current
//! reproducers use the exact-cell test-harness front door and never call the
//! filesystem-mutating `runner::build_spec` path.

use std::fs;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use hermit_manifest_plan::ci_selection::CiDisabledReasonData;
use hermit_manifest_plan::ci_selection::CiSelection;
use hermit_manifest_plan::runner::DirectCommand;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::ModeRecipe;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::SelectedCell;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::TestRecipe;
use hermit_manifest_plan::timeouts::MANIFEST_SCHEMA;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

const EXPORT_SCHEMA: u64 = 1;
const HELP: &str = "manifest-metadata - export current E2E manifest policy as JSON

USAGE:
  manifest-metadata

The JSON contains current test metadata, every comparable cell in the manifest,
and custom commands selected by full validation. Each comparable cell records
whether full validation selects it. It contains no run result or measurement
state.
";

#[derive(Debug, Serialize)]
struct ManifestMetadata {
    schema: u64,
    manifest_schema: u64,
    manifest_sha256: String,
    tests: Vec<TestMetadata>,
    cells: Vec<CellMetadata>,
    selected_by_full_custom_commands: Vec<CellMetadata>,
}

#[derive(Debug, Serialize)]
struct TestMetadata {
    id: String,
    description: String,
    category: String,
    lane: String,
    requires: Vec<String>,
    occasional: bool,
    program: Option<String>,
    direct: Option<DirectMetadata>,
    build: Option<BuildMetadata>,
    observation: ObservationMetadata,
    preprocessors: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum DirectMetadata {
    Shell { command: String },
    Argv { argv: Vec<String> },
}

#[derive(Debug, Serialize)]
struct BuildMetadata {
    cflags: Vec<String>,
    rustflags: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ObservationMetadata {
    status: bool,
    stdout: bool,
    stderr: bool,
    artifacts: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CellMetadata {
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: String,
    selected_by_full: bool,
    not_selected_by_full_reason: Option<CiDisabledReasonData>,
    not_applicable_reason: Option<String>,
    timeout_seconds: u64,
    guest_args: Vec<String>,
    workdir: Option<String>,
    current_reproducer: Option<CurrentReproducer>,
    current_reproducer_unavailable_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct CurrentReproducer {
    argv: Vec<String>,
    shell_command: String,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("manifest-metadata: REFUSED: {error}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        if matches!(arg.as_str(), "-h" | "--help") && args.next().is_none() {
            print!("{HELP}");
            return Ok(());
        }
        return Err(format!("unexpected argument {arg:?}\n\n{HELP}"));
    }

    let root = repo_root()?;
    let export = build_export(&root)?;
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, &export)
        .map_err(|error| format!("cannot encode manifest metadata: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|error| format!("cannot write manifest metadata: {error}"))?;
    Ok(())
}

fn repo_root() -> Result<PathBuf, String> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository root: {error}"))
}

fn build_export(root: &Path) -> Result<ManifestMetadata, String> {
    let manifest_sha256_before = manifest_sha256(root)?;
    let manifests = ManifestSet::load(root)?;
    let selected_by_full_cells = manifests.select(&Selection {
        population: Some(Population::Required),
        ..Selection::default()
    })?;
    let selected_by_full_ids = selected_by_full_cells
        .iter()
        .map(|cell| cell.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let tests = manifests
        .all_tests()
        .map(|(category, _, test)| test_metadata(category, test))
        .collect();

    let mut cells = Vec::new();
    // Keep both source populations: selection by full validation is a separate
    // fact from whether a comparable cell is present in the manifest.
    for population in [Population::Enabled, Population::Disabled] {
        let selected = manifests.select(&Selection {
            population: Some(population),
            include_occasional: true,
            include_manual: true,
            ..Selection::default()
        })?;
        for cell in selected {
            if cell.id.mode != "custom" {
                cells.push(cell_metadata(
                    &cell,
                    selected_by_full_ids.contains(&cell.id),
                )?);
            }
        }
    }
    sort_and_require_unique_cells("comparable cells in the manifest", &mut cells)?;

    let mut selected_by_full_custom_commands = selected_by_full_cells
        .into_iter()
        .filter(|cell| cell.id.mode == "custom")
        .map(|cell| cell_metadata(&cell, true))
        .collect::<Result<Vec<_>, _>>()?;
    sort_and_require_unique_cells(
        "custom commands selected by full validation",
        &mut selected_by_full_custom_commands,
    )?;

    let manifest_sha256 =
        require_stable_manifest_sha(manifest_sha256_before, manifest_sha256(root)?)?;

    Ok(ManifestMetadata {
        schema: EXPORT_SCHEMA,
        manifest_schema: MANIFEST_SCHEMA,
        manifest_sha256,
        tests,
        cells,
        selected_by_full_custom_commands,
    })
}

fn require_stable_manifest_sha(before: String, after: String) -> Result<String, String> {
    if before == after {
        Ok(before)
    } else {
        Err(format!(
            "manifest inputs changed while they were being read: before={before}, after={after}"
        ))
    }
}

fn test_metadata(category: &str, test: &TestRecipe) -> TestMetadata {
    let direct = test.direct.as_ref().map(|direct| match direct {
        DirectCommand::Shell(command) => DirectMetadata::Shell {
            command: command.clone(),
        },
        DirectCommand::Argv(argv) => DirectMetadata::Argv { argv: argv.clone() },
    });
    let build = test.build.as_ref().map(|build| BuildMetadata {
        cflags: build.cflags.clone(),
        rustflags: build.rustflags.clone(),
    });
    TestMetadata {
        id: test.id.clone(),
        description: test.description.clone(),
        category: category.to_string(),
        lane: test.lane.clone(),
        requires: test.requires.clone(),
        occasional: test.occasional,
        program: test.program.clone(),
        direct,
        build,
        observation: ObservationMetadata {
            status: test.observation.status,
            stdout: test.observation.stdout,
            stderr: test.observation.stderr,
            artifacts: test.observation.artifacts.clone(),
        },
        preprocessors: test.preprocessors.clone(),
    }
}

fn cell_metadata(cell: &SelectedCell, selected_by_full: bool) -> Result<CellMetadata, String> {
    let backend = cell.id.backend.as_deref().unwrap_or("native").to_string();
    let recipe = cell
        .test
        .modes
        .get(&cell.id.mode)
        .ok_or_else(|| format!("{} has no {} mode", cell.id.test, cell.id.mode))?;
    let configured_selection = configured_selection(recipe)
        .map_err(|error| format!("{}: {} {error}", cell.id.test, cell.id.mode))?;
    if selected_by_full && !cell.enabled {
        return Err(format!(
            "{}/{}@{} is selected by full validation but is not applicable",
            cell.id.test, cell.id.mode, backend
        ));
    }
    if selected_by_full && !configured_selection.selected(&backend) {
        return Err(format!(
            "{}/{}@{} is selected by full validation but its manifest selection is false",
            cell.id.test, cell.id.mode, backend
        ));
    }
    if selected_by_full && cell.test.occasional {
        return Err(format!(
            "{}/{}@{} is selected by full validation but its test is marked occasional",
            cell.id.test, cell.id.mode, backend
        ));
    }

    let (not_selected_by_full_reason, not_applicable_reason) = if cell.enabled {
        let reason = if selected_by_full {
            None
        } else if let Some(reason) = configured_selection.reason(&backend) {
            Some(reason.clone())
        } else if cell.test.occasional {
            Some(CiDisabledReasonData {
                result: None,
                evidence: None,
                reason: "This test is marked occasional, and full validation does not select occasional tests."
                    .to_string(),
            })
        } else {
            return Err(format!(
                "{}/{}@{} is not selected by full validation without a reason",
                cell.id.test, cell.id.mode, backend
            ));
        };
        (reason, None)
    } else {
        (
            None,
            Some(
                recipe
                    .backends_disabled
                    .get(&backend)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "{}/{}@{} is not applicable without a reason",
                            cell.id.test, cell.id.mode, backend
                        )
                    })?,
            ),
        )
    };
    let (current_reproducer, current_reproducer_unavailable_reason) =
        exact_cell_reproducer(&cell.id.test, &cell.id.mode, &backend, cell.enabled);

    Ok(CellMetadata {
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: backend.clone(),
        selected_by_full,
        not_selected_by_full_reason,
        not_applicable_reason,
        timeout_seconds: cell.timeout_seconds,
        guest_args: recipe.guest_args.get(&backend).cloned().unwrap_or_default(),
        workdir: recipe.workdir.clone(),
        current_reproducer,
        current_reproducer_unavailable_reason,
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

fn exact_cell_reproducer(
    test: &str,
    mode: &str,
    backend: &str,
    applicable: bool,
) -> (Option<CurrentReproducer>, Option<String>) {
    if !applicable && mode == "naked" && backend == "native" {
        return (
            None,
            Some(
                "test-harness --probe-disabled requires --backend, but its backend selector does not accept native"
                    .to_string(),
            ),
        );
    }

    let mut argv = vec![
        "target/debug/test-harness".to_string(),
        "run".to_string(),
        if applicable {
            "--include-manual".to_string()
        } else {
            "--probe-disabled".to_string()
        },
        "--include-occasional".to_string(),
        "--test".to_string(),
        test.to_string(),
        "--mode".to_string(),
        mode.to_string(),
    ];
    if backend != "native" {
        argv.extend(["--backend".to_string(), backend.to_string()]);
    }
    let shell_command = argv
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    (
        Some(CurrentReproducer {
            argv,
            shell_command,
        }),
        None,
    )
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn sort_and_require_unique_cells(label: &str, cells: &mut [CellMetadata]) -> Result<(), String> {
    cells.sort_by(|left, right| cell_key(left).cmp(&cell_key(right)));
    for pair in cells.windows(2) {
        if cell_key(&pair[0]) == cell_key(&pair[1]) {
            return Err(format!(
                "{label} contains duplicate identity {}/{}/{}@{}",
                pair[0].category, pair[0].test, pair[0].mode, pair[0].backend
            ));
        }
    }
    Ok(())
}

fn cell_key(cell: &CellMetadata) -> (&str, &str, &str, &str, &str) {
    (
        &cell.lane,
        &cell.category,
        &cell.test,
        &cell.mode,
        &cell.backend,
    )
}

fn manifest_sha256(root: &Path) -> Result<String, String> {
    let dir = root.join("tests/e2e/manifests");
    let mut paths = fs::read_dir(&dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("cannot read an entry in {}: {error}", dir.display()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no YAML manifests found in {}", dir.display()));
    }

    let mut digest = Sha256::new();
    digest.update(b"hermit-manifest-metadata-v1\0");
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("manifest name is not UTF-8: {}", path.display()))?;
        let contents =
            fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        digest.update(
            u64::try_from(name.len())
                .map_err(|_| "manifest name length does not fit u64".to_string())?
                .to_le_bytes(),
        );
        digest.update(name.as_bytes());
        digest.update(
            u64::try_from(contents.len())
                .map_err(|_| "manifest length does not fit u64".to_string())?
                .to_le_bytes(),
        );
        digest.update(contents);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn identity(cell: &CellMetadata) -> (String, String, String, String, String) {
        (
            cell.lane.clone(),
            cell.category.clone(),
            cell.test.clone(),
            cell.mode.clone(),
            cell.backend.clone(),
        )
    }

    struct TemporaryManifestRoot(PathBuf);

    impl TemporaryManifestRoot {
        fn new(name: &str, manifest: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "hermit-manifest-metadata-{name}-{}-{nonce}",
                std::process::id()
            ));
            let directory = root.join("tests/e2e/manifests");
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("defaults.yaml"),
                "schema: 3\ntimeout_seconds: 15\nnextest: []\n",
            )
            .unwrap();
            fs::write(directory.join(format!("{name}.yaml")), manifest).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryManifestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const OCCASIONAL_MANIFEST: &str = r#"schema: 3
bucket: occasional
test:
  - id: occasional/full-selection
    description: Fixture proving that full validation excludes occasional tests
    lane: portable
    requires: []
    occasional: true
    direct:
      - /bin/true
    observation:
      status: true
      stdout: false
      stderr: false
      artifacts: []
    modes:
      verify:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture}
      naked:
        ci: false
        backends_enabled: []
        backends_disabled: {native: Not applicable in this fixture}
      replay:
        ci: false
        backends_enabled: []
        backends_disabled: {ptrace: Not applicable in this fixture, dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture}
      chaos:
        ci: false
        backends_enabled: []
        backends_disabled: {ptrace: Not applicable in this fixture, dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture}
      custom:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture}
"#;

    #[test]
    fn occasional_cells_remain_in_the_manifest_but_full_does_not_select_them() {
        let fixture = TemporaryManifestRoot::new("occasional", OCCASIONAL_MANIFEST);
        let manifests = ManifestSet::load(fixture.path()).unwrap();
        let cells_in_manifest = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                include_occasional: true,
                include_manual: true,
                ..Selection::default()
            })
            .unwrap();
        assert!(cells_in_manifest.iter().any(|cell| {
            cell.id.mode == "verify" && cell.id.backend.as_deref() == Some("ptrace")
        }));
        assert!(cells_in_manifest.iter().any(|cell| {
            cell.id.mode == "custom" && cell.id.backend.as_deref() == Some("ptrace")
        }));
        assert!(
            manifests
                .select(&Selection {
                    population: Some(Population::Required),
                    ..Selection::default()
                })
                .unwrap()
                .is_empty()
        );

        let export = build_export(fixture.path()).unwrap();
        let verify = export
            .cells
            .iter()
            .find(|cell| {
                cell.test == "occasional/full-selection"
                    && cell.mode == "verify"
                    && cell.backend == "ptrace"
            })
            .unwrap();
        assert!(!verify.selected_by_full);
        assert_eq!(
            verify
                .not_selected_by_full_reason
                .as_ref()
                .map(|reason| reason.reason.as_str()),
            Some(
                "This test is marked occasional, and full validation does not select occasional tests."
            )
        );
        assert!(verify.not_applicable_reason.is_none());
        assert!(export.selected_by_full_custom_commands.is_empty());
    }

    #[test]
    fn current_reproducer_uses_only_the_exact_harness_front_door() {
        let (applicable, missing) = exact_cell_reproducer("bucket/test", "verify", "ptrace", true);
        assert!(missing.is_none());
        let applicable = applicable.unwrap();
        assert_eq!(applicable.argv[0], "target/debug/test-harness");
        assert!(applicable.argv.iter().any(|arg| arg == "--include-manual"));
        assert!(!applicable.argv.iter().any(|arg| arg == "--probe-disabled"));
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--test", "bucket/test"])
        );
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--mode", "verify"])
        );
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--backend", "ptrace"])
        );

        let (not_applicable, missing) =
            exact_cell_reproducer("bucket/test", "verify", "sabre", false);
        assert!(missing.is_none());
        let not_applicable = not_applicable.unwrap();
        assert!(
            not_applicable
                .argv
                .iter()
                .any(|arg| arg == "--probe-disabled")
        );
        assert!(
            !not_applicable
                .argv
                .iter()
                .any(|arg| arg == "--include-manual")
        );

        let (native, missing) = exact_cell_reproducer("bucket/test", "naked", "native", true);
        assert!(missing.is_none());
        assert!(!native.unwrap().argv.iter().any(|arg| arg == "--backend"));

        let (not_applicable_native, missing) =
            exact_cell_reproducer("bucket/test", "naked", "native", false);
        assert!(not_applicable_native.is_none());
        assert!(missing.unwrap().contains("does not accept native"));
    }

    #[test]
    fn shell_command_quotes_untrusted_arguments() {
        let (reproducer, missing) =
            exact_cell_reproducer("bucket/test with space", "verify", "ptrace", true);
        assert!(missing.is_none());
        assert!(
            reproducer
                .unwrap()
                .shell_command
                .contains("'bucket/test with space'")
        );
    }

    #[test]
    fn manifest_digest_must_bound_one_stable_read() {
        assert_eq!(
            require_stable_manifest_sha("same".into(), "same".into()).unwrap(),
            "same"
        );
        let error = require_stable_manifest_sha("before".into(), "after".into()).unwrap_err();
        assert!(error.contains("changed while they were being read"));
        assert!(error.contains("before=before"));
        assert!(error.contains("after=after"));
    }

    #[test]
    fn shipped_export_is_deterministic_and_matches_the_comparable_projection() {
        let root = root();
        let first = build_export(&root).unwrap();
        let second = build_export(&root).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );

        let test_ids = first
            .tests
            .iter()
            .map(|test| test.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(test_ids.len(), first.tests.len());

        let mut per_test_modes = BTreeMap::<&str, BTreeMap<&str, usize>>::new();
        for cell in &first.cells {
            *per_test_modes
                .entry(&cell.test)
                .or_default()
                .entry(&cell.mode)
                .or_default() += 1;
        }
        assert_eq!(per_test_modes.len(), first.tests.len());
        for (test, modes) in per_test_modes {
            assert_eq!(
                modes,
                BTreeMap::from([("chaos", 5), ("naked", 1), ("replay", 5), ("verify", 5),]),
                "wrong comparable matrix for {test}"
            );
        }

        let tracked: Value =
            serde_json::from_slice(&fs::read(root.join("ci/compat-envelope/cells.json")).unwrap())
                .unwrap();
        let tracked = tracked["cells"].as_array().unwrap();
        let tracked_identities = tracked
            .iter()
            .map(|cell| {
                (
                    cell["lane"].as_str().unwrap().to_string(),
                    cell["category"].as_str().unwrap().to_string(),
                    cell["test"].as_str().unwrap().to_string(),
                    cell["mode"].as_str().unwrap().to_string(),
                    cell["backend"].as_str().unwrap().to_string(),
                )
            })
            .collect::<BTreeSet<_>>();
        let exported_identities = first.cells.iter().map(identity).collect::<BTreeSet<_>>();
        assert_eq!(exported_identities, tracked_identities);

        let encoded = serde_json::to_string(&first).unwrap();
        assert!(!encoded.contains(&root.to_string_lossy().to_string()));
        let encoded_value = serde_json::to_value(&first).unwrap();
        assert!(encoded_value.get("selected_custom_commands").is_none());
        assert!(
            encoded_value
                .get("selected_by_full_custom_commands")
                .is_some()
        );
        assert!(
            encoded_value["cells"]
                .as_array()
                .unwrap()
                .iter()
                .all(|cell| {
                    cell.get("measurement").is_none()
                        && cell.get("measured").is_none()
                        && cell.get("state").is_none()
                        && cell.get("enabled").is_none()
                        && cell.get("ci").is_none()
                        && cell.get("ci_disabled_reason").is_none()
                        && cell.get("selected_by_full").is_some()
                        && cell.get("not_selected_by_full_reason").is_some()
                })
        );

        let cells_in_manifest = first.cells.len();
        let cells_selected_by_full = first
            .cells
            .iter()
            .filter(|cell| cell.selected_by_full)
            .count();
        assert!(cells_selected_by_full > 0);
        assert!(cells_selected_by_full < cells_in_manifest);
        for cell in &first.cells {
            if cell.selected_by_full {
                assert!(cell.not_selected_by_full_reason.is_none());
                assert!(cell.not_applicable_reason.is_none());
            } else {
                assert!(
                    cell.not_selected_by_full_reason.is_some()
                        || cell.not_applicable_reason.is_some()
                );
                assert!(
                    cell.not_selected_by_full_reason.is_none()
                        || cell.not_applicable_reason.is_none()
                );
            }
        }
        assert!(
            first
                .selected_by_full_custom_commands
                .iter()
                .all(|cell| cell.mode == "custom" && cell.selected_by_full)
        );
    }
}
