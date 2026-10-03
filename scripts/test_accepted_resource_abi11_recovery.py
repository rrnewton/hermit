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

import accepted_resource_abi11_recovery as ar
import accepted_resource_recovery as legacy
import test_network_recovery as old_fixture

nr = ar.nr
LABEL = old_fixture.LABEL


class Fixture(old_fixture.PrivateFixture):
    def root(self):
        # Use the production inspector's verified dependency instance.
        return nr.HeldRoot(self.path, os.getuid(), 384)

    def accepted(self):
        names, rows, service, artifact = super().accepted()
        # Explicit controlled ABI11 original inventory/package premise.
        artifact["maps"] = 25
        artifact["wire_format"] = "abi11-copy5"
        artifact["topology"]["contract_sha256"] = list(bytes.fromhex(ar.CONTRACT_CANONICAL))
        service[0]["inventories"][0]["ids"].append({"kind": 0, "id": 25})
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
        self.assertEqual(len(result["ids"]), 123)
        self.assertEqual([sum(row[1] == kind for row in result["ids"]) for kind in range(3)], [25, 49, 49])
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
        self.assertEqual(len(self.read_source()["ids"]), 123)

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
        self.assertEqual(result["ids_per_pass"], 123)
        self.assertEqual(result["passes"], 2)
        data = (self.f.path / self.names[0]).read_bytes()
        self.assertTrue(data.startswith(prefix))
        rows = data.splitlines(keepends=True)
        self.assertEqual(len(rows), 4)
        intent, proof = [nr.decode(row) for row in rows[2:]]
        self.assertEqual(proof[1], nr.digest(rows[2]))
        self.assertEqual(len(proof[7]), 2)
        self.assertTrue(all(len(scan[2]) == 123 for scan in proof[7]))
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
        self.assertEqual(nr.decode(data.splitlines()[2])[0], "hermit-accepted-abi11-resource-intent-v1")
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



