//! Package-facing command-line interface.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::classify::{flaky_signatures_from_value, ClassifyConfig};
use crate::collect::{collect_graph, CollectOptions};
use crate::context::parse_landing_context;
use crate::emit::{
    render_actions, render_clusters_human, render_clusters_json, render_graph_human,
    render_graph_json, render_human, render_json, render_status_human, render_status_json,
};
use crate::fixture::{load_fixture_text, FakeHost};
use crate::host::{GitHubHost, VcsHost};
use crate::model::{PlanResult, DEFAULT_BASE, DEFAULT_GATE_CHECK, DEFAULT_REPO};
use crate::plan::{assemble_result, DEFAULT_FRESHNESS_MAX_BEHIND};
use crate::priority::{make_priority_provider, DEFAULT_LABEL_PATTERN};
use crate::VERSION;

/// Installed executable name.
pub const PROG: &str = "pr-landing-planner";
/// Default open-PR count at which the status view emits a warning.
pub const DEFAULT_WARN_THRESHOLD: usize = 8;
const USER_GUIDE: &str = include_str!("embedded_userguide.md");

const DEMO_FIXTURE: &str = r#"repo: OWNER/NAME
base: main
prs:
  - number: 1043
    title: fast, fresh, all green
    head_ref: feat-a
    additions: 10
    labels: [validated-locally]
    changed_files: [src/a.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
  - number: 987
    title: green but 6 behind base
    head_ref: feat-b
    additions: 40
    commits_behind: 6
    changed_files: [src/b.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
  - number: 942
    title: CI green, required gate stale
    head_ref: feat-c
    changed_files: [src/c.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: FAILURE, text: stale result}
  - number: 1050
    title: gate fired while CI queued
    head_ref: feat-d
    changed_files: [src/d.rs]
    checks:
      - {name: CI, status: IN_PROGRESS, conclusion: ""}
      - {name: merge-gate, status: COMPLETED, conclusion: FAILURE, text: "Full CI still queued; rerun after CI completes"}
  - number: 1049
    title: red on wasm-core (add a --flaky-signatures file to reclassify as flaky)
    head_ref: feat-e
    changed_files: [src/e.rs]
    checks:
      - {name: wasm-core, status: COMPLETED, conclusion: FAILURE, text: browser flake}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
conflicts:
  - {a: 987, b: 942, paths: [src/shared.rs]}
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    Plan,
    Graph,
    Clusters,
    Status,
    Quickstart,
}

#[derive(Debug)]
struct Options {
    command: Option<CommandKind>,
    userguide: bool,
    help: bool,
    emit_demo: bool,
    repo: String,
    base: String,
    prs: Option<String>,
    git_dir: PathBuf,
    remote: String,
    landing_context: Option<PathBuf>,
    net_wrapper: String,
    gh_cmd: String,
    conflict_detector: String,
    fixture: Option<PathBuf>,
    gate_check: String,
    flaky_signatures: Option<PathBuf>,
    outage_min_prs: usize,
    freshness_max_behind: Option<i64>,
    priority_source: String,
    priority_label_pattern: String,
    priority_cmd: String,
    batch: bool,
    format: String,
    archive_dir: Option<PathBuf>,
    no_archive: bool,
    warn_threshold: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            command: None,
            userguide: false,
            help: false,
            emit_demo: false,
            repo: DEFAULT_REPO.to_owned(),
            base: DEFAULT_BASE.to_owned(),
            prs: None,
            git_dir: PathBuf::from("."),
            remote: "origin".to_owned(),
            landing_context: None,
            net_wrapper: String::new(),
            gh_cmd: "gh".to_owned(),
            conflict_detector: "merge-tree".to_owned(),
            fixture: None,
            gate_check: DEFAULT_GATE_CHECK.to_owned(),
            flaky_signatures: None,
            outage_min_prs: 2,
            freshness_max_behind: DEFAULT_FRESHNESS_MAX_BEHIND,
            priority_source: "none".to_owned(),
            priority_label_pattern: DEFAULT_LABEL_PATTERN.to_owned(),
            priority_cmd: String::new(),
            batch: false,
            format: "human".to_owned(),
            archive_dir: None,
            no_archive: false,
            warn_threshold: DEFAULT_WARN_THRESHOLD,
        }
    }
}

