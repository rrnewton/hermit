/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

static LITEINST_ADVANCED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPAT_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_SEMANTIC_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPRESSED_FIXTURES: OnceLock<[PathBuf; 3]> = OnceLock::new();

const COMPAT_FIXTURE_CONTENT: &[u8] = b"liteinst compatibility fixture\n";
const COMPAT_FIXTURE_SHA256: &str =
    "e5c4447a0a9f796a0b72bb47875e9879aa7722c74e601385e74058f029ae60cd";
const COMPAT_FIXTURE_SHA1: &str = "41396e2c2d5ce6332143190b04e78ba101db58f8";
const COMPAT_FIXTURE_SHA224: &str = "344c0ace4382f9d738db9a385af4435e493e876fdc334c21485917ba";
const COMPAT_FIXTURE_SHA384: &str = "38184361b2dbdee2b75d92506acf3ab1dba402eed33cc0691841d5b33521382e5752437b3cfa2232d2241ad6baaf5fa9";
const COMPAT_FIXTURE_SHA512: &str = "2c856cc937ac0a50cedf2a3d3d0a6c10570791ace2e3cd44374a1308844bc2acca0fd6e100b38306da9844c7248bebcca3fd46a1c2651b98d36f588126925078";
const COMPAT_FIXTURE_BLAKE2: &str = "d69629a852f326482ab1e50881d63a17028e3205b66a6a54d7d85c0cb9ceff149ba03c45585a6a94e1a1edd120fe50c44e9dfce62830ffac3460a57bde29c5aa";
const SEMANTIC_FIXTURE_CONTENT: &[u8] = b"gamma:3\nalpha:1\nalpha:1\nbeta:2\n";
const SEMANTIC_FIXTURE_MD5: &str = "c61c6cb65c4b5e1a6f3eb32b601db629";

fn group_name_by_gid<'a>(contents: &'a str, gid: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        fields.next()?;
        (fields.next()? == gid).then_some(name)
    })
}

fn advanced_guest() -> &'static Path {
    LITEINST_ADVANCED_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("liteinst_advanced");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/liteinst_advanced.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile LiteInst advanced guest");
        assert!(
            output.status.success(),
            "LiteInst advanced guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn mmap_guest() -> &'static Path {
    LITEINST_MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("mmap_determinism");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/mmap_determinism.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile LiteInst mmap guest");
        assert!(
            output.status.success(),
            "LiteInst mmap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn compatibility_fixture() -> &'static Path {
    LITEINST_COMPAT_FIXTURE.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let fixture = build_root.join("compatibility-fixture.txt");
        fs::write(&fixture, COMPAT_FIXTURE_CONTENT).expect("failed to write LiteInst fixture");
        fixture
    })
}

fn semantic_fixture() -> &'static Path {
    LITEINST_SEMANTIC_FIXTURE.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let mut fixture = tempfile::Builder::new()
            .prefix("semantic-fixture-")
            .tempfile_in(build_root)
            .expect("failed to create LiteInst semantic fixture");
        fixture
            .write_all(SEMANTIC_FIXTURE_CONTENT)
            .expect("failed to write LiteInst semantic fixture");
        let (_file, path) = fixture
            .keep()
            .expect("failed to retain LiteInst semantic fixture");
        path
    })
}

fn compressed_fixtures() -> &'static [PathBuf; 3] {
    LITEINST_COMPRESSED_FIXTURES.get_or_init(|| {
        let source = compatibility_fixture();
        let build_root = source
            .parent()
            .expect("compatibility fixture should have a parent directory");
        [
            (
                "/usr/bin/gzip",
                &["-n", "-c"][..],
                "compatibility-fixture.gz",
            ),
            ("/usr/bin/bzip2", &["-c"][..], "compatibility-fixture.bz2"),
            ("/usr/bin/xz", &["-c"][..], "compatibility-fixture.xz"),
        ]
        .map(|(program, args, filename)| {
            let output = Command::new(program)
                .args(args)
                .arg(source)
                .output()
                .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
            assert!(
                output.status.success(),
                "{program} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let path = build_root.join(filename);
            fs::write(&path, output.stdout).expect("failed to write compressed fixture");
            path
        })
    })
}

fn run_liteinst(program: &Path, args: &[&str], verify: bool) -> Output {
    run_liteinst_with_input(program, args, verify, None, None)
}

