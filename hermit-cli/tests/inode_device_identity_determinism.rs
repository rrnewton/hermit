/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for https://github.com/rrnewton/hermit/issues/2897.
//!
//! Hermit used to number deterministic inodes from one counter shared by every
//! filesystem, in first-observation order. A host file replaced while the guest
//! ran therefore shifted every inode minted after it on every filesystem. In the
//! compat workloads `chef` renamed a fresh `/etc/ld.so.cache` into place between
//! the two runs of `--verify`; the second run minted one more host inode, so the
//! guest's tmpfs work directory was inode 22 instead of 21 and `getdents64` of
//! that empty directory differed.
//!
//! This test reproduces that without waiting for `chef`. The same guest program
//! runs twice over a host directory holding `a` and `b`. In one run they are
//! hard links to one host inode; in the other they are two host files, which is
//! what a replacement looks like to the inode pool. The guest stats both names
//! and then reports the inodes of a directory it creates on its own tmpfs, both
//! through `stat` and through `getdents64` (`ls -ai`). Those tmpfs inodes must not
//! depend on how many host inodes a different filesystem holds.
//!
//! Keying on `(st_dev, st_ino)` must not split one file in two where
//! `/proc/<pid>/maps` names a different device than `stat` does (btrfs reports
//! `00:20` in maps and `0x21` from `stat` on the development host). The kernel
//! maps the shell's own executable at execve and nothing `stat`s it, so reading
//! the shell's maps before `stat`ing `/proc/<pid>/exe` resolves the maps
//! identity first; the second test checks that both orders agree.
//!
//! Hermit's own stdin, stdout and stderr are the guest's inherited stdio
//! objects, and they are wherever the person running hermit pointed them: a
//! terminal, a file, a pipe, /dev/null. With one counter per device, a
//! deterministic inode minted for one of them took a slot if its device was
//! new, or a counter value if not, and so shifted the inodes of unrelated files
//! with how hermit was invoked. That happened whenever the guest reached the
//! object through a descriptor other than `fstat` of fds 0 to 2: opening or
//! `stat`ing `/dev/stdout`, `/proc/self/fd/1` or `/dev/fd/3`, `fstat` or
//! fdinfo of a duplicate above fd 2, `statx` with `AT_EMPTY_PATH`, and the
//! virtual-mtime bump of a write. Those routes now report the fixed stdio
//! inodes (1000 + fd, or the stream's for a descriptor above fd 2) without
//! consulting the inode pool. The object's own path still goes through the
//! pool, so it gets the same inode whether or not it is also hermit's stdio.
//! The third test runs the same guest with byte-identical stdin from two
//! filesystems, and the guest reads that file by its own path. The fourth and
//! fifth reach hermit's stdout, then its stdin, through each descriptor route
//! with the stream on /dev/null, on a regular file and on a pipe, and require
//! identical output.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

const GUEST_SCRIPT: &str = r#"
set -eu
stat -c '%n' "$1/a" "$1/b" > /dev/null
mkdir /test/d
printf 'stat:%s\n' "$(stat -c %i /test/d)"
ls -ai /test/d
"#;

/// Per-test scratch directory under the Cargo target tmpdir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "inode-device-identity-{}-{test}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("failed to create the scratch directory");
        ScratchDir(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn host_directory(scratch: &ScratchDir, name: &str, hard_linked: bool) -> PathBuf {
    let dir = scratch.0.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the host directory");
    fs::write(dir.join("a"), b"same contents\n").expect("failed to write a");
    if hard_linked {
        fs::hard_link(dir.join("a"), dir.join("b")).expect("failed to link b to a");
    } else {
        fs::write(dir.join("b"), b"same contents\n").expect("failed to write b");
    }
    dir
}

/// The shell's executable inode as its maps header reports it and as `stat`
/// reports it, in the order the arguments name.
const EXECUTABLE_INODE_SCRIPT: &str = r#"
set -eu
exe=$(readlink /proc/$$/exe)
maps() { awk -v exe="$exe" '$6 == exe { print $5; exit }' /proc/$$/maps; }
exe_stat() { stat -L -c %i /proc/$$/exe; }
for source in "$@"; do
    case "$source" in
        maps) printf 'maps:%s\n' "$(maps)" ;;
        stat) printf 'stat:%s\n' "$(exe_stat)" ;;
    esac
