/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the local `--image` inputs
//! (`--image=oci-dir:...` and `--image=docker-archive:...`): both formats
//! materialize and run a no-libc guest with identical trees,
//! malformed inputs are refused by name with exit 125 before any guest
//! starts, and the pre-existing podman-store path keeps working (driven by
//! a scripted fake `podman`), while prefixed runs never invoke podman.
//!
//! Fixtures are built in-test from the same layer tars — no network, no
//! real podman. The guest is a no-libc binary (`cc -nostdlib
//! -nodefaultlibs -static`, raw syscalls only), because the image rootfs
//! contains nothing but the fixture and the pinned validation root has
//! no static libc to link a `cc -static` guest against.

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/image_fixture.rs"]
mod image_fixture;

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use image_fixture::*;

// ---------------------------------------------------------------------------
// Running Hermit and inspecting the materialized cache
// ---------------------------------------------------------------------------

fn run_hermit(cache: &Path, args: &[String]) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args(args)
        .env("XDG_CACHE_HOME", cache)
        .stdin(std::process::Stdio::null());
    hermit_test::hermit_output("hermit image run", &mut command)
}

fn run_image(cache: &Path, image: &str) -> Output {
    run_hermit(
        cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    )
}

/// Run the guest with `hermit`'s working directory set, so `./`- and
/// `../`-prefixed local image paths resolve as a user's shell would.
fn run_image_in_dir(cache: &Path, cwd: &Path, image: &str) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args([
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ])
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cache)
        .stdin(std::process::Stdio::null());
    hermit_test::hermit_output("hermit image run (relative)", &mut command)
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The single materialized rootfs under a per-test XDG cache.
fn find_rootfs(cache: &Path) -> PathBuf {
    let base = cache.join("hermit").join("oci-rootfs");
    let mut rootfs_dirs: Vec<PathBuf> = std::fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("cache base {} unreadable: {e}", base.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path().join("rootfs")))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        rootfs_dirs.len(),
        1,
        "expected exactly one materialized rootfs under {}, found {rootfs_dirs:?}",
        base.display()
    );
    rootfs_dirs.pop().unwrap()
}

fn assert_guest_ran(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "guest run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(output),
        stderr_of(output)
    );
    let stdout = stdout_of(output);
    assert!(stdout.contains("GUEST_MARKER=hello-from-image"), "{stdout}");
    assert!(stdout.contains("TEST_ENV=from-image"), "{stdout}");
    assert!(stdout.contains("CWD=/opt/appdir"), "{stdout}");
}

/// The golden-tree expectations both formats must materialize.
fn assert_golden_tree(rootfs: &Path) {
    let read = |rel: &str| std::fs::read(rootfs.join(rel)).unwrap();
    assert_eq!(read("etc/marker.txt"), b"hello-layer2\n");
    assert_eq!(
        std::fs::metadata(rootfs.join("etc/marker.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert!(
        !rootfs.join("etc/to-delete.txt").exists(),
        "whiteout must delete"
    );
    assert!(
        !rootfs.join("opt/opaque/old.txt").exists(),
        "opaque must clear"
    );
    assert_eq!(read("opt/opaque/new.txt"), b"new\n");

    let guest_meta = std::fs::metadata(rootfs.join("bin/guest")).unwrap();
    assert_eq!(read("bin/guest"), std::fs::read(guest_binary()).unwrap());
    assert_eq!(guest_meta.permissions().mode() & 0o7777, 0o755);
    assert_eq!(guest_meta.mtime(), GUEST_MTIME);
    // SAFETY: getuid always succeeds.
    let uid = unsafe { libc::getuid() };
    assert_eq!(
        guest_meta.uid(),
        uid,
        "layer uid/gid flatten to the invoker"
    );

    let hard_meta = std::fs::metadata(rootfs.join("bin/guest-hard")).unwrap();
    assert_eq!(
        hard_meta.ino(),
        guest_meta.ino(),
        "hardlink shares the inode"
    );
    assert_eq!(
        std::fs::read_link(rootfs.join("link.txt")).unwrap(),
        PathBuf::from("etc/marker.txt")
    );

    let appdir_meta = std::fs::metadata(rootfs.join("opt/appdir")).unwrap();
    assert_eq!(appdir_meta.permissions().mode() & 0o7777, 0o750);
    assert_eq!(appdir_meta.mtime(), APPDIR_MTIME, "deferred dir mtime");
    assert_eq!(read("opt/appdir/data.txt"), b"data\n");

    let fifo_meta = std::fs::symlink_metadata(rootfs.join("run/myfifo")).unwrap();
    assert_eq!(
        fifo_meta.mode() & libc::S_IFMT,
        libc::S_IFIFO,
        "FIFO preserved"
    );

    // The device entry under dev/ is skipped; finish provides placeholders
    // for its own bind targets only, so dev/console is deterministically
    // ABSENT (Hermit never binds the host console into an image guest).
    match std::fs::symlink_metadata(rootfs.join("dev/console")) {
        Ok(meta) => panic!(
            "dev/console must be absent (skipped device), found mode {:o}",
            meta.mode()
        ),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
    }

    // finish_materialized_rootfs products, shared with the store path.
    assert_eq!(read("etc/hostname"), b"hermetic-container.local\n");
    assert!(rootfs.join("proc").is_dir() && rootfs.join("sys").is_dir());
    assert!(rootfs.join("dev/null").is_file() && !rootfs.join("dev/null").is_symlink());
    let env = String::from_utf8(read(".hermit-oci-env")).unwrap();
    assert!(env.contains("TEST_ENV=from-image"), "{env}");
    assert_eq!(read(".hermit-oci-workdir"), b"/opt/appdir\n");
    assert_eq!(read(".hermit-oci-cmd"), b"/bin/guest\n");

    // No whiteout markers survive anywhere in the tree.
    fn walk(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".wh."),
                "whiteout marker survived at {}",
                entry.path().display()
            );
            if entry.path().is_dir() && !entry.path().is_symlink() {
                walk(&entry.path());
            }
        }
    }
    walk(rootfs);
}

