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

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402


BASE_LOG = "line-a\nline-b\nline-c\n"


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
    metadata = {
        "kind": "qemu-boot",
        "worker_idx": idx,
        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
        "qcow2_sha256": qcow2_sha,
        "serial_sha256": "s" * 64,
        "info_log": str((work / "hermit-info.log").resolve()),
    }
    worker_dc._write_json(work / "run-metadata.json", metadata)

    barrier.wait()  # release every worker into the rename race simultaneously
    won = worker_dc.publish_anchor(work, anchor_dir)

    outcome = {"idx": idx, "won": won, "work_survived": work.exists()}
    if not won:
        anchor = worker_dc.load_committed_anchor(anchor_dir)
        # Compare while the working dir (and its info_log) is still in place.
        passed, _report = worker_dc.compare_runs(anchor, metadata)
        outcome["passed"] = passed
        outcome["anchor_worker_idx"] = anchor.get("worker_idx")
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
        self.assertEqual(committed["worker_idx"], winners[0]["idx"])

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
            (first / "run-metadata.json").write_text('{"worker_idx": 1}\n')
            self.assertTrue(dc.publish_anchor(first, anchor_dir))

            second = dc.make_temp_result_dir(assets, "boot")
            (second / "run-metadata.json").write_text('{"worker_idx": 2}\n')
            self.assertFalse(dc.publish_anchor(second, anchor_dir))

            # The first winner's content was not clobbered by the second claim.
            committed = dc.load_committed_anchor(anchor_dir)
            self.assertEqual(committed["worker_idx"], 1)
            # The loser's dir is untouched (caller decides how to archive it).
            self.assertTrue((second / "run-metadata.json").is_file())


if __name__ == "__main__":
    unittest.main(verbosity=2)