done
"#;

/// Where hermit's own standard input comes from.
#[derive(Clone, Copy, Debug)]
enum HermitStdin<'a> {
    Null,
    File(&'a Path),
    /// These bytes, then end of file. They must fit in the pipe buffer: they
    /// are written before hermit's output is read.
    Pipe(&'a [u8]),
}

/// Where hermit's own standard output goes.
#[derive(Clone, Copy, Debug)]
enum HermitStdout<'a> {
    /// Captured and returned.
    Pipe,
    Null,
    /// Appended to, and created if missing. Append mode keeps every write at
    /// the end whichever open file description makes it: the guest reopens
    /// its stdout through /dev/stdout, and a duplicate of fd 1 shares fd 1's
    /// offset, which a write through the reopened description does not move.
    File(&'a Path),
}

fn run_guest(script: &str, extra_options: &[&str], args: &[&std::ffi::OsStr]) -> String {
    run_guest_with_stdio(
        script,
        extra_options,
        args,
        HermitStdin::Null,
        HermitStdout::Pipe,
    )
}

/// Runs `script` under hermit and returns hermit's stdout if it is
/// [`HermitStdout::Pipe`], or the empty string.
fn run_guest_with_stdio(
    script: &str,
    extra_options: &[&str],
    args: &[&std::ffi::OsStr],
    stdin: HermitStdin,
    stdout: HermitStdout,
) -> String {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args(["run", "--base-env=minimal"]);
    command.args(extra_options);
    command.args(["--", "/bin/sh", "-c", script, "sh"]);
    command.args(args);
    hermit_test::configure_guest_execution(&mut command);
    // After `configure_guest_execution`, which may rebuild the command.
    command.stdin(match stdin {
        HermitStdin::Null => Stdio::null(),
        HermitStdin::File(path) => fs::File::open(path)
            .unwrap_or_else(|error| panic!("failed to open stdin {}: {error}", path.display()))
            .into(),
        HermitStdin::Pipe(_) => Stdio::piped(),
    });
    command.stdout(match stdout {
        HermitStdout::Pipe => Stdio::piped(),
        HermitStdout::Null => Stdio::null(),
        HermitStdout::File(path) => fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|error| panic!("failed to open stdout {}: {error}", path.display()))
            .into(),
    });
    command.stderr(Stdio::piped());
    let rendered = format!("{command:?} < {stdin:?} > {stdout:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    if let HermitStdin::Pipe(bytes) = stdin {
        // Dropping the pipe at the end of this block closes it.
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(bytes)
            .unwrap_or_else(|error| panic!("failed to write the stdin of {rendered}: {error}"));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{rendered} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("guest output should be UTF-8")
}

fn guest_tmpfs_inodes(host: &Path) -> String {
    let stdout = run_guest(
        GUEST_SCRIPT,
        &["--mount=type=tmpfs,target=/test", "--workdir=/test"],
        &[host.as_os_str()],
    );
    assert!(
        stdout.starts_with("stat:") && stdout.contains(" .\n") && stdout.contains(" ..\n"),
        "the guest omitted its stat or getdents64 output:\n{stdout}"
    );
    stdout
}

#[test]
fn tmpfs_inodes_do_not_depend_on_host_inodes_of_another_filesystem() {
    let scratch = ScratchDir::new("tmpfs");
    let linked = guest_tmpfs_inodes(&host_directory(&scratch, "linked", true));
    let separate = guest_tmpfs_inodes(&host_directory(&scratch, "separate", false));
    assert_eq!(
        linked, separate,
        "the guest's tmpfs inodes changed with the number of host inodes on a \
         different filesystem (https://github.com/rrnewton/hermit/issues/2897)"
    );
}

/// Returns (maps inode, stat inode) of the shell's executable, read in `order`.
fn executable_inodes(order: [&str; 2]) -> (String, String) {
    let args: Vec<&std::ffi::OsStr> = order.iter().map(std::ffi::OsStr::new).collect();
    let stdout = run_guest(EXECUTABLE_INODE_SCRIPT, &[], &args);
    let field = |source: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{source}:")))
            .filter(|inode| !inode.is_empty())
            .unwrap_or_else(|| panic!("the guest printed no {source} inode:\n{stdout}"))
            .to_owned()
    };
    (field("maps"), field("stat"))
}

