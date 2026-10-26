#!/usr/bin/env python3
"""Native receipt transport controls, never source/ELF qualification evidence."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


READER = Path(__file__).with_name("verify-native-packaged-detcore.py")
spec = importlib.util.spec_from_file_location("native_packaged_detcore", READER)
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


def symbol(name):
    return {"name": name, "address": 4096, "size": 32, "binding": 1, "visibility": 3}


def fixture(root):
    """Make real transport files around deliberately non-ELF synthetic bytes."""
    original = root / "original-build"
    original.mkdir()
    resources = root / "install" / "rsrcs"
    resources.mkdir(parents=True)

    def file(name, data=b"synthetic transport evidence; not physical proof"):
        path = original / name
        path.write_bytes(data)
        return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest()}

    product = file("libdetcore_liteinst.so", b"synthetic native install bytes, not an ELF")
    runtime = resources / verifier.RUNTIME
    shutil.copyfile(product["path"], runtime)
    installed = {"path": str(runtime), "sha256": product["sha256"]}
    version_log = file("readelf-versions.stdout")
    symbols_log = file("readelf-symbols.stdout")
    disassembly = file("objdump-scope.stdout")
    empty_log = file("command.stderr", b"")

    def command(program, arguments, stdout):
        return {"program": program, "arguments": arguments, "cwd": str(original),
                "status": 0, "elapsed_millis": 1, "stdout": stdout, "stderr": empty_log}

    def route(initializer, name):
        evidence = {field: symbol(field) for field in
                    ("body", "bridge", "prepare", "native_gate", "native_gate_owner", "native_gate_return")}
        evidence.update({initializer: symbol(name), "function_sha256": {"fixture": "4" * 64},
                         "observation": "synthetic reader fixture only"})
        return evidence

    # These result-shape fixtures exercise receipt consistency. They deliberately
    # cannot satisfy the source factory or the native physical ELF validator.
    fixed_backing = {"symbol": symbol("backing"), "payload_sha256": "2" * 64,
                     "file_offset": 128, "section_index": 1, "load_segment_index": 1}
    physical = {
        "descriptor": dict(fixed_backing, mode="fixed", version=1),
        "fixed_backing_record": fixed_backing,
        "compiler_routes": {"alloc": {"shim": symbol("shim"), "implementation": symbol("impl"),
                                      "call_path": [4096], "local_got_edges": []}},
        "function_sha256": {"fixed_patch_alloc": "3" * 64},
    }
    absence = {
        "kind": "no_installation_scope", "version": 1, "policy": verifier.ORDINARY_POLICIES[1],
        "elf_sha256": product["sha256"], "source": file("patch_alloc.rs"),
        "allocation": symbol("fixed_patch_alloc"), "allocation_sha256": "3" * 64,
        "installation_symbols": [], "installation_getters": [], "installation_writer_count": 0,
        "mappings": [], "static_symbol_count": 1, "dynamic_symbol_count": 1,
        "raw_names_sha256": "5" * 64, "loader_writer_count": 0, "loader_writers_sha256": "6" * 64,
        "other_tls_getters": [], "other_getter_pointer_slots": [],
        "other_tls_unisolated": True, "native_tls_layout100": None,
    }
    record = {
        "schema": verifier.SCHEMA,
        "native_expected": file("expected.json"), "native_declared": file("declared.json"),
        "native_compilation": {"product": product, "evidence": file("producer-evidence.json"), "reused": True},
        "qualification": {
            "kind": "native_constructor_qualification", "version": 1, "native_sources_sha256": "1" * 64,
            "artifact": product, "initializer": symbol("detcore_liteinst_initialize"),
            "init_array_slot": 8192, "init_array_resolution": "synthetic transport fixture",
            "allocator_backing": {"physical": physical, "no_installation_scope": absence},
            "constructor": route("protected_initializer", "detcore_liteinst_initialize"),
            "generic_bootstrap": route("public_initializer", "reverie_liteinst_initialize"),
            "needed": [], "required_versions": [], "readelf_versions": version_log,
            "readelf_symbols_relocations": symbols_log, "objdump_disassembly": disassembly,
        },
        "installed": installed, "source_close_completed": True,
        "commands": [
            command("readelf", ["--version-info", "--wide", product["path"]], version_log),
            command("objdump", ["--disassemble", product["path"]], disassembly),
            command("readelf", ["--symbols", "--relocs", "--dynamic", "--wide", product["path"]], symbols_log),
        ],
    }
    # JSON is a value tree: mutations must not share Python object aliases and
    # accidentally alter both sides of the consistency check under test.
    record = json.loads(json.dumps(record))
    (resources / verifier.RECEIPT).write_text(json.dumps(record))
    return record


class NativeReceiptControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.record = fixture(self.root)
        self.install = self.root / "install"
        self.resources = self.install / "rsrcs"
        self.receipt = self.resources / verifier.RECEIPT

    def write(self, record):
        self.receipt.write_text(json.dumps(record))

    def test_retained_native_result_accepts_both_existing_ordinary_name_encodings(self):
        for version, policy in verifier.ORDINARY_POLICIES.items():
            with self.subTest(version=version):
                absence = self.record["qualification"]["allocator_backing"]["no_installation_scope"]
                absence.update(version=version, policy=policy)
                self.write(self.record)
                self.assertEqual(verifier.verify(self.install), self.record)

    def test_relocated_install_needs_no_checkout_build_outputs_logs_or_compiler(self):
        relocated = self.root / "copied-package"
        shutil.move(self.install, relocated)
        shutil.rmtree(self.root / "original-build")
        self.assertFalse(Path(self.record["native_compilation"]["product"]["path"]).exists())
        self.assertFalse(Path(self.record["native_expected"]["path"]).exists())
        self.assertEqual(verifier.verify(relocated), self.record)
        result = subprocess.run([sys.executable, "-B", str(READER), str(relocated)],
                                capture_output=True, text=True, env={"PATH": ""}, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_schema_kind_product_source_close_and_command_refusals(self):
        mutations = [
            (("schema",), "hermit-packaged-detcore/v1"),
            (("qualification", "kind"), "native_entry_qualification"),
            (("qualification", "version"), True),
            (("qualification", "native_sources_sha256"), "not-a-hash"),
            (("qualification", "initializer", "name"), "detcore_liteinst_exec_initialize"),
            (("qualification", "constructor", "protected_initializer", "address"), 999),
            (("qualification", "generic_bootstrap", "public_initializer", "name"), "foreign_entry"),
            (("native_compilation", "product", "path"), "/another/product.so"),
            (("native_compilation", "evidence", "sha256"), "X" * 64),
            (("native_compilation", "reused"), False),
            (("native_expected", "path"), "relative/expected.json"),
            (("native_declared", "sha256"), ""),
            (("installed", "sha256"), "0" * 64),
            (("installed", "path"), "/other/runtime.so"),
            (("source_close_completed",), False),
            (("source_close_completed",), 1),
            (("commands",), []),
            (("commands", 0, "status"), 1),
            (("commands", 0, "status"), False),
            (("commands", 0, "stdout", "sha256"), "0" * 64),
            (("commands", 0, "arguments"), ["--version-info", "--wide", "/foreign.so"]),
            (("commands", 1, "program"), "cargo"),
        ]
        for path, value in mutations:
            with self.subTest(path=path, value=value):
                bad = copy.deepcopy(self.record)
                parent = bad
                for key in path[:-1]:
                    parent = parent[key]
                parent[path[-1]] = value
                self.write(bad)
                with self.assertRaises(ValueError):
                    verifier.verify(self.install)

    def test_native_allocator_cannot_be_replaced_by_cargo_or_other_entry_evidence(self):
        original = self.record["qualification"]["allocator_backing"]["no_installation_scope"]
        for field, value in (
                ("policy", "detcore_native_off_alloc101_reviewed_source_current_build_rx_loader_census_v1"),
                ("policy", "standalone_release_off_alloc62_reviewed_source_current_build_rx_loader_census_v1"),
                ("version", 99), ("elf_sha256", "0" * 64), ("allocation_sha256", "0" * 64),
                ("installation_symbols", ["installation"]), ("installation_getters", [4096]),
                ("installation_writer_count", 1), ("other_tls_unisolated", False),
                ("native_tls_layout100", {})):
            with self.subTest(field=field):
                bad = copy.deepcopy(self.record)
                bad["qualification"]["allocator_backing"]["no_installation_scope"] = dict(original, **{field: value})
                self.write(bad)
                with self.assertRaises(ValueError):
                    verifier.verify(self.install)
        bad = copy.deepcopy(self.record)
        bad["qualification"]["allocator_backing"] = {"tls_boundary": {"NoInstallationScope": original}}
        self.write(bad)
        with self.assertRaises(ValueError):
            verifier.verify(self.install)

    def test_receipt_digest_or_empty_physical_result_cannot_replace_the_native_result(self):
        for field, value in (("qualification", {"sha256": self.record["installed"]["sha256"]}),
                             ("native_expected", {"sha256": "1" * 64})):
            with self.subTest(field=field):
                self.write(dict(self.record, **{field: value}))
                with self.assertRaises(ValueError):
                    verifier.verify(self.install)
        for field in ("constructor", "generic_bootstrap", "allocator_backing"):
            bad = copy.deepcopy(self.record)
            bad["qualification"][field] = {}
            self.write(bad)
            with self.assertRaises(ValueError):
                verifier.verify(self.install)
        for field in ("descriptor", "fixed_backing_record", "compiler_routes"):
            bad = copy.deepcopy(self.record)
            bad["qualification"]["allocator_backing"]["physical"][field] = {"success": True}
            self.write(bad)
            with self.assertRaises(ValueError):
                verifier.verify(self.install)

    def test_installed_bytes_are_checked_and_missing_receipt_is_refused(self):
        runtime = self.resources / verifier.RUNTIME
        runtime.write_bytes(b"replaced installed bytes")
        with self.assertRaisesRegex(ValueError, "installed runtime differs"):
            verifier.verify(self.install)
        self.receipt.unlink()
        with self.assertRaises(FileNotFoundError):
            verifier.verify(self.install)

    def test_symlinks_and_special_files_are_refused_without_blocking(self):
        for name in (verifier.RUNTIME, verifier.RECEIPT):
            with self.subTest(name=name):
                path = self.resources / name
                saved = self.root / name
                shutil.move(path, saved)
                path.symlink_to(saved)
                with self.assertRaises(OSError):
                    verifier.verify(self.install)
                path.unlink()
                os.mkfifo(path)
                with self.assertRaisesRegex(ValueError, "must be regular"):
                    verifier.verify(self.install)
                path.unlink()
                shutil.move(saved, path)

    def test_replacement_during_read_is_refused(self):
        path = self.resources / verifier.RUNTIME
        real_lstat = Path.lstat

        def replace_then_stat(current):
            if current == path:
                replacement = self.root / "replacement"
                replacement.write_bytes(path.read_bytes())
                os.replace(replacement, path)
            return real_lstat(current)

        with mock.patch.object(Path, "lstat", replace_then_stat):
            with self.assertRaisesRegex(ValueError, "changed while reading"):
                verifier.read_regular(path)

    def test_duplicate_fields_and_cargo_only_receipt_are_refused(self):
        text = json.dumps(self.record)
        self.receipt.write_text('{"source_close_completed": false,' + text[1:])
        with self.assertRaisesRegex(ValueError, "duplicate receipt field"):
            verifier.verify(self.install)
        self.receipt.unlink()
        (self.resources / "libdetcore_liteinst.package.json").write_text(text)
        with self.assertRaises(FileNotFoundError):
            verifier.verify(self.install)

    def test_cli_refusal_is_nonzero_and_does_not_claim_qualification(self):
        self.record["source_close_completed"] = False
        self.write(self.record)
        result = subprocess.run([sys.executable, "-B", str(READER), str(self.install)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 2)
        self.assertIn("native source/custody transaction did not close", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_cli_help_forms_explain_schema_and_retained_evidence_boundary(self):
        self.receipt.unlink()
        for flag in ("-h", "--help"):
            with self.subTest(flag=flag):
                result = subprocess.run([sys.executable, "-B", str(READER), flag],
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, "")
                self.assertIn("usage:", result.stdout)
                self.assertIn("INSTALL", result.stdout)
                self.assertIn(verifier.SCHEMA, result.stdout)
                self.assertIn("retained evidence", result.stdout)
                self.assertIn("physical qualification", result.stdout)
                self.assertIn("stage_detcore", result.stdout)
                self.assertIn(verifier.RECEIPT, result.stdout)

    def test_cli_missing_receipt_points_to_native_stage_and_help(self):
        self.receipt.unlink()
        result = subprocess.run([sys.executable, "-B", str(READER), str(self.install)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")
        self.assertIn(verifier.RECEIPT, result.stderr)
        self.assertIn("complete native installation", result.stderr)
        self.assertIn("stage_detcore", result.stderr)
        self.assertIn("--help", result.stderr)


if __name__ == "__main__":
    unittest.main()
