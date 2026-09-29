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
//! virtual-mtime bump of a write. Those routes now report the object's fixed
//! inode without consulting the inode pool.
//!
//! Each object has one identity, whatever route reaches it: its descriptors,
//! links to them in any process's descriptor table, its own path, and its maps
//! header all report 1000 plus the lowest stream it was handed on. An object is
//! recognized by its host `(st_dev, st_ino)`, by the maps identity hermit
//! learns for a regular stdio file at startup, or failing that by a maps
//! header's device paired with that `st_dev`, never by the inode number alone,
//! and a descriptor's stream is decided by the resource it holds, not its
//! number.
//! Hermit's stdout on the same object as its stdin therefore reports 1000
//! through every stdout route, as Linux gives both streams one inode.
//!
//! The tests:
//! - The third runs the same guest with byte-identical stdin from two
//!   filesystems; the guest reads that file by its own path.
//! - The next three reach hermit's stdout, its stderr and then one object
//!   handed on two streams through each descriptor route, with the stream on
//!   /dev/null, on a regular file and on a pipe, and require identical output
//!   and the fixed inode. The sixth does the same for stdin.
//! - Three check maps headers: a mapped file that shares only its host inode
//!   number with /dev/null keeps its own identity, and a mapped stdio object
//!   reports its fixed inode there as through `stat`, on /dev/shm and in the
//!   Cargo target tmpdir (btrfs on the development host, where the maps
//!   header names a device `stat` does not report).
//! - Two run `cat F` and `cp F /dev/stdout` with hermit's stdout appended to F.
//!   Both must see one file, refuse, and leave F as it was. With two
//!   identities `cat` copied F onto its own end without stopping, and `cp`
//!   truncated F.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
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

/// Where hermit's own standard output or standard error goes.
#[derive(Clone, Copy, Debug)]
enum HermitOutput<'a> {
    /// Captured and returned.
    Pipe,
    Null,
    /// Appended to, and created if missing. Append mode keeps every write at
    /// the end whichever open file description makes it: the guest reopens
    /// its stdout through /dev/stdout, and a duplicate of fd 1 shares fd 1's
    /// offset, which a write through the reopened description does not move.
    File(&'a Path),
}

impl HermitOutput<'_> {
    fn stdio(self, stream: &str) -> Stdio {
        match self {
            HermitOutput::Pipe => Stdio::piped(),
            HermitOutput::Null => Stdio::null(),
            HermitOutput::File(path) => fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap_or_else(|error| {
                    panic!("failed to open {stream} {}: {error}", path.display())
                })
                .into(),
        }
    }
}

fn run_guest(script: &str, extra_options: &[&str], args: &[&std::ffi::OsStr]) -> String {
    run_guest_with_stdio(
        script,
        extra_options,
        args,
        HermitStdin::Null,
        HermitOutput::Pipe,
    )
}

/// Runs `script` under hermit with its stderr captured, requires success, and
/// returns hermit's stdout if it is [`HermitOutput::Pipe`], or the empty
/// string.
fn run_guest_with_stdio(
    script: &str,
    extra_options: &[&str],
    args: &[&std::ffi::OsStr],
    stdin: HermitStdin,
    stdout: HermitOutput,
) -> String {
    let (rendered, output) = spawn_guest(
        guest_command(script, extra_options, args),
        stdin,
        stdout,
        HermitOutput::Pipe,
    );
    assert_guest_succeeded(&rendered, &output);
    String::from_utf8(output.stdout).expect("guest output should be UTF-8")
}

/// The hermit command that runs `script` with `args`. Its stdio is set by
/// [`spawn_guest`].
fn guest_command(script: &str, extra_options: &[&str], args: &[&std::ffi::OsStr]) -> Command {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args(["run", "--base-env=minimal"]);
    command.args(extra_options);
    command.args(["--", "/bin/sh", "-c", script, "sh"]);
    command.args(args);
    // Last: this may rebuild the command, dropping stdio or a `pre_exec` hook
    // set before it.
    hermit_test::configure_guest_execution(&mut command);
    command
}

