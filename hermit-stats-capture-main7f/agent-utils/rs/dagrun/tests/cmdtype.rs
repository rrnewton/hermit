//! Known cmdtype command-line shapes and DAGRUN_EXTRA_ARGS delivery.

use dagrun::{cmdtype_extra_args, dag_from_json, dag_to_json, run_dag, CmdType, DagConfig};
use serde_json::json;

fn config(cmdtype: &str, cmd: &str, jobs_flag: Option<&str>) -> DagConfig {
    let mut step = json!({
        "group": "g",
        "job": "j",
        "cmd": cmd,
        "cmdtype": cmdtype,
        "hint": {"preferred_inner_jobs": 3}
    });
    if let Some(flag) = jobs_flag {
        step["jobs_flag"] = json!(flag);
    }
    dag_from_json(&json!({"steps": [step]}).to_string()).unwrap()
}

#[test]
fn every_cmdtype_has_known_arguments() {
    let cases = [
        ("unknown", None, None),
        ("make", None, Some("-j3")),
        ("cargo-build", None, Some("--jobs 3")),
        ("cargo-test", None, Some("--jobs 3")),
        ("cargo-nextest", None, Some("--test-threads 3")),
        ("generic-dash-j-command", None, Some("-j3")),
        ("generic-with-flag", Some("--workers"), Some("--workers 3")),
    ];
    for (cmdtype, jobs_flag, expected) in cases {
        let cfg = config(cmdtype, "true", jobs_flag);
        assert_eq!(
            cmdtype_extra_args(&cfg.steps[0], Some(3)).as_deref(),
            expected,
            "{cmdtype}"
        );
    }
}

#[test]
fn known_simple_command_gets_arguments_appended() {
    let cfg = config(
        "cargo-build",
        r#"sh -c 'test "$#" -eq 2 && test "$1" = --jobs && test "$2" = 3' capture"#,
        None,
    );
    assert!(run_dag(&cfg, 3, false, 0).ok);
}

#[test]
fn compound_command_expands_extra_args_once() {
    let cfg = config(
        "cargo-build",
        r#"capture() { [ "$#" -eq 2 ] && [ "$1" = --jobs ] && [ "$2" = 3 ]; }; final() { [ "$#" -eq 0 ]; }; capture $DAGRUN_EXTRA_ARGS && final"#,
        None,
    );
    let mut cfg = cfg;
    cfg.steps[0]
        .env
        .insert("DAGRUN_EXTRA_ARGS".into(), "poison".into());
    assert!(run_dag(&cfg, 3, false, 0).ok);
}

#[test]
fn unknown_neither_appends_cmdtype_arguments_nor_sets_the_variable() {
    let cfg = config("unknown", r#"test -z "${DAGRUN_EXTRA_ARGS+x}""#, Some(""));
    let mut cfg = cfg;
    cfg.steps[0]
        .env
        .insert("DAGRUN_EXTRA_ARGS".into(), "poison".into());
    assert!(run_dag(&cfg, 3, false, 0).ok);
}

#[test]
fn multi_word_extra_args_must_not_be_quoted() {
    let error = dag_from_json(
        &json!({
            "steps": [{
                "group": "g",
                "job": "j",
                "cmd": "cargo build \"$DAGRUN_EXTRA_ARGS\"",
                "cmdtype": "cargo-build",
                "hint": {"preferred_inner_jobs": 3}
            }]
        })
        .to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("must be unquoted"), "{error}");
    assert!(error.contains("multiple shell words"), "{error}");
}

#[test]
fn compound_command_without_extra_args_is_refused() {
    let error = dag_from_json(
        &json!({"steps": [{
            "group": "g", "job": "j", "cmd": "prepare && cargo build",
            "cmdtype": "cargo-build", "hint": {"preferred_inner_jobs": 3}
        }]})
        .to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("compound cmd"), "{error}");
    assert!(error.contains("must place unquoted"), "{error}");
}

#[test]
fn cmdtype_and_jobs_flag_combinations_are_unambiguous() {
    let missing = dag_from_json(
        &json!({"steps": [{
            "group": "g", "job": "j", "cmd": "true", "cmdtype": "generic-with-flag"
        }]})
        .to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        missing.contains("requires a non-empty jobs_flag"),
        "{missing}"
    );

    let conflict = dag_from_json(
        &json!({"steps": [{
            "group": "g", "job": "j", "cmd": "true", "cmdtype": "cargo-build",
            "jobs_flag": "--workers"
        }]})
        .to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        conflict.contains("jobs_flag is valid with cmdtype generic-with-flag"),
        "{conflict}"
    );
}

#[test]
fn cmdtype_round_trips_and_unknown_values_are_refused() {
    let cfg = config("make", "make", None);
    assert_eq!(cfg.steps[0].cmdtype, CmdType::Make);
    let encoded = dag_to_json(&cfg);
    assert_eq!(dag_to_json(&dag_from_json(&encoded).unwrap()), encoded);

    let error = dag_from_json(
        &json!({"steps": [{"group": "g", "job": "j", "cmd": "true", "cmdtype": "cargo"}]})
            .to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("valid values: unknown, make, cargo-build"),
        "{error}"
    );
}
