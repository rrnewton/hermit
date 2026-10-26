#!/usr/bin/env python3
"""Check a native ordinary Detcore install receipt against the installed bytes.

This is the native counterpart of verify-packaged-detcore.py. Keeping these
small install readers in Python preserves their existing packaging interface;
the Cargo reader and its genuine Cargo/SaBRe fields remain separate.

A successful read establishes retained-result consistency only. Neither this
receipt nor its hashes can construct Reverie's opaque source or qualification
authority. The native producer must freshly bind the expected and declared
graphs through qualify_declared_constructor before publishing this receipt.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys


SCHEMA = "hermit-native-packaged-detcore/v1"
RUNTIME = "libdetcore_liteinst.so"
RECEIPT = "libdetcore_liteinst.native-package.json"
ORDINARY_POLICIES = {
    1: "detcore_ordinary_off_alloc101_reviewed_source_current_build_rx_loader_census_v1",
    2: "ordinary_off_alloc101_semantic_v0_reviewed_source_current_build_rx_loader_census_v2",
}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def object_fields(value, fields, label):
    require(isinstance(value, dict) and set(value) == set(fields),
            f"malformed {label}")


def digest(value):
    return isinstance(value, str) and re.fullmatch("[0-9a-f]{64}", value) is not None


def integer(value):
    return type(value) is int and value >= 0


def identity(value):
    object_fields(value, ("path", "sha256"), "file identity")
    require(isinstance(value["path"], str) and Path(value["path"]).is_absolute()
            and "\0" not in value["path"] and digest(value["sha256"]),
            "malformed file identity")


def symbol(value):
    object_fields(value, ("name", "address", "size", "binding", "visibility"),
                  "symbol evidence")
    require(isinstance(value["name"], str) and value["name"]
            and all(integer(value[field]) for field in
                    ("address", "size", "binding", "visibility")),
            "malformed symbol evidence")


def function_hashes(evidence):
    require(isinstance(evidence, dict), "malformed physical result")
    hashes = evidence.get("function_sha256")
    require(isinstance(hashes, dict) and hashes
            and all(isinstance(key, str) and key and digest(value)
                    for key, value in hashes.items()),
            "malformed physical function identities")


def allocator_object(value, descriptor=False):
    fields = ("symbol", "payload_sha256", "file_offset", "section_index", "load_segment_index")
    if descriptor:
        fields += ("version", "mode")
    object_fields(value, fields, "allocator object")
    symbol(value["symbol"])
    require(digest(value["payload_sha256"])
            and all(integer(value[field]) for field in
                    ("file_offset", "section_index", "load_segment_index")),
            "malformed allocator object identity")
    if descriptor:
        require(integer(value["version"]) and value["mode"] == "fixed",
                "native allocator descriptor is not fixed backing")


def compiler_routes(routes):
    require(isinstance(routes, dict) and routes, "missing compiler allocator routes")
    for name, value in routes.items():
        require(isinstance(name, str) and name, "malformed compiler allocator route")
        object_fields(value, ("shim", "implementation", "call_path", "local_got_edges"),
                      "compiler allocator route")
        symbol(value["shim"])
        symbol(value["implementation"])
        require(isinstance(value["call_path"], list)
                and all(integer(address) for address in value["call_path"])
                and isinstance(value["local_got_edges"], list), "malformed compiler allocator route")
        for edge in value["local_got_edges"]:
            object_fields(edge, ("source_function", "instruction", "slot", "target_function"),
                          "local GOT edge")
            require(all(integer(address) for address in edge.values()), "malformed local GOT edge")


def route(evidence, initializer):
    function_hashes(evidence)
    for field in (initializer, "body", "bridge", "prepare", "native_gate",
                  "native_gate_owner", "native_gate_return"):
        symbol(evidence[field])
    require(isinstance(evidence.get("observation"), str) and evidence["observation"],
            "missing constructor observation")


def read_regular(path):
    # Refuse symlinks and special files without following or blocking on them.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode), "packaged file must be regular")
        data = stream.read()
        after = os.fstat(stream.fileno())
        current = path.lstat()
        fields = ("st_dev", "st_ino", "st_mode", "st_size", "st_mtime_ns", "st_ctime_ns")
        require(all(getattr(before, field) == getattr(after, field)
                    == getattr(current, field) for field in fields),
                "packaged file changed while reading")
        return data


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value, f"duplicate receipt field: {key}")
        value[key] = item
    return value


def verify(install):
    resources = Path(install) / "rsrcs"
    record = json.loads(read_regular(resources / RECEIPT), object_pairs_hook=unique_object)
    object_fields(record, ("schema", "native_expected", "native_declared",
                           "native_compilation", "qualification", "installed",
                           "source_close_completed", "commands"), "native install receipt")
    require(record["schema"] == SCHEMA, "unsupported native Detcore packaging receipt")
    for field in ("native_expected", "native_declared", "installed"):
        identity(record[field])
    require(Path(record["installed"]["path"]).name == RUNTIME,
            "receipt names another installed runtime")
    compilation = record["native_compilation"]
    object_fields(compilation, ("product", "evidence", "reused"), "native compilation")
    identity(compilation["product"])
    identity(compilation["evidence"])
    require(compilation["reused"] is True, "native installation did not retain declared rebinding")

    qualification = record["qualification"]
    object_fields(qualification, (
        "kind", "version", "native_sources_sha256", "artifact", "allocator_backing",
        "initializer", "init_array_slot", "init_array_resolution", "constructor",
        "generic_bootstrap", "needed", "required_versions", "readelf_versions",
        "readelf_symbols_relocations", "objdump_disassembly"), "native qualification")
    require(qualification["kind"] == "native_constructor_qualification"
            and type(qualification["version"]) is int and qualification["version"] == 1,
            "receipt is not ordinary native constructor evidence")
    require(digest(qualification["native_sources_sha256"]), "missing native source identity")
    identity(qualification["artifact"])
    require(compilation["product"] == qualification["artifact"],
            "native compilation and qualification identify different products")
    symbol(qualification["initializer"])
    require(qualification["initializer"]["name"] == "detcore_liteinst_initialize",
            "missing ordinary Detcore constructor")
    require(integer(qualification["init_array_slot"])
            and isinstance(qualification["init_array_resolution"], str)
            and qualification["init_array_resolution"], "missing constructor routing evidence")
    for field in ("needed", "required_versions"):
        require(isinstance(qualification[field], list)
                and all(isinstance(item, str) for item in qualification[field]),
                "malformed ELF dependency observations")
    route(qualification["constructor"], "protected_initializer")
    route(qualification["generic_bootstrap"], "public_initializer")
    require(qualification["constructor"]["protected_initializer"]
            == qualification["initializer"], "constructor initializer identity differs")
    require(qualification["generic_bootstrap"]["public_initializer"]["name"]
            == "reverie_liteinst_initialize", "missing generic bootstrap initializer")

    # Native allocator provenance has no Cargo current_build/tls_boundary arm.
    # These are consistency joins in the retained physical result, not a second
    # implementation of ELF/source/constructor admission.
    backing = qualification["allocator_backing"]
    object_fields(backing, ("physical", "no_installation_scope"), "native allocator backing")
    physical = backing["physical"]
    function_hashes(physical)
    allocator_object(physical["descriptor"], descriptor=True)
    allocator_object(physical["fixed_backing_record"])
    compiler_routes(physical["compiler_routes"])
    absence = backing["no_installation_scope"]
    object_fields(absence, (
        "kind", "version", "policy", "elf_sha256", "source", "allocation", "allocation_sha256",
        "mappings", "static_symbol_count", "dynamic_symbol_count", "raw_names_sha256",
        "loader_writer_count", "loader_writers_sha256", "installation_symbols", "installation_getters",
        "installation_writer_count", "other_tls_getters", "other_getter_pointer_slots",
        "other_tls_unisolated", "native_tls_layout100"), "native no-installation-scope result")
    require(absence["kind"] == "no_installation_scope"
            and type(absence["version"]) is int
            and absence["version"] in ORDINARY_POLICIES
            and ORDINARY_POLICIES.get(absence["version"]) == absence["policy"],
            "no-installation-scope result is for another cohort")
    identity(absence["source"])
    symbol(absence["allocation"])
    require(absence["elf_sha256"] == qualification["artifact"]["sha256"],
            "allocator evidence names different runtime bytes")
    require(digest(absence["allocation_sha256"])
            and physical["function_sha256"].get("fixed_patch_alloc")
            == absence["allocation_sha256"], "allocator function identity differs")
    require(all(integer(absence[field]) for field in
                ("static_symbol_count", "dynamic_symbol_count", "loader_writer_count"))
            and digest(absence["raw_names_sha256"]) and digest(absence["loader_writers_sha256"])
            and isinstance(absence["mappings"], list)
            and isinstance(absence["other_tls_getters"], list)
            and isinstance(absence["other_getter_pointer_slots"], list)
            and all(integer(slot) for slot in absence["other_getter_pointer_slots"]),
            "incomplete native allocator census")
    require(absence["installation_symbols"] == [] and absence["installation_getters"] == []
            and type(absence["installation_writer_count"]) is int
            and absence["installation_writer_count"] == 0
            and absence["other_tls_unisolated"] is True
            and absence["native_tls_layout100"] is None,
            "native ordinary allocator observation is inconsistent")

    commands = record["commands"]
    require(isinstance(commands, list) and commands, "missing actual qualification commands")
    for command in commands:
        object_fields(command, ("program", "arguments", "cwd", "status", "elapsed_millis",
                                "stdout", "stderr"), "command evidence")
        require(isinstance(command["program"], str) and command["program"]
                and isinstance(command["arguments"], list)
                and all(isinstance(arg, str) for arg in command["arguments"])
                and isinstance(command["cwd"], str) and Path(command["cwd"]).is_absolute()
                and type(command["status"]) is int and command["status"] == 0
                and integer(command["elapsed_millis"]), "unsuccessful or malformed qualification command")
        identity(command["stdout"])
        identity(command["stderr"])
    artifact_path = qualification["artifact"]["path"]
    for field, args in (
            ("readelf_versions", ["--version-info", "--wide", artifact_path]),
            ("readelf_symbols_relocations", ["--symbols", "--relocs", "--dynamic", "--wide", artifact_path])):
        identity(qualification[field])
        require(any(command["program"] == "readelf" and command["arguments"] == args
                    and command["stdout"] == qualification[field] for command in commands),
                "physical evidence lacks its actual readelf command")
    identity(qualification["objdump_disassembly"])
    require(any(command["program"] == "objdump" and command["arguments"]
                and command["arguments"][-1] == artifact_path for command in commands),
            "physical evidence lacks its actual objdump command")
    require(record["source_close_completed"] is True, "native source/custody transaction did not close")
    installed_digest = hashlib.sha256(read_regular(resources / RUNTIME)).hexdigest()
    require(installed_digest == record["installed"]["sha256"]
            == qualification["artifact"]["sha256"], "installed runtime differs from qualified bytes")
    return record


def main(argv=None):
    parser = argparse.ArgumentParser(
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description=f"Check a native ordinary Detcore installation.\nSchema: {SCHEMA}\n\n"
                    "This checks retained evidence and installed bytes; it does not establish\n"
                    "source authority or repeat physical qualification.",
        epilog="For build-time staging, run the native stage_detcore action with its actual\n"
               "expected/declared inputs as described in hermit-install/native/README.md.\n"
               "For a copied package, supply the complete native installation. Reading a\n"
               "complete installation needs no source checkout, Buck outputs, or compiler.")
    parser.add_argument("install", metavar="INSTALL",
                        help=f"installation root containing rsrcs/{RUNTIME} and rsrcs/{RECEIPT}")
    args = parser.parse_args(argv)
    try:
        verify(args.install)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"verify-native-packaged-detcore: {error}. "
              "Supply a complete native installation from its stage_detcore action; "
              "see --help for staging and package requirements.", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
