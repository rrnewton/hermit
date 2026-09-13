// Exercise the actual startup boundary in a fresh process. The parent test
// process never changes its environment or transfers ownership of its files.
use std::fs;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const FD_ENV: &str = "DAGRUN_DELEGATED_PARENT_FD";
const CHILD_ENV: &str = "DAGRUN_DELEGATED_PARENT_CHILD";
const CHILD_FD: i32 = 198;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn harness() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_test-harness"));
    command
        .current_dir(root())
        .args(["plan", "--lane", "portable"])
        .env_remove(FD_ENV)
        .env_remove(CHILD_ENV);
    command
}

fn inherited(command: &mut Command, file: &File) {
    let source = file.as_raw_fd();
    command.env(FD_ENV, CHILD_FD.to_string());
    // SAFETY: only async-signal-safe descriptor operations run after fork. The
    // parent retains its own File; the child receives a separate descriptor.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source, CHILD_FD) == -1 || libc::fcntl(CHILD_FD, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn refused(output: Output, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(diagnostic), "{stderr}");
}

#[test]
fn harness_startup_refuses_malformed_transport() {
    for descriptor in ["", "-1", "0", "2", "3junk", "2147483648"] {
        refused(
            harness()
                .env(FD_ENV, descriptor)
                .env(CHILD_ENV, "child")
                .output()
                .unwrap(),
            "delegated parent handoff requires a descriptor number above standard I/O",
        );
    }
    refused(
        harness().env(CHILD_ENV, "child").output().unwrap(),
        "delegated parent handoff requires a descriptor number above standard I/O",
    );
}

#[test]
fn harness_startup_refuses_a_closed_descriptor() {
    let mut command = harness();
    command
        .env(FD_ENV, CHILD_FD.to_string())
        .env(CHILD_ENV, "child");
    // SAFETY: close is async-signal-safe, and this is only the child's fd table.
    unsafe {
        command.pre_exec(|| {
            libc::close(CHILD_FD);
            Ok(())
        });
    }
    refused(
        command.output().unwrap(),
        "delegated parent descriptor is unavailable",
    );
}

#[test]
fn harness_startup_requires_both_transport_fields() {
    let file = File::open("/dev/null").unwrap();
    let mut command = harness();
    inherited(&mut command, &file);
    refused(
        command.output().unwrap(),
        "delegated parent handoff is missing its child name",
    );
}

#[test]
fn harness_startup_refuses_a_file_or_ordinary_directory_as_authority() {
    let directory = tempfile::tempdir().unwrap();
    let file_path = directory.path().join("file");
    fs::write(&file_path, b"not a cgroup").unwrap();
    for (path, diagnostic) in [
        (file_path.as_path(), "must hold a live directory"),
        (directory.path(), "not on cgroup-v2"),
    ] {
        let file = File::open(path).unwrap();
        let mut command = harness();
        inherited(&mut command, &file);
        command.env(CHILD_ENV, "child");
        refused(command.output().unwrap(), diagnostic);
    }
}

fn wrapper_command(temp: &tempfile::TempDir) -> Command {
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    fs::write(&podman, "#!/bin/sh\nif [ \"$1\" = image ] && [ \"$2\" = exists ]; then exit 0; fi\nprintf '%s\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new("/bin/bash");
    command
        .arg(root().join("ci/hermetic/run-in-pinned-root.sh"))
        .args([
            "--src",
            source.to_str().unwrap(),
            "--out",
            temp.path().join("out").to_str().unwrap(),
            "--digest",
            "local-test@sha256:unused",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove(FD_ENV)
        .env_remove(CHILD_ENV);
    command
}

#[test]
fn pinned_wrapper_does_not_transfer_a_parent_to_ordinary_commands() {
    let temp = tempfile::tempdir().unwrap();
    let output = wrapper_command(&temp)
        .args(["--", "/bin/true"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let argv: Vec<_> = stdout.lines().collect();
    assert_eq!(&argv[..2], ["run", "--rm"]);
    assert_eq!(
        argv.iter()
            .filter(|arg| **arg == "--cgroups=disabled")
            .count(),
        1
    );
    assert!(!stdout.contains("--preserve-fd"));
    assert!(!stdout.contains(FD_ENV));
    assert!(!stdout.contains(CHILD_ENV));
    assert_eq!(argv.last(), Some(&"/bin/true"));
}

#[test]
fn pinned_wrapper_refuses_an_unowned_handoff_or_reserved_forwarding() {
    for name in [FD_ENV, CHILD_ENV] {
        let temp = tempfile::tempdir().unwrap();
        refused(
            wrapper_command(&temp)
                .env(name, "")
                .args(["--", "/bin/true"])
                .output()
                .unwrap(),
            &format!("inherited {name} is not an owned parent handoff"),
        );
        let temp = tempfile::tempdir().unwrap();
        refused(
            wrapper_command(&temp)
                .args(["--env", name, "--", "/bin/true"])
                .output()
                .unwrap(),
            &format!("{name} is reserved for --share-cgroup-parent"),
        );
    }
}