/// A comparable snapshot of a rootfs tree for OCI/docker parity.
fn tree_snapshot(rootfs: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let mode = meta.mode();
            let desc = if meta.file_type().is_symlink() {
                format!("symlink->{}", std::fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                format!("dir mode={:o}", mode & 0o7777)
            } else if mode & libc::S_IFMT == libc::S_IFIFO {
                format!("fifo mode={:o}", mode & 0o7777)
            } else {
                // No mtime here: finish-created files carry the host
                // wall-time of their run. Layer mtimes are asserted
                // exactly in assert_golden_tree for both formats.
                format!(
                    "file mode={:o} sha256={}",
                    mode & 0o7777,
                    sha256_hex(&std::fs::read(&path).unwrap())
                )
            };
            out.insert(rel, desc);
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(rootfs, rootfs, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn oci_dir_materializes_runs_and_reuses_cache() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());

    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    assert_golden_tree(&rootfs);

    // A second run reuses the cached tree: a file the layers would
    // rewrite on re-materialization keeps the observer's edit instead.
    std::fs::write(rootfs.join("etc/marker.txt"), b"observer-edit\n").unwrap();
    assert_guest_ran(&run_image(&cache, &image));
    assert_eq!(
        std::fs::read(rootfs.join("etc/marker.txt")).unwrap(),
        b"observer-edit\n",
        "second run must reuse the cached tree, not re-apply layers"
    );
    assert_eq!(find_rootfs(&cache), rootfs, "same cache dir reused");
}

/// `--mount` and `--bind` compose with `--image`: a bind mount, a tmpfs,
/// and a `--bind` work directory land inside the rootfs at their guest
/// targets, a `readonly` bind refuses guest writes, and mounts Hermit
/// cannot install in a rootfs are refused by name before the guest runs.
#[test]
fn oci_dir_composes_with_mount_and_bind() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());

    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("input.txt"), b"from-bind-mount\n").unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("work.txt"), b"from-bind-flag\n").unwrap();

    // /data is not in the fixture image (created as a mountpoint),
    // /opt/appdir is (an existing directory target), /scratch is a
    // fresh tmpfs, and /work comes through --bind.
    let output = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            format!(
                "--mount=type=bind,source={},target=/data,readonly",
                data.display()
            ),
            "--mount=type=tmpfs,target=/scratch".to_string(),
            format!("--bind={}:/work", work.display()),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/data/input.txt".to_string(),
            "write:/data/input.txt:nope".to_string(),
            "write:/scratch/scratch.txt:from-tmpfs".to_string(),
            "read:/scratch/scratch.txt".to_string(),
            "read:/work/work.txt".to_string(),
            "write:/work/out.txt:guest-output".to_string(),
            "read:/proc/self/mountinfo".to_string(),
        ],
    );
    assert_guest_ran(&output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("FILE /data/input.txt=from-bind-mount"),
        "{stdout}"
    );
    assert!(
        stdout.contains("WROTE /data/input.txt=ERR30"),
        "a readonly bind refuses the guest write with EROFS: {stdout}"
    );
    assert!(
        stdout.contains("WROTE /scratch/scratch.txt=from-tmpfs"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FILE /scratch/scratch.txt=from-tmpfs"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FILE /work/work.txt=from-bind-flag"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(work.join("out.txt")).unwrap(),
        b"guest-output",
        "a writable --bind into the rootfs writes through to the host"
    );
    assert_eq!(
        std::fs::read(data.join("input.txt")).unwrap(),
        b"from-bind-mount\n",
        "the readonly bind's host source is unchanged"
    );
    // The guest's mount table shows each bind at its guest target as
    // its root, never the host source subpath (measured before the
    // rewrite: the /data row's root was the host tempdir subpath, and
    // the tmpfs row carried the host uid/gid).
    let mountinfo_row = |mountpoint: &str| {
        stdout
            .lines()
            .find(|line| line.split(' ').nth(4) == Some(mountpoint))
            .unwrap_or_else(|| panic!("no mountinfo row at {mountpoint}:\n{stdout}"))
            .to_string()
    };
    let data_row = mountinfo_row("/data");
    assert_eq!(
        data_row.split(' ').nth(3),
        Some("/data"),
        "the bind row's root is its guest target: {data_row}"
    );
    assert!(
        !stdout.contains(&data.display().to_string()),
        "the host bind source path leaked into the guest mount table:\n{stdout}"
    );
    let scratch_row = mountinfo_row("/scratch");
    assert!(
        !scratch_row.contains("uid=") && !scratch_row.contains("gid="),
        "the tmpfs row carries host ids in its superblock options: {scratch_row}"
    );

    // Determinism: the read-only composition under --verify --verify-strict
    // runs the guest twice and compares the runs; a writable bind would
    // let run one change run two's inputs, so this uses the readonly bind
    // and the (always empty at start) tmpfs only.
    let verified = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--verify".to_string(),
            "--verify-strict".to_string(),
            format!(
                "--mount=type=bind,source={},target=/data,readonly",
                data.display()
            ),
            "--mount=type=tmpfs,target=/scratch".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/data/input.txt".to_string(),
            "write:/scratch/scratch.txt:from-tmpfs".to_string(),
            "read:/scratch/scratch.txt".to_string(),
        ],
    );
    assert_eq!(
        verified.status.code(),
        Some(0),
        "verified image+mount run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&verified),
        stderr_of(&verified)
    );
    assert!(
        stdout_of(&verified).contains("FILE /data/input.txt=from-bind-mount"),
        "{}",
        stdout_of(&verified)
    );

    // Targets under /tmp, both spellings: prepared in the private /tmp
    // staging the guest's /tmp is bound from, not in the cached rootfs
    // (where the staging bind would hide the mountpoint).
    let under_tmp = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            format!(
                "--mount=type=bind,source={},target=/tmp/data,readonly",
                data.display()
            ),
            format!("--bind={}:/tmp/work", work.display()),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/tmp/data/input.txt".to_string(),
            "read:/tmp/work/work.txt".to_string(),
            "write:/tmp/work/tmp-out.txt:from-tmp-bind".to_string(),
        ],
    );
    assert_guest_ran(&under_tmp);
    let stdout = stdout_of(&under_tmp);
    assert!(
        stdout.contains("FILE /tmp/data/input.txt=from-bind-mount"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FILE /tmp/work/work.txt=from-bind-flag"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(work.join("tmp-out.txt")).unwrap(),
        b"from-tmp-bind",
        "a --bind below /tmp in an image writes through to the host"
    );

    // Refusals, by name, before any guest runs.
    for (mount, named) in [
        (
            "--mount=type=volume,target=/data",
            "type=volume is not supported",
        ),
        ("--mount=type=tmpfs,target=/proc", "inside /proc"),
        (
            "--mount=type=tmpfs,target=relative",
            "must be an absolute path",
        ),
        ("--mount=target=/data", "bind mount with no source"),
        ("--bind=/srv:/proc", "inside /proc"),
        ("--bind=/srv:/", "would replace the whole OCI rootfs"),
    ] {
        let output = run_hermit(
            &cache,
            &[
                "run".to_string(),
                format!("--image={image}"),
                mount.to_string(),
                "--".to_string(),
                "/bin/guest".to_string(),
            ],
        );
        assert_ne!(output.status.code(), Some(0), "{mount} must refuse");
        let stderr = stderr_of(&output);
        assert!(stderr.contains(named), "{mount}: {stderr}");
        assert!(
            !stdout_of(&output).contains("GUEST_MARKER"),
            "{mount}: the guest must not run"
        );
    }
}

