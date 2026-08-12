#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Ergonomic front-door to the schema-v2 e2e manifest corpus.
//!
//! Where `manifest-to-commands.rs` expands *every* enabled cell into bucket
//! command files, this CLI answers the three questions an operator actually
//! asks about a single test:
//!
//! ```text
//! ./tests/manifest-cli.rs list [--bucket B] [--backend BE] [--tag T] [--mode M]
//! ./tests/manifest-cli.rs get  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL]
//! ./tests/manifest-cli.rs run  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [-- <extra hermit flags>]
//! ```
//!
//! - `list` enumerates tests across all manifests, filterable by bucket,
//!   by a backend that is enabled in some mode, by a `requires` capability
//!   token ("tag"), and/or by mode.
//! - `get` prints the exact Hermit command(s) a test runs, ready to paste.
//! - `run` executes a single test cell directly, honoring `--log`, a backend
//!   override, a lane override, and any extra flags after `--` (injected into
//!   the hermit invocation before the `-- <guest>` separator).
//!
//! The command construction mirrors `manifest-to-commands.rs` exactly so a
//! `get`/`run` line is byte-for-byte the same contract the CI expansion uses.
//! `run` executes from the repository root and uses `target/release/hermit`
//! unless `HERMIT_BIN` is set (a release binary is required — the debug binary
//! is far too slow for the corpus timeouts).
//!
//! ```cargo
//! [dependencies]
//! clap = { version = "4", features = ["derive"] }
//! toml = "0.8"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use clap::Args as ClapArgs;
use clap::Parser;
use clap::Subcommand;
use toml::Value;

const MANIFEST_SCHEMA: i64 = 2;
const RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=\"$cell/tmp\" E2E_FIXTURE_DIR=\"$cell/fixtures\"";

fn fail(message: impl AsRef<str>) -> ! {
    eprintln!("manifest-cli: {}", message.as_ref());
    std::process::exit(2);
}

fn repo_root() -> PathBuf {
    let script = Path::new(file!());
    let root = script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn string_array(value: Option<&Value>, context: &str) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let array = value
        .as_array()
        .unwrap_or_else(|| fail(format!("{context} must be an array")));
    array
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| fail(format!("{context} entries must be strings")))
                .to_owned()
        })
        .collect()
}

fn integer_array(value: Option<&Value>, context: &str) -> Vec<i64> {
    let Some(value) = value else {
        return Vec::new();
    };
    let array = value
        .as_array()
        .unwrap_or_else(|| fail(format!("{context} must be an array")));
    array
        .iter()
        .map(|item| {
            item.as_integer()
                .unwrap_or_else(|| fail(format!("{context} entries must be integers")))
        })
        .collect()
}

fn test_id(test: &Value, bucket: &str) -> String {
    test.get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail(format!("{bucket}: [[test]] is missing `id`")))
        .to_owned()
}

/// Build the shell setup prefix (compile/prepare the guest) and return the
/// guest invocation string. Identical contract to `manifest-to-commands.rs`.
fn setup_prefix(test: &Value, id: &str) -> (String, String) {
    let cell = format!("ignored/e2e-commands/work/{}", slug(id));
    let mut commands = vec![
        format!("cell={}", shell_quote(&cell)),
        "hermit_bin=${HERMIT_BIN:-target/release/hermit}".to_owned(),
        "mkdir -p \"$cell/home\" \"$cell/xdg-config\" \"$cell/tmp\" \"$cell/fixtures\" \"$cell/captures\""
            .to_owned(),
        "if [ -d tests/e2e/xdg-config ]; then cp -a tests/e2e/xdg-config/. \"$cell/xdg-config/\"; fi"
            .to_owned(),
    ];

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct");
    let guest = match (program, direct) {
        (Some(_), Some(_)) => fail(format!("{id}: set only one of `program` and `direct`")),
        (None, None) => fail(format!("{id}: missing `program` or `direct`")),
        (None, Some(Value::String(command))) => format!("sh -c {}", shell_quote(command)),
        (None, Some(Value::Array(_))) => {
            let argv = string_array(direct, &format!("{id}.direct"));
            if argv.is_empty() {
                fail(format!("{id}: direct argv must not be empty"));
            }
            argv.iter()
                .map(|argument| shell_quote(argument))
                .collect::<Vec<_>>()
                .join(" ")
        }
        (None, Some(_)) => fail(format!(
            "{id}: direct must be a shell command string or an argv array"
        )),
        (Some(program), None) => match Path::new(program).extension().and_then(|x| x.to_str()) {
            Some("sh") => {
                let script = shell_quote(program);
                commands.push(format!("{RUN_ENV} {script} --prepare"));
                format!("{script} --run")
            }
            Some("c") => {
                let build = test.get("build").and_then(Value::as_table);
                let mut args = vec![
                    "-std=c11".to_owned(),
                    "-O2".to_owned(),
                    "-g".to_owned(),
                    "-Wall".to_owned(),
                    "-Wextra".to_owned(),
                    "-Werror".to_owned(),
                ];
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("cflags"),
                        &format!("{id}.build.cflags"),
                    ));
                }
                args.push(program.to_owned());
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("extra_sources"),
                        &format!("{id}.build.extra_sources"),
                    ));
                }
                let args = args
                    .iter()
                    .map(|x| shell_quote(x))
                    .collect::<Vec<_>>()
                    .join(" ");
                commands.push(format!("${{CC:-cc}} {args} -o \"$cell/guest\""));
                "\"$cell/guest\"".to_owned()
            }
            Some("rs") => {
                let build = test.get("build").and_then(Value::as_table);
                let mut args = vec!["-O".to_owned()];
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("cflags"),
                        &format!("{id}.build.cflags"),
                    ));
                }
                args.push(program.to_owned());
                let args = args
                    .iter()
                    .map(|x| shell_quote(x))
                    .collect::<Vec<_>>()
                    .join(" ");
                commands.push(format!("${{RUSTC:-rustc}} {args} -o \"$cell/guest\""));
                "\"$cell/guest\"".to_owned()
            }
            other => fail(format!("{id}: unsupported program extension {other:?}")),
        },
    };

    (commands.join(" && "), guest)
}

