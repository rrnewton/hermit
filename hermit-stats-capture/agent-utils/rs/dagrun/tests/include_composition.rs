//! Path-aware DAG composition is strict, confined, and namespace-safe.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use dagrun::{
    dag_from_json, dag_from_path, dag_from_yaml, dag_to_json, load_report_dag,
    MAX_DAG_FLATTENED_STEPS, MAX_DAG_INCLUDE_DEPTH, MAX_DAG_INCLUDE_INSTANCES,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "dagrun-include-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn context_free_parsers_reserve_include_instead_of_ignoring_it() {
    for error in [
        dag_from_json(r#"{"include":[],"steps":[]}"#).unwrap_err(),
        dag_from_yaml("include: []\nsteps: []\n").unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("includes require filesystem context"),
            "{error}"
        );
    }
    let error = dag_from_json(r#"{"include":{},"steps":[]}"#).unwrap_err();
    assert!(error.to_string().contains("include: must be a list"));
    let error = dag_from_json(r#"{"include":[1],"steps":[]}"#).unwrap_err();
    assert!(
        error.to_string().contains("expected an object, got number"),
        "{error}"
    );
    let error = dag_from_json(
        r#"{"include":[{"path":"child.json","namespace":"child","future":1}],"steps":[]}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field(s) 'future'"));
    let error = dag_from_json(
        r#"{"include":[{"path":"child.json","namespace":"child","after":"g.j"}],"steps":[]}"#,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("after: must be a list of strings"));
}

#[test]
fn nested_json_yaml_includes_flatten_and_rewrite_identities() {
    let dir = TestDir::new();
    dir.write(
        "leaf.json",
        r#"{
          "resource_caps":{"browser":1},
          "steps":[{
            "group":"build","job":"app","cmd":"true","deps":null,"labels":["fast"],
            "fail_fast_family":"compile",
            "result_manifests":[{
              "kind":"structured-test-results","schema":2,
              "path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"build.app"
            }]
          }]
        }"#,
    );
    dir.write(
        "middle.yaml",
        r#"include:
  - path: leaf.json
    namespace: leaf
    after: [prep.ready]
description: child description is not promoted
resource_caps:
  gpu: 2
steps:
  - group: test
    job: unit
    cmd: "true"
    deps: [leaf.build.app]
  - group: diagnose
    job: unit
    cmd: "true"
    explains: [test.unit]
  - group: prep
    job: ready
    cmd: "true"
"#,
    );
    let root = dir.write(
        "root.json",
        r#"{
          "description":"root description",
          "include":[{
            "path":"middle.yaml","namespace":"component","after":["setup.ready"]
          }],
          "resource_caps":{"browser":1},
          "steps":[
            {"group":"setup","job":"ready","cmd":"true"},
            {"group":"publish","job":"all","cmd":"true","deps":["component.test.unit"]}
          ]
        }"#,
    );

    let cfg = dag_from_path(&root).unwrap();
    let tags: Vec<String> = cfg.steps.iter().map(|step| step.tag()).collect();
    assert_eq!(
        tags,
        [
            "component.leaf.build.app",
            "component.test.unit",
            "component.diagnose.unit",
            "component.prep.ready",
            "setup.ready",
            "publish.all",
        ]
    );
    assert_eq!(cfg.steps[0].deps, ["component.prep.ready"]);
    assert_eq!(cfg.steps[1].deps, ["component.leaf.build.app"]);
    assert_eq!(cfg.steps[2].deps, ["setup.ready"]);
    assert_eq!(cfg.steps[2].explains, ["component.test.unit"]);
    assert_eq!(cfg.steps[3].deps, ["setup.ready"]);
    // An outer dependency may target one exact internal node. Includes do not synthesize a
    // completion node or turn that edge into an implicit wait for every fragment terminal.
    assert_eq!(cfg.steps[5].deps, ["component.test.unit"]);
    assert_eq!(
        cfg.steps[0].fail_fast_family.as_deref(),
        Some("component.leaf.compile")
    );
    assert_eq!(cfg.steps[0].labels, ["fast"]);
    assert_eq!(
        cfg.steps[0]
            .structured_test_results_manifest()
            .unwrap()
            .unwrap()
            .owner,
        "component.leaf.build.app"
    );
    assert_eq!(cfg.resource_caps["browser"], 1);
    assert_eq!(cfg.resource_caps["gpu"], 2);
    assert_eq!(cfg.description, "root description");
    assert!(!dag_to_json(&cfg).contains("\"include\""));
    assert_eq!(load_report_dag(&root).unwrap().steps.len(), cfg.steps.len());
}

#[test]
fn include_after_wires_every_upstream_to_every_internal_root_only() {
    let dir = TestDir::new();
    dir.write(
        "child.json",
        r#"{
          "steps":[
            {"group":"g","job":"first","cmd":"true"},
            {"group":"g","job":"second","cmd":"true","deps":null},
            {"group":"g","job":"descendant","cmd":"true","deps":["g.first"]}
          ]
        }"#,
    );
    let root = dir.write(
        "root.json",
        r#"{
          "include":[{
            "path":"child.json","namespace":"child",
            "after":["setup.one","setup.two"]
          }],
          "steps":[
            {"group":"setup","job":"one","cmd":"true"},
            {"group":"setup","job":"two","cmd":"true"},
            {"group":"publish","job":"exact","cmd":"true","deps":["child.g.first"]}
          ]
        }"#,
    );

    let config = dag_from_path(root).unwrap();
    let by_tag: std::collections::BTreeMap<String, &dagrun::Step> =
        config.steps.iter().map(|step| (step.tag(), step)).collect();
    assert_eq!(
        by_tag.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "child.g.descendant",
            "child.g.first",
            "child.g.second",
            "publish.exact",
            "setup.one",
            "setup.two",
        ]
    );
    assert_eq!(by_tag["child.g.first"].deps, ["setup.one", "setup.two"]);
    assert_eq!(by_tag["child.g.second"].deps, ["setup.one", "setup.two"]);
    assert_eq!(by_tag["child.g.descendant"].deps, ["child.g.first"]);
    assert_eq!(by_tag["publish.exact"].deps, ["child.g.first"]);
}

