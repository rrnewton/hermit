//! Compatibility-corpus sections of E2E manifest documents.
//!
//! A corpus is one lane's settings written once and a list of `(label, argv)`
//! rows written once. A manifest document may carry it as a `corpus:` section;
//! [`expand_corpus`] turns every row into an ordinary test recipe before either
//! validator or the runner sees the document, so a corpus row is validated,
//! selected, executed and reported exactly like a hand-written test. The
//! expansion adds no semantics of its own: everything it writes is an existing
//! manifest key.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde_yaml::Mapping;
use serde_yaml::Value;

/// The backends a manifest mode partitions into enabled and disabled.
const BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];

/// The modes every test recipe declares.
const NON_VERIFY_MODES: [&str; 4] = ["naked", "replay", "chaos", "custom"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    /// Prefix of every expanded test's description.
    description: String,
    lane: String,
    requires: Vec<String>,
    /// The one backend every row's verify cell runs on.
    backend: String,
    verify: CorpusVerify,
    /// Rows whose measured failure is a non-blocking diagnostic.
    #[serde(default)]
    diagnostic: Option<CorpusRowPolicy>,
    /// Rows whose corpus-backend cell gets a longer budget than `verify`'s,
    /// each with the reason.
    #[serde(default)]
    heavy: Option<CorpusRowPolicy>,
    /// Verify cells on further backends that a focused run type adds to the
    /// rows; the default (full) validation does not select them.
    #[serde(default)]
    focused: Vec<CorpusFocused>,
    /// Enabled cells measured red: each stays enabled with `ci: false` and a
    /// structured `ci_disabled_reason`, so its run type does not require it.
    #[serde(default)]
    unselected: Vec<CorpusUnselected>,
    rows: Vec<CorpusRow>,
}

/// One failure class: the cells on `backend` of the named rows, with the
/// result class, the evidence (an issue) and the reason every one carries.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusUnselected {
    backend: String,
    result: crate::ci_selection::CiDisabledResult,
    evidence: String,
    reason: String,
    rows: Vec<String>,
}

/// One focused run type's verify cell on one more backend, on every row
/// except the named ones. The cell shares the corpus's verify settings
/// (environment, comparator, single attempt, budget), except `hermit_args`,
/// which belong to the corpus backend alone, and diagnostic status, which no
/// focused cell has.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFocused {
    /// The run type, written as the cell's label (runner::cell_labels).
    label: String,
    backend: String,
    /// Rows without this cell, each with the reason that becomes the
    /// backend's `backends_disabled` entry on that row.
    #[serde(default)]
    except: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusVerify {
    #[serde(default)]
    hermit_args: Vec<String>,
    hermit_args_reason: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    comparator: Option<String>,
    comparator_reason: Option<String>,
    no_retry_reason: Option<String>,
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
}

/// A budget and a reason per named row.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRowPolicy {
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
    rows: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRow {
    /// The program's name in the corpus; diagnostic policy names rows by it.
    label: String,
    /// The test id's suffix after `<bucket>/`, when the label is not a valid
    /// one (test ids are lowercase letters, digits and `-`, so `g++` needs
    /// one). Defaults to the label.
    #[serde(default)]
    id: Option<String>,
    argv: Vec<String>,
    /// Run types of the whole test (every cell of it); empty means the
    /// default (full) validation.
    #[serde(default)]
    labels: Vec<String>,
}

fn string(value: &str) -> Value {
    Value::String(value.to_string())
}

fn strings(values: &[String]) -> Value {
    Value::Sequence(values.iter().map(|value| string(value)).collect())
}

fn mapping<'a>(entries: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    let mut out = Mapping::new();
    for (key, value) in entries {
        out.insert(string(key), value);
    }
    Value::Mapping(out)
}

