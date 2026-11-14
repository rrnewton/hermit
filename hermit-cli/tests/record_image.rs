/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for `hermit record start --image` and the
//! replay re-entry it binds: a dynamically linked guest from the
//! image records and verifies at L2 inside an `oci-dir:` rootfs on
//! ptrace, the recording's metadata carries the fixture's content
//! identity, `hermit replay` re-enters the image (an image-only
//! file is visible to the replayed guest), and every refusal — the
//! image moved away, different content at the same path, an edited
//! metadata identity, a library replay with no identity — names the
//! recording and the image before any guest starts. A recording
//! made without `--image` replays as before.

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/image_fixture.rs"]
mod image_fixture;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use image_fixture::*;

// Serializes this file's Hermit child processes, as in cli.rs: a
// failed assertion carries no protected state and must not poison
// unrelated tests.
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

fn hermit_run_guard() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static DYNAMIC_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The dynamically linked guest: an ordinary libc binary, so running
/// it from the image exercises the image's loader and libraries —
/// the point of recording inside a rootfs. It prints the marker,
/// then the contents of each `read:<path>` argument.
fn dynamic_guest() -> &'static Path {
    DYNAMIC_GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("record-image-guest")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&build_root).expect("create guest build dir");
        let source = build_root.join("guest.c");
        std::fs::write(
            &source,
            r#"
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    printf("GUEST_MARKER=hello-from-image\n");
    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "read:", 5) == 0) {
            const char *p = argv[i] + 5;
            FILE *f = fopen(p, "r");
            printf("FILE %s=", p);
            if (!f) { printf("ERR\n"); continue; }
            char buf[1024];
            size_t n;
            while ((n = fread(buf, 1, sizeof buf, f)) > 0)
                fwrite(buf, 1, n, stdout);
            fclose(f);
            printf("\n");
        }
    }
    return 0;
}
"#,
        )
        .expect("write guest source");
        let guest = build_root.join("guest");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&guest)
            .arg(&source)
            .output()
            .expect("failed to run cc for the dynamic image guest");
        assert!(
            output.status.success(),
            "dynamic image guest compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        guest
    })
}

/// The host paths of the dynamic guest's loader and libraries, from
/// `ldd`: the files the image must carry for the guest to run there.
fn guest_libraries(guest: &Path) -> Vec<PathBuf> {
    let output = Command::new("ldd")
        .arg(guest)
        .output()
        .expect("failed to run ldd for the dynamic image guest");
    assert!(
        output.status.success(),
        "ldd failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut libraries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        let candidate = match line.split_once("=>") {
            Some((_, rest)) => rest.trim().split(' ').next().unwrap_or_default(),
            None if line.starts_with('/') => line.split(' ').next().unwrap_or_default(),
            None => continue,
        };
        if candidate.starts_with('/') {
            libraries.push(PathBuf::from(candidate));
        }
    }
    assert!(
        !libraries.is_empty(),
        "ldd named no libraries for the dynamic guest"
    );
    libraries
}

/// An oci-dir holding the dynamic guest at `/bin/guest`, its loader
/// and libraries at their host paths, and `/etc/marker.txt` with
/// `marker`: a one-layer fixture in the shape of
/// `write_valid_oci_dir`. Returns the manifest digest hex, which is
/// the content identity a recording of this image binds.
fn write_dynamic_oci_dir(root: &Path, marker: &[u8]) -> String {
    let guest = dynamic_guest();
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        ("bin/guest".to_owned(), std::fs::read(guest).unwrap(), 0o755),
        ("etc/marker.txt".to_owned(), marker.to_vec(), 0o644),
    ];
    for library in guest_libraries(guest) {
        files.push((
            library.to_string_lossy().trim_start_matches('/').to_owned(),
            std::fs::read(&library).unwrap(),
            0o755,
        ));
    }
    let mut dirs = std::collections::BTreeSet::new();
    for (name, _, _) in &files {
        let mut parent = Path::new(name).parent();
        while let Some(dir) = parent {
            if !dir.as_os_str().is_empty() {
                dirs.insert(dir.to_string_lossy().into_owned());
            }
            parent = dir.parent();
        }
    }
    let mut entries: Vec<Entry> = dirs
        .iter()
        .map(|dir| dir_entry(dir, 0o755, 1_600_000_000))
        .collect();
    entries.extend(
        files
            .iter()
            .map(|(name, data, mode)| entry(name, tar::EntryType::Regular, *mode, data)),
    );
    let layer = build_tar(&entries);
    let config = config_bytes(&[sha256_hex(&layer)]);
    write_oci_dir(
        root,
        &config,
        &[StoredLayer {
            blob: layer,
            media_type: MT_TAR,
        }],
    )
}

