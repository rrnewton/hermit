/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Real programs under in-guest LiteInst (`--backend=liteinst`), which runs
//! Detcore's Tool inside the guest through the `detcore-liteinst` preload
//! (https://github.com/rrnewton/hermit/issues/3520).
//!
//! These tests were `hermit-cli/tests/liteinst_advanced.rs`, which ran the same
//! programs under the ptrace-hosted LiteInst hybrid. When Hermit's LiteInst
//! became in-guest only they moved here, into the `cli` test binary, and kept
//! their programs, inputs and expected output. Three things differ, each
//! because in-guest LiteInst refuses what the hybrid accepted (see
//! `refuse_unqualified_liteinst_in_guest_options` in
//! `hermit-cli/src/bin/hermit/run.rs`):
//!
//! - Every run passes `--max-timeslice=disabled`. The in-guest Tool host
//!   cannot deliver Detcore's preemption timer yet, so a run with a maximum
//!   timeslice is refused. None of these guests depends on preemption.
//! - No run passes `--verify` yet. The exact expected output each test
//!   asserts remains; the determinism verdict does not. The in-guest Tool now
//!   forwards its records (see
//!   `liteinst_in_guest_verify_compares_the_records_the_guest_forwards` in
//!   `cli.rs`), so restoring it is the next step of
//!   https://github.com/rrnewton/hermit/issues/3520.
//! - The hybrid's activation banner and `--verify` banner are gone, so each
//!   run instead asserts Hermit's in-guest selection line and the absence of
//!   any host-hybrid line.
//!
//! The runtime library is the `libdetcore_liteinst.so` beside the Hermit this
//! binary was built with. The validation builds put it there:
//! `build.workspace_compile_in_pinned_root` and `build.workspace_on_host` run
//! `cargo build --profile validate --workspace --all-targets`, which builds the
//! `detcore-liteinst` cdylib next to `target/validate/hermit`. A plain
//! `cargo nextest run -p hermit --test cli` does not build it; run
//! `cargo build -p detcore-liteinst` (in the same profile) first, or Hermit
//! refuses the run and names that command.

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

use super::dispatch_stats;
use super::hermit_run_guard;
use super::process_build_root;

static LITEINST_ADVANCED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static USERFAULTFD_GUEST: OnceLock<PathBuf> = OnceLock::new();
static EXIT_REAPING_GUEST: OnceLock<PathBuf> = OnceLock::new();
static UNSCHEDULED_EXIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DUP_ALIAS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static CLOSE_RANGE_PORT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static IOV_OVERWRITTEN_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPAT_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_SEMANTIC_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPRESSED_FIXTURES: OnceLock<[PathBuf; 2]> = OnceLock::new();

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

/// The line Hermit prints when it runs the guest under in-guest LiteInst
/// (`RunOpts::main` in `hermit-cli/src/bin/hermit/run.rs`).
const IN_GUEST_SELECTED: &str =
    "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";

fn hermit_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hermit"))
}

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
        let build_root = process_build_root("liteinst-advanced");
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
        let build_root = process_build_root("liteinst-advanced");
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
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let fixture = build_root.join("compatibility-fixture.txt");
        fs::write(&fixture, COMPAT_FIXTURE_CONTENT).expect("failed to write LiteInst fixture");
        fixture
    })
}

fn semantic_fixture() -> &'static Path {
    LITEINST_SEMANTIC_FIXTURE.get_or_init(|| {
        let build_root = process_build_root("liteinst-advanced");
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

fn compressed_fixtures() -> &'static [PathBuf; 2] {
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(liteinst-c2-hermit): Review running a Nix wrapper's ELF directly.
/// The ELF that runs when `program` is executed. Nix installs some programs
/// (gzip in the pinned validation root) as a `#!` shell wrapper that execs
/// `.<name>-wrapped` beside it. In-guest LiteInst does not support execve yet,
/// so a wrapper would fail at that exec; run the wrapped ELF directly. An
/// ordinary ELF program is returned unchanged.
fn elf_program(program: &Path) -> PathBuf {
    fn is_elf(path: &Path) -> bool {
        let mut magic = [0_u8; 4];
        fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok()
            && magic == *b"\x7fELF"
    }
    if is_elf(program) {
        return program.to_path_buf();
    }
    let resolved = fs::canonicalize(program)
        .unwrap_or_else(|error| panic!("failed to resolve {}: {error}", program.display()));
    let name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no UTF-8 file name", resolved.display()));
    let wrapped = resolved.with_file_name(format!(".{name}-wrapped"));
    assert!(
        is_elf(&wrapped),
        "{} is neither an ELF nor a Nix wrapper around {}",
        program.display(),
        wrapped.display()
    );
    wrapped
}

const VIRTUAL_TIME_EPOCH: &str = "2026-01-01T00:00:00Z";

fn liteinst_command_at_epoch(log_level: &str, epoch: Option<&str>) -> Command {
    let mut command = Command::new(hermit_binary());
    command
        .arg(format!("--log={log_level}"))
        .args(["--backend", "liteinst", "run"]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args([
        "--max-timeslice=disabled",
        "--strict",
        "--base-env=minimal",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
    ]);
    command
}

fn liteinst_command(log_level: &str) -> Command {
    liteinst_command_at_epoch(log_level, None)
}

#[test]
fn liteinst_in_guest_commands_use_minimal_environment_and_private_workdir() {
    let command = liteinst_command("off");
    let args = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            "--log=off",
            "--backend",
            "liteinst",
            "run",
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ]
    );
    let epoch_args = liteinst_command_at_epoch("off", Some(VIRTUAL_TIME_EPOCH))
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        epoch_args,
        [
            "--log=off",
            "--backend",
            "liteinst",
            "run",
            "--epoch=2026-01-01T00:00:00Z",
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ]
    );
}

fn run_liteinst(program: &Path, args: &[&str]) -> Output {
    run_liteinst_with_input(program, args, None, None)
}

