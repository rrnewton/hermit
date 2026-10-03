#!/usr/bin/env python3
"""Inspect an incomplete ABI11 accepted launch without changing its execution result.

This distinct ABI11 producer preserves the legacy accepted-only and paired
recovery protocols and their source identities. Inspection grants no admission authority and changes no evidence. Certification
appends one separate intent/result and performs two fresh full absence scans;
it never loads or deletes resources or repairs a missing normal terminal.
See https://github.com/rrnewton/hermit/pull/3464.
"""
from __future__ import annotations

import argparse
import contextlib
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import stat
import subprocess
import sys
import time
import types
import uuid

DEPENDENCY_SHA256 = "b6c42c56c88f45cd14dce607fbb673a4ccae3fc4b267f98d423d3858f09565a9"
CONTRACT_RAW = "8c8c7f81f92823cb5d280268cecbd973ed71e153ac26a5e92ec8d6fce2557bfc"
CONTRACT_CANONICAL = "a930de47d49081fefd650b3c55f0d885fc0633f3a211c90679de47015b1e0778"
LIMIT = 1_048_576
ROLES = ("terminal.jsonl", "stdout.log", "stderr.log")
ACTOR_KEYS = ("Id", "LoadState", "ActiveState", "SubState", "InvocationID", "Result",
              "ExecMainStatus", "MainPID", "ControlGroup")


def load_dependency(path):
    """Execute only the verified actual bytes, never an import-cache substitute."""
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_size > LIMIT:
            raise ValueError("paired producer is not a bounded regular file")
        data = os.pread(fd, LIMIT + 1, 0)
        after = os.fstat(fd)
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
                after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError("paired producer changed during read")
        if hashlib.sha256(data).hexdigest() != DEPENDENCY_SHA256:
            raise ValueError("paired producer source differs")
    finally:
        os.close(fd)
    module = types.ModuleType("_accepted_abi11_recovery_pinned_dependency")
    module.__file__ = str(path)
    exec(compile(data, str(path), "exec"), module.__dict__)
    return module


nr = load_dependency(Path(__file__).resolve().with_name("network_recovery.py"))
Refused, require = nr.Refused, nr.require


def fields(value, names):
    require(isinstance(value, dict) and set(value) == set(names), "original field set differs")


def artifact_valid(value):
    fields(value, ("topology", "wire_format", "object_sha256", "library_sha256", "btf_sha256",
                   "maps", "programs", "links"))
    fields(value["topology"], ("kind", "contract_sha256"))
    require(value["topology"]["kind"] == "ftrace-v1" and value["wire_format"] == "abi11-copy5"
            and all(type(value[k]) is int for k in ("maps", "programs", "links"))
            and [value[k] for k in ("maps", "programs", "links")] == [25, 49, 49],
            "unsupported accepted topology")
    for digest in (value["topology"]["contract_sha256"],
                   *(value[k] for k in ("object_sha256", "library_sha256", "btf_sha256"))):
        require(isinstance(digest, list) and len(digest) == 32
                and all(type(x) is int and 0 <= x <= 255 for x in digest) and any(digest),
                "zero or malformed artifact digest")
    require(bytes(value["topology"]["contract_sha256"]).hex() == CONTRACT_CANONICAL,
            "unsupported original ABI11 contract digest")