#[test]
fn docker_archive_materializes_runs_with_oci_tree_parity() {
    let tmp = tempfile::TempDir::new().unwrap();
    // OCI side.
    let oci_cache = tmp.path().join("cache-oci");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    assert_guest_ran(&run_image(
        &oci_cache,
        &format!("oci-dir:{}", image_dir.display()),
    ));
    let oci_rootfs = find_rootfs(&oci_cache);
    assert_golden_tree(&oci_rootfs);

    // Docker-archive side over the same layers.
    let docker_cache = tmp.path().join("cache-docker");
    let archive = tmp.path().join("image.tar");
    write_valid_docker_archive(&archive);
    assert_guest_ran(&run_image(
        &docker_cache,
        &format!("docker-archive:{}", archive.display()),
    ));
    let docker_rootfs = find_rootfs(&docker_cache);
    assert_golden_tree(&docker_rootfs);

    assert_eq!(
        tree_snapshot(&oci_rootfs),
        tree_snapshot(&docker_rootfs),
        "both formats must materialize identical trees"
    );
}

#[test]
fn relative_prefixed_paths_run_oci_and_gzip_docker_archive() {
    let tmp = tempfile::TempDir::new().unwrap();
    // `./` against the OCI layout (whose layer 2 is already gzip).
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let output = run_image_in_dir(
        &tmp.path().join("cache-rel-oci"),
        tmp.path(),
        "oci-dir:./oci-image",
    );
    assert_guest_ran(&output);
    assert_golden_tree(&find_rootfs(&tmp.path().join("cache-rel-oci")));
    // `../` against a docker-archive whose layer tars are gzip.
    let archive = tmp.path().join("image-gzip.tar");
    write_valid_docker_archive_gzip(&archive);
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let output = run_image_in_dir(
        &tmp.path().join("cache-rel-docker"),
        &work,
        "docker-archive:../image-gzip.tar",
    );
    assert_guest_ran(&output);
    assert_golden_tree(&find_rootfs(&tmp.path().join("cache-rel-docker")));
}

