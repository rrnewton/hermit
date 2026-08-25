#!/usr/bin/env -S rust-script --force
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
//! - `get` prints the exact Hermit command(s) a test runs, ready to paste;
//!   required cells include the checked-in boundary that creates and removes
//!   the otherwise-absent host `/test` mountpoint.
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
//! serde = { version = "1", features = ["derive"] }
//! serde_yaml = "0.9"
//! toml = "0.8"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../ci/manifest-plan/src/manifest_value.rs"]
mod manifest_value;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use manifest_value::Value;

const MANIFEST_SCHEMA: i64 = 2;
const GUEST_FIXTURE_ROOT: &str = "/tmp/hermit-e2e-fixtures";
const RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=\"$cell/tmp\" E2E_FIXTURE_DIR=\"$cell/fixtures\"";
const HERMIT_RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=\"$cell/fixtures\"";
const HERMIT_GUEST_ENV_ARGS: &str = "--env LC_ALL=C --env TZ=UTC --env HOME=\"$cell/home\" --env XDG_CONFIG_HOME=\"$cell/xdg-config\" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR=\"$cell/fixtures\"";

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

fn render_guest_argument(value: &str) -> String {
    let Some(relative) = value.strip_prefix("./") else {
        return shell_quote(value);
    };
    if Path::new(relative)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        fail(format!(
            "explicit checkout argument must not escape the repository: {value}"
        ));
    }
    format!("\"$(pwd)\"/{}", shell_quote(relative))
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

fn first_chaos_seed(spec: &Value, id: &str) -> Result<i64, String> {
    integer_array(
        spec.get("seeds"),
        &format!("{id}.modes.chaos.seeds"),
    )
    .first()
    .copied()
    .ok_or_else(|| {
        format!(
            "{id}: chaos mode is unavailable because its manifest declares no seeds; no guest command can be printed or run"
        )
    })
}

fn test_id(test: &Value, bucket: &str) -> String {
    test.get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail(format!("{bucket}: [[test]] is missing `id`")))
        .to_owned()
}

/// Build the shell setup prefix (compile/prepare the guest) and return the
/// guest invocation string. Identical contract to `manifest-to-commands.rs`.
fn setup_prefix(test: &Value, id: &str) -> (String, String, String) {
    let cell = format!("ignored/e2e-commands/work/{}", slug(id));
    let mut commands = vec![
        format!("cell={}", shell_quote(&cell)),
        "hermit_bin=${HERMIT_BIN:-target/release/hermit}".to_owned(),
        "run_verify_strict=; if run_help=$(\"$hermit_bin\" run --help 2>&1); then case \"$run_help\" in *--verify-strict*) run_verify_strict=--verify-strict;; esac; fi".to_owned(),
        "record_verify_strict=; if record_help=$(\"$hermit_bin\" record start --help 2>&1); then case \"$record_help\" in *--verify-strict*) record_verify_strict=--verify-strict;; esac; fi".to_owned(),
        "mkdir -p \"$cell/home\" \"$cell/xdg-config\" \"$cell/tmp\" \"$cell/fixtures\" \"$cell/captures\""
            .to_owned(),
        "if [ -d tests/e2e/xdg-config ]; then cp -a tests/e2e/xdg-config/. \"$cell/xdg-config/\"; fi"
            .to_owned(),
    ];

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct");
    let (host_guest, lockdown_guest) = match (program, direct) {
        (Some(_), Some(_)) => fail(format!("{id}: set only one of `program` and `direct`")),
        (None, None) => fail(format!("{id}: missing `program` or `direct`")),
        (None, Some(Value::String(command))) => {
            let guest = format!("sh -c {}", shell_quote(command));
            (guest.clone(), guest)
        }
        (None, Some(Value::Array(_))) => {
            let argv = string_array(direct, &format!("{id}.direct"));
            if argv.is_empty() {
                fail(format!("{id}: direct argv must not be empty"));
            }
            let host = argv.iter().map(|argument| shell_quote(argument)).collect::<Vec<_>>();
            let lockdown = argv
                .iter()
                .map(|argument| render_guest_argument(argument))
                .collect::<Vec<_>>();
            (host.join(" "), lockdown.join(" "))
        }
        (None, Some(_)) => fail(format!(
            "{id}: direct must be a shell command string or an argv array"
        )),
        (Some(program), None) => match Path::new(program).extension().and_then(|x| x.to_str()) {
            Some("sh") => {
                let script = shell_quote(program);
                commands.push(format!("{RUN_ENV} {script} --prepare"));
                (
                    format!("{script} --run"),
                    format!("\"$(pwd)\"/{script} --run"),
                )
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
                (
                    "\"$cell/guest\"".to_owned(),
                    "\"$(pwd)/$cell/guest\"".to_owned(),
                )
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
                (
                    "\"$cell/guest\"".to_owned(),
                    "\"$(pwd)/$cell/guest\"".to_owned(),
                )
            }
            other => fail(format!("{id}: unsupported program extension {other:?}")),
        },
    };

    (commands.join(" && "), host_guest, lockdown_guest)
}