fn run_liteinst_with_input(
    program: &Path,
    args: &[&str],
    verify: bool,
    input: Option<&[u8]>,
    // Per-run witness nonce (`WITNESS_TOKEN_ENV`) exported to the hermit
    // *supervisor* only. Reverie stamps it into the skid-overshoot marker;
    // hermit strips it from the guest environment, so guest code can neither
    // read nor forge it. `None` disables authenticated skid detection.
    witness_token: Option<&str>,
) -> Output {
    liteinst_runtime::ensure_liteinst_runtime();
    let home = tempfile::tempdir().expect("failed to create isolated LiteInst HOME");
    let xdg_config_home = home.path().join(".config");
    fs::create_dir_all(&xdg_config_home).expect("failed to create isolated XDG config directory");
    let mut command = Command::new(liteinst_runtime::hermit_binary());
    command.args(["--log=info", "run", "--backend", "liteinst", "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command
        .arg(format!("--env=HOME={}", home.path().display()))
        .arg(format!(
            "--env=XDG_CONFIG_HOME={}",
            xdg_config_home.display()
        ))
        .env("HOME", home.path());
    if let Some(token) = witness_token {
        // Supervisor-only: hermit strips WITNESS_TOKEN_ENV from the guest env.
        command.env(WITNESS_TOKEN_ENV, token);
    }
    command.arg("--").arg(program).args(args);
    let Some(input) = input else {
        return command.output().expect("failed to run Hermit LiteInst");
    };

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run Hermit LiteInst with stdin");
    child
        .stdin
        .take()
        .expect("LiteInst stdin pipe should exist")
        .write_all(input)
        .expect("failed to write LiteInst stdin");
    child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst output")
}

fn assert_liteinst_strict_verify(program: &Path, args: &[&str], expected_stdout: &[u8]) {
    let output = run_liteinst_strict_verify(program, args);
    assert_eq!(output.stdout, expected_stdout);
}

fn assert_liteinst_virtual_time_is_continuous() {
    const EPOCH_SECONDS: u64 = 1_767_225_600;
    const MAX_STARTUP_SECONDS: u64 = 60;

    // Whole seconds remain stable across verified LiteInst runs. Do not assert
    // the old exact epoch: that encoded #1095's reset-on-exec behavior and
    // rejects legitimate deterministic startup progress.
    let output = run_liteinst_strict_verify(Path::new("/usr/bin/date"), &["-u", "+%s"]);
    let timestamp = String::from_utf8(output.stdout).expect("date output should be UTF-8");
    let seconds = timestamp
        .trim()
        .parse::<u64>()
        .expect("date seconds should be numeric");

    assert!(
        seconds >= EPOCH_SECONDS,
        "guest time preceded the configured epoch: {timestamp}"
    );
    assert!(
        seconds < EPOCH_SECONDS + MAX_STARTUP_SECONDS,
        "guest startup consumed an implausible amount of virtual time: {timestamp}"
    );
    // Verify continuous progression independently of the startup offset.
    assert_liteinst_strict_verify(
        advanced_guest(),
        &["clock-progress"],
        b"clock-progress-ok\n",
    );
}

#[test]
fn liteinst_strict_verify_heap_growth_avoids_trampoline_mappings() {
    let output = run_liteinst_strict_verify(mmap_guest(), &["heap"]);
    assert!(
        output.stdout.starts_with(b"heap "),
        "heap-growth guest omitted its success marker: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Maximum number of skid-gated attempts for a single strict-verify run before
/// failing loud. Kept small: a genuine skid overshoot is rare, so more than a
/// couple in a row means the skid tail is systematically exceeding the margin on
/// this host — which is itself a defect worth surfacing, not papering over.
const MAX_SKID_ATTEMPTS: u32 = 3;

/// Env var carrying this harness's per-run witness nonce into reverie's
/// skid-overshoot marker. Set on the hermit *supervisor* only; hermit strips it
/// from the guest environment, so the nonce is unknowable and unforgeable by
/// guest code. A marker is trusted iff it carries `witness=<this exact nonce>`.
const WITNESS_TOKEN_ENV: &str = "HERMIT_SKID_WITNESS_TOKEN";

/// Substring identifying the `--strict --verify` divergence failure class. A
/// genuine RCB skid overshoot manifests *only* as this class (the single-step
/// interrupt overshot the target, so the two runs' schedules diverge). Any other
/// failure that happens to carry a coincident marker is a *different* bug and
/// must never be retried — that is the "real-failure-plus-marker co-occurrence"
/// hazard the reviewers named.
const VERIFY_DIVERGENCE_MARKER: &str = "Mismatch between run 1 and run 2";

/// Optional JSONL ledger path (one object per retry and per exhaustion). Defect
/// (b): successful retries are invisible in CI because the test passes and
/// nextest's `success-output = "never"` (and plain `cargo test`) suppress output.
/// When set, every retry is durably recorded so `validate.sh` can surface the
/// per-run retry count even on a green run.
const RETRY_LEDGER_ENV: &str = "HERMIT_SKID_RETRY_LEDGER";

/// Generate a unique, unguessable per-run witness nonce. It need not be
/// cryptographically strong — only unknowable to the guest, which is guaranteed
/// structurally by hermit stripping [`WITNESS_TOKEN_ENV`] from the guest env.
fn witness_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wit-{}-{}-{}", std::process::id(), seq, nanos)
}

/// Split a candidate marker line into its `key=value` fields, or `None` if the
/// canonical marker token is absent.
fn parse_marker_fields(line: &str) -> Option<std::collections::HashMap<&str, &str>> {
    let (_, rest) = line.split_once(reverie_ptrace::SKID_OVERSHOOT_MARKER)?;
    let mut fields = std::collections::HashMap::new();
    for tok in rest.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            fields.insert(k, v);
        }
    }
    Some(fields)
}

/// True iff `line` is a supervisor-authenticated, strictly-parsed skid-overshoot
/// record: it carries the canonical marker, a `witness` field exactly equal to
/// `expected_token` (which the guest cannot know), and well-formed positive
/// numeric overshoot fields. This is the defect-(a) fix: origin authentication
/// plus strict field parsing replaces the old spoofable `stderr.contains`.
fn is_authenticated_overshoot(line: &str, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return false;
    }
    let Some(fields) = parse_marker_fields(line) else {
        return false;
    };
    if fields.get("witness").copied() != Some(expected_token) {
        return false;
    }
    let overshoot_positive =
        matches!(fields.get("overshoot").and_then(|v| v.parse::<u64>().ok()), Some(n) if n > 0);
    overshoot_positive
        && fields
            .get("rcb_actual")
            .and_then(|v| v.parse::<u64>().ok())
            .is_some()
        && fields
            .get("rcb_target")
            .and_then(|v| v.parse::<u64>().ok())
            .is_some()
}

