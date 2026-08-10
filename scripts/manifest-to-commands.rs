#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Expand the schema-v2 e2e manifests into runnable command files.
//!
//! Run from anywhere inside the checkout:
//!
//! ```text
//! ./scripts/manifest-to-commands.rs
//! ```
//!
//! Each `ignored/e2e-commands/<bucket>.txt` file contains one self-contained
//! shell command per enabled `(test, mode, backend)` cell. Chaos seeds expand
//! to separate lines. Commands compile implicit C/Rust guests and prepare shell
//! wrappers before invoking Hermit, so any individual line can be rerun from
//! the repository root.
//!
//! ```cargo
//! [dependencies]
//! toml = "0.8"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use toml::Value;

const MANIFEST_SCHEMA: i64 = 2;
const RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=\"$cell/tmp\" E2E_FIXTURE_DIR=\"$cell/fixtures\"";

fn fail(message: impl AsRef<str>) -> ! {
    eprintln!("manifest-to-commands: {}", message.as_ref());
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

/// Per-backend guest arguments declared by `modes.<mode>.guest_args.<backend>`.
///
/// These are arguments for the **guest**, not for Hermit, so they go after the
/// `--` separator. They are per-backend because a manifest may qualify one
/// backend on a cheaper scenario than another.
///
/// A guest that requires an argument and is not given one prints usage and
/// exits non-zero. Before this channel existed, every consumer of the manifests
/// invoked such a guest bare, and the resulting non-zero exit was recorded as a
/// determinism failure — see rrnewton/hermit#1815.
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

/// Append the guest's own arguments to an already-quoted guest word.
fn guest_with_args(guest: &str, guest_args: &[String]) -> String {
    if guest_args.is_empty() {
        return guest.to_owned();
    }
    let rendered = guest_args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{guest} {rendered}")
}

/// The normalized verify contract for one cell, as produced by
/// `hermit-manifest-plan --format verify-contracts`.
///
/// THIS SCRIPT DERIVES NONE OF THIS. A guest whose deterministic outcome is a
/// crash or a deliberate non-zero exit needs `--verify-allow both` (otherwise
/// `--verify` refuses the second run and compares nothing), `--verify-strict`
/// (the canonical comparator the acceptance rests on), and `--verify-json` (the
/// only artifact carrying guest-termination provenance). Choosing that set is
/// policy, and policy re-implemented in three places drifts even while the
/// copies agree, so the plan crate owns it and this script only transports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VerifyContract {
    shell_status: i64,
    extra_args: Vec<String>,
    require_record: String,
}

impl VerifyContract {
    fn declared(&self) -> bool {
        !self.extra_args.is_empty()
    }
}

/// Parse the plan crate's TSV. Kept separate from the subprocess call so the
/// wire format is testable without a build.
fn parse_verify_contracts(tsv: &str) -> BTreeMap<String, VerifyContract> {
    let mut contracts = BTreeMap::new();
    for line in tsv.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            fail(format!(
                "hermit-manifest-plan --format verify-contracts emitted {} field(s), expected 4: {line:?}",
                fields.len()
            ));
        }
        let shell_status = fields[1].parse::<i64>().unwrap_or_else(|_| {
            fail(format!("verify-contracts shell status is not an integer: {line:?}"))
        });
        contracts.insert(
            fields[0].to_owned(),
            VerifyContract {
                shell_status,
                extra_args: fields[2]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                require_record: fields[3].to_owned(),
            },
        );
    }
    contracts
}

