/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

#[test]
fn mount_introspection_syscalls_fall_back_deterministically() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mount-introspection-{}", std::process::id()));
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("sysfs", repository.join("tests/c/sysfs_enosys.c")),
        ("statmount", repository.join("tests/c/statmount_enosys.c")),
        ("listmount", repository.join("tests/c/listmount_enosys.c")),
    ];

    for (syscall, source) in cases {
        let guest = build_root.join(format!("{syscall}_enosys"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest);
        command_output(compile, &format!("{syscall} guest compilation"));

        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let verify_output = command_output(verify, &format!("{syscall} strict verification"));
        let stdout = String::from_utf8_lossy(&verify_output.stdout);
        let stderr = String::from_utf8_lossy(&verify_output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{syscall} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// Compile tests/c/mountinfo_reread.c into this test's build directory.
fn mountinfo_reread_guest(build_root: &Path) -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    fs::create_dir_all(build_root).expect("failed to create guest build directory");
    let guest = build_root.join("mountinfo_reread");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/mountinfo_reread.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "mountinfo_reread guest compilation");
    guest
}

/// The guest's mounts carry no propagation tag (`master:N`, `shared:N`): the
/// container remounts its tree private before any other mount, so host mount
/// events cannot reach it. A mount namespace made inside a user namespace
/// starts as a slave of the host's; before, 156 of 158 rows carried
/// `master:N`, and a host mount during a run changed the guest's mount table
/// (https://github.com/rrnewton/hermit/issues/4167).
#[test]
fn guest_mounts_receive_no_host_propagation() {
    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--base-env=minimal",
            "--",
            "/bin/cat",
            "/proc/self/mountinfo",
        ]);
    let output = command_output(run, "guest mountinfo read");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.lines().count() > 1, "no mountinfo rows:\n{stdout}");
    let tagged: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            line.split(" - ")
                .next()
                .unwrap_or("")
                .split_whitespace()
                .any(|field| field.starts_with("master:") || field.starts_with("shared:"))
        })
        .collect();
    assert!(
        tagged.is_empty(),
        "guest mounts carry propagation tags:\n{}",
        tagged.join("\n")
    );
}

/// Host mounts made and removed during a run do not reach the guest. Hermit
/// runs inside a user and mount namespace whose mounts are shared, while a
/// loop mounts and unmounts a tmpfs there; the guest rereads its mountinfo
/// 500 times (within the default per-test CPU budget of 22 s). Before the container's private remount, the first read after a
/// mount event aborted the run ("mountinfo snapshot failed strict identity
/// validation"), which killed `go build` within minutes
/// (https://github.com/rrnewton/hermit/issues/4167).
#[test]
fn host_mount_events_during_a_run_leave_the_guest_mountinfo_unchanged() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mountinfo-propagation-{}", std::process::id()));
    let guest = mountinfo_reread_guest(&build_root);
    let mount_point = build_root.join("churn");
    fs::create_dir_all(&mount_point).expect("failed to create the churn mount point");
    let script = r#"
        churn_point=$1; shift
        ( while :; do
            mount -t tmpfs none "$churn_point" || exit 1
            sleep 0.02
            umount "$churn_point" || exit 1
            sleep 0.02
          done ) &
        churn=$!
        sleep 0.2
        kill -0 "$churn" || { echo "the mount churn did not start" >&2; exit 3; }
        "$@"
        status=$?
        kill "$churn"
        exit "$status"
    "#;
    let mut run = Command::new("unshare");
    run.args([
        "--user",
        "--map-root-user",
        "--mount",
        "--propagation",
        "shared",
        "sh",
        "-c",
        script,
        "sh",
    ])
    .arg(&mount_point)
    .args(["timeout", "--kill-after", "5s", "120s"])
    .arg(env!("CARGO_BIN_EXE_hermit"))
    .args(["run", "--strict", "--base-env=minimal", "--"])
    .arg(&guest)
    .arg("500");
    let output = command_output(run, "guest mountinfo rereads during host mount churn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "500 reads; lines constant\n");
}

