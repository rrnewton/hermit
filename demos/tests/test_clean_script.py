#!/usr/bin/env python3
"""Tests that demos/clean.sh removes only files under the QEMU asset directory.

clean.sh removes the temporary files an interrupted QEMU demo run can leave in
the asset directory, such as `.bzImage.<suffix>`, by matching patterns under
that directory. A pattern written with an unquoted `*` in the array that holds
it expands when the array is defined, against the directory clean.sh was
started from, not the asset directory. A file there named `.bzImage.a b` then
replaced the pattern; the loop that removes the matches split it into
`<assets>/.bzImage.a` and `b` and removed `b` from the caller's directory, and
a name with a ` *` in it removed every entry there. The patterns are quoted, so
they expand only under the asset directory.

Each test runs a copy of clean.sh inside a temporary directory that stands in
for the repository, so its removals under `<repo>/target/` stay in that
temporary directory, and points QEMU_ASSETS at a temporary asset directory.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


DEMO_DIR = Path(__file__).resolve().parent.parent

# Files that clean.sh's patterns match: one for each pattern, under the asset
# directory.
TRANSIENT_FILES = (
    "run-metadata.json.tmp.456",
    "hermit-boot.qcow2.tmp.789",
    ".bzImage.123",
    ".initramfs.cpio.gz.1",
    ".initramfs-version.2",
    ".vmlinux.3",
    ".vmlinux-types.4",
)

# Files under the asset directory that clean.sh without --distclean keeps: the
# downloaded and built inputs, and a file the demos do not create.
KEPT_ASSET_FILES = (
    "bzImage",
    "initramfs.cpio.gz",
    ".initramfs-version",
    "vmlinux",
    "notes.txt",
)

# Entries in the directory clean.sh is started from. The first two have names
# that the patterns `.bzImage.*` and `.vmlinux.*` match and that contain a
# space; split at that space they give the words `b` and `*`.
CALLER_FILES = (
    ".bzImage.a b",
    ".vmlinux.x *",
    "b",
    "keep1",
    "keepdir/inner",
)


def _snapshot(directory: Path) -> dict:
    """Return every file and directory below DIRECTORY with its contents."""
    entries = {}
    for path in sorted(directory.rglob("*")):
        relative = str(path.relative_to(directory))
        if path.is_dir():
            entries[relative + "/"] = None
        else:
            entries[relative] = path.read_bytes()
    return entries


class CleanTransientGlobTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="demo-clean-")
        self.addCleanup(temporary.cleanup)
        base = Path(temporary.name)

        # A copy of clean.sh and the helper it sources, in a stand-in
        # repository: clean.sh takes the repository as the parent of its own
        # directory.
        self.root = base / "repo"
        (self.root / "demos/lib").mkdir(parents=True)
        self.script = self.root / "demos/clean.sh"
        shutil.copy2(DEMO_DIR / "clean.sh", self.script)
        shutil.copy2(DEMO_DIR / "lib/qemu-paths.sh", self.root / "demos/lib/qemu-paths.sh")

        self.assets = base / "assets"
        self.assets.mkdir()
        for name in TRANSIENT_FILES + KEPT_ASSET_FILES:
            (self.assets / name).write_text(name + "\n")

        self.caller = base / "caller"
        self.caller.mkdir()
        for name in CALLER_FILES:
            path = self.caller / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name + "\n")
        self.caller_before = _snapshot(self.caller)

    def _clean(self, *arguments):
        environment = dict(os.environ)
        environment["QEMU_ASSETS"] = str(self.assets)
        return subprocess.run(
            ["bash", str(self.script), *arguments],
            cwd=self.caller,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )

    def test_files_in_the_callers_directory_are_never_removed(self):
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(_snapshot(self.caller), self.caller_before, completed.stdout)

    def test_matching_files_under_the_asset_directory_are_removed(self):
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(KEPT_ASSET_FILES),
            completed.stdout,
        )

    def test_the_dry_run_names_exactly_the_matching_asset_files(self):
        completed = self._clean("--dry-run")
        self.assertEqual(completed.returncode, 0, completed.stdout)
        named = sorted(
            line.removeprefix("  would remove ")
            for line in completed.stdout.splitlines()
            if line.startswith("  would remove ")
        )
        self.assertEqual(
            named,
            sorted(str(self.assets / name) for name in TRANSIENT_FILES),
            completed.stdout,
        )
        self.assertEqual(_snapshot(self.caller), self.caller_before, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(TRANSIENT_FILES + KEPT_ASSET_FILES),
        )

    def test_matching_files_are_removed_when_the_caller_has_none(self):
        # Positive control: with no matching names in the caller's directory,
        # the patterns reach the asset directory with or without the quoting.
        for name in CALLER_FILES[:2]:
            (self.caller / name).unlink()
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(KEPT_ASSET_FILES),
            completed.stdout,
        )
        self.assertEqual(
            sorted(_snapshot(self.caller)),
            ["b", "keep1", "keepdir/", "keepdir/inner"],
        )


if __name__ == "__main__":
    unittest.main()
