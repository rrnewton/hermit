/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exercise the existing runtime guard through its actual public dispatcher.
//! Only ordinary children run here: no runtime installation and no Hermit.

mod exec_support;

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::time::Duration;

use reverie_inguest::dispatch::PassthroughDispatcher;
use reverie_inguest::trap;

const ROLE: &str = "REVERIE_LB_PRODUCTION_GATE_TEST_ROLE";
const TEST_NAME: &str = "lb7_production_exec_gate_remains_refused";
const GATE_COMPLETE: &str = "LB7_ACTUAL_PRODUCTION_EXEC_GATE_COMPLETE";
const CHECK_COMPLETE: &str = "LB7_NATIVE_EXECVE_CHECK_COMPLETE";
const AT_EXECVE_CHECK: u64 = 0x10000;

struct ExecRequests {
    path_fd: File,
    argv: [*const libc::c_char; 2],
    envp: [*const libc::c_char; 1],
}

impl ExecRequests {
    fn new() -> Self {
        Self {
            path_fd: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open("/bin/true")
                .expect("pin native executable"),
            argv: [c"/bin/true".as_ptr(), std::ptr::null()],
            envp: [std::ptr::null()],
        }
    }

    fn request(&self, case: &str) -> (i64, [u64; 6]) {
        let path = c"/bin/true".as_ptr() as u64;
        let empty = c"".as_ptr() as u64;
        let argv = self.argv.as_ptr() as u64;
        let envp = self.envp.as_ptr() as u64;
        let cwd = libc::AT_FDCWD as i64 as u64;
        let pinned = self.path_fd.as_raw_fd() as u64;
        match case {
            "execve" => (libc::SYS_execve, [path, argv, envp, 0, 0, 0]),
            "execveat-path" => (libc::SYS_execveat, [cwd, path, argv, envp, 0, 0]),
            "execveat-pinned" => (
                libc::SYS_execveat,
                [pinned, empty, argv, envp, libc::AT_EMPTY_PATH as u64, 0],
            ),
            "execveat-check-path" => (
                libc::SYS_execveat,
                [cwd, path, argv, envp, AT_EXECVE_CHECK, 0],
            ),
            "execveat-check-pinned" => (
                libc::SYS_execveat,
                [
                    pinned,
                    empty,
                    argv,
                    envp,
                    AT_EXECVE_CHECK | libc::AT_EMPTY_PATH as u64,
                    0,
                ],
            ),
            other => panic!("unknown exec request: {other}"),
        }
    }
}

const CASES: [&str; 5] = [
    "execve",
    "execveat-path",
    "execveat-pinned",
    "execveat-check-path",
    "execveat-check-pinned",
];

#[derive(Debug, Eq, PartialEq)]
struct ImageWitness {
    pid: u32,
    exe_link: PathBuf,
    exe_device: u64,
    exe_inode: u64,
    auxv: Vec<u8>,
    seccomp_state: Vec<String>,
    descriptor_flags: i32,
}

impl ImageWitness {
    fn current(file: &File) -> Self {
        let exe = fs::metadata("/proc/self/exe").expect("current image metadata");
        let status = fs::read_to_string("/proc/self/status").expect("current process status");
        // The test runner can already inherit a sandbox filter. The direct
        // dispatcher must leave that existing state exactly as it was.
        let seccomp_state = status
            .lines()
            .filter(|line| line.starts_with("Seccomp:") || line.starts_with("Seccomp_filters:"))
            .map(str::to_owned)
            .collect();
        // SAFETY: F_GETFD only inspects this owned, live descriptor.
        let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor_flags >= 0, "pinned descriptor must stay open");
        Self {
            pid: std::process::id(),
            exe_link: fs::read_link("/proc/self/exe").expect("current image link"),
            exe_device: exe.dev(),
            exe_inode: exe.ino(),
            auxv: fs::read("/proc/self/auxv").expect("current image auxv"),
            seccomp_state,
            descriptor_flags,
        }
    }
}

