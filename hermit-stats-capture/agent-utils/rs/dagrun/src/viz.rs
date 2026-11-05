//! Human-readable DAG visualizations in Graphviz DOT and ASCII forms.

// Human-readable DAG visualizations: Graphviz DOT and quick ASCII art.
//
// Pure functions over a [`DagConfig`] — no I/O. Direct port of
// `py/dagrun/viz.py`; the output is BYTE-IDENTICAL to the Python build (same
// headers, cluster order, edge lines, ASCII layers) and cross-tested against it.
//
// * [`to_dot`] emits Graphviz: one cluster per group, solid dependency edges, and a dashed
//   edge chain for each cap-1 scarce resource.
// * [`to_ascii`] emits a compact topological-layer view for a glance in the terminal.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use crate::model::{step_classification, DagConfig, Step};

// Selected steps (all, since the CLI never passes a subset). Kept as a helper to mirror the
// Python structure and leave room for a future `--select` flag.
fn selected_steps<'a>(cfg: &'a DagConfig, selected: Option<&HashSet<String>>) -> Vec<&'a Step> {
    cfg.steps
        .iter()
        .filter(|s| selected.is_none_or(|sel| sel.contains(&s.tag())))
        .collect()
}

/// Each step's deps, filtered to steps actually present in the selection (order preserved).
fn kept_deps(steps: &[&Step]) -> HashMap<String, Vec<String>> {
    let tags: HashSet<String> = steps.iter().map(|s| s.tag()).collect();
    steps
        .iter()
        .map(|s| {
            let kept: Vec<String> = s
                .deps
                .iter()
                .filter(|d| tags.contains(*d))
                .cloned()
                .collect();
            (s.tag(), kept)
        })
        .collect()
}

// Concise per-node profiling annotation for a DOT label: `\n{est}s, {mb}MB`
// (expected wall-seconds and max resident memory in decimal MB, floored).
//
// Returns `""` when the step carries neither a duration nor an RSS estimate, so
// undecorated DAGs render exactly as before. Formatting is integer-floored for MB
// and fixed one-decimal for seconds so the Rust and Python builds stay byte-identical.
fn profile_suffix(step: &Step) -> String {
    let est = step.hint.est_duration_s;
    let rss = step.hint.rss_baseline_bytes;
    if est <= 0.0 && rss.is_none() {
        return String::new();
    }
    let mb = rss.unwrap_or(0) / 1_000_000;
    format!("\\n{est:.1}s, {mb}MB")
}

/// Longest weighted finish time over the DAG (critical path in expected seconds),
/// weighting each node by its `est_duration_s`. Memoized; visits in `steps` order.
fn critical_path_seconds(steps: &[&Step], deps: &HashMap<String, Vec<String>>) -> f64 {
    fn visit(
        tag: &str,
        est_of: &HashMap<String, f64>,
        deps: &HashMap<String, Vec<String>>,
        finish: &mut HashMap<String, f64>,
    ) -> f64 {
        if let Some(f) = finish.get(tag) {
            return *f;
        }
        let parents = deps.get(tag).cloned().unwrap_or_default();
        let base = parents
            .iter()
            .map(|p| visit(p, est_of, deps, finish))
            .fold(0.0_f64, f64::max);
        let f = base + est_of.get(tag).copied().unwrap_or(0.0);
        finish.insert(tag.to_string(), f);
        f
    }
    let est_of: HashMap<String, f64> = steps
        .iter()
        .map(|s| (s.tag(), s.hint.est_duration_s))
        .collect();
    let mut finish: HashMap<String, f64> = HashMap::new();
    steps
        .iter()
        .map(|s| visit(&s.tag(), &est_of, deps, &mut finish))
        .fold(0.0_f64, f64::max)
}