/// One test's scratch: the rootfs cache, the recording store, and
/// the oci-dir, each in its own directory under one temporary root.
struct Scratch {
    root: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        std::fs::create_dir_all(root.path().join("cache")).unwrap();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        Self { root }
    }

    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn oci(&self) -> PathBuf {
        self.root.path().join("oci")
    }

    fn image(&self) -> String {
        format!("oci-dir:{}", self.oci().display())
    }

    /// Run Hermit with this scratch's cache and recording store.
    fn hermit(&self, args: &[&str]) -> Output {
        let mut command = Command::new(hermit_test::hermit_binary());
        command
            .args(args)
            .env("XDG_CACHE_HOME", self.cache())
            .env("HERMIT_DATA_DIR", self.data())
            .stdin(std::process::Stdio::null());
        hermit_test::hermit_output(&format!("hermit {args:?}"), &mut command)
    }

    /// Record the image guest (no `--verify`) and return the
    /// recording's directory.
    fn record(&self) -> PathBuf {
        let image = format!("--image={}", self.image());
        let output = self.hermit(&[
            "record",
            "start",
            &image,
            "--",
            "/bin/guest",
            "read:/etc/marker.txt",
        ]);
        assert_success(&output);
        let mut recordings: Vec<PathBuf> = std::fs::read_dir(self.data())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.join(hermit::RECORDING_METADATA_NAME).exists())
            .collect();
        assert_eq!(recordings.len(), 1, "recordings: {recordings:?}");
        recordings.pop().unwrap()
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "hermit failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        stdout_of(output),
        stderr_of(output)
    );
}

/// L2 on the ptrace backend: a dynamically linked guest from the
/// image records and immediately replays inside the same rootfs,
/// with the strict comparison's bitwise parity over a nonzero INFO
/// comparison. Both runs read `/etc/marker.txt`, which exists only
/// in the image, so a replay on the host root could not match.
#[test]
fn record_verify_inside_an_image_is_bitwise() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let report = scratch.root.path().join("verdict.json");
    let image = format!("--image={}", scratch.image());
    let report_arg = format!("--verify-json={}", report.display());
    let output = scratch.hermit(&[
        "record",
        "start",
        "--strict",
        &image,
        "--verify",
        "--verify-strict",
        &report_arg,
        "--",
        "/bin/guest",
        "read:/etc/marker.txt",
    ]);
    assert_success(&output);
    assert_eq!(
        stdout_of(&output),
        "GUEST_MARKER=hello-from-image\nFILE /etc/marker.txt=hello-image\n\n"
    );
    assert!(
        stderr_of(&output).contains("Success: replay matched recording."),
        "stderr: {}",
        stderr_of(&output)
    );
    let verdict: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).expect("record --verify wrote no verdict"))
            .expect("record --verify verdict is valid JSON");
    assert_eq!(verdict["bitwise_parity"], true, "{verdict}");
    assert_eq!(
        verdict["comparison"]["strictness"], "canonical",
        "{verdict}"
    );
    for side in ["left", "right"] {
        assert!(
            verdict["compared_log_messages"][side].as_u64().unwrap_or(0) > 0,
            "no INFO messages compared: {verdict}"
        );
    }
}