/// Record a runtime skip for `test` (the pattern of
/// `hermit-cli/tests/cli.rs`'s `record_test_skip`): the marker goes to
/// stdout and, when the runner set `HERMIT_TEST_SKIP_FILE`, to that
/// sidecar, so the test is classified as a skip rather than a pass.
/// `require_env`, when set in the environment, makes the skip a failure.
fn record_test_skip(test: &str, reason: &str, require_env: &str) {
    assert!(
        std::env::var_os(require_env).is_none(),
        "skipping {test} ({reason}) but {require_env} is set"
    );
    println!("HERMIT-TEST-SKIP: {test}\t{reason}");
    let Some(path) = std::env::var_os("HERMIT_TEST_SKIP_FILE").map(PathBuf::from) else {
        return;
    };
    let line = format!("HERMIT-TEST-SKIP: {test}\t{reason}\n");
    assert!(
        line.len() < 4096,
        "skip marker for {test} is {} bytes; keep the reason under one PIPE_BUF line",
        line.len()
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("HERMIT_TEST_SKIP_FILE must name the runner's writable skip sidecar");
    let written = file
        .write(line.as_bytes())
        .expect("writing a skip marker to HERMIT_TEST_SKIP_FILE failed");
    assert_eq!(
        written,
        line.len(),
        "a skip marker must land in one write() so parallel appends cannot interleave"
    );
}

/// Compile a `tests/c/` program into `build_root` under `name`.
fn compile_c_guest(build_root: &Path, name: &str) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    fs::create_dir_all(build_root).expect("failed to create guest build directory");
    let guest = build_root.join(name);
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join(format!("tests/c/{name}.c")))
        .arg("-o")
        .arg(&guest);
    command_output(compile, &format!("{name} compilation"));
    guest
}

/// One of this host's autofs mountpoints, if it has any (from
/// `/proc/self/mountinfo`, paths unescaped).
fn host_autofs_root() -> Option<PathBuf> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in mountinfo.lines() {
        let (left, right) = line.split_once(" - ")?;
        if right.split_whitespace().next() != Some("autofs") {
            continue;
        }
        let mountpoint = left.split_whitespace().nth(4)?;
        let unescaped = mountpoint
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        return Some(PathBuf::from(unescaped));
    }
    None
}

/// A missing name directly under an autofs root answers ENOENT under
/// Hermit, exactly as natively. Before the container made its copied
/// autofs mounts unbindable, Linux 6.19's `autofs_d_automount` refused
/// the lookup from Hermit's recursively private container with EPERM.
#[test]
fn autofs_missing_name_is_enoent_under_hermit() {
    let Some(root) = host_autofs_root() else {
        record_test_skip(
            "autofs_missing_name_is_enoent_under_hermit",
            "host has no autofs mount",
            "HERMIT_REQUIRE_AUTOFS",
        );
        return;
    };
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("autofs-missing-{}", std::process::id()));
    let guest = compile_c_guest(&build_root, "autofs_probe");
    let missing = root.join(format!("hermit-nonexistent-{}-7f3a", std::process::id()));

    let mut native = Command::new(&guest);
    native.arg("stat").arg(&missing);
    let native = command_output(native, "native autofs missing-name stat");
    let native = String::from_utf8_lossy(&native.stdout);
    assert!(
        native.contains("errno=2"),
        "native lookup did not answer ENOENT (host changed?): {native}"
    );

    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--base-env=minimal", "--"])
        .arg(&guest)
        .arg("stat")
        .arg(&missing);
    let run = command_output(run, "hermit autofs missing-name stat");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("errno=2"),
        "missing name under {} did not answer ENOENT under Hermit:\n{stdout}",
        root.display()
    );
    assert!(
        !stdout.contains("errno=1"),
        "missing name still answers EPERM under Hermit:\n{stdout}"
    );
}

/// Privileged synthetic-autofs fixture (design G0): a stub v5 daemon
/// serves a real autofs mount in a root-owned private mount namespace,
/// and the test measures, in that working namespace, the trigger
/// primitives Hermit's pre-trigger holds rely on and Hermit's own
/// behavior for unlisted keys. Needs passwordless sudo (autofs has no
/// `FS_USERNS_MOUNT`, so an unprivileged user namespace cannot mount
/// it); belongs in the privileged lane and records a named skip
/// elsewhere.
#[test]
fn autofs_privileged_fixture_measures_triggers_and_eloop_notice() {
    let test = "autofs_privileged_fixture_measures_triggers_and_eloop_notice";
    let sudo_ok = Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|status| status.success());
    if !sudo_ok {
        record_test_skip(
            test,
            "needs passwordless sudo for the synthetic autofs fixture namespace",
            "HERMIT_REQUIRE_AUTOFS_FIXTURE",
        );
        return;
    }
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("autofs-fixture-{}", std::process::id()));
    let daemon = compile_c_guest(&build_root, "autofs_fixture_daemon");
    let probe = compile_c_guest(&build_root, "autofs_probe");
    let work = build_root.join("run");
    fs::create_dir_all(&work).expect("failed to create fixture work directory");
    // The driver runs as root inside the private mount namespace. It
    // asserts as it goes and prints AUTOFS-FIXTURE-PASS only at the end.
    let driver = r#"
