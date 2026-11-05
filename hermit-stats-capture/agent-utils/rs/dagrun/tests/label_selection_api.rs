//! Public selection API coverage for library consumers.

use std::collections::{BTreeMap, BTreeSet};

use dagrun::{
    dag_config_carry_diff, dag_from_json, run_dag, select_steps_by_labels, select_steps_by_tags,
    DagConfig, RunResult, Step, WriteDomainPolicy,
};

fn graph() -> DagConfig {
    let mut cfg = dag_from_json(
        r#"{"steps":[
            {"group":"build","job":"shared","cmd":"true","write_domains":[]},
            {"group":"test","job":"portable","cmd":"true","labels":["portable","all"],"deps":["build.shared"],"write_domains":[]},
            {"group":"test","job":"privileged","cmd":"true","labels":["privileged","all"],"deps":["build.shared"],"write_domains":[]}
        ]}"#,
    )
    .unwrap();
    cfg.description = "non-default policy fixture".into();
    cfg.resource_caps = BTreeMap::from([("exclusive-host".into(), 2)]);
    cfg.mem_cap_factor = 1.5;
    cfg.mem_cap_floor_bytes = 4 * 1024i64.pow(3);
    cfg.outer_mem_safety_factor = 1.2;
    cfg.default_step_timeout = 600;
    cfg.default_jobs_flag = "--jobs {n}".into();
    cfg.default_jobs_env = "BUILD_JOBS".into();
    cfg.default_step_mem_cap_bytes = None;
    cfg.default_step_cpu_count = Some(4);
    cfg.default_step_cpu_timeout = 120;
    cfg.cpu_timeout_multiplier = 2.0;
    cfg.cpu_timeout_platform = "test-platform".into();
    cfg.write_domain_policy = WriteDomainPolicy {
        require_explicit: true,
        allowed_domains: BTreeSet::from(["shared-output".into()]),
    };
    cfg
}

fn tags(cfg: &DagConfig) -> Vec<String> {
    cfg.steps.iter().map(Step::tag).collect()
}

fn assert_only_steps_changed(source: &DagConfig, selected: &DagConfig) {
    let diff = dag_config_carry_diff(source, selected);
    assert_eq!(
        diff.len(),
        1,
        "selection changed top-level policy: {diff:?}"
    );
    assert!(diff[0].starts_with("steps: "), "unexpected diff: {diff:?}");
}

#[test]
fn library_consumers_select_exact_tags_with_dependency_ancestry_and_typed_result() {
    let cfg = graph();

    let selected = select_steps_by_tags(&cfg, &["test.portable".into()], false).unwrap();
    assert_eq!(tags(&selected), ["build.shared", "test.portable"]);
    assert_only_steps_changed(&cfg, &selected);

    let result: RunResult = run_dag(&selected, 2, false, 0);
    assert!(result.ok);
    assert_eq!(result.outcomes.len(), 2);
}

#[test]
fn library_consumers_can_ignore_unselected_dependencies_without_rewriting_the_source() {
    let cfg = graph();

    let leaf_only = select_steps_by_tags(&cfg, &["test.portable".into()], true).unwrap();
    assert_eq!(tags(&leaf_only), ["test.portable"]);
    assert!(leaf_only.steps[0].deps.is_empty());
    assert_only_steps_changed(&cfg, &leaf_only);

    let connected =
        select_steps_by_tags(&cfg, &["build.shared".into(), "test.portable".into()], true).unwrap();
    assert_eq!(tags(&connected), ["build.shared", "test.portable"]);
    assert_eq!(connected.steps[1].deps, ["build.shared"]);
    assert_only_steps_changed(&cfg, &connected);
}

#[test]
fn tag_selection_refuses_non_exact_or_unknown_ids_without_changing_the_source() {
    let cfg = graph();
    let authored = cfg.clone();

    let attempted_run: Result<RunResult, String> =
        select_steps_by_tags(&cfg, &[], false).map(|selected| run_dag(&selected, 2, false, 0));
    let error = attempted_run.unwrap_err();
    assert_eq!(error, "--selected requires at least one tag");
    let error = select_steps_by_tags(&cfg, &["portable".into()], false).unwrap_err();
    assert!(error.contains("unknown step tag(s): portable"), "{error}");
    assert!(error.contains("test.portable"), "{error}");
    assert_eq!(dag_config_carry_diff(&authored, &cfg), Vec::<String>::new());
}

#[test]
fn library_consumers_select_labels_with_dependency_ancestry() {
    let cfg = graph();

    let selected = select_steps_by_labels(&cfg, &["privileged".into()]).unwrap();
    assert_eq!(tags(&selected), ["build.shared", "test.privileged"]);
    assert_only_steps_changed(&cfg, &selected);
}

#[test]
fn label_selection_unions_matches_and_refuses_unknown_labels() {
    let cfg = graph();
    let authored = cfg.clone();

    let empty = select_steps_by_labels(&cfg, &[]).unwrap();
    assert!(empty.steps.is_empty());
    assert_only_steps_changed(&cfg, &empty);

    let selected = select_steps_by_labels(&cfg, &["portable".into(), "privileged".into()]).unwrap();
    assert_eq!(
        tags(&selected),
        ["build.shared", "test.portable", "test.privileged"]
    );

    let error = select_steps_by_labels(&cfg, &["test.portable".into()]).unwrap_err();
    assert!(error.contains("unknown label(s): test.portable"), "{error}");
    assert_eq!(dag_config_carry_diff(&authored, &cfg), Vec::<String>::new());
}