/// The recording binds the fixture's identity, and a later
/// `hermit replay` re-enters the image: the replayed guest sees the
/// image-only marker file and gives the recording's output.
#[test]
fn replay_reenters_the_recorded_image() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(recording.join(hermit::RECORDING_METADATA_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["image"]["reference"], scratch.image());
    assert_eq!(metadata["image"]["identity"]["source"], "oci-dir");
    assert_eq!(metadata["image"]["identity"]["identity"], digest);
    assert_eq!(
        metadata["image"]["identity"]["manifest_digest"],
        format!("sha256:{digest}")
    );

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_success(&output);
    assert_eq!(
        stdout_of(&output),
        "GUEST_MARKER=hello-from-image\nFILE /etc/marker.txt=hello-image\n\n"
    );
}

fn assert_refused(output: &Output, named: &[&str]) {
    assert!(
        !output.status.success(),
        "hermit unexpectedly succeeded: {}\nstdout: {}\nstderr: {}",
        output.status,
        stdout_of(output),
        stderr_of(output)
    );
    assert!(
        !stdout_of(output).contains("GUEST_MARKER"),
        "the guest ran: {}",
        stdout_of(output)
    );
    let stderr = stderr_of(output);
    for name in named {
        assert!(stderr.contains(name), "stderr missing {name:?}: {stderr}");
    }
}

/// Refusal 1: the recorded reference cannot be materialized (the
/// oci-dir moved away). The refusal names the recording, the image
/// and its identity, and no guest starts.
#[test]
fn replay_refuses_when_the_image_moved_away() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::rename(scratch.oci(), scratch.root.path().join("oci-gone")).unwrap();

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_refused(
        &output,
        &[
            &format!("recording {id} ran inside image {}", scratch.image()),
            &digest,
            "replay cannot re-enter it",
        ],
    );
}

/// Refusal 2: the recorded reference now resolves to different
/// content. The refusal names both identities.
#[test]
fn replay_refuses_different_content_at_the_same_path() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::remove_dir_all(scratch.oci()).unwrap();
    let swapped = write_dynamic_oci_dir(&scratch.oci(), b"hello-swapped\n");
    assert_ne!(swapped, digest);

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_refused(
        &output,
        &[
            &format!("recording {id} ran inside image {}", scratch.image()),
            &digest,
            &swapped,
            "does not match",
        ],
    );
}

/// A metadata.json edited to another (well-formed) identity refuses
/// the same way: the reference's content no longer matches what the
/// recording claims it ran in.
#[test]
fn replay_refuses_an_edited_metadata_identity() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let metadata_path = recording.join(hermit::RECORDING_METADATA_NAME);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    let edited = "b".repeat(64);
    metadata["image"]["identity"]["identity"] = edited.clone().into();
    metadata["image"]["identity"]["manifest_digest"] = format!("sha256:{edited}").into();
    std::fs::write(&metadata_path, metadata.to_string()).unwrap();

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_refused(
        &output,
        &[
            &format!("recording {id} ran inside image {}", scratch.image()),
            &edited,
            &digest,
            "does not match",
        ],
    );
}

/// Deleting the `image` field altogether must not downgrade the
/// recording to a host-root replay either: load refuses a
/// current-version metadata that states no field.
#[test]
fn replay_refuses_a_deleted_image_field() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let metadata_path = recording.join(hermit::RECORDING_METADATA_NAME);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata.as_object_mut().unwrap().remove("image");
    std::fs::write(&metadata_path, metadata.to_string()).unwrap();

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_refused(&output, &["states no \"image\" field"]);
}

/// The library guard: replaying an image recording through the
/// identity-less library entry points refuses instead of running it
/// on the host root. (The same guard at `Replay::spawn` is covered
/// directly by the library's unit tests; this is its public face.)
#[test]
fn library_replay_without_an_identity_refuses() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();

    let error = hermit::replay_with_output_and_mounts(&recording, &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("ran inside image"), "{error}");
    assert!(error.contains(&scratch.image()), "{error}");
    assert!(error.contains(&digest), "{error}");
    assert!(error.contains("host root"), "{error}");
}