set -eu
W="$1"; DAEMON="$2"; PROBE="$3"; HERMIT="$4"
# Root is for the daemon, the autofs mount and the owner-namespace probes
# only; Hermit itself runs as the invoking user (the bug's real case: an
# unprivileged invoker whose Hermit makes a user namespace over an autofs
# mount it does not own).
[ -n "${SUDO_UID:-}" ] && [ "$SUDO_UID" != 0 ] || { echo "FIXTURE-FAIL: SUDO_UID unset or 0"; exit 1; }
[ -n "${SUDO_GID:-}" ] || { echo "FIXTURE-FAIL: SUDO_GID unset"; exit 1; }
as_invoker() { setpriv --reuid="$SUDO_UID" --regid="$SUDO_GID" --init-groups -- "$@"; }
ROOT="$W/root"; mkdir -p "$ROOT"
"$DAEMON" "$ROOT" "$W/ready" "$W/daemon.log" "ghostTrig,ghostTrigDir,ghostStat,ghostStatx" &
DPID=$!
trap 'kill $DPID 2>/dev/null || true; chown -R "$SUDO_UID:$SUDO_GID" "$W" 2>/dev/null || true' EXIT
chmod 0755 "$W" "$ROOT"
for i in $(seq 1 100); do [ -f "$W/ready" ] && break; sleep 0.1; done
[ -f "$W/ready" ] || { echo "daemon never became ready"; cat "$W/daemon.log"; exit 1; }
reqs() { grep -c '^REQ' "$W/daemon.log" || true; }
fail() { echo "FIXTURE-FAIL: $*"; cat "$W/daemon.log"; exit 1; }

# Trigger primitives in the owner namespace (where Hermit-parent holds
# also execute): plain O_PATH on a ghost key must not automount;
# O_PATH|O_DIRECTORY must; classic stat must not; statx must.
B=$(reqs); "$PROBE" opath "$ROOT/ghostTrig" | grep -q ": OK" || fail "opath ghost"
[ "$(reqs)" = "$B" ] || fail "plain O_PATH triggered an automount"
B=$(reqs); "$PROBE" opathdir "$ROOT/ghostTrigDir" | grep -q ": OK" || fail "opathdir ghost"
[ "$(reqs)" -gt "$B" ] || fail "O_PATH|O_DIRECTORY did not trigger an automount"
B=$(reqs); "$PROBE" stat "$ROOT/ghostStat" | grep -q ": OK" || fail "stat ghost"
[ "$(reqs)" = "$B" ] || fail "classic stat triggered an automount"
B=$(reqs); "$PROBE" statx "$ROOT/ghostStatx" | grep -q ": OK" || fail "statx ghost"
[ "$(reqs)" -gt "$B" ] || fail "statx did not trigger an automount"

# Hermit in the fixture namespace: missing name answers ENOENT, an
# unlisted real key's contents loop to ELOOP with exactly one NOTICE,
# and a $HOME key the launcher pre-triggered is simply there.
OUT=$(as_invoker timeout --kill-after 5s 120s "$HERMIT" run --strict --base-env=minimal --epoch=2026-01-01T00:00:00Z -- "$PROBE" stat "$ROOT/missing1" 2>/dev/null) \
  || fail "hermit missing-name run failed: $OUT"
echo "$OUT" | grep -q "errno=2" || fail "hermit missing name not ENOENT: $OUT"
ERR=$(as_invoker timeout --kill-after 5s 120s "$HERMIT" run --strict --base-env=minimal --epoch=2026-01-01T00:00:00Z -- "$PROBE" stat "$ROOT/realHermit/marker" "$ROOT/realHermit/marker" 2>&1 >/dev/null) \
  || true
echo "$ERR" | grep -q "hermit: NOTICE: the guest's lookup of \"$ROOT/realHermit/marker\" failed with ELOOP, likely" \
  || fail "ELOOP notice missing or unhedged: $ERR"
[ "$(echo "$ERR" | grep -c 'hermit: NOTICE:')" = 1 ] || fail "notice printed more than once: $ERR"
OUT=$(as_invoker timeout --kill-after 5s 120s "$HERMIT" run --strict --base-env=minimal --epoch=2026-01-01T00:00:00Z --env "HOME=$ROOT/realHome" -- "$PROBE" stat "$ROOT/realHome/marker" 2>/dev/null) \
  || fail "hermit pretriggered-home run failed: $OUT"
echo "$OUT" | grep -q ": OK" || fail "pre-triggered \$HOME key not mounted in guest: $OUT"
echo "AUTOFS-FIXTURE-PASS"
"#;
    let driver_path = build_root.join("driver.sh");
    fs::write(&driver_path, driver).expect("failed to write fixture driver");
    let mut run = Command::new("sudo");
    run.args(["-n", "unshare", "-m", "--propagation", "private", "bash"])
        .arg(&driver_path)
        .arg(&work)
        .arg(&daemon)
        .arg(&probe)
        .arg(env!("CARGO_BIN_EXE_hermit"));
    let output = hermit_test::hermit_output("autofs privileged fixture", &mut run);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("AUTOFS-FIXTURE-PASS"),
        "privileged autofs fixture failed\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}
