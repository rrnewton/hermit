/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--verify` must not claim determinism it did not check.
//!
//! Reverie types many syscall output buffers as bare pointers, so the compared
//! records show the buffer's ADDRESS and not its bytes. Two runs whose buffers
//! differ while their return values agree therefore produce character-identical
//! records and compare equal. Measured on a netlink `recvmsg` that returns a
//! stable `Ok(1468)` while four payload bytes vary: the same guest reported
//! `verdict: matched, bitwise_parity: true` and printed "Determinism verified",
//! while the same command with `--detlog-io-buffers` reported `diverged`.
//!
//! The coverage fix was to make that hashing the DEFAULT rather than an opt-in;
//! `--no-detlog-io-buffers` now selects the weaker comparison deliberately. This
//! test guards the other half: that when content was NOT compared, the verdict
//! says so instead of claiming determinism outright.

use std::path::Path;
use std::process::Command;

/// Run `/bin/true` under `--verify --verify-strict`, optionally with the
/// output-buffer hash, and return (stderr, parsed verify JSON).
///
/// THE SENSE OF THE FLAG INVERTED, so which branch needs an argument inverted
/// with it. Buffer hashing is ON BY DEFAULT since the io-buffer default flip,
/// and the positive `--detlog-io-buffers` spelling no longer parses at all, so
/// `with_io_buffers == true` is now the plain invocation and it is the FALSE
/// case that has to ask for the weaker comparison. Every assertion in both
/// tests below is unchanged; only the way the two cases are selected moved.
/// The `true` case is now strictly more valuable than before, because it
/// exercises the configuration an ordinary user actually gets.
fn verify(with_io_buffers: bool) -> (String, serde_json::Value) {
    let json = tempfile::NamedTempFile::new().expect("temp file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["run", "--strict", "--verify", "--verify-strict"]);
    if !with_io_buffers {
        command.arg("--no-detlog-io-buffers");
    }
    command
        .arg("--verify-json")
        .arg(json.path())
        .args(["--", "/bin/true"]);
    let output = command.output().expect("failed to start hermit");
    let text = std::fs::read_to_string(json.path()).expect("verify json");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        serde_json::from_str(&text).expect("verify json parses"),
    )
}

/// The console phrase that claims bitwise parity. It must appear exactly when
/// the published report says `bitwise_parity: true`.
const BITWISE_PARITY_CLAIM: &str = "bitwise parity established";

/// Run `/bin/true` under a plain `--verify` (the lossy `Stripped` comparison,
/// no `--verify-strict`) and return (stderr, parsed verify JSON).
fn verify_plain() -> (String, serde_json::Value) {
    let json = tempfile::NamedTempFile::new().expect("temp file");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--verify", "--verify-json"])
        .arg(json.path())
        .args(["--", "/bin/true"])
        .output()
        .expect("failed to start hermit");
    let text = std::fs::read_to_string(json.path()).expect("verify json");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        serde_json::from_str(&text).expect("verify json parses"),
    )
}

/// Guard: `/bin/true` must actually verify, or neither assertion below means
/// anything.
fn assert_matched(report: &serde_json::Value) {
    assert_eq!(
        report["verdict"], "matched",
        "/bin/true should verify; this test cannot say anything about the wording of a \
         success message that was never printed"
    );
}

#[test]
fn a_verdict_without_buffer_content_does_not_claim_determinism() {
    let (stderr, report) = verify(false);
    assert_matched(&report);
    assert_eq!(report["bitwise_parity"], false);
    assert_eq!(
        report["comparison"]["compare_io_buffers"], false,
        "the envelope must record that buffer content did not participate, so a consumer can \
         require it rather than assume it"
    );
    // The "Determinism verified" marker itself is deliberately NOT removed --
    // ~110 files assert on that substring -- so what is asserted here is that
    // the claim is QUALIFIED, not that it is absent.
    assert!(
        stderr.contains("output-buffer CONTENT was not compared"),
        "reported success without comparing syscall output-buffer content and without saying \
         so. A divergence confined to a buffer whose length is stable is invisible to this \
         comparison, so an unqualified claim overstates what was \
         established.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(BITWISE_PARITY_CLAIM),
        "the report says bitwise_parity is false, so the console must not claim it.\nstderr:\n{stderr}"
    );
}

#[test]
fn a_verdict_with_buffer_content_may_claim_determinism() {
    // The converse, so the wording change is not just "never claim anything":
    // when content IS compared the strong sentence is earned and still printed.
    let (stderr, report) = verify(true);
    assert_matched(&report);
    assert_eq!(report["comparison"]["compare_io_buffers"], true);
    assert_eq!(report["bitwise_parity"], true);
    assert!(
        stderr.contains("Determinism verified"),
        "with buffer content compared the strong claim is earned and should be \
         printed.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("output-buffer CONTENT was not compared"),
        "the qualification must NOT appear when content WAS compared, or it is noise rather \
         than information.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(BITWISE_PARITY_CLAIM),
        "the report says bitwise_parity is true, so the console should say so.\nstderr:\n{stderr}"
    );
}

#[test]
fn a_plain_verify_says_its_match_is_not_bitwise() {
    // A plain `--verify` prints the same "Determinism verified" sentence as
    // `--verify-strict`, but its log comparison is the lossy Stripped one. The
    // line after the sentence must say so and must not claim bitwise parity.
    let (stderr, report) = verify_plain();
    assert_matched(&report);
    assert_eq!(report["bitwise_parity"], false);
    assert_eq!(report["comparison"]["strip_lines"], true);
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "the success sentence is kept verbatim for its consumers.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(BITWISE_PARITY_CLAIM),
        "a Stripped match is not bitwise parity.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("under the lossy Stripped comparison")
            && stderr.contains("This is not a bitwise comparison; add --verify-strict for one."),
        "the console must say which comparison the match rests on.\nstderr:\n{stderr}"
    );
}

#[test]
fn the_guest_binary_this_test_relies_on_exists() {
    assert!(Path::new("/bin/true").exists(), "/bin/true is required");
}

// A divergence caused by a host file changing during a run.
//
// Hermit numbers inodes in the order it first sees them, so a host file that
// is replaced between two opens in one run gives that run one more inode, and
// every later inode number of that run is one higher. On 2026-10-05 a package
// update replacing `/etc/ld.so.cache` between the opens of `sar` and its child
// `sadc` made `compat/sar-resource-tables` diverge that way. `--verify` must
// name such a cause rather than report a bare divergence, and must not name
// one for a divergence that has a different cause.

/// The guest: open `F`, wait for the host on `fifo`, print the line it sent,
/// open `F` again, and print the (virtual) inode number of `G`, which it sees
/// last.
const HOST_INPUT_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    cat "$1/F" > /dev/null || exit 3
    stat -c %i "$1/G" || exit 3
"#;

/// A guest that replaces `F` itself, in one run only: after the host's line
/// it moves `A` over `F` when the line is `moveA`, and otherwise moves `B` to
/// `H`, leaving `F` alone. Both are one `mv`, which opens no file named here,
/// so both runs make the same opens. Run 1 then finds a new host inode at its
/// second open of `F` where run 2 finds the one it found before, exactly as a
/// host replacement would show. But the runs diverged before that open: they
/// read different lines.
const SELF_REPLACING_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    if [ "$line" = moveA ]; then mv "$1/A" "$1/F"; else mv "$1/B" "$1/H"; fi || exit 3
    cat "$1/F" > /dev/null || exit 3
"#;

/// Open `fifo` for writing once a reader has it open, or panic at `deadline`.
fn open_fifo_writer(fifo: &Path, deadline: std::time::Instant) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(file) => return file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the guest never opened {} for reading",
                    fifo.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("cannot open {}: {error}", fifo.display()),
        }
    }
}

/// Run `guest` ([`HOST_INPUT_GUEST`] or [`SELF_REPLACING_GUEST`]) under
/// `--verify --verify-strict`. The host sends `lines[0]` to run 1 and
/// `lines[1]` to run 2; before sending to run 1, it replaces `F` with an
/// identical copy when `replace_in_run1` is set. Returns the input directory,
/// stderr, and the typed report.
fn verify_across_host_action(
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    use std::io::Write;

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test directory");
    std::fs::write(root.join("F"), "input\n").expect("write F");
    std::fs::write(root.join("G"), "late\n").expect("write G");
    std::fs::write(root.join("A"), "input\n").expect("write A");
    std::fs::write(root.join("B"), "input\n").expect("write B");
    let fifo = root.join("fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("mkfifo");
    let report_path = root.join("verify.json");
    let stderr_path = root.join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let child = Command::new("timeout")
        .args(["--kill-after", "5s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&report_path)
        .args([
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/sh",
            "-c",
            guest,
            "sh",
        ])
        .arg(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr_file)
        .spawn()
        .expect("failed to start hermit");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(170);

    // Run 1: once its guest waits on the FIFO it has opened F once.
    let mut writer = open_fifo_writer(&fifo, deadline);
    if replace_in_run1 {
        let copy = root.join("F.new");
        std::fs::copy(root.join("F"), &copy).expect("copy F");
        std::fs::rename(&copy, root.join("F")).expect("replace F");
    }
    writeln!(writer, "{}", lines[0]).expect("send run 1 its line");
    drop(writer);

    // Run 2 starts only after run 1 has finished with the FIFO.
    while !std::fs::read_to_string(&stderr_path)
        .unwrap_or_default()
        .contains("Run2")
    {
        assert!(std::time::Instant::now() < deadline, "run 2 never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut writer = open_fifo_writer(&fifo, deadline);
    writeln!(writer, "{}", lines[1]).expect("send run 2 its line");
    drop(writer);

    let output = child.wait_with_output().expect("wait for hermit");
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert_eq!(
        output.status.code(),
        Some(hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "a divergence exits 1\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report = hermit::canonical_verdict::VerificationReport::from_current_json_slice(
        &std::fs::read(&report_path).expect("read verify json"),
    )
    .expect("parse verify json");
    (root, stderr, report)
}

/// The sar divergence in miniature: a host file replaced between run 1's two
/// opens of it. `--verify` reports the divergence as a typed host input
/// change, naming the run, the file and both of its host identities, and
/// still fails.
#[test]
fn a_host_file_replaced_during_run_1_is_named_as_the_cause() {
    let (root, stderr, report) =
        verify_across_host_action("host-input-replaced", HOST_INPUT_GUEST, true, ["go", "go"]);
    assert!(!report.verified, "{stderr}");
    assert!(!report.bitwise_parity);
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::InfrastructureError,
        "{stderr}"
    );
    assert!(
        report.comparison.is_some(),
        "the comparison is kept as evidence"
    );
    assert!(
        report.first_divergent_record.is_some(),
        "and where it diverged"
    );
    let Some(hermit::canonical_verdict::InfrastructureError::HostInputChanged {
        run,
        path,
        before,
        after,
    }) = &report.infrastructure_error
    else {
        panic!(
            "no host input change named: {:?}\n{stderr}",
            report.infrastructure_error
        );
    };
    assert_eq!(*run, hermit::canonical_verdict::VerificationRun::Run1);
    assert_eq!(Path::new(path), root.join("F"));
    assert_ne!(before.ino, after.ino, "the replacement is a new host inode");
    assert!(
        stderr.contains("HERMIT_HOST_INPUT_CHANGED host input changed during run 1: "),
        "{stderr}"
    );
    assert!(stderr.contains("Failure: nondeterministic."), "{stderr}");
}

/// The control: the same guest diverging because the host sent each run a
/// different line, with no host file changed. That divergence is not
/// attributed to a host input change.
#[test]
fn a_divergence_with_no_host_file_change_names_no_host_input_change() {
    let (_root, stderr, report) = verify_across_host_action(
        "host-input-unchanged",
        HOST_INPUT_GUEST,
        false,
        ["one", "two"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
}

/// The counterexample a pattern alone cannot refuse: the guest replaces `F`
/// itself, in run 1 only, because the runs read different lines. The runs'
/// patterns differ as a host replacement's would, but the divergence came
/// first, so it is not attributed to a host input change and stays a failure.
#[test]
fn a_guest_that_replaces_a_file_after_diverging_names_no_host_input_change() {
    let (_root, stderr, report) = verify_across_host_action(
        "host-input-self-replaced",
        SELF_REPLACING_GUEST,
        false,
        // The same length, since the shell reads its line a byte per syscall.
        ["moveA", "moveB"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
}