fn run_liteinst_with_input(
    program: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    epoch: Option<&str>,
) -> Output {
    let home = tempfile::tempdir().expect("failed to create isolated LiteInst HOME");
    let xdg_config_home = home.path().join(".config");
    fs::create_dir_all(&xdg_config_home).expect("failed to create isolated XDG config directory");
    let mut command = liteinst_command_at_epoch("info", epoch);
    command
        .arg(format!("--env=HOME={}", home.path().display()))
        .arg(format!(
            "--env=XDG_CONFIG_HOME={}",
            xdg_config_home.display()
        ))
        .arg("--env=PYTHONDONTWRITEBYTECODE=1")
        .env("HOME", home.path())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    command.arg("--").arg(program).args(args);
    let Some(input) = input else {
        return command
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit LiteInst");
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

/// Requires that Hermit ran the guest under in-guest LiteInst and never named
/// the retired host hybrid.
fn assert_in_guest_selected(stderr: &str) {
    assert!(
        stderr.lines().any(|line| line == IN_GUEST_SELECTED),
        "{stderr}"
    );
    assert!(!stderr.contains("liteinst host hybrid"), "{stderr}");
    assert!(!stderr.contains("LiteInst host hybrid"), "{stderr}");
}

fn assert_liteinst_in_guest_output(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
    output
}

fn run_liteinst_in_guest(program: &Path, args: &[&str]) -> Output {
    assert_liteinst_in_guest_output(run_liteinst(program, args))
}

fn run_liteinst_in_guest_with_stdin(program: &Path, args: &[&str], input: &[u8]) -> Output {
    assert_liteinst_in_guest_output(run_liteinst_with_input(program, args, Some(input), None))
}

fn assert_liteinst_in_guest(program: &Path, args: &[&str], expected_stdout: &[u8]) {
    let output = run_liteinst_in_guest(program, args);
    assert_eq!(
        output.stdout,
        expected_stdout,
        "{} {args:?}: stdout={:?}",
        program.display(),
        String::from_utf8_lossy(&output.stdout),
    );
}

fn assert_liteinst_virtual_time_is_continuous() {
    const EPOCH_SECONDS: u64 = 1_767_225_600;
    const MAX_STARTUP_SECONDS: u64 = 60;

    // Whole seconds remain stable across LiteInst runs. Do not assert the old
    // exact epoch: that encoded #1095's reset-on-exec behavior and rejects
    // legitimate deterministic startup progress.
    let output = assert_liteinst_in_guest_output(run_liteinst_with_input(
        Path::new("/usr/bin/date"),
        &["-u", "+%s"],
        None,
        Some(VIRTUAL_TIME_EPOCH),
    ));
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
    let progress = assert_liteinst_in_guest_output(run_liteinst_with_input(
        advanced_guest(),
        &["clock-progress"],
        None,
        Some(VIRTUAL_TIME_EPOCH),
    ));
    assert_eq!(progress.stdout, b"clock-progress-ok\n");
}

#[test]
fn liteinst_in_guest_heap_growth_avoids_trampoline_mappings() {
    let _guard = hermit_run_guard();
    let output = run_liteinst_in_guest(mmap_guest(), &["heap"]);
    assert!(
        output.stdout.starts_with(b"heap "),
        "heap-growth guest omitted its success marker: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Signal phase 1, step I3: with guest SIGALRM handlers admitted (and site
/// patching off, as phase 1 requires), the guest's handler is installed
/// virtually and Detcore learns of it: the phase 1 table then refuses a
/// socket, a fork and rt_sigpending with EOPNOTSUPP; an expiry while SIGALRM
/// is blocked is held in the scheduler's ledger, so a change to SIG_DFL is
/// refused with EPERM; SIG_IGN then discards it and the socket succeeds. With
/// handlers not admitted (the default), the installation is refused with
/// EPERM as before and nothing changes. Both runs are verified deterministic.
#[test]
fn liteinst_in_guest_sigalrm_handler_is_virtual_and_published() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-sigalrm");
    fs::create_dir_all(&build_root).expect("failed to create the SIGALRM guest directory");
    let guest = build_root.join("sigalrm_virtual_handler");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_sigalrm_virtual_handler.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the SIGALRM guest");
    assert!(compiled.status.success(), "{compiled:?}");
    for (admitted, expected) in [
        (
            "1",
            "install=0 errno=0\nquery_is_handler=1\nsigpending=-1 errno=95\n\
             socket=refused errno=95\nfork=refused errno=95\nto_default=-1 errno=1\n\
             ignore=0\nsocket_after=ok errno=0\n",
        ),
        (
            "0",
            "install=-1 errno=1\nquery_is_handler=0\nsigpending=0 errno=0\n\
             socket=ok errno=0\nfork=ok errno=0\nto_default=0 errno=0\n\
             ignore=0\nsocket_after=ok errno=0\n",
        ),
    ] {
        let output = liteinst_command("info")
            .arg("--verify")
            .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
            .arg(format!(
                "--env=REVERIE_LITEINST_SIGALRM_HANDLERS={admitted}"
            ))
            .arg("--")
            .arg(&guest)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit LiteInst");
        let output = assert_liteinst_in_guest_output(output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "admitted={admitted}"
        );
    }
    // A signalfd inherited on stdin (a run without --verify keeps the
    // caller's stdin) refuses the handler although Detcore's model labels
    // stdin a regular file: the run then behaves as with handlers not
    // admitted.
    let not_admitted = "install=-1 errno=1\nquery_is_handler=0\nsigpending=0 errno=0\n\
                        socket=ok errno=0\nfork=ok errno=0\nto_default=0 errno=0\n\
                        ignore=0\nsocket_after=ok errno=0\n";
    let signalfd = {
        use std::os::fd::FromRawFd;
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut mask) };
        unsafe { libc::sigaddset(&mut mask, libc::SIGUSR2) };
        let raw = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC) };
        assert!(raw >= 0, "signalfd: {}", std::io::Error::last_os_error());
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
    };
    let output = liteinst_command("info")
        .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
        .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
        .arg("--")
        .arg(&guest)
        .stdin(Stdio::from(signalfd))
        .output()
        .expect("failed to run Hermit LiteInst");
    let output = assert_liteinst_in_guest_output(output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), not_admitted);
}

/// In-guest LiteInst runs the Detcore Tool inside the guest, so the Tool must
/// keep out of the guest's C library heap. Installing a SIGALRM handler makes
/// Detcore list the kernel's descriptor table; listed with glibc's opendir,
/// the freed buffer kept raw /proc inode numbers that the guest's next
/// opendir got back, so the padding of its getdents64 records (which Linux
/// never writes) differed between runs (how `iostat` failed --verify). The
/// fixture prints that padding; the run must verify and match native Linux.
#[test]
fn liteinst_in_guest_tool_directory_reads_stay_out_of_the_guest_heap() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("tool_keeps_out_of_guest_heap");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_tool_keeps_out_of_guest_heap.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");
    let output = liteinst_command("info")
        .arg("--verify")
        .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
        .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
        .arg("--")
        .arg(&guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst");
    let output = assert_liteinst_in_guest_output(output);
    let mut expected = String::from("install=0\nrecord 0 pad: 00 00 00\nrecord 1 pad: 00 00\n");
    for record in 2..10 {
        expected.push_str(&format!("record {record} pad: 00 00 00 00 00 00\n"));
    }
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn liteinst_in_guest_detcore_micro_suite() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/bin/true"), &[], b"");
    assert_liteinst_in_guest(Path::new("/bin/echo"), &["hello"], b"hello\n");

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let readme = repository.join("README.md");
    let expected = fs::read(&readme).expect("read README fixture");
    assert_liteinst_in_guest(
        Path::new("/bin/cat"),
        &[readme.to_str().unwrap()],
        &expected,
    );
}

#[test]
fn liteinst_in_guest_identity_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/usr/bin/uname"), &["-s"], b"Linux\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/id"), &["-u"], b"0\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/whoami"), &[], b"root\n");
}