#[test]
fn sibling_after_reference_is_independent_of_include_order() {
    let dir = TestDir::new();
    dir.write(
        "a.json",
        r#"{"steps":[{"group":"g","job":"ready","cmd":"true"}]}"#,
    );
    dir.write(
        "b.json",
        r#"{"steps":[{"group":"g","job":"start","cmd":"true"}]}"#,
    );

    for (name, includes) in [
        (
            "forward.json",
            r#"[
              {"path":"b.json","namespace":"b","after":["a.g.ready"]},
              {"path":"a.json","namespace":"a"}
            ]"#,
        ),
        (
            "reverse.json",
            r#"[
              {"path":"a.json","namespace":"a"},
              {"path":"b.json","namespace":"b","after":["a.g.ready"]}
            ]"#,
        ),
    ] {
        let root = dir.write(name, &format!(r#"{{"include":{includes},"steps":[]}}"#));
        let config = dag_from_path(root).unwrap();
        let by_tag: std::collections::BTreeMap<String, &dagrun::Step> =
            config.steps.iter().map(|step| (step.tag(), step)).collect();
        assert_eq!(by_tag["b.g.start"].deps, ["a.g.ready"]);
    }
}

#[test]
fn empty_include_with_valid_after_is_a_noop() {
    let dir = TestDir::new();
    dir.write("empty.json", r#"{"steps":[]}"#);
    let root = dir.write(
        "root.json",
        r#"{
          "include":[{"path":"empty.json","namespace":"empty","after":["setup.ready"]}],
          "steps":[{"group":"setup","job":"ready","cmd":"true"}]
        }"#,
    );

    let tags: Vec<String> = dag_from_path(root)
        .unwrap()
        .steps
        .iter()
        .map(|step| step.tag())
        .collect();
    assert_eq!(tags, ["setup.ready"]);
}

#[test]
fn outer_dependency_typo_for_included_step_is_an_ordinary_missing_tag() {
    let dir = TestDir::new();
    dir.write(
        "child.json",
        r#"{"steps":[{"group":"g","job":"real","cmd":"true"}]}"#,
    );
    let root = dir.write(
        "root.json",
        r#"{
          "include":[{"path":"child.json","namespace":"child"}],
          "steps":[{
            "group":"outer","job":"consumer","cmd":"true","deps":["child.g.typo"]
          }]
        }"#,
    );

    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(
        error.contains("depends on 'child.g.typo', which no step declares"),
        "{error}"
    );
}

#[test]
fn a_file_can_be_reused_only_under_different_namespaces() {
    let dir = TestDir::new();
    dir.write(
        "child.json",
        r#"{"steps":[{"group":"g","job":"j","cmd":"true"}]}"#,
    );
    let good = dir.write(
        "good.json",
        r#"{"include":[
          {"path":"child.json","namespace":"one"},
          {"path":"child.json","namespace":"two"}
        ],"steps":[]}"#,
    );
    let tags: Vec<String> = dag_from_path(good)
        .unwrap()
        .steps
        .iter()
        .map(|step| step.tag())
        .collect();
    assert_eq!(tags, ["one.g.j", "two.g.j"]);

    let bad = dir.write(
        "bad.json",
        r#"{"include":[
          {"path":"child.json","namespace":"same"},
          {"path":"child.json","namespace":"same"}
        ],"steps":[]}"#,
    );
    let error = dag_from_path(bad).unwrap_err().to_string();
    assert!(error.contains("duplicate include"), "{error}");
}

