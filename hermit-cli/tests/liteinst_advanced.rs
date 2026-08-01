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
static LITEINST_COMPAT_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_SEMANTIC_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPRESSED_FIXTURES: OnceLock<[PathBuf; 3]> = OnceLock::new();
static LITEINST_ARCHIVE_FIXTURES: OnceLock<[PathBuf; 3]> = OnceLock::new();
static LITEINST_CPIO_FIXTURE: OnceLock<PathBuf> = OnceLock::new();

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

fn archive_fixtures() -> &'static [PathBuf; 3] {
    LITEINST_ARCHIVE_FIXTURES.get_or_init(|| {
        let source = compatibility_fixture();
        let build_root = source
            .parent()
            .expect("compatibility fixture should have a parent directory");
        let filename = source
            .file_name()
            .expect("compatibility fixture should have a filename");
        let tar_path = build_root.join("compatibility-fixture.tar");
        let ar_path = build_root.join("compatibility-fixture.a");
        let zip_path = build_root.join("compatibility-fixture.zip");

        let mut tar = Command::new("/usr/bin/tar");
        tar.arg("-cf")
            .arg(&tar_path)
            .arg("-C")
            .arg(build_root)
            .arg(filename);
        let mut ar = Command::new("/usr/bin/ar");
        ar.arg("rcs").arg(&ar_path).arg(source);
        let mut zip = Command::new("/usr/bin/zip");
        zip.args(["-q", "-j"]).arg(&zip_path).arg(source);

        [
            ("/usr/bin/tar", tar_path, tar),
            ("/usr/bin/ar", ar_path, ar),
            ("/usr/bin/zip", zip_path, zip),
        ]
        .map(|(program, path, mut command)| {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to replace {}: {error}", path.display()),
            }
            let output = command
                .output()
                .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
            assert!(
                output.status.success(),
                "{program} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            path
        })
    })
}

fn cpio_fixture() -> &'static Path {
    LITEINST_CPIO_FIXTURE.get_or_init(|| {
        let source = compatibility_fixture();
        let build_root = source
            .parent()
            .expect("compatibility fixture should have a parent directory");
        let filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .expect("compatibility fixture filename should be UTF-8");
        let path = build_root.join("compatibility-fixture.cpio");

        let mut child = Command::new("/usr/bin/cpio")
            .args(["-o", "-H", "newc"])
            .current_dir(build_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run /usr/bin/cpio");
        writeln!(
            child
                .stdin
                .take()
                .expect("cpio fixture stdin pipe should exist"),
            "{filename}"
        )
        .expect("failed to write cpio fixture member name");
        let output = child
            .wait_with_output()
            .expect("failed to collect cpio fixture output");
        assert!(
            output.status.success(),
            "cpio failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        fs::write(&path, output.stdout).expect("failed to write cpio fixture");
        path
    })
}

fn run_liteinst(program: &Path, args: &[&str], verify: bool) -> Output {
    run_liteinst_with_input(program, args, verify, None)
}

fn run_liteinst_with_input(
    program: &Path,
    args: &[&str],
    verify: bool,
    input: Option<&[u8]>,
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

fn run_liteinst_strict_verify(program: &Path, args: &[&str]) -> Output {
    assert_liteinst_strict_verify_output(run_liteinst(program, args, true))
}

fn run_liteinst_strict_verify_with_stdin(program: &Path, args: &[&str], input: &[u8]) -> Output {
    assert_liteinst_strict_verify_output(run_liteinst_with_input(program, args, true, Some(input)))
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
fn liteinst_strict_verify_round4_stdin_transform_utilities() {
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/expand"),
        &["-t", "4"],
        b"a\tb\n",
    );
    assert_eq!(output.stdout, b"a   b\n");

    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/unexpand"),
        &["-a", "-t", "4"],
        b"a   b\n",
    );
    assert_eq!(output.stdout, b"a\tb\n");

    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/m4"),
        &["-DNAME=liteinst"],
        b"NAME-round4\n",
    );
    assert_eq!(output.stdout, b"liteinst-round4\n");

    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/openssl"),
        &["enc", "-base64", "-A"],
        b"liteinst-openssl\n",
    );
    assert_eq!(output.stdout, b"bGl0ZWluc3Qtb3BlbnNzbAo=");

    let output =
        run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/bc"), &[], b"scale=3; 1/8\n");
    assert_eq!(output.stdout, b".125\n");

    let output = run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/dc"), &[], b"5 7 + p\n");
    assert_eq!(output.stdout, b"12\n");
}

#[test]
fn liteinst_strict_verify_round4_encoding_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/basenc"),
        &["--base16", fixture],
        b"6C697465696E737420636F6D7061746962696C69747920666978747572650A\n",
    );
    assert_liteinst_strict_verify(Path::new("/usr/bin/c++filt"), &["_Z3foov"], b"foo()\n");
}

#[test]
fn liteinst_strict_verify_round4_archive_utilities() {
    let filename = compatibility_fixture()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("compatibility fixture filename should be UTF-8");
    let [tar_fixture, ar_fixture, zip_fixture] = archive_fixtures();

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/tar"),
        &[
            "-xOf",
            tar_fixture
                .to_str()
                .expect("tar fixture path should be UTF-8"),
            filename,
        ],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/ar"),
        &[
            "p",
            ar_fixture
                .to_str()
                .expect("ar fixture path should be UTF-8"),
            filename,
        ],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/unzip"),
        &[
            "-p",
            zip_fixture
                .to_str()
                .expect("zip fixture path should be UTF-8"),
            filename,
        ],
        COMPAT_FIXTURE_CONTENT,
    );
}

#[test]
fn liteinst_strict_verify_round5_language_utilities() {
    let lua = [Path::new("/usr/bin/lua5.4"), Path::new("/usr/bin/lua")]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .expect("portable CI should provide Lua 5.4");
    assert_liteinst_strict_verify(
        lua,
        &["-e", "print(table.concat({1,4,9}, ','))"],
        b"1,4,9\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/ruby"),
        &["--disable-gems", "-e", "puts [1,4,9].join(',')"],
        b"1,4,9\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/php"),
        &["-r", "echo implode(',', [1,4,9]), PHP_EOL;"],
        b"1,4,9\n",
    );
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/tclsh"),
        &[],
        b"puts [join [list 1 4 9] \",\"]\n",
    );
    assert_eq!(output.stdout, b"1,4,9\n");
}

#[test]
fn liteinst_strict_verify_round5_portable_data_utilities() {
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/xmllint"),
        &["--xpath", "string(/root/item[@id=\"2\"])", "-"],
        b"<root><item id=\"2\">beta</item></root>\n",
    );
    assert_eq!(output.stdout, b"beta\n");

    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/file"),
        &["-b", "--mime-type", fixture],
        b"text/plain\n",
    );

    let cpio = fs::read(cpio_fixture()).expect("failed to read cpio fixture");
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/cpio"),
        &["-i", "--to-stdout", "compatibility-fixture.txt"],
        &cpio,
    );
    assert_eq!(output.stdout, COMPAT_FIXTURE_CONTENT);

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/xxd"),
        &["-p", fixture],
        b"6c697465696e737420636f6d7061746962696c6974792066697874757265\n0a\n",
    );

    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/column"),
        &["-t"],
        b"alpha 1\nbeta 22\n",
    );
    assert_eq!(output.stdout, b"alpha  1\nbeta   22\n");
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