/// The one retry predicate. Returns true iff a failed run should be retried. A
/// retry is authorized only when ALL of the following hold, closing the two
/// review-blocking defects in the original substring gate:
///
///  1. the run failed;
///  2. the failure is the `--strict --verify` divergence class
///     ([`VERIFY_DIVERGENCE_MARKER`]) — causal binding to the skid symptom, so a
///     different real bug carrying a coincident marker is never retried; and
///  3. the supervisor emitted at least one authenticated, strictly-parsed
///     overshoot record ([`is_authenticated_overshoot`]) carrying this run's
///     witness nonce — so guest-emitted marker text (which lacks the nonce) can
///     never trigger a retry.
///
/// An unmarked divergence is a real determinism bug and returns `false`, so the
/// caller's assertion surfaces it immediately: a retry can never launder a real
/// bug into a green result.
fn is_retryable_skid_failure(success: bool, stderr: &str, expected_token: &str) -> bool {
    if success {
        return false;
    }
    if !stderr.contains(VERIFY_DIVERGENCE_MARKER) {
        return false;
    }
    stderr
        .lines()
        .any(|l| is_authenticated_overshoot(l, expected_token))
}

/// Append one JSONL record to the retry ledger if [`RETRY_LEDGER_ENV`] is set.
/// Defect (b): makes an otherwise-invisible retry durable for CI surfacing.
fn record_retry_ledger(attempt: u32, max: u32, overshoot_lines: &[&str]) {
    let Ok(path) = std::env::var(RETRY_LEDGER_ENV) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    // Handwritten JSON keeps this test harness free of a serde dependency.
    let markers = overshoot_lines.join(" | ").replace('\\', "").replace('"', "'");
    let exhausted = attempt >= max;
    let line = format!(
        "{{\"event\":\"skid_retry\",\"attempt\":{attempt},\"max\":{max},\
         \"exhausted\":{exhausted},\"markers\":\"{markers}\"}}\n"
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Runs a strict-verify closure, retrying ONLY when the failure is a
/// supervisor-authenticated skid overshoot bound to this run's witness nonce.
/// Counts and reports every retry (ledger + stderr), caps at
/// [`MAX_SKID_ATTEMPTS`], and fails loud at the cap. The closure receives the
/// per-run witness nonce to export to the hermit supervisor.
fn run_with_skid_gated_retry(mut run: impl FnMut(&str) -> Output) -> Output {
    let token = witness_token();
    let mut attempt: u32 = 1;
    loop {
        let output = run(&token);
        let success = output.status.success();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        // A pass, or any failure that is NOT an authenticated skid overshoot,
        // returns immediately. Only the authenticated-skid failure retries.
        if !is_retryable_skid_failure(success, &stderr, &token) {
            return output;
        }

        let overshoot_lines: Vec<&str> = stderr
            .lines()
            .filter(|l| is_authenticated_overshoot(l, &token))
            .collect();

        // Visibility (defect b): record BEFORE the cap assert so an exhaustion is
        // logged too, not just the retries that preceded it.
        record_retry_ledger(attempt, MAX_SKID_ATTEMPTS, &overshoot_lines);

        assert!(
            attempt < MAX_SKID_ATTEMPTS,
            "SKID-RETRY EXHAUSTED after {attempt} attempts: every attempt overshot the RCB \
             skid margin (authenticated marker present each time). This is not a transient — the \
             skid tail is systematically exceeding the margin on this host. Overshoot markers:\n{}",
            overshoot_lines.join("\n"),
        );

        // Count + report every retry so N-per-run is a legible defect signal,
        // not a silent papering-over.
        eprintln!(
            "SKID-RETRY attempt={attempt}/{MAX_SKID_ATTEMPTS}: strict-verify run overshot the \
             skid margin ({} authenticated marker line(s)); retrying. Markers:\n{}",
            overshoot_lines.len(),
            overshoot_lines.join("\n"),
        );
        attempt += 1;
    }
}

/// Build a canonical, authenticated marker line for tests.
#[cfg(test)]
fn test_marker_line(token: &str, overshoot: u64) -> String {
    format!(
        "{} rcb_actual={} rcb_target=33000 skid_margin=128 overshoot={overshoot} witness={token}",
        reverie_ptrace::SKID_OVERSHOOT_MARKER,
        33000 + overshoot,
    )
}

#[test]
fn authenticated_divergence_overshoot_retries() {
    // The one case that retries: divergence class + authenticated overshoot.
    let tok = "wit-good-1";
    let stderr = format!(
        "{VERIFY_DIVERGENCE_MARKER} outputs (logs retained).\n{}\n",
        test_marker_line(tok, 500)
    );
    assert!(is_retryable_skid_failure(false, &stderr, tok));
}

/// Task 3 NEGATIVE TEST for defect (a): a guest that prints the canonical marker
/// text without the witness nonce (which it cannot know) must NOT trigger a
/// retry, even alongside a genuine divergence line. This is the guest-spoofing
/// case the substring gate failed.
#[test]
fn guest_spoofed_marker_without_nonce_does_not_retry() {
    let tok = "wit-secret-2";
    // Guest prints the bare marker (no `witness=` field at all).
    let bare = format!(
        "{VERIFY_DIVERGENCE_MARKER} outputs (logs retained).\n{} rcb_actual=33500 rcb_target=33000 overshoot=500\n",
        reverie_ptrace::SKID_OVERSHOOT_MARKER
    );
    assert!(!is_retryable_skid_failure(false, &bare, tok));
    // Guest guesses the field shape but supplies a WRONG nonce.
    let wrong = format!(
        "{VERIFY_DIVERGENCE_MARKER} outputs (logs retained).\n{}\n",
        test_marker_line("wit-WRONG", 500)
    );
    assert!(!is_retryable_skid_failure(false, &wrong, tok));
}

/// Defect-(a) causal binding: an authenticated overshoot marker on a
/// NON-divergence failure (a different real bug that merely coincides with a
/// marker) must NOT retry.
#[test]
fn authenticated_marker_on_nondivergence_failure_does_not_retry() {
    let tok = "wit-xyz-3";
    let stderr = format!(
        "thread 'main' panicked: index out of bounds\n{}\n",
        test_marker_line(tok, 500)
    );
    assert!(!is_retryable_skid_failure(false, &stderr, tok));
}

/// A real determinism bug: divergence, no marker at all => never retry.
#[test]
fn unmarked_divergence_never_retries() {
    assert!(!is_retryable_skid_failure(
        false,
        "Mismatch between run 1 and run 2 outputs (logs retained).",
        "wit-4"
    ));
}

/// A passing run never retries, authenticated marker present or not.
#[test]
fn success_never_retries() {
    let tok = "wit-5";
    let stderr = format!(
        "{VERIFY_DIVERGENCE_MARKER}\n{}\n",
        test_marker_line(tok, 500)
    );
    assert!(!is_retryable_skid_failure(true, &stderr, tok));
    assert!(!is_retryable_skid_failure(true, "Determinism verified.", tok));
}

/// Strict parse: a malformed marker (non-numeric or zero overshoot) is rejected
/// even with the correct nonce and a divergence line.
#[test]
fn malformed_marker_fields_rejected() {
    let tok = "wit-6";
    let non_numeric = format!(
        "{VERIFY_DIVERGENCE_MARKER}\n{} rcb_actual=x rcb_target=y overshoot=notnum witness={tok}\n",
        reverie_ptrace::SKID_OVERSHOOT_MARKER
    );
    assert!(!is_retryable_skid_failure(false, &non_numeric, tok));
    let zero = format!(
        "{VERIFY_DIVERGENCE_MARKER}\n{}\n",
        test_marker_line(tok, 0)
    );
    assert!(!is_retryable_skid_failure(false, &zero, tok));
}

/// An empty witness token (authentication disabled) never authenticates any
/// marker — no accidental retry when the mechanism is off.
#[test]
fn empty_token_never_authenticates() {
    let stderr = format!("{VERIFY_DIVERGENCE_MARKER}\n{}\n", test_marker_line("", 500));
    assert!(!is_retryable_skid_failure(false, &stderr, ""));
}

/// F3 fix: exercise `run_with_skid_gated_retry` end-to-end with a fake runner
/// that fails once with an authenticated overshoot then succeeds. Confirms the
/// retry fires exactly once, the token is threaded to the closure, and the final
/// result is the success.
#[test]
fn harness_retries_authenticated_overshoot_then_succeeds() {
    use std::cell::Cell;
    use std::os::unix::process::ExitStatusExt;
    let calls = Cell::new(0u32);
    let out = run_with_skid_gated_retry(|token| {
        let n = calls.get() + 1;
        calls.set(n);
        if n < 2 {
            let stderr = format!(
                "{VERIFY_DIVERGENCE_MARKER} outputs (logs retained).\n{}\n",
                test_marker_line(token, 7)
            );
            Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: stderr.into_bytes(),
            }
        } else {
            Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
            }
        }
    });
    assert!(out.status.success());
    assert_eq!(calls.get(), 2, "expected exactly one retry then success");
}