#[test]
fn cycles_and_conflicts_are_refused() {
    let dir = TestDir::new();
    let a = dir.write(
        "a.json",
        r#"{"include":[{"path":"b.json","namespace":"b"}],"steps":[]}"#,
    );
    dir.write(
        "b.json",
        r#"{"include":[{"path":"a.json","namespace":"a"}],"steps":[]}"#,
    );
    let error = dag_from_path(&a).unwrap_err().to_string();
    assert!(error.contains("include cycle:"), "{error}");
    assert!(error.matches("a.json").count() >= 2, "{error}");

    dir.write(
        "child.json",
        r#"{"resource_caps":{"browser":2},"default_step_timeout":9,"steps":[]}"#,
    );
    let root = dir.write(
        "root.json",
        r#"{
          "include":[{"path":"child.json","namespace":"x"}],
          "resource_caps":{"browser":1},"default_step_timeout":10,"steps":[]
        }"#,
    );
    let error = dag_from_path(&root).unwrap_err().to_string();
    assert!(
        error.contains("global policy 'default_step_timeout' conflicts"),
        "{error}"
    );

    dir.write(
        "child.json",
        r#"{"resource_caps":{"browser":2},"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(
        error.contains("resource cap 'browser'=2 conflicts"),
        "{error}"
    );
}