fn help() -> String {
    format!(
        "{PROG} v{VERSION}\nA conflict-graph + CI-aware, advisory pull-request landing planner.\n\nUSAGE:\n  {PROG} <command> [OPTIONS]\n  {PROG} --userguide\n\nCOMMANDS:\n  plan        build the graph and print the fused landing plan\n  graph       print the conflict and ordering graph\n  clusters    print real-conflict stack landing lanes\n  status      print per-PR CI health\n  quickstart  print a self-contained getting-started guide\n\nCOMMON OPTIONS:\n  --repo OWNER/NAME          repository (required for live collection)\n  --base BRANCH              base branch (default: {DEFAULT_BASE})\n  --fixture FILE             deterministic JSON/YAML input instead of a live host\n  --format FORMAT            human/json; plan also supports actions\n  --git-dir DIR              local clone for live collection\n  --prs N,N                  restrict to PR numbers\n  --conflict-detector KIND   merge-tree or file-overlap\n  --gate-check NAME          required gate name (default: {DEFAULT_GATE_CHECK})\n  --landing-context FILE     exact-head evidence, validation authority, policy, and agent context\n  --flaky-signatures FILE    known flaky name/text regexes\n  --net-wrapper COMMAND      prefix live network commands\n  --gh-cmd PATH              gh-compatible executable\n\nPLAN OPTIONS:\n  --outage-min-prs N         minimum correlated failures for outage classification\n  --freshness-max-behind N   caller override (default: none; being behind is not a rebase reason)\n  --priority-source SOURCE   none, labels, or command\n  --priority-label-pattern R label priority pattern (default: {DEFAULT_LABEL_PATTERN})\n  --priority-cmd COMMAND     command used by the command priority source\n  --batch                    emit a conflict/dependency-free root batch\n  --archive-dir DIR          directory for canonical JSON plan archives\n  --no-archive               disable the default live-run archive\n\nSTATUS OPTIONS:\n  --warn-threshold N         unhealthy-PR warning threshold (default: {DEFAULT_WARN_THRESHOLD})\n\nQUICKSTART OPTIONS:\n  --emit-demo                print the bundled deterministic fixture\n\nGENERAL OPTIONS:\n  -h, --help                 print help\n  --version                  print version\n  --userguide                print the complete embedded reference\n\nRun `{PROG} quickstart` for examples. The tool never mutates a PR."
    )
}

fn quickstart() -> String {
    format!(
        "{PROG} v{VERSION}\nA conflict-graph + CI-aware, ADVISORY pull-request landing planner.\n\nThe idea\n  Fuse real merge conflicts, exact-head CI evidence, caller validation authority, freshness, holds,\n  priority, and semantic mechanism overlaps into deterministic per-PR actions. The planner never\n  changes a PR.\n\n1. Install\n  cargo install pr-landing-planner\n\n2. Try the bundled fixture (no repository or network)\n  {PROG} quickstart --emit-demo > demo.yaml\n  {PROG} plan --fixture demo.yaml\n\n3. Run against a live repository\n  {PROG} plan --repo OWNER/NAME --base main --git-dir /path/to/clone\n\nRed classifications\n  real -> hold-fix | flaky -> refire-ci | stale-required-check -> refire-stale-gate\n  evaluate-once-race -> wait | runner-outage -> escalate-runner-outage\n\nOutput\n  --format human | json | actions (plan only)\n  JSON is deterministic; actions starts with capturable key=value counts.\n\nUse `{PROG} --userguide` for all flags, fixture fields, and landing-context semantics.\n\nDemo fixture\n{DEMO_FIXTURE}"
    )
}