/// Assemble the Hermit invocation for one (mode, backend) cell. `log` overrides
/// the `--log=` level; `extra` are additional hermit flags injected before the
/// `-- <guest>` separator. Mirrors `manifest-to-commands.rs::hermit_command`
/// with the added override hooks used by `get`/`run`.
fn hermit_command(
    mode: &str,
    backend: &str,
    lane: &str,
    timeout: i64,
    seed: Option<i64>,
    mode_args: &[String],
    log: &str,
    extra: &[String],
    guest: &str,
) -> String {
    let portable = lane == "portable";
    let profile: Vec<String> = if portable {
        vec![
            "--no-virtualize-cpuid".to_owned(),
            "--max-timeslice=disabled".to_owned(),
        ]
    } else {
        Vec::new()
    };
    let be = shell_quote(backend);
    let extra_joined = {
        let mut all: Vec<String> = profile;
        all.extend(extra.iter().map(|x| shell_quote(x)));
        let joined = all.join(" ");
        if joined.is_empty() {
            String::new()
        } else {
            format!(" {joined}")
        }
    };
    let command = match mode {
        "verify" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log={log} run --backend {be} --strict --verify{extra_joined} -- {guest}"
        ),
        "replay" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log={log} --backend {be} record start --strict --verify{extra_joined} -- {guest}"
        ),
        "chaos" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log={log} run --backend {be} --strict --chaos --sched-heuristic=random --seed={}{extra_joined} -- {guest}",
            seed.unwrap_or(0)
        ),
        "custom" => {
            let margs = mode_args
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ");
            let sep = if margs.is_empty() { "" } else { " " };
            format!(
                "{RUN_ENV} \"$hermit_bin\" --log={log} run --backend {be} --strict{sep}{margs}{extra_joined} -- {guest}"
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    format!("timeout --kill-after=10s {timeout}s {command}")
}

/// Default `--log` level per mode, matching the CI expansion (`off` for chaos,
/// `info` otherwise).
fn default_log(mode: &str) -> &'static str {
    if mode == "chaos" { "off" } else { "info" }
}

struct Manifests {
    /// (bucket, test-value) for every [[test]] across all manifests, sorted.
    tests: Vec<(String, Value)>,
}

fn load_manifests(root: &Path) -> Manifests {
    let dir = root.join("tests/e2e/manifests");
    let mut paths = fs::read_dir(&dir)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", dir.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut tests = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| fail(format!("non-UTF-8 manifest name: {}", path.display())));
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
        let manifest: Value = source
            .parse()
            .unwrap_or_else(|e| fail(format!("{}: invalid TOML: {e}", path.display())));
        let schema = manifest.get("schema").and_then(Value::as_integer);
        if schema != Some(MANIFEST_SCHEMA) {
            fail(format!(
                "{}: expected schema {MANIFEST_SCHEMA}, got {schema:?}",
                path.display()
            ));
        }
        let bucket = manifest
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or_else(|| fail(format!("{}: missing `bucket`", path.display())))
            .to_owned();
        if bucket != stem {
            fail(format!(
                "{}: bucket `{bucket}` must match file stem `{stem}`",
                path.display()
            ));
        }
        let entries = manifest
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: missing [[test]] entries", path.display())));
        for test in entries {
            tests.push((bucket.clone(), test.clone()));
        }
    }
    Manifests { tests }
}

