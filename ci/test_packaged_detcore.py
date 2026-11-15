#!/usr/bin/env python3
"""Receipt-reader controls. Synthetic receipts are not constructor qualification."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("packaged_detcore", Path(__file__).with_name("verify-packaged-detcore.py"))
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


def fixture_receipt(install, pin, *, core=False):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    source = {"path": "/fixture/Cargo.toml", "sha256": "1" * 64}
    package = {"name": "detcore-liteinst", "target": "detcore_liteinst", "source": None,
               "kind": ["cdylib"], "crate_types": ["cdylib"]}
    artifact = "/fixture/target/liteinst-runtime/libdetcore_liteinst.so"
    args = ["build", "--locked", "--offline", "--manifest-path", source["path"],
            "-p", "detcore-liteinst", "--lib", "--no-default-features", "--profile", "liteinst-runtime",
            "--target-dir", "/fixture/target", "--message-format=json-render-diagnostics"]
    # Shape-only transport fixtures, not actual emitted machine-code evidence.
    backing = {name: {"fixture": True} for name in ("descriptor", "fixed_backing_record", "tls_boundary", "compiler_routes")}
    backing["function_sha256"] = {"fixture": "3" * 64}
    constructor = {name: {"fixture": True} for name in ("body", "bridge", "prepare", "native_gate")}
    constructor.update(function_sha256={"fixture": "4" * 64}, observation="synthetic receipt transport")
    record = {
        "schema": "hermit-packaged-detcore/v1", "reverie_pin": pin,
        "profile_name": "liteinst-runtime", "profile_inputs": source,
        "profile_objects": {"liteinst-runtime": {"inherits": "validate", "lto": False}},
        "cargo_artifact": {"features": [], "profile": {"opt_level": "3", "debuginfo": 0,
            "debug_assertions": True, "overflow_checks": True, "test": False},
            "package": package, "reported_path": artifact},
        "compiled_sources": {"image_kind": "Detcore", "reverie_pin": pin,
            "manifest": source, "leaf": package, "reverie_root": "/fixture/reverie"},
        "qualifier_source": {"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs", "sha256": "2" * 64}, "compiled_qualifier_sha256": "2" * 64,
        "qualification": {"initializer": {"name": "detcore_liteinst_initialize"},
            "allocator_backing": backing, "detcore_constructor": constructor,
            "generic_bootstrap": constructor, "artifact": {"path": artifact, "sha256": digest}},
        "installed": {"sha256": digest}, "cargo_argv": args,
        "commands": [{"status": 0, "program": "cargo", "arguments": args}],
        "source_close_completed": True}
    (resources / ("libreverie_liteinst.so.revision" if core else "sabre.revision")).write_text(pin + "\n")
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


def buck_fixture_receipt(install, pin, *, core=False):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    # The Buck arm's profile input is the reviewed profile definition, and
    # its compiled_sources manifest slot carries that same file identity.
    source = {"path": "/fixture/fb-only/liteinst-runtime-profile.bzl", "sha256": "1" * 64}
    package = {"name": "detcore-liteinst", "target": "detcore_liteinst", "source": None,
               "kind": ["cdylib"], "crate_types": ["cdylib"]}
    artifact = "/fixture/buck-out/libdetcore_liteinst.so"
    flags = ["-Copt-level=3", "-Cdebuginfo=0", "-Cdebug-assertions=yes",
             "-Coverflow-checks=yes", "-Ccodegen-units=16",
             "-Cembed-bitcode=no", "-Cpanic=unwind", "-Cstrip=none"]
    target = "//hermetic_infra/hermit:detcore-liteinst[cdylib]"
    platform = "ovr_config//platform/linux:x86_64-fbcode"
    args = ["build", target, "--target-platforms", platform]
    # Shape-only transport fixtures, not actual emitted machine-code evidence.
    backing = {name: {"fixture": True} for name in ("descriptor", "fixed_backing_record", "tls_boundary", "compiler_routes")}
    backing["function_sha256"] = {"fixture": "3" * 64}
    constructor = {name: {"fixture": True} for name in ("body", "bridge", "prepare", "native_gate")}
    constructor.update(function_sha256={"fixture": "4" * 64}, observation="synthetic receipt transport")
    record = {
        "schema": "hermit-packaged-detcore/v1", "builder": "buck2", "reverie_pin": pin,
        "profile_name": "liteinst-runtime", "profile_inputs": source,
        "profile_objects": {"liteinst-runtime": {"rustc_flags": flags}},
        "buck_target": target, "buck_platform": platform,
        "buck_constraints": ["ovr_config//cpu:x86_64", "ovr_config//os:linux"],
        "buck_artifact": {"features": [], "profile": {"opt_level": "3", "debuginfo": 0,
            "debug_assertions": True, "overflow_checks": True, "test": False},
            "package": package, "reported_path": artifact},
        "compiled_sources": {"image_kind": "Detcore", "reverie_pin": pin,
            "manifest": source, "leaf": package, "reverie_root": "/fixture/reverie"},
        "qualifier_source": {"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs", "sha256": "2" * 64}, "compiled_qualifier_sha256": "2" * 64,
        "qualification": {"initializer": {"name": "detcore_liteinst_initialize"},
            "allocator_backing": backing, "detcore_constructor": constructor,
            "generic_bootstrap": constructor, "artifact": {"path": artifact, "sha256": digest}},
        "installed": {"sha256": digest}, "buck_argv": args,
        "commands": [{"status": 0, "program": "readelf", "arguments": ["--version-info", "--wide", artifact]},
                     {"status": 0, "program": "objdump", "arguments": ["--disassemble", artifact]}],
        "source_close_completed": True}
    (resources / ("libreverie_liteinst.so.revision" if core else "sabre.revision")).write_text(pin + "\n")
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


class ReceiptControls(unittest.TestCase):
    def test_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            good = fixture_receipt(install, "a" * 40)
            verifier.verify(install)
            for field, value in [("profile_name", "validate"), ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install)
            for section, field, value in [("cargo_artifact", "features", ["allocator-fixture"]),
                    ("compiled_sources", "reverie_pin", "b" * 40),
                    ("installed", "sha256", "0" * 64),
                    ("qualifier_source", "path", "/wrong/proof.rs"),
                    ("cargo_artifact", "reported_path", "/wrong/libdetcore_liteinst.so"),
                    ("qualification", "generic_bootstrap", {}),
                    ("qualification", "allocator_backing", {"success": False}),
                    ("qualification", "detcore_constructor", {"allocation_length": 232}),
                    ("commands", 0, {"status": 1})]:
                bad = copy.deepcopy(good)
                bad[section][field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install)


    def test_core_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            good = fixture_receipt(install, "a" * 40, core=True)
            verifier.verify(install, core=True)
            marker=resources/'libreverie_liteinst.so.revision'
            marker.write_text('b'*40+'\n')
            with self.assertRaisesRegex(ValueError,'different Reverie pins'):
                verifier.verify(install,core=True)
            marker.write_text('a'*40+'\n')
            for field, value in [("profile_name", "validate"), ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install, core=True)
            for section, field, value in [("cargo_artifact", "features", ["allocator-fixture"]),
                    ("compiled_sources", "reverie_pin", "b" * 40),
                    ("installed", "sha256", "0" * 64),
                    ("qualifier_source", "path", "/wrong/proof.rs"),
                    ("cargo_artifact", "reported_path", "/wrong/libdetcore_liteinst.so"),
                    ("qualification", "generic_bootstrap", {}),
                    ("qualification", "allocator_backing", {"success": False}),
                    ("qualification", "detcore_constructor", {"allocation_length": 232}),
                    ("commands", 0, {"status": 1})]:
                bad = copy.deepcopy(good)
                bad[section][field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install, core=True)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install, core=True)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install, core=True)


    def _buck_controls(self, core):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            # Round-trip so nested mutations cannot alias fixture dicts.
            good = json.loads(json.dumps(buck_fixture_receipt(install, "a" * 40, core=core)))
            verifier.verify(install, core=core)

            def refused(bad):
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install, core=core)

            for field, value in [("schema", "hermit-packaged-detcore/v2"),
                    ("reverie_pin", "b" * 40), ("profile_name", "validate"),
                    ("buck_target", "//hermetic_infra/hermit:detcore-liteinst"),
                    ("buck_platform", "ovr_config//platform/linux:aarch64-fbcode"),
                    ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                refused(bad)
            for section, field, value in [("buck_artifact", "features", ["allocator-fixture"]),
                    ("compiled_sources", "reverie_pin", "b" * 40),
                    ("compiled_sources", "image_kind", "Other"),
                    ("profile_inputs", "sha256", "0" * 64),
                    ("installed", "sha256", "0" * 64),
                    ("qualifier_source", "path", "/wrong/proof.rs"),
                    ("qualifier_source", "sha256", "0" * 64),
                    ("compiled_qualifier_sha256", None, "0" * 64),
                    ("buck_artifact", "reported_path", "/wrong/libdetcore_liteinst.so"),
                    ("qualification", "generic_bootstrap", {}),
                    ("qualification", "allocator_backing", {"success": False}),
                    ("qualification", "detcore_constructor", {"allocation_length": 232})]:
                bad = copy.deepcopy(good)
                if field is None:
                    bad[section] = value
                else:
                    bad[section][field] = value
                refused(bad)
            for field, value in [("opt_level", "2"), ("debuginfo", 2),
                    ("debug_assertions", False), ("overflow_checks", False),
                    ("test", True)]:
                bad = copy.deepcopy(good)
                bad["buck_artifact"]["profile"][field] = value
                refused(bad)
            for field, value in [("name", "other"), ("target", "other"),
                    ("source", "registry"), ("kind", ["lib"]),
                    ("crate_types", ["lib"])]:
                bad = copy.deepcopy(good)
                bad["buck_artifact"]["package"][field] = value
                refused(bad)
            flags = good["profile_objects"]["liteinst-runtime"]["rustc_flags"]
            for index, flag in enumerate(flags):
                bad = copy.deepcopy(good)
                bad["profile_objects"]["liteinst-runtime"]["rustc_flags"][index] = flag + "X"
                refused(bad)
            for mutated in [flags + ["-Clto=off"], flags + ["-Clto=thin"],
                    flags[:-1], flags[::-1]]:
                bad = copy.deepcopy(good)
                bad["profile_objects"]["liteinst-runtime"]["rustc_flags"] = mutated
                refused(bad)
            bad = copy.deepcopy(good)
            bad["profile_objects"]["liteinst-runtime"]["lto"] = False
            refused(bad)
            bad = copy.deepcopy(good)
            bad["profile_objects"]["validate"] = {"inherits": "release"}
            refused(bad)
            for index, argument in enumerate(good["buck_argv"]):
                bad = copy.deepcopy(good)
                bad["buck_argv"][index] = argument + "X"
                refused(bad)
            for index, constraint in enumerate(good["buck_constraints"]):
                bad = copy.deepcopy(good)
                bad["buck_constraints"][index] = constraint + "X"
                refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = good["buck_constraints"][::-1]
            refused(bad)
            for commands in [[{"status": 1, "program": "readelf", "arguments": []},
                              {"status": 0, "program": "objdump", "arguments": []}],
                    [{"status": 0, "program": "readelf", "arguments": []}],
                    [{"status": 0, "program": "objdump", "arguments": []}],
                    []]:
                bad = copy.deepcopy(good)
                bad["commands"] = commands
                refused(bad)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install, core=core)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install, core=core)

    def test_buck_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        self._buck_controls(core=False)

    def test_core_buck_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        self._buck_controls(core=True)

    def test_builder_dispatch_refuses_mixed_and_unknown_provenance(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            (resources / "libdetcore_liteinst.so").write_bytes(
                b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            cargo = fixture_receipt(install, "a" * 40)
            buck = buck_fixture_receipt(install, "a" * 40)

            def refused(record):
                receipt.write_text(json.dumps(record))
                with self.assertRaises(ValueError):
                    verifier.verify(install)

            # An explicit cargo builder takes the same arm as its absence.
            explicit = copy.deepcopy(cargo)
            explicit["builder"] = "cargo"
            receipt.write_text(json.dumps(explicit))
            verifier.verify(install)
            receipt.write_text(json.dumps(buck))
            verifier.verify(install)
            for key in ("cargo_argv", "cargo_artifact", "cargo_version"):
                bad = copy.deepcopy(buck)
                bad[key] = cargo.get(key, "synthetic")
                refused(bad)
            for key in ("buck_argv", "buck_artifact", "buck_target"):
                bad = copy.deepcopy(cargo)
                bad[key] = buck[key]
                refused(bad)
            bad = copy.deepcopy(cargo)
            bad["builder"] = "buck2"
            refused(bad)
            bad = copy.deepcopy(buck)
            del bad["builder"]
            refused(bad)
            bad = copy.deepcopy(buck)
            bad["builder"] = "ninja"
            refused(bad)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] in ("--write-fixture", "--write-core-fixture"):
        fixture_receipt(sys.argv[2], sys.argv[3], core=sys.argv[1] == "--write-core-fixture")
    elif len(sys.argv) == 4 and sys.argv[1] in ("--write-buck-fixture", "--write-core-buck-fixture"):
        buck_fixture_receipt(sys.argv[2], sys.argv[3], core=sys.argv[1] == "--write-core-buck-fixture")
    else:
        unittest.main()