/// The host's own (maps device column, `stat` device) of the shell's
/// executable, read natively, both as `major:minor` in lowercase hex. When they
/// are equal (ext4, xfs, tmpfs) the test below passes without exercising the
/// cross-device pairing it exists for; btrfs and overlayfs make them differ.
fn host_executable_devices() -> (String, String) {
    const SCRIPT: &str = r#"
set -eu
exe=$(readlink /proc/$$/exe)
awk -v exe="$exe" '$6 == exe { print $4; exit }' /proc/$$/maps
printf '%s\n' "$exe"
"#;
    let output = Command::new("/bin/sh")
        .args(["-c", SCRIPT])
        .output()
        .expect("failed to run /bin/sh natively");
    assert!(
        output.status.success(),
        "native device probe failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("native probe output should be UTF-8");
    let mut lines = stdout.lines();
    let maps = lines.next().unwrap_or_default().to_owned();
    let exe = lines.next().unwrap_or_default();
    assert!(
        !maps.is_empty() && !exe.is_empty(),
        "native device probe printed no maps device or executable:\n{stdout}"
    );
    let dev = fs::metadata(exe)
        .unwrap_or_else(|error| panic!("failed to stat {exe}: {error}"))
        .dev();
    let stat = format!("{:02x}:{:02x}", libc::major(dev), libc::minor(dev));
    (maps, stat)
}

#[test]
fn maps_and_stat_agree_on_the_executable_inode_in_either_order() {
    let (maps_dev, stat_dev) = host_executable_devices();
    let case = if maps_dev == stat_dev {
        format!(
            "host maps device {maps_dev} EQUALS stat device {stat_dev}: cross-device pairing \
             NOT exercised on this host"
        )
    } else {
        format!(
            "host maps device {maps_dev} differs from stat device {stat_dev}: cross-device \
             pairing exercised"
        )
    };
    eprintln!("{case}");
    let (maps_first_maps, maps_first_stat) = executable_inodes(["maps", "stat"]);
    assert_eq!(
        maps_first_maps, maps_first_stat,
        "reading /proc/<pid>/maps before stat gave the shell's executable two inodes ({case})"
    );
    let (stat_first_maps, stat_first_stat) = executable_inodes(["stat", "maps"]);
    assert_eq!(
        stat_first_maps, stat_first_stat,
        "stat before /proc/<pid>/maps gave the shell's executable two inodes ({case})"
    );
}

/// Writes to stdout, then stats root-filesystem files.
const STDIO_THEN_STAT_SCRIPT: &str = r#"
set -eu
echo written-to-stdout
stat -c '%i %n' "$@"
"#;

/// A directory on a filesystem other than `root_dev`, for a copy of stdin.
fn directory_on_another_filesystem(root_dev: u64, scratch: &ScratchDir) -> PathBuf {
    let candidates = [PathBuf::from("/dev/shm"), scratch.0.clone()];
    candidates
        .iter()
        .find(|dir| {
            fs::metadata(dir).is_ok_and(|meta| meta.is_dir() && meta.dev() != root_dev)
                && tempfile_probe(dir)
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "none of {candidates:?} is a writable directory on a filesystem other than \
                 device {root_dev:#x}; this test needs two filesystems"
            )
        })
}

