//! Shared typed projection of the current manifest policy.
//!
//! This is a read-only projection. It deliberately contains no observation or
//! measurement state: those facts belong to the canonical ledgers. Current
//! reproducers use the exact-cell test-harness front door and never call the
//! filesystem-mutating `runner::build_spec` path.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::ci_selection::CiDisabledReasonData;
use crate::ci_selection::CiSelection;
use crate::runner::DirectCommand;
use crate::runner::ManifestSet;
use crate::runner::ModeRecipe;
use crate::runner::Population;
use crate::runner::SelectedCell;
use crate::runner::Selection;
use crate::runner::TestRecipe;
use crate::timeouts::MANIFEST_SCHEMA;

const EXPORT_SCHEMA: u64 = 1;
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestMetadata {
    pub schema: u64,
    pub manifest_schema: u64,
    pub manifest_sha256: String,
    pub tests: Vec<TestMetadata>,
    pub cells: Vec<CellMetadata>,
    pub selected_by_full_custom_commands: Vec<CellMetadata>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestMetadata {
    pub id: String,
    pub description: String,
    pub category: String,
    pub lane: String,
    pub requires: Vec<String>,
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectMetadata>,
    pub build: Option<BuildMetadata>,
    pub observation: ObservationMetadata,
    pub preprocessors: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectMetadata {
    Shell { command: String },
    Argv { argv: Vec<String> },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildMetadata {
    pub cflags: Vec<String>,
    pub rustflags: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationMetadata {
    pub status: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellMetadata {
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: String,
    pub selected_by_full: bool,
    pub not_selected_by_full_reason: Option<CiDisabledReasonData>,
    pub not_applicable_reason: Option<String>,
    pub timeout_seconds: u64,
    pub guest_args: Vec<String>,
    pub workdir: Option<String>,
    pub current_reproducer: Option<CurrentReproducer>,
    pub current_reproducer_unavailable_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentReproducer {
    pub argv: Vec<String>,
    pub shell_command: String,
}

pub fn build_export(root: &Path) -> Result<ManifestMetadata, String> {
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
    use std::path::PathBuf;

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

    fn contract_fixture() -> ManifestMetadata {
        ManifestMetadata {
            schema: 1,
            manifest_schema: 3,
            manifest_sha256: "abc".into(),
            tests: vec![
                TestMetadata {
                    id: "shell".into(),
                    description: "shell command".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    requires: vec!["kvm".into()],
                    occasional: false,
                    program: None,
                    direct: Some(DirectMetadata::Shell {
                        command: "true".into(),
                    }),
                    build: None,
                    observation: ObservationMetadata {
                        status: true,
                        stdout: false,
                        stderr: true,
                        artifacts: vec!["result.txt".into()],
                    },
                    preprocessors: Vec::new(),
                },
                TestMetadata {
                    id: "argv".into(),
                    description: "argv command".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    requires: Vec::new(),
                    occasional: true,
                    program: Some("fixture-bin".into()),
                    direct: Some(DirectMetadata::Argv {
                        argv: vec!["fixture-bin".into(), "--flag".into()],
                    }),
                    build: Some(BuildMetadata {
                        cflags: vec!["-O2".into()],
                        rustflags: vec!["-Copt-level=2".into()],
                    }),
                    observation: ObservationMetadata {
                        status: false,
                        stdout: true,
                        stderr: false,
                        artifacts: Vec::new(),
                    },
                    preprocessors: vec!["e9patch".into()],
                },
            ],
            cells: vec![
                CellMetadata {
                    test: "shell".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    mode: "verify".into(),
                    backend: "ptrace".into(),
                    selected_by_full: false,
                    not_selected_by_full_reason: Some(CiDisabledReasonData {
                        result: Some(crate::ci_selection::CiDisabledResult::Unavailable),
                        evidence: Some("fixture-evidence".into()),
                        reason: "fixture reason".into(),
                    }),
                    not_applicable_reason: None,
                    timeout_seconds: 15,
                    guest_args: vec!["--guest".into()],
                    workdir: Some("fixture-workdir".into()),
                    current_reproducer: Some(CurrentReproducer {
                        argv: vec!["test-harness".into(), "run".into()],
                        shell_command: "test-harness run".into(),
                    }),
                    current_reproducer_unavailable_reason: None,
                },
                CellMetadata {
                    test: "argv".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    mode: "naked".into(),
                    backend: "native".into(),
                    selected_by_full: false,
                    not_selected_by_full_reason: None,
                    not_applicable_reason: Some("not applicable".into()),
                    timeout_seconds: 30,
                    guest_args: Vec::new(),
                    workdir: None,
                    current_reproducer: None,
                    current_reproducer_unavailable_reason: Some("no reproducer".into()),
                },
            ],
            selected_by_full_custom_commands: Vec::new(),
        }
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
    fn metadata_contract_has_stable_json_and_round_trips() {
        let encoded = serde_json::to_string(&contract_fixture()).unwrap();
        let expected = concat!(
            r#"{"schema":1,"manifest_schema":3,"manifest_sha256":"abc","tests":["#,
            r#"{"id":"shell","description":"shell command","category":"fixture","lane":"portable","requires":["kvm"],"occasional":false,"program":null,"direct":{"kind":"shell","command":"true"},"build":null,"observation":{"status":true,"stdout":false,"stderr":true,"artifacts":["result.txt"]},"preprocessors":[]},"#,
            r#"{"id":"argv","description":"argv command","category":"fixture","lane":"portable","requires":[],"occasional":true,"program":"fixture-bin","direct":{"kind":"argv","argv":["fixture-bin","--flag"]},"build":{"cflags":["-O2"],"rustflags":["-Copt-level=2"]},"observation":{"status":false,"stdout":true,"stderr":false,"artifacts":[]},"preprocessors":["e9patch"]}],"#,
            r#""cells":[{"test":"shell","category":"fixture","lane":"portable","mode":"verify","backend":"ptrace","selected_by_full":false,"not_selected_by_full_reason":{"result":"unavailable","evidence":"fixture-evidence","reason":"fixture reason"},"not_applicable_reason":null,"timeout_seconds":15,"guest_args":["--guest"],"workdir":"fixture-workdir","current_reproducer":{"argv":["test-harness","run"],"shell_command":"test-harness run"},"current_reproducer_unavailable_reason":null},"#,
            r#"{"test":"argv","category":"fixture","lane":"portable","mode":"naked","backend":"native","selected_by_full":false,"not_selected_by_full_reason":null,"not_applicable_reason":"not applicable","timeout_seconds":30,"guest_args":[],"workdir":null,"current_reproducer":null,"current_reproducer_unavailable_reason":"no reproducer"}],"#,
            r#""selected_by_full_custom_commands":[]}"#,
        );
        assert_eq!(encoded, expected);

        let decoded: ManifestMetadata = serde_json::from_str(&encoded).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
    }

    #[test]
    fn metadata_deserialization_refuses_unknown_and_missing_fields() {
        let mut unknown = serde_json::to_value(contract_fixture()).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(unknown).is_err());

        let mut missing = serde_json::to_value(contract_fixture()).unwrap();
        missing.as_object_mut().unwrap().remove("schema");
        assert!(serde_json::from_value::<ManifestMetadata>(missing).is_err());

        let mut nested_unknown = serde_json::to_value(contract_fixture()).unwrap();
        nested_unknown["tests"][0]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(nested_unknown).is_err());

        let mut direct_unknown = serde_json::to_value(contract_fixture()).unwrap();
        direct_unknown["tests"][0]["direct"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(direct_unknown).is_err());

        let mut nested_missing = serde_json::to_value(contract_fixture()).unwrap();
        nested_missing["tests"][0]["observation"]
            .as_object_mut()
            .unwrap()
            .remove("status");
        assert!(serde_json::from_value::<ManifestMetadata>(nested_missing).is_err());
    }

    #[test]
    fn duplicate_cell_identity_is_refused() {
        let mut first = contract_fixture();
        let mut second = contract_fixture();
        let mut cells = vec![first.cells.remove(0), second.cells.remove(0)];
        let error = sort_and_require_unique_cells("fixture", &mut cells).unwrap_err();
        assert!(error.contains("duplicate identity fixture/shell/verify@ptrace"));
    }

    #[test]
    fn binary_is_a_thin_caller_without_duplicate_metadata_types() {
        let binary = include_str!("bin/manifest-metadata.rs");
        assert!(binary.contains("manifest_metadata::build_export"));
        for declaration in [
            "struct ManifestMetadata",
            "struct TestMetadata",
            "enum DirectMetadata",
            "struct BuildMetadata",
            "struct ObservationMetadata",
            "struct CellMetadata",
            "struct CurrentReproducer",
            "fn require_stable_manifest_sha",
            "fn test_metadata",
            "fn cell_metadata",
            "fn configured_selection",
            "fn exact_cell_reproducer",
            "fn shell_quote",
            "fn sort_and_require_unique_cells",
            "fn cell_key",
            "fn manifest_sha256",
            "mod tests",
        ] {
            assert!(!binary.contains(declaration), "duplicate {declaration}");
        }
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