/// Runs `command` with hermit's stdio as given, and returns the rendered
/// command with its output. A stream that is not [`HermitOutput::Pipe`] is
/// empty in the output. The exit status is not checked.
fn spawn_guest(
    mut command: Command,
    stdin: HermitStdin,
    stdout: HermitOutput,
    stderr: HermitOutput,
) -> (String, Output) {
    command.stdin(match stdin {
        HermitStdin::Null => Stdio::null(),
        HermitStdin::File(path) => fs::File::open(path)
            .unwrap_or_else(|error| panic!("failed to open stdin {}: {error}", path.display()))
            .into(),
        HermitStdin::Pipe(_) => Stdio::piped(),
    });
    command.stdout(stdout.stdio("stdout"));
    command.stderr(stderr.stdio("stderr"));
    let rendered = format!("{command:?} < {stdin:?} > {stdout:?} 2> {stderr:?}");
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
    (rendered, output)
}

fn assert_guest_succeeded(rendered: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{rendered} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
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
            HermitOutput::Pipe,
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

/// Reaches hermit's stdin (`fd` 0), stdout (`fd` 1) or stderr (`fd` 2)
/// through every descriptor route other than `fstat` of that descriptor, and
/// after each step stats files on the devices a leaked mint would shift.
/// Appends everything to the file named by the first argument; the remaining
/// arguments are the files to stat. A route's line is `<route>: <inode>`.
///
/// Every route reports the stream's object's fixed inode (see
/// `InheritedStdio` in detcore), whichever process's descriptor table a link
/// names; only the route decides whether the inode pool is consulted. Never
/// names the object by its own path (/dev/null, or the file hermit's stdio
/// was redirected to), which reports the same fixed inode but consults the
/// pool, so a leak through it would not show here.
///
/// For stdout and stderr it also reports `lseek` of the stream reopened
/// through /dev/stdout or /dev/stderr, and of one opened `O_PATH`, as
/// `lseek-reopened-dev-<stream>` and `lseek-o-path-dev-<stream>`.
const STDIO_ALIASES_SCRIPT: &str = r#"
set -eu
out=$1
fd=$2
shift 2
case "$fd" in
    0) stream=stdin ;;
    1) stream=stdout ;;
    2) stream=stderr ;;
    *) echo "no stream $fd" >&2; exit 2 ;;
esac
after() { step=$1; shift; stat -c "$step: %i %n" "$@" >> "$out"; }
if [ "$fd" != 0 ]; then
    # Open through links; each write bumps the object's virtual mtime. `>>`,
    # because `>` would truncate a regular file under the stream.
    echo "open-dev-$stream" >> "/dev/$stream"
    after "open-dev-$stream" "$@"
    echo "open-proc-self-fd-$fd" >> "/proc/self/fd/$fd"
    after "open-proc-self-fd-$fd" "$@"
    exec 3>&"$fd"
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
if [ "$fd" = 1 ]; then
    # `2>&1` makes fd 2 a duplicate of hermit's stdout. It and a duplicate of
    # it carry stdout's resource, so both report stdout's inode: the stream
    # is decided by the resource, not by the descriptor number.
    /usr/bin/perl -MPOSIX -e '
        open(my $report, ">>", $ARGV[0]) or die "$ARGV[0]: $!";
        my @s = POSIX::fstat(2) or die "fstat 2: $!";
        my $dup = POSIX::dup(2) // die "dup 2: $!";
        my @d = POSIX::fstat($dup) or die "fstat of a duplicate of fd 2: $!";
        my @l = stat("/dev/stderr") or die "stat /dev/stderr: $!";
        print $report "fstat-fd-2-after-2-to-1: $s[1]\n",
            "fstat-dup-of-fd-2-after-2-to-1: $d[1]\n",
            "stat-dev-stderr-after-2-to-1: $l[1]\n";
    ' "$out" 2>&1
    after fd-2-after-2-to-1 "$@"
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
        ["stat-proc-pid-task-tid-fd-$fd", "/proc/$$/task/$$/fd/$fd"],
        # The shell that started perl holds the stream on the same number.
        ["stat-proc-parent-fd-$fd", "/proc/" . getppid() . "/fd/$fd"],
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
    if ($fd) {
        # lseek of a reopened stdout or stderr, and of one opened O_PATH
        # (010000000 on x86_64 and aarch64).
        for my $open (["reopened", POSIX::O_WRONLY() | POSIX::O_APPEND()], ["o-path", 010000000]) {
            my $d = POSIX::open("/dev/$stream", $open->[1]) // die "open /dev/$stream: $!";
            # This perl returns -1, not undef, when lseek fails.
            my $offset = POSIX::lseek($d, 0, POSIX::SEEK_CUR());
            print $report "lseek-$open->[0]-dev-$stream: ",
                defined $offset && $offset >= 0 ? "offset " . ($offset + 0) : "errno " . ($! + 0), "\n";
            POSIX::close($d);
        }
    }
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