fn native_control(case: &str) {
    assert!(
        !trap::has_dispatcher(),
        "constructor feature must be disabled"
    );
    let request = ExecRequests::new();
    let (number, args) = request.request(case);
    // SAFETY: the executable name, argv, envp and O_PATH descriptor are live;
    // this isolated child may be replaced by the ordinary native executable.
    let result =
        unsafe { libc::syscall(number, args[0], args[1], args[2], args[3], args[4], args[5]) };
    if case.contains("check") {
        assert_eq!(
            result,
            0,
            "native CHECK: {}",
            std::io::Error::last_os_error()
        );
        println!("{CHECK_COMPLETE} {case}");
    } else {
        panic!(
            "native {case} must replace this child, returned {result}: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn actual_gate_child() {
    assert_eq!(libc::ENOTSUP, libc::EOPNOTSUPP);
    assert!(
        !trap::has_dispatcher(),
        "constructor feature must be disabled"
    );
    let requests = ExecRequests::new();
    let witness = ImageWitness::current(&requests.path_fd);
    let sentinel = Box::new([0x937f_b316_c9a4_20d5_u64; 37]);
    let sentinel_address = sentinel.as_ptr();
    trap::set_dispatcher(Box::new(PassthroughDispatcher::new()));
    assert!(trap::has_dispatcher());
    for case in CASES {
        let (number, args) = requests.request(case);
        let result = trap::dispatch_direct(number, args, 0x12_3456);
        assert_eq!(result, -i64::from(libc::EOPNOTSUPP), "{case}");
        assert_eq!(ImageWitness::current(&requests.path_fd), witness, "{case}");
        assert_eq!(sentinel.as_ptr(), sentinel_address, "{case}");
        assert!(sentinel.iter().all(|word| *word == 0x937f_b316_c9a4_20d5));
    }
    // A forwarded successful /bin/true would exit zero without this marker.
    // Requiring it proves that the original test image resumed after all calls.
    println!("{GATE_COMPLETE}");
}

fn bounded_child(role: &str) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(ROLE, role);
    // Expiry kills the child's process group and fails immediately; reaping
    // is bounded and asynchronous, never a blocking wait after SIGKILL.
    let directory = exec_support::fixture_dir(&format!("lb7-production-gate-{role}"));
    exec_support::run_monitored_output(
        command,
        &format!("bounded {role} child, output under {directory:?}"),
        Duration::from_secs(5),
        &directory,
    )
}

#[test]
fn lb7_production_exec_gate_remains_refused() {
    if let Some(role) = std::env::var_os(ROLE) {
        let role = role.to_str().expect("test role is ASCII");
        if role == "guard" {
            actual_gate_child();
        } else {
            native_control(role);
        }
        return;
    }
    assert!(
        !trap::has_dispatcher(),
        "parent must remain isolated from dispatcher changes"
    );
    for case in CASES {
        let output = bounded_child(case);
        assert!(
            output.status.success(),
            "native {case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if case.contains("check") {
            assert!(String::from_utf8_lossy(&output.stdout).contains(CHECK_COMPLETE));
        }
    }
    let output = bounded_child("guard");
    assert!(
        output.status.success(),
        "actual production guard: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(GATE_COMPLETE));
    assert!(
        !trap::has_dispatcher(),
        "child dispatcher cannot affect parent"
    );
    fs::write(
        Path::new(env!("ELF_LOADER_ARTIFACT_DIR")).join("lb7-production-gate.result"),
        "PASS actual production exec gate\nNative execve, execveat path/pinned and CHECK path/pinned succeed; actual PassthroughDispatcher returns -EOPNOTSUPP for all five. Original exe, auxv, pid, descriptor, memory and seccomp state stay unchanged. No runtime installation or Hermit execution.\n",
    )
    .unwrap();
}

fn assert_no_loader_reference(path: &Path) -> usize {
    if path.is_dir() {
        let mut checked = 0;
        for entry in fs::read_dir(path).expect("read production source directory") {
            let entry = entry.expect("production source entry");
            if entry.file_type().expect("source entry type").is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if ["target", "tests", ".git", "reverie-elf-loader"]
                .iter()
                .any(|skip| name == *skip)
            {
                continue;
            }
            checked += assert_no_loader_reference(&entry.path());
        }
        checked
    } else if path.extension().is_some_and(|extension| extension == "rs")
        || path.file_name().is_some_and(|name| name == "Cargo.toml")
    {
        let source = fs::read_to_string(path).expect("read production source");
        for reference in ["reverie_elf_loader", "reverie-elf-loader"] {
            assert!(
                !source.contains(reference),
                "production source references inactive loader: {} ({reference})",
                path.display()
            );
        }
        1
    } else {
        0
    }
}

#[test]
fn lb7_production_sources_do_not_wire_the_loader() {
    const EXEC_GUARD: &str = "        // AUTONOMOUS-BOT-IMPLEMENTED\n        // exec cannot safely cross an inherited trap filter: the filter survives\n        // but the handler, altstack, and mappings do not.\n        if number == libc::SYS_execve || number == libc::SYS_execveat {\n            event.fail(libc::ENOTSUP);\n            return true;\n        }";
    let dispatcher = include_str!("../../reverie-inguest/src/dispatch.rs");
    assert!(
        dispatcher.contains(EXEC_GUARD),
        "existing exec guard changed"
    );
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let manifest = fs::read_to_string(workspace.join("Cargo.toml")).expect("workspace manifest");
    let members = manifest
        .split_once("members = [")
        .expect("workspace members")
        .1
        .split_once(']')
        .expect("workspace member array")
        .0;
    let mut checked = 0;
    let mut packages = 0;
    for member in members
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let member = member.trim_end_matches(',').trim_matches('"');
        if member == "reverie-elf-loader" {
            continue;
        }
        assert!(workspace.join(member).is_dir(), "workspace member exists");
        checked += assert_no_loader_reference(&workspace.join(member));
        packages += 1;
    }
    assert!(
        checked > 100,
        "scan must cover the actual production sources"
    );
    fs::write(
        Path::new(env!("ELF_LOADER_ARTIFACT_DIR")).join("lb7-production-source-inactivity.result"),
        format!(
            "PASS Reverie production source inactivity\nChecked {checked} Rust sources/Cargo manifests in {packages} production workspace packages; no inactive loader crate reference. Exact original exec guard text remains present. Hermit source is outside this checkout and is a separate read-only review item.\n"
        ),
    )
    .unwrap();
}