/// No regression: a recording made without `--image` stores no
/// image and replays on the host root as before.
#[test]
fn host_recording_still_replays() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let guest = dynamic_guest().to_string_lossy().into_owned();
    let output = scratch.hermit(&["record", "start", "--", &guest]);
    assert_success(&output);

    let mut recordings: Vec<PathBuf> = std::fs::read_dir(scratch.data())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join(hermit::RECORDING_METADATA_NAME).exists())
        .collect();
    assert_eq!(recordings.len(), 1, "recordings: {recordings:?}");
    let recording = recordings.pop().unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(recording.join(hermit::RECORDING_METADATA_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["image"], serde_json::Value::Null);

    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_success(&output);
    assert_eq!(stdout_of(&output), "GUEST_MARKER=hello-from-image\n");
}

/// Both help spellings work on the changed surfaces and name the
/// new flag.
#[test]
fn record_image_help_names_the_flag() {
    for help in ["--help", "-h"] {
        let output = Command::new(hermit_test::hermit_binary())
            .args(["record", "start", help])
            .output()
            .unwrap();
        assert!(output.status.success(), "{help}: {}", output.status);
        assert!(
            stdout_of(&output).contains("--image"),
            "{help} does not name --image:\n{}",
            stdout_of(&output)
        );
        let output = Command::new(hermit_test::hermit_binary())
            .args(["replay", help])
            .output()
            .unwrap();
        assert!(output.status.success(), "replay {help}: {}", output.status);
    }
}

/// `--mount` composes with `--image` as it does for `run --image`:
/// the bind installs inside the rootfs for the recording and its
/// verify replay, and a target in a tree the image container
/// manages refuses by name before any guest starts.
#[test]
fn record_image_composes_with_mount() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let bound = scratch.root.path().join("mnt");
    std::fs::create_dir_all(&bound).unwrap();
    std::fs::write(bound.join("bound.txt"), b"from-bind\n").unwrap();
    let image = format!("--image={}", scratch.image());
    let mount = format!("type=bind,source={},target=/data", bound.display());
    let output = scratch.hermit(&[
        "record",
        "start",
        &image,
        "--mount",
        &mount,
        "--verify",
        "--",
        "/bin/guest",
        "read:/data/bound.txt",
        "read:/etc/marker.txt",
    ]);
    assert_success(&output);
    assert_eq!(
        stdout_of(&output),
        "GUEST_MARKER=hello-from-image\nFILE /data/bound.txt=from-bind\n\nFILE /etc/marker.txt=hello-image\n\n"
    );

    let refused = scratch.hermit(&[
        "record",
        "start",
        &image,
        "--mount",
        &format!("type=bind,source={},target=/proc/data", bound.display()),
        "--",
        "/bin/guest",
    ]);
    assert_refused(&refused, &["inside /proc"]);
}

/// Both `--verify-run` halves run inside the image: the first
/// records, the second replays the first's recording in its
/// recorded rootfs, and a replay half whose `--image` resolves to
/// other content refuses by name.
#[test]
fn verify_run_halves_run_inside_the_image() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    let digest = write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let image = format!("--image={}", scratch.image());
    let first = scratch.root.path().join("first");
    let second = scratch.root.path().join("second");
    let first_arg = format!("--verify-run={}", first.display());
    let output = scratch.hermit(&[
        "record",
        "start",
        &image,
        &first_arg,
        "--",
        "/bin/guest",
        "read:/etc/marker.txt",
    ]);
    assert_success(&output);

    let second_arg = format!("--verify-run={}", second.display());
    let replay_of = format!("--verify-replay-of={}", first.display());
    let output = scratch.hermit(&[
        "record",
        "start",
        &image,
        &second_arg,
        &replay_of,
        "--",
        "/bin/guest",
        "read:/etc/marker.txt",
    ]);
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(second.join("stdout")).unwrap(),
        "GUEST_MARKER=hello-from-image\nFILE /etc/marker.txt=hello-image\n\n"
    );

    // A replay half naming other content refuses, naming both.
    let other = scratch.root.path().join("other-oci");
    let other_digest = write_dynamic_oci_dir(&other, b"hello-other\n");
    let third = scratch.root.path().join("third");
    let output = scratch.hermit(&[
        "record",
        "start",
        &format!("--image=oci-dir:{}", other.display()),
        &format!("--verify-run={}", third.display()),
        &replay_of,
        "--",
        "/bin/guest",
        "read:/etc/marker.txt",
    ]);
    assert_refused(&output, &[&digest, &other_digest, "resolved to identity"]);
}