/// Graph-title scaling annotation: `  |  {N.N}X max par-spdup`, the ideal parallel
/// speedup (total serial work / critical path). Omitted when no step has a duration
/// estimate (critical path is zero), so undecorated DAGs render exactly as before.
fn scaling_suffix(steps: &[&Step], deps: &HashMap<String, Vec<String>>) -> String {
    let serial: f64 = steps.iter().map(|s| s.hint.est_duration_s).sum();
    let crit = critical_path_seconds(steps, deps);
    if crit <= 0.0 {
        return String::new();
    }
    format!("  |  {:.1}X max par-spdup", serial / crit)
}

/// Render the DAG as Graphviz DOT.
pub fn to_dot(cfg: &DagConfig, name: &str, selected: Option<&HashSet<String>>) -> String {
    let steps = selected_steps(cfg, selected);
    let deps = kept_deps(&steps);

    // Groups in sorted order; steps within a group sorted by job (mirrors Python).
    let mut by_group: BTreeMap<String, Vec<&Step>> = BTreeMap::new();
    for step in &steps {
        by_group.entry(step.group.clone()).or_default().push(step);
    }

    let scaling = scaling_suffix(&steps, &deps);
    let mut out: Vec<String> = vec![
        format!("digraph {name} {{"),
        "  rankdir=LR;".to_string(),
        "  node [shape=box, style=rounded, fontsize=10];".to_string(),
        "  labelloc=\"t\";".to_string(),
        format!(
            "  label=\"DAG  (solid = dependency;  dashed = shared cap-1 resource -> serialized){scaling}\";"
        ),
    ];

    for (i, (group, group_steps)) in by_group.iter().enumerate() {
        out.push(format!("  subgraph cluster_{i} {{"));
        out.push(format!(
            "    label=\"{group}\"; style=dashed; color=gray70;"
        ));
        let mut sorted_steps: Vec<&&Step> = group_steps.iter().collect();
        sorted_steps.sort_by(|a, b| a.job.cmp(&b.job));
        for step in sorted_steps {
            let tag = step.tag();
            out.push(format!(
                "    \"{tag}\" [label=\"{tag}\\n[{}]{}\"];",
                step_classification(step).value(),
                profile_suffix(step)
            ));
        }
        out.push("  }".to_string());
    }

    for step in &steps {
        let tag = step.tag();
        for dep in &deps[&tag] {
            out.push(format!("  \"{dep}\" -> \"{tag}\";"));
        }
    }

    // A dashed chain across the users of each cap-1 resource: a hint that they serialize.
    for (res, cap) in &cfg.resource_caps {
        if *cap == 1 {
            let mut users: Vec<String> = steps
                .iter()
                .filter(|s| s.hint.resources.get(res).copied().unwrap_or(0) > 0)
                .map(|s| s.tag())
                .collect();
            users.sort();
            for pair in users.windows(2) {
                out.push(format!(
                    "  \"{}\" -> \"{}\" [style=dashed, color=gray60, constraint=false, label=\"{res}\"];",
                    pair[0], pair[1]
                ));
            }
        }
    }
    out.push("}".to_string());
    out.join("\n") + "\n"
}

/// The one `--group-by` key the renderers understand: a step's `group` field.
pub const GROUP_BY_KEYS: &[&str] = &["group"];

// Escape a name for a double-quoted DOT ID or label (the group-level renderer only; the
// step-level renderer's output is pinned byte-for-byte and is left exactly as it was).
fn dot_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn step_count_text(n: usize) -> String {
    if n == 1 {
        "1 step".to_string()
    } else {
        format!("{n} steps")
    }
}

// The step graph collapsed onto its groups: step count per group, and the number of step-level
// dependency edges behind each ordered (dependency group -> dependent group) pair. Edges inside
// one group are counted separately and never become a group edge.
struct GroupGraph {
    counts: BTreeMap<String, usize>,
    edges: BTreeMap<(String, String), usize>,
    cross_edges: usize,
    intra_edges: usize,
}