/// Hermit reports every descriptor of its stdout or stderr as unseekable,
/// including one reopened through /dev/stdout or /dev/stderr, whatever object
/// the stream is: Linux would seek a regular file or /dev/null and refuse a
/// pipe, so the answer would depend on how hermit was invoked. A descriptor
/// opened `O_PATH` is not open for I/O, so its `lseek` fails with `EBADF`
/// first, as on Linux.
fn assert_container_output_is_unseekable(stream: &str, reports: &[(&str, String)]) {
    for (label, report) in reports {
        for (route, errno) in [("reopened", libc::ESPIPE), ("o-path", libc::EBADF)] {
            let line = format!("\nlseek-{route}-dev-{stream}: errno {errno}\n");
            assert!(
                report.contains(&line),
                "{label}: expected {:?} for lseek of /dev/{stream}:\n{report}",
                line.trim()
            );
        }
    }
}

/// The routes [`STDIO_ALIASES_SCRIPT`] reports for hermit's stdout.
const STDOUT_ROUTES: &[&str] = &[
    "fstat-fd-2-after-2-to-1",
    "fstat-dup-of-fd-2-after-2-to-1",
    "stat-dev-stderr-after-2-to-1",
    "stat-dev-stdout",
    "stat-proc-self-fd-1",
    "stat-proc-pid-fd-1",
    "stat-proc-pid-task-tid-fd-1",
    "stat-proc-parent-fd-1",
    "stat-dev-fd-3",
    "stat-proc-thread-self-fd-3",
    "fstat-fd-3",
    "open-dev-stdout-then-fstat",
    "statx-dev-fd-3",
];

/// The routes [`STDIO_ALIASES_SCRIPT`] reports for hermit's stderr.
const STDERR_ROUTES: &[&str] = &[
    "stat-dev-stderr",
    "stat-proc-self-fd-2",
    "stat-proc-pid-fd-2",
    "stat-proc-pid-task-tid-fd-2",
    "stat-proc-parent-fd-2",
    "stat-dev-fd-3",
    "stat-proc-thread-self-fd-3",
    "fstat-fd-3",
    "open-dev-stderr-then-fstat",
    "statx-dev-fd-3",
];

fn require_perl() {
    assert!(
        Path::new("/usr/bin/perl").is_file(),
        "this test fstats a descriptor above 2 with /usr/bin/perl, which is missing"
    );
}

