#!/usr/bin/env python3
"""Concurrency tests for the demo 5 atomic anchor claim.

These exercise the primitive that makes ``05-qemu-boot.py`` safe to run in 2+
terminals at once: many runs build a private result directory and then race to
publish it as THE anchor with a single atomic, no-clobber rename. Exactly one
run must win; the rest must lose cleanly and compare against a fully-committed
(never partial) anchor.

The tests drive the real ``demo_common`` primitives across genuinely concurrent
processes (fork + a barrier so every worker calls ``publish_anchor`` at the same
instant); they do not need Hermit, QEMU, or a kernel and run in well under a
second. Run directly (``python3 demos/tests/test_concurrent_anchor.py``) or via
``make -C demos test``.
"""

import multiprocessing
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402


BASE_LOG = "line-a\nline-b\nline-c\n"


def _boot_record(work, idx=0, qcow2_sha="d" * 64, info_log=None):
    info_log = work / "hermit-info.log" if info_log is None else Path(info_log)
    return {
        "schema_version": dc.RUN_METADATA_SCHEMA_VERSION,
        "kind": "qemu-boot",
        "created_at": str(idx),
        "info_log": str(info_log.resolve()),
        "info_log_sha256": "a" * 64,
        "hermit_version": "hermit-test",
        "qemu_version": "qemu-test",
        "qemu_binary_sha256": "b" * 64,
        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
        "serial_log": str((work / "serial.log").resolve()),
        "serial_sha256": "c" * 64,
        "qcow2_path": str((work / "boot-snapshot.qcow2").resolve()),
        "qcow2_sha256": qcow2_sha,
        "qcow2_size": 1,
        "snapshot_name": "booted",
        "snapshot_date_nsec_canonicalized": True,
    }


def _resume_record(schema_version=dc.RUN_METADATA_SCHEMA_VERSION):
    return {
        "schema_version": schema_version,
        "kind": "qemu-resume",
        "created_at": "2026-08-28T07:00:00Z",
        "info_log": "/tmp/info.log",
        "info_log_sha256": "a" * 64,
        "hermit_version": "hermit-test",
        "qemu_version": "qemu-test",
        "qemu_binary_sha256": "b" * 64,
        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
        "serial_log": "/tmp/serial.log",
        "command": "uname -a",
        "command_sha256": "c" * 64,
        "guest_output": "/tmp/output.log",
        "guest_output_sha256": "d" * 64,
        "snapshot_saved": False,
    }


def _build_and_publish(assets_str, lib_str, barrier, queue, idx, divergent):
    """One concurrent run: build a private result dir, then race to publish it."""
    if lib_str not in sys.path:
        sys.path.insert(0, lib_str)
    import demo_common as worker_dc

    assets = Path(assets_str)
    anchor_dir = assets / "boot-anchor"
    work = worker_dc.make_temp_result_dir(assets, "boot")

    # A divergent run differs from its peers (distinct snapshot hash + log tail),
    # so a loser comparing against a different winner must report a mismatch.
    qcow2_sha = "cafe{:060d}".format(idx) if divergent else "d" * 64
    log_text = BASE_LOG + ("extra-{}\n".format(idx) if divergent else "")
    (work / "hermit-info.log").write_text(log_text)
    metadata = worker_dc.parse_run_metadata(_boot_record(work, idx, qcow2_sha))
    worker_dc._write_json(work / "run-metadata.json", dict(metadata.raw))

    barrier.wait()  # release every worker into the rename race simultaneously
    won = worker_dc.publish_anchor(work, anchor_dir)

    outcome = {"idx": idx, "won": won, "work_survived": work.exists()}
    if not won:
        anchor = worker_dc.load_committed_anchor(anchor_dir)
        # Compare while the working dir (and its info_log) is still in place.
        passed, _report = worker_dc.compare_runs(anchor, metadata)
        outcome["passed"] = passed
        outcome["anchor_worker_idx"] = int(anchor.created_at)
        worker_dc.archive_result_dir(work, assets, "boot")
    queue.put(outcome)


def _run_race(assets, count, divergent):
    """Fork ``count`` workers that publish simultaneously; return their outcomes."""
    ctx = multiprocessing.get_context("fork")
    barrier = ctx.Barrier(count)
    queue = ctx.Queue()
    procs = [
        ctx.Process(
            target=_build_and_publish,
            args=(str(assets), str(LIB_DIR), barrier, queue, idx, divergent),
        )
        for idx in range(count)
    ]
    for proc in procs:
        proc.start()
    outcomes = [queue.get() for _ in range(count)]
    for proc in procs:
        proc.join(timeout=30)
        assert proc.exitcode == 0, "worker exited with {}".format(proc.exitcode)
    return outcomes


