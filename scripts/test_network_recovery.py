#!/usr/bin/env python3
"""Controlled recovery tests; no live BPF mutation or privileged execution.

``--fixture NEW_ABSOLUTE_DIRECTORY`` exports the same production-writer
fixture for the Rust admission reader. Privilege, BPF and actor observations
remain explicit substituted premises, never native recovery evidence.
``--resume-fixture NEW_ABSOLUTE_DIRECTORY`` additionally exercises the pending
failed-scan and new append-only continuation paths under those same premises.
"""
import contextlib
import copy
import ctypes
import errno
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock

import network_recovery as nr


LABEL = "1234567890abcdef1234567890abcdef"
INC = int(LABEL[:16], 16)


def encoded(values):
    return b"".join(json.dumps(value).encode() + b"\n" for value in values)


def journal_rows():
    rows = []

    def add(phase, seq=0, kind=0, ident=0, error=0, name=b""):
        rows.append([0x554750494E303031, INC, len(rows) + 1, seq, 11, 22,
                     3, phase, kind, ident, error, 0, name])

    add(1)
    for kind, count, prefix, first in ((0, 10, "m", 100), (1, 31, "l", 300)):
        for i in range(count):
            for phase in (2, 3):
                add(phase, kind=kind, ident=first + i, name=f"{prefix}{i:02}".encode())
    for phase, seq in ((4, 0), (5, 4), (6, 4), (11, 5), (7, 4), (7, 4), (8, 2), (9, 2)):
        add(phase, seq)
    add(10, 6, error=errno.EPROTO)
    for kind, count, first in ((0, 10, 100), (1, 31, 200), (2, 31, 300)):
        for i in range(count):
            add(18, 7, kind, first + i)
    add(12, 7)
    add(10, 7, error=errno.EPIPE)
    return rows


def packed(values):
    return b"".join(nr.JOURNAL.pack(*r) for r in values)


class PrivateFixture:
    def __init__(self, path=None):
        self.temp = tempfile.TemporaryDirectory() if path is None else None
        self.path = Path(self.temp.name) if self.temp else path
        if self.temp is None:
            self.path.mkdir(mode=0o700)
        self.path.chmod(0o700)

    def write(self, name, data):
        path = self.path / name
        path.write_bytes(data)
        path.chmod(0o600)
        return path

    def root(self):
        return nr.HeldRoot(self.path, os.getuid(), 384)

    def accepted(self):
        names = [f"accepted-b1-{LABEL}.{role}" for role in ("terminal.jsonl", "stdout.log", "stderr.log")]
        for name in names:
            self.write(name, b"")
        artifact = {"maps": 24, "programs": 49, "links": 49, "wire_format": "abi9-copy5",
                    "topology": {"kind": "ftrace-v1", "contract_sha256": [9] * 32},
                    "object_sha256": [1] * 32, "library_sha256": [2] * 32, "btf_sha256": [3] * 32}
        run = bytes(range(1, 17))
        actors = {}
        for key, prefix in (("loader", "hermit-accepted-"), ("query", "hermit-accepted-readback-")):
            unit = prefix + run.hex() + ".service"
            actors[key] = {"unit": unit, "invocation": LABEL if key == "loader" else "2" * 32,
                           "device": 9, "inode": 12 if key == "loader" else 13,
                           "cgroup": "/sys/fs/cgroup/system.slice/" + unit}
        records = [
            {"schema": 1, "stage": "accepted_before_launch", "label": LABEL,
             "root": nr.identity(self.path.stat()), "files": [nr.identity((self.path / n).stat()) for n in names],
             "artifact": artifact},
            {"schema": 1, "stage": "accepted_started", "label": LABEL,
             "observed": {"schema": "hermit-accepted-parent-startup-v1", "artifact": artifact,
                          "run": list(run), **actors}},
            {"schema": 1, "stage": "accepted_failed", "label": LABEL, "error": "wrapper exit 125"},
        ]
        ids = [{"kind": k, "id": k * 100 + i + 1} for k, count in enumerate((24, 49, 49)) for i in range(count)]
        inv = {"complete": True, "count_invalid": False, "ids": ids,
               "status": {"returned": 0, "errno": None, "operation": "ap_identifiers"}}
        shared = {"schema": "hermit-accepted-provider-terminal-v1", "run": list(run),
                  "controller_terminal": True, "requires_external_absence": True}
        service = [
            {**shared, "phase": "before_close", "failure": None, "inventories": [inv], "fd_journal_unretired": 728},
            {**shared, "phase": "after_close", "service_status": 125, "close_error": None,
             "socket_release": "pending_process_exit", "closed_ns": 12345,
             "close_receipts": [{"incarnation": int.from_bytes(run[:8], "little"),
                                 "close": {"returned": 0, "errno": None, "operation": "ap_close"},
                                 "unexpected_drop": False, "requires_external_absence": True, "inventory": inv}]},
        ]
        self.write(names[0], encoded(records))
        self.write(names[1], encoded(service))
        return names, records, service, artifact

    def unix(self):
        unit = "hermit-unix-" + LABEL + ".service"
        records = [{"schema": 1, "stage": "before_launch", "incarnation": INC, "loader_unit": unit},
                   {"schema": 1, "stage": "terminal_failed", "units": [unit, None], "ids": None,
                    "child_wait": "Exited(125)", "child_pid": 123, "error": "original failure"}]
        for role, data in (("terminal.jsonl", encoded(records)), ("stdout.log", b""), ("stderr.log", b"")):
            self.write(f"guard-b1-{LABEL}.{role}", data)
        self.write("ugb1-" + LABEL[:16], packed(journal_rows()))


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.f = PrivateFixture()
        self.addCleanup(self.f.temp.cleanup)

    def opened(self):
        root = self.f.root()
        self.addCleanup(root.close)
        return root

    def test_accepted_preserves_failure_full_inventory_and_file_custody(self):
        _, _, _, artifact = self.f.accepted()
        root = self.opened()
        result = nr.inspect_accepted(root, LABEL, artifact)
        self.assertEqual(len(result["ids"]), 122)
        self.assertEqual(result["service_status"], 125)
        self.assertEqual(result["fd_journal_unretired"], 728)
        self.assertEqual(result["failure"], "wrapper exit 125")
        root.recheck()

    def test_accepted_rejects_mutated_original_identity_inventory_and_close(self):
        names, records, service, artifact = self.f.accepted()
        variants = []
        for operation in (
            lambda r, s: r[0]["root"].update(device=0),
            lambda r, s: r[0]["files"][1].update(inode=0),
            lambda r, s: r[1]["observed"]["loader"].update(unit="foreign.service"),
            lambda r, s: r[1]["observed"]["query"].update(invocation=LABEL),
            lambda r, s: r[1]["observed"]["query"].update(device=9, inode=12),
            lambda r, s: r[1]["observed"].update(run=[0] * 16),
            lambda r, s: r[2].update(stage="accepted_terminal"),
            lambda r, s: s[0].update(controller_terminal=False),
            lambda r, s: s[0]["inventories"][0]["ids"].pop(),
            lambda r, s: s[0]["inventories"][0]["ids"].__setitem__(1, s[0]["inventories"][0]["ids"][0]),
            lambda r, s: s[1]["close_receipts"][0]["close"].update(returned=-1),
            lambda r, s: s[1]["close_receipts"][0].update(unexpected_drop=True),
            lambda r, s: s[1].update(service_status=0),
        ):
            r, s = copy.deepcopy(records), copy.deepcopy(service)
            operation(r, s)
            variants.append((r, s))
        for index, (r, s) in enumerate(variants):
            with self.subTest(index=index):
                self.f.write(names[0], encoded(r)); self.f.write(names[1], encoded(s))
                root = self.f.root()
                try:
                    with self.assertRaises(nr.Refused):
                        nr.inspect_accepted(root, LABEL, artifact)
                finally:
                    root.close()

    def test_unix_exact_failure_and_same_incarnation_ambiguity(self):
        self.f.unix()
        root = self.f.root()
        try:
            result = nr.inspect_unix(root, LABEL)
            self.assertEqual(result["journal"]["rows"], 166)
            self.assertEqual(len(result["journal"]["ids"]), 72)
        finally:
            root.close()
        self.f.write(f"guard-b1-{LABEL[:16]}0000000000000001.stdout.log", b"")
        with self.assertRaisesRegex(nr.Refused, "ambiguous"):
            nr.inspect_unix(self.opened(), LABEL)

    def test_held_files_reject_symlink_hardlink_fifo_permissions_and_growth(self):
        regular = self.f.write("regular", b"old")
        (self.f.path / "symlink").symlink_to(regular)
        os.link(regular, self.f.path / "hardlink")
        os.mkfifo(self.f.path / "fifo", 0o600)
        self.f.write("public", b"bad").chmod(0o644)
        self.f.write("oversize", b"12345")
        root = self.opened()
        for name in ("regular", "symlink", "hardlink", "fifo", "public", "oversize"):
            with self.subTest(name=name), self.assertRaises((nr.Refused, OSError)):
                root.read(name, 4)

    def test_double_census_detects_in_place_rewrite_replacement_and_unknown_role(self):
        path = self.f.write("owned", b"abc")
        root = self.opened()
        held = root.read("owned", 10)
        path.write_bytes(b"xyz")
        with self.assertRaises(nr.Refused): held.recheck()
        path.unlink(); self.f.write("owned", b"abc")
        with self.assertRaises(nr.Refused): held.recheck()
        self.f.write("unknown", b"")
        with self.assertRaises(nr.Refused): root.recheck()

    def test_unknown_role_extra_recovery_row_and_duplicate_json_key_refuse(self):
        names, records, _, artifact = self.f.accepted()
        records.append({"stage": "resource_recovery_intent"})
        self.f.write(names[0], encoded(records))
        root = self.opened()
        with self.assertRaises(nr.Refused): nr.inspect_accepted(root, LABEL, artifact)
        with self.assertRaises(nr.Refused): nr.decode(b'{"stage":"failed","stage":"terminal"}')
        with self.assertRaises(nr.Refused): nr.decode(b'{"count":NaN}')
        with self.assertRaises(nr.Refused): nr.typed_ids([{"kind": 0, "id": True}], [1, 0, 0])

    def test_root_lock_and_population_bound(self):
        self.opened()
        with self.assertRaises(BlockingIOError): self.f.root()
        other = PrivateFixture()
        self.addCleanup(other.temp.cleanup)
        other.write("one", b"")
        with self.assertRaises(nr.Refused): nr.HeldRoot(other.path, os.getuid(), 0)
        with tempfile.TemporaryDirectory() as temporary:
            alias = Path(temporary) / "alias"
            alias.symlink_to(other.path)
            with self.assertRaises(nr.Refused): nr.HeldRoot(alias, os.getuid(), 384)