#[test]
fn guest_inodes_do_not_depend_on_where_hermit_stdout_goes() {
    require_perl();
    let scratch = ScratchDir::new("stdout");
    // Beside hermit's stdout file, so a counter value taken by it would shift
    // this file's inode.
    let probe = scratch.0.join("probe");
    fs::write(&probe, b"probe\n").expect("failed to write the probe file");
    let stdout_file = scratch.0.join("stdout");
    let expected_stdout = "open-dev-stdout\nopen-proc-self-fd-1\nwrite-fd-3\n";
    // Stdin is an empty pipe, so it is never the object stdout is: /dev/null
    // on both would be one object, reporting stdin's inode (see
    // `a_stdio_object_handed_on_two_streams_reports_the_lower_streams_inode`).
    let stdin = HermitStdin::Pipe(b"");

    let null = stdio_alias_report(&scratch, "null", 1, &probe, |args| {
        let stdout =
            run_guest_with_stdio(STDIO_ALIASES_SCRIPT, &[], args, stdin, HermitOutput::Null);
        assert_eq!(stdout, "");
    });
    let file = stdio_alias_report(&scratch, "file", 1, &probe, |args| {
        run_guest_with_stdio(
            STDIO_ALIASES_SCRIPT,
            &[],
            args,
            stdin,
            HermitOutput::File(&stdout_file),
        );
        assert_eq!(
            fs::read_to_string(&stdout_file).expect("failed to read hermit's stdout file"),
            expected_stdout
        );
    });
    let pipe = stdio_alias_report(&scratch, "pipe", 1, &probe, |args| {
        let stdout =
            run_guest_with_stdio(STDIO_ALIASES_SCRIPT, &[], args, stdin, HermitOutput::Pipe);
        assert_eq!(stdout, expected_stdout);
    });
    let reports = [("/dev/null", null), ("regular file", file), ("pipe", pipe)];
    assert_stdio_alias_reports_agree("stdout", &reports, STDOUT_ROUTES, 1001);
    assert_container_output_is_unseekable("stdout", &reports);
}

/// The stderr variant of the test above. It sets hermit's stderr itself,
/// because [`run_guest_with_stdio`] always captures stderr in a pipe.
#[test]
fn guest_inodes_do_not_depend_on_where_hermit_stderr_goes() {
    require_perl();
    let scratch = ScratchDir::new("stderr");
    let probe = scratch.0.join("probe");
    fs::write(&probe, b"probe\n").expect("failed to write the probe file");
    let stderr_file = scratch.0.join("stderr");
    // Hermit writes its own diagnostics to the same stderr, so the guest's
    // lines are looked for, not required to be all of it.
    let expected_lines = ["open-dev-stderr", "open-proc-self-fd-2", "write-fd-3"];
    let run = |args: &[&std::ffi::OsStr], stderr: HermitOutput| {
        let (rendered, output) = spawn_guest(
            guest_command(STDIO_ALIASES_SCRIPT, &[], args),
            HermitStdin::Pipe(b""),
            HermitOutput::Pipe,
            stderr,
        );
        assert_guest_succeeded(&rendered, &output);
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let assert_guest_lines = |label: &str, stderr: &str| {
        for line in expected_lines {
            assert!(
                stderr.lines().any(|written| written == line),
                "{label}: the guest's write {line:?} did not reach hermit's stderr:\n{stderr}"
            );
        }
    };

    let null = stdio_alias_report(&scratch, "null", 2, &probe, |args| {
        assert_eq!(run(args, HermitOutput::Null), "");
    });
    let file = stdio_alias_report(&scratch, "file", 2, &probe, |args| {
        run(args, HermitOutput::File(&stderr_file));
        assert_guest_lines(
            "regular file",
            &fs::read_to_string(&stderr_file).expect("failed to read hermit's stderr file"),
        );
    });
    let pipe = stdio_alias_report(&scratch, "pipe", 2, &probe, |args| {
        assert_guest_lines("pipe", &run(args, HermitOutput::Pipe));
    });
    let reports = [("/dev/null", null), ("regular file", file), ("pipe", pipe)];
    assert_stdio_alias_reports_agree("stderr", &reports, STDERR_ROUTES, 1002);
    assert_container_output_is_unseekable("stderr", &reports);
}

/// One host object handed to hermit on two streams has one identity, the
/// lower stream's inode, on every route of either stream, as the two streams
/// share one inode on Linux.
#[test]
fn a_stdio_object_handed_on_two_streams_reports_the_lower_streams_inode() {
    require_perl();
    let scratch = ScratchDir::new("two-streams");
    let probe = scratch.0.join("probe");
    fs::write(&probe, b"probe\n").expect("failed to write the probe file");

    // `< /dev/null > /dev/null`: stdout reports stdin's inode.
    let null = stdio_alias_report(&scratch, "null", 1, &probe, |args| {
        run_guest_with_stdio(
            STDIO_ALIASES_SCRIPT,
            &[],
            args,
            HermitStdin::Null,
            HermitOutput::Null,
        );
    });
    assert_stdio_alias_reports_agree(
        "stdout, which is also its stdin",
        &[("/dev/null on stdin and stdout", null)],
        STDOUT_ROUTES,
        1000,
    );

    // `> F 2> F`, two descriptions of one file: stderr reports stdout's inode.
    let merged_file = scratch.0.join("merged");
    let merged = stdio_alias_report(&scratch, "merged", 2, &probe, |args| {
        let (rendered, output) = spawn_guest(
            guest_command(STDIO_ALIASES_SCRIPT, &[], args),
            HermitStdin::Pipe(b""),
            HermitOutput::File(&merged_file),
            HermitOutput::File(&merged_file),
        );
        assert_guest_succeeded(&rendered, &output);
    });
    assert_stdio_alias_reports_agree(
        "stderr, which is also its stdout",
        &[("one file on stdout and stderr", merged)],
        STDERR_ROUTES,
        1001,
    );
}

#[test]
fn guest_inodes_do_not_depend_on_whether_hermit_stdin_is_null_a_file_or_a_pipe() {
    require_perl();
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
            run_guest_with_stdio(STDIO_ALIASES_SCRIPT, &[], args, stdin, HermitOutput::Pipe);
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
            "stat-proc-pid-task-tid-fd-0",
            "stat-proc-parent-fd-0",
            "stat-dev-fd-3",
            "stat-proc-thread-self-fd-3",
            "fstat-fd-3",
            "open-dev-stdin-then-fstat",
            "statx-dev-fd-3",
        ],
        1000,
    );
}

