#!/usr/bin/env python3
"""Exercise corpus admission with controlled transport and the real typed reader.

No compiler, Hermit guest, or e9patch process is executed. VERIFICATION_REPORT_BIN
must identify the current built reader supplied by the existing test caller.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import e9patch_corpus as corpus  # noqa: E402


REAL_SUBPROCESS_RUN = subprocess.run
STDOUT = b"corpus-write\n"
GOLDEN_TAIL = (
    b"INFO detcore: inbound syscall: write(1, 0x1000, 13) = ?\n"
    b"INFO detcore: inbound syscall: exit_group(0) = ?\n"
)
E9PATCH_TAIL = (
    b"INFO detcore: inbound syscall: readlink(0x2000, 0x3000, 4096) = ?\n"
    + GOLDEN_TAIL
)


def canonical_report() -> dict:
    """Synthetic current evidence, not a retained guest measurement."""
    output = {
        "exit_code": 0,
        "signal": None,
        "stdout_sha256": hashlib.sha256(STDOUT).hexdigest(),
        "stdout_bytes": len(STDOUT),
        "stderr_sha256": hashlib.sha256(b"").hexdigest(),
        "stderr_bytes": 0,
    }
    return {
        "verified": True,
        "bitwise_parity": True,
        "verdict": "matched",
        "no_result_reason": None,
        "infrastructure_error": None,
        "comparison": {
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
        },
        "compared_log_messages": {"left": 123, "right": 123},
        "compared_outputs": {"left": dict(output), "right": dict(output)},
        "dbt_counted_branches": None,
        "runtime": None,
        "guest_exit_code": 0,
        "guest_signal": None,
        "first_divergent_scheduler_turn": None,
        "first_divergent_virtual_nanoseconds": None,
        "first_divergent_record": None,
        "first_divergent_syscall": None,
        "first_divergent_left_message": None,
        "first_divergent_right_message": None,
    }


def write_report(path: Path, kind: str) -> None:
    if kind == "missing":
        return
    if kind in ("empty", "invalid"):
        path.write_text("" if kind == "empty" else "{", encoding="utf-8")
        return
    report = canonical_report()
    if kind == "stripped":
        report["comparison"]["strictness"] = "stripped"
        report["bitwise_parity"] = False
    elif kind == "no-logs":
        report["comparison"]["compare_logs"] = False
    elif kind == "zero":
        report["compared_log_messages"] = {"left": 0, "right": 0}
    elif kind == "unequal":
        report["compared_log_messages"]["right"] = 124
    elif kind == "incomplete":
        del report["compared_outputs"]
    elif kind == "diverged":
        report["verified"] = False
        report["bitwise_parity"] = False
        report["verdict"] = "diverged"
    elif kind != "canonical":
        raise AssertionError(f"unknown report control: {kind}")
    path.write_text(json.dumps(report), encoding="utf-8")


class E9patchCorpusVerificationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not corpus.VERIFICATION_REPORT_BIN:
            raise RuntimeError("set VERIFICATION_REPORT_BIN to the current built typed reader")
        cls.reader = corpus.verification_report_bin(Path("/not-executed-hermit"))
        if not cls.reader.is_file() or not os.access(cls.reader, os.X_OK):
            raise RuntimeError(f"typed verification-report reader is not executable: {cls.reader}")

    def run_case(
        self,
        *,
        verify_status=None,
        report_kind=None,
        plain_status=None,
        plain_stdout=None,
        engagement=None,
        tails=None,
    ):
        verify_status = verify_status or {}
        report_kind = report_kind or {}
        plain_status = plain_status or {}
        plain_stdout = plain_stdout or {}
        tails = tails or {}
        engagement = engagement if engagement is not None else {
            "schema": 2,
            "engagement": {
                "backend": "e9patch",
                "candidate_sites": 4,
                "mapped_sites": 4,
                "b0_sites": 0,
            },
        }
        reader_calls = []
        guest_calls = []
        with tempfile.TemporaryDirectory(prefix="e9patch-verification-control-") as raw:
            root = Path(raw)
            hermit = root / "not-executed-hermit"
            guest = root / "not-compiled-guest"

            def transport(command, **kwargs):
                if command[0] == str(self.reader):
                    self.assertEqual(kwargs["timeout"], 10)
                    reader_calls.append(list(command))
                    return REAL_SUBPROCESS_RUN(command, **kwargs)
                self.assertEqual(command[0], str(hermit), command)
                self.assertIn("--strict", command)
                self.assertEqual(command[-2:], ["--", str(guest)])
                e9 = "e9patch" in command
                verify = "--verify" in command
                detlog = "--log=info" in command
                timeout = 60 if detlog else (90 if e9 else 60) if verify else (60 if e9 else 40)
                self.assertEqual(kwargs["timeout"], timeout)
                guest_calls.append((e9, verify, detlog, timeout))
                if verify:
                    path = Path(next(arg.split("=", 1)[1] for arg in command
                                     if arg.startswith("--verify-json=")))
                    write_report(path, report_kind.get(e9, "canonical"))
                    status = verify_status.get(e9, 0)
                    if status == "timeout":
                        # Exercise production run()'s TimeoutExpired -> 124 path.
                        raise subprocess.TimeoutExpired(command, kwargs["timeout"])
                    return subprocess.CompletedProcess(command, status, b"", b"verification transport")
                if detlog:
                    stderr = tails.get(e9, E9PATCH_TAIL if e9 else GOLDEN_TAIL)
                    return subprocess.CompletedProcess(command, 0, b"", stderr)
                if e9:
                    path = Path(next(arg.split("=", 1)[1] for arg in command
                                     if arg.startswith("--backend-engagement-json=")))
                    path.write_text(json.dumps(engagement), encoding="utf-8")
                return subprocess.CompletedProcess(
                    command, plain_status.get(e9, 0), plain_stdout.get(e9, STDOUT), b""
                )

            with mock.patch.object(corpus, "compile_guest", return_value=guest), mock.patch.object(
                corpus.subprocess, "run", side_effect=transport
            ):
                result = corpus.run_guest(hermit, "write_stdout", root)

            # Read both saved operands with the same real production helper,
            # independently of the caller's status decision above.
            saved = {
                e9: corpus.verification_matched(
                    hermit, root / f"write_stdout-{label}-verify.json"
                )[0]
                for e9, label in ((False, "golden"), (True, "e9patch"))
            }
        return result, reader_calls, guest_calls, saved

    def test_canonical_operands_and_independent_oracles_qualify(self) -> None:
        result, readers, guests, saved = self.run_case()
        self.assertEqual(result, ("PASS_L2", "exit=0 sites c/4 m/4 b0/0 prologue=1 tail_match=yes"))
        self.assertEqual(saved, {False: True, True: True})
        self.assertEqual(len(readers), 2)
        self.assertTrue(all(command[1] == "canonical-match" for command in readers))
        self.assertEqual(guests, [
            (False, False, False, 40), (False, True, False, 60),
            (True, False, False, 60), (True, True, False, 90),
            (False, False, True, 60), (True, False, True, 60),
        ])

    def test_valid_reports_do_not_excuse_either_failed_verification_process(self) -> None:
        for e9, label in ((False, "golden"), (True, "e9patch")):
            for status in (1, 126, 127, -9, "timeout"):
                with self.subTest(operand=label, status=status):
                    result, _, _, saved = self.run_case(verify_status={e9: status})
                    detail = f"{label} verification timed out" if status == "timeout" else (
                        f"{label} verification exited {status}"
                    )
                    self.assertEqual(result, ("FAIL", detail))
                    self.assertEqual(saved, {False: True, True: True})

    def test_both_operands_require_current_canonical_evidence(self) -> None:
        for e9, label in ((False, "golden"), (True, "e9patch")):
            for kind in ("stripped", "no-logs", "zero", "unequal", "incomplete",
                         "missing", "empty", "invalid", "diverged"):
                with self.subTest(operand=label, report=kind):
                    result, _, _, saved = self.run_case(report_kind={e9: kind})
                    self.assertEqual(result[0], "FAIL")
                    self.assertIn(f"{label} typed verification report did not match:", result[1])
                    self.assertFalse(saved[e9])
                    self.assertTrue(saved[not e9])

    def test_canonical_reports_do_not_excuse_plain_status_or_stdout_failures(self) -> None:
        cases = [
            ({"plain_status": {False: 124}}, "timeout (golden=124, e9patch=0)"),
            ({"plain_status": {True: 124}}, "timeout (golden=0, e9patch=124)"),
            ({"plain_status": {False: 2}}, "golden exit 2, expected 0"),
            ({"plain_status": {True: 3}}, "exit divergence golden=0 e9patch=3"),
            ({"plain_stdout": {True: b"different\n"}}, "stdout divergence"),
            ({"plain_stdout": {False: b"wrong\n", True: b"wrong\n"}}, "golden stdout"),
        ]
        for changes, detail in cases:
            with self.subTest(changes=changes):
                result, _, _, saved = self.run_case(**changes)
                self.assertEqual(result[0], "FAIL")
                self.assertIn(detail, result[1])
                self.assertEqual(saved, {False: True, True: True})

    def test_canonical_reports_do_not_excuse_missing_coverage_or_fallback(self) -> None:
        for candidate, mapped, fallback, detail in (
            (0, 0, 0, "candidate_sites=0"),
            (4, 3, 0, "incomplete coverage mapped=3 candidate=4"),
            (4, 4, 1, "b0_sites=1"),
        ):
            with self.subTest(candidate=candidate, mapped=mapped, fallback=fallback):
                engagement = {"schema": 2, "engagement": {
                    "backend": "e9patch", "candidate_sites": candidate,
                    "mapped_sites": mapped, "b0_sites": fallback,
                }}
                result, _, _, saved = self.run_case(engagement=engagement)
                self.assertEqual(result[0], "FAIL")
                self.assertIn(detail, result[1])
                self.assertEqual(saved, {False: True, True: True})

    def test_canonical_reports_do_not_excuse_the_existing_tail_mismatch(self) -> None:
        for tail in (b"", E9PATCH_TAIL.replace(b"write(1,", b"write(2,")):
            with self.subTest(tail=tail):
                result, _, _, saved = self.run_case(tails={True: tail})
                self.assertEqual(result[0], "FAIL")
                self.assertIn("guest-syscall DETLOG tail mismatch", result[1])
                self.assertEqual(saved, {False: True, True: True})


if __name__ == "__main__":
    unittest.main()