class JournalTests(unittest.TestCase):
    def test_missing_truncated_reordered_and_wrong_typed_originals_refuse(self):
        source = journal_rows()
        self.assertEqual(nr.parse_journal(packed(source), LABEL)["initial_sequences"], [2])
        variants = [b"", packed(source)[:-1], packed(source) + b"\0"]
        for row, field, value in ((0, 0, 0), (1, 1, INC + 1), (3, 2, 2), (8, 4, 999),
                                  (9, 6, 4), (10, 11, 1), (11, 7, 13), (12, 9, 0),
                                  (13, 12, b"foreign"), (90, 3, 3), (92, 8, 3),
                                  (92, 9, 999), (164, 7, 17)):
            changed = copy.deepcopy(source); changed[row][field] = value
            variants.append(packed(changed))
        for index, data in enumerate(variants):
            with self.subTest(index=index), self.assertRaises(nr.Refused):
                nr.parse_journal(data, LABEL)


class FakeNative:
    SHAPES = {"m00": (2, 4, 16, 1, "ug_config"), "m01": (2, 4, 112, 1, "ug_status"),
              "m05": (1, 8, 24, 4096, "ug_sockets"), "m06": (1, 8, 32, 128, "ug_namespaces"),
              "m08": (1, 8, 16, 256, "ug_initial_task")}

    def __init__(self):
        self.shapes = copy.deepcopy(self.SHAPES)
        self.config = (INC, 1, 3)
        self.status = [0, 0, 1024, 0, 0, 0, 2, 2, INC, 100, 200, 2, 0, 0, 19, 4]
        self.tables = {"m05": {}, "m06": {}, "m08": {2: struct.pack("<QQ", INC, 3)}}
        self.read_flags = []

    def info(self, fd, kind):
        return dict(zip(("type", "key_size", "value_size", "max_entries", "name"), self.shapes[fd]), flags=0)

    def lookup(self, fd, key, size, flags=0):
        self.read_flags.append((fd, flags))
        return struct.pack("<QII", *self.config) if fd == "m00" else struct.pack("<II12QII", *self.status)

    def entries(self, fd, size, limit):
        return self.tables[fd]