fn modes_table<'a>(test: &'a Value, id: &str) -> &'a toml::map::Map<String, Value> {
    test.get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| fail(format!("{id}: missing `modes`")))
}

/// Backends enabled for a given mode (native for `naked`).
fn mode_backends(spec: &Value, mode: &str, id: &str) -> Vec<String> {
    if mode == "naked" {
        return vec!["native".to_owned()];
    }
    string_array(
        spec.get("backends_enabled"),
        &format!("{id}.modes.{mode}.backends_enabled"),
    )
}

/// Union of backends enabled across all of a test's modes.
fn test_backends(test: &Value, id: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for (mode, spec) in modes_table(test, id) {
        for be in mode_backends(spec, mode, id) {
            set.insert(be);
        }
    }
    set
}

fn requires(test: &Value, id: &str) -> Vec<String> {
    string_array(test.get("requires"), &format!("{id}.requires"))
}

fn test_lane(test: &Value) -> &str {
    test.get("lane")
        .and_then(Value::as_str)
        .unwrap_or("portable")
}

fn test_timeout(test: &Value) -> i64 {
    test.get("timeout_seconds")
        .and_then(Value::as_integer)
        .unwrap_or(60)
}

/// Pick a default mode: prefer `verify` if it has enabled backends, else the
/// first (sorted) non-naked mode with enabled backends, else `naked`.
fn default_mode(test: &Value, id: &str) -> String {
    let modes = modes_table(test, id);
    if let Some(spec) = modes.get("verify") {
        if !mode_backends(spec, "verify", id).is_empty() {
            return "verify".to_owned();
        }
    }
    let mut names = modes.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in &names {
        if name == "naked" {
            continue;
        }
        if !mode_backends(&modes[name], name, id).is_empty() {
            return name.clone();
        }
    }
    if modes.contains_key("naked") {
        return "naked".to_owned();
    }
    fail(format!("{id}: no mode has an enabled backend"))
}

fn find_test<'a>(manifests: &'a Manifests, id: &str) -> &'a Value {
    manifests
        .tests
        .iter()
        .find(|(bucket, test)| test_id(test, bucket) == id)
        .map(|(_, test)| test)
        .unwrap_or_else(|| {
            fail(format!(
                "no test with id `{id}` (try `manifest-cli list` to see ids)"
            ))
        })
}

// ---- command line ------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "manifest-cli",
    about = "Inspect and run cells from Hermit's end-to-end test manifests",
    long_about = "Inspect and run cells from Hermit's end-to-end test manifests.\n\n\
Global cell selectors may appear before or after the subcommand. Use `list` to \
discover test IDs, `get` to print a pasteable command without running it, and \
`run` to execute one selected cell."
)]
struct Cli {
    /// Backend used or selected by the cell (for example ptrace, DBT, or KVM).
    #[arg(long, global = true, value_name = "BACKEND")]
    backend: Option<String>,

    /// Manifest mode to inspect or run: verify, naked, replay, chaos, or custom.
    #[arg(long, global = true, value_name = "MODE")]
    mode: Option<String>,

    /// Validation lane to inspect or run: portable or privileged.
    #[arg(long, global = true, value_name = "LANE")]
    lane: Option<String>,

    /// Hermit log level for get/run: info, debug, trace, or off.
    #[arg(long, global = true, value_name = "LEVEL")]
    log: Option<String>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// List test IDs and the lanes and backends available for each test.
    List(ListArgs),
    /// Print the exact shell command for one test cell without executing it.
    Get(GetArgs),
    /// Execute one test cell from the repository root.
    Run(RunArgs),
}

#[derive(ClapArgs)]
struct ListArgs {
    /// Restrict results to one manifest bucket.
    #[arg(long, value_name = "BUCKET")]
    bucket: Option<String>,

    /// Restrict results to tests requiring this capability token.
    #[arg(long, value_name = "CAPABILITY")]
    tag: Option<String>,

    /// Include requirements and the backend list for every mode.
    #[arg(long)]
    verbose: bool,
}