/// Copies an executable to four files on the guest's own tmpfs, runs each,
/// and appends each copy's inode from its maps header and from `stat` to the
/// file named by the first argument. A line is `<path>: maps <ino> stat <ino>`.
const MAPS_COLLISION_SCRIPT: &str = r#"
set -eu
out=$1
for f in a b c d; do cp /bin/cat "/test/$f"; done
for f in a b c d; do
    exe=/test/$f
    maps=$("$exe" /proc/self/maps | awk -v exe="$exe" '$6 == exe { print $5; exit }')
    printf '%s: maps %s stat %s\n' "$exe" "$maps" "$(stat -c %i "$exe")" >> "$out"
done
"#;

/// A maps header names its file by device and inode. A file that shares only
/// its host inode number with one of hermit's stdio objects is a different
/// object and must keep its own identity. Hermit's stdin is /dev/null, host
/// inode 3 on devtmpfs, and the guest's fresh tmpfs numbers its first files
/// from the same small range, so matching on the inode alone gave one of the
/// copies stdin's fixed inode in its maps header while `stat` gave it a
/// pooled one.
#[test]
fn a_mapped_file_sharing_only_an_inode_number_with_stdio_keeps_its_own_identity() {
    let null_inode = fs::metadata("/dev/null")
        .expect("/dev/null must exist")
        .ino();
    // Only a diagnostic: the host inodes of the guest's tmpfs files are not
    // visible here, so whether a copy collided cannot be asserted.
    eprintln!(
        "/dev/null is host inode {null_inode}; the collision this test guards against is \
         exercised when that is the host inode of one of the four copies, which a fresh tmpfs \
         numbers from 2 or 3 upwards ({})",
        if (2..=6).contains(&null_inode) {
            "likely here"
        } else {
            "unlikely here"
        }
    );
    let scratch = ScratchDir::new("maps-collision");
    let stdout_file = scratch.0.join("stdout");
    let report = |label: &str, stdout: HermitOutput| {
        let report = scratch.0.join(format!("report-{label}"));
        run_guest_with_stdio(
            MAPS_COLLISION_SCRIPT,
            &["--mount=type=tmpfs,target=/test"],
            &[report.as_os_str()],
            HermitStdin::Null,
            stdout,
        );
        fs::read_to_string(&report).unwrap_or_else(|error| {
            panic!("the guest wrote no report {}: {error}", report.display())
        })
    };
    let reports = [
        ("/dev/null", report("null", HermitOutput::Null)),
        (
            "regular file",
            report("file", HermitOutput::File(&stdout_file)),
        ),
        ("pipe", report("pipe", HermitOutput::Pipe)),
    ];
    for (label, report) in &reports {
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 4, "{label}: expected four copies:\n{report}");
        for line in lines {
            let fields: Vec<&str> = line.split_whitespace().collect();
            assert!(
                matches!(fields.as_slice(), [_, "maps", maps, "stat", stat] if maps == stat),
                "{label}: the maps header and stat disagree on a copy's inode: {line}\n{report}"
            );
        }
    }
    for (label, report) in &reports[1..] {
        assert_eq!(
            &reports[0].1, report,
            "the copies' inodes changed with where hermit's stdout went ({} vs {label})",
            reports[0].0
        );
    }
}