def source_original(root, selected, expected_artifact):
    """Authenticate exactly the missing-parent / successful-service-close shape."""
    nr.label(selected)
    current = 0
    for name in root.names:
        require(re.fullmatch(r"accepted-(?:b1-)?[0-9a-f]{32}\.(?:terminal\.jsonl|stdout\.log|stderr\.log)", name),
                "unknown accepted recovery role")
        current += name.startswith("accepted-b1-")
    require(current <= 384, "accepted role population exceeds unchanged bound")
    require(not any(name.startswith("accepted-" + selected + ".") for name in root.names),
            "ambiguous accepted launch identity")
    files = [root.read(f"accepted-b1-{selected}.{role}", LIMIT) for role in ROLES]
    require(len({(f.stat["device"], f.stat["inode"]) for f in files}) == 3, "original files alias")
    before, started = nr.rows(files[0].data, 2)
    fields(before, ("schema", "stage", "label", "root", "artifact", "files"))
    fields(started, ("schema", "stage", "label", "observed"))
    for row, stage in ((before, "accepted_before_launch"), (started, "accepted_started")):
        require(type(row["schema"]) is int and row["schema"] == 1
                and row["stage"] == stage and row["label"] == selected, "original startup stage differs")
    require(before["root"] == root.identity, "original accepted root differs")
    require(before["files"] == [nr.identity(os.fstat(f.fd)) for f in files], "original file identity differs")
    artifact_valid(expected_artifact)
    artifact_valid(before["artifact"])
    require(before["artifact"] == expected_artifact, "original artifact differs from actual package")
    startup = started["observed"]
    fields(startup, ("schema", "run", "artifact", "loader", "query"))
    require(startup["schema"] == "hermit-accepted-parent-startup-v1"
            and startup["artifact"] == expected_artifact, "startup package differs")
    require(isinstance(startup["run"], list), "original run is not bytes")
    artifact_valid(startup["artifact"])
    run = bytes(nr.uint(x, 8) for x in startup["run"])
    require(len(run) == 16 and any(run), "original run is zero or malformed")
    for key, prefix in (("loader", "hermit-accepted-"), ("query", "hermit-accepted-readback-")):
        nr.actor(startup[key], prefix + run.hex() + ".service")
    require(startup["loader"]["invocation"] != startup["query"]["invocation"]
            and (startup["loader"]["device"], startup["loader"]["inode"])
            != (startup["query"]["device"], startup["query"]["inode"]), "original actors alias")
    pre, post = nr.rows(files[1].data, 2)
    for row, phase in ((pre, "before_close"), (post, "after_close")):
        require(row.get("schema") == "hermit-accepted-provider-terminal-v1"
                and row.get("phase") == phase and row.get("run") == list(run)
                and row.get("controller_terminal") is True and row.get("requires_external_absence") is True,
                "original service identity differs")
    require("failure" in pre and pre["failure"] is None
            and type(post.get("service_status")) is int and post["service_status"] == 0
            and "close_error" in post and post["close_error"] is None
            and post.get("socket_release") == "pending_process_exit", "unsupported service close shape")
    require(isinstance(pre.get("inventories"), list) and len(pre["inventories"]) == 1
            and isinstance(post.get("close_receipts"), list) and len(post["close_receipts"]) == 1,
            "partial original provider population")
    close = post["close_receipts"][0]
    require(type(close.get("incarnation")) is int
            and close["incarnation"] == int.from_bytes(run[:8], "little")
            and close.get("close") == {"returned": 0, "errno": None, "operation": "ap_close"}
            and type(close["close"]["returned"]) is int
            and close.get("unexpected_drop") is False and close.get("requires_external_absence") is True,
            "physical close differs")
    for inv in (pre["inventories"][0], close["inventory"]):
        fields(inv, ("complete", "count_invalid", "ids", "status"))
        require(type(inv.get("status", {}).get("returned")) is int, "inventory result type differs")
    ids = nr.inventory(pre["inventories"][0], [25, 49, 49])
    require(nr.inventory(close["inventory"], [25, 49, 49]) == ids, "before/after typed inventory differs")
    require(nr.uint(post["closed_ns"]) > 0, "original close time absent")
    return {"label": selected, "run": run.hex(), "root": root.identity,
            "files": [f.proof() for f in files], "artifact": expected_artifact,
            "ids": [[0, kind, ident] for kind, ident in ids],
            "actors": {key: startup[key] for key in ("loader", "query")},
            "original_closed_ns": post["closed_ns"], "original_rows": 2,
            "execution_status": "missing-parent-terminal", "service_status": 0}