/// The gdb-served verify replay runs inside the image too: record,
/// then replay to completion under gdb.
#[test]
fn verify_with_gdbex_replays_inside_the_image() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let image = format!("--image={}", scratch.image());
    let output = scratch.hermit(&[
        "record",
        "start",
        &image,
        "--verify-with-gdbex",
        "continue",
        "--",
        "/bin/guest",
        "read:/etc/marker.txt",
    ]);
    assert_success(&output);
    assert!(
        stdout_of(&output).contains("GUEST_MARKER=hello-from-image"),
        "stdout: {}",
        stdout_of(&output)
    );
}

/// The image's Env/WorkingDir/Cmd are NOT applied to a recording
/// (unlike `run --image`, as the flag's help states): the static
/// fixture declares `TEST_ENV=from-image` and
/// `WorkingDir=/opt/appdir`, and the recorded guest still sees the
/// recorded (minimal) environment and `/`. The replay then runs
/// with exactly what was recorded.
#[test]
fn record_image_does_not_apply_image_config() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_valid_oci_dir(&scratch.oci());
    let image = format!("--image={}", scratch.image());
    let output = scratch.hermit(&[
        "record",
        "start",
        "--base-env=minimal",
        &image,
        "--verify",
        "--",
        "/bin/guest",
    ]);
    assert_success(&output);
    let stdout = stdout_of(&output);
    assert!(stdout.contains("TEST_ENV=(unset)"), "stdout: {stdout}");
    assert!(stdout.contains("CWD=/\n"), "stdout: {stdout}");
}

/// A replay does not need the recording's materialized cache: with
/// the cache deleted it re-materializes by identity and gives the
/// same output.
#[test]
fn replay_rematerializes_with_a_fresh_cache() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    let recording = scratch.record();
    let id = recording
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::remove_dir_all(scratch.cache()).unwrap();
    std::fs::create_dir_all(scratch.cache()).unwrap();

    let output = scratch.hermit(&["replay", "--autopilot", &id]);
    assert_success(&output);
    assert_eq!(
        stdout_of(&output),
        "GUEST_MARKER=hello-from-image\nFILE /etc/marker.txt=hello-image\n\n"
    );
}

/// Hermit's recording directory is bound into the image at its host
/// path — the named relaxation the flag's help states, matching
/// its visibility on the host root: the recording guest can read a
/// file placed there before the recording.
#[test]
fn recording_directory_is_bound_at_its_host_path() {
    let _guard = hermit_run_guard();
    let scratch = Scratch::new();
    write_dynamic_oci_dir(&scratch.oci(), b"hello-image\n");
    std::fs::write(scratch.data().join("host-note.txt"), b"host-note\n").unwrap();
    let note = format!("read:{}", scratch.data().join("host-note.txt").display());
    let image = format!("--image={}", scratch.image());
    let output = scratch.hermit(&[
        "record",
        "start",
        &image,
        "--verify",
        "--",
        "/bin/guest",
        &note,
    ]);
    assert_success(&output);
    assert!(
        stdout_of(&output).contains("host-note"),
        "stdout: {}",
        stdout_of(&output)
    );
}