#[test]
fn liteinst_in_guest_virtual_identity_and_time() {
    let _guard = hermit_run_guard();
    assert_liteinst_virtual_time_is_continuous();
    assert_liteinst_in_guest(
        Path::new("/usr/bin/hostname"),
        &[],
        b"hermetic-container.local\n",
    );
    let group_file = fs::read_to_string("/etc/group").expect("failed to read host group database");
    let root_group = group_name_by_gid(&group_file, "0").expect("GID 0 should have a name");
    let overflow_group = group_name_by_gid(&group_file, "65534").unwrap_or("nobody");
    // Detcore reports both primary GIDs as zero. Container::map_root maps the
    // caller's effective GID to zero; other inherited supplementary groups
    // appear as the overflow GID. A group-file entry does not add membership.
    // The groups utility prints each distinct GID only once.
    let effective_gid = nix::unistd::getegid();
    let has_overflow_group = nix::unistd::getgroups()
        .expect("failed to read caller supplementary groups")
        .iter()
        .any(|gid| *gid != effective_gid);
    let expected_groups = if has_overflow_group {
        format!("{root_group} {overflow_group}\n")
    } else {
        format!("{root_group}\n")
    };
    assert_liteinst_in_guest(
        Path::new("/usr/bin/groups"),
        &[],
        expected_groups.as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_file_and_text_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/printf"),
        &["liteinst-printf-ok\n"],
        b"liteinst-printf-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/grep"),
        &["^liteinst", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/head"),
        &["-n", "1", fixture],
        COMPAT_FIXTURE_CONTENT,
    );

    let expected_wc = format!("{} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_in_guest(
        Path::new("/usr/bin/wc"),
        &["-c", fixture],
        expected_wc.as_bytes(),
    );
    let expected_sha256 = format!("{COMPAT_FIXTURE_SHA256}  {fixture}\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sha256sum"),
        &[fixture],
        expected_sha256.as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/stat"),
        &["-c", "%s", fixture],
        format!("{}\n", COMPAT_FIXTURE_CONTENT.len()).as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_semantic_text_utilities() {
    let _guard = hermit_run_guard();
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/tail"),
        &["-n", "2", fixture],
        b"alpha:1\nbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/uniq"),
        &[fixture],
        b"gamma:3\nalpha:1\nbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/cut"),
        &["-d", ":", "-f", "1", fixture],
        b"gamma\nalpha\nalpha\nbeta\n",
    );
    assert_liteinst_in_guest(Path::new("/usr/bin/diff"), &[fixture, fixture], b"");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sed"),
        &["-n", "2,3p", fixture],
        b"alpha:1\nalpha:1\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sort"),
        &[fixture],
        b"alpha:1\nalpha:1\nbeta:2\ngamma:3\n",
    );
}

#[test]
fn liteinst_in_guest_semantic_file_and_sqlite_utilities() {
    let _guard = hermit_run_guard();
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/find"),
        &[fixture, "-maxdepth", "0", "-type", "f", "-print"],
        format!("{fixture}\n").as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/md5sum"),
        &[fixture],
        format!("{SEMANTIC_FIXTURE_MD5}  {fixture}\n").as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/du"),
        &["-b", fixture],
        format!("{}\t{fixture}\n", SEMANTIC_FIXTURE_CONTENT.len()).as_bytes(),
    );
    assert_liteinst_in_guest(
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
fn liteinst_in_guest_encoding_and_digest_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
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
        assert_liteinst_in_guest(Path::new(program), &[fixture], expected.as_bytes());
    }
    let expected_cksum = format!("2216041199 {} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_in_guest(
        Path::new("/usr/bin/cksum"),
        &[fixture],
        expected_cksum.as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_formatting_and_sequence_utilities() {
    let _guard = hermit_run_guard();
    let compat_fixture = compatibility_fixture();
    let compat_fixture = compat_fixture
        .to_str()
        .expect("fixture path should be UTF-8");
    let semantic_fixture = semantic_fixture();
    let semantic_fixture = semantic_fixture
        .to_str()
        .expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(Path::new("/usr/bin/seq"), &["5"], b"1\n2\n3\n4\n5\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/fmt"),
        &["--width=10", compat_fixture],
        b"liteinst\ncompatibility\nfixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/fold"),
        &["--width=8", compat_fixture],
        b"liteinst\n compati\nbility f\nixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/nl"),
        &["-ba", semantic_fixture],
        b"     1\tgamma:3\n     2\talpha:1\n     3\talpha:1\n     4\tbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/tac"),
        &[semantic_fixture],
        b"beta:2\nalpha:1\nalpha:1\ngamma:3\n",
    );
}

#[test]
fn liteinst_in_guest_round2_encoding_and_comparison_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/base32"),
        &["--wrap=0", fixture],
        b"NRUXIZLJNZZXIIDDN5WXAYLUNFRGS3DJOR4SAZTJPB2HK4TFBI======",
    );
    let sum_output = run_liteinst_in_guest(Path::new("/usr/bin/sum"), &[fixture]);
    let sum_stdout = String::from_utf8(sum_output.stdout).expect("sum output should be UTF-8");
    let sum_fields = sum_stdout.split_whitespace().collect::<Vec<_>>();
    match sum_fields.as_slice() {
        ["04458", "1"] => {}
        ["04458", "1", output_path] => assert_eq!(*output_path, fixture),
        _ => panic!("unexpected sum output: {sum_stdout:?}"),
    }
    assert_liteinst_in_guest(Path::new("/usr/bin/cmp"), &[fixture, fixture], b"");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/comm"),
        &[fixture, fixture],
        b"\t\tliteinst compatibility fixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/join"),
        &[fixture, fixture],
        b"liteinst compatibility fixture compatibility fixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/paste"),
        &[fixture, fixture],
        b"liteinst compatibility fixture\tliteinst compatibility fixture\n",
    );
}

#[test]
fn liteinst_in_guest_round2_representation_and_path_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/od"),
        &["-An", "-tx1", fixture],
        b" 6c 69 74 65 69 6e 73 74 20 63 6f 6d 70 61 74 69\n 62 69 6c 69 74 79 20 66 69 78 74 75 72 65 0a\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/pr"),
        &["-t", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/readlink"),
        &["-f", "/etc/../etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/rev"),
        &[fixture],
        b"erutxif ytilibitapmoc tsnietil\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/strings"),
        &[fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    let dd_input = format!("if={fixture}");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/dd"),
        &[&dd_input, "bs=7", "count=2", "status=none"],
        b"liteinst compa",
    );
}

#[test]
fn liteinst_in_guest_round2_arithmetic_and_predicate_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(Path::new("/usr/bin/factor"), &["84"], b"84: 2 2 3 7\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/expr"), &["6", "*", "7"], b"42\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/numfmt"),
        &["--to=iec", "1024"],
        b"1.0K\n",
    );
    assert_liteinst_in_guest(Path::new("/usr/bin/test"), &["-f", fixture], b"");
    assert_liteinst_in_guest(Path::new("/usr/bin/pathchk"), &[fixture], b"");
}

#[test]
fn liteinst_in_guest_round3_portable_system_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/usr/bin/arch"), &[], b"x86_64\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/getconf"), &["LONG_BIT"], b"64\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/getopt"),
        &["-o", "ab:", "--", "-a", "-b", "value", "rest"],
        b" -a -b 'value' -- 'rest'\n",
    );
    assert_liteinst_in_guest(
        Path::new("/bin/bash"),
        &[
            "--noprofile",
            "--norc",
            "-c",
            "printf 'liteinst-bash-ok\\n'",
        ],
        b"liteinst-bash-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/jq"),
        &["-nr", "[3,1,2] | sort | join(\",\")"],
        b"1,2,3\n",
    );

    let existing_directory = compatibility_fixture()
        .parent()
        .expect("compatibility fixture should have a parent directory");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/mkdir"),
        &[
            "-p",
            existing_directory.to_str().expect("path should be UTF-8"),
        ],
        b"",
    );
}