/// Maps hermit's stdin by reopening /dev/stdin and hermit's stdout file by its
/// own path, then reports each object's inode through its maps header, `fstat`
/// of its stream, `stat` of its own path and `fstat` of the mapped handle. The
/// arguments are the report file, the stdin path and the stdout path; the
/// report names no path.
const MAPS_ROUTE_SCRIPT: &str = r#"
set -eu
exec /usr/bin/perl -e '
    use POSIX ();
    my ($out, $stdin_path, $stdout_path) = @ARGV;
    open(my $report, ">>", $out) or die "$out: $!";
    open(my $in, "<:mmap", "/dev/stdin") or die "mmap /dev/stdin: $!";
    my $line = <$in>;
    print $report "read-stdin: $line";
    open(my $mapped_out, "<:mmap", $stdout_path) or die "mmap $stdout_path: $!";
    $line = <$mapped_out>;
    print $report "read-stdout: $line";
    my %maps;
    open(my $maps, "<", "/proc/self/maps") or die "/proc/self/maps: $!";
    while (<$maps>) {
        my @f = split;
        $maps{$f[5]} //= $f[4] if @f >= 6;
    }
    for my $object (["stdin", $stdin_path, 0, $in], ["stdout", $stdout_path, 1, $mapped_out]) {
        my ($stream, $path, $fd, $handle) = @$object;
        my $maps_inode = $maps{$path} // "none";
        print $report "maps-$stream: $maps_inode\n";
        my @s = POSIX::fstat($fd) or die "fstat $fd: $!";
        print $report "fstat-fd-$fd: $s[1]\n";
        @s = stat($path) or die "stat $path: $!";
        print $report "stat-$stream-path: $s[1]\n";
        @s = stat($handle) or die "fstat of the mapped $stream: $!";
        print $report "fstat-mapped-$stream: $s[1]\n";
    }
' "$@"
"#;

fn is_tmpfs(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .expect("the directory path has no interior NUL");
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `buf` is a writable `statfs`.
    let rc = unsafe { libc::statfs(path.as_ptr(), buf.as_mut_ptr()) };
    // SAFETY: statfs filled `buf` because it returned 0.
    rc == 0 && unsafe { buf.assume_init() }.f_type == libc::TMPFS_MAGIC
}

