#!/usr/bin/env python3
"""Controlled accepted-only evidence tests; no manager/BPF/live recovery action.

Actor observations and package qualification are explicit substituted premises.
The production descriptor custody, parser, lock and inspect orchestration run.
"""
import copy
import contextlib
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import tempfile
import sys
from types import SimpleNamespace
import unittest
from unittest import mock

import accepted_resource_recovery as ar
import test_network_recovery as old_fixture

nr = ar.nr
LABEL = old_fixture.LABEL


class Fixture(old_fixture.PrivateFixture):
    def root(self):
        # Use the production inspector's verified dependency instance.
        return nr.HeldRoot(self.path, os.getuid(), 384)

    def accepted(self):
        names, rows, service, artifact = super().accepted()
        rows.pop()
        service[0]["fd_journal_unretired"] = None
        service[1]["service_status"] = 0
        self.write(names[0], old_fixture.encoded(rows))
        self.write(names[1], old_fixture.encoded(service))
        return names, rows, service, artifact


def properties(unit):
    return {"Id": unit, "LoadState": "not-found", "ActiveState": "inactive", "SubState": "dead",
            "InvocationID": "", "Result": "success", "ExecMainStatus": "0", "MainPID": "0", "ControlGroup": ""}


class InspectorTests(unittest.TestCase):
    def setUp(self):
        self.f = Fixture()
        self.addCleanup(self.f.temp.cleanup)
        self.names, self.rows, self.service, self.artifact = self.f.accepted()

    def read_source(self, expected=None):
        root = self.f.root()
        try:
            value = ar.source_original(root, LABEL, self.artifact if expected is None else expected)
            root.recheck()
            return value
        finally:
            root.close()

    def test_exact_original_remains_missing_parent_and_read_only(self):
        before = self.census()
        result = self.read_source()
        self.assertEqual(result["execution_status"], "missing-parent-terminal")
        self.assertEqual(result["original_rows"], 2)
        self.assertEqual(result["service_status"], 0)
        self.assertEqual(len(result["ids"]), 122)
        self.assertEqual([sum(row[1] == kind for row in result["ids"]) for kind in range(3)], [24, 49, 49])
        self.assertEqual(before, self.census())
        # The previous protocol must still refuse this different source shape.
        root = self.f.root()
        try:
            with self.assertRaises(nr.Refused):
                nr.inspect_accepted(root, LABEL, self.artifact)
        finally:
            root.close()

    def census(self):
        result = {}
        for p in self.f.path.iterdir():
            s = p.lstat()
            result[p.name] = [s.st_dev, s.st_ino, s.st_mode, s.st_uid, s.st_nlink,
                              s.st_size, s.st_mtime_ns, s.st_ctime_ns, hashlib.sha256(p.read_bytes()).hexdigest()]
        return result

    def test_exact_original_identity_and_close_negative_matrix(self):
        changes = [
            lambda r, s: r.append({"schema": 1, "stage": "accepted_failed", "error": "invented"}),
            lambda r, s: r[0].update(stage="accepted_terminal"),
            lambda r, s: r[0].update(label="f" * 32),
            lambda r, s: r[1].update(schema=True),
            lambda r, s: r[0]["root"].update(inode=0),
            lambda r, s: r[0]["files"][1].update(inode=0),
            lambda r, s: r[0].update(extra=1),
            lambda r, s: r[1]["observed"].update(run=[0] * 16),
            lambda r, s: r[1]["observed"]["loader"].update(unit="wrong.service"),
            lambda r, s: r[1]["observed"]["query"].update(invocation=LABEL),
            lambda r, s: r[1]["observed"]["query"].update(inode=12),
            lambda r, s: s[0].update(controller_terminal=False),
            lambda r, s: s[0].pop("failure"),
            lambda r, s: s[1].pop("close_error"),
            lambda r, s: s[1].update(service_status=125),
            lambda r, s: s[1].update(service_status=False),
            lambda r, s: s[1].update(closed_ns=0),
            lambda r, s: s[1].update(socket_release="complete"),
            lambda r, s: s[1]["close_receipts"][0].update(incarnation=0),
            lambda r, s: s[1]["close_receipts"][0].update(unexpected_drop=True),
            lambda r, s: s[1]["close_receipts"][0]["close"].pop("errno"),
            lambda r, s: s[1]["close_receipts"][0]["close"].update(returned=False),
            lambda r, s: s[0]["inventories"][0]["ids"].pop(),
            lambda r, s: s[0]["inventories"][0]["ids"].append({"kind": 2, "id": 999}),
            lambda r, s: s[0]["inventories"][0]["ids"].__setitem__(1, {"kind": 0, "id": 1}),
            lambda r, s: s[0]["inventories"][0]["ids"][0].update(kind=3),
            lambda r, s: s[0]["inventories"][0]["ids"][0].update(id=0),
            lambda r, s: s[1]["close_receipts"][0]["inventory"]["ids"][0].update(id=999),
            lambda r, s: s[0]["inventories"][0].update(complete=False),
            lambda r, s: s[0]["inventories"][0]["status"].pop("errno"),
            lambda r, s: s[0]["inventories"][0]["status"].update(returned=False),
        ]
        for index, change in enumerate(changes):
            with self.subTest(index=index):
                # Copy separately: actual before/after JSON inventories are separate objects.
                r, s = copy.deepcopy(self.rows), json.loads(json.dumps(self.service))
                change(r, s)
                self.f.write(self.names[0], old_fixture.encoded(r))
                self.f.write(self.names[1], old_fixture.encoded(s))
                with self.assertRaises((nr.Refused, KeyError, ValueError)):
                    self.read_source()

    def test_zero_artifacts_joint_mutation_cannot_manufacture_package_authority(self):
        for key in ("contract_sha256", "object_sha256", "library_sha256", "btf_sha256"):
            with self.subTest(key=key):
                a = copy.deepcopy(self.artifact)
                (a["topology"] if key == "contract_sha256" else a)[key] = [0] * 32
                r = copy.deepcopy(self.rows)
                r[0]["artifact"] = a
                r[1]["observed"]["artifact"] = a
                self.f.write(self.names[0], old_fixture.encoded(r))
                with self.assertRaisesRegex(nr.Refused, "digest"):
                    self.read_source(a)

    def test_joint_float_artifact_counts_and_foreign_cgroup_refuse(self):
        for key in ("maps", "programs", "links"):
            a = copy.deepcopy(self.artifact)
            a[key] = float(a[key])
            r = copy.deepcopy(self.rows)
            r[0]["artifact"] = a
            r[1]["observed"]["artifact"] = a
            self.f.write(self.names[0], old_fixture.encoded(r))
            with self.subTest(key=key), self.assertRaisesRegex(nr.Refused, "topology"):
                self.read_source(a)
        r = copy.deepcopy(self.rows)
        r[1]["observed"]["loader"]["cgroup"] = "/sys/fs/cgroup/foreign/" + r[1]["observed"]["loader"]["unit"]
        self.f.write(self.names[0], old_fixture.encoded(r))
        with self.assertRaisesRegex(nr.Refused, "original run"):
            self.read_source()

    def test_partial_duplicate_and_extra_rows_refuse(self):
        original = (self.f.path / self.names[0]).read_bytes()
        for changed in (original[:-1], original + b"{}\n",
                        original.replace(b'"schema": 1', b'"schema": 1, "schema": 1', 1)):
            with self.subTest(changed=changed[-30:]):
                self.f.write(self.names[0], changed)
                with self.assertRaises(nr.Refused):
                    self.read_source()

    def test_role_custody_and_unknown_names_refuse(self):
        selected = self.f.path / self.names[1]
        selected.chmod(0o644)
        with self.assertRaises(nr.Refused):
            self.read_source()
        selected.chmod(0o600)
        self.f.write("unexpected", b"kept")
        with self.assertRaisesRegex(nr.Refused, "unknown"):
            self.read_source()
        (self.f.path / "unexpected").unlink()
        selected.unlink()
        selected.symlink_to(self.f.path / self.names[2])
        with self.assertRaises(OSError):
            self.read_source()
        selected.unlink()
        os.mkfifo(selected, 0o600)
        with self.assertRaises(nr.Refused):
            self.read_source()

    def test_actual_shared_launch_lock_blocks_inspector(self):
        fd = os.open(self.f.path, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError):
                self.read_source()
        finally:
            os.close(fd)
        self.assertEqual(len(self.read_source()["ids"]), 122)

    def test_actor_requires_exact_collected_state_and_actual_lookup(self):
        original = self.rows[1]["observed"]["loader"]
        valid = properties(original["unit"])
        with mock.patch.object(ar, "manager_query", return_value=(0, valid)), \
                mock.patch.object(ar, "cgroup_absent", return_value=errno.ENOENT) as absence:
            row = ar.observe_actor(original, nr.Deadline())
            absence.assert_called_once_with(original["cgroup"])
            self.assertEqual(row[:2], [original["unit"], [original[k] for k in ("invocation", "cgroup", "device", "inode")]])
            self.assertLessEqual(row[4], row[5])
        for key, value in (("Id", "wrong"), ("LoadState", "loaded"), ("ActiveState", "active"),
                           ("SubState", "running"), ("InvocationID", LABEL), ("MainPID", "2"),
                           ("ControlGroup", original["cgroup"]), ("ExecMainStatus", "125")):
            changed = {**valid, key: value}
            with self.subTest(key=key), mock.patch.object(ar, "manager_query", return_value=(0, changed)), \
                    mock.patch.object(ar, "cgroup_absent") as absence:
                with self.assertRaises(nr.Refused):
                    ar.observe_actor(original, nr.Deadline())
                absence.assert_not_called()
        with mock.patch.object(ar, "manager_query", return_value=(1, valid)):
            with self.assertRaises(nr.Refused):
                ar.observe_actor(original, nr.Deadline())

    def test_cgroup_lookup_only_actual_enoent_counts(self):
        missing = self.f.path / "absent"
        self.assertEqual(ar.cgroup_absent(missing), errno.ENOENT)
        missing.symlink_to(self.f.path / "also-absent")
        with self.assertRaises(nr.Refused):
            ar.cgroup_absent(missing)
        with mock.patch.object(ar.os, "stat", side_effect=PermissionError(errno.EACCES, "denied")):
            with self.assertRaises(nr.Refused):
                ar.cgroup_absent(missing)

    def test_production_inspect_preserves_files_and_has_stable_plan(self):
        args = SimpleNamespace(owner_uid=os.getuid(), accepted_root=str(self.f.path),
                               accepted_label=LABEL, accepted_package="/controlled/package")
        before = self.census()
        # Only package/kernel/manager absence are substituted; no BPF method runs.
        with mock.patch.object(nr, "expected_package", return_value=self.artifact), \
                mock.patch.object(ar, "manager_query", side_effect=lambda unit, _: (0, properties(unit))), \
                mock.patch.object(ar, "cgroup_absent", return_value=errno.ENOENT), \
                mock.patch.object(nr, "NativeRead", side_effect=AssertionError("inspection must not query BPF")):
            first, second = ar.inspect(args), ar.inspect(args)
        self.assertEqual(first["plan_sha256"], second["plan_sha256"])
        self.assertNotEqual(first["actor_observations_before"], second["actor_observations_before"])
        for key in ("execution_success", "admission_authority", "mutations_performed", "resource_recovery_complete"):
            self.assertIs(first[key], False)
        self.assertEqual(before, self.census())

    def test_dependency_bytes_are_checked_before_execution(self):
        path = self.f.write("bad-producer.py", b"raise AssertionError('unverified code executed')\n")
        with self.assertRaisesRegex(ValueError, "source differs"):
            ar.load_dependency(path)
        path.unlink()
        path.symlink_to(Path(nr.__file__))
        with self.assertRaises(OSError):
            ar.load_dependency(path)