#[test]
fn invalid_namespaces_and_unsafe_missing_or_nonfile_paths_are_refused() {
    let dir = TestDir::new();
    for (index, included) in [
        "/tmp/outside.json",
        "https://example.test/dag.json",
        "~/dag.json",
        "$DAG/dag.json",
        "fragments/*.json",
        "missing.json",
        ".",
    ]
    .iter()
    .enumerate()
    {
        let root = dir.write(
            &format!("path-{index}.json"),
            &format!(r#"{{"include":[{{"path":"{included}","namespace":"x"}}],"steps":[]}}"#),
        );
        let error = dag_from_path(root).unwrap_err().to_string();
        assert!(error.contains("include path"), "{error}");
    }

    let root = dir.write(
        "namespace.json",
        r#"{"include":[{"path":"missing.json","namespace":"bad.name"}],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("namespace: must match"), "{error}");
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_refused_after_canonicalization() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new();
    symlink("loop.json", dir.path("loop.json")).unwrap();
    let root = dir.write(
        "loop-root.json",
        r#"{"include":[{"path":"loop.json","namespace":"x"}],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("cannot resolve include path"), "{error}");

    let outside = dir.0.parent().unwrap().join(format!(
        "{}-outside.json",
        dir.0.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&outside, r#"{"steps":[]}"#).unwrap();
    symlink(&outside, dir.path("escape.json")).unwrap();
    let root = dir.write(
        "root.json",
        r#"{"include":[{"path":"escape.json","namespace":"x"}],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("resolves outside"), "{error}");
    fs::remove_file(outside).unwrap();

    dir.write(
        "child.json",
        r#"{"steps":[{"group":"g","job":"j","cmd":"true"}]}"#,
    );
    symlink(dir.path("child.json"), dir.path("child-alias.json")).unwrap();
    let root = dir.write(
        "alias-root.json",
        r#"{"include":[
          {"path":"child.json","namespace":"same"},
          {"path":"child-alias.json","namespace":"same"}
        ],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("duplicate include"), "{error}");

    dir.write("leaf.data", "steps: [{group: g, job: j, cmd: echo yaml}]\n");
    symlink(dir.path("leaf.data"), dir.path("leaf.yaml")).unwrap();
    dir.write(
        "root.data",
        "include: [{path: leaf.yaml, namespace: child}]\nsteps: []\n",
    );
    symlink(dir.path("root.data"), dir.path("root.yaml")).unwrap();
    let tags: Vec<String> = dag_from_path(dir.path("root.yaml"))
        .unwrap()
        .steps
        .iter()
        .map(|step| step.tag())
        .collect();
    assert_eq!(tags, ["child.g.j"]);
}

#[test]
fn cross_fragment_tag_collision_names_both_sources() {
    let dir = TestDir::new();
    let step = r#"{"steps":[{"group":"g","job":"j","cmd":"true"}]}"#;
    dir.write("a.json", step);
    dir.write("b.json", step);
    let root = dir.write(
        "root.json",
        r#"{"include":[
          {"path":"a.json","namespace":"same"},
          {"path":"b.json","namespace":"same"}
        ],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("duplicate step tag 'same.g.j'"), "{error}");
    assert!(error.contains("a.json steps[0]"), "{error}");
    assert!(error.contains("b.json steps[0]"), "{error}");
}

#[test]
fn include_after_unknown_outer_tag_is_refused_by_flattened_graph() {
    let dir = TestDir::new();
    dir.write(
        "child.json",
        r#"{"steps":[{"group":"g","job":"j","cmd":"true"}]}"#,
    );
    let root = dir.write(
        "root.json",
        r#"{"include":[{
          "path":"child.json","namespace":"child","after":["missing.step"]
        }],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(
        error.contains("after references missing outer step 'missing.step'"),
        "{error}"
    );

    dir.write("child.json", r#"{"steps":[]}"#);
    let error = dag_from_path(dir.path("root.json"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("after references missing outer step 'missing.step'"),
        "{error}"
    );
}

#[test]
fn include_after_must_reference_an_outer_step() {
    let dir = TestDir::new();
    dir.write(
        "child.json",
        r#"{"steps":[{"group":"g","job":"j","cmd":"true"}]}"#,
    );
    let root = dir.write(
        "root.json",
        r#"{"include":[{
          "path":"child.json","namespace":"child","after":["child.g.j"]
        }],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("after must name outer steps"), "{error}");
}

#[test]
fn hostile_include_path_and_bytes_fail_as_dag_errors() {
    let dir = TestDir::new();
    let root = dir.write(
        "root.json",
        "{\"include\":[{\"path\":\"nul\\u0000.json\",\"namespace\":\"x\"}],\"steps\":[]}",
    );
    let error = dag_from_path(&root).unwrap_err().to_string();
    assert!(error.contains("cannot resolve include path"), "{error}");

    fs::write(dir.path("invalid.json"), [0xff]).unwrap();
    dir.write(
        "root.json",
        r#"{"include":[{"path":"invalid.json","namespace":"x"}],"steps":[]}"#,
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(error.contains("cannot read DAG"), "{error}");
}

#[test]
fn include_depth_limit_accepts_boundary_and_refuses_next() {
    let dir = TestDir::new();
    for index in 0..=MAX_DAG_INCLUDE_DEPTH {
        let document = if index < MAX_DAG_INCLUDE_DEPTH {
            format!(
                "{{\"include\":[{{\"path\":\"depth-{}.json\",\"namespace\":\"n{}\"}}],\"steps\":[]}}",
                index + 1,
                index + 1
            )
        } else {
            r#"{"steps":[]}"#.to_string()
        };
        dir.write(&format!("depth-{index}.json"), &document);
    }
    dag_from_path(dir.path("depth-0.json")).unwrap();

    dir.write(
        &format!("depth-{}.json", MAX_DAG_INCLUDE_DEPTH + 1),
        r#"{"steps":[]}"#,
    );
    dir.write(
        &format!("depth-{MAX_DAG_INCLUDE_DEPTH}.json"),
        &format!(
            "{{\"include\":[{{\"path\":\"depth-{}.json\",\"namespace\":\"overflow\"}}],\"steps\":[]}}",
            MAX_DAG_INCLUDE_DEPTH + 1
        ),
    );
    let error = dag_from_path(dir.path("depth-0.json"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!("maximum depth {MAX_DAG_INCLUDE_DEPTH}")),
        "{error}"
    );
}

#[test]
fn include_instance_limit_accepts_boundary_and_refuses_next() {
    let dir = TestDir::new();
    dir.write("empty.json", r#"{"steps":[]}"#);
    let mut includes: Vec<String> = (0..MAX_DAG_INCLUDE_INSTANCES)
        .map(|index| format!(r#"{{"path":"empty.json","namespace":"n{index}"}}"#))
        .collect();
    let root = dir.write(
        "root.json",
        &format!(r#"{{"include":[{}],"steps":[]}}"#, includes.join(",")),
    );
    dag_from_path(&root).unwrap();

    includes.push(r#"{"path":"empty.json","namespace":"overflow"}"#.to_string());
    dir.write(
        "root.json",
        &format!(r#"{{"include":[{}],"steps":[]}}"#, includes.join(",")),
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(
        error.contains(&format!("{MAX_DAG_INCLUDE_INSTANCES} include instances")),
        "{error}"
    );
}

#[test]
fn flattened_step_limit_accepts_boundary_and_refuses_next() {
    let dir = TestDir::new();
    let boundary_steps = std::iter::repeat_n("null", MAX_DAG_FLATTENED_STEPS)
        .collect::<Vec<_>>()
        .join(",");
    let root = dir.write("root.json", &format!(r#"{{"steps":[{boundary_steps}]}}"#));
    let error = dag_from_path(&root).unwrap_err().to_string();
    assert!(error.contains("steps[0]: expected an object"), "{error}");
    assert!(!error.contains("flattened steps"), "{error}");

    dir.write(
        "root.json",
        &format!(r#"{{"steps":[{boundary_steps},null]}}"#),
    );
    let error = dag_from_path(root).unwrap_err().to_string();
    assert!(
        error.contains(&format!("{MAX_DAG_FLATTENED_STEPS} flattened steps")),
        "{error}"
    );
}