fn parse_command(value: &str) -> Option<CommandKind> {
    match value {
        "plan" => Some(CommandKind::Plan),
        "graph" => Some(CommandKind::Graph),
        "clusters" => Some(CommandKind::Clusters),
        "status" => Some(CommandKind::Status),
        "quickstart" => Some(CommandKind::Quickstart),
        _ => None,
    }
}

fn required_value(
    args: &[String],
    index: &mut usize,
    inline: Option<&str>,
    flag: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(format!("{flag} requires a value"));
        }
        return Ok(value.to_owned());
    }
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if !argument.starts_with('-') {
            if options.command.is_some() {
                return Err(format!("unexpected positional argument {argument:?}"));
            }
            options.command = Some(
                parse_command(argument).ok_or_else(|| format!("unknown command {argument:?}"))?,
            );
            index += 1;
            continue;
        }
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        let mut value = || required_value(args, &mut index, inline, flag);
        match flag {
            "-h" | "--help" => options.help = true,
            "--userguide" => options.userguide = true,
            "--version" => return Err("__VERSION__".to_owned()),
            "--emit-demo" => options.emit_demo = true,
            "--batch" => options.batch = true,
            "--no-archive" => options.no_archive = true,
            "--repo" => options.repo = value()?,
            "--base" => options.base = value()?,
            "--prs" => options.prs = Some(value()?),
            "--git-dir" => options.git_dir = PathBuf::from(value()?),
            "--remote" => options.remote = value()?,
            "--landing-context" => options.landing_context = Some(PathBuf::from(value()?)),
            "--net-wrapper" => options.net_wrapper = value()?,
            "--gh-cmd" => options.gh_cmd = value()?,
            "--conflict-detector" => options.conflict_detector = value()?,
            "--fixture" => options.fixture = Some(PathBuf::from(value()?)),
            "--gate-check" => options.gate_check = value()?,
            "--flaky-signatures" => options.flaky_signatures = Some(PathBuf::from(value()?)),
            "--outage-min-prs" => {
                options.outage_min_prs = parse_nonnegative_usize(&value()?, flag)?
            }
            "--freshness-max-behind" => {
                options.freshness_max_behind = Some(parse_number(&value()?, flag)?)
            }
            "--priority-source" => options.priority_source = value()?,
            "--priority-label-pattern" => options.priority_label_pattern = value()?,
            "--priority-cmd" => options.priority_cmd = value()?,
            "--format" => options.format = value()?,
            "--archive-dir" => options.archive_dir = Some(PathBuf::from(value()?)),
            "--warn-threshold" => {
                options.warn_threshold = parse_nonnegative_usize(&value()?, flag)?
            }
            _ => return Err(format!("unknown option {flag:?}")),
        }
        index += 1;
    }
    validate(&options)?;
    Ok(options)
}

fn parse_number<T: std::str::FromStr>(value: &str, flag: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} expects an integer, got {value:?}"))
}

fn parse_nonnegative_usize(value: &str, flag: &str) -> Result<usize, String> {
    let parsed: i64 = parse_number(value, flag)?;
    usize::try_from(parsed)
        .map_err(|_| format!("{flag} expects a nonnegative integer, got {value:?}"))
}

fn validate(options: &Options) -> Result<(), String> {
    if options
        .freshness_max_behind
        .is_some_and(|freshness| freshness < 0)
    {
        return Err("--freshness-max-behind must be nonnegative".to_owned());
    }
    if options.gate_check.trim().is_empty() {
        return Err("--gate-check must be non-empty".to_owned());
    }
    if !matches!(
        options.conflict_detector.as_str(),
        "merge-tree" | "file-overlap"
    ) {
        return Err("--conflict-detector must be merge-tree or file-overlap".to_owned());
    }
    if !matches!(
        options.priority_source.as_str(),
        "none" | "labels" | "command"
    ) {
        return Err("--priority-source must be none, labels, or command".to_owned());
    }
    let valid_format = match options.command {
        Some(CommandKind::Plan) => matches!(options.format.as_str(), "human" | "json" | "actions"),
        Some(CommandKind::Graph | CommandKind::Clusters | CommandKind::Status) => {
            matches!(options.format.as_str(), "human" | "json")
        }
        _ => true,
    };
    if !valid_format {
        return Err(format!(
            "unsupported --format {:?} for this command",
            options.format
        ));
    }
    Ok(())
}

