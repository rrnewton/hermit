// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the repository LICENSE file.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

struct Controller {
    child: Option<Child>,
    deadline: Instant,
    socket: PathBuf,
    contact: PathBuf,
    stdout_prefix: Vec<u8>,
}

impl Controller {
    fn start(fixture: &Path, directory: &Path) -> Self {
        fs::create_dir(directory).unwrap();
        let started = Instant::now();
        let child = Command::new(fixture)
            .current_dir(directory)
            .args(["controller", "external.sock", "contact"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start Unix Q/A controller");
        let mut controller = Self {
            child: Some(child),
            deadline: started + Duration::from_secs(11),
            socket: directory.join("external.sock"),
            contact: directory.join("contact"),
            stdout_prefix: Vec::new(),
        };
        let stdout = controller
            .child
            .as_ref()
            .unwrap()
            .stdout
            .as_ref()
            .unwrap()
            .as_raw_fd();
        let flags = unsafe { libc::fcntl(stdout, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(stdout, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        loop {
            assert!(
                Instant::now() < started + Duration::from_secs(2),
                "Unix controller did not announce listen readiness"
            );
            assert!(
                controller
                    .child
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "Unix controller exited before listen readiness"
            );
            let mut byte = [0u8; 1];
            match controller
                .child
                .as_mut()
                .unwrap()
                .stdout
                .as_mut()
                .unwrap()
                .read(&mut byte)
            {
                Ok(0) => panic!("Unix controller closed output before ready"),
                Ok(1) => {
                    controller.stdout_prefix.push(byte[0]);
                    assert!(
                        controller.stdout_prefix.len() <= 1024,
                        "controller readiness byte bound"
                    );
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Ok(_) => unreachable!(),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("controller readiness: {error}"),
            }
        }
        assert!(Instant::now() < started + Duration::from_secs(2));
        assert_eq!(controller.stdout_prefix, b"controller=ready\n");
        assert_eq!(unsafe { libc::fcntl(stdout, libc::F_SETFL, flags) }, 0);
        assert!(
            fs::symlink_metadata(&controller.socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
        controller
    }

    fn finish(mut self, evidence: &Path, label: &str) -> Output {
        loop {
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < self.deadline,
                "Unix controller exceeded its original independent 11-second bound"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let mut output = self.child.take().unwrap().wait_with_output().unwrap();
        self.stdout_prefix.extend_from_slice(&output.stdout);
        output.stdout = std::mem::take(&mut self.stdout_prefix);
        assert!(
            Instant::now() <= self.deadline,
            "Unix controller terminal readback exceeded deadline"
        );
        fs::write(
            evidence.join(format!("{label}.controller.stdout")),
            &output.stdout,
        )
        .unwrap();
        fs::write(
            evidence.join(format!("{label}.controller.stderr")),
            &output.stderr,
        )
        .unwrap();
        fs::write(
            evidence.join(format!("{label}.controller.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "exit_code": output.status.code(), "contact": self.contact.exists(),
                "socket_absent": !self.socket.exists(), "natural_completion": true,
            }))
            .unwrap(),
        )
        .unwrap();
        output
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn guard_roots() -> (PathBuf, PathBuf) {
    let required = |name| {
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| {
            panic!("normal Unix gate requires prepared deployment input {name}")
        }))
    };
    (
        required("HERMIT_PREPARED_NETWORK_GUARD_BPFFS"),
        required("HERMIT_PREPARED_NETWORK_GUARD_RECOVERY"),
    )
}
fn receipt_names(root: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".terminal.jsonl")
        })
        .collect()
}
fn assert_guard_receipts(root: &Path, before: &BTreeSet<PathBuf>, count: usize, policy: bool) {
    let after = receipt_names(root);
    let new: Vec<_> = after.difference(before).collect();
    assert_eq!(
        new.len(),
        count,
        "actual invocation terminal receipt population"
    );
    for path in new {
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= 1024 * 1024);
        let rows: Vec<serde_json::Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let terminal: Vec<_> = rows.iter().filter(|r| r["stage"] == "terminal").collect();
        assert_eq!(terminal.len(), 1, "{rows:?}");
        let r = terminal[0];
        assert_eq!(r["schema"], 1);
        let g = &r["guard"];
        let incarnation = g["incarnation"].as_u64().unwrap();
        assert_ne!(incarnation, 0);
        assert_eq!(g["birth"]["incarnation"], incarnation);
        for field in ["sequence", "object", "generation", "cookie"] {
            assert!(g["birth"][field].as_u64().unwrap() > 0);
        }
        assert!(g["terminal"]["initial_tasks"].as_u64().unwrap() > 0);
        assert_eq!(g["terminal"]["removed_links"], 31);
        assert_eq!(g["terminal"]["removed_map_pins"], 10);
        assert_eq!(g["inventory"]["counts"], serde_json::json!([10, 31, 31]));
        let ids = g["inventory"]["ids"].as_array().unwrap();
        assert_eq!(ids.len(), 72);
        let exact: BTreeSet<_> = ids
            .iter()
            .map(|p| (p[0].as_u64().unwrap(), p[1].as_u64().unwrap()))
            .collect();
        assert_eq!(exact.len(), 72);
        for (kind, n) in [(0, 10), (1, 31), (2, 31)] {
            assert_eq!(
                exact.iter().filter(|(k, id)| *k == kind && *id > 0).count(),
                n
            );
        }
        let read = &g["readback"];
        assert_eq!(read["original_ids"], 72);
        assert_eq!(read["complete_passes"], 2);
        let closed = read["closed_ns"].as_u64().unwrap();
        let deadline = read["deadline_ns"].as_u64().unwrap();
        let observed = read["observed_ns"].as_u64().unwrap();
        assert!(closed < observed && observed < deadline && deadline - closed <= 1_000_000_000);
        let drained = r["terminal_observed_ns"].as_u64().unwrap();
        assert!(
            observed <= drained && drained < deadline,
            "all actor drain must fit the same original window"
        );
        for actor in ["loader", "query"] {
            let unit = &r[actor];
            assert!(!unit["invocation"].as_str().unwrap().is_empty());
            assert!(!Path::new(unit["cgroup"].as_str().unwrap()).exists());
            assert!(unit["inode"].as_u64().unwrap() > 0);
        }
        assert_eq!(r["loader_wait"], 0);
        assert_eq!(r["query_wait"], 0);
        assert!(r["child"]["pid"].as_i64().unwrap() > 0);
        assert!(r["child"]["signal"].is_null());
        if policy {
            assert_eq!(g["terminal"]["outcome"], "policy");
            assert_eq!(r["child"]["exit_code"], 122);
            let d = &g["denial"];
            assert_eq!(d["incarnation"], incarnation);
            assert_eq!(d["phase"], 2);
            assert_eq!(d["reason"], 3, "actual external-peer denial required");
            assert!(matches!(d["hook"].as_u64(), Some(1 | 2)));
            assert!(d["task_start"].as_u64().unwrap() > 0 && d["pid_tgid"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(g["terminal"]["outcome"], "running");
            assert!(g["denial"].is_null());
            assert_eq!(r["child"]["exit_code"], 0);
        }
    }
}

#[test]
fn default_unix_denies_external_contact_and_preserves_guest_ipc() {
    super::network_boundary::initialize("unix");
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_default_unix").path;
    let (bpffs_root, recovery_root) = guard_roots();
    let root_arguments = [
        format!("--network-guard-bpffs={}", bpffs_root.display()),
        format!("--network-guard-recovery={}", recovery_root.display()),
    ];
    let temporary;
    let evidence = if let Some(path) = std::env::var_os("HERMIT_NETWORK_UNIX_EVIDENCE") {
        let path = PathBuf::from(path);
        assert!(!path.exists(), "Unix evidence must be a new directory");
        fs::create_dir_all(&path).unwrap();
        path
    } else {
        temporary = tempfile::tempdir().unwrap();
        temporary.path().to_owned()
    };
    // Positive controller control uses the unchanged native Q/A fixture, so a
    // broken external controller cannot satisfy the no-contact requirement.
    let native_directory = evidence.join("native-controller");
    let native_controller = Controller::start(fixture, &native_directory);
    let native = Command::new("timeout")
        .args(["--kill-after=1s", "10s"])
        .arg(fixture)
        .current_dir(&native_directory)
        .args(["client", "external.sock"])
        .output()
        .unwrap();
    assert!(
        native.status.success(),
        "native Unix Q/A failed: {native:?}"
    );
    assert_eq!(native.stdout, b"connect=success request=Q response=A\n");
    let native_server = native_controller.finish(&evidence, "native");
    assert!(native_server.status.success(), "{native_server:?}");
    assert_eq!(
        fs::read(native_directory.join("contact")).unwrap(),
        b"external_contact=1\n"
    );
    assert_eq!(
        native_server.stdout,
        b"controller=ready\ncontroller=complete\n"
    );

    let sockets = evidence.join("external-controller");
    let controller = Controller::start(fixture, &sockets);
    let directory = fs::symlink_metadata(&sockets).unwrap();
    assert!(directory.is_dir() && !directory.file_type().is_symlink());
    let identity = (directory.dev(), directory.ino());
    let socket_identity = fs::symlink_metadata(&controller.socket).unwrap();
    assert!(socket_identity.file_type().is_socket());
    let address = "/tmp/zero-origin/external.sock";
    assert!(address.len() < 108 && "external.sock".len() < 108);
    fs::write(
        evidence.join("external-identities.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "directory_device": identity.0, "directory_inode": identity.1,
            "socket_device": socket_identity.dev(), "socket_inode": socket_identity.ino(),
            "controller_bind": "external.sock", "guest_connect": address,
        }))
        .unwrap(),
    )
    .unwrap();
    let guest = Path::new("/tmp/unix-fixtures").join(fixture.file_name().unwrap());
    let mut arguments = super::network_only::common_run_arguments(0, 1_000_000);
    arguments.extend(root_arguments.clone());
    let receipts_before = receipt_names(&recovery_root);
    arguments.push(format!(
        "--bind={}:/tmp/unix-fixtures",
        fixture.parent().unwrap().display()
    ));
    arguments.push(format!(
        "--mount=type=bind,source={},target=/tmp/zero-origin,bind-propagation=rshared",
        sockets.display()
    ));
    let output = super::network_only::safehermit_command(
        &evidence,
        "external",
        &arguments,
        &guest,
        &["client", address],
    );
    let server = controller.finish(&evidence, "external");
    assert_guard_receipts(&recovery_root, &receipts_before, 1, true);
    // Preserve the historical pre-contact oracle and refusal diagnostic. A
    // crash/startup failure or forcibly killed controller is not a denial.
    assert!(
        !sockets.join("contact").exists(),
        "outside controller accepted a guest connection"
    );
    assert_eq!(
        server.status.code(),
        Some(4),
        "no-contact requires natural controller timeout, not killed controller"
    );
    assert!(
        !output.status.success() && !matches!(output.status.code(), None | Some(124 | 125 | 126)),
        "{output:?}"
    );
    // The shared controller's typed policy terminal is exactly 122. A guest
    // errno alone, a startup error, or another nonzero result is insufficient.
    assert_eq!(
        output.status.code(),
        Some(122),
        "expected typed Unix policy denial"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();
    assert!(
        text.contains("network") && text.contains("disabled"),
        "wrong refusal: {text}"
    );
    assert!(!sockets.join("external.sock").exists());
    let after = fs::symlink_metadata(&sockets).unwrap();
    assert_eq!((after.dev(), after.ino()), identity);
    assert_eq!(server.stdout, b"controller=ready\n");

    // Both supported local mechanisms must succeed under default policy and
    // full strict two-execution parity. The internal fixture forks and reaps.
    for (case, expected) in [
        ("pair", "pair=success payload=P\n"),
        (
            "internal",
            "connect=success request=Q response=A\ninternal=success child_reaped=1\n",
        ),
    ] {
        let report = evidence.join(format!("{case}.verify.json"));
        let logs = evidence.join(format!("{case}.logs"));
        fs::create_dir(&logs).unwrap();
        let mut arguments = super::network_only::common_run_arguments(0, 1_000_000);
        arguments.extend(root_arguments.clone());
        let receipts_before = receipt_names(&recovery_root);
        arguments.extend([
            format!(
                "--bind={}:/tmp/unix-fixtures",
                fixture.parent().unwrap().display()
            ),
            "--verify".into(),
            "--verify-strict".into(),
            "--keep-logs".into(),
            format!("--verify-json={}", report.display()),
            format!("--verify-log-dir={}", logs.display()),
        ]);
        let client = if case == "pair" {
            vec![case]
        } else {
            vec![case, "/tmp/guest-local.sock"]
        };
        let output =
            super::network_only::safehermit_command(&evidence, case, &arguments, &guest, &client);
        super::network_only::assert_success(&output, case);
        assert_guard_receipts(&recovery_root, &receipts_before, 2, false);
        assert_eq!(std::str::from_utf8(&output.stdout).unwrap(), expected);
        super::network_only::assert_l2_report(&report, case);
        assert_eq!(
            fs::read_dir(logs).unwrap().count(),
            2,
            "retain both strict execution logs"
        );
    }
}
