//! `dagrun dot` / `dagrun ascii`: `--labels` selection and `--group-by group` collapse, pinned to
//! exact bytes on a three-group fixture. The default (no-flag) outputs are the bytes the build
//! emitted before these flags existed, so they pin that the default rendering did not move.

use std::path::PathBuf;
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/viz_groups.json")
}

fn dagrun(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .args(args)
        .arg("--dag")
        .arg(fixture())
        .output()
        .expect("spawn dagrun");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn ok(args: &[&str]) -> String {
    let (rc, out, err) = dagrun(args);
    assert_eq!(rc, 0, "dagrun {args:?} failed: {err}");
    assert_eq!(err, "", "dagrun {args:?} wrote to stderr");
    out
}

const DEFAULT_DOT: &str = r#"digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG  (solid = dependency;  dashed = shared cap-1 resource -> serialized)";
  subgraph cluster_0 {
    label="build"; style=dashed; color=gray70;
    "build.app" [label="build.app\n[light]"];
    "build.lib" [label="build.lib\n[light]"];
  }
  subgraph cluster_1 {
    label="e2e"; style=dashed; color=gray70;
    "e2e.smoke" [label="e2e.smoke\n[light]"];
  }
  subgraph cluster_2 {
    label="test"; style=dashed; color=gray70;
    "test.lint" [label="test.lint\n[light]"];
    "test.unit" [label="test.unit\n[light]"];
  }
  "build.app" -> "build.lib";
  "build.app" -> "test.unit";
  "build.lib" -> "test.unit";
  "build.app" -> "test.lint";
  "build.app" -> "e2e.smoke";
  "test.unit" -> "e2e.smoke";
}
"#;

const DEFAULT_ASCII: &str = "DAG - 5 steps, 6 edges, 4 layer(s)

layer 0:
  build.app  [light]
layer 1:
  build.lib  [light]  <- build.app
  test.lint  [light]  <- build.app
layer 2:
  test.unit  [light]  <- build.app, build.lib
layer 3:
  e2e.smoke  [light]  <- build.app, test.unit
";

#[test]
fn default_dot_and_ascii_are_unchanged() {
    assert_eq!(ok(&["dot"]), DEFAULT_DOT);
    assert_eq!(ok(&["ascii"]), DEFAULT_ASCII);
}

#[test]
fn group_dot_counts_steps_and_merged_cross_group_edges() {
    // build.app -> build.lib stays inside `build` and draws no edge; the three build -> test
    // step edges (app->unit, lib->unit, app->lint) merge into one edge labelled 3.
    let expected = r#"digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG by group  (node = group and its step count;  edge label = step-level dependencies merged)";
  "build" [label="build\n2 steps"];
  "e2e" [label="e2e\n1 step"];
  "test" [label="test\n2 steps"];
  "build" -> "e2e" [label="1"];
  "build" -> "test" [label="3"];
  "test" -> "e2e" [label="1"];
}
"#;
    assert_eq!(ok(&["dot", "--group-by", "group"]), expected);
    assert_eq!(ok(&["dot", "--group-by=group"]), expected);
}

#[test]
fn label_filter_keeps_labelled_steps_and_their_dependencies() {
    // `full` names build.app, test.unit, e2e.smoke; build.lib carries no label but test.unit
    // needs it, so it is kept (the `run --labels` selection). test.lint (`quick` only) is dropped.
    let expected = r#"digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG by group  (node = group and its step count;  edge label = step-level dependencies merged)";
  "build" [label="build\n2 steps"];
  "e2e" [label="e2e\n1 step"];
  "test" [label="test\n1 step"];
  "build" -> "e2e" [label="1"];
  "build" -> "test" [label="2"];
  "test" -> "e2e" [label="1"];
}
"#;
    assert_eq!(
        ok(&["dot", "--labels", "full", "--group-by", "group"]),
        expected
    );

    // Step level: `quick` keeps test.lint and test.unit plus the build steps they need; the e2e
    // cluster disappears entirely.
    let quick = ok(&["dot", "--labels=quick"]);
    assert_eq!(
        quick,
        r#"digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG  (solid = dependency;  dashed = shared cap-1 resource -> serialized)";
  subgraph cluster_0 {
    label="build"; style=dashed; color=gray70;
    "build.app" [label="build.app\n[light]"];
    "build.lib" [label="build.lib\n[light]"];
  }
  subgraph cluster_1 {
    label="test"; style=dashed; color=gray70;
    "test.lint" [label="test.lint\n[light]"];
    "test.unit" [label="test.unit\n[light]"];
  }
  "build.app" -> "build.lib";
  "build.app" -> "test.unit";
  "build.lib" -> "test.unit";
  "build.app" -> "test.lint";
}
"#
    );
    // A union of labels is the whole fixture again.
    assert_eq!(ok(&["dot", "--labels", "quick,full"]), DEFAULT_DOT);
}

#[test]
fn group_ascii_lists_counts_and_upstream_groups() {
    assert_eq!(
        ok(&["ascii", "--group-by", "group"]),
        "DAG by group - 3 groups, 5 steps, 3 group edges (5 cross-group step edges merged; 1 within groups)

  build  2 steps
  test   2 steps  <- build (3)
  e2e    1 step   <- build (1), test (1)
"
    );
    assert_eq!(
        ok(&["ascii", "--labels", "full", "--group-by", "group"]),
        "DAG by group - 3 groups, 4 steps, 3 group edges (4 cross-group step edges merged; 1 within groups)

  build  2 steps
  test   1 step   <- build (2)
  e2e    1 step   <- build (1), test (1)
"
    );
}

#[test]
fn bad_view_flags_are_refused_with_exit_2() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["dot", "--labels", "nope"],
            "dagrun: --labels: unknown label(s): nope. Known labels: full, quick\n",
        ),
        (
            &["ascii", "--labels", ","],
            "dagrun ascii: error: --labels requires at least one label\n",
        ),
        (
            &["dot", "--group-by", "job"],
            "dagrun dot: error: argument --group-by: invalid choice: 'job' (choose from group)\n",
        ),
        (
            &["dot", "--labels"],
            "dagrun dot: error: the argument --labels requires a value\n",
        ),
        (
            &["dot", "--labels", "-x", "--group-by", "group"],
            "dagrun dot: error: the argument --labels requires a value\n",
        ),
        (
            &["list", "--group-by", "group"],
            "dagrun list: error: unrecognized argument: --group-by\n",
        ),
    ];
    for (args, stderr) in cases {
        let (rc, out, err) = dagrun(args);
        assert_eq!(rc, 2, "dagrun {args:?}");
        assert_eq!(out, "", "dagrun {args:?}");
        assert_eq!(&err, stderr, "dagrun {args:?}");
    }
}