class NativeControls(unittest.TestCase):
    def test_current_manager_command_must_match_fragment_exact_package_and_roots(self):
        helper = "/known/package/hermit-unix-keeper"
        package = {"helper_path": helper, "helper_sha256": "a" * 64}
        fragment = ('[Service]\nUser=123\nReadWritePaths="/known/pins" "/known/recovery"\n'
                    f'ExecStart=\nExecStart=:"{helper}" "--bootstrap-deadline-ns" "1234567"\n').encode()
        command = (f"{{ path={helper} ; argv[]={helper} --bootstrap-deadline-ns 1234567 ; ignore_errors=no ; "
                   "start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }")
        properties = {"ExecStart": command}
        proof = nr.unit_command_binding(fragment, properties, package, 123, "/known/pins", "/known/recovery")
        self.assertEqual(proof["argv"], [helper, "--bootstrap-deadline-ns", "1234567"])
        self.assertFalse(proof["historical_artifact_custody"])
        mutations = [
            (fragment.replace(b"/known/package", b"/foreign/package"), properties, package),
            (fragment.replace(b"1234567", b"1234568"), properties, package),
            (fragment.replace(b"User=123", b"User=0"), properties, package),
            (fragment.replace(b"/known/pins", b"/foreign/pins"), properties, package),
            (fragment + b'ExecStart="/extra/helper"\n', properties, package),
            (fragment, {"ExecStart": command.replace("1234567", "1234568")}, package),
            (fragment, {"ExecStart": command.replace("ignore_errors=no", "ignore_errors=yes")}, package),
            (fragment, {"ExecStart": command + command}, package),
            (fragment, properties, {**package, "helper_path": "/different/helper"}),
        ]
        for index, (data, current, expected) in enumerate(mutations):
            with self.subTest(index=index), self.assertRaises(nr.Refused):
                nr.unit_command_binding(data, current, expected, 123, "/known/pins", "/known/recovery")

    def test_actual_state_consumer_requires_fault_class_empty_tables_and_terminal_ledger(self):
        journal = nr.parse_journal(packed(journal_rows()), LABEL)
        native = FakeNative()
        proof = nr.check_unix_state(native, {key: key for key in native.SHAPES}, journal)
        self.assertTrue(proof["sockets_empty"])
        self.assertIn(("m01", 4), native.read_flags)
        mutations = [lambda n: n.tables["m05"].update({1: bytes(24)}),
                     lambda n: n.tables["m06"].update({1: bytes(32)}),
                     lambda n: n.tables["m08"].clear(),
                     lambda n: n.tables["m08"].update({2: struct.pack("<QQ", INC, 2)}),
                     lambda n: setattr(n, "config", (INC + 1, 1, 3)),
                     lambda n: n.shapes.update(m01=(2, 4, 104, 1, "ug_status"))]
        for index, value in ((2, 0), (2, 1025), (3, 1), (4, 1), (5, 1), (8, INC + 1),
                             (11, 3), (12, 1), (13, 1), (14, 18), (15, 3)):
            mutations.append(lambda n, i=index, v=value: n.status.__setitem__(i, v))
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                candidate = FakeNative(); mutate(candidate)
                with self.assertRaises(nr.Refused):
                    nr.check_unix_state(candidate, {key: key for key in candidate.SHAPES}, journal)

    def test_enumeration_accepts_only_enoent_and_refuses_duplicate_or_excess_keys(self):
        native = nr.NativeRead()
        native.call = mock.Mock(side_effect=OSError(errno.ENOENT, "end"))
        self.assertEqual(native.entries(7, 24, 3), {})
        for code in (errno.EACCES, errno.EIO, errno.EBADF):
            native.call = mock.Mock(side_effect=OSError(code, "not absence"))
            with self.assertRaises(OSError): native.entries(7, 24, 3)

        def duplicate(command, fields):
            self.assertEqual(command, 4)
            address = struct.unpack_from("<Q", fields, 16)[0]
            ctypes.c_uint64.from_address(address).value = 9
            return 0
        native.call = duplicate
        native.lookup = mock.Mock(return_value=bytes(24))
        with self.assertRaises(nr.Refused): native.entries(7, 24, 3)
        with self.assertRaises(nr.Refused): nr.NativeRead().call(2, b"")

    def test_current_manager_proof_never_substitutes_for_retained_unix_identity(self):
        properties = {"Id": "hermit-unix-" + LABEL + ".service", "LoadState": "loaded",
                      "ActiveState": "failed", "SubState": "failed", "MainPID": "0",
                      "ControlGroup": "", "ExecMainCode": "1", "ExecMainStatus": "125",
                      "InvocationID": LABEL, "ExecMainPID": "100", "ExecMainStartTimestampMonotonic": "10",
                      "ExecMainExitTimestampMonotonic": "20"}
        with mock.patch.object(nr.os.path, "lexists", return_value=False):
            nr.actor_gone(properties, unix=True)
            for key, value in (("LoadState", "not-found"), ("ActiveState", "active"), ("MainPID", "100"),
                               ("InvocationID", ""), ("ExecMainStatus", "0"), ("ExecMainExitTimestampMonotonic", "0")):
                changed = {**properties, key: value}
                with self.subTest(key=key), self.assertRaises(nr.Refused): nr.actor_gone(changed, unix=True)
            with self.assertRaises(nr.Refused): nr.actor_gone(properties, {"invocation": "f" * 32})
        with mock.patch.object(nr.os.path, "lexists", return_value=True), self.assertRaises(nr.Refused):
            nr.actor_gone(properties, unix=True)

    def test_real_child_output_and_time_bounds_and_no_apply_mode(self):
        self.assertEqual(nr.bounded_command([sys.executable, "-c", "print('bounded')"], nr.Deadline()),
                         (b"bounded\n", b""))
        with self.assertRaises(nr.Refused):
            nr.bounded_command([sys.executable, "-c", "import os;os.write(1,b'x'*9000)"], nr.Deadline())
        with self.assertRaises(nr.Refused):
            nr.bounded_command([sys.executable, "-c", "import time;time.sleep(2)"], nr.Deadline(.02))
        child = subprocess.run([sys.executable, nr.__file__, "apply"], capture_output=True, timeout=5)
        self.assertEqual(child.returncode, 2)
        self.assertIn(b"invalid choice", child.stderr)


class AbsentNative:
    def by_id(self, kind, ident):
        raise OSError(errno.ENOENT, "controlled original object absence")


