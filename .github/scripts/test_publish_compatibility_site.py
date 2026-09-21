"""Small regression tests for nightly append-only publication handoffs."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / ".github/scripts/publish-compatibility-site.py"
PINS = ROOT / ".github/compatibility-site-releases.json"


class NightlyRegistryTests(unittest.TestCase):
    def test_previous_publication_cannot_be_dropped_or_rewritten(self):
        baseline = json.loads(PINS.read_bytes())
        added = copy.deepcopy(baseline["releases"][-1])
        added.update(identity="f" * 64, tag="compatibility-website-" + "f" * 64,
                     asset="compatibility-website-" + "f" * 64 + ".tar.gz")
        previous = copy.deepcopy(baseline)
        previous["releases"] = sorted([*previous["releases"], added], key=lambda pin: pin["identity"])
        previous["latest_identity"] = added["identity"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old, proposed = root / "published.json", root / "proposed.json"
            old.write_text(json.dumps(previous))
            def check(value):
                proposed.write_text(json.dumps(value))
                return subprocess.run([sys.executable, str(HELPER), "validate-update", str(proposed), str(PINS), str(ROOT), str(old)], capture_output=True)
            self.assertNotEqual(check(baseline).returncode, 0)
            self.assertEqual(check(previous).returncode, 0)
            changed = copy.deepcopy(previous)
            changed["releases"][-1]["archive_sha256"] = "e" * 64
            self.assertNotEqual(check(changed).returncode, 0)

    def test_describe_reports_actual_tree_and_manifest_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "assets").mkdir()
            (root / "assets/style.css").write_bytes(b"body {}")
            build = {"freshness_sha256": "a" * 64, "artifacts_sha256": "b" * 64,
                     "tree_sha256": "c" * 64, "counts": {"cells": 1},
                     "directories": [{"path": "assets", "mode": 0o555}]}
            (root / "build.json").write_text(json.dumps(build))
            result = subprocess.run([sys.executable, str(HELPER), "describe", str(root)], capture_output=True, check=True)
            described = json.loads(result.stdout)
            self.assertEqual(described["directories"], ["assets"])
            self.assertEqual(described["file_count"], 2)
            self.assertEqual(described["file_bytes"], sum(path.stat().st_size for path in root.rglob("*") if path.is_file()))


if __name__ == "__main__":
    unittest.main()