/// Ask the plan crate for the contracts. Failing loudly matters: silently
/// treating every cell as undeclared would regenerate commands that reproduce a
/// NO_RESULT instead of the cell, which is exactly the bug this channel exists
/// to prevent.
fn load_verify_contracts(root: &Path) -> BTreeMap<String, VerifyContract> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "verify-contracts",
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| {
            fail(format!("cannot run hermit-manifest-plan: {error}"))
        });
    if !output.status.success() {
        fail(format!(
            "hermit-manifest-plan --format verify-contracts failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_verify_contracts(&String::from_utf8_lossy(&output.stdout))
}

fn hermit_command(
    mode: &str,
    backend: &str,
    lane: &str,
    timeout: i64,
    seed: Option<i64>,
    extra: &[String],
    guest: &str,
    contract: &VerifyContract,
) -> String {
    let portable = lane == "portable";
    let profile = if portable {
        " --no-virtualize-cpuid --max-timeslice=disabled"
    } else {
        ""
    };
    // Transported verbatim from hermit-manifest-plan, plus the record path this
    // script owns (the plan crate cannot know where a reproduction writes it).
    let mut declared = String::new();
    if contract.declared() {
        for argument in &contract.extra_args {
            declared.push(' ');
            declared.push_str(&shell_quote(argument));
        }
        declared.push_str(" --verify-json \"$cell/verify.json\"");
    }
    let command = match mode {
        "verify" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=info run --backend {} --strict --verify{declared}{profile} -- {guest}",
            shell_quote(backend)
        ),
        "replay" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=info --backend {} record start --strict --verify -- {guest}",
            shell_quote(backend)
        ),
        "chaos" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=off run --backend {} --strict --chaos --sched-heuristic=random --seed={}{profile} -- {guest}",
            shell_quote(backend),
            seed.unwrap_or(0)
        ),
        "custom" => {
            let extra = extra
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ");
            let separator = if extra.is_empty() { "" } else { " " };
            format!(
                "{RUN_ENV} \"$hermit_bin\" --log=info run --backend {} --strict{separator}{extra} -- {guest}",
                shell_quote(backend)
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    format!("timeout --kill-after=10s {timeout}s {command}")
}

fn repeat(command: &str, count: i64) -> String {
    if count <= 1 {
        return command.to_owned();
    }
    let iterations = (1..=count)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!("for _run in {iterations}; do {command} || exit; done")
}

fn commands_for_test(
    test: &Value,
    bucket: &str,
    contracts: &BTreeMap<String, VerifyContract>,
) -> Vec<String> {
    let id = test_id(test, bucket);
    let lane = test
        .get("lane")
        .and_then(Value::as_str)
        .unwrap_or("portable");
    let timeout = test
        .get("timeout_seconds")
        .and_then(Value::as_integer)
        .unwrap_or(60);
    let modes = test
        .get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| fail(format!("{id}: missing `modes`")));
    let (setup, guest) = setup_prefix(test, &id);
    let mut mode_names = modes.keys().map(String::as_str).collect::<Vec<_>>();
    mode_names.sort_unstable();
    let mut lines = Vec::new();

    for mode in mode_names {
        let spec = &modes[mode];
        if mode == "naked" {
            let runs = spec.get("runs").and_then(Value::as_integer).unwrap_or(3);
            let run = format!("timeout --kill-after=10s {timeout}s {RUN_ENV} {guest}");
            lines.push(format!(
                "{setup} && {} # {id} mode=naked backend=native",
                repeat(&run, runs)
            ));
            continue;
        }

        let backends = string_array(
            spec.get("backends_enabled"),
            &format!("{id}.modes.{mode}.backends_enabled"),
        );
        if backends.is_empty() {
            // Schema v2 explicitly allows a mode with zero enabled backends
            // (`ci = false` plus a reason per disabled backend), and
            // hermit-manifest-plan accepts it. Aborting here made the whole
            // generator unusable: on the tree this comment was written, it died
            // on the FIRST manifest at applications/timed-progress-bar's
            // single-threaded chaos gap. There is simply no command to emit.
            continue;
        }
        let extra = string_array(spec.get("args"), &format!("{id}.modes.{mode}.args"));
        // `args` are Hermit's; `guest_args` are the guest's and are per-backend,
        // so they are resolved inside the backend loop below.
        let assert = spec.get("assert").and_then(Value::as_table);
        let custom_runs = assert
            .and_then(|a| a.get("runs"))
            .and_then(Value::as_integer)
            .unwrap_or(1);
        let seeds = if mode == "chaos" {
            let seeds = integer_array(spec.get("seeds"), &format!("{id}.modes.chaos.seeds"));
            if seeds.is_empty() { vec![0, 1] } else { seeds }
        } else {
            vec![0]
        };

        // Per-MODE, unlike guest_args: the guest's termination is a property of
        // the program under the deterministic schedule, not of the backend that
        // hosts it. Only `verify` has a contract; every other mode exits 0.
        let contract = if mode == "verify" {
            contracts.get(&id).cloned().unwrap_or_default()
        } else {
            VerifyContract::default()
        };

        for backend in backends {
            let guest_args = mode_guest_args(spec, mode, &backend, &id);
            let guest = guest_with_args(&guest, &guest_args);
            for seed in &seeds {
                let seed = (mode == "chaos").then_some(*seed);
                let command = hermit_command(
                    mode, &backend, lane, timeout, seed, &extra, &guest, &contract,
                );
                let runs = match mode {
                    "chaos" => 2,
                    "custom" => custom_runs,
                    _ => 1,
                };
                let seed_note = seed.map(|s| format!(" seed={s}")).unwrap_or_default();
                // A declaring cell EXITS non-zero on success, so the note has
                // to say so or a reader (or a shell `|| exit`) reads the
                // reproduction as a failure -- and the record, not the status,
                // is what certifies it.
                let status_note = if contract.declared() {
                    format!(
                        " expect_shell_status={} require_record={}",
                        contract.shell_status, contract.require_record
                    )
                } else {
                    String::new()
                };
                lines.push(format!(
                    "{setup} && {} # {id} mode={mode} backend={backend}{seed_note}{status_note}",
                    repeat(&command, runs)
                ));
            }
        }
    }
    lines
}

/// Emit every declared per-backend guest-argument vector as TSV on stdout:
/// `<test-id>\t<mode>\t<backend>\t<arg>...`, sorted, one line per cell.
///
/// This is the machine-readable form of the same `guest_args` the generated
/// commands embed, so an out-of-tree harness (the `compat-envelope` corpus
/// collector) can invoke a guest correctly without maintaining a second copy of
/// the argument list, which would drift. Cells with no declared arguments are
/// omitted rather than emitted empty, so a consumer can distinguish "declared
/// nothing" from "not in the manifests" only by the test id's absence.
fn guest_args_tsv(tests: &[(String, Value)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (bucket, test) in tests {
        let id = test_id(test, bucket);
        let Some(modes) = test.get("modes").and_then(Value::as_table) else {
            continue;
        };
        let mut mode_names = modes.keys().map(String::as_str).collect::<Vec<_>>();
        mode_names.sort_unstable();
        for mode in mode_names {
            let spec = &modes[mode];
            let backends = match spec.get("backends_enabled") {
                Some(value) => string_array(
                    Some(value),
                    &format!("{id}.modes.{mode}.backends_enabled"),
                ),
                None => continue,
            };
            for backend in backends {
                let args = mode_guest_args(spec, mode, &backend, &id);
                if args.is_empty() {
                    continue;
                }
                lines.push(format!("{id}\t{mode}\t{backend}\t{}", args.join("\t")));
            }
        }
    }
    lines.sort();
    lines
}

// TODO-HUMAN-REVIEW(PR-1081): Review the manifest-to-command CLI and generated shell contract.
const USAGE: &str = "\
Usage: manifest-to-commands.rs [-h|--help] [--guest-args]

Regenerate the flattened e2e command files under ignored/e2e-commands/ from the
TOML manifests in tests/e2e/manifests/. It discovers the repo root from git and
rewrites the generated *.txt files in place.

  --guest-args  Write nothing; print the declared per-backend guest arguments as
                TSV (`<test-id> <mode> <backend> <arg>...`) on stdout instead.

Verify contracts (the flags and required --verify-json record for a guest that
deterministically crashes or exits non-zero) are NOT derived here; they come
from `cargo run -p hermit-manifest-plan -- --format verify-contracts`, which is
also the channel an out-of-tree harness should read.";

/// Parse every manifest under `manifests`, validating schema and bucket naming,
/// and return `(bucket, test)` pairs in file order.
fn load_manifest_tests(manifests: &Path) -> Vec<(String, Value)> {
    let mut paths = fs::read_dir(manifests)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", manifests.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut collected = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| {
                fail(format!(
                    "manifest has a non-UTF-8 file name: {}",
                    path.display()
                ))
            });
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
            .unwrap_or_else(|| fail(format!("{}: missing `bucket`", path.display())));
        if bucket != stem {
            fail(format!(
                "{}: bucket `{bucket}` must match file stem `{stem}`",
                path.display()
            ));
        }
        let tests = manifest
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: missing [[test]] entries", path.display())));
        for test in tests {
            collected.push((bucket.to_owned(), test.clone()));
        }
    }
    collected
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let root = repo_root();
    let manifests = root.join("tests/e2e/manifests");
    if std::env::args().skip(1).any(|a| a == "--guest-args") {
        for line in guest_args_tsv(&load_manifest_tests(&manifests)) {
            println!("{line}");
        }
        return ExitCode::SUCCESS;
    }
    let output = root.join("ignored/e2e-commands");
    fs::create_dir_all(&output)
        .unwrap_or_else(|e| fail(format!("cannot create {}: {e}", output.display())));
    for entry in fs::read_dir(&output)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", output.display())))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "txt") {
            fs::remove_file(&path)
                .unwrap_or_else(|e| fail(format!("cannot remove stale {}: {e}", path.display())));
        }
    }

    let contracts = load_verify_contracts(&root);
    let mut by_bucket: Vec<(String, Vec<String>)> = Vec::new();
    for (bucket, test) in load_manifest_tests(&manifests) {
        let lines = commands_for_test(&test, &bucket, &contracts);
        match by_bucket.iter_mut().find(|(name, _)| name == &bucket) {
            Some((_, existing)) => existing.extend(lines),
            None => by_bucket.push((bucket, lines)),
        }
    }

    let mut files = 0usize;
    let mut commands = 0usize;
    for (bucket, lines) in by_bucket {
        let destination = output.join(format!("{bucket}.txt"));
        let body = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        fs::write(&destination, body)
            .unwrap_or_else(|e| fail(format!("cannot write {}: {e}", destination.display())));
        println!(
            "{}: {} commands",
            destination
                .strip_prefix(&root)
                .unwrap_or(&destination)
                .display(),
            lines.len()
        );
        files += 1;
        commands += lines.len();
    }

    println!("generated {commands} commands across {files} bucket files");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> Vec<(String, Value)> {
        let value: Value = body.parse().expect("test manifest must parse");
        value
            .get("test")
            .and_then(Value::as_array)
            .expect("test manifest needs [[test]]")
            .iter()
            .map(|test| ("c-programs".to_owned(), test.clone()))
            .collect()
    }

    const DECLARED: &str = r#"