/// F3 fix: a runner that overshoots on every attempt exhausts the cap and fails
/// loud rather than papering over a systematic skid.
#[test]
#[should_panic(expected = "SKID-RETRY EXHAUSTED")]
fn harness_exhausts_after_cap() {
    use std::os::unix::process::ExitStatusExt;
    run_with_skid_gated_retry(|token| {
        let stderr = format!(
            "{VERIFY_DIVERGENCE_MARKER} outputs (logs retained).\n{}\n",
            test_marker_line(token, 7)
        );
        Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: stderr.into_bytes(),
        }
    });
}

fn run_liteinst_strict_verify(program: &Path, args: &[&str]) -> Output {
    assert_liteinst_strict_verify_output(run_with_skid_gated_retry(|token| {
        run_liteinst_with_input(program, args, true, None, Some(token))
    }))
}

fn run_liteinst_strict_verify_with_stdin(program: &Path, args: &[&str], input: &[u8]) -> Output {
    assert_liteinst_strict_verify_output(run_with_skid_gated_retry(|token| {
        run_liteinst_with_input(program, args, true, Some(input), Some(token))
    }))
}

fn assert_liteinst_strict_verify_output(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
        ),
        "{stderr}"
    );
    let perf_supported = reverie_ptrace::is_perf_supported();
    assert_eq!(
        stderr.contains("perf_event_open is unavailable; continuing with --max-timeslice=disabled"),
        !perf_supported,
        "perf_supported={perf_supported}\n{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "LiteInst host hybrid (reverie-liteinst patch runtime + ptrace Detcore Tool)"
        ),
        "{stderr}"
    );
    output
}