def manager_query(unit, deadline):
    """Retain real bounded command status; errors never become absence."""
    command = ["/usr/bin/systemctl", "show", "--no-pager", "--property=" + ",".join(ACTOR_KEYS), unit]
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
                            require(len(output[key.data]) <= 8192, "manager output exceeds bound")
                status = child.wait(timeout=deadline.remaining())
            require(status == 0 and not output[1], "manager query did not complete cleanly")
            props = nr.unique_object(line.split("=", 1) for line in output[0].decode().splitlines())
            fields(props, ACTOR_KEYS)
            return status, props
        except BaseException:
            child.kill()
            child.wait()
            raise


def cgroup_absent(path):
    try:
        os.stat(path, follow_symlinks=False)
    except OSError as error:
        require(error.errno == errno.ENOENT, "actor cgroup lookup did not prove absence")
        return errno.ENOENT
    raise Refused("actor cgroup path still exists")


def observe_actor(original, deadline):
    before = time.monotonic_ns()
    status, props = manager_query(original["unit"], deadline)
    fields(props, ACTOR_KEYS)
    require(status == 0 and props == {"Id": original["unit"], "LoadState": "not-found",
            "ActiveState": "inactive", "SubState": "dead", "InvocationID": "",
            "Result": "success", "ExecMainStatus": "0", "MainPID": "0", "ControlGroup": ""},
            "original actor is not currently collected")
    missing = cgroup_absent(original["cgroup"])
    after = time.monotonic_ns()
    require(0 < before <= after, "actor observation chronology differs")
    return [original["unit"], [original[k] for k in ("invocation", "cgroup", "device", "inode")],
            sorted(props.items()), status, before, after, missing]


def actor_authority(observations):
    # Fresh times remain evidence, but cannot be part of a stable inspect-plan hash.
    return [[*row[:4], row[6]] for row in observations]


def boot_id():
    boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    require(str(uuid.UUID(boot)) == boot, "boot UUID is not canonical")
    return boot


def source_identity():
    return nr.digest(nr.bounded_regular(Path(__file__).resolve(), LIMIT)[0])


def expected_package(directory):
    """Authenticate exactly ABI11; the pinned dependency remains ABI9-only."""
    directory = Path(directory)
    contract_path = Path(__file__).resolve().parents[1] / "hermit-cli/network-provider/accepted-contract.json"
    contract_data, _ = nr.bounded_regular(contract_path, 65536)
    require(nr.digest(contract_data) == CONTRACT_RAW, "original ABI11 contract bytes differ")
    contract = nr.decode(contract_data)
    require(nr.digest(json.dumps(contract, sort_keys=True, separators=(",", ":"),
                                ensure_ascii=False).encode()) == CONTRACT_CANONICAL,
            "original ABI11 canonical contract differs")
    manifest = nr.decode(nr.bounded_regular(directory / "manifest.json", 32768)[0])
    for key in ("schema", "abi_version", "copy_version", "maps", "programs", "links", "btf_sha256",
                "grouped_event", "ftrace_only"):
        require(key in manifest and nr.wire(manifest[key]) == nr.wire(contract[key]),
                "original package contract differs")
    require(manifest.get("kind") == "hermit-accepted-provider"
            and manifest["abi_version"] == "415052555354000b" and manifest["copy_version"] == 5
            and manifest["ftrace_only"] is True
            and manifest.get("sources", {}).get("accepted-contract.json") == CONTRACT_RAW,
            "unsupported original package identity")
    for role, name in (("object", "accepted-provider.bpf.o"), ("library", "libhermit_accepted_provider.so")):
        require(manifest.get(role) == name, "original artifact name differs")
        require(nr.digest(nr.bounded_regular(directory / name, 64 * 1024 * 1024)[0])
                == manifest[role + "_sha256"], "original artifact bytes differ")
    require(nr.digest(nr.bounded_regular(Path("/sys/kernel/btf/vmlinux"), 64 * 1024 * 1024)[0])
            == manifest["btf_sha256"], "running kernel BTF differs")
    artifact = {"topology": {"kind": "ftrace-v1", "contract_sha256": list(bytes.fromhex(CONTRACT_CANONICAL))},
                "wire_format": "abi11-copy5", **{k: list(bytes.fromhex(manifest[k]))
                for k in ("object_sha256", "library_sha256", "btf_sha256")},
                **{k: manifest[k] for k in ("maps", "programs", "links")}}
    artifact_valid(artifact)
    return artifact