class ConcurrentAnchorTest(unittest.TestCase):
    def _assert_single_complete_anchor(self, assets, outcomes):
        winners = [o for o in outcomes if o["won"]]
        losers = [o for o in outcomes if not o["won"]]
        self.assertEqual(len(winners), 1, "exactly one run must win the anchor")
        self.assertEqual(len(losers), len(outcomes) - 1)

        # The winner's working dir was renamed away; losers were archived away.
        self.assertFalse(winners[0]["work_survived"])

        anchor_dir = assets / "boot-anchor"
        self.assertTrue((anchor_dir / "run-metadata.json").is_file())
        self.assertTrue((anchor_dir / "hermit-info.log").is_file())

        # The committed anchor is complete and belongs to the sole winner.
        committed = dc.load_committed_anchor(anchor_dir)
        self.assertEqual(int(committed.created_at), winners[0]["idx"])

        # Every loser saw the winner's anchor (not a partial, not its own).
        for loser in losers:
            self.assertEqual(loser["anchor_worker_idx"], winners[0]["idx"])

        # Exactly one boot-anchor exists; losers landed in run-history.
        self.assertEqual(len(list(assets.glob("boot-anchor"))), 1)
        history = list((assets / "run-history").glob("boot-*")) if (
            assets / "run-history"
        ).is_dir() else []
        self.assertEqual(len(history), len(losers))
        return winners, losers

    def test_identical_runs_all_losers_pass(self):
        for count in (2, 3):
            with tempfile.TemporaryDirectory() as tmp:
                assets = Path(tmp) / "qemu-linux"
                outcomes = _run_race(assets, count, divergent=False)
                _winners, losers = self._assert_single_complete_anchor(
                    assets, outcomes
                )
                for loser in losers:
                    self.assertTrue(
                        loser["passed"],
                        "identical run should PASS against anchor: {}".format(loser),
                    )

    def test_divergent_runs_report_mismatch(self):
        for count in (2, 3):
            with tempfile.TemporaryDirectory() as tmp:
                assets = Path(tmp) / "qemu-linux"
                outcomes = _run_race(assets, count, divergent=True)
                _winners, losers = self._assert_single_complete_anchor(
                    assets, outcomes
                )
                for loser in losers:
                    self.assertFalse(
                        loser["passed"],
                        "divergent run must NOT falsely PASS: {}".format(loser),
                    )

    def test_second_publish_loses_without_clobber(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "qemu-linux"
            anchor_dir = assets / "boot-anchor"

            first = dc.make_temp_result_dir(assets, "boot")
            (first / "hermit-info.log").write_text(BASE_LOG)
            dc._write_json(first / "run-metadata.json", _boot_record(first, 1))
            self.assertTrue(dc.publish_anchor(first, anchor_dir))

            second = dc.make_temp_result_dir(assets, "boot")
            (second / "hermit-info.log").write_text(BASE_LOG)
            dc._write_json(second / "run-metadata.json", _boot_record(second, 2))
            self.assertFalse(dc.publish_anchor(second, anchor_dir))

            # The first winner's content was not clobbered by the second claim.
            committed = dc.load_committed_anchor(anchor_dir)
            self.assertEqual(int(committed.created_at), 1)
            # The loser's dir is untouched (caller decides how to archive it).
            self.assertTrue((second / "run-metadata.json").is_file())


class InfoLogAdmissionTest(unittest.TestCase):
    def _metadata(self, log_path):
        return dc.parse_run_metadata(
            _boot_record(Path(log_path).parent, 0, info_log=log_path)
        )

    def test_missing_qemu_argv_fails_by_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            record = _boot_record(Path(tmp))
            del record["qemu_argv"]
            with self.assertRaisesRegex(ValueError, "qemu-run-metadata-qemu_argv"):
                dc.parse_run_metadata(record)

    def test_documented_launcher_fields_are_normalized(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            anchor_log = root / "anchor.log"
            current_log = root / "current.log"
            anchor_log.write_text(
                "2026-08-17T04:27:14.000000Z INFO detcore: "
                "launcher read FileContents(123) at 0x7fffffffa210\n"
            )
            current_log.write_text(
                "2026-08-17T04:29:10.000000Z INFO detcore: "
                "launcher read FileContents(987) at 0x7fffffff9210\n"
            )

            passed, report = dc.compare_runs(
                self._metadata(anchor_log), self._metadata(current_log)
            )

            self.assertTrue(passed, "documented normalized fields should match")
            self.assertTrue(
                any(line.startswith("PASS: exact Hermit log") for line in report)
            )

    def test_info_divergence_fails_even_when_vm_artifacts_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            anchor_log = root / "anchor.log"
            current_log = root / "current.log"
            anchor_log.write_text(
                "INFO detcore::scheduler: COMMIT turn 48 on previously committed "
                "1_767_225_600.042_170_525s\n"
            )
            current_log.write_text(
                "INFO detcore::scheduler: COMMIT turn 48 on previously committed "
                "1_767_225_600.042_170_465s\n"
            )

            passed, report = dc.compare_runs(
                self._metadata(anchor_log), self._metadata(current_log)
            )

            self.assertFalse(
                passed, "a canonical INFO divergence must make the demo red"
            )
            self.assertTrue(
                any("canonical repeat verification failed" in line for line in report)
            )

    def test_missing_info_log_fails_repeat_verification(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            missing = root / "missing.log"
            current_log = root / "current.log"
            current_log.write_text("INFO detcore: identical work\n")

            passed, report = dc.compare_runs(
                self._metadata(missing), self._metadata(current_log)
            )

            self.assertFalse(passed, "missing INFO evidence must not produce SUCCESS")
            self.assertTrue(
                any(
                    "canonical repeat verification requires both logs" in line
                    for line in report
                )
            )

    def test_missing_current_info_log_fails_repeat_verification(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            anchor_log = root / "anchor.log"
            missing = root / "missing.log"
            anchor_log.write_text("INFO detcore: identical work\n")

            passed, report = dc.compare_runs(
                self._metadata(anchor_log), self._metadata(missing)
            )

            self.assertFalse(
                passed, "missing current INFO evidence must not produce SUCCESS"
            )
            self.assertTrue(
                any(
                    "canonical repeat verification requires both logs" in line
                    for line in report
                )
            )


class RunMetadataContractTest(unittest.TestCase):
    def test_every_kind_has_an_explicit_field_contract(self):
        self.assertEqual(set(dc.QemuRunKind), set(dc.METADATA_FIELDS_BY_KIND))

    def test_producer_writes_the_complete_current_type(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            info_log = run / "hermit-info.log"
            qcow2 = run / "boot-snapshot.qcow2"
            serial_log = run / "serial.log"
            info_log.write_text("INFO deterministic run\n")
            qcow2.write_bytes(b"qcow2")
            serial_log.write_text("serial\n")
            with mock.patch.object(
                dc, "_tool_version", return_value="test-version"
            ), mock.patch.object(dc, "_tool_sha256", return_value="b" * 64):
                metadata = dc.save_metadata(
                    run,
                    qcow2,
                    info_log,
                    {
                        "kind": "qemu-boot",
                        "snapshot_name": "booted",
                        "snapshot_date_nsec_canonicalized": True,
                        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
                        "serial_log": str(serial_log),
                        "serial_sha256": dc.hash_file(serial_log),
                    },
                )

            self.assertEqual(dc.RUN_METADATA_SCHEMA_VERSION, metadata.schema_version)
            self.assertEqual(dc.QemuRunKind.BOOT, metadata.kind)
            self.assertEqual(metadata, dc.load_anchor(run))

    def test_schema_two_requires_qemu_binary_identity(self):
        record = _boot_record(Path("/tmp"))
        del record["qemu_binary_sha256"]
        with self.assertRaisesRegex(
            ValueError, "qemu-run-metadata-qemu_binary_sha256"
        ):
            dc.parse_run_metadata(record)

    def test_schema_one_retains_the_older_optional_qemu_binary(self):
        record = _resume_record(schema_version=1)
        del record["qemu_binary_sha256"]
        metadata = dc.parse_run_metadata(record)
        self.assertIsNone(metadata.qemu_binary_sha256)

    def test_saved_resume_requires_its_snapshot_fields(self):
        record = _resume_record()
        record["snapshot_saved"] = True
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-qcow2_path"):
            dc.parse_run_metadata(record)

    def test_new_kind_fails_by_name(self):
        record = _boot_record(Path("/tmp"))
        record["kind"] = "qemu-future"
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-kind"):
            dc.parse_run_metadata(record)

    def test_new_field_fails_by_name(self):
        record = _boot_record(Path("/tmp"))
        record["future_field"] = True
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-field"):
            dc.parse_run_metadata(record)


if __name__ == "__main__":
    unittest.main(verbosity=2)