#[derive(ClapArgs)]
struct GetArgs {
    /// Manifest test ID, as printed by `manifest-cli list`.
    #[arg(value_name = "TEST_ID")]
    test_id: String,

    /// Print one command for every enabled mode/backend pair.
    #[arg(long)]
    all_modes: bool,
}

#[derive(ClapArgs)]
struct RunArgs {
    /// Manifest test ID, as printed by `manifest-cli list`.
    #[arg(value_name = "TEST_ID")]
    test_id: String,

    /// Extra Hermit flags inserted before the guest command; pass after `--`.
    #[arg(last = true, value_name = "HERMIT_FLAG")]
    extra: Vec<String>,
}

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
    passthrough: Vec<String>,
}

impl Args {
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
}

// ---- subcommands -------------------------------------------------------------

fn cmd_list(manifests: &Manifests, args: &Args) -> ExitCode {
    let bucket_f = args.flag("bucket");
    let backend_f = args.flag("backend");
    let tag_f = args.flag("tag");
    let mode_f = args.flag("mode");
    let lane_f = args.flag("lane");
    let verbose = args.has("verbose");

    let mut shown = 0usize;
    for (bucket, test) in &manifests.tests {
        let id = test_id(test, bucket);
        if let Some(b) = bucket_f {
            if bucket != b {
                continue;
            }
        }
        if let Some(l) = lane_f {
            if test_lane(test) != l {
                continue;
            }
        }
        let backends = test_backends(test, &id);
        if let Some(be) = backend_f {
            // If a mode filter is present, restrict to that mode's backends.
            let ok = match mode_f {
                Some(m) => modes_table(test, &id)
                    .get(m)
                    .map(|spec| mode_backends(spec, m, &id).iter().any(|x| x == be))
                    .unwrap_or(false),
                None => backends.contains(be),
            };
            if !ok {
                continue;
            }
        } else if let Some(m) = mode_f {
            // mode filter without backend filter: test must define that mode
            // with at least one enabled backend
            let has = modes_table(test, &id)
                .get(m)
                .map(|spec| !mode_backends(spec, m, &id).is_empty())
                .unwrap_or(false);
            if !has {
                continue;
            }
        }
        let reqs = requires(test, &id);
        if let Some(t) = tag_f {
            if !reqs.iter().any(|r| r == t) {
                continue;
            }
        }
        shown += 1;
        let backends_str = backends.iter().cloned().collect::<Vec<_>>().join(",");
        println!(
            "{:<48} lane={:<10} backends=[{}]",
            id,
            test_lane(test),
            backends_str
        );
        if verbose {
            println!("    requires=[{}]", reqs.join(","));
            let modes = modes_table(test, &id);
            let mut names = modes.keys().cloned().collect::<Vec<_>>();
            names.sort();
            for name in names {
                let bes = mode_backends(&modes[name.as_str()], &name, &id);
                if !bes.is_empty() {
                    println!("    mode {:<8} -> [{}]", name, bes.join(","));
                }
            }
        }
    }
    eprintln!("manifest-cli: {shown} test(s) listed");
    ExitCode::SUCCESS
}

/// Resolve mode + backend + lane for a get/run, applying overrides.
fn resolve_cell(test: &Value, id: &str, args: &Args) -> (String, String, String, i64) {
    let mode = args
        .flag("mode")
        .map(str::to_owned)
        .unwrap_or_else(|| default_mode(test, id));
    let modes = modes_table(test, id);
    let spec = modes.get(&mode).unwrap_or_else(|| {
        fail(format!(
            "{id}: no mode `{mode}` (have: {:?})",
            modes.keys().collect::<Vec<_>>()
        ))
    });
    let enabled = mode_backends(spec, &mode, id);
    let backend = match args.flag("backend") {
        Some(b) => b.to_owned(),
        None => enabled.first().cloned().unwrap_or_else(|| {
            fail(format!(
                "{id}: mode `{mode}` has no enabled backend; pass --backend"
            ))
        }),
    };
    if args.flag("backend").is_none() && mode != "naked" && !enabled.contains(&backend) {
        // (unreachable given the first() default, but keep the invariant clear)
    }
    let lane = args
        .flag("lane")
        .map(str::to_owned)
        .unwrap_or_else(|| test_lane(test).to_owned());
    let timeout = test_timeout(test);
    (mode, backend, lane, timeout)
}