fn prepared_fixture_paths(test: &Value, id: &str, native: bool) -> Vec<String> {
    let fixtures = string_array(
        test.get("prepared_fixture_args"),
        &format!("{id}.prepared_fixture_args"),
    );
    if !fixtures.is_empty()
        && !test
            .get("program")
            .and_then(Value::as_str)
            .is_some_and(|program| program.ends_with(".sh"))
    {
        fail(format!(
            "{id}: prepared_fixture_args requires a shell program with a --prepare phase"
        ));
    }
    let mut seen = BTreeSet::new();
    let mut rendered = Vec::with_capacity(fixtures.len());
    for path in fixtures {
        let path = Path::new(&path);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            fail(format!(
                "{id}.prepared_fixture_args must contain relative paths without `..`"
            ));
        }
        if !seen.insert(path.to_path_buf()) {
            fail(format!(
                "{id}: duplicate prepared fixture argument: {}",
                path.display()
            ));
        }
        rendered.push(if native {
            format!(
                "\"$cell/fixtures\"/{}",
                shell_quote(&path.to_string_lossy())
            )
        } else {
            shell_quote(&format!("{GUEST_FIXTURE_ROOT}/{}", path.display()))
        });
    }
    rendered
}

fn guest_with_prepared_fixtures(guest: &str, fixtures: &[String]) -> String {
    if fixtures.is_empty() {
        guest.to_owned()
    } else {
        format!("{guest} {}", fixtures.join(" "))
    }
}

fn expected_scheduled_worker_capacity(test: &Value, id: &str) -> Option<i64> {
    test.get("expected_scheduled_worker_capacity").map(|value| {
        value
            .as_integer()
            .filter(|capacity| *capacity > 0)
            .unwrap_or_else(|| {
                fail(format!(
                    "{id}.expected_scheduled_worker_capacity must be a positive integer"
                ))
            })
    })
}

fn guest_with_scheduled_worker_capacity(guest: &str, capacity: Option<i64>) -> String {
    capacity.map_or_else(|| guest.to_owned(), |capacity| format!("{guest} {capacity}"))
}

fn mode_guest_args(spec: &Value, mode: &str, backend: &str, id: &str) -> Vec<String> {
    let Some(by_backend) = spec.get("guest_args") else {
        return Vec::new();
    };
    let by_backend = by_backend
        .as_table()
        .unwrap_or_else(|| fail(format!("{id}.modes.{mode}.guest_args must be a table")));
    string_array(
        by_backend.get(backend),
        &format!("{id}.modes.{mode}.guest_args.{backend}"),
    )
}