#[test]
fn liteinst_detcore_strict_verify_micro_suite() {
    assert_liteinst_strict_verify(Path::new("/bin/true"), &[], b"");
    assert_liteinst_strict_verify(Path::new("/bin/echo"), &["hello"], b"hello\n");

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let readme = repository.join("README.md");
    let expected = fs::read(&readme).expect("read README fixture");
    assert_liteinst_strict_verify(
        Path::new("/bin/cat"),
        &[readme.to_str().unwrap()],
        &expected,
    );
}

#[test]
fn liteinst_strict_verify_identity_utilities() {
    assert_liteinst_strict_verify(Path::new("/usr/bin/uname"), &["-s"], b"Linux\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/id"), &["-u"], b"0\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/whoami"), &[], b"root\n");
}

#[test]
fn liteinst_strict_verify_virtual_identity_and_time() {
    assert_liteinst_virtual_time_is_continuous();
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/hostname"),
        &[],
        b"hermetic-container.local\n",
    );
    let group_file = fs::read_to_string("/etc/group").expect("failed to read host group database");
    let root_group = group_name_by_gid(&group_file, "0").expect("GID 0 should have a name");
    let overflow_group = group_name_by_gid(&group_file, "65534").unwrap_or("nobody");
    let expected_groups = format!("{root_group} {overflow_group}\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/groups"),
        &[],
        expected_groups.as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_file_and_text_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/printf"),
        &["liteinst-printf-ok\n"],
        b"liteinst-printf-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/grep"),
        &["^liteinst", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/head"),
        &["-n", "1", fixture],
        COMPAT_FIXTURE_CONTENT,
    );

    let expected_wc = format!("{} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/wc"),
        &["-c", fixture],
        expected_wc.as_bytes(),
    );
    let expected_sha256 = format!("{COMPAT_FIXTURE_SHA256}  {fixture}\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sha256sum"),
        &[fixture],
        expected_sha256.as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/stat"),
        &["-c", "%s", fixture],
        format!("{}\n", COMPAT_FIXTURE_CONTENT.len()).as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_semantic_text_utilities() {
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/tail"),
        &["-n", "2", fixture],
        b"alpha:1\nbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/uniq"),
        &[fixture],
        b"gamma:3\nalpha:1\nbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/cut"),
        &["-d", ":", "-f", "1", fixture],
        b"gamma\nalpha\nalpha\nbeta\n",
    );
    assert_liteinst_strict_verify(Path::new("/usr/bin/diff"), &[fixture, fixture], b"");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sed"),
        &["-n", "2,3p", fixture],
        b"alpha:1\nalpha:1\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sort"),
        &[fixture],
        b"alpha:1\nalpha:1\nbeta:2\ngamma:3\n",
    );
}