fn build_full_command(test: &Value, id: &str, args: &Args) -> (String, String, String) {
    let (mode, backend, lane, timeout) = resolve_cell(test, id, args);
    let (setup, guest) = setup_prefix(test, id);
    let log = args
        .flag("log")
        .map(str::to_owned)
        .unwrap_or_else(|| default_log(&mode).to_owned());
    let mode_args = if mode == "custom" {
        let modes = modes_table(test, id);
        string_array(modes[&mode].get("args"), &format!("{id}.modes.custom.args"))
    } else {
        Vec::new()
    };
    let seed = if mode == "chaos" {
        let modes = modes_table(test, id);
        let seeds = integer_array(
            modes[&mode].get("seeds"),
            &format!("{id}.modes.chaos.seeds"),
        );
        Some(seeds.first().copied().unwrap_or(0))
    } else {
        None
    };
    let run = if mode == "naked" {
        format!("timeout --kill-after=10s {timeout}s {RUN_ENV} {guest}")
    } else {
        hermit_command(
            &mode,
            &backend,
            &lane,
            timeout,
            seed,
            &mode_args,
            &log,
            &args.passthrough,
            &guest,
        )
    };
    let full = format!("{setup} && {run}");
    (full, mode, backend)
}

fn cmd_get(manifests: &Manifests, args: &Args) -> ExitCode {
    let id = args
        .positional
        .first()
        .unwrap_or_else(|| fail("get: missing <test-id>"));
    let test = find_test(manifests, id);
    if args.has("all-modes") {
        let modes = modes_table(test, id);
        let mut names = modes.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            for be in mode_backends(&modes[name.as_str()], &name, id) {
                let mut sub = Args::default();
                sub.flags.push(("mode".to_owned(), Some(name.clone())));
                sub.flags.push(("backend".to_owned(), Some(be.clone())));
                if let Some(l) = args.flag("log") {
                    sub.flags.push(("log".to_owned(), Some(l.to_owned())));
                }
                if let Some(l) = args.flag("lane") {
                    sub.flags.push(("lane".to_owned(), Some(l.to_owned())));
                }
                sub.positional.push(id.clone());
                let (full, mode, backend) = build_full_command(test, id, &sub);
                println!("# {id} mode={mode} backend={backend}");
                println!("{full}\n");
            }
        }
        return ExitCode::SUCCESS;
    }
    let (full, mode, backend) = build_full_command(test, id, args);
    println!("# {id} mode={mode} backend={backend}");
    println!("{full}");
    ExitCode::SUCCESS
}

fn cmd_run(manifests: &Manifests, args: &Args, root: &Path) -> ExitCode {
    let id = args
        .positional
        .first()
        .unwrap_or_else(|| fail("run: missing <test-id>"));
    let test = find_test(manifests, id);
    let (full, mode, backend) = build_full_command(test, id, args);
    eprintln!("manifest-cli: running {id} mode={mode} backend={backend}");
    eprintln!("manifest-cli: $ {full}");
    let status = Command::new("sh")
        .arg("-c")
        .arg(&full)
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| fail(format!("failed to spawn shell: {e}")));
    match status.code() {
        Some(0) => {
            eprintln!("manifest-cli: {id} exited 0");
            ExitCode::SUCCESS
        }
        Some(code) => {
            eprintln!("manifest-cli: {id} exited {code}");
            ExitCode::from(code as u8)
        }
        None => {
            eprintln!("manifest-cli: {id} terminated by signal");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let cli = Cli::parse();
    let mut args = Args::default();
    for (name, value) in [
        ("backend", cli.backend),
        ("mode", cli.mode),
        ("lane", cli.lane),
        ("log", cli.log),
    ] {
        if let Some(value) = value {
            args.flags.push((name.to_owned(), Some(value)));
        }
    }
    let sub = match cli.command {
        CommandKind::List(list) => {
            if let Some(value) = list.bucket {
                args.flags.push(("bucket".to_owned(), Some(value)));
            }
            if let Some(value) = list.tag {
                args.flags.push(("tag".to_owned(), Some(value)));
            }
            if list.verbose {
                args.flags.push(("verbose".to_owned(), None));
            }
            "list"
        }
        CommandKind::Get(get) => {
            args.positional.push(get.test_id);
            if get.all_modes {
                args.flags.push(("all-modes".to_owned(), None));
            }
            "get"
        }
        CommandKind::Run(run) => {
            args.positional.push(run.test_id);
            args.passthrough = run.extra;
            "run"
        }
    };
    let root = repo_root();
    let manifests = load_manifests(&root);
    match sub {
        "list" => cmd_list(&manifests, &args),
        "get" => cmd_get(&manifests, &args),
        "run" => cmd_run(&manifests, &args, &root),
        _ => unreachable!("Clap only constructs known subcommands"),
    }
}