/// H1 end to end: a root-level `.wh...` whiteout targets `..`; the run is
/// refused by name and nothing outside the (never-published) rootfs is
/// removed.
#[test]
fn whiteout_targeting_parent_is_refused_end_to_end() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sentinel = tmp.path().join("host-sentinel.txt");
    std::fs::write(&sentinel, b"host").unwrap();
    let (layer1, layer2) = fixture_layers(
        &[entry(".wh...", tar::EntryType::Regular, 0o644, b"")],
        None,
    );
    let dir = tmp.path().join("whiteout-escape");
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        &dir,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: layer2,
                media_type: MT_TAR,
            },
        ],
    );
    let output = run_image(
        &tmp.path().join("cache-whiteout"),
        &format!("oci-dir:{}", dir.display()),
    );
    assert_refused_by_name(&output, "whiteout escape", &["refusing whiteout"]);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"host");
}

/// H3 end to end: a top layer that replaces `etc` with a symlink to a
/// host directory is refused during finishing, and no hostname lands in
/// the host directory.
#[test]
fn symlinked_etc_is_refused_end_to_end_without_touching_host() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tmp.path().join("outside-etc");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"host").unwrap();
    let (layer1, _) = fixture_layers(&[], None);
    let mut etc_link = entry("etc", tar::EntryType::Symlink, 0o777, b"");
    etc_link.link = Some(outside.to_string_lossy().into_owned());
    let layer2 = build_tar(&[etc_link]);
    let dir = tmp.path().join("symlink-etc");
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        &dir,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: layer2,
                media_type: MT_TAR,
            },
        ],
    );
    let output = run_image(
        &tmp.path().join("cache-symlink-etc"),
        &format!("oci-dir:{}", dir.display()),
    );
    assert_refused_by_name(&output, "symlinked etc", &["symlink"]);
    assert!(!outside.join("hostname").exists());
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"host");
}

/// M2: a partial tree left at the final cache location (marker gone, a
/// layer directory locked read-only) does not block the next run: the
/// retry publishes a fresh tree atomically.
#[test]
fn local_materialization_recovers_from_partial_tree() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());
    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    let cache_entry = rootfs.parent().unwrap().to_path_buf();
    // Simulate an interrupted/legacy partial tree at the final location.
    std::fs::remove_file(cache_entry.join(".hermit-oci-ready")).unwrap();
    std::fs::remove_file(rootfs.join("etc/marker.txt")).unwrap();
    std::fs::set_permissions(rootfs.join("etc"), std::fs::Permissions::from_mode(0o555)).unwrap();
    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    assert_golden_tree(&rootfs);
}

fn assert_refused_by_name(output: &Output, case: &str, needles: &[&str]) {
    assert_eq!(
        output.status.code(),
        Some(hermit::HERMIT_INTERNAL_FAILURE_EXIT),
        "{case}: expected exit 125, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout_of(output),
        stderr_of(output)
    );
    let stderr = stderr_of(output);
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{case}: stderr must name {needle:?}\nstderr:\n{stderr}"
        );
    }
    assert!(
        !stdout_of(output).contains("GUEST_MARKER"),
        "{case}: the guest must not have run\nstdout:\n{}",
        stdout_of(output)
    );
}