[[test]]
id = "c-programs/example"
program = "tests/c/example.c"
[test.modes.verify]
backends_enabled = ["ptrace", "liteinst"]
guest_args = { ptrace = ["multi", "value with spaces"], liteinst = ["edge"] }
"#;

    /// POSITIVE side: a declared argument vector must reach the guest word, and
    /// must be quoted, so a scenario name containing a space stays one argument.
    #[test]
    fn declared_guest_args_are_appended_and_quoted() {
        let tests = manifest(DECLARED);
        let spec = &tests[0].1["modes"]["verify"];
        let args = mode_guest_args(spec, "verify", "ptrace", "c-programs/example");
        assert_eq!(args, vec!["multi", "value with spaces"]);
        assert_eq!(
            guest_with_args("\"$cell/guest\"", &args),
            "\"$cell/guest\" multi 'value with spaces'"
        );
    }

    /// The channel is per-BACKEND, not per-cell: two backends of the same cell
    /// must be able to disagree. A per-cell channel would silently hand one
    /// backend the other's scenario.
    #[test]
    fn guest_args_are_resolved_per_backend() {
        let tests = manifest(DECLARED);
        let spec = &tests[0].1["modes"]["verify"];
        assert_eq!(
            mode_guest_args(spec, "verify", "liteinst", "c-programs/example"),
            vec!["edge"]
        );
        assert_ne!(
            mode_guest_args(spec, "verify", "ptrace", "c-programs/example"),
            mode_guest_args(spec, "verify", "liteinst", "c-programs/example")
        );
    }

    /// NEGATIVE side: a cell that declares nothing must gain nothing. If this
    /// flips, every argument-less guest starts receiving a stray argument.
    #[test]
    fn undeclared_guest_args_leave_the_guest_word_untouched() {
        let tests = manifest(
            r#"
[[test]]
id = "c-programs/bare"
program = "tests/c/bare.c"
[test.modes.verify]
backends_enabled = ["ptrace"]
"#,
        );
        let spec = &tests[0].1["modes"]["verify"];
        let args = mode_guest_args(spec, "verify", "ptrace", "c-programs/bare");
        assert!(args.is_empty());
        assert_eq!(guest_with_args("\"$cell/guest\"", &args), "\"$cell/guest\"");
    }

    /// A backend that is enabled but not named in `guest_args` gets nothing,
    /// rather than inheriting a sibling backend's arguments.
    #[test]
    fn enabled_backend_absent_from_guest_args_gets_none() {
        let tests = manifest(
            r#"
[[test]]
id = "c-programs/partial"
program = "tests/c/partial.c"
[test.modes.verify]
backends_enabled = ["ptrace", "kvm"]
guest_args = { ptrace = ["multi"] }
"#,
        );
        let spec = &tests[0].1["modes"]["verify"];
        assert!(mode_guest_args(spec, "verify", "kvm", "c-programs/partial").is_empty());
    }

    /// The TSV dump is the out-of-tree harness's only source for these
    /// arguments, so its shape is a contract: one line per (id, mode, backend)
    /// that declares arguments, sorted, tab-separated, and cells declaring
    /// nothing omitted entirely.
    #[test]
    fn guest_args_tsv_emits_one_sorted_line_per_declaring_cell() {
        let mut tests = manifest(DECLARED);
        tests.extend(manifest(
            r#"
[[test]]
id = "c-programs/bare"
program = "tests/c/bare.c"
[test.modes.verify]
backends_enabled = ["ptrace"]
"#,
        ));
        let lines = guest_args_tsv(&tests);
        assert_eq!(
            lines,
            vec![
                "c-programs/example\tverify\tliteinst\tedge",
                "c-programs/example\tverify\tptrace\tmulti\tvalue with spaces",
            ]
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("c-programs/bare")),
            "a cell declaring no guest_args must not appear in the dump"
        );
    }

    const CONTRACT_TSV: &str = "c-programs/crasher\t139\t--verify-allow both --verify-strict\t{\"bitwise_parity\":true,\"guest_exit_code\":null,\"guest_signal\":11,\"verdict\":\"matched\",\"verified\":true}\n";

    /// POSITIVE side: the plan crate's contract must reach the generated line
    /// intact — the allowance, the canonical comparator, AND the record path.
    /// Missing any one makes the reproduction measure something weaker than the
    /// cell: without the allowance a NO_RESULT, without `--verify-strict` only
    /// the stripped comparator, without the record no guest provenance at all.
    #[test]
    fn declared_contract_reaches_the_generated_command() {
        let contracts = parse_verify_contracts(CONTRACT_TSV);
        let contract = contracts
            .get("c-programs/crasher")
            .expect("crasher contract");
        assert_eq!(contract.shell_status, 139);
        assert!(contract.declared());
        let command = hermit_command(
            "verify",
            "ptrace",
            "portable",
            60,
            None,
            &[],
            "\"$cell/guest\"",
            contract,
        );
        assert!(
            command.contains(
                "--verify --verify-allow both --verify-strict --verify-json \"$cell/verify.json\" \
                 --no-virtualize-cpuid"
            ),
            "expected the full contract next to --verify, got: {command}"
        );
    }

    /// NEGATIVE side: an undeclared cell must keep the historical command
    /// exactly. If any of it leaked in unconditionally, every cell would stop
    /// enforcing the success contract Hermit applies by default.
    #[test]
    fn undeclared_cell_leaves_the_command_untouched() {
        let contract = VerifyContract::default();
        assert!(!contract.declared());
        let command = hermit_command(
            "verify",
            "ptrace",
            "portable",
            60,
            None,
            &[],
            "\"$cell/guest\"",
            &contract,
        );
        assert!(!command.contains("--verify-allow"));
        assert!(!command.contains("--verify-strict"));
        assert!(!command.contains("--verify-json"));
    }

    /// This script transports the plan crate's policy and derives none of it,
    /// so the wire format is the contract between them. A field-count change
    /// must fail loudly rather than silently downgrade a declaring cell to the
    /// exit-0 default, which would regenerate a command that measures a
    /// NO_RESULT.
    #[test]
    fn contract_tsv_round_trips_every_field() {
        let contracts = parse_verify_contracts(CONTRACT_TSV);
        let contract = &contracts["c-programs/crasher"];
        assert_eq!(
            contract.extra_args,
            vec!["--verify-allow", "both", "--verify-strict"]
        );
        assert!(contract.require_record.contains("\"guest_signal\":11"));
        assert!(contract.require_record.contains("\"bitwise_parity\":true"));
        assert!(parse_verify_contracts("").is_empty());
    }
}