fn tempfile_probe(dir: &Path) -> bool {
    let probe = dir.join(format!(
        ".inode-device-identity-probe-{}",
        std::process::id()
    ));
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// Removes a file on drop.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn guest_inodes_do_not_depend_on_where_hermit_stdin_comes_from() {
    // Stdin from a root-filesystem file, and the stat targets on that same
    // device, so a mint caused by stdin would shift them. Not /etc/group:
    // hermit mounts its own synthesized copy there, on another device.
    let root_stdin = Path::new("/etc/passwd");
    let targets = [
        Path::new("/etc"),
        Path::new("/etc/shells"),
        Path::new("/bin/sh"),
    ];
    let root_dev = fs::metadata(root_stdin)
        .expect("/etc/passwd must exist")
        .dev();
    for target in targets {
        let dev = fs::metadata(target)
            .unwrap_or_else(|error| panic!("{} must exist: {error}", target.display()))
            .dev();
        assert_eq!(
            dev,
            root_dev,
            "{} is not on the device of {}; the test premise does not hold",
            target.display(),
            root_stdin.display()
        );
    }

    let scratch = ScratchDir::new("stdin");
    let other_dir = directory_on_another_filesystem(root_dev, &scratch);
    let other_stdin = RemoveOnDrop(other_dir.join(format!(
        "inode-device-identity-stdin-{}",
        std::process::id()
    )));
    fs::copy(root_stdin, &other_stdin.0).expect("failed to copy stdin to the other filesystem");
    assert_eq!(
        fs::read(root_stdin).unwrap(),
        fs::read(&other_stdin.0).unwrap(),
        "the two stdin files must be byte-identical"
    );

    let args: Vec<&std::ffi::OsStr> = targets.iter().map(|path| path.as_os_str()).collect();
    let from_stdin = |stdin: &Path| {
        run_guest_with_stdio(
            STDIO_THEN_STAT_SCRIPT,
            &[],
            &args,
            HermitStdin::File(stdin),
            HermitStdout::Pipe,
        )
    };
    let from_root = from_stdin(root_stdin);
    let from_other = from_stdin(&other_stdin.0);
    assert!(
        from_root.starts_with("written-to-stdout\n") && from_root.contains(" /etc/shells\n"),
        "the guest omitted its output:\n{from_root}"
    );
    assert_eq!(
        from_root,
        from_other,
        "the guest's root-filesystem inodes changed with where hermit's stdin came from \
         (root filesystem {} vs {}, byte-identical contents)",
        root_stdin.display(),
        other_stdin.0.display()
    );
}

/// Reaches hermit's stdout (`fd` 1) or stdin (`fd` 0) through every
/// descriptor route other than `fstat` of that descriptor, and after each step
/// stats files on the devices a leaked mint would shift. Appends everything to
/// the file named by the first argument; the remaining arguments are the files
/// to stat. A route's line is `<route>: <inode>`.
///
/// Every `stat` of a link runs in a process whose own descriptor it names:
/// only those routes report the fixed inode, and a link into another process's
/// table (`/proc/<parent>/fd/1`) resolves through the inode pool. Never names
/// /dev/null by its own path, which is not a descriptor route and legitimately
/// reports a different inode from the stream when the stream is /dev/null.
const STDIO_ALIASES_SCRIPT: &str = r#"
set -eu
out=$1
fd=$2
shift 2
if [ "$fd" = 1 ]; then stream=stdout; else stream=stdin; fi
after() { step=$1; shift; stat -c "$step: %i %n" "$@" >> "$out"; }
if [ "$fd" = 1 ]; then
    # Open through links; each write bumps the object's virtual mtime. `>>`,
    # because `>` would truncate a regular file under fd 1.
    echo open-dev-stdout >> /dev/stdout
    after open-dev-stdout "$@"
    echo open-proc-self-fd-1 >> /proc/self/fd/1
    after open-proc-self-fd-1 "$@"
    exec 3>&1
    echo write-fd-3 >&3
    after write-fd-3 "$@"
else
    cat /dev/stdin > "$out.copy"
    after open-dev-stdin "$@"
    cat /proc/self/fd/0 >> "$out.copy"
    after open-proc-self-fd-0 "$@"
    exec 3<&0
    # coreutils stats `-` with statx(0, "", AT_EMPTY_PATH).
    stat -L -c 'statx-empty-path-fd-0: %i' - >> "$out"
    after statx-empty-path-fd-0 "$@"