fn group_graph(steps: &[&Step], deps: &HashMap<String, Vec<String>>) -> GroupGraph {
    let group_of: HashMap<String, &str> =
        steps.iter().map(|s| (s.tag(), s.group.as_str())).collect();
    let mut g = GroupGraph {
        counts: BTreeMap::new(),
        edges: BTreeMap::new(),
        cross_edges: 0,
        intra_edges: 0,
    };
    for step in steps {
        *g.counts.entry(step.group.clone()).or_default() += 1;
        for dep in &deps[&step.tag()] {
            let from = group_of[dep];
            if from == step.group {
                g.intra_edges += 1;
            } else {
                g.cross_edges += 1;
                *g.edges
                    .entry((from.to_string(), step.group.clone()))
                    .or_default() += 1;
            }
        }
    }
    g
}

/// Render the DAG collapsed to one node per `group`, as Graphviz DOT.
///
/// Each node is labelled with its group name and step count. Each edge is one ordered
/// (dependency group -> dependent group) pair, labelled with the number of step-level dependency
/// edges it merges; dependencies inside one group draw no edge.
pub fn to_dot_groups(cfg: &DagConfig, name: &str, selected: Option<&HashSet<String>>) -> String {
    let steps = selected_steps(cfg, selected);
    let deps = kept_deps(&steps);
    let g = group_graph(&steps, &deps);
    let scaling = scaling_suffix(&steps, &deps);
    let mut out: Vec<String> = vec![
        format!("digraph {name} {{"),
        "  rankdir=LR;".to_string(),
        "  node [shape=box, style=rounded, fontsize=10];".to_string(),
        "  labelloc=\"t\";".to_string(),
        format!(
            "  label=\"DAG by group  (node = group and its step count;  edge label = step-level dependencies merged){scaling}\";"
        ),
    ];
    for (group, n) in &g.counts {
        let q = dot_quote(group);
        out.push(format!(
            "  \"{q}\" [label=\"{q}\\n{}\"];",
            step_count_text(*n)
        ));
    }
    for ((from, to), n) in &g.edges {
        out.push(format!(
            "  \"{}\" -> \"{}\" [label=\"{n}\"];",
            dot_quote(from),
            dot_quote(to)
        ));
    }
    out.push("}".to_string());
    out.join("\n") + "\n"
}

/// Render the DAG collapsed to one line per `group`, as compact ASCII.
///
/// Groups are ordered by the earliest dependency layer any of their steps occupies, then by
/// name. Each line shows the step count and the upstream groups with their merged edge counts.
pub fn to_ascii_groups(cfg: &DagConfig, selected: Option<&HashSet<String>>) -> String {
    let steps = selected_steps(cfg, selected);
    let deps = kept_deps(&steps);
    let depth = layers_of(&steps, &deps);
    let g = group_graph(&steps, &deps);

    let mut first_layer: BTreeMap<String, i64> = BTreeMap::new();
    for step in &steps {
        let d = depth[&step.tag()];
        first_layer
            .entry(step.group.clone())
            .and_modify(|v| *v = (*v).min(d))
            .or_insert(d);
    }
    let mut upstream: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for ((from, to), n) in &g.edges {
        upstream
            .entry(to.clone())
            .or_default()
            .push((from.clone(), *n));
    }
    let mut order: Vec<(&String, i64)> = first_layer.iter().map(|(k, v)| (k, *v)).collect();
    order.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));

    let width = g
        .counts
        .keys()
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(1);
    let count_width = g
        .counts
        .values()
        .map(|n| step_count_text(*n).chars().count())
        .max()
        .unwrap_or(1);

    let mut out: Vec<String> = vec![
        format!(
            "DAG by group - {} groups, {} steps, {} group edges ({} cross-group step edges merged; {} within groups)",
            g.counts.len(),
            steps.len(),
            g.edges.len(),
            g.cross_edges,
            g.intra_edges
        ),
        String::new(),
    ];
    for (group, _) in order {
        let count = step_count_text(g.counts[group]);
        match upstream.get(group) {
            Some(ups) => {
                let ups: Vec<String> = ups.iter().map(|(u, n)| format!("{u} ({n})")).collect();
                out.push(format!(
                    "  {group:<width$}  {count:<count_width$}  <- {}",
                    ups.join(", ")
                ));
            }
            None => out.push(format!("  {group:<width$}  {count}")),
        }
    }
    out.join("\n") + "\n"
}