class Abi11ProtocolTests(unittest.TestCase):
    setUp = InspectorTests.setUp
    census = InspectorTests.census
    read_source = InspectorTests.read_source

    def test_distinct_closed_protocol_and_exact_contract(self):
        original = self.census()
        root = self.f.root()
        try:
            with self.assertRaises(legacy.Refused):
                legacy.source_original(root, LABEL, self.artifact)
        finally:
            root.close()
        self.assertEqual(original, self.census())
        for wire in ("abi8-copy5", "abi9-copy5", "abi10-copy5", "abi11-copy4", "abi11-copy6"):
            artifact = copy.deepcopy(self.artifact)
            artifact["wire_format"] = wire
            with self.subTest(wire=wire), self.assertRaisesRegex(nr.Refused, "topology"):
                ar.artifact_valid(artifact)
        for counts in ((24, 49, 49), (26, 49, 49), (25, 48, 50), (25.0, 49, 49), (True, 49, 49)):
            artifact = copy.deepcopy(self.artifact)
            artifact.update(zip(("maps", "programs", "links"), counts))
            with self.subTest(counts=counts), self.assertRaisesRegex(nr.Refused, "topology"):
                ar.artifact_valid(artifact)
        artifact = copy.deepcopy(self.artifact)
        artifact["topology"]["contract_sha256"] = [9] * 32
        with self.assertRaisesRegex(nr.Refused, "ABI11 contract"):
            ar.artifact_valid(artifact)

    def test_fixed_wire_arrays_and_original_bounds(self):
        inspected, result = controlled_certificate(self.f, self.artifact)
        rows = (self.f.path / self.names[0]).read_bytes().splitlines(keepends=True)
        intent, proof = [nr.decode(row) for row in rows[2:]]
        self.assertEqual(inspected["schema"], "hermit-accepted-abi11-resource-inspection-v1")
        self.assertEqual(inspected["plan"][0], "hermit-accepted-abi11-resource-plan-v1")
        self.assertEqual(result["schema"], "hermit-accepted-abi11-resource-certification-v1")
        self.assertEqual(len(inspected["plan"]), 8)
        self.assertEqual(len(intent), 12)
        self.assertEqual(len(proof), 12)
        self.assertEqual(intent[0], "hermit-accepted-abi11-resource-intent-v1")
        self.assertEqual(proof[0], "hermit-accepted-abi11-resource-result-v1")
        self.assertEqual(intent[11], "incomplete-parent-after-provider-close")
        self.assertEqual(intent[3] - intent[2], 5_000_000_000)
        self.assertEqual(proof[4] - proof[3], 1_000_000_000)
        self.assertEqual(intent[7][5], ["abi11-copy5", ar.CONTRACT_CANONICAL,
                                      *([bytes([k] * 32).hex() for k in (1, 2, 3)]), 25, 49, 49])
        self.assertEqual(intent[6], ar.DEPENDENCY_SHA256)
        self.assertEqual(proof[11], ar.DEPENDENCY_SHA256)
        self.assertEqual(intent[10], nr.digest(nr.wire(intent[9])[:-1]))
        self.assertEqual(proof[1], nr.digest(rows[2]))
        self.assertEqual(len(intent[9]), 123)
        for scan in proof[7]:
            self.assertEqual(scan[2], [[*row, errno.ENOENT] for row in intent[9]])
        self.assertFalse(result["execution_success"])
        self.assertFalse(result["normal_terminal_success"])

    def test_actual_wait_and_each_scan_answer_are_required(self):
        # Every mutant starts from a real fork/scanner wait with controlled ENOENT.
        # Only the returned proof is corrupted; none can append a result.
        mutations = [
            lambda p: p.__setitem__(1, 256),
            lambda p: p[0].__setitem__(0, 0),
            lambda p: p[0].__setitem__(1, 0),
            lambda p: p[0].__setitem__(2, 123),
            lambda p: p[0].__setitem__(3, 123),
            lambda p: p[0].__setitem__(4, "0" * 64),
            lambda p: p[0].__setitem__(5, "f" * 64),
            lambda p: p[2].pop(),
            lambda p: p[2][0][2].pop(),
            lambda p: p[2][1][2].append(p[2][1][2][0]),
            lambda p: p[2][1][2][0].__setitem__(0, 1),
            lambda p: p[2][1][2][0].__setitem__(1, 2),
            lambda p: p[2][1][2][0].__setitem__(2, 9999),
            lambda p: p[2][1][2][0].__setitem__(3, errno.EPERM),
            lambda p: p[2][1].__setitem__(1, 2**64 - 1),
        ]
        scanner = nr.run_scanner
        for index, mutation in enumerate(mutations):
            fixture = Fixture()
            try:
                names, _, _, artifact = fixture.accepted()
                prefix = (fixture.path / names[0]).read_bytes()
                args = arguments(fixture)
                def corrupt(*values):
                    proof = copy.deepcopy(list(scanner(*values)))
                    mutation(proof)
                    return proof
                with self.subTest(index=index), controlled_platform(artifact):
                    inspected = ar.inspect(args)
                    args.action = "certify"
                    args.expect_inspection_sha256 = inspected["plan_sha256"]
                    with mock.patch.object(nr, "run_scanner", side_effect=corrupt), self.assertRaises(nr.Refused):
                        ar.inspect(args)
                data = (fixture.path / names[0]).read_bytes()
                self.assertTrue(data.startswith(prefix))
                self.assertEqual(len(data.splitlines()), 3)
                self.assertEqual(nr.decode(data.splitlines()[2])[0], "hermit-accepted-abi11-resource-intent-v1")
            finally:
                fixture.temp.cleanup()