fi
# From here fd 3 duplicates the stream. glibc's stat() is newfstatat, and its
# fstat() is fstat.
/usr/bin/perl -MPOSIX -e '
    my ($out, $fd, $stream) = @ARGV;
    open(my $report, ">>", $out) or die "$out: $!";
    for my $route (
        ["stat-dev-$stream", "/dev/$stream"],
        ["stat-proc-self-fd-$fd", "/proc/self/fd/$fd"],
        ["stat-proc-pid-fd-$fd", "/proc/$$/fd/$fd"],
        ["stat-dev-fd-3", "/dev/fd/3"],
        ["stat-proc-thread-self-fd-3", "/proc/thread-self/fd/3"],
    ) {
        my @s = stat($route->[1]) or die "stat $route->[1]: $!";
        print $report "$route->[0]: $s[1]\n";
    }
    my @s = POSIX::fstat(3) or die "fstat 3: $!";
    print $report "fstat-fd-3: $s[1]\n";
    open(my $reopened, $fd ? ">>" : "<", "/dev/$stream") or die "open /dev/$stream: $!";
    my @t = stat($reopened) or die "fstat of /dev/$stream: $!";
    print $report "open-dev-$stream-then-fstat: $t[1]\n";
' "$out" "$fd" "$stream"
after perl-routes "$@"
# coreutils `stat -L` is statx.
stat -L -c 'statx-dev-fd-3: %i' /dev/fd/3 >> "$out"
after statx-dev-fd-3 "$@"
grep '^ino:' /proc/self/fdinfo/3 >> "$out"
after fdinfo-3 "$@"
"#;

/// The report [`STDIO_ALIASES_SCRIPT`] writes for descriptor `fd` when hermit's
/// stdio is set up by `run`, which receives the report path.
fn stdio_alias_report(
    scratch: &ScratchDir,
    label: &str,
    fd: u8,
    probe: &Path,
    run: impl FnOnce(&[&std::ffi::OsStr]),
) -> String {
    let report = scratch.0.join(format!("report-{label}"));
    let fd = fd.to_string();
    let args = [
        report.as_os_str(),
        std::ffi::OsStr::new(&fd),
        std::ffi::OsStr::new("/proc/self/status"),
        std::ffi::OsStr::new("/dev/zero"),
        std::ffi::OsStr::new("/etc/group"),
        std::ffi::OsStr::new("/etc/shells"),
        probe.as_os_str(),
    ];
    run(&args);
    let report_text = fs::read_to_string(&report)
        .unwrap_or_else(|error| panic!("the guest wrote no report {}: {error}", report.display()));
    // The probe's host path differs between test processes; its inode is what
    // is compared.
    report_text.replace(&probe.display().to_string(), "PROBE")
}

/// Requires `reports` (label, report) to be identical, and each to give hermit's
/// stdio object the fixed inode `expected` through every route in `routes`.
fn assert_stdio_alias_reports_agree(
    stream: &str,
    reports: &[(&str, String)],
    routes: &[&str],
    expected: u64,
) {
    let (first_label, first) = &reports[0];
    for (label, report) in &reports[1..] {
        assert_eq!(
            first, report,
            "the guest's inodes changed with where hermit's {stream} went ({first_label} vs \
             {label}); the first differing step localizes the leaking route"
        );
    }
    for (label, report) in reports {
        for route in routes {
            assert!(
                report.contains(&format!("\n{route}: {expected}\n")),
                "{label}: {route} did not report hermit's {stream} as inode {expected}:\n{report}"
            );
        }
        assert!(
            report.contains(&format!("\nino:\t{expected}\n")),
            "{label}: fdinfo of a duplicate of hermit's {stream} did not report inode \
             {expected}:\n{report}"
        );
        assert!(
            report.contains(" PROBE\n"),
            "{label}: the guest did not stat the probe file:\n{report}"
        );
    }
}