#[test]
fn liteinst_in_guest_round3_encoding_and_compression_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/iconv"),
        &["-f", "UTF-8", "-t", "UTF-16LE", fixture],
        b"l\0i\0t\0e\0i\0n\0s\0t\0 \0c\0o\0m\0p\0a\0t\0i\0b\0i\0l\0i\0t\0y\0 \0f\0i\0x\0t\0u\0r\0e\0\n\0",
    );

    // xz is not here: it installs signal handlers before it decompresses,
    // and the in-guest runtime refuses every guest handler other than
    // SIG_DFL and SIG_IGN (https://github.com/rrnewton/reverie/issues/243).
    // It returns when that is fixed.
    let [gzip_fixture, bzip2_fixture] = compressed_fixtures();
    for (program, compressed_fixture) in [
        ("/usr/bin/gzip", gzip_fixture),
        ("/usr/bin/bzip2", bzip2_fixture),
    ] {
        assert_liteinst_in_guest(
            &elf_program(Path::new(program)),
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
fn liteinst_in_guest_round3_stdin_filter_utilities() {
    let _guard = hermit_run_guard();
    let output = run_liteinst_in_guest_with_stdin(
        Path::new("/usr/bin/tr"),
        &["a-z", "A-Z"],
        b"gamma\nalpha\nbeta\n",
    );
    assert_eq!(output.stdout, b"GAMMA\nALPHA\nBETA\n");

    let output =
        run_liteinst_in_guest_with_stdin(Path::new("/usr/bin/tee"), &[], b"liteinst-tee-ok\n");
    assert_eq!(output.stdout, b"liteinst-tee-ok\n");

    let output = run_liteinst_in_guest_with_stdin(Path::new("/usr/bin/tsort"), &[], b"a b\nb c\n");
    assert_eq!(output.stdout, b"a\nb\nc\n");
}

#[test]
fn liteinst_in_guest_path_and_language_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(
        Path::new("/usr/bin/basename"),
        &["/tmp/hermit-example.txt", ".txt"],
        b"hermit-example\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/dirname"),
        &["/tmp/hermit-example.txt"],
        b"/tmp\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/realpath"),
        &["/etc/../etc/passwd"],
        b"/etc/passwd\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/ls"),
        &["-1", "/etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/awk"),
        &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
        b"55\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/perl"),
        &["-e", r#"print join(q{,}, map { $_ * $_ } 1..5), qq{\n}"#],
        b"1,4,9,16,25\n",
    );
}

#[test]
fn liteinst_in_guest_shell_and_entropy_consumer() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(
        Path::new("/bin/sh"),
        &["-c", "printf 'liteinst-shell-ok\\n'"],
        b"liteinst-shell-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/hexdump"),
        &["/dev/urandom", "--length", "16"],
        b"0000000 7229 04bb 964d 28df ba71 4c03 de95 7027\n0000010\n",
    );
}

/// Without `--verify`, the second run stands in for the determinism verdict
/// the hybrid's verified run gave: the guest prints eight bytes of
/// `/dev/urandom`, which must be the same in both runs.
#[test]
fn liteinst_in_guest_python_entropy() {
    let _guard = hermit_run_guard();
    let run = || {
        let output = run_liteinst_in_guest(
            Path::new("/usr/bin/python3"),
            &[
                "-c",
                "import os; print(os.getpid(), len(os.urandom(8)), os.urandom(8).hex())",
            ],
        );
        String::from_utf8(output.stdout).expect("Python output should be UTF-8")
    };
    let stdout = run();
    let fields = stdout.split_whitespace().collect::<Vec<_>>();
    assert_eq!(fields.len(), 3, "stdout={stdout:?}");
    // The container init is PID 1. Synchronous tracing consumes TID 2 to
    // match the other logging paths, so the root guest starts as PID 3.
    assert_eq!(fields[0], "3", "stdout={stdout:?}");
    assert_eq!(fields[1], "8", "stdout={stdout:?}");
    assert_eq!(fields[2].len(), 16, "stdout={stdout:?}");
    assert!(
        fields[2].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "stdout={stdout:?}"
    );
    assert_eq!(run(), stdout, "the second run printed different entropy");
}

/// `examples/rand.py` prints ten values, each in `1..=101`.
///
/// `random` imports `hashlib`, whose `_hashlib` extension loads
/// `libcrypto.so.3`, and libcrypto's initializer executes CPUID in code mapped
/// after the in-guest runtime started. Before the pinned Reverie emulated such
/// instructions through the Tool, the guest died there with SIGSEGV (exit 139);
/// `liteinst_in_guest_cpuid_in_a_late_loaded_library_runs` covers the same path
/// without Python.
#[test]
fn liteinst_in_guest_python_random_example() {
    let _guard = hermit_run_guard();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = run_liteinst_in_guest(&repository.join("examples/rand.py"), &[]);
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

static LATE_CPUID_GUEST: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// Builds `tests/c/dlopen_cpuid_vendor.c` and the shared library
/// `tests/c/dlopen_cpuid_vendor_lib.c` it loads, under `CARGO_TARGET_TMPDIR`.
fn late_cpuid_guest() -> &'static (PathBuf, PathBuf) {
    LATE_CPUID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("dlopen-cpuid-vendor");
        fs::create_dir_all(&build_root).expect("failed to create late-CPUID guest directory");
        let library = build_root.join("libdlopen_cpuid_vendor.so");
        let guest = build_root.join("dlopen_cpuid_vendor");
        for (source, output, flags) in [
            (
                "tests/c/dlopen_cpuid_vendor_lib.c",
                &library,
                &["-shared", "-fPIC"][..],
            ),
            ("tests/c/dlopen_cpuid_vendor.c", &guest, &[][..]),
        ] {
            let compiled = Command::new("cc")
                .args(["-O2", "-Wall", "-Wextra", "-Werror"])
                .args(flags)
                .arg(repository.join(source))
                .arg("-o")
                .arg(output)
                .arg("-ldl")
                .output()
                .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
            assert!(
                compiled.status.success(),
                "{source} compilation failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&compiled.stdout),
                String::from_utf8_lossy(&compiled.stderr),
            );
        }
        (guest, library)
    })
}