#[test]
fn malformed_local_images_are_refused_by_name() {
    struct Case {
        name: &'static str,
        image: String,
        needles: &'static [&'static str],
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let mut cases: Vec<Case> = Vec::new();
    let mut add = |name: &'static str, image: String, needles: &'static [&'static str]| {
        cases.push(Case {
            name,
            image,
            needles,
        });
    };

    // Missing oci-dir path.
    add(
        "missing oci-dir",
        format!("oci-dir:{}/nope", tmp.path().display()),
        &["does not exist", "oci-dir"],
    );
    // Missing docker-archive path.
    add(
        "missing docker-archive",
        format!("docker-archive:{}/nope.tar", tmp.path().display()),
        &["does not exist", "podman save"],
    );
    // No index.json.
    let no_index = tmp.path().join("no-index");
    write_valid_oci_dir(&no_index);
    std::fs::remove_file(no_index.join("index.json")).unwrap();
    add(
        "no index.json",
        format!("oci-dir:{}", no_index.display()),
        &["no index.json"],
    );
    // Zero linux/amd64 manifests.
    let zero_amd64 = tmp.path().join("zero-amd64");
    write_valid_oci_dir(&zero_amd64);
    let index = std::fs::read_to_string(zero_amd64.join("index.json")).unwrap();
    std::fs::write(
        zero_amd64.join("index.json"),
        index.replace("\"amd64\"", "\"arm64\""),
    )
    .unwrap();
    add(
        "zero amd64",
        format!("oci-dir:{}", zero_amd64.display()),
        &["no linux/amd64 manifest"],
    );
    // Two linux/amd64 manifests.
    let two_amd64 = tmp.path().join("two-amd64");
    write_valid_oci_dir(&two_amd64);
    let mut index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(two_amd64.join("index.json")).unwrap()).unwrap();
    let descriptor = index["manifests"][0].clone();
    index["manifests"].as_array_mut().unwrap().push(descriptor);
    std::fs::write(
        two_amd64.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    add(
        "two amd64",
        format!("oci-dir:{}", two_amd64.display()),
        &["2 linux/amd64 manifests", "ambiguous"],
    );
    // Missing layer blob.
    let missing_blob = tmp.path().join("missing-blob");
    let (layer1, _) = fixture_layers(&[], None);
    write_valid_oci_dir(&missing_blob);
    std::fs::remove_file(blob_path(&missing_blob, &sha256_hex(&layer1))).unwrap();
    add(
        "missing blob",
        format!("oci-dir:{}", missing_blob.display()),
        &["missing blob"],
    );
    // Corrupted manifest blob (digest mismatch).
    let bad_manifest = tmp.path().join("bad-manifest");
    let (l1, l2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    let manifest_hex = write_oci_dir(
        &bad_manifest,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: l2.clone(),
                media_type: MT_TAR,
            },
        ],
    );
    std::fs::write(blob_path(&bad_manifest, &manifest_hex), b"{}").unwrap();
    add(
        "manifest digest mismatch",
        format!("oci-dir:{}", bad_manifest.display()),
        &["manifest digest mismatch"],
    );
    // diff_id mismatch.
    let bad_diff = tmp.path().join("bad-diff");
    let config = config_bytes(&[sha256_hex(b"bogus"), sha256_hex(&l2)]);
    write_oci_dir(
        &bad_diff,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: l2.clone(),
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "diff_id mismatch",
        format!("oci-dir:{}", bad_diff.display()),
        &["diff_id mismatch"],
    );
    // zstd layer.
    let zstd_dir = tmp.path().join("zstd");
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    write_oci_dir(
        &zstd_dir,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0],
                media_type: MT_TAR_ZSTD,
            },
        ],
    );
    add(
        "zstd layer",
        format!("oci-dir:{}", zstd_dir.display()),
        &["zstd", "not supported"],
    );
    // Device entry outside dev/.
    let (dl1, dl2) = fixture_layers(&[entry("usr/tty9", tar::EntryType::Char, 0o600, b"")], None);
    let device_dir = tmp.path().join("device");
    let config = config_bytes(&[sha256_hex(&dl1), sha256_hex(&dl2)]);
    write_oci_dir(
        &device_dir,
        &config,
        &[
            StoredLayer {
                blob: dl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: dl2,
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "device outside dev",
        format!("oci-dir:{}", device_dir.display()),
        &["device entry", "outside /dev"],
    );
    // ../ traversal and absolute path (patched into a valid tar).
    const TRAVERSAL_NEEDLES: &[&str] = &["path traversal"];
    const ABSOLUTE_NEEDLES: &[&str] = &["absolute path"];
    for (name, placeholder, target, needles) in [
        ("traversal", "zz/evil", "../evil", TRAVERSAL_NEEDLES),
        ("absolute", "zabs/evil", "/abs/evil", ABSOLUTE_NEEDLES),
    ] {
        let (pl1, pl2) = fixture_layers(
            &[entry(placeholder, tar::EntryType::Regular, 0o644, b"x")],
            Some((placeholder, target)),
        );
        let dir = tmp.path().join(name);
        let config = config_bytes(&[sha256_hex(&pl1), sha256_hex(&pl2)]);
        write_oci_dir(
            &dir,
            &config,
            &[
                StoredLayer {
                    blob: pl1,
                    media_type: MT_TAR,
                },
                StoredLayer {
                    blob: pl2,
                    media_type: MT_TAR,
                },
            ],
        );
        // Leak the needles slice of string literals is fine: they are 'static.
        add(
            match name {
                "traversal" => "path traversal",
                _ => "absolute path",
            },
            format!("oci-dir:{}", dir.display()),
            needles,
        );
    }
    // Escaping hardlink.
    let mut escape = entry("escape", tar::EntryType::Link, 0o644, b"");
    escape.link = Some("/etc/passwd".to_string());
    let (hl1, hl2) = fixture_layers(&[escape], None);
    let hardlink_dir = tmp.path().join("hardlink");
    let config = config_bytes(&[sha256_hex(&hl1), sha256_hex(&hl2)]);
    write_oci_dir(
        &hardlink_dir,
        &config,
        &[
            StoredLayer {
                blob: hl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: hl2,
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "escaping hardlink",
        format!("oci-dir:{}", hardlink_dir.display()),
        &["escaping hardlink"],
    );
    // Corrupt gzip layer.
    let corrupt_dir = tmp.path().join("corrupt-gzip");
    let (cl1, cl2) = fixture_layers(&[], None);
    let mut corrupt = gzip(&cl2);
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    corrupt[mid + 1] ^= 0xff;
    corrupt.truncate(corrupt.len() - 4);
    let config = config_bytes(&[sha256_hex(&cl1), sha256_hex(&cl2)]);
    write_oci_dir(
        &corrupt_dir,
        &config,
        &[
            StoredLayer {
                blob: cl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: corrupt,
                media_type: MT_TAR_GZIP,
            },
        ],
    );
    add(
        "corrupt gzip",
        format!("oci-dir:{}", corrupt_dir.display()),
        &["corrupt gzip"],
    );
    // docker-archive without manifest.json.
    let no_manifest = tmp.path().join("no-manifest.tar");
    let (nl1, _) = fixture_layers(&[], None);
    write_outer_tar(&no_manifest, &[outer_file("layer1/layer.tar", &nl1)]);
    add(
        "no manifest.json",
        format!("docker-archive:{}", no_manifest.display()),
        &["no manifest.json"],
    );
    // docker-archive with two images.
    let two_images = tmp.path().join("two-images.tar");
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    let one = serde_json::json!({
        "Config": "config.json",
        "RepoTags": ["a:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    });
    let manifest = serde_json::to_vec(&serde_json::json!([one.clone(), one])).unwrap();
    write_outer_tar(
        &two_images,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &l1),
            outer_file("layer2/layer.tar", &l2),
            outer_file("manifest.json", &manifest),
        ],
    );
    add(
        "two images",
        format!("docker-archive:{}", two_images.display()),
        &["contains 2 images"],
    );
    // Truncated docker-archive.
    let truncated = tmp.path().join("truncated.tar");
    write_valid_docker_archive(&truncated);
    let bytes = std::fs::read(&truncated).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    add(
        "truncated archive",
        format!("docker-archive:{}", truncated.display()),
        &["truncated or corrupt"],
    );
    // A plain rootfs tar is not a docker-archive.
    let plain = tmp.path().join("plain-rootfs.tar");
    let (rl1, _) = fixture_layers(&[], None);
    std::fs::write(&plain, &rl1).unwrap();
    add(
        "plain rootfs tar",
        format!("docker-archive:{}", plain.display()),
        &["no manifest.json", "plain rootfs tar"],
    );
    // Bare-relative prefixed path.
    add(
        "bare relative",
        "oci-dir:some-image".to_string(),
        &["bare relative path", "./some-image"],
    );

    for (i, case) in cases.iter().enumerate() {
        let cache = tmp.path().join(format!("cache-{i}"));
        let output = run_image(&cache, &case.image);
        assert_refused_by_name(&output, case.name, case.needles);
    }
}

/// A scripted `podman` that records argv and serves one store image, so the
/// store path is regression-tested without a real podman or network.
struct FakePodman {
    bin_dir: PathBuf,
    log: PathBuf,
    graph_root: PathBuf,
    layer: PathBuf,
}

fn write_fake_podman(tmp: &Path) -> FakePodman {
    let bin_dir = tmp.join("fakebin");
    let layer = tmp.join("store-layer");
    std::fs::create_dir_all(bin_dir.clone()).unwrap();
    std::fs::create_dir_all(layer.join("bin")).unwrap();
    // The image's working directory, as the OCI fixture's layer has it.
    std::fs::create_dir_all(layer.join("opt/appdir")).unwrap();
    let guest = layer.join("bin/guest");
    std::fs::copy(guest_binary(), &guest).unwrap();
    std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = bin_dir.join("podman");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_PODMAN_LOG"
case "$1" in
  info)
    printf '%s\n' "$FAKE_PODMAN_GRAPHROOT"
    ;;
  pull)
    printf '%s\n' "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ;;
  unshare)
    shift
    exec "$@"
    ;;
  image)
    case "$*" in
      *RepoDigests*)
        printf '%s\n' '["docker.io/library/store-test@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"]'
        ;;
      *Digest*)
        printf 'sha256:%s\n' "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ;;
      *".Config"*)
        printf '%s' "$FAKE_PODMAN_CONFIG"
        ;;
      *GraphDriver*)
        printf '{"Name":"overlay","Data":{"UpperDir":"%s"}}' "$FAKE_PODMAN_LAYER"
        ;;
      *)
        echo "unexpected podman args: $*" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "unexpected podman args: $*" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakePodman {
        bin_dir,
        log: tmp.join("podman.log"),
        graph_root: tmp.join("graphroot"),
        layer,
    }
}