class AbsentNative:
    """Controlled GET_FD_BY_ID premise; production scanner child/wait is real."""
    def by_id(self, kind, ident):
        raise OSError(errno.ENOENT, "controlled absent object")


@contextlib.contextmanager
def controlled_platform(artifact, native=AbsentNative):
    # The actual privileged identity and kernel inventory are substituted.
    # Fork, child executable/start identity, IPC, waitpid, parser and append are real.
    with mock.patch.object(nr, "expected_package", return_value=artifact), \
            mock.patch.object(ar, "manager_query", side_effect=lambda unit, _: (0, properties(unit))), \
            mock.patch.object(ar, "cgroup_absent", return_value=errno.ENOENT), \
            mock.patch.object(nr, "NativeRead", native), \
            mock.patch.object(ar.os, "getuid", return_value=0), \
            mock.patch.object(ar.os, "geteuid", return_value=0):
        yield


def arguments(fixture, action="inspect"):
    return SimpleNamespace(owner_uid=fixture.path.stat().st_uid, accepted_root=str(fixture.path),
                           accepted_label=LABEL, accepted_package="/controlled/package", action=action,
                           expect_inspection_sha256=None)


def controlled_certificate(fixture, artifact):
    args = arguments(fixture)
    with controlled_platform(artifact):
        inspected = ar.inspect(args)
        args.action = "certify"
        args.expect_inspection_sha256 = inspected["plan_sha256"]
        result = ar.inspect(args)
    return inspected, result