class Abi11PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.package = Path(self.temp.name)
        self.contract_path = Path(ar.__file__).resolve().parents[1] / "hermit-cli/network-provider/accepted-contract.json"
        self.contract = nr.decode(self.contract_path.read_bytes())
        self.manifest = {key: self.contract[key] for key in
                         ("schema", "abi_version", "copy_version", "maps", "programs", "links",
                          "btf_sha256", "grouped_event", "ftrace_only")}
        self.manifest.update(kind="hermit-accepted-provider", sources={"accepted-contract.json": ar.CONTRACT_RAW})
        for role, name in (("object", "accepted-provider.bpf.o"), ("library", "libhermit_accepted_provider.so")):
            data = ("controlled offline " + role).encode()
            (self.package / name).write_bytes(data)
            self.manifest[role] = name
            self.manifest[role + "_sha256"] = hashlib.sha256(data).hexdigest()
        self.write_manifest(self.manifest)

    def write_manifest(self, value):
        (self.package / "manifest.json").write_text(json.dumps(value) + "\n")

    @contextlib.contextmanager
    def kernel_premise(self, *, btf_matches=True, contract_bytes=None):
        # Only running-BTF identity is supplied. Manifest, object and DSO reads/hashes
        # are real bounded O_NOFOLLOW reads. No native provider is loaded.
        bounded, digest = nr.bounded_regular, nr.digest
        btf = b"explicit controlled running BTF identity"
        def read(path, limit):
            if Path(path) == Path("/sys/kernel/btf/vmlinux"):
                return btf, None
            if contract_bytes is not None and Path(path) == self.contract_path:
                return contract_bytes, None
            return bounded(path, limit)
        def sha(data):
            if data is btf:
                return self.contract["btf_sha256"] if btf_matches else "0" * 64
            return digest(data)
        with mock.patch.object(nr, "bounded_regular", side_effect=read), \
                mock.patch.object(nr, "digest", side_effect=sha):
            yield

    def test_exact_package_real_files_and_closed_contract(self):
        with self.kernel_premise():
            artifact = ar.expected_package(self.package)
        self.assertEqual(ar.artifact_fields(artifact),
                         ["abi11-copy5", ar.CONTRACT_CANONICAL,
                          self.manifest["object_sha256"], self.manifest["library_sha256"],
                          self.contract["btf_sha256"], 25, 49, 49])
        for key, value in (("abi_version", "4150525553540009"), ("abi_version", "415052555354000a"),
                           ("copy_version", 4), ("copy_version", 5.0), ("schema", True),
                           ("maps", 24), ("maps", 25.0), ("programs", 48), ("links", 50),
                           ("ftrace_only", 1), ("grouped_event", {}), ("kind", "foreign"),
                           ("object", "../accepted-provider.bpf.o"), ("library", "foreign.so"),
                           ("object_sha256", "0" * 64), ("library_sha256", "0" * 64),
                           ("btf_sha256", "0" * 64), ("sources", {})):
            mutated = copy.deepcopy(self.manifest)
            mutated[key] = value
            self.write_manifest(mutated)
            with self.subTest(key=key, value=value), self.kernel_premise(), self.assertRaises(nr.Refused):
                ar.expected_package(self.package)
        for key in ("schema", "abi_version", "copy_version", "maps", "programs", "links", "btf_sha256",
                    "grouped_event", "ftrace_only", "sources"):
            mutated = copy.deepcopy(self.manifest)
            del mutated[key]
            self.write_manifest(mutated)
            with self.subTest(missing=key), self.kernel_premise(), self.assertRaises(nr.Refused):
                ar.expected_package(self.package)

    def test_changed_kernel_contract_artifact_and_symlink_refuse(self):
        with self.kernel_premise(btf_matches=False), self.assertRaisesRegex(nr.Refused, "kernel BTF"):
            ar.expected_package(self.package)
        with self.kernel_premise(contract_bytes=self.contract_path.read_bytes() + b" "), \
                self.assertRaisesRegex(nr.Refused, "contract bytes"):
            ar.expected_package(self.package)
        obj = self.package / self.manifest["object"]
        original = obj.read_bytes()
        obj.write_bytes(original + b"changed")
        with self.kernel_premise(), self.assertRaisesRegex(nr.Refused, "artifact bytes"):
            ar.expected_package(self.package)
        obj.unlink()
        obj.symlink_to(self.package / self.manifest["library"])
        with self.kernel_premise(), self.assertRaises(OSError):
            ar.expected_package(self.package)

    def test_producer_sorted_object_keys_preserve_exact_package(self):
        # The real package producer sorts every JSON object's keys; the source
        # contract has its own insertion order. Arrays and scalar types stay exact.
        (self.package / "manifest.json").write_text(json.dumps(self.manifest, sort_keys=True) + "\n")
        with self.kernel_premise():
            artifact = ar.expected_package(self.package)
        self.assertEqual(ar.artifact_fields(artifact),
                         ["abi11-copy5", ar.CONTRACT_CANONICAL,
                          self.manifest["object_sha256"], self.manifest["library_sha256"],
                          self.contract["btf_sha256"], 25, 49, 49])

    def test_sorted_nested_types_values_and_array_order_still_refuse(self):
        mutations = (
            lambda group: group.update(version=2.0),
            lambda group: group.update(cookie=False),
            lambda group: group["sites"][0].update(role=True),
            lambda group: group["sites"][0].update(offset=29),
            lambda group: group["sites"].reverse(),
            lambda group: group["receive_entry"].reverse(),
            lambda group: group["receive_return"].reverse(),
        )
        for index, mutate in enumerate(mutations):
            value = copy.deepcopy(self.manifest)
            mutate(value["grouped_event"])
            (self.package / "manifest.json").write_text(json.dumps(value, sort_keys=True) + "\n")
            with self.subTest(index=index), self.kernel_premise(), self.assertRaises(nr.Refused):
                ar.expected_package(self.package)


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