/// CPUID executed by a library the guest loads with `dlopen` after start-up.
///
/// The guest executes CPUID leaf 0 in its main executable, then dlopens a
/// library whose constructor executes it again, and prints both vendor
/// strings. Both must be present and equal: Detcore answers CPUID the same way
/// wherever the instruction is. The ptrace backend runs the same guest first as
/// the control, so a failure here is not a broken guest.
///
/// The library is mapped after the in-guest runtime started, so its CPUID has
/// no patch arena; the runtime must emulate it through the Tool. Without that
/// (Reverie before the late-code CPUID fix), the guest printed the main
/// executable's line and then died with SIGSEGV (exit 139) in the constructor,
/// the defect that also broke `import hashlib`
/// (`liteinst_in_guest_python_random_example`).
#[test]
fn liteinst_in_guest_cpuid_in_a_late_loaded_library_runs() {
    let _guard = hermit_run_guard();
    let (guest, library) = late_cpuid_guest();
    let check = |output: &Output, backend: &str| {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status
        );
        let lines = stdout.lines().collect::<Vec<_>>();
        let [main, loaded] = lines[..] else {
            panic!("{backend}: expected two vendor lines, got {stdout:?}");
        };
        let main = main
            .strip_prefix("main-vendor=")
            .unwrap_or_else(|| panic!("{backend}: no main-vendor line in {stdout:?}"));
        let loaded = loaded
            .strip_prefix("library-vendor=")
            .unwrap_or_else(|| panic!("{backend}: no library-vendor line in {stdout:?}"));
        assert!(!main.is_empty(), "{backend}: empty vendor in {stdout:?}");
        assert_eq!(main, loaded, "{backend}: {stdout:?}");
    };

    let control = Command::new(hermit_binary())
        .args(["--log=error", "--backend=ptrace", "run"])
        .args([
            "--strict",
            "--max-timeslice=disabled",
            "--base-env=minimal",
            "--",
        ])
        .arg(guest)
        .arg(library)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the late-CPUID guest under ptrace");
    check(&control, "ptrace");

    let output = liteinst_command("error")
        .arg("--")
        .arg(guest)
        .arg(library)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the late-CPUID guest under in-guest LiteInst");
    check(&output, "liteinst");
    assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
}

/// A forking guest finishes under in-guest LiteInst.
///
/// `fork-ok` is the guest's own end-of-mode line, printed only after the child
/// has been created, run and reaped (see `tests/c/liteinst_advanced.c`), so a
/// regression that skips the child cannot satisfy this by exiting zero. The
/// negative assertions carry the two failure shapes the hybrid test existed
/// for: `ENOTSUPP` is a refused task creation, and `Bad system call` is a
/// SIGSYS the guest must never receive. The guest's `threads` mode is not run
/// here: in-guest LiteInst refuses thread clone ("clone injection requires
/// ptrace fallback").
#[test]
fn liteinst_in_guest_fork_runs_without_hanging() {
    let _guard = hermit_run_guard();
    let mut command = liteinst_command("error");
    let mut child = command
        .arg("--")
        .arg(advanced_guest())
        .arg("fork")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start Hermit LiteInst fork guest");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung running fork");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst fork output");
    assert_eq!(output.status, status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        status.code(),
        Some(0),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert!(
        stdout.contains("fork-ok"),
        "guest did not reach the end of fork\nstdout={stdout}\nstderr={stderr}",
    );
    assert!(
        !stderr.contains("ENOTSUPP (Operation is not supported)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Bad system call"), "{stderr}");
    assert_in_guest_selected(&stderr);
}

#[test]
fn liteinst_in_guest_abnormal_exit_after_registration_does_not_hang() {
    let _guard = hermit_run_guard();
    // INFO-level Detcore diagnostics can exceed a pipe's capacity before the
    // guest reaches its fatal signal. Keep draining out of the child process
    // while retaining the diagnostics for the scheduler-start assertion.
    let mut stderr = tempfile::tempfile().expect("create LiteInst diagnostic sink");
    let stderr_sink = stderr.try_clone().expect("clone LiteInst diagnostic sink");
    let mut command = liteinst_command("info");
    let mut child = command
        .args(["--", "/bin/sh", "-c", "kill -9 $$"])
        .stdin(Stdio::null())
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
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "{output:?}\nstderr={diagnostics}"
    );
    assert_eq!(output.status, status);
    assert!(
        diagnostics.contains("[scheduler] guest in queue"),
        "stderr={diagnostics}",
    );
    assert_in_guest_selected(&diagnostics);
}

// Regression coverage for https://github.com/rrnewton/hermit/issues/3338: the
// LiteInst runtime's preload constructor issues a few hundred syscalls before
// the guest's main runs. Charging them as guest syscalls pushed sysinfo(2)
// uptime from 121 to 123 under --max-timeslice=disabled, where each syscall is
// charged at the no-PMU rate.
static BOOTSTRAP_TIME_HOST_IDENTITY: OnceLock<PathBuf> = OnceLock::new();

