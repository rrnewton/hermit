#!/usr/bin/env python3
"""Demo 6 restores only a boot snapshot of the initramfs that is built now.

Background. Demo 5 (demos/05-qemu-boot/run.py) boots Linux under Hermit from
the kernel and initramfs that demos/lib/qemu-assets.sh builds, and saves the
running machine as a qcow2 snapshot: the boot snapshot. Demo 6
(demos/06-qemu-resume/run.py) restores that snapshot instead of booting. The
guest's /init, which reads demo 6's command, runs it and frames its output
(the command frame), is therefore the /init in the snapshot's memory, not the
one in the initramfs on disk. An /init of frame format 2 or older runs the
command as root with the console as its standard input, so the command can
print a whole frame itself and end the resume with a forged exit status while
it keeps running (https://github.com/rrnewton/hermit/pull/3497).

Demo 5 now writes a record next to each boot snapshot it saves,
<snapshot>.producer.json, naming the snapshot's SHA-256 and the
INITRAMFS_VERSION and SHA-256 of the initramfs it booted. Demo 6 restores the
default snapshot only when that record matches the snapshot and the initramfs
it would use now, and runs demo 5 again to rebuild it otherwise; it refuses a
snapshot named by QEMU_BOOT_SNAPSHOT_DISK without such a record, saying why and
how to rebuild it. These tests run the record functions of demo_common on
stand-in files, and demo 5's boot_once and demo 6's ensure_boot_snapshot and
resume_once with QEMU, Hermit and demo 5 replaced.
"""

import contextlib
import hashlib
import io
import json
import os
import runpy
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

DEMOS_DIR = Path(__file__).resolve().parent.parent
REPOSITORY_ROOT = DEMOS_DIR.parent
sys.path.insert(0, str(DEMOS_DIR / "lib"))

import demo_common as dc  # noqa: E402

INITRAMFS = b"stand-in for the initramfs"
SNAPSHOT = b"stand-in for the demo 5 boot snapshot"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _write_assets_script(root: Path, version: int) -> None:
    """Make ``root`` a stand-in checkout whose qemu-assets.sh builds ``version``."""
    script = root / "demos/lib/qemu-assets.sh"
    script.parent.mkdir(parents=True, exist_ok=True)
    script.write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        "# Change the /init below together and bump INITRAMFS_VERSION.\n"
        "INITRAMFS_VERSION={}\n"
        'INITRAMFS_VERSION_FILE="$ARTIFACT_DIR/.initramfs-version"\n'.format(version)
    )