#[test]
fn liteinst_strict_verify_semantic_file_and_sqlite_utilities() {
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/find"),
        &[fixture, "-maxdepth", "0", "-type", "f", "-print"],
        format!("{fixture}\n").as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/md5sum"),
        &[fixture],
        format!("{SEMANTIC_FIXTURE_MD5}  {fixture}\n").as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/du"),
        &["-b", fixture],
        format!("{}\t{fixture}\n", SEMANTIC_FIXTURE_CONTENT.len()).as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sqlite3"),
        &[
            ":memory:",
            "CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); \
             SELECT v FROM t ORDER BY v;",
        ],
        b"1\n2\n3\n",
    );
}

#[test]
fn liteinst_strict_verify_encoding_and_digest_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/base64"),
        &["--wrap=0", fixture],
        b"bGl0ZWluc3QgY29tcGF0aWJpbGl0eSBmaXh0dXJlCg==",
    );
    for (program, digest) in [
        ("/usr/bin/sha1sum", COMPAT_FIXTURE_SHA1),
        ("/usr/bin/sha224sum", COMPAT_FIXTURE_SHA224),
        ("/usr/bin/sha384sum", COMPAT_FIXTURE_SHA384),
        ("/usr/bin/sha512sum", COMPAT_FIXTURE_SHA512),
        ("/usr/bin/b2sum", COMPAT_FIXTURE_BLAKE2),
    ] {
        let expected = format!("{digest}  {fixture}\n");
        assert_liteinst_strict_verify(Path::new(program), &[fixture], expected.as_bytes());
    }
    let expected_cksum = format!("2216041199 {} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/cksum"),
        &[fixture],
        expected_cksum.as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_formatting_and_sequence_utilities() {
    let compat_fixture = compatibility_fixture();
    let compat_fixture = compat_fixture
        .to_str()
        .expect("fixture path should be UTF-8");
    let semantic_fixture = semantic_fixture();
    let semantic_fixture = semantic_fixture
        .to_str()
        .expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(Path::new("/usr/bin/seq"), &["5"], b"1\n2\n3\n4\n5\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/fmt"),
        &["--width=10", compat_fixture],
        b"liteinst\ncompatibility\nfixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/fold"),
        &["--width=8", compat_fixture],
        b"liteinst\n compati\nbility f\nixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/nl"),
        &["-ba", semantic_fixture],
        b"     1\tgamma:3\n     2\talpha:1\n     3\talpha:1\n     4\tbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/tac"),
        &[semantic_fixture],
        b"beta:2\nalpha:1\nalpha:1\ngamma:3\n",
    );
}

#[test]
fn liteinst_strict_verify_round2_encoding_and_comparison_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/base32"),
        &["--wrap=0", fixture],
        b"NRUXIZLJNZZXIIDDN5WXAYLUNFRGS3DJOR4SAZTJPB2HK4TFBI======",
    );
    let sum_output = run_liteinst_strict_verify(Path::new("/usr/bin/sum"), &[fixture]);
    let sum_stdout = String::from_utf8(sum_output.stdout).expect("sum output should be UTF-8");
    let sum_fields = sum_stdout.split_whitespace().collect::<Vec<_>>();
    match sum_fields.as_slice() {
        ["04458", "1"] => {}
        ["04458", "1", output_path] => assert_eq!(*output_path, fixture),
        _ => panic!("unexpected sum output: {sum_stdout:?}"),
    }
    assert_liteinst_strict_verify(Path::new("/usr/bin/cmp"), &[fixture, fixture], b"");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/comm"),
        &[fixture, fixture],
        b"\t\tliteinst compatibility fixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/join"),
        &[fixture, fixture],
        b"liteinst compatibility fixture compatibility fixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/paste"),
        &[fixture, fixture],
        b"liteinst compatibility fixture\tliteinst compatibility fixture\n",
    );
}