fn bootstrap_time_host_identity() -> &'static Path {
    // The unchanged host-identity fixture. It asserts sysinfo.uptime == 121.
    BOOTSTRAP_TIME_HOST_IDENTITY.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("liteinst-bootstrap-time");
        fs::create_dir_all(&build_root).expect("failed to create bootstrap-time guest directory");
        let guest = build_root.join("host_identity");
        // -D_GNU_SOURCE matches the build flags that the c-programs/host-identity
        // cell in tests/e2e/manifests/c-programs.yaml gives host_identity.c.
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-D_GNU_SOURCE"])
            .arg(repository.join("tests/c/host_identity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile tests/c/host_identity.c");
        assert!(
            output.status.success(),
            "tests/c/host_identity.c compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// The runtime's own bootstrap is not charged to the guest's virtual time.
/// The hybrid version also ran `--verify --verify-strict --verify-json` and
/// required a matched, bitwise-parity report; that half is not restored yet
/// (https://github.com/rrnewton/hermit/issues/3520).
#[test]
fn liteinst_in_guest_runtime_bootstrap_is_not_charged_to_host_identity_uptime() {
    let _guard = hermit_run_guard();
    let home = tempfile::tempdir().expect("failed to create bootstrap-time HOME");
    let output = liteinst_command_at_epoch("info", Some(VIRTUAL_TIME_EPOCH))
        .args(["--env=LC_ALL=C", "--env=TZ=UTC"])
        .arg(format!("--env=HOME={}", home.path().display()))
        .env("HOME", home.path())
        .arg("--")
        .arg(bootstrap_time_host_identity())
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst on host_identity");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert!(
        stdout.lines().any(|line| line == "sysinfo.uptime=121"),
        "LiteInst host_identity must observe uptime 121:\nstdout={stdout}\nstderr={stderr}"
    );
    assert_in_guest_selected(&stderr);
}

/// LiteInst's dispatch record: it measures its patch candidates and finds some
/// in the guest. `--max-timeslice=disabled` because in-guest LiteInst refuses
/// a maximum timeslice.
#[test]
fn liteinst_in_guest_dispatch_record_reports_patched_sites() {
    let _guard = hermit_run_guard();
    let guest = dispatch_stats::build_guest("guest-liteinst", &[]);
    let record = dispatch_stats::dispatch_record(
        "liteinst",
        hermit_binary(),
        &["--max-timeslice=disabled"],
        &[],
        &guest,
    );
    let candidates = record
        .sites
        .candidates
        .expect("LiteInst measures its candidates");
    assert!(candidates > 0, "{record}");
}

fn userfaultfd_guest() -> &'static Path {
    USERFAULTFD_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("userfaultfd_self_service");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/userfaultfd_self_service.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the userfaultfd guest");
        assert!(
            output.status.success(),
            "tests/c/userfaultfd_self_service.c compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// Coord ruling D.2: userfaultfd stays supported under in-guest LiteInst,
/// whose exits hold the schedule until they are physically complete. One
/// process serves its own faults from one thread (tests/c/
/// userfaultfd_self_service.c, which c-programs.yaml runs on ptrace): it
/// resolves its registered pages ahead of use with `UFFDIO_COPY`, reads them,
/// and exits still registered. The run completes, matches under strict
/// verification, and prints what the pages hold. Skipped, with the reason
/// printed, where the kernel refuses an unprivileged userfaultfd.
#[test]
fn liteinst_in_guest_serves_its_own_userfaultfd_and_exits_registered() {
    // Skip only when the host itself refuses an unprivileged userfaultfd, so
    // a backend failure can never pass as an unavailable host.
    let native = Command::new(userfaultfd_guest())
        .output()
        .expect("failed to run the userfaultfd guest natively");
    let native_stdout = String::from_utf8_lossy(&native.stdout);
    assert!(native.status.success(), "native run failed: {native:?}");
    if let Some(reason) = native_stdout.strip_prefix("userfaultfd-unavailable ") {
        eprintln!(
            "skipping: this host refuses an unprivileged userfaultfd ({})",
            reason.trim()
        );
        return;
    }
    assert_eq!(native_stdout, "userfaultfd-served pages=2 sum=798720\n");
    let _guard = hermit_run_guard();
    let output = liteinst_command("info")
        .args(["--verify", "--verify-strict"])
        .arg("--")
        .arg(userfaultfd_guest())
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst on the userfaultfd guest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert_eq!(
        stdout, "userfaultfd-served pages=2 sum=798720\n",
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(stderr.contains("bitwise parity established"), "{stderr}");
    assert_in_guest_selected(&stderr);
}

/// Coord ruling A: a guest that holds a FUSE device could be the server
/// another guest's exit-time flush waits for, which can never be answered
/// while Hermit holds every turn until that exit completes. In-guest LiteInst
/// refuses the open by name, before the guest's next line runs; ptrace runs
/// the same program. Skipped, with the reason printed, without /dev/fuse.
#[test]
fn liteinst_in_guest_refuses_a_guest_that_opens_dev_fuse() {
    if let Err(error) = fs::File::open("/dev/fuse") {
        eprintln!("skipping: /dev/fuse cannot be opened on this host ({error})");
        return;
    }
    let _guard = hermit_run_guard();
    let output = liteinst_command("info")
        .args(["--", "/bin/sh", "-c", ": < /dev/fuse; echo opened"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains("opened"), "{stdout}");
    // Detcore names the capability; the in-guest runtime adds the backend.
    assert!(
        stderr.contains("refusing a guest that holds a FUSE device (/dev/fuse)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("this backend completes process exits asynchronously"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "--backend=liteinst (in-guest LiteInst) cannot run this program; run it with \
             --backend=ptrace."
        ),
        "{stderr}"
    );
    assert_in_guest_selected(&stderr);

    // ptrace runs the same program: the refusal is in-guest LiteInst's.
    let output = Command::new(hermit_binary())
        .args(["--log=info", "run", "--strict", "--"])
        .args(["/bin/sh", "-c", ": < /dev/fuse; echo opened"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit with ptrace");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "opened\n");
}

/// The other half of ruling A: a plain file on a FUSE filesystem is not
/// refused. Its server is outside the guest set, so no exit waits for a guest.
/// Uses the first regular file at the root of a FUSE mount this host has;
/// skipped, with the reason printed, on a host with none.
#[test]
fn liteinst_in_guest_reads_a_plain_file_on_a_fuse_filesystem() {
    let mounts = fs::read_to_string("/proc/self/mounts").expect("reading /proc/self/mounts");
    let file = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let mount = fields.nth(1)?;
            let kind = fields.next()?;
            (kind.starts_with("fuse") && kind != "fusectl").then(|| PathBuf::from(mount))
        })
        .find_map(|mount| {
            fs::read_dir(&mount).ok()?.flatten().find_map(|entry| {
                let path = entry.path();
                let metadata = fs::metadata(&path).ok()?;
                (metadata.is_file() && metadata.len() > 0 && fs::File::open(&path).is_ok())
                    .then_some(path)
            })
        });
    let Some(file) = file else {
        eprintln!("skipping: this host has no readable regular file at a FUSE mount's root");
        return;
    };
    let mut expected = vec![0u8; 16];
    let read = fs::File::open(&file)
        .and_then(|mut opened| opened.read(&mut expected))
        .expect("reading the FUSE file natively");
    expected.truncate(read);
    let _guard = hermit_run_guard();
    let output = liteinst_command("info")
        .args(["--", "/usr/bin/head", "-c", "16"])
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst");
    let output = assert_liteinst_in_guest_output(output);
    assert_eq!(output.stdout, expected, "{}", file.display());
}

/// Compiles `tests/c/<name>.c` once per test process into `cell`.
fn c_guest(cell: &'static OnceLock<PathBuf>, name: &str) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join(name);
        let source = repository.join(format!("tests/c/{name}.c"));
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to run cc");
        assert!(
            output.status.success(),
            "{} compilation failed:\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// The C3.5 auto-reap and SIGCHLD cases (dev-hermit
/// ai_docs/transient/liteinst-inguest-exit-control-design-20261006.md,
/// "Evidence planned"), under strict verification on ptrace and on in-guest
/// LiteInst, whose exits hold the schedule until they are physically
/// complete. The parent blocks SIGCHLD and learns of each exit from EOF on a
/// pipe the child held. Every run is checked against Linux's output, per
/// backend; the two backends print the same lines in the first two modes and
/// differ, by one named LiteInst deviation, in the third:
///
/// - SA_NOCLDWAIT: wait reports ECHILD (auto-reaped) and SIGCHLD is pending,
///   as on Linux, on both backends.
/// - a raw exit system call: SIGCHLD is pending and wait returns status 7, as
///   on Linux, on both backends.
/// - SIGCHLD ignored: wait reports ECHILD, as on Linux. Linux sends no
///   SIGCHLD to a parent that ignores it (`do_notify_parent`), so nothing is
///   pending. ptrace prints that since SIGCHLD Phase A; in-guest LiteInst
///   still leaves SIGCHLD pending, a known deviation pinned at its current
///   value (dev-hermit
///   ignored/parity-green-issues/liteinst-sigign-sigchld-pending.md). This
///   mode is not a parity check; the name predates it.
#[test]
fn liteinst_in_guest_exit_reaping_matches_ptrace() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&EXIT_REAPING_GUEST, "exit_reaping_probe");
    // Each backend's expected SIGCHLD-pending value, which is Linux's (the
    // fixture's header) except where a backend's deviation is named.
    //
    // sigign: Linux sends no SIGCHLD when the parent explicitly ignores it,
    // even while it is blocked (do_notify_parent sets sig = 0), so nothing is
    // pending. ptrace matches Linux since SIGCHLD Phase A stopped its
    // scheduler sending a synthetic SIGCHLD there. In-guest LiteInst still
    // leaves SIGCHLD pending: a known deviation, dev-hermit
    // ignored/parity-green-issues/liteinst-sigign-sigchld-pending.md. It is
    // pinned at its current value, which is what this test enforced before
    // through ptrace's (then equally wrong) output.
    for (mode, ptrace_pending, liteinst_pending, wait, status) in [
        ("sigign", "0", "1", "ECHILD", -1),
        ("nocldwait", "1", "1", "ECHILD", -1),
        ("rawexit", "1", "1", "child", 7),
    ] {
        let mut outputs = Vec::new();
        for backend in ["ptrace", "liteinst"] {
            let pending = if backend == "ptrace" {
                ptrace_pending
            } else {
                liteinst_pending
            };
            let mut command = Command::new(hermit_binary());
            command.args(["--log=info", "--backend", backend, "run"]);
            if backend == "liteinst" {
                command.arg("--max-timeslice=disabled");
            }
            let output = command
                .args(["--strict", "--verify", "--verify-strict", "--"])
                .arg(guest)
                .arg(mode)
                .stdin(Stdio::null())
                .output()
                .expect("failed to run Hermit");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{backend} {mode}: status={:?}\nstdout={stdout}\nstderr={stderr}",
                output.status
            );
            assert!(
                stderr.contains("bitwise parity established"),
                "{backend} {mode}: {stderr}"
            );
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines.len(), 3, "{backend} {mode}: {stdout}");
            for (round, line) in lines.iter().enumerate() {
                let prefix = format!("{mode} round={round} read=0 sigchld_pending=");
                let suffix = format!(" wait={wait} status={status}");
                assert!(
                    line.starts_with(&prefix) && line.ends_with(&suffix),
                    "{backend} {mode}: {line}"
                );
                assert_eq!(
                    *line,
                    format!("{prefix}{pending}{suffix}"),
                    "{backend} {mode}"
                );
            }
            outputs.push(stdout);
        }
        if ptrace_pending == liteinst_pending {
            assert_eq!(
                outputs[0], outputs[1],
                "{mode}: ptrace and in-guest LiteInst differ"
            );
        }
    }
}

/// Processes that die at a moment the host chooses, without an exit system
/// call of their own: a child killed by PR_SET_PDEATHSIG while its parent
/// exits, and a vfork parent killed by its child while the grandparent goes
/// on. In-guest LiteInst cannot schedule such a death, so it retires the
/// process when its exit completes and records a determinism loss (the C3.5
/// design's consumption case). Each run completes with the program's output,
/// never a hang, and verification refuses to compare it, never a match.
#[test]
fn liteinst_in_guest_unscheduled_deaths_complete_and_refuse_verification() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&UNSCHEDULED_EXIT_GUEST, "unscheduled_exit_probe");
    for (mode, expected) in [
        (
            "pdeathsig",
            "parent exits with its child armed\npdeathsig parent-status=0 armed-child-gone=1\n",
        ),
        (
            "vfork-parent-killed",
            "vfork-parent-killed parent-signal=9 surviving-work-done=1\n",
        ),
    ] {
        let output = liteinst_command("info")
            .arg("--")
            .arg(guest)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit LiteInst");
        let output = assert_liteinst_in_guest_output(output);
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{mode}");

        let output = liteinst_command("info")
            .args(["--verify", "--verify-strict", "--"])
            .arg(guest)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit LiteInst with --verify");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{mode}: verification matched: {stderr}"
        );
        assert!(
            stderr.contains("run 1: determinism loss recorded: process ")
                && stderr.contains(" exited without deregistering; its exit was not scheduled"),
            "{mode}: {stderr}"
        );
        assert!(!stderr.contains("Determinism verified"), "{mode}: {stderr}");
    }
}