def inspect_locked(args, root, deadline):
    require(nr.digest(nr.bounded_regular(Path(nr.__file__), LIMIT)[0]) == DEPENDENCY_SHA256,
            "paired dependency changed")
    boot = boot_id()
    source = source_identity()
    expected = expected_package(args.accepted_package)
    original = source_original(root, args.accepted_label, expected)
    before = [observe_actor(original["actors"][key], deadline) for key in ("loader", "query")]
    after = [observe_actor(original["actors"][key], deadline) for key in ("loader", "query")]
    require(actor_authority(before) == actor_authority(after), "actors changed during inspection")
    require(expected_package(args.accepted_package) == expected, "package changed during inspection")
    root.recheck()
    require(boot_id() == boot and source_identity() == source
            and nr.digest(nr.bounded_regular(Path(nr.__file__), LIMIT)[0]) == DEPENDENCY_SHA256,
            "boot or producer changed")
    deadline.remaining()
    stable = ["hermit-accepted-abi11-resource-plan-v1", boot, args.owner_uid, source, DEPENDENCY_SHA256,
              str(root.path), original, actor_authority(before)]
    return {"schema": "hermit-accepted-abi11-resource-inspection-v1", "outcome": "inspected",
            "execution_success": False, "admission_authority": False, "mutations_performed": False,
            "resource_recovery_complete": False, "plan_sha256": nr.digest(nr.wire(stable)),
            "plan": stable, "actor_observations_before": before, "actor_observations_after": after,
            "trust_premise": "private owner evidence and conforming trusted manager; no migration or unit recreation"}



class VerificationDeadline:
    def __init__(self, deadline_ns):
        self.deadline_ns = deadline_ns

    def remaining(self, maximum=5):
        remaining = (self.deadline_ns - time.monotonic_ns()) / 1e9
        require(remaining > 0, "fresh accepted verification deadline expired")
        return min(maximum, remaining)


def artifact_fields(artifact):
    return [artifact["wire_format"], bytes(artifact["topology"]["contract_sha256"]).hex(),
            *(bytes(artifact[key]).hex() for key in ("object_sha256", "library_sha256", "btf_sha256")),
            25, 49, 49]