#[test]
fn guest_inodes_do_not_depend_on_where_hermit_stdout_goes() {
    assert!(
        Path::new("/usr/bin/perl").is_file(),
        "this test fstats a descriptor above 2 with /usr/bin/perl, which is missing"
    );
    let scratch = ScratchDir::new("stdout");
    // Beside hermit's stdout file, so a counter value taken by it would shift
    // this file's inode.
    let probe = scratch.0.join("probe");
    fs::write(&probe, b"probe\n").expect("failed to write the probe file");
    let stdout_file = scratch.0.join("stdout");
    let expected_stdout = "open-dev-stdout\nopen-proc-self-fd-1\nwrite-fd-3\n";

    let null = stdio_alias_report(&scratch, "null", 1, &probe, |args| {
        let stdout = run_guest_with_stdio(
            STDIO_ALIASES_SCRIPT,
            &[],
            args,
            HermitStdin::Null,
            HermitStdout::Null,
        );
        assert_eq!(stdout, "");
    });
    let file = stdio_alias_report(&scratch, "file", 1, &probe, |args| {
        run_guest_with_stdio(
            STDIO_ALIASES_SCRIPT,
            &[],
            args,
            HermitStdin::Null,
            HermitStdout::File(&stdout_file),
        );
        assert_eq!(
            fs::read_to_string(&stdout_file).expect("failed to read hermit's stdout file"),
            expected_stdout
        );
    });
    let pipe = stdio_alias_report(&scratch, "pipe", 1, &probe, |args| {
        let stdout = run_guest_with_stdio(
            STDIO_ALIASES_SCRIPT,
            &[],
            args,
            HermitStdin::Null,
            HermitStdout::Pipe,
        );
        assert_eq!(stdout, expected_stdout);
    });
    assert_stdio_alias_reports_agree(
        "stdout",
        &[("/dev/null", null), ("regular file", file), ("pipe", pipe)],
        &[
            "stat-dev-stdout",
            "stat-proc-self-fd-1",
            "stat-proc-pid-fd-1",
            "stat-dev-fd-3",
            "stat-proc-thread-self-fd-3",
            "fstat-fd-3",
            "open-dev-stdout-then-fstat",
            "statx-dev-fd-3",
        ],
        1001,
    );
}

#[test]
fn guest_inodes_do_not_depend_on_whether_hermit_stdin_is_null_a_file_or_a_pipe() {
    assert!(
        Path::new("/usr/bin/perl").is_file(),
        "this test fstats a descriptor above 2 with /usr/bin/perl, which is missing"
    );
    let scratch = ScratchDir::new("stdin-kinds");
    // Beside hermit's stdin file, so a counter value taken by it would shift
    // this file's inode.
    let probe = scratch.0.join("probe");
    fs::write(&probe, b"probe\n").expect("failed to write the probe file");
    let contents = b"stdin contents\n";
    let stdin_file = scratch.0.join("stdin");
    fs::write(&stdin_file, contents).expect("failed to write hermit's stdin file");

    fn run(stdin: HermitStdin<'_>) -> impl FnOnce(&[&std::ffi::OsStr]) + '_ {
        move |args| {
            run_guest_with_stdio(STDIO_ALIASES_SCRIPT, &[], args, stdin, HermitStdout::Pipe);
        }
    }
    let null = stdio_alias_report(&scratch, "null", 0, &probe, run(HermitStdin::Null));
    let file = stdio_alias_report(
        &scratch,
        "file",
        0,
        &probe,
        run(HermitStdin::File(&stdin_file)),
    );
    let pipe = stdio_alias_report(
        &scratch,
        "pipe",
        0,
        &probe,
        run(HermitStdin::Pipe(contents)),
    );
    // Opening /dev/stdin or /proc/self/fd/0 reopens a regular file from its
    // start, while the first read of a pipe drains it.
    for (label, copies) in [("null", 0), ("file", 2), ("pipe", 1)] {
        assert_eq!(
            fs::read(scratch.0.join(format!("report-{label}.copy")))
                .expect("the guest wrote no copy of its stdin"),
            contents.repeat(copies),
            "{label}: what the guest read through /dev/stdin and /proc/self/fd/0"
        );
    }
    assert_stdio_alias_reports_agree(
        "stdin",
        &[("/dev/null", null), ("regular file", file), ("pipe", pipe)],
        &[
            "statx-empty-path-fd-0",
            "stat-dev-stdin",
            "stat-proc-self-fd-0",
            "stat-proc-pid-fd-0",
            "stat-dev-fd-3",
            "stat-proc-thread-self-fd-3",
            "fstat-fd-3",
            "open-dev-stdin-then-fstat",
            "statx-dev-fd-3",
        ],
        1000,
    );
}