/// Under `--max-log-bytes`, Hermit writes its in-guest selection line
/// ([`IN_GUEST_SELECTED`]) before the guest starts, so before the cap or the
/// guest's exit can end the run. With stderr a full pipe nobody reads, the run
/// must still end within the bound of
/// `max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads`.
/// In-guest LiteInst refuses `--timeout`, so the run has none.
#[test]
fn liteinst_in_guest_max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe() {
    super::CappedRun {
        build: |args| {
            let mut command = Command::new(hermit_binary());
            command.args(args);
            command
        },
        global: &["--backend", "liteinst"],
        run_options: &[
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ],
        timeout: false,
        guest: &["/bin/true"],
        exit_code: 0,
        prints: Some(IN_GUEST_SELECTED),
        ..super::CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// Two descriptors aliasing one open file description (after dup) still
/// share it in a forked child. In-guest LiteInst serializes the parent's
/// Detcore state into the child, and before interning by OpenFileId the
/// child's aliases came back as two copies: both read the same random bytes.
/// The child's two reads must continue one stream, exactly as under ptrace.
#[test]
fn liteinst_in_guest_dup_aliases_share_one_cursor_after_fork() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&DUP_ALIAS_GUEST, "dup_alias_after_fork");
    let mut outputs = Vec::new();
    for backend in ["ptrace", "liteinst"] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=info", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = command
            .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
            .args(["--strict", "--"])
            .arg(guest)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{backend}: {output:?}");
        assert!(
            stdout.contains("aliases continue one stream=1"),
            "{backend}: {stdout}"
        );
        outputs.push(stdout);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "in-guest LiteInst differs from ptrace"
    );
}

/// close_range(3, ~0U, 0) closes a bound socket, and Detcore must account
/// for it like a close: the socket's port goes back to the allocator, so the
/// next bind to port 0 gets the same port. Under in-guest LiteInst the range
/// also covers the runtime's coordinator connection, and the runtime once
/// answered such a call itself, before Detcore saw it: the port was never
/// released and the second bind got the next port. Both backends must reuse
/// the port and print the same output.
#[test]
fn liteinst_in_guest_close_range_releases_ports_like_ptrace() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&CLOSE_RANGE_PORT_GUEST, "close_range_releases_port");
    let mut outputs = Vec::new();
    for backend in ["ptrace", "liteinst"] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=info", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = command
            .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
            .args(["--strict", "--"])
            .arg(guest)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout, "port reused=1\n", "{backend}: {output:?}");
        outputs.push(stdout);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "in-guest LiteInst differs from ptrace"
    );
}