impl FakePodman {
    fn command(&self, cache: &Path, args: &[String]) -> Output {
        let fake_path = format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(hermit_test::hermit_binary());
        command
            .args(args)
            .env("XDG_CACHE_HOME", cache)
            .env("PATH", fake_path)
            .env("FAKE_PODMAN_LOG", &self.log)
            .env("FAKE_PODMAN_GRAPHROOT", &self.graph_root)
            .env("FAKE_PODMAN_LAYER", &self.layer)
            .env(
                "FAKE_PODMAN_CONFIG",
                r#"{"Env":["TEST_ENV=from-image","STORE_ENV=from-store","PATH=/bin"],"WorkingDir":"/opt/appdir","Cmd":["/bin/guest"],"Entrypoint":null}"#,
            )
            .stdin(std::process::Stdio::null());
        hermit_test::hermit_output("hermit fake-podman run", &mut command)
    }

    fn recorded(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn store_reference_still_materializes_via_podman_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "run".to_string(),
            "--image=store-test:latest".to_string(),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "store run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("GUEST_MARKER=hello-from-image"), "{stdout}");
    assert!(stdout.contains("STORE_ENV=from-store"), "{stdout}");
    let recorded = fake.recorded();
    for needle in [
        "info --format",
        "pull --policy=never store-test:latest",
        "image inspect",
        "unshare cp -a",
    ] {
        assert!(
            recorded.contains(needle),
            "fake podman must record {needle:?}\nrecorded:\n{recorded}"
        );
    }
}

fn find_manifest(cache: &Path) -> serde_json::Value {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            // Do not follow symlinks: a materialized rootfs contains
            // dev/fd -> /proc/self/fd, so following it recurses into the
            // walker's own fd directory forever (mirrors the guard in
            // tree_snapshot above).
            if path.is_dir() && !path.is_symlink() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|name| name == "manifest.json") {
                out.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&cache.join("hermit"), &mut found);
    assert_eq!(
        found.len(),
        1,
        "manifests under {}: {found:?}",
        cache.display()
    );
    serde_json::from_slice(&std::fs::read(&found[0]).unwrap()).unwrap()
}

