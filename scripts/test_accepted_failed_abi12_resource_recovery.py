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

import accepted_failed_abi12_resource_recovery as ar
import accepted_failed_resource_recovery as abi8_ar
import accepted_resource_recovery as old_ar
import test_network_recovery as old_fixture

nr = ar.nr
LABEL = old_fixture.LABEL


class Fixture(old_fixture.PrivateFixture):
    def root(self):
        # Use the production inspector's verified dependency instance.
        return nr.HeldRoot(self.path, os.getuid(), 384)

    def accepted(self):
        names, rows, service, artifact = super().accepted()
        artifact["maps"] = 26
        ids = [{"kind": kind, "id": kind * 100 + index + 1}
               for kind, count in enumerate((26, 49, 49)) for index in range(count)]
        service[0]["inventories"][0]["ids"] = ids
        service[1]["close_receipts"][0]["inventory"]["ids"] = copy.deepcopy(ids)
        service[0]["provider"] = {"active_setters": 1}
        service[0]["fd_journal_unretired"] = 322
        artifact["wire_format"] = "abi12-copy5"
        artifact["topology"]["contract_sha256"] = list(bytes.fromhex(ar.CONTRACT_CANONICAL))
        rows[2]["error"] = ar.FAILED_ERROR
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

    def test_child125_is_exclusive_and_abi8_cannot_consume_it(self):
        root = self.f.root()
        try:
            with self.assertRaises(abi8_ar.nr.Refused):
                abi8_ar.source_original(root, LABEL, self.artifact)
        finally:
            root.close()
        for wait in ("Some(0)", "None", "Some(9)", "Some(32256)"):
            rows = copy.deepcopy(self.rows)
            rows[2]["error"] = ar.FAILED_ERROR.replace("Some(32000)", wait)
            self.f.write(self.names[0], old_fixture.encoded(rows))
            before = self.census()
            with self.subTest(wait=wait), self.assertRaisesRegex(nr.Refused, "failed wrapper"):
                self.read_source()
            self.assertEqual(before, self.census())

    def test_exact_original_remains_failed_and_read_only(self):
        before = self.census()
        result = self.read_source()
        self.assertEqual(result["execution_status"], "failed-service125")
        self.assertEqual(result["original_rows"], 3)
        self.assertEqual(result["service_status"], 125)
        self.assertEqual(len(result["ids"]), 124)
        self.assertEqual([sum(row[1] == kind for row in result["ids"]) for kind in range(3)], [26, 49, 49])
        self.assertEqual(before, self.census())
        # The previous protocol must still refuse this different source shape.
        root = self.f.root()
        try:
            with self.assertRaises(old_ar.nr.Refused):
                old_ar.source_original(root, LABEL, self.artifact)
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
            lambda r, s: r.pop(),
            lambda r, s: r[2].update(error="invented"),
            lambda r, s: r[2].update(stage="accepted_terminal"),
            lambda r, s: r[2].update(label="f" * 32),
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
            lambda r, s: s[1].update(service_status=0),
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
        self.assertEqual(len(self.read_source()["ids"]), 124)

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
                               accepted_label=LABEL, accepted_package="/controlled/package", accepted_contract="/controlled/contract")
        before = self.census()
        # Only package/kernel/manager absence are substituted; no BPF method runs.
        with mock.patch.object(ar, "expected_package", return_value=self.artifact), \
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
    with mock.patch.object(ar, "expected_package", return_value=artifact), \
            mock.patch.object(ar, "manager_query", side_effect=lambda unit, _: (0, properties(unit))), \
            mock.patch.object(ar, "cgroup_absent", return_value=errno.ENOENT), \
            mock.patch.object(nr, "NativeRead", native), \
            mock.patch.object(ar.os, "getuid", return_value=0), \
            mock.patch.object(ar.os, "geteuid", return_value=0):
        yield


