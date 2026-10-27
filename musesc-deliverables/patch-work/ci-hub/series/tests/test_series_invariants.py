#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Invariants of the stress/flakiness series store.

⚠️ EVERY GUARD HERE IS DEMONSTRATED REFUSING, not merely passing. A validator
whose negative case is untested is indistinguishable from one that returns
`True`, and that pattern -- a gate nobody has watched refuse -- is the failure
this repository has hit repeatedly.
"""

import base64
import contextlib
import copy
import errno
import fcntl
import hashlib
import io
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import series  # noqa: E402

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "ledger"))
import publish_backlog  # noqa: E402
import publisher  # noqa: E402


def classification_for_series_outcome(outcome):
    return {
        "passed": ("pass", None),
        "diverged": ("determinism-failure", "product_failure"),
        "timeout": ("timeout", "no_result"),
        "errored": ("crash-error", "product_failure"),
        "no_result": (None, "no_result"),
        "skipped": (None, "understood_prerequisite_failure"),
    }.get(outcome, ("pass", None))


def row(**over):
    base = {
        "schema": series.ENVELOPE_VERSION,
        "event_id": "01J000000000000000000000AA",
        "event_type": "series.observation",
        "emitted_at": "2026-08-25T03:00:00Z",
        "team": "hermit",
        "host": "devbig014",
        "run_id": "campaign-1",
        "producer": "pressure-test",
        "series": {
            "cell": "backend-parity-c/flock-lifecycle/verify/ptrace",
            "tree": "a" * 40,
            "run_index": 0,
            "outcome": "passed",
            "machine_shortname": "devbig014",
            "kernel_version": "7.1.3-test",
            "host_capabilities": {
                "cpuid-faulting": {"present": False, "evidence": "fixture absence"},
                "kvm": {"present": True, "evidence": "fixture presence"},
            },
        },
    }
    for key, value in over.items():
        if key == "series":
            base["series"] = {**base["series"], **value}
        else:
            base[key] = value
    result, failure_class = classification_for_series_outcome(
        base["series"].get("outcome")
    )
    base["series"].setdefault("result", result)
    base["series"].setdefault("failure_class", failure_class)
    return base


class PressureSampleClassification(unittest.TestCase):
    def setUp(self):
        self.projection = CellResultProjection()
        self.projection.setUp()
        self.addCleanup(self.projection.tearDown)
        self.CELL = series._cell_name(self.projection._cell(1))

    def _cells(self, kinds):
        cells = []
        for repetition, kind in enumerate(kinds, 1):
            cell = self.projection._cell(repetition)
            if kind == "product":
                cell.update(outcome="FAIL", result="determinism-failure",
                            failure_class="product_failure")
                cell["attempts"] = [self.projection._attempt("diverged")]
            elif kind != "pass":
                cell.update(outcome="ERROR", result=None, failure_class={
                    "infrastructure": "understood_infrastructure_failure",
                    "prerequisite": "understood_prerequisite_failure",
                    "no_result": "no_result",
                }[kind])
                cell["attempts"] = [self.projection._attempt(
                    "infrastructure_error" if kind == "infrastructure" else "no_result"
                )]
                if kind == "prerequisite":
                    cell["attempts"][0]["error_kind"] = "backend-unavailable"
            cells.append(cell)
        return cells

    def _rows(self, kinds):
        rows = self.projection._project(self._cells(kinds))
        for item in rows:
            item["run_id"] = "campaign-10x"
        return rows

    def _classify_cells(self, cells):
        rows = self.projection._project(cells)
        cell = series._cell_name(cells[0])
        return series.classify_pressure_sample(rows, [cell], "pressure-one")[cell]

    def _classification(self, kinds):
        return series.classify_pressure_sample(
            self._rows(kinds), [self.CELL], "campaign-10x"
        )[self.CELL]

    def test_ten_qualifying_passes_are_the_only_promotion_candidate(self):
        candidate = self._classification(["pass"] * 10)
        self.assertEqual(candidate["classification"], "promotion-candidate")
        self.assertTrue(candidate["promotion_candidate"])
        compressed = self._rows(["pass"])[0]
        # A historical compressed row does not retain individual inner facts.
        compressed["series"].pop("pressure_evidence")
        compressed["series"].pop("attempt")
        compressed["series"].update({"num_runs": 10, "last_run_index": 10})
        compressed_candidate = series.classify_pressure_sample(
            [compressed], [self.CELL], "campaign-10x"
        )[self.CELL]
        self.assertEqual(
            compressed_candidate["classification"], "incomplete"
        )
        self.assertEqual(compressed_candidate["unknown_history_repetitions"], 10)
        self.assertEqual(compressed_candidate["terminal_passes"], 10)
        for passes in (1, 9):
            with self.subTest(passes=passes):
                result = self._classification(
                    ["pass"] * passes + ["product"] * (10 - passes)
                )
                self.assertEqual(result["classification"], "intermittent")
                self.assertFalse(result["promotion_candidate"])

    def test_only_ten_terminal_product_failures_are_confirmed_failing(self):
        failing = self._classification(["product"] * 10)
        self.assertEqual(failing["classification"], "confirmed-failing")
        for kind, classification, count_field in (
            ("infrastructure", "infrastructure-failure", "infrastructure_failures"),
            ("prerequisite", "prerequisite-failure", "prerequisite_failures"),
            ("no_result", "no-result", "no_results"),
        ):
            with self.subTest(kind=kind):
                result = self._classification([kind] * 10)
                self.assertEqual(result["classification"], classification)
                self.assertEqual(result[count_field], 10)
                self.assertFalse(result["promotion_candidate"])

        mixed = self._classification(["product"] * 9 + ["infrastructure"])
        self.assertEqual(mixed["classification"], "incomplete")
        self.assertEqual(mixed["terminal_product_failures"], 9)
        self.assertEqual(mixed["infrastructure_failures"], 1)

    def test_missing_duplicate_and_retry_evidence_cannot_promote(self):
        missing = self._classification(["pass"] * 9)
        self.assertEqual(missing["classification"], "incomplete")
        self.assertEqual(missing["missing_repetitions"], 1)

        duplicate_rows = self._rows(["pass"] * 10)
        publication_replay = duplicate_rows + [
            json.loads(json.dumps(duplicate_rows[0]))
        ]
        replay_result = series.classify_pressure_sample(
            publication_replay, [self.CELL], "campaign-10x"
        )[self.CELL]
        self.assertEqual(replay_result["classification"], "promotion-candidate")
        duplicate = dict(duplicate_rows[0])
        duplicate["event_id"] = "different-event-same-attempt"
        duplicate_rows.append(duplicate)
        duplicate_result = series.classify_pressure_sample(
            duplicate_rows, [self.CELL], "campaign-10x"
        )[self.CELL]
        self.assertEqual(duplicate_result["classification"], "incomplete")
        self.assertEqual(duplicate_result["duplicate_attempts"], 1)

        retried_rows = self._rows(["pass"] * 10)
        retried_rows[0]["series"]["attempt"] = 2
        first_attempt = self._rows(["product"])[0]
        first_attempt["event_id"] = "pressure-first-product"
        retried_rows.append(first_attempt)
        retried = series.classify_pressure_sample(
            retried_rows, [self.CELL], "campaign-10x"
        )[self.CELL]
        self.assertEqual(retried["classification"], "intermittent")
        self.assertEqual(retried["terminal_passes"], 10)
        self.assertEqual(retried["qualifying_passes"], 9)
        self.assertEqual(retried["retried_repetitions"], 1)

        regressed_rows = self._rows(["product"] * 10)
        regressed_rows[0]["series"]["attempt"] = 2
        first_pass = self._rows(["pass"])[0]
        first_pass["event_id"] = "pressure-first-pass"
        regressed_rows.append(first_pass)
        regressed = series.classify_pressure_sample(
            regressed_rows, [self.CELL], "campaign-10x"
        )[self.CELL]
        self.assertEqual(regressed["classification"], "incomplete")
        self.assertEqual(regressed["terminal_passes"], 0)
        self.assertEqual(regressed["mixed_repetitions"], 1)

    def test_inner_failures_behind_terminal_pass_never_qualify(self):
        for kind in ("product", "infrastructure", "prerequisite", "no_result"):
            with self.subTest(kind=kind):
                cells = self._cells(["pass"] * 10)
                adverse = self._cells([kind])[0]["attempts"][0]
                cells[0]["attempts"][0]["index"] = "2"
                cells[0]["attempts"].insert(0, adverse)
                original = json.dumps(cells, sort_keys=True)
                result = self._classify_cells(cells)
                self.assertEqual(result["classification"], "intermittent")
                self.assertEqual(result["terminal_passes"], 10)
                self.assertEqual(result["qualifying_passes"], 9)
                self.assertEqual(result["retried_repetitions"], 0)
                self.assertEqual(json.dumps(cells, sort_keys=True), original)

    def test_multiple_declared_successes_are_not_outer_retries(self):
        for mode, status, signal in (
            ("verify", 0, None), ("chaos", 17, None), ("custom", None, 15),
        ):
            with self.subTest(mode=mode):
                cells = self._cells(["pass"] * 10)
                for cell in cells:
                    cell["mode"] = mode
                    first = cell["attempts"][0]
                    first.update(status=status, signal=signal)
                    second = json.loads(json.dumps(first))
                    second["index"] = "seed-2"
                    cell["attempts"].append(second)
                result = self._classify_cells(cells)
                self.assertEqual(result["classification"], "promotion-candidate")
                self.assertEqual(result["qualifying_passes"], 10)
                self.assertEqual(result["retried_repetitions"], 0)

    def test_nonpassing_fold_retains_adverse_facts_and_neutral_successes(self):
        cells = self._cells(["product"] * 10)
        for cell in cells:
            good = self.projection._attempt()
            good["index"] = "2"
            cell["attempts"].append(good)
        result = self._classify_cells(cells)
        self.assertEqual(result["classification"], "confirmed-failing")
        bad = self.projection._attempt("infrastructure_error")
        bad["index"] = "3"
        cells[0]["attempts"].append(bad)
        result = self._classify_cells(cells)
        self.assertEqual(result["classification"], "incomplete")
        self.assertEqual(result["terminal_product_failures"], 9)
        self.assertEqual(result["mixed_repetitions"], 1)

    def test_every_inner_report_must_support_canonical_qualification(self):
        for change in ("unequal-counts", "wrong-time", "skip-commit", "bad-hash", "missing-report", "duplicate-index"):
            with self.subTest(change=change):
                cells = self._cells(["pass"] * 10)
                first = cells[0]["attempts"][0]
                second = json.loads(json.dumps(first))
                second["index"] = "2"
                cells[0]["attempts"].append(second)
                report = json.loads(first["verification_report"])
                if change == "unequal-counts":
                    report["compared_log_messages"]["right"] += 1
                elif change == "wrong-time":
                    report["comparison"]["virtualize_time"] = False
                elif change == "skip-commit":
                    report["comparison"]["skip_commit"] = True
                first["verification_report"] = json.dumps(report)
                first["verification_report_sha256"] = hashlib.sha256(
                    first["verification_report"].encode()
                ).hexdigest()
                if change == "bad-hash":
                    first["verification_report_sha256"] = "0" * 64
                elif change == "missing-report":
                    first["verification_report"] = None
                    first["verification_report_sha256"] = None
                elif change == "duplicate-index":
                    second["index"] = first["index"]
                if change in ("bad-hash", "missing-report", "duplicate-index"):
                    with self.assertRaises(series.MalformedRow):
                        self._classify_cells(cells)
                else:
                    result = self._classify_cells(cells)
                    self.assertEqual(result["classification"], "intermittent")
                    self.assertEqual(result["qualifying_passes"], 9)

    def test_retry_order_gaps_and_dirty_or_unknown_history(self):
        for history in (("product", "infrastructure"), ("infrastructure", "pass")):
            cells = self._cells(["product"] * 10)
            attempts = [self._cells([kind])[0] for kind in history]
            for ordinal, attempt in enumerate(attempts, 1):
                attempt["attempt"] = ordinal
            result = self._classify_cells(attempts + cells[1:])
            self.assertEqual(result["retried_repetitions"], 1)
            self.assertFalse(result["promotion_candidate"])
            self.assertEqual(result["terminal_passes"], int(history[-1] == "pass"))
            self.assertEqual(result["classification"],
                             "intermittent" if history[-1] == "pass" else "incomplete")
        for change in ("gap", "third-attempt", "dirty", "missing-history"):
            with self.subTest(change=change):
                rows = self._rows(["pass"] * 10)
                payload = rows[0]["series"]
                if change == "gap":
                    payload["attempt"] = 2
                elif change == "third-attempt":
                    payload["attempt"] = 3
                elif change == "dirty":
                    payload["source_tree_dirty"] = True
                else:
                    payload.pop("pressure_evidence")
                result = series.classify_pressure_sample(rows, [self.CELL], "campaign-10x")[self.CELL]
                self.assertEqual(result["classification"], "incomplete")
                self.assertFalse(result["promotion_candidate"])

    def test_first_run_rejected_is_product_failure_with_valid_guest_disposition(self):
        cells = self._cells(["product"] * 10)
        for cell in cells:
            cell["result"] = "crash-error"
            attempt = self.projection._attempt("no_result")
            attempt.update(outcome="FAIL", error_kind=None)
            report = json.loads(attempt["verification_report"])
            report.update(guest_exit_code=17, guest_signal=None,
                          no_result_reason={"kind": "first_run_rejected", "exit_code": 17, "signal": None})
            attempt["verification_report"] = json.dumps(report)
            attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
            cell["attempts"] = [attempt]
        result = self._classify_cells(cells)
        self.assertEqual(result["classification"], "confirmed-failing")
        self.assertEqual(result["terminal_product_failures"], 10)
        for value in ("invalid", True, -1, 2**31):
            with self.subTest(guest_exit_code=value):
                malformed = json.loads(json.dumps(cells))
                attempt = malformed[0]["attempts"][0]
                report = json.loads(attempt["verification_report"])
                report["guest_exit_code"] = value
                report["no_result_reason"]["exit_code"] = value
                attempt["verification_report"] = json.dumps(report)
                attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
                with self.assertRaisesRegex(series.MalformedRow, "invalid guest_exit_code"):
                    self._classify_cells(malformed)

    def test_run_failed_is_product_failure_with_its_reason_and_no_guest_disposition(self):
        cells = self._cells(["product"] * 10)
        for cell in cells:
            cell["result"] = "crash-error"
            attempt = self.projection._attempt("no_result")
            attempt.update(outcome="FAIL", error_kind=None, status=125, signal=None)
            report = json.loads(attempt["verification_report"])
            report.update(guest_exit_code=None, guest_signal=None, no_result_reason={
                "kind": "run_failed", "run": "run1", "failure": "backend_failure",
                "detail": "drrun started and the DBT run then failed",
            })
            attempt["verification_report"] = json.dumps(report)
            attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
            cell["attempts"] = [attempt]
        result = self._classify_cells(cells)
        self.assertEqual(result["classification"], "confirmed-failing")
        self.assertEqual(result["terminal_product_failures"], 10)
        deadlocked = json.loads(json.dumps(cells))
        for cell in deadlocked:
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report["no_result_reason"].update(
                run="run2", failure="deadlock",
                detail="Deadlock detected: thread(s) waiting in rt_sigsuspend with no possible signal, "
                       "but no runnable threads left.",
            )
            attempt["verification_report"] = json.dumps(report)
            attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
        self.assertEqual(self._classify_cells(deadlocked)["terminal_product_failures"], 10)
        for change in ("failure", "run", "detail", "extra-field", "guest-exit", "error-kind", "outcome"):
            with self.subTest(change=change):
                malformed = json.loads(json.dumps(cells))
                attempt = malformed[0]["attempts"][0]
                report = json.loads(attempt["verification_report"])
                reason = report["no_result_reason"]
                if change == "failure":
                    reason["failure"] = "host_was_busy"
                elif change == "run":
                    reason["run"] = "run3"
                elif change == "detail":
                    reason["detail"] = " "
                elif change == "extra-field":
                    reason["exit_code"] = 1
                elif change == "guest-exit":
                    report["guest_exit_code"] = 0
                elif change == "error-kind":
                    attempt["error_kind"] = "incomplete-verification-evidence"
                else:
                    attempt["outcome"] = "ERROR"
                attempt["verification_report"] = json.dumps(report)
                attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
                with self.assertRaises(series.MalformedRow):
                    self._classify_cells(malformed)

    def test_matched_reports_require_typed_guest_dispositions_and_info_counts(self):
        for change in ("guest-string", "guest-bool", "signal-zero", "signal-overflow", "both", "counts-overflow"):
            with self.subTest(change=change):
                cells = self._cells(["pass"] * 10)
                attempt = cells[0]["attempts"][0]
                report = json.loads(attempt["verification_report"])
                if change == "counts-overflow":
                    report["compared_log_messages"] = {"left": 2**64, "right": 2**64}
                elif change == "both":
                    report.update(guest_exit_code=0, guest_signal=9)
                else:
                    field = "guest_exit_code" if change.startswith("guest") else "guest_signal"
                    report[field] = {"guest-string": "invalid", "guest-bool": True,
                                     "signal-zero": 0, "signal-overflow": 2**31}[change]
                attempt["verification_report"] = json.dumps(report)
                attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
                with self.assertRaises(series.MalformedRow):
                    self._classify_cells(cells)

    def test_timeouts_and_known_nonproduct_causes_keep_their_categories(self):
        for error_kind, outer_kind, count in (
            ("guest-launch-refused", "prerequisite", "prerequisite_failures"),
            ("backend-unavailable", "prerequisite", "prerequisite_failures"),
            ("infrastructure", "infrastructure", "infrastructure_failures"),
            ("result-publication", "infrastructure", "infrastructure_failures"),
            ("incomplete-verification-evidence", "no_result", "no_results"),
            ("invalid-backend-evidence", "no_result", "no_results"),
        ):
            with self.subTest(error_kind=error_kind):
                cells = self._cells([outer_kind] * 10)
                for cell in cells:
                    attempt = self.projection._attempt("no_result", index="2")
                    attempt["error_kind"] = error_kind
                    cell["attempts"].append(attempt)
                result = self._classify_cells(cells)
                self.assertEqual(result[count], 10)
                self.assertFalse(result["promotion_candidate"])
        for missing_report in (False, True):
            cells = self._cells(["no_result"] * 10)
            for cell in cells:
                cell["attempts"] = [self.projection._attempt("no_result", timed_out=True)]
                if missing_report:
                    cell["attempts"][0].pop("verification_report")
                    cell["attempts"][0].pop("verification_report_sha256")
            result = self._classify_cells(cells)
            self.assertEqual(result["classification"], "no-result")
            self.assertEqual(result["no_results"], 10)

    def test_a_host_input_change_counts_as_the_skid_overshoot_infrastructure_error_does(self):
        skid = self._cells(["infrastructure"] * 10)
        host_input = self._cells(["infrastructure"] * 10)
        for cell in host_input:
            cell["attempts"] = [self.projection._host_input_attempt()]
        self.assertEqual(self._classify_cells(host_input), self._classify_cells(skid))
        for change in ("verified", "parity", "reason"):
            with self.subTest(change=change):
                cell = self._cells(["infrastructure"])[0]
                cell["attempts"] = [self.projection._host_input_attempt()]
                attempt = cell["attempts"][0]
                report = json.loads(attempt["verification_report"])
                if change == "reason":
                    report["no_result_reason"] = {"kind": "not_run"}
                else:
                    report["verified" if change == "verified" else "bitwise_parity"] = True
                self.projection._replace_comparison_report(attempt, report)
                with self.assertRaises(series.MalformedRow):
                    self._classify_cells([cell])

    def test_infrastructure_report_requires_actual_count_and_consistent_flags(self):
        for change in ("zero", "negative", "overflow", "verified", "parity", "reason"):
            with self.subTest(change=change):
                cell = self._cells(["infrastructure"])[0]
                attempt = cell["attempts"][0]
                report = json.loads(attempt["verification_report"])
                if change in ("zero", "negative", "overflow"):
                    report["infrastructure_error"]["count"] = {"zero": 0, "negative": -1, "overflow": 2**64}[change]
                elif change == "reason":
                    report["no_result_reason"] = {"kind": "not_run"}
                else:
                    report["verified" if change == "verified" else "bitwise_parity"] = True
                attempt["verification_report"] = json.dumps(report)
                attempt["verification_report_sha256"] = hashlib.sha256(attempt["verification_report"].encode()).hexdigest()
                with self.assertRaisesRegex(series.MalformedRow, "inconsistent infrastructure evidence"):
                    self._classify_cells([cell])

    def test_noncomparison_product_failure_requires_a_process_disposition(self):
        cells = self._cells(["product"] * 10)
        for cell in cells:
            cell["mode"] = "custom"
            cell["result"] = "crash-error"
        self.assertEqual(self._classify_cells(cells)["classification"], "confirmed-failing")
        for cell in cells:
            cell["attempts"][0]["status"] = None
        with self.assertRaisesRegex(series.MalformedRow, "lacks a process disposition"):
            self._classify_cells(cells)

    def test_classification_is_bound_to_and_does_not_mutate_the_selected_plan(self):
        selected = [self.CELL]
        original = list(selected)
        result = series.classify_pressure_sample(
            self._rows(["pass"] * 10), selected, "campaign-10x"
        )
        self.assertEqual(selected, original)
        self.assertEqual(set(result), set(selected))

        foreign = self._rows(["pass"] * 10)
        foreign[0]["series"]["cell"] = "unselected/test/verify/ptrace"
        with self.assertRaisesRegex(series.SeriesError, "unselected cell"):
            series.classify_pressure_sample(foreign, selected, "campaign-10x")


class ShardPath(unittest.TestCase):
    def test_shape_matches_the_ledger_convention(self):
        self.assertEqual(
            series.shard_path("hermit", "devbig014", "2026-08"),
            "series/hermit/devbig014/2026-08.jsonl",
        )

    def test_refuses_a_month_that_is_not_yyyy_mm(self):
        for bad in ("2026-8", "2026", "", "2026-08-25"):
            with self.assertRaises(series.MalformedRow, msg=bad):
                series.shard_path("hermit", "devbig014", bad)

    def test_refuses_a_host_that_would_escape_its_shard(self):
        # A traversal here would let one machine write another's shard, which is
        # exactly what per-machine sharding exists to prevent.
        for bad in ("..", "a/b", ""):
            with self.assertRaises(series.MalformedRow, msg=bad):
                series.shard_path("hermit", bad, "2026-08")

    def test_part_zero_is_the_month_shard_and_later_parts_sort_after_it(self):
        self.assertEqual(
            series.shard_part_path("hermit", "devbig014", "2026-08", 0),
            series.shard_path("hermit", "devbig014", "2026-08"),
        )
        names = [
            series.shard_part_path("hermit", "devbig014", "2026-08", part)
            for part in (0, 1, 2, 10, series.SHARD_PART_LIMIT)
        ]
        self.assertEqual(names[1], "series/hermit/devbig014/2026-08.part-0001.jsonl")
        self.assertEqual(names[-1], "series/hermit/devbig014/2026-08.part-9999.jsonl")
        # Readers read a month's files in path order; that must be part order,
        # and a hand-split file named after the month must sort before both.
        self.assertEqual(sorted(names), names)
        self.assertLess("series/hermit/devbig014/2026-08-01-to-12.jsonl", names[0])

    def test_refuses_a_part_outside_zero_to_the_limit(self):
        for bad in (-1, series.SHARD_PART_LIMIT + 1, True, "1", 1.0, None):
            with self.assertRaises(series.MalformedRow, msg=repr(bad)):
                series.shard_part_path("hermit", "devbig014", "2026-08", bad)
        with self.assertRaises(series.MalformedRow):
            series.shard_part_path("hermit", "..", "2026-08", 1)

    def test_parses_only_the_names_it_gives(self):
        for part in (0, 1, 2, series.SHARD_PART_LIMIT):
            name = series.shard_part_path("hermit", "devbig014", "2026-08", part)
            self.assertEqual(
                series.parse_shard_part_path(name), ("hermit", "devbig014", "2026-08", part),
            )
            self.assertEqual(
                series.parse_shard_part_path(Path(name)), ("hermit", "devbig014", "2026-08", part),
            )
        for name in (
            "series/hermit/devbig014/2026-08-01-to-12.jsonl",
            "series/hermit/devbig014/2026-08.part-1.jsonl",
            "series/hermit/devbig014/2026-08.part-0000.jsonl",
            "series/hermit/devbig014/2026-08.part-00001.jsonl",
            "series/hermit/devbig014/2026-8.jsonl",
            "series/hermit/devbig014/2026-08.jsonl.bak",
            "quarantine/series/hermit/devbig014/2026-08.jsonl",
            "ledger/hermit/devbig014/2026-08.jsonl",
            "series/hermit/2026-08.jsonl",
            "series/hermit/../2026-08.jsonl",
            "",
            None,
            7,
        ):
            self.assertIsNone(series.parse_shard_part_path(name), repr(name))

    def test_month_files_are_its_parts_and_files_named_after_it(self):
        for name in (
            "2026-08.jsonl",
            "2026-08.part-0001.jsonl",
            "2026-08-01-to-12.jsonl",
            "2026-08.hand-split.jsonl",
        ):
            self.assertTrue(series.is_month_shard_name(name, "2026-08"), name)
        for name in (
            "2026-09.jsonl",
            "2026-07.part-0001.jsonl",
            "2026-08.jsonl.bak",
            "2026-080.jsonl",
            "x2026-08.jsonl",
            "2026-08",
        ):
            self.assertFalse(series.is_month_shard_name(name, "2026-08"), name)
        for bad in ("2026-8", "", None, "2026-08.*"):
            with self.assertRaises(series.MalformedRow, msg=repr(bad)):
                series.is_month_shard_name("2026-08.jsonl", bad)


class RowLinter(unittest.TestCase):
    def test_accepts_a_well_formed_row(self):
        self.assertIsNotNone(series.validate_row(row()))

    def test_refuses_a_missing_required_envelope_key(self):
        bad = row()
        del bad["event_id"]
        with self.assertRaises(series.MalformedRow):
            series.validate_row(bad)

    def test_refuses_a_foreign_schema(self):
        with self.assertRaises(series.MalformedRow):
            series.validate_row(row(schema="validate-ledger/v1"))

    def test_requires_the_machine_kernel_and_capabilities_recorded_by_the_cell(self):
        missing = row()
        del missing["series"]["kernel_version"]
        with self.assertRaises(series.MalformedRow):
            series.validate_row(missing)

        mismatched = row(host="other-host")
        with self.assertRaises(series.MalformedRow):
            series.validate_row(mismatched)

        missing = row()
        del missing["series"]["host_capabilities"]
        with self.assertRaisesRegex(series.MalformedRow, "host_capabilities"):
            series.validate_row(missing)

        for capability in ("cpuid-faulting", "kvm"):
            incomplete = row()
            del incomplete["series"]["host_capabilities"][capability]
            with self.assertRaisesRegex(series.MalformedRow, capability):
                series.validate_row(incomplete)

    def test_readers_refuse_incomplete_host_facts_by_capability_name_but_keep_v1_readable(self):
        legacy = row(schema=series.LEGACY_ENVELOPE_VERSION)
        for field in ("machine_shortname", "kernel_version", "host_capabilities"):
            del legacy["series"][field]
        self.assertEqual(series.validate_read_rows([legacy]), [legacy])

        missing = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
        del missing["series"]["host_capabilities"]
        with self.assertRaisesRegex(series.MalformedRow, "host_capabilities"):
            series.validate_read_rows([missing])

        for capability in ("cpuid-faulting", "kvm"):
            incomplete = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
            del incomplete["series"]["host_capabilities"][capability]
            with self.assertRaisesRegex(series.MalformedRow, capability):
                series.validate_read_rows([incomplete])

    def test_union_refuses_an_incomplete_v2_row_before_it_becomes_evidence(self):
        incomplete = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
        del incomplete["series"]["host_capabilities"]["cpuid-faulting"]
        with tempfile.TemporaryDirectory() as td:
            shard = Path(td) / "rows.jsonl"
            shard.write_text(json.dumps(incomplete) + "\n")
            with self.assertRaisesRegex(series.MalformedRow, "cpuid-faulting"):
                series.union([shard])

    def test_refuses_a_tree_that_is_not_a_full_sha(self):
        for bad in ("abc", "A" * 40, "", "a" * 39):
            with self.assertRaises(series.MalformedRow, msg=bad):
                series.validate_row(row(series={"tree": bad}))

    def test_refuses_a_negative_or_non_integer_run_index(self):
        for bad in (-1, "0", 1.5, True):
            with self.assertRaises(series.MalformedRow, msg=repr(bad)):
                series.validate_row(row(series={"run_index": bad}))

    def test_refuses_an_unknown_outcome(self):
        with self.assertRaises(series.MalformedRow):
            series.validate_row(row(series={"outcome": "green"}))

    def test_v3_refuses_missing_or_mismatched_classification_by_name(self):
        missing = row(series={"outcome": "diverged", "result": None})
        with self.assertRaisesRegex(series.MalformedRow, "result=None"):
            series.validate_row(missing)
        with self.assertRaisesRegex(series.MalformedRow, "result=None"):
            series.validate_read_rows([missing])

        missing = row(series={
            "outcome": "diverged",
            "result": "determinism-failure",
            "failure_class": None,
        })
        with self.assertRaisesRegex(series.MalformedRow, "failure_class=None"):
            series.validate_row(missing)

        mismatched = row(series={
            "outcome": "diverged",
            "result": "crash-error",
            "failure_class": "product_failure",
        })
        with self.assertRaisesRegex(series.MalformedRow, "CrashError"):
            series.validate_row(mismatched)

        retained_v2 = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
        del retained_v2["series"]["result"]
        del retained_v2["series"]["failure_class"]
        self.assertEqual(series.validate_read_rows([retained_v2]), [retained_v2])
        with self.assertRaisesRegex(
            series.MalformedRow, "new rows must use stress-series/v3"
        ):
            series.validate_rows([retained_v2])

    def test_refuses_an_unknown_coordinate_key(self):
        with self.assertRaises(series.MalformedRow):
            series.validate_row(row(series={
                "outcome": "diverged", "coordinates": {"first_divergent_bytes": 1},
            }))

    def test_accepts_first_divergent_messages_and_nonconsecutive_run_span(self):
        series.validate_row(row(series={
            "outcome": "diverged",
            "num_runs": 2,
            "last_run_index": 7,
            "coordinates": {"first_divergent_record": 98},
            "first_divergent_messages": {
                "left": "INFO detcore: inbound syscall: read(3) = ?",
                "right": "INFO detcore: finish syscall #37: read(3) = Ok(1)",
            },
        }))

    def test_refuses_malformed_first_divergent_messages_and_run_span(self):
        bad_payloads = [
            {"first_divergent_messages": {"left": "only one side"}},
            {"first_divergent_messages": {"left": None, "right": None}},
            {"first_divergent_messages": {"left": "", "right": "right"}},
            {"last_run_index": -1},
            {"last_run_index": "7"},
            {"num_runs": 3, "last_run_index": 1},
        ]
        for payload in bad_payloads:
            with self.assertRaises(series.MalformedRow, msg=payload):
                series.validate_row(row(series={"outcome": "diverged", **payload}))

    def test_refuses_coordinates_on_a_row_that_did_not_diverge(self):
        # THE CROSS-FIELD RULE. A passing run contributing a position would put a
        # number into a range that describes divergences.
        for outcome in ("passed", "timeout", "errored", "no_result", "skipped"):
            with self.assertRaises(series.MalformedRow, msg=outcome):
                series.validate_row(row(series={
                    "outcome": outcome,
                    "coordinates": {"first_divergent_record": 98},
                }))
            with self.assertRaises(series.MalformedRow, msg=outcome):
                series.validate_row(row(series={
                    "outcome": outcome,
                    "first_divergent_messages": {"left": "a", "right": "b"},
                }))

    def test_allows_diverged_with_no_located_coordinate(self):
        # ⚠️ THE DISTINCT STATE, and the reason the rule above is one-directional.
        # 'it diverged and we could not locate where' is a real, different answer
        # from 'it passed' -- refusing it would force a producer to lie.
        self.assertIsNotNone(series.validate_row(row(series={
            "outcome": "diverged",
            "coordinates": {k: None for k in series.COORDINATE_KEYS},
        })))

    def test_allows_diverged_with_the_coordinates_key_ABSENT(self):
        # THE SHAPE A PRODUCER ACTUALLY EMITS. The test above pins
        # `coordinates` PRESENT and all-null; a writer with nothing to report is
        # far likelier to omit the key entirely, and that path was accepted but
        # undefended -- so a later change making `coordinates` mandatory would
        # have passed the suite and silently forced producers to skip the row.
        #
        # ⚠️ SKIPPING IS THE FAILURE THIS GUARDS. A cell with NO rows reads
        # `never-measured`; a cell with one coordinate-less diverged row reads
        # `diverged-unlocated`. Those are different answers, so a producer that
        # drops the row does not merely lose it -- it RELABELS a divergence it
        # could not locate as a cell nobody ever ran, which is the exact
        # under-count the store exists to make visible.
        self.assertIsNotNone(series.validate_row(row(series={
            "outcome": "diverged",
        })))
        self.assertEqual(
            series.measurement_state([row(series={"outcome": "diverged"})]),
            "diverged-unlocated",
        )
        self.assertEqual(series.measurement_state([]), "never-measured")

    def test_refuses_a_workspace_local_owner_path(self):
        needle = "/" + "home" + "/newton/secret"
        with self.assertRaises(series.MalformedRow):
            series.validate_row(row(producer=needle))

    def test_accepts_runtime_counts_compression_and_existing_depth_shape(self):
        series.validate_row(row(series={
            "num_runs": 200,
            "main_ancestry": True,
            "depth": {
                "hermit": {"commits": 2031, "first_parent": 1957},
                "reverie": {"commits": 941, "first_parent": 925},
            },
            "runtime": {
                "run1": {
                    "scheduler_turns": 1231,
                    "virtual_nanoseconds": 1_413_842_550,
                    "syscalls": 612,
                },
                "run2": {
                    "scheduler_turns": 1231,
                    "virtual_nanoseconds": 1_413_842_550,
                    "syscalls": 612,
                },
                "wall_time_min_ms": 5_101,
                "wall_time_max_ms": 5_285,
            },
        }))

    def test_refuses_false_runtime_or_compression_counts(self):
        bad_payloads = [
            {"num_runs": 0},
            {"num_runs": True},
            {"main_ancestry": "yes"},
            {"runtime": {"run1": {"scheduler_turns": -1}}},
            {"runtime": {"run1": {"syscalls": True}}},
            {"runtime": {"wall_time_min_ms": 10, "wall_time_max_ms": 9}},
            {"depth": {"hermit": {"commits": 10, "first_parent": 0}}},
        ]
        for payload in bad_payloads:
            with self.assertRaises(series.MalformedRow, msg=payload):
                series.validate_row(row(series=payload))


class SharedCheckerToolchain(unittest.TestCase):
    def test_checker_uses_its_tool_dependency_channel_not_inherited_measurement(self):
        selected = "nightly-2026-07-12-x86_64-unknown-linux-gnu"
        checker = Path(series.__file__).resolve().with_name("check-series-rows.rs")
        completed = subprocess.CompletedProcess([], 0, "", "")
        with mock.patch.dict(os.environ, {"RUSTUP_TOOLCHAIN": "nightly-2025-01-01", "RUSTUP_AUTO_INSTALL": "1"}), \
             mock.patch.object(series.rust_toolchain, "selected_toolchain", return_value=selected) as resolve, \
             mock.patch.object(series.subprocess, "run", return_value=completed) as run:
            # Without an explicit tool root the checker is the one beside this
            # series.py; the tool-root cases below pin the other branch.
            os.environ.pop(series.TOOL_ROOT_ENV, None)
            result = series._run_shared_checker("immutable input\n", ["--read"])
            self.assertEqual(os.environ["RUSTUP_TOOLCHAIN"], "nightly-2025-01-01")
            self.assertEqual(os.environ["RUSTUP_AUTO_INSTALL"], "1")
        self.assertIs(result, completed)
        self.assertEqual(resolve.call_args.args, (checker.parents[2] / "hermit",))
        self.assertEqual(run.call_args.args[0], [
            "rust-script", "--force", "--toolchain", selected, str(checker), "--read",
        ])
        self.assertEqual(run.call_args.kwargs["input"], "immutable input\n")
        self.assertEqual(run.call_args.kwargs["env"]["RUSTUP_TOOLCHAIN"], selected)
        self.assertEqual(run.call_args.kwargs["env"]["RUSTUP_AUTO_INSTALL"], "0")
        self.assertEqual(run.call_args.kwargs["timeout"], 300)
        self.assertFalse(run.call_args.kwargs["check"])

    def test_unavailable_tool_dependency_compiler_refuses_before_checker(self):
        with mock.patch.object(
            series.rust_toolchain, "selected_toolchain",
            side_effect=series.rust_toolchain.ToolchainRefused("declared compiler unavailable"),
        ), mock.patch.object(series.subprocess, "run") as run:
            with self.assertRaisesRegex(series.SeriesError, "declared compiler unavailable"):
                series._run_shared_checker("immutable input\n", ["--projection-results"])
        run.assert_not_called()


class SharedCheckerToolRoot(unittest.TestCase):
    """The checker compiles the admitted tool's Hermit, not the shared checkout's.

    Hermit's pressure test runs the STATE root's series.py. On 2026-10-03 the
    shared Hermit checkout under it was stale beside a newer agent-utils, the
    checker it compiled failed with E0063, and every pressure-test cell batch
    was refused while the run's own immutable tool root compiled cleanly.
    """

    SELECTED = "nightly-2026-07-12-x86_64-unknown-linux-gnu"

    def _tool_root(self, directory, *, checker=True):
        root = Path(directory) / "tool"
        (root / "hermit").mkdir(parents=True)
        (root / "ci-hub" / "series").mkdir(parents=True)
        if checker:
            (root / series.SHARED_CHECKER_RELATIVE).write_text("// checker\n")
        return root

    def _run_with_tool_root(self, value, completed=None):
        completed = completed or subprocess.CompletedProcess([], 0, "", "")
        with mock.patch.dict(os.environ, {series.TOOL_ROOT_ENV: value}), \
             mock.patch.object(series.rust_toolchain, "selected_toolchain",
                               return_value=self.SELECTED) as resolve, \
             mock.patch.object(series.subprocess, "run", return_value=completed) as run:
            result = series._run_shared_checker("row\n", ["--write-results"])
        return result, resolve, run

    def test_explicit_tool_root_compiles_its_own_checker_and_hermit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self._tool_root(directory)
            # operational-tool hands out /proc/<pid>/fd/<n>, a link to the
            # materialized tree; the checker builds under the resolved path.
            handle = Path(directory) / "fd-handle"
            handle.symlink_to(root, target_is_directory=True)
            for value in (str(root), str(handle)):
                with self.subTest(tool_root=value):
                    _result, resolve, run = self._run_with_tool_root(value)
                    resolved = root.resolve()
                    self.assertEqual(resolve.call_args.args, (resolved / "hermit",))
                    self.assertEqual(run.call_args.args[0], [
                        "rust-script", "--force", "--toolchain", self.SELECTED,
                        str(resolved / series.SHARED_CHECKER_RELATIVE), "--write-results",
                    ])
                    beside_this_file = Path(series.__file__).resolve().with_name(
                        "check-series-rows.rs")
                    self.assertNotEqual(run.call_args.args[0][4], str(beside_this_file))

    def test_empty_tool_root_means_the_checker_beside_this_file(self):
        _result, resolve, run = self._run_with_tool_root("")
        checker = Path(series.__file__).resolve().with_name("check-series-rows.rs")
        self.assertEqual(resolve.call_args.args, (checker.parents[2] / "hermit",))
        self.assertEqual(run.call_args.args[0][4], str(checker))

    def test_unusable_tool_root_refuses_instead_of_using_the_shared_checkout(self):
        with tempfile.TemporaryDirectory() as directory:
            without_checker = self._tool_root(directory, checker=False)
            cases = {
                "relative": "relative/tool-root",
                "missing": str(Path(directory) / "absent"),
                "no checker": str(without_checker),
            }
            for label, value in cases.items():
                with self.subTest(label):
                    with mock.patch.dict(os.environ, {series.TOOL_ROOT_ENV: value}), \
                         mock.patch.object(series.rust_toolchain, "selected_toolchain") as resolve, \
                         mock.patch.object(series.subprocess, "run") as run:
                        with self.assertRaisesRegex(series.SeriesError, series.TOOL_ROOT_ENV):
                            series._run_shared_checker("row\n", ["--read"])
                    resolve.assert_not_called()
                    run.assert_not_called()

    def test_inaccessible_checker_is_a_series_refusal_not_an_os_error(self):
        # e.g. DEV_HERMIT_TOOL_ROOT=/root: the directory stats, the checker does
        # not. append-cells would otherwise report a raw PermissionError as a
        # spool I/O failure.
        path_type = type(Path("/"))
        real_stat = path_type.stat

        def guarded(path, *args, **kwargs):
            if path.name == series.SHARED_CHECKER_RELATIVE.name:
                raise PermissionError(13, "Permission denied", str(path))
            return real_stat(path, *args, **kwargs)

        with tempfile.TemporaryDirectory() as directory:
            root = self._tool_root(directory)
            with mock.patch.dict(os.environ, {series.TOOL_ROOT_ENV: str(root)}), \
                 mock.patch.object(path_type, "stat", guarded), \
                 mock.patch.object(series.rust_toolchain, "selected_toolchain") as resolve, \
                 mock.patch.object(series.subprocess, "run") as run:
                with self.assertRaisesRegex(series.SeriesError, "Permission denied"):
                    series._run_shared_checker("row\n", ["--read"])
            resolve.assert_not_called()
            run.assert_not_called()

    @staticmethod
    def _git(*args):
        subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@invalid", *args],
            check=True, capture_output=True, text=True,
        )

    @staticmethod
    def _head(repository):
        return subprocess.run(
            ["git", "-C", str(repository), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def _incident_tree(self, directory):
        """A tool root shaped like the 2026-10-03 shared checkout."""
        root = self._tool_root(directory)
        hermit = root / "hermit"
        self._git("-C", str(root), "init", "-q", "-b", "main")
        self._git("-C", str(hermit), "init", "-q", "-b", "main")
        (hermit / "source.rs").write_text("pinned\n")
        self._git("-C", str(hermit), "add", "source.rs")
        self._git("-C", str(hermit), "commit", "-q", "-m", "pinned")
        pinned = self._head(hermit)
        agent_utils = hermit / "agent-utils"
        agent_utils.mkdir()
        self._git("-C", str(agent_utils), "init", "-q", "-b", "main")
        (agent_utils / "dagrun.rs").write_text("old Step\n")
        self._git("-C", str(agent_utils), "add", "dagrun.rs")
        self._git("-C", str(agent_utils), "commit", "-q", "-m", "old")
        agent_pin = self._head(agent_utils)
        (hermit / ".gitmodules").write_text(
            '[submodule "agent-utils"]\n\tpath = agent-utils\n\turl = ./agent-utils\n'
        )
        self._git("-C", str(hermit), "add", ".gitmodules")
        self._git("-C", str(hermit), "update-index", "--add", "--cacheinfo",
                  f"160000,{agent_pin},agent-utils")
        self._git("-C", str(hermit), "commit", "-q", "-m", "stale")
        # Active, as `git submodule update --init` leaves it.
        self._git("-C", str(hermit), "config", "submodule.agent-utils.url", str(agent_utils))
        stale = self._head(hermit)
        # The submodule moved forward past Hermit's pin...
        (agent_utils / "dagrun.rs").write_text("Step with delegated_children\n")
        self._git("-C", str(agent_utils), "commit", "-q", "-am", "new field")
        moved = self._head(agent_utils)
        # ...and one tracked Hermit file is mid-edit.
        (hermit / "source.rs").write_text("mid-edit\n")
        self._git("-C", str(root), "update-index", "--add", "--cacheinfo",
                  f"160000,{pinned},hermit")
        self._git("-C", str(root), "commit", "-q", "-m", "pin hermit")
        return root, stale, pinned, agent_pin, moved

    INCIDENT_STDERR = (
            "   Compiling hermit-manifest-plan v0.1.0\n"
            "error[E0063]: missing field `delegated_children` in initializer of `dagrun::Step`\n"
            "   --> /x/hermit/ci/manifest-plan/src/validation_dag_static.rs:601:9\n"
            "    |\n"
            "error: could not compile `hermit-manifest-plan` (lib) due to 1 previous error\n"
            "error: Could not execute cargo\n"
    )

    def _incident_failure(self, root):
        """Admit one row with rust-script failing as it did on 2026-10-03."""
        real_run = subprocess.run
        real_git = series._git

        def fake_run(argv, *args, **kwargs):
            if argv[0] == "rust-script":
                return subprocess.CompletedProcess(argv, 1, "", self.INCIDENT_STDERR)
            return real_run(argv, *args, **kwargs)

        with mock.patch.dict(os.environ, {series.TOOL_ROOT_ENV: str(root)}), \
             mock.patch.object(series.rust_toolchain, "selected_toolchain",
                               return_value=self.SELECTED), \
             mock.patch.object(series.subprocess, "run", side_effect=fake_run), \
             mock.patch.object(series, "_git", side_effect=real_git) as probes:
            with self.assertRaises(series.SeriesError) as raised:
                series.validate_rows([row()])
        # The diagnostic is bounded: at most five probes, each with a short timeout.
        self.assertLessEqual(probes.call_count, 5)
        for call in probes.call_args_list:
            self.assertEqual(call.kwargs.get("timeout"), series._DIAGNOSTIC_GIT_TIMEOUT_SECONDS)
        # A tool failure is not a verdict on the row.
        self.assertNotIsInstance(raised.exception, series.MalformedRow)
        return str(raised.exception)

    def test_build_failure_names_the_tool_hermit_path_and_head_in_one_line(self):
        with tempfile.TemporaryDirectory() as directory:
            root, stale, pinned, agent_pin, moved = self._incident_tree(directory)
            tool_hermit = root.resolve() / "hermit"
            message = self._incident_failure(root)
        first = message.splitlines()[0]
        for expected in (
            f"did not build against tool Hermit {tool_hermit} at HEAD {stale}",
            f"dev-hermit pins {pinned}: DIFFERS",
            f"agent-utils at {moved[:12]} (pin {agent_pin[:12]})",
            "1 modified tracked path(s)",
            "first error: error[E0063]: missing field `delegated_children` in initializer "
            "of `dagrun::Step` --> /x/hermit/ci/manifest-plan/src/validation_dag_static.rs:601:9",
            f"checker {root.resolve() / series.SHARED_CHECKER_RELATIVE}",
            f"Remedy: the immutable tool root named by {series.TOOL_ROOT_ENV} does not build",
        ):
            self.assertIn(expected, first)
        # The compiler's own output is retained after the diagnostic line.
        self.assertIn("error: Could not execute cargo", message)

    def test_staged_submodule_drift_is_still_reported_against_hermits_commit(self):
        # `git submodule status` compares with the index: once the moved
        # gitlink is staged its line loses the `+`, and --ignore-submodules
        # hides it from the modified-path count. HEAD's gitlink is the pin.
        with tempfile.TemporaryDirectory() as directory:
            root, _stale, _pinned, agent_pin, moved = self._incident_tree(directory)
            self._git("-C", str(root / "hermit"), "add", "agent-utils")
            message = self._incident_failure(root)
        self.assertIn(f"agent-utils at {moved[:12]} (pin {agent_pin[:12]})",
                      message.splitlines()[0])

    def test_build_failure_outside_a_checkout_still_refuses_loudly(self):
        failed = subprocess.CompletedProcess(
            [], 1, "", "error: failed to load manifest\nerror: Could not execute cargo\n")
        with tempfile.TemporaryDirectory() as directory:
            root = self._tool_root(directory)
            for probe_error in (OSError("no git"), subprocess.TimeoutExpired("git", 10)):
                with self.subTest(probe_error=type(probe_error).__name__):
                    with mock.patch.object(series, "_git", side_effect=probe_error):
                        with self.assertRaises(series.SeriesError) as raised:
                            self._run_with_tool_root(str(root), failed)
                    first = str(raised.exception).splitlines()[0]
                    self.assertIn(
                        f"tool Hermit {root.resolve() / 'hermit'} at HEAD unknown", first)
                    self.assertIn("first error: error: failed to load manifest", first)

    def test_build_failure_without_a_tool_root_names_the_shared_checkout_remedy(self):
        failed = subprocess.CompletedProcess(
            [], 1, "", "error: failed to load manifest\nerror: Could not execute cargo\n")
        with mock.patch.object(series, "_git", side_effect=OSError("no git")):
            with self.assertRaises(series.SeriesError) as raised:
                self._run_with_tool_root("", failed)
        hermit = Path(series.__file__).resolve().parents[2] / "hermit"
        first = str(raised.exception).splitlines()[0]
        self.assertIn(f"tool Hermit {hermit} at HEAD unknown", first)
        self.assertIn(
            f"Remedy: run under an immutable tool root ({series.TOOL_ROOT_ENV}) or "
            f"restore {hermit}", first)

    def test_checker_refusal_is_returned_not_reported_as_a_build_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self._tool_root(directory)
            for stdout, stderr in (
                ("", "check-series-rows: stdin line 1: unknown field `x`\n"),
                ("{}", "error: Could not execute cargo\n"),
            ):
                with self.subTest(stderr=stderr):
                    refused = subprocess.CompletedProcess([], 1, stdout, stderr)
                    result, _resolve, _run = self._run_with_tool_root(str(root), refused)
                    self.assertIs(result, refused)


class ProjectionResultsReader(unittest.TestCase):
    @staticmethod
    def _legacy(event_id="legacy"):
        legacy = row(event_id=event_id, schema=series.LEGACY_ENVELOPE_VERSION)
        for field in ("machine_shortname", "kernel_version", "host_capabilities"):
            del legacy["series"][field]
        return legacy

    @staticmethod
    def _checker_output(rows, results, **changes):
        ordered = sorted(
            rows,
            key=lambda item: json.dumps(item, sort_keys=True, separators=(",", ":")),
        )
        body = "".join(
            json.dumps(item, sort_keys=True, separators=(",", ":")) + "\n"
            for item in ordered
        )
        output = {
            "input_jsonl_sha256": hashlib.sha256(body.encode()).hexdigest(),
            "input_row_count": len(ordered),
            "results": results,
            "schema_version": 1,
        }
        output.update(changes)
        return subprocess.CompletedProcess(
            ["check-series-rows.rs", "--projection-results"],
            0,
            json.dumps(output),
            "",
        )

    def test_shared_projection_results_round_trip_is_complete_and_order_independent(self):
        current = row(event_id="current")
        retained_v2 = row(
            event_id="retained-v2",
            schema=series.HOST_FACTS_ENVELOPE_VERSION,
        )
        legacy = self._legacy()
        dirty = row(
            event_id="dirty",
            series={"source_tree_dirty": True},
        )
        rows = [current, retained_v2, legacy, dirty]

        forward = series.projection_results(rows)
        reverse = series.projection_results(reversed(rows))

        self.assertEqual(forward, reverse)
        self.assertEqual(forward.row_count, 4)
        self.assertEqual(
            forward.results,
            {
                "current": None,
                "dirty": (
                    "source_tree_dirty is true; dirty source is not checked-in evidence"
                ),
                "legacy": (
                    "stress-series/v1 does not record machine_shortname, "
                    "kernel_version, and host_capabilities"
                ),
                "retained-v2": None,
            },
        )

    def test_one_field_mutation_changes_the_projection_input_digest(self):
        original = row()
        mutated = row(emitted_at="2026-08-25T03:00:01Z")

        before = series.projection_results([original])
        after = series.projection_results([mutated])

        self.assertEqual(before.results, after.results)
        self.assertNotEqual(before.input_jsonl_sha256, after.input_jsonl_sha256)

    def test_duplicate_input_event_id_refuses(self):
        with self.assertRaisesRegex(series.MalformedRow, "repeats event_id"):
            series.projection_results([row(), row(series={"run_index": 1})])

    def test_non_json_projection_input_refuses_with_a_named_series_error(self):
        malformed = row(series={"unexpected": {"not", "json"}})
        with self.assertRaisesRegex(series.MalformedRow, "not canonical JSON"):
            series.projection_results([malformed])

    def test_projection_checker_process_failure_refuses(self):
        failed = subprocess.CompletedProcess(
            ["check-series-rows.rs", "--projection-results"],
            17,
            "",
            "fixture checker failure",
        )
        with mock.patch.object(series.subprocess, "run", return_value=failed):
            with self.assertRaisesRegex(series.MalformedRow, "fixture checker failure"):
                series.projection_results([row()])

    def test_malformed_projection_checker_output_refuses(self):
        malformed = subprocess.CompletedProcess(
            ["check-series-rows.rs", "--projection-results"],
            0,
            "not-json",
            "",
        )
        with mock.patch.object(series.subprocess, "run", return_value=malformed):
            with self.assertRaisesRegex(series.SeriesError, "malformed JSON"):
                series.projection_results([row()])

    def test_projection_checker_count_and_digest_disagreement_refuse(self):
        fixture = row()
        valid_result = [{"event_id": fixture["event_id"], "result": None}]
        cases = [
            (
                {"input_row_count": 2},
                "input_row_count disagrees with rows",
            ),
            (
                {"input_jsonl_sha256": "0" * 64},
                "input_jsonl_sha256 disagrees with rows",
            ),
        ]
        for changes, error in cases:
            with self.subTest(changes=changes):
                completed = self._checker_output([fixture], valid_result, **changes)
                with mock.patch.object(
                    series.subprocess, "run", return_value=completed
                ):
                    with self.assertRaisesRegex(series.SeriesError, error):
                        series.projection_results([fixture])

    def test_projection_checker_missing_duplicate_and_unknown_event_ids_refuse(self):
        fixture = row()
        event_id = fixture["event_id"]
        cases = [
            ([], "omitted event_id"),
            (
                [
                    {"event_id": event_id, "result": None},
                    {"event_id": event_id, "result": None},
                ],
                "repeated event_id",
            ),
            (
                [
                    {"event_id": event_id, "result": None},
                    {"event_id": "unknown", "result": None},
                ],
                "unknown event_id",
            ),
        ]
        for results, error in cases:
            with self.subTest(results=results):
                completed = self._checker_output([fixture], results)
                with mock.patch.object(
                    series.subprocess, "run", return_value=completed
                ):
                    with self.assertRaisesRegex(series.SeriesError, error):
                        series.projection_results([fixture])


class DerivedViews(unittest.TestCase):
    def _diverged(self, run_index, **coords):
        return row(
            event_id=f"01J00000000000000000000{run_index:03d}",
            series={"run_index": run_index, "outcome": "diverged",
                    "coordinates": {**{k: None for k in series.COORDINATE_KEYS}, **coords}},
        )

    def test_observed_range_is_derived_per_coordinate(self):
        rows = [
            self._diverged(0, first_divergent_record=98),
            self._diverged(1, first_divergent_record=12),
            self._diverged(2, first_divergent_record=250),
        ]
        self.assertEqual(
            series.observed_range(rows, "first_divergent_record"),
            {"earliest": 12, "latest": 250, "samples": 3},
        )

    def test_observed_range_counts_collapsed_runs(self):
        collapsed = self._diverged(4, first_divergent_record=98)
        collapsed["series"]["num_runs"] = 30
        collapsed["series"]["last_run_index"] = 87
        self.assertEqual(
            series.observed_range([collapsed], "first_divergent_record"),
            {"earliest": 98, "latest": 98, "samples": 30},
        )

    def test_a_coordinate_no_row_located_is_none_not_zero(self):
        rows = [self._diverged(0, first_divergent_record=98)]
        self.assertIsNone(series.observed_range(rows, "first_divergent_syscall"))

    def test_a_passing_run_contributes_nothing_to_a_range(self):
        rows = [row(), self._diverged(1, first_divergent_record=5)]
        self.assertEqual(
            series.observed_range(rows, "first_divergent_record")["samples"], 1
        )

    def test_reader_ignores_a_located_coordinate_on_a_non_diverged_row(self):
        # ⚠️ CONSTRUCTED DIRECTLY, DELIBERATELY BYPASSING THE LINTER, because
        # this is the case the reader exists to survive. The linter refuses this
        # row at the WRITE boundary -- but the ledger model is explicit that a
        # reader "applies whatever is in the log, in order, without
        # re-litigating it", so a row written before the rule existed, or by a
        # buggy producer, will be read. If `observed_range` did not filter on
        # outcome, that row's position would silently enter a range describing
        # divergences.
        #
        # I FOUND THIS BY MUTATION: removing the outcome filter failed NO test,
        # because the previous case used a passing row with no coordinates at
        # all and therefore could not discriminate.
        smuggled = row(series={
            "outcome": "passed",
            "coordinates": {"first_divergent_record": 7},
        })
        with self.assertRaises(series.MalformedRow):
            series.validate_row(smuggled)          # the writer refuses it
        rows = [smuggled, self._diverged(1, first_divergent_record=500)]
        observed = series.observed_range(rows, "first_divergent_record")
        self.assertEqual(
            observed, {"earliest": 500, "latest": 500, "samples": 1},
            "a non-diverged row's coordinate leaked into the derived range",
        )

    def test_refuses_to_range_across_axes(self):
        # Record 98, syscall 37 and turn 4 described ONE divergence. A function
        # that accepted a list of axes would invite min() across that.
        with self.assertRaises(series.SeriesError):
            series.observed_range([], "first_divergent_anything")

    def test_measurement_state_distinguishes_all_four(self):
        self.assertEqual(series.measurement_state([]), "never-measured")
        self.assertEqual(series.measurement_state([row()]), "measured-and-passed")
        self.assertEqual(
            series.measurement_state([self._diverged(0)]), "diverged-unlocated"
        )
        self.assertEqual(
            series.measurement_state([self._diverged(0, first_divergent_record=1)]),
            "diverged",
        )


class StoreMechanics(unittest.TestCase):
    def test_union_dedups_the_same_row_from_spool_and_shard(self, ):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            spool = Path(d) / "spool.jsonl"
            shard = Path(d) / "shard.jsonl"
            payload = json.dumps(row(), sort_keys=True) + "\n"
            spool.write_text(payload)
            shard.write_text(payload)
            merged = series.union([spool, shard])
            self.assertEqual(len(merged), 1, "event_id dedup did not collapse them")

    def test_publisher_reads_retained_v1_before_appending_current_v3(self):
        legacy = row(schema=series.LEGACY_ENVELOPE_VERSION)
        current = row(event_id="current-v3", series={"run_index": 1})
        merged = publisher.merge_shard(
            json.dumps(legacy, sort_keys=True) + "\n",
            [current],
            store="series",
        )
        self.assertEqual(len(merged.splitlines()), 2)

    def test_publisher_reads_a_retained_v2_spool_that_the_current_writer_refuses(self):
        import tempfile

        retained = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
        del retained["series"]["result"]
        del retained["series"]["failure_class"]
        with self.assertRaisesRegex(
            series.MalformedRow, "new rows must use stress-series/v3"
        ):
            series.validate_rows([retained])

        with tempfile.TemporaryDirectory() as temporary:
            spool = Path(temporary)
            batch = spool / "retained-v2.jsonl"
            batch.write_text(json.dumps(retained, sort_keys=True) + "\n")
            pending, files = publisher._series_spool_snapshot(spool)

        self.assertEqual(files, [batch])
        self.assertEqual(
            pending[(retained["team"], retained["host"], "2026-08")],
            [retained],
        )

    def test_series_for_returns_repetitions_in_run_order(self):
        import tempfile
        rows = [
            row(event_id=f"01J0000000000000000000{i:04d}", series={"run_index": i})
            for i in (2, 0, 1)
        ]
        ordered = series.series_for(rows, rows[0]["series"]["cell"], "a" * 40)
        self.assertEqual([r["series"]["run_index"] for r in ordered], [0, 1, 2])

    def test_append_refuses_the_whole_batch_before_writing_any(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "series" / "s.jsonl"
            good, bad = row(), row(series={"tree": "nope"})
            with self.assertRaises(series.MalformedRow):
                series.append_rows(path, [good, bad])
            self.assertFalse(
                path.exists(),
                "a rejected batch must leave no partial write behind",
            )

    def test_append_then_rewrite_is_a_detectable_violation(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "s.jsonl"
            series.append_rows(path, [row()])
            before = path.read_text()
            after = before.replace('"passed"', '"diverged"')
            self.assertTrue(
                series.verify_append_only(before, after),
                "an edited line must be reported, not silently accepted",
            )

    # A directory whose own entry was never synced can vanish in a crash, with
    # every file renamed into it.

    def test_a_quarantine_file_syncs_the_entry_of_each_directory_created_for_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            synced = []
            with mock.patch.object(series, "_fsync_quarantine_directory",
                                   side_effect=lambda directory: synced.append(Path(directory))):
                series.write_quarantine_file(base / "a" / "b", "one.jsonl", "{}\n")
                self.assertEqual([base, base / "a", base / "a" / "b"], synced)
                synced.clear()
                series.write_quarantine_file(base / "a" / "b", "two.jsonl", "{}\n")
                self.assertEqual([base / "a" / "b"], synced, "only the new file's entry")
            self.assertEqual("{}\n", (base / "a" / "b" / "one.jsonl").read_text())

    def test_syncing_a_quarantine_file_syncs_it_its_entry_and_its_directory_s_entry(self):
        with tempfile.TemporaryDirectory() as temporary:
            held = Path(temporary) / "held"
            path = series.write_quarantine_file(held, "one.jsonl", "{}\n")
            files, directories = [], []
            native = os.fsync

            def fsync(descriptor):
                files.append(Path(os.readlink(f"/proc/self/fd/{descriptor}")))
                native(descriptor)

            with mock.patch.object(series.os, "fsync", side_effect=fsync), \
                 mock.patch.object(series, "_fsync_quarantine_directory",
                                   side_effect=lambda directory: directories.append(Path(directory))):
                series.sync_quarantine_file(path)
            self.assertEqual([path.resolve()], files)
            self.assertEqual([held, held.parent], directories)

    def test_setting_aside_a_quarantine_file_syncs_the_new_corrupt_directory_s_entry(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "series-quarantine"
            (root / "held").mkdir(parents=True)
            (root / "held" / "bad.jsonl").write_text("not json\n")
            synced = []
            with mock.patch.object(series, "_fsync_quarantine_directory",
                                   side_effect=lambda directory: synced.append(Path(directory))):
                moved = series.set_aside_unreadable_quarantine(root, host="fixture", folder="held")
            corrupt = root / series.QUARANTINE_CORRUPT_SUBDIR
            self.assertEqual(["held--bad.jsonl"], moved)
            self.assertEqual([root, corrupt, root / "held"], synced)


class Publication(unittest.TestCase):
    def setUp(self):
        # These existing fixtures are the selected history repository. Identity
        # routing across distinct repos is covered by test_mirror.py.
        self.enterContext(mock.patch.object(series, "ledger_root", side_effect=lambda parent: Path(parent)))

    def _git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    def test_a_series_run_one_parent_writes_is_readable_by_another(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bare = root / "origin.git"
            producer = root / "producer"
            publisher_clone = root / "publisher"
            reader = root / "reader"
            subprocess.run(
                ["git", "init", "-q", "--bare", "-b", "main", str(bare)],
                check=True,
            )
            subprocess.run(
                ["git", "init", "-q", "-b", "main", str(producer)], check=True,
            )
            (producer / "README").write_text("series\n")
            self._git(producer, "add", "README")
            self._git(
                producer,
                "-c", "user.name=t",
                "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "seed",
            )
            self._git(producer, "remote", "add", "origin", str(bare))
            self._git(producer, "push", "-q", "origin", "main")
            subprocess.run(
                ["git", "clone", "-q", str(bare), str(publisher_clone)], check=True,
            )
            subprocess.run(["git", "clone", "-q", str(bare), str(reader)], check=True)

            test = "backend-parity-c/flock-lifecycle"
            observation = row(series={"cell": f"{test}/verify/ptrace"})
            spool = series.spool_dir(producer)
            spool.mkdir(parents=True)
            series.append_rows(spool / "campaign-1.jsonl", [observation])

            before = series.history_rows(series.union(series.store_paths(reader)), test)
            self.assertEqual(before, [], "an independent checkout must not see local spool data")
            self.assertEqual(set(publish_backlog.pending_stores(producer)), {"series"})

            report = publisher.publish(
                publisher_clone,
                spool,
                source_root=producer,
                store="series",
            )
            self.assertEqual(report["published"], 1)
            self.assertEqual(
                report["shards"], ["series/hermit/devbig014/2026-08.jsonl"]
            )
            self.assertEqual(
                len(series.history_rows(series.union(series.store_paths(producer)), test)),
                1,
                "the producer must retain visibility until it fetches the tracked shard",
            )

            reader_head = self._git(reader, "rev-parse", "HEAD")
            after = series.history_rows(series.published_rows(reader), test)
            self.assertEqual(len(after), 1)
            self.assertEqual(after[0]["event_id"], observation["event_id"])
            import contextlib
            import io

            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                rc = series.main(["history", "--parent", str(reader), "--test", test])
            self.assertEqual(rc, 0)
            self.assertIn("rows=1 runs=1 source=test-ledger-origin-main+local-spool", stdout.getvalue())
            self.assertEqual(
                self._git(reader, "rev-parse", "HEAD"),
                reader_head,
                "the query must read fetched parent main without moving the checkout",
            )

    def test_publish_drains_a_retained_v2_spool_without_reopening_v2_writes(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bare = root / "origin.git"
            producer = root / "producer"
            publisher_clone = root / "publisher"
            subprocess.run(
                ["git", "init", "-q", "--bare", "-b", "main", str(bare)],
                check=True,
            )
            subprocess.run(
                ["git", "init", "-q", "-b", "main", str(producer)], check=True,
            )
            (producer / "README").write_text("series\n")
            self._git(producer, "add", "README")
            self._git(
                producer,
                "-c", "user.name=t",
                "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "seed",
            )
            self._git(producer, "remote", "add", "origin", str(bare))
            self._git(producer, "push", "-q", "origin", "main")
            subprocess.run(
                ["git", "clone", "-q", str(bare), str(publisher_clone)], check=True,
            )

            retained = row(schema=series.HOST_FACTS_ENVELOPE_VERSION)
            del retained["series"]["result"]
            del retained["series"]["failure_class"]
            with self.assertRaisesRegex(
                series.MalformedRow, "new rows must use stress-series/v3"
            ):
                series.validate_rows([retained])

            spool = series.spool_dir(producer)
            spool.mkdir(parents=True)
            batch = spool / "retained-v2.jsonl"
            batch.write_text(json.dumps(retained, sort_keys=True) + "\n")

            report = publisher.publish(
                publisher_clone,
                spool,
                source_root=producer,
                store="series",
            )
            self.assertEqual(report["published"], 1)
            self.assertFalse(batch.exists())
            published = self._git(
                publisher_clone,
                "show",
                "origin/main:series/hermit/devbig014/2026-08.jsonl",
            )
            parsed = json.loads(published)
            self.assertEqual(series.validate_read_rows([parsed]), [retained])
    def test_published_snapshot_stays_on_one_commit_when_the_ref_moves(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary) / "parent"
            subprocess.run(
                ["git", "init", "-q", "-b", "main", str(repo)], check=True
            )
            shard = repo / "series/hermit/devbig014/2026-08.jsonl"
            shard.parent.mkdir(parents=True)
            first = row()
            shard.write_text(json.dumps(first, sort_keys=True) + "\n")
            self._git(repo, "add", "series")
            self._git(
                repo,
                "-c", "user.name=t",
                "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "first row",
            )
            first_commit = self._git(repo, "rev-parse", "HEAD")

            second = row(
                event_id="01J000000000000000000000AB",
                series={"run_index": 1},
            )
            with shard.open("a") as output:
                output.write(json.dumps(second, sort_keys=True) + "\n")
            self._git(repo, "add", "series")
            self._git(
                repo,
                "-c", "user.name=t",
                "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "second row",
            )
            second_commit = self._git(repo, "rev-parse", "HEAD")
            published_ref = "refs/remotes/origin/main"
            self._git(repo, "update-ref", published_ref, first_commit)

            real_git = series._git

            def move_after_resolution(checkout, args):
                result = real_git(checkout, args)
                if args == [
                    "--no-replace-objects",
                    "rev-parse",
                    "--verify",
                    f"{published_ref}^{{commit}}",
                ]:
                    self._git(repo, "update-ref", published_ref, second_commit)
                return result

            with mock.patch.object(series, "_git", side_effect=move_after_resolution):
                snapshot = series.published_snapshot_at(repo, published_ref)

            self.assertEqual(snapshot.commit, first_commit)
            self.assertEqual([item["event_id"] for item in snapshot.rows], [first["event_id"]])
            self.assertEqual(self._git(repo, "rev-parse", published_ref), second_commit)

    def test_published_snapshot_fetches_and_resolves_origin_main_once(self):
        expected = series.PublishedSnapshot("a" * 40, [])
        with mock.patch.object(series, "refresh_origin_main") as refresh:
            with mock.patch.object(
                series, "published_snapshot_at", return_value=expected
            ) as snapshot_at:
                actual = series.published_snapshot(Path("/parent"))
        self.assertIs(actual, expected)
        refresh.assert_called_once_with(Path("/parent"), "cell ledger")
        snapshot_at.assert_called_once_with(
            Path("/parent"), "refs/remotes/origin/main", repository=series.LEDGER_REPOSITORY
        )

    def _shard_repo(self, temporary: str, shards: dict[str, bytes]) -> tuple[Path, str]:
        repo = Path(temporary) / "parent"
        subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
        for relative, content in shards.items():
            path = repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self._git(repo, "add", "series")
        self._git(
            repo, "-c", "user.name=t", "-c", "user.email=t@invalid",
            "commit", "-q", "-m", "shards",
        )
        return repo, self._git(repo, "rev-parse", "HEAD")

    def test_git_blobs_reads_every_path_in_order_from_one_git(self):
        # One empty shard, one non-ASCII, one larger than a pipe buffer.
        shards = {
            "series/hermit/devbig014/2026-08.jsonl": b"",
            "series/hermit/devbig014/2026-09.jsonl": '{"a": "\u00e9t\u00e9"}\n'.encode(),
            "series/hermit/devbig030/2026-09.jsonl": b'{"b": 1}\n' * 40000,
        }
        with tempfile.TemporaryDirectory() as temporary:
            repo, commit = self._shard_repo(temporary, shards)
            paths = list(reversed(shards))
            with mock.patch.object(
                series.subprocess, "Popen", wraps=series.subprocess.Popen
            ) as popen:
                blobs = list(series._git_blobs(repo, commit, paths))
            self.assertEqual(popen.call_count, 1)
            self.assertEqual(
                blobs, [(relative, shards[relative].decode()) for relative in paths]
            )

    def test_git_blobs_refuses_a_path_the_commit_lacks(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo, commit = self._shard_repo(
                temporary, {"series/hermit/devbig014/2026-08.jsonl": b"{}\n"}
            )
            with self.assertRaisesRegex(series.SeriesError, "missing"):
                list(series._git_blobs(repo, commit, ["series/hermit/devbig014/2026-07.jsonl"]))

    def test_git_blobs_kills_a_git_that_stops_making_progress(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake = Path(temporary) / "bin"
            fake.mkdir()
            (fake / "git").write_text("#!/bin/sh\nexec sleep 30\n")
            (fake / "git").chmod(0o755)
            started = time.monotonic()
            with mock.patch.dict(
                os.environ, {"PATH": f"{fake}{os.pathsep}{os.environ['PATH']}"}
            ):
                with self.assertRaisesRegex(series.SeriesError, "ended without output"):
                    list(series._git_blobs(
                        Path(temporary), "a" * 40, ["series/a/b/2026-08.jsonl"],
                        idle_timeout=1,
                    ))
            self.assertLess(time.monotonic() - started, 10)

    def test_published_snapshot_refuses_a_shard_without_a_final_newline(self):
        with tempfile.TemporaryDirectory() as temporary:
            relative = "series/hermit/devbig014/2026-08.jsonl"
            repo, commit = self._shard_repo(
                temporary, {relative: json.dumps(row(), sort_keys=True).encode()}
            )
            with self.assertRaisesRegex(
                series.MalformedEvent,
                f"parent {commit}:{relative}: final line has no newline terminator",
            ):
                series._published_snapshot_at_repo(repo, commit, repository="x")

    # published_rows_at_repo streams what _published_snapshot_at_repo read as
    # one list of every row (https://github.com/rrnewton/dev-hermit/issues/554).
    # Both run here on the same commit and must agree exactly, refusals included.
    @staticmethod
    def _checker(returncode=0, stderr=""):
        def run(checker_input, arguments):
            assert arguments == ["--read"], arguments
            return subprocess.CompletedProcess(["checker"], returncode, "", stderr)
        return run

    def _both(self, shards, checker=None):
        checker = checker or self._checker()
        with tempfile.TemporaryDirectory() as temporary:
            repo, commit = self._shard_repo(temporary, shards)
            outcomes = []
            for read in (
                lambda: series._published_snapshot_at_repo(repo, commit, repository="x").rows,
                lambda: series.published_rows_at_repo(repo, commit, repository="x"),
            ):
                with mock.patch.object(series, "_run_shared_checker", side_effect=checker) as called:
                    try:
                        outcomes.append(("rows", read(), called.call_args_list))
                    except Exception as error:  # the refusal is the outcome compared
                        outcomes.append(("refused", (type(error), str(error)), called.call_args_list))
            return outcomes

    def test_streamed_published_rows_equal_the_published_snapshot(self):
        a = row(event_id="01J000000000000000000000AA", emitted_at="2026-08-25T03:00:02Z")
        b = row(event_id="01J000000000000000000000BB", emitted_at="2026-08-25T03:00:01Z",
                run_id="caf\u00e9-\u2603")
        c = row(event_id="01J000000000000000000000CC", emitted_at="2026-08-25T03:00:01Z")
        reordered_a = "{" + ", ".join(f"{json.dumps(k)}: {json.dumps(v)}" for k, v in reversed(list(a.items()))) + "}"
        shards = {
            "series/hermit/devbig014/2026-08.jsonl": (json.dumps(a) + "\n\n" + json.dumps(c) + "\n").encode(),
            "series/hermit/devbig030/2026-08.jsonl": (json.dumps(b) + "\n" + reordered_a + "\n").encode(),
        }
        (old_kind, old_rows, old_calls), (new_kind, new_rows, new_calls) = self._both(shards)
        self.assertEqual(("rows", "rows"), (old_kind, new_kind), (old_rows, new_rows))
        self.assertEqual(old_rows, list(new_rows))
        self.assertEqual([row_["event_id"][-2:] for row_ in old_rows], ["BB", "CC", "AA"])
        self.assertEqual(hashlib.sha256(series._canonical_rows_bytes(old_rows)).hexdigest(), new_rows.digest())
        self.assertEqual("2026-08-25T03:00:02Z", new_rows.max_emitted_at)
        # The checker sees the same rows in the same order, the duplicate included.
        self.assertEqual(old_calls, new_calls)

    def test_streamed_published_rows_refuse_as_the_published_snapshot_does(self):
        owner = row(event_id="01J000000000000000000000OW", run_id="/" + "home" + "/someone/x")
        first = row(event_id="01J000000000000000000000DD")
        conflicts = [dict(first, run_id="other"), row(event_id="01J000000000000000000000EE"),
                     dict(row(event_id="01J000000000000000000000EE"), run_id="later")]
        cases = {
            "conflict": ({"series/hermit/devbig014/2026-08.jsonl":
                          "".join(json.dumps(r) + "\n" for r in [first, *conflicts]).encode()}, None),
            "owner-path": ({"series/hermit/devbig014/2026-08.jsonl":
                            (json.dumps(first) + "\n" + json.dumps(owner) + "\n").encode()}, None),
            "parse-error-after-owner-path": ({
                "series/hermit/devbig014/2026-08.jsonl": (json.dumps(owner) + "\n").encode(),
                "series/hermit/devbig030/2026-08.jsonl": b"{not json\n",
            }, None),
            "checker-refusal": ({"series/hermit/devbig014/2026-08.jsonl": (json.dumps(first) + "\n").encode()},
                                self._checker(1, "check-series-rows: stdin line 1: refused\n")),
        }
        for name, (shards, checker) in cases.items():
            with self.subTest(name):
                (old_kind, old, old_calls), (new_kind, new, new_calls) = self._both(shards, checker)
                self.assertEqual("refused", old_kind, old)
                self.assertEqual((old_kind, old), (new_kind, new))
                self.assertEqual(len(old_calls), len(new_calls))

    def test_streamed_published_rows_of_an_empty_series_run_no_checker(self):
        (_, old, old_calls), (_, new, new_calls) = self._both({"series/hermit/devbig014/2026-08.jsonl": b"\n"})
        self.assertEqual([], old)
        self.assertEqual(0, len(new))
        self.assertEqual([], old_calls)
        self.assertEqual([], new_calls)

    # The streamed read decides duplicates by comparing canonical texts
    # (compact separators, ensure_ascii=False) where ledger.union_events
    # compares json.dumps(event, sort_keys=True). The two encodings differ only
    # in separator tokens and string escaping, and each is decodable, so two
    # rows have equal texts exactly when they have equal union bodies. These
    # pin that, and which duplicate is reported, against the snapshot reader.
    @staticmethod
    def _published_rows_parts(rows):
        return (bytes(rows._texts), list(rows._ends), bytes(rows._ids), list(rows._id_ends),
                rows.max_emitted_at, len(rows), rows.digest())

    @staticmethod
    def _line(value, *, ensure_ascii=True, reverse=False, separators=None):
        if reverse:
            value = dict(reversed(list(value.items())))
        return json.dumps(value, ensure_ascii=ensure_ascii, separators=separators) + "\n"

    def test_streamed_published_rows_collapse_the_duplicates_union_events_collapses(self):
        accented = row(event_id="01J000000000000000000000AC", run_id="café-☃-\U0001f600",
                       emitted_at="2026-08-25T03:00:03Z")
        escaped_text = row(event_id="01J000000000000000000000ES", run_id="\\u2603 a, b: c",
                           emitted_at="2026-08-25T03:00:01Z")
        not_a_number = row(event_id="01J000000000000000000000NN", emitted_at="2026-08-25T03:00:01Z")
        not_a_number["series"]["extra_nan"] = float("nan")
        plain = row(event_id="01J000000000000000000000PL", emitted_at="2026-08-25T03:00:02Z")
        shards = {
            "series/hermit/devbig014/2026-08.jsonl": "".join([
                self._line(accented), self._line(escaped_text), self._line(not_a_number),
                self._line(plain),
            ]).encode(),
            # The same events again: raw UTF-8 instead of \u escapes, the keys
            # in another order, compact separators.
            "series/hermit/devbig030/2026-08.jsonl": "".join([
                self._line(accented, ensure_ascii=False, reverse=True),
                self._line(escaped_text, ensure_ascii=False, separators=(",", ":")),
                self._line(not_a_number, reverse=True),
                self._line(plain, ensure_ascii=False, reverse=True, separators=(",", ":")),
            ]).encode(),
        }
        (old_kind, old_rows, _), (new_kind, new_rows, _) = self._both(shards)
        self.assertEqual(("rows", "rows"), (old_kind, new_kind), (old_rows, new_rows))
        self.assertEqual(self._published_rows_parts(series.PublishedRows(old_rows)),
                         self._published_rows_parts(new_rows))
        self.assertEqual([r["event_id"][-2:] for r in old_rows], ["ES", "NN", "PL", "AC"])

    def test_streamed_published_rows_refuse_each_conflict_union_events_refuses(self):
        def conflict_of(first, second):
            return {"series/hermit/devbig014/2026-08.jsonl":
                    (self._line(first) + self._line(second, ensure_ascii=False)).encode()}

        base = row(event_id="01J000000000000000000000CF")
        cases = {
            # Equal under Python's ==, different JSON: union_events refuses.
            "int-vs-float": conflict_of(row(event_id=base["event_id"], series={"run_index": 1}),
                                        row(event_id=base["event_id"], series={"run_index": 1.0})),
            "true-vs-1": conflict_of(row(event_id=base["event_id"], series={"run_index": True}),
                                     row(event_id=base["event_id"], series={"run_index": 1})),
            "composed-vs-decomposed": conflict_of(dict(base, run_id="caf\u00e9"),
                                                  dict(base, run_id="cafe\u0301")),
            "escape-text-vs-character": conflict_of(dict(base, run_id="\\u2603"),
                                                    dict(base, run_id="☃")),
            "separator-inside-a-string": conflict_of(dict(base, run_id="a, b"),
                                                     dict(base, run_id="a,b")),
            "key-vs-string": conflict_of(dict(base, run_id={"a": "b"}),
                                         dict(base, run_id='{"a": "b"}')),
            # Equal-length canonical texts: a comparison of lengths alone
            # would collapse these instead of refusing.
            "equal-length-strings": conflict_of(dict(base, run_id="campaign-1"),
                                                dict(base, run_id="campaign-2")),
            "true-vs-null": conflict_of(row(event_id=base["event_id"], series={"run_index": True}),
                                        row(event_id=base["event_id"], series={"run_index": None})),
        }
        for name, shards in cases.items():
            with self.subTest(name):
                (old_kind, old, _), (new_kind, new, _) = self._both(shards)
                self.assertEqual(("refused", series.ConflictingEventId), (old_kind, old[0]), old)
                self.assertEqual((old_kind, old), (new_kind, new))

    def test_streamed_published_rows_report_the_conflict_union_events_reports(self):
        # The first differing repeat in reading order is the one reported, not
        # the first in event-id order, across shards, and a repeat identical to
        # an earlier repeat collapses.
        x = row(event_id="01J000000000000000000000AA")
        y = row(event_id="01J000000000000000000000ZZ")
        cases = {
            "later-id-conflicts-first": {
                "series/hermit/devbig014/2026-08.jsonl": "".join([
                    self._line(x), self._line(y), self._line(y, reverse=True),
                    self._line(dict(y, run_id="y-differs")), self._line(dict(x, run_id="x-differs")),
                ]).encode()},
            "conflict-across-shards": {
                "series/hermit/devbig014/2026-08.jsonl": (self._line(y) + self._line(x)).encode(),
                "series/hermit/devbig030/2026-08.jsonl": "".join([
                    self._line(x, ensure_ascii=False), self._line(dict(x, run_id="x-differs")),
                    self._line(dict(y, run_id="y-differs")),
                ]).encode()},
            "repeat-of-a-repeat": {
                "series/hermit/devbig014/2026-08.jsonl": "".join([
                    self._line(y), self._line(dict(x, run_id="x-1")), self._line(dict(x, run_id="x-1")),
                    self._line(dict(y, run_id="y-differs")), self._line(dict(x, run_id="x-2")),
                ]).encode()},
        }
        expected = {"later-id-conflicts-first": "ZZ", "conflict-across-shards": "AA",
                    "repeat-of-a-repeat": "ZZ"}
        for name, shards in cases.items():
            with self.subTest(name):
                (old_kind, old, _), (new_kind, new, _) = self._both(shards)
                self.assertEqual(("refused", series.ConflictingEventId), (old_kind, old[0]), old)
                self.assertIn(f"'01J000000000000000000000{expected[name]}'", old[1])
                self.assertEqual((old_kind, old), (new_kind, new))

    def test_streamed_published_rows_refuse_a_conflict_only_after_every_other_refusal(self):
        # A ledger holding a conflicting repeat AND another fault reports the
        # other fault, as the snapshot reader does: unreadable or truncated
        # shards and malformed lines first, then an owner path, then the
        # shared checker, and the conflict last.
        first = row(event_id="01J000000000000000000000DD")
        conflict = (self._line(first) + self._line(dict(first, run_id="other"))).encode()
        owner = self._line(row(event_id="01J000000000000000000000OW",
                               run_id="/" + "home" + "/someone/x")).encode()
        a, b = "series/hermit/devbig014/2026-08.jsonl", "series/hermit/devbig030/2026-08.jsonl"
        cases = {
            "malformed-line": ({a: conflict, b: b"{not json\n"}, None, series.MalformedEvent),
            "truncated-shard": ({a: conflict, b: self._line(row()).encode()[:-1]}, None,
                                series.MalformedEvent),
            "owner-path": ({a: conflict, b: owner}, None, series.MalformedRow),
            "checker": ({a: conflict}, self._checker(1, "check-series-rows: stdin line 2: refused\n"),
                        series.MalformedRow),
        }
        for name, (shards, checker, kind) in cases.items():
            with self.subTest(name):
                (old_kind, old, _), (new_kind, new, _) = self._both(shards, checker)
                self.assertEqual(("refused", kind), (old_kind, old[0]), old)
                self.assertEqual((old_kind, old), (new_kind, new))

    def test_streamed_published_rows_order_by_stamp_then_id_with_odd_identities(self):
        # Rows that sort differently by id than by stamp, ids and stamps that are
        # prefixes of one another, non-ASCII ids, and rows missing either field.
        shards = {"series/hermit/devbig014/2026-08.jsonl": "".join(self._line(r) for r in [
            row(event_id="01J000000000000000000000B", emitted_at="2026-08-25T03:00:00Z"),
            row(event_id="01J000000000000000000000BA", emitted_at="2026-08-25T03:00:00"),
            row(event_id="01J000000000000000000000é", emitted_at="2026-08-25T03:00:00Z"),
            row(event_id="01J000000000000000000000A", emitted_at="2026-08-25T03:00:01Z"),
            {key: value for key, value in row(event_id="01J000000000000000000000C").items()
             if key != "emitted_at"},
            {key: value for key, value in row(emitted_at="2026-08-25T02:00:00Z").items()
             if key != "event_id"},
        ]).encode()}
        (old_kind, old_rows, _), (new_kind, new_rows, _) = self._both(shards)
        self.assertEqual(("rows", "rows"), (old_kind, new_kind), (old_rows, new_rows))
        self.assertEqual(old_rows, list(new_rows))
        self.assertEqual([r.get("event_id") for r in old_rows], [r.get("event_id") for r in new_rows])
        texts = [bytes(text) for text in new_rows.texts()]
        self.assertEqual(texts, [json.dumps(r, sort_keys=True, separators=(",", ":"),
                                            ensure_ascii=False).encode() for r in old_rows])
        self.assertEqual(max(str(r.get("emitted_at", "")) for r in old_rows), new_rows.max_emitted_at)

    def test_series_reap_view_keeps_only_the_rows_retained_batches_name(self):
        # The view used to hold every published row; it now holds the rows of
        # the event ids the retained batches name, each equal to the old view's.
        a = row(event_id="01J000000000000000000000AA")
        b = row(event_id="01J000000000000000000000BB", emitted_at="2026-08-25T03:00:01Z")
        c = row(event_id="01J000000000000000000000CC", emitted_at="2026-08-25T03:00:02Z")
        shards = {"series/hermit/devbig014/2026-08.jsonl":
                  "".join(json.dumps(r) + "\n" for r in (a, b, c)).encode()}
        with tempfile.TemporaryDirectory() as temporary:
            repo, commit = self._shard_repo(temporary, shards)
            subprocess.run(["git", "-C", str(repo), "update-ref", "refs/remotes/origin/main", commit],
                           check=True, capture_output=True)
            spool = Path(temporary) / "spool"
            retained = publisher.retained_dir(spool)
            retained.mkdir(parents=True)
            missing = row(event_id="01J000000000000000000000ZZ")
            (retained / "batch.jsonl").write_text(json.dumps(b) + "\n" + json.dumps(missing) + "\n")
            (retained / "corrupt.jsonl").write_text("{not json\n")
            with mock.patch.object(series, "_run_shared_checker", side_effect=self._checker()):
                view = publisher._reap_view(spool, repo, store="series")
                old = {r["event_id"]: r for r in
                       series._published_snapshot_at_repo(repo, "refs/remotes/origin/main", repository="x").rows}
        self.assertEqual({b["event_id"]: old[b["event_id"]]}, view)
        self.assertEqual(b, view[b["event_id"]])

    def test_series_publication_refuses_a_row_the_series_linter_rejects(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bare = root / "origin.git"
            clone = root / "clone"
            producer = root / "producer"
            subprocess.run(
                ["git", "init", "-q", "--bare", "-b", "main", str(bare)],
                check=True,
            )
            subprocess.run(
                ["git", "init", "-q", "-b", "main", str(producer)], check=True,
            )
            (producer / "README").write_text("series\n")
            self._git(producer, "add", "README")
            self._git(
                producer,
                "-c", "user.name=t",
                "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "seed",
            )
            self._git(producer, "remote", "add", "origin", str(bare))
            self._git(producer, "push", "-q", "origin", "main")
            subprocess.run(["git", "clone", "-q", str(bare), str(clone)], check=True)

            spool = series.spool_dir(producer)
            spool.mkdir(parents=True)
            invalid = row(series={"outcome": "passed", "coordinates": {
                "first_divergent_record": 1,
            }})
            (spool / "invalid.jsonl").write_text(json.dumps(invalid) + "\n")

            with self.assertRaises(publisher.PublishRefused):
                publisher.publish(clone, spool, source_root=producer, store="series")
            self.assertTrue((spool / "invalid.jsonl").exists())
            self.assertFalse((clone / "series").exists())

    def test_scheduled_sweep_passes_the_series_store_to_the_publisher(self):
        from unittest import mock

        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"published": 1}\n', stderr="",
        )
        with mock.patch.object(publish_backlog.subprocess, "run", return_value=completed) as run:
            report = publish_backlog.publish_parent(
                Path("/producer"),
                Path("/publisher"),
                store="series",
                spool=Path("/producer/ignored/ci-hub/series-spool"),
                timeout=60,
                attribution="test attribution",
            )
        command = run.call_args.args[0]
        self.assertTrue(command[1].endswith("ci-hub/series/mirror.py"))
        self.assertIn("--publish-only", command)
        self.assertEqual(command[command.index("--parent") + 1], "/producer")
        self.assertNotIn("--writer", command)
        self.assertEqual(report["store"], "series")
        self.assertEqual(report["published"], 1)

    def test_scheduled_sweep_fails_the_parity_store_while_a_deferred_call_is_kept(self):
        # The parity store's mirror exits 0 when its replay keeps a deferred
        # append-parity call or fails outright, and its parity spool may be
        # empty; the store must still fail while those rows are off the ledger.
        # A series failure exits 1 with a failure report, and the store is
        # judged by the same sub-reports there.
        def sweep(raw_publication, failed=False):
            mirrored = ({"state": "failed", "error": "SeriesPublicationFailed: deliberate series refusal",
                         "raw_publication": {"quarantine": quarantine, **raw_publication}}
                        if failed else {"state": "published", "raw_publication": raw_publication})
            completed = subprocess.CompletedProcess(
                args=[], returncode=1 if failed else 0, stderr="",
                stdout=json.dumps(mirrored) + "\n")
            with mock.patch.object(publish_backlog.subprocess, "run", return_value=completed):
                report = publish_backlog.publish_parent(
                    Path("/producer"), Path("/publisher"), store="parity",
                    spool=Path("/producer/ignored/ci-hub/parity-spool"), timeout=60,
                    attribution="test attribution")
            return report["rc"], report["published"], report.get("error")

        parity = {"published": 193, "batches": ["a.jsonl"], "refused": [], "error": None}
        quarantine = {"published": 0, "error": None}
        replay = {"replayed": 1, "appended": 1, "already_recorded": 0, "refused": 0,
                  "batches": ["a.jsonl"], "kept": []}
        kept = {**replay, "kept": [{"file": "k.jsonl", "reason": "unreadable: k.jsonl: is not exactly one line"}]}
        refused = {**parity, "refused": [{"batch": "r.jsonl", "error": "row 1 is malformed"}]}
        cases = {
            "a kept call": (
                {"parity": parity, "parity_deferred": kept},
                (1, 193, "1 kept parity call(s) not replayed: k.jsonl: unreadable: k.jsonl: "
                         "is not exactly one line")),
            "a failed replay": (
                {"parity": parity, "parity_deferred": {"error": "OSError: [Errno 5] Input/output error"}},
                (1, 193, "replaying the kept parity calls failed: OSError: [Errno 5] Input/output error")),
            "a refused batch and a kept call": (
                {"parity": refused, "parity_deferred": kept},
                (1, 193, "1 parity batch(es) refused: r.jsonl: row 1 is malformed; 1 kept parity "
                         "call(s) not replayed: k.jsonl: unreadable: k.jsonl: is not exactly one line")),
            "a refused batch alone": (
                {"parity": refused, "parity_deferred": replay},
                (1, 193, "1 parity batch(es) refused: r.jsonl: row 1 is malformed")),
            "a failed parity publication": (
                {"parity": {**parity, "error": "push refused"}, "parity_deferred": kept},
                (1, 0, "push refused")),
            "every call replayed": ({"parity": parity, "parity_deferred": replay}, (0, 193, None)),
            # This store's mirror always replays, so a report without the
            # replay says nothing about the kept calls.
            "no replay": ({"parity": parity},
                          (1, 193, "the mirror reported no replay of the kept parity calls")),
        }
        for name, (raw_publication, expected) in cases.items():
            with self.subTest(name):
                self.assertEqual(expected, sweep(raw_publication))
        failed_cases = {
            "a series failure and a kept call": (
                {"parity": parity, "parity_deferred": kept},
                (1, 193, "1 kept parity call(s) not replayed: k.jsonl: unreadable: k.jsonl: "
                         "is not exactly one line")),
            "a series failure and a failed replay": (
                {"parity": parity, "parity_deferred": {"error": "OSError: [Errno 5] Input/output error"}},
                (1, 193, "replaying the kept parity calls failed: OSError: [Errno 5] Input/output error")),
            "a series failure without the replay": (
                {"parity": parity},
                (1, 193, "the mirror reported no replay of the kept parity calls")),
            # The series store reports the rows; this store published its
            # batches alone and replayed every call, so it passes.
            "a series failure, every call replayed": (
                {"parity": parity, "parity_deferred": replay},
                (0, 193, "SeriesPublicationFailed: deliberate series refusal")),
        }
        for name, (raw_publication, expected) in failed_cases.items():
            with self.subTest(name):
                self.assertEqual(expected, sweep(raw_publication, failed=True))


class SeriesShardParts(unittest.TestCase):
    """A host's series month rolls to a new part before it can jam a push.

    GitHub refuses a pushed file over 100 MiB, and one host's month outgrew
    that: https://github.com/rrnewton/dev-hermit/issues/453. Every test here
    shrinks ``SERIES_SHARD_CEILING_BYTES`` to a few rows so the arithmetic is
    visible; the schema checker is stubbed because these tests are about where
    rows go, not whether they are well formed.
    """

    DIRECTORY = "series/hermit/devbig014"

    def setUp(self):
        self.enterContext(mock.patch.object(series, "ledger_root", side_effect=lambda parent: Path(parent)))
        self.enterContext(mock.patch.object(
            series, "_run_shared_checker",
            return_value=subprocess.CompletedProcess(["schema-fixture"], 0, "", ""),
        ))
        self.enterContext(mock.patch.object(publisher, "_series_store", return_value=series))
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="series-parts-")))
        self.repos = 0

    def _git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), *args], capture_output=True, text=True, check=True,
        ).stdout.strip()

    @staticmethod
    def _row(index, **over):
        # Four-digit run indexes keep every row the same width.
        return row(event_id=f"01J0000000000000000000{index:04d}", series={"run_index": index}, **over)

    @staticmethod
    def _line(event):
        return json.dumps(event, sort_keys=True) + "\n"

    def _width(self):
        widths = {len(self._line(self._row(index)).encode()) for index in range(1000, 1010)}
        self.assertEqual(len(widths), 1, "fixture rows must share one width")
        return widths.pop()

    def _ceiling(self, value):
        self.enterContext(mock.patch.object(publisher, "SERIES_SHARD_CEILING_BYTES", value))

    def _repo(self, files):
        self.repos += 1
        repo = self.root / f"ledger-{self.repos}"
        subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
        (repo / "README").write_text("ledger\n")
        for name, content in files.items():
            path = repo / self.DIRECTORY / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content if isinstance(content, str) else "".join(map(self._line, content)))
        self._git(repo, "add", "-A")
        self._git(repo, "-c", "user.name=t", "-c", "user.email=t@invalid", "commit", "-q", "-m", "seed")
        return repo

    def _plan(self, repo, events):
        return publisher._plan_series_month(repo, "HEAD", "hermit", "devbig014", "2026-08", events)

    def _path(self, part):
        return series.shard_part_path("hermit", "devbig014", "2026-08", part)

    def test_an_append_that_would_pass_the_ceiling_starts_the_next_part(self):
        width = self._width()
        self._ceiling(3 * width)
        first = self._row(1000)
        repo = self._repo({"2026-08.jsonl": [first]})
        new = [self._row(index) for index in range(1001, 1006)]

        planned = self._plan(repo, new)

        published = self._line(first)
        self.assertEqual(planned, [
            (self._path(0), published, published + "".join(map(self._line, new[:2]))),
            (self._path(1), "", "".join(map(self._line, new[2:]))),
        ])
        for name, _, merged in planned:
            self.assertLessEqual(len(merged.encode()), 3 * width, name)

    def test_appends_go_to_the_highest_numbered_part(self):
        self._ceiling(1024 * 1024)
        top = self._row(1003)
        # Part 4 is missing, so neither the number of parts nor the number of
        # month files names the highest one.
        repo = self._repo({
            "2026-08-01-to-12.jsonl": [self._row(1000)],
            "2026-08.jsonl": [self._row(1001)],
            "2026-08.part-0001.jsonl": [self._row(1002)],
            "2026-08.part-0005.jsonl": [top],
            "2026-09.jsonl": [self._row(1009, emitted_at="2026-09-01T00:00:00Z")],
        })
        new = self._row(1004)

        planned = self._plan(repo, [new])

        self.assertEqual(planned, [
            (self._path(5), self._line(top), self._line(top) + self._line(new)),
        ])

    def test_a_part_already_over_the_ceiling_gets_no_new_row(self):
        width = self._width()
        self._ceiling(2 * width)
        # A month shard published before parts existed can be over the ceiling.
        legacy = "".join(self._line(self._row(index)) for index in range(1000, 1003))
        repo = self._repo({"2026-08.jsonl": legacy})
        new = self._row(1003)

        planned = self._plan(repo, [new])

        self.assertEqual(planned, [
            (self._path(0), legacy, legacy),
            (self._path(1), "", self._line(new)),
        ])

    def test_a_part_may_reach_the_ceiling_exactly(self):
        width = self._width()
        first, new = self._row(1000), self._row(1001)
        repo = self._repo({"2026-08.jsonl": [first]})

        self._ceiling(2 * width)
        self.assertEqual(self._plan(repo, [new]), [
            (self._path(0), self._line(first), self._line(first) + self._line(new)),
        ])

        self._ceiling(2 * width - 1)
        self.assertEqual(self._plan(repo, [new]), [
            (self._path(0), self._line(first), self._line(first)),
            (self._path(1), "", self._line(new)),
        ])

    def test_a_row_wider_than_the_ceiling_is_refused_not_split(self):
        width = self._width()
        self._ceiling(width - 1)
        repo = self._repo({})

        with self.assertRaisesRegex(publisher.PublishRefused, "one series row is"):
            self._plan(repo, [self._row(1000)])

    def test_a_changed_body_in_a_closed_part_is_refused_naming_the_file(self):
        self._ceiling(1024 * 1024)
        original = self._row(1000)
        changed = self._row(1000, emitted_at="2026-08-25T04:00:00Z")
        for closed in ("2026-08.jsonl", "2026-08-01-to-12.jsonl"):
            top = "2026-08.part-0001.jsonl" if closed == "2026-08.jsonl" else "2026-08.jsonl"
            repo = self._repo({closed: [original], top: [self._row(1001)]})
            with self.subTest(closed=closed):
                with self.assertRaisesRegex(
                    publisher.PublishRefused,
                    re.escape(f"{self.DIRECTORY}/{closed}: event_id") + ".*different body",
                ):
                    self._plan(repo, [changed, self._row(1002)])

    def test_a_row_already_in_a_closed_part_is_not_appended_again(self):
        self._ceiling(1024 * 1024)
        old, top, new = self._row(1000), self._row(1001), self._row(1002)
        for closed in ("2026-08.jsonl", "2026-08-01-to-12.jsonl"):
            name = "2026-08.part-0001.jsonl" if closed == "2026-08.jsonl" else "2026-08.jsonl"
            repo = self._repo({closed: [old], name: [top]})
            with self.subTest(closed=closed):
                self.assertEqual(self._plan(repo, [old, new]), [
                    (f"{self.DIRECTORY}/{name}", self._line(top),
                     self._line(top) + self._line(new)),
                ])

    def test_two_bodies_for_one_published_id_in_a_batch_are_refused(self):
        self._ceiling(1024 * 1024)
        published = self._row(1000)
        other = self._row(1000, emitted_at="2026-08-25T04:00:00Z")
        repo = self._repo({
            "2026-08.jsonl": [published],
            "2026-08.part-0001.jsonl": [self._row(1001)],
        })
        # Either order: the published body must not excuse the other one.
        for batch in ([other, published], [published, other]):
            with self.subTest(first=batch[0]["emitted_at"]):
                with self.assertRaisesRegex(publisher.PublishRefused, "different body"):
                    self._plan(repo, batch)

    def test_a_truncated_closed_part_is_refused(self):
        self._ceiling(1024 * 1024)
        repo = self._repo({
            "2026-08.jsonl": self._line(self._row(1000)).rstrip("\n"),
            "2026-08.part-0001.jsonl": [self._row(1001)],
        })
        with self.assertRaisesRegex(publisher.PublishRefused, "2026-08.jsonl: final line is truncated"):
            self._plan(repo, [self._row(1002)])

    def test_a_late_sorting_hand_split_is_respected_on_rollover(self):
        # A hand split is a month file whose name is not a canonical part.
        # `2026-08.part-1.jsonl` sorts after parts 0001 to 0999 and before part
        # 1000; `2026-08.tail.jsonl` sorts after every canonical name. A row
        # published after B must come after B in path order, so it may go
        # neither to part 0 nor to part 0001. Rows are read in raw path order
        # here: the canonical union orders rows by their fields and would hide
        # a misplaced row.
        width = self._width()
        a, b, c = self._row(1000), self._row(1001), self._row(1002)
        for hand_split in ("2026-08.part-1.jsonl", "2026-08.tail.jsonl"):
            # Part 0 full (C would roll to part 0001) and part 0 with room (C
            # would be appended to part 0): both put C before B.
            for ceiling in (width, 1024 * 1024):
                self._ceiling(ceiling)
                seed = {"2026-08.jsonl": [a], hand_split: [b]}
                repo = self._repo(seed)
                with self.subTest(hand_split=hand_split, ceiling=ceiling):
                    if hand_split == "2026-08.tail.jsonl":
                        # No canonical part name sorts after `.tail`, and a
                        # hand split is never appended to, so C has no place.
                        with self.assertRaisesRegex(
                            publisher.PublishRefused,
                            re.escape(f"{self.DIRECTORY}/{hand_split}: no series part name sorts after"),
                        ):
                            self._plan(repo, [c])
                        continue
                    planned = self._plan(repo, [c])
                    files = {f"{self.DIRECTORY}/{name}": "".join(map(self._line, rows))
                             for name, rows in seed.items()}
                    for name, published, merged in planned:
                        self.assertEqual(published, files.get(name, ""), name)
                        files[name] = merged
                    stream = [json.loads(line)["event_id"]
                              for name in sorted(files) for line in files[name].splitlines()]
                    self.assertEqual(stream, [a["event_id"], b["event_id"], c["event_id"]])
                    self.assertEqual(planned, [(self._path(1000), "", self._line(c))])

    def test_a_retry_with_nothing_new_is_not_refused_by_a_late_sorting_hand_split(self):
        # Every row of this retry is published already, one of them in a hand
        # split that sorts after every canonical name. Nothing is appended, so
        # nothing needs a place after the hand split.
        self._ceiling(1024 * 1024)
        a, b = self._row(1000), self._row(1001)
        repo = self._repo({"2026-08.jsonl": [a], "2026-08.tail.jsonl": [b]})

        self.assertEqual(self._plan(repo, [a, b]), [
            (self._path(0), self._line(a), self._line(a)),
        ])

    def test_rows_keep_their_supplied_order_across_a_part_boundary(self):
        width = self._width()
        self._ceiling(width)
        later, earlier = self._row(1001), self._row(1000)
        # The supplied order is publication order. JSON text order is the
        # opposite here: the event ids differ only in their last digit.
        self.assertGreater(self._line(later), self._line(earlier))
        repo = self._repo({})

        self.assertEqual(self._plan(repo, [later, earlier]), [
            (self._path(0), "", self._line(later)),
            (self._path(1), "", self._line(earlier)),
        ])

    def test_a_full_last_part_refuses_rather_than_wrapping(self):
        width = self._width()
        self._ceiling(width)
        repo = self._repo({"2026-08.part-9999.jsonl": [self._row(1000)]})

        with self.assertRaisesRegex(
            publisher.PublishRefused,
            re.escape(f"shard part must be an integer from 0 to {series.SHARD_PART_LIMIT}, got 10000")
            + ".*spool retained",
        ):
            self._plan(repo, [self._row(1001)])

    def test_the_production_ceiling_stays_well_under_githubs_file_limit(self):
        # GitHub refuses a pushed file over 104,857,600 bytes and warns over
        # 52,428,800. Every other test here patches the ceiling, so only this
        # one reads the production value.
        ceiling = publisher.SERIES_SHARD_CEILING_BYTES
        self.assertIs(type(ceiling), int)
        self.assertGreater(ceiling, 0)
        self.assertLessEqual(ceiling, 104_857_600 // 2)

    def _published_fixture(self):
        bare = self.root / "origin.git"
        producer = self.root / "producer"
        clone = self.root / "publisher"
        reader = self.root / "reader"
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(bare)], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", str(producer)], check=True)
        (producer / "README").write_text("series\n")
        self._git(producer, "add", "README")
        self._git(producer, "-c", "user.name=t", "-c", "user.email=t@invalid", "commit", "-q", "-m", "seed")
        self._git(producer, "remote", "add", "origin", str(bare))
        self._git(producer, "push", "-q", "origin", "main")
        subprocess.run(["git", "clone", "-q", str(bare), str(clone)], check=True)
        subprocess.run(["git", "clone", "-q", str(bare), str(reader)], check=True)
        spool = series.spool_dir(producer)
        spool.mkdir(parents=True)
        return producer, clone, reader, spool

    def test_publish_writes_every_part_and_readers_see_each_row_once(self):
        width = self._width()
        self._ceiling(2 * width)
        producer, clone, reader, spool = self._published_fixture()
        rows = [self._row(index) for index in range(1000, 1005)]
        series.append_rows(spool / "campaign-1.jsonl", rows)

        report = publisher.publish(clone, spool, source_root=producer, store="series")

        self.assertEqual(report["published"], 5)
        self.assertEqual(report["shards"], [self._path(0), self._path(1), self._path(2)])
        self.assertEqual(
            publisher._source_root_event_ids(clone, store="series"),
            {event["event_id"] for event in rows},
        )
        self.assertEqual(
            [event["event_id"] for event in series.published_rows(reader)],
            [event["event_id"] for event in rows],
        )
        self._git(reader, "fetch", "-q", "origin", "main")
        for part, expected in enumerate((rows[0:2], rows[2:4], rows[4:5])):
            self.assertEqual(
                self._git(reader, "show", f"origin/main:{self._path(part)}") + "\n",
                "".join(map(self._line, expected)),
            )

        # The same batch again is a no-op, though its rows are now spread over
        # three parts.
        series.append_rows(spool / "campaign-1-retry.jsonl", rows)
        again = publisher.publish(clone, spool, source_root=producer, store="series")
        self.assertEqual(again["shards"], [])
        self.assertEqual(again["note"], "all events already published")

    def test_publish_refuses_a_planned_part_over_the_ceiling(self):
        width = self._width()
        self._ceiling(width)
        producer, clone, _, spool = self._published_fixture()
        rows = [self._row(1000), self._row(1001)]
        series.append_rows(spool / "campaign-1.jsonl", rows)
        oversized = [(self._path(0), "", "".join(map(self._line, rows)))]

        with mock.patch.object(publisher, "_plan_series_month", return_value=oversized):
            with self.assertRaisesRegex(publisher.PublishRefused, "series shard ceiling"):
                publisher.publish(clone, spool, source_root=producer, store="series")

        self.assertTrue((spool / "campaign-1.jsonl").exists())
        self.assertFalse((clone / "series").exists())

    def test_publish_refuses_a_planned_later_part_over_the_ceiling(self):
        width = self._width()
        self._ceiling(width)
        producer, clone, _, spool = self._published_fixture()
        rows = [self._row(1000), self._row(1001)]
        series.append_rows(spool / "campaign-1.jsonl", rows)
        head = self._git(clone, "rev-parse", "HEAD")
        oversized = [(self._path(1), "", "".join(map(self._line, rows)))]

        with mock.patch.object(publisher, "_plan_series_month", return_value=oversized):
            with self.assertRaisesRegex(
                publisher.PublishRefused,
                re.escape(f"{self._path(1)}: part would be") + ".*series shard ceiling",
            ):
                publisher.publish(clone, spool, source_root=producer, store="series")

        self.assertTrue((spool / "campaign-1.jsonl").exists())
        self.assertEqual(self._git(clone, "rev-parse", "HEAD"), head)
        self.assertEqual(self._git(clone, "status", "--porcelain", "--untracked-files=all"), "")

    def _blob(self, repo, ref, path):
        # Exact bytes: `_git` strips the final newline.
        return subprocess.run(
            ["git", "-C", str(repo), "show", f"{ref}:{path}"],
            capture_output=True, check=True,
        ).stdout

    def test_publish_leaves_a_part_already_over_the_ceiling_byte_for_byte(self):
        width = self._width()
        producer, clone, reader, spool = self._published_fixture()
        legacy = [self._row(index) for index in range(1000, 1003)]
        self._ceiling(10 * width)
        series.append_rows(spool / "campaign-1.jsonl", legacy)
        first = publisher.publish(clone, spool, source_root=producer, store="series")
        self.assertEqual(first["shards"], [self._path(0)])
        self._git(reader, "fetch", "-q", "origin", "main")
        part_zero = self._blob(reader, "origin/main", self._path(0))
        self.assertEqual(part_zero, "".join(map(self._line, legacy)).encode())

        # Three rows are now over a two-row ceiling, as a month shard published
        # before parts existed can be.
        self._ceiling(2 * width)
        fourth = self._row(1003)
        series.append_rows(spool / "campaign-2.jsonl", [fourth])
        report = publisher.publish(clone, spool, source_root=producer, store="series")

        self.assertEqual(report["published"], 1)
        self.assertEqual(report["shards"], [self._path(1)])
        self._git(reader, "fetch", "-q", "origin", "main")
        self.assertEqual(self._blob(reader, "origin/main", self._path(0)), part_zero)
        self.assertEqual(self._blob(reader, "origin/main", self._path(1)), self._line(fourth).encode())

    def test_a_later_month_refusal_leaves_the_writer_clean(self):
        # The publisher used to write each month's file as soon as it was
        # planned, so a refusal in September left August's file written but
        # uncommitted, and the next writer run stopped at its clean-checkout
        # check. Planning every month before writing anything keeps it clean.
        self._ceiling(1024 * 1024)
        producer, clone, _, spool = self._published_fixture()
        september = "2026-09-01T03:00:00Z"
        original = self._row(1000, emitted_at=september)
        hand_split = "2026-09-01-to-12.jsonl"
        for name, rows in {hand_split: [original],
                           "2026-09.jsonl": [self._row(1001, emitted_at=september)]}.items():
            path = producer / self.DIRECTORY / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("".join(map(self._line, rows)))
        self._git(producer, "add", "series")
        self._git(producer, "-c", "user.name=t", "-c", "user.email=t@invalid",
                  "commit", "-q", "-m", "september, split by hand")
        self._git(producer, "push", "-q", "origin", "main")
        self._git(clone, "fetch", "-q", "origin", "main")
        self._git(clone, "merge", "-q", "--ff-only", "origin/main")
        head = self._git(clone, "rev-parse", "HEAD")
        # A fresh August row, and X again with a changed body.
        changed = self._row(1000, emitted_at="2026-09-01T04:00:00Z")
        series.append_rows(spool / "campaign-1.jsonl", [self._row(1002), changed])

        with self.assertRaisesRegex(
            publisher.PublishRefused,
            re.escape(f"{self.DIRECTORY}/{hand_split}: event_id") + ".*different body",
        ):
            publisher.publish(clone, spool, source_root=producer, store="series")

        self.assertTrue((spool / "campaign-1.jsonl").exists())
        self.assertEqual(self._git(clone, "rev-parse", "HEAD"), head)
        self.assertEqual(self._git(clone, "status", "--porcelain", "--untracked-files=all"), "")


class ScorecardProjectionSnapshot(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "parent"
        self.enterContext(mock.patch.object(series, "ledger_root", return_value=self.repo))
        subprocess.run(
            ["git", "init", "-q", "-b", "main", str(self.repo)], check=True
        )
        shard = self.repo / "series/hermit/devbig014/2026-08.jsonl"
        shard.parent.mkdir(parents=True)
        self.published = row()
        shard.write_text(json.dumps(self.published, sort_keys=True) + "\n")
        self._git("add", "series")
        self._git(
            "-c", "user.name=t", "-c", "user.email=t@invalid",
            "commit", "-q", "-m", "published series",
        )
        self.commit = self._git("rev-parse", "HEAD")
        self.tree = self._git("rev-parse", f"{self.commit}:series")
        self._git("update-ref", "refs/remotes/origin/main", self.commit)

    def _git(self, *arguments):
        return subprocess.run(
            ["git", "-C", str(self.repo), *arguments],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def _write_local(self, relative, item):
        target = series.spool_dir(self.repo) / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(item, sort_keys=True) + "\n")
        return target

    def _bounded_child(self, operation):
        # Only local file and metadata operations run in these children. The
        # deadline belongs to this parent, including old-source FIFO controls.
        program = (
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "sys.path.insert(0, sys.argv[1])\n"
            "import series\n"
            "parent = Path(sys.argv[2])\n"
            "try:\n"
            "    exec(sys.argv[3])\n"
            "except series.SeriesError as error:\n"
            "    print(json.dumps({'refused': str(error)}))\n"
            "else:\n"
            "    print(json.dumps({'accepted': True}))\n"
        )
        child = subprocess.Popen(
            [sys.executable, "-c", program, str(Path(series.__file__).parent),
             str(self.repo), operation],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        timed_out = False
        try:
            stdout, stderr = child.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            timed_out = True
            child.kill()
            stdout, stderr = child.communicate(timeout=3)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=3)
        print(json.dumps({
            "control": self.id(), "pid": child.pid, "status": child.returncode,
            "timed_out": timed_out, "reaped": child.poll() is not None,
            "stdout": stdout, "stderr": stderr,
        }, sort_keys=True))
        self.assertFalse(timed_out, "snapshot control exceeded its external child deadline")
        self.assertEqual(child.returncode, 0, stderr)
        return json.loads(stdout)

    def test_fifo_snapshot_reader_refuses_without_waiting_for_a_writer(self):
        os.mkfifo(self.repo / "snapshot.json")
        result = self._bounded_child('''
series.read_scorecard_projection_snapshot(
    parent / "snapshot.json", expected_sha256="0" * 64,
)
''')
        self.assertIn("not a regular file", result.get("refused", ""))

    def test_regular_source_replaced_by_fifo_refuses_during_capture(self):
        source = series.spool_dir(self.repo) / "live.jsonl"
        source.parent.mkdir(parents=True)
        source.write_bytes(b"")
        result = self._bounded_child('''
spool = series.spool_dir(parent)
with series.series_spool_transition_lock(spool, exclusive=False) as lock:
    measured = series._snapshot_source_generation(lock)
    assert len(measured[2]) == 1
    assert measured[2][0][:2] == ("live", "live.jsonl")
source = spool / "live.jsonl"
os.rename(source, source.with_suffix(".old"))
os.mkfifo(source)
with series._local_snapshot_capture(parent):
    pass
''')
        self.assertIn("not a regular file", result.get("refused", ""))
        self.assertTrue(source.with_suffix(".old").is_file())

    def test_snapshot_reader_rejects_equal_and_conflicting_duplicate_fields(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        self._write_local("live.jsonl", local)
        output = Path(self.temporary.name) / "snapshot.json"
        with mock.patch.object(series, "refresh_origin_main"):
            report = series.write_scorecard_projection_snapshot(self.repo, output)
        original = output.read_bytes()
        valid = series.read_scorecard_projection_snapshot(
            output, expected_sha256=report["sha256"]
        )
        self.assertEqual(valid["rows_read"], 2)
        fields = (
            ("schema", valid["schema"], "other-schema"),
            ("commit", valid["source"]["commit"], "c" * 40),
            ("inode", valid["source"]["local_sources"][0]["inode"], 0),
            ("event_id", self.published["event_id"], "01J000000000000000000000AC"),
            ("tree", self.published["series"]["tree"], "b" * 40),
        )
        for key, value, conflicting in fields:
            for first in (value, conflicting):
                with self.subTest(field=key, first=first):
                    field = json.dumps(key) + ":" + json.dumps(value)
                    duplicate = json.dumps(key) + ":" + json.dumps(first) + "," + field
                    self.assertIn(field.encode(), original)
                    payload = original.replace(field.encode(), duplicate.encode(), 1)
                    output.write_bytes(payload)
                    with self.assertRaisesRegex(series.SeriesError, "duplicate JSON field"):
                        series.read_scorecard_projection_snapshot(
                            output,
                            expected_sha256=hashlib.sha256(payload).hexdigest(),
                        )

    def test_cli_failure_reports_retained_published_and_staging_bytes(self):
        output = Path(self.temporary.name) / "snapshot.json"
        stdout = io.StringIO()
        stderr = io.StringIO()
        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_open_anonymous_snapshot_stage",
            side_effect=OSError(errno.EOPNOTSUPP, "forced named stage"),
        ), mock.patch.object(
            series,
            "_rename_noreplace",
            side_effect=OSError(errno.ENOSYS, "forced hardlink fallback"),
        ), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = series._cmd_snapshot([
                "--parent", str(self.repo), "--output", str(output),
            ])
        self.assertEqual(status, 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertIn("snapshot failed", stderr.getvalue())
        self.assertIn("output state is not asserted", stderr.getvalue())
        self.assertIn("refusing success with named staging residue", stderr.getvalue())
        self.assertNotIn("no new snapshot was published", stderr.getvalue())
        stages = list(output.parent.glob(f".{output.name}.staging-*"))
        self.assertTrue(output.is_file())
        self.assertEqual(len(stages), 1)
        self.assertEqual(output.read_bytes(), stages[0].read_bytes())
        self.assertEqual(output.stat().st_ino, stages[0].stat().st_ino)

    def _counting_scandir(self, visited):
        original = os.scandir

        class CountingIterator:
            def __init__(self, directory_fd):
                self.inner = original(directory_fd)

            def __iter__(self):
                return self

            def __next__(self):
                entry = next(self.inner)
                visited.append(entry.name)
                return entry

            def close(self):
                self.inner.close()

        return CountingIterator

    def test_snapshot_binds_published_tree_and_exact_local_sources(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        self._write_local("live.jsonl", local)
        self._write_local("published/duplicate.jsonl", self.published)
        output = Path(self.temporary.name) / "artifacts/run-1/series-snapshot.json"

        with mock.patch.object(series, "refresh_origin_main"):
            report = series.write_scorecard_projection_snapshot(self.repo, output)

        snapshot = series.read_scorecard_projection_snapshot(
            output, expected_sha256=report["sha256"]
        )
        self.assertEqual(series.SCORECARD_SNAPSHOT_SCHEMA, snapshot["schema"])
        self.assertEqual(self.commit, snapshot["source"]["commit"])
        self.assertEqual(self.tree, snapshot["source"]["tree"])
        self.assertEqual(1, snapshot["source"]["published_rows"])
        self.assertEqual(2, snapshot["rows_read"])
        self.assertEqual(
            [self.published["event_id"], local["event_id"]],
            [item["event_id"] for item in snapshot["rows"]],
        )
        self.assertEqual(
            ["live.jsonl", "published/duplicate.jsonl"],
            [item["path"] for item in snapshot["source"]["local_sources"]],
        )
        for item in snapshot["source"]["local_sources"]:
            self.assertGreaterEqual(item["device"], 0)
            self.assertGreater(item["inode"], 0)
        self.assertEqual(report["rows_read"], snapshot["rows_read"])
        self.assertEqual(report["source_commit"], self.commit)
        self.assertEqual(report["source_tree"], self.tree)

    def test_existing_identical_snapshot_is_idempotent_but_conflict_is_refused(self):
        output = Path(self.temporary.name) / "snapshot.json"
        with mock.patch.object(series, "refresh_origin_main"):
            first = series.write_scorecard_projection_snapshot(self.repo, output)
            second = series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertEqual(first, second)
        original = output.read_bytes()
        output.write_text("foreign\n")
        with mock.patch.object(series, "refresh_origin_main"):
            with self.assertRaisesRegex(series.SeriesError, "different content"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertEqual(b"foreign\n", output.read_bytes())
        self.assertNotEqual(original, output.read_bytes())

    def test_failed_post_publish_readback_leaves_replacement_untouched(self):
        output = Path(self.temporary.name) / "snapshot.json"
        foreign = output.with_name("foreign.json")
        foreign.write_text("foreign\n")
        original_finish = series._finish_snapshot_publication

        def replace_then_fail(directory_fd, published, digest):
            os.replace(foreign, published)
            return original_finish(directory_fd, published, digest)

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_finish_snapshot_publication", side_effect=replace_then_fail,
        ):
            with self.assertRaisesRegex(series.SeriesError, "digest mismatch"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertEqual(b"foreign\n", output.read_bytes())

    def test_network_and_output_readback_do_not_hold_writer_lock(self):
        output = Path(self.temporary.name) / "snapshot.json"
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        self._write_local("live.jsonl", local)
        original_finish = series._finish_snapshot_publication
        refreshes = 0

        def require_unlocked():
            with series.series_spool_transition_lock(
                series.spool_dir(self.repo),
                exclusive=True,
                timeout_seconds=0,
            ):
                pass

        def refresh_without_lock(_parent, _repository):
            nonlocal refreshes
            refreshes += 1
            require_unlocked()

        def finish_without_lock(directory_fd, published, digest):
            require_unlocked()
            return original_finish(directory_fd, published, digest)

        with mock.patch.object(
            series, "refresh_origin_main", side_effect=refresh_without_lock,
        ), mock.patch.object(
            series, "_finish_snapshot_publication", side_effect=finish_without_lock,
        ):
            series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertEqual(refreshes, 2)

    def test_named_stage_replacement_refuses_without_deleting_foreign_file(self):
        output = Path(self.temporary.name) / "snapshot.json"
        original_rename = series._rename_noreplace

        def replace_stage_then_rename(directory_fd, stage_name, output_name):
            foreign = output.parent / "replacement"
            foreign.write_text("foreign staging replacement\n")
            os.replace(foreign, output.parent / stage_name)
            original_rename(directory_fd, stage_name, output_name)

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_open_anonymous_snapshot_stage",
            side_effect=OSError(errno.EOPNOTSUPP, "forced named stage"),
        ), mock.patch.object(
            series, "_rename_noreplace", side_effect=replace_stage_then_rename,
        ):
            with self.assertRaisesRegex(series.SeriesError, "digest mismatch"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertEqual(b"foreign staging replacement\n", output.read_bytes())

    def test_hardlink_fallback_reports_non_success_and_retains_staging(self):
        output = Path(self.temporary.name) / "snapshot.json"
        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_open_anonymous_snapshot_stage",
            side_effect=OSError(errno.EOPNOTSUPP, "forced named stage"),
        ), mock.patch.object(
            series,
            "_rename_noreplace",
            side_effect=OSError(errno.ENOSYS, "forced hardlink fallback"),
        ):
            with self.assertRaisesRegex(
                series.SeriesError, "refusing success with named staging residue"
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)

        stages = list(output.parent.glob(f".{output.name}.staging-*"))
        self.assertTrue(output.is_file())
        self.assertEqual(len(stages), 1)
        self.assertEqual(output.read_bytes(), stages[0].read_bytes())
        self.assertEqual(output.stat().st_ino, stages[0].stat().st_ino)

    def test_identical_concurrent_output_does_not_hide_named_staging_residue(self):
        output = Path(self.temporary.name) / "snapshot.json"

        def link_then_report_exists(directory_fd, stage_name, output_name):
            os.link(
                stage_name,
                output_name,
                src_dir_fd=directory_fd,
                dst_dir_fd=directory_fd,
                follow_symlinks=False,
            )
            raise FileExistsError(errno.EEXIST, "simulated concurrent output")

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_open_anonymous_snapshot_stage",
            side_effect=OSError(errno.EOPNOTSUPP, "forced named stage"),
        ), mock.patch.object(
            series, "_rename_noreplace", side_effect=link_then_report_exists,
        ):
            with self.assertRaisesRegex(
                series.SeriesError, "refusing success because the staging pathname"
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)

        stages = list(output.parent.glob(f".{output.name}.staging-*"))
        self.assertTrue(output.is_file())
        self.assertEqual(len(stages), 1)
        self.assertEqual(output.stat().st_ino, stages[0].stat().st_ino)

    def test_consumer_refuses_tampered_bytes_and_row_digest(self):
        output = Path(self.temporary.name) / "snapshot.json"
        with mock.patch.object(series, "refresh_origin_main"):
            report = series.write_scorecard_projection_snapshot(self.repo, output)
        snapshot = json.loads(output.read_text())
        snapshot["rows_sha256"] = "0" * 64
        output.write_text(
            json.dumps(snapshot, sort_keys=True, separators=(",", ":")) + "\n"
        )
        tampered_digest = hashlib.sha256(output.read_bytes()).hexdigest()
        with self.assertRaisesRegex(series.SeriesError, "row digest"):
            series.read_scorecard_projection_snapshot(
                output, expected_sha256=tampered_digest
            )
        with self.assertRaisesRegex(series.SeriesError, "digest mismatch"):
            series.read_scorecard_projection_snapshot(
                output, expected_sha256=report["sha256"]
            )

    def test_conflicting_event_identity_refuses_without_output(self):
        conflict = row(series={"outcome": "diverged"})
        self._write_local("conflict.jsonl", conflict)
        output = Path(self.temporary.name) / "snapshot.json"
        with mock.patch.object(series, "refresh_origin_main"):
            with self.assertRaisesRegex(series.SeriesError, "different bodies"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())

    def test_local_source_set_change_refuses_without_output(self):
        first = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        second = row(
            event_id="01J000000000000000000000AC",
            run_id="campaign-3",
            series={"run_index": 2},
        )
        self._write_local("first.jsonl", first)
        output = Path(self.temporary.name) / "snapshot.json"
        original = series._open_snapshot_source

        def add_source_after_read(directory_fd, name, *, kind, relative):
            captured = original(
                directory_fd, name, kind=kind, relative=relative,
            )
            self._write_local("second.jsonl", second)
            return captured

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_open_snapshot_source", side_effect=add_source_after_read
        ):
            with self.assertRaisesRegex(series.SeriesError, "spool changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())

    def test_local_source_symlink_is_refused_without_output(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        target = self._write_local("real", local)
        linked = target.with_name("linked.jsonl")
        linked.symlink_to(target.name)
        output = Path(self.temporary.name) / "snapshot.json"

        with mock.patch.object(series, "refresh_origin_main"):
            with self.assertRaisesRegex(
                series.SeriesError, "series snapshot source is not a regular file"
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())

    def test_local_source_symlink_replacement_is_refused_without_output(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        target = self._write_local("live.jsonl", local)
        output = Path(self.temporary.name) / "snapshot.json"
        original = series._open_snapshot_source

        def replace_with_symlink(directory_fd, name, *, kind, relative):
            captured = original(
                directory_fd, name, kind=kind, relative=relative,
            )
            saved = target.with_name("saved-source")
            target.rename(saved)
            target.symlink_to(saved.name)
            return captured

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_open_snapshot_source", side_effect=replace_with_symlink,
        ):
            with self.assertRaisesRegex(series.SeriesError, "changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())

    def test_local_source_inode_replacement_is_refused_without_output(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        target = self._write_local("live.jsonl", local)
        output = Path(self.temporary.name) / "snapshot.json"
        original = series._open_snapshot_source

        def replace_after_open(directory_fd, name, *, kind, relative):
            captured = original(
                directory_fd, name, kind=kind, relative=relative,
            )
            replacement = target.with_name("replacement")
            replacement.write_bytes(target.read_bytes())
            os.replace(replacement, target)
            return captured

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_open_snapshot_source", side_effect=replace_after_open,
        ):
            with self.assertRaisesRegex(series.SeriesError, "changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())

    def test_unavailable_published_source_never_falls_back_to_tracked_output(self):
        import contextlib
        import io

        output = Path(self.temporary.name) / "snapshot.json"
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(
            series, "refresh_origin_main", side_effect=series.SeriesError("fetch failed")
        ), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = series.main([
                "snapshot", "--parent", str(self.repo), "--output", str(output)
            ])
        self.assertEqual(1, rc)
        self.assertFalse(output.exists())
        self.assertEqual("", stdout.getvalue())
        self.assertIn("snapshot failed: fetch failed", stderr.getvalue())
        self.assertIn("output state is not asserted", stderr.getvalue())
        self.assertIn(str(output), stderr.getvalue())
        self.assertIn("correct the named failure", stderr.getvalue())
        self.assertNotIn("no new snapshot was published", stderr.getvalue())

    def test_snapshot_cli_help_forms_name_every_flag(self):
        import contextlib
        import io

        for flag in ("--help", "-h"):
            with self.subTest(flag=flag):
                stdout, stderr = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
                    stderr
                ):
                    rc = series.main(["snapshot", flag])
                self.assertEqual(0, rc)
                self.assertEqual("", stderr.getvalue())
                self.assertIn("series.py snapshot", stdout.getvalue())
                self.assertIn("--parent", stdout.getvalue())
                self.assertIn("--output", stdout.getvalue())
                self.assertIn("-h, --help", stdout.getvalue())

    def test_snapshot_cli_happy_path_reports_published_identity(self):
        import contextlib
        import io

        output = Path(self.temporary.name) / "snapshot.json"
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(series, "refresh_origin_main"), contextlib.redirect_stdout(
            stdout
        ), contextlib.redirect_stderr(stderr):
            rc = series.main([
                "snapshot", "--parent", str(self.repo), "--output", str(output),
            ])
        self.assertEqual(0, rc)
        self.assertEqual("", stderr.getvalue())
        report = json.loads(stdout.getvalue())
        self.assertEqual("available", report["state"])
        self.assertEqual(str(output.absolute()), report["path"])
        self.assertTrue(output.is_file())

    def test_snapshot_cli_usage_error_redirects_to_help(self):
        import contextlib
        import io

        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
            stderr
        ):
            rc = series.main(["snapshot", "--parent", str(self.repo)])
        self.assertEqual(2, rc)
        self.assertEqual("", stdout.getvalue())
        self.assertIn("snapshot requires --output", stderr.getvalue())
        self.assertIn("series.py snapshot --help", stderr.getvalue())
        self.assertIn("--parent", stderr.getvalue())
        self.assertIn("--output", stderr.getvalue())

    def test_snapshot_cli_late_failure_reports_state_neutrally(self):
        import contextlib
        import io

        output = Path(self.temporary.name) / "snapshot.json"
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_open_anonymous_snapshot_stage",
            side_effect=OSError(errno.EOPNOTSUPP, "forced named stage"),
        ), mock.patch.object(
            series,
            "_rename_noreplace",
            side_effect=OSError(errno.ENOSYS, "forced hardlink fallback"),
        ), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = series.main([
                "snapshot", "--parent", str(self.repo), "--output", str(output),
            ])

        self.assertEqual(1, rc)
        self.assertEqual("", stdout.getvalue())
        self.assertTrue(output.is_file())
        stages = list(output.parent.glob(f".{output.name}.staging-*"))
        self.assertEqual(1, len(stages))
        self.assertIn("snapshot failed", stderr.getvalue())
        self.assertIn("output state is not asserted", stderr.getvalue())
        self.assertIn(str(output), stderr.getvalue())
        self.assertIn(
            str(output.parent / f".{output.name}.staging-*"), stderr.getvalue()
        )
        self.assertIn("Preserve any file found", stderr.getvalue())

    def test_lock_timeout_is_bounded_and_names_the_lock(self):
        spool = series.spool_dir(self.repo)
        spool.mkdir(parents=True, exist_ok=True)
        with series.series_spool_transition_lock(spool, exclusive=True):
            with self.assertRaisesRegex(
                series.SeriesError,
                rf"timed out after 0\.050s.*{re.escape(str(spool / '.append.lock'))}",
            ):
                with series.series_spool_transition_lock(
                    spool,
                    exclusive=False,
                    timeout_seconds=0.05,
                    poll_seconds=0.005,
                ):
                    self.fail("contended lock unexpectedly admitted")

    def test_source_identity_count_limit_refuses_and_names_the_lock(self):
        first = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        second = row(
            event_id="01J000000000000000000000AC",
            run_id="campaign-3",
            series={"run_index": 2},
        )
        self._write_local("first.jsonl", first)
        self._write_local("second.jsonl", second)
        output = Path(self.temporary.name) / "snapshot.json"
        lock_path = series.spool_dir(self.repo) / ".append.lock"

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "SCORECARD_SNAPSHOT_MAX_DIRECTORY_ENTRIES", 1,
        ):
            with self.assertRaisesRegex(
                series.SeriesError,
                rf"2 total directory entries.*maximum of 1.*{re.escape(str(lock_path))}",
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)
        self.assertFalse(output.exists())


    def _assert_total_entry_limit_stops_early(self, suffix):
        spool = series.spool_dir(self.repo)
        spool.mkdir(parents=True, exist_ok=True)
        for index in range(20):
            (spool / f"entry-{index:02d}{suffix}").write_text("not read\n")
        lock_path = spool / ".append.lock"
        visited = []

        with series.series_spool_transition_lock(spool, exclusive=False) as lock:
            with mock.patch.object(
                series, "SCORECARD_SNAPSHOT_MAX_DIRECTORY_ENTRIES", 4,
            ), mock.patch.object(
                series.os, "scandir", side_effect=self._counting_scandir(visited),
            ):
                with self.assertRaisesRegex(
                    series.SeriesError,
                    rf"5 total directory entries.*maximum of 4.*"
                    rf"{re.escape(str(lock_path))}",
                ):
                    series._snapshot_source_generation(lock)

        self.assertEqual(5, len(visited), "entry cap was checked after a full scan")

    def test_source_identity_total_entry_limit_stops_jsonl_scan_early(self):
        self._assert_total_entry_limit_stops_early(".jsonl")

    def test_source_identity_total_entry_limit_counts_non_jsonl_entries(self):
        self._assert_total_entry_limit_stops_early(".tmp")

    def test_source_identity_generation_enumerates_each_directory_once(self):
        self._write_local("live.jsonl", row())
        self._write_local("published/retained.jsonl", row())
        spool = series.spool_dir(self.repo)
        original_scandir = os.scandir

        with series.series_spool_transition_lock(spool, exclusive=False) as lock:
            with mock.patch.object(
                series.os, "scandir", wraps=original_scandir,
            ) as scandir:
                series._snapshot_source_generation(lock)

        self.assertEqual(
            2,
            scandir.call_count,
            "source generation enumerated a directory more than once",
        )

    def test_source_identity_deadline_refuses_and_names_the_lock(self):
        spool = series.spool_dir(self.repo)
        spool.mkdir(parents=True, exist_ok=True)
        with series.series_spool_transition_lock(spool, exclusive=False) as lock:
            with mock.patch.object(
                series.time, "monotonic", side_effect=[10.0, 12.0]
            ):
                with self.assertRaisesRegex(
                    series.SeriesError,
                    rf"identity phase exceeded 1\.000s.*{re.escape(str(spool / '.append.lock'))}",
                ):
                    series._snapshot_source_generation(lock)

    def test_source_identity_deadline_stops_slow_enumeration_early(self):
        spool = series.spool_dir(self.repo)
        spool.mkdir(parents=True, exist_ok=True)
        for index in range(20):
            (spool / f"entry-{index:02d}.tmp").write_text("not read\n")
        visited = []
        now = 0.0

        def advancing_clock():
            nonlocal now
            now += 0.26
            return now

        with series.series_spool_transition_lock(spool, exclusive=False) as lock:
            with mock.patch.object(
                series.time, "monotonic", side_effect=advancing_clock,
            ), mock.patch.object(
                series.os, "scandir", side_effect=self._counting_scandir(visited),
            ):
                with self.assertRaisesRegex(
                    series.SeriesError,
                    rf"identity phase exceeded 1\.000s.*{re.escape(str(spool / '.append.lock'))}",
                ):
                    series._snapshot_source_generation(lock)

        self.assertGreater(len(visited), 0)
        self.assertLess(len(visited), 21, "deadline was checked after a full scan")

    def test_slow_row_validation_does_not_block_append(self):
        original = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        appended = row(
            event_id="01J000000000000000000000AC",
            run_id="campaign-3",
            series={"run_index": 2},
        )
        self._write_local("first.jsonl", original)
        append_path = series.spool_dir(self.repo) / "second.jsonl"
        output = Path(self.temporary.name) / "snapshot.json"
        checker_entered = threading.Event()
        release_checker = threading.Event()
        snapshot_errors = []
        calls = 0
        original_validate = series.validate_read_rows

        def slow_local_validation(rows):
            nonlocal calls
            calls += 1
            if calls == 2:
                checker_entered.set()
                if not release_checker.wait(2):
                    raise AssertionError("test did not release the slow checker")
            return original_validate(rows)

        def run_snapshot():
            try:
                series.write_scorecard_projection_snapshot(self.repo, output)
            except Exception as error:  # pragma: no cover - asserted below
                snapshot_errors.append(error)

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "validate_read_rows", side_effect=slow_local_validation,
        ):
            thread = threading.Thread(target=run_snapshot)
            thread.start()
            try:
                self.assertTrue(checker_entered.wait(2))
                self.assertEqual(
                    series.append_cell_rows(self.repo, append_path, [appended]), 1,
                    "append EX did not complete while row validation was paused",
                )
            finally:
                release_checker.set()
            thread.join(2)

        self.assertFalse(thread.is_alive())
        self.assertEqual(len(snapshot_errors), 1)
        self.assertIsInstance(snapshot_errors[0], series.SeriesError)
        self.assertIn("spool changed", str(snapshot_errors[0]))
        self.assertEqual(series.read_shard(append_path), [appended])
        self.assertFalse(output.exists())

    def test_move_between_pre_generation_and_capture_refuses(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        live = self._write_local("live.jsonl", local)
        retained = live.parent / "published" / live.name
        output = Path(self.temporary.name) / "snapshot.json"
        original_generation = series._snapshot_source_generation
        calls = 0

        def move_after_generation(parent):
            nonlocal calls
            generation = original_generation(parent)
            calls += 1
            if calls == 1:
                retained.parent.mkdir(parents=True, exist_ok=True)
                os.replace(live, retained)
            return generation

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_snapshot_source_generation", side_effect=move_after_generation,
        ):
            with self.assertRaisesRegex(series.SeriesError, "generation changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)

        self.assertFalse(output.exists())
        self.assertFalse(live.exists())
        self.assertEqual(series.read_shard(retained), [local])

    def test_move_between_capture_and_post_generation_refuses(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        live = self._write_local("live.jsonl", local)
        retained = live.parent / "published" / live.name
        output = Path(self.temporary.name) / "snapshot.json"
        original_generation = series._snapshot_source_generation
        calls = 0

        def move_before_second_generation(parent):
            nonlocal calls
            calls += 1
            if calls == 2:
                retained.parent.mkdir(parents=True, exist_ok=True)
                os.replace(live, retained)
            return original_generation(parent)

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series,
            "_snapshot_source_generation",
            side_effect=move_before_second_generation,
        ):
            with self.assertRaisesRegex(series.SeriesError, "generation changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)

        self.assertFalse(output.exists())
        self.assertFalse(live.exists())
        self.assertEqual(series.read_shard(retained), [local])

    def test_move_during_second_refresh_refuses(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        live = self._write_local("live.jsonl", local)
        retained = live.parent / "published" / live.name
        output = Path(self.temporary.name) / "snapshot.json"
        refreshes = 0

        def move_on_second_refresh(_parent, _repository):
            nonlocal refreshes
            refreshes += 1
            if refreshes == 2:
                retained.parent.mkdir(parents=True, exist_ok=True)
                os.replace(live, retained)

        with mock.patch.object(
            series, "refresh_origin_main", side_effect=move_on_second_refresh,
        ):
            with self.assertRaisesRegex(
                series.SeriesError, "generation changed after published-ref"
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)

        self.assertFalse(output.exists())
        self.assertFalse(live.exists())
        self.assertEqual(series.read_shard(retained), [local])

    def test_path_replacement_after_ref_check_refuses_without_deleting_foreign(self):
        local = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        foreign = row(
            event_id="01J000000000000000000000AC",
            run_id="campaign-3",
            series={"run_index": 2},
        )
        live = self._write_local("live.jsonl", local)
        output = Path(self.temporary.name) / "snapshot.json"
        original_origin = series._origin_main_commit
        calls = 0

        def replace_after_second_ref(parent):
            nonlocal calls
            commit = original_origin(parent)
            calls += 1
            if calls == 2:
                replacement = live.with_name("foreign")
                replacement.write_text(json.dumps(foreign, sort_keys=True) + "\n")
                os.replace(replacement, live)
            return commit

        with mock.patch.object(series, "refresh_origin_main"), mock.patch.object(
            series, "_origin_main_commit", side_effect=replace_after_second_ref,
        ):
            with self.assertRaisesRegex(
                series.SeriesError, "generation changed after published-ref"
            ):
                series.write_scorecard_projection_snapshot(self.repo, output)

        self.assertFalse(output.exists())
        self.assertEqual(series.read_shard(live), [foreign])

    def test_published_ref_move_during_capture_refuses(self):
        shard = self.repo / "series/hermit/devbig014/2026-08.jsonl"
        later = row(
            event_id="01J000000000000000000000AB",
            run_id="campaign-2",
            series={"run_index": 1},
        )
        with shard.open("a") as stream:
            stream.write(json.dumps(later, sort_keys=True) + "\n")
        self._git("add", "series")
        self._git(
            "-c", "user.name=t", "-c", "user.email=t@invalid",
            "commit", "-q", "-m", "later published row",
        )
        later_commit = self._git("rev-parse", "HEAD")
        self._git("update-ref", "refs/remotes/origin/main", self.commit)
        output = Path(self.temporary.name) / "snapshot.json"
        refreshes = 0

        def advance_on_second_refresh(_parent, _repository):
            nonlocal refreshes
            refreshes += 1
            if refreshes == 2:
                self._git("update-ref", "refs/remotes/origin/main", later_commit)

        with mock.patch.object(
            series, "refresh_origin_main", side_effect=advance_on_second_refresh,
        ):
            with self.assertRaisesRegex(series.SeriesError, "origin/main changed"):
                series.write_scorecard_projection_snapshot(self.repo, output)

        self.assertFalse(output.exists())


class AppendParityCli(unittest.TestCase):
    """`series.py append-parity` as validate calls it: a real process fed
    hermit's `test-harness parity export` bytes on stdin.

    The input is RUN 1953's export (ci-hub/tests/fixtures/parity-run-1953,
    192 cells) with the typed unavailable_class and operand fields that
    parity-ledger/v1 requires: ledger-sources-typed.jsonl, derived from the
    verbatim ledger-sources.jsonl by the rule in its provenance.json. The
    verbatim export, which predates those fields, is refused.
    https://github.com/rrnewton/hermit/issues/3301
    """

    FIXTURE = Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "parity-run-1953"
    TYPED = "ledger-sources-typed.jsonl"
    RUN_ID = "validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da"
    TREE = "d3a0a4ae3d168565595ae4157b25ab8efbb1861a"

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.parent = Path(self.temporary.name)
        self.spool = series.parity_spool_dir(self.parent)

    def _run(self, data: bytes, *extra, run_id=None, emitted_at="2026-09-28T21:40:00Z",
             dirty="false", parent=None):
        command = [
            sys.executable, str(Path(series.__file__).resolve()), "append-parity",
            "--parent", str(parent or self.parent), "--producer", "validate",
            "--run-id", run_id or self.RUN_ID, "--tree", self.TREE,
            "--emitted-at", emitted_at,
            *(() if dirty is None else ("--source-tree-dirty", dirty)), *extra,
        ]
        return subprocess.run(command, input=data, capture_output=True, timeout=120, check=False)

    def _batches(self):
        return sorted(p.name for p in self.spool.glob("*.jsonl")) if self.spool.is_dir() else []

    def _recorded(self):
        """Every spooled batch as (first event, rows)."""
        parity = series.parity_ledger_module()
        return [parity.read_batch(self.spool / name) for name in self._batches()]

    def _only_refusal(self, data: bytes, error: str, dirty):
        """The spool holds exactly one batch, the refusal of this input for `error`."""
        parity = series.parity_ledger_module()
        ((refusal, rows),) = self._recorded()
        self.assertEqual(parity.REFUSAL_EVENT_TYPE, refusal["event_type"])
        self.assertEqual([], rows)
        self.assertEqual(
            ("validate", self.RUN_ID, self.TREE, dirty, hashlib.sha256(data).hexdigest(), error),
            (refusal["producer"], refusal["run_id"], refusal["hermit_sha"],
             refusal["source_tree_dirty"], refusal["input_sha256"], refusal["error"]))
        return refusal

    def test_round_trip_of_a_real_run_then_an_identical_replay(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        first = self._run(data)
        self.assertEqual(0, first.returncode, first.stderr.decode())
        self.assertIn(
            "parity: appended 192 record(s) (candidate-missing 3, inputs-not-equalized 14, "
            f"diverged 175) for validate run {self.RUN_ID} -> ", first.stdout.decode())
        self.assertIn("published ledger not consulted", first.stderr.decode())
        (batch,) = self._batches()
        parity = series.parity_ledger_module()
        marker, rows = parity.read_batch(self.spool / batch)
        self.assertEqual((192, self.TREE, False), (marker["records"], marker["hermit_sha"],
                                                    marker["source_tree_dirty"]))
        sources = [json.loads(line) for line in data.decode().splitlines()]
        self.assertEqual([s["record"] for s in sources], [r["record"] for r in rows])
        self.assertEqual([s["cell"] for s in sources], [r["cell"] for r in rows])

        replay = self._run(data, emitted_at="2026-09-28T23:00:00Z")
        self.assertEqual(0, replay.returncode, replay.stderr.decode())
        self.assertIn("parity: 192 record(s) for validate run", replay.stdout.decode())
        self.assertIn("are already recorded", replay.stdout.decode())
        self.assertEqual([batch], self._batches())

    def test_a_malformed_record_refuses_the_batch_and_names_it(self):
        lines = (self.FIXTURE / self.TYPED).read_text().splitlines()
        tampered = json.loads(lines[7])
        self.assertEqual("diverged", tampered["verdict"])
        tampered["verdict"] = "matched"
        lines[7] = json.dumps(tampered)
        data = ("\n".join(lines) + "\n").encode()
        refused = self._run(data)
        self.assertEqual(1, refused.returncode)
        self.assertEqual(b"", refused.stdout)
        error = (f"input row 8: parity ledger source {tampered['cell']}: row verdict matched "
                 "disagrees with record verdict diverged")
        (batch,) = self._batches()
        self.assertIn(
            "series: REFUSED the parity batch, no row was written; the refusal is recorded -> "
            f"{self.spool / batch}: {error}", refused.stderr.decode())
        # The run reads as refused, with the message, and no row was spooled.
        refusal = self._only_refusal(data, error, False)
        self.assertEqual(192, refusal["input_lines"])
        # The same input refused again for the same reason is the one refusal.
        again = self._run(data, emitted_at="2026-09-28T22:00:00Z")
        self.assertEqual(1, again.returncode)
        self.assertIn("the refusal is already recorded", again.stderr.decode())
        self.assertEqual([batch], self._batches())

    def test_the_verbatim_untyped_export_is_refused_and_the_refusal_recorded(self):
        # parity-ledger/v1 was never published without the typed fields, so a
        # row without them has no admissible reading.
        data = (self.FIXTURE / "ledger-sources.jsonl").read_bytes()
        refused = self._run(data)
        self.assertEqual(1, refused.returncode)
        self.assertEqual(b"", refused.stdout)
        error = ("input row 1: parity ledger source fields are not exactly its schema's: missing "
                 "unavailable_class, operand")
        (batch,) = self._batches()
        self.assertIn(
            "series: REFUSED the parity batch, no row was written; the refusal is recorded -> "
            f"{self.spool / batch}: {error}", refused.stderr.decode())
        refusal = self._only_refusal(data, error, False)
        self.assertEqual(192, refusal["input_lines"])

    def test_a_blank_line_or_bad_json_is_refused_not_skipped(self):
        line = (self.FIXTURE / self.TYPED).read_text().splitlines()[0]
        blank = self._run((line + "\n\n" + line + "\n").encode())
        self.assertEqual(1, blank.returncode)
        self.assertIn("stdin line 2 is blank", blank.stderr.decode())
        truncated = self._run((line + "\n" + line[:40] + "\n").encode())
        self.assertEqual(1, truncated.returncode)
        self.assertIn("stdin line 2", truncated.stderr.decode())
        # Two refusals of two inputs; neither spooled a row.
        recorded = self._recorded()
        self.assertEqual(["stdin line 2 is blank"],
                         [event["error"] for event, _ in recorded if event["input_lines"] == 3])
        self.assertEqual(2, len(recorded))
        self.assertEqual([[], []], [rows for _, rows in recorded])

    def test_empty_stdin_records_an_empty_selection(self):
        empty = self._run(b"", run_id="validate-empty-selection")
        self.assertEqual(0, empty.returncode, empty.stderr.decode())
        self.assertIn("parity: recorded an empty parity selection for validate run "
                      "validate-empty-selection", empty.stdout.decode())
        (batch,) = self._batches()
        marker, rows = series.parity_ledger_module().read_batch(self.spool / batch)
        self.assertEqual((0, [], []), (marker["records"], marker["nodes"], rows))

    def test_usage_errors_exit_2(self):
        missing = subprocess.run(
            [sys.executable, str(Path(series.__file__).resolve()), "append-parity",
             "--parent", str(self.parent), "--producer", "validate", "--run-id", "x"],
            input=b"", capture_output=True, timeout=60, check=False)
        self.assertEqual(2, missing.returncode)
        self.assertIn("append-parity requires --tree", missing.stderr.decode())
        dirty = self._run(b"", dirty="maybe")
        self.assertEqual(2, dirty.returncode)
        self.assertIn("--source-tree-dirty must be true or false", dirty.stderr.decode())
        self._only_refusal(
            b"", "append-parity --source-tree-dirty must be true or false, got 'maybe'", None)
        bad_producer = subprocess.run(
            [sys.executable, str(Path(series.__file__).resolve()), "append-parity",
             "--parent", str(self.parent), "--producer", "hermit-repeat", "--run-id", "x",
             "--tree", self.TREE, "--source-tree-dirty", "false"],
            input=b"", capture_output=True, timeout=60, check=False)
        self.assertEqual(1, bad_producer.returncode)
        self.assertIn("--producer", bad_producer.stderr.decode())
        # No valid producer names the refusal, so none is recorded for it.
        self.assertIn("the refusal is not recorded: no valid producer, run and tree name it",
                      bad_producer.stderr.decode())
        self.assertEqual(1, len(self._batches()))


    # --source-tree-dirty is evidence, so append-parity never defaults it.

    def test_an_append_without_the_tree_state_is_a_usage_error_and_a_recorded_refusal(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        refused = self._run(data, dirty=None)
        self.assertEqual(2, refused.returncode)
        self.assertEqual(b"", refused.stdout)
        stderr = refused.stderr.decode()
        self.assertIn("series: append-parity requires --source-tree-dirty true|false", stderr)
        self.assertIn("usage:", stderr)
        self.assertIn("series: the refusal is recorded -> ", stderr)
        refusal = self._only_refusal(
            data, "append-parity requires --source-tree-dirty true|false", None)
        self.assertEqual(192, refusal["input_lines"])

    def test_a_dirty_tree_is_stamped_on_the_marker_and_every_row(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        appended = self._run(data, dirty="true")
        self.assertEqual(0, appended.returncode, appended.stderr.decode())
        ((marker, rows),) = self._recorded()
        self.assertEqual({True}, {marker["source_tree_dirty"], *(row["source_tree_dirty"] for row in rows)})

    # hermit's parity export (append_ledger_rows in ci/manifest-plan/src/parity.rs)
    # probes the writer with `append-parity --help`, stdin /dev/null, and sends
    # no rows unless that exits 0.

    def _series(self, *arguments, stdin=subprocess.DEVNULL):
        return subprocess.run(
            [sys.executable, str(Path(series.__file__).resolve()), *arguments],
            stdin=stdin, capture_output=True, timeout=60, check=False)

    def test_help_prints_the_usage_naming_every_flag_on_stdout_and_exits_0(self):
        shown = self._series("append-parity", "--help")
        self.assertEqual(0, shown.returncode, shown.stderr.decode())
        self.assertEqual(b"", shown.stderr)
        self.assertEqual((series.APPEND_PARITY_USAGE + "\n").encode(), shown.stdout)
        usage = shown.stdout.decode()
        self.assertIn("--source-tree-dirty <true|false>", usage)
        # Its options list is exactly the flags the parser accepts, plus help.
        self.assertEqual(
            {*series.APPEND_PARITY_FLAGS, "-h, --help"},
            set(re.findall(r"^  (--[a-z-]+|-h, --help) ", usage, re.MULTILINE)))
        producers = "|".join(series.parity_ledger_module().PRODUCERS)
        self.assertIn(f"--producer <{producers}>", usage)
        self.assertFalse(self.spool.exists())

    def test_short_help_is_the_same_help(self):
        short = self._series("append-parity", "-h")
        self.assertEqual(0, short.returncode, short.stderr.decode())
        self.assertEqual(b"", short.stderr)
        self.assertIn(b"--source-tree-dirty", short.stdout)
        self.assertEqual(self._series("append-parity", "--help").stdout, short.stdout)

    def test_help_with_another_argument_is_a_usage_error_that_writes_nothing(self):
        for arguments in (("--help", "--parent", "/x"), ("--parent", "/x", "-h")):
            mixed = self._series("append-parity", *arguments)
            self.assertEqual(2, mixed.returncode, arguments)
            self.assertEqual(b"", mixed.stdout, arguments)
            stderr = mixed.stderr.decode()
            self.assertIn("series: help cannot be combined with other arguments", stderr)
            self.assertIn("Run `series.py append-parity --help` for the supported flags.", stderr)
            self.assertIn(series.APPEND_PARITY_USAGE, stderr)
        # Help inside a complete call with rows on stdin reads and spools nothing.
        complete = self._run((self.FIXTURE / self.TYPED).read_bytes(), "--help")
        self.assertEqual(2, complete.returncode)
        self.assertEqual(b"", complete.stdout)
        self.assertIn("series: help cannot be combined with other arguments",
                      complete.stderr.decode())
        self.assertFalse(self.spool.exists())

    def test_a_bare_unknown_flag_is_still_a_usage_error(self):
        bare = self._series("append-parity", "--bogus")
        self.assertEqual(2, bare.returncode)
        self.assertEqual(b"", bare.stdout)
        self.assertIn("series: unknown append-parity argument '--bogus'", bare.stderr.decode())

    def test_hermits_exact_call_for_each_producer_and_tree_state(self):
        """append_ledger_rows' argv, which has no --emitted-at, with its rows on
        stdin from a regular file: hermit's own export lines, one compact
        ParityLedgerSource per line."""
        parity = series.parity_ledger_module()
        staging = tempfile.TemporaryDirectory()
        self.addCleanup(staging.cleanup)
        lines = (self.FIXTURE / self.TYPED).read_text().splitlines()[:2]
        sent = {}
        for producer in parity.PRODUCERS:
            for dirty in ("false", "true"):
                run_id = f"{producer}-hermit-call-dirty-{dirty}"
                # RUN 1953's lines, renamed to this run wherever they name it:
                # the row's run_id, its record's run_id and the record's log path.
                renamed = [line.replace(self.RUN_ID, run_id) for line in lines]
                sent[run_id] = [json.loads(line) for line in renamed]
                self.assertEqual(
                    [run_id] * 4,
                    [value for source in sent[run_id]
                     for value in (source["run_id"], source["record"]["run_id"])])
                rows = Path(staging.name) / f"{run_id}.jsonl"
                rows.write_text("".join(line + "\n" for line in renamed))
                with rows.open("rb") as stdin:
                    appended = self._series(
                        "append-parity", "--parent", str(self.parent), "--producer", producer,
                        "--run-id", run_id, "--tree", self.TREE, "--source-tree-dirty", dirty,
                        stdin=stdin)
                self.assertEqual(0, appended.returncode, appended.stderr.decode())
                stdout = appended.stdout.decode()
                self.assertTrue(stdout.startswith("parity: appended 2 record(s) ("), stdout)
                self.assertIn(f" for {producer} run {run_id} -> ", stdout)
        recorded = {marker["run_id"]: (marker, rows) for marker, rows in self._recorded()}
        self.assertEqual(sorted(sent), sorted(recorded))
        for run_id, (marker, rows) in recorded.items():
            producer, dirty = marker["producer"], marker["source_tree_dirty"]
            self.assertEqual(run_id, f"{producer}-hermit-call-dirty-{str(dirty).lower()}")
            self.assertEqual((2, self.TREE), (marker["records"], marker["hermit_sha"]))
            self.assertRegex(marker["emitted_at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
            for source, row in zip(sent[run_id], rows, strict=True):
                # Every field hermit sent arrives unchanged, under the call's identity.
                self.assertEqual(source, {field: row[field] for field in source})
                self.assertEqual(
                    (producer, run_id, self.TREE, dirty, marker["emitted_at"]),
                    (row["producer"], row["run_id"], row["hermit_sha"],
                     row["source_tree_dirty"], row["emitted_at"]))

    # hermit reads exit 0 as acceptance and never sends the rows again, so a
    # parity spool that does not take a call (a publisher holding its lock
    # past the writer's 5 s, a failing write) must delay the rows, not lose
    # them: the call is deferred, and the next series publication replays it
    # (series.replay_deferred_parity, called by mirror.publish_series).

    NOTHING_REPLAYED = {"replayed": 0, "appended": 0, "already_recorded": 0, "refused": 0,
                        "batches": [], "kept": []}

    def _deferred(self):
        directory = series.parity_deferred_dir(self.parent)
        return sorted(directory.glob("*.jsonl")) if directory.is_dir() else []

    @contextlib.contextmanager
    def _hold_the_spool_lock(self):
        """Hold the parity spool's .append.lock as a publisher holds it."""
        with series.series_spool_transition_lock(self.spool, exclusive=True):
            yield

    def _lock_timeout(self, seconds="5.000"):
        return (f"timed out after {seconds}s acquiring exclusive series spool lock "
                f"{self.spool / '.append.lock'}")

    def _tampered(self):
        """RUN 1953's rows with one row's verdict changed, and the refusal it earns."""
        lines = (self.FIXTURE / self.TYPED).read_text().splitlines()
        tampered = json.loads(lines[7])
        tampered["verdict"] = "matched"
        lines[7] = json.dumps(tampered)
        error = (f"input row 8: parity ledger source {tampered['cell']}: row verdict matched "
                 "disagrees with record verdict diverged")
        return ("\n".join(lines) + "\n").encode(), error

    def _call(self, data: bytes, *, run_id=None, emitted_at="2026-09-28T21:40:00Z"):
        """The record append-parity keeps for a call the spool did not take."""
        return {
            "schema": series.PARITY_DEFERRED_SCHEMA, "kind": "rows",
            "deferred_at": "2026-09-28T21:40:05Z", "reason": self._lock_timeout(),
            "producer": "validate", "run_id": run_id or self.RUN_ID, "tree": self.TREE,
            "emitted_at": emitted_at, "source_tree_dirty": False, "host": series._writer_host(),
            "input": data.decode(), "input_sha256": hashlib.sha256(data).hexdigest(),
            "input_bytes": len(data),
        }

    def test_rows_turned_away_by_a_held_spool_lock_still_arrive(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        with self._hold_the_spool_lock():
            started = time.monotonic()
            deferred = self._run(data)
            waited = time.monotonic() - started
            self.assertEqual([], self._batches())
        # Exit 0: the call is accepted, so hermit must not and does not resend it.
        self.assertEqual(0, deferred.returncode, deferred.stderr.decode())
        self.assertGreaterEqual(waited, series.SERIES_SPOOL_LOCK_TIMEOUT_SECONDS)
        (kept,) = self._deferred()
        self.assertEqual(
            "parity: deferred 192 record(s) (candidate-missing 3, inputs-not-equalized 14, "
            f"diverged 175) for validate run {self.RUN_ID}: the parity spool did not take them "
            f"({self._lock_timeout()}); the call is kept -> {kept}, and the next series "
            "publication appends it\n", deferred.stdout.decode())
        self.assertIn("published ledger not consulted", deferred.stderr.decode())
        parity = series.parity_ledger_module()
        self.assertTrue(kept.name.startswith(parity.run_event_id("validate", self.RUN_ID) + "."))
        record = json.loads(kept.read_text())
        self.assertRegex(record.pop("deferred_at"), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        expected = self._call(data)
        expected.pop("deferred_at")
        self.assertEqual(expected, record)

        # The next publication replays the call: exactly the batch a direct
        # append would have written, still dated by the call's --emitted-at.
        replayed = series.replay_deferred_parity(self.parent)
        (batch,) = self._batches()
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "appended": 1, "batches": [batch]},
                         replayed)
        self.assertEqual([], self._deferred())
        direct_parent = tempfile.TemporaryDirectory()
        self.addCleanup(direct_parent.cleanup)
        direct = self._run(data, parent=Path(direct_parent.name))
        self.assertEqual(0, direct.returncode, direct.stderr.decode())
        direct_spool = series.parity_spool_dir(direct_parent.name)
        self.assertEqual([batch], sorted(p.name for p in direct_spool.glob("*.jsonl")))
        self.assertEqual((direct_spool / batch).read_bytes(), (self.spool / batch).read_bytes())
        # Replaying again finds nothing to do.
        self.assertEqual(self.NOTHING_REPLAYED, series.replay_deferred_parity(self.parent))
        self.assertEqual([batch], self._batches())

    def test_a_refusal_turned_away_by_a_held_spool_lock_is_deferred_then_recorded(self):
        data, error = self._tampered()
        with self._hold_the_spool_lock():
            refused = self._run(data)
        self.assertEqual(1, refused.returncode)
        self.assertEqual(b"", refused.stdout)
        (kept,) = self._deferred()
        self.assertIn(
            "series: REFUSED the parity batch, no row was written; the refusal is deferred -> "
            f"{kept} (the parity spool did not take it: {self._lock_timeout()}); the next series "
            f"publication records it: {error}", refused.stderr.decode())
        self.assertEqual([], self._batches())
        replayed = series.replay_deferred_parity(self.parent)
        (batch,) = self._batches()
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "refused": 1, "batches": [batch]},
                         replayed)
        refusal = self._only_refusal(data, error, False)
        self.assertEqual(("2026-09-28T21:40:00Z", 192), (refusal["emitted_at"], refusal["input_lines"]))
        self.assertEqual([], self._deferred())

    def test_a_replay_the_spool_lock_turns_away_keeps_the_call(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        kept = series.defer_parity_call(self.parent, self._call(data))
        with self._hold_the_spool_lock():
            replayed = series.replay_deferred_parity(self.parent, timeout_seconds=0.2)
        self.assertEqual({**self.NOTHING_REPLAYED, "kept": [
            {"file": kept.name, "reason": f"the parity spool lock failed: {self._lock_timeout('0.200')}"},
        ]}, replayed)
        self.assertEqual([kept], self._deferred())
        self.assertEqual([], self._batches())
        # Once the lock is free, the same call arrives.
        replayed = series.replay_deferred_parity(self.parent)
        (batch,) = self._batches()
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "appended": 1, "batches": [batch]},
                         replayed)
        marker, rows = series.parity_ledger_module().read_batch(self.spool / batch)
        self.assertEqual((192, "2026-09-28T21:40:00Z"), (marker["records"], marker["emitted_at"]))
        self.assertEqual(192, len(rows))

    # parity_ledger._write_batch renames a batch into place without syncing the
    # spool directory, so a call deleted before that sync could be lost with
    # its batch in a crash.

    def test_a_replayed_call_is_deleted_only_after_the_spool_directory_is_synced(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        kept = series.defer_parity_call(self.parent, self._call(data))
        native = series._fsync_quarantine_directory
        synced = []

        def fsync(directory):
            if Path(directory) == self.spool:
                # Still under the spool lock, with the batch in place and the call not yet deleted.
                with self.assertRaises(series.SeriesError):
                    with series.series_spool_transition_lock(self.spool, exclusive=True, timeout_seconds=0):
                        pass
                synced.append((self._batches(), kept.exists()))
            native(directory)

        with mock.patch.object(series, "_fsync_quarantine_directory", side_effect=fsync):
            replayed = series.replay_deferred_parity(self.parent)
        (batch,) = self._batches()
        self.assertEqual([([batch], True)], synced)
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "appended": 1, "batches": [batch]},
                         replayed)
        self.assertEqual([], self._deferred())

    def test_a_spool_directory_that_cannot_be_synced_keeps_every_decided_call(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        kept = series.defer_parity_call(self.parent, self._call(data))
        native = series._fsync_quarantine_directory

        def fsync(directory):
            if Path(directory) == self.spool:
                raise OSError(errno.EIO, "Input/output error")
            native(directory)

        with mock.patch.object(series, "_fsync_quarantine_directory", side_effect=fsync):
            replayed = series.replay_deferred_parity(self.parent)
        self.assertEqual({**self.NOTHING_REPLAYED, "kept": [
            {"file": kept.name, "reason": "the parity spool was not synced: [Errno 5] Input/output error"},
        ]}, replayed)
        # The batch may or may not survive a crash now; the call stays either way.
        (batch,) = self._batches()
        self.assertEqual([kept], self._deferred())
        # The next replay finds the batch already recorded, writes nothing, and
        # only then deletes the call.
        replayed = series.replay_deferred_parity(self.parent)
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "already_recorded": 1}, replayed)
        self.assertEqual([batch], self._batches())
        self.assertEqual([], self._deferred())

    def test_a_deferred_call_the_ledger_now_contradicts_is_refused_not_dropped(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        appended = self._run(data)
        self.assertEqual(0, appended.returncode, appended.stderr.decode())
        (batch,) = self._batches()
        subset = b"".join(data.splitlines(keepends=True)[:2])
        series.defer_parity_call(self.parent, self._call(subset, emitted_at="2026-09-28T22:10:00Z"))
        replayed = series.replay_deferred_parity(self.parent)
        (refusal_batch,) = [name for name in self._batches() if name != batch]
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "refused": 1,
                          "batches": [refusal_batch]}, replayed)
        parity = series.parity_ledger_module()
        refusal, rows = parity.read_batch(self.spool / refusal_batch)
        self.assertEqual(parity.REFUSAL_EVENT_TYPE, refusal["event_type"])
        self.assertIn(f"validate run {self.RUN_ID} is already recorded in", refusal["error"])
        self.assertIn("with different content", refusal["error"])
        self.assertEqual((hashlib.sha256(subset).hexdigest(), 2, "2026-09-28T22:10:00Z", []),
                         (refusal["input_sha256"], refusal["input_lines"], refusal["emitted_at"], rows))
        self.assertEqual([], self._deferred())

    def test_a_deferred_file_that_cannot_be_read_is_kept_and_the_rest_replayed(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        good = series.defer_parity_call(self.parent, self._call(data))
        parity = series.parity_ledger_module()
        torn = good.parent / f"{parity.run_event_id('validate', self.RUN_ID)}.1-2-0badf00d.jsonl"
        torn.write_bytes(good.read_bytes()[:100])
        other = parity.run_event_id("validate", "validate-another-run")
        misnamed = good.parent / f"{other}.1-2-0badf00d.jsonl"
        misnamed.write_bytes(good.read_bytes())
        # A kind that cannot be looked up used to raise TypeError, which no
        # replay caught, so one such file stopped every replay.
        unkinded = good.parent / f"{parity.run_event_id('validate', self.RUN_ID)}.1-3-0badf00d.jsonl"
        unkinded.write_text(json.dumps({**json.loads(good.read_bytes()), "kind": []}) + "\n")
        replayed = series.replay_deferred_parity(self.parent)
        (batch,) = self._batches()
        self.assertEqual({
            torn.name: f"unreadable: {torn.name}: is not exactly one line",
            misnamed.name: (f"unreadable: {misnamed.name}: names a different run than its "
                            f"validate run {self.RUN_ID}"),
            unkinded.name: f"unreadable: {unkinded.name}: a [] call does not carry exactly its fields",
        }, {item["file"]: item["reason"] for item in replayed.pop("kept")})
        self.assertEqual({**self.NOTHING_REPLAYED, "replayed": 1, "appended": 1, "batches": [batch]},
                         {**replayed, "kept": []})
        # None is ever deleted; the next replay keeps them again.
        self.assertEqual(sorted([torn, misnamed, unkinded]), self._deferred())
        again = series.replay_deferred_parity(self.parent)
        self.assertEqual((0, 3), (again["replayed"], len(again["kept"])))
        self.assertEqual(sorted([torn, misnamed, unkinded]), self._deferred())

    def test_a_call_that_can_be_neither_written_nor_deferred_exits_2(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        directory = series.parity_deferred_dir(self.parent)
        directory.parent.mkdir(parents=True)
        directory.write_text("not a directory\n")
        with self._hold_the_spool_lock():
            failed = self._run(data)
        self.assertEqual(2, failed.returncode)
        self.assertEqual(b"", failed.stdout)
        self.assertIn(
            f"series: cannot write the parity spool {self.spool}: {self._lock_timeout()}; and the "
            "call could not be deferred either: ", failed.stderr.decode())
        self.assertEqual([], self._batches())

    def test_a_call_is_never_kept_in_a_form_the_replay_would_refuse(self):
        data = (self.FIXTURE / self.TYPED).read_bytes()
        parity = series.parity_ledger_module()

        def refusal(record):
            for name in list(record):
                if name not in series.PARITY_DEFERRED_FIELDS["refusal"]:
                    del record[name]
            record.update(kind="refusal", event={"event_type": parity.REFUSAL_EVENT_TYPE})

        cases = {
            "another schema": lambda r: r.update(schema="parity-deferred/v2"),
            "an unknown kind": lambda r: r.update(kind="batch"),
            "a kind that is not a string": lambda r: r.update(kind=[]),
            "an extra field": lambda r: r.update(spooled_at="2026-09-28T21:40:05Z"),
            "a missing field": lambda r: r.pop("host"),
            "an empty reason": lambda r: r.update(reason=""),
            "a tree state that is not a boolean": lambda r: r.update(source_tree_dirty="false"),
            "a byte count that is a boolean": lambda r: r.update(input_bytes=True),
            "an input that is not the one it names": lambda r: r.update(input=r["input"] + "\n"),
            "a digest that is not its input's": lambda r: r.update(input_sha256="0" * 64),
            "a refusal that is not a refusal event": refusal,
        }
        for name, change in cases.items():
            with self.subTest(name):
                record = self._call(data)
                change(record)
                with self.assertRaises(parity.ParityLedgerError):
                    series.defer_parity_call(self.parent, record)
                self.assertEqual([], self._deferred())
        kept = series.defer_parity_call(self.parent, self._call(data))
        self.assertEqual([kept], self._deferred())


class WriteBoundaryCli(unittest.TestCase):
    """The CLI exists so a producer can CALL `validate_row` rather than
    reimplement it in another language. These pin the properties that make
    calling it worth more than matching it."""

    SHA = "a" * 40
    CELL = "c-programs/pipecap/verify/ptrace"

    def _row(self, event_id, run_index, outcome, coordinates=None, run_id="campaign-1"):
        result, failure_class = classification_for_series_outcome(outcome)
        payload = {
            "cell": self.CELL, "tree": self.SHA,
            "run_index": run_index, "outcome": outcome,
            "result": result, "failure_class": failure_class,
            "machine_shortname": "devbig014",
            "kernel_version": "7.1.3-test",
            "host_capabilities": {
                "cpuid-faulting": {"present": False, "evidence": "fixture absence"},
                "kvm": {"present": True, "evidence": "fixture presence"},
            },
        }
        if coordinates is not None:
            payload["coordinates"] = coordinates
        return {
            "schema": series.ENVELOPE_VERSION, "event_id": event_id,
            "event_type": "series.observation", "emitted_at": "2026-08-25T16:00:00Z",
            "team": "hermit", "host": "devbig014", "run_id": run_id,
            "producer": "pressure-test",
            "series": payload,
        }

    def _append(self, parent, rows):
        import io
        import contextlib
        stdin = io.StringIO("\n".join(json.dumps(r) for r in rows) + "\n")
        out, err = io.StringIO(), io.StringIO()
        real = sys.stdin
        sys.stdin = stdin
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = series.main(["append", "--parent", str(parent)])
        finally:
            sys.stdin = real
        return rc, out.getvalue() + err.getvalue()

    def _spool_lines(self, parent):
        d = series.spool_dir(parent)
        if not d.is_dir():
            return []
        return [l for f in sorted(d.glob("*.jsonl")) for l in f.read_text().splitlines() if l]

    def test_appends_to_the_spool_and_never_the_tracked_shard(self):
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            rc, _ = self._append(parent, [self._row("e1", 0, "passed")])
            self.assertEqual(rc, 0)
            self.assertEqual(len(self._spool_lines(parent)), 1)
            # The tracked root must not be created by a producer.
            self.assertFalse((Path(parent) / "series").exists())

    def test_a_malformed_row_refuses_the_WHOLE_batch(self):
        """Partial writes are the failure this must not have: a half-written
        campaign reads as a complete one with fewer repetitions."""
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            self._append(parent, [self._row("good", 0, "passed")])
            before = self._spool_lines(parent)
            rc, text = self._append(parent, [
                self._row("ok", 1, "diverged", {"first_divergent_record": 98}),
                self._row("bad", 2, "passed", {"first_divergent_record": 98}),
            ])
            self.assertEqual(rc, 1)
            self.assertIn("REFUSED", text)
            self.assertEqual(self._spool_lines(parent), before,
                             "a refused batch must leave the spool byte-identical")

    def test_diverged_without_a_coordinate_is_accepted(self):
        """diverged-unlocated is a legitimate state; a producer must never be
        forced to invent a position it does not have."""
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            rc, _ = self._append(parent, [self._row("e1", 0, "diverged")])
            self.assertEqual(rc, 0)
            rows = series.union(sorted(series.spool_dir(parent).glob("*.jsonl")))
            self.assertEqual(series.measurement_state(rows), "diverged-unlocated")

    def test_a_batch_spanning_two_run_ids_is_refused(self):
        """The batch is named after its run, so two runs in one batch would
        silently file half of them under the other's name."""
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            rc, text = self._append(parent, [
                self._row("e1", 0, "passed", run_id="campaign-1"),
                self._row("e2", 0, "passed", run_id="campaign-2"),
            ])
            self.assertEqual(rc, 1)
            self.assertIn("exactly one run_id", text)
            self.assertEqual(self._spool_lines(parent), [])

    def test_the_batch_name_is_stable_so_a_rerun_dedups(self):
        """Not clock-derived: re-running one campaign must land in the same file
        so union_events can dedup by event_id instead of double-counting."""
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            rows = [self._row("e1", 0, "passed"), self._row("e2", 1, "diverged")]
            self._append(parent, rows)
            self._append(parent, rows)
            files = sorted(series.spool_dir(parent).glob("*.jsonl"))
            self.assertEqual(len(files), 1, "a rerun must not create a second batch file")
            unioned = series.union(files)
            self.assertEqual(len(unioned), 2, "event_id dedup must collapse the rerun")

    def test_no_rows_is_not_an_error_but_is_not_silent(self):
        import tempfile
        with tempfile.TemporaryDirectory() as parent:
            rc, text = self._append(parent, [])
            self.assertEqual(rc, 0)
            self.assertIn("nothing appended", text)


class RetractParityCli(unittest.TestCase):
    """`series.py retract-parity`: one `parity.retraction` of published parity
    events, spooled only when every id it names is published (parity summary
    format v4, section 9)."""

    REASON = "test fixture published by a leaking spool path"

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.parent = Path(self.temporary.name)
        # Stop Git discovery here so the spool is this parent's own.
        subprocess.run(["git", "init", "-q", str(self.parent)], check=True)
        (self.parent / ".gitmodules").write_text(
            '[submodule "hermit_test_ledger"]\n path = hermit_test_ledger\n'
            f' url = {series.LEDGER_REPOSITORY}\n')
        parity = series.parity_ledger_module()
        typed = AppendParityCli.FIXTURE / AppendParityCli.TYPED
        sources = [parity.loads(line, "fixture") for line in typed.read_text().splitlines()[:2]]
        self.events = parity.build_batch(
            sources, producer="validate", run_id=AppendParityCli.RUN_ID, hermit_sha=AppendParityCli.TREE,
            emitted_at="2026-09-28T21:40:00Z", source_tree_dirty=False, host="fixturehost")
        ledger = self.parent / "hermit_test_ledger"
        subprocess.run(["git", "init", "-q", str(ledger)], check=True)
        for event in self.events:
            shard = ledger / parity.event_shard(event)
            shard.parent.mkdir(parents=True, exist_ok=True)
            with shard.open("a") as handle:
                handle.write(parity.canonical_line(event) + "\n")
        git = ["git", "-C", str(ledger), "-c", "user.name=t", "-c", "user.email=t@invalid"]
        subprocess.run([*git, "add", "."], check=True)
        subprocess.run([*git, "commit", "-qm", "published fixture"], check=True)
        subprocess.run([*git, "remote", "add", "origin", series.LEDGER_REPOSITORY], check=True)
        subprocess.run([*git, "update-ref", "refs/remotes/origin/main", "HEAD"], check=True)
        self.spool = series.parity_spool_dir(self.parent)
        self.assertEqual(self.parent / "ignored/ci-hub/parity-spool", self.spool)

    def _run(self, ids, *extra):
        return subprocess.run(
            [sys.executable, str(Path(series.__file__).resolve()), "retract-parity",
             "--parent", str(self.parent), "--reason", self.REASON,
             "--emitted-at", "2026-10-07T12:00:00Z", *extra],
            input="".join(f"{item}\n" for item in ids).encode(), capture_output=True, timeout=120,
            check=False)

    def test_a_retraction_of_published_events_is_spooled_alone(self):
        ids = [event["event_id"] for event in self.events[1:]]
        result = self._run(ids)
        self.assertEqual(0, result.returncode, result.stderr.decode())
        [batch] = sorted(self.spool.glob("*.jsonl"))
        retraction, rows = series.parity_ledger_module().read_batch(batch)
        self.assertEqual(([], sorted(ids), self.REASON), (rows, retraction["retracts"], retraction["reason"]))
        self.assertIn(f"parity: spooled retraction {retraction['event_id']} of 2 event(s)",
                      result.stdout.decode())

    def test_an_unpublished_id_is_refused_and_nothing_is_spooled(self):
        result = self._run([self.events[1]["event_id"], "f" * 64])
        self.assertEqual(1, result.returncode)
        self.assertIn("1 of 2 event id(s) are not published parity events", result.stderr.decode())
        self.assertFalse(self.spool.exists() and any(self.spool.glob("*.jsonl")))

    def test_a_retraction_of_a_published_retraction_is_refused_before_it_is_spooled(self):
        parity = series.parity_ledger_module()
        earlier = parity.build_retraction([self.events[1]["event_id"]], self.REASON, team="hermit",
                                          host="fixturehost", emitted_at="2026-10-07T11:00:00Z")
        ledger = self.parent / "hermit_test_ledger"
        with (ledger / parity.event_shard(earlier)).open("a") as handle:
            handle.write(parity.canonical_line(earlier) + "\n")
        git = ["git", "-C", str(ledger), "-c", "user.name=t", "-c", "user.email=t@invalid"]
        subprocess.run([*git, "add", "."], check=True)
        subprocess.run([*git, "commit", "-qm", "published retraction"], check=True)
        subprocess.run([*git, "update-ref", "refs/remotes/origin/main", "HEAD"], check=True)
        result = self._run([earlier["event_id"]])
        self.assertEqual(1, result.returncode)
        self.assertIn("name a published retraction, which is never withdrawn", result.stderr.decode())
        self.assertFalse(self.spool.exists() and any(self.spool.glob("*.jsonl")))

    def test_help_names_the_command(self):
        result = subprocess.run([sys.executable, str(Path(series.__file__).resolve()), "retract-parity", "-h"],
                                capture_output=True, timeout=60, check=False)
        self.assertEqual((0, series.RETRACT_PARITY_USAGE + "\n"), (result.returncode, result.stdout.decode()))


class CellResultProjection(unittest.TestCase):
    SHA = ""

    def setUp(self):
        import tempfile

        self.temporary = tempfile.TemporaryDirectory()
        self.parent = Path(self.temporary.name)
        # TMPDIR may itself be inside a dev-hermit checkout. Stop Git discovery
        # here so shared_parent_root cannot route fixture rows to its real spool.
        # The linked-worktree case below still exercises real common-dir lookup.
        subprocess.run(["git", "init", "-q", str(self.parent)], check=True)
        self.assertEqual(series.spool_dir(self.parent), self.parent / "ignored/ci-hub/series-spool")
        (self.parent / ".gitmodules").write_text(
            '[submodule "hermit_test_ledger"]\n path = hermit_test_ledger\n'
            f' url = {series.LEDGER_REPOSITORY}\n')
        (self.parent / ".gitignore").write_text("hermit_test_ledger/\nignored/\n")
        ledger = self.parent / "hermit_test_ledger"
        subprocess.run(["git", "init", "-q", str(ledger)], check=True)
        subprocess.run(["git", "-C", str(ledger), "-c", "user.name=t", "-c", "user.email=t@invalid",
                        "commit", "--allow-empty", "-qm", "empty published fixture"], check=True)
        subprocess.run(["git", "-C", str(ledger), "remote", "add", "origin", series.LEDGER_REPOSITORY], check=True)
        subprocess.run(["git", "-C", str(ledger), "update-ref", "refs/remotes/origin/main", "HEAD"], check=True)
        self.repo = self.parent / "hermit"
        self.repo.mkdir()
        subprocess.run(["git", "-C", str(self.repo), "init", "-q"], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.name", "Series Test"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.email", "series@example.invalid"],
            check=True,
        )
        (self.repo / "one").write_text("one\n")
        # The committed manifests are the authority for a declared guest exit
        # (series.manifest_guest_exit_declarations); _expected_exit_cell names
        # one of these ids. defaults.yaml and README.md must be ignored: the
        # defaults file repeats an id with a different declaration. The
        # runner-red tests append -red to a cell's test id so it never shares
        # a cell name with its passing sibling; the two declared ids that
        # makes are declared here as well.
        self.manifest_dir = self.repo / series.HERMIT_MANIFEST_DIR
        self.manifest_dir.mkdir(parents=True)
        (self.manifest_dir / "fixture-exit.yaml").write_text(
            self._manifest_yaml("fixture-exit", [
                ("fixture/exit-7", "code: 7"),
                ("fixture/exit-3", "code: 3"),
                ("fixture/exit-1", "code: 1"),
                ("fixture/undeclared", None),
                ("fixture/exit-7-red", "code: 7"),
            ]),
            encoding="utf-8",
        )
        (self.manifest_dir / "fixture-signal.yaml").write_text(
            self._manifest_yaml("fixture-signal", [
                ("fixture/signal-11", "signal: 11"),
                ("fixture/signal-6", "signal: 6"),
                ("fixture/signal-11-red", "signal: 11"),
            ]),
            encoding="utf-8",
        )
        (self.manifest_dir / series.HERMIT_MANIFEST_DEFAULTS).write_text(
            self._manifest_yaml("defaults", [("fixture/exit-7", "code: 9")]),
            encoding="utf-8",
        )
        (self.manifest_dir / "README.md").write_text("test: [not a manifest]\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "."], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "commit", "-qm", "one"], check=True,
        )
        self.detcore_tree = subprocess.run(
            ["git", "-C", str(self.repo), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        subprocess.run(
            [
                "git", "-C", str(self.repo), "update-index", "--add", "--cacheinfo",
                f"160000,{self.detcore_tree},detcore",
            ],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(self.repo), "commit", "-qm", "record detcore"], check=True,
        )
        self.tree = subprocess.run(
            ["git", "-C", str(self.repo), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        subprocess.run(
            [
                "git", "-C", str(self.repo), "update-ref",
                "refs/remotes/origin/main", self.tree,
            ],
            check=True,
        )

    def tearDown(self):
        self.temporary.cleanup()

    #: The reason every fixture declaration carries, on the manifest entry and
    #: on the row the runner copies it to.
    FIXTURE_EXIT_REASON = "the fixture guest fails on purpose"

    @classmethod
    def _manifest_yaml(cls, bucket, entries, reason=None):
        """A schema-3 manifest whose verify modes declare ``entries``.

        Each entry is ``(test id, declaration line or None)``; the declaration
        line is ``code: N`` or ``signal: N`` as a manifest author writes it.
        """
        reason = cls.FIXTURE_EXIT_REASON if reason is None else reason
        lines = ["schema: 3", f"bucket: {bucket}", "test:"]
        for identifier, declaration in entries:
            lines += [
                f"  - id: {identifier}",
                "    lane: portable",
                "    modes:",
                "      verify:",
                "        ci: true",
                "        backends_enabled: [ptrace]",
            ]
            if declaration is not None:
                lines += [
                    "        expected_guest_exit:",
                    f"          {declaration}",
                    f"          reason: {reason}",
                ]
        return "\n".join(lines) + "\n"

    def _commit_manifests(self, files):
        """Commit ``files`` (name -> text, bytes, or None to delete); return the new tree."""
        for name, text in files.items():
            if text is None:
                subprocess.run(
                    ["git", "-C", str(self.repo), "rm", "-q", str(self.manifest_dir / name)],
                    check=True,
                )
            else:
                if isinstance(text, bytes):
                    (self.manifest_dir / name).write_bytes(text)
                else:
                    (self.manifest_dir / name).write_text(text, encoding="utf-8")
                subprocess.run(
                    ["git", "-C", str(self.repo), "add", str(self.manifest_dir / name)],
                    check=True,
                )
        subprocess.run(
            ["git", "-C", str(self.repo), "commit", "-qm", "manifest change"], check=True,
        )
        tree = subprocess.run(
            ["git", "-C", str(self.repo), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        subprocess.run(
            ["git", "-C", str(self.repo), "update-ref", "refs/remotes/origin/main", tree],
            check=True,
        )
        return tree

    def test_linked_parent_worktree_uses_the_primary_checkout_spool(self):
        primary = self.parent / "dev-hermit"
        (primary / "ci-hub" / "series").mkdir(parents=True)
        (primary / "ci-hub" / "series" / "series.py").write_text(
            "# producer marker\n", encoding="utf-8"
        )
        subprocess.run(["git", "init", "-q", "-b", "main", str(primary)], check=True)
        subprocess.run(["git", "-C", str(primary), "add", "."], check=True)
        subprocess.run(
            [
                "git", "-C", str(primary),
                "-c", "user.name=t", "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "seed",
            ],
            check=True,
        )
        linked = self.parent / "nested" / "slot" / "dev-hermit"
        linked.parent.mkdir(parents=True)
        subprocess.run(
            ["git", "-C", str(primary), "worktree", "add", "--detach", str(linked), "HEAD"],
            check=True,
            capture_output=True,
        )

        self.assertEqual(
            series.spool_dir(linked),
            primary / "ignored" / "ci-hub" / "series-spool",
        )

        outside = self.parent / "plain-directory"
        outside.mkdir()
        self.assertEqual(
            series.spool_dir(outside),
            outside / "ignored" / "ci-hub" / "series-spool",
        )

    def test_a_directory_nested_inside_a_checkout_keeps_its_own_spool(self):
        # 2026-10-07: pytest ran with TMPDIR under the shared checkout's
        # ignored/ tree. Git discovery walked up from each test's temporary
        # directory, so shared_parent_root returned the shared checkout and
        # test fixtures (a `{}` batch, a campaign-1 batch recording e1 twice
        # with different bodies) were written to the live series spool. The
        # next validate's append-cells then refused all 1102 of its rows.
        primary = self.parent / "dev-hermit"
        (primary / "ci-hub" / "series").mkdir(parents=True)
        (primary / "ci-hub" / "series" / "series.py").write_text(
            "# producer marker\n", encoding="utf-8"
        )
        subprocess.run(["git", "init", "-q", "-b", "main", str(primary)], check=True)
        subprocess.run(["git", "-C", str(primary), "add", "."], check=True)
        subprocess.run(
            [
                "git", "-C", str(primary),
                "-c", "user.name=t", "-c", "user.email=t@invalid",
                "commit", "-q", "-m", "seed",
            ],
            check=True,
        )
        linked = self.parent / "slots" / "one" / "dev-hermit"
        linked.parent.mkdir(parents=True)
        subprocess.run(
            ["git", "-C", str(primary), "worktree", "add", "--detach", str(linked), "HEAD"],
            check=True,
            capture_output=True,
        )
        # Checkout roots still resolve to the primary checkout.
        self.assertEqual(series.shared_parent_root(primary), primary.resolve())
        self.assertEqual(series.shared_parent_root(linked), primary.resolve())

        for nested in (
            primary / "ignored" / "pytest-tmp" / "tmpabc123",
            linked / "ignored" / "tmp",
            primary / "ci-hub",
        ):
            nested.mkdir(parents=True, exist_ok=True)
            with self.subTest(nested=str(nested.relative_to(self.parent))):
                own = nested.resolve()
                self.assertEqual(series.shared_parent_root(nested), own)
                self.assertEqual(
                    series.spool_dir(nested), own / "ignored" / "ci-hub" / "series-spool",
                )
                self.assertEqual(
                    series.quarantine_dir(nested),
                    own / "ignored" / "ci-hub" / "series-quarantine",
                )
                self.assertEqual(
                    series.parity_spool_dir(nested), own / "ignored" / "ci-hub" / "parity-spool",
                )
                self.assertEqual(
                    series.parity_deferred_dir(nested),
                    own / "ignored" / "ci-hub" / "parity-deferred",
                )

    def _attempt(
        self,
        verdict="matched",
        *,
        record=None,
        syscall=None,
        scheduler_turn=None,
        virtual_nanoseconds=None,
        left=None,
        right=None,
        timed_out=False,
        index="1",
    ):
        comparison = {
            "strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": True,
            "compare_io_buffers": True,
            "log_scope": "info",
            "record_envelope": "all_records_v1",
            "virtualize_time": True,
            "strip_lines": False,
            "canonicalize_addresses": True,
            "full_trace": True,
            "exact_remainder": True,
            "stripped_prefixes": ["real-wall-clock-prefix/v1"],
            "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines": False,
            "skip_commit": False,
            "skip_detlog": False,
        }
        compared = {"left": 17, "right": 17}
        report = {
            "verified": verdict == "matched",
            "bitwise_parity": verdict == "matched",
            "verdict": verdict,
            "no_result_reason": None,
            "infrastructure_error": None,
            "comparison": comparison if verdict in {"matched", "diverged"} else None,
            "compared_log_messages": compared if verdict in {"matched", "diverged"} else None,
            "runtime": None,
            "guest_exit_code": None,
            "guest_signal": None,
            "first_divergent_scheduler_turn": scheduler_turn,
            "first_divergent_virtual_nanoseconds": virtual_nanoseconds,
            "first_divergent_record": record,
            "first_divergent_syscall": syscall,
            "first_divergent_left_message": left,
            "first_divergent_right_message": right,
        }
        if verdict == "no_result":
            report["no_result_reason"] = {"kind": "not_run"}
        elif verdict == "infrastructure_error":
            report["infrastructure_error"] = {
                "kind": "skid_overshoot",
                "count": 1,
            }
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        return {
            "index": index,
            "outcome": (
                "PASS" if verdict == "matched"
                else "FAIL" if verdict == "diverged"
                else "ERROR"
            ),
            "error_kind": (
                "incomplete-verification-evidence"
                if timed_out or verdict == "no_result"
                else None
            ),
            "status": None if timed_out else (0 if verdict == "matched" else 1),
            "signal": 15 if timed_out else None,
            "timed_out": timed_out,
            "verification_report": raw,
            "verification_report_sha256": hashlib.sha256(raw.encode()).hexdigest(),
            "first_divergent_record": record,
            "first_divergent_syscall": syscall,
            "first_divergent_scheduler_turn": scheduler_turn,
            "first_divergent_virtual_nanoseconds": virtual_nanoseconds,
            "first_divergent_left_message": left,
            "first_divergent_right_message": right,
        }

    def _cell(self, run_index, outcome="PASS", duration_ms=100):
        result, failure_class = {
            "PASS": ("pass", None),
            "FAIL": ("determinism-failure", "product_failure"),
            "ERROR": (None, "understood_infrastructure_failure"),
            "HOST-INAPPLICABLE": (None, "understood_prerequisite_failure"),
        }.get(outcome, (None, None))
        return {
            "schema": 4,
            # The raw harness uses a separate evidence id per pressure
            # repetition. append-cells must NOT use it as the batch identity.
            "run_id": f"cell-evidence-{run_index}",
            "machine_shortname": "devbig014",
            "kernel_version": "7.1.3-test",
            "host_capabilities": {
                "cpuid-faulting": {"present": False, "evidence": "fixture absence"},
                "kvm": {"present": True, "evidence": "fixture presence"},
            },
            "hermit_sha": self.tree,
            "source_tree_dirty": False,
            "test": "applications/git-repository-workflow",
            "category": "applications",
            "lane": "portable",
            "mode": "verify",
            "backend": "ptrace",
            "classification": "required",
            "outcome": outcome,
            "result": result,
            "failure_class": failure_class,
            "duration_ms": duration_ms,
            "run_index": run_index,
            "runtime": {
                "run1": {
                    "scheduler_turns": 1231,
                    "virtual_nanoseconds": 1_413_842_550,
                    "syscalls": 612,
                },
                "run2": {
                    "scheduler_turns": 1231,
                    "virtual_nanoseconds": 1_413_842_550,
                    "syscalls": 612,
                },
            },
            "attempts": [self._attempt()],
        }

    def _project(self, cells, *, producer="pressure-test"):
        return series.rows_from_cell_results(
            cells,
            parent=self.parent,
            producer=producer,
            run_id="pressure-one",
            tree=self.tree,
            emitted_at="2026-08-26T10:00:00Z",
        )

    def _validate_cell(self, attempt, outcome="PASS", duration_ms=100):
        cell = self._cell(attempt - 1, outcome=outcome, duration_ms=duration_ms)
        cell.pop("run_index")
        cell["attempt"] = attempt
        return cell

    def _validate_diverged_cell(self, attempt, duration_ms=100):
        cell = self._diverged_cell(
            attempt - 1,
            record=1_718,
            syscall=7,
            scheduler_turn=1_248,
            virtual_nanoseconds=80_000,
            duration_ms=duration_ms,
        )
        cell.pop("run_index")
        cell["attempt"] = attempt
        return cell

    def _validate_no_verdict_cell(self, attempt, *, timed_out, backend):
        cell = self._validate_cell(attempt, outcome="ERROR")
        cell["backend"] = backend
        cell["result"] = None
        cell["failure_class"] = "no_result"
        inner = self._attempt("no_result", timed_out=timed_out)
        if not timed_out:
            inner["error_kind"] = "incomplete-verification-evidence"
            inner["status"] = 125
        cell["attempts"] = [inner]
        # The synthetic repository does not contain a Detcore gitlink. Real
        # retained results omit this field and the producer resolves it from
        # the immutable Hermit tree.
        cell["detcore_tree"] = self.detcore_tree
        return cell

    def _comparison_no_result_cell(self, reason, *, mode="verify"):
        cell = self._validate_no_verdict_cell(1, timed_out=False, backend="ptrace")
        cell["mode"] = mode
        report = json.loads(cell["attempts"][0]["verification_report"])
        report["no_result_reason"] = reason
        self._replace_comparison_report(cell["attempts"][0], report)
        return cell

    @staticmethod
    def _replace_comparison_report(attempt, report):
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        attempt["verification_report"] = raw
        attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()

    def _host_input_attempt(self, index="1"):
        """An attempt as Hermit's runner retains a verify divergence whose
        cause Hermit named outside the guest: the report keeps its comparison
        and divergence position, its verdict is infrastructure_error with a
        host_input_changed reason (hermit-cli/src/canonical_verdict.rs), and
        the runner records an ERROR of kind infrastructure over Hermit's exit
        status 1."""
        attempt = self._attempt(
            "diverged", record=492, syscall=53, scheduler_turn=14,
            virtual_nanoseconds=10, left="read = Ok(6)", right="read = Ok(0)", index=index,
        )
        report = json.loads(attempt["verification_report"])
        identity = {"dev": 48, "ino": 2249086520, "size": 6, "mtime_sec": 1791216972, "mtime_nsec": 5}
        report.update(
            verdict="infrastructure_error",
            infrastructure_error={
                "kind": "host_input_changed",
                "run": "run1",
                "path": "/etc/ld.so.cache",
                "before": identity,
                "after": {**identity, "ino": identity["ino"] + 1},
            },
        )
        self._replace_comparison_report(attempt, report)
        attempt.update(outcome="ERROR", error_kind="infrastructure", status=1)
        return attempt

    def test_a_failure_reports_exact_branch_counter_is_read_as_serde_reads_it(self):
        # Hermit's typed reader reads exact_branch_counter for every verdict,
        # so a no-result or infrastructure-error report is held to it too.
        no_result = self._attempt("no_result")
        no_result.update(error_kind="incomplete-verification-evidence", status=125)
        for label, attempt in (
            ("no_result", no_result),
            ("infrastructure_error", self._host_input_attempt()),
        ):
            expected = series._typed_comparison_attempt(attempt, mode="verify", index=1)
            report = json.loads(attempt["verification_report"])
            for accepted in (
                None,
                {"present": False, "evidence": "GenuineIntel: validation failed"},
                [True, "AuthenticAMD: validation passed"],
            ):
                with self.subTest(verdict=label, accepted=accepted):
                    changed = dict(attempt)
                    self._replace_comparison_report(
                        changed, {**report, "exact_branch_counter": accepted})
                    self.assertEqual(
                        series._typed_comparison_attempt(changed, mode="verify", index=1),
                        expected)
            for refused in (
                {"present": True, "evidence": None},
                {"present": 1, "evidence": "e"},
                {"present": True, "evidence": "e", "measured_counts": 91},
                {"present": True, "evidence": "\ud800"},
                [True],
                [],
            ):
                with self.subTest(verdict=label, refused=refused):
                    changed = dict(attempt)
                    self._replace_comparison_report(
                        changed, {**report, "exact_branch_counter": refused})
                    with self.assertRaisesRegex(series.MalformedRow, "exact_branch_counter"):
                        series._typed_comparison_attempt(changed, mode="verify", index=1)

    def test_a_host_input_change_is_an_errored_attempt_only_when_fully_typed(self):
        attempt = self._host_input_attempt()
        self.assertEqual(series._typed_comparison_attempt(attempt, mode="verify", index=1), "errored")
        report = json.loads(attempt["verification_report"])
        reason = report["infrastructure_error"]
        identity = reason["before"]
        for label, change in (
            ("unknown kind", {"kind": "host_input_drift"}),
            ("unknown run", {"run": "run3"}),
            ("run not a string", {"run": 1}),
            ("empty path", {"path": ""}),
            ("path not a string", {"path": ["/etc/ld.so.cache"]}),
            ("missing after", {"after": None}),
            ("identity missing a field", {"before": {k: v for k, v in identity.items() if k != "mtime_nsec"}}),
            ("identity with an extra field", {"before": {**identity, "mode": 33188}}),
            ("negative device", {"before": {**identity, "dev": -1}}),
            ("inode past u64", {"after": {**identity, "ino": 2**64}}),
            ("size past i64", {"after": {**identity, "size": 2**63}}),
            ("boolean size", {"after": {**identity, "size": True}}),
            ("count instead of a change", {"count": 1}),
        ):
            with self.subTest(label=label):
                changed = dict(attempt)
                self._replace_comparison_report(
                    changed, {**report, "infrastructure_error": {**reason, **change}}
                )
                with self.assertRaisesRegex(series.MalformedRow, "infrastructure_error report"):
                    series._typed_comparison_attempt(changed, mode="verify", index=1)
        for field in ("comparison", "compared_log_messages"):
            with self.subTest(missing=field):
                changed = dict(attempt)
                self._replace_comparison_report(changed, {**report, field: None})
                with self.assertRaisesRegex(series.MalformedRow, "infrastructure_error report"):
                    series._typed_comparison_attempt(changed, mode="verify", index=1)
        for label, change in (("not an ERROR", {"outcome": "FAIL"}), ("timed out", {"timed_out": True})):
            with self.subTest(label=label):
                with self.assertRaises(series.MalformedRow):
                    series._typed_comparison_attempt({**attempt, **change}, mode="verify", index=1)

    def _parity_cell(self):
        # Original run 1838, aio-refusal/KVM attempt 1: two matched repeatability
        # reports and a separate divergent cross-backend comparison. Provenance
        # and original raw-row hashes are retained beside the unchanged row.
        fixture = Path(__file__).with_name("fixtures") / "run1838-backend-parity.json"
        cell = json.loads(fixture.read_text())["row"]
        cell["hermit_sha"] = self.tree
        cell["detcore_tree"] = self.detcore_tree
        return cell

    @staticmethod
    def _make_parity_match(cell):
        cell.update(outcome="PASS", result="pass", failure_class=None)
        parity = cell["backend_parity"]
        parity["verdict"] = "matched"
        comparison = parity["comparison"]
        comparison["verdict"] = "matched"
        comparison["selected_messages"]["left"] = comparison["selected_messages"]["right"]
        for key in (*series.COORDINATE_KEYS, "first_divergent_left_message", "first_divergent_right_message"):
            cell[key] = None
            comparison[key] = None

    def test_backend_parity_validate_projection_preserves_real_red_witness(self):
        cell = self._parity_cell()
        self.assertEqual([
            json.loads(attempt["verification_report"])["verdict"]
            for attempt in cell["attempts"]
        ], ["matched", "matched"])
        projected = self._project([cell], producer="validate")[0]["series"]
        self.assertEqual(projected["outcome"], "diverged")
        self.assertEqual(projected["result"], "parity-failure")
        self.assertEqual(projected["failure_class"], "product_failure")
        self.assertEqual(projected["coordinates"]["first_divergent_record"], 10)
        # No unsupported witness/digest field is invented in the validate schema.
        self.assertNotIn("pressure_evidence", projected)
        self.assertNotIn("no_verdict_evidence", projected)

    def test_backend_parity_match_and_output_only_divergence_are_distinct(self):
        cell = self._parity_cell()
        self._make_parity_match(cell)
        self.assertEqual(self._project([cell], producer="validate")[0]["series"]["outcome"], "passed")
        # Different raw record populations are legal; complete selected streams
        # and exact outputs decide a cross-backend match.
        self.assertNotEqual(cell["backend_parity"]["comparison"]["records"]["available_left"],
                            cell["backend_parity"]["comparison"]["records"]["available_right"])
        operand = cell["backend_parity"]["candidate"]
        operand["output"]["stdout_sha256"] = "a" * 64
        for side in ("left", "right"):
            operand["verification"]["compared_outputs"][side]["stdout_sha256"] = "a" * 64
        self._replace_comparison_report(cell["attempts"][0], operand["verification"])
        cell.update(outcome="FAIL", result="parity-failure", failure_class="product_failure")
        cell["backend_parity"]["verdict"] = "diverged"
        projected = self._project([cell], producer="validate")[0]["series"]
        self.assertEqual(projected["outcome"], "diverged")
        self.assertNotIn("coordinates", projected)

    def test_backend_parity_refuses_missing_and_mismatched_witnesses(self):
        mutations = [
            ("missing witness", lambda c: c.pop("backend_parity")),
            ("null witness", lambda c: c.update(backend_parity=None)),
            ("wrong schema", lambda c: c["backend_parity"].update(schema=2)),
            ("boolean schema", lambda c: c["backend_parity"].update(schema=True)),
            ("wrong verdict", lambda c: c["backend_parity"].update(verdict="matched")),
            ("false green over divergence", lambda c: c.update(outcome="PASS", result="pass", failure_class=None)),
            ("wrong mode", lambda c: c.update(mode="replay")),
            ("wrong candidate", lambda c: c["backend_parity"]["candidate"].update(backend="sabre")),
            ("wrong reference", lambda c: c["backend_parity"]["reference"].update(backend="kvm")),
            ("one operand", lambda c: c["attempts"].pop()),
            ("extra operand", lambda c: c["attempts"].append(c["attempts"][0])),
            ("attempt hash", lambda c: c["attempts"][0].update(verification_report_sha256="f" * 64)),
            ("attempt backend", lambda c: c["attempts"][0]["argv"].__setitem__(
                c["attempts"][0]["argv"].index("--backend") + 1, "sabre")),
            ("embedded operand", lambda c: c["backend_parity"]["candidate"]["verification"][
                "compared_log_messages"].update(left=98)),
            ("operand boolean", lambda c: c["backend_parity"]["candidate"]["verification"][
                "comparison"].update(skip_detlog=0)),
            ("output hash", lambda c: c["backend_parity"]["candidate"]["output"].update(stdout_sha256="bad")),
            ("output identity", lambda c: c["backend_parity"]["candidate"]["output"].update(stdout_sha256="f" * 64)),
            ("output boolean", lambda c: c["backend_parity"]["candidate"]["output"].update(exit_code=False)),
            ("empty retained path", lambda c: c["backend_parity"]["candidate"].update(retained_log="")),
            ("retained hash", lambda c: c["backend_parity"]["candidate"].update(retained_log_sha256="f" * 64)),
            ("input hash", lambda c: c["backend_parity"]["comparison"]["inputs"]["left"].update(sha256="f" * 64)),
            ("input bytes", lambda c: c["backend_parity"]["comparison"]["inputs"]["left"].update(bytes=0)),
            ("outer position", lambda c: c.update(first_divergent_record=11)),
            ("outer message", lambda c: c.update(first_divergent_left_message="different evidence")),
            ("unknown witness field", lambda c: c["backend_parity"].update(unchecked=True)),
        ]
        for label, mutate in mutations:
            with self.subTest(mutation=label):
                cell = self._parity_cell()
                mutate(cell)
                with self.assertRaises(series.MalformedRow):
                    self._project([cell], producer="validate")

    def test_backend_parity_refuses_relaxed_incomplete_or_vacuous_comparison(self):
        mutations = [
            ("comparison schema", lambda c: c.update(schema=2)),
            ("no result", lambda c: c.update(verdict="no_result")),
            ("refusal", lambda c: c.update(refusal="refused")),
            ("follow", lambda c: c.update(follow_stopped_because="deadline")),
            ("no inputs", lambda c: c.pop("inputs")),
            ("tail", lambda c: c["records"].update(withheld_incomplete_tail=True)),
            ("partial records", lambda c: c["records"].update(compared=1)),
            ("zero selection", lambda c: c["selected_messages"].update(left=0)),
            ("excess selection", lambda c: c["selected_messages"].update(left=999)),
            ("boolean counts", lambda c: c["records"].update(compared=True)),
            ("wrong envelope", lambda c: c["comparison"].update(record_envelope="all_records_v1")),
            ("stripping", lambda c: c["comparison"].update(unsafe_strip_lines=True)),
            ("ignored lines", lambda c: c["comparison"].update(ignored_line_substrings=["syscall"])),
            ("skip commit", lambda c: c["comparison"].update(skip_commit=True)),
            ("skip detlog", lambda c: c["comparison"].update(skip_detlog=True)),
            ("no structure", lambda c: c["comparison"].update(require_structured_events=False)),
            ("wrong kinds", lambda c: c["comparison"].update(included_detlog_kinds=["other"])),
            ("wrong stream", lambda c: c["comparison"].update(stream="stdout")),
            ("git diff", lambda c: c["comparison"].update(git_diff=True)),
            ("boolean policy", lambda c: c["comparison"].update(skip_detlog=0)),
        ]
        for label, mutate in mutations:
            with self.subTest(mutation=label):
                cell = self._parity_cell()
                mutate(cell["backend_parity"]["comparison"])
                with self.assertRaises(series.MalformedRow):
                    self._project([cell], producer="validate")

    def test_backend_parity_requires_current_complete_matching_operands(self):
        for change in ("missing outputs", "missing producer field", "different outputs", "relaxed policy",
                       "duplicate report key", "runtime null count"):
            with self.subTest(change=change):
                cell = self._parity_cell()
                attempt = cell["attempts"][0]
                report = json.loads(attempt["verification_report"])
                if change == "missing outputs":
                    report.pop("compared_outputs")
                elif change == "missing producer field":
                    report.pop("no_result_reason")
                elif change == "different outputs":
                    report["compared_outputs"]["right"]["stdout_sha256"] = "f" * 64
                elif change == "relaxed policy":
                    report["comparison"]["skip_detlog"] = True
                elif change == "runtime null count":
                    report["runtime"]["run1"]["scheduler_turns"] = None
                cell["backend_parity"]["candidate"]["verification"] = report
                self._replace_comparison_report(attempt, report)
                if change == "duplicate report key":
                    raw = attempt["verification_report"].replace('"verified":true', '"verified":false,"verified":true')
                    self.assertNotEqual(raw, attempt["verification_report"])
                    attempt["verification_report"] = raw
                    attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()
                with self.assertRaises(series.MalformedRow):
                    self._project([cell], producer="validate")

    def test_backend_parity_identity_uses_typed_serialization(self):
        retained = json.loads((Path(__file__).with_name("fixtures") /
                               "run1838-backend-parity.json").read_text())["row"]
        self.assertEqual(series._cell_result_evidence_sha256(retained),
                         "3770426247f7b20e2f8eebcefb90647b35ae7537bd5aecdd6b7a6173c5e442e2")
        cell = self._parity_cell()
        original = series._cell_result_evidence_sha256(cell)
        parity = cell["backend_parity"]
        # Rust omits these null optional fields and fills omitted nullable fields.
        parity["comparison"].update(refusal=None, follow_stopped_because=None)
        parity["candidate"]["verification"]["dbt_counted_branches"] = None
        parity["candidate"]["verification"].pop("guest_signal")
        parity["candidate"]["output"].pop("signal")
        self.assertEqual(series._cell_result_evidence_sha256(cell), original)
        # The report's retained paths are not rewritten using the invocation cwd.
        parity["candidate"]["retained_log"] = cell["cwd"] + "/retained.log"
        normalized = series._typed_backend_parity(cell, ["matched", "matched"])
        self.assertEqual(normalized["candidate"]["retained_log"], cell["cwd"] + "/retained.log")
        self.assertNotEqual(series._cell_result_evidence_sha256(cell), original)

    def test_backend_parity_identity_carries_the_exact_branch_counter_verdict(self):
        # Shared with hermit's ci/compat-envelope/testdata/branch-counter-parity-row.json:
        # Rust's evidence_identity serializes the operand's typed exact_branch_counter
        # verdict whenever it is present, so the projection must keep it too.
        fixture = json.loads((Path(__file__).with_name("fixtures") /
                              "branch-counter-parity-row.json").read_text())
        expected = "763763fee9e6daf7bfe2bd571ce4c6dfce703878665c1fe68c241280896c87c2"
        self.assertEqual(fixture["provenance"]["evidence_sha256"], expected)
        self.assertEqual(series._cell_result_evidence_sha256(fixture["row"]), expected)
        # The row it was derived from, without the verdict, keeps run 1838's identity.
        retained = json.loads((Path(__file__).with_name("fixtures") /
                               "run1838-backend-parity.json").read_text())["row"]
        self.assertEqual(series._cell_result_evidence_sha256(retained),
                         "3770426247f7b20e2f8eebcefb90647b35ae7537bd5aecdd6b7a6173c5e442e2")

    def test_an_exact_branch_counter_verdict_is_read_as_serde_reads_it(self):
        # The derived deserializer also takes the two-element sequence form,
        # and Rust writes either back as the object, so both hash alike.
        fixture = json.loads((Path(__file__).with_name("fixtures") /
                              "branch-counter-parity-row.json").read_text())

        def with_verdict(verdict):
            row = json.loads(json.dumps(fixture["row"]))
            attempt = row["attempts"][1]
            report = json.loads(attempt["verification_report"])
            report["exact_branch_counter"] = verdict
            attempt["verification_report"] = json.dumps(report, separators=(",", ":")) + "\n"
            attempt["verification_report_sha256"] = hashlib.sha256(
                attempt["verification_report"].encode()).hexdigest()
            row["backend_parity"]["reference"]["verification"]["exact_branch_counter"] = verdict
            return row, report

        evidence = "AuthenticAMD: validation passed — déclaré"
        as_object, object_report = with_verdict({"present": True, "evidence": evidence})
        as_sequence, sequence_report = with_verdict([True, evidence])
        for report in (object_report, sequence_report):
            series._require_retained_comparison_report(report, index=1)
        object_parity = series._typed_backend_parity(
            as_object, ["matched", "matched"])["reference"]["verification"]
        sequence_parity = series._typed_backend_parity(
            as_sequence, ["matched", "matched"])["reference"]["verification"]
        self.assertEqual(object_parity["exact_branch_counter"],
                         {"present": True, "evidence": evidence})
        self.assertEqual(sequence_parity["exact_branch_counter"],
                         object_parity["exact_branch_counter"])

    def test_a_malformed_exact_branch_counter_verdict_is_refused(self):
        fixture = json.loads((Path(__file__).with_name("fixtures") /
                              "branch-counter-parity-row.json").read_text())
        for label, bad in (
            ("an unknown field", {"present": True, "evidence": "e", "measured_counts": 91}),
            ("a non-boolean present", {"present": "yes", "evidence": "e"}),
            ("a non-string evidence", {"present": True, "evidence": 7}),
            ("a missing evidence", {"present": True}),
            # serde_json refuses a string that is not valid UTF-8.
            ("an unpaired surrogate evidence", {"present": True, "evidence": "\ud800"}),
            ("a three-element sequence", [True, "e", 1]),
        ):
            with self.subTest(label=label):
                row = json.loads(json.dumps(fixture["row"]))
                attempt = row["attempts"][1]
                report = json.loads(attempt["verification_report"])
                report["exact_branch_counter"] = bad
                attempt["verification_report"] = json.dumps(report, separators=(",", ":")) + "\n"
                attempt["verification_report_sha256"] = hashlib.sha256(
                    attempt["verification_report"].encode()).hexdigest()
                row["backend_parity"]["reference"]["verification"]["exact_branch_counter"] = bad
                with self.assertRaises(series.MalformedRow):
                    series._cell_result_evidence_sha256(row)
                with self.assertRaises(series.MalformedRow):
                    series._require_retained_comparison_report(report, index=1)

    def test_existing_no_parity_evidence_replay_keeps_original_digest(self):
        fixture = json.loads((Path(__file__).with_name("fixtures") /
                              "run1838-backend-parity.json").read_text())
        for case in fixture["legacy_no_parity_replay"]["cases"]:
            with self.subTest(case=case["name"]):
                stored = case["stored_row"]
                cell = case["input"]
                incoming = json.loads(json.dumps(stored))
                field = ("pressure_evidence" if case["name"] == "pressure"
                         else "no_verdict_evidence")
                for explicit_null in (False, True):
                    current = json.loads(json.dumps(cell))
                    if explicit_null:
                        current["backend_parity"] = None
                    evidence = (series._pressure_evidence(current)
                                if field == "pressure_evidence" else
                                series._no_verdict_evidence(current, stored["series"]["outcome"]))
                    self.assertEqual(evidence, stored["series"][field])
                    incoming["series"][field] = evidence
                self.assertEqual(incoming["event_id"], stored["event_id"])
                target = series.spool_dir(self.parent) / (case["name"] + ".jsonl")
                self.assertEqual(series.append_cell_rows(self.parent, target, [stored]), 1)
                before = target.read_bytes()
                self.assertEqual(series.append_cell_rows(self.parent, target, [incoming]), 0)
                self.assertEqual(target.read_bytes(), before)

    def test_a_replay_with_other_write_time_observations_is_a_no_op(self):
        # pressure-03c24d69a8e7-20261015T004056Z: the run's own emission saw
        # main_ancestry false; its emit-series retry 74 s later saw true. The
        # retry must not quarantine the run's rows.
        cell = self._parity_cell()
        cell["run_index"] = 0
        first = self._project([cell])
        target = series.spool_dir(self.parent) / "write-time.jsonl"
        self.assertEqual(series.append_cell_rows(self.parent, target, first), 1)
        before = target.read_bytes()
        replay = json.loads(json.dumps(first))
        replay[0]["emitted_at"] = "2099-01-01T00:00:00Z"
        replay[0]["series"]["main_ancestry"] = not first[0]["series"]["main_ancestry"]
        depth = dict(first[0]["series"]["depth"])
        depth["reverie"] = {"commits": 99999, "first_parent": 9999}
        replay[0]["series"]["depth"] = depth
        self.assertEqual(series.append_cell_rows(self.parent, target, replay), 0)
        self.assertEqual(target.read_bytes(), before)
        # A field about the run itself still refuses the rewrite, write-time
        # observations changed beside it or not.
        cell["backend_parity"]["candidate"]["retained_log_sha256"] = "f" * 64
        cell["backend_parity"]["comparison"]["inputs"]["right"]["sha256"] = "f" * 64
        changed = self._project([cell])
        self.assertEqual(changed[0]["event_id"], first[0]["event_id"])
        changed[0]["series"]["main_ancestry"] = replay[0]["series"]["main_ancestry"]
        with self.assertRaisesRegex(series.SeriesError, "already records a different row"):
            series.append_cell_rows(self.parent, target, changed)
        self.assertEqual(target.read_bytes(), before)

    def test_backend_parity_changed_witness_refuses_same_pressure_identity(self):
        # Validate PASS/divergence has no durable witness-digest field in the
        # current shared schema. This same-ID guarantee is pressure-specific.
        cell = self._parity_cell()
        cell["run_index"] = 0
        first = self._project([cell])
        target = series.spool_dir(self.parent) / "parity.jsonl"
        self.assertEqual(series.append_cell_rows(self.parent, target, first), 1)
        before = target.read_bytes()
        digest = first[0]["series"]["pressure_evidence"]["evidence_sha256"]
        cell["backend_parity"]["candidate"]["retained_log_sha256"] = "f" * 64
        cell["backend_parity"]["comparison"]["inputs"]["right"]["sha256"] = "f" * 64
        second = self._project([cell])
        self.assertEqual(first[0]["event_id"], second[0]["event_id"])
        self.assertNotEqual(second[0]["series"]["pressure_evidence"]["evidence_sha256"], digest)
        with self.assertRaisesRegex(series.SeriesError, "already records a different row"):
            series.append_cell_rows(self.parent, target, second)
        self.assertEqual(target.read_bytes(), before)

    def test_backend_parity_malformed_json_refuses_before_projection(self):
        cell = self._parity_cell()
        raw = json.dumps(cell)
        for malformed in (
            raw.replace('"backend_parity": {', '"backend_parity": null, "backend_parity": {', 1),
            raw.replace('"backend_parity": {', '"backend_parity": {"invalid": NaN,', 1),
        ):
            with self.subTest(raw=malformed[:40]):
                self.assertNotEqual(malformed, raw)
                with mock.patch.object(sys, "stdin", io.StringIO(malformed)), contextlib.redirect_stderr(io.StringIO()):
                    rows, status = series._stdin_rows()
                self.assertIsNone(rows)
                self.assertEqual(status, 2)

    def _container_failure_cell(self, disposition=None, *, run="run1", mode="verify"):
        reason = {
            "kind": "container_failed", "run": run,
            "disposition": disposition if disposition is not None else {
                "kind": "signaled", "signal": 14, "core_dumped": False,
            },
        }
        cell = self._comparison_no_result_cell(reason, mode=mode)
        cell.update(outcome="FAIL", result="crash-error", failure_class="product_failure")
        cell["attempts"][0].update(
            outcome="FAIL", status=125, signal=None, timed_out=False, error_kind=None,
        )
        return cell

    def test_container_failures_retain_product_classification_and_report_identity(self):
        dispositions = [
            {"kind": "exited", "code": code} for code in (0, 101, 128, 193, 255)
        ] + [
            {"kind": "signaled", "signal": 14, "core_dumped": False},
            {"kind": "signaled", "signal": 64, "core_dumped": True},
        ]
        for producer in ("validate", "pressure-test"):
            for run in ("run1", "run2"):
                for disposition in dispositions:
                    with self.subTest(producer=producer, run=run, disposition=disposition):
                        cell = self._container_failure_cell(disposition, run=run)
                        if producer == "pressure-test":
                            cell["run_index"] = 0
                        original = json.dumps(cell, sort_keys=True)
                        rows = self._project([cell], producer=producer)
                        self.assertEqual(len(rows), 1)
                        payload = rows[0]["series"]
                        self.assertEqual(
                            (payload["outcome"], payload["result"], payload["failure_class"]),
                            ("errored", "crash-error", "product_failure"),
                        )
                        evidence = payload["no_verdict_evidence"]
                        retained = evidence["attempts"][0]
                        self.assertEqual(retained["kind"], "container_failed")
                        self.assertEqual(retained["attempt_outcome"], "FAIL")
                        self.assertEqual(retained["disposition"], "no_result")
                        self.assertEqual(retained["verification_report_sha256"],
                                         cell["attempts"][0]["verification_report_sha256"])
                        self.assertEqual(evidence["evidence_sha256"],
                                         series._cell_result_evidence_sha256(cell))
                        if producer == "pressure-test":
                            self.assertEqual(series._pressure_inner_kinds(payload), ["product"])
                        self.assertEqual(json.dumps(cell, sort_keys=True), original)

    def test_container_failure_rejects_invalid_reason_and_process_evidence(self):
        bad_dispositions = [
            {"kind": "exited", "code": code}
            for code in (-1, 122, 124, 129, 142, 192, 256, True, 1.0, "1")
        ] + [
            {"kind": "signaled", "signal": signal, "core_dumped": False}
            for signal in (0, 65, True, 14.0, "14")
        ] + [
            {"kind": "signaled", "signal": 14},
            {"kind": "signaled", "signal": 14, "core_dumped": 0},
            {"kind": "exited", "code": 1, "signal": 14},
            {"kind": "stopped", "signal": 14},
            None,
        ]
        for disposition in bad_dispositions:
            with self.subTest(disposition=disposition):
                cell = self._container_failure_cell()
                report = json.loads(cell["attempts"][0]["verification_report"])
                report["no_result_reason"]["disposition"] = disposition
                self._replace_comparison_report(cell["attempts"][0], report)
                with self.assertRaises(series.MalformedRow):
                    self._project([self._validate_cell(1), cell], producer="validate")
        for field, value in (
            ("run", "run3"), ("run", []), ("extra", True),
        ):
            cell = self._container_failure_cell()
            report = json.loads(cell["attempts"][0]["verification_report"])
            report["no_result_reason"][field] = value
            self._replace_comparison_report(cell["attempts"][0], report)
            with self.subTest(reason_field=field, value=value), self.assertRaises(series.MalformedRow):
                self._project([cell], producer="validate")
        for field, value in (
            ("outcome", "ERROR"), ("status", 0), ("status", 124),
            ("status", True), ("signal", 14), ("timed_out", True),
            ("error_kind", "incomplete-verification-evidence"),
        ):
            cell = self._container_failure_cell()
            cell["attempts"][0][field] = value
            with self.subTest(attempt_field=field, value=value), self.assertRaises(series.MalformedRow):
                self._project([cell], producer="validate")

    def test_container_failure_rejects_missing_duplicate_and_contradictory_report_fields(self):
        template = json.loads(self._container_failure_cell()["attempts"][0]["verification_report"])
        required = (
            "verified", "bitwise_parity", "verdict", "infrastructure_error",
            "comparison", "no_result_reason", "compared_log_messages", "guest_exit_code",
            "guest_signal", "first_divergent_scheduler_turn", "first_divergent_virtual_nanoseconds",
            "first_divergent_record", "first_divergent_syscall", "first_divergent_left_message",
            "first_divergent_right_message",
        )
        for field in required:
            cell = self._container_failure_cell()
            report = dict(template)
            del report[field]
            self._replace_comparison_report(cell["attempts"][0], report)
            with self.subTest(missing=field), self.assertRaises(series.MalformedRow):
                self._project([cell], producer="validate")
        for field in ("guest_exit_code", "guest_signal", "compared_outputs", "runtime",
                      "dbt_counted_branches", "comparison", "first_divergent_record"):
            cell = self._container_failure_cell()
            report = dict(template)
            report[field] = 1
            self._replace_comparison_report(cell["attempts"][0], report)
            with self.subTest(populated=field), self.assertRaises(series.MalformedRow):
                self._project([cell], producer="validate")
        for needle, replacement in (
            ('"run":"run1"', '"run":"run2","run":"run1"'),
            ('"signal":14', '"signal":14,"signal":14'),
            ('"core_dumped":false', '"core_dumped":false,"core_dumped":false'),
        ):
            cell = self._container_failure_cell()
            attempt = cell["attempts"][0]
            raw = attempt["verification_report"]
            self.assertIn(needle, raw)
            attempt["verification_report"] = raw.replace(needle, replacement, 1)
            attempt["verification_report_sha256"] = hashlib.sha256(
                attempt["verification_report"].encode()
            ).hexdigest()
            with self.subTest(duplicate=needle), self.assertRaisesRegex(series.MalformedRow, "duplicate"):
                self._project([cell], producer="validate")

    def test_pressure_retains_a_container_crash_before_a_successful_retry(self):
        cell = self._container_failure_cell()
        cell["run_index"] = 0
        cell.update(outcome="PASS", result="pass", failure_class=None)
        recovered = self._attempt()
        recovered["index"] = "2"
        cell["attempts"].append(recovered)
        rows = self._project([cell])
        self.assertEqual(rows[0]["series"]["outcome"], "passed")
        self.assertEqual(series._pressure_inner_kinds(rows[0]["series"]), ["product", "pass"])

    def test_container_failures_cannot_qualify_a_pressure_campaign(self):
        for mode in ("verify", "replay", "chaos"):
            with self.subTest(mode=mode):
                cells = []
                for repetition in range(1, 11):
                    cell = self._container_failure_cell(mode=mode)
                    cell["run_index"] = repetition
                    cells.append(cell)
                rows = self._project(cells)
                name = series._cell_name(cells[0])
                result = series.classify_pressure_sample(rows, [name], "pressure-one")[name]
                self.assertEqual(result["classification"], "confirmed-failing")
                self.assertEqual(result["terminal_product_failures"], 10)
                self.assertEqual(result["no_results"], 0)
                self.assertEqual(result["qualifying_passes"], 0)
                self.assertFalse(result["promotion_candidate"])

    def test_container_failure_keeps_report_extensions_but_refuses_non_json_and_bad_hash(self):
        cell = self._container_failure_cell()
        attempt = cell["attempts"][0]
        report = json.loads(attempt["verification_report"])
        report["future_optional_field"] = {"detail": "still a failed container"}
        self._replace_comparison_report(attempt, report)
        self.assertEqual(self._project([cell], producer="validate")[0]["series"]["outcome"],
                         "errored")
        valid_raw = attempt["verification_report"]
        for constant in ("NaN", "Infinity", "-Infinity"):
            attempt["verification_report"] = valid_raw[:-1] + ',"invalid":' + constant + '}'
            attempt["verification_report_sha256"] = hashlib.sha256(
                attempt["verification_report"].encode()
            ).hexdigest()
            with self.subTest(constant=constant), self.assertRaisesRegex(series.MalformedRow, "non-JSON"):
                self._project([cell], producer="validate")
        attempt["verification_report"] = valid_raw
        attempt["verification_report_sha256"] = "f" * 64
        with self.assertRaisesRegex(series.MalformedRow, "hash mismatch"):
            self._project([cell], producer="validate")

    def test_unspecified_and_refused_comparisons_preserve_evidence(self):
        detail = "  second log exceeded its bound: left\\right\nπ  "
        for reason, expected_kind in (
            (None, "unspecified"),
            ({"kind": "comparison_refused", "detail": detail}, "comparison_refused"),
        ):
            for mode in ("verify", "replay", "chaos"):
                for producer in ("validate", "pressure-test"):
                    for signaled in (False, True):
                        with self.subTest(kind=expected_kind, mode=mode,
                                          producer=producer, signaled=signaled):
                            cell = self._comparison_no_result_cell(reason, mode=mode)
                            if producer == "pressure-test":
                                cell["run_index"] = 0
                            attempt = cell["attempts"][0]
                            if signaled:
                                attempt.update(status=None, signal=11)
                            original = json.dumps(cell, sort_keys=True)
                            rows = self._project([cell], producer=producer)
                            self.assertEqual(len(rows), 1)
                            payload = rows[0]["series"]
                            self.assertEqual(payload["outcome"], "no_result")
                            self.assertIsNone(payload["result"])
                            self.assertEqual(payload["failure_class"], "no_result")
                            evidence = payload["no_verdict_evidence"]
                            retained = evidence["attempts"][0]
                            self.assertEqual(retained["kind"], expected_kind)
                            self.assertEqual(retained.get("detail"),
                                             detail if reason is not None else None)
                            for field in ("error_kind", "status", "signal", "timed_out",
                                          "verification_report_sha256"):
                                self.assertEqual(retained[field], attempt[field])
                            self.assertEqual(retained["attempt_outcome"], "ERROR")
                            self.assertEqual(retained["disposition"], "no_result")
                            self.assertEqual(evidence["evidence_sha256"],
                                             series._cell_result_evidence_sha256(cell))
                            if producer == "pressure-test":
                                pressure = payload["pressure_evidence"]
                                self.assertEqual(pressure["evidence_sha256"],
                                                 evidence["evidence_sha256"])
                                self.assertEqual(pressure["attempts"][0]["comparison"], {
                                    "verdict": "no_result",
                                    "canonical": False,
                                    "report_sha256": attempt["verification_report_sha256"],
                                    "no_result_kind": expected_kind,
                                })
                            self.assertEqual(json.dumps(cell, sort_keys=True), original)

    def test_new_no_result_reasons_refuse_incomplete_or_contradictory_evidence(self):
        for reason in (None, {"kind": "comparison_refused", "detail": "truncated log"}):
            for mode in ("verify", "replay", "chaos"):
                cell = self._comparison_no_result_cell(reason, mode=mode)
                original = cell["attempts"][0]
                for mutation in (
                    {"outcome": "PASS"}, {"outcome": "FAIL"},
                    {"error_kind": None}, {"error_kind": " "},
                    {"status": 0}, {"status": None}, {"status": True},
                    {"signal": 11}, {"status": None, "signal": False},
                    {"timed_out": True}, {"timed_out": None},
                    {"verification_report_sha256": "0" * 64},
                ):
                    with self.subTest(reason=reason, mode=mode, mutation=mutation):
                        attempt = {**original, **mutation}
                        with self.assertRaises(series.MalformedRow):
                            series._typed_comparison_attempt(attempt, mode=mode, index=1)
                report = json.loads(original["verification_report"])
                for field, value in (
                    ("verified", True), ("bitwise_parity", True),
                    ("comparison", {}), ("compared_log_messages", {"left": 1, "right": 1}),
                    ("infrastructure_error", {"kind": "skid_overshoot", "count": 1}),
                    ("first_divergent_record", 1),
                    ("guest_exit_code", True), ("guest_exit_code", -1),
                    ("guest_signal", 0), ("guest_signal", 2**31),
                    ("no_result_reason", {"kind": "unknown"}),
                    ("no_result_reason", {"kind": "unspecified"}),
                    ("no_result_reason", {"kind": []}),
                    ("no_result_reason", {"kind": "comparison_refused"}),
                    ("no_result_reason", {"kind": "comparison_refused", "detail": " "}),
                    ("no_result_reason", {"kind": "comparison_refused", "detail": 1}),
                ):
                    with self.subTest(reason=reason, mode=mode, field=field, value=value):
                        attempt = dict(original)
                        self._replace_comparison_report(attempt, {**report, field: value})
                        with self.assertRaises(series.MalformedRow):
                            series._typed_comparison_attempt(attempt, mode=mode, index=1)
                for field in report:
                    with self.subTest(reason=reason, mode=mode, omitted=field):
                        if field == "runtime":
                            continue  # Runtime is not part of the current report contract.
                        omitted = dict(report)
                        del omitted[field]
                        attempt = dict(original)
                        self._replace_comparison_report(attempt, omitted)
                        with self.assertRaises(series.MalformedRow):
                            series._typed_comparison_attempt(attempt, mode=mode, index=1)
                other_error = {**original, "error_kind": "cli-error"}
                if reason is None:
                    self.assertEqual(series._typed_comparison_attempt(
                        other_error, mode=mode, index=1), "no_result")
                else:
                    with self.assertRaises(series.MalformedRow):
                        series._typed_comparison_attempt(other_error, mode=mode, index=1)

    def test_ten_refused_or_unspecified_comparisons_cannot_qualify(self):
        for reason in (None, {"kind": "comparison_refused", "detail": "truncated log"}):
            cells = []
            for index in range(1, 11):
                cell = self._comparison_no_result_cell(reason)
                cell["run_index"] = index
                cells.append(cell)
            rows = self._project(cells)
            name = series._cell_name(cells[0])
            result = series.classify_pressure_sample(rows, [name], "pressure-one")[name]
            self.assertEqual(result["classification"], "no-result")
            self.assertEqual(result["no_results"], 10)
            self.assertEqual(result["terminal_passes"], 0)
            self.assertFalse(result["promotion_candidate"])

    def test_current_no_result_reports_refuse_duplicate_fields(self):
        for reason in (None, {"kind": "comparison_refused", "detail": "truncated log"}):
            attempt = self._comparison_no_result_cell(reason)["attempts"][0]
            raw = attempt["verification_report"]
            encoded_reason = json.dumps(reason, separators=(",", ":"))
            replacements = [
                ('"verified":false', '"verified":true,"verified":false'),
                ('"verified":false', '"verified":false,"verified":false'),
                ('"no_result_reason":',
                 '"no_result_reason":{"kind":"not_run"},"no_result_reason":'),
                ('"no_result_reason":',
                 '"no_result_reason":' + encoded_reason + ',"no_result_reason":'),
            ]
            if reason is not None:
                replacements.extend([
                    ('"detail":"truncated log"', '"detail":" ","detail":"truncated log"'),
                    ('"detail":"truncated log"',
                     '"detail":"truncated log","detail":"truncated log"'),
                ])
            for old, new in replacements:
                with self.subTest(reason=reason, repeated=old, values=new):
                    self.assertEqual(raw.count(old), 1)
                    duplicated = raw.replace(old, new, 1)
                    changed = {**attempt, "verification_report": duplicated,
                               "verification_report_sha256": hashlib.sha256(
                                   duplicated.encode()).hexdigest()}
                    with self.assertRaisesRegex(series.MalformedRow, "duplicate"):
                        series._typed_comparison_attempt(changed, mode="verify", index=1)

    def _diverged_cell(
        self,
        run_index,
        *,
        record,
        syscall,
        scheduler_turn,
        virtual_nanoseconds,
        left="inbound read",
        right="finish read",
        duration_ms=100,
    ):
        cell = self._cell(run_index, outcome="FAIL", duration_ms=duration_ms)
        cell.update({
            "first_divergent_record": record,
            "first_divergent_syscall": syscall,
            "first_divergent_scheduler_turn": scheduler_turn,
            "first_divergent_virtual_nanoseconds": virtual_nanoseconds,
            "first_divergent_left_message": left,
            "first_divergent_right_message": right,
        })
        cell["attempts"] = [self._attempt(
            "diverged",
            record=record,
            syscall=syscall,
            scheduler_turn=scheduler_turn,
            virtual_nanoseconds=virtual_nanoseconds,
            left=left,
            right=right,
        )]
        return cell

    def _fully_typed_report_cell(self, verdict):
        """A matched or diverged cell whose report carries every optional field."""
        if verdict == "matched":
            cell = self._cell(0)
        else:
            cell = self._diverged_cell(
                0, record=98, syscall=37, scheduler_turn=4, virtual_nanoseconds=80_000,
            )
        output = {
            "exit_code": 0 if verdict == "matched" else 1,
            "signal": None,
            "stdout_sha256": "a" * 64,
            "stdout_bytes": 12,
            "stderr_sha256": "b" * 64,
            "stderr_bytes": 0,
        }

        def fill(report):
            report["compared_outputs"] = {
                "left": dict(output),
                "right": dict(output, stdout_sha256=("a" if verdict == "matched" else "c") * 64),
            }
            report["guest_exit_code"] = output["exit_code"]
            report["dbt_counted_branches"] = {"left": 40, "right": 40 if verdict == "matched" else 41}
            report["runtime"] = {
                "run1": {"scheduler_turns": 4, "virtual_nanoseconds": 80_000, "syscalls": 37},
                "run2": {"scheduler_turns": 4, "virtual_nanoseconds": 80_000},
            }
            # serde does not deny unknown top-level report fields.
            report["future_producer_field"] = {"anything": [1, "two"]}
        return self._edit_report(cell, fill)

    def _outputs_cell(self, backend, stdout_sha, *, run2_stdout_sha=None):
        """A validate verify PASS whose report carries both runs' outputs."""
        cell = self._validate_cell(1)
        cell["backend"] = backend
        output = {
            "exit_code": 0, "signal": None,
            "stdout_sha256": stdout_sha, "stdout_bytes": 12,
            "stderr_sha256": "b" * 64, "stderr_bytes": 0,
        }

        def fill(report):
            report["compared_outputs"] = {
                "left": dict(output),
                "right": dict(output, stdout_sha256=run2_stdout_sha or stdout_sha),
            }
            report["guest_exit_code"] = 0
        return self._edit_report(cell, fill)

    def test_a_verify_row_carries_its_divergence_in_all_three_senses(self):
        """append-cells folds into each verify row both runs' log lengths
        (L1), the guest outputs against the same run's ptrace row (L2) and
        the run's parity record for the cell (L3), with the messages cut and
        the log paths dropped (dev-hermit
        ai_docs/2026-10-21-cell-row-first-divergence-design.md)."""
        test_id = "applications/git-repository-workflow"
        ptrace = self._outputs_cell("ptrace", "a" * 64)
        kvm = self._outputs_cell("kvm", "c" * 64)
        long_reference = "INFO detcore::scheduler: " + "r" * 400
        parity = {
            (test_id, "kvm"): {
                "test_id": test_id, "backend": "kvm", "verdict": "diverged",
                "unavailable_class": None, "credit": 0.25, "unequalized_credit": None,
                "first_divergent_record": 5, "left_len": 20, "right_len": 18,
                "matched_prefix": 4,
                "first_difference": {
                    "field": "message", "syscall": None, "scheduler_turn": 3,
                    "virtual_nanoseconds": None,
                    "reference_message": long_reference,
                    "candidate_message": "INFO detcore::random: init",
                },
                "reference_log": "/tmp/ref.log", "candidate_log": "/tmp/cand.log",
            },
            # A ptrace record is never the ptrace row's own L3.
            (test_id, "ptrace"): {"test_id": test_id, "backend": "ptrace", "verdict": "matched"},
        }
        rows = series.rows_from_cell_results(
            [ptrace, kvm], parent=self.parent, producer="validate",
            run_id="validate-divergence", tree=self.tree,
            emitted_at="2026-10-21T12:00:00Z", parity_records=parity,
        )
        series.validate_rows(rows)
        by_backend = {row["series"]["cell"].rsplit("/", 1)[1]: row["series"]["divergence"]
                      for row in rows}
        run1 = {"exit_code": 0, "signal": None, "stderr": "b" * 16 + ":0"}
        self.assertEqual(by_backend["ptrace"], {
            "self": {"left_len": 17, "right_len": 17},
            "outputs": {"run1": {**run1, "stdout": "a" * 16 + ":12"}, "runs_equal": True,
                        "vs_ptrace": "reference"},
        })
        divergence = by_backend["kvm"]
        self.assertEqual(divergence["outputs"], {
            "run1": {**run1, "stdout": "c" * 16 + ":12"}, "runs_equal": True,
            "vs_ptrace": "differs", "vs_ptrace_fields": ["stdout"],
        })
        reference = divergence["vs_ptrace"]["first_difference"].pop("reference")
        self.assertEqual(len(reference), series.DIVERGENCE_MESSAGE_MAX_CHARS)
        self.assertEqual(reference, long_reference[:239] + "…")
        self.assertEqual(divergence["vs_ptrace"], {
            "verdict": "diverged", "unavailable_class": None, "credit": 0.25,
            "unequalized_credit": None, "matched_prefix": 4, "left_len": 20, "right_len": 18,
            "first_difference": {"record": 5, "syscall": None, "scheduler_turn": 3,
                                 "virtual_nanoseconds": None,
                                 "candidate": "INFO detcore::random: init"},
        })
        self.assertNotIn("/tmp/", json.dumps(rows))

        # Equal outputs, no parity record, and a batch with no ptrace row.
        same = self._outputs_cell("kvm", "a" * 64)
        rows = series.rows_from_cell_results(
            [ptrace, same], parent=self.parent, producer="validate",
            run_id="validate-divergence-equal", tree=self.tree,
            emitted_at="2026-10-21T12:00:00Z",
        )
        kvm_row = next(row for row in rows if row["series"]["cell"].endswith("/kvm"))
        self.assertEqual(kvm_row["series"]["divergence"]["outputs"]["vs_ptrace"], "equal")
        self.assertNotIn("vs_ptrace", kvm_row["series"]["divergence"])
        rows = series.rows_from_cell_results(
            [same], parent=self.parent, producer="validate",
            run_id="validate-divergence-alone", tree=self.tree,
            emitted_at="2026-10-21T12:00:00Z",
        )
        self.assertEqual(rows[0]["series"]["divergence"]["outputs"]["vs_ptrace"], "no-reference")

    def test_a_red_candidates_runs_against_the_golden_reach_its_row(self):
        """The post-pass's comparison of each run of a red candidate with the
        ptrace golden (hermit's ParityRecord.runs) is copied beside the
        unscored nondeterministic verdict, messages cut and no paths."""
        test_id = "applications/git-repository-workflow"
        long_candidate = "INFO detcore::random: " + "c" * 400
        parity = {(test_id, "kvm"): {
            "test_id": test_id, "backend": "kvm", "verdict": "nondeterministic",
            "unavailable_class": "determinism-mismatch", "credit": None,
            "unequalized_credit": None, "first_divergent_record": None,
            "left_len": None, "right_len": None, "matched_prefix": None,
            "first_difference": None, "reference_log": "/tmp/golden.log",
            "candidate_log": None,
            "runs": [
                {"run": 1, "verdict": "matched", "first_divergent_record": None,
                 "left_len": 3, "right_len": 3, "matched_prefix": 3,
                 "first_difference": None},
                {"run": 2, "verdict": "diverged", "first_divergent_record": 2,
                 "left_len": 3, "right_len": 3, "matched_prefix": 1,
                 "first_difference": {
                     "field": "token 4", "syscall": 2, "scheduler_turn": None,
                     "virtual_nanoseconds": None,
                     "reference_message": "INFO detcore: read 3",
                     "candidate_message": long_candidate}},
            ],
        }}
        rows = series.rows_from_cell_results(
            [self._outputs_cell("kvm", "c" * 64)], parent=self.parent, producer="validate",
            run_id="validate-divergence-red", tree=self.tree,
            emitted_at="2026-10-22T06:00:00Z", parity_records=parity,
        )
        series.validate_rows(rows)
        vs = rows[0]["series"]["divergence"]["vs_ptrace"]
        self.assertEqual(vs["verdict"], "nondeterministic")
        self.assertIsNone(vs["credit"])
        self.assertNotIn("first_difference", vs)
        self.assertEqual(vs["runs"][0], {
            "run": 1, "verdict": "matched", "matched_prefix": 3, "left_len": 3, "right_len": 3,
        })
        self.assertEqual(vs["runs"][1]["first_difference"], {
            "record": 2, "syscall": 2, "scheduler_turn": None, "virtual_nanoseconds": None,
            "reference": "INFO detcore: read 3", "candidate": long_candidate[:239] + "…",
        })
        self.assertNotIn("/tmp/", json.dumps(rows))

    def test_only_the_row_the_post_pass_compared_carries_its_parity_record(self):
        """A validate node's post-pass reruns with each outer attempt, and a
        pressure test's compares each cell's first repetition."""
        test_id = "applications/git-repository-workflow"
        parity = {(test_id, "kvm"): {
            "test_id": test_id, "backend": "kvm", "verdict": "matched",
            "unavailable_class": None, "credit": 1.0, "unequalized_credit": None,
            "first_divergent_record": None, "left_len": 17, "right_len": 17,
            "matched_prefix": 17, "first_difference": None,
        }}
        attempts = []
        for attempt in (1, 2):
            cell = self._outputs_cell("kvm", "c" * 64)
            cell["attempt"] = attempt
            attempts.append(cell)
        rows = series.rows_from_cell_results(
            attempts, parent=self.parent, producer="validate",
            run_id="validate-divergence-retry", tree=self.tree,
            emitted_at="2026-10-21T12:00:00Z", parity_records=parity,
        )
        self.assertEqual(
            {row["series"]["run_index"]: "vs_ptrace" in row["series"]["divergence"] for row in rows},
            {1: False, 2: True},
        )
        repetitions = []
        for run_index in (1, 0):
            cell = self._cell(run_index)
            cell["backend"] = "kvm"
            repetitions.append(cell)
        rows = series.rows_from_cell_results(
            repetitions, parent=self.parent, producer="pressure-test",
            run_id="pressure-divergence", tree=self.tree,
            emitted_at="2026-10-21T12:00:00Z", parity_records=parity,
        )
        self.assertEqual(
            {row["series"]["run_index"]: "vs_ptrace" in row["series"]["divergence"]
             for row in rows},
            {0: True, 1: False},
        )

    def test_parity_records_repeated_for_a_cell_are_not_guessed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "parity.jsonl"
            path.write_text(
                json.dumps({"test_id": "t", "backend": "kvm", "verdict": "matched"}) + "\n"
                + json.dumps({"test_id": "t", "backend": "kvm", "verdict": "diverged"}) + "\n"
                + json.dumps({"test_id": "u", "backend": "kvm", "verdict": "matched"}) + "\n\n"
            )
            self.assertEqual(
                series.load_parity_records([path]),
                {("u", "kvm"): {"test_id": "u", "backend": "kvm", "verdict": "matched"}},
            )
            path.write_text("[1]\n")
            with self.assertRaises(series.SeriesError):
                series.load_parity_records([path])

    def test_a_fully_typed_comparison_report_projects(self):
        for verdict, outcome in (("matched", "passed"), ("diverged", "diverged")):
            with self.subTest(verdict=verdict):
                rows = self._project([self._fully_typed_report_cell(verdict)])
                self.assertEqual(rows[0]["series"]["outcome"], outcome)
        # Typed but not the canonical policy: still an honest no_result, not a refusal.
        cell = self._edit_report(
            self._cell(0), lambda r: r["comparison"].update(display_name=None, log_scope=None),
        )
        self.assertEqual(
            series._typed_comparison_attempt(cell["attempts"][0], mode="verify", index=1),
            "no_result",
        )

    def test_comparison_reports_refuse_every_field_hermit_cannot_read(self):
        # Authority: VerificationReport::from_retained_cell_json_slice in the
        # pinned hermit-cli/src/canonical_verdict.rs, which Hermit's scorecard
        # uses for cell receipts.
        def drop(field):
            return lambda r: r.pop(field)

        def put(field, value):
            return lambda r: r.__setitem__(field, value)

        def put_in(outer, field, value):
            return lambda r: r[outer].__setitem__(field, value)

        def put_side(side, field, value):
            return lambda r: r["compared_outputs"][side].__setitem__(field, value)

        mutations = [(f"missing {field}", drop(field)) for field in series._RETAINED_REPORT_FIELDS]
        mutations += [
            (f"comparison missing {field}", lambda r, f=field: r["comparison"].pop(f))
            for field in series._COMPARISON_FIELDS
        ]
        mutations += [
            ("verified string", put("verified", "true")),
            ("bitwise_parity integer", put("bitwise_parity", 1)),
            ("no_result_reason present", put("no_result_reason", {"kind": "not_run"})),
            ("infrastructure_error present",
             put("infrastructure_error", {"kind": "skid_overshoot", "count": 1})),
            ("comparison not an object", put("comparison", ["canonical"])),
            ("strictness unknown", put_in("comparison", "strictness", "bitwise")),
            ("display_name integer", put_in("comparison", "display_name", 7)),
            ("compare_logs null", put_in("comparison", "compare_logs", None)),
            ("record_envelope null", put_in("comparison", "record_envelope", None)),
            ("record_envelope unknown", put_in("comparison", "record_envelope", "all_records_v2")),
            ("log_scope unknown", put_in("comparison", "log_scope", "everything")),
            ("stripped_prefixes string", put_in("comparison", "stripped_prefixes", "x")),
            ("canonicalizations of integers", put_in("comparison", "canonicalizations", [1])),
            ("counts string", put("compared_log_messages", {"left": "17", "right": 17})),
            ("counts past u64", put("compared_log_messages", {"left": 2**64, "right": 17})),
            ("counts missing right", put("compared_log_messages", {"left": 17})),
            ("dbt strings", put("dbt_counted_branches", {"left": "bad", "right": "bad"})),
            ("dbt missing right", put("dbt_counted_branches", {"left": 40})),
            ("dbt negative", put("dbt_counted_branches", {"left": -1, "right": 40})),
            ("dbt past u64", put("dbt_counted_branches", {"left": 2**64, "right": 40})),
            ("dbt boolean", put("dbt_counted_branches", {"left": True, "right": 40})),
            ("dbt list", put("dbt_counted_branches", [40, 40])),
            ("runtime string count",
             lambda r: r["runtime"]["run1"].__setitem__("scheduler_turns", "4")),
            ("outputs missing right", lambda r: r["compared_outputs"].pop("right")),
            ("outputs unknown side", lambda r: r["compared_outputs"].__setitem__("middle", None)),
            ("output unknown field", put_side("left", "stdin_bytes", 0)),
            ("output negative bytes", put_side("left", "stdout_bytes", -1)),
            ("output string exit", put_side("right", "exit_code", "0")),
            ("output integer digest", put_side("right", "stderr_sha256", 7)),
            ("guest_exit_code string", put("guest_exit_code", "0")),
            ("guest_exit_code past i32", put("guest_exit_code", 2**31)),
            ("guest_exit_code float", put("guest_exit_code", 1.0)),
            ("guest_signal string", put("guest_signal", "SIGSEGV")),
        ]
        for coordinate in series.COORDINATE_KEYS:
            for label, value in (("string", "bad"), ("negative", -1), ("past u64", 2**64),
                                 ("float", 1.5), ("boolean", True)):
                mutations.append((f"{coordinate} {label}", put(coordinate, value)))
        for side in series.FIRST_DIVERGENT_MESSAGE_KEYS:
            field = f"first_divergent_{side}_message"
            for label, value in (("integer", 7), ("list", ["x"]), ("lone surrogate", "\ud800")):
                mutations.append((f"{field} {label}", put(field, value)))
        # An unhashable value where a string enum belongs must refuse as a
        # malformed row, not escape as a TypeError from set membership.
        for label, value in (("array", ["canonical"]), ("object", {"canonical": True})):
            for field in ("strictness", "log_scope", "record_envelope"):
                mutations.append((f"{field} {label}", put_in("comparison", field, value)))
            mutations.append((f"verdict {label}", put("verdict", value)))
        for verdict in ("matched", "diverged"):
            for label, mutate in mutations:
                with self.subTest(verdict=verdict, mutation=label):
                    cell = self._edit_report(self._fully_typed_report_cell(verdict), mutate)
                    with self.assertRaises(series.MalformedRow):
                        self._project([cell])

    def test_comparison_report_refuses_the_reviewed_malformed_pass(self):
        # The reviewer's reproduction on a PASS: an omitted producer field and
        # two DBT/coordinate fields Hermit cannot type, each refused on its own.
        edits = {
            "no_result_reason omitted": lambda r: r.pop("no_result_reason"),
            "dbt_counted_branches strings":
                lambda r: r.__setitem__("dbt_counted_branches", {"left": "bad", "right": "bad"}),
            "first_divergent_record string":
                lambda r: r.__setitem__("first_divergent_record", "bad"),
        }
        for label, edit in [*edits.items(), ("all three", lambda r: [e(r) for e in edits.values()])]:
            with self.subTest(edit=label), self.assertRaisesRegex(
                series.MalformedRow, "not Hermit's typed retained report"
            ):
                self._project([self._edit_report(self._cell(0), edit)])

    def test_two_hundred_raw_run_ids_become_one_batch_row(self):
        cells = [self._cell(i, duration_ms=5_100 + i) for i in range(200)]
        # Ordinary repeats still compress; pressure keeps individual evidence.
        rows = self._project(cells, producer="hermit-repeat")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["run_id"], "pressure-one")
        self.assertEqual(rows[0]["event_type"], "series.observation")
        self.assertEqual(rows[0]["producer"], "hermit-repeat")
        payload = rows[0]["series"]
        self.assertEqual(payload["run_index"], 0)
        self.assertEqual(payload["num_runs"], 200)
        self.assertEqual(payload["runtime"]["wall_time_min_ms"], 5_100)
        self.assertEqual(payload["runtime"]["wall_time_max_ms"], 5_299)
        self.assertEqual(payload["runtime"]["run1"]["syscalls"], 612)

        pressure = self._project(cells)
        self.assertEqual(len(pressure), 200)
        self.assertEqual({item["run_id"] for item in pressure}, {"pressure-one"})
        self.assertEqual({item["producer"] for item in pressure}, {"pressure-test"})
        self.assertEqual(len({item["event_id"] for item in pressure}), 200)
        self.assertEqual(len({item["series"]["pressure_evidence"]["evidence_sha256"]
                              for item in pressure}), 200)
        for index, item in enumerate(pressure):
            payload = item["series"]
            self.assertEqual(payload["run_index"], index)
            self.assertEqual(payload["attempt"], 1)
            self.assertEqual(payload["num_runs"], 1)
            self.assertEqual(payload["runtime"]["wall_time_min_ms"], 5_100 + index)
            self.assertEqual(payload["runtime"]["wall_time_max_ms"], 5_100 + index)
            self.assertEqual(payload["runtime"]["run1"]["syscalls"], 612)
            retained = payload["pressure_evidence"]["attempts"]
            self.assertEqual(len(retained), 1)
            self.assertEqual(retained[0]["comparison"]["report_sha256"],
                             cells[index]["attempts"][0]["verification_report_sha256"])

    def test_different_host_capability_sets_are_not_compressed_together(self):
        first = self._cell(0)
        second = self._cell(1)
        second["host_capabilities"]["cpuid-faulting"] = {
            "present": True,
            "evidence": "fixture presence",
        }
        rows = self._project([first, second])
        self.assertEqual(len(rows), 2)
        self.assertEqual(
            {
                row["series"]["host_capabilities"]["cpuid-faulting"]["present"]
                for row in rows
            },
            {False, True},
        )

    def test_two_hundred_runs_keep_two_exact_divergence_points_and_counts(self):
        cells = []
        for index in range(200):
            choice = index % 20
            if choice < 3:
                cells.append(self._diverged_cell(
                    index,
                    record=1_718,
                    syscall=7,
                    scheduler_turn=1_248,
                    virtual_nanoseconds=80_000,
                    duration_ms=4_000 + index,
                ))
            elif choice < 8:
                cells.append(self._diverged_cell(
                    index,
                    record=1_976,
                    syscall=7,
                    scheduler_turn=1_248,
                    virtual_nanoseconds=80_000,
                    duration_ms=4_000 + index,
                ))
            else:
                cells.append(self._cell(index, duration_ms=4_000 + index))

        rows = self._project(cells, producer="hermit-repeat")
        self.assertEqual(len(rows), 3)
        by_coordinates = {
            json.dumps(item["series"].get("coordinates"), sort_keys=True): item["series"]
            for item in rows
        }
        first = by_coordinates[json.dumps({
            "first_divergent_record": 1_718,
            "first_divergent_syscall": 7,
            "first_divergent_scheduler_turn": 1_248,
            "first_divergent_virtual_nanoseconds": 80_000,
        }, sort_keys=True)]
        second = by_coordinates[json.dumps({
            "first_divergent_record": 1_976,
            "first_divergent_syscall": 7,
            "first_divergent_scheduler_turn": 1_248,
            "first_divergent_virtual_nanoseconds": 80_000,
        }, sort_keys=True)]
        passed = by_coordinates["null"]
        self.assertEqual(first["num_runs"], 30)
        self.assertEqual(second["num_runs"], 50)
        self.assertEqual(passed["num_runs"], 120)
        self.assertEqual(first["coordinates"]["first_divergent_record"], 1_718)
        self.assertEqual(second["coordinates"]["first_divergent_record"], 1_976)
        self.assertEqual(first["first_divergent_messages"], {
            "left": "inbound read",
            "right": "finish read",
        })
        self.assertEqual(first["run_index"], 0)
        self.assertEqual(first["last_run_index"], 182)
        self.assertLess(len("\n".join(json.dumps(item) for item in rows)), 10_000)

        pressure = self._project(cells)
        self.assertEqual(len(pressure), 200)
        self.assertEqual(sum(item["series"]["outcome"] == "passed" for item in pressure), 120)
        for record, count in ((1_718, 30), (1_976, 50)):
            located = [item for item in pressure
                       if item["series"].get("coordinates", {}).get("first_divergent_record") == record]
            self.assertEqual(len(located), count)
            self.assertTrue(all(item["series"]["num_runs"] == 1 for item in located))
        self.assertEqual(series.observed_range(pressure, "first_divergent_record"),
                         series.observed_range(rows, "first_divergent_record"))

    def test_scattered_positions_remain_distinct_with_the_same_messages(self):
        cells = [
            self._diverged_cell(
                index,
                record=1_500 + index,
                syscall=7,
                scheduler_turn=1_248,
                virtual_nanoseconds=80_000,
            )
            for index in range(200)
        ]
        rows = self._project(cells, producer="hermit-repeat")
        self.assertEqual(len(rows), 200)
        self.assertEqual(
            len({row["series"]["coordinates"]["first_divergent_record"] for row in rows}),
            200,
        )
        self.assertTrue(all(row["series"]["num_runs"] == 1 for row in rows))
        self.assertLess(len("\n".join(json.dumps(item) for item in rows)), 300_000)

    def test_same_coordinates_with_different_messages_remain_distinct(self):
        coordinates = {
            "record": 1_718,
            "syscall": 7,
            "scheduler_turn": 1_248,
            "virtual_nanoseconds": 80_000,
        }
        rows = self._project([
            self._diverged_cell(0, **coordinates),
            self._diverged_cell(
                1,
                **coordinates,
                left="inbound write",
                right="finish write",
            ),
        ])
        self.assertEqual(len(rows), 2)

    def test_unmet_prerequisite_has_no_row_but_measured_zero_stays_zero(self):
        not_run = self._cell(0, outcome="HOST-INAPPLICABLE")
        not_run.pop("duration_ms")
        not_run["runtime"] = None
        self.assertEqual(self._project([not_run]), [])

        measured = self._cell(0, duration_ms=0)
        measured["runtime"] = None
        zero = self._project([measured])[0]["series"]["runtime"]
        self.assertEqual(zero["wall_time_min_ms"], 0)
        self.assertEqual(zero["wall_time_max_ms"], 0)

        for key, bad in (
            ("duration_ms", "0"),
            ("runtime", "not-an-object"),
            ("schema", 3),
            ("source_tree_dirty", "false"),
        ):
            malformed = self._cell(0)
            malformed[key] = bad
            with self.assertRaises(series.MalformedRow, msg=f"accepted malformed {key}"):
                self._project([malformed])

    def test_failed_retry_then_success_is_not_compressed_away(self):
        rows = series.rows_from_cell_results(
            [
                self._validate_diverged_cell(1, duration_ms=90),
                self._validate_cell(2, outcome="PASS", duration_ms=80),
            ],
            parent=self.parent,
            producer="validate",
            run_id="validate-one",
            tree=self.tree,
            emitted_at="2026-08-26T10:00:00Z",
        )
        self.assertEqual(
            [
                (
                    r["series"]["run_index"],
                    r["series"]["outcome"],
                    r["series"]["result"],
                    r["series"]["failure_class"],
                )
                for r in rows
            ],
            [
                (1, "diverged", "determinism-failure", "product_failure"),
                (2, "passed", "pass", None),
            ],
        )

    def test_validate_no_verdict_rows_keep_attempt_and_typed_disposition(self):
        timeout = self._validate_no_verdict_cell(
            1, timed_out=True, backend="sabre"
        )
        root = "/tmp/fixture/hermit"
        sibling = f"{root}-backup"
        embedded = f"--hermit={root}/target/debug/hermit"
        path_value = f"/usr/bin:{root}/bin"
        interior = f"prefix{root}/target/debug/hermit"
        timeout.update({
            "cwd": root,
            "argv": [
                f"{root}/target/debug/hermit",
                root,
                embedded,
                sibling,
                interior,
            ],
            "guest_argv": [f"{root}/guest"],
            "effective_args": [f"{root}/config", "π"],
            "env": {
                "HOME": f"{root}/home",
                "PATH": path_value,
                "SIBLING": sibling,
                "INTERIOR": interior,
            },
            "shell_command": "stale",
        })
        timeout["attempts"][0].update({
            "cwd": root,
            "argv": [f"{root}/target/debug/hermit"],
            "guest_argv": [f"{root}/guest"],
            "env": {"HOME": f"{root}/home"},
            "shell_command": "stale",
        })
        normalised = series._normalise_cell_result_for_evidence(timeout)
        self.assertEqual(normalised["cwd"], series.RECORDED_ROOT)
        self.assertEqual(
            normalised["argv"][:2],
            [f"{series.RECORDED_ROOT}/target/debug/hermit", series.RECORDED_ROOT],
        )
        self.assertEqual(
            normalised["argv"][2],
            f"--hermit={series.RECORDED_ROOT}/target/debug/hermit",
        )
        self.assertEqual(normalised["env"]["HOME"], f"{series.RECORDED_ROOT}/home")
        self.assertEqual(
            normalised["env"]["PATH"], f"/usr/bin:{series.RECORDED_ROOT}/bin"
        )
        self.assertEqual(normalised["argv"][3:], [sibling, interior])
        self.assertEqual(normalised["env"]["SIBLING"], sibling)
        self.assertEqual(normalised["env"]["INTERIOR"], interior)
        self.assertEqual(
            normalised["attempts"][0]["cwd"], series.RECORDED_ROOT
        )
        self.assertEqual(
            normalised["attempts"][0]["shell_command"],
            "cd /repo && env HOME=/repo/home /repo/target/debug/hermit",
        )
        encoded = json.dumps(normalised, sort_keys=True, ensure_ascii=False)
        self.assertEqual(
            series._replace_evidence_root_token(encoded, root, series.RECORDED_ROOT),
            encoded,
        )
        for delimiter in (
            " ", "\t", "\n", "\r", "\v", "\f", "\u00a0", "\u2003",
        ):
            self.assertEqual(
                series._replace_evidence_root_token(
                    f"{delimiter}{root}/nested", root, series.RECORDED_ROOT
                ),
                f"{delimiter}{series.RECORDED_ROOT}/nested",
            )
        for codepoint in range(0x1C, 0x20):
            control = chr(codepoint)
            value = f"{control}{root}/nested"
            self.assertEqual(
                series._replace_evidence_root_token(
                    value, root, series.RECORDED_ROOT
                ),
                value,
            )
        utf8_contract = json.loads(json.dumps(timeout))
        utf8_contract["hermit_sha"] = "a" * 40
        # Preserve the existing ordinary-row digest. Rust also hashes a null
        # backend_parity here; that older cross-language mismatch is unchanged.
        self.assertEqual(
            series._cell_result_evidence_sha256(utf8_contract),
            "6de0ee5d7b0c18ed4cf1bbd2c41f1df75e7d688ec59995c86cf6250e7c0b3096",
        )
        unavailable = self._validate_no_verdict_cell(
            2, timed_out=False, backend="kvm"
        )
        rows = series.rows_from_cell_results(
            [timeout, unavailable],
            parent=self.parent,
            producer="validate",
            run_id="validate-no-verdict",
            tree=self.tree,
            emitted_at="2026-08-30T20:00:00Z",
        )
        self.assertEqual(len(rows), 2)
        self.assertEqual(
            sorted([
                (
                    row["series"]["run_index"],
                    row["series"]["attempt"],
                    row["series"]["no_verdict_evidence"]["attempts"][0]["kind"],
                    row["series"]["no_verdict_evidence"]["attempts"][0]["attempt_outcome"],
                    row["series"]["no_verdict_evidence"]["attempts"][0]["disposition"],
                    row["series"]["no_verdict_evidence"]["attempts"][0]["timed_out"],
                )
                for row in rows
            ]),
            [
                (1, 1, "not_run", "ERROR", "timeout", True),
                (2, 2, "not_run", "ERROR", "no_result", False),
            ],
        )
        by_attempt = {row["series"]["attempt"]: row for row in rows}
        self.assertEqual(
            by_attempt[1]["series"]["no_verdict_evidence"]["evidence_sha256"],
            series._cell_result_evidence_sha256(timeout),
        )
        self.assertEqual(
            by_attempt[2]["series"]["no_verdict_evidence"]["evidence_sha256"],
            series._cell_result_evidence_sha256(unavailable),
        )
        self.assertNotEqual(rows[0]["event_id"], rows[1]["event_id"])
        self.assertEqual(
            [row["series"]["detcore_tree"] for row in rows],
            [self.detcore_tree, self.detcore_tree],
        )

        prelaunch = self._validate_no_verdict_cell(
            1, timed_out=True, backend="ptrace"
        )
        prelaunch["attempts"][0]["status"] = None
        prelaunch["attempts"][0]["signal"] = None
        recovered = self._validate_cell(2, outcome="PASS")
        recovered["detcore_tree"] = self.detcore_tree
        recovered_rows = series.rows_from_cell_results(
            [prelaunch, recovered],
            parent=self.parent,
            producer="validate",
            run_id="validate-recovered-prelaunch-not-run",
            tree=self.tree,
            emitted_at="2026-08-31T09:00:00Z",
        )
        self.assertEqual(
            [row["series"]["outcome"] for row in recovered_rows],
            ["no_result", "passed"],
        )
        prelaunch_disposition = recovered_rows[0]["series"][
            "no_verdict_evidence"
        ]["attempts"][0]
        self.assertEqual(
            (
                prelaunch_disposition["kind"],
                prelaunch_disposition["attempt_outcome"],
                prelaunch_disposition["disposition"],
                prelaunch_disposition["status"],
                prelaunch_disposition["signal"],
                prelaunch_disposition["timed_out"],
            ),
            ("not_run", "ERROR", "timeout", None, None, True),
        )

        for label, mutation in (
            ("not timed out", {"timed_out": False}),
            ("wrong error kind", {"error_kind": "infrastructure"}),
        ):
            malformed = json.loads(json.dumps(prelaunch))
            malformed["attempts"][0].update(mutation)
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow,
                "process or pre-launch timeout disposition",
            ):
                series.rows_from_cell_results(
                    [malformed],
                    parent=self.parent,
                    producer="validate",
                    run_id=f"validate-malformed-prelaunch-{label}",
                    tree=self.tree,
                    emitted_at="2026-08-31T09:00:00Z",
                )

        noncanonical = self._validate_cell(3, outcome="ERROR")
        noncanonical["result"] = None
        noncanonical["failure_class"] = "no_result"
        noncanonical["detcore_tree"] = self.detcore_tree
        report = json.loads(noncanonical["attempts"][0]["verification_report"])
        report["comparison"]["display_name"] = "Stripped"
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        noncanonical["attempts"][0]["verification_report"] = raw
        noncanonical["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        weak_row = series.rows_from_cell_results(
            [noncanonical],
            parent=self.parent,
            producer="validate",
            run_id="validate-noncanonical",
            tree=self.tree,
            emitted_at="2026-08-30T20:00:00Z",
        )[0]
        disposition = weak_row["series"]["no_verdict_evidence"]["attempts"][0]
        self.assertEqual(disposition["kind"], "noncanonical_match")
        self.assertEqual(disposition["attempt_outcome"], "PASS")
        self.assertEqual(disposition["disposition"], "no_result")

        inconsistent_match = json.loads(json.dumps(noncanonical))
        report = json.loads(
            inconsistent_match["attempts"][0]["verification_report"]
        )
        report["verified"] = False
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        inconsistent_match["attempts"][0]["verification_report"] = raw
        inconsistent_match["attempts"][0]["verification_report_sha256"] = (
            hashlib.sha256(raw.encode()).hexdigest()
        )
        with self.assertRaisesRegex(
            series.MalformedRow, "matched report is internally inconsistent"
        ):
            series.rows_from_cell_results(
                [inconsistent_match],
                parent=self.parent,
                producer="validate",
                run_id="validate-noncanonical-bad-match",
                tree=self.tree,
                emitted_at="2026-08-30T20:00:00Z",
            )

        noncanonical_divergence = json.loads(json.dumps(noncanonical))
        attempt = noncanonical_divergence["attempts"][0]
        report = json.loads(attempt["verification_report"])
        report["verdict"] = "diverged"
        report["verified"] = False
        report["bitwise_parity"] = False
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        attempt.update({
            "outcome": "FAIL",
            "status": 1,
            "verification_report": raw,
            "verification_report_sha256": hashlib.sha256(raw.encode()).hexdigest(),
        })
        divergence_row = series.rows_from_cell_results(
            [noncanonical_divergence],
            parent=self.parent,
            producer="validate",
            run_id="validate-noncanonical-divergence",
            tree=self.tree,
            emitted_at="2026-08-30T20:00:00Z",
        )[0]
        self.assertEqual(
            divergence_row["series"]["no_verdict_evidence"]["attempts"][0]["kind"],
            "noncanonical_divergence",
        )
        inconsistent_divergence = json.loads(json.dumps(noncanonical_divergence))
        attempt = inconsistent_divergence["attempts"][0]
        report = json.loads(attempt["verification_report"])
        report["verified"] = True
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        attempt["verification_report"] = raw
        attempt["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        with self.assertRaisesRegex(
            series.MalformedRow, "divergence report is internally inconsistent"
        ):
            series.rows_from_cell_results(
                [inconsistent_divergence],
                parent=self.parent,
                producer="validate",
                run_id="validate-noncanonical-bad-divergence",
                tree=self.tree,
                emitted_at="2026-08-30T20:00:00Z",
            )

        passing = series.rows_from_cell_results(
            [self._validate_cell(1)],
            parent=self.parent,
            producer="validate",
            run_id="validate-pass",
            tree=self.tree,
            emitted_at="2026-08-30T20:00:00Z",
        )[0]
        self.assertNotIn("attempt", passing["series"])
        self.assertNotIn("no_verdict_evidence", passing["series"])

    def _expected_exit_cell(
        self, *, status, signal, guest_exit_code, guest_signal, declared,
        argv=None,
    ):
        """A matched verify cell whose guest ended nonzero, as run 213 wrote it.

        ``declared`` is ``(code, signal)`` for the manifest's
        ``expected_guest_exit``, or ``None`` for the row shape the runner
        produced before it carried the declaration.
        """
        cell = self._validate_cell(1)
        attempt = cell["attempts"][0]
        attempt["argv"] = argv if argv is not None else [
            "/src/hermit", "--log", "info", "run", "--strict", "--verify-strict",
            "--verify", "--verify-allow=failure", "--verify-json", "/results/verify-1.json",
            "--", "/results/fixtures/program",
        ]
        attempt["status"] = status
        attempt["signal"] = signal
        report = json.loads(attempt["verification_report"])
        report["guest_exit_code"] = guest_exit_code
        report["guest_signal"] = guest_signal
        report["compared_outputs"] = {
            side: {
                "exit_code": guest_exit_code,
                "signal": guest_signal,
                "stdout_sha256": "a" * 64,
                "stdout_bytes": 12,
                "stderr_sha256": "b" * 64,
                "stderr_bytes": 0,
            }
            for side in ("left", "right")
        }
        self._replace_comparison_report(attempt, report)
        if declared is not None:
            cell["test"] = self._declared_test_id(declared)
            cell["expected_guest_exit"] = {
                "code": declared[0],
                "signal": declared[1],
                "reason": self.FIXTURE_EXIT_REASON,
            }
        return cell

    @staticmethod
    def _declared_test_id(declared):
        """The setUp manifest id whose entry makes ``declared``, when one does."""
        code, signal = declared
        if type(code) is int and signal is None:
            return f"fixture/exit-{code}"
        if code is None and type(signal) is int:
            return f"fixture/signal-{signal}"
        return f"fixture/declared-{code}-{signal}"

    def _project_validate(self, cell, label):
        rows = series.rows_from_cell_results(
            [cell],
            parent=self.parent,
            producer="validate",
            run_id=f"validate-expected-exit-{label}",
            tree=self.tree,
            emitted_at="2026-09-25T12:00:00Z",
        )
        series.validate_rows(rows)
        return rows

    OBSERVED_STDOUT = "vdso ok 42\n"
    DECLARED_STDOUT = "getrandom ok\n"

    @staticmethod
    def _stdout_identity(text):
        encoded = text.encode()
        return len(encoded), hashlib.sha256(encoded).hexdigest()

    def _exact_stdout_reason(self, run="first", declared=None):
        """The text Hermit's ``expected_stdout_mismatch_reason`` writes."""
        observed_bytes, observed_sha = self._stdout_identity(self.OBSERVED_STDOUT)
        declared_bytes, declared_sha = self._stdout_identity(
            self.DECLARED_STDOUT if declared is None else declared
        )
        return (
            f"verify {run} run stdout ({observed_bytes} bytes, sha256 {observed_sha}) "
            f"differs from the declared expected_stdout ({declared_bytes} bytes, "
            f"sha256 {declared_sha})"
        )

    def _contains_stdout_reason(self, quoted):
        """The text Hermit's ``expected_stdout_contains_mismatch_reason`` writes.

        ``quoted`` is spelled as Rust's ``{:?}`` renders the declared text.
        """
        observed_bytes, observed_sha = self._stdout_identity(self.OBSERVED_STDOUT)
        return (
            f"verify stdout ({observed_bytes} bytes, sha256 {observed_sha}) does not "
            f"contain the declared expected_stdout_contains text {quoted}"
        )

    def _expected_output_failed_cell(self, reason, *, status=0, stdout=None):
        """A matched verify attempt the runner FAILed for its declared stdout.

        Both compared runs printed ``OBSERVED_STDOUT``. The runner checks the
        declared stdout after the canonical comparison matched and before the
        guest's exit, so the attempt carries no timeout, no error kind, and
        the guest's own status, and the row is crash-error.
        """
        cell = self._validate_cell(1)
        cell.update(outcome="FAIL", result="crash-error", failure_class="product_failure")
        # The synthetic repository has no Detcore gitlink; see
        # _validate_no_verdict_cell.
        cell["detcore_tree"] = self.detcore_tree
        attempt = cell["attempts"][0]
        attempt["outcome"] = "FAIL"
        attempt["status"] = status
        attempt["reason"] = reason
        if stdout is not None:
            attempt["stdout"] = stdout
        observed_bytes, observed_sha = self._stdout_identity(self.OBSERVED_STDOUT)
        report = json.loads(attempt["verification_report"])
        # Hermit's require_exact_output_match: the report's guest disposition
        # is the one its compared outputs record.
        report["guest_exit_code"] = status
        report["guest_signal"] = None
        report["compared_outputs"] = {
            side: {
                "exit_code": status,
                "signal": None,
                "stdout_sha256": observed_sha,
                "stdout_bytes": observed_bytes,
                "stderr_sha256": hashlib.sha256(b"").hexdigest(),
                "stderr_bytes": 0,
            }
            for side in ("left", "right")
        }
        self._replace_comparison_report(attempt, report)
        return cell

    def test_matched_attempt_failed_by_declared_stdout_is_retained_as_failed_match(self):
        # The random-sources-root-only rows quarantined in hermit_test_ledger
        # d4402b7 are this case: both runs agree, exit 0, and print other
        # stdout than the cell declares.
        tab_and_quotes = self._contains_stdout_reason('"getrandom\\t\\"ok\\""')
        cases = (
            ("exact stdout, exit 0", self._expected_output_failed_cell(
                self._exact_stdout_reason())),
            ("exact stdout, exit 3", self._expected_output_failed_cell(
                self._exact_stdout_reason(), status=3)),
            ("contained text, capture recorded", self._expected_output_failed_cell(
                tab_and_quotes, stdout=self.OBSERVED_STDOUT)),
            # Hermit records a capture it cannot read as UTF-8 as empty.
            ("contained text, capture not text", self._expected_output_failed_cell(
                tab_and_quotes, stdout="")),
            ("contained text, no capture", self._expected_output_failed_cell(
                self._contains_stdout_reason('"getrandom"'))),
            # The shared Rust checker rebuilds the reason with the runner's own
            # formatter, so it is the oracle for this reader's ASCII rendering.
            ("contained text, every ASCII character", self._expected_output_failed_cell(
                self._contains_stdout_reason(
                    series._rust_debug_ascii("".join(map(chr, range(128))))
                ))),
            # An exact claim is decided from the report alone; the capture,
            # even another one, is not what the runner compared.
            ("exact stdout, other capture", self._expected_output_failed_cell(
                self._exact_stdout_reason(), stdout="something else\n")),
        )
        for label, cell in cases:
            with self.subTest(label=label):
                reason = cell["attempts"][0]["reason"]
                row = self._project_validate(cell, label.replace(" ", "-").replace(",", ""))[0]
                payload = row["series"]
                self.assertEqual(
                    (payload["outcome"], payload["result"], payload["failure_class"]),
                    ("errored", "crash-error", "product_failure"),
                )
                attempt = cell["attempts"][0]
                self.assertEqual(payload["no_verdict_evidence"]["attempts"], [{
                    "index": "1",
                    "kind": "failed_match",
                    "detail": reason,
                    "attempt_outcome": "FAIL",
                    "disposition": "errored",
                    "error_kind": None,
                    "status": attempt["status"],
                    "signal": None,
                    "timed_out": False,
                    "verification_report_sha256": attempt["verification_report_sha256"],
                }])

        # Declared, the runner still checks stdout first, so a guest that
        # ended exactly as declared is failed by its stdout all the same.
        declared = self._expected_exit_cell(
            status=7, signal=None, guest_exit_code=7, guest_signal=None, declared=(7, None),
        )
        declared.update(outcome="FAIL", result="crash-error", failure_class="product_failure")
        declared["detcore_tree"] = self.detcore_tree
        attempt = declared["attempts"][0]
        attempt["outcome"] = "FAIL"
        # _expected_exit_cell's compared runs printed 12 bytes of sha256 a...a.
        attempt["reason"] = (
            f"verify first run stdout (12 bytes, sha256 {'a' * 64}) differs from the "
            f"declared expected_stdout (13 bytes, sha256 {'c' * 64})"
        )
        row = self._project_validate(declared, "declared-exit-7-stdout")[0]
        self.assertEqual(
            row["series"]["no_verdict_evidence"]["attempts"][0]["detail"],
            attempt["reason"],
        )
        self.assertEqual(row["series"]["declared_guest_exit"]["attempts"][0]["outcome"], "FAIL")

    def test_failed_declared_stdout_claims_the_evidence_contradicts_are_refused(self):
        exact = self._exact_stdout_reason()
        contains = self._contains_stdout_reason('"getrandom"')

        def edited(reason=exact, *, status=0, stdout=None, edit=None):
            cell = self._expected_output_failed_cell(reason, status=status, stdout=stdout)
            if edit is not None:
                edit(cell)
            return cell

        def other_report_stdout(cell):
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            for side in ("left", "right"):
                report["compared_outputs"][side]["stdout_sha256"] = "e" * 64
            self._replace_comparison_report(attempt, report)

        def unequal_runs(cell):
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report["compared_outputs"]["right"]["stderr_bytes"] = 1
            self._replace_comparison_report(attempt, report)

        def no_compared_outputs(cell):
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            del report["compared_outputs"]
            self._replace_comparison_report(attempt, report)

        def chaos(cell):
            cell["mode"] = "chaos"

        def with_error_kind(cell):
            cell["attempts"][0]["error_kind"] = "wall-timeout"

        def no_disposition(cell):
            cell["attempts"][0]["status"] = None

        def determinism_failure(cell):
            cell["result"] = "determinism-failure"

        def beside_a_pass(cell):
            # Only one supporting attempt is required, so without its own
            # refusal a pass row would silently drop the failed match.
            cell.update(outcome="PASS", result="pass", failure_class=None)
            cell["attempts"].append(self._attempt(index="2"))

        contradicts = "reason claims a failed declared expected output that contradicts its evidence"
        unrecognized = "matched report is internally inconsistent"
        for label, cell, expected in (
            ("the second run of a matched pair",
             edited(self._exact_stdout_reason(run="second")),
             f"{contradicts}: it names the second run"),
            ("declared stdout that is the observed stdout",
             edited(self._exact_stdout_reason(declared=self.OBSERVED_STDOUT)),
             f"{contradicts}: the declared stdout is the observed stdout"),
            ("the report printed other stdout", edited(edit=other_report_stdout),
             f"{contradicts}: both compared runs printed"),
            ("the contained reason names other stdout", edited(contains, edit=other_report_stdout),
             f"{contradicts}: both compared runs printed"),
            # Refused as malformed evidence before its reason is read.
            ("unequal compared runs", edited(edit=unequal_runs),
             "matched report is internally inconsistent: left and right compared outputs differ"),
            ("no compared outputs", edited(edit=no_compared_outputs),
             f"{contradicts}: the matched report has no identical"),
            ("a chaos cell", edited(edit=chaos),
             f"{contradicts}: only a verify or replay cell declares expected stdout"),
            ("an error kind", edited(edit=with_error_kind),
             f"{contradicts}: the runner fails declared stdout only"),
            ("no process disposition", edited(edit=no_disposition),
             f"{contradicts}: the runner fails declared stdout only"),
            ("an empty declared text", edited(self._contains_stdout_reason('""')),
             f"{contradicts}: the runner finds an empty declared text"),
            ("a capture that contains the text",
             edited(self._contains_stdout_reason('"vdso ok"'), stdout=self.OBSERVED_STDOUT),
             f"{contradicts}: the captured stdout contains the declared text"),
            ("a capture that is other bytes", edited(contains, stdout="vdso ok 43\n"),
             f"{contradicts}: the captured stdout is not the stdout the reason names"),
            ("a capture that is not a string", edited(contains, stdout=["vdso"]),
             f"{contradicts}: the captured stdout is not text"),
            ("a failed match under a determinism-failure result",
             edited(edit=determinism_failure),
             "supports only a crash-error result"),
            ("a failed match beside a passing attempt", edited(edit=beside_a_pass),
             "supports only a crash-error result"),
            # Texts the runner does not write, or that this reader cannot
            # prove it wrote, keep the refusal the attempt had before.
            ("another reason", edited("the guest printed something unexpected"), unrecognized),
            ("trailing text", edited(exact + " "), unrecognized),
            ("a text Rust would escape", edited(self._contains_stdout_reason('"a\tb"')), unrecognized),
            ("a non-ASCII text", edited(self._contains_stdout_reason('"café"')), unrecognized),
            ("a zero-padded count", edited(exact.replace("stdout (11 bytes", "stdout (011 bytes")),
             unrecognized),
            ("no reason", edited(None), unrecognized),
        ):
            with self.subTest(label=label):
                with self.assertRaisesRegex(series.MalformedRow, re.escape(expected)):
                    self._project_validate(cell, "refused")

        # A pressure row carries the failed match as a match that is not
        # canonical, and the row stays the crash-error red: never a pass.
        pressure = edited()
        pressure["run_index"] = 0
        pressure.pop("attempt")
        [projected] = self._project([pressure])
        payload = projected["series"]
        self.assertEqual((payload["outcome"], payload["result"]), ("errored", "crash-error"))
        [inner] = payload["pressure_evidence"]["attempts"]
        self.assertEqual((inner["outcome"], inner["error_kind"]), ("FAIL", None))
        self.assertEqual(
            (inner["comparison"]["verdict"], inner["comparison"]["canonical"]), ("matched", False)
        )

    def test_declared_expected_guest_exit_admits_exactly_the_declared_disposition(self):
        # The three run-213 rows, and the 128 + signo spelling of a signal
        # death that Hermit uses when it cannot re-raise the guest's signal.
        for label, status, signal, guest_code, guest_signal, declared in (
            ("qemu-hello exit 7", 7, None, 7, None, (7, None)),
            ("pmu-skid exit 3", 3, None, 3, None, (3, None)),
            ("nanosleep signal 11", None, 11, None, 11, (None, 11)),
            ("signal 11 as status 139", 139, None, None, 11, (None, 11)),
        ):
            with self.subTest(label=label):
                cell = self._expected_exit_cell(
                    status=status, signal=signal, guest_exit_code=guest_code,
                    guest_signal=guest_signal, declared=declared,
                )
                row = self._project_validate(cell, label.replace(" ", "-"))[0]
                self.assertEqual(row["series"]["outcome"], "passed")
                # The same row without its declaration is the run-213 batch
                # failure, and must stay refused.
                cell.pop("expected_guest_exit")
                with self.assertRaisesRegex(
                    series.MalformedRow, "matched report is internally inconsistent"
                ):
                    self._project_validate(cell, label.replace(" ", "-") + "-undeclared")

    def test_expected_guest_exit_refuses_every_unauthorised_or_contradicted_failure(self):
        inconsistent = "matched report is internally inconsistent"

        def cell(**overrides):
            fields = dict(
                status=7, signal=None, guest_exit_code=7, guest_signal=None,
                declared=(7, None),
            )
            fields.update(overrides)
            return self._expected_exit_cell(**fields)

        def signal_cell(**overrides):
            fields = dict(
                status=None, signal=11, guest_exit_code=None, guest_signal=11,
                declared=(None, 11),
            )
            fields.update(overrides)
            return self._expected_exit_cell(**fields)

        def report_edit(target, edit):
            attempt = target["attempts"][0]
            report = json.loads(attempt["verification_report"])
            edit(report)
            self._replace_comparison_report(attempt, report)
            return target

        def declaration_edit(target, **fields):
            target["expected_guest_exit"].update(fields)
            return target

        hermit_argv = [
            "/src/hermit", "run", "--strict", "--verify-strict", "--verify",
            "--verify-json", "/results/verify-1.json",
        ]
        cases = [
            ("declared 7, observed 3",
             cell(status=3, guest_exit_code=3), inconsistent),
            ("declared signal 6, observed signal 11",
             signal_cell(declared=(None, 6)), inconsistent),
            ("report guest_signal disagrees with the attempt",
             report_edit(signal_cell(), lambda r: r.update(guest_signal=6)), inconsistent),
            ("report guest_exit_code disagrees with the attempt",
             report_edit(cell(), lambda r: r.update(guest_exit_code=3)), inconsistent),
            ("right compared output disagrees",
             report_edit(signal_cell(), lambda r: r["compared_outputs"]["right"].update(signal=6)),
             inconsistent),
            ("left compared output disagrees",
             report_edit(cell(), lambda r: r["compared_outputs"]["left"].update(exit_code=3)),
             inconsistent),
            ("compared outputs absent",
             report_edit(cell(), lambda r: r.pop("compared_outputs")), inconsistent),
            ("no --verify-allow=failure, declaration present",
             cell(argv=[*hermit_argv, "--", "/results/fixtures/program"]), inconsistent),
            ("--verify-allow=failure only in guest argv",
             cell(argv=[*hermit_argv, "--", "/results/fixtures/program",
                        "--verify-allow=failure"]), inconsistent),
            ("--verify-allow=failure but no declaration",
             cell(declared=None), inconsistent),
            ("signal death with --verify-allow=failure but no declaration",
             signal_cell(declared=None), inconsistent),
            ("declared failure but the guest succeeded",
             cell(status=0, guest_exit_code=0), inconsistent),
            ("declared signal, Hermit exited with an unrelated status",
             signal_cell(status=11, signal=None), inconsistent),
            ("boolean status",
             cell(status=True, guest_exit_code=1, declared=(1, None)), inconsistent),
            ("declaration names both code and signal",
             cell(declared=(7, 11)), "exactly one of code or signal"),
            ("declaration names neither",
             cell(declared=(None, None)), "exactly one of code or signal"),
            ("declared code 0", cell(declared=(0, None)), "code must be a nonzero"),
            ("declared code 256", cell(declared=(256, None)), "code must be a nonzero"),
            ("declared boolean code", cell(declared=(True, None)), "code must be a nonzero"),
            ("declared signal 0", signal_cell(declared=(None, 0)), "signal must be a signal"),
            ("declared signal 65", signal_cell(declared=(None, 65)), "signal must be a signal"),
            ("blank reason", declaration_edit(cell(), reason="  "), "reason must be substantive"),
            ("unknown declaration field",
             declaration_edit(cell(), note="x"), "exactly code, signal and reason"),
        ]
        # The complete canonical output shape, and left == right: a declared
        # failure's outputs must support the match, not only name the exit.
        def output_edit(side, **fields):
            return report_edit(cell(), lambda r: r["compared_outputs"][side].update(fields))

        def output_drop(side, field):
            return report_edit(cell(), lambda r: r["compared_outputs"][side].pop(field))

        for side in ("left", "right"):
            for stream in ("stdout", "stderr"):
                cases += [
                    (f"{side} {stream} hash differs",
                     output_edit(side, **{f"{stream}_sha256": "c" * 64}), inconsistent),
                    (f"{side} {stream} byte count differs",
                     output_edit(side, **{f"{stream}_bytes": 13}), inconsistent),
                    (f"{side} {stream} hash omitted",
                     output_drop(side, f"{stream}_sha256"), inconsistent),
                    (f"{side} {stream} byte count omitted",
                     output_drop(side, f"{stream}_bytes"), inconsistent),
                ]
            cases += [
                (f"{side} exit_code omitted", output_drop(side, "exit_code"), inconsistent),
                (f"{side} signal omitted", output_drop(side, "signal"), inconsistent),
                (f"{side} unknown output field", output_edit(side, note="x"), inconsistent),
            ]
        for label, fields in (
            ("uppercase stdout hash", {"stdout_sha256": "A" * 64}),
            ("short stderr hash", {"stderr_sha256": "b" * 63}),
            ("non-hex stdout hash", {"stdout_sha256": "g" * 64}),
            ("negative stdout byte count", {"stdout_bytes": -1}),
            ("boolean stderr byte count", {"stderr_bytes": False}),
            ("string stdout byte count", {"stdout_bytes": "12"}),
            ("fractional stderr byte count", {"stderr_bytes": 0.0}),
            ("string exit_code", {"exit_code": "7"}),
            ("unknown output field", {"note": "x"}),
        ):
            cases.append((
                f"both outputs carry {label}",
                report_edit(cell(), lambda r, fields=fields: [
                    r["compared_outputs"][side].update(fields) for side in ("left", "right")
                ]),
                inconsistent,
            ))
        # One side only: Python's == equates 12 with 12.0 and 1 with True, so
        # equality with a well-typed left side cannot type the right one.
        for side in ("left", "right"):
            cases += [
                (f"{side} alone has fractional stdout byte count",
                 output_edit(side, stdout_bytes=12.0), inconsistent),
                (f"{side} alone has fractional exit_code",
                 output_edit(side, exit_code=7.0), inconsistent),
            ]
        cases += [
            ("right alone has boolean stderr byte count",
             report_edit(cell(), lambda r: (
                 r["compared_outputs"]["left"].update(stderr_bytes=1),
                 r["compared_outputs"]["right"].update(stderr_bytes=True),
             )), inconsistent),
            ("right alone has boolean exit_code for declared 1",
             report_edit(
                 cell(status=1, guest_exit_code=1, declared=(1, None)),
                 lambda r: r["compared_outputs"]["right"].update(exit_code=True),
             ), inconsistent),
        ]
        cases.append((
            "unknown compared_outputs member",
            report_edit(cell(), lambda r: r["compared_outputs"].update(
                middle=r["compared_outputs"]["left"])),
            inconsistent,
        ))

        # Duplicate fields in the original report bytes. The last copy is the
        # consistent one, so only the duplicate refusal can catch each.
        def duplicated(target, needle, earlier):
            attempt = target["attempts"][0]
            raw = attempt["verification_report"]
            self.assertEqual(raw.count(needle), 1, needle)
            raw = raw.replace(needle, earlier + "," + needle)
            attempt["verification_report"] = raw
            attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()
            return target

        cases += [
            ("duplicate guest_exit_code",
             duplicated(cell(), '"guest_exit_code":7', '"guest_exit_code":3'),
             "duplicate field 'guest_exit_code'"),
            ("duplicate verdict",
             duplicated(cell(), '"verdict":"matched"', '"verdict":"diverged"'),
             "duplicate field 'verdict'"),
            ("duplicate compared_outputs",
             duplicated(cell(), '"compared_outputs":', '"compared_outputs":null'),
             "duplicate field 'compared_outputs'"),
        ]
        dup_output = cell()
        raw = dup_output["attempts"][0]["verification_report"]
        right = raw.index('"right":{')
        stdout = raw.index('"stdout_sha256":', right)
        raw = raw[:stdout] + '"stdout_sha256":"' + "c" * 64 + '",' + raw[stdout:]
        dup_output["attempts"][0]["verification_report"] = raw
        dup_output["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()).hexdigest()
        cases.append(("duplicate right stdout_sha256", dup_output,
                      "duplicate field 'stdout_sha256'"))

        null_declaration = cell()
        null_declaration["expected_guest_exit"] = None
        cases.append(("null declaration", null_declaration, "exactly code, signal and reason"))
        for mode in ("chaos", "naked"):
            other = cell()
            other["mode"] = mode
            cases.append((f"declaration in {mode} mode", other, "valid only on a verify or replay cell"))
        # A replay row may carry its verify recipe's declaration (hermit
        # b94f9215), but only from a record start compared with
        # --verify-allow=both; this fixture's verify invocation is refused.
        replay = cell()
        replay["mode"] = "replay"
        cases.append(("declaration in replay mode from a verify invocation", replay,
                      "without a record start compared with --verify-allow=both"))
        for label, target, message in cases:
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow, message
            ):
                self._project_validate(target, "mutant")

    # Hermit's ci/compat-envelope/scorecard.rs `ordinary()` fixture. Its
    # Rust ResultRow identities below are pinned by that file's
    # evidence_identity_tests; equality here is cross-language parity.
    _SCORECARD_ORDINARY_ROW = r'''{"argv":["hermit","run","--verify","fixture"],"attempt":1,"attempts":[{"argv":["hermit","run","--verify","fixture"],"cwd":"/repo","env":{"LC_ALL":"C"},"error_kind":"incomplete-verification-evidence","guest_argv":["fixture"],"index":"1","outcome":"ERROR","shell_command":"cd /repo && env LC_ALL=C hermit run --verify fixture","signal":null,"status":75,"timed_out":false,"verification_report":"{\"bitwise_parity\":false,\"compared_log_messages\":null,\"comparison\":null,\"first_divergent_left_message\":null,\"first_divergent_record\":null,\"first_divergent_right_message\":null,\"first_divergent_scheduler_turn\":null,\"first_divergent_syscall\":null,\"first_divergent_virtual_nanoseconds\":null,\"guest_exit_code\":null,\"guest_signal\":null,\"infrastructure_error\":null,\"no_result_reason\":{\"kind\":\"not_run\"},\"runtime\":null,\"verdict\":\"no_result\",\"verified\":false}","verification_report_sha256":"0cf756d63a02c73f989586d9f6572c428ce52d55a510ffa4659baab4bdcef405"}],"backend":"ptrace","binary_sha256":"0f533a26257a88f0550ddbabdb318a47f029991cfcf743714ce7f6ee3243105b","category":"fixture","classification":"required","cwd":"/repo","effective_args":["run","--verify","fixture"],"env":{"LC_ALL":"C"},"execution_cpu_timeout_seconds":10,"execution_wall_timeout_seconds":15,"failure_class":"no_result","guest_argv":["fixture"],"hermit_sha":"e6a1657c5cbe966d24f70ae88151471ca3fbd362","lane":"portable","log_level":"info","mode":"verify","outcome":"ERROR","relaxations":[],"result":null,"run_id":"finalized-a","schema":4,"shell_command":"cd /repo && env LC_ALL=C hermit run --verify fixture","source_tree_dirty":false,"test":"fixture/no-result","test_sha256":"f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d","timeout_seconds":15}'''
    _SCORECARD_ORDINARY = "9bf1be90807ed0c84ae4592759239ad264779bd962a220542cca372d2151f032"
    _SCORECARD_DECLARED = {
        (7, None, "the fixture guest fails on purpose"):
            "0953b8206ec868fc6c4d2466b5877d479eb18a26a75eb4750f032ff11be6ad0b",
        (None, 11, "the fixture guest fails on purpose"):
            "330611d0f1cefdc37b2dc6534b4178cf2bc1f73713c2fc2517ad3dc9429ebef6",
        (7, None, "d\u00e9clar\u00e9 \u2014 on purpose"):
            "32fab131e58b1532412f050c8a4f48e6bb9e44e0286cc207c55009c1709d5b4c",
    }

    def _runner_red(self, cell):
        """The runner's own red for a matched comparison: attempt FAIL inside
        a crash-error/product_failure row (runner.rs, the matched branch)."""
        cell["attempts"][0]["outcome"] = "FAIL"
        cell.update(outcome="FAIL", result="crash-error", failure_class="product_failure")
        return cell

    def test_matched_attempt_the_runner_failed_is_retained_red_beside_a_valid_sibling(self):
        """A declared cell that no longer ends as declared, or an undeclared
        verify that exited nonzero, is kept as a red row. It must not refuse
        the batch: the valid sibling still projects as passed, exactly as the
        pinned Hermit scorecard retains the same row."""
        for label, red in (
            ("declared 7, observed 0", self._expected_exit_cell(
                status=0, signal=None, guest_exit_code=0, guest_signal=None,
                declared=(7, None))),
            ("declared 7, observed 5", self._expected_exit_cell(
                status=5, signal=None, guest_exit_code=5, guest_signal=None,
                declared=(7, None))),
            ("declared signal 11, observed exit 3", self._expected_exit_cell(
                status=3, signal=None, guest_exit_code=3, guest_signal=None,
                declared=(None, 11))),
            ("undeclared verify exited 3", self._expected_exit_cell(
                status=3, signal=None, guest_exit_code=3, guest_signal=None,
                declared=None)),
        ):
            with self.subTest(label=label):
                red = self._runner_red(red)
                red["test"] = red["test"] + "-red"
                sibling = self._validate_cell(1)
                rows = series.rows_from_cell_results(
                    [sibling, red],
                    parent=self.parent,
                    producer="validate",
                    run_id="validate-runner-red-" + label.replace(" ", "-").replace(",", ""),
                    tree=self.tree,
                    emitted_at="2026-09-25T12:00:00Z",
                )
                series.validate_rows(rows)
                outcomes = sorted(row["series"]["outcome"] for row in rows)
                self.assertEqual(outcomes, ["errored", "passed"], label)
                (red_row,) = [row for row in rows if row["series"]["outcome"] == "errored"]
                # The red is retained as typed evidence, never as a verdict.
                (evidence,) = red_row["series"]["no_verdict_evidence"]["attempts"]
                self.assertEqual(evidence["kind"], "failed_match", label)
                self.assertEqual(evidence["disposition"], "errored", label)
                self.assertEqual(evidence["attempt_outcome"], "FAIL", label)
                self.assertIs(evidence["timed_out"], False, label)
                self.assertIsNone(evidence["error_kind"], label)
                self.assertEqual(red_row["series"]["result"], "crash-error", label)
                if red.get("expected_guest_exit") is not None:
                    self.assertEqual(
                        red_row["series"]["declared_guest_exit"]["code"],
                        red["expected_guest_exit"]["code"],
                    )

    def test_matched_attempt_the_runner_failed_by_its_exit_retains_no_detail(self):
        """The runner writes a reason for every FAIL. For a match that did not
        end as its row allows, the reason names the exit, not a failed
        declared stdout, and Hermit's shared checker admits a failed_match
        detail only as the latter. The evidence carries no detail, and the
        shared checker admits the row."""
        for label, red, reason in (
            ("declared 7, observed 0", self._expected_exit_cell(
                status=0, signal=None, guest_exit_code=0, guest_signal=None,
                declared=(7, None)),
             "guest ended with exit code 0, but the manifest expects exit code 7"),
            ("declared 7, observed 5", self._expected_exit_cell(
                status=5, signal=None, guest_exit_code=5, guest_signal=None,
                declared=(7, None)),
             "guest ended with exit code 5, but the manifest expects exit code 7"),
            ("declared signal 11, observed exit 3", self._expected_exit_cell(
                status=3, signal=None, guest_exit_code=3, guest_signal=None,
                declared=(None, 11)),
             "guest ended with exit code 3, but the manifest expects signal 11"),
            ("undeclared verify exited 3", self._expected_exit_cell(
                status=3, signal=None, guest_exit_code=3, guest_signal=None,
                declared=None),
             "verify exited with status 3"),
        ):
            with self.subTest(label=label):
                red = self._runner_red(red)
                red["attempts"][0]["reason"] = reason
                (row,) = self._project_validate(
                    red, "runner-red-reason-" + label.replace(" ", "-").replace(",", "")
                )
                self.assertEqual(row["series"]["outcome"], "errored", label)
                (evidence,) = row["series"]["no_verdict_evidence"]["attempts"]
                self.assertEqual(evidence["kind"], "failed_match", label)
                self.assertNotIn("detail", evidence, label)

    def test_matched_attempt_the_runner_failed_refuses_its_contradictions(self):
        """Retention is for the runner's red alone. A FAIL that in fact ended
        as its row allows, a divergence coordinate, a passing or
        comparison-failure row, or a FAIL carrying a timeout or error kind
        without being the runner's timeout is evidence it cannot write."""
        def red(**overrides):
            fields = dict(status=5, signal=None, guest_exit_code=5, guest_signal=None,
                          declared=(7, None))
            fields.update(overrides)
            return self._runner_red(self._expected_exit_cell(**fields))

        def edit(target, fn):
            fn(target)
            return target

        def report_coordinate(target):
            attempt = target["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report["first_divergent_record"] = 12
            self._replace_comparison_report(attempt, report)

        def report_message(target):
            attempt = target["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report["first_divergent_left_message"] = "INFO detcore: result=1"
            self._replace_comparison_report(attempt, report)

        cases = [
            ("FAIL that ended as declared",
             red(status=7, guest_exit_code=7), "ends as its row allows"),
            ("undeclared FAIL that exited 0",
             red(status=0, guest_exit_code=0, declared=None), "ends as its row allows"),
            ("divergence coordinate in the report",
             edit(red(), report_coordinate), "carries a divergence coordinate"),
            ("divergence coordinate on the attempt",
             edit(red(), lambda c: c["attempts"][0].update(first_divergent_syscall=4)),
             "carries a divergence coordinate"),
            ("divergence coordinate on the row",
             edit(red(), lambda c: c.update(first_divergent_scheduler_turn=4)),
             "carries a divergence coordinate"),
            ("first-divergent message in the report",
             edit(red(), report_message), "carries a divergence coordinate"),
            ("first-divergent message on the attempt",
             edit(red(), lambda c: c["attempts"][0].update(
                 first_divergent_right_message="INFO detcore: result=2")),
             "carries a divergence coordinate"),
            ("retained in a PASS row",
             edit(red(), lambda c: c.update(outcome="PASS", result="pass", failure_class=None)),
             "records result 'pass'"),
            ("retained in a determinism-failure row",
             edit(red(), lambda c: c.update(result="determinism-failure")),
             "records result 'determinism-failure'"),
            ("FAIL carrying an error kind",
             edit(red(), lambda c: c["attempts"][0].update(error_kind="wall-timeout")),
             "matched report is internally inconsistent"),
            ("FAIL marked timed out without the runner's timeout row",
             edit(red(), lambda c: c["attempts"][0].update(timed_out=True)),
             "matched report is internally inconsistent"),
            ("ERROR attempt",
             edit(red(), lambda c: c["attempts"][0].update(outcome="ERROR")),
             "matched report is internally inconsistent"),
        ]
        for label, cell, message in cases:
            with self.subTest(label=label):
                with self.assertRaisesRegex(series.MalformedRow, message):
                    self._project_validate(cell, label.replace(" ", "-"))

    def test_report_numbers_and_runtime_are_typed_as_hermit_reads_them(self):
        """A report Hermit's typed reader refuses is refused, declared or not:
        a non-finite number (1e999 parses to infinity in Python), a runtime of
        the wrong type, and an integer past Python's digit limit."""
        def declared():
            return self._expected_exit_cell(
                status=7, signal=None, guest_exit_code=7, guest_signal=None,
                declared=(7, None),
            )

        def undeclared():
            return self._validate_cell(1)

        def with_runtime(target, runtime):
            attempt = target["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report["runtime"] = runtime
            self._replace_comparison_report(attempt, report)
            return target

        def with_raw(target, member):
            attempt = target["attempts"][0]
            raw = attempt["verification_report"]
            raw = raw[:-1] + "," + member + "}"
            attempt["verification_report"] = raw
            attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()
            return target

        valid = {"run1": {"scheduler_turns": 12, "virtual_nanoseconds": 34, "syscalls": 5},
                 "run2": {"scheduler_turns": 13, "virtual_nanoseconds": 35}}
        for make in (declared, undeclared):
            label = make.__name__
            for runtime in (None, valid, {}, {"run1": None}):
                with self.subTest(label=label, runtime=runtime):
                    rows = self._project_validate(with_runtime(make(), runtime), f"rt-{label}")
                    self.assertEqual(rows[0]["series"]["outcome"], "passed")
            typed = "runtime is not Hermit's typed runtime"
            cases = [
                ("runtime integer", with_runtime(make(), 5), typed),
                ("runtime string", with_runtime(make(), "x"), typed),
                ("runtime array", with_runtime(make(), []), typed),
                ("run1 string", with_runtime(make(), {"run1": "x"}), typed),
                ("turns omitted", with_runtime(make(), {"run1": {"virtual_nanoseconds": 1}}), typed),
                ("turns negative",
                 with_runtime(make(), {"run1": {"scheduler_turns": -1, "virtual_nanoseconds": 1}}),
                 typed),
                ("turns fractional",
                 with_runtime(make(), {"run1": {"scheduler_turns": 1.0, "virtual_nanoseconds": 1}}),
                 typed),
                ("nanoseconds boolean",
                 with_runtime(make(), {"run2": {"scheduler_turns": 1, "virtual_nanoseconds": True}}),
                 typed),
                ("nanoseconds past u64",
                 with_runtime(make(), {"run2": {"scheduler_turns": 1,
                                                "virtual_nanoseconds": 2**64}}),
                 typed),
                ("syscalls string",
                 with_runtime(make(), {"run1": {"scheduler_turns": 1, "virtual_nanoseconds": 1,
                                                "syscalls": "5"}}),
                 typed),
                ("runtime 1e999", with_raw(make(), '"runtime_probe":1e999'), "out-of-range number"),
                ("runtime -1e999", with_raw(make(), '"runtime_probe":-1e999'), "out-of-range number"),
                ("5000-digit integer", with_raw(make(), '"runtime_probe":' + "1" * 5000),
                 "verification report is malformed"),
            ]
            for case, target, message in cases:
                with self.subTest(label=label, case=case), self.assertRaisesRegex(
                    series.MalformedRow, message
                ):
                    self._project_validate(target, f"bad-{label}")

    def test_a_lone_surrogate_is_a_named_refusal_not_a_crash(self):
        """serde_json refuses a lone surrogate; Python accepts it and then
        cannot encode it. The identity hash and both stdin readers refuse it
        by name."""
        # A declared reason must equal its manifest entry's, so a surrogate
        # there is refused by that binding before any hash is taken.
        in_reason = self._declared_exit_7()
        in_reason["expected_guest_exit"]["reason"] = "fails \ud800 on purpose"
        with self.assertRaisesRegex(series.MalformedRow, "disagrees with its manifest entry"):
            self._project_validate(in_reason, "surrogate-reason")
        # category is hashed and nothing else reads it, so this surrogate
        # reaches the identity hash.
        target = self._declared_exit_7()
        target["category"] = "fails \ud800 on purpose"
        with self.assertRaisesRegex(series.MalformedRow, "not valid Unicode"):
            self._project_validate(target, "surrogate")
        for label, line, error_type in (
            ("surrogate", '{"test": "\\ud800"}', "UnicodeEncodeError"),
            ("5000-digit integer", '{"attempt": ' + "1" * 5000 + "}", "ValueError"),
        ):
            # Plain append refuses its batch; append-cells quarantines the line.
            with self.subTest(label=label, reader="append"), \
                    mock.patch.object(series.sys, "stdin", io.StringIO(line + "\n")), \
                    contextlib.redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(series._stdin_rows(), (None, 2))
                self.assertIn("stdin line 1 is not JSON", stderr.getvalue())
            with self.subTest(label=label, reader="append-cells"), \
                    mock.patch.object(series.sys, "stdin", io.StringIO(line + "\n")):
                parsed, failures = series._stdin_cell_results()
                self.assertEqual(parsed, [])
                [(number, raw, error, reason)] = failures
                self.assertEqual((number, raw, error), (1, line.encode(), error_type))
                self.assertTrue(reason.startswith("stdin line 1 is not JSON: "), reason)

    def test_declaration_is_bound_into_the_rust_and_python_evidence_identity(self):
        ordinary = json.loads(self._SCORECARD_ORDINARY_ROW)
        self.assertEqual(
            series._cell_result_evidence_sha256(ordinary), self._SCORECARD_ORDINARY
        )
        for (code, signal, reason), golden in self._SCORECARD_DECLARED.items():
            with self.subTest(code=code, signal=signal, reason=reason):
                declared = copy.deepcopy(ordinary)
                declared["expected_guest_exit"] = {
                    "code": code, "signal": signal, "reason": reason,
                }
                self.assertEqual(series._cell_result_evidence_sha256(declared), golden)

    def test_declaration_removal_or_change_cannot_keep_an_accepted_row_identity(self):
        for label, status, signal, guest_code, guest_signal, declared in (
            ("exit 7", 7, None, 7, None, (7, None)),
            ("signal 11", None, 11, None, 11, (None, 11)),
            ("signal 11 as status 139", 139, None, None, 11, (None, 11)),
        ):
            with self.subTest(label=label):
                cell = self._expected_exit_cell(
                    status=status, signal=signal, guest_exit_code=guest_code,
                    guest_signal=guest_signal, declared=declared,
                )
                row = self._project_validate(cell, label.replace(" ", "-"))[0]
                accepted = series._cell_result_evidence_sha256(cell)
                audit = row["series"]["declared_guest_exit"]
                attempt = cell["attempts"][0]
                self.assertEqual(audit, {
                    "code": declared[0],
                    "signal": declared[1],
                    "reason": "the fixture guest fails on purpose",
                    "evidence_sha256": accepted,
                    "attempts": [{
                        "index": attempt["index"],
                        "outcome": "PASS",
                        "status": status,
                        "signal": signal,
                        "timed_out": False,
                        "verification_report_sha256":
                            attempt["verification_report_sha256"],
                    }],
                })
                self.assertEqual(row["series"]["attempt"], 1)
                self.assertEqual(row["series"]["outcome"], "passed")

                undeclared = copy.deepcopy(cell)
                undeclared.pop("expected_guest_exit")
                self.assertNotEqual(
                    series._cell_result_evidence_sha256(undeclared), accepted
                )
                identities = {accepted}
                for field, value in (
                    ("reason", "a different reason"),
                    *((("code", declared[0] + 1),) if declared[0] else ()),
                    *((("signal", declared[1] + 1),) if declared[1] else ()),
                ):
                    changed = copy.deepcopy(cell)
                    changed["expected_guest_exit"][field] = value
                    digest = series._cell_result_evidence_sha256(changed)
                    self.assertNotIn(digest, identities, field)
                    identities.add(digest)

    def test_a_cells_strictness_is_copied_into_its_series_row_only_when_present(self):
        # https://github.com/rrnewton/hermit/issues/4276: the harness records
        # how each cell ran; the series row carries it as written, and a row
        # whose result does not record it carries none.
        cell = self._validate_cell(1)
        self.assertNotIn("strictness", self._project_validate(cell, "unrecorded")[0]["series"])
        strictness = {"level": "strict", "virtualize_cpuid": True,
                      "max_timeslice": 200000000, "capped": False}
        recorded = copy.deepcopy(cell)
        recorded["strictness"] = strictness
        row = self._project_validate(recorded, "strict")[0]
        self.assertEqual(row["series"]["strictness"], strictness)
        malformed = copy.deepcopy(cell)
        malformed["strictness"] = "strict"
        with self.assertRaisesRegex(series.MalformedRow, "strictness must be an object"):
            self._project_validate(malformed, "malformed")

    def test_undeclared_rows_carry_no_declared_guest_exit_audit(self):
        row = self._project_validate(self._validate_cell(1), "undeclared")[0]
        self.assertNotIn("declared_guest_exit", row["series"])

    def _project_at(self, cell, tree, label):
        cell = copy.deepcopy(cell)
        cell["hermit_sha"] = tree
        rows = series.rows_from_cell_results(
            [cell], parent=self.parent, producer="validate",
            run_id=f"validate-manifest-{label}", tree=tree,
            emitted_at="2026-09-25T12:00:00Z",
        )
        series.validate_rows(rows)
        return rows

    def test_a_declared_guest_exit_must_be_its_manifest_entrys_own_declaration(self):
        # Every row below is internally consistent: its report, status and
        # compared outputs all show exactly the exit its row declares, so only
        # the manifest binding can refuse it.
        def exit_cell(code, test=None, reason=None):
            target = self._expected_exit_cell(
                status=code, signal=None, guest_exit_code=code, guest_signal=None,
                declared=(code, None),
            )
            if test is not None:
                target["test"] = test
            if reason is not None:
                target["expected_guest_exit"]["reason"] = reason
            return target

        def signal_cell(signal, test=None):
            target = self._expected_exit_cell(
                status=None, signal=signal, guest_exit_code=None, guest_signal=signal,
                declared=(None, signal),
            )
            if test is not None:
                target["test"] = test
            return target

        # Qualifying: each manifest declaration, including one in the second
        # manifest file, admits the row that carries it verbatim.
        for label, target in (
            ("exit 7", exit_cell(7)),
            ("exit 3", exit_cell(3)),
            ("signal 6 from the second manifest", signal_cell(6)),
        ):
            with self.subTest(qualifying=label):
                row = self._project_validate(target, "bound-" + label.replace(" ", "-"))[0]
                self.assertEqual(row["series"]["outcome"], "passed")
                self.assertEqual(row["series"]["cell"].split("/verify/")[0], target["test"])

        disagrees = "disagrees with its manifest entry"
        no_test_id = exit_cell(7)
        no_test_id.pop("test")
        cases = [
            ("the manifest declares a different code",
             exit_cell(7, test="fixture/exit-3"), disagrees),
            ("the manifest declares a signal, the row a code",
             exit_cell(11, test="fixture/signal-11"), disagrees),
            ("the manifest declares a code, the row a signal",
             signal_cell(7, test="fixture/exit-7"), disagrees),
            ("the manifest declares a different signal",
             signal_cell(11, test="fixture/signal-6"), disagrees),
            ("the reason differs from the manifest's",
             exit_cell(7, reason="a reason the manifest never gave"), disagrees),
            # defaults.yaml repeats fixture/exit-7 with code 9. It declares no
            # test, so the row that copies it is not authorised by anything.
            ("defaults.yaml is not a manifest",
             exit_cell(9, test="fixture/exit-7"), disagrees),
            ("the manifest entry declares no guest exit",
             exit_cell(7, test="fixture/undeclared"), "declares none"),
            ("no manifest has the row's test id",
             exit_cell(7, test="fixture/in-no-manifest"), "has no manifest entry"),
            ("the row names no test id", no_test_id, "manifest test id"),
        ]
        for label, target, message in cases:
            with self.subTest(violating=label), self.assertRaisesRegex(
                series.MalformedRow, message
            ):
                self._project_validate(target, "mutant")

        # The authority is the committed tree, never the working tree: an
        # uncommitted edit that declares fixture/undeclared does not authorise
        # a dirty-tree row that copies it.
        manifest = self.manifest_dir / "fixture-exit.yaml"
        committed = manifest.read_text(encoding="utf-8")
        manifest.write_text(self._manifest_yaml("fixture-exit", [
            ("fixture/exit-7", "code: 7"),
            ("fixture/exit-3", "code: 3"),
            ("fixture/exit-1", "code: 1"),
            ("fixture/undeclared", "code: 5"),
        ]), encoding="utf-8")
        dirty = exit_cell(5, test="fixture/undeclared")
        dirty["source_tree_dirty"] = True
        with self.subTest(violating="declared only in the uncommitted manifest"), \
                self.assertRaisesRegex(series.MalformedRow, "declares none"):
            self._project_validate(dirty, "dirty")
        manifest.write_text(committed, encoding="utf-8")

        # Each row is judged at its own tree: once a later commit withdraws the
        # declaration, the same row is refused there and still admitted at the
        # tree whose manifest made it.
        withdrawn = self._commit_manifests({
            "fixture-exit.yaml": self._manifest_yaml("fixture-exit", [
                ("fixture/exit-7", None),
                ("fixture/exit-3", "code: 3"),
                ("fixture/exit-1", "code: 1"),
                ("fixture/undeclared", None),
            ]),
        })
        self.assertNotEqual(withdrawn, self.tree)
        with self.subTest(violating="withdrawn at the row's tree"), \
                self.assertRaisesRegex(series.MalformedRow, "declares none"):
            self._project_at(exit_cell(7), withdrawn, "withdrawn")
        self.assertEqual(
            self._project_at(exit_cell(7), self.tree, "declaring")[0]["series"]["outcome"],
            "passed",
        )

        # Scalars are read as Hermit's serde_yaml loader reads them: `0o7` is
        # 7 and `yes` is the string "yes". PyYAML's default rules read the
        # string "0o7" and the boolean True, and would refuse this genuine row.
        spelled = self._commit_manifests({
            "fixture-spelled.yaml": self._manifest_yaml(
                "fixture-spelled", [("fixture/spelled", "code: 0o7")], reason="yes",
            ),
        })
        self.assertEqual(
            self._project_at(
                exit_cell(7, test="fixture/spelled", reason="yes"), spelled, "spelled",
            )[0]["series"]["outcome"],
            "passed",
        )

    def test_a_manifest_tree_that_cannot_be_read_refuses_only_declared_batches(self):
        # A declared row whose tree's manifests do not load cannot be bound,
        # and the fault is in the source, not the row: SeriesError, which
        # append-cells turns into a whole-batch refusal that quarantines every
        # input. A row with no declaration never reads the manifests.
        declared = self._expected_exit_cell(
            status=7, signal=None, guest_exit_code=7, guest_signal=None, declared=(7, None),
        )
        undeclared = self._validate_cell(1)
        declaration = "          code: 7\n          reason: r\n"
        for label, files, message in (
            ("a test id declared in two manifests",
             {"fixture-again.yaml": self._manifest_yaml("again", [("fixture/exit-7", "code: 7")])},
             "declares manifest test 'fixture/exit-7' twice"),
            ("a manifest that is not YAML",
             {"broken.yaml": "schema: 3\ntest: [\n"}, "is not UTF-8 YAML"),
            ("a manifest that is not UTF-8",
             {"latin1.yaml": "schema: 3\nbucket: caf\xe9\ntest: []\n".encode("latin-1")},
             "is not UTF-8 YAML"),
            ("a manifest without a test list",
             {"empty.yaml": "schema: 3\nbucket: empty\n"}, "has no test list"),
            ("a test entry without a string id",
             {"no-id.yaml": "schema: 3\nbucket: x\ntest:\n  - id: 7\n"},
             "test without a string id"),
            ("a declaration that is not an object",
             {"scalar.yaml": ("schema: 3\nbucket: x\ntest:\n  - id: x/scalar\n"
                              "    modes:\n      verify:\n        expected_guest_exit: 7\n")},
             "is not an object of code, signal and reason"),
            ("a declaration with an unknown key",
             {"extra.yaml": ("schema: 3\nbucket: x\ntest:\n  - id: x/extra\n"
                             "    modes:\n      verify:\n        expected_guest_exit:\n"
                             + declaration + "          note: x\n")},
             "is not an object of code, signal and reason"),
            # serde_yaml refuses a duplicate key; PyYAML alone keeps the last.
            ("a declaration with a duplicate key",
             {"twice.yaml": ("schema: 3\nbucket: x\ntest:\n  - id: x/twice\n"
                             "    modes:\n      verify:\n        expected_guest_exit:\n"
                             "          code: 9\n" + declaration)},
             "duplicate manifest key 'code'"),
            ("a corpus section without rows",
             {"bad-corpus.yaml": "schema: 3\nbucket: x\ncorpus: {rows: []}\n"},
             "malformed corpus section"),
            # A corpus row is a test whose id is <bucket>/<id or label>.
            ("a corpus row whose id another manifest declares",
             {"fixture-corpus.yaml": ("schema: 3\nbucket: fixture\ncorpus:\n  rows:\n"
                                      "    - {label: exit-7, argv: [/bin/true]}\n")},
             "declares manifest test 'fixture/exit-7' twice"),
        ):
            with self.subTest(label=label):
                tree = self._commit_manifests(files)
                with self.assertRaisesRegex(series.SeriesError, message) as refused:
                    self._project_at(declared, tree, "unreadable")
                self.assertNotIsInstance(refused.exception, series.MalformedRow)
                self.assertEqual(
                    self._project_at(undeclared, tree, "unread")[0]["series"]["outcome"],
                    "passed",
                )
                # Restore the readable manifests for the next case.
                self._commit_manifests({name: None for name in files})

    def test_a_corpus_manifest_reads_as_its_expanded_test_ids(self):
        # Hermit expands a corpus section (tests/e2e/manifests/compat.yaml)
        # into one test per row; a tree carrying one still binds a declared
        # row of another manifest instead of refusing the whole tree.
        tree = self._commit_manifests({
            "compat.yaml": ("schema: 3\nbucket: compat\ncorpus:\n  rows:\n"
                            "    - {label: echo, argv: [/bin/echo]}\n"
                            "    - {label: g++, id: gxx, argv: [g++, --version]}\n"),
        })
        self.assertEqual(
            self._project_at(self._declared_exit_7(), tree, "corpus")[0]["series"]["outcome"],
            "passed",
        )
        self._commit_manifests({"compat.yaml": None})

    def _declared_exit_7(self):
        return self._expected_exit_cell(
            status=7, signal=None, guest_exit_code=7, guest_signal=None,
            declared=(7, None),
        )

    def test_verification_report_refuses_non_json_constants_and_duplicates(self):
        # Hermit's serde_json never writes NaN, Infinity or a repeated field,
        # and its readers refuse all of them. Python's json module accepts
        # both unless told otherwise, declared row or not.
        def raw_edit(cell, old, new):
            attempt = cell["attempts"][0]
            raw = attempt["verification_report"]
            self.assertEqual(raw.count(old), 1, old)
            raw = raw.replace(old, new)
            attempt["verification_report"] = raw
            attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()
            return cell

        for label, make in (
            ("declared", self._declared_exit_7),
            ("undeclared", lambda: self._validate_cell(1)),
        ):
            self.assertEqual(
                self._project_validate(make(), f"{label}-control")[0]["series"]["outcome"],
                "passed",
            )
            for constant in ("NaN", "Infinity", "-Infinity"):
                with self.subTest(row=label, constant=constant), self.assertRaisesRegex(
                    series.MalformedRow, "non-JSON constant"
                ):
                    self._project_validate(
                        raw_edit(make(), '"runtime":null', '"runtime":' + constant),
                        f"{label}-constant",
                    )
            with self.subTest(row=label, duplicate="verdict"), self.assertRaisesRegex(
                series.MalformedRow, "duplicate field 'verdict'"
            ):
                self._project_validate(
                    raw_edit(make(), '"verdict":"matched"',
                             '"verdict":"diverged","verdict":"matched"'),
                    f"{label}-duplicate",
                )

    def test_compared_output_byte_counts_are_bounded_by_u64(self):
        def with_bytes(count):
            cell = self._declared_exit_7()
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            for side in ("left", "right"):
                report["compared_outputs"][side]["stdout_bytes"] = count
                report["compared_outputs"][side]["stderr_bytes"] = count
            self._replace_comparison_report(attempt, report)
            return cell

        row = self._project_validate(with_bytes(2**64 - 1), "u64-max")[0]
        self.assertEqual(row["series"]["outcome"], "passed")
        for count in (2**64, 2**70):
            with self.subTest(count=count), self.assertRaisesRegex(
                series.MalformedRow, "matched report is internally inconsistent"
            ):
                self._project_validate(with_bytes(count), "u64-overflow")

    def _assert_stdin_line_quarantined(self, line, run_id, message):
        # append-cells judges each stdin line on its own. A refused line is
        # quarantined at parse with its reason and exact text; when it is the
        # only line, nothing is spooled and the command exits 1.
        rc, stdout, stderr = self._append_cells(
            [line], producer="validate", run_id=run_id,
            emitted_at="2026-09-25T12:00:00Z",
        )
        self.assertEqual(rc, 1, stdout + stderr)
        self.assertIn(message, stderr)
        [record] = self._quarantined(run_id)
        [item] = record["inputs"]
        self.assertEqual(
            (record["stage"], item["input_row"], item["raw"]), ("parse", 1, line)
        )
        self.assertIn(message, record["reason"])
        self.assertEqual(sorted(series.spool_dir(self.parent).glob("*.jsonl")), [])

    def _edit_report(self, cell, edit):
        attempt = cell["attempts"][0]
        report = json.loads(attempt["verification_report"])
        edit(report)
        self._replace_comparison_report(attempt, report)
        return cell

    def _project_one(self, cell, label):
        rows = series.rows_from_cell_results(
            [self._validate_cell(1), cell],
            parent=self.parent,
            producer="validate",
            run_id=f"validate-integrity-{label}",
            tree=self.tree,
            emitted_at="2026-09-26T12:00:00Z",
        )
        series.validate_rows(rows)
        return rows

    def test_a_runner_red_is_typed_only_after_its_evidence_is_valid(self):
        """The review reproduction from run 221's qemu-hello row: declared,
        status and report exit 7, attempt and outer row FAIL/crash-error, and
        only the right stdout_sha256 altered. The runner-red branch must not
        retype that contradiction as failed_match; structure is refused first.
        A structurally valid mismatch (declared 7, observed 5) is still kept
        as failed_match, beside a passing sibling."""
        def qemu_red():
            cell = self._runner_red(self._declared_exit_7())
            cell["test"] = cell["test"] + "-red"
            return cell

        def valid_mismatch():
            cell = self._runner_red(self._expected_exit_cell(
                status=5, signal=None, guest_exit_code=5, guest_signal=None,
                declared=(7, None)))
            cell["test"] = cell["test"] + "-red"
            return cell

        def undeclared_red():
            cell = self._runner_red(self._expected_exit_cell(
                status=3, signal=None, guest_exit_code=3, guest_signal=None,
                declared=None))
            cell["test"] = cell["test"] + "-red"
            return cell

        rows = self._project_one(valid_mismatch(), "valid-mismatch")
        (red,) = [row for row in rows if row["series"]["outcome"] == "errored"]
        (evidence,) = red["series"]["no_verdict_evidence"]["attempts"]
        self.assertEqual(evidence["kind"], "failed_match")
        self.assertEqual(sorted(row["series"]["outcome"] for row in rows), ["errored", "passed"])

        def right(**fields):
            return lambda r: r["compared_outputs"]["right"].update(fields)

        def left(**fields):
            return lambda r: r["compared_outputs"]["left"].update(fields)

        def both(**fields):
            return lambda r: [r["compared_outputs"][side].update(fields) for side in ("left", "right")]

        def attempt_edit(make, **fields):
            cell = make()
            cell["attempts"][0].update(fields)
            return cell

        cases = [
            ("qemu exit 7, right stdout_sha256 altered",
             self._edit_report(qemu_red(), right(stdout_sha256="c" * 64)),
             "left and right compared outputs differ"),
            ("qemu exit 7, left stdout_sha256 altered",
             self._edit_report(qemu_red(), left(stdout_sha256="c" * 64)),
             "left and right compared outputs differ"),
            ("valid mismatch, right stdout_sha256 altered",
             self._edit_report(valid_mismatch(), right(stdout_sha256="c" * 64)),
             "left and right compared outputs differ"),
            ("valid mismatch, right stderr_bytes altered",
             self._edit_report(valid_mismatch(), right(stderr_bytes=1)),
             "left and right compared outputs differ"),
            ("valid mismatch, right exit_code altered",
             self._edit_report(valid_mismatch(), right(exit_code=7)),
             "left and right compared outputs differ"),
            ("valid mismatch, outputs name the declaration but the report does not",
             self._edit_report(valid_mismatch(), both(exit_code=7)),
             "contradict the report's guest disposition"),
            ("valid mismatch, outputs record no disposition",
             self._edit_report(valid_mismatch(), both(exit_code=None)),
             "exactly one guest disposition"),
            ("valid mismatch, outputs record both dispositions",
             self._edit_report(valid_mismatch(), both(signal=11)),
             "exactly one guest disposition"),
            ("valid mismatch, right output shape altered",
             self._edit_report(valid_mismatch(), lambda r: r["compared_outputs"]["right"].pop("stderr_bytes")),
             "right compared output is not Hermit's canonical shape"),
            ("valid mismatch, compared outputs absent",
             self._edit_report(valid_mismatch(), lambda r: r.pop("compared_outputs")),
             "requires compared_outputs"),
            ("valid mismatch, Hermit status disagrees with the report",
             attempt_edit(valid_mismatch, status=6),
             "status does not report the guest's disposition"),
            ("valid mismatch, Hermit signal beside an exit code",
             attempt_edit(valid_mismatch, signal=11),
             "status does not report the guest's disposition"),
            ("valid mismatch, boolean status",
             attempt_edit(valid_mismatch, status=True),
             "attempt status is not an i32"),
            ("valid mismatch, no --verify-allow=failure",
             attempt_edit(valid_mismatch, argv=[
                 "/src/hermit", "run", "--strict", "--verify-strict", "--verify",
                 "--verify-json", "/results/verify-1.json", "--", "/results/fixtures/program",
                 "--verify-allow=failure"]),
             "without --verify-allow=failure"),
            ("valid mismatch, argv not a list",
             attempt_edit(valid_mismatch, argv="/src/hermit run --verify-allow=failure"),
             "argv is not a list of strings"),
            ("undeclared red, right stdout_sha256 altered",
             self._edit_report(undeclared_red(), right(stdout_sha256="c" * 64)),
             "left and right compared outputs differ"),
        ]
        for label, target, message in cases:
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow, "matched report is internally inconsistent: .*" + re.escape(message)
            ):
                self._project_one(target, "mutant")

    def test_an_ordinary_matched_pass_refuses_mismatched_compared_outputs(self):
        """Hermit's reader refuses any present compared_outputs that do not
        support a match (canonical_verdict.rs require_exact_output_match),
        declared row or not."""
        def ordinary(**right_fields):
            cell = self._validate_cell(1)

            def edit(report):
                report["guest_exit_code"] = 0
                report["compared_outputs"] = {
                    side: {
                        "exit_code": 0, "signal": None,
                        "stdout_sha256": "a" * 64, "stdout_bytes": 12,
                        "stderr_sha256": "b" * 64, "stderr_bytes": 0,
                    }
                    for side in ("left", "right")
                }
                report["compared_outputs"]["right"].update(right_fields)
            return self._edit_report(cell, edit)

        row = self._project_validate(ordinary(), "ordinary-outputs")[0]
        self.assertEqual(row["series"]["outcome"], "passed")
        for label, fields, message in (
            ("right stdout_sha256", {"stdout_sha256": "c" * 64}, "compared outputs differ"),
            ("right stderr_sha256", {"stderr_sha256": "c" * 64}, "compared outputs differ"),
            ("right stdout_bytes", {"stdout_bytes": 13}, "compared outputs differ"),
            ("right exit_code", {"exit_code": 1}, "compared outputs differ"),
            ("right fractional bytes", {"stdout_bytes": 12.0}, "not Hermit's canonical shape"),
            ("right uppercase hash", {"stdout_sha256": "A" * 64}, "not Hermit's canonical shape"),
        ):
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow, "matched report is internally inconsistent: .*" + re.escape(message)
            ):
                self._project_validate(ordinary(**fields), "ordinary-mutant")
        contradicted = self._edit_report(ordinary(), lambda r: r.update(guest_exit_code=3))
        with self.assertRaisesRegex(series.MalformedRow, "contradict the report's guest disposition"):
            self._project_validate(contradicted, "ordinary-contradicted")
        # Noncanonical matches project no_result, but their outputs are typed
        # by the same reader and refused the same way.
        noncanonical = self._edit_report(
            ordinary(stdout_sha256="c" * 64),
            lambda r: r["comparison"].update(strictness="stripped"),
        )
        with self.assertRaisesRegex(series.MalformedRow, "compared outputs differ"):
            self._project_validate(noncanonical, "noncanonical-mutant")

    def test_a_failed_match_cannot_erase_a_diverged_sibling_attempt(self):
        red = self._runner_red(self._expected_exit_cell(
            status=5, signal=None, guest_exit_code=5, guest_signal=None,
            declared=(7, None)))
        red["test"] = red["test"] + "-red"
        diverged = self._validate_diverged_cell(1)["attempts"][0]
        diverged["index"] = "2"
        for label, attempts in (
            ("diverged then failed match", [diverged, red["attempts"][0]]),
            ("failed match then diverged", [red["attempts"][0], diverged]),
        ):
            mixed = json.loads(json.dumps(red))
            mixed["attempts"] = json.loads(json.dumps(attempts))
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow, "also carries a diverged attempt"
            ):
                self._project_one(mixed, "mixed")

    def test_compared_info_message_counts_are_bounded_by_u64(self):
        def with_counts(left, right):
            return self._edit_report(
                self._validate_cell(1),
                lambda r: r.update(compared_log_messages={"left": left, "right": right}),
            )

        row = self._project_validate(with_counts(2**64 - 1, 2**64 - 1), "count-max")[0]
        self.assertEqual(row["series"]["outcome"], "passed")
        for label, left, right, message in (
            ("left u64::MAX + 1", 2**64, 17, "exceed u64"),
            ("right u64::MAX + 1", 17, 2**64, "exceed u64"),
            ("both far past u64", 2**70, 2**70, "exceed u64"),
            ("zero", 0, 17, "no nonzero INFO-message counts"),
            ("boolean", True, 17, "no nonzero INFO-message counts"),
        ):
            with self.subTest(label=label), self.assertRaisesRegex(series.MalformedRow, message):
                self._project_validate(with_counts(left, right), "count-mutant")

    def test_strict_stdin_refuses_an_overflowing_number_in_any_field(self):
        """A real row with an unused extra field 1e999 parsed as Python
        infinity and projected PASS; serde_json refuses it. append-cells
        quarantines the line, and plain append refuses a backend parity row
        carrying one."""
        base = json.dumps(self._validate_cell(1), separators=(",", ":"))
        for index, (label, line) in enumerate((
            ("extra 1e999", base[:-1] + ',"extra":1e999}'),
            ("extra -1e999", base[:-1] + ',"extra":-1e999}'),
            ("nested 1E400", base[:-1] + ',"extra":{"deep":[1E400]}}'),
        )):
            with self.subTest(label=label):
                with mock.patch.object(series.sys, "stdin", io.StringIO(line + "\n")):
                    parsed, failures = series._stdin_cell_results()
                self.assertEqual(parsed, [])
                [(number, raw, error, reason)] = failures
                self.assertEqual((number, raw, error), (1, line.encode(), "MalformedRow"))
                self.assertTrue(reason.startswith("stdin line 1 is not JSON: "), reason)
                self.assertIn("out-of-range number", reason)
                self._assert_stdin_line_quarantined(
                    line, f"overflow-mutant-{index}", "out-of-range number"
                )
        parity = json.dumps(self._parity_cell(), separators=(",", ":"))
        with mock.patch.object(
            series.sys, "stdin", io.StringIO(parity[:-1] + ',"extra":1e999}\n')
        ), contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(series._stdin_rows(), (None, 2))
        self.assertIn("out-of-range number", stderr.getvalue())
        # A finite float in an extra field is still ordinary JSON to both readers.
        with mock.patch.object(
            series.sys, "stdin", io.StringIO(base[:-1] + ',"extra":1.5e3}\n')
        ):
            parsed, failures = series._stdin_cell_results()
        self.assertEqual((failures, parsed[0][1]["extra"]), ([], 1500.0))
        with mock.patch.object(
            series.sys, "stdin", io.StringIO(parity[:-1] + ',"extra":1.5e3}\n')
        ):
            rows, status = series._stdin_rows()
        self.assertEqual((status, rows[0]["extra"]), (0, 1500.0))

    def test_append_cells_refuses_duplicate_fields_and_constants_in_the_outer_row(self):
        # A null then valid declaration would otherwise collapse to the valid
        # one, and a valid then null one to undeclared, before validation.
        declaration = json.dumps(
            self._declared_exit_7()["expected_guest_exit"], separators=(",", ":")
        )
        cases = []
        for label, first, last in (
            ("null then valid declaration", "null", declaration),
            ("valid then null declaration", declaration, "null"),
            ("valid declaration twice", declaration, declaration),
        ):
            cell = self._declared_exit_7()
            cell.pop("expected_guest_exit")
            raw = json.dumps(cell, separators=(",", ":"))
            cases.append((
                label,
                raw[:-1] + ',"expected_guest_exit":' + first
                + ',"expected_guest_exit":' + last + "}",
                "duplicate field 'expected_guest_exit'",
            ))
        undeclared = json.dumps(self._validate_cell(1), separators=(",", ":"))
        cases.append((
            "undeclared row with a duplicate outcome",
            '{"outcome":"FAIL",' + undeclared[1:],
            "duplicate field 'outcome'",
        ))
        for constant in ("NaN", "Infinity", "-Infinity"):
            cases.append((
                f"outer {constant}",
                undeclared[:-1] + ',"extra":' + constant + "}",
                "non-JSON numeric constant",
            ))
        for index, (label, text, message) in enumerate(cases):
            with self.subTest(label=label):
                self._assert_stdin_line_quarantined(text, f"outer-mutant-{index}", message)
        # The same rows written once each are accepted by the same command.
        spool = series.spool_dir(self.parent)
        rc, stdout, stderr = self._append_cells(
            [json.dumps(self._declared_exit_7())], producer="validate",
            run_id="outer-control", emitted_at="2026-09-25T12:00:00Z",
        )
        self.assertEqual(rc, 0, stderr)
        self.assertEqual(self._quarantined("outer-control"), [])
        stored = series.union(sorted(spool.glob("*.jsonl")))
        self.assertEqual([row["series"]["outcome"] for row in stored], ["passed"])
        # A refused line does not take a valid sibling with it.
        _label, text, message = cases[0]
        rc, stdout, stderr = self._append_cells(
            [text, json.dumps(self._declared_exit_7())], producer="validate",
            run_id="outer-sibling", emitted_at="2026-09-25T12:00:00Z",
        )
        self.assertEqual(rc, 0, stderr)
        [record] = self._quarantined("outer-sibling")
        self.assertEqual((record["stage"], record["inputs"][0]["input_row"]), ("parse", 1))
        self.assertIn(message, record["reason"])
        stored = series.union([spool / "outer-sibling.jsonl"])
        self.assertEqual([row["series"]["outcome"] for row in stored], ["passed"])


    def test_no_verdict_contradiction_refuses_the_batch(self):
        contradictory = self._validate_no_verdict_cell(
            1, timed_out=True, backend="sabre"
        )
        contradictory["outcome"] = "FAIL"
        contradictory["result"] = "crash-error"
        contradictory["failure_class"] = "product_failure"
        with self.assertRaisesRegex(
            series.MalformedRow,
            "no supporting canonical typed attempt",
        ):
            series.rows_from_cell_results(
                [contradictory],
                parent=self.parent,
                producer="validate",
                run_id="validate-contradiction",
                tree=self.tree,
                emitted_at="2026-08-30T20:00:00Z",
            )

        for status in (0, None):
            incomplete = self._validate_no_verdict_cell(
                1, timed_out=False, backend="kvm"
            )
            incomplete["attempts"][0]["status"] = status
            with self.subTest(status=status), self.assertRaisesRegex(
                series.MalformedRow,
                "NotRun report has no complete process or pre-launch timeout disposition",
            ):
                series.rows_from_cell_results(
                    [incomplete],
                    parent=self.parent,
                    producer="validate",
                    run_id="validate-incomplete-disposition",
                    tree=self.tree,
                    emitted_at="2026-08-30T20:00:00Z",
                )

    def test_hermit_repeat_preserves_detcore_tree(self):
        import contextlib
        import io

        cell = self._cell(0)
        cell["detcore_tree"] = "b" * 40
        rows = series.rows_from_cell_results(
            [cell],
            parent=self.parent,
            producer="hermit-repeat",
            run_id="repeat-one",
            tree=self.tree,
            emitted_at="2026-08-27T10:00:00Z",
        )
        self.assertEqual(rows[0]["producer"], "hermit-repeat")
        self.assertEqual(rows[0]["series"]["detcore_tree"], "b" * 40)

        real_stdin = sys.stdin
        sys.stdin = io.StringIO(json.dumps(cell) + "\n")
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "hermit-repeat",
                    "--run-id", "repeat-one",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-27T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        stored = series.union(sorted(series.spool_dir(self.parent).glob("*.jsonl")))
        self.assertEqual(len(stored), 1)
        self.assertEqual(stored[0]["producer"], "hermit-repeat")
        self.assertEqual(stored[0]["series"]["detcore_tree"], "b" * 40)

        cell["detcore_tree"] = "not-an-object-id"
        with self.assertRaisesRegex(
            series.MalformedRow,
            "detcore_tree must be a 40-hex object id",
        ):
            series.rows_from_cell_results(
                [cell],
                parent=self.parent,
                producer="hermit-repeat",
                run_id="repeat-one",
                tree=self.tree,
                emitted_at="2026-08-27T10:00:00Z",
            )

    def test_pressure_retry_retains_both_attempts_at_one_repetition(self):
        first = self._diverged_cell(
            7,
            record=1_718,
            syscall=7,
            scheduler_turn=1_248,
            virtual_nanoseconds=80_000,
            duration_ms=90,
        )
        first["attempt"] = 1
        second = self._cell(7, outcome="PASS", duration_ms=80)
        second["attempt"] = 2
        rows = self._project([second, first])
        self.assertEqual(len(rows), 2)
        self.assertEqual(
            [
                (
                    row["series"]["run_index"],
                    row["series"]["attempt"],
                    row["series"]["outcome"],
                )
                for row in rows
            ],
            [(7, 1, "diverged"), (7, 2, "passed")],
        )
        self.assertNotEqual(rows[0]["event_id"], rows[1]["event_id"])
        self.assertEqual(
            rows[0]["series"]["coordinates"]["first_divergent_record"],
            1_718,
        )
        self.assertEqual(
            rows[0]["series"]["first_divergent_messages"],
            {"left": "inbound read", "right": "finish read"},
        )
        self.assertEqual(
            [row["series"]["attempt"] for row in series.history_rows(rows, self._cell(0)["test"])],
            [1, 2],
        )

    def test_pressure_attempt_must_be_positive(self):
        cell = self._cell(7)
        cell["attempt"] = 0
        with self.assertRaisesRegex(
            series.MalformedRow,
            "pressure-test cell result attempt must be a positive int",
        ):
            self._project([cell])

    def test_validate_requires_the_appended_attempt_ordinal(self):
        cell = self._cell(0)
        with self.assertRaisesRegex(
            series.MalformedRow,
            "validate cell result attempt must be a positive int",
        ):
            series.rows_from_cell_results(
                [cell],
                parent=self.parent,
                producer="validate",
                run_id="validate-one",
                tree=self.tree,
                emitted_at="2026-08-26T10:00:00Z",
            )

    def test_cell_outcome_mapping_is_closed(self):
        cases = []
        for raw, result, failure_class, projected in (
            ("PASS", "pass", None, "passed"),
            ("FAIL", "determinism-failure", "product_failure", "diverged"),
            ("FAIL", "crash-error", "product_failure", "errored"),
            ("FAIL", "timeout", "no_result", "timeout"),
            ("FAIL", "oom", "no_result", "no_result"),
            ("ERROR", None, "understood_infrastructure_failure", "errored"),
            ("ERROR", None, "understood_prerequisite_failure", "skipped"),
            ("ERROR", None, "no_result", "no_result"),
        ):
            cell = self._cell(0, outcome=raw)
            cell["result"] = result
            cell["failure_class"] = failure_class
            cases.append((cell, projected))
        for mode in ("naked", "custom"):
            for cell, projected in cases:
                with self.subTest(mode=mode, outcome=cell["outcome"]):
                    cell = dict(cell)
                    cell["mode"] = mode
                    if mode == "naked":
                        cell["backend"] = None
                    rows = self._project([cell])
                    self.assertEqual(rows[0]["series"]["outcome"], projected)
        self.assertEqual(
            self._project([self._cell(0, outcome="HOST-INAPPLICABLE")]),
            [],
        )
        with self.assertRaises(series.MalformedRow):
            self._project([self._cell(0, outcome="SOMETHING_NEW")])

    def test_runner_refined_infrastructure_result_projects_as_errored(self):
        # Hermit's runner refines a cell whose attempt output shows an
        # environmental block to infrastructure-error or sandbox-denied, with
        # failure class understood_infrastructure_failure, under outcome FAIL
        # or ERROR (runner::observed_result), and Hermit's SeriesPayload admits
        # both as errored. The writer refused them as an unknown pair; on
        # 2026-10-10 four KVM compat cells (lsirq, lsmod and their strict
        # twins, verify exited 125 before a comparison) were quarantined so.
        for result in ("infrastructure-error", "sandbox-denied"):
            for outcome in ("FAIL", "ERROR"):
                with self.subTest(result=result, outcome=outcome):
                    cell = self._cell(0, outcome="FAIL")
                    # The quarantined rows' shape: the first run was rejected
                    # (the guest exited 1), so no comparison verdict exists.
                    attempt = self._attempt("no_result")
                    report = json.loads(attempt["verification_report"])
                    report.update(
                        no_result_reason={"kind": "first_run_rejected", "exit_code": 1, "signal": None},
                        guest_exit_code=1,
                    )
                    self._replace_comparison_report(attempt, report)
                    attempt.update(outcome="FAIL", error_kind=None, status=1)
                    cell.update(
                        outcome=outcome,
                        result=result,
                        failure_class="understood_infrastructure_failure",
                        reason="verify exited with status 125 before producing a terminal comparison",
                        attempts=[attempt],
                        runtime={"run1": cell["runtime"]["run1"]},
                    )
                    rows = self._project([cell])
                    self.assertEqual(rows[0]["series"]["outcome"], "errored")
                    self.assertEqual(rows[0]["series"]["result"], result)
                    self.assertEqual(
                        rows[0]["series"]["failure_class"], "understood_infrastructure_failure"
                    )
        # The refinement is not a product verdict: a matched attempt cannot
        # support it.
        cell = self._cell(0, outcome="FAIL")
        cell.update(result="infrastructure-error", failure_class="understood_infrastructure_failure")
        with self.assertRaisesRegex(series.MalformedRow, "no supporting canonical typed attempt"):
            self._project([cell])

    def test_cell_outcome_refuses_unhashable_or_untyped_enum_values(self):
        # A list or object is unhashable: membership in a closed enum would raise
        # a raw TypeError. Every such value is a row refusal instead.
        for field in ("outcome", "result", "failure_class"):
            for value in (["PASS"], {"PASS": True}, 1, True):
                with self.subTest(field=field, value=value):
                    cell = self._cell(0, outcome="PASS")
                    cell[field] = value
                    with self.assertRaises(series.MalformedRow):
                        series._cell_outcome(cell)
                    with self.assertRaises(series.MalformedRow):
                        self._project([cell])

    def test_cell_outcome_refuses_unknown_or_missing_mode(self):
        for mode in ("unknown", None):
            for outcome in ("PASS", "FAIL"):
                with self.subTest(mode=mode, outcome=outcome):
                    cell = self._cell(0, outcome=outcome)
                    if mode is None:
                        del cell["mode"]
                    else:
                        cell["mode"] = mode
                    with self.assertRaisesRegex(series.MalformedRow, "mode"):
                        series._cell_outcome(cell)
                    with self.assertRaisesRegex(series.MalformedRow, "mode"):
                        self._project([cell])

    def test_comparison_cell_outcome_validates_all_attempts_and_uses_framework_result(self):
        matched = self._attempt("matched")
        timed_out = self._attempt("no_result", timed_out=True, index="2")
        diverged = self._attempt(
            "diverged",
            record=98,
            syscall=37,
            scheduler_turn=4,
            virtual_nanoseconds=80_000,
            index="3",
        )

        for attempts in ([matched, timed_out], [timed_out, matched]):
            cell = self._cell(0, outcome="FAIL", duration_ms=30_453)
            cell["attempts"] = attempts
            cell["result"] = "timeout"
            cell["failure_class"] = "no_result"
            row = self._project([cell])[0]["series"]
            self.assertEqual(row["outcome"], "timeout")
            self.assertNotIn("coordinates", row)

        timeout_without_report = self._attempt("no_result", timed_out=True, index="2")
        del timeout_without_report["verification_report"]
        del timeout_without_report["verification_report_sha256"]
        cell = self._cell(0, outcome="ERROR", duration_ms=30_453)
        cell["attempts"] = [matched, timeout_without_report]
        cell["result"] = None
        cell["failure_class"] = "no_result"
        self.assertEqual(
            self._project([cell])[0]["series"]["outcome"],
            "no_result",
        )

        cell = self._cell(0, outcome="FAIL")
        cell["attempts"] = [matched, diverged]
        row = self._project([cell])[0]["series"]
        self.assertEqual(row["outcome"], "diverged")

        # Hermit's runner checks for a timed-out attempt before any divergence
        # (observed_result_from_typed_evidence in
        # hermit/ci/manifest-plan/src/runner.rs), so a cell that retains one is
        # written as a timeout even beside a located divergence. The shared
        # checker refuses the determinism-failure the runner never writes.
        for attempts in ([diverged, timed_out], [timed_out, diverged]):
            cell = self._cell(0, outcome="FAIL", duration_ms=30_453)
            cell["attempts"] = attempts
            cell["result"] = "timeout"
            cell["failure_class"] = "no_result"
            row = self._project([cell])[0]["series"]
            self.assertEqual(row["outcome"], "timeout")
            self.assertNotIn("coordinates", row)

            contradicted = self._cell(0, outcome="FAIL")
            contradicted["attempts"] = attempts
            with self.assertRaisesRegex(
                series.MalformedRow,
                r"pressure_evidence retains a timed-out attempt, which contradicts "
                r"result Some\(DeterminismFailure\)",
            ):
                self._project([contradicted])

        passing = self._cell(0, outcome="PASS")
        self.assertEqual(self._project([passing])[0]["series"]["outcome"], "passed")

        replay = self._cell(0, outcome="PASS")
        replay["mode"] = "replay"
        report = json.loads(replay["attempts"][0]["verification_report"])
        report["comparison"]["virtualize_time"] = False
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        replay["attempts"][0]["verification_report"] = raw
        replay["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        self.assertEqual(self._project([replay])[0]["series"]["outcome"], "passed")

        for mode, field, value in (
            ("replay", "virtualize_time", True),
            ("verify", "virtualize_time", False),
            ("chaos", "virtualize_time", False),
            ("verify", "display_name", "weakened-display"),
        ):
            with self.subTest(mode=mode, field=field, value=value):
                noncanonical_pass = self._cell(0, outcome="PASS")
                noncanonical_pass["mode"] = mode
                report = json.loads(
                    noncanonical_pass["attempts"][0]["verification_report"]
                )
                report["comparison"][field] = value
                raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
                noncanonical_pass["attempts"][0]["verification_report"] = raw
                noncanonical_pass["attempts"][0][
                    "verification_report_sha256"
                ] = hashlib.sha256(raw.encode()).hexdigest()
                with self.assertRaisesRegex(
                    series.MalformedRow,
                    "no supporting canonical typed attempt",
                ):
                    self._project([noncanonical_pass])

        chaos_nonzero = self._cell(0, outcome="PASS")
        chaos_nonzero["mode"] = "chaos"
        chaos_nonzero["attempts"][0]["status"] = 7
        self.assertEqual(
            self._project([chaos_nonzero])[0]["series"]["outcome"],
            "passed",
        )

        dbt = self._cell(0, outcome="PASS")
        report = json.loads(dbt["attempts"][0]["verification_report"])
        report["comparison"]["record_envelope"] = "dbt_evidence_transport_v1"
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        dbt["attempts"][0]["verification_report"] = raw
        dbt["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        self.assertEqual(self._project([dbt])[0]["series"]["outcome"], "passed")

        all_match_fail = self._cell(0, outcome="FAIL")
        all_match_fail["result"] = "crash-error"
        all_match_fail["failure_class"] = "product_failure"
        with self.assertRaisesRegex(
            series.MalformedRow,
            "no supporting canonical typed attempt",
        ):
            self._project([all_match_fail])

        crash_before_comparison = self._cell(0, outcome="FAIL")
        crash_before_comparison["attempts"] = [self._attempt("no_result")]
        crash_before_comparison["result"] = "crash-error"
        crash_before_comparison["failure_class"] = "product_failure"
        self.assertEqual(
            self._project([crash_before_comparison])[0]["series"]["outcome"],
            "errored",
        )

        mixed_no_result = self._cell(0, outcome="ERROR")
        mixed_no_result["attempts"] = [matched, self._attempt("no_result", index="2")]
        mixed_no_result["result"] = None
        mixed_no_result["failure_class"] = "no_result"
        self.assertEqual(
            self._project([mixed_no_result])[0]["series"]["outcome"],
            "no_result",
        )

        mixed_error = self._cell(0, outcome="ERROR")
        mixed_error["attempts"] = [matched, self._attempt("infrastructure_error", index="2")]
        mixed_error["result"] = None
        mixed_error["failure_class"] = "understood_infrastructure_failure"
        self.assertEqual(
            self._project([mixed_error])[0]["series"]["outcome"],
            "errored",
        )

    def test_prelaunch_timeout_cell_preserves_the_failed_producer_result(self):
        for mode in ("verify", "replay", "chaos"):
            for error_kind in ("cpu-timeout", "wall-timeout"):
                with self.subTest(mode=mode, error_kind=error_kind):
                    cell = self._cell(0, outcome="ERROR")
                    cell.update({
                        "mode": mode,
                        "result": "timeout",
                        "failure_class": "no_result",
                        "error_kind": error_kind,
                    })
                    attempt = self._attempt("no_result", timed_out=True)
                    attempt.update({"signal": None, "error_kind": error_kind})
                    cell["attempts"] = [attempt]
                    original = json.dumps(cell, sort_keys=True)
                    self.assertEqual(series._cell_outcome(cell), "timeout")
                    evidence = series._no_verdict_evidence(cell, "timeout")
                    self.assertEqual(evidence["attempts"], [{
                        "index": "1",
                        "kind": "not_run",
                        "attempt_outcome": "ERROR",
                        "disposition": "timeout",
                        "error_kind": error_kind,
                        "status": None,
                        "signal": None,
                        "timed_out": True,
                        "verification_report_sha256": attempt[
                            "verification_report_sha256"
                        ],
                    }])
                    self.assertEqual(json.dumps(cell, sort_keys=True), original)
                    for mutation in (
                        {"outcome": "PASS"},
                        {"outcome": "HOST-INAPPLICABLE"},
                        {"failure_class": "product_failure"},
                        {"result": "pass", "failure_class": None},
                    ):
                        with self.subTest(mutation=mutation):
                            with self.assertRaises(series.MalformedRow):
                                series._cell_outcome({**cell, **mutation})

    def test_launched_timeout_without_a_report_keeps_its_process_disposition(self):
        for error_kind in ("cpu-timeout", "wall-timeout"):
            for status, signal in ((124, None), (None, 15)):
                with self.subTest(error_kind=error_kind, status=status, signal=signal):
                    cell = self._cell(0, outcome="ERROR")
                    cell.update({
                        "result": "timeout",
                        "failure_class": "no_result",
                        "error_kind": error_kind,
                    })
                    attempt = self._attempt("no_result", timed_out=True)
                    attempt.update({
                        "error_kind": error_kind,
                        "status": status,
                        "signal": signal,
                        "verification_report": None,
                        "verification_report_sha256": None,
                    })
                    cell["attempts"] = [attempt]
                    self.assertEqual(series._cell_outcome(cell), "timeout")
                    retained = series._no_verdict_evidence(cell, "timeout")["attempts"][0]
                    self.assertEqual(retained["kind"], "missing_report_timeout")
                    self.assertEqual(retained["attempt_outcome"], "ERROR")
                    self.assertEqual(retained["disposition"], "timeout")
                    self.assertEqual((retained["status"], retained["signal"]), (status, signal))
                    self.assertIsNone(retained["verification_report_sha256"])
                    attempt.update({"status": None, "signal": None})
                    with self.assertRaisesRegex(series.MalformedRow, "did not time out"):
                        series._cell_outcome(cell)

    def test_prelaunch_timeout_accepts_only_the_recorded_error_kinds(self):
        for error_kind in (
            "incomplete-verification-evidence", "cpu-timeout", "wall-timeout",
        ):
            attempt = self._attempt("no_result", timed_out=True)
            attempt.update({"signal": None, "error_kind": error_kind})
            with self.subTest(error_kind=error_kind):
                self.assertEqual(
                    series._typed_comparison_attempt(attempt, mode="verify", index=1),
                    "timeout",
                )

        for error_kind in ("cpu-timeout", "wall-timeout"):
            for label, mutation in (
                ("not timed out", {"timed_out": False}),
                ("wrong attempt outcome", {"outcome": "FAIL"}),
                ("unrelated cause", {"error_kind": "infrastructure"}),
                ("missing cause", {"error_kind": None}),
                ("array cause", {"error_kind": ["cpu-timeout"]}),
                ("object cause", {"error_kind": {"kind": "cpu-timeout"}}),
                ("zero exit status", {"status": 0}),
                ("negative exit status", {"status": -1}),
                ("zero signal", {"signal": 0}),
                ("negative signal", {"signal": -9}),
                ("wrong report identity", {"verification_report_sha256": "0" * 64}),
                ("missing report", {"verification_report": None}),
            ):
                attempt = self._attempt("no_result", timed_out=True)
                attempt.update({"signal": None, "error_kind": error_kind})
                attempt.update(mutation)
                with self.subTest(error_kind=error_kind, label=label):
                    with self.assertRaises(series.MalformedRow):
                        series._typed_comparison_attempt(attempt, mode="verify", index=1)

    def test_comparison_cell_outcome_refuses_malformed_or_reidentified_reports(self):
        malformed = self._cell(0)
        malformed["attempts"][0]["verification_report"] = "{"
        malformed["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            b"{"
        ).hexdigest()
        with self.assertRaisesRegex(series.MalformedRow, "report is malformed"):
            self._project([malformed])

        reidentified = self._cell(0)
        reidentified["attempts"][0]["verification_report_sha256"] = "0" * 64
        with self.assertRaisesRegex(series.MalformedRow, "report hash mismatch"):
            self._project([reidentified])

        orphaned_hash = self._cell(0, outcome="ERROR")
        orphaned_hash["attempts"] = [self._attempt("no_result", timed_out=True)]
        del orphaned_hash["attempts"][0]["verification_report"]
        with self.assertRaisesRegex(series.MalformedRow, "hash without a report"):
            self._project([orphaned_hash])

        negative_unreported_status = self._cell(0, outcome="ERROR")
        negative_unreported_status["attempts"] = [
            self._attempt("no_result", timed_out=True)
        ]
        attempt = negative_unreported_status["attempts"][0]
        del attempt["verification_report"]
        del attempt["verification_report_sha256"]
        attempt.update({"status": -1, "signal": None})
        with self.assertRaisesRegex(series.MalformedRow, "did not time out"):
            self._project([negative_unreported_status])

        incomplete = self._cell(0)
        report = json.loads(incomplete["attempts"][0]["verification_report"])
        del report["comparison"]["compare_io_buffers"]
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        incomplete["attempts"][0]["verification_report"] = raw
        incomplete["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        with self.assertRaisesRegex(series.MalformedRow, "comparison omitted"):
            self._project([incomplete])

        contradictory = self._cell(0, outcome="ERROR")
        contradictory["attempts"] = [self._attempt("no_result", timed_out=True)]
        report = json.loads(contradictory["attempts"][0]["verification_report"])
        report["first_divergent_record"] = 1
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        contradictory["attempts"][0]["verification_report"] = raw
        contradictory["attempts"][0]["verification_report_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        with self.assertRaisesRegex(series.MalformedRow, "internally inconsistent"):
            self._project([contradictory])

        contradictory_match = self._cell(0)
        contradictory_match["attempts"][0].update({
            "outcome": "ERROR",
            "error_kind": "incomplete-verification-evidence",
            "status": None,
            "signal": 15,
            "timed_out": True,
        })
        with self.assertRaisesRegex(series.MalformedRow, "internally inconsistent"):
            self._project([contradictory_match])

        nonzero_verify_match = self._cell(0)
        nonzero_verify_match["attempts"][0]["status"] = 7
        with self.assertRaisesRegex(series.MalformedRow, "internally inconsistent"):
            self._project([nonzero_verify_match])

        matched_with_no_result_reason = self._cell(0)
        attempt = matched_with_no_result_reason["attempts"][0]
        report = json.loads(attempt["verification_report"])
        report["no_result_reason"] = {"kind": "not_run"}
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        attempt["verification_report"] = raw
        attempt["verification_report_sha256"] = hashlib.sha256(raw.encode()).hexdigest()
        with self.assertRaisesRegex(series.MalformedRow, "internally inconsistent"):
            self._project([matched_with_no_result_reason])

        for label, mutation in (
            ("wrong outcome", {"outcome": "FAIL"}),
            ("missing error kind", {"error_kind": None}),
            ("zero signal", {"signal": 0}),
            ("negative signal", {"signal": -9}),
            ("negative status", {"status": -1, "signal": None}),
        ):
            incomplete_timeout = self._cell(0, outcome="ERROR")
            incomplete_timeout["attempts"] = [
                self._attempt("no_result", timed_out=True)
            ]
            incomplete_timeout["attempts"][0].update(mutation)
            with self.subTest(label=label), self.assertRaisesRegex(
                series.MalformedRow,
                "NotRun report has no complete process or pre-launch timeout disposition",
            ):
                self._project([incomplete_timeout])

    def test_projection_refuses_missing_or_disagreeing_framework_classification(self):
        missing = self._cell(0, outcome="FAIL")
        del missing["failure_class"]
        with self.assertRaisesRegex(series.MalformedRow, "failure_class=None"):
            self._project([missing])

        disagreeing = self._cell(0, outcome="FAIL")
        disagreeing["result"] = "pass"
        disagreeing["failure_class"] = None
        with self.assertRaisesRegex(series.MalformedRow, "outcome disagrees"):
            self._project([disagreeing])

        fixed_attempts = self._cell(0, outcome="FAIL")
        fixed_attempts["result"] = "crash-error"
        fixed_attempts["failure_class"] = "product_failure"
        with self.assertRaisesRegex(
            series.MalformedRow,
            "no supporting canonical typed attempt",
        ):
            self._project([fixed_attempts])
        fixed_attempts["result"] = "determinism-failure"
        fixed_attempts["attempts"] = [self._attempt("diverged")]
        self.assertEqual(
            self._project([fixed_attempts])[0]["series"]["outcome"],
            "diverged",
        )

    def test_projection_carries_the_recorded_machine_kernel_and_capabilities(self):
        projected = self._project([self._cell(0)])[0]
        self.assertEqual(projected["schema"], "stress-series/v3")
        self.assertEqual(projected["host"], "devbig014")
        self.assertEqual(projected["series"]["machine_shortname"], "devbig014")
        self.assertEqual(projected["series"]["kernel_version"], "7.1.3-test")
        self.assertEqual(
            projected["series"]["host_capabilities"]["cpuid-faulting"]["present"],
            False,
        )

        for missing in ("machine_shortname", "kernel_version", "host_capabilities"):
            malformed = self._cell(0)
            del malformed[missing]
            with self.assertRaises(series.MalformedRow, msg=missing):
                self._project([malformed])

    def test_append_cells_writes_nothing_for_an_unmet_prerequisite(self):
        import contextlib
        import io

        cell = self._cell(0, outcome="HOST-INAPPLICABLE")
        real_stdin = sys.stdin
        sys.stdin = io.StringIO(json.dumps(cell) + "\n")
        stderr = io.StringIO()
        try:
            with contextlib.redirect_stderr(stderr):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "pressure-test",
                    "--run-id", "unmet-prerequisite",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-27T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        self.assertIn("no completed cell rows", stderr.getvalue())
        self.assertEqual(list(series.spool_dir(self.parent).glob("*.jsonl")), [])

    def _chaos_divergence_cell(self):
        """A chaos cell as Hermit's runner writes one in which a seed diverged.

        The first seed matched and the second seed's two runs diverged. The
        runner's ``observed_result_from_typed_evidence`` has no chaos arm, so
        the FAILed cell is typed crash-error.
        """
        cell = self._validate_diverged_cell(1)
        cell.update(mode="chaos", result="crash-error")
        diverged = cell["attempts"][0]
        diverged["index"] = "2"
        cell["attempts"] = [self._attempt(), diverged]
        return cell

    def test_chaos_divergence_typed_crash_error_is_written_as_a_determinism_failure(self):
        cell = self._chaos_divergence_cell()
        received = json.dumps(cell, sort_keys=True)
        rows = self._project_validate(cell, "chaos-divergence")
        self.assertEqual(json.dumps(cell, sort_keys=True), received)
        payload = rows[0]["series"]
        self.assertEqual(
            (payload["outcome"], payload["result"], payload["failure_class"]),
            ("diverged", "determinism-failure", "product_failure"),
        )
        self.assertEqual(payload["coordinates"], {
            "first_divergent_record": 1_718,
            "first_divergent_syscall": 7,
            "first_divergent_scheduler_turn": 1_248,
            "first_divergent_virtual_nanoseconds": 80_000,
        })
        self.assertEqual(payload["first_divergent_messages"],
                         {"left": "inbound read", "right": "finish read"})
        self.assertNotIn("no_verdict_evidence", payload)
        # Exactly the row written for the same cell typed as a runner with a
        # chaos arm would type it.
        typed = copy.deepcopy(cell)
        typed["result"] = "determinism-failure"
        self.assertEqual(self._project_validate(typed, "chaos-divergence"), rows)

        # Through the quarantining writer and the shared checker unstubbed:
        # admitted, not refused.
        refused, provenance = [], []
        admitted = series._rows_from_cell_results(
            [cell], parent=self.parent, producer="validate",
            run_id="validate-expected-exit-chaos-divergence", tree=self.tree,
            emitted_at="2026-09-25T12:00:00Z", quarantine=refused,
            raw_inputs=[received.encode()], provenance=provenance,
        )
        self.assertEqual((admitted, refused, provenance), (rows, [], [[0]]))

        pressure = copy.deepcopy(cell)
        pressure.pop("attempt")
        pressure["run_index"] = 0
        payload = self._project([pressure])[0]["series"]
        self.assertEqual(series._pressure_outer_kind(payload), "product")
        self.assertEqual(series._pressure_inner_kinds(payload), ["pass", "product"])
        self.assertEqual(payload["pressure_evidence"]["evidence_sha256"],
                         series._cell_result_evidence_sha256(pressure))

    def test_only_the_runner_mistyped_chaos_divergence_is_retyped(self):
        base = self._chaos_divergence_cell()
        self.assertEqual(series._retype_chaos_divergence(base),
                         {**base, "result": "determinism-failure"})

        def mutant(change):
            cell = copy.deepcopy(base)
            change(cell)
            return cell

        def failed_match_sibling(cell):
            # A match the runner FAILed by its exit: the crash-error red.
            cell["attempts"][0].update(outcome="FAIL", status=None, signal=9)

        def noncanonical(index):
            def change(cell):
                attempt = cell["attempts"][index]
                report = json.loads(attempt["verification_report"])
                report["comparison"]["virtualize_time"] = False
                self._replace_comparison_report(attempt, report)
            return change

        for label, change in (
            ("verify mode", lambda cell: cell.update(mode="verify")),
            ("replay mode", lambda cell: cell.update(mode="replay")),
            ("already a determinism-failure",
             lambda cell: cell.update(result="determinism-failure")),
            ("no failure class", lambda cell: cell.update(failure_class=None)),
            ("ERROR outcome", lambda cell: cell.update(outcome="ERROR")),
            ("row error kind", lambda cell: cell.update(error_kind="wall-timeout")),
            ("attempt error kind",
             lambda cell: cell["attempts"][1].update(error_kind="wall-timeout")),
            ("timed-out attempt", lambda cell: cell["attempts"][0].update(timed_out=True)),
            ("backend parity witness", lambda cell: cell.update(backend_parity={})),
            ("every seed matched",
             lambda cell: cell.update(attempts=[cell["attempts"][0]])),
            ("no attempts", lambda cell: cell.update(attempts=[])),
            ("attempts not a list", lambda cell: cell.update(attempts={})),
            ("attempt not an object",
             lambda cell: cell.update(attempts=[cell["attempts"][1], "x"])),
            ("noncanonical divergence", noncanonical(1)),
            ("noncanonical match beside the divergence", noncanonical(0)),
            ("failed match beside the divergence", failed_match_sibling),
        ):
            with self.subTest(label):
                cell = mutant(change)
                self.assertIs(series._retype_chaos_divergence(cell), cell)

        # Still refused, with the refusal _cell_outcome gives the row as received.
        crash = self._validate_diverged_cell(1)
        crash["result"] = "crash-error"
        for label, cell, refusal in (
            ("verify divergence typed crash-error", crash,
             "no supporting canonical typed attempt"),
            ("chaos population failure",
             mutant(lambda cell: cell.update(attempts=[cell["attempts"][0]])),
             "no supporting canonical typed attempt"),
            ("failed match beside the divergence", mutant(failed_match_sibling),
             "matched attempt the runner failed carries a divergence coordinate"),
        ):
            with self.subTest(label):
                with self.assertRaisesRegex(series.MalformedRow, refusal) as received:
                    series._cell_outcome(cell)
                with self.assertRaises(series.MalformedRow) as projected:
                    self._project_validate(cell, "mutant")
                self.assertEqual(str(projected.exception), str(received.exception))

        # A malformed attempt raises the refusal _cell_outcome raises for it.
        torn = mutant(lambda cell: cell["attempts"][1].update(
            verification_report_sha256="0" * 64))
        for project in (series._retype_chaos_divergence, series._cell_outcome):
            with self.subTest(project.__name__), self.assertRaisesRegex(
                series.MalformedRow, "cell attempt 2 verification report hash mismatch",
            ):
                project(torn)

    #: The reason Hermit's runner gives an attempt whose verification report
    #: diverged, whatever the report's comparison.
    DIVERGED_REPORT_REASON = (
        "canonical verification did not match: "
        "verified=false verdict=diverged bitwise_parity=false"
    )

    def _noncanonical_divergence_cell(self):
        """A verify cell as Hermit's runner writes one whose two runs diverged
        under the comparison its recipe weakened.

        The recipe sets ``compare_io_buffers: false``, so the runner passed
        ``--no-detlog-io-buffers``, recorded its relaxation, and wrote the
        report with the canonical comparison but for ``compare_io_buffers``.
        A verify cell runs one attempt. The runner FAILed it over Hermit's
        exit status 1 with the reason above and typed the cell a
        determinism-failure. A local pressure run wrote this shape, with
        both relaxations, for system-utils/procfs-sanitized-paths on SaBRe.
        """
        cell = self._validate_diverged_cell(1)
        cell["reason"] = self.DIVERGED_REPORT_REASON
        cell["relaxations"] = [
            series.NO_DETLOG_IO_BUFFERS_RELAXATION_PREFIX
            + "the guest's raw procfs bytes legitimately differ between runs",
            "--no-rcb-time: the guest's data-dependent work would move virtual time",
        ]
        attempt = cell["attempts"][0]
        attempt["reason"] = self.DIVERGED_REPORT_REASON
        report = json.loads(attempt["verification_report"])
        report["comparison"]["compare_io_buffers"] = False
        report["compared_log_messages"] = {"left": 810, "right": 809}
        report["guest_exit_code"] = 0
        self._replace_comparison_report(attempt, report)
        return cell

    @staticmethod
    def _noncanonical_divergence_evidence(attempt):
        """The no_verdict_evidence entry written for ``attempt``."""
        return {
            "index": attempt["index"],
            "kind": "noncanonical_divergence",
            "attempt_outcome": "FAIL",
            "disposition": "no_result",
            "error_kind": None,
            "status": attempt["status"],
            "signal": None,
            "timed_out": False,
            "verification_report_sha256": attempt["verification_report_sha256"],
        }

    def test_noncanonical_divergence_is_written_as_the_no_result_hermit_counts(self):
        cell = self._noncanonical_divergence_cell()
        received = json.dumps(cell, sort_keys=True)
        rows = self._project_validate(cell, "noncanonical-divergence")
        self.assertEqual(json.dumps(cell, sort_keys=True), received)
        payload = rows[0]["series"]
        self.assertEqual(
            (payload["outcome"], payload["result"], payload["failure_class"]),
            ("no_result", None, "no_result"),
        )
        # The divergence is no canonical verdict, so it has no position.
        self.assertNotIn("coordinates", payload)
        self.assertNotIn("first_divergent_messages", payload)
        self.assertEqual(payload["no_verdict_evidence"], {
            "evidence_sha256": series._cell_result_evidence_sha256(cell),
            "attempts": [self._noncanonical_divergence_evidence(cell["attempts"][0])],
        })
        self.assertEqual(payload["detcore_tree"], self.detcore_tree)

        # Through the quarantining writer and the shared checker unstubbed:
        # admitted, not refused.
        refused, provenance = [], []
        admitted = series._rows_from_cell_results(
            [cell], parent=self.parent, producer="validate",
            run_id="validate-expected-exit-noncanonical-divergence",
            tree=self.tree, emitted_at="2026-09-25T12:00:00Z",
            quarantine=refused, raw_inputs=[received.encode()],
            provenance=provenance,
        )
        self.assertEqual((admitted, refused, provenance), (rows, [], [[0]]))

        # Pressure evidence keeps the attempt, which is not canonical, so the
        # sample is neither a pass nor a red.
        pressure = copy.deepcopy(cell)
        pressure.pop("attempt")
        pressure["run_index"] = 0
        pressure_rows = self._project([pressure])
        series.validate_rows(pressure_rows)
        payload = pressure_rows[0]["series"]
        self.assertEqual(
            [
                (attempt["comparison"]["verdict"], attempt["comparison"]["canonical"])
                for attempt in payload["pressure_evidence"]["attempts"]
            ],
            [("diverged", False)],
        )
        self.assertEqual(series._pressure_outer_kind(payload), "no_result")
        self.assertEqual(series._pressure_inner_kinds(payload), ["no_result"])

    def test_only_a_divergence_no_canonical_comparison_saw_is_written_as_no_result(self):
        base = self._noncanonical_divergence_cell()
        self.assertEqual(series._cell_outcome(base), "no_result")

        def mutant(change):
            cell = copy.deepcopy(base)
            change(cell)
            return cell

        def reported(change):
            def apply(cell):
                attempt = cell["attempts"][0]
                report = json.loads(attempt["verification_report"])
                change(report)
                self._replace_comparison_report(attempt, report)
            return apply

        def both(*changes):
            def apply(cell):
                for change in changes:
                    change(cell)
            return apply

        # The canonical comparison's divergence supports the row's own result.
        self.assertEqual(series._cell_outcome(mutant(reported(
            lambda report: report["comparison"].update(compare_io_buffers=True),
        ))), "diverged")

        no_support = "framework result has no supporting canonical typed attempt"
        not_run = self._attempt("no_result", index="1")
        # An attempt whose first run Hermit rejected: a FAIL with no error kind
        # over a nonzero status, whose report has no verdict.
        rejected = self._attempt("no_result", index="1")
        report = json.loads(rejected["verification_report"])
        report.update(
            no_result_reason={"kind": "first_run_rejected", "exit_code": 17, "signal": None},
            guest_exit_code=17,
        )
        self._replace_comparison_report(rejected, report)
        rejected.update(outcome="FAIL", error_kind=None, status=17)
        second = copy.deepcopy(base["attempts"][0])
        second["index"] = "2"
        # The LiteInst shape: the runner FAILs a verified match below bitwise
        # parity over status 0 and types the cell a crash-error. It is out of
        # scope here and keeps its refusal.
        relaxed_match = self._attempt()
        report = json.loads(relaxed_match["verification_report"])
        report["comparison"]["compare_io_buffers"] = False
        report.update(bitwise_parity=False, compared_log_messages={"left": 4317, "right": 4317})
        self._replace_comparison_report(relaxed_match, report)
        relaxed_match.update(
            outcome="FAIL",
            reason="canonical verification did not match: "
            "verified=true verdict=matched bitwise_parity=false",
        )

        def relaxed_match_cell(cell):
            cell.update(result="crash-error", reason=relaxed_match["reason"])
            for key in list(cell):
                if key.startswith("first_divergent_"):
                    cell[key] = None
            cell["attempts"] = [relaxed_match]

        def matched_report(cell):
            # Only the report turns into a match; no runner writes it under a
            # row and reason that still say it diverged.
            attempt = cell["attempts"][0]
            report = json.loads(attempt["verification_report"])
            report.update(verdict="matched", verified=True, bitwise_parity=True)
            for key in list(report):
                if key.startswith("first_divergent_"):
                    report[key] = None
                    attempt[key] = None
            self._replace_comparison_report(attempt, report)

        for label, change, refusal in (
            ("no --no-detlog-io-buffers relaxation",
             lambda cell: cell.update(relaxations=cell["relaxations"][1:]), no_support),
            ("a comparison weaker in another field too",
             reported(lambda report: report["comparison"].update(strip_lines=True)),
             no_support),
            ("two attempts", lambda cell: cell["attempts"].append(second), no_support),
            ("chaos", lambda cell: cell.update(mode="chaos"), no_support),
            ("replay",
             both(
                 lambda cell: cell.update(mode="replay", result="replay-failure"),
                 reported(lambda report: report["comparison"].update(virtualize_time=False)),
             ),
             no_support),
            ("an attempt PASS", lambda cell: cell["attempts"][0].update(outcome="PASS"),
             no_support),
            ("an attempt status 0", lambda cell: cell["attempts"][0].update(status=0),
             no_support),
            ("a timed-out attempt",
             lambda cell: cell["attempts"][0].update(timed_out=True), no_support),
            ("attempt error kind",
             lambda cell: cell["attempts"][0].update(error_kind="infrastructure"), no_support),
            ("row error kind", lambda cell: cell.update(error_kind="wall-timeout"), no_support),
            ("a not-run attempt", lambda cell: cell.update(attempts=[not_run]), no_support),
            ("a first-run-rejected attempt",
             lambda cell: cell.update(attempts=[rejected]), no_support),
            ("a matched report", matched_report, no_support),
            ("replay-failure in verify", lambda cell: cell.update(result="replay-failure"),
             no_support),
            ("parity-failure", lambda cell: cell.update(result="parity-failure"),
             "parity-failure has no backend parity witness"),
            ("ERROR outcome", lambda cell: cell.update(outcome="ERROR"),
             "outcome disagrees with its recorded classification"),
            ("no failure class", lambda cell: cell.update(failure_class=None),
             "no recognised result/failure_class pair"),
            ("a verified match the runner FAILed below bitwise parity", relaxed_match_cell,
             "cell attempt 1 matched report is internally inconsistent"),
        ):
            with self.subTest(label):
                cell = mutant(change)
                with self.assertRaisesRegex(series.MalformedRow, refusal) as received:
                    series._cell_outcome(cell)
                with self.assertRaises(series.MalformedRow) as projected:
                    self._project_validate(cell, "mutant")
                self.assertEqual(str(projected.exception), str(received.exception))

        # The rest of the shape, which _cell_outcome refuses or decides
        # before it asks.
        dispositions = ["no_result"]
        self.assertTrue(series._noncanonical_divergence(base, dispositions))
        stripped = {**base, "relaxations": [
            *base["relaxations"],
            series.STRIPPED_COMPARATOR_RELAXATION_PREFIX + "the guest prints addresses",
        ]}

        def with_comparison(comparison):
            attempt = copy.deepcopy(base["attempts"][0])
            report = json.loads(attempt["verification_report"])
            report["comparison"] = comparison
            self._replace_comparison_report(attempt, report)
            return {**base, "attempts": [attempt]}

        canonical = json.loads(base["attempts"][0]["verification_report"])["comparison"]
        canonical = {**canonical, "compare_io_buffers": True}
        for label, row, given in (
            ("a report with no comparison", with_comparison(None), dispositions),
            ("the canonical comparison", with_comparison(canonical), dispositions),
            ("backend parity witness", {**base, "backend_parity": {}}, dispositions),
            ("not a product failure", {**base, "failure_class": "no_result"}, dispositions),
            ("not a FAIL", {**base, "outcome": "ERROR"}, dispositions),
            ("declared stripped comparator", stripped, dispositions),
            ("two attempts", {**base, "attempts": [*base["attempts"], second]}, dispositions),
            ("two dispositions", base, dispositions * 2),
            ("a canonical divergence", base, ["diverged"]),
            ("a stripped divergence", base, ["stripped_diverged"]),
        ):
            with self.subTest(label):
                self.assertFalse(series._noncanonical_divergence(row, given))

    def test_an_oom_no_result_keeps_the_result_the_runner_wrote(self):
        # oom is the other result that projects no_result. The writer rewrites
        # only the product failure that _noncanonical_divergence projects to
        # no_result, so an oom row keeps its result, in a comparison mode or not.
        for mode in ("naked", "custom"):
            with self.subTest(mode):
                cell = self._cell(0, outcome="FAIL")
                cell.update(mode=mode, result="oom", failure_class="no_result")
                if mode == "naked":
                    cell["backend"] = None
                payload = self._project([cell])[0]["series"]
                self.assertEqual(
                    (payload["outcome"], payload["result"], payload["failure_class"]),
                    ("no_result", "oom", "no_result"),
                )
        with self.subTest("verify"):
            cell = self._validate_no_verdict_cell(1, timed_out=False, backend="ptrace")
            cell.update(outcome="FAIL", result="oom")
            payload = self._project_validate(cell, "oom")[0]["series"]
            self.assertEqual(
                (payload["outcome"], payload["result"], payload["failure_class"]),
                ("no_result", "oom", "no_result"),
            )

    def _timed_out_match_cell(self):
        """A chaos cell as Hermit's runner writes one whose last seed matched
        as the cell ran out of CPU.

        The first seed matched. The second seed's two runs matched as well,
        but when the runner reaped that process it found the cell over its
        CPU budget, so it FAILed the attempt as a CPU timeout over the guest's
        normal status 0, with the timeout's reason, and typed the row a
        timeout. A local validation wrote exactly this shape for
        c-programs/fp-reduction-nondeterminism under chaos on ptrace.
        """
        cell = self._validate_cell(1, outcome="FAIL")
        cell.update(
            mode="chaos", result="timeout", failure_class="no_result",
            error_kind="cpu-timeout",
        )
        timed_out = self._attempt(index="2")
        timed_out.update(
            outcome="FAIL", error_kind="cpu-timeout", timed_out=True,
            reason="cell exceeded 46 CPU s",
        )
        cell["attempts"] = [self._attempt(), timed_out]
        return cell

    @staticmethod
    def _killed_by_timeout(attempt, error_kind):
        """The runner killed the attempt's process when its timeout fired."""
        attempt.update(
            outcome="FAIL", timed_out=True, status=None, signal=9, error_kind=error_kind,
            reason=(
                "cell exceeded 138 wall s backstop (46 s CPU budget)"
                if error_kind == "wall-timeout" else "cell exceeded 46 CPU s"
            ),
        )

    def _declared_timed_out_match_cell(self):
        """A declared exit-7 verify cell whose second attempt matched and was
        killed at the wall-clock backstop: the kill's signal, not Hermit's
        status 7, is what the runner recorded."""
        cell = self._declared_exit_7()
        cell.update(
            outcome="FAIL", result="timeout", failure_class="no_result",
            error_kind="wall-timeout",
        )
        killed = copy.deepcopy(cell["attempts"][0])
        killed["index"] = "2"
        self._killed_by_timeout(killed, "wall-timeout")
        cell["attempts"].append(killed)
        return cell

    def test_timed_out_match_is_written_as_its_timeout(self):
        cell = self._timed_out_match_cell()
        received = json.dumps(cell, sort_keys=True)
        rows = self._project_validate(cell, "timed-out-match")
        self.assertEqual(json.dumps(cell, sort_keys=True), received)
        payload = rows[0]["series"]
        self.assertEqual(
            (payload["outcome"], payload["result"], payload["failure_class"]),
            ("timeout", "timeout", "no_result"),
        )
        self.assertEqual(payload["no_verdict_evidence"]["attempts"], [{
            "index": "2",
            "kind": "timed_out_match",
            "attempt_outcome": "FAIL",
            "disposition": "timeout",
            "error_kind": "cpu-timeout",
            "status": 0,
            "signal": None,
            "timed_out": True,
            "verification_report_sha256": cell["attempts"][1]["verification_report_sha256"],
        }])

        # Through the quarantining writer and the shared checker unstubbed:
        # admitted, not refused.
        refused, provenance = [], []
        admitted = series._rows_from_cell_results(
            [cell], parent=self.parent, producer="validate",
            run_id="validate-expected-exit-timed-out-match", tree=self.tree,
            emitted_at="2026-09-25T12:00:00Z", quarantine=refused,
            raw_inputs=[received.encode()], provenance=provenance,
        )
        self.assertEqual((admitted, refused, provenance), (rows, [], [[0]]))

        # Pressure evidence keeps the match as the timeout's: never canonical,
        # so never a pass.
        pressure = copy.deepcopy(cell)
        pressure.pop("attempt")
        pressure["run_index"] = 0
        pressure_rows = self._project([pressure])
        series.validate_rows(pressure_rows)
        payload = pressure_rows[0]["series"]
        self.assertEqual(
            [
                (attempt["comparison"]["verdict"], attempt["comparison"]["canonical"])
                for attempt in payload["pressure_evidence"]["attempts"]
            ],
            [("matched", True), ("matched", False)],
        )
        self.assertEqual(series._pressure_outer_kind(payload), "no_result")
        self.assertEqual(series._pressure_inner_kinds(payload), ["pass", "no_result"])

    def test_every_timed_out_match_the_runner_writes_is_admitted(self):
        def killed(error_kind):
            def change(cell):
                self._killed_by_timeout(cell["attempts"][1], error_kind)
                cell["error_kind"] = error_kind
            return change

        def noncanonical(outcome):
            def change(cell):
                attempt = cell["attempts"][1]
                report = json.loads(attempt["verification_report"])
                report["comparison"]["virtualize_time"] = False
                self._replace_comparison_report(attempt, report)
                attempt["outcome"] = outcome
            return change

        for label, change in (
            ("verify mode", lambda cell: cell.update(mode="verify")),
            ("killed at the wall-clock backstop", killed("wall-timeout")),
            ("killed while the CPU budget is polled", killed("cpu-timeout")),
            ("nonzero status", lambda cell: cell["attempts"][1].update(status=3)),
            ("noncanonical report", noncanonical("FAIL")),
            # The runner refused the report's comparison policy as well.
            ("noncanonical report the runner refused", noncanonical("ERROR")),
            ("row without a result",
             lambda cell: cell.update(outcome="ERROR", result=None)),
            ("its only attempt", lambda cell: cell.update(attempts=cell["attempts"][1:])),
        ):
            with self.subTest(label):
                cell = self._timed_out_match_cell()
                change(cell)
                payload = self._project_validate(cell, "timed-out-match")[0]["series"]
                timed_out = cell["attempts"][-1]
                self.assertEqual(payload["no_verdict_evidence"]["attempts"], [{
                    "index": "2",
                    "kind": "timed_out_match",
                    "attempt_outcome": timed_out["outcome"],
                    "disposition": "timeout",
                    "error_kind": timed_out["error_kind"],
                    "status": timed_out["status"],
                    "signal": timed_out["signal"],
                    "timed_out": True,
                    "verification_report_sha256": timed_out["verification_report_sha256"],
                }])

        with self.subTest("prerequisite failure"):
            # A skipped row retains no no-verdict evidence, but its timed-out
            # match still supports it.
            cell = self._timed_out_match_cell()
            cell.update(outcome="ERROR", result=None,
                        failure_class="understood_prerequisite_failure",
                        attempts=cell["attempts"][1:])
            payload = self._project_validate(cell, "timed-out-match")[0]["series"]
            self.assertEqual(
                (payload["outcome"], payload["result"], payload["failure_class"]),
                ("skipped", None, "understood_prerequisite_failure"),
            )
            self.assertNotIn("no_verdict_evidence", payload)

        with self.subTest("declared exit killed at the wall-clock backstop"):
            cell = self._declared_timed_out_match_cell()
            payload = self._project_validate(cell, "declared-timed-out-match")[0]["series"]
            self.assertEqual(
                (payload["outcome"], payload["result"], payload["failure_class"]),
                ("timeout", "timeout", "no_result"),
            )
            self.assertEqual(
                [
                    (attempt["kind"], attempt["status"], attempt["signal"])
                    for attempt in payload["no_verdict_evidence"]["attempts"]
                ],
                [("timed_out_match", None, 9)],
            )

    def test_timed_out_match_supports_only_a_timeout_or_no_result(self):
        for result, failure_class, outcome in (
            ("pass", None, "PASS"),
            ("determinism-failure", "product_failure", "FAIL"),
            ("crash-error", "product_failure", "FAIL"),
            ("oom", "no_result", "FAIL"),
        ):
            for label, make in (
                ("chaos", self._timed_out_match_cell),
                ("declared exit", self._declared_timed_out_match_cell),
            ):
                with self.subTest(result=result, cell=label):
                    # Beside a match that would support a pass.
                    cell = make()
                    cell.update(outcome=outcome, result=result, failure_class=failure_class)
                    with self.assertRaises(series.MalformedRow) as received:
                        series._cell_outcome(cell)
                    self.assertEqual(str(received.exception), (
                        f"row retaining a timed-out matched attempt records result {result!r} "
                        f"with failure_class {failure_class!r}, but a timed-out attempt "
                        "supports only the result timeout or none"
                    ))
                    with self.assertRaises(series.MalformedRow) as projected:
                        self._project_validate(cell, "mutant")
                    self.assertEqual(str(projected.exception), str(received.exception))

    def test_a_timed_out_match_the_runner_never_writes_is_refused(self):
        inconsistent = "cell attempt 2 matched report is internally inconsistent"
        for label, fields, refusal in (
            ("passed", {"outcome": "PASS"}, inconsistent),
            ("not timed out", {"timed_out": False}, inconsistent),
            ("no error kind", {"error_kind": None}, inconsistent),
            ("not a timeout's error kind",
             {"error_kind": "incomplete-verification-evidence"}, inconsistent),
            ("neither status nor signal", {"status": None}, inconsistent),
            ("both status and signal", {"signal": 9}, inconsistent),
            ("negative status", {"status": -1}, inconsistent),
            ("signal 0", {"status": None, "signal": 0}, inconsistent),
            # The runner replaces a timed-out attempt's reason with the
            # timeout's, so it never also fails the attempt's declared stdout.
            ("reason of a failed declared stdout",
             {"reason": self._exact_stdout_reason()},
             "cell attempt 2 reason claims a failed declared expected output that "
             "contradicts its evidence: only a verify or replay cell declares expected stdout, "
             "and this is a chaos cell"),
        ):
            with self.subTest(label):
                cell = self._timed_out_match_cell()
                cell["attempts"][1].update(fields)
                with self.assertRaises(series.MalformedRow) as received:
                    series._cell_outcome(cell)
                self.assertEqual(str(received.exception), refusal)

        with self.subTest("verify reason of a failed declared stdout"):
            cell = self._timed_out_match_cell()
            cell["mode"] = "verify"
            cell["attempts"][1]["reason"] = self._exact_stdout_reason()
            with self.assertRaisesRegex(
                series.MalformedRow,
                "^cell attempt 2 reason claims a failed declared expected output that "
                "contradicts its evidence: the runner fails declared stdout only for an "
                "attempt that completed",
            ):
                series._cell_outcome(cell)

        # The kill's signal stands in for Hermit's status only on a timed-out
        # match: a declared exit that Hermit's status does not report is still
        # refused when the attempt did not time out, or timed out with a
        # status.
        not_reported = (
            "cell attempt 2 matched report is internally inconsistent: Hermit's status "
            "does not report the guest's disposition"
        )
        for label, fields in (
            ("killed without a timeout",
             {"outcome": "FAIL", "timed_out": False, "error_kind": None}),
            ("timed out with another status", {"status": 0, "signal": None}),
        ):
            with self.subTest(label):
                cell = self._declared_timed_out_match_cell()
                cell["attempts"][1].update(fields)
                with self.assertRaises(series.MalformedRow) as received:
                    series._cell_outcome(cell)
                self.assertEqual(str(received.exception), not_reported)

    #: Hermit's runner's reason for a pass it failed because the cell's SaBRe
    #: execution path was not eligible (``execution_path_ineligible`` in
    #: ``runner.rs``).
    INELIGIBLE_PATH_REASON = (
        "SaBRe execution path is incomplete or used fallback/native sites"
    )
    #: The shared objects in which one real SaBRe execution ran trusted sites.
    TRUSTED_SHARED_OBJECTS = ("/usr/lib64/ld-linux-x86-64.so.2", "/usr/lib64/libc.so.6")
    SABRE_VERIFY_ARGV = (
        "/src/hermit", "--log", "info", "run", "--base-env=minimal", "--backend", "sabre",
        "--strict", "--verify-strict", "--verify", "--verify-json", "/results/verify-1.json",
        "--", "/results/fixtures/program",
    )

    @classmethod
    def _path_record(cls, *, observed=True, fallback=0, trusted=34):
        """One execution's SaBRe path evidence, the line as Hermit writes it."""
        return json.dumps({
            "schema": 1,
            "guest_rpc_observed": observed,
            "ptrace_fallback_sites": fallback,
            "trusted_shared_object_sites": trusted,
            "trusted_shared_objects": list(cls.TRUSTED_SHARED_OBJECTS) if trusted else [],
        }, separators=(",", ":"))

    @staticmethod
    def _set_path_evidence(attempt, text):
        """The attempt retains ``text`` as its SaBRe path evidence."""
        attempt["sabre_path_evidence"] = text
        attempt["sabre_path_evidence_sha256"] = (
            None if text is None else hashlib.sha256(text.encode()).hexdigest()
        )

    @staticmethod
    def _summarize_path(cell):
        """Give the row Hermit's ``execution_path`` for its attempts' evidence.

        As ``summarize_sabre_path_evidence`` in Hermit's ``runner.rs`` writes
        it: every retained record, two expected of each attempt (all of them
        verify on SaBRe here), and eligible only when complete, with the
        guest's RPC observed in every execution and no ptrace-fallback or
        trusted-shared-object site.
        """
        records = [
            json.loads(line)
            for attempt in cell["attempts"]
            for line in (attempt["sabre_path_evidence"] or "").splitlines()
        ]
        expected = 2 * len(cell["attempts"])
        complete = len(records) == expected
        observed = complete and all(record["guest_rpc_observed"] for record in records)
        fallback = sum(record["ptrace_fallback_sites"] for record in records)
        trusted = sum(record["trusted_shared_object_sites"] for record in records)
        cell["execution_path"] = {
            "schema": 1,
            "expected_execution_count": expected,
            "complete": complete,
            "execution_count": len(records),
            "guest_rpc_observed": observed,
            "ptrace_fallback_sites": fallback,
            "trusted_shared_object_sites": trusted,
            "trusted_shared_objects": sorted({
                name for record in records for name in record["trusted_shared_objects"]
            }),
            "eligible": complete and observed and fallback == 0 and trusted == 0,
            "executions": records,
        }
        return cell

    def test_split_verify_sabre_attempts_expect_two_executions(self):
        # hermit a1c953ebc (2026-10-13) runs a verify cell as two harness-
        # launched `--verify-run` executions; Hermit's summary expects two of
        # them, as for `--verify`. Counting one quarantined pressure run
        # pressure-03c24d69a8e7-20261015T004056Z's compat pathchk and uuidgen
        # SaBRe cells as MalformedRow.
        record = self._path_record(trusted=0)
        split = (
            "/src/hermit", "--backend", "sabre", "--verify-strict", "--verify-run",
            "--", "/results/fixtures/program",
        )
        plain = ("/src/hermit", "run", "--backend", "sabre", "--", "/results/fixtures/program")
        for argv, expected in ((self.SABRE_VERIFY_ARGV, 2), (split, 2), (plain, 1)):
            with self.subTest(argv=argv):
                attempt = {"argv": list(argv)}
                self._set_path_evidence(attempt, f"{record}\n" * expected)
                summary = series._summarize_sabre_path_evidence([attempt])
                self.assertEqual(summary["expected_execution_count"], expected)
                self.assertEqual(summary["execution_count"], expected)
                self.assertTrue(summary["complete"])
                self.assertTrue(summary["guest_rpc_observed"])
                self.assertTrue(summary["eligible"])

    def _on_ineligible_sabre_path(self, cell):
        """Hermit's runner's row for the passing verify ``cell`` run on SaBRe
        when the execution path it reports is not eligible.

        Every attempt selects SaBRe, and each of its two executions reports
        34 sites run natively in trusted shared objects. The runner keeps
        every passing attempt as it was and fails the cell's pass as a
        crash-error with the execution-path reason.
        """
        for attempt in cell["attempts"]:
            argv = list(attempt.get("argv") or self.SABRE_VERIFY_ARGV)
            if "--backend" not in argv:
                separator = argv.index("--")
                argv[separator:separator] = ["--backend", "sabre"]
            attempt["argv"] = argv
            record = self._path_record()
            self._set_path_evidence(attempt, f"{record}\n{record}\n")
        cell.update(
            backend="sabre", outcome="FAIL", result="crash-error",
            failure_class="product_failure", error_kind=None,
            reason=self.INELIGIBLE_PATH_REASON,
        )
        return self._summarize_path(cell)

    def _ineligible_path_cell(self):
        """A verify cell as Hermit's runner writes one that passed on SaBRe
        over an execution path that was not eligible.

        The attempt's canonical comparison matched and the guest exited 0, so
        the runner passed it; the trusted-shared-object sites its two
        executions reported then made the runner fail the pass. A pressure
        test wrote 83 rows of this shape, among them
        determinism-stress-c/pid-tid-identity under verify on SaBRe.
        """
        return self._on_ineligible_sabre_path(self._validate_cell(1))

    @staticmethod
    def _ineligible_path_evidence(attempt):
        """The no-verdict evidence a series row retains for ``attempt``."""
        return {
            "index": attempt["index"],
            "kind": "ineligible_execution_path",
            "attempt_outcome": "PASS",
            "disposition": "errored",
            "error_kind": None,
            "status": attempt["status"],
            "signal": attempt["signal"],
            "timed_out": False,
            "verification_report_sha256": attempt["verification_report_sha256"],
        }

    def test_ineligible_path_fixture_is_what_hermit_wrote(self):
        # The retained evidence and the execution_path of the real
        # determinism-stress-c/pid-tid-identity row.
        cell = self._ineligible_path_cell()
        line = (
            '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,'
            '"trusted_shared_object_sites":34,"trusted_shared_objects":'
            '["/usr/lib64/ld-linux-x86-64.so.2","/usr/lib64/libc.so.6"]}\n'
        )
        self.assertEqual(cell["attempts"][0]["sabre_path_evidence"], line * 2)
        execution = {
            "schema": 1,
            "guest_rpc_observed": True,
            "ptrace_fallback_sites": 0,
            "trusted_shared_object_sites": 34,
            "trusted_shared_objects": list(self.TRUSTED_SHARED_OBJECTS),
        }
        self.assertEqual(cell["execution_path"], {
            "schema": 1,
            "expected_execution_count": 2,
            "complete": True,
            "execution_count": 2,
            "guest_rpc_observed": True,
            "ptrace_fallback_sites": 0,
            "trusted_shared_object_sites": 68,
            "trusted_shared_objects": list(self.TRUSTED_SHARED_OBJECTS),
            "eligible": False,
            "executions": [execution, execution],
        })

    def test_ineligible_execution_path_is_written_as_its_crash_error(self):
        cell = self._ineligible_path_cell()
        received = json.dumps(cell, sort_keys=True)
        rows = self._project_validate(cell, "ineligible-path")
        self.assertEqual(json.dumps(cell, sort_keys=True), received)
        payload = rows[0]["series"]
        self.assertEqual(payload["cell"], "applications/git-repository-workflow/verify/sabre")
        self.assertEqual(
            (payload["outcome"], payload["result"], payload["failure_class"]),
            ("errored", "crash-error", "product_failure"),
        )
        self.assertEqual(
            payload["no_verdict_evidence"]["attempts"],
            [self._ineligible_path_evidence(cell["attempts"][0])],
        )
        self.assertNotIn("comparison", payload)
        self.assertNotIn("coordinates", payload)

        # Through the quarantining writer and the shared checker unstubbed:
        # admitted, not refused for want of a supporting attempt.
        refused, provenance = [], []
        admitted = series._rows_from_cell_results(
            [cell], parent=self.parent, producer="validate",
            run_id="validate-expected-exit-ineligible-path", tree=self.tree,
            emitted_at="2026-09-25T12:00:00Z", quarantine=refused,
            raw_inputs=[received.encode()], provenance=provenance,
        )
        self.assertEqual((admitted, refused, provenance), (rows, [], [[0]]))

        # Pressure evidence keeps the match, never canonical, so never a pass.
        pressure = copy.deepcopy(cell)
        pressure.pop("attempt")
        pressure["run_index"] = 0
        pressure_rows = self._project([pressure])
        series.validate_rows(pressure_rows)
        payload = pressure_rows[0]["series"]
        self.assertEqual(
            [
                (attempt["comparison"]["verdict"], attempt["comparison"]["canonical"])
                for attempt in payload["pressure_evidence"]["attempts"]
            ],
            [("matched", False)],
        )
        self.assertEqual(series._pressure_outer_kind(payload), "product")
        self.assertEqual(series._pressure_inner_kinds(payload), ["no_result"])

    def test_every_ineligible_execution_path_the_runner_writes_is_admitted(self):
        record = self._path_record()
        clean = self._path_record(trusted=0)

        def evidence(*texts):
            def change(cell):
                for attempt, text in zip(cell["attempts"], texts, strict=True):
                    self._set_path_evidence(attempt, text)
                self._summarize_path(cell)
            return change

        def second_attempt(cell):
            attempt = copy.deepcopy(cell["attempts"][0])
            attempt["index"] = "2"
            cell["attempts"].append(attempt)
            self._summarize_path(cell)

        fallback = self._path_record(fallback=3, trusted=0)
        unobserved = self._path_record(observed=False, trusted=0)
        # serde_json keeps the last of a repeated field, after reading the
        # one it replaces: here 126 nested arrays inside the record, the
        # deepest its recursion limit admits.
        nested = "[" * 126 + "]" * 126
        for label, change in (
            ("ptrace-fallback sites", evidence(f"{fallback}\n{fallback}\n")),
            ("the guest's RPC not observed", evidence(f"{unobserved}\n{clean}\n")),
            ("one of two executions reported", evidence(f"{clean}\n")),
            ("no execution reported", evidence(None)),
            ("an empty report", evidence("")),
            ("records ended by CRLF", evidence(f"{record}\r\n{record}\r\n")),
            ("the last record without its newline", evidence(f"{record}\n{record}")),
            ("a repeated field, the last kept", evidence(
                clean.replace('"guest_rpc_observed":true',
                              '"guest_rpc_observed":true,"guest_rpc_observed":false')
                + f"\n{clean}\n")),
            ("a repeated field nested as deep as serde_json reads", evidence(
                record.replace('{"schema":1,',
                               '{"schema":1,"trusted_shared_objects":' + nested + ',')
                + f"\n{record}\n")),
            ("two passing attempts", second_attempt),
        ):
            with self.subTest(label):
                cell = self._ineligible_path_cell()
                change(cell)
                self.assertFalse(cell["execution_path"]["eligible"])
                payload = self._project_validate(cell, "ineligible-path")[0]["series"]
                self.assertEqual(
                    (payload["outcome"], payload["result"], payload["failure_class"]),
                    ("errored", "crash-error", "product_failure"),
                )
                self.assertEqual(
                    payload["no_verdict_evidence"]["attempts"],
                    [self._ineligible_path_evidence(attempt) for attempt in cell["attempts"]],
                )

        with self.subTest("declared guest signal"):
            # Two real rows of c-programs/nanosleep-threads-simple, whose
            # declared SIGSEGV Hermit's status reports as that signal.
            cell = self._on_ineligible_sabre_path(self._expected_exit_cell(
                status=None, signal=11, guest_exit_code=None, guest_signal=11,
                declared=(None, 11),
            ))
            payload = self._project_validate(cell, "declared-ineligible-path")[0]["series"]
            self.assertEqual(
                (payload["outcome"], payload["result"], payload["failure_class"]),
                ("errored", "crash-error", "product_failure"),
            )
            self.assertEqual(
                [
                    (attempt["kind"], attempt["status"], attempt["signal"])
                    for attempt in payload["no_verdict_evidence"]["attempts"]
                ],
                [("ineligible_execution_path", None, 11)],
            )

    def test_ineligible_execution_path_is_told_only_by_the_runners_row(self):
        # Without every field of the runner's row, the reason alone is not
        # the kind: each row below is read exactly as it was before, and
        # refused for want of a supporting attempt.
        for label, fields, projected in (
            ("another reason", {"reason": "verify exited with status 1"}, "errored"),
            ("no reason", {"reason": None}, "errored"),
            ("chaos mode", {"mode": "chaos"}, "errored"),
            ("ptrace", {"backend": "ptrace"}, "errored"),
            ("a determinism-failure",
             {"result": "determinism-failure"}, "diverged"),
        ):
            with self.subTest(label):
                cell = self._ineligible_path_cell()
                cell.update(fields)
                for attempt in cell["attempts"]:
                    if fields.get("backend") == "ptrace":
                        attempt["argv"][attempt["argv"].index("sabre")] = "ptrace"
                self.assertFalse(series._ineligible_execution_path(cell))
                with self.assertRaises(series.MalformedRow) as received:
                    series._cell_outcome(cell)
                self.assertEqual(str(received.exception), (
                    "framework result has no supporting canonical typed attempt: "
                    f"projected={projected!r} dispositions=['matched']"
                ))

    def test_an_ineligible_execution_path_the_runner_never_writes_is_refused(self):
        not_ineligible = (
            "row records a SaBRe execution-path failure, but its retained path evidence "
            "is not ineligible"
        )
        not_its_summary = (
            "the row's execution_path is not the summary of its retained SaBRe path evidence"
        )
        not_its_bytes = (
            "cell attempt 1's retained SaBRe path evidence is not the bytes its "
            "sabre_path_evidence_sha256 names"
        )
        not_sabre = "cell attempt 1's command line does not select the row's backend 'sabre'"
        clean = self._path_record(trusted=0)

        def attempt_edit(**fields):
            return lambda cell: cell["attempts"][0].update(fields)

        def path_edit(**fields):
            return lambda cell: cell["execution_path"].update(fields)

        def eligible(cell):
            self._set_path_evidence(cell["attempts"][0], f"{clean}\n{clean}\n")
            self._summarize_path(cell)

        def guest_backend(cell):
            argv = list(self.SABRE_VERIFY_ARGV)
            argv.remove("--backend")
            argv.remove("sabre")
            cell["attempts"][0]["argv"] = argv + ["--backend", "sabre"]

        def failed_by_exit(cell):
            cell["attempts"][0].update(
                outcome="FAIL", status=1, reason="verify exited with status 1",
            )

        def diverged(cell):
            attempt = self._attempt("diverged", record=98, syscall=41)
            attempt["argv"] = list(self.SABRE_VERIFY_ARGV)
            self._set_path_evidence(attempt, cell["attempts"][0]["sabre_path_evidence"])
            cell["attempts"] = [attempt]

        for label, change, refusal in (
            ("a row error kind", lambda cell: cell.update(error_kind="cpu-timeout"),
             "a SaBRe execution-path failure carries an error kind"),
            ("an eligible path", eligible, not_ineligible),
            ("ptrace selected", lambda cell: cell["attempts"][0]["argv"].__setitem__(
                cell["attempts"][0]["argv"].index("sabre"), "ptrace"), not_sabre),
            ("SaBRe named only to the guest", guest_backend, not_sabre),
            ("no command line", lambda cell: cell["attempts"][0].pop("argv"),
             "cell attempt 1 argv is not a list of strings"),
            ("evidence its digest does not name",
             attempt_edit(sabre_path_evidence_sha256="0" * 64), not_its_bytes),
            ("evidence without its digest",
             attempt_edit(sabre_path_evidence_sha256=None), not_its_bytes),
            ("a digest without its evidence",
             attempt_edit(sabre_path_evidence=None), not_its_bytes),
            ("evidence that is not text",
             attempt_edit(sabre_path_evidence=[clean]),
             "cell attempt 1 SaBRe path evidence and its digest must be strings or null"),
            ("a summary of other sites", path_edit(trusted_shared_object_sites=67),
             not_its_summary),
            # serde_json's Value equality tells 2 from 2.0 and true from 1.
            ("an execution count written as a float", path_edit(execution_count=2.0),
             not_its_summary),
            ("completeness written as a number", path_edit(complete=1), not_its_summary),
            ("no execution_path", lambda cell: cell.pop("execution_path"), not_its_summary),
            ("a divergence coordinate", lambda cell: cell.update(first_divergent_record=98),
             "row recording a SaBRe execution-path failure carries a divergence coordinate"),
            ("an attempt the runner failed by its exit", failed_by_exit,
             "row records a SaBRe execution-path failure, but not every attempt is a "
             "match the runner passed: dispositions=['failed_match']"),
            ("a diverged attempt", diverged,
             "row records a SaBRe execution-path failure, but not every attempt is a "
             "match the runner passed: dispositions=['diverged']"),
        ):
            with self.subTest(label):
                cell = self._ineligible_path_cell()
                change(cell)
                with self.assertRaises(series.MalformedRow) as received:
                    series._cell_outcome(cell)
                self.assertEqual(str(received.exception), refusal)

        with self.subTest("quarantined by the writer"):
            cell = self._ineligible_path_cell()
            cell["attempts"][0]["sabre_path_evidence_sha256"] = "0" * 64
            refused = []
            admitted = series._rows_from_cell_results(
                [cell], parent=self.parent, producer="validate",
                run_id="validate-expected-exit-ineligible-path", tree=self.tree,
                emitted_at="2026-09-25T12:00:00Z", quarantine=refused,
                raw_inputs=[json.dumps(cell, sort_keys=True).encode()],
            )
            self.assertEqual(admitted, [])
            self.assertEqual(
                [(record["error"], record["reason"]) for record in refused],
                [("MalformedRow", not_its_bytes)],
            )

    def test_sabre_path_evidence_is_read_as_hermits_path_evidence(self):
        # Hermit reads each line as a serde_json Value whose schema is 1, then
        # as its PathEvidence, which refuses an unknown field; every line it
        # refuses refuses the row.
        record = self._path_record()
        not_path_evidence = (
            "invalid SaBRe path-evidence record: it is not Hermit's PathEvidence"
        )
        not_schema_1 = "invalid SaBRe path-evidence record: schema must be 1"
        no_value = (
            "invalid SaBRe path-evidence record: Expecting value: line 1 column 1 (char 0)"
        )

        def edited(old, new):
            self.assertEqual(record.count(old), 1, old)
            return record.replace(old, new) + f"\n{record}\n"

        for label, text, refusal in (
            ("negative zero", edited('"ptrace_fallback_sites":0', '"ptrace_fallback_sites":-0'),
             not_path_evidence),
            ("a float count", edited('"ptrace_fallback_sites":0', '"ptrace_fallback_sites":0.0'),
             not_path_evidence),
            ("a negative count",
             edited('"ptrace_fallback_sites":0', '"ptrace_fallback_sites":-1'),
             not_path_evidence),
            ("a count past u64", edited(
                '"ptrace_fallback_sites":0', f'"ptrace_fallback_sites":{2**64}'),
             not_path_evidence),
            ("an infinite count",
             edited('"ptrace_fallback_sites":0', '"ptrace_fallback_sites":1e999'),
             "invalid SaBRe path-evidence record: number out of range: 1e999"),
            ("NaN", edited('"ptrace_fallback_sites":0', '"ptrace_fallback_sites":NaN'),
             "invalid SaBRe path-evidence record: NaN is not JSON"),
            ("an unknown field", edited('{"schema":1,', '{"schema":1,"sites":0,'),
             not_path_evidence),
            ("a missing field", edited('"ptrace_fallback_sites":0,', ""), not_path_evidence),
            ("schema 2", edited('"schema":1', '"schema":2'), not_schema_1),
            ("schema as text", edited('"schema":1', '"schema":"1"'), not_schema_1),
            ("schema 1.0", edited('"schema":1', '"schema":1.0'), not_schema_1),
            ("schema true", edited('"schema":1', '"schema":true'), not_schema_1),
            ("the guest's RPC as a number",
             edited('"guest_rpc_observed":true', '"guest_rpc_observed":1'), not_path_evidence),
            ("a shared object that is not text",
             edited('"/usr/lib64/libc.so.6"', "7"), not_path_evidence),
            ("a record that is not an object", f"[1]\n{record}\n", not_schema_1),
            ("an empty line between records", f"{record}\n\n{record}\n", no_value),
            ("an empty line after the last record", f"{record}\n{record}\n\n", no_value),
            ("nesting past serde_json's recursion limit", edited(
                '{"schema":1,', '{"schema":1,"trusted_shared_objects":'
                + "[" * 127 + "]" * 127 + ","),
             "invalid SaBRe path-evidence record: recursion limit exceeded"),
            ("an infinite number in a replaced repeated field", edited(
                '{"schema":1,', '{"schema":1,"ptrace_fallback_sites":1e999,'),
             "invalid SaBRe path-evidence record: number out of range: 1e999"),
            ("site counts that overflow a usize", "".join(
                self._path_record(fallback=2**64 - 1, trusted=0) + "\n" for _ in range(2)),
             "SaBRe path-evidence site counts overflow a usize"),
        ):
            with self.subTest(label):
                cell = self._ineligible_path_cell()
                self._set_path_evidence(cell["attempts"][0], text)
                with self.assertRaises(series.MalformedRow) as received:
                    series._cell_outcome(cell)
                self.assertEqual(str(received.exception), refusal)

        with self.subTest("an unpaired surrogate in a replaced repeated field"):
            cell = self._ineligible_path_cell()
            self._set_path_evidence(cell["attempts"][0], edited(
                '{"schema":1,', '{"schema":1,"trusted_shared_objects":["\\ud800"],'))
            with self.assertRaisesRegex(
                series.MalformedRow,
                "^invalid SaBRe path-evidence record: 'utf-8' codec can't encode "
                "character '\\\\ud800'",
            ):
                series._cell_outcome(cell)

    def test_projection_preserves_the_first_available_coordinate_per_axis(self):
        cell = self._cell(0, outcome="FAIL")
        cell["attempts"] = [
            self._attempt("diverged", syscall=37),
            self._attempt("diverged", record=98, syscall=41, index="2"),
        ]
        rows = self._project([cell])
        self.assertEqual(
            rows[0]["series"]["coordinates"],
            {"first_divergent_record": 98, "first_divergent_syscall": 37},
        )

        cell["attempts"] = [self._attempt("diverged")]
        rows = self._project([cell])
        self.assertNotIn("coordinates", rows[0]["series"])

    def test_projection_keeps_a_message_pair_from_one_attempt(self):
        cell = self._cell(0, outcome="FAIL")
        cell["attempts"] = [
            self._attempt("diverged", left="first left", right="first right"),
            self._attempt("diverged", left="second left", right="second right", index="2"),
        ]
        rows = self._project([cell])
        self.assertEqual(
            rows[0]["series"]["first_divergent_messages"],
            {"left": "first left", "right": "first right"},
        )

    def test_projection_makes_workspace_paths_in_divergence_messages_portable(self):
        owner_home = "/" + "home" + "/operator"
        workspace = owner_home + "/work/dev-hermit"
        cwd = workspace + "/worktrees/slots/pressure/ignored/pressure-fresh-one"
        cell = self._diverged_cell(
            0,
            record=98,
            syscall=37,
            scheduler_turn=4,
            virtual_nanoseconds=80_000,
            left=f'newfstatat("{cwd}", st_size=2234)',
            right=f'newfstatat("{cwd}/fixtures/program", st_size=2176)',
        )
        cell["cwd"] = cwd

        rows = self._project([cell])

        messages = rows[0]["series"]["first_divergent_messages"]
        self.assertEqual(
            messages,
            {
                "left": 'newfstatat("{{WORKSPACE_ROOT}}", st_size=2234)',
                "right": (
                    'newfstatat("{{WORKSPACE_ROOT}}/fixtures/program", '
                    'st_size=2176)'
                ),
            },
        )
        self.assertNotIn("/" + "home" + "/", json.dumps(rows))

    def test_projection_still_refuses_an_unattributed_owner_path(self):
        self._assert_divergence_message_is_refused(
            "path=" + "/" + "home" + "/someone-else/secret"
        )

    def test_projection_does_not_redact_a_workspace_sibling_prefix(self):
        owner_home = "/" + "home" + "/operator"
        workspace = owner_home + "/work/dev-hermit"
        self._assert_divergence_message_is_refused(
            f'path="{workspace}-foreign/secret"', cwd=workspace + "/hermit"
        )

    def test_projection_does_not_redact_an_owner_home_sibling_prefix(self):
        owner_home = "/" + "home" + "/operator"
        self._assert_divergence_message_is_refused(f'path="{owner_home}2/secret"')

    def test_projection_does_not_redact_cwd_inside_an_enclosing_path(self):
        owner_home = "/" + "home" + "/operator"
        cwd = owner_home + "/work/dev-hermit/hermit"
        self._assert_divergence_message_is_refused(f'path="/tmp{cwd}/secret"')

    def test_projection_does_not_redact_cwd_after_a_plain_character(self):
        owner_home = "/" + "home" + "/operator"
        cwd = owner_home + "/work/dev-hermit/hermit"
        self._assert_divergence_message_is_refused(f"path=x{cwd}/secret")

    def test_projection_does_not_treat_colon_as_a_trailing_path_boundary(self):
        self._assert_cwd_suffix_is_refused(":foreign")

    def test_projection_does_not_treat_semicolon_as_a_trailing_path_boundary(self):
        self._assert_cwd_suffix_is_refused(";foreign")

    def test_projection_does_not_treat_parenthesis_as_a_trailing_path_boundary(self):
        self._assert_cwd_suffix_is_refused(")foreign")

    def test_projection_does_not_treat_space_as_a_trailing_path_boundary(self):
        self._assert_cwd_suffix_is_refused(" foreign")

    def _assert_cwd_suffix_is_refused(self, suffix):
        owner_home = "/" + "home" + "/operator"
        cwd = owner_home + "/work/dev-hermit/hermit"
        self._assert_divergence_message_is_refused(f"path={cwd}{suffix}", cwd=cwd)

    def _assert_divergence_message_is_refused(self, message, *, cwd=None):
        owner_home = "/" + "home" + "/operator"
        cwd = cwd or owner_home + "/work/dev-hermit/hermit"
        cell = self._diverged_cell(
            0,
            record=98,
            syscall=37,
            scheduler_turn=4,
            virtual_nanoseconds=80_000,
            left="inbound read",
            right=message,
        )
        cell["cwd"] = cwd

        with self.assertRaisesRegex(series.MalformedRow, "workspace-local owner path"):
            self._project([cell])

    def test_append_cells_uses_the_supplied_run_id_for_the_whole_batch(self):
        import contextlib
        import io

        stdin = io.StringIO(
            "\n".join(json.dumps(self._cell(i)) for i in range(2)) + "\n"
        )
        stdout = io.StringIO()
        real_stdin = sys.stdin
        sys.stdin = stdin
        try:
            with contextlib.redirect_stdout(stdout):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "pressure-test",
                    "--run-id", "pressure-one",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-26T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        self.assertIn("representing 2 run(s)", stdout.getvalue())
        files = list(series.spool_dir(self.parent).glob("*.jsonl"))
        self.assertEqual([path.name for path in files], ["pressure-one.jsonl"])
        stored = series.union(files)
        self.assertEqual(len(stored), 2)
        self.assertEqual({item["run_id"] for item in stored}, {"pressure-one"})
        self.assertCountEqual([item["series"]["run_index"] for item in stored], [0, 1])
        self.assertTrue(all(item["series"]["num_runs"] == 1 for item in stored))
        self.assertTrue(all(item["series"]["pressure_evidence"] for item in stored))

        stdin = io.StringIO(
            "\n".join(json.dumps(self._cell(i)) for i in range(2)) + "\n"
        )
        stdout = io.StringIO()
        real_stdin = sys.stdin
        sys.stdin = stdin
        try:
            with contextlib.redirect_stdout(stdout):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "pressure-test",
                    "--run-id", "pressure-one",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-27T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        self.assertIn("appended 0 row(s) representing 2 run(s)", stdout.getvalue())
        self.assertEqual(len(files[0].read_text().splitlines()), 2)
        self.assertEqual(series.union(files), stored)

        ledger = self.parent / "hermit_test_ledger"
        tracked = ledger / "series/hermit/host/2026-08.jsonl"
        tracked.parent.mkdir(parents=True)
        files[0].replace(tracked)
        subprocess.run(["git", "-C", str(ledger), "add", "series"], check=True)
        subprocess.run([
            "git", "-C", str(ledger), "-c", "user.name=t", "-c", "user.email=t@invalid",
            "commit", "-qm", "publish original batch",
        ], check=True)
        subprocess.run([
            "git", "-C", str(ledger), "update-ref", "refs/remotes/origin/main", "HEAD",
        ], check=True)
        stdin = io.StringIO(
            "\n".join(json.dumps(self._cell(i)) for i in range(2)) + "\n"
        )
        stdout = io.StringIO()
        real_stdin = sys.stdin
        sys.stdin = stdin
        try:
            with contextlib.redirect_stdout(stdout):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "pressure-test",
                    "--run-id", "pressure-one",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-28T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        self.assertIn("appended 0 row(s) representing 2 run(s)", stdout.getvalue())
        self.assertFalse(files[0].exists())
        self.assertEqual(len(tracked.read_text().splitlines()), 2)
        self.assertEqual(series.union([tracked]), stored)

    def _append_cells(self, lines, *, producer, run_id, emitted_at, tree=None):
        """Run append-cells on raw stdin lines; return (rc, stdout, stderr).

        Each line is text or exact bytes; stdin carries them as bytes, as the
        producer's pipe does.
        """
        import contextlib
        import io

        data = b"".join(
            (line if isinstance(line, bytes) else line.encode("utf-8")) + b"\n"
            for line in lines
        )
        real_stdin = sys.stdin
        sys.stdin = io.TextIOWrapper(io.BytesIO(data), encoding="utf-8")
        stdout = io.StringIO()
        stderr = io.StringIO()
        try:
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", producer,
                    "--run-id", run_id,
                    "--tree", tree or self.tree,
                    "--emitted-at", emitted_at,
                ])
        finally:
            sys.stdin = real_stdin
        return rc, stdout.getvalue(), stderr.getvalue()

    def _quarantine_files(self, run_id):
        """Each append writes its own immutable file, named for the run."""
        return sorted(series.quarantine_dir(self.parent).glob(f"{run_id}.*.jsonl"))

    def _quarantined(self, run_id):
        return [record for path in self._quarantine_files(run_id)
                for record in series._read_quarantine_file(path)]

    @staticmethod
    def _received(item):
        """The exact bytes one quarantine input records."""
        if item["raw_base64"] is not None:
            return base64.b64decode(item["raw_base64"])
        return item["raw"].encode("utf-8")

    def test_append_cells_retains_typed_pressure_rows_when_one_cell_is_untyped(self):
        thread_output = self._cell(0)
        thread_output["test"] = "applications/thread-output"
        thread_output["backend"] = "kvm"
        thread_output_retry = self._cell(0)
        thread_output_retry["test"] = "applications/thread-output"
        thread_output_retry["backend"] = "kvm"
        thread_output_retry["attempt"] = 2
        subprocess_time = self._cell(0)
        subprocess_time["test"] = "applications/perl-io-subprocess-time"
        subprocess_time["backend"] = "kvm"
        untyped = self._cell(0, outcome="ERROR")
        untyped["test"] = "applications/invalid-without-typed-attempts"
        untyped["backend"] = "kvm"
        untyped["attempts"] = []
        cells = [thread_output, untyped, thread_output_retry, subprocess_time]

        # The library projection still refuses a batch it cannot write whole.
        with self.assertRaisesRegex(
            series.MalformedRow, "comparison cell result has no typed attempts"
        ):
            self._project(cells)
        self.assertEqual(list(series.spool_dir(self.parent).glob("*.jsonl")), [])

        def append(emitted_at):
            return self._append_cells(
                [json.dumps(cell) for cell in cells],
                producer="pressure-test",
                run_id="pressure-mixed-evidence",
                emitted_at=emitted_at,
            )

        rc, stdout, stderr = append("2026-08-26T10:00:00Z")
        # A pressure campaign reports a refused cell with a nonzero status,
        # but the independent valid rows are still retained.
        self.assertEqual(rc, 1)
        self.assertIn("appended 3 row(s) representing 3 run(s)", stdout)
        self.assertIn("series: QUARANTINED ", stderr)
        self.assertIn('"input_row":2', stderr)
        self.assertIn('"stage":"projection"', stderr)
        self.assertIn('"run_id":"pressure-mixed-evidence"', stderr)
        self.assertIn(f'"tree":"{self.tree}"', stderr)
        self.assertIn(
            '"cell":"applications/invalid-without-typed-attempts/verify/kvm"', stderr
        )
        self.assertIn("comparison cell result has no typed attempts", stderr)
        self.assertRegex(stderr, r'"raw_sha256":"[0-9a-f]{64}"')
        self.assertIn("quarantined 1 record(s) covering 1 of 4 input(s) to ", stderr)

        quarantined = self._quarantined("pressure-mixed-evidence")
        self.assertEqual(len(quarantined), 1)
        self.assertEqual(quarantined[0]["schema"], series.QUARANTINE_SCHEMA)
        # The refused input is retained exactly as it was received.
        [received] = quarantined[0]["inputs"]
        self.assertEqual(received["input_row"], 2)
        self.assertEqual(received["raw"], json.dumps(untyped))
        self.assertEqual(
            received["raw_sha256"], hashlib.sha256(json.dumps(untyped).encode()).hexdigest(),
        )
        self.assertEqual(received["raw_bytes"], len(json.dumps(untyped).encode()))

        target = series.spool_dir(self.parent) / "pressure-mixed-evidence.jsonl"
        first_bytes = target.read_bytes()
        stored = series.union([target])
        self.assertEqual(len(stored), 3)
        self.assertEqual(
            {row["series"]["cell"] for row in stored},
            {
                "applications/thread-output/verify/kvm",
                "applications/perl-io-subprocess-time/verify/kvm",
            },
        )
        self.assertEqual(len({row["event_id"] for row in stored}), 3)
        thread_rows = [
            row for row in stored
            if row["series"]["cell"] == "applications/thread-output/verify/kvm"
        ]
        self.assertEqual({row["series"]["attempt"] for row in thread_rows}, {1, 2})
        self.assertTrue(all(row["run_id"] == "pressure-mixed-evidence" for row in stored))
        self.assertTrue(all(row["series"]["tree"] == self.tree for row in stored))
        self.assertTrue(all(row["series"]["run_index"] == 0 for row in stored))
        for row in stored:
            payload = row["series"]
            identity = {
                "run_id": "pressure-mixed-evidence",
                "producer": "pressure-test",
                "cell": payload["cell"],
                "tree": self.tree,
                "run_index": payload["run_index"],
            }
            if "attempt" in payload:
                identity["attempt"] = payload["attempt"]
            expected = "series-" + hashlib.sha256(json.dumps(
                identity, sort_keys=True, separators=(",", ":")
            ).encode()).hexdigest()
            self.assertEqual(row["event_id"], expected)
        self.assertEqual(sum(row["series"]["num_runs"] for row in stored), 3)
        self.assertNotIn("invalid-without-typed-attempts", json.dumps(stored))
        series.validate_read_rows(stored)

        # Replaying the same run appends nothing and retains the refusal once.
        rc, stdout, stderr = append("2026-08-27T10:00:00Z")
        self.assertEqual(rc, 1)
        self.assertIn("appended 0 row(s) representing 3 run(s)", stdout)
        self.assertIn("series: QUARANTINED ", stderr)
        self.assertEqual(target.read_bytes(), first_bytes)
        replayed = self._quarantined("pressure-mixed-evidence")
        self.assertEqual(len(replayed), 1)
        self.assertEqual(replayed[0]["event_id"], quarantined[0]["event_id"])
        self.assertEqual(replayed, quarantined, "the first observation stands")

    def test_append_cells_quarantines_refused_cells_and_keeps_their_valid_siblings(self):
        untyped = self._cell(0, outcome="ERROR")
        untyped["attempts"] = []
        rc, stdout, stderr = self._append_cells(
            [json.dumps(untyped)],
            producer="pressure-test",
            run_id="pressure-untyped-only",
            emitted_at="2026-08-26T10:00:00Z",
        )
        # Nothing independent survived, so the batch does not read as a success.
        self.assertEqual(rc, 1)
        self.assertNotIn("appended", stdout)
        self.assertIn("quarantined 1 record(s) covering 1 of 1 input(s)", stderr)
        self.assertEqual(list(series.spool_dir(self.parent).glob("*.jsonl")), [])
        self.assertEqual(len(self._quarantined("pressure-untyped-only")), 1)

        # A second malformed provenance field on the refused result is named
        # in its own refusal, and the valid sibling is still appended.
        untyped["detcore_tree"] = "not-a-sha"
        valid = self._cell(0)
        valid["test"] = "applications/thread-output"
        rc, stdout, stderr = self._append_cells(
            [json.dumps(valid), json.dumps(untyped)],
            producer="pressure-test",
            run_id="pressure-untyped-and-malformed",
            emitted_at="2026-08-26T10:00:00Z",
        )
        self.assertEqual(rc, 1)
        self.assertIn("appended 1 row(s) representing 1 run(s)", stdout)
        self.assertIn("detcore_tree must be a 40-hex object id", stderr)
        stored = series.union(
            [series.spool_dir(self.parent) / "pressure-untyped-and-malformed.jsonl"]
        )
        self.assertEqual(
            [row["series"]["cell"] for row in stored],
            ["applications/thread-output/verify/ptrace"],
        )
        quarantined = self._quarantined("pressure-untyped-and-malformed")
        self.assertEqual(len(quarantined), 1)
        self.assertEqual(quarantined[0]["inputs"][0]["input_row"], 2)
        self.assertEqual(quarantined[0]["inputs"][0]["raw"], json.dumps(untyped))
        self.assertIn("detcore_tree must be a 40-hex object id", quarantined[0]["reason"])

    def test_validate_append_cells_keeps_the_batch_when_one_row_is_refused(self):
        # The run-213 shape: one verify cell whose guest failed on purpose but
        # whose manifest declaration had not reached the row. Before this, that
        # one row refused the whole validate batch and every cell was lost.
        passing = self._validate_cell(1)
        passing["test"] = "applications/thread-output"
        undeclared = self._expected_exit_cell(
            status=7, signal=None, guest_exit_code=7, guest_signal=None, declared=None,
        )
        undeclared["test"] = "applications/qemu-hello"
        not_json = '{"schema": 4, "truncated'
        rc, stdout, stderr = self._append_cells(
            [json.dumps(passing), not_json, json.dumps(undeclared)],
            producer="validate",
            run_id="validate-one-refused",
            emitted_at="2026-09-28T10:00:00Z",
        )
        # A validate's verdict stands on its tests. The valid row is appended,
        # and each refusal is retained and printed where the validate log
        # records the writer's successful output.
        self.assertEqual(rc, 0, stderr)
        self.assertIn("appended 1 row(s) representing 1 run(s)", stdout)
        self.assertIn("quarantined 2 record(s) covering 2 of 3 input(s) to ", stdout)
        self.assertEqual(stdout.count("series: QUARANTINED "), 2)
        [summary_line] = [line for line in stdout.splitlines()
                          if line.startswith("series: QUARANTINE_SUMMARY ")]
        summary = json.loads(summary_line.removeprefix("series: QUARANTINE_SUMMARY "))
        self.assertEqual(summary, {
            "schema": series.QUARANTINE_SUMMARY_SCHEMA,
            "run_id": "validate-one-refused",
            "producer": "validate",
            "tree": self.tree,
            "inputs": 3,
            "rows_appended": 1,
            "rows_admitted": 1,
            "quarantined_records": 2,
            "quarantined_inputs": 2,
            "stages": {"parse": 1, "projection": 1},
            "retained": True,
            "written": 2,
            "duplicates": 0,
            "published_check_error": summary["published_check_error"],
            "retention_error": None,
            "exit_code": 0,
        })
        stored = series.union(
            [series.spool_dir(self.parent) / "validate-one-refused.jsonl"]
        )
        self.assertEqual(
            [row["series"]["cell"] for row in stored],
            ["applications/thread-output/verify/ptrace"],
        )
        quarantined = {
            record["inputs"][0]["input_row"]: record
            for record in self._quarantined("validate-one-refused")
        }
        self.assertEqual(set(quarantined), {2, 3})
        self.assertEqual(quarantined[2]["stage"], "parse")
        self.assertEqual(quarantined[2]["error"], "JSONDecodeError")
        self.assertEqual(quarantined[2]["inputs"][0]["raw"], not_json)
        self.assertIn("stdin line 2 is not JSON", quarantined[2]["reason"])
        self.assertEqual(quarantined[3]["error"], "MalformedRow")
        self.assertEqual(quarantined[3]["inputs"][0]["raw"], json.dumps(undeclared))
        self.assertIn("matched report is internally inconsistent", quarantined[3]["reason"])
        self.assertEqual(quarantined[3]["cell"], "applications/qemu-hello/verify/ptrace")

        # A validate batch whose every row is refused is not a success.
        rc, stdout, stderr = self._append_cells(
            [json.dumps(undeclared)],
            producer="validate",
            run_id="validate-all-refused",
            emitted_at="2026-09-28T10:00:00Z",
        )
        self.assertEqual(rc, 1)
        self.assertIn("quarantined 1 record(s) covering 1 of 1 input(s)", stderr)
        self.assertIn('"exit_code": 1', stderr)
        self.assertFalse(
            (series.spool_dir(self.parent) / "validate-all-refused.jsonl").exists()
        )

    def test_append_cells_quarantines_every_row_when_the_invocation_is_refused(self):
        cells = [self._validate_cell(1), self._validate_cell(2)]
        rc, stdout, stderr = self._append_cells(
            [json.dumps(cell) for cell in cells],
            producer="validate",
            run_id="validate-bad-tree",
            emitted_at="2026-09-28T10:00:00Z",
            tree="not-a-sha",
        )
        self.assertEqual(rc, 1)
        self.assertIn("REFUSED the cell batch, nothing was written", stderr)
        self.assertIn("tree must be a 40-hex commit sha", stderr)
        self.assertEqual(list(series.spool_dir(self.parent).glob("*.jsonl")), [])
        quarantined = self._quarantined("validate-bad-tree")
        self.assertEqual([record["stage"] for record in quarantined], ["batch", "batch"])
        self.assertEqual(
            [record["inputs"][0]["raw"] for record in quarantined],
            [json.dumps(cell) for cell in cells],
        )

    def test_append_cells_quarantines_a_changed_repeat_and_appends_the_rest(self):
        first = self._validate_cell(1)
        rc, stdout, stderr = self._append_cells(
            [json.dumps(first)],
            producer="validate",
            run_id="validate-repeat",
            emitted_at="2026-09-28T10:00:00Z",
        )
        self.assertEqual(rc, 0, stderr)
        changed = self._validate_cell(1, duration_ms=250)
        changed["runtime"]["run1"]["syscalls"] = 613
        changed["runtime"]["run2"]["syscalls"] = 613
        other = self._validate_cell(1)
        other["test"] = "applications/thread-output"
        rc, stdout, stderr = self._append_cells(
            [json.dumps(changed), json.dumps(other)],
            producer="validate",
            run_id="validate-repeat",
            emitted_at="2026-09-28T11:00:00Z",
        )
        self.assertEqual(rc, 0, stderr)
        self.assertIn("appended 1 row(s) representing 1 run(s)", stdout)
        quarantined = self._quarantined("validate-repeat")
        self.assertEqual([record["stage"] for record in quarantined], ["spool-conflict"])
        self.assertIn("already records a different row", quarantined[0]["reason"])
        self.assertEqual(quarantined[0]["inputs"][0]["raw"], json.dumps(changed))
        self.assertEqual(quarantined[0]["row_event_id"], quarantined[0]["row"]["event_id"])
        stored = series.union([series.spool_dir(self.parent) / "validate-repeat.jsonl"])
        self.assertEqual(len(stored), 2)

    def test_append_cells_retains_each_received_line_byte_for_byte(self):
        passing = self._validate_cell(1)
        # Valid JSON a writer may emit without escaping: U+2028 inside a
        # string. It is one input line, not two.
        separated = '{"schema": 4,  "note": "a\u2028b"}'.encode("utf-8")
        # Not UTF-8 at all: retained as base64 of the exact bytes.
        undecodable = b'{"schema": 4, "note": "\xff\xfe"}'
        # Valid JSON whose spacing a re-serialization would not preserve.
        spaced = b'{ "schema" : 3 ,"test":"x" }'
        rc, stdout, stderr = self._append_cells(
            [json.dumps(passing), separated, undecodable, spaced],
            producer="validate",
            run_id="validate-exact-bytes",
            emitted_at="2026-09-28T10:00:00Z",
        )
        self.assertEqual(rc, 0, stderr)
        self.assertIn("quarantined 3 record(s) covering 3 of 4 input(s) to ", stdout)
        [path] = self._quarantine_files("validate-exact-bytes")
        # The local file is ASCII JSON lines, one per record.
        stored_bytes = path.read_bytes()
        stored_bytes.decode("ascii")
        self.assertEqual(stored_bytes.count(b"\n"), 3)
        quarantined = {
            record["inputs"][0]["input_row"]: record for record in self._quarantined(
                "validate-exact-bytes"
            )
        }
        expected = {2: separated, 3: undecodable, 4: spaced}
        self.assertEqual(set(quarantined), set(expected))
        for number, raw in expected.items():
            with self.subTest(input_row=number):
                [item] = quarantined[number]["inputs"]
                self.assertEqual(self._received(item), raw)
                self.assertEqual(item["raw_sha256"], hashlib.sha256(raw).hexdigest())
                self.assertEqual(item["raw_bytes"], len(raw))
        self.assertIsNone(quarantined[3]["inputs"][0]["raw"])
        self.assertEqual(quarantined[3]["stage"], "parse")
        self.assertEqual(quarantined[3]["error"], "UnicodeDecodeError")
        self.assertEqual(quarantined[2]["stage"], "projection")
        self.assertEqual(quarantined[4]["stage"], "projection")
        # With no owner path in them, the published form is the record itself.
        for record in quarantined.values():
            self.assertEqual(series.portable_quarantine_record(record), record)
            series.validate_quarantine_records([record], public=True)

    def _quarantine_fixture(self, raw, *, reason="refused", stage="projection", row=None):
        return series.quarantine_record(
            stage=stage, run_id="validate-fixture", producer="validate", tree=self.tree,
            emitted_at="2026-09-28T10:00:00Z", error="MalformedRow", reason=reason,
            inputs=[series.quarantine_input(1, raw)], row=row,
            row_event_id=None if row is None else row["event_id"], host="testhost",
        )

    def test_portable_quarantine_record_replaces_owner_homes_by_tokens(self):
        needle = series._OWNER_PATH_NEEDLE
        alice, bob = needle + "alice", needle + "bob"
        raw = json.dumps({
            "cmd": alice + "/work/x",
            "cwd": bob + "/y",
            "nested": json.dumps({"again": alice + "/z"}),
        }).encode()
        record = self._quarantine_fixture(raw, reason=f"cannot read {alice}/work/x")
        with self.assertRaisesRegex(series.MalformedQuarantine, "would publish an owner path"):
            series.validate_quarantine_records([record], public=True)

        portable = series.portable_quarantine_record(record)
        text = json.dumps(portable)
        self.assertNotIn(needle, text)
        self.assertEqual(portable["owner_paths"], [
            {"token": "{{OWNER_HOME_1}}", "bytes": len(alice.encode())},
            {"token": "{{OWNER_HOME_2}}", "bytes": len(bob.encode())},
        ])
        self.assertIsNone(portable["withheld"])
        self.assertEqual(portable["event_id"], record["event_id"])
        self.assertEqual(portable["reason"], "cannot read {{OWNER_HOME_1}}/work/x")
        series.validate_quarantine_records([portable], public=True)
        # The ledger holds only each prefix's length, never its bytes ...
        self.assertNotIn("alice", text)
        self.assertNotIn("bob", text)
        # ... so the received bytes come back only for a reader who already
        # knows each prefix and substitutes it; the digest proves that result.
        restored = portable["inputs"][0]["raw"]
        restored = restored.replace("{{OWNER_HOME_1}}", alice).replace("{{OWNER_HOME_2}}", bob)
        self.assertEqual(restored.encode(), raw)
        self.assertEqual(hashlib.sha256(restored.encode()).hexdigest(),
                         portable["inputs"][0]["raw_sha256"])
        self.assertEqual(series.portable_quarantine_record(portable), portable)

        # A token whose listed length does not restore raw_bytes is refused.
        wrong = copy.deepcopy(portable)
        wrong["owner_paths"][0]["bytes"] += 1
        with self.assertRaisesRegex(series.MalformedQuarantine, "restored does not match"):
            series.validate_quarantine_records([wrong], public=True)

    def test_portable_quarantine_record_withholds_what_token_substitution_cannot_replace(self):
        needle = series._OWNER_PATH_NEEDLE
        cases = {
            "token already present": json.dumps(
                {"cwd": needle + "alice/x", "note": "{{OWNER_HOME_1}}"}
            ).encode(),
            "base64-held bytes": needle.encode() + b"alice/\xff",
        }
        for label, raw in cases.items():
            with self.subTest(label=label):
                record = self._quarantine_fixture(raw)
                portable = series.portable_quarantine_record(record)
                self.assertEqual(portable["withheld"]["reason"], "owner-path")
                self.assertEqual(portable["inputs"], [{
                    "input_row": 1, "raw": None, "raw_base64": None,
                    "raw_sha256": hashlib.sha256(raw).hexdigest(), "raw_bytes": len(raw),
                }])
                self.assertEqual(portable["owner_paths"], [])
                self.assertEqual(portable["event_id"], record["event_id"])
                self.assertNotIn(needle, json.dumps(portable))
                series.validate_quarantine_records([portable], public=True)
                # The exact record is still valid local evidence.
                series.validate_quarantine_records([record], public=False)
                self.assertEqual(self._received(record["inputs"][0]), raw)

    def test_validate_quarantine_records_refuses_inexact_evidence(self):
        raw = b'{"schema": 4, "truncated": true}'
        record = self._quarantine_fixture(raw)
        series.validate_quarantine_records([record], public=True)
        series.validate_quarantine_records([record], public=False)

        def mutated(change):
            candidate = copy.deepcopy(record)
            change(candidate)
            return candidate

        def rebind(candidate):
            candidate["event_id"] = series._quarantine_event_id(candidate)

        needle = series._OWNER_PATH_NEEDLE
        owner = self._quarantine_fixture(json.dumps({"cwd": needle + "alice/x"}).encode())
        series.validate_quarantine_records([owner], public=False)
        cases = {
            "digest mismatch": (
                mutated(lambda r: (r["inputs"][0].update(raw_sha256="0" * 64), rebind(r))),
                "raw does not match raw_sha256/raw_bytes",
            ),
            "length mismatch": (
                mutated(lambda r: r["inputs"][0].update(raw_bytes=len(raw) + 1)),
                "raw does not match raw_sha256/raw_bytes",
            ),
            "content changed": (
                mutated(lambda r: r["inputs"][0].update(raw=raw.decode().replace(" ", ""))),
                "raw does not match raw_sha256/raw_bytes",
            ),
            "no inputs": (
                mutated(lambda r: (r.update(inputs=[]), rebind(r))),
                "must carry the inputs it refused",
            ),
            "both encodings": (
                mutated(lambda r: r["inputs"][0].update(
                    raw_base64=base64.b64encode(raw).decode())),
                "exactly one of raw and raw_base64",
            ),
            "wrong base64": (
                mutated(lambda r: r["inputs"][0].update(
                    raw=None, raw_base64=base64.b64encode(raw + b" ").decode())),
                "raw_base64 does not match",
            ),
            "wrong event id": (
                mutated(lambda r: r.update(event_id="series-quarantine-" + "0" * 64)),
                "event_id does not bind",
            ),
            "timestamp in identity": (
                mutated(lambda r: r.update(run_id="another-run")),
                "event_id does not bind",
            ),
            "extra key": (mutated(lambda r: r.update(extra=1)), "keys differ"),
            "unknown stage": (mutated(lambda r: r.update(stage="elsewhere")), "stage"),
            "untyped error": (mutated(lambda r: r.update(error="")), "error must name"),
            "post-projection without row": (
                mutated(lambda r: (r.update(stage="shared-checker"), rebind(r))),
                "names its row event id",
            ),
            "unused token": (
                mutated(lambda r: r.update(owner_paths=[{"token": "{{OWNER_HOME_1}}",
                                                        "bytes": 20}])),
                "listed but unused",
            ),
            "owner path in public": (owner, "would publish an owner path"),
        }
        for label, (candidate, message) in cases.items():
            with self.subTest(label=label):
                with self.assertRaisesRegex(series.MalformedQuarantine, message):
                    series.validate_quarantine_records([candidate], public=True)
        # The observation time is not part of the identity.
        later = mutated(lambda r: r.update(emitted_at="2026-09-29T10:00:00Z"))
        series.validate_quarantine_records([later], public=True)

    def test_slim_quarantine_record_moves_the_input_bytes_to_files_losslessly(self):
        needle = series._OWNER_PATH_NEEDLE
        cases = {
            "text": b'{"schema": 4, "truncated": true}',
            "not UTF-8": b'{"cwd": "\xff\xfe"}',
            "owner home": json.dumps({"cwd": needle + "alice/x"}).encode(),
            "empty": b"",
        }
        for label, raw in cases.items():
            with self.subTest(label=label):
                published = series.portable_quarantine_record(self._quarantine_fixture(raw))
                series.validate_quarantine_records([published], public=True)
                slim, files = series.slim_quarantine_record(published)
                self.assertEqual(slim["schema"], series.QUARANTINE_REF_SCHEMA)
                self.assertEqual(slim["event_id"], published["event_id"])
                self.assertEqual({k: v for k, v in slim.items() if k not in ("schema", "inputs")},
                                 {k: v for k, v in published.items()
                                  if k not in ("schema", "inputs")})
                [item] = slim["inputs"]
                self.assertEqual((item["raw"], item["raw_base64"]), (None, None))
                self.assertEqual(list(files), [item["raw_ref"]])
                content = files[item["raw_ref"]]
                self.assertEqual(item["raw_ref"], series.quarantine_input_path(
                    hashlib.sha256(content).hexdigest()))
                self.assertNotIn(needle.encode(), content)
                if published["owner_paths"]:
                    # The file holds the tokenized text the row held.
                    self.assertEqual(content, published["inputs"][0]["raw"].encode())
                    self.assertNotEqual(item["raw_ref"].rpartition("/")[2], item["raw_sha256"])
                else:
                    self.assertEqual(content, raw)
                    self.assertEqual(item["raw_ref"].rpartition("/")[2], item["raw_sha256"])
                series.validate_quarantine_records([slim], public=True)
                expanded = series.expand_quarantine_record(slim, files)
                self.assertEqual(json.dumps(expanded, sort_keys=True),
                                 json.dumps(published, sort_keys=True))
                # A record is slimmed once; an expanded record slims the same way.
                self.assertEqual(series.slim_quarantine_record(slim), (slim, {}))
                self.assertEqual(series.slim_quarantine_record(expanded), (slim, files))

    def test_slim_quarantine_record_keeps_v2_what_it_cannot_round_trip(self):
        needle = series._OWNER_PATH_NEEDLE
        withheld = series.portable_quarantine_record(self._quarantine_fixture(
            json.dumps({"cwd": needle + "alice/x", "note": "{{OWNER_HOME_1}}"}).encode()))
        self.assertIsNotNone(withheld["withheld"])
        text = series.portable_quarantine_record(self._quarantine_fixture(b"hello"))
        # Valid v2, but not as quarantine_input stores UTF-8 bytes, so its
        # expansion would not be the same record.
        as_base64 = copy.deepcopy(text)
        as_base64["inputs"][0].update(raw=None, raw_base64=base64.b64encode(b"hello").decode())
        series.validate_quarantine_records([as_base64], public=True)
        lone_surrogate = copy.deepcopy(text)
        lone_surrogate["inputs"][0]["raw"] = "\ud800"
        # Not a public record: slimming it would publish the owner path.
        exact_owner = self._quarantine_fixture(json.dumps({"cwd": needle + "alice/x"}).encode())
        for label, record in {
            "withheld": withheld, "base64 of UTF-8 bytes": as_base64,
            "no encodable content": lone_surrogate, "owner path": exact_owner,
            "already v3": series.slim_quarantine_record(text)[0],
        }.items():
            with self.subTest(label=label):
                self.assertEqual(series.slim_quarantine_record(record), (record, {}))

    def test_a_v3_quarantine_record_is_refused_unless_its_files_restore_it_exactly(self):
        needle = series._OWNER_PATH_NEEDLE
        published = series.portable_quarantine_record(
            self._quarantine_fixture(b'{"schema": 4, "truncated": true}'))
        slim, files = series.slim_quarantine_record(published)
        [ref] = files

        def mutated(change):
            candidate = copy.deepcopy(slim)
            change(candidate)
            return candidate

        other_ref = series.quarantine_input_path(hashlib.sha256(b"other").hexdigest())
        digest = ref.rpartition("/")[2]
        wrong_directory = "00" if not digest.startswith("00") else "11"
        cases = {
            "content in the row": (
                mutated(lambda r: r["inputs"][0].update(raw="x")),
                "keeps its bytes in its raw_ref file"),
            "no raw_ref": (
                mutated(lambda r: r["inputs"][0].pop("raw_ref")), "inputs entries are"),
            "another file": (
                mutated(lambda r: r["inputs"][0].update(raw_ref=other_ref)),
                "raw_ref does not name the raw_sha256 file"),
            "outside quarantine/inputs": (
                mutated(lambda r: r["inputs"][0].update(raw_ref="series/" + ref)),
                "is not a quarantine input file"),
            "wrong fan-out directory": (
                mutated(lambda r: r["inputs"][0].update(
                    raw_ref=f"{series.QUARANTINE_INPUTS_ROOT}/{wrong_directory}/{digest}")),
                "is not a quarantine input file"),
            "withheld": (
                mutated(lambda r: r.update(withheld={"reason": "owner-path", "detail": "x"})),
                "moves the bytes of the inputs it refused"),
        }
        for label, (candidate, message) in cases.items():
            with self.subTest(label=label):
                with self.assertRaisesRegex(series.MalformedQuarantine, message):
                    series.validate_quarantine_records([candidate], public=True)
                with self.assertRaisesRegex(series.MalformedQuarantine, message):
                    series.expand_quarantine_record(candidate, files)
        # A file reference is not part of the v2 type.
        with self.assertRaisesRegex(series.MalformedQuarantine, "inputs entries are"):
            series.validate_quarantine_records(
                [mutated(lambda r: r.update(schema=series.QUARANTINE_SCHEMA))], public=True)

        expand_cases = {
            "missing file": ({}, "is missing"),
            "file absent from the ledger": ({ref: None}, "is missing"),
            "changed file": ({ref: files[ref] + b" "}, "does not hash to its name"),
        }
        for label, (given, message) in expand_cases.items():
            with self.subTest(label=label):
                with self.assertRaisesRegex(series.MalformedQuarantine, message):
                    series.expand_quarantine_record(slim, given)
        # With owner-home tokens the file is named by its own digest, so the
        # restored record is what proves the content, as for a v2 record with
        # tokens: another file that hashes to its name and uses the token
        # still does not restore raw_bytes.
        tokenized = series.portable_quarantine_record(self._quarantine_fixture(
            json.dumps({"cwd": needle + "alice/x"}).encode()))
        slim_tokens, token_files = series.slim_quarantine_record(tokenized)
        self.assertEqual(len(token_files), 1)
        forged_bytes = b'{"cwd": "{{OWNER_HOME_1}}/xy"}'
        forged_ref = series.quarantine_input_path(hashlib.sha256(forged_bytes).hexdigest())
        swapped = copy.deepcopy(slim_tokens)
        swapped["inputs"][0]["raw_ref"] = forged_ref
        series.validate_quarantine_records([swapped], public=True)
        with self.assertRaisesRegex(series.MalformedQuarantine, "restored does not match raw_bytes"):
            series.expand_quarantine_record(swapped, {forged_ref: forged_bytes})
        # A file must never bring an owner path back into the public record.
        owner_raw = json.dumps({"cwd": needle + "alice/x"}).encode()
        exact = self._quarantine_fixture(owner_raw)
        owner_ref = series.quarantine_input_path(hashlib.sha256(owner_raw).hexdigest())
        forged = copy.deepcopy(exact)
        forged["schema"] = series.QUARANTINE_REF_SCHEMA
        forged["inputs"][0].update(raw=None, raw_base64=None, raw_ref=owner_ref)
        series.validate_quarantine_records([forged], public=True)
        with self.assertRaisesRegex(series.MalformedQuarantine, "would publish an owner path"):
            series.expand_quarantine_record(forged, {owner_ref: owner_raw})

    def test_legacy_quarantine_is_published_as_legacy_not_as_received_bytes(self):
        raw = {"schema": 4, "test": "applications/qemu-hello"}
        canonical = json.dumps(raw, sort_keys=True, separators=(",", ":"),
                               ensure_ascii=False).encode()
        # The v1 writer's identity: the canonical re-serialization's digest.
        identity = json.dumps({
            "event_id": None, "input_row": 3, "producer": "validate",
            "raw_sha256": hashlib.sha256(canonical).hexdigest(),
            "run_id": "validate-legacy", "stage": "projection", "tree": self.tree,
        }, sort_keys=True, separators=(",", ":"))
        legacy = {
            "schema": series.LEGACY_QUARANTINE_SCHEMA,
            "quarantine_id": "series-quarantine-" + hashlib.sha256(identity.encode()).hexdigest(),
            "host": None,
            "event_id": None,
            "emitted_at": "2026-09-27T10:00:00Z",
            "run_id": "validate-legacy",
            "producer": "validate",
            "tree": self.tree,
            "stage": "projection",
            "reason": "matched report is internally inconsistent",
            "cell": "applications/qemu-hello/verify/ptrace",
            "input_row": 3,
            "raw": raw,
            "raw_sha256": hashlib.sha256(canonical).hexdigest(),
            "raw_bytes": len(canonical),
        }
        upgraded = series._published_form(legacy, host="testhost")
        self.assertEqual(upgraded["schema"], series.QUARANTINE_SCHEMA)
        self.assertEqual(upgraded["event_id"], legacy["quarantine_id"])
        self.assertEqual(upgraded["inputs"], [])
        self.assertEqual(upgraded["error"], "legacy-v1-unrecorded")
        # The v1 writer recorded no received bytes, so the published form has
        # none to give back, and reaping its retained copy holds nothing.
        self.assertTrue(series.published_quarantine_restores(upgraded, legacy))
        self.assertEqual(upgraded["legacy"]["raw"], raw)
        self.assertEqual(series.quarantine_identity(legacy), legacy["quarantine_id"])
        series.validate_quarantine_records([upgraded], public=True)
        self.assertEqual(upgraded["host"], "testhost")
        # An upgraded v1 record has no input bytes to move, and one shard holds
        # it beside both forms of a record that has some.
        self.assertEqual(series.slim_quarantine_record(upgraded), (upgraded, {}))
        kept = series.portable_quarantine_record(self._quarantine_fixture(b"kept in the row"))
        moved, _ = series.slim_quarantine_record(
            series.portable_quarantine_record(self._quarantine_fixture(b"moved to a file")))
        self.assertEqual(moved["schema"], series.QUARANTINE_REF_SCHEMA)
        series.validate_quarantine_records([upgraded, kept, moved], public=True)
        forged = copy.deepcopy(upgraded)
        forged["legacy"]["raw"]["test"] = "applications/other"
        with self.assertRaisesRegex(series.MalformedQuarantine, "legacy raw does not match"):
            series.validate_quarantine_records([forged], public=True)
        renamed = dict(legacy, quarantine_id="series-quarantine-" + "a" * 64)
        with self.assertRaisesRegex(series.MalformedQuarantine, "event_id does not bind"):
            series._published_form(renamed, host="testhost")

    def test_append_cells_propagates_a_writer_defect_after_retaining_every_input(self):
        # A projection bug is not a data refusal: it must not be quarantined
        # as if the cell were malformed. Every input is retained exactly and
        # the exception propagates.
        passing = self._validate_cell(1)
        not_json = '{"schema": 4, "truncated'
        with mock.patch.object(series, "_cell_runtime",
                               side_effect=TypeError("simulated projection defect")):
            with self.assertRaisesRegex(TypeError, "simulated projection defect"):
                self._append_cells(
                    [json.dumps(passing), not_json],
                    producer="validate",
                    run_id="validate-writer-defect",
                    emitted_at="2026-09-28T10:00:00Z",
                )
        self.assertEqual(list(series.spool_dir(self.parent).glob("*.jsonl")), [])
        quarantined = {
            record["inputs"][0]["input_row"]: record
            for record in self._quarantined("validate-writer-defect")
        }
        self.assertEqual(set(quarantined), {1, 2})
        self.assertEqual(quarantined[1]["stage"], "writer-error")
        self.assertEqual(quarantined[1]["error"], "TypeError")
        self.assertIn("simulated projection defect", quarantined[1]["reason"])
        self.assertEqual(quarantined[1]["inputs"][0]["raw"], json.dumps(passing))
        self.assertEqual(quarantined[2]["stage"], "parse")
        self.assertEqual(quarantined[2]["inputs"][0]["raw"], not_json)

    def test_projection_refuses_every_mutated_field_as_data_never_as_a_crash(self):
        """Malformed cell data must reach MalformedRow, not escape as a bug.

        Each JSON path of representative cells (and of their embedded
        verification report) is replaced by values of every JSON type, or
        deleted. Every mutant must be either admitted or quarantined; any
        other exception names a data check that is missing. The admission
        checker is stubbed so this measures the projection alone.
        """
        missing = object()
        values = [None, True, False, 0, 1, -1, 1.5, 2**64, "x", "", [], {}, [None],
                  {"x": None}, "matched", "diverged", "0" * 40, missing]

        def paths(node, prefix=()):
            if isinstance(node, dict):
                for key, value in node.items():
                    yield prefix + (key,)
                    yield from paths(value, prefix + (key,))
            elif isinstance(node, list):
                for index, value in enumerate(node):
                    yield prefix + (index,)
                    yield from paths(value, prefix + (index,))

        def replaced(node, path, value):
            node = copy.deepcopy(node)
            target = node
            for step in path[:-1]:
                target = target[step]
            if value is missing:
                del target[path[-1]]
            else:
                target[path[-1]] = value
            return node

        def mutants(base):
            for path in paths(base):
                for value in values:
                    yield path, value, replaced(base, path, value)
            for index, attempt in enumerate(base.get("attempts") or []):
                report = json.loads(attempt["verification_report"])
                for path in paths(report):
                    for value in values:
                        cell = copy.deepcopy(base)
                        self._replace_comparison_report(
                            cell["attempts"][index], replaced(report, path, value)
                        )
                        yield ("attempts", index, "verification_report") + path, value, cell

        escaped = []

        def judge(items, producer):
            cells = [cell for _, _, cell in items]
            quarantine, provenance = [], []
            try:
                series._rows_from_cell_results(
                    cells, parent=self.parent, producer=producer, run_id="fuzz",
                    tree=self.tree, emitted_at="2026-09-28T10:00:00Z",
                    quarantine=quarantine, raw_inputs=[b"{}"] * len(cells),
                    provenance=provenance,
                )
            except Exception as error:  # bisect to the mutant that escaped
                if len(items) == 1:
                    escaped.append((producer, items[0][0], repr(items[0][1]),
                                    f"{type(error).__name__}: {error}"))
                    return
                middle = len(items) // 2
                judge(items[:middle], producer)
                judge(items[middle:], producer)
                return
            covered = {index for sources in provenance for index in sources}
            covered |= {item["input_row"] - 1 for record in quarantine
                        for item in record["inputs"]}
            for index, cell in enumerate(cells):
                inapplicable = isinstance(cell, dict) and cell.get("outcome") == "HOST-INAPPLICABLE"
                if index not in covered and not inapplicable:
                    escaped.append((producer, items[index][0], repr(items[index][1]),
                                    "neither admitted nor quarantined"))

        # One base per projected shape: each payload branch of
        # _rows_from_cell_results (no_verdict_evidence, declared_guest_exit,
        # pressure_evidence, coordinates and divergence messages, the
        # backend_parity witness, container failures) under each producer
        # that emits it, each non-verify mode family, and the chaos
        # divergence the writer retypes before projection.
        parity_matched = self._parity_cell()
        self._make_parity_match(parity_matched)
        divergence = dict(record=1_718, syscall=7, scheduler_turn=1_248,
                          virtual_nanoseconds=80_000)
        pressure_replay_failure = self._container_failure_cell(mode="replay")
        pressure_replay_failure["run_index"] = 0
        naked = self._cell(0)
        naked.update(mode="naked", backend=None)
        custom = self._cell(0, outcome="FAIL")
        custom["mode"] = "custom"
        chaos_divergence = self._chaos_divergence_cell()
        pressure_chaos_divergence = copy.deepcopy(chaos_divergence)
        pressure_chaos_divergence.pop("attempt")
        pressure_chaos_divergence["run_index"] = 0
        bases = {
            "validate": {
                "ordinary pass": self._validate_cell(1),
                "diverged": self._validate_diverged_cell(1),
                "chaos divergence typed crash-error": chaos_divergence,
                "no verdict (kvm, timed out)":
                    self._validate_no_verdict_cell(1, timed_out=True, backend="kvm"),
                "backend_parity, red witness": self._parity_cell(),
                "backend_parity, matched": parity_matched,
                "declared guest exit code": self._expected_exit_cell(
                    status=7, signal=None, guest_exit_code=7, guest_signal=None,
                    declared=(7, None)),
                "declared guest signal": self._expected_exit_cell(
                    status=None, signal=11, guest_exit_code=None, guest_signal=11,
                    declared=(None, 11)),
                "chaos container failure": self._container_failure_cell(mode="chaos"),
            },
            "pressure-test": {
                "ordinary pass": self._cell(0),
                "diverged": self._diverged_cell(0, **divergence),
                "replay container failure": pressure_replay_failure,
                "chaos divergence typed crash-error": pressure_chaos_divergence,
                "naked": naked,
                "custom failure": custom,
            },
            "hermit-repeat": {
                "ordinary pass": self._cell(0),
                "diverged": self._diverged_cell(0, **divergence),
            },
        }
        count = 0
        per_base = {}
        with mock.patch.object(series, "write_admission",
                               side_effect=lambda rows: [None] * len(rows)):
            for producer, cells in bases.items():
                for label, base in cells.items():
                    # A base that is itself refused would fuzz only the path
                    # to its first refusal, not the shape it stands for.
                    refused = []
                    admitted = series._rows_from_cell_results(
                        [copy.deepcopy(base)], parent=self.parent, producer=producer,
                        run_id="fuzz-base", tree=self.tree,
                        emitted_at="2026-09-28T10:00:00Z", quarantine=refused,
                        raw_inputs=[b"{}"], provenance=[],
                    )
                    self.assertEqual((len(admitted), refused), (1, []), (producer, label))
                    items = list(mutants(base))
                    per_base[(producer, label)] = len(items)
                    count += len(items)
                    judge(items, producer)
        self.assertGreater(count, 5000)
        self.assertEqual(
            [key for key, number in per_base.items() if number < 100], [], per_base,
        )
        self.assertEqual(escaped, [], f"{len(escaped)} of {count} mutants escaped")

    def test_write_admission_judges_every_row_in_one_checker_call(self):
        rows = series.rows_from_cell_results(
            [self._validate_cell(1)],
            parent=self.parent,
            producer="validate",
            run_id="validate-admission",
            tree=self.tree,
            emitted_at="2026-09-28T10:00:00Z",
        )
        good = rows[0]
        unknown = json.loads(json.dumps(good))
        unknown["series"]["outcome"] = "not-an-outcome"
        owner = json.loads(json.dumps(good))
        owner["series"]["cell"] = "/home/" + "newton/leak"
        calls = []
        real = series._run_shared_checker

        def counting(checker_input, arguments):
            calls.append(list(arguments))
            return real(checker_input, arguments)

        with mock.patch.object(series, "_run_shared_checker", side_effect=counting):
            verdicts = series.write_admission([good, unknown, owner, good])
        self.assertEqual(calls, [["--write-results"]])
        self.assertIsNone(verdicts[0])
        self.assertIsNone(verdicts[3])
        self.assertIn("does not match the shared SeriesRow", verdicts[1])
        self.assertIsInstance(verdicts[2], str)

    def test_write_admission_refuses_a_result_that_does_not_account_for_the_rows(self):
        good = series.rows_from_cell_results(
            [self._validate_cell(1)],
            parent=self.parent,
            producer="validate",
            run_id="validate-admission-envelope",
            tree=self.tree,
            emitted_at="2026-09-28T10:00:00Z",
        )[0]
        checker_input = series._checker_input([good])
        digest = hashlib.sha256(checker_input.encode()).hexdigest()
        cases = {
            "missing result": {
                "results": [], "input_row_count": 1,
                "input_jsonl_sha256": digest, "schema_version": 1,
            },
            "wrong digest": {
                "results": [{"line": 1, "result": None}], "input_row_count": 1,
                "input_jsonl_sha256": "0" * 64, "schema_version": 1,
            },
            "wrong line": {
                "results": [{"line": 2, "result": None}], "input_row_count": 1,
                "input_jsonl_sha256": digest, "schema_version": 1,
            },
            "unknown schema": {
                "results": [{"line": 1, "result": None}], "input_row_count": 1,
                "input_jsonl_sha256": digest, "schema_version": 2,
            },
        }
        for label, output in cases.items():
            with self.subTest(label=label):
                completed = subprocess.CompletedProcess(
                    [], 0, stdout=json.dumps(output), stderr="",
                )
                with mock.patch.object(series, "_run_shared_checker", return_value=completed):
                    with self.assertRaises(series.SeriesError):
                        series.write_admission([good])
        failed = subprocess.CompletedProcess([], 2, stdout="", stderr="checker broke")
        with mock.patch.object(series, "_run_shared_checker", return_value=failed):
            with self.assertRaisesRegex(series.SeriesError, "checker broke"):
                series.write_admission([good])

    def test_append_cells_uses_the_producing_checkout_for_an_off_main_tree(self):
        import contextlib
        import io

        producer = self.parent / "producer-checkout"
        producer.mkdir()
        subprocess.run(["git", "-C", str(producer), "init", "-q", "-b", "main"], check=True)
        subprocess.run(
            ["git", "-C", str(producer), "config", "user.name", "Series Test"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(producer), "config", "user.email", "series@example.invalid"],
            check=True,
        )
        (producer / "base").write_text("base\n")
        subprocess.run(["git", "-C", str(producer), "add", "base"], check=True)
        subprocess.run(
            ["git", "-C", str(producer), "commit", "-qm", "base"], check=True,
        )
        base = subprocess.run(
            ["git", "-C", str(producer), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        subprocess.run(
            ["git", "-C", str(producer), "update-ref", "refs/remotes/origin/main", base],
            check=True,
        )
        (producer / "feature").write_text("feature\n")
        subprocess.run(["git", "-C", str(producer), "add", "feature"], check=True)
        subprocess.run(
            ["git", "-C", str(producer), "commit", "-qm", "feature"], check=True,
        )
        feature = subprocess.run(
            ["git", "-C", str(producer), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        missing = subprocess.run(
            ["git", "-C", str(self.repo), "cat-file", "-e", f"{feature}^{{commit}}"],
            check=False, capture_output=True, text=True,
        )
        self.assertNotEqual(missing.returncode, 0)

        cell = self._cell(0)
        cell["hermit_sha"] = feature
        real_stdin = sys.stdin
        sys.stdin = io.StringIO(json.dumps(cell) + "\n")
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--checkout", str(producer),
                    "--producer", "pressure-test",
                    "--run-id", "off-main-feature",
                    "--tree", feature,
                    "--emitted-at", "2026-08-27T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 0)
        stored = series.union(sorted(series.spool_dir(self.parent).glob("*.jsonl")))
        self.assertEqual(len(stored), 1)
        self.assertFalse(stored[0]["series"]["main_ancestry"])
        self.assertEqual(stored[0]["series"]["depth"]["hermit"]["commits"], 2)

    def test_append_cells_refuses_a_changed_replay_before_writing(self):
        import contextlib
        import io

        def append(cell):
            real_stdin = sys.stdin
            sys.stdin = io.StringIO(json.dumps(cell) + "\n")
            try:
                with contextlib.redirect_stderr(io.StringIO()):
                    return series.main([
                        "append-cells",
                        "--parent", str(self.parent),
                        "--producer", "pressure-test",
                        "--run-id", "pressure-one",
                        "--tree", self.tree,
                        "--emitted-at", "2026-08-26T10:00:00Z",
                    ])
            finally:
                sys.stdin = real_stdin

        self.assertEqual(append(self._cell(0, duration_ms=100)), 0)
        target = series.spool_dir(self.parent) / "pressure-one.jsonl"
        before = target.read_bytes()
        self.assertEqual(append(self._cell(0, duration_ms=101)), 1)
        self.assertEqual(target.read_bytes(), before)

    def test_append_cells_refuses_duplicate_run_indices_before_writing(self):
        import contextlib
        import io

        real_stdin = sys.stdin
        duplicate = self._validate_cell(1)
        sys.stdin = io.StringIO(json.dumps(duplicate) + "\n" + json.dumps(duplicate) + "\n")
        try:
            with contextlib.redirect_stderr(io.StringIO()):
                rc = series.main([
                    "append-cells",
                    "--parent", str(self.parent),
                    "--producer", "validate",
                    "--run-id", "validate-one",
                    "--tree", self.tree,
                    "--emitted-at", "2026-08-26T10:00:00Z",
                ])
        finally:
            sys.stdin = real_stdin
        self.assertEqual(rc, 1)
        self.assertFalse((series.spool_dir(self.parent) / "validate-one.jsonl").exists())

    def test_an_empty_submodule_directory_does_not_resolve_to_the_parent_repo(self):
        empty = self.repo / "reverie"
        empty.mkdir()
        self.assertIsNone(series.commit_depth(empty, self.tree) if series._is_checkout(empty) else None)
        self.assertEqual(series.main_ancestry(empty, [self.tree]), {self.tree: (None, None)})


class HistoryCli(unittest.TestCase):
    TEST = "applications/git-repository-workflow"

    def setUp(self):
        self.enterContext(mock.patch.object(series, "ledger_root", side_effect=lambda parent: Path(parent)))

    def _git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def _publish_parent(self, parent: Path) -> None:
        bare = parent.parent / "parent-origin.git"
        subprocess.run(
            ["git", "init", "-q", "--bare", "-b", "main", str(bare)], check=True,
        )
        self._git(parent, "init", "-q", "-b", "main")
        self._git(parent, "config", "user.name", "Series Test")
        self._git(parent, "config", "user.email", "series@example.invalid")
        self._git(parent, "remote", "add", "origin", str(bare))
        (parent / "README").write_text("parent\n")
        self._git(parent, "add", "-A")
        self._git(parent, "commit", "-qm", "series")
        self._git(parent, "push", "-q", "origin", "main")

    def test_one_command_reads_tracked_and_live_rows_for_one_exact_test(self):
        import contextlib
        import io
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent = root / "parent"
            parent.mkdir()
            hermit_bare = root / "hermit-origin.git"
            subprocess.run(
                ["git", "init", "-q", "--bare", "-b", "main", str(hermit_bare)],
                check=True,
            )
            repo = parent / "hermit"
            repo.mkdir()
            self._git(repo, "init", "-q", "-b", "main")
            self._git(repo, "config", "user.name", "Series Test")
            self._git(repo, "config", "user.email", "series@example.invalid")
            (repo / "one").write_text("one\n")
            self._git(repo, "add", "one")
            self._git(repo, "commit", "-qm", "one")
            first_tree = self._git(repo, "rev-parse", "HEAD")
            self._git(repo, "remote", "add", "origin", str(hermit_bare))
            self._git(repo, "push", "-q", "-u", "origin", "main")

            writer = root / "hermit-writer"
            subprocess.run(["git", "clone", "-q", str(hermit_bare), str(writer)], check=True)
            self._git(writer, "config", "user.name", "Series Test")
            self._git(writer, "config", "user.email", "series@example.invalid")
            (writer / "two").write_text("two\n")
            self._git(writer, "add", "two")
            self._git(writer, "commit", "-qm", "two")
            self._git(writer, "push", "-q", "origin", "main")
            tree = self._git(writer, "rev-parse", "HEAD")
            self.assertEqual(self._git(repo, "rev-parse", "origin/main"), first_tree)
            missing = subprocess.run(
                ["git", "-C", str(repo), "cat-file", "-e", f"{tree}^{{commit}}"],
                check=False,
            )
            self.assertNotEqual(
                missing.returncode,
                0,
                "the history fixture must begin with a stale Hermit origin/main",
            )

            first = row(
                event_id="history-validate",
                emitted_at="2026-08-25T03:00:00Z",
                run_id="validate-1",
                producer="validate",
                series={
                    "cell": f"{self.TEST}/verify/ptrace",
                    "tree": tree,
                    "run_index": 0,
                    "outcome": "diverged",
                    "num_runs": 1,
                    "coordinates": {
                        "first_divergent_record": 98,
                        "first_divergent_syscall": 37,
                        "first_divergent_scheduler_turn": 4,
                        "first_divergent_virtual_nanoseconds": 55,
                    },
                    "first_divergent_messages": {
                        "left": "inbound read",
                        "right": "finish read",
                    },
                    "main_ancestry": True,
                    "depth": {"hermit": {"commits": 2, "first_parent": 2}},
                    "runtime": {
                        "run1": {
                            "scheduler_turns": 12,
                            "virtual_nanoseconds": 34,
                            "syscalls": 5,
                        },
                        "run2": {
                            "scheduler_turns": 13,
                            "virtual_nanoseconds": 35,
                            "syscalls": 6,
                        },
                        "wall_time_min_ms": 90,
                        "wall_time_max_ms": 90,
                    },
                },
            )
            second = row(
                event_id="history-pressure",
                emitted_at="2026-08-26T03:00:00Z",
                run_id="pressure-1",
                producer="pressure-test",
                series={
                    "cell": f"{self.TEST}/verify/ptrace",
                    "tree": tree,
                    "run_index": 0,
                    "outcome": "passed",
                    "num_runs": 200,
                    "main_ancestry": True,
                    "depth": {"hermit": {"commits": 2, "first_parent": 2}},
                    "runtime": {
                        "run1": {
                            "scheduler_turns": 1231,
                            "virtual_nanoseconds": 1_413_842_550,
                            "syscalls": 612,
                        },
                        "run2": {
                            "scheduler_turns": 1231,
                            "virtual_nanoseconds": 1_413_842_550,
                            "syscalls": 612,
                        },
                        "wall_time_min_ms": 5_101,
                        "wall_time_max_ms": 5_285,
                    },
                },
            )
            unrelated = row(
                event_id="history-other",
                series={"cell": "applications/other/verify/ptrace", "tree": tree},
            )
            tracked = parent / "series/hermit/host/2026-08.jsonl"
            tracked.parent.mkdir(parents=True)
            tracked.write_text(json.dumps(first) + "\n" + json.dumps(unrelated) + "\n")
            self._publish_parent(parent)
            live = series.spool_dir(parent) / "pressure-1.jsonl"
            live.parent.mkdir(parents=True)
            live.write_text(json.dumps(second) + "\n")

            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                rc = series.main([
                    "history", "--parent", str(parent), "--test", self.TEST,
                ])
            text = stdout.getvalue()
            self.assertEqual(rc, 0)
            self.assertIn(f"test={self.TEST} rows=2 runs=201", text)
            self.assertIn(
                f"cell={self.TEST}/verify/ptrace\truns=201\t"
                "wall_time_min_ms=90\twall_time_max_ms=5285",
                text,
            )
            self.assertIn("2026-08-25\t" + tree + "\tyes/yes\thermit:2/2", text)
            self.assertEqual(
                self._git(repo, "rev-parse", "origin/main"),
                tree,
                "history must refresh Hermit origin/main before labeling current ancestry",
            )
            self.assertIn(
                "\t12/13\t34/35\t5/6\t90\t98\t37\t4\t55\t"
                '{"left":"inbound read","right":"finish read"}'
                "\tvalidate\tvalidate-1",
                text,
            )
            self.assertIn("\t0-199 (200)\t1231/1231", text)
            self.assertIn(
                "\t5101-5285\t-\t-\t-\t-\t-\tpressure-test\tpressure-1",
                text,
            )
            self.assertNotIn("applications/other/verify/ptrace", text)

    def test_empty_history_is_an_explicit_nonzero_result(self):
        import contextlib
        import io
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary) / "parent"
            parent.mkdir()
            (parent / "series").mkdir()
            self._publish_parent(parent)
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                rc = series.main([
                    "history", "--parent", str(parent), "--test", self.TEST,
                ])
            self.assertEqual(rc, 1, stderr.getvalue())
            self.assertIn("no runs recorded", stderr.getvalue())

    OLD_TEST = "backend-parity-c/flock-lifecycle"
    NEW_TEST = "c-programs/flock-lifecycle"

    def _retired_history_fixture(self, root: Path, retired_map: str | None) -> tuple[Path, str, str]:
        """A parent whose stale Hermit origin/main gains ``retired_map`` on refresh.

        The old-id row was recorded at the first Hermit commit and the new-id
        row at the second, which is the commit that adds the map (or, with
        ``retired_map=None``, a commit that does not). Returns the parent and
        the two Hermit commits.
        """
        parent = root / "parent"
        parent.mkdir()
        hermit_bare = root / "hermit-origin.git"
        subprocess.run(
            ["git", "init", "-q", "--bare", "-b", "main", str(hermit_bare)], check=True,
        )
        repo = parent / "hermit"
        repo.mkdir()
        self._git(repo, "init", "-q", "-b", "main")
        self._git(repo, "config", "user.name", "Series Test")
        self._git(repo, "config", "user.email", "series@example.invalid")
        (repo / "one").write_text("one\n")
        self._git(repo, "add", "one")
        self._git(repo, "commit", "-qm", "one")
        old_tree = self._git(repo, "rev-parse", "HEAD")
        self._git(repo, "remote", "add", "origin", str(hermit_bare))
        self._git(repo, "push", "-q", "-u", "origin", "main")

        writer = root / "hermit-writer"
        subprocess.run(["git", "clone", "-q", str(hermit_bare), str(writer)], check=True)
        self._git(writer, "config", "user.name", "Series Test")
        self._git(writer, "config", "user.email", "series@example.invalid")
        if retired_map is None:
            (writer / "two").write_text("two\n")
            self._git(writer, "add", "two")
        else:
            path = writer / series.RETIRED_IDS_FILE
            path.parent.mkdir(parents=True)
            path.write_text(retired_map)
            self._git(writer, "add", series.RETIRED_IDS_FILE)
        self._git(writer, "commit", "-qm", "two")
        self._git(writer, "push", "-q", "origin", "main")
        new_tree = self._git(writer, "rev-parse", "HEAD")
        self.assertEqual(
            self._git(repo, "rev-parse", "origin/main"),
            old_tree,
            "the fixture must begin with a Hermit origin/main that predates the map",
        )

        rows = [
            row(
                event_id="retired-old",
                emitted_at="2026-08-24T03:00:00Z",
                run_id="validate-old",
                producer="validate",
                series={
                    "cell": f"{self.OLD_TEST}/verify/ptrace",
                    "tree": old_tree,
                    "outcome": "passed",
                    "num_runs": 3,
                },
            ),
            row(
                event_id="retired-new",
                emitted_at="2026-08-26T03:00:00Z",
                run_id="validate-new",
                producer="validate",
                series={
                    "cell": f"{self.NEW_TEST}/verify/ptrace",
                    "tree": new_tree,
                    "outcome": "diverged",
                    "num_runs": 1,
                },
            ),
            row(
                event_id="retired-other",
                emitted_at="2026-08-25T03:00:00Z",
                run_id="validate-old",
                producer="validate",
                series={"cell": "backend-parity-c/other/verify/ptrace", "tree": old_tree},
            ),
        ]
        tracked = parent / "series/hermit/host/2026-08.jsonl"
        tracked.parent.mkdir(parents=True)
        tracked.write_text("".join(json.dumps(item) + "\n" for item in rows))
        self._publish_parent(parent)
        return parent, old_tree, new_tree

    def _history(self, parent: Path, test: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = series.main(["history", "--parent", str(parent), "--test", test])
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_history_joins_a_retired_id_and_its_successor_into_one_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent, old_tree, new_tree = self._retired_history_fixture(
                Path(temporary), RetiredIdMap.fixture_text(),
            )
            for query in (self.NEW_TEST, self.OLD_TEST):
                with self.subTest(query=query):
                    rc, text, errors = self._history(parent, query)
                    self.assertEqual(rc, 0, errors)
                    self.assertIn(f"test={query} rows=2 runs=4", text)
                    self.assertIn(
                        f"joined test ids {self.OLD_TEST} + {self.NEW_TEST} as {self.NEW_TEST} "
                        f"(retired-id map {series.RETIRED_IDS_FILE} at Hermit {new_tree})",
                        text,
                    )
                    # The old-id and new-id rows continue one cell's totals.
                    self.assertIn(
                        f"cell={self.NEW_TEST}/verify/ptrace\truns=4\t", text,
                    )
                    self.assertNotIn(f"cell={self.OLD_TEST}/verify/ptrace\t", text)
                    # Each row still shows the id it was recorded under, oldest first.
                    old_line = f"2026-08-24\t{old_tree}\t"
                    new_line = f"2026-08-26\t{new_tree}\t"
                    self.assertIn(old_line, text)
                    self.assertIn(new_line, text)
                    self.assertLess(text.index(old_line), text.index(new_line))
                    old_row = text[text.index(old_line):].splitlines()[0]
                    new_row = text[text.index(new_line):].splitlines()[0]
                    self.assertIn(f"\t{self.OLD_TEST}/verify/ptrace\tpassed\t", old_row)
                    self.assertIn(f"\t{self.NEW_TEST}/verify/ptrace\tdiverged\t", new_row)
                    self.assertNotIn("other/verify/ptrace", text)

    def test_history_without_a_retired_id_map_joins_exact_ids_as_before(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent, old_tree, new_tree = self._retired_history_fixture(Path(temporary), None)
            for query, tree, cell, other in (
                (self.NEW_TEST, new_tree, f"{self.NEW_TEST}/verify/ptrace", self.OLD_TEST),
                (self.OLD_TEST, old_tree, f"{self.OLD_TEST}/verify/ptrace", self.NEW_TEST),
            ):
                with self.subTest(query=query):
                    rc, text, errors = self._history(parent, query)
                    self.assertEqual(rc, 0, errors)
                    runs = 1 if query == self.NEW_TEST else 3
                    self.assertIn(f"test={query} rows=1 runs={runs}", text)
                    self.assertIn(f"cell={cell}\truns={runs}\t", text)
                    self.assertIn(f"\t{tree}\t", text)
                    self.assertNotIn("joined test ids", text)
                    self.assertNotIn(other, text)

    def test_history_refuses_a_malformed_retired_id_map(self):
        document = json.loads(RetiredIdMap.fixture_text())
        document["retirements"][0]["ids"]["backend-parity-c/other"] = self.NEW_TEST
        document["retirements"][0]["collisions"]["backend-parity-c/other"] = "fixture"
        with tempfile.TemporaryDirectory() as temporary:
            parent, _old, new_tree = self._retired_history_fixture(
                Path(temporary), json.dumps(document),
            )
            rc, text, errors = self._history(parent, self.NEW_TEST)
            self.assertEqual(rc, 2, text + errors)
            self.assertIn("series: cannot read Hermit's retired-id map:", errors)
            self.assertIn(f"at Hermit {new_tree}", errors)
            self.assertIn("is shared by two retired ids", errors)
            self.assertEqual(text, "")


class RetiredIdMap(unittest.TestCase):
    """series reads Hermit's retired-id map with Hermit's own acceptance rules.

    The rules are those of ``hermit_manifest_plan::retired_ids::RetiredIds::parse``
    at Hermit 0818c472f25d7e89e9df6f621fa86e5baef573ae
    (https://github.com/rrnewton/hermit/issues/3301, slice S6).
    """

    @staticmethod
    def fixture(**override):
        retirement = {
            "retired_bucket": "backend-parity-c",
            "successor_bucket": "c-programs",
            "last_live_commit": "3f66a249b30fada86b81e722b8e5439ac0789f8e",
            "reason": "backend-parity-c folded into c-programs",
            "ids": {
                "backend-parity-c/flock-lifecycle": "c-programs/flock-lifecycle",
                "backend-parity-c/other": "c-programs/other",
                "backend-parity-c/pidfd-open-self": "c-programs/pidfd-open-self-pair",
            },
            "collisions": {
                "backend-parity-c/pidfd-open-self": "c-programs/pidfd-open-self is another program",
            },
        }
        retirement.update(override)
        return {"schema": 1, "description": "fixture map", "retirements": [retirement]}

    @classmethod
    def fixture_text(cls, **override):
        return json.dumps(cls.fixture(**override))

    def test_resolves_retired_ids_and_passes_every_other_id_through(self):
        retired = series.parse_retired_ids(self.fixture_text())
        self.assertEqual(
            retired,
            {
                "backend-parity-c/flock-lifecycle": "c-programs/flock-lifecycle",
                "backend-parity-c/other": "c-programs/other",
                "backend-parity-c/pidfd-open-self": "c-programs/pidfd-open-self-pair",
            },
        )
        self.assertEqual(
            series.resolve_test_id("backend-parity-c/pidfd-open-self", retired),
            "c-programs/pidfd-open-self-pair",
        )
        self.assertEqual(
            series.resolve_test_id("c-programs/pidfd-open-self", retired),
            "c-programs/pidfd-open-self",
        )
        self.assertEqual(
            series.resolve_cell("backend-parity-c/flock-lifecycle/verify/dbi", retired),
            "c-programs/flock-lifecycle/verify/dbi",
        )
        self.assertEqual(
            series.resolve_cell("backend-parity-c/unlisted/verify/dbi", retired),
            "backend-parity-c/unlisted/verify/dbi",
        )
        self.assertEqual(
            series.resolve_cell("backend-parity-c/flock-lifecycle/verify/dbi", None),
            "backend-parity-c/flock-lifecycle/verify/dbi",
        )

    def test_history_rows_join_an_old_id_row_and_a_new_id_row(self):
        retired = series.parse_retired_ids(self.fixture_text())
        old = row(
            event_id="old",
            emitted_at="2026-08-24T03:00:00Z",
            series={"cell": "backend-parity-c/flock-lifecycle/verify/ptrace"},
        )
        new = row(
            event_id="new",
            emitted_at="2026-08-26T03:00:00Z",
            series={"cell": "c-programs/flock-lifecycle/verify/ptrace"},
        )
        other = row(event_id="other", series={"cell": "backend-parity-c/other/verify/ptrace"})
        stored = [new, other, old]
        for test in ("c-programs/flock-lifecycle", "backend-parity-c/flock-lifecycle"):
            with self.subTest(test=test):
                joined = series.history_rows(stored, test, retired=retired)
                self.assertEqual([item["event_id"] for item in joined], ["old", "new"])
                self.assertEqual(
                    [item["series"]["cell"] for item in joined],
                    [
                        "backend-parity-c/flock-lifecycle/verify/ptrace",
                        "c-programs/flock-lifecycle/verify/ptrace",
                    ],
                )
        # No map: one exact test id, as before the map existed.
        self.assertEqual(
            [item["event_id"] for item in series.history_rows(stored, "c-programs/flock-lifecycle")],
            ["new"],
        )
        self.assertEqual(
            [
                item["event_id"]
                for item in series.history_rows(stored, "backend-parity-c/flock-lifecycle", retired=None)
            ],
            ["old"],
        )

    def test_refuses_every_map_hermit_refuses(self):
        flock = "backend-parity-c/flock-lifecycle"
        cases = []

        def case(label, needle, document):
            cases.append((label, needle, document))

        base = self.fixture()
        case("not json", "invalid retired-id map", "{")
        case("unknown top-level field", "exactly schema",
             {**base, "extra": 1})
        case("missing top-level field", "exactly schema",
             {key: value for key, value in base.items() if key != "description"})
        case("schema 2", "schema must be 1", {**base, "schema": 2})
        case("schema true", "schema must be 1", {**base, "schema": True})
        case("schema float", "schema must be 1", {**base, "schema": 1.0})
        case("blank description", "description", {**base, "description": " "})
        case("no retirement", "has no retirement", {**base, "retirements": []})
        case("unknown retirement field", "must hold exactly",
             {**base, "retirements": [{**base["retirements"][0], "extra": 1}]})
        case("self successor", "names itself",
             self.fixture(successor_bucket="backend-parity-c"))
        case("bucket twice", "retired twice",
             {**base, "retirements": [base["retirements"][0], base["retirements"][0]]})
        case("short sha", "last_live_commit", self.fixture(last_live_commit="3f66a249"))
        case("upper sha", "last_live_commit",
             self.fixture(last_live_commit="3F66A249B30FADA86B81E722B8E5439AC0789F8E"))
        case("blank reason", "reason", self.fixture(reason=""))
        case("no ids", "retires no id", self.fixture(ids={}, collisions={}))
        case("non-string successor", "strings to strings", self.fixture(ids={flock: 1}))
        case("collision not retired", "is not a retired id",
             self.fixture(collisions={"backend-parity-c/absent": "reason"}))
        case("blank collision reason", "reason",
             self.fixture(collisions={"backend-parity-c/pidfd-open-self": " "}))
        case("retired id outside bucket", "is not a test of bucket backend-parity-c",
             self.fixture(ids={"c-programs/flock-lifecycle": "c-programs/flock-lifecycle"}, collisions={}))
        case("uppercase name", "is not a test of bucket backend-parity-c",
             self.fixture(ids={"backend-parity-c/Flock": "c-programs/Flock"}, collisions={}))
        case("successor outside bucket", "is not a test of bucket c-programs",
             self.fixture(ids={flock: "applications/flock-lifecycle"}, collisions={}))
        case("unlisted rename", "neither the prefix rename",
             self.fixture(ids={flock: "c-programs/flock-pair"}, collisions={}))
        case("listed plain rename", "plain prefix rename",
             self.fixture(ids={flock: "c-programs/flock-lifecycle"}, collisions={flock: "why"}))
        case("shared successor", "shared by two",
             self.fixture(
                 ids={flock: "c-programs/flock-lifecycle",
                      "backend-parity-c/twin": "c-programs/flock-lifecycle"},
                 collisions={"backend-parity-c/twin": "reuses flock"},
             ))
        case("chain", "both a retired id and a successor", self._chained())
        for label, needle, document in cases:
            with self.subTest(label=label):
                text = document if isinstance(document, str) else json.dumps(document)
                with self.assertRaises(series.SeriesError) as raised:
                    series.parse_retired_ids(text)
                self.assertIn(needle, str(raised.exception))

    def _chained(self):
        return {
            "schema": 1,
            "description": "fixture map",
            "retirements": [
                {
                    "retired_bucket": "old-a",
                    "successor_bucket": "old-b",
                    "last_live_commit": "a" * 40,
                    "reason": "first fold",
                    "ids": {"old-a/x": "old-b/x"},
                    "collisions": {},
                },
                {
                    "retired_bucket": "old-b",
                    "successor_bucket": "live",
                    "last_live_commit": "b" * 40,
                    "reason": "second fold",
                    "ids": {"old-b/x": "live/x"},
                    "collisions": {},
                },
            ],
        }

    def test_retired_ids_at_reads_the_committed_file_or_reports_none(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary) / "hermit"
            repo.mkdir()

            def git(*args):
                return subprocess.run(
                    ["git", "-C", str(repo), *args], check=True, capture_output=True, text=True,
                ).stdout.strip()

            # Not a checkout: no map, exact joins.
            self.assertIsNone(series.retired_ids_at(repo, "HEAD"))
            git("init", "-q", "-b", "main")
            git("config", "user.name", "Series Test")
            git("config", "user.email", "series@example.invalid")
            (repo / "one").write_text("one\n")
            git("add", "one")
            git("commit", "-qm", "one")
            before = git("rev-parse", "HEAD")
            # A commit that predates the file, and a ref that does not resolve.
            self.assertIsNone(series.retired_ids_at(repo, "HEAD"))
            self.assertIsNone(series.retired_ids_at(repo, "origin/main"))
            path = repo / series.RETIRED_IDS_FILE
            path.parent.mkdir(parents=True)
            path.write_text(self.fixture_text())
            # An uncommitted working-tree file is not read.
            self.assertIsNone(series.retired_ids_at(repo, "HEAD"))
            git("add", series.RETIRED_IDS_FILE)
            git("commit", "-qm", "map")
            after = git("rev-parse", "HEAD")
            commit, retired = series.retired_ids_at(repo, "HEAD")
            self.assertEqual(commit, after)
            self.assertEqual(retired["backend-parity-c/other"], "c-programs/other")
            self.assertIsNone(series.retired_ids_at(repo, before))
            # The committed bytes are read, not the working tree's.
            path.write_text("{")
            self.assertEqual(series.retired_ids_at(repo, "HEAD")[0], after)
            git("commit", "-qam", "break")
            with self.assertRaises(series.SeriesError) as raised:
                series.retired_ids_at(repo, "HEAD")
            self.assertIn(f"{series.RETIRED_IDS_FILE} at Hermit ", str(raised.exception))
            self.assertIn("invalid retired-id map", str(raised.exception))


class ManifestScalarResolution(unittest.TestCase):
    """series reads Hermit manifests with the scalar rules of Hermit's own loader.

    ``ci/manifest-plan/src/manifest_value.rs`` parses manifests with serde_yaml
    0.9.34 (hermit Cargo.lock at pin 10770b2f34686587418598fbde9c48de9445ed76).
    GOLDEN below is serde_yaml's own reading of ``v: <scalar>``, recorded on
    2026-09-28 by a probe that deserialized each line into a
    ``serde_yaml::Mapping`` (cargo --offline, serde_yaml =0.9.34); REFUSED is
    a serde_yaml error. PyYAML's default YAML 1.1 rules disagree on several.
    """

    REFUSED = object()

    def golden(self):
        REFUSED = self.REFUSED
        return [
        ('7', 7),
        ('+7', 7),
        ('0x7', 7),
        ('0o7', 7),
        ('0b111', 7),
        ('-7', -7),
        ('-0x7', -7),
        ('-0o7', -7),
        ('-0b111', -7),
        ('+0x7', 7),
        ('07', '07'),
        ('-07', '-07'),
        ('00', '00'),
        ('0', 0),
        ('-0', 0),
        ('1_0', '1_0'),
        ('yes', 'yes'),
        ('no', 'no'),
        ('on', 'on'),
        ('off', 'off'),
        ('Yes', 'Yes'),
        ('y', 'y'),
        ('true', True),
        ('True', True),
        ('TRUE', True),
        ('tRUE', 'tRUE'),
        ('false', False),
        ('~', None),
        ('null', None),
        ('Null', None),
        ('NULL', None),
        ('nUll', 'nUll'),
        ('1:30', '1:30'),
        ('2001-12-14', '2001-12-14'),
        ('1e3', float('1000')),
        ('1E3', float('1000')),
        ('1.', float('1')),
        ('.5', float('0.5')),
        ('0.5', float('0.5')),
        ('01.5', float('1.5')),
        ('-1.5', float('-1.5')),
        ('+1.5', float('1.5')),
        ('+-1.5', '+-1.5'),
        ('.inf', math.inf),
        ('-.inf', -math.inf),
        ('+.inf', math.inf),
        ('.nan', math.nan),
        ('+.nan', '+.nan'),
        ('inf', 'inf'),
        ('nan', 'nan'),
        ('1e999', '1e999'),
        ('0x', '0x'),
        ('0x+5', '0x+5'),
        ('0xg', '0xg'),
        ('+-5', '+-5'),
        ('-', REFUSED),
        ('.', '.'),
        ('e5', 'e5'),
        ('340282366920938463463374607431768211456', float('340282366920938500000000000000000000000')),
        ('340282366920938463463374607431768211455', REFUSED),
        ('-170141183460469231731687303715884105728', REFUSED),
        ('-170141183460469231731687303715884105729', float('-170141183460469230000000000000000000000')),
        ('18446744073709551616', REFUSED),
        ('<<', '<<'),
        ('=', '='),
        ('the fixture guest fails on purpose', 'the fixture guest fails on purpose'),
        ("'7'", '7'),
        ('"7"', '7'),
        ]

    def read(self, loader, scalar):
        import yaml

        return yaml.load(f"v: {scalar}\n", Loader=loader)["v"]

    def assertSameValue(self, actual, expected, scalar):
        self.assertIs(type(actual), type(expected), scalar)
        if isinstance(expected, float) and math.isnan(expected):
            self.assertTrue(math.isnan(actual), scalar)
        else:
            self.assertEqual(actual, expected, scalar)

    def test_plain_scalars_resolve_exactly_as_serde_yaml_resolves_them(self):
        import yaml

        loader = series._manifest_yaml_loader(yaml)
        for scalar, expected in self.golden():
            with self.subTest(scalar=scalar):
                if expected is self.REFUSED:
                    with self.assertRaises(yaml.YAMLError):
                        self.read(loader, scalar)
                else:
                    self.assertSameValue(self.read(loader, scalar), expected, scalar)

    def test_pyyaml_default_rules_would_misread_the_manifest(self):
        # The reason the loader exists: each of these is read differently by
        # PyYAML's SafeLoader, so a manifest spelling that Hermit's loader
        # accepts would be refused, or read as another value.
        import yaml

        golden = dict(self.golden())
        default = getattr(yaml, "CSafeLoader", yaml.SafeLoader)
        for scalar in ("yes", "on", "0o7", "07", "1_0", "1:30", "2001-12-14", "1e3"):
            with self.subTest(scalar=scalar):
                misread = self.read(default, scalar)
                self.assertFalse(
                    type(misread) is type(golden[scalar]) and misread == golden[scalar],
                    f"PyYAML reads {scalar!r} as {misread!r}, which serde_yaml also reads",
                )

    def test_duplicate_and_non_string_keys_are_refused_as_hermit_refuses_them(self):
        import yaml

        loader = series._manifest_yaml_loader(yaml)
        for text, message in (
            ("code: 7\ncode: 9\n", "duplicate manifest key 'code'"),
            ("a:\n  code: 7\n  code: 7\n", "duplicate manifest key 'code'"),
            ("7: x\n", "is not a string"),
            ("true: x\n", "is not a string"),
            ("~: x\n", "is not a string"),
        ):
            with self.subTest(text=text), self.assertRaisesRegex(yaml.YAMLError, message):
                yaml.load(text, Loader=loader)
        # A quoted key is a string, and `<<` is an ordinary key, not a merge.
        self.assertEqual(
            yaml.load("'7': x\n<<: y\n", Loader=loader), {"7": "x", "<<": "y"}
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