/// Under `--verify` a recvmsg whose buffers cannot be observed after the call
/// still returns the kernel's result to the guest: the buffer digest records
/// an explicit, compared `unobserved` entry instead of hashing, and the verify
/// report counts it. A plain receive is hashed as before. In the overwritten
/// mode the guest's iovec array is its own receive buffer, so the payload
/// overwrites the iovec with one byte at an unmapped address; before the
/// entry, the failed read turned the guest's completed recvmsg into EFAULT
/// under --verify.
#[test]
fn verify_digest_records_an_unobservable_recvmsg_and_keeps_the_kernel_result() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&IOV_OVERWRITTEN_GUEST, "recvmsg_iov_overwritten_by_payload");
    let logs = tempfile::tempdir().expect("failed to create the verify-log directory");
    for (mode, unobservable) in [("plain", false), ("overwritten", true)] {
        let log_dir = logs.path().join(mode);
        fs::create_dir(&log_dir).expect("failed to create a verify-log directory");
        let output = Command::new(hermit_binary())
            .args([
                "--log",
                "info",
                "--backend",
                "ptrace",
                "run",
                "--strict",
                "--verify",
            ])
            .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
            .arg("--keep-logs")
            .arg("--verify-log-dir")
            .arg(&log_dir)
            .arg("--")
            .arg(guest)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {output:?}");
        assert!(stderr.contains("Determinism verified"), "{mode}: {stderr}");
        let buffer = stdout
            .split_whitespace()
            .find_map(|field| field.strip_prefix("buffer="))
            .unwrap_or_else(|| panic!("{mode}: {stdout}"));
        let expected_stdout = format!(
            "buffer={buffer} received=16 overwritten={}\n",
            u8::from(unobservable)
        );
        assert_eq!(stdout, expected_stdout, "{mode}");
        let hashed = format!("recvmsg in fd=4 {buffer}+16->");
        let marker = "recvmsg in fd=4 unobserved ret=16 reason=buffer-unreadable at=";
        let counted = "unobservable after the call (compared as 'unobserved' buffer-digest entries): run1=1, run2=1";
        let golden: Vec<PathBuf> = fs::read_dir(&log_dir)
            .expect("failed to read the retained verify logs")
            .map(|entry| entry.expect("failed to read a retained log").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("run1_log_"))
            })
            .collect();
        assert_eq!(golden.len(), 1, "{mode}: {golden:?}");
        let log = fs::read_to_string(&golden[0]).expect("failed to read the retained log");
        let digests: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("[iobuf]") && line.contains("recvmsg in"))
            .collect();
        if unobservable {
            assert!(
                digests.len() == 1 && digests[0].contains(marker),
                "{mode}: expected one {marker:?} entry: {digests:?}"
            );
            assert!(stderr.contains(counted), "{mode}: {stderr}");
        } else {
            assert!(
                digests.len() == 1 && digests[0].contains(&hashed),
                "{mode}: expected {hashed:?}: {digests:?}"
            );
            assert!(
                !stderr.contains("unobservable after the call"),
                "{mode}: {stderr}"
            );
        }
    }
}

static USER_ADDRESS_ERRNO_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The guest for
/// `liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno`. Its
/// probes all go through one raw `read` instruction (`raw_read`, written in
/// assembly so the compiler cannot copy it), which sets no `errno`, so any
/// change to `errno` across a probe was made by whoever served it. The
/// function has unwind information (`.cfi_startproc`) and room after the
/// instruction, which LiteInst needs before it patches a site. The guest asks
/// the in-guest LiteInst runtime, when it is loaded, how often that
/// instruction's installed hook was entered.
const USER_ADDRESS_ERRNO_GUEST_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

long raw_read(long fd, void *buf, unsigned long count);
/* SYS_read is 0; the five-byte mov puts the syscall at raw_read + 5. */
#define READ_SITE_OFFSET 5
__asm__(".text\n"
        ".p2align 4\n"
        ".globl raw_read\n"
        ".type raw_read, @function\n"
        "raw_read:\n"
        "\t.cfi_startproc\n"
        "\tmovl $0, %eax\n"
        "\tsyscall\n"
        "\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n"
        "\tret\n"
        "\t.cfi_endproc\n"
        ".size raw_read, .-raw_read\n");

typedef uint64_t (*hook_count_fn)(uint64_t);

int main(void) {
    hook_count_fn hook_count =
        (hook_count_fn)dlsym(RTLD_DEFAULT, "reverie_liteinst_site_hook_count");
    const unsigned char *site_bytes = (const unsigned char *)raw_read + READ_SITE_OFFSET;
    uint64_t site = (uint64_t)(uintptr_t)site_bytes;
    if (site_bytes[0] != 0x0f || site_bytes[1] != 0x05) {
        printf("raw_read + %d is not a syscall instruction\n", READ_SITE_OFFSET);
        return 1;
    }
    uint64_t hooks[4] = {0, 0, 0, 0};
    char byte = 0;
    int rng = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (rng < 0) {
        perror("open /dev/urandom");
        return 1;
    }
    if (hook_count) hooks[0] = hook_count(site);
    /* An invalid descriptor: Detcore does not ask for the user address
       limit, and the instruction becomes a patched site. */
    long warm = raw_read(-1, &byte, 1);
    if (hook_count) hooks[1] = hook_count(site);
    /* A zero-length random-device read: Detcore checks the buffer against
       the user address limit, which this process has not measured yet. */
    errno = 4242;
    long cold = raw_read(rng, &byte, 0);
    int cold_errno = errno;
    if (hook_count) hooks[2] = hook_count(site);
    /* The same read of a buffer beyond the limit, now cached: EFAULT. */
    errno = 4243;
    long cached = raw_read(rng, (void *)UINTPTR_MAX, 0);
    int cached_errno = errno;
    if (hook_count) hooks[3] = hook_count(site);
    printf("warm=%ld\n", warm);
    printf("cold=%ld errno=%d\n", cold, cold_errno);
    printf("cached=%ld errno=%d\n", cached, cached_errno);
    if (hook_count) {
        printf("hook entries per probe: warm=%lu cold=%lu cached=%lu\n",
               (unsigned long)(hooks[1] - hooks[0]), (unsigned long)(hooks[2] - hooks[1]),
               (unsigned long)(hooks[3] - hooks[2]));
    } else {
        printf("hook entries per probe: no in-guest runtime\n");
    }
    return 0;
}
"#;

fn user_address_errno_guest() -> &'static Path {
    USER_ADDRESS_ERRNO_GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let source = build_root.join("user_address_errno.c");
        fs::write(&source, USER_ADDRESS_ERRNO_GUEST_SOURCE)
            .expect("failed to write the user-address errno guest");
        let guest = build_root.join("user_address_errno");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .arg("-ldl")
            .output()
            .expect("failed to run cc");
        assert!(
            output.status.success(),
            "{} compilation failed:\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// In-guest LiteInst runs Detcore on the guest's own thread, so the guest and
/// Detcore share one C `errno`. Detcore asks the backend for the user address
/// limit (`Guest::user_address_limit`) before it checks a random-device read's
/// buffer; on this backend the answer is measured in the guest process with
/// failing `process_vm_readv` range checks, which set `errno` to `EFAULT`.
/// The query must leave the guest's `errno` as it found it, also when Detcore
/// is entered through a patched site's installed hook, which, unlike the
/// SIGSYS fallback, does not restore `errno` itself.
///
/// One raw `read` instruction serves every probe: an invalid descriptor first
/// (no query; the instruction becomes a patched site), then a zero-length
/// random-device read (the first query, which measures), then the same read
/// of a buffer at the top of the address space (the cached answer, EFAULT).
/// Before each probe the guest sets a sentinel `errno` that no system call
/// sets. Linux, ptrace and in-guest LiteInst must print the same results with
/// the sentinel kept, and LiteInst must have entered the instruction's
/// installed hook once per probe.
#[test]
fn liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno() {
    const PROBES: &str = "warm=-9\ncold=0 errno=4242\ncached=-14 errno=4243\n";
    let guest = user_address_errno_guest();
    let native = Command::new(guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the user-address errno guest natively");
    assert!(native.status.success(), "native: {native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        format!("{PROBES}hook entries per probe: no in-guest runtime\n"),
        "native: {native:?}"
    );
    let _guard = hermit_run_guard();
    for (backend, hooks) in [
        ("ptrace", "no in-guest runtime"),
        ("liteinst", "warm=1 cold=1 cached=1"),
    ] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=off", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = command
            .args(["--strict", "--"])
            .arg(guest)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(
            stdout,
            format!("{PROBES}hook entries per probe: {hooks}\n"),
            "{backend}: {output:?}"
        );
        if backend == "liteinst" {
            assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
        }
    }
}