fn ci_selected(spec: &Value, mode: &str, backend: &str, id: &str) -> bool {
    let Some(ci) = spec.get("ci") else {
        return false;
    };
    if let Some(selected) = ci.as_bool() {
        return selected;
    }
    let by_backend = ci
        .as_table()
        .unwrap_or_else(|| fail(format!("{id}.modes.{mode}.ci must be a boolean or table")));
    by_backend
        .get(backend)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn cell_required(spec: &Value, mode: &str, backend: &str, id: &str) -> bool {
    mode_backends(spec, mode, id)
        .iter()
        .any(|enabled| enabled == backend)
        && ci_selected(spec, mode, backend, id)
}

fn guest_with_args(guest: &str, guest_args: &[String], resolve_checkout: bool) -> String {
    if guest_args.is_empty() {
        return guest.to_owned();
    }
    let rendered = guest_args
        .iter()
        .map(|argument| {
            if resolve_checkout {
                render_guest_argument(argument)
            } else {
                shell_quote(argument)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{guest} {rendered}")
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
    verify_bitwise_parity: bool,
    log: &str,
    extra: &[String],
    required: bool,
    prepared_fixtures: bool,
    guest: &str,
) -> String {
    let _lane = lane;
    let profile: Vec<String> = Vec::new();
    let be = shell_quote(backend);
    let lockdown = if !required {
        ""
    } else if prepared_fixtures {
        " --mount=type=bind,source=\"$(pwd)/$cell/fixtures\",target=/tmp/hermit-e2e-fixtures,readonly --mount=type=tmpfs,target=/test --workdir=/test"
    } else {
        " --mount=type=tmpfs,target=/test --workdir=/test"
    };
    let legacy_guest_env = if required {
        String::new()
    } else {
        format!(" {HERMIT_GUEST_ENV_ARGS}")
    };
    let run_extra_joined = {
        let mut all: Vec<String> = profile;
        all.extend(extra.iter().map(|x| shell_quote(x)));
        let joined = all.join(" ");
        if joined.is_empty() {
            String::new()
        } else {
            format!(" {joined}")
        }
    };
    let extra_joined = if extra.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            extra
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let command = match mode {
        "verify" => {
            let _verify_bitwise_parity = verify_bitwise_parity;
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run --base-env=minimal --backend {be} --strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\"{run_extra_joined}{legacy_guest_env}{lockdown} -- {guest}"
            )
        }
        "replay" => {
            if required && prepared_fixtures {
                fail("prepared_fixture_args are not supported by record/replay");
            }
            let base_env = if required { " --base-env=minimal" } else { "" };
            let workdir = if required {
                " --mount=type=tmpfs,target=/test --workdir=/test"
            } else {
                ""
            };
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log {log} --backend {be} record start{base_env} --strict $record_verify_strict --verify --verify-json \"$cell/captures/verify.json\" --data-dir \"$cell/recording\" --record-timeout {timeout}{workdir}{legacy_guest_env}{extra_joined} -- {guest}"
            )
        }
        "chaos" => {
            let seed = seed.unwrap_or_else(|| {
                fail("internal error: chaos command construction requires a declared seed")
            });
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run --base-env=minimal --backend {be} --strict $run_verify_strict --verify --verify-allow=both --verify-json \"$cell/captures/verify-seed-{seed}.json\" --chaos --sched-heuristic=random --seed={seed}{run_extra_joined}{legacy_guest_env}{lockdown} -- {guest}"
            )
        }
        "custom" => {
            let mut args = mode_args.to_vec();
            args.extend(extra.iter().cloned());
            let margs = args
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ");
            let sep = if margs.is_empty() { "" } else { " " };
            let base_env = if required { " --base-env=minimal" } else { "" };
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run{base_env} --backend {be}{sep}{margs}{lockdown} -- {guest}"
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    let command = if required {
        format!("./ci/run-with-ephemeral-test-root.sh -- {command}")
    } else {
        command
    };
    format!("timeout --kill-after=10s {timeout}s {command}")
}

/// Default `--log` level per mode, matching the CI expansion.
fn default_log(_mode: &str) -> &'static str {
    "info"
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
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
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
            .unwrap_or_else(|e| fail(format!("{}: invalid YAML: {e}", path.display())));
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

fn modes_table<'a>(test: &'a Value, id: &str) -> &'a std::collections::BTreeMap<String, Value> {
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

// ---- argument parsing --------------------------------------------------------

/// Split argv into flags (--k v / --k=v), positionals, and a passthrough tail
/// after a literal `--`.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
    passthrough: Vec<String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut passthrough = Vec::new();
    let mut iter = argv.iter().peekable();
    let mut after_dashdash = false;
    while let Some(arg) = iter.next() {
        if after_dashdash {
            passthrough.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                flags.push((k.to_owned(), Some(v.to_owned())));
            } else {
                // consume a following value unless the next token is a flag
                let takes_value = iter
                    .peek()
                    .map(|n| !n.starts_with("--") && *n != "--")
                    .unwrap_or(false);
                if takes_value {
                    flags.push((rest.to_owned(), Some(iter.next().unwrap().clone())));
                } else {
                    flags.push((rest.to_owned(), None));
                }
            }
        } else {
            positional.push(arg.clone());
        }
    }
    Args {
        positional,
        flags,
        passthrough,
    }
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
    let (setup, host_guest, lockdown_guest) = setup_prefix(test, id);
    let native_prepared = prepared_fixture_paths(test, id, true);
    let guest_prepared = prepared_fixture_paths(test, id, false);
    let scheduled_capacity = expected_scheduled_worker_capacity(test, id);
    let has_prepared_fixtures = !guest_prepared.is_empty();
    let modes = modes_table(test, id);
    let spec = &modes[&mode];
    let required = mode != "naked" && cell_required(spec, &mode, &backend, id);
    let guest_args = mode_guest_args(spec, &mode, &backend, id);
    let log = args
        .flag("log")
        .map(str::to_owned)
        .unwrap_or_else(|| default_log(&mode).to_owned());
    let mode_args = if mode == "custom" {
        string_array(spec.get("args"), &format!("{id}.modes.custom.args"))
    } else {
        Vec::new()
    };
    let verify_bitwise_parity = if mode == "verify" {
        spec.get("assert")
            .and_then(Value::as_table)
            .and_then(|assert| assert.get("bitwise_parity"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    } else {
        false
    };
    let seed = if mode == "chaos" {
        Some(first_chaos_seed(spec, id).unwrap_or_else(|error| fail(error)))
    } else {
        None
    };
    let run = if mode == "naked" {
        let guest = guest_with_prepared_fixtures(&host_guest, &native_prepared);
        let guest = guest_with_scheduled_worker_capacity(&guest, scheduled_capacity);
        let guest = guest_with_args(&guest, &guest_args, false);
        format!("timeout --kill-after=10s {timeout}s {RUN_ENV} {guest}")
    } else {
        let guest = if required {
            guest_with_prepared_fixtures(&lockdown_guest, &guest_prepared)
        } else {
            guest_with_prepared_fixtures(&host_guest, &native_prepared)
        };
        let guest = guest_with_scheduled_worker_capacity(&guest, scheduled_capacity);
        let guest = guest_with_args(&guest, &guest_args, required);
        hermit_command(
            &mode,
            &backend,
            &lane,
            timeout,
            seed,
            &mode_args,
            verify_bitwise_parity,
            &log,
            &args.passthrough,
            required,
            has_prepared_fixtures,
            &guest,
        )
    };
    let full = format!("{setup} && {run}");
    (full, mode, backend)
}

fn self_test() -> ExitCode {
    let replay = hermit_command(
        "replay",
        "ptrace",
        "portable",
        60,
        None,
        &[],
        false,
        "info",
        &[],
        true,
        false,
        "guest",
    );
    assert!(replay.contains("--data-dir \"$cell/recording\" --record-timeout 60"));
    assert!(replay.contains("--strict $record_verify_strict --verify"));
    assert!(replay.contains("--verify-json \"$cell/captures/verify.json\""));
    assert!(replay.contains("record start --base-env=minimal"));
    assert!(replay.contains(
        "--mount=type=tmpfs,target=/test --workdir=/test -- guest"
    ));
    assert!(replay.contains("./ci/run-with-ephemeral-test-root.sh -- env LC_ALL=C"));
    assert!(!replay.contains("--env"));
    assert!(!replay.contains("--no-virtualize-cpuid"));

    let chaos = hermit_command(
        "chaos",
        "ptrace",
        "portable",
        60,
        Some(7),
        &[],
        false,
        "info",
        &[],
        true,
        false,
        "guest",
    );
    assert!(chaos.contains("run --base-env=minimal"));
    assert!(chaos.contains("--verify --verify-allow=both"));
    assert!(chaos.contains("--strict $run_verify_strict --verify"));
    assert!(chaos.contains("--verify-json \"$cell/captures/verify-seed-7.json\""));
    assert!(chaos.contains("--log=info"));
    assert!(!chaos.contains("--no-virtualize-cpuid"));
    assert!(!chaos.contains("--max-timeslice=disabled"));
    assert!(chaos.contains("--mount=type=tmpfs,target=/test --workdir=/test"));
    let seeded_chaos: Value = "seeds: [7, 9]".parse().unwrap();
    let no_seed_chaos = Value::Table(Default::default());
    assert_eq!(first_chaos_seed(&seeded_chaos, "fixture").unwrap(), 7);
    assert!(
        first_chaos_seed(&no_seed_chaos, "fixture")
            .unwrap_err()
            .contains("declares no seeds")
    );

    let custom = hermit_command(
        "custom",
        "ptrace",
        "portable",
        60,
        None,
        &["--summary".to_owned()],
        false,
        "info",
        &[],
        true,
        false,
        "guest",
    );
    assert!(custom.contains("run --base-env=minimal --backend ptrace --summary"));
    assert!(custom.contains("--mount=type=tmpfs,target=/test --workdir=/test -- guest"));
    assert!(!custom.contains("--strict"));
    assert!(!custom.contains("--no-virtualize-cpuid"));

    let verify = hermit_command(
        "verify",
        "ptrace",
        "portable",
        60,
        None,
        &[],
        true,
        "info",
        &[],
        true,
        false,
        "guest",
    );
    assert!(verify.contains("run --base-env=minimal"));
    assert!(verify.contains(
        "--strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\""
    ));
    assert!(!verify.contains("--no-virtualize-cpuid"));
    assert!(!verify.contains("--max-timeslice=disabled"));
    assert!(verify.contains("--mount=type=tmpfs,target=/test --workdir=/test"));
    let weak_verify = hermit_command(
        "verify",
        "ptrace",
        "portable",
        60,
        None,
        &[],
        false,
        "info",
        &[],
        true,
        false,
        "guest",
    );
    assert!(weak_verify.contains("--verify --verify-json \"$cell/captures/verify.json\""));
    assert!(weak_verify.contains("--strict $run_verify_strict --verify"));
    let manual_verify = hermit_command(
        "verify",
        "liteinst",
        "portable",
        60,
        None,
        &[],
        false,
        "info",
        &[],
        false,
        false,
        "./examples/example.py",
    );
    assert!(manual_verify.contains("--env LC_ALL=C"));
    assert!(!manual_verify.contains("--workdir=/test"));
    assert!(!manual_verify.contains("target=/test"));
    assert!(!manual_verify.contains("run-with-ephemeral-test-root.sh"));
    assert!(manual_verify.ends_with("-- ./examples/example.py"));

    let selection: Value = r#"
ci:
  ptrace: true
  liteinst: false
backends_enabled: [ptrace, liteinst]
"#
    .parse()
    .unwrap();
    assert!(cell_required(
        &selection,
        "verify",
        "ptrace",
        "fixture"
    ));
    assert!(!cell_required(
        &selection,
        "verify",
        "liteinst",
        "fixture"
    ));
    let uniform: Value = "ci: true\nbackends_enabled: [ptrace]".parse().unwrap();
    assert!(!cell_required(&uniform, "verify", "kvm", "fixture"));
    assert_eq!(render_guest_argument("./README.md"), "\"$(pwd)\"/README.md");
    assert_eq!(render_guest_argument("README.md"), "README.md");
    let prepared: Value = r#"
program: tests/e2e/prepared.sh
prepared_fixture_args: [fork-tree, nested/pipe-chain]
"#
    .parse()
    .unwrap();
    assert_eq!(
        prepared_fixture_paths(&prepared, "fixture", false),
        vec![
            "/tmp/hermit-e2e-fixtures/fork-tree",
            "/tmp/hermit-e2e-fixtures/nested/pipe-chain",
        ]
    );
    let width: Value = "expected_scheduled_worker_capacity: 1".parse().unwrap();
    assert_eq!(expected_scheduled_worker_capacity(&width, "fixture"), Some(1));
    assert_eq!(
        guest_with_scheduled_worker_capacity("guest --run", Some(1)),
        "guest --run 1"
    );
    println!("manifest-cli self-test: PASS");
    ExitCode::SUCCESS
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
                let mut sub = parse_args(&[]);
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

fn usage() -> ! {
    eprintln!(
        "manifest-cli — front-door to the e2e manifest corpus

USAGE:
  manifest-cli list [--bucket B] [--backend BE] [--tag T] [--mode M] [--lane L] [--verbose]
  manifest-cli get  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [--all-modes]
  manifest-cli run  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [-- <extra hermit flags>]

FILTERS (list):
  --bucket   manifest bucket (e.g. system-utils, c-programs)
  --backend  a backend enabled in some mode (ptrace, dbt, kvm, sabre, liteinst, native)
  --tag      a `requires` capability token (e.g. python3, bash, kvm, cpuid)
  --mode     verify | naked | replay | chaos | custom
  --lane     portable | privileged
  --verbose  also print requires + per-mode backend breakdown

get/run:
  --mode/--backend/--lane pick the cell (defaults: verify mode, first enabled backend, test lane)
  --log      override the --log= level (info|debug|trace|off); default info for every mode
  --all-modes (get only) print every enabled (mode,backend) command
  -- <flags> (run only) extra hermit flags injected before the `-- <guest>` separator
  A chaos mode without declared seeds is unavailable; get/run refuse rather than invent seed 0.
  Required cells run through ci/run-with-ephemeral-test-root.sh, whose only privileged operations
  create and remove the exact empty /test mountpoint; the rendered guest command stays unprivileged.

ENV:
  HERMIT_BIN  hermit binary for `run` (default target/release/hermit; a RELEASE binary is required)"
    );
    std::process::exit(2)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        usage();
    }
    let sub = argv[0].clone();
    if sub == "-h" || sub == "--help" || sub == "help" {
        usage();
    }
    if sub == "self-test" {
        return self_test();
    }
    let rest = &argv[1..];
    let args = parse_args(rest);
    let root = repo_root();
    let manifests = load_manifests(&root);
    match sub.as_str() {
        "list" => cmd_list(&manifests, &args),
        "get" => cmd_get(&manifests, &args),
        "run" => cmd_run(&manifests, &args, &root),
        other => {
            eprintln!("manifest-cli: unknown subcommand `{other}`\n");
            usage();
        }
    }
}