/// The device and inode a maps header shows for `path`, and the device `stat`
/// reports, both as `major:minor` in hex, found by mapping one page of `path`
/// in this process. Only a diagnostic: it says whether a run on `path`'s
/// filesystem exercised a maps header whose device `stat` does not report.
fn maps_and_stat_devices(path: &Path) -> Option<(String, String)> {
    use std::os::unix::io::AsRawFd;
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: a fresh private read-only mapping of one page of an open file;
    // nothing reads through it and it is unmapped below.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            1,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return None;
    }
    let prefix = format!("{:08x}-", addr as usize);
    let header = fs::read_to_string("/proc/self/maps").ok().and_then(|maps| {
        maps.lines()
            .find(|line| line.starts_with(&prefix))
            .and_then(|line| line.split_whitespace().nth(3).map(str::to_owned))
    });
    // SAFETY: `addr` is the one-page mapping made above and nothing refers to it.
    unsafe { libc::munmap(addr, 1) };
    let stat_dev = format!(
        "{:02x}:{:02x}",
        libc::major(meta.dev()),
        libc::minor(meta.dev())
    );
    Some((header?, stat_dev))
}

/// Maps hermit's stdin and stdout, two regular files in `dir`, through
/// `MAPS_ROUTE_SCRIPT` and requires each to report its fixed inode through its
/// maps header, `fstat` of its stream, `stat` of its own path and `fstat` of
/// the mapped handle.
fn assert_mapped_stdio_reports_its_fixed_inode(dir: &Path, scratch: &ScratchDir) {
    let stdin = RemoveOnDrop(dir.join(format!(
        "inode-device-identity-maps-stdin-{}",
        std::process::id()
    )));
    let stdout = RemoveOnDrop(dir.join(format!(
        "inode-device-identity-maps-stdout-{}",
        std::process::id()
    )));
    fs::write(&stdin.0, b"stdin line\n").expect("failed to write hermit's stdin file");
    fs::write(&stdout.0, b"stdout line\n").expect("failed to write hermit's stdout file");
    match maps_and_stat_devices(&stdin.0) {
        Some((maps, stat)) => eprintln!(
            "{}: a maps header names device {maps}, stat reports {stat} ({})",
            dir.display(),
            if maps == stat {
                "the same device"
            } else {
                "different devices, so the maps route needs the learnt identity"
            }
        ),
        None => eprintln!("{}: could not compare maps and stat devices", dir.display()),
    }
    let report = scratch.0.join("report");
    let _ = fs::remove_file(&report);
    run_guest_with_stdio(
        MAPS_ROUTE_SCRIPT,
        &[],
        &[
            report.as_os_str(),
            stdin.0.as_os_str(),
            stdout.0.as_os_str(),
        ],
        HermitStdin::File(&stdin.0),
        HermitOutput::File(&stdout.0),
    );
    let report = fs::read_to_string(&report).expect("the guest wrote no report");
    assert_eq!(
        report,
        "read-stdin: stdin line\n\
         read-stdout: stdout line\n\
         maps-stdin: 1000\n\
         fstat-fd-0: 1000\n\
         stat-stdin-path: 1000\n\
         fstat-mapped-stdin: 1000\n\
         maps-stdout: 1001\n\
         fstat-fd-1: 1001\n\
         stat-stdout-path: 1001\n\
         fstat-mapped-stdout: 1001\n",
        "a mapped stdio object in {} did not report its fixed inode on every route",
        dir.display()
    );
}

/// Hermit's stdio objects mapped into memory report their fixed inodes in the
/// maps header, as they do through `fstat` and through their own paths, so a
/// program comparing the two sees one file. The objects are on /dev/shm, a
/// tmpfs, whose maps header names the device `stat` reports.
#[test]
fn a_mapped_stdio_object_reports_its_fixed_inode_on_every_route() {
    require_perl();
    let shm = Path::new("/dev/shm");
    assert!(
        is_tmpfs(shm),
        "{} is not a tmpfs; this test needs a filesystem whose maps device is its stat device",
        shm.display()
    );
    let scratch = ScratchDir::new("maps-route");
    assert_mapped_stdio_reports_its_fixed_inode(shm, &scratch);
}

