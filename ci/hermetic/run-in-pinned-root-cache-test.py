#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Exercise the actual wrapper's cache mounts without starting a container."""

import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


WRAPPER = Path(__file__).with_name("run-in-pinned-root.sh")


class CargoCacheMounts(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="hermit-pinned-root-cache-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        for directory in (
            "source/agent-utils/rs/target",
            "source/agent-utils/rs/.agent-utils-locks",
            "source/agent-utils/rs/.agent-utils-snapshots",
            "tools",
            "cargo/bin",
            "cargo/registry",
            "cargo/git",
        ):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        (self.root / "cargo/config.toml").write_text("host configuration must not be imported\n")
        (self.root / "cargo/bin/cargo-clippy").write_text("host executable must not be imported\n")
        self.capture = self.root / "podman.jsonl"
        fake = self.root / "tools/podman"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "with open(os.environ['PINNED_ROOT_CAPTURE'], 'a') as out:\n"
            "    out.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if sys.argv[1] == 'run' and os.environ.get('PINNED_ROOT_WRITE_AGENT_UTILS_STATE') == '1':\n"
            "    mounts = [sys.argv[i + 1] for i, arg in enumerate(sys.argv) if arg == '--mount']\n"
            "    by_destination = {dict(field.split('=', 1) for field in mount.split(','))['destination']: dict(field.split('=', 1) for field in mount.split(','))['source'] for mount in mounts}\n"
            "    for destination in ['/src/agent-utils/rs/target', '/src/agent-utils/rs/.agent-utils-locks', '/src/agent-utils/rs/.agent-utils-snapshots']:\n"
            "        pathlib.Path(by_destination[destination], 'container-write').write_text(destination + '\\n')\n"
            "if sys.argv[1:3] == ['image', 'inspect']: print('c' * 64); sys.exit(0)\n"
            "sys.exit(0 if sys.argv[1:3] == ['image', 'exists'] or sys.argv[1] == 'run' else 90)\n"
        )
        fake.chmod(0o755)

    def invoke(self, cargo_home="cargo", run_state=None, source="source", output="output", proc_locks_runtime=None, calibration=False):
        env = os.environ.copy()
        env["PATH"] = str(self.root / "tools") + os.pathsep + env["PATH"]
        env["PINNED_ROOT_CAPTURE"] = str(self.capture)
        forwarded = ["--nextest-calibration"] if calibration else []
        if proc_locks_runtime is not None:
            env["XDG_RUNTIME_DIR"] = str(proc_locks_runtime)
            forwarded.append("--proc-locks-runtime")
        if run_state is not None:
            env["VALIDATE_RUN_STATE"] = str(run_state)
            forwarded += ["--env", "VALIDATE_RUN_STATE"]
        result = subprocess.run(
            [
                "bash", str(WRAPPER), "--src", str(source), "--out", output,
                "--cargo-home", cargo_home, "--digest", "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                *forwarded,
                "--", "/not-executed/command", "literal argument",
            ],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=10,
        )
        calls = [json.loads(line) for line in self.capture.read_text().splitlines()]
        return result, calls

    def prepare_capture_fixture(self, probe_status=0, resolve_status=0):
        source = self.root / "source"
        published = source / "target/ci/rust-scripts"
        published.mkdir(parents=True, exist_ok=True)
        (published / "manifest.tsv").write_text("fixture producer manifest\n")
        runner = source / "ci/rust-script-bin/rust-script"
        runner.parent.mkdir(parents=True, exist_ok=True)
        runner.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "manifest = pathlib.Path(os.environ.get('HERMIT_RUST_SCRIPT_ARTIFACT_ROOT', '.'), 'manifest.tsv')\n"
            "if sys.argv[1] == '--resolve-optional' and (not manifest.is_file() or manifest.is_symlink()): sys.exit(2)\n"
            f"if sys.argv[1] == '--resolve-optional': print(pathlib.Path(__file__).resolve()) if {resolve_status} == 0 else None; sys.exit({resolve_status})\n"
            f"if sys.argv[-1] == '--probe': print('nextest-launch-observation-v2'); sys.exit({probe_status})\n"
            "assert sys.argv[1:3] == ['capture-launch', '--pinned-image']\n"
            "assert sys.argv[4:6] == ['--image-id', 'c' * 64]\n"
            "assert sys.argv[6] == '--output'\n"
            "path = pathlib.Path(sys.argv[7])\n"
            "fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o400)\n"
            "with os.fdopen(fd, 'w') as out: json.dump({'fixture_only': True, 'image': sys.argv[3]}, out)\n"
        )
        runner.chmod(0o755)

    def test_calibration_opt_in_uses_unique_read_only_proofs_and_exact_inspected_image(self):
        self.prepare_capture_fixture()
        retained = []
        for _ in range(2):
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 3)
            self.assertEqual(calls[1], ["image", "inspect", "--format", "{{.Id}}", calls[0][-1]])
            self.assertEqual(calls[2][-3], calls[0][-1])
            mounts = [calls[2][i + 1] for i, value in enumerate(calls[2]) if value == "--mount"]
            proof_mounts = [m for m in mounts if "destination=/run/hermit-nextest-launch.json" in m]
            self.assertEqual(len(proof_mounts), 1)
            fields = dict(part.split("=", 1) for part in proof_mounts[0].split(","))
            self.assertEqual(fields["ro"], "true")
            path = Path(fields["source"])
            self.assertTrue(path.is_relative_to(self.root / "output"))
            self.assertEqual(path.stat().st_mode & 0o777, 0o400)
            self.assertEqual(json.loads(path.read_text())["image"], calls[0][-1])
            retained.append((path, path.read_bytes()))
        self.assertNotEqual(retained[0][0], retained[1][0])
        self.assertEqual(retained[0][0].read_bytes(), retained[0][1])

    def test_bootstrap_and_unavailable_capture_do_not_compile_or_claim_a_proof(self):
        # Bootstrap remains independent of a producer which may itself use this wrapper.
        for installed, status, opt_in, expected in [(False, 0, True, 0), (True, 23, False, 0), (True, 2, True, 0), (True, 126, True, 126), (True, 127, True, 127)]:
            with self.subTest(installed=installed, status=status, opt_in=opt_in):
                if installed:
                    self.prepare_capture_fixture(probe_status=status)
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(calibration=opt_in)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(len(calls), 2 if expected == 0 else 1)
                self.assertFalse(any("hermit-nextest-launch.json" in arg for call in calls for arg in call))

    def test_malformed_preparation_is_distinct_from_missing_optional_capture(self):
        for status in [2, 3]:
            self.prepare_capture_fixture(resolve_status=status)
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 2 if status == 2 else 0, result.stderr)
            self.assertEqual(len(calls), 1 if status == 2 else 2)
            self.assertFalse(any("hermit-nextest-launch.json" in arg for call in calls for arg in call))

    def test_existing_invalid_manifest_is_not_optional_absence(self):
        self.prepare_capture_fixture()
        manifest = self.root / "source/target/ci/rust-scripts/manifest.tsv"
        manifest.unlink()
        for kind in ["directory", "dangling-symlink"]:
            if kind == "directory":
                manifest.mkdir()
            else:
                manifest.symlink_to(manifest.with_name("absent"))
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual(len(calls), 1)
            if kind == "directory":
                manifest.rmdir()
            else:
                manifest.unlink()

    def test_imports_only_registry_and_git_from_the_host_cargo_home(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], ["image", "exists", "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"])
        argv = calls[1]
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        imports = [mount for mount in mounts if f"source={self.root}/cargo" in mount]
        self.assertEqual(
            imports,
            [
                f"type=bind,source={self.root}/cargo/registry,destination=/build/.cargo/registry",
                f"type=bind,source={self.root}/cargo/git,destination=/build/.cargo/git",
            ],
        )
        self.assertIn(f"type=bind,source={self.root}/source,destination=/src,ro=true", mounts)
        self.assertIn(f"type=bind,source={self.root}/output/target,destination=/src/target", mounts)
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/target,destination=/src/agent-utils/rs/target",
            mounts,
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/locks,destination=/src/agent-utils/rs/.agent-utils-locks",
            mounts,
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/snapshots,destination=/src/agent-utils/rs/.agent-utils-snapshots",
            mounts,
        )
        self.assertIn("CARGO_HOME=/build/.cargo", argv)
        self.assertEqual(argv.count("--cgroups=disabled"), 1)
        self.assertFalse(any(arg.startswith(("--cgroup-parent", "--cgroupns")) for arg in argv))
        self.assertIn("--network=none", argv)
        self.assertIn("--http-proxy=false", argv)
        self.assertEqual(argv[-3:], ["fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000", "/not-executed/command", "literal argument"])

    def test_agent_utils_writable_state_is_confined_to_the_pinned_output(self):
        host_state = self.root / "source/agent-utils/rs"
        sentinels = {}
        for relative in ("target", ".agent-utils-locks", ".agent-utils-snapshots"):
            directory = host_state / relative
            sentinel = directory / "host-sentinel"
            sentinel.write_bytes((relative + "\n").encode())
            sentinels[relative] = (
                sentinel.read_bytes(),
                sentinel.stat().st_mtime_ns,
                directory.stat().st_mtime_ns,
            )

        os.environ["PINNED_ROOT_WRITE_AGENT_UTILS_STATE"] = "1"
        self.addCleanup(os.environ.pop, "PINNED_ROOT_WRITE_AGENT_UTILS_STATE", None)
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)

        for relative, before in sentinels.items():
            directory = host_state / relative
            sentinel = directory / "host-sentinel"
            self.assertEqual(
                (sentinel.read_bytes(), sentinel.stat().st_mtime_ns, directory.stat().st_mtime_ns),
                before,
                f"host agent-utils {relative} state changed",
            )
            self.assertFalse((directory / "container-write").exists())

        output_state = self.root / "output/agent-utils-rs"
        expected = {
            "target": "/src/agent-utils/rs/target\n",
            "locks": "/src/agent-utils/rs/.agent-utils-locks\n",
            "snapshots": "/src/agent-utils/rs/.agent-utils-snapshots\n",
        }
        for relative, contents in expected.items():
            marker = output_state / relative / "container-write"
            self.assertEqual(marker.read_text(), contents)
            self.assertTrue(marker.is_relative_to(self.root / "output"))

    def test_proc_locks_mount_reuses_the_native_host_inode(self):
        runtime = self.root / "runtime with spaces"
        runtime.mkdir(mode=0o700)
        lease = runtime / "hermit-proc-locks-determinism.lock"
        lease.write_bytes(b"preserve an existing host lease")
        lease.chmod(0o600)
        before = lease.stat()
        for output in ["first-container-output", "second-container-output"]:
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(output=output, proc_locks_runtime=runtime)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 2)
            argv = calls[1]
            self.assertIn("/run/hermit-proc-locks:rw,nosuid,nodev,noexec,mode=0700", argv)
            self.assertIn(f"type=bind,source={lease},destination=/run/hermit-proc-locks/hermit-proc-locks-determinism.lock", argv)
            self.assertIn("XDG_RUNTIME_DIR=/run/hermit-proc-locks", argv)
            self.assertIn(f"HERMIT_PROC_LOCKS_LEASE_ID={before.st_dev}:{before.st_ino}", argv)
            self.assertFalse(any(f"source={runtime}," in arg for arg in argv))
            self.assertEqual(lease.read_bytes(), b"preserve an existing host lease")
            after = lease.stat()
            self.assertEqual((after.st_dev, after.st_ino, after.st_uid, after.st_mode, after.st_mtime_ns),
                             (before.st_dev, before.st_ino, before.st_uid, before.st_mode, before.st_mtime_ns))
        # The real wrapper and host file opens executed above; Podman was a
        # recorder. This is not a container UID-mapping or flock experiment.

    def test_proc_locks_mount_refuses_unsafe_inputs_before_podman_run(self):
        for case in ["missing", "public-directory", "directory-symlink", "public-file", "fifo", "file-symlink", "mount-delimiter"]:
            with self.subTest(case=case):
                runtime = self.root / case
                runtime.mkdir(mode=0o700)
                lease = runtime / "hermit-proc-locks-determinism.lock"
                if case == "missing":
                    runtime.rmdir()
                elif case == "public-directory":
                    runtime.chmod(0o755)
                elif case == "directory-symlink":
                    target = self.root / "real-private-directory"
                    runtime.rename(target)
                    runtime.symlink_to(target, target_is_directory=True)
                elif case == "public-file":
                    lease.write_bytes(b"preserve unsafe bytes")
                    lease.chmod(0o666)
                elif case == "fifo":
                    os.mkfifo(lease, 0o600)
                elif case == "file-symlink":
                    target = runtime / "other-file"
                    target.write_bytes(b"preserve symlink target")
                    target.chmod(0o600)
                    lease.symlink_to(target)
                elif case == "mount-delimiter":
                    other = self.root / "invalid,mount"
                    runtime.rename(other)
                    runtime = other
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(proc_locks_runtime=runtime)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [["image", "exists", "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"]])

    def test_normalized_output_with_spaces_keeps_the_exact_private_home(self):
        result, calls = self.invoke(output="unused directory/../output with spaces")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        output = self.root / "output with spaces"
        private_home = output / "home"
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertIn(f"type=bind,source={output / 'target'},destination=/src/target", mounts)
        self.assertIn(f"type=bind,source={private_home},destination=/build", mounts)
        for cache in ("registry", "git"):
            self.assertTrue((private_home / ".cargo" / cache).is_dir(), cache)
            self.assertIn(
                f"type=bind,source={self.root / 'cargo' / cache},destination=/build/.cargo/{cache}",
                mounts,
            )
        self.assertFalse((self.root / "unused directory").exists())
        self.assertIn("HOME=/build", argv)
        self.assertIn("CARGO_HOME=/build/.cargo", argv)
        self.assertIn("--network=none", argv)
        self.assertIn("--http-proxy=false", argv)
        self.assertEqual(argv[-3:], ["fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000", "/not-executed/command", "literal argument"])

    def test_absent_dependency_cache_is_not_replaced_by_a_whole_home_mount(self):
        (self.root / "cargo/registry").rmdir()
        (self.root / "cargo/git").rmdir()
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertFalse(any(f"source={self.root}/cargo" in mount for mount in mounts))
        self.assertIn("CARGO_HOME=/build/.cargo", argv)

    def test_missing_cargo_home_refuses_before_container_launch(self):
        result, calls = self.invoke("missing")
        self.assertEqual(result.returncode, 2)
        self.assertIn("is not a directory", result.stderr)
        self.assertEqual(calls, [["image", "exists", "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"]])

    def test_run_state_uses_the_same_host_directory_for_each_pinned_command(self):
        run_state = self.root / "run state"
        run_state.mkdir()
        (run_state / "fixture").write_bytes(b"existing fixture")
        result, calls = self.invoke(run_state=run_state)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertIn(f"type=bind,source={run_state},destination=/validate-run-state", argv)
        self.assertIn("VALIDATE_RUN_STATE=/validate-run-state", argv)
        self.assertEqual((run_state / "fixture").read_bytes(), b"existing fixture")
        self.assertEqual(argv[-2:], ["/not-executed/command", "literal argument"])

    def test_relative_run_state_refuses_before_container_launch(self):
        result, calls = self.invoke(run_state="relative-state")
        self.assertEqual(result.returncode, 2)
        self.assertIn("VALIDATE_RUN_STATE must be absolute", result.stderr)
        self.assertEqual(calls, [["image", "exists", "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"]])

    def test_relocates_real_nested_submodule_configs_without_changing_host_metadata(self):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = os.environ.copy()
        git_env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       GIT_OPTIONAL_LOCKS="0")

        def git(root, *args):
            result = subprocess.run(
                [git_bin, "-c", "protocol.file.allow=always", "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", "-C", str(root), *args],
                env=git_env, capture_output=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            return result.stdout

        def seed(name):
            root = self.root / name
            root.mkdir()
            git(root, "init", "-q")
            (root / "payload").write_text(name + "\n")
            git(root, "add", "payload")
            git(root, "commit", "-qm", "fixture")
            return root

        leaf = seed("leaf-seed")
        child = seed("child-seed")
        git(child, "submodule", "add", "-q", str(leaf), "nested child")
        git(child, "commit", "-qam", "nested fixture")
        superproject = seed("super-seed")
        git(superproject, "submodule", "add", "-q", str(child), "third-party/fixture")
        git(superproject, "commit", "-qam", "submodule fixture")

        for separate_metadata in (False, True):
            with self.subTest(separate_metadata=separate_metadata):
                source = self.root / ("separate-source" if separate_metadata else "plain-source")
                options = (["--separate-git-dir", str(self.root / "super-metadata")]
                           if separate_metadata else [])
                git(self.root, "clone", "-q", *options, str(superproject), str(source))
                git(source, "submodule", "update", "--init", "--recursive")
                paths = ["third-party/fixture", "third-party/fixture/nested child"]
                metadata = {}
                for path in paths:
                    directory = Path(git(source / path, "rev-parse", "--absolute-git-dir").decode().strip())
                    metadata[path] = directory
                nested = metadata[paths[1]]
                original_worktree = git(source / paths[1], "config", "--get", "core.worktree").decode().strip()
                git(source / paths[1], "config", "extensions.worktreeConfig", "true")
                git(source / paths[1], "config", "--file", str(nested / "config.worktree"),
                    "core.worktree", original_worktree)
                git(source / paths[1], "config", "--file", str(nested / "config.worktree"),
                    "fixture.value", "preserve this value")
                before = {
                    str(file): file.read_bytes()
                    for directory in metadata.values()
                    for file in (directory / "config", directory / "config.worktree", directory / "index")
                    if file.exists()
                }
                heads = {path: git(source / path, "rev-parse", "HEAD") for path in paths}
                indexes = {path: git(source / path, "ls-files", "--stage", "-z") for path in paths}
                objects = {path: git(source / path, "show", "HEAD:payload") for path in paths}
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(source=source)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 2)
                argv = calls[1]
                mounts = [argv[i + 1] for i, arg in enumerate(argv) if arg == "--mount"]
                overlays = {}
                for mount in mounts:
                    fields = dict(part.split("=", 1) for part in mount.split(","))
                    if "/git-configs." in fields.get("source", ""):
                        self.assertEqual(fields["ro"], "true")
                        overlays[fields["destination"]] = Path(fields["source"])
                self.assertEqual(len(overlays), 3, "both nested configs and config.worktree must relocate")
                for path, directory in metadata.items():
                    raw = (source / path / ".git").read_text().removeprefix("gitdir: ").strip()
                    guest_dir = os.path.normpath(os.path.join("/src", path, raw))
                    for name in ("config", "config.worktree"):
                        original = directory / name
                        if not original.exists():
                            continue
                        copied = overlays[guest_dir + "/" + name]
                        self.assertEqual(
                            git(source, "config", "--file", str(copied), "--get", "core.worktree").decode().strip(),
                            "/src/" + path,
                        )
                        def other_values(config):
                            values = git(source, "config", "--file", str(config), "--null", "--list").split(b"\0")
                            return [value for value in values if not value.startswith(b"core.worktree\n")]
                        self.assertEqual(other_values(copied), other_values(original))
                    self.assertEqual(git(source / path, "rev-parse", "HEAD"), heads[path])
                    self.assertEqual(git(source / path, "ls-files", "--stage", "-z"), indexes[path])
                    self.assertEqual(git(source / path, "show", "HEAD:payload"), objects[path])
                for file, contents in before.items():
                    self.assertEqual(Path(file).read_bytes(), contents, file)


    def test_relocates_nested_linked_worktrees_with_external_common_metadata(self):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = os.environ.copy()
        git_env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       GIT_OPTIONAL_LOCKS="0")

        def git(root, *args):
            result = subprocess.run(
                [git_bin, "-c", "protocol.file.allow=always", "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", "-C", str(root), *args],
                env=git_env, capture_output=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            return result.stdout

        def seed(name):
            root = self.root / name
            root.mkdir()
            git(root, "init", "-q")
            (root / "payload").write_text(name + "\n")
            git(root, "add", "payload")
            git(root, "commit", "-qm", "fixture")
            return root

        leaf = seed("external-leaf")
        child = seed("external-child")
        git(child, "submodule", "add", "-q", str(leaf), "nested leaf")
        git(child, "commit", "-qam", "nested fixture")
        product = seed("external-product")
        git(product, "submodule", "add", "-q", str(child), "nested child")
        git(product, "commit", "-qam", "product fixture")
        source = self.root / "linked-source"
        git(product, "worktree", "add", "--detach", str(source))
        paths = ["nested child", "nested child/nested leaf"]
        # These child worktrees share the external seed stores, not the root's
        # metadata tree. Their unchanged commondir files must resolve in /src.
        git(child, "worktree", "add", "--detach", str(source / paths[0]))
        git(leaf, "worktree", "add", "--detach", str(source / paths[1]))
        metadata = {}
        for path in paths:
            repo = source / path
            git(repo, "config", "extensions.worktreeConfig", "true")
            git(repo, "config", "--worktree", "core.worktree", str(repo))
            git(repo, "config", "--worktree", "fixture.value", "preserve child value")
            directory = Path(git(repo, "rev-parse", "--absolute-git-dir").decode().strip())
            common = Path(git(repo, "rev-parse", "--path-format=absolute",
                              "--git-common-dir").decode().strip())
            self.assertNotEqual(directory, common)
            self.assertFalse(common.is_relative_to(product / ".git"))
            metadata[path] = (directory, common)
        self.assertEqual(git(source, "status", "--porcelain=v1", "--ignore-submodules=none"), b"")
        before = {str(f): f.read_bytes() for directory, common in metadata.values()
                  for f in (directory / "HEAD", directory / "index", directory / "commondir",
                            directory / "config.worktree", common / "config") if f.is_file()}
        gitfiles = {path: (source / path / ".git").read_bytes() for path in paths}
        identities = {path: (git(source / path, "rev-parse", "HEAD"),
                             git(source / path, "ls-files", "--stage", "-z"),
                             git(source / path, "show", "HEAD:payload")) for path in paths}
        result, calls = self.invoke(source=source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        mounts = [dict(field.split("=", 1) for field in argv[i + 1].split(","))
                  for i, arg in enumerate(argv) if arg == "--mount"]
        destinations = {mount["destination"]: mount for mount in mounts}
        for path, (directory, common) in metadata.items():
            raw = gitfiles[path].decode().removeprefix("gitdir: ").strip()
            guest_dir = os.path.normpath(os.path.join("/src", path, raw))
            guest_common = os.path.normpath(os.path.join(
                guest_dir, (directory / "commondir").read_text().strip()))
            self.assertIn(guest_common, destinations,
                          "nested commondir must resolve outside the root metadata mount")
            for actual, destination in ((common, guest_common), (directory, guest_dir)):
                self.assertEqual(destinations[destination]["source"], str(actual))
                self.assertEqual(destinations[destination]["ro"], "true")
            for actual, destination in ((common / "config", guest_common + "/config"),
                                         (directory / "config.worktree", guest_dir + "/config.worktree")):
                overlay = destinations[destination]
                self.assertEqual(overlay["ro"], "true")
                copied = Path(overlay["source"])
                self.assertNotEqual(copied, actual)
                self.assertTrue(copied.is_relative_to(self.root / "output"))
                self.assertEqual(git(source, "config", "--file", str(copied),
                                     "--get", "core.worktree").decode().strip(), "/src/" + path)

                def non_worktree(config):
                    values = git(source, "config", "--file", str(config),
                                 "--null", "--list").split(b"\0")
                    return [value for value in values if not value.startswith(b"core.worktree\n")]

                self.assertEqual(non_worktree(copied), non_worktree(actual))
            self.assertEqual((source / path / ".git").read_bytes(), gitfiles[path])
            self.assertEqual((git(source / path, "rev-parse", "HEAD"),
                              git(source / path, "ls-files", "--stage", "-z"),
                              git(source / path, "show", "HEAD:payload")), identities[path])
        for filename, data in before.items():
            self.assertEqual(Path(filename).read_bytes(), data, filename)
        self.assertFalse(any(value.startswith(("GIT_DIR=", "GIT_WORK_TREE=", "GIT_CONFIG_COUNT="))
                             for value in argv))


    def test_relocates_gitfile_roots_and_common_metadata_without_global_git_overrides(self):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = os.environ.copy()
        git_env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       GIT_OPTIONAL_LOCKS="0")

        def git(root, *args):
            result = subprocess.run(
                [git_bin, "-c", "protocol.file.allow=always", "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", "-C", str(root), *args],
                env=git_env, capture_output=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            return result.stdout

        def seed(name):
            root = self.root / name
            root.mkdir()
            git(root, "init", "-q")
            (root / "payload").write_text(name + "\n")
            git(root, "add", "payload")
            git(root, "commit", "-qm", "fixture")
            return root

        leaf = seed("root-leaf-seed")
        product = seed("root-product-seed")
        git(product, "submodule", "add", "-q", str(leaf), "nested module")
        git(product, "commit", "-qam", "nested product fixture")
        parent = seed("root-parent-seed")
        git(parent, "submodule", "add", "-q", str(product), "hermit")
        git(parent, "commit", "-qam", "product submodule fixture")

        for topology in ("parent-submodule", "absolute-worktree", "relative-worktree"):
            with self.subTest(topology=topology):
                checkout = self.root / topology
                if topology == "parent-submodule":
                    # The failing production topology: Hermit itself is a submodule
                    # of a parent's linked worktree, with a relative root gitfile.
                    git(parent, "worktree", "add", "--detach", str(checkout))
                    git(checkout, "submodule", "update", "--init", "--recursive")
                    source = checkout / "hermit"
                else:
                    git(product, "worktree", "add", "--detach", str(checkout))
                    source = checkout
                    git(source, "submodule", "update", "--init", "--recursive")
                    if topology == "relative-worktree":
                        directory = git(source, "rev-parse", "--absolute-git-dir").decode().strip()
                        (source / ".git").write_text(
                            "gitdir: " + os.path.relpath(directory, source) + "\n")

                directory = Path(git(source, "rev-parse", "--absolute-git-dir").decode().strip())
                common = Path(git(source, "rev-parse", "--path-format=absolute",
                                  "--git-common-dir").decode().strip())
                git(source, "config", "extensions.worktreeConfig", "true")
                git(source, "config", "--worktree", "core.worktree", str(source))
                git(source, "config", "--worktree", "fixture.value", "keep root-only value")
                nested = source / "nested module"
                nested_dir = Path(git(nested, "rev-parse", "--absolute-git-dir").decode().strip())
                metadata_dirs = set((directory, common, nested_dir))
                before = {str(f): f.read_bytes() for d in metadata_dirs
                          for name in ("config", "config.worktree", "HEAD", "index", "commondir")
                          if (f := d / name).is_file()}
                identities = {str(repo): (git(repo, "rev-parse", "HEAD"),
                                         git(repo, "ls-files", "--stage", "-z"),
                                         git(repo, "show", "HEAD:payload"))
                              for repo in (source, nested)}
                raw = (source / ".git").read_text().removeprefix("gitdir: ").strip()
                guest_dir = os.path.normpath(os.path.join("/src", raw))
                self.assertEqual(os.path.isabs(raw), topology == "absolute-worktree")
                if topology != "absolute-worktree":
                    self.assertNotEqual(guest_dir, str(directory))
                common_raw = ((directory / "commondir").read_text().strip()
                              if (directory / "commondir").is_file() else ".")
                guest_common = os.path.normpath(os.path.join(guest_dir, common_raw))
                self.assertEqual(directory == common, topology == "parent-submodule")
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(source=source)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 2)
                argv = calls[1]
                mounts = [dict(field.split("=", 1) for field in argv[i + 1].split(","))
                          for i, arg in enumerate(argv) if arg == "--mount"]
                destinations = {m["destination"]: m for m in mounts}
                self.assertIn(guest_dir, destinations,
                              "root gitfile must resolve to the actual root metadata mount")
                self.assertEqual(destinations[guest_dir]["source"], str(directory))
                self.assertEqual(destinations[guest_dir]["ro"], "true")
                self.assertIn(guest_common, destinations,
                              "relative commondir must resolve to the actual common metadata")
                self.assertEqual(destinations[guest_common]["source"], str(common))
                self.assertEqual(destinations[guest_common]["ro"], "true")
                for actual, destination in ((common / "config", guest_common + "/config"),
                                             (directory / "config.worktree", guest_dir + "/config.worktree")):
                    overlay = destinations[destination]
                    self.assertEqual(overlay["ro"], "true")
                    copied = Path(overlay["source"])
                    self.assertNotEqual(copied, actual)
                    self.assertTrue(copied.is_relative_to(self.root / "output"))
                    self.assertEqual(git(source, "config", "--file", str(copied),
                                         "--get", "core.worktree").decode().strip(), "/src")
                    def non_worktree(config):
                        values = git(source, "config", "--file", str(config),
                                     "--null", "--list").split(b"\0")
                        return [value for value in values if not value.startswith(b"core.worktree\n")]
                    self.assertEqual(non_worktree(copied), non_worktree(actual))
                nested_raw = (nested / ".git").read_text().removeprefix("gitdir: ").strip()
                guest_nested = os.path.normpath(os.path.join("/src/nested module", nested_raw))
                self.assertEqual(destinations[guest_nested]["source"], str(nested_dir))
                nested_copy = destinations[guest_nested + "/config"]
                self.assertEqual(nested_copy["ro"], "true")
                self.assertEqual(git(source, "config", "--file", nested_copy["source"],
                                     "--get", "core.worktree").decode().strip(), "/src/nested module")
                self.assertEqual(argv[-3:], ["fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000", "/not-executed/command", "literal argument"])
                self.assertFalse(any(value.startswith(("GIT_DIR=", "GIT_WORK_TREE=", "GIT_CONFIG_COUNT="))
                                     for value in argv), "root Git overrides must not leak to nested Git")
                for filename, data in before.items():
                    self.assertEqual(Path(filename).read_bytes(), data, filename)
                for repo in (source, nested):
                    self.assertEqual((git(repo, "rev-parse", "HEAD"),
                                      git(repo, "ls-files", "--stage", "-z"),
                                      git(repo, "show", "HEAD:payload")), identities[str(repo)])


class PinnedGuestPathContract(unittest.TestCase):
    def test_declared_guest_paths_cover_existing_liteinst_and_portable_consumers(self):
        here = Path(__file__).resolve().parent
        paths = (here / "guest-paths.txt").read_text().splitlines()
        self.assertEqual(len(paths), len(set(paths)), "duplicate guest paths")
        self.assertTrue(paths)
        for path in paths:
            self.assertRegex(path, r"^/usr/bin/[A-Za-z0-9_-]+$")
        source = (here.parents[1] / "hermit-cli/tests/liteinst_advanced.rs").read_text()
        literals = set(re.findall(r'"(/usr/bin/[A-Za-z0-9_-]+)"', source))
        self.assertTrue(literals, "the source population must not disappear")
        self.assertEqual(literals - set(paths), set(), "missing existing LiteInst guest")
        portable = {"/usr/bin/" + command for command in
                    "bash date df du find git node nodejs nproc python3 sort stat tr".split()}
        self.assertEqual(portable - set(paths), set(), "lost existing portable guest")
        self.assertIn("/usr/bin/printf", paths, "DBT ptrace argument-forwarding reference")


if __name__ == "__main__":
    unittest.main()