class CertificateTests(unittest.TestCase):
    setUp = InspectorTests.setUp
    census = InspectorTests.census
    def test_actual_writer_and_child_wait_append_only_one_use(self):
        before = self.census()
        prefix = (self.f.path / self.names[0]).read_bytes()
        inspected, result = controlled_certificate(self.f, self.artifact)
        self.assertFalse(result["execution_success"])
        self.assertFalse(result["normal_terminal_success"])
        self.assertEqual(result["ids_per_pass"], 122)
        self.assertEqual(result["passes"], 2)
        data = (self.f.path / self.names[0]).read_bytes()
        self.assertTrue(data.startswith(prefix))
        rows = data.splitlines(keepends=True)
        self.assertEqual(len(rows), 4)
        intent, proof = [nr.decode(row) for row in rows[2:]]
        self.assertEqual(proof[1], nr.digest(rows[2]))
        self.assertEqual(len(proof[7]), 2)
        self.assertTrue(all(len(scan[2]) == 122 for scan in proof[7]))
        self.assertEqual(proof[6], 0)
        self.assertNotEqual(proof[5][0], os.getpid())  # Actual scanner child, not parent synthesis.
        self.assertEqual(proof[5][5], ar.source_identity())
        self.assertEqual(proof[10], ar.source_identity())
        for name in self.names[1:]:
            self.assertEqual(before[name], self.census()[name])
        after = self.census()
        with controlled_platform(self.artifact):
            with self.assertRaises(nr.Refused):
                ar.inspect(arguments(self.f, "certify"))
        self.assertEqual(after, self.census())
        self.assertEqual(inspected["plan"][6]["original_rows"], 2)

    def test_wrong_plan_or_actual_privilege_refuses_before_intent(self):
        before = self.census()
        args = arguments(self.f, "certify")
        args.expect_inspection_sha256 = "0" * 64
        with controlled_platform(self.artifact):
            with self.assertRaisesRegex(nr.Refused, "plan digest"):
                ar.inspect(args)
        self.assertEqual(before, self.census())
        with controlled_platform(self.artifact):
            args.expect_inspection_sha256 = ar.inspect(arguments(self.f))["plan_sha256"]
            with mock.patch.object(ar.os, "geteuid", return_value=123):
                with self.assertRaisesRegex(nr.Refused, "privileged"):
                    ar.inspect(args)
        self.assertEqual(before, self.census())

    def test_live_or_reused_id_keeps_intent_and_real_failed_child(self):
        class LiveNative:
            def by_id(self, kind, ident):
                return os.open(os.devnull, os.O_RDONLY)
        prefix = (self.f.path / self.names[0]).read_bytes()
        args = arguments(self.f)
        with controlled_platform(self.artifact, LiveNative):
            inspected = ar.inspect(args)
            args.action, args.expect_inspection_sha256 = "certify", inspected["plan_sha256"]
            with self.assertRaisesRegex(nr.Refused, "privileged scanner did not exit successfully"):
                ar.inspect(args)
        data = (self.f.path / self.names[0]).read_bytes()
        self.assertTrue(data.startswith(prefix))
        self.assertEqual(len(data.splitlines()), 3)
        self.assertEqual(nr.decode(data.splitlines()[2])[0], "hermit-accepted-resource-intent-v1")
        with controlled_platform(self.artifact):
            with self.assertRaises(nr.Refused):
                ar.inspect(args)
        self.assertEqual(data, (self.f.path / self.names[0]).read_bytes())

    def test_query_error_is_not_absence(self):
        class DeniedNative:
            def by_id(self, kind, ident):
                raise OSError(errno.EPERM, "controlled permission refusal")
        args = arguments(self.f)
        with controlled_platform(self.artifact, DeniedNative):
            inspected = ar.inspect(args)
            args.action, args.expect_inspection_sha256 = "certify", inspected["plan_sha256"]
            with self.assertRaisesRegex(nr.Refused, "errno=1"):
                ar.inspect(args)
        self.assertEqual(len((self.f.path / self.names[0]).read_bytes().splitlines()), 3)


def export_fixture(path):
    path = Path(path)
    nr.require(path.is_absolute() and not path.exists(), "fixture needs a new absolute directory")
    path.mkdir(mode=0o700)
    fixture = Fixture(path / "accepted")
    names, rows, service, artifact = fixture.accepted()
    for name in ("unix", "pins"):
        (path / name).mkdir(mode=0o700)
    inspected, result = controlled_certificate(fixture, artifact)
    metadata = {"accepted_root": str(fixture.path), "unix_root": str(path / "unix"),
                "pin_root": str(path / "pins"), "accepted_label": LABEL,
                "inspection": inspected, "result": result,
                "controlled_premises": ["privileged uid", "kernel BPF absence", "package", "manager and cgroup absence"],
                "actual_operations": ["original parser", "directory flock", "fork", "child IPC/wait", "append/readback"]}
    (path / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    return metadata


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        print(json.dumps(export_fixture(sys.argv[2]), sort_keys=True))
    else:
        unittest.main()