@contextlib.contextmanager
def recovery_fixture(base):
    """Actual temporary-file evidence; controlled privilege/BPF/actor premises."""
    with contextlib.ExitStack() as stack:
        fixtures = [PrivateFixture(base / name) for name in ("accepted", "unix", "pins")]
        accepted_fixture, unix_fixture, pin_fixture = fixtures
        _, _, _, artifact = accepted_fixture.accepted()
        unix_fixture.unix()
        leaf = pin_fixture.path / ("ugb1-" + LABEL[:16]); leaf.mkdir(mode=0o700)
        rows = journal_rows()
        for row in rows: row[4], row[5] = leaf.stat().st_dev, leaf.stat().st_ino
        unix_fixture.write("ugb1-" + LABEL[:16], packed(rows))
        journal = nr.parse_journal(packed(rows), LABEL)
        for name, (_, ident) in journal["pins"].items():
            path = leaf / name; path.write_text(str(ident)); path.chmod(0o600)
        roots = {str(f.path): f.root() for f in fixtures}
        for root in roots.values(): stack.callback(root.close)
        args = SimpleNamespace(accepted_root=str(accepted_fixture.path), unix_root=str(unix_fixture.path),
                               pin_root=str(pin_fixture.path), owner_uid=os.getuid(), expect_inspection_sha256=None,
                               accepted_label=LABEL, unix_label=LABEL,
                               accepted_package=str(base / "accepted-package"), unix_package=str(base / "package"),
                               expect_intent_sha256=None)
        accepted = nr.inspect_accepted(roots[args.accepted_root], LABEL, artifact)
        unix = nr.inspect_unix(roots[args.unix_root], LABEL)
        pin_fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY); stack.callback(os.close, pin_fd)
        pin_stats = {name: nr.identity((leaf / name).stat()) for name in journal["pins"]}
        actors = {a["unit"]: {"Id": a["unit"], "LoadState": "not-found"} for a in accepted["actors"].values()}
        helper = str(base / "package" / "hermit-unix-keeper")
        fragment_path = "/run/systemd/transient/" + unix["loader_unit"]
        command = (f"{{ path={helper} ; argv[]={helper} --bootstrap-deadline-ns 99 ; ignore_errors=no ; "
                   "start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }")
        actors[unix["loader_unit"]] = {
            "Id": unix["loader_unit"], "LoadState": "loaded", "InvocationID": LABEL,
            "ActiveState": "failed", "SubState": "failed", "MainPID": "0", "ControlGroup": "",
            "ExecMainCode": "1", "ExecMainStatus": "125", "ExecMainPID": "100",
            "ExecMainStartTimestampMonotonic": "10", "ExecMainExitTimestampMonotonic": "20",
            "FragmentPath": fragment_path, "ExecStart": command,
        }
        # This is a modeled root-owned manager fragment, not a new real systemd
        # unit or evidence of historical PIDFD custody. The production actor
        # sampler is replaced explicitly when recover() is invoked below.
        fragment_bytes = (f'[Service]\nUser={os.getuid()}\n'
                          f'ReadWritePaths="{pin_fixture.path}" "{unix_fixture.path}"\n'
                          f'ExecStart=\nExecStart=:"{helper}" "--bootstrap-deadline-ns" "99"\n').encode()
        fragment = {"path": fragment_path, "device": 1, "inode": 2, "mode": 0o100644,
                    "uid": 0, "nlink": 1, "bytes": len(fragment_bytes), "mtime_ns": 1, "ctime_ns": 1,
                    "sha256": nr.digest(fragment_bytes),
                    "command": {"path": helper, "helper_sha256": "c" * 64,
                                "argv": [helper, "--bootstrap-deadline-ns", "99"]}}
        plan = {"accepted": accepted, "unix": unix, "pin_root": roots[args.pin_root].identity,
                "pin_directory": nr.identity(os.fstat(pin_fd)), "current_actors": actors,
                "retained_unix_manager_fragment": fragment,
                "tool_sha256": nr.digest(Path(nr.__file__).read_bytes()), "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
        plan["current_unix_package"] = {"helper_path": helper, "helper_sha256": "c" * 64}
        args.expect_inspection_sha256 = nr.digest(json.dumps(plan, sort_keys=True, indent=2).encode() + b"\n")
        prefixes = [next(f for f in roots[p].files if f.name.endswith("terminal.jsonl")).data
                    for p in (args.accepted_root, args.unix_root)]
        owners = [nr.ObjectOwner(os.open("/dev/null", os.O_RDONLY)) for _ in range(2)]
        for owner in owners: stack.callback(owner.close)

        class PinNative(FakeNative):
            def pin(self, directory, name): return os.open(name, os.O_RDONLY, dir_fd=directory)
            def info(self, descriptor, kind):
                if isinstance(descriptor, str): return super().info(descriptor, kind)
                return {"id": int(os.pread(descriptor, 16, 0))}
        native = PinNative()
        yield SimpleNamespace(args=args, plan=plan, roots=roots, pin_fd=pin_fd,
                              leaf=leaf, native=native, pin_stats=pin_stats,
                              owners=owners, prefixes=prefixes, artifact=artifact,
                              fragment_bytes=fragment_bytes)


def controlled_recover(fixture):
    # Actual writer, append, descriptor-relative unlink, fork, pipe and waitpid.
    # These three substituted authorities must not be read as native evidence.
    with mock.patch.object(nr.os, "getuid", return_value=0), \
         mock.patch.object(nr.os, "geteuid", return_value=0), \
         mock.patch.object(nr, "NativeRead", AbsentNative), \
         mock.patch.object(nr, "recheck_actors"):
        return nr.recover(fixture.args, fixture.plan, fixture.roots, fixture.pin_fd,
                          fixture.leaf.name, fixture.native,
                          {k: k for k in fixture.native.SHAPES}, fixture.pin_stats, fixture.owners)


def export_fixture(destination):
    base = Path(destination)
    nr.require(base.is_absolute() and str(base) == str(base.resolve()), "fixture destination must be canonical absolute")
    # Never reuse a path: this mode has no authority over existing evidence.
    base.mkdir(mode=0o700)
    with recovery_fixture(base) as fixture:
        result = controlled_recover(fixture)
        metadata = {"schema": "hermit-controlled-resource-fixture-v1",
                    "native_recovery_evidence": False,
                    "substituted_premises": ["privilege", "BPF identity/state/absence", "actor observations"],
                    "accepted_root": fixture.args.accepted_root, "unix_root": fixture.args.unix_root,
                    "pin_root": fixture.args.pin_root, "accepted_label": LABEL, "unix_label": LABEL,
                    "producer_sha256": fixture.plan["tool_sha256"], "result": result}
        (base / "fixture.json").write_bytes(nr.wire(metadata))
        (base / "inspection.json").write_bytes(nr.wire(fixture.plan))
        (base / "controlled-fragment.txt").write_bytes(fixture.fragment_bytes)
        return metadata


def pending_fixture(base):
    """Real failed orchestration, with explicitly modeled old source/time."""
    with recovery_fixture(base) as fixture:
        fixture.plan["tool_sha256"] = nr.RESUME_PREDECESSOR
        fixture.args.expect_inspection_sha256 = nr.digest(
            json.dumps(fixture.plan, sort_keys=True, indent=2).encode() + b"\n")
        clock = nr.time.monotonic_ns
        # Model a completed old five-second action interval without sleeping or
        # changing the continuation's actual clock. The failed attempt still
        # uses real temporary-file unlinks and actual object-owner closes.
        with mock.patch.object(nr.time, "monotonic_ns", side_effect=lambda: clock() - 6_000_000_000), \
             mock.patch.object(nr, "run_scanner", side_effect=nr.Refused("controlled first scanner failure")):
            try:
                controlled_recover(fixture)
            except nr.Refused as error:
                nr.require(str(error) == "controlled first scanner failure", "fixture failed before scanner")
            else:
                raise AssertionError("pending fixture unexpectedly completed")
        terminal = Path(fixture.args.accepted_root) / f"accepted-b1-{LABEL}.terminal.jsonl"
        raw = terminal.read_bytes().splitlines(keepends=True)[-1]
        fixture.args.expect_intent_sha256 = nr.digest(raw)
        fixture.intent_row = raw
    # All fixture root locks/target descriptors are gone before real resume().
    return fixture


@contextlib.contextmanager
def resume_premises(fixture):
    """Substitute only privilege/kernel/package/actor observations, not joins."""
    plan = fixture.plan
    with mock.patch.object(nr.os, "getuid", return_value=0), \
         mock.patch.object(nr.os, "geteuid", return_value=0), \
         mock.patch.object(nr, "NativeRead", AbsentNative), \
         mock.patch.object(nr, "expected_package", return_value=fixture.artifact), \
         mock.patch.object(nr, "expected_unix_package", return_value=plan["current_unix_package"]), \
         mock.patch.object(nr, "manager", side_effect=lambda unit, deadline: copy.deepcopy(plan["current_actors"][unit])), \
         mock.patch.object(nr, "retained_fragment", side_effect=lambda *args: copy.deepcopy(plan["retained_unix_manager_fragment"])), \
         mock.patch.object(nr.os.path, "lexists", return_value=False), \
         mock.patch.object(nr, "unlink_owned_pins", side_effect=AssertionError("resume repeated cleanup")), \
         mock.patch.object(nr.ObjectOwner, "close", side_effect=AssertionError("resume closed original object")), \
         mock.patch.object(nr.os, "unlink", side_effect=AssertionError("resume unlinked evidence")), \
         mock.patch.object(nr.os, "rmdir", side_effect=AssertionError("resume removed directory")):
        yield


def export_resume_fixture(destination):
    base = Path(destination)
    nr.require(base.is_absolute() and str(base) == str(base.resolve()), "fixture destination must be canonical absolute")
    base.mkdir(mode=0o700)
    fixture = pending_fixture(base)
    with resume_premises(fixture):
        result = nr.resume(fixture.args)
    metadata = {"schema": "hermit-controlled-resource-fixture-v1", "native_recovery_evidence": False,
                "substituted_premises": ["privilege", "BPF identity/state/absence", "actor observations",
                                         "old intent producer identity and historical action time"],
                "accepted_root": fixture.args.accepted_root, "unix_root": fixture.args.unix_root,
                "pin_root": fixture.args.pin_root, "accepted_label": LABEL, "unix_label": LABEL,
                "producer_sha256": nr.digest(Path(nr.__file__).read_bytes()), "result": result}
    (base / "fixture.json").write_bytes(nr.wire(metadata))
    (base / "inspection.json").write_bytes(nr.wire(fixture.plan))
    (base / "controlled-fragment.txt").write_bytes(fixture.fragment_bytes)
    return metadata


class RecoveryActionTests(unittest.TestCase):
    def test_export_is_new_directory_only_and_retains_complete_controlled_actor_shape(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary) / "export"
            metadata = export_fixture(str(base))
            self.assertFalse(metadata["native_recovery_evidence"])
            plan = nr.decode((base / "inspection.json").read_bytes())
            self.assertEqual(plan["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
            unit = plan["unix"]["loader_unit"]
            properties = plan["current_actors"][unit]
            fragment = plan["retained_unix_manager_fragment"]
            with mock.patch.object(nr.os.path, "lexists", return_value=False):
                nr.actor_gone(properties, unix=True)
            binding = nr.unit_command_binding((base / "controlled-fragment.txt").read_bytes(), properties,
                                               {"helper_path": fragment["command"]["path"],
                                                "helper_sha256": fragment["command"]["helper_sha256"]},
                                               os.getuid(), metadata["pin_root"], metadata["unix_root"])
            self.assertEqual(binding["argv"], fragment["command"]["argv"])
            before = {p: p.read_bytes() for p in base.rglob("*") if p.is_file()}
            with self.assertRaises(FileExistsError): export_fixture(str(base))
            self.assertEqual(before, {p: p.read_bytes() for p in base.rglob("*") if p.is_file()})
            for domain, expected_rows in (("accepted", 5), ("unix", 4)):
                terminal = next((base / domain).glob("*.terminal.jsonl"))
                self.assertEqual(len(terminal.read_bytes().splitlines()), expected_rows)

    def test_actual_recovery_orchestration_requires_two_intents_and_preserves_failed_prefixes(self):
        for failure in (None, "wrong_plan", "second_intent", "state", "second_result"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                with recovery_fixture(Path(temporary)) as fixture:
                    args, plan, roots = fixture.args, fixture.plan, fixture.roots
                    leaf, native, owners = fixture.leaf, fixture.native, fixture.owners
                    prefixes, artifact = fixture.prefixes, fixture.artifact
                    if failure == "wrong_plan": args.expect_inspection_sha256 = "0" * 64
                    if failure == "state": native.status[2] |= 1
                    calls = 0; real_append = nr.HeldFile.append

                    def append(file, data, limit):
                        nonlocal calls
                        calls += 1
                        if (failure == "second_intent" and calls == 2) or (failure == "second_result" and calls == 4):
                            raise OSError(errno.EIO, "controlled paired append failure")
                        return real_append(file, data, limit)
                    with mock.patch.object(nr.HeldFile, "append", append):
                        if failure:
                            with self.assertRaises((nr.Refused, OSError)):
                                controlled_recover(fixture)
                        else:
                            result = controlled_recover(fixture)
                            self.assertTrue(result["resource_recovery_complete"])
                            self.assertFalse(result["execution_success"])
                    files = [next(f for f in roots[p].files if f.name.endswith("terminal.jsonl"))
                             for p in (args.accepted_root, args.unix_root)]
                    tails = [(Path(f.directory.path) / f.name).read_bytes()[len(prefix):]
                             for f, prefix in zip(files, prefixes)]
                    for f, prefix in zip(files, prefixes):
                        self.assertTrue((Path(f.directory.path) / f.name).read_bytes().startswith(prefix))
                    if failure in ("wrong_plan", "second_intent", "state"):
                        self.assertEqual(len(list(leaf.iterdir())), 41)
                        self.assertTrue(all(owner.fd >= 0 for owner in owners))
                    else:
                        self.assertFalse(leaf.exists())
                        self.assertTrue(all(owner.fd == -1 for owner in owners))
                    if failure is None:
                        self.assertEqual(tails[0], tails[1]); self.assertEqual(len(tails[0].splitlines()), 2)
                        intent, result = [json.loads(line) for line in tails[0].splitlines()]
                        self.assertEqual(len(intent), 11); self.assertEqual(len(result), 12)
                        self.assertEqual(result[1], nr.digest(tails[0].splitlines(keepends=True)[0]))
                        self.assertEqual(len(intent[9]), 194); self.assertEqual(len(result[3]), 42)
                        self.assertEqual([len(scan[2]) for scan in result[8]], [194, 194])
                        # Existing exact-row parsers cannot reinterpret resource tails as execution success.
                        with self.assertRaises(nr.Refused): nr.inspect_accepted(roots[args.accepted_root], LABEL, artifact)
                        with self.assertRaises(nr.Refused): nr.inspect_unix(roots[args.unix_root], LABEL)
                    elif failure == "second_result":
                        self.assertEqual([len(t.splitlines()) for t in tails], [2, 1])
                        self.assertNotEqual(tails[0], tails[1])

    def test_complete_absence_requires_every_original_query_twice_and_actual_child_wait(self):
        ids = [[0, kind, ident] for kind, count in enumerate((24, 49, 49)) for ident in range(1, count + 1)]
        ids += [[1, kind, ident] for kind, count in enumerate((10, 31, 31)) for ident in range(1, count + 1)]
        started = nr.time.monotonic_ns()
        # Real fork/pipe/waitpid; BPF reads and privilege identity are controlled
        # premises. This is not a native privileged resource-recovery receipt.
        with mock.patch.object(nr, "NativeRead", AbsentNative), \
             mock.patch.object(nr.os, "getuid", return_value=0), \
             mock.patch.object(nr.os, "geteuid", return_value=0):
            actor, status, scans = nr.run_scanner(ids, started, started + 1_000_000_000, "a" * 64)
        self.assertEqual(status, 0)
        self.assertEqual([len(scan[2]) for scan in scans], [194, 194])
        with self.assertRaises(ChildProcessError): os.waitpid(actor[0], os.WNOHANG)
        final = nr.time.monotonic_ns()
        nr.scan_shape(scans, ids, started, started + 1_000_000_000, final)
        for mutate in (lambda x: x.pop(), lambda x: x[0][2].pop(),
                       lambda x: x[1][2].__setitem__(0, x[1][2][1]),
                       lambda x: x[1][2][0].__setitem__(3, errno.EPERM),
                       lambda x: x[1].__setitem__(0, started - 1)):
            changed = copy.deepcopy(scans); mutate(changed)
            with self.assertRaises(nr.Refused): nr.scan_shape(changed, ids, started, started + 1_000_000_000, final)
        for deadline in (0, started, started + 999_999_999, (1 << 64) - 1):
            with self.assertRaises(nr.Refused): nr.scan_shape(scans, ids, started, deadline, final)

    def test_query_errors_and_live_reused_id_are_not_absence(self):
        for code in (errno.EPERM, errno.EIO, errno.EINVAL):
            native = SimpleNamespace(by_id=mock.Mock(side_effect=OSError(code, "query failed")))
            with self.subTest(code=code), self.assertRaises(nr.Refused):
                nr.scan_once(native, [[0, 0, 123]], nr.time.monotonic_ns() + 1_000_000_000)
        with open("/dev/null", "rb") as file:
            fd = os.dup(file.fileno())
            native = SimpleNamespace(by_id=mock.Mock(return_value=fd))
            with self.assertRaises(nr.Refused): nr.scan_once(native, [[0, 2, 123]], nr.time.monotonic_ns() + 1_000_000_000)
            with self.assertRaises(OSError): os.fstat(fd)

    def test_failed_scanner_is_reaped_and_cannot_supply_certificate(self):
        class Failed(AbsentNative):
            def by_id(self, kind, ident):
                raise OSError(errno.EPERM, "not absence")
        with mock.patch.object(nr, "NativeRead", Failed), self.assertRaises(nr.Refused):
            start = nr.time.monotonic_ns()
            nr.run_scanner([[0, 0, 1]], start, start + 1_000_000_000, "a" * 64)

    def test_append_preserves_prefix_and_uncertain_second_write_stays_partial(self):
        fixture = PrivateFixture(); self.addCleanup(fixture.temp.cleanup)
        fixture.write("terminal", b'{"stage":"failed"}\n')
        root = fixture.root(); self.addCleanup(root.close)
        held = root.read("terminal", 65536)
        original = held.data
        intent = nr.wire(["hermit-failed-resource-intent-v1", "controlled"])
        held.append(intent, 65536)
        self.assertEqual((fixture.path / "terminal").read_bytes(), original + intent)
        root.recheck()
        with self.assertRaises(nr.Refused): held.append(b"X" * 65536 + b"\n", 65536)
        writer = nr.os.write
        count = 0

        def partial(fd, data):
            nonlocal count
            count += 1
            if count == 1: return writer(fd, data[:4])
            raise OSError(errno.EIO, "uncertain append")
        with mock.patch.object(nr.os, "write", side_effect=partial), self.assertRaises(OSError):
            held.append(nr.wire(["hermit-failed-resource-result-v1", "controlled"]), 65536)
        actual = (fixture.path / "terminal").read_bytes()
        self.assertTrue(actual.startswith(original + intent))
        self.assertFalse(actual.endswith(b"\n"))
        with self.assertRaises(nr.Refused): held.recheck()

    def test_exact_scoped_unlink_keeps_other_populations_and_rejects_replacement(self):
        for replace in (False, True):
            with self.subTest(replace=replace):
                fixture = PrivateFixture(); self.addCleanup(fixture.temp.cleanup)
                leaf = fixture.path / "ugb1-original"; leaf.mkdir(mode=0o700)
                outsider = fixture.write("unrelated", b"retained")
                pins, stats = {}, {}
                for kind, count, prefix, base in ((0, 10, "m", 100), (1, 31, "l", 300)):
                    for i in range(count):
                        name = f"{prefix}{i:02}"; path = leaf / name
                        path.write_text(str(base + i)); path.chmod(0o600)
                        pins[name] = (kind, base + i); stats[name] = nr.identity(path.stat())
                root = fixture.root(); self.addCleanup(root.close)
                fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY); self.addCleanup(os.close, fd)
                expected = nr.identity(os.fstat(fd))

                class PinFiles:
                    def pin(self, directory, name): return os.open(name, os.O_RDONLY, dir_fd=directory)
                    def info(self, opened, kind): return {"id": int(os.pread(opened, 16, 0))}
                if replace:
                    (leaf / "l00").write_text("999")
                    with self.assertRaises(nr.Refused):
                        nr.unlink_owned_pins(PinFiles(), fd, root, leaf.name, pins, stats, expected,
                                             nr.time.monotonic_ns() + 1_000_000_000)
                    self.assertEqual(len(list(leaf.iterdir())), 41)
                else:
                    actions = nr.unlink_owned_pins(PinFiles(), fd, root, leaf.name, pins, stats, expected,
                                                    nr.time.monotonic_ns() + 1_000_000_000)
                    self.assertEqual(len(actions), 42)
                    self.assertEqual([r[0] for r in actions[:31]], [f"l{i:02}" for i in range(31)])
                    self.assertEqual(actions[-1][0], "@directory")
                    self.assertFalse(leaf.exists())
                self.assertEqual(outsider.read_bytes(), b"retained")


class ResumeTests(unittest.TestCase):
    @staticmethod
    def terminals(fixture):
        return [Path(root) / f"{prefix}-b1-{LABEL}.terminal.jsonl"
                for root, prefix in ((fixture.args.accepted_root, "accepted"), (fixture.args.unix_root, "guard"))]

    def rewrite_intent(self, fixture, mutate, both=True):
        intent = nr.decode(fixture.intent_row)
        mutate(intent)
        raw = nr.wire(intent)
        for path, prefix in zip(self.terminals(fixture)[:2 if both else 1], fixture.prefixes):
            path.write_bytes(prefix + raw)
        fixture.args.expect_intent_sha256 = nr.digest(raw)

    def test_actual_resume_preserves_failed_attempt_and_has_no_cleanup_actions(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = pending_fixture(Path(temporary))
            originals = {p: p.read_bytes() for root in (fixture.args.accepted_root, fixture.args.unix_root)
                         for p in Path(root).iterdir()}
            with resume_premises(fixture):
                result = nr.resume(fixture.args)
            self.assertTrue(result["resource_recovery_complete"])
            self.assertFalse(result["execution_success"])
            self.assertFalse(result["original_recovery_completed"])
            current_source = nr.digest(Path(nr.__file__).read_bytes())
            tails = []
            for path, before in originals.items():
                if path in self.terminals(fixture):
                    self.assertTrue(path.read_bytes().startswith(before))
                    tails.append(path.read_bytes()[len(before):])
                else:
                    self.assertEqual(path.read_bytes(), before)
            self.assertEqual(tails[0], tails[1])
            row = nr.decode(tails[0]); intent = nr.decode(fixture.intent_row)
            self.assertEqual(row[0], nr.RESUME_TAG); self.assertEqual(len(row), 12)
            self.assertEqual(row[1], nr.digest(fixture.intent_row))
            self.assertEqual(intent[5], nr.RESUME_PREDECESSOR)
            self.assertEqual(row[6][5], current_source); self.assertEqual(row[11], current_source)
            self.assertEqual(row[3][0], fixture.leaf.name); self.assertEqual(row[3][2::2], [2, 2])
            self.assertLessEqual(intent[3], row[4])
            self.assertEqual(row[5] - row[4], 1_000_000_000)
            self.assertLessEqual(row[4], row[3][1]); self.assertLessEqual(row[3][1], row[8][0][0])
            self.assertLessEqual(row[8][1][1], row[3][3]); self.assertLessEqual(row[3][3], row[9])
            nr.scan_shape(row[8], intent[9], row[4], row[5], row[9])
            self.assertEqual(row[7], 0)
            with self.assertRaises(ChildProcessError): os.waitpid(row[6][0], os.WNOHANG)
            with resume_premises(fixture), self.assertRaises(nr.Refused): nr.resume(fixture.args)
            for root_path, parse in ((fixture.args.accepted_root, lambda r: nr.inspect_accepted(r, LABEL, fixture.artifact)),
                                     (fixture.args.unix_root, lambda r: nr.inspect_unix(r, LABEL))):
                root = nr.HeldRoot(root_path, fixture.args.owner_uid, 384)
                try:
                    with self.assertRaises(nr.Refused): parse(root)
                finally:
                    root.close()

    def test_paired_intent_authenticates_all_original_joins_before_append(self):
        mutations = [
            lambda i: i.__setitem__(5, "f" * 64),
            lambda i: i.__setitem__(1, "foreign-boot"),
            lambda i: i.__setitem__(4, i[4] + 1),
            lambda i: i.__setitem__(3, i[3] + 1),
            lambda i: i[6].__setitem__(0, "/foreign/root"),
            lambda i: i[6][1].__setitem__(1, 0),
            lambda i: i[6].__setitem__(2, "f" * 32),
            lambda i: i[6].__setitem__(3, "f" * 32),
            lambda i: i[6][4][0].__setitem__(9, "f" * 64),
            lambda i: i[6][5].__setitem__(2, "f" * 64),
            lambda i: i[7][3][3].__setitem__(9, "f" * 64),
            lambda i: i[7][6].__setitem__(0, 0),
            lambda i: i[7][7].__setitem__(1, 0),
            lambda i: i[8][2][1][0].__setitem__(1, "changed"),
            lambda i: i[8][2][3].__setitem__(9, "f" * 64),
            lambda i: i[9].pop(),
            lambda i: i[9].__setitem__(0, i[9][1]),
            lambda i: i.__setitem__(10, "f" * 64),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index), tempfile.TemporaryDirectory() as temporary:
                fixture = pending_fixture(Path(temporary))
                self.rewrite_intent(fixture, mutate)
                before = [p.read_bytes() for p in self.terminals(fixture)]
                with resume_premises(fixture), self.assertRaises(nr.Refused): nr.resume(fixture.args)
                self.assertEqual([p.read_bytes() for p in self.terminals(fixture)], before)

    def test_wrong_expected_digest_partial_peer_extra_rows_and_changed_original_bytes_refuse(self):
        for variant in ("digest", "peer", "missing", "partial", "extra", "original", "journal"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                fixture = pending_fixture(Path(temporary)); a, u = self.terminals(fixture)
                if variant == "digest": fixture.args.expect_intent_sha256 = "0" * 64
                if variant == "peer": self.rewrite_intent(fixture, lambda i: i.__setitem__(10, "a" * 64), both=False)
                if variant == "missing": u.write_bytes(fixture.prefixes[1])
                if variant == "partial": u.write_bytes(u.read_bytes()[:-1])
                if variant == "extra": a.write_bytes(a.read_bytes() + fixture.intent_row)
                if variant == "original": a.write_bytes(a.read_bytes().replace(b"wrapper exit 125", b"wrapper exit 126"))
                if variant == "journal":
                    path = Path(fixture.args.unix_root) / fixture.leaf.name
                    path.write_bytes(path.read_bytes()[:-1] + b"x")
                before = [p.read_bytes() for p in (a, u)]
                with resume_premises(fixture), self.assertRaises(nr.Refused): nr.resume(fixture.args)
                self.assertEqual([p.read_bytes() for p in (a, u)], before)

    def test_current_actor_change_pin_reuse_and_shared_launch_lock_refuse(self):
        for variant in ("actor", "fragment", "pin", "symlink", "lock", "late_pin"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                fixture = pending_fixture(Path(temporary))
                before = [p.read_bytes() for p in self.terminals(fixture)]
                if variant == "actor": fixture.plan["current_actors"][fixture.plan["unix"]["loader_unit"]]["InvocationID"] = "f" * 32
                if variant == "fragment": fixture.plan["retained_unix_manager_fragment"]["sha256"] = "f" * 64
                if variant == "pin": fixture.leaf.mkdir()
                if variant == "symlink": fixture.leaf.symlink_to("/missing/foreign/object")
                held = None
                if variant == "lock":
                    held = os.open(fixture.args.unix_root, os.O_RDONLY | os.O_DIRECTORY)
                    nr.fcntl.flock(held, nr.fcntl.LOCK_SH | nr.fcntl.LOCK_NB)
                real_scan = nr.run_scanner

                def scanner(*args):
                    result = real_scan(*args)
                    if variant == "late_pin": fixture.leaf.mkdir()
                    return result
                try:
                    with resume_premises(fixture), mock.patch.object(nr, "run_scanner", side_effect=scanner), \
                         self.assertRaises((nr.Refused, BlockingIOError)):
                        nr.resume(fixture.args)
                finally:
                    if held is not None: os.close(held)
                self.assertEqual([p.read_bytes() for p in self.terminals(fixture)], before)

    def test_failed_or_partial_second_append_never_repairs_or_relabels_first_append(self):
        for partial in (False, True):
            with self.subTest(partial=partial), tempfile.TemporaryDirectory() as temporary:
                fixture = pending_fixture(Path(temporary)); paths = self.terminals(fixture)
                before = [p.read_bytes() for p in paths]; real = nr.HeldFile.append; calls = 0

                def append(held, data, limit):
                    nonlocal calls
                    calls += 1
                    if calls == 2:
                        if partial:
                            with open(held.directory.path / held.name, "ab") as f: f.write(data[:9])
                        raise OSError(errno.EIO, "controlled second resume append failure")
                    return real(held, data, limit)
                with resume_premises(fixture), mock.patch.object(nr.HeldFile, "append", append), \
                     self.assertRaises(OSError): nr.resume(fixture.args)
                after = [p.read_bytes() for p in paths]
                self.assertTrue(all(a.startswith(b) for a, b in zip(after, before)))
                self.assertEqual(len(after[0].splitlines()), 5)
                self.assertEqual(after[1], before[1] + (after[0][len(before[0]):][:9] if partial else b""))
                with resume_premises(fixture), self.assertRaises(nr.Refused): nr.resume(fixture.args)
                self.assertEqual([p.read_bytes() for p in paths], after)

    def test_new_scanner_failure_keeps_bounded_stage_query_errno_and_actual_wait(self):
        class Failed(AbsentNative):
            def by_id(self, kind, ident): raise OSError(errno.EPERM, "controlled no authority")
        with mock.patch.object(nr, "NativeRead", Failed), \
             mock.patch.object(nr.os, "getuid", return_value=0), \
             mock.patch.object(nr.os, "geteuid", return_value=0):
            start = nr.time.monotonic_ns()
            with self.assertRaises(nr.Refused) as caught:
                nr.run_scanner([[1, 2, 987]], start, start + 1_000_000_000, "a" * 64)
        message = str(caught.exception)
        self.assertIn("wait_status=32000", message)
        self.assertIn("scans", message); self.assertIn("1/2/987", message); self.assertIn("errno=1", message)
        self.assertLess(len(message), 8192)

    def test_resume_refuses_real_second_scan_errors_live_ids_and_bad_sample_order(self):
        for variant in ("first_query", "second_query", "live", "before_pin", "after_deadline", "future_old_deadline"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                fixture = pending_fixture(Path(temporary))
                if variant == "future_old_deadline":
                    def future(intent):
                        intent[2] = nr.time.monotonic_ns()
                        intent[3] = intent[2] + 5_000_000_000
                    self.rewrite_intent(fixture, future)
                before = [p.read_bytes() for p in self.terminals(fixture)]

                class Failed(AbsentNative):
                    def __init__(self): self.queries = 0
                    def by_id(self, kind, ident):
                        self.queries += 1
                        if variant == "first_query" and self.queries == 1:
                            raise OSError(errno.EPERM, "controlled query authority failure")
                        if variant == "second_query" and self.queries == 195:
                            raise OSError(errno.EIO, "controlled second full scan failure")
                        if variant == "live" and self.queries == 195:
                            return os.open("/dev/null", os.O_RDONLY)
                        return super().by_id(kind, ident)
                actual = nr.run_scanner

                def scanner(ids, started, deadline, source):
                    actor, status, scans = actual(ids, started, deadline, source)
                    # Explicitly corrupted receipt controls: no accepted sample
                    # may predate the pin check or exceed the unchanged bound.
                    if variant == "before_pin": scans[0][0] = started
                    if variant == "after_deadline": scans[1][1] = deadline + 1
                    return actor, status, scans
                with resume_premises(fixture), mock.patch.object(nr, "NativeRead", Failed), \
                     mock.patch.object(nr, "run_scanner", side_effect=scanner), self.assertRaises(nr.Refused):
                    nr.resume(fixture.args)
                self.assertEqual([p.read_bytes() for p in self.terminals(fixture)], before)

    def test_resume_export_uses_real_append_and_is_new_directory_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary) / "export"
            result = export_resume_fixture(str(base))
            self.assertFalse(result["native_recovery_evidence"])
            self.assertFalse(result["result"]["original_recovery_completed"])
            with self.assertRaises(FileExistsError): export_resume_fixture(str(base))


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        print(json.dumps(export_fixture(sys.argv[2]), sort_keys=True))
    elif len(sys.argv) == 3 and sys.argv[1] == "--resume-fixture":
        print(json.dumps(export_resume_fixture(sys.argv[2]), sort_keys=True))
    else:
        unittest.main()