fn load_doc(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let yaml = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "yaml" | "yml"));
    load_fixture_text(&text, yaml)
}

fn only_numbers(raw: Option<&str>) -> Result<Option<BTreeSet<i64>>, String> {
    raw.map(|raw| {
        raw.split(',')
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                let number: i64 = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid PR number {value:?}"))?;
                if number <= 0 {
                    return Err(format!(
                        "invalid PR number {value:?}: expected a positive i64"
                    ));
                }
                Ok(number)
            })
            .collect()
    })
    .transpose()
}

fn build_result(options: &Options) -> Result<PlanResult, String> {
    let wrapper = if options.net_wrapper.is_empty() {
        Vec::new()
    } else {
        shell_words::split(&options.net_wrapper)
            .map_err(|error| format!("invalid --net-wrapper: {error}"))?
    };
    let (mut host, repo, base): (Box<dyn VcsHost>, String, String) = if let Some(path) =
        &options.fixture
    {
        let (host, repo, base) = FakeHost::from_value(&load_doc(path)?)?;
        (Box::new(host), repo, base)
    } else {
        if options.repo.trim().is_empty() {
            return Err(
                "--repo OWNER/NAME is required for live runs (or use --fixture FILE)".to_owned(),
            );
        }
        (
            Box::new(GitHubHost::new(
                options.git_dir.clone(),
                options.remote.clone(),
                wrapper.clone(),
                options.gh_cmd.clone(),
            )),
            options.repo.clone(),
            options.base.clone(),
        )
    };
    let landing_context = options
        .landing_context
        .as_ref()
        .map(|path| parse_landing_context(&load_doc(path)?))
        .transpose()?
        .unwrap_or_default();
    let mut classify = ClassifyConfig {
        gate_check: options.gate_check.clone(),
        outage_min_prs: options.outage_min_prs,
        ..ClassifyConfig::default()
    };
    if let Some(path) = &options.flaky_signatures {
        classify.flaky_signatures = flaky_signatures_from_value(&load_doc(path)?);
    }
    classify.validate()?;
    let mut priority = make_priority_provider(
        &options.priority_source,
        &options.priority_label_pattern,
        &options.priority_cmd,
        &wrapper,
    )?;
    let only = only_numbers(options.prs.as_deref())?;
    let graph = collect_graph(
        host.as_mut(),
        CollectOptions {
            repo: &repo,
            base: &base,
            only: only.as_ref(),
            conflict_detector: &options.conflict_detector,
            classify_config: &classify,
            priority_provider: priority.as_mut(),
            landing_context: &landing_context,
        },
    )?;
    if let Some(error) = priority.last_error() {
        return Err(error.to_owned());
    }
    Ok(assemble_result(
        graph,
        options.freshness_max_behind,
        options.outage_min_prs,
        options.batch,
    ))
}

fn archive_dir(options: &Options) -> Option<PathBuf> {
    if options.no_archive {
        return None;
    }
    if let Some(path) = &options.archive_dir {
        return Some(expand_user(path));
    }
    if options.fixture.is_some() {
        return None;
    }
    if let Some(path) = std::env::var_os("PR_LANDING_PLANNER_ARCHIVE_DIR") {
        return Some(expand_user(Path::new(&path)));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Some(expand_user(Path::new(&path)).join("pr-landing-planner/plans"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".local/state/pr-landing-planner/plans"))
}

fn expand_user(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_owned();
    };
    if raw == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_owned());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_owned()
}

fn utc_stamp(now: std::time::Duration) -> String {
    let seconds = now.as_secs();
    let days = (seconds / 86_400) as i64;
    let seconds_in_day = seconds % 86_400;

    // Convert days since 1970-01-01 to a proleptic Gregorian date. This is the
    // civil-from-days algorithm, shifted so day zero is the Unix epoch.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    let hour = seconds_in_day / 3_600;
    let minute = seconds_in_day % 3_600 / 60;
    let second = seconds_in_day % 60;
    format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}_{:06}Z",
        now.subsec_micros()
    )
}