def certify_locked(args, root, inspection, inspection_deadline):
    """One fresh absence proof; no cleanup actions or repeated intent attempts."""
    require(os.getuid() == os.geteuid() == 0, "certification requires actual privileged scanner parent")
    require(args.expect_inspection_sha256 == inspection["plan_sha256"], "inspected plan digest differs")
    plan = inspection["plan"]
    _, boot, owner, producer, dependency, path, original, authority = plan
    require(dependency == DEPENDENCY_SHA256, "inspection dependency differs")
    started = time.monotonic_ns()
    action_deadline = nr.uint(started + 5_000_000_000)
    require(nr.uint(original["original_closed_ns"] + 1_000_000_000) <= started,
            "original close deadline has not expired")
    before = inspection["actor_observations_after"]
    require(actor_authority(before) == authority
            and all(0 < row[4] <= row[5] <= started for row in before),
            "original actor inspection cut differs")
    accepted = [path, nr.root_fields(original["root"]), original["label"], original["run"],
                [nr.file_fields(file) for file in original["files"]],
                artifact_fields(original["artifact"]), original["original_closed_ns"]]
    ids = original["ids"]
    require(len(ids) == 123 and ids == sorted(ids) and len({tuple(row) for row in ids}) == 123,
            "original accepted ID population differs")
    intent = ["hermit-accepted-abi11-resource-intent-v1", boot, started, action_deadline, owner,
              producer, dependency, accepted, before, ids, nr.digest(nr.wire(ids)[:-1]),
              "incomplete-parent-after-provider-close"]
    intent_raw = nr.wire(intent)
    root.recheck()
    terminal = root.files[0]
    require(terminal.name == f"accepted-b1-{original['label']}.terminal.jsonl",
            "retained original terminal differs")
    terminal.append(intent_raw, LIMIT)
    inspection_deadline.remaining()
    verification = time.monotonic_ns()
    deadline = nr.uint(verification + 1_000_000_000)
    require(started <= verification < action_deadline, "one-use action deadline expired")
    scanner, status, scans = nr.run_scanner(ids, verification, deadline, producer)
    require(status == 0 and isinstance(scanner, list) and len(scanner) == 6
            and type(scanner[0]) is int and 0 < scanner[0] <= 0x7fffffff
            and type(scanner[1]) is int and scanner[1] > 0 and scanner[2:4] == [0, 0]
            and isinstance(scanner[4], str) and re.fullmatch(r"[0-9a-f]{64}", scanner[4])
            and int(scanner[4], 16) != 0 and scanner[5] == producer,
            "actual privileged scanner identity/wait differs")
    fresh = VerificationDeadline(min(deadline, action_deadline))
    after = [observe_actor(original["actors"][key], fresh) for key in ("loader", "query")]
    require(actor_authority(after) == authority, "actors changed after fresh verification")
    require(expected_package(args.accepted_package) == original["artifact"],
            "accepted package changed during certification")
    require(boot_id() == boot and source_identity() == producer
            and nr.digest(nr.bounded_regular(Path(nr.__file__), LIMIT)[0]) == dependency,
            "boot or producer changed during certification")
    root.recheck()
    final = time.monotonic_ns()
    nr.scan_shape(scans, ids, verification, deadline, final)
    require(final <= action_deadline
            and scans[1][1] <= after[0][4] <= after[0][5] <= after[1][4] <= after[1][5] <= final,
            "actor/scanner final chronology differs")
    result = ["hermit-accepted-abi11-resource-result-v1", nr.digest(intent_raw), boot,
              verification, deadline, scanner, status, scans, final, after, producer, dependency]
    result_raw = nr.wire(result)
    terminal.append(result_raw, LIMIT)
    root.recheck()
    return {"schema": "hermit-accepted-abi11-resource-certification-v1", "outcome": "resource-absence-certified",
            "execution_success": False, "normal_terminal_success": False,
            "resource_recovery_complete": True,
            "admission_authority": "only the separately validated accepted-only certificate",
            "original_execution_status": "missing-parent-terminal", "plan_sha256": inspection["plan_sha256"],
            "intent_sha256": nr.digest(intent_raw), "result_sha256": nr.digest(result_raw),
            "verification_started_ns": verification, "verification_final_ns": final,
            "verification_deadline_ns": deadline, "ids_per_pass": len(ids), "passes": 2}


def inspect(args):
    require(type(args.owner_uid) is int and args.owner_uid >= 0, "invalid original owner UID")
    deadline = nr.Deadline()
    with contextlib.ExitStack() as stack:
        root = nr.HeldRoot(args.accepted_root, args.owner_uid, 4096)
        stack.callback(root.close)
        inspection = inspect_locked(args, root, deadline)
        if getattr(args, "action", "inspect") == "certify":
            return certify_locked(args, root, inspection, deadline)
        return inspection


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["inspect", "certify"])
    for option in ("accepted-root", "accepted-label", "accepted-package"):
        parser.add_argument("--" + option, required=True)
    parser.add_argument("--owner-uid", type=int, required=True)
    parser.add_argument("--expect-inspection-sha256")
    args = parser.parse_args()
    try:
        result = inspect(args)
    except (Refused, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        result = {"schema": "hermit-accepted-abi11-resource-inspection-v1", "outcome": "refused",
                  "execution_success": False, "admission_authority": False, "resource_recovery_complete": False,
                  "reason": str(error)}
        print(json.dumps(result, sort_keys=True))
        return 1
    print(json.dumps(result, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