/// Expand a document's `corpus:` section, if it has one, into ordinary `test:`
/// recipes appended after any hand-written ones. A document without the
/// section is returned unchanged.
pub fn expand_corpus(mut document: Value) -> Result<Value, String> {
    let Some(table) = document.as_mapping_mut() else {
        return Ok(document);
    };
    let Some(section) = table.remove(string("corpus")) else {
        return Ok(document);
    };
    let bucket = table
        .get(string("bucket"))
        .and_then(Value::as_str)
        .ok_or("a manifest with a corpus section needs a string bucket")?
        .to_string();
    let corpus: Corpus = serde_yaml::from_value(section)
        .map_err(|error| format!("{bucket}: invalid corpus section: {error}"))?;
    let tests = expand(&bucket, &corpus)?;
    let entry = table
        .entry(string("test"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    let Value::Sequence(list) = entry else {
        return Err(format!("{bucket}: test must be a list"));
    };
    list.extend(tests);
    Ok(document)
}

fn nonempty(bucket: &str, what: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.trim() != value {
        return Err(format!(
            "{bucket}: corpus {what} must be nonempty and trimmed"
        ));
    }
    Ok(())
}

fn expand(bucket: &str, corpus: &Corpus) -> Result<Vec<Value>, String> {
    if !BACKENDS.contains(&corpus.backend.as_str()) {
        return Err(format!(
            "{bucket}: corpus backend `{}` is not one of {BACKENDS:?}",
            corpus.backend
        ));
    }
    nonempty(bucket, "description", &corpus.description)?;
    let mut labels = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for row in &corpus.rows {
        if !labels.insert(row.label.as_str()) {
            return Err(format!("{bucket}: corpus row `{}` is repeated", row.label));
        }
        if row.id.as_deref() == Some(row.label.as_str()) {
            return Err(format!(
                "{bucket}: corpus row `{}` repeats its label as its id",
                row.label
            ));
        }
        if !ids.insert(row.id.as_deref().unwrap_or(&row.label)) {
            return Err(format!(
                "{bucket}: corpus row `{}` repeats another row's test id",
                row.label
            ));
        }
        if row.argv.is_empty() {
            return Err(format!(
                "{bucket}: corpus row `{}` has an empty argv",
                row.label
            ));
        }
    }
    if labels.is_empty() {
        return Err(format!("{bucket}: corpus has no rows"));
    }
    let policy_rows = |what: &str, rows: &BTreeMap<String, String>| -> Result<(), String> {
        for (label, reason) in rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {what} names `{label}`, which is no row"
                ));
            }
            nonempty(bucket, &format!("{what} reason for `{label}`"), reason)?;
        }
        Ok(())
    };
    if let Some(diagnostic) = &corpus.diagnostic {
        policy_rows("diagnostic", &diagnostic.rows)?;
    }
    if let Some(heavy) = &corpus.heavy {
        policy_rows("heavy", &heavy.rows)?;
        if let Some(both) = heavy.rows.keys().find(|label| {
            corpus
                .diagnostic
                .as_ref()
                .is_some_and(|diagnostic| diagnostic.rows.contains_key(*label))
        }) {
            return Err(format!(
                "{bucket}: corpus row `{both}` is both diagnostic and heavy"
            ));
        }
    }
    let mut focused_backends = BTreeSet::new();
    for focused in &corpus.focused {
        if focused.backend == corpus.backend || !BACKENDS.contains(&focused.backend.as_str()) {
            return Err(format!(
                "{bucket}: corpus focused backend `{}` must be one of {BACKENDS:?} other than `{}`",
                focused.backend, corpus.backend
            ));
        }
        if !focused_backends.insert(focused.backend.as_str()) {
            return Err(format!(
                "{bucket}: corpus focused backend `{}` is repeated",
                focused.backend
            ));
        }
        nonempty(bucket, "focused label", &focused.label)?;
        policy_rows(
            &format!("focused {} except", focused.backend),
            &focused.except,
        )?;
    }
    let mut unselected_cells = BTreeMap::<(&str, &str), &CorpusUnselected>::new();
    for class in &corpus.unselected {
        nonempty(bucket, "unselected evidence", &class.evidence)?;
        nonempty(bucket, "unselected reason", &class.reason)?;
        if class.rows.is_empty() {
            return Err(format!("{bucket}: corpus unselected class names no rows"));
        }
        for label in &class.rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}`, which is no row"
                ));
            }
            let on_backend = class.backend == corpus.backend
                || corpus.focused.iter().any(|focused| {
                    focused.backend == class.backend && !focused.except.contains_key(label)
                });
            if !on_backend {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}` on `{}`, which has no cell there",
                    class.backend
                ));
            }
            if unselected_cells
                .insert((class.backend.as_str(), label.as_str()), class)
                .is_some()
            {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}` on `{}` twice",
                    class.backend
                ));
            }
        }
    }
    let off_reason = format!(
        "A {bucket} corpus row runs only its lane's verify cell on {}",
        corpus.backend
    );
    let mut tests = Vec::with_capacity(corpus.rows.len());
    for row in &corpus.rows {
        let diagnostic = corpus
            .diagnostic
            .as_ref()
            .and_then(|policy| policy.rows.get(&row.label).map(|reason| (policy, reason)));
        let mut heavy_reason = None;
        let (timeout, cpu_timeout, slow_reason) = match diagnostic {
            Some((policy, _)) => (
                policy.timeout_seconds,
                policy.cpu_timeout_seconds,
                policy.slow_reason.as_str(),
            ),
            None => match corpus
                .heavy
                .as_ref()
                .filter(|heavy| heavy.rows.contains_key(&row.label))
            {
                Some(heavy) => (
                    heavy.timeout_seconds,
                    heavy.cpu_timeout_seconds,
                    heavy_reason
                        .insert(format!("{}: {}", heavy.slow_reason, heavy.rows[&row.label]))
                        .as_str(),
                ),
                None => (
                    corpus.verify.timeout_seconds,
                    corpus.verify.cpu_timeout_seconds,
                    corpus.verify.slow_reason.as_str(),
                ),
            },
        };
        // The corpus backend first, then every focused cell this row keeps.
        let cells = std::iter::once((
            corpus.backend.as_str(),
            None,
            timeout,
            cpu_timeout,
            slow_reason,
        ))
        .chain(
            corpus
                .focused
                .iter()
                .filter(|focused| !focused.except.contains_key(&row.label))
                .map(|focused| {
                    (
                        focused.backend.as_str(),
                        Some(focused.label.as_str()),
                        corpus.verify.timeout_seconds,
                        corpus.verify.cpu_timeout_seconds,
                        corpus.verify.slow_reason.as_str(),
                    )
                }),
        )
        .collect::<Vec<_>>();
        let enabled = cells.iter().map(|cell| cell.0).collect::<Vec<_>>();
        let off_reason = if enabled.len() == 1 {
            off_reason.clone()
        } else {
            format!(
                "{off_reason}, and a focused run type's verify cell on {}",
                enabled[1..].join(" and ")
            )
        };
        let disabled = BACKENDS
            .iter()
            .filter(|backend| !enabled.contains(backend))
            .map(|backend| {
                let reason = corpus
                    .focused
                    .iter()
                    .find(|focused| focused.backend == *backend)
                    .and_then(|focused| focused.except.get(&row.label))
                    .map_or(off_reason.as_str(), String::as_str);
                (*backend, string(reason))
            });
        let per_cell =
            |value: &dyn Fn(u64, u64, &str) -> Value| {
                mapping(cells.iter().map(|(backend, _, timeout, cpu, slow)| {
                    (*backend, value(*timeout, *cpu, slow))
                }))
            };
        let unselected = enabled
            .iter()
            .filter_map(|backend| {
                unselected_cells
                    .get(&(*backend, row.label.as_str()))
                    .map(|class| (*backend, *class))
            })
            .collect::<Vec<_>>();
        let ci = if unselected.is_empty() {
            Value::Bool(true)
        } else {
            mapping(enabled.iter().map(|backend| {
                (
                    *backend,
                    Value::Bool(!unselected.iter().any(|(off, _)| off == backend)),
                )
            }))
        };
        let mut verify = vec![
            ("ci", ci),
            (
                "backends_enabled",
                Value::Sequence(enabled.iter().map(|backend| string(backend)).collect()),
            ),
            ("backends_disabled", mapping(disabled)),
            (
                "timeout_seconds",
                per_cell(&|timeout, _, _| Value::from(timeout)),
            ),
            (
                "cpu_timeout_seconds",
                per_cell(&|_, cpu, _| Value::from(cpu)),
            ),
            ("slow_reason", per_cell(&|_, _, slow| string(slow))),
        ];
        if !unselected.is_empty() {
            let result = |class: &CorpusUnselected| {
                serde_yaml::to_value(class.result).expect("a result class serializes")
            };
            verify.push((
                "ci_disabled_reason",
                mapping(unselected.iter().map(|(backend, class)| {
                    (
                        *backend,
                        mapping([
                            ("result", result(class)),
                            ("evidence", string(&class.evidence)),
                            ("reason", string(&class.reason)),
                        ]),
                    )
                })),
            ));
        }
        let focused_labels = cells
            .iter()
            .filter_map(|(backend, label, ..)| {
                label.map(|label| (*backend, strings(&[label.to_string()])))
            })
            .collect::<Vec<_>>();
        if !focused_labels.is_empty() {
            verify.push(("labels", mapping(focused_labels)));
        }
        if !corpus.verify.hermit_args.is_empty() {
            verify.push((
                "hermit_args",
                mapping([(corpus.backend.as_str(), strings(&corpus.verify.hermit_args))]),
            ));
        }
        if let Some(reason) = &corpus.verify.hermit_args_reason {
            verify.push(("hermit_args_reason", string(reason)));
        }
        if !corpus.verify.env.is_empty() {
            verify.push((
                "env",
                mapping(
                    corpus
                        .verify
                        .env
                        .iter()
                        .map(|(k, v)| (k.as_str(), string(v))),
                ),
            ));
        }
        if let Some(comparator) = &corpus.verify.comparator {
            verify.push(("comparator", string(comparator)));
        }
        if let Some(reason) = &corpus.verify.comparator_reason {
            verify.push(("comparator_reason", string(reason)));
        }
        if let Some(reason) = &corpus.verify.no_retry_reason {
            verify.push(("no_retry_reason", string(reason)));
        }
        if let Some((_, reason)) = diagnostic {
            verify.push((
                "diagnostic",
                mapping([(corpus.backend.as_str(), string(reason))]),
            ));
        }
        let mut modes = vec![("verify", mapping(verify))];
        for mode in NON_VERIFY_MODES {
            let backends_disabled = if mode == "naked" {
                mapping([("native", string(&off_reason))])
            } else {
                mapping(
                    BACKENDS
                        .iter()
                        .map(|backend| (*backend, string(&off_reason))),
                )
            };
            modes.push((
                mode,
                mapping([
                    ("ci", Value::Bool(false)),
                    ("ci_disabled_reason", string(&off_reason)),
                    ("backends_enabled", Value::Sequence(Vec::new())),
                    ("backends_disabled", backends_disabled),
                ]),
            ));
        }
        let description = format!("{} `{}`", corpus.description, row.label);
        let test = vec![
            (
                "id",
                string(&format!(
                    "{bucket}/{}",
                    row.id.as_deref().unwrap_or(&row.label)
                )),
            ),
            ("description", string(&description)),
            ("lane", string(&corpus.lane)),
            ("requires", strings(&corpus.requires)),
            ("occasional", Value::Bool(false)),
            ("direct", strings(&row.argv)),
            (
                "observation",
                mapping([
                    ("status", Value::Bool(true)),
                    ("stdout", Value::Bool(true)),
                    ("stderr", Value::Bool(true)),
                    ("artifacts", Value::Sequence(Vec::new())),
                ]),
            ),
            ("modes", mapping(modes)),
        ];
        let mut test = test;
        if !row.labels.is_empty() {
            test.push(("labels", strings(&row.labels)));
        }
        tests.push(mapping(test));
    }
    Ok(tests)
}

/// The repository root, in a `direct` argv element.
pub const ROOT_DIR_PLACEHOLDER: &str = "{{ROOT_DIR}}";
/// The validation's per-run state directory, in a `direct` argv element: the
/// value of the harness's `VALIDATE_RUN_STATE`. A cell that names it in a run
/// without one is refused rather than run with the literal text.
pub const VALIDATE_RUN_STATE_PLACEHOLDER: &str = "{{VALIDATE_RUN_STATE}}";
const VALIDATE_RUN_STATE_ENV: &str = "VALIDATE_RUN_STATE";

/// Refuse a `direct` argv that spells any `{{...}}` token other than the two
/// known placeholders, so a typo cannot reach a guest as literal text.
pub fn check_direct_placeholders(id: &str, argv: &[String]) -> Result<(), String> {
    for arg in argv {
        let mut rest = arg.as_str();
        while let Some(start) = rest.find("{{") {
            let tail = &rest[start..];
            let known = [ROOT_DIR_PLACEHOLDER, VALIDATE_RUN_STATE_PLACEHOLDER]
                .into_iter()
                .find(|token| tail.starts_with(token));
            let Some(token) = known else {
                return Err(format!(
                    "{id}: direct argv element `{arg}` names an unknown placeholder; only {ROOT_DIR_PLACEHOLDER} and {VALIDATE_RUN_STATE_PLACEHOLDER} exist"
                ));
            };
            rest = &tail[token.len()..];
        }
    }
    Ok(())
}

/// Substitute the two placeholders in a `direct` argv. `run_state` is the
/// harness's `VALIDATE_RUN_STATE`, when it has one.
pub fn resolve_direct_placeholders(
    id: &str,
    argv: &[String],
    root: &std::path::Path,
    run_state: Option<&std::ffi::OsStr>,
) -> Result<Vec<String>, String> {
    check_direct_placeholders(id, argv)?;
    argv.iter()
        .map(|arg| {
            let mut out = arg.replace(ROOT_DIR_PLACEHOLDER, &root.to_string_lossy());
            if out.contains(VALIDATE_RUN_STATE_PLACEHOLDER) {
                let state = run_state.filter(|state| !state.is_empty()).ok_or_else(|| {
                    format!(
                        "{id}: its argv names {VALIDATE_RUN_STATE_PLACEHOLDER} but {VALIDATE_RUN_STATE_ENV} is not set"
                    )
                })?;
                out = out.replace(VALIDATE_RUN_STATE_PLACEHOLDER, &state.to_string_lossy());
            }
            Ok(out)
        })
        .collect()
}

/// The harness's `VALIDATE_RUN_STATE`, for [`resolve_direct_placeholders`].
pub fn validate_run_state() -> Option<std::ffi::OsString> {
    std::env::var_os(VALIDATE_RUN_STATE_ENV)
}

/// Render one `direct` argv element as a shell word for a command line run
/// from the repository root, as the rerunnable command files do: literal text
/// through `quote`, `{{ROOT_DIR}}` as `"$PWD"`, and `{{VALIDATE_RUN_STATE}}` as
/// an expansion that refuses to run when `VALIDATE_RUN_STATE` is unset, as the
/// harness does.
pub fn direct_shell_word(
    id: &str,
    arg: &str,
    quote: impl Fn(&str) -> String,
) -> Result<String, String> {
    check_direct_placeholders(id, &[arg.to_string()])?;
    let mut out = String::new();
    let mut rest = arg;
    while let Some((index, token)) = [ROOT_DIR_PLACEHOLDER, VALIDATE_RUN_STATE_PLACEHOLDER]
        .into_iter()
        .filter_map(|token| rest.find(token).map(|index| (index, token)))
        .min()
    {
        if index > 0 {
            out.push_str(&quote(&rest[..index]));
        }
        out.push_str(if token == ROOT_DIR_PLACEHOLDER {
            "\"$PWD\""
        } else {
            "\"${VALIDATE_RUN_STATE:?VALIDATE_RUN_STATE is not set}\""
        });
        rest = &rest[index + token.len()..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push_str(&quote(rest));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(corpus: &str) -> Value {
        serde_yaml::from_str(&format!("schema: 3\nbucket: fixture\ncorpus:\n{corpus}")).unwrap()
    }

    const CORPUS: &str = r#"
  description: Fixture program
  lane: portable
  requires: [linux]
  backend: ptrace
  verify:
    hermit_args: [--no-virtualize-cpuid]
    hermit_args_reason: fixture configuration
    env: {TMPDIR: /tmp}
    comparator: stripped
    comparator_reason: fixture policy
    no_retry_reason: fixture single run
    timeout_seconds: 60
    cpu_timeout_seconds: 59
    slow_reason: fixture budget
  diagnostic:
    timeout_seconds: 20
    cpu_timeout_seconds: 19
    slow_reason: shortened fixture budget
    rows: {slow: a bounded fixture probe}
  rows:
    - {label: "echo", argv: ["/bin/echo", "x"]}
    - {label: "g++", id: gxx, argv: ["/usr/bin/g++", "--version"]}
    - {label: "slow", argv: ["/bin/true"]}
"#;

    fn tests_of(document: &Value) -> &Vec<Value> {
        document["test"].as_sequence().unwrap()
    }

    #[test]
    fn a_corpus_row_expands_into_one_verify_cell_with_the_lane_settings() {
        let expanded = expand_corpus(document(CORPUS)).unwrap();
        assert!(expanded.get("corpus").is_none());
        let tests = tests_of(&expanded);
        let ids = tests
            .iter()
            .map(|test| test["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["fixture/echo", "fixture/gxx", "fixture/slow"]);
        let echo = &tests[0];
        assert_eq!(echo["description"], "Fixture program `echo`");
        assert_eq!(
            echo["direct"],
            serde_yaml::from_str::<Value>("[/bin/echo, x]").unwrap()
        );
        let verify = &echo["modes"]["verify"];
        assert_eq!(verify["ci"], true);
        assert_eq!(
            verify["backends_enabled"],
            serde_yaml::from_str::<Value>("[ptrace]").unwrap()
        );
        for backend in ["dbt", "kvm", "sabre", "liteinst"] {
            assert!(
                verify["backends_disabled"][backend].as_str().is_some(),
                "{backend}"
            );
        }
        assert_eq!(verify["hermit_args"]["ptrace"][0], "--no-virtualize-cpuid");
        assert_eq!(verify["env"]["TMPDIR"], "/tmp");
        assert_eq!(verify["comparator"], "stripped");
        assert_eq!(verify["no_retry_reason"], "fixture single run");
        assert_eq!(verify["timeout_seconds"]["ptrace"], 60);
        assert_eq!(verify["cpu_timeout_seconds"]["ptrace"], 59);
        assert!(verify.get("diagnostic").is_none());
        for mode in NON_VERIFY_MODES {
            let table = &echo["modes"][mode];
            assert_eq!(table["ci"], false, "{mode}");
            assert!(table["ci_disabled_reason"].as_str().is_some(), "{mode}");
            assert_eq!(
                table["backends_enabled"],
                Value::Sequence(Vec::new()),
                "{mode}"
            );
        }
        // A diagnostic row carries its reason and its own shortened budget.
        let slow = &tests[2]["modes"]["verify"];
        assert_eq!(slow["diagnostic"]["ptrace"], "a bounded fixture probe");
        assert_eq!(slow["timeout_seconds"]["ptrace"], 20);
        assert_eq!(slow["cpu_timeout_seconds"]["ptrace"], 19);
        // A row without focused cells carries no labels.
        assert!(echo.get("labels").is_none());
        assert!(verify.get("labels").is_none());
        // A document without the section is unchanged.
        let plain: Value = serde_yaml::from_str("schema: 3\nbucket: plain\ntest: []").unwrap();
        assert_eq!(expand_corpus(plain.clone()).unwrap(), plain);
    }

    /// The corpus with a focused SaBRe run type, a heavy row, a row only that
    /// run type has, and two red cells.
    fn focused_corpus() -> String {
        CORPUS
            .replace(
                "  rows:\n    - {label: \"echo\"",
                r#"  heavy:
    timeout_seconds: 600
    cpu_timeout_seconds: 599
    slow_reason: heavy fixture budget
    rows: {big: a compile workload}
  focused:
    - label: sabre-compat-only
      backend: sabre
      except: {slow: not in the fixture SaBRe corpus}
  unselected:
    - backend: sabre
      result: crash-error
      evidence: https://github.com/rrnewton/hermit/issues/1
      reason: fixture execution-path failure
      rows: [g++]
    - backend: ptrace
      result: determinism-failure
      evidence: https://github.com/rrnewton/hermit/issues/2
      reason: fixture divergence
      rows: [big]
  rows:
    - {label: "echo""#,
            )
            .replace(
                "    - {label: \"slow\", argv: [\"/bin/true\"]}\n",
                "    - {label: \"slow\", argv: [\"/bin/true\"]}\n    - {label: \"big\", argv: [\"/bin/true\"], labels: [sabre-compat-only]}\n",
            )
    }

    #[test]
    fn a_focused_run_type_adds_labelled_cells_and_records_red_ones() {
        let expanded = expand_corpus(document(&focused_corpus())).unwrap();
        let tests = tests_of(&expanded);
        let by_id = |id: &str| {
            tests
                .iter()
                .find(|test| test["id"] == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        // echo: ptrace for the default run type plus a SaBRe cell labelled
        // with the focused run type, which inherits no ptrace hermit_args.
        let echo = &by_id("fixture/echo")["modes"]["verify"];
        assert_eq!(echo["backends_enabled"], seq("[ptrace, sabre]"));
        assert!(echo["backends_disabled"].get("sabre").is_none());
        assert!(
            echo["backends_disabled"]["dbt"]
                .as_str()
                .unwrap()
                .ends_with("and a focused run type's verify cell on sabre")
        );
        assert_eq!(echo["labels"], seq("{sabre: [sabre-compat-only]}"));
        assert_eq!(
            echo["hermit_args"],
            seq("{ptrace: [--no-virtualize-cpuid]}")
        );
        assert_eq!(echo["timeout_seconds"], seq("{ptrace: 60, sabre: 60}"));
        assert_eq!(echo["ci"], true);
        // slow: excepted, so SaBRe is not applicable, with the except reason;
        // its diagnostic budget stays on ptrace only.
        let slow = &by_id("fixture/slow")["modes"]["verify"];
        assert_eq!(slow["backends_enabled"], seq("[ptrace]"));
        assert_eq!(
            slow["backends_disabled"]["sabre"],
            "not in the fixture SaBRe corpus"
        );
        assert!(slow.get("labels").is_none());
        // g++: its SaBRe cell is enabled but red.
        let gxx = &by_id("fixture/gxx")["modes"]["verify"];
        assert_eq!(gxx["ci"], seq("{ptrace: true, sabre: false}"));
        assert_eq!(
            gxx["ci_disabled_reason"],
            seq(
                "{sabre: {result: crash-error, evidence: 'https://github.com/rrnewton/hermit/issues/1', reason: fixture execution-path failure}}"
            )
        );
        // big: the whole test belongs to the focused run type, its ptrace cell
        // has the heavy budget with its reason and is red.
        let big = by_id("fixture/big");
        assert_eq!(big["labels"], seq("[sabre-compat-only]"));
        let verify = &big["modes"]["verify"];
        assert_eq!(verify["timeout_seconds"], seq("{ptrace: 600, sabre: 60}"));
        assert_eq!(
            verify["cpu_timeout_seconds"],
            seq("{ptrace: 599, sabre: 59}")
        );
        assert_eq!(
            verify["slow_reason"]["ptrace"],
            "heavy fixture budget: a compile workload"
        );
        assert_eq!(verify["ci"], seq("{ptrace: false, sabre: true}"));
    }

    #[test]
    fn a_malformed_corpus_is_refused() {
        let refused = |from: &str, to: &str| {
            assert!(CORPUS.contains(from), "{from}");
            expand_corpus(document(&CORPUS.replace(from, to))).unwrap_err()
        };
        assert!(refused("label: \"slow\"", "label: \"echo\"").contains("is repeated"));
        assert!(refused("id: gxx", "id: echo").contains("repeats another row's test id"));
        assert!(refused("id: gxx", "id: g++").contains("repeats its label as its id"));
        assert!(refused("[\"/bin/true\"]", "[]").contains("has an empty argv"));
        assert!(refused("{slow: a bounded", "{absent: a bounded").contains("which is no row"));
        assert!(refused("backend: ptrace", "backend: e9patch").contains("is not one of"));
        let focused = focused_corpus();
        let refused_focused = |from: &str, to: &str| {
            assert!(focused.contains(from), "{from}");
            expand_corpus(document(&focused.replace(from, to))).unwrap_err()
        };
        assert!(
            refused_focused(
                "      backend: sabre\n      except",
                "      backend: ptrace\n      except"
            )
            .contains("other than `ptrace`")
        );
        assert!(refused_focused("{slow: not in", "{absent: not in").contains("which is no row"));
        assert!(refused_focused("rows: [g++]", "rows: [slow]").contains("which has no cell there"));
        assert!(refused_focused("rows: [g++]", "rows: [absent]").contains("which is no row"));
        assert!(refused_focused("rows: [big]", "rows: [big, big]").contains("twice"));
        assert!(refused_focused("rows: [g++]", "rows: []").contains("names no rows"));
        assert!(
            refused_focused("rows: {big: a compile", "rows: {slow: a compile")
                .contains("both diagnostic and heavy")
        );
        assert!(
            refused("  lane: portable", "  lane: portable\n  surprise: 1")
                .contains("invalid corpus section")
        );
    }

    #[test]
    fn placeholders_resolve_or_refuse() {
        let root = std::path::Path::new("/repo");
        let argv = [
            "{{ROOT_DIR}}/tool".to_string(),
            "{{VALIDATE_RUN_STATE}}/in".to_string(),
        ];
        assert_eq!(
            resolve_direct_placeholders("t", &argv, root, Some(std::ffi::OsStr::new("/state")))
                .unwrap(),
            ["/repo/tool", "/state/in"]
        );
        for missing in [None, Some(std::ffi::OsStr::new(""))] {
            assert!(
                resolve_direct_placeholders("t", &argv, root, missing)
                    .unwrap_err()
                    .contains("VALIDATE_RUN_STATE is not set")
            );
        }
        assert!(
            check_direct_placeholders("t", &["{{ROOT}}".to_string()])
                .unwrap_err()
                .contains("unknown placeholder")
        );
    }

    #[test]
    fn a_direct_shell_word_expands_its_placeholders_in_the_shell() {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
        let words = [
            "plain",
            "{{ROOT_DIR}}/a b",
            "x={{VALIDATE_RUN_STATE}}/{{ROOT_DIR}}",
            "",
        ]
        .map(|arg| direct_shell_word("t", arg, quote).unwrap());
        let script = format!("cd /tmp && printf '%s\\n' {}", words.join(" "));
        let run = |state: Option<&str>| {
            let mut command = std::process::Command::new("bash");
            command
                .args(["-c", &script])
                .env_remove("VALIDATE_RUN_STATE");
            if let Some(state) = state {
                command.env("VALIDATE_RUN_STATE", state);
            }
            command.output().unwrap()
        };
        let output = run(Some("/state"));
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "plain\n/tmp/a b\nx=/state//tmp\n\n"
        );
        let output = run(None);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("VALIDATE_RUN_STATE is not set"));
        assert!(direct_shell_word("t", "{{TYPO}}", quote).is_err());
    }
}