/// The same as the /dev/shm test, with the objects in the Cargo target tmpdir.
/// On btrfs, a maps header names the filesystem's device (00:2f on the
/// development host) while `stat` reports the subvolume's (0:32), and on
/// overlayfs the lower filesystem's. Hermit learns each regular stdio file's
/// maps identity at startup, so its maps header reports the fixed inode there
/// too; before that, the stdin object's maps header reported a provisional
/// inode on btrfs (21474836481 on the development host) while every other
/// route reported 1000. On a filesystem whose maps device is its stat device
/// this repeats the /dev/shm test; the diagnostic line says which case ran.
#[test]
fn a_mapped_stdio_object_in_the_target_tmpdir_reports_its_fixed_inode_on_every_route() {
    require_perl();
    let scratch = ScratchDir::new("maps-route-target-tmpdir");
    let dir = scratch.0.join("objects");
    fs::create_dir_all(&dir).expect("failed to create the objects directory");
    assert_mapped_stdio_reports_its_fixed_inode(&dir, &scratch);
}

/// Runs `cat F` or `cp F /dev/stdout`, named by the first argument, with F the
/// second, and reports the tool's exit status on stderr.
const SELF_COPY_SCRIPT: &str = r#"
set -u
case "$1" in
    cat) cat "$2" ;;
    cp) cp "$2" /dev/stdout ;;
    *) echo "no tool $1" >&2; exit 2 ;;
esac
echo "$1 exited $?" >&2
"#;

/// Runs `tool` over a file that is also hermit's stdout, opened for append,
/// and requires the tool to see one file, refuse with `refusal`, and leave the
/// file as it was. Each object has one identity on every route: had the file
/// reported one inode through its own path and another through fd 1, `cat`
/// would copy the file onto its own end until the disk filled, and `cp` would
/// truncate it.
fn assert_self_copy_refused(tool: &str, refusal: &str) {
    use std::os::unix::process::CommandExt;
    let scratch = ScratchDir::new(&format!("self-copy-{tool}"));
    let file = scratch.0.join("F");
    let contents = b"contents\n";
    fs::write(&file, contents).expect("failed to write F");
    let mut command = guest_command(
        SELF_COPY_SCRIPT,
        &[],
        &[std::ffi::OsStr::new(tool), file.as_os_str()],
    );
    // Bounds a regression: `cat` copying F onto itself stops at 1 MiB instead
    // of filling the disk. A wrapper that starts hermit in a separate service
    // does not pass these limits on.
    // SAFETY: the closure runs in the child between fork and exec and calls
    // only setrlimit, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            for (resource, limit) in [(libc::RLIMIT_FSIZE, 1 << 20), (libc::RLIMIT_CORE, 0)] {
                let rlimit = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &rlimit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let (rendered, output) = spawn_guest(
        command,
        HermitStdin::Null,
        HermitOutput::File(&file),
        HermitOutput::Pipe,
    );
    let after = fs::read(&file).expect("failed to read F");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        after.len(),
        contents.len(),
        "{tool} changed the length of a file that is hermit's stdout: {rendered}\n{stderr}"
    );
    assert_eq!(after, contents, "{tool} changed F: {rendered}\n{stderr}");
    assert_guest_succeeded(&rendered, &output);
    assert!(
        stderr.contains(refusal) && stderr.contains(&format!("\n{tool} exited 1\n")),
        "{tool} did not refuse to copy F onto itself with {refusal:?}: {rendered}\n{stderr}"
    );
}

#[test]
fn cat_refuses_a_file_that_is_hermits_stdout() {
    assert_self_copy_refused("cat", "input file is output file");
}

#[test]
fn cp_to_dev_stdout_refuses_a_file_that_is_hermits_stdout() {
    assert_self_copy_refused("cp", "are the same file");
}