#[test]
fn liteinst_strict_verify_round2_representation_and_path_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/od"),
        &["-An", "-tx1", fixture],
        b" 6c 69 74 65 69 6e 73 74 20 63 6f 6d 70 61 74 69\n 62 69 6c 69 74 79 20 66 69 78 74 75 72 65 0a\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/pr"),
        &["-t", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/readlink"),
        &["-f", "/etc/../etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/rev"),
        &[fixture],
        b"erutxif ytilibitapmoc tsnietil\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/strings"),
        &[fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    let dd_input = format!("if={fixture}");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/dd"),
        &[&dd_input, "bs=7", "count=2", "status=none"],
        b"liteinst compa",
    );
}

#[test]
fn liteinst_strict_verify_round2_arithmetic_and_predicate_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(Path::new("/usr/bin/factor"), &["84"], b"84: 2 2 3 7\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/expr"), &["6", "*", "7"], b"42\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/numfmt"),
        &["--to=iec", "1024"],
        b"1.0K\n",
    );
    assert_liteinst_strict_verify(Path::new("/usr/bin/test"), &["-f", fixture], b"");
    assert_liteinst_strict_verify(Path::new("/usr/bin/pathchk"), &[fixture], b"");
}

#[test]
fn liteinst_strict_verify_round3_portable_system_utilities() {
    assert_liteinst_strict_verify(Path::new("/usr/bin/arch"), &[], b"x86_64\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/getconf"), &["LONG_BIT"], b"64\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/getopt"),
        &["-o", "ab:", "--", "-a", "-b", "value", "rest"],
        b" -a -b 'value' -- 'rest'\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/bin/bash"),
        &[
            "--noprofile",
            "--norc",
            "-c",
            "printf 'liteinst-bash-ok\\n'",
        ],
        b"liteinst-bash-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/jq"),
        &["-nr", "[3,1,2] | sort | join(\",\")"],
        b"1,2,3\n",
    );

    let existing_directory = compatibility_fixture()
        .parent()
        .expect("compatibility fixture should have a parent directory");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/mkdir"),
        &[
            "-p",
            existing_directory.to_str().expect("path should be UTF-8"),
        ],
        b"",
    );
}

#[test]
fn liteinst_strict_verify_round3_encoding_and_compression_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/iconv"),
        &["-f", "UTF-8", "-t", "UTF-16LE", fixture],
        b"l\0i\0t\0e\0i\0n\0s\0t\0 \0c\0o\0m\0p\0a\0t\0i\0b\0i\0l\0i\0t\0y\0 \0f\0i\0x\0t\0u\0r\0e\0\n\0",
    );

    let [gzip_fixture, bzip2_fixture, xz_fixture] = compressed_fixtures();
    for (program, compressed_fixture) in [
        ("/usr/bin/gzip", gzip_fixture),
        ("/usr/bin/bzip2", bzip2_fixture),
        ("/usr/bin/xz", xz_fixture),
    ] {
        assert_liteinst_strict_verify(
            Path::new(program),
            &[
                "-cd",
                compressed_fixture
                    .to_str()
                    .expect("compressed fixture path should be UTF-8"),
            ],
            COMPAT_FIXTURE_CONTENT,
        );
    }
}

#[test]
fn liteinst_strict_verify_round3_stdin_filter_utilities() {
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/tr"),
        &["a-z", "A-Z"],
        b"gamma\nalpha\nbeta\n",
    );
    assert_eq!(output.stdout, b"GAMMA\nALPHA\nBETA\n");

    let output =
        run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/tee"), &[], b"liteinst-tee-ok\n");
    assert_eq!(output.stdout, b"liteinst-tee-ok\n");

    let output =
        run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/tsort"), &[], b"a b\nb c\n");
    assert_eq!(output.stdout, b"a\nb\nc\n");
}

#[test]
fn liteinst_strict_verify_path_and_language_utilities() {
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/basename"),
        &["/tmp/hermit-example.txt", ".txt"],
        b"hermit-example\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/dirname"),
        &["/tmp/hermit-example.txt"],
        b"/tmp\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/realpath"),
        &["/etc/../etc/passwd"],
        b"/etc/passwd\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/ls"),
        &["-1", "/etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/awk"),
        &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
        b"55\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/perl"),
        &["-e", r#"print join(q{,}, map { $_ * $_ } 1..5), qq{\n}"#],
        b"1,4,9,16,25\n",
    );
}

#[test]
fn liteinst_strict_verify_shell_and_entropy_consumer() {
    assert_liteinst_strict_verify(
        Path::new("/bin/sh"),
        &["-c", "printf 'liteinst-shell-ok\\n'"],
        b"liteinst-shell-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/hexdump"),
        &["/dev/urandom", "--length", "16"],
        b"0000000 7229 04bb 964d 28df ba71 4c03 de95 7027\n0000010\n",
    );
}