class _StandIns(unittest.TestCase):
    """A stand-in checkout at initramfs version 9, an initramfs, a snapshot."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.root = self.directory / "checkout"
        _write_assets_script(self.root, 9)
        self.assets = self.directory / "assets"
        self.assets.mkdir()
        self.initramfs = self.assets / "initramfs.cpio.gz"
        self.initramfs.write_bytes(INITRAMFS)
        self.snapshot = self.assets / "hermit-boot.qcow2"

    def record(self, snapshot: Path) -> Path:
        """Write the record demo 5 writes for ``snapshot`` booted from the
        initramfs there is now."""
        return dc.write_boot_snapshot_record(
            snapshot, dc.hash_file(snapshot), dc.initramfs_producer(self.root, self.assets)
        )

    def assert_mismatch(self, snapshot: Path, expected: str, disk=None) -> None:
        with self.assertRaises(dc.BootSnapshotMismatch) as caught:
            dc.verify_boot_snapshot(snapshot, self.root, self.assets, disk=disk)
        self.assertEqual(str(caught.exception), expected)


class BootSnapshotRecordTest(_StandIns):
    """The record, and what verify_boot_snapshot accepts."""

    def test_the_record_names_the_snapshot_and_the_initramfs(self):
        self.snapshot.write_bytes(SNAPSHOT)
        record = self.record(self.snapshot)
        self.assertEqual(record, self.assets / "hermit-boot.qcow2.producer.json")
        self.assertEqual(
            json.loads(record.read_text()),
            {
                "format": 1,
                "initramfs_sha256": _sha256(INITRAMFS),
                "initramfs_version": 9,
                "snapshot_sha256": _sha256(SNAPSHOT),
            },
        )
        # Renamed into place: no temporary file is left behind.
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            ["hermit-boot.qcow2", "hermit-boot.qcow2.producer.json", "initramfs.cpio.gz"],
        )
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_snapshot_without_a_record_does_not_match(self):
        # A snapshot demo 5 saved before it wrote records.
        self.snapshot.write_bytes(SNAPSHOT)
        self.assert_mismatch(
            self.snapshot,
            "it has no record of the initramfs it was booted from ({} is missing): "
            "demo 5 saved it before it wrote such records, or demo 5 did not save "
            "it".format(self.assets / "hermit-boot.qcow2.producer.json"),
        )

    def test_a_snapshot_of_an_older_initramfs_version_does_not_match(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        _write_assets_script(self.root, 10)
        self.assert_mismatch(
            self.snapshot,
            "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
            "now builds version 10",
        )

    def test_a_snapshot_of_another_initramfs_does_not_match(self):
        # The same version number, but the initramfs was rebuilt differently.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.initramfs.write_bytes(b"another initramfs")
        self.assert_mismatch(
            self.snapshot,
            "it was booted from an initramfs with SHA-256 {}, and {} now has "
            "SHA-256 {}".format(
                _sha256(INITRAMFS), self.initramfs, _sha256(b"another initramfs")
            ),
        )

    def test_a_replaced_snapshot_does_not_match_the_record_of_the_one_before(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.snapshot.write_bytes(b"a snapshot saved later")
        self.assert_mismatch(
            self.snapshot,
            "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                self.snapshot, _sha256(b"a snapshot saved later"), _sha256(SNAPSHOT)
            ),
        )

    def test_a_copy_is_checked_against_the_record_of_the_snapshot(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        copy = self.directory / "copy.qcow2"
        copy.write_bytes(SNAPSHOT)
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets, disk=copy)
        copy.write_bytes(b"not the recorded snapshot")
        self.assert_mismatch(
            self.snapshot,
            "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                copy, _sha256(b"not the recorded snapshot"), _sha256(SNAPSHOT)
            ),
            disk=copy,
        )

    def test_without_an_initramfs_nothing_matches(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.initramfs.unlink()
        self.assert_mismatch(
            self.snapshot,
            "there is no initramfs at {} to compare it with".format(self.initramfs),
        )

    def test_a_record_demo_5_did_not_write_does_not_match(self):
        self.snapshot.write_bytes(SNAPSHOT)
        record_path = self.record(self.snapshot)
        good = json.loads(record_path.read_text())
        for content, problem in (
            ("[]", "it is not a JSON object"),
            (json.dumps(dict(good, format=2)), "its format is 2, not 1"),
            (json.dumps(dict(good, format="1")), "its format is '1', not 1"),
            (json.dumps(dict(good, format=True)), "its format is True, not 1"),
            (
                json.dumps(dict(good, initramfs_version="9")),
                "its initramfs_version '9' is not a whole number",
            ),
            (
                json.dumps(dict(good, initramfs_version=True)),
                "its initramfs_version True is not a whole number",
            ),
            (
                json.dumps(dict(good, initramfs_sha256="ABC")),
                "its initramfs_sha256 'ABC' is not a SHA-256 digest",
            ),
            (
                json.dumps({key: value for key, value in good.items() if key != "snapshot_sha256"}),
                "its snapshot_sha256 None is not a SHA-256 digest",
            ),
        ):
            with self.subTest(content=content):
                record_path.write_text(content)
                self.assert_mismatch(
                    self.snapshot,
                    "its record {} is not one demo 5 wrote: {}".format(record_path, problem),
                )
        record_path.write_text("{not json")
        with self.assertRaises(dc.BootSnapshotMismatch) as caught:
            dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)
        self.assertTrue(
            str(caught.exception).startswith("its record {} cannot be read: ".format(record_path)),
            str(caught.exception),
        )

    def test_the_version_is_read_from_the_one_initramfs_version_line(self):
        self.assertEqual(dc.current_initramfs_version(self.root), 9)
        script = self.root / "demos/lib/qemu-assets.sh"
        for text, found in (("echo no version\n", 0), ("INITRAMFS_VERSION=1\nINITRAMFS_VERSION=2\n", 2)):
            with self.subTest(text=text):
                script.write_text(text)
                with self.assertRaises(RuntimeError) as caught:
                    dc.current_initramfs_version(self.root)
                self.assertEqual(
                    str(caught.exception),
                    "expected one INITRAMFS_VERSION= line in {}, found {}".format(script, found),
                )

    def test_the_version_is_the_one_the_real_qemu_assets_script_builds(self):
        script = REPOSITORY_ROOT / "demos/lib/qemu-assets.sh"
        assignments = [
            line.split("=", 1)[1]
            for line in script.read_text().splitlines()
            if line.startswith("INITRAMFS_VERSION=")
        ]
        self.assertEqual(len(assignments), 1, assignments)
        self.assertEqual(dc.current_initramfs_version(REPOSITORY_ROOT), int(assignments[0]))


class Demo6EnsureBootSnapshotTest(_StandIns):
    """Which boot snapshot demo 6 accepts before it resumes, and what it does
    about one it does not."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMOS_DIR / "06-qemu-resume" / "run.py"))

    def setUp(self):
        super().setUp()
        self.commands = []
        self.demo5_command = [
            "make",
            "--no-print-directory",
            "-C",
            str(self.demo6["DEMOS_DIR"]),
            "demo5",
        ]

    def ensure(self, boot_snapshot: Path, demo5=None) -> str:
        """Run ensure_boot_snapshot with QEMU_BOOT_SNAPSHOT_DISK ``boot_snapshot``.

        ``demo5`` stands in for demo 5 when demo 6 runs it. Returns what demo 6
        printed.
        """

        def run_checked(command, **keywords):
            self.commands.append(list(command))
            if demo5 is not None:
                demo5()

        ensure_boot_snapshot = self.demo6["ensure_boot_snapshot"]
        printed = io.StringIO()
        with mock.patch.dict(
            ensure_boot_snapshot.__globals__,
            {
                "ASSETS": self.assets,
                "ROOT": self.root,
                "BOOT_SNAPSHOT_DISK": boot_snapshot,
                "run_checked": run_checked,
            },
        ), contextlib.redirect_stdout(printed):
            ensure_boot_snapshot()
        return printed.getvalue()

    def demo5_saves(self, data: bytes = b"a snapshot of the current initramfs"):
        """A stand-in for demo 5 that saves the default snapshot and its record."""

        def demo5():
            self.snapshot.write_bytes(data)
            self.record(self.snapshot)

        return demo5

    def test_a_default_snapshot_of_the_current_initramfs_is_used_as_it_is(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.ensure(self.snapshot)
        self.assertEqual(self.commands, [])
        self.assertEqual(self.snapshot.read_bytes(), SNAPSHOT)

    def test_a_default_snapshot_without_a_record_is_rebuilt(self):
        # A snapshot demo 5 saved before it wrote records may hold an /init of
        # an older frame format. Demo 6 used to restore it because it existed.
        self.snapshot.write_bytes(SNAPSHOT)
        printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
        self.assertEqual(self.commands, [self.demo5_command])
        self.assertIn(
            "Demo 5 boot snapshot {} is not from the current initramfs: it has no "
            "record".format(self.snapshot),
            printed,
        )
        self.assertEqual(self.snapshot.read_bytes(), b"a snapshot of the current initramfs")
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_default_snapshot_of_an_older_initramfs_is_rebuilt(self):
        for change, reason in (
            (
                lambda: _write_assets_script(self.root, 10),
                "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
                "now builds version 10",
            ),
            (
                lambda: self.initramfs.write_bytes(b"another initramfs"),
                "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            ),
        ):
            with self.subTest(reason=reason):
                _write_assets_script(self.root, 9)
                self.initramfs.write_bytes(INITRAMFS)
                self.snapshot.write_bytes(SNAPSHOT)
                self.record(self.snapshot)
                change()
                self.commands.clear()
                printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
                self.assertEqual(self.commands, [self.demo5_command])
                self.assertIn(reason, printed)
                dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_missing_default_snapshot_is_built(self):
        printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
        self.assertEqual(self.commands, [self.demo5_command])
        self.assertIn("Demo 5 boot snapshot missing; running demo 5 first...", printed)

    def test_demo5_that_leaves_a_snapshot_without_a_record_fails(self):
        self.snapshot.write_bytes(SNAPSHOT)

        def demo5():
            self.snapshot.write_bytes(b"a snapshot without a record")

        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot, demo5=demo5)
        self.assertTrue(
            str(caught.exception).startswith(
                "Demo 5 ran, but {} still does not match the current initramfs: it has "
                "no record".format(self.snapshot)
            ),
            str(caught.exception),
        )

    def test_demo5_that_saves_no_snapshot_fails(self):
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot)
        self.assertEqual(str(caught.exception), "Demo 5 did not produce {}".format(self.snapshot))

    def test_a_custom_snapshot_without_a_matching_record_is_refused(self):
        custom = self.directory / "custom-boot.qcow2"
        record_path = self.directory / "custom-boot.qcow2.producer.json"
        cases = (
            ("no record", lambda: None, "it has no record of the initramfs it was booted from"),
            (
                "older version",
                lambda: (self.record(custom), _write_assets_script(self.root, 10)),
                "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
                "now builds version 10",
            ),
            (
                "another initramfs",
                lambda: (self.record(custom), self.initramfs.write_bytes(b"another initramfs")),
                "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            ),
            (
                "replaced snapshot",
                lambda: (self.record(custom), custom.write_bytes(b"replaced")),
                "{} has SHA-256 {}".format(custom, _sha256(b"replaced")),
            ),
        )
        for name, prepare, reason in cases:
            with self.subTest(case=name):
                _write_assets_script(self.root, 9)
                self.initramfs.write_bytes(INITRAMFS)
                record_path.unlink(missing_ok=True)
                custom.write_bytes(SNAPSHOT)
                prepare()
                before = custom.read_bytes()
                had_record = record_path.exists()
                self.commands.clear()
                with self.assertRaises(RuntimeError) as caught:
                    self.ensure(custom, demo5=self.demo5_saves())
                message = str(caught.exception)
                self.assertTrue(
                    message.startswith(
                        "refusing to restore the custom boot snapshot {} "
                        "(QEMU_BOOT_SNAPSHOT_DISK): {}".format(custom, reason)
                    ),
                    message,
                )
                # Why it matters, and how to rebuild it.
                self.assertIn("an /init from an older initramfs runs the command as root", message)
                self.assertIn(
                    "Rebuild it by running demo 5 with QEMU_SNAPSHOT_DISK={} and the same "
                    "QEMU_ASSETS".format(custom),
                    message,
                )
                self.assertIsInstance(caught.exception.__cause__, dc.BootSnapshotMismatch)
                # Demo 6 neither ran demo 5 nor touched the custom snapshot.
                self.assertEqual(self.commands, [])
                self.assertEqual(custom.read_bytes(), before)
                self.assertEqual(record_path.exists(), had_record)
                self.assertFalse(self.snapshot.exists())

    def test_a_custom_snapshot_of_the_current_initramfs_is_used(self):
        custom = self.directory / "custom-boot.qcow2"
        custom.write_bytes(SNAPSHOT)
        self.record(custom)
        self.ensure(custom)
        self.assertEqual(self.commands, [])

    def test_a_missing_custom_snapshot_is_refused_as_before(self):
        custom = self.directory / "custom-boot.qcow2"
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(custom, demo5=self.demo5_saves())
        self.assertEqual(
            str(caught.exception),
            "missing custom boot snapshot: {}; produce it before Demo 6".format(custom),
        )
        self.assertEqual(self.commands, [])


class ReachedHermit(Exception):
    """Raised by the stand-in for starting Hermit."""


class Demo6ResumeCopyTest(_StandIns):
    """Demo 6 checks the copy of the boot snapshot that QEMU restores."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMOS_DIR / "06-qemu-resume" / "run.py"))

    def resume(self, after_check=None):
        """Run resume_once until it would start Hermit.

        ensure_boot_snapshot is replaced by ``after_check``, which runs where
        the real one would have accepted the snapshot. Returns what the demo
        raised and how often it started Hermit and released the demo lock.
        """
        resume_once = self.demo6["resume_once"]
        started = []
        released = []

        def start_hermit(command, **keywords):
            started.append(command)
            raise ReachedHermit()

        replacements = {
            "ASSETS": self.assets,
            "ROOT": self.root,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK": self.assets / "hermit-snapshot.qcow2",
            "BOOT_SNAPSHOT_DISK": self.snapshot,
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            "ensure_boot_snapshot": after_check or (lambda: None),
            "acquire_demo_lock": lambda path: "lock",
            "release_demo_lock": lambda handle: released.append(handle),
            "stage_guest_controller": lambda destination: destination,
            "subprocess": types.SimpleNamespace(
                Popen=start_hermit,
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "stop_process": lambda process: None,
            "stop_process_group": lambda process: None,
        }
        printed = io.StringIO()
        with mock.patch.dict(resume_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            try:
                resume_once("echo done", False)
            except Exception as error:  # noqa: BLE001 - the outcome under test
                return error, started, released
        self.fail("resume_once returned without starting Hermit")

    def test_a_snapshot_replaced_after_the_check_is_not_restored(self):
        # Demo 5 does not take the demo lock: it can publish a new snapshot
        # between demo 6's check and its copy, before it writes the new record.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)

        def demo5_publishes_after_the_check():
            self.snapshot.write_bytes(b"a snapshot published after the check")

        outcome, started, released = self.resume(demo5_publishes_after_the_check)
        self.assertIs(type(outcome), RuntimeError, repr(outcome))
        copy = self.assets / "hermit-snapshot.qcow2"
        self.assertEqual(
            str(outcome),
            "the copy {} of the boot snapshot {} does not match demo 5's record: {} "
            "has SHA-256 {}, not the {} that demo 5 recorded for it; demo 5 may have "
            "replaced the boot snapshot after this demo checked it, so run demo 6 "
            "again".format(
                copy,
                self.snapshot,
                copy,
                _sha256(b"a snapshot published after the check"),
                _sha256(SNAPSHOT),
            ),
        )
        self.assertEqual(started, [])
        self.assertEqual(released, ["lock"])

    def test_an_unchanged_snapshot_is_restored(self):
        # Positive control: the same run with nothing replaced starts Hermit.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        outcome, started, released = self.resume()
        self.assertIsInstance(outcome, ReachedHermit)
        self.assertEqual(len(started), 1)
        self.assertEqual(released, ["lock"])


class ReachedTheNextStep(Exception):
    """Raised by the stand-in for saving the run's metadata."""


class Demo5RecordTest(_StandIns):
    """Demo 5 writes the record next to each boot snapshot it saves."""

    @classmethod
    def setUpClass(cls):
        cls.demo5 = runpy.run_path(str(DEMOS_DIR / "05-qemu-boot" / "run.py"))

    def setUp(self):
        super().setUp()
        (self.assets / "bzImage").write_bytes(b"stand-in for the kernel")
        # The build record qemu-assets.sh writes after it builds the initramfs.
        self.build_record = self.assets / ".initramfs-build"
        self.build_record.write_text("9 {}\n".format(_sha256(INITRAMFS)))
        # The Hermit command lines boot_once starts.
        self.commands = []

    def host_file_the_guest_reads(self, command, option: str) -> Path:
        """The host file behind the guest path ``command`` gives the guest
        controller after ``option``: the last --bind whose target covers it is
        the mount the guest sees there."""
        guest = Path(command[command.index(option) + 1])
        host = None
        for index, argument in enumerate(command[: command.index("--")]):
            if argument != "--bind":
                continue
            source, target = command[index + 1].rsplit(":", 1)
            try:
                host = Path(source) / guest.relative_to(target)
            except ValueError:
                continue
        self.assertIsNotNone(host, "no --bind covers {}".format(guest))
        return host

    def boot(self, snapshot_disk_override=None, during_boot=None) -> None:
        """Run boot_once with QEMU and Hermit replaced, until it saves metadata.

        ``during_boot`` runs while Hermit would be booting the guest.
        """
        boot_once = self.demo5["boot_once"]
        copier = mock.Mock()
        copier.is_alive.return_value = False

        def boot_the_guest(process, timeout, **keywords):
            if during_boot is not None:
                during_boot()
            Path(keywords["stream_path"]).write_text("2022-01-01T00:00:00\n")
            return 0

        def save_the_snapshot(path, name):
            Path(path).write_bytes(SNAPSHOT)
            return True

        def save_metadata(run_dir, disk, info_log, fields):
            raise ReachedTheNextStep()

        replacements = {
            "ASSETS": self.assets,
            "ROOT": self.root,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK_OVERRIDE": snapshot_disk_override,
            "check_qemu_dependencies": lambda root: "QEMU dependency check replaced by the test",
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            # qemu-assets.sh, `qemu-img create` and `qemu-img snapshot -l`.
            "run_checked": lambda *arguments, **keywords: None,
            "stage_guest_controller": lambda destination: destination,
            "hermit_tmp_args": lambda root: [],
            "subprocess": types.SimpleNamespace(
                Popen=lambda command, **keywords: self.commands.append(command)
                or mock.Mock(),
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "start_output_copier": lambda process, log: copier,
            "wait_for_process": boot_the_guest,
            "drain_output": lambda *arguments, **keywords: None,
            "stop_process_group": lambda process: None,
            "stop_process": lambda process: None,
            "snapshot_exists": save_the_snapshot,
            "canonicalize_qcow2_snapshot_timestamp": lambda path, name: None,
            "extract_info_tail": lambda path: [],
            "save_metadata": save_metadata,
        }
        printed = io.StringIO()
        with mock.patch.dict(boot_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            with self.assertRaises(ReachedTheNextStep):
                boot_once()

    def assert_record(self, snapshot: Path, initramfs: bytes) -> None:
        record = dc.boot_snapshot_record_path(snapshot)
        self.assertEqual(
            json.loads(record.read_text()),
            {
                "format": 1,
                "initramfs_sha256": _sha256(initramfs),
                "initramfs_version": 9,
                "snapshot_sha256": _sha256(SNAPSHOT),
            },
        )

    def test_the_published_snapshot_has_a_record_that_demo_6_accepts(self):
        self.boot()
        self.assertEqual(self.snapshot.read_bytes(), SNAPSHOT)
        self.assert_record(self.snapshot, INITRAMFS)
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_the_record_names_the_initramfs_the_boot_started_from(self):
        # qemu-assets.sh run by another demo rebuilds the initramfs while this
        # boot runs: the snapshot holds the /init of the one it started from.
        self.boot(during_boot=lambda: self.initramfs.write_bytes(b"rebuilt during the boot"))
        self.assert_record(self.snapshot, INITRAMFS)
        with self.assertRaises(dc.BootSnapshotMismatch):
            dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_snapshot_saved_at_qemu_snapshot_disk_has_a_record_too(self):
        # The path demo 6's refusal tells the user to rebuild a custom snapshot at.
        custom = self.directory / "custom-boot.qcow2"
        self.boot(snapshot_disk_override=str(custom))
        self.assertEqual(custom.read_bytes(), SNAPSHOT)
        self.assert_record(custom, INITRAMFS)
        dc.verify_boot_snapshot(custom, self.root, self.assets)
        self.assert_record(self.snapshot, INITRAMFS)

    def test_the_record_names_the_bytes_qemu_boots(self):
        # Another checkout replaces the shared initramfs with its version 8
        # archive after this boot took its record of the initramfs and before
        # QEMU reads -initrd, then puts version 9 back before the boot ends.
        # The record must name the bytes QEMU read.
        version_8 = b"stand-in for the version 8 initramfs"
        booted = []

        def replace_the_initramfs_while_qemu_reads_it():
            self.initramfs.write_bytes(version_8)
            command = self.commands[-1]
            booted.append(self.host_file_the_guest_reads(command, "--initrd").read_bytes())
            self.initramfs.write_bytes(INITRAMFS)

        self.boot(during_boot=replace_the_initramfs_while_qemu_reads_it)
        record = json.loads(dc.boot_snapshot_record_path(self.snapshot).read_text())
        self.assertEqual(record["initramfs_sha256"], _sha256(booted[0]))
        self.assert_record(self.snapshot, INITRAMFS)
        # The copies QEMU booted are not kept with the run.
        self.assertEqual(list(self.assets.glob(".work/*/boot-assets")), [])

    def assert_refused_before_the_boot(self, expected: str) -> None:
        with self.assertRaises(RuntimeError) as caught:
            self.boot()
        self.assertIn(expected, str(caught.exception))
        self.assertEqual(self.commands, [])
        self.assertFalse(dc.boot_snapshot_record_path(self.snapshot).exists())
        self.assertEqual(list(self.assets.glob(".work/*/boot-assets")), [])

    def test_a_build_record_of_other_bytes_is_refused_before_the_boot(self):
        # The shared initramfs was replaced after qemu-assets.sh built it, by
        # a checkout whose qemu-assets.sh writes no build record.
        self.initramfs.write_bytes(b"stand-in for an initramfs built elsewhere")
        self.assert_refused_before_the_boot(
            "its build record {} says '9 {}'".format(self.build_record, _sha256(INITRAMFS))
        )

    def test_a_build_record_of_another_version_is_refused_before_the_boot(self):
        self.build_record.write_text("8 {}\n".format(_sha256(INITRAMFS)))
        self.assert_refused_before_the_boot(
            "is not one that demos/lib/qemu-assets.sh built at INITRAMFS_VERSION 9"
        )

    def test_without_a_build_record_the_boot_is_refused(self):
        self.build_record.unlink()
        self.assert_refused_before_the_boot("its build record {} says".format(self.build_record))

    def test_a_snapshot_disk_in_the_asset_directory_keeps_its_guest_path(self):
        custom = self.assets / "custom-boot.qcow2"
        seen = {}

        def look_while_the_guest_runs():
            command = self.commands[-1]
            seen["disk"] = command[command.index("--disk") + 1]
            seen["disk host file"] = self.host_file_the_guest_reads(command, "--disk")
            seen["kernel"] = self.host_file_the_guest_reads(command, "--kernel").read_bytes()

        self.boot(snapshot_disk_override=str(custom), during_boot=look_while_the_guest_runs)
        self.assertEqual(
            seen,
            {
                "disk": "/tmp/hermit-demo-assets/custom-boot.qcow2",
                "disk host file": custom,
                "kernel": b"stand-in for the kernel",
            },
        )
        self.assert_record(custom, INITRAMFS)


if __name__ == "__main__":
    unittest.main()