def arguments(fixture, action="inspect"):
    return SimpleNamespace(owner_uid=fixture.path.stat().st_uid, accepted_root=str(fixture.path),
                           accepted_label=LABEL, accepted_package="/controlled/package", accepted_contract="/controlled/contract", action=action,
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
        self.assertEqual(result["ids_per_pass"], 124)
        self.assertEqual(result["passes"], 2)
        data = (self.f.path / self.names[0]).read_bytes()
        self.assertTrue(data.startswith(prefix))
        rows = data.splitlines(keepends=True)
        self.assertEqual(len(rows), 5)
        intent, proof = [nr.decode(row) for row in rows[3:]]
        self.assertEqual(proof[1], nr.digest(rows[3]))
        self.assertEqual(len(proof[7]), 2)
        self.assertTrue(all(len(scan[2]) == 124 for scan in proof[7]))
        self.assertEqual(proof[6], 0)
        self.assertNotEqual(proof[5][0], os.getpid())  # Actual scanner child, not parent synthesis.
        self.assertEqual(proof[5][5], ar.source_identity())
        self.assertEqual(proof[10], ar.source_identity())
        self.assertEqual(intent[0], "hermit-accepted-failed-abi12-resource-intent-v1")
        self.assertEqual(intent[11], "failed-service125-child125-after-physical-close-abi12")
        self.assertEqual(proof[0], "hermit-accepted-failed-abi12-resource-result-v1")
        self.assertEqual(intent[3] - intent[2], 5_000_000_000)
        self.assertEqual(proof[4] - proof[3], 1_000_000_000)
        self.assertEqual(intent[7][5][0], "abi12-copy5")
        self.assertEqual(intent[7][5][-3:], [26, 49, 49])
        self.assertEqual(intent[10], nr.digest(nr.wire(intent[9])[:-1]))
        retained_service = nr.rows((self.f.path / self.names[1]).read_bytes(), 2)
        self.assertEqual(retained_service[0]["provider"]["active_setters"], 1)
        self.assertEqual(retained_service[0]["fd_journal_unretired"], 322)
        self.assertEqual(nr.rows(prefix, 3)[2]["error"], ar.FAILED_ERROR)
        for name in self.names[1:]:
            self.assertEqual(before[name], self.census()[name])
        after = self.census()
        with controlled_platform(self.artifact):
            with self.assertRaises(nr.Refused):
                ar.inspect(arguments(self.f, "certify"))
        self.assertEqual(after, self.census())
        self.assertEqual(inspected["plan"][6]["original_rows"], 3)

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
        self.assertEqual(len(data.splitlines()), 4)
        self.assertEqual(nr.decode(data.splitlines()[3])[0], "hermit-accepted-failed-abi12-resource-intent-v1")
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
        self.assertEqual(len((self.f.path / self.names[0]).read_bytes().splitlines()), 4)

    def test_bad_scanner_evidence_never_appends_result(self):
        changes = [
            lambda actor, wait, scans: (actor[:2] + [123] + actor[3:], wait, scans),
            lambda actor, wait, scans: (actor, 32000, scans),
            lambda actor, wait, scans: (actor, wait, scans[:1]),
            lambda actor, wait, scans: (actor, wait, list(reversed(scans))),
            lambda actor, wait, scans: (actor, wait, [
                [scans[0][0], scans[0][1], scans[0][2][:-1]], scans[1]]),
            lambda actor, wait, scans: (actor, wait, [
                [scans[0][0], scans[0][1], [[1, *scans[0][2][0][1:]], *scans[0][2][1:]]], scans[1]]),
            lambda actor, wait, scans: (actor, wait, [
                [scans[0][0], scans[0][1], [[*scans[0][2][0][:-1], errno.EPERM], *scans[0][2][1:]]], scans[1]]),
        ]
        scanner = nr.run_scanner
        for index, change in enumerate(changes):
            f = Fixture()
            self.addCleanup(f.temp.cleanup)
            names, _, _, artifact = f.accepted()
            prefix = (f.path / names[0]).read_bytes()
            args = arguments(f)
            # The child, IPC and genuine wait run before this deliberately
            # invalid returned evidence is presented to the production writer.
            def corrupt(*values):
                return change(*scanner(*values))
            with self.subTest(index=index), controlled_platform(artifact):
                inspected = ar.inspect(args)
                args.action = "certify"
                args.expect_inspection_sha256 = inspected["plan_sha256"]
                with mock.patch.object(nr, "run_scanner", side_effect=corrupt):
                    with self.assertRaises(nr.Refused):
                        ar.inspect(args)
            data = (f.path / names[0]).read_bytes()
            self.assertTrue(data.startswith(prefix))
            self.assertEqual(len(data.splitlines()), 4)

    def test_original_log_rewrite_after_scan_keeps_only_intent(self):
        args = arguments(self.f)
        prefix = (self.f.path / self.names[0]).read_bytes()
        scanner = nr.run_scanner
        def rewrite(*values):
            proof = scanner(*values)
            self.f.write(self.names[1], old_fixture.encoded(self.service) + b" ")
            return proof
        with controlled_platform(self.artifact):
            inspected = ar.inspect(args)
            args.action, args.expect_inspection_sha256 = "certify", inspected["plan_sha256"]
            with mock.patch.object(nr, "run_scanner", side_effect=rewrite):
                with self.assertRaisesRegex(nr.Refused, "receipt changed"):
                    ar.inspect(args)
        data = (self.f.path / self.names[0]).read_bytes()
        self.assertTrue(data.startswith(prefix))
        self.assertEqual(len(data.splitlines()), 4)


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


class PackageTests(unittest.TestCase):
    def test_original_contract_and_actual_artifact_bytes_are_required(self):
        import base64
        import zlib
        # Exact ABI12 contract; kernel BTF bytes alone are substituted.
        contract_bytes = zlib.decompress(base64.b64decode('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'))
        self.assertEqual(nr.digest(contract_bytes), ar.CONTRACT_RAW)
        contract = nr.decode(contract_bytes)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            contract_path = root / "contract.json"
            contract_path.write_bytes(contract_bytes)
            obj, library = b"controlled BPF artifact", b"controlled DSO artifact"
            (root / "accepted-provider.bpf.o").write_bytes(obj)
            (root / "libhermit_accepted_provider.so").write_bytes(library)
            manifest = {**contract, "kind": "hermit-accepted-provider", "object": "accepted-provider.bpf.o",
                        "library": "libhermit_accepted_provider.so", "object_sha256": nr.digest(obj),
                        "library_sha256": nr.digest(library), "sources": {"accepted-contract.json": ar.CONTRACT_RAW}}
            (root / "manifest.json").write_bytes(nr.wire(manifest))
            read = nr.bounded_regular
            def bounded(path, limit):
                if Path(path) == Path("/sys/kernel/btf/vmlinux"):
                    # Digest is overridden solely for this named kernel fixture.
                    return b"controlled kernel BTF", {}
                return read(path, limit)
            digest = nr.digest
            def hashed(data):
                return contract["btf_sha256"] if data == b"controlled kernel BTF" else digest(data)
            with mock.patch.object(nr, "bounded_regular", side_effect=bounded), mock.patch.object(nr, "digest", side_effect=hashed):
                artifact = ar.expected_package(root, contract_path)
                self.assertEqual(artifact["wire_format"], "abi12-copy5")
                (root / "accepted-provider.bpf.o").write_bytes(obj + b"changed")
                with self.assertRaisesRegex(nr.Refused, "artifact bytes"):
                    ar.expected_package(root, contract_path)
                (root / "accepted-provider.bpf.o").write_bytes(obj)
                for key, value in (("abi_version", "415052555354000b"), ("copy_version", 4), ("maps", 25),
                                   ("current_close_profile_version", 0), ("current_close_profile_bytes", 575),
                                   ("ftrace_only", False)):
                    changed = {**manifest, key: value}
                    (root / "manifest.json").write_bytes(nr.wire(changed))
                    with self.subTest(key=key), self.assertRaisesRegex(nr.Refused, "contract"):
                        ar.expected_package(root, contract_path)
                for key in ("current_close_profile_version", "current_close_profile_bytes"):
                    for omit in (False, True):
                        changed = {**manifest, key: None}
                        if omit:
                            del changed[key]
                        (root / "manifest.json").write_bytes(nr.wire(changed))
                        with self.subTest(key=key, omitted=omit), self.assertRaisesRegex(nr.Refused, "contract"):
                            ar.expected_package(root, contract_path)
                changed = {**manifest, "current_close_profile_version": True}
                (root / "manifest.json").write_bytes(nr.wire(changed))
                with self.assertRaisesRegex(nr.Refused, "current-close profile"):
                    ar.expected_package(root, contract_path)
                (root / "manifest.json").write_bytes(nr.wire(manifest))
                contract_path.write_bytes(contract_bytes + b" ")
                with self.assertRaisesRegex(nr.Refused, "contract bytes"):
                    ar.expected_package(root, contract_path)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        print(json.dumps(export_fixture(sys.argv[2]), sort_keys=True))
    else:
        unittest.main()