/// Longest-dependency-depth layer for each step tag.
fn layers_of(steps: &[&Step], deps: &HashMap<String, Vec<String>>) -> HashMap<String, i64> {
    let mut depth: HashMap<String, i64> = HashMap::new();

    fn visit(
        tag: &str,
        deps: &HashMap<String, Vec<String>>,
        depth: &mut HashMap<String, i64>,
    ) -> i64 {
        if let Some(d) = depth.get(tag) {
            return *d;
        }
        let parents = deps.get(tag).cloned().unwrap_or_default();
        let d = if parents.is_empty() {
            0
        } else {
            1 + parents
                .iter()
                .map(|p| visit(p, deps, depth))
                .max()
                .unwrap_or(0)
        };
        depth.insert(tag.to_string(), d);
        d
    }

    for step in steps {
        visit(&step.tag(), deps, &mut depth);
    }
    depth
}

/// Render the DAG as compact ASCII art, grouped by topological layer.
pub fn to_ascii(cfg: &DagConfig, selected: Option<&HashSet<String>>) -> String {
    let steps = selected_steps(cfg, selected);
    let deps = kept_deps(&steps);
    let depth = layers_of(&steps, &deps);

    // depth -> steps (cfg order preserved before the per-layer tag sort).
    let mut layers: BTreeMap<i64, Vec<&Step>> = BTreeMap::new();
    for step in &steps {
        layers.entry(depth[&step.tag()]).or_default().push(step);
    }

    let edge_count: usize = deps.values().map(|d| d.len()).sum();
    let width = steps.iter().map(|s| s.tag().len()).max().unwrap_or(1);

    let mut out: Vec<String> = vec![
        format!(
            "DAG - {} steps, {} edges, {} layer(s)",
            steps.len(),
            edge_count,
            layers.len()
        ),
        String::new(),
    ];

    for (level, layer_steps) in &layers {
        out.push(format!("layer {level}:"));
        let mut sorted_steps: Vec<&&Step> = layer_steps.iter().collect();
        sorted_steps.sort_by_key(|a| a.tag());
        for step in sorted_steps {
            let tag = step.tag();
            let mut res = String::new();
            for (key, value) in &step.hint.resources {
                write!(&mut res, " {{{key}:{value}}}").expect("writing to a String cannot fail");
            }
            let mut step_deps = deps[&tag].clone();
            step_deps.sort();
            let dep = if step_deps.is_empty() {
                String::new()
            } else {
                format!("  <- {}", step_deps.join(", "))
            };
            out.push(format!(
                "  {tag:<width$}  [{}]{res}{dep}",
                step_classification(step).value(),
                width = width
            ));
        }
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceHint;

    fn cfg() -> DagConfig {
        let browser = |n: i64| {
            let mut m = BTreeMap::new();
            m.insert("browser".to_string(), n);
            m
        };
        let mk = |group: &str, job: &str, deps: &[&str], res: BTreeMap<String, i64>| Step {
            group: group.into(),
            job: job.into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                resources: res,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        let mut caps = BTreeMap::new();
        caps.insert("browser".to_string(), 1);
        DagConfig {
            steps: vec![
                mk("build", "app", &[], BTreeMap::new()),
                mk("test", "unit", &["build.app"], BTreeMap::new()),
                mk("e2e", "a", &["build.app"], browser(1)),
                mk("e2e", "b", &["build.app"], browser(1)),
            ],
            resource_caps: caps,
            ..Default::default()
        }
    }

    #[test]
    fn dot_has_clusters_nodes_and_edges() {
        let dot = to_dot(&cfg(), "dag", None);
        assert!(dot.starts_with("digraph dag {"));
        assert!(dot.contains("\"build.app\" -> \"test.unit\";"));
        assert!(dot.contains("\"build.app\" -> \"e2e.a\";"));
        assert!(dot.contains("\"e2e.a\" -> \"e2e.b\" [style=dashed"));
        assert!(dot.trim_end().ends_with('}'));
    }

    #[test]
    fn ascii_shows_layers_deps_and_resources() {
        let art = to_ascii(&cfg(), None);
        assert!(art.contains("layer 0:") && art.contains("layer 1:"));
        assert!(art.contains("build.app"));
        assert!(art.contains("<- build.app"));
        assert!(art.contains("{browser:1}"));
    }

    #[test]
    fn group_dot_quotes_names_and_draws_no_self_edges() {
        let mut cfg = cfg();
        // Every dependency stays inside one group: no group edge at all.
        for step in &mut cfg.steps {
            step.group = "we\"ird\\grp".into();
        }
        let tags: Vec<String> = cfg.steps.iter().map(|s| s.tag()).collect();
        cfg.steps[1].deps = vec![tags[0].clone()];
        cfg.steps[2].deps = vec![tags[0].clone()];
        cfg.steps[3].deps = vec![tags[0].clone()];
        let dot = to_dot_groups(&cfg, "dag", None);
        assert!(dot.contains("  \"we\\\"ird\\\\grp\" [label=\"we\\\"ird\\\\grp\\n4 steps\"];\n"));
        assert!(!dot.contains("->"), "{dot}");
        let art = to_ascii_groups(&cfg, None);
        assert!(art.starts_with("DAG by group - 1 groups, 4 steps, 0 group edges (0 cross-group step edges merged; 3 within groups)\n"));
    }

    #[test]
    fn dot_omits_profiling_when_no_estimates() {
        // An undecorated DAG (no est/rss) must render exactly as before: no per-node
        // "Xs, YMB" annotation and no scaling suffix on the graph title.
        let dot = to_dot(&cfg(), "dag", None);
        assert!(dot.contains("\"build.app\" [label=\"build.app\\n[light]\"];"));
        assert!(!dot.contains("max par-spdup"));
        assert!(!dot.contains("MB"));
    }

    #[test]
    fn dot_annotates_profiling_and_scaling() {
        // Two nodes on the critical path (a: 30s -> b: 60s = 90s), plus an off-path
        // node c: 30s. Serial = 120s, critical path = 90s, ideal speedup = 1.3X.
        let profiled = |group: &str, job: &str, deps: &[&str], est: f64, rss: i64| Step {
            group: group.into(),
            job: job.into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                est_duration_s: est,
                rss_baseline_bytes: Some(rss),
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        let cfg = DagConfig {
            steps: vec![
                profiled("build", "a", &[], 30.0, 268_435_456),
                profiled("test", "b", &["build.a"], 60.0, 3_221_225_472),
                profiled("test", "c", &["build.a"], 30.0, 1_073_741_824),
            ],
            ..Default::default()
        };
        let dot = to_dot(&cfg, "dag", None);
        // Per-node "est-s, RSS-MB" (RSS floored to decimal MB).
        assert!(dot.contains("\"build.a\" [label=\"build.a\\n[light]\\n30.0s, 268MB\"];"));
        assert!(dot.contains("\"test.b\" [label=\"test.b\\n[light]\\n60.0s, 3221MB\"];"));
        // Graph-title scaling: serial 120 / critpath 90 = 1.3X.
        assert!(dot.contains("max par-spdup"));
        assert!(dot.contains("|  1.3X max par-spdup"));
    }

    #[test]
    fn critical_path_is_longest_weighted_chain() {
        let profiled = |group: &str, job: &str, deps: &[&str], est: f64| Step {
            group: group.into(),
            job: job.into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                est_duration_s: est,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        let cfg = DagConfig {
            steps: vec![
                profiled("a", "x", &[], 10.0),
                profiled("b", "y", &["a.x"], 5.0),
                profiled("c", "z", &["a.x", "b.y"], 7.0),
            ],
            ..Default::default()
        };
        let steps = selected_steps(&cfg, None);
        let deps = kept_deps(&steps);
        // Longest chain a.x(10) -> b.y(5) -> c.z(7) = 22, not a.x -> c.z = 17.
        assert_eq!(critical_path_seconds(&steps, &deps), 22.0);
    }
}