#[test]
fn liteinst_strict_verify_python_entropy() {
    let output = run_liteinst_strict_verify(
        Path::new("/usr/bin/python3"),
        &[
            "-c",
            "import os; print(os.getpid(), len(os.urandom(8)), os.urandom(8).hex())",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("Python output should be UTF-8");
    let fields = stdout.split_whitespace().collect::<Vec<_>>();
    assert_eq!(fields.len(), 3, "stdout={stdout:?}");
    assert_eq!(fields[0], "3", "stdout={stdout:?}");
    assert_eq!(fields[1], "8", "stdout={stdout:?}");
    assert_eq!(fields[2].len(), 16, "stdout={stdout:?}");
    assert!(
        fields[2].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "stdout={stdout:?}"
    );
}

#[test]
fn liteinst_strict_verify_python_random_example() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = run_liteinst_strict_verify(&repository.join("examples/rand.py"), &[]);
    let stdout = String::from_utf8(output.stdout).expect("Python output should be UTF-8");
    let values = stdout
        .split_whitespace()
        .map(|field| field.parse::<u8>().expect("random value should be decimal"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 10, "stdout={stdout:?}");
    assert!(
        values.iter().all(|value| (1..=101).contains(value)),
        "stdout={stdout:?}"
    );
}

fn assert_clone_boundary(mode: &str) {
    liteinst_runtime::ensure_liteinst_runtime();
    let mut child = Command::new(liteinst_runtime::hermit_binary())
        .args([
            "--log=error",
            "run",
            "--backend",
            "liteinst",
            "--strict",
            "--",
        ])
        .arg(advanced_guest())
        .arg(mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start Hermit LiteInst clone-boundary guest");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung while rejecting {mode}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst clone-boundary output");
    assert_eq!(output.status, status);
    assert_eq!(
        status.code(),
        Some(1),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ENOTSUPP (Operation is not supported)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Bad system call"), "{stderr}");
}

/// Path to the freshly built `libreverie_liteinst.so` preload runtime.
///
/// [`liteinst_runtime::ensure_liteinst_runtime`] builds it beside the Hermit
/// test binary, so it lives in the same profile directory.
fn liteinst_runtime_library() -> PathBuf {
    liteinst_runtime::ensure_liteinst_runtime();
    liteinst_runtime::liteinst_runtime_library()
}

/// A bare preload must not create a second in-guest Detcore Tool.
///
/// Host mode is selected only by `run_host_with_preload`. Without that private
/// selector, even a stale legacy coordinator variable must leave the patch DSO
/// inert and let the program run normally.
#[test]
fn liteinst_preload_is_inert_without_host_runtime_selector() {
    let runtime = liteinst_runtime_library();
    assert!(
        runtime.is_file(),
        "expected LiteInst preload runtime at {}",
        runtime.display(),
    );

    let output = Command::new("/bin/true")
        .env(
            reverie_liteinst::COORDINATOR_ENV,
            "/definitely/not/a/coordinator.sock",
        )
        .env("LD_PRELOAD", &runtime)
        .output()
        .expect("failed to launch /bin/true under the LiteInst preload");

    assert_eq!(
        output.status.code(),
        Some(0),
        "bare patch preload must remain inert\nstatus={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("reverie-liteinst initialization failed"),
        "bare preload attempted to install an in-guest Detcore Tool: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn liteinst_thread_clone_fails_closed_without_sigsys() {
    assert_clone_boundary("threads");
}

#[test]
fn liteinst_fork_fails_closed_without_hanging() {
    assert_clone_boundary("fork");
}

#[test]
fn liteinst_abnormal_exit_after_registration_does_not_hang() {
    liteinst_runtime::ensure_liteinst_runtime();
    // INFO-level Detcore diagnostics can exceed a pipe's capacity before the
    // guest reaches its fatal signal. Keep draining out of the child process
    // while retaining the diagnostics for the scheduler-start assertion.
    let mut stderr = tempfile::tempfile().expect("create LiteInst diagnostic sink");
    let stderr_sink = stderr.try_clone().expect("clone LiteInst diagnostic sink");
    let mut child = Command::new(liteinst_runtime::hermit_binary())
        .args([
            "--log",
            "info",
            "run",
            "--backend",
            "liteinst",
            "--strict",
            "--base-env=minimal",
            "--no-namespace",
            "--",
            "/bin/sh",
            "-c",
            "kill -9 $$",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_sink))
        .spawn()
        .expect("failed to start Hermit LiteInst fatal-exit guest");
    let deadline = Instant::now() + Duration::from_secs(5);

    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung after a registered guest exited by signal");
        }
        thread::sleep(Duration::from_millis(10));
    };

    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst output");
    stderr.rewind().expect("rewind LiteInst diagnostic sink");
    let mut diagnostics = String::new();
    stderr
        .read_to_string(&mut diagnostics)
        .expect("read LiteInst diagnostics");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{output:?}");
    assert_eq!(output.status, status);
    assert!(
        diagnostics.contains("[scheduler] guest in queue"),
        "stderr={diagnostics}",
    );
}
