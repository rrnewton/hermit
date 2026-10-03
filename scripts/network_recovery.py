#!/usr/bin/env python3
"""Inspect or recover one exact failed network launch's retained resources.

Inspection changes no evidence or objects. Recovery requires an exact inspected
plan digest and appends a separate resource-only proof; it cannot change the
original execution failure. The coordinator explicitly selected
Python for this bounded recovery tool. See https://github.com/rrnewton/hermit/pull/3464.

The admission reader compiles these exact source bytes into its trusted producer
identity. A later edit intentionally makes older certificates unresolved until
an explicit review adds support for that producer version. This tool does not
accept arbitrary source hashes or rewrite an older certificate.

The resume operation recognizes exactly one reviewed predecessor intent. It
does not repeat cleanup or reconstruct missing unlink receipts: it appends a
distinct certificate of newly measured resource absence, preserving the failed
attempt and its expired action deadline.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import stat
import struct
import subprocess
import sys
import time


class Refused(Exception):
    """Incomplete or inconsistent evidence remains unresolved."""


RESUME_PREDECESSOR = "5754ddedd11be06d2112de0c1d0f724b8ae981e93a9d8a08aad9957204fda5e1"
RESUME_TAG = "hermit-failed-resource-resume-result-v1"


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


class Deadline:
    def __init__(self, seconds=30):
        self.end = time.monotonic() + seconds

    def remaining(self, maximum=5):
        remaining = self.end - time.monotonic()
        require(remaining > 0, "inspection exceeded fixed deadline")
        return min(maximum, remaining)


def bounded_regular(path, limit):
    """Bound the actual read, then verify the held file and its pathname."""
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = snapshot(os.fstat(fd))
        require(stat.S_ISREG(before["mode"]), "artifact is not regular")
        require(before["bytes"] <= limit, "artifact exceeds byte bound")
        pieces, length = [], 0
        while length <= limit:
            piece = os.read(fd, min(65536, limit + 1 - length))
            if not piece:
                break
            pieces.append(piece)
            length += len(piece)
        data = b"".join(pieces)
        # Sysfs BTF reports size zero, so only the read itself bounds that file.
        require(len(data) <= limit, "artifact exceeds byte bound")
        require(snapshot(os.fstat(fd)) == before
                and snapshot(os.stat(path, follow_symlinks=False)) == before,
                "artifact changed during read")
        return data, before
    finally:
        os.close(fd)


def uint(value, bits=64):
    require(type(value) is int and 0 <= value < 1 << bits, "invalid unsigned integer")
    return value


def label(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{32}", value), "invalid launch label")
    require(int(value, 16) != 0, "zero launch label")
    return value


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def decode(data):
    return json.loads(data, object_pairs_hook=unique_object,
                      parse_constant=lambda _: (_ for _ in ()).throw(Refused("nonfinite JSON")))


def rows(data, count):
    require(data.endswith(b"\n") and len(data.splitlines()) == count, "incomplete/extra receipt rows")
    result = [decode(line) for line in data.splitlines()]
    require(all(isinstance(row, dict) for row in result), "receipt row is not an object")
    return result


def identity(st):
    return {"device": st.st_dev, "inode": st.st_ino, "mode": st.st_mode, "uid": st.st_uid}


def snapshot(st):
    return {**identity(st), "nlink": st.st_nlink, "bytes": st.st_size,
            "mtime_ns": st.st_mtime_ns, "ctime_ns": st.st_ctime_ns}


class HeldFile:
    def __init__(self, directory, name, limit):
        require("/" not in name and name not in ("", ".", ".."), "unsafe file name")
        self.directory, self.name = directory, name
        self.fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK,
                          dir_fd=directory.fd)
        try:
            st = os.fstat(self.fd)
            require(stat.S_ISREG(st.st_mode) and stat.S_IMODE(st.st_mode) == 0o600
                    and st.st_uid == directory.uid and st.st_nlink == 1, "receipt file custody differs")
            require(st.st_size <= limit, "receipt exceeds fixed byte bound")
            self.stat = snapshot(st)
            self.data = os.pread(self.fd, limit + 1, 0)
            require(len(self.data) == st.st_size, "receipt read length changed")
            self.recheck()
        except BaseException:
            os.close(self.fd)
            raise

    def recheck(self):
        require(snapshot(os.fstat(self.fd)) == self.stat, "held receipt changed")
        require(snapshot(os.stat(self.name, dir_fd=self.directory.fd, follow_symlinks=False)) == self.stat,
                "receipt name changed")
        require(os.pread(self.fd, self.stat["bytes"] + 1, 0) == self.data, "receipt bytes changed")

    def proof(self):
        return {"name": self.name, **self.stat, "sha256": digest(self.data)}

    def close(self):
        os.close(self.fd)

    def append(self, data, limit):
        """Append once, sync, then rebind only our exact known new suffix."""
        self.recheck()
        require(data.endswith(b"\n") and len(self.data) + len(data) <= limit,
                "recovery append exceeds original file bound")
        writer = os.open(self.name, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW | os.O_CLOEXEC,
                         dir_fd=self.directory.fd)
        try:
            require(snapshot(os.fstat(writer)) == self.stat, "append descriptor differs")
            left = memoryview(data)
            while left:
                count = os.write(writer, left)
                require(count > 0, "uncertain recovery append")
                left = left[count:]
            os.fdatasync(writer)
            expected = self.data + data
            require(os.pread(self.fd, len(expected) + 1, 0) == expected, "recovery append readback differs")
            self.data = expected
            self.stat = snapshot(os.fstat(self.fd))
            self.recheck()
        finally:
            os.close(writer)


class HeldRoot:
    """Existing private directory custody; flock creates no extra file role."""

    def __init__(self, path, uid, limit):
        self.path, self.uid = Path(path), uid
        require(self.path.is_absolute() and self.path.resolve(strict=True) == self.path,
                "root must be canonical absolute")
        self.fd = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        self.files = []
        try:
            st = os.fstat(self.fd)
            require(stat.S_IMODE(st.st_mode) == 0o700 and st.st_uid == uid, "root is not private owner directory")
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.identity = identity(st)
            self.names = sorted(os.listdir(self.fd))
            require(len(self.names) <= limit, "directory exceeds fixed entry bound")
            self.recheck()
        except BaseException:
            os.close(self.fd)
            raise

    def read(self, name, limit):
        f = HeldFile(self, name, limit)
        self.files.append(f)
        return f

    def recheck(self):
        require(identity(os.fstat(self.fd)) == self.identity
                and identity(os.stat(self.path, follow_symlinks=False)) == self.identity,
                "root identity changed")
        require(sorted(os.listdir(self.fd)) == self.names, "root population changed")
        for f in self.files:
            f.recheck()

    def close(self):
        for f in reversed(self.files):
            f.close()
        os.close(self.fd)


def typed_ids(items, counts):
    require(isinstance(items, list), "inventory is not a list")
    result = []
    for item in items:
        require(isinstance(item, dict) and set(item) == {"kind", "id"}, "invalid typed identifier")
        kind, ident = uint(item["kind"], 32), uint(item["id"], 32)
        require(kind < 3 and ident != 0, "invalid object type or zero ID")
        result.append((kind, ident))
    require(len(result) == len(set(result)) == sum(counts), "partial/duplicate original inventory")
    require([sum(k == i for k, _ in result) for i in range(3)] == list(counts), "inventory topology differs")
    return sorted(result)


def inventory(value, counts):
    require(value.get("complete") is True and value.get("count_invalid") is False,
            "incomplete original inventory")
    require(value.get("status") == {"returned": 0, "errno": None, "operation": "ap_identifiers"},
            "inventory call was unsuccessful")
    return typed_ids(value["ids"], counts)


def actor(value, unit):
    require(set(value) == {"unit", "invocation", "cgroup", "device", "inode"}, "actor fields differ")
    require(value["unit"] == unit and value["cgroup"] == "/sys/fs/cgroup/system.slice/" + unit,
            "actor does not belong to original run")
    label(value["invocation"])
    require(uint(value["device"]) != 0 and uint(value["inode"]) != 0, "actor identity absent")


def inspect_accepted(root, selected, expected_artifact):
    label(selected)
    for name in root.names:
        require(re.fullmatch(r"accepted-(?:b1-)?[0-9a-f]{32}\.(?:terminal\.jsonl|stdout\.log|stderr\.log)", name),
                "unknown accepted recovery role")
    require(not any(name.startswith("accepted-" + selected + ".") for name in root.names),
            "ambiguous accepted launch identity")
    files = [root.read(f"accepted-b1-{selected}.{role}", 1_048_576)
             for role in ("terminal.jsonl", "stdout.log", "stderr.log")]
    before, started, failed = rows(files[0].data, 3)
    for r, stage in zip((before, started, failed), ("accepted_before_launch", "accepted_started", "accepted_failed")):
        require(r.get("schema") == 1 and r.get("stage") == stage and r.get("label") == selected,
                "not an exact failed accepted launch")
    require(isinstance(failed.get("error"), str) and failed["error"], "missing original failure")
    require(before.get("root") == root.identity, "original accepted root differs")
    require(before.get("files") == [identity(os.fstat(f.fd)) for f in files], "original file identity differs")
    require(before.get("artifact") == expected_artifact, "original artifact differs from package")
    start = started["observed"]
    require(start.get("schema") == "hermit-accepted-parent-startup-v1"
            and start.get("artifact") == expected_artifact, "startup package differs")
    run = bytes(uint(x, 8) for x in start["run"])
    require(len(run) == 16 and any(run), "invalid original run")
    run_hex = run.hex()
    for key, prefix in (("loader", "hermit-accepted-"), ("query", "hermit-accepted-readback-")):
        actor(start[key], prefix + run_hex + ".service")
    require(start["loader"]["invocation"] != start["query"]["invocation"]
            and (start["loader"]["device"], start["loader"]["inode"])
            != (start["query"]["device"], start["query"]["inode"]),
            "original provider and query actors alias")
    pre, post = rows(files[1].data, 2)
    for r, phase in ((pre, "before_close"), (post, "after_close")):
        require(r.get("schema") == "hermit-accepted-provider-terminal-v1" and r.get("phase") == phase
                and r.get("run") == list(run) and r.get("controller_terminal") is True
                and r.get("requires_external_absence") is True, "service identity/controller differs")
    require(pre.get("failure") is None and len(pre.get("inventories", [])) == 1
            and len(post.get("close_receipts", [])) == 1, "unknown/partial provider population")
    require(post.get("service_status") == 125 and post.get("close_error") is None
            and post.get("socket_release") == "pending_process_exit", "not the supported failed-close shape")
    counts = [expected_artifact[k] for k in ("maps", "programs", "links")]
    require(counts == [24, 49, 49], "unsupported accepted topology")
    ids = inventory(pre["inventories"][0], counts)
    close = post["close_receipts"][0]
    require(close.get("incarnation") == int.from_bytes(run[:8], "little")
            and close.get("close") == {"returned": 0, "errno": None, "operation": "ap_close"}
            and close.get("unexpected_drop") is False and close.get("requires_external_absence") is True,
            "physical close was not exact")
    require(inventory(close["inventory"], counts) == ids, "close population differs")
    require(uint(post["closed_ns"]) != 0, "missing original close time")
    return {"kind": "accepted", "label": selected, "run": run_hex, "root": root.identity,
            "files": [f.proof() for f in files], "artifact": expected_artifact, "ids": ids,
            "actors": {k: start[k] for k in ("loader", "query")}, "failure": failed["error"],
            "original_closed_ns": post["closed_ns"], "service_status": post["service_status"],
            "fd_journal_unretired": pre.get("fd_journal_unretired")}


JOURNAL = struct.Struct("<6Q4IiI32s")
MAPS = ["ug_config", "ug_status", "ug_allocator", "ug_events", "ug_tasks",
        "ug_sockets", "ug_namespaces", "ug_births", "ug_initial_task", "ug_probes"]


def parse_journal(data, selected):
    incarnation = int(label(selected)[:16], 16)
    require(incarnation and data and len(data) <= 65536 and len(data) % JOURNAL.size == 0,
            "partial/oversize native journal")
    records = [JOURNAL.unpack_from(data, i) for i in range(0, len(data), JOURNAL.size)]
    directory = records[0][4:6]
    require(all(directory), "journal lacks directory identity")
    pins, intents, ids, initial, live, failures = {}, {}, [], [], [], []
    for ordinal, r in enumerate(records, 1):
        magic, inc, number, seq, dev, ino, abi, phase, kind, ident, error, reserved, raw_name = r
        require((magic, inc, number, dev, ino, abi, reserved)
                == (0x554750494E303031, incarnation, ordinal, *directory, 3, 0), "native journal identity/ordinal differs")
        require(1 <= phase <= 18 and phase not in (13, 14, 15, 16, 17), "not an unclosed failed journal")
        require((ordinal == 1) == (phase == 1), "journal BEGIN placement differs")
        name, nul, tail = raw_name.partition(b"\0")
        require(nul and not tail.strip(b"\0"), "journal pin name padding differs")
        name = name.decode("ascii")
        if phase in (2, 3):
            require(kind in (0, 1) and ident != 0 and error == 0 and seq == 0,
                    "invalid pin row")
            require(re.fullmatch(r"m0[0-9]" if kind == 0 else r"l(?:[0-2][0-9]|30)", name), "invalid owned pin name")
            if phase == 2:
                require(name not in intents, "repeated pin intent")
                intents[name] = (kind, ident)
            else:
                require(name not in pins and intents.get(name) == (kind, ident), "pin commit lacks exact intent")
                pins[name] = (kind, ident)
        elif phase == 18:
            require(not name and error == 0 and seq != 0, "invalid original ID row")
            ids.append({"kind": kind, "id": ident})
        elif phase in (8, 9):
            require(seq != 0 and not name and kind == ident == error == 0, "invalid initial registration")
            (initial if phase == 8 else live).append(seq)
        elif phase == 10:
            require(seq != 0 and not name and kind == ident == 0
                    and error in (errno.EPROTO, errno.EPIPE), "unsupported failed journal marker")
            failures.append({"ordinal": ordinal, "sequence": seq, "error": error, "marker": name})
        else:
            require(not name and kind == ident == error == 0, "unexpected journal payload")
    require(len(pins) == 41 and pins == intents, "incomplete original pin population")
    original = typed_ids(ids, [10, 31, 31])
    require({(0 if k == 0 else 2, v) for k, v in pins.values()} <= set(original), "pins differ from original inventory")
    require(initial and len(initial) == len(set(initial)) and initial == live and len(initial) <= 256,
            "partial/duplicate initial registrations")
    require(failures and any(r[7] == 12 for r in records), "missing failure/admission close")
    return {"incarnation": incarnation, "directory": {"device": directory[0], "inode": directory[1]},
            "pins": pins, "ids": original, "initial_sequences": initial, "failures": failures,
            "rows": len(records), "sha256": digest(data)}


def inspect_unix(root, selected):
    label(selected)
    for name in root.names:
        require(re.fullmatch(r"(?:guard-(?:b1-)?[0-9a-f]{32}\.(?:terminal\.jsonl|stdout\.log|stderr\.log)|ugb1-[0-9a-f]{16})", name),
                "unknown Unix recovery role")
    aliases = {name.split(".", 1)[0].removeprefix("guard-").removeprefix("b1-")
               for name in root.names if name.startswith("guard-")}
    require([v for v in aliases if v.startswith(selected[:16])] == [selected],
            "ambiguous native journal incarnation")
    files = [root.read(f"guard-b1-{selected}.{role}", 65536)
             for role in ("terminal.jsonl", "stdout.log", "stderr.log")]
    before, failed = rows(files[0].data, 2)
    unit = "hermit-unix-" + selected + ".service"
    inc = int(selected[:16], 16)
    require(before == {"schema": 1, "stage": "before_launch", "incarnation": inc, "loader_unit": unit},
            "Unix original launch identity differs")
    require(failed.get("schema") == 1 and failed.get("stage") == "terminal_failed"
            and failed.get("units") == [unit, None] and failed.get("ids") is None
            and failed.get("child_wait") == "Exited(125)" and uint(failed.get("child_pid"), 32) != 0,
            "unsupported Unix failed actor shape")
    require(isinstance(failed.get("error"), str) and failed["error"], "Unix failure missing")
    require(not files[1].data and not files[2].data, "unknown Unix loader transcript")
    journal_file = root.read("ugb1-" + selected[:16], 65536)
    journal = parse_journal(journal_file.data, selected)
    return {"kind": "unix", "label": selected, "root": root.identity,
            "files": [f.proof() for f in files] + [journal_file.proof()],
            "journal": journal, "loader_unit": unit, "failure": failed}


class NativeRead:
    """Read-only x86-64 BPF syscalls; never loads, updates, detaches or unpins."""

    def __init__(self):
        require(os.uname().machine == "x86_64", "unsupported syscall architecture")
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.libc.syscall.restype = ctypes.c_long

    def call(self, command, fields):
        require(command in (1, 4, 7, 13, 14, 15, 30), "mutation is not an inspection operation")
        buf = ctypes.create_string_buffer(fields.ljust(144, b"\0"))
        result = self.libc.syscall(321, command, ctypes.byref(buf), 144)
        if result < 0:
            raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
        return result

    def pin(self, directory, name):
        path = ctypes.create_string_buffer(os.fsencode(f"/proc/self/fd/{directory}/{name}"))
        return self.call(7, struct.pack("<QII", ctypes.addressof(path), 0, 0))

    def info(self, fd, kind):
        buf = ctypes.create_string_buffer(256)
        self.call(15, struct.pack("<IIQ", fd, 256, ctypes.addressof(buf)))
        result = {"type": struct.unpack_from("<I", buf)[0], "id": struct.unpack_from("<I", buf, 4)[0]}
        if kind == 0:
            _, _, key, value, maximum, flags = struct.unpack_from("<6I", buf)
            result.update(key_size=key, value_size=value, max_entries=maximum, flags=flags,
                          name=buf.raw[24:40].split(b"\0", 1)[0].decode("ascii"))
        elif kind == 2:
            result["prog_id"] = struct.unpack_from("<I", buf, 8)[0]
        return result

    def by_id(self, kind, ident):
        return self.call({0: 14, 1: 13, 2: 30}[kind], struct.pack("<III", ident, 0, 0))

    def lookup(self, fd, key, size, flags=0):
        k = ctypes.create_string_buffer(key)
        v = ctypes.create_string_buffer(size)
        self.call(1, struct.pack("<IIQQQ", fd, 0, ctypes.addressof(k), ctypes.addressof(v), flags))
        return v.raw

    def entries(self, fd, size, limit):
        result, previous = {}, None
        for _ in range(limit + 1):
            key = ctypes.c_uint64()
            prior = ctypes.c_uint64(previous) if previous is not None else None
            try:
                self.call(4, struct.pack("<IIQQ", fd, 0, ctypes.addressof(prior) if prior else 0, ctypes.addressof(key)))
            except OSError as error:
                if error.errno == errno.ENOENT:
                    return result
                raise
            require(len(result) < limit and key.value not in result, "map enumeration repeated/exceeded bound")
            result[key.value] = self.lookup(fd, struct.pack("<Q", key.value), size)
            previous = key.value
        raise Refused("map enumeration did not terminate")


class ObjectOwner:
    """Idempotent ownership allows explicit close before absence scanning."""
    def __init__(self, fd):
        self.fd = fd

    def close(self):
        fd, self.fd = self.fd, -1
        if fd >= 0:
            # Even a close error never authorizes retry on a reused numeric FD.
            os.close(fd)


def check_unix_state(native, descriptors, journal):
    expected = {
        "m00": (2, 4, 16, 1, "ug_config"), "m01": (2, 4, 112, 1, "ug_status"),
        "m05": (1, 8, 24, 4096, "ug_sockets"), "m06": (1, 8, 32, 128, "ug_namespaces"),
        "m08": (1, 8, 16, 256, "ug_initial_task"),
    }
    for name, shape in expected.items():
        value = native.info(descriptors[name], 0)
        require(tuple(value[k] for k in ("type", "key_size", "value_size", "max_entries", "name")) == shape
                and value["flags"] == 0, "map ABI differs")
    config = struct.unpack("<QII", native.lookup(descriptors["m00"], struct.pack("<I", 0), 16))
    require(config == (journal["incarnation"], 1, 3), "guard config differs")
    raw = native.lookup(descriptors["m01"], struct.pack("<I", 0), 112, 4)
    values = struct.unpack("<II12QII", raw)
    require(values[1] == 0 and values[2] == 1024 and values[3:6] == (0, 0, 0),
            "untrusted fault class or live tracked population")
    require(values[6] == 2 and values[7] == 2 and values[8] == journal["incarnation"]
            and values[9] != 0 and values[10] != 0 and values[-2:] == (19, 4),
            "fault does not identify the original owned readiness observation")
    require(values[11] in journal["initial_sequences"] and values[12:14] == (0, 0),
            "denial is not the original initial-task generation")
    require(not native.entries(descriptors["m05"], 24, 4096), "socket table remains populated")
    require(not native.entries(descriptors["m06"], 32, 128), "namespace table remains populated")
    initial = native.entries(descriptors["m08"], 16, 256)
    require(initial == {s: struct.pack("<QQ", journal["incarnation"], 3) for s in journal["initial_sequences"]},
            "initial task ledger is not exactly terminal")
    return {"config_hex": struct.pack("<QII", *config).hex(), "locked_status_hex": raw.hex(),
            "initial_terminal_sequences": sorted(initial), "sockets_empty": True, "namespaces_empty": True}


def bounded_command(command, deadline):
    """Drain both pipes with real byte/time bounds; kill and reap on refusal."""
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}) as child:
        try:
            output = [bytearray(), bytearray()]
            with selectors.DefaultSelector() as selector:
                for index, stream in enumerate((child.stdout, child.stderr)):
                    os.set_blocking(stream.fileno(), False)
                    selector.register(stream, selectors.EVENT_READ, index)
                while selector.get_map():
                    for key, _ in selector.select(deadline.remaining()):
                        data = os.read(key.fileobj.fileno(), 8193)
                        if not data:
                            selector.unregister(key.fileobj)
                        else:
                            output[key.data].extend(data)
                            require(len(output[key.data]) <= 8192, "manager output exceeds byte bound")
                status = child.wait(timeout=deadline.remaining())
            require(status in (0, 1), "manager read failed")
            return bytes(output[0]), bytes(output[1])
        except BaseException:
            child.kill()
            child.wait()
            raise


def manager(unit, deadline):
    command = ["/usr/bin/systemctl", "show", "--no-pager",
               "--property=Id,LoadState,ActiveState,SubState,ControlGroup,InvocationID,MainPID,ExecMainPID,ExecMainCode,ExecMainStatus,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic,FragmentPath,ExecStart", unit]
    stdout, _ = bounded_command(command, deadline)
    result = unique_object(line.split("=", 1) for line in stdout.decode().splitlines())
    require(result.get("Id") == unit, "manager unit identity differs")
    return result


def unit_command_binding(data, properties, package, uid, pin_root, recovery_root):
    helper = package["helper_path"]
    for path in (helper, pin_root, recovery_root):
        require(re.fullmatch(r"/[A-Za-z0-9_./-]+", path), "unsupported command path spelling")
    lines = data.decode().splitlines()
    commands = [line for line in lines if line.startswith("ExecStart=")]
    require(len(commands) == 2 and commands[0] == "ExecStart=", "fragment has extra/missing executable")
    match = re.fullmatch(r'ExecStart=:"' + re.escape(helper)
                         + r'" "--bootstrap-deadline-ns" "([1-9][0-9]*)"', commands[1])
    require(match is not None, "fragment executable/arguments differ from package")
    deadline = uint(int(match.group(1)))
    require(lines.count("User=" + str(uid)) == 1
            and lines.count(f'ReadWritePaths="{pin_root}" "{recovery_root}"') == 1,
            "fragment owner/recovery roots differ")
    actual = properties.get("ExecStart", "")
    prefix = f"{{ path={helper} ; argv[]={helper} --bootstrap-deadline-ns {deadline} ; ignore_errors=no ; "
    require(actual.startswith(prefix) and actual.endswith(" }"), "manager executable/arguments differ from fragment")
    suffix = actual[len(prefix):-2].split(" ; ")
    require(len(suffix) == 5 and [part.split("=", 1)[0] for part in suffix]
            == ["start_time", "stop_time", "pid", "code", "status"], "manager executable metadata differs")
    return {"path": helper, "argv": [helper, "--bootstrap-deadline-ns", str(deadline)],
            "helper_sha256": package["helper_sha256"], "historical_artifact_custody": False}


def retained_fragment(properties, package, uid, pin_root, recovery_root):
    # The legacy Unix receipt omitted InvocationID and held actor custody.
    # This is a *current* retained-manager join under the trusted privileged
    # launcher/exclusive private-root premise, never historical PIDFD proof.
    path = Path(properties.get("FragmentPath", ""))
    require(path == Path("/run/systemd/transient") / properties["Id"],
            "manager fragment is not the original named transient unit")
    data, st = bounded_regular(path, 65536)
    require(st["uid"] == 0 and stat.S_IMODE(st["mode"]) == 0o644 and st["nlink"] == 1,
            "manager fragment custody differs")
    command = unit_command_binding(data, properties, package, uid, pin_root, recovery_root)
    return {"path": str(path), **st, "sha256": digest(data),
            "exec_start": properties["ExecStart"], "command": command, "historical_pidfd_custody": False}


def actor_gone(properties, expected=None, unix=False):
    unit = properties["Id"]
    group = Path("/sys/fs/cgroup/system.slice") / unit
    require(not os.path.lexists(group), "actor cgroup remains present")
    if not unix and properties.get("LoadState") == "not-found":
        require(expected is not None, "missing original actor binding")
        return
    require(properties.get("LoadState") == "loaded" and properties.get("ActiveState") == "failed"
            and properties.get("SubState") == "failed" and properties.get("MainPID") == "0"
            and properties.get("ControlGroup") == "" and properties.get("ExecMainCode") == "1"
            and properties.get("ExecMainStatus") == "125", "actor is not the retained failed invocation")
    label(properties.get("InvocationID"))
    require(int(properties.get("ExecMainPID", "0")) > 0, "manager lacks original main process")
    require(0 < int(properties.get("ExecMainStartTimestampMonotonic", "0"))
            < int(properties.get("ExecMainExitTimestampMonotonic", "0")), "manager lacks actual terminal timing")
    if expected:
        require(properties["InvocationID"] == expected["invocation"], "original actor invocation changed")


def expected_package(directory):
    directory = Path(directory)
    manifest_bytes, _ = bounded_regular(directory / "manifest.json", 32768)
    manifest = decode(manifest_bytes)
    contract_path = Path(__file__).resolve().parents[1] / "hermit-cli/network-provider/accepted-contract.json"
    contract = decode(bounded_regular(contract_path, 65536)[0])
    for key in ("schema", "abi_version", "copy_version", "maps", "programs", "links", "btf_sha256", "grouped_event", "ftrace_only"):
        require(manifest.get(key) == contract.get(key), "package differs from maintained contract")
    require(manifest.get("kind") == "hermit-accepted-provider" and manifest["ftrace_only"] is True,
            "unsupported provider package")
    for role, name in (("object", "accepted-provider.bpf.o"), ("library", "libhermit_accepted_provider.so")):
        require(manifest.get(role) == name, "package artifact name differs")
        require(digest(bounded_regular(directory / name, 64 * 1024 * 1024)[0]) == manifest[role + "_sha256"], "package artifact bytes differ")
    require(digest(bounded_regular(Path("/sys/kernel/btf/vmlinux"), 64 * 1024 * 1024)[0]) == manifest["btf_sha256"], "running kernel BTF differs")
    require(str(manifest["abi_version"]) == "4150525553540009" and manifest["copy_version"] == 5,
            "unsupported provider wire format")
    return {"topology": {"kind": "ftrace-v1", "contract_sha256": list(bytes.fromhex(digest(json.dumps(contract, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode())))},
            "wire_format": "abi9-copy5", **{k: list(bytes.fromhex(manifest[k])) for k in ("object_sha256", "library_sha256", "btf_sha256")},
            **{k: manifest[k] for k in ("maps", "programs", "links")}}


def expected_unix_package(directory):
    directory = Path(directory)
    require(directory.is_absolute() and not directory.is_symlink(), "Unix package must be an absolute real directory")
    manifest = decode(bounded_regular(directory / "manifest.json", 32768)[0])
    contract = decode(bounded_regular(Path(__file__).resolve().parents[1]
                                      / "hermit-cli/network-provider/unix-guard-contract.json", 65536)[0])
    require(manifest.get("kind") == "hermit-unix-guard", "wrong Unix package kind")
    for key in ("schema", "abi_version", "btf_sha256", "maps", "programs", "links"):
        require(manifest.get(key) == contract[key], "Unix package differs from maintained contract")
    require([manifest[k] for k in ("maps", "programs", "links")] == [10, 31, 31], "Unix inventory differs")
    for role, name in (("object", "unix-guard.bpf.o"), ("helper", "hermit-unix-keeper"),
                       ("readback", "hermit-unix-readback")):
        require(manifest.get(role) == name, "Unix artifact name differs")
        data, st = bounded_regular(directory / name, 64 * 1024 * 1024)
        require(digest(data) == manifest[role + "_sha256"], "Unix artifact bytes differ")
        require(role == "object" or st["mode"] & 0o111, "Unix helper lacks executable mode")
    require(digest(bounded_regular(Path("/sys/kernel/btf/vmlinux"), 64 * 1024 * 1024)[0])
            == manifest["btf_sha256"], "Unix package running kernel differs")
    return {"helper_path": str(directory / "hermit-unix-keeper"),
            **{k: manifest[k] for k in ("helper_sha256", "object_sha256", "readback_sha256", "btf_sha256")}}


def wire(value):
    # New authority records use fixed arrays, not unordered object hashing.
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True).encode() + b"\n"


def root_fields(value):
    return [value[k] for k in ("device", "inode", "mode", "uid")]


def file_fields(value):
    return [value[k] for k in ("name", "device", "inode", "mode", "uid", "nlink", "bytes",
                              "mtime_ns", "ctime_ns", "sha256")]


def actors_fields(plan):
    actors = []
    for key in ("loader", "query"):
        original = plan["accepted"]["actors"][key]
        actors.append([original["unit"], sorted(plan["current_actors"][original["unit"]].items()),
                       [original[k] for k in ("invocation", "cgroup", "device", "inode")], None])
    fragment = plan["retained_unix_manager_fragment"]
    fragment = [fragment[k] for k in ("path", "device", "inode", "mode", "uid", "nlink", "bytes",
                                      "mtime_ns", "ctime_ns", "sha256")] + [
        fragment["command"]["path"], fragment["command"]["helper_sha256"],
        int(fragment["command"]["argv"][2])]
    unit = plan["unix"]["loader_unit"]
    actors.append([unit, sorted(plan["current_actors"][unit].items()), None, fragment])
    return actors


def original_ids(plan):
    result = sorted([domain, kind, ident] for domain, key in enumerate(("accepted", "unix"))
                    for kind, ident in (plan[key]["ids"] if domain == 0 else plan[key]["journal"]["ids"]))
    require(len(result) == 194 and len({tuple(row) for row in result}) == 194, "recovery population differs")
    return result


def intent_for(plan, args, started):
    accepted, unix = plan["accepted"], plan["unix"]
    artifact = accepted["artifact"]
    artifact = [artifact["wire_format"], bytes(artifact["topology"]["contract_sha256"]).hex(),
                *[bytes(artifact[k]).hex() for k in ("object_sha256", "library_sha256", "btf_sha256")],
                *[artifact[k] for k in ("maps", "programs", "links")]]
    accepted = [args.accepted_root, root_fields(accepted["root"]), accepted["label"], accepted["run"],
                [file_fields(f) for f in accepted["files"]], artifact, accepted["original_closed_ns"]]
    journal = unix["journal"]
    journal = [journal["incarnation"], journal["rows"], journal["sha256"], journal["initial_sequences"],
               [[name, *pair] for name, pair in sorted(journal["pins"].items())], journal["ids"]]
    unix = [args.unix_root, root_fields(unix["root"]), unix["label"], [file_fields(f) for f in unix["files"]],
            journal, args.pin_root, root_fields(plan["pin_root"]), root_fields(plan["pin_directory"])]
    ids = original_ids(plan)
    deadline = uint(started + 5_000_000_000)
    return ["hermit-failed-resource-intent-v1", plan["boot_id"], started, deadline, args.owner_uid,
            plan["tool_sha256"], accepted, unix, actors_fields(plan), ids, digest(wire(ids)[:-1])]


def scan_once(native, ids, deadline_ns):
    started = time.monotonic_ns()
    answers = []
    for domain, kind, ident in ids:
        require(time.monotonic_ns() <= deadline_ns, "resource scan deadline expired")
        try:
            fd = native.by_id(kind, ident)
        except OSError as error:
            require(error.errno == errno.ENOENT,
                    f"resource query failed without proving absence: {domain}/{kind}/{ident}, errno={error.errno}")
        else:
            os.close(fd)
            raise Refused(f"original or reused object ID remains live: {domain}/{kind}/{ident}")
        answers.append([domain, kind, ident, errno.ENOENT])
    ended = time.monotonic_ns()
    require(started <= ended <= deadline_ns, "resource scan exceeded deadline")
    return [started, ended, answers]


def scan_shape(scans, ids, closed, deadline, final):
    require(uint(closed) > 0 and uint(deadline) == uint(closed + 1_000_000_000)
            and closed <= final <= deadline, "resource scan time interval differs")
    require(isinstance(scans, list) and len(scans) == 2, "two complete resource scans required")
    previous = closed
    for scan in scans:
        require(isinstance(scan, list) and len(scan) == 3, "resource scan shape differs")
        start, end, answers = scan
        require(previous <= uint(start) <= uint(end) <= final, "resource scan ordering differs")
        require(answers == [[*row, errno.ENOENT] for row in ids], "resource scan population/result differs")
        previous = end


def scanner_identity(tool_hash):
    # /proc/self/exe and stat are kernel-owned descriptions of this actual child.
    executable = os.readlink("/proc/self/exe")
    image = bounded_regular(executable, 64 * 1024 * 1024)[0]
    fields = Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()
    return [os.getpid(), int(fields[19]), os.getuid(), os.geteuid(), digest(image), tool_hash]


def run_scanner(ids, closed, deadline_ns, tool_hash):
    """Real child performs both scans; parent consumes bytes AND actual waitpid."""
    read_fd, write_fd = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
    child = os.fork()
    if child == 0:
        os.close(read_fd)
        status = 125
        stage = "identity"
        try:
            native = NativeRead()
            actor = scanner_identity(tool_hash)
            stage = "scans"
            result = [actor, [scan_once(native, ids, deadline_ns) for _ in range(2)]]
            stage = "output"
            data = wire(result)
            require(len(data) <= 32768, "scanner output exceeds fixed bound")
            with selectors.DefaultSelector() as selector:
                selector.register(write_fd, selectors.EVENT_WRITE)
                left = memoryview(data)
                while left:
                    remaining = (deadline_ns - time.monotonic_ns()) / 1e9
                    require(remaining > 0, "scanner output deadline expired")
                    if selector.select(remaining):
                        count = os.write(write_fd, left)
                        require(count > 0, "scanner output failed")
                        left = left[count:]
            status = 0
        except BaseException as error:
            # Diagnostic only: a nonzero actual wait can never carry authority.
            # One nonblocking write is bounded below PIPE_BUF; failure to retain
            # it does not change the refusal or retry the original queries.
            detail = wire(["hermit-resource-scanner-failure-v1", stage,
                           type(error).__name__, str(error)[:512],
                           error.errno if isinstance(error, OSError) else None])
            try:
                os.write(write_fd, detail[:4096])
            except OSError:
                pass
        os.close(write_fd)
        os._exit(status)
    os.close(write_fd)
    reaped = False
    try:
        data = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(read_fd, selectors.EVENT_READ)
            while True:
                remaining = (deadline_ns - time.monotonic_ns()) / 1e9
                require(remaining > 0, "scanner receipt deadline expired")
                if not selector.select(remaining):
                    continue
                piece = os.read(read_fd, 32769)
                if not piece:
                    break
                data.extend(piece)
                require(len(data) <= 32768, "scanner receipt exceeds fixed bound")
        while True:
            pid, status = os.waitpid(child, os.WNOHANG)
            if pid == child:
                reaped = True
                break
            require(time.monotonic_ns() <= deadline_ns, "scanner wait deadline expired")
            time.sleep(.001)
        require(status == 0, "privileged scanner did not exit successfully: "
                f"wait_status={status}, detail={bytes(data[:4096])!r}")
        require(data.endswith(b"\n") and len(data.splitlines()) == 1, "incomplete scanner receipt")
        result = decode(data)
        require(isinstance(result, list) and len(result) == 2, "scanner receipt shape differs")
        actor, scans = result
        require(len(actor) == 6 and actor[0] == child and actor[1] > 0 and actor[2:4] == [0, 0]
                and actor[5] == tool_hash, "privileged scanner identity differs")
        scan_shape(scans, ids, closed, deadline_ns, time.monotonic_ns())
        return actor, status, scans
    finally:
        os.close(read_fd)
        if not reaped:
            try:
                os.kill(child, 9)
            except ProcessLookupError:
                pass
            os.waitpid(child, 0)


def recheck_actors(plan, args, deadline):
    for unit, before in plan["current_actors"].items():
        current = manager(unit, deadline)
        require(current == before, "original actor changed during recovery")
        expected = next((a for a in plan["accepted"]["actors"].values() if a["unit"] == unit), None)
        actor_gone(current, expected, unix=expected is None)
    require(retained_fragment(plan["current_actors"][plan["unix"]["loader_unit"]],
                              plan["current_unix_package"], args.owner_uid, args.pin_root, args.unix_root)
            == plan["retained_unix_manager_fragment"], "original retained manager fragment changed")
    require(Path("/proc/sys/kernel/random/boot_id").read_text().strip() == plan["boot_id"], "boot changed")
    require(digest(bounded_regular(Path(__file__), 1_048_576)[0]) == plan["tool_sha256"], "producer source changed")


def unlink_owned_pins(native, pin_fd, pin_root, leaf, pins, stats, expected_directory, deadline_ns):
    actions = []
    for name, (kind, ident) in sorted(pins.items(), key=lambda item: (item[0][0] != "l", item[0])):
        require(time.monotonic_ns() <= deadline_ns, "recovery action deadline expired")
        current = os.stat(name, dir_fd=pin_fd, follow_symlinks=False)
        require(identity(current) == stats[name], "owned pin name changed before unlink")
        opened = native.pin(pin_fd, name)
        try:
            require(native.info(opened, 0 if kind == 0 else 2)["id"] == ident, "pin target changed before unlink")
        finally:
            os.close(opened)
        os.unlink(name, dir_fd=pin_fd)
        try:
            os.stat(name, dir_fd=pin_fd, follow_symlinks=False)
        except FileNotFoundError:
            pass
        else:
            raise Refused("unlinked pin pathname remains present")
        actions.append([name, 0 if kind == 0 else 2, ident, *root_fields(stats[name]),
                        time.monotonic_ns(), 0, errno.ENOENT])
    require(not os.listdir(pin_fd), "owned pin directory remains populated")
    require(identity(os.fstat(pin_fd)) == expected_directory
            and identity(os.stat(leaf, dir_fd=pin_root.fd, follow_symlinks=False)) == expected_directory,
            "owned pin directory changed before removal")
    os.rmdir(leaf, dir_fd=pin_root.fd)
    try:
        os.stat(leaf, dir_fd=pin_root.fd, follow_symlinks=False)
    except FileNotFoundError:
        pass
    else:
        raise Refused("owned pin directory removal not observed")
    actions.append(["@directory", 3, 0, *root_fields(expected_directory), time.monotonic_ns(), 0, errno.ENOENT])
    require(time.monotonic_ns() <= deadline_ns, "recovery action deadline expired")
    return actions


def recover(args, plan, roots, pin_fd, leaf, native, descriptors, pin_stats, owners):
    require(os.getuid() == os.geteuid() == 0, "recovery requires actual privileged executor")
    require(re.fullmatch(r"[0-9a-f]{64}", args.expect_inspection_sha256 or ""), "exact inspected-plan digest required")
    inspected = json.dumps(plan, sort_keys=True, indent=2).encode() + b"\n"
    require(digest(inspected) == args.expect_inspection_sha256, "inspected plan changed")
    started = time.monotonic_ns()
    intent = intent_for(plan, args, started)
    intent_row = wire(intent)
    terminals = [next(f for f in roots[p].files if f.name.endswith("terminal.jsonl"))
                 for p in (args.accepted_root, args.unix_root)]
    # Reserve enough for the finite proof BEFORE the first append/unlink.
    for f, limit in zip(terminals, (1_048_576, 65536)):
        require(len(f.data) + len(intent_row) + 32768 <= limit, "insufficient original recovery file capacity")
    for f, limit in zip(terminals, (1_048_576, 65536)):
        f.append(intent_row, limit)
    action_deadline = Deadline((intent[3] - time.monotonic_ns()) / 1e9)
    recheck_actors(plan, args, action_deadline)
    check_unix_state(native, descriptors, plan["unix"]["journal"])
    for root in roots.values():
        root.recheck()
    actions = unlink_owned_pins(native, pin_fd, roots[args.pin_root], leaf,
                                plan["unix"]["journal"]["pins"], pin_stats,
                                plan["pin_directory"], intent[3])
    roots[args.pin_root].names.remove(leaf)
    for owner in reversed(owners):
        owner.close()
    require(all(owner.fd == -1 for owner in owners), "target references remain owned")
    closed = time.monotonic_ns()
    require(closed <= intent[3], "resource close exceeded action deadline")
    deadline_ns = uint(closed + 1_000_000_000)
    scanner, status, scans = run_scanner(original_ids(plan), closed, deadline_ns, plan["tool_sha256"])
    recheck_actors(plan, args, Deadline((deadline_ns - time.monotonic_ns()) / 1e9))
    for root in roots.values():
        root.recheck()
    # This is the completed state-verification cut, before durable publication.
    # Both scans, child wait and final rechecks fit the fresh one-second window;
    # later append/sync time is not mislabeled as part of that sampling interval.
    final = time.monotonic_ns()
    scan_shape(scans, original_ids(plan), closed, deadline_ns, final)
    result = ["hermit-failed-resource-result-v1", digest(intent_row), plan["boot_id"], actions,
              closed, deadline_ns, scanner, status, scans, final, actors_fields(plan), plan["tool_sha256"]]
    result_row = wire(result)
    require(len(result_row) <= 32768, "resource proof exceeds reserved byte bound")
    for f, limit in zip(terminals, (1_048_576, 65536)):
        f.append(result_row, limit)
    for root in roots.values():
        root.recheck()
    return {"schema": "hermit-failed-resource-recovery-v1", "resource_recovery_complete": True,
            "execution_success": False, "intent_sha256": digest(intent_row),
            "result_sha256": digest(result_row), "original_failure_preserved": True,
            "admission_authority": "only independently validated paired resource proof"}


class PrefixFile:
    """Read-only authenticated old prefix; never changes the held actual file."""

    def __init__(self, held, proof, prefix=None):
        keys = ("name", "device", "inode", "mode", "uid", "nlink", "bytes",
                "mtime_ns", "ctime_ns", "sha256")
        require(isinstance(proof, list) and len(proof) == len(keys), "original file proof shape differs")
        require(proof[0] == held.name and isinstance(proof[9], str)
                and re.fullmatch(r"[0-9a-f]{64}", proof[9]) and int(proof[9], 16),
                "original file proof identity differs")
        for value in proof[1:9]:
            uint(value)
        self.fd, self.data = held.fd, held.data if prefix is None else prefix
        self.original = dict(zip(keys, proof))
        expected = held.proof()
        # Appending the intent changed only terminal length/timestamps/hash.
        # The old timestamps are historical; only unchanged files can match
        # their current timestamps. All prefixes still require exact old bytes.
        for key in keys[1:6] if prefix is not None else keys[1:9]:
            require(self.original[key] == expected[key], "original held file metadata differs")
        require(len(self.data) == proof[6] and digest(self.data) == proof[9],
                "original failed prefix/file bytes differ")

    def proof(self):
        return self.original.copy()


class PrefixRoot:
    """Feeds exact old bytes through the unchanged original receipt parsers."""

    def __init__(self, root, proofs, terminal, prefix):
        require(isinstance(proofs, list), "original file proofs are not an array")
        self.root, self.identity, self.names = root, root.identity, root.names
        self.proofs, self.terminal, self.prefix = proofs, terminal, prefix
        self.used = 0

    def read(self, name, limit):
        require(self.used < len(self.proofs), "missing original file proof")
        proof = self.proofs[self.used]
        self.used += 1
        held = self.terminal if name == self.terminal.name else self.root.read(name, limit)
        return PrefixFile(held, proof, self.prefix if held is self.terminal else None)

    def finish(self):
        require(self.used == len(self.proofs), "extra original file proof")


def pin_absent(root, leaf):
    try:
        os.stat(leaf, dir_fd=root.fd, follow_symlinks=False)
    except OSError as error:
        require(error.errno == errno.ENOENT, "owned pin absence query failed")
    else:
        raise Refused("owned pin name remains present or was reused")
    return time.monotonic_ns()


def pending_plan(args, roots, deadline):
    """Authenticate one known old paired intent without reopening absent objects."""
    terminals, prefixes, intent_rows = [], [], []
    for path, prefix, selected, count, limit in (
            (args.accepted_root, "accepted", args.accepted_label, 3, 1_048_576),
            (args.unix_root, "guard", args.unix_label, 2, 65536)):
        label(selected)
        held = roots[path].read(f"{prefix}-b1-{selected}.terminal.jsonl", limit)
        parts = held.data.splitlines(keepends=True)
        require(held.data.endswith(b"\n") and len(parts) == count + 1,
                "resume requires exactly one complete pending intent")
        terminals.append(held)
        prefixes.append(b"".join(parts[:-1]))
        intent_rows.append(parts[-1])
    require(intent_rows[0] == intent_rows[1], "pending paired intents differ")
    raw = intent_rows[0]
    require(isinstance(args.expect_intent_sha256, str)
            and re.fullmatch(r"[0-9a-f]{64}", args.expect_intent_sha256)
            and digest(raw) == args.expect_intent_sha256, "exact pending intent digest differs")
    intent = decode(raw)
    require(isinstance(intent, list) and len(intent) == 11
            and intent[0] == "hermit-failed-resource-intent-v1"
            and intent[5] == RESUME_PREDECESSOR, "unknown pending intent producer/schema")
    require(uint(intent[4], 32) == args.owner_uid
            and 0 < uint(intent[2]) < uint(intent[3]) == uint(intent[2] + 5_000_000_000),
            "pending owner/action interval differs")
    require(intent[1] == Path("/proc/sys/kernel/random/boot_id").read_text().strip(), "pending boot differs")
    accepted_input, unix_input = intent[6:8]
    require(isinstance(accepted_input, list) and len(accepted_input) == 7
            and isinstance(unix_input, list) and len(unix_input) == 8, "pending inputs shape differs")
    require(accepted_input[:3] == [args.accepted_root, root_fields(roots[args.accepted_root].identity), args.accepted_label]
            and unix_input[:3] == [args.unix_root, root_fields(roots[args.unix_root].identity), args.unix_label]
            and unix_input[5:7] == [args.pin_root, root_fields(roots[args.pin_root].identity)],
            "pending configured roots/labels differ")
    accepted_view = PrefixRoot(roots[args.accepted_root], accepted_input[4], terminals[0], prefixes[0])
    unix_view = PrefixRoot(roots[args.unix_root], unix_input[3], terminals[1], prefixes[1])
    accepted = inspect_accepted(accepted_view, args.accepted_label, expected_package(args.accepted_package))
    unix = inspect_unix(unix_view, args.unix_label)
    accepted_view.finish(); unix_view.finish()
    leaf_identity = unix_input[7]
    require(isinstance(leaf_identity, list) and len(leaf_identity) == 4,
            "original pin directory identity shape differs")
    for value in leaf_identity:
        uint(value)
    require(leaf_identity == [unix["journal"]["directory"][k] for k in ("device", "inode")]
            + [stat.S_IFDIR | 0o700, args.owner_uid], "original pin directory identity differs")
    package = expected_unix_package(args.unix_package)
    actors = {}
    for expected in accepted["actors"].values():
        current = manager(expected["unit"], deadline)
        actor_gone(current, expected)
        actors[expected["unit"]] = current
    current = manager(unix["loader_unit"], deadline)
    actor_gone(current, unix=True)
    actors[unix["loader_unit"]] = current
    fragment = retained_fragment(current, package, args.owner_uid, args.pin_root, args.unix_root)
    plan = {"accepted": accepted, "unix": unix, "pin_root": roots[args.pin_root].identity,
            "pin_directory": dict(zip(("device", "inode", "mode", "uid"), leaf_identity)),
            "current_actors": actors, "retained_unix_manager_fragment": fragment,
            "current_unix_package": package, "boot_id": intent[1], "tool_sha256": RESUME_PREDECESSOR}
    # Reconstruct every old field from the original evidence/current bindings.
    # Exact wire equality also excludes bool/float aliases and noncanonical rows.
    require(wire(intent_for(plan, args, intent[2])) == raw, "pending intent evidence/actor joins differ")
    plan["tool_sha256"] = digest(bounded_regular(Path(__file__), 1_048_576)[0])
    return plan, intent, raw, terminals


def resume(args):
    """Append only: newly prove absence, without repeating any old cleanup."""
    require(os.getuid() == os.geteuid() == 0, "resume requires actual privileged executor")
    require(len({args.accepted_root, args.unix_root, args.pin_root}) == 3, "resume roots alias")
    with contextlib.ExitStack() as stack:
        roots = {}
        for path, limit in sorted(((args.accepted_root, 4096), (args.unix_root, 384), (args.pin_root, 128))):
            roots[path] = root = HeldRoot(path, args.owner_uid, limit)
            stack.callback(root.close)
        require(len({(r.identity["device"], r.identity["inode"]) for r in roots.values()}) == 3,
                "resume roots alias by identity")
        plan, intent, raw, terminals = pending_plan(args, roots, Deadline())
        for held, limit in zip(terminals, (1_048_576, 65536)):
            require(len(held.data) + 32768 <= limit, "insufficient original recovery file capacity")
        leaf = "ugb1-" + args.unix_label[:16]
        started = time.monotonic_ns()
        require(intent[3] <= started, "original action deadline has not expired")
        deadline = uint(started + 1_000_000_000)
        recheck_actors(plan, args, Deadline((deadline - time.monotonic_ns()) / 1e9))
        for root in roots.values():
            root.recheck()
        pre = pin_absent(roots[args.pin_root], leaf)
        scanner, status, scans = run_scanner(original_ids(plan), started, deadline, plan["tool_sha256"])
        post = pin_absent(roots[args.pin_root], leaf)
        recheck_actors(plan, args, Deadline((deadline - time.monotonic_ns()) / 1e9))
        for root in roots.values():
            root.recheck()
        final = time.monotonic_ns()
        scan_shape(scans, original_ids(plan), started, deadline, final)
        require(started <= pre <= scans[0][0] and scans[1][1] <= post <= final,
                "resume pin observation order differs")
        result = [RESUME_TAG, digest(raw), plan["boot_id"], [leaf, pre, errno.ENOENT, post, errno.ENOENT],
                  started, deadline, scanner, status, scans, final, actors_fields(plan), plan["tool_sha256"]]
        result_row = wire(result)
        require(len(result_row) <= 32768, "resume proof exceeds reserved byte bound")
        for held, limit in zip(terminals, (1_048_576, 65536)):
            held.append(result_row, limit)
        for root in roots.values():
            root.recheck()
        return {"schema": RESUME_TAG, "resource_recovery_complete": True,
                "execution_success": False, "original_recovery_completed": False,
                "intent_sha256": digest(raw), "result_sha256": digest(result_row),
                "original_failure_preserved": True,
                "admission_authority": "only independently validated paired resource proof"}


def inspect(args):
    require(len({str(Path(p)) for p in (args.accepted_root, args.unix_root, args.pin_root)}) == 3,
            "recovery roots alias")
    boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    deadline = Deadline()
    with contextlib.ExitStack() as stack:
        roots = {}
        for p, limit in sorted(((args.accepted_root, 4096), (args.unix_root, 384), (args.pin_root, 128))):
            roots[p] = root = HeldRoot(p, args.owner_uid, limit)
            stack.callback(root.close)
        require(len({(r.identity["device"], r.identity["inode"]) for r in roots.values()}) == 3,
                "recovery roots alias by identity")
        accepted = inspect_accepted(roots[args.accepted_root], args.accepted_label, expected_package(args.accepted_package))
        unix = inspect_unix(roots[args.unix_root], args.unix_label)
        unix_package = expected_unix_package(args.unix_package)
        pin_root = roots[args.pin_root]
        leaf = "ugb1-" + args.unix_label[:16]
        fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=pin_root.fd)
        stack.callback(os.close, fd)
        pin_identity = identity(os.fstat(fd))
        j = unix["journal"]
        require(pin_identity == {**j["directory"], "mode": stat.S_IFDIR | 0o700, "uid": args.owner_uid}, "owned pin directory changed")
        require(sorted(os.listdir(fd)) == sorted(j["pins"]), "owned pin population differs")
        native, descriptors, pin_stats, object_info, owners = NativeRead(), {}, {}, {}, []
        for name, (kind, ident) in sorted(j["pins"].items()):
            deadline.remaining()
            st = os.stat(name, dir_fd=fd, follow_symlinks=False)
            require(stat.S_ISREG(st.st_mode) and stat.S_IMODE(st.st_mode) == 0o600 and st.st_uid == args.owner_uid,
                    "pin ownership differs")
            pin_stats[name] = identity(st)
            opened = native.pin(fd, name)
            owner = ObjectOwner(opened); owners.append(owner); stack.callback(owner.close)
            descriptors[name] = opened
            actual_kind = 0 if kind == 0 else 2
            info = native.info(opened, actual_kind)
            require(info["id"] == ident and info["type"] != 0, "actual pin object differs")
            if kind == 0:
                require(info["name"] == MAPS[int(name[1:])], "actual map name differs")
            object_info[name] = info
        programs = {i for k, i in j["ids"] if k == 1}
        require({v["prog_id"] for n, v in object_info.items() if n.startswith("l")} == programs,
                "link program inventory differs")
        for ident in programs:
            deadline.remaining()
            opened = native.by_id(1, ident)
            owner = ObjectOwner(opened); owners.append(owner); stack.callback(owner.close)
            require(native.info(opened, 1)["id"] == ident, "program identity differs")
        state = check_unix_state(native, descriptors, j)
        actors = {}
        for key, expected in accepted["actors"].items():
            properties = manager(expected["unit"], deadline); actor_gone(properties, expected)
            actors[expected["unit"]] = properties
        properties = manager(unix["loader_unit"], deadline); actor_gone(properties, unix=True)
        fragment = retained_fragment(properties, unix_package, args.owner_uid, args.pin_root, args.unix_root)
        actors[unix["loader_unit"]] = properties
        require(check_unix_state(native, descriptors, j) == state, "guard state changed during inspection")
        for unit, properties in actors.items():
            require(manager(unit, deadline) == properties, "actor changed during inspection")
        require(retained_fragment(actors[unix["loader_unit"]], unix_package, args.owner_uid,
                                  args.pin_root, args.unix_root) == fragment,
                "retained manager fragment changed")
        require(expected_unix_package(args.unix_package) == unix_package, "Unix package changed during inspection")
        for root in roots.values(): root.recheck()
        require(identity(os.stat(leaf, dir_fd=pin_root.fd, follow_symlinks=False)) == pin_identity
                and identity(os.fstat(fd)) == pin_identity and sorted(os.listdir(fd)) == sorted(j["pins"]),
                "owned pin directory changed during inspection")
        for name, old in pin_stats.items():
            require(identity(os.stat(name, dir_fd=fd, follow_symlinks=False)) == old, "pin name replaced")
            require(native.info(descriptors[name], 0 if name.startswith("m") else 2) == object_info[name],
                    "held object metadata changed")
        require(Path("/proc/sys/kernel/random/boot_id").read_text().strip() == boot, "boot changed")
        deadline.remaining()
        plan = {"schema": "hermit-failed-resource-inspection-v1", "outcome": "inspected",
                "resource_recovery_complete": False, "execution_success": False,
                "admission_authority": False, "mutations_performed": False,
                "tool_sha256": digest(Path(__file__).read_bytes()), "boot_id": boot,
                "accepted": accepted, "unix": unix, "pinned_objects": object_info,
                "pin_directory": pin_identity, "pin_root": pin_root.identity,
                "guard_state": state, "current_actors": actors,
                "retained_unix_manager_fragment": fragment,
                "current_unix_package": unix_package,
                "trust_premise": "exclusive private roots and conforming trusted privileged launchers; no unit reset or recreation",
                "required_next_action": "independently reviewed scoped cleanup and two fresh full absence scans"}
        if args.action == "recover":
            return recover(args, plan, roots, fd, leaf, native, descriptors, pin_stats, owners)
        return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["inspect", "recover", "resume"])
    for option in ("accepted-root", "accepted-label", "accepted-package", "unix-root", "unix-label", "unix-package", "pin-root"):
        parser.add_argument("--" + option, required=True)
    parser.add_argument("--owner-uid", required=True, type=int)
    parser.add_argument("--expect-inspection-sha256")
    parser.add_argument("--expect-intent-sha256")
    args = parser.parse_args()
    try:
        require(args.owner_uid >= 0, "invalid owner UID")
        result = resume(args) if args.action == "resume" else inspect(args)
    except (Refused, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(json.dumps({"schema": "hermit-failed-resource-inspection-v1", "outcome": "refused",
                          "resource_recovery_complete": False, "admission_authority": False,
                          "reason": str(error)}))
        return 1
    print(json.dumps(result, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