#[test]
fn find_manifest_does_not_follow_symlinked_directories() {
    // A materialized rootfs links dev/fd at /proc/self/fd; a walk that
    // follows directory symlinks recurses into its own fd table forever.
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let recording = cache.join("hermit").join("recording");
    std::fs::create_dir_all(&recording).unwrap();
    std::fs::write(recording.join("manifest.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(cache.join("hermit"), recording.join("loop")).unwrap();
    let manifest = find_manifest(&cache);
    assert!(manifest.is_object(), "{manifest}");
}

#[test]
fn image_runs_record_resolved_identity_in_v2_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Store path: the supplied tag is preserved while IDs come from the
    // store resolution that also selected the layer to materialize.
    let fake = write_fake_podman(tmp.path());
    let store_cache = tmp.path().join("cache-store");
    let output = fake.command(
        &store_cache,
        &[
            "run".to_string(),
            "--image=store-test:latest".to_string(),
            "--strict".to_string(),
            "--manifest=always".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_guest_ran(&output);
    let manifest = find_manifest(&store_cache);
    assert_eq!(manifest["schema"], "hermit-recording-manifest/v2");
    assert_eq!(manifest["image"]["source"], "podman-store");
    assert_eq!(
        manifest["image"]["reference"], "store-test:latest",
        "supplied reference must survive canonicalization: {manifest}"
    );
    assert_eq!(manifest["image"]["identity"], "a".repeat(64));
    assert_eq!(manifest["image"]["image_id"], "a".repeat(64));
    assert_eq!(
        manifest["image"]["manifest_digest"],
        format!("sha256:{}", "b".repeat(64))
    );

    // Local OCI path: the manifest identity is the verified OCI manifest
    // digest named by the fixture index, with no host path in `reference`.
    let oci_cache = tmp.path().join("cache-oci");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(image_dir.join("index.json")).unwrap()).unwrap();
    let expected_digest = index["manifests"][0]["digest"].as_str().unwrap();
    let output = run_hermit(
        &oci_cache,
        &[
            "run".to_string(),
            format!("--image=oci-dir:{}", image_dir.display()),
            "--strict".to_string(),
            "--manifest=always".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_guest_ran(&output);
    let manifest = find_manifest(&oci_cache);
    assert_eq!(manifest["schema"], "hermit-recording-manifest/v2");
    assert_eq!(manifest["image"]["source"], "oci-dir");
    assert!(manifest["image"].get("reference").is_none(), "{manifest}");
    assert_eq!(manifest["image"]["manifest_digest"], expected_digest);
    assert_eq!(
        manifest["image"]["identity"],
        expected_digest.strip_prefix("sha256:").unwrap()
    );

    // Docker archive: identity is the verified config digest.
    let docker_cache = tmp.path().join("cache-docker");
    let archive = tmp.path().join("image.tar");
    write_valid_docker_archive(&archive);
    let output = run_hermit(
        &docker_cache,
        &[
            "run".to_string(),
            format!("--image=docker-archive:{}", archive.display()),
            "--strict".to_string(),
            "--manifest=always".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_guest_ran(&output);
    let manifest = find_manifest(&docker_cache);
    assert_eq!(manifest["image"]["source"], "docker-archive");
    assert!(manifest["image"].get("reference").is_none(), "{manifest}");
    assert!(
        manifest["image"].get("manifest_digest").is_none(),
        "{manifest}"
    );
    assert_eq!(manifest["image"]["identity"], manifest["image"]["image_id"]);
}

#[test]
fn prefixed_runs_never_invoke_podman() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "run".to_string(),
            format!("--image=oci-dir:{}", image_dir.display()),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_guest_ran(&output);
    assert!(
        !fake.log.exists(),
        "prefixed runs must never invoke podman; recorded:\n{}",
        fake.recorded()
    );
}

#[test]
fn oci_run_refuses_prefixed_images_with_hermit_run_remedy() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "oci".to_string(),
            "run".to_string(),
            "oci-dir:/definitely/not/used".to_string(),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_refused_by_name(
        &output,
        "oci run prefixed",
        &[
            "hermit oci run",
            "hermit run --image=oci-dir:/definitely/not/used",
        ],
    );
    assert!(
        !fake.log.exists(),
        "the prefixed refusal must precede any podman access; recorded:\n{}",
        fake.recorded()
    );
}

#[test]
fn run_help_documents_local_image_forms_without_buildah() {
    let output = Command::new(hermit_test::hermit_binary())
        .args(["run", "--help"])
        .output()
        .expect("run hermit for help");
    assert_eq!(output.status.code(), Some(0));
    let help = stdout_of(&output);
    assert!(help.contains("oci-dir:"), "{help}");
    assert!(help.contains("docker-archive:"), "{help}");
    assert!(help.contains("flattened to the invoking user"), "{help}");
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("no manifest blob digest"), "{help}");
    assert!(flat.contains("config hash and layer diff_ids"), "{help}");
    assert!(
        flat.contains("unspecified os/architecture is accepted"),
        "{help}"
    );
    assert!(flat.contains("subsecond mtimes"), "{help}");
    assert!(
        !help.contains("buildah"),
        "help must not claim buildah materialization:\n{help}"
    );
    assert!(
        !help.contains("PROTOTYPE"),
        "image mode is supported, not a prototype:\n{help}"
    );
    assert!(
        flat.contains("compose with it"),
        "help must state that --mount/--bind compose with --image:\n{help}"
    );
}