fn archive(options: &Options, result: &PlanResult) {
    let Some(directory) = archive_dir(options) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let repo = result.graph.repository.replace('/', "_");
    let base = result.graph.base.replace('/', "_");
    let path = directory.join(format!(
        "plan-{}-{}-{}.json",
        if repo.is_empty() { "repo" } else { &repo },
        if base.is_empty() { "base" } else { &base },
        utc_stamp(now),
    ));
    let write = fs::create_dir_all(&directory)
        .and_then(|()| fs::write(&path, format!("{}\n", render_json(result))));
    match write {
        Ok(()) => eprintln!("{PROG}: NOTE: plan archived to {}", path.display()),
        Err(error) => eprintln!(
            "{PROG}: WARN: could not archive plan to {}: {error}",
            path.display()
        ),
    }
}

fn run(options: &Options) -> Result<String, String> {
    let result = build_result(options)?;
    let output = match options.command {
        Some(CommandKind::Plan) => {
            let output = match options.format.as_str() {
                "json" => render_json(&result),
                "actions" => render_actions(&result),
                _ => render_human(&result),
            };
            archive(options, &result);
            output
        }
        Some(CommandKind::Graph) => match options.format.as_str() {
            "json" => render_graph_json(&result),
            _ => render_graph_human(&result),
        },
        Some(CommandKind::Clusters) => match options.format.as_str() {
            "json" => render_clusters_json(&result),
            _ => render_clusters_human(&result),
        },
        Some(CommandKind::Status) => match options.format.as_str() {
            "json" => render_status_json(&result),
            _ => render_status_human(&result, options.warn_threshold),
        },
        _ => return Err("no collection command selected".to_owned()),
    };
    Ok(output)
}

/// Run the command-line interface with arguments excluding the executable name.
pub fn main(args: &[String]) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        Err(error) if error == "__VERSION__" => {
            println!("{PROG} {VERSION}");
            return 0;
        }
        Err(error) => {
            eprintln!("{PROG}: {error}");
            eprintln!("Try '{PROG} --help' for usage.");
            return 2;
        }
    };
    if options.userguide {
        print!("{USER_GUIDE}");
        return 0;
    }
    if options.help || options.command.is_none() {
        println!("{}", help());
        return 0;
    }
    if options.command == Some(CommandKind::Quickstart) {
        if options.emit_demo {
            print!("{DEMO_FIXTURE}");
        } else {
            println!("{}", quickstart());
        }
        return 0;
    }
    match run(&options) {
        Ok(output) => {
            println!("{output}");
            0
        }
        Err(error) => {
            eprintln!("{PROG}: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_equals_forms_and_rejects_unknown_formats() {
        let options = parse(&[
            "plan".into(),
            "--fixture=x.yaml".into(),
            "--format=actions".into(),
        ])
        .unwrap();
        assert_eq!(options.format, "actions");
        assert!(parse(&["status".into(), "--format=actions".into()]).is_err());
    }

    #[test]
    fn docs_are_rust_package_specific() {
        assert!(quickstart().contains("cargo install"));
        assert!(USER_GUIDE.contains("cargo install pr-landing-planner"));
        assert!(USER_GUIDE.contains("pr-landing-planner = \"0.1\""));
        assert!(help().contains("none, labels, or command"));
        assert!(!help().to_lowercase().contains("beads"));
    }

    #[test]
    fn archive_timestamp_matches_the_utc_filename_contract() {
        assert_eq!(
            utc_stamp(std::time::Duration::new(0, 123_456_789)),
            "19700101T000000_123456Z"
        );
        assert_eq!(
            utc_stamp(std::time::Duration::new(1_709_210_096, 987_654_321)),
            "20240229T123456_987654Z"
        );
    }
}
