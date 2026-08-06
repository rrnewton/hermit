#!/usr/bin/env python3
"""Both crux properties, each paired with the case that would catch it cheating.

An idempotency test passes trivially if the writer emits nothing, and a
monotonicity lint passes trivially if it never blocks. So every property below
has a partner asserting the opposite direction actually fires.
"""
import json, subprocess, sys, tempfile, unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
TOOL = HERE / "scorecard.py"
sys.path.insert(0, str(HERE))
from scorecard import aggregate, find_decreases, load_csv, read_rows, render_csv  # noqa: E402


def rec(**kw):
    base = dict(category="c-programs", test="t1", mode="verify", backend="ptrace",
                outcome="PASS",  # REAL casing emitted by ci/test_harness.sh effective_args=["--strict"],
                run_id="RUN-1", duration_ms=123, binary_sha256="aa", hermit_sha="bb",
                source_tree_dirty=False)
    base.update(kw)
    return json.dumps(base)


class TestIdempotence(unittest.TestCase):
    def test_same_input_twice_is_byte_identical(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "r.jsonl"; p.write_text(rec() + "\n")
            self.assertEqual(render_csv(read_rows([p])), render_csv(read_rows([p])))

    def test_run_metadata_changes_do_NOT_change_the_scorecard(self):
        """The crux: two runs of the same code differ only in metadata."""
        with tempfile.TemporaryDirectory() as d:
            a = Path(d) / "a.jsonl"; a.write_text(rec(run_id="RUN-1", duration_ms=10, binary_sha256="x") + "\n")
            b = Path(d) / "b.jsonl"; b.write_text(rec(run_id="RUN-2", duration_ms=99, binary_sha256="y") + "\n")
            self.assertEqual(render_csv(read_rows([a])), render_csv(read_rows([b])))

    def test_but_a_real_capability_change_DOES_change_it(self):
        """Partner to the above: the writer is not simply ignoring everything."""
        with tempfile.TemporaryDirectory() as d:
            a = Path(d) / "a.jsonl"; a.write_text(rec(outcome="PASS") + "\n")
            b = Path(d) / "b.jsonl"; b.write_text(rec(outcome="FAIL") + "\n")
            self.assertNotEqual(render_csv(read_rows([a])), render_csv(read_rows([b])))

    def test_input_order_does_not_matter(self):
        with tempfile.TemporaryDirectory() as d:
            a = Path(d) / "a.jsonl"; a.write_text(rec(test="t1") + "\n" + rec(test="t2") + "\n")
            b = Path(d) / "b.jsonl"; b.write_text(rec(test="t2") + "\n" + rec(test="t1") + "\n")
            self.assertEqual(render_csv(read_rows([a])), render_csv(read_rows([b])))

    def test_no_run_metadata_field_appears_in_the_csv(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "r.jsonl"; p.write_text(rec() + "\n")
            text = render_csv(read_rows([p]))
            for bad in ("RUN-1", "123", "aa", "bb"):
                self.assertNotIn(bad, text.replace("c-programs", ""))


class TestMonotonicityLint(unittest.TestCase):
    def _rows(self, det=1, heap=0):
        return [{"bucket": "b", "test": "t", "mode": "run", "backend": "ptrace",
                 "strict": 1, "detlog_stack": 0, "detlog_heap": heap, "chaos": 0,
                 "determinism": det, "parity": ""}]

    def test_increase_is_allowed(self):
        self.assertEqual(find_decreases(self._rows(det=0), self._rows(det=1)), [])

    def test_equal_is_allowed(self):
        self.assertEqual(find_decreases(self._rows(), self._rows()), [])

    def test_determinism_regression_is_caught(self):
        self.assertEqual(len(find_decreases(self._rows(det=1), self._rows(det=0))), 1)

    def test_losing_a_stricter_tier_is_caught(self):
        self.assertEqual(len(find_decreases(self._rows(heap=1), self._rows(heap=0))), 1)

    def test_deleting_a_cell_is_caught(self):
        """Otherwise a regression could be hidden by removing the row."""
        self.assertEqual(len(find_decreases(self._rows(), [])), 1)


class TestLintCLI(unittest.TestCase):
    def _write(self, d, name, det):
        p = Path(d) / name
        p.write_text("bucket,test,mode,backend,strict,detlog_stack,detlog_heap,chaos,determinism,parity\n"
                     f"b,t,run,ptrace,1,0,0,0,{det},\n")
        return p

    def test_planted_decrease_is_BLOCKED(self):
        with tempfile.TemporaryDirectory() as d:
            old = self._write(d, "old.csv", 1); new = self._write(d, "new.csv", 0)
            r = subprocess.run([sys.executable, str(TOOL), "lint", "--old", str(old), "--new", str(new)],
                               capture_output=True, text=True)
            self.assertEqual(r.returncode, 1)
            self.assertIn("BLOCKED", r.stdout)

    def test_decrease_with_strong_reason_and_P0_is_allowed(self):
        with tempfile.TemporaryDirectory() as d:
            old = self._write(d, "old.csv", 1); new = self._write(d, "new.csv", 0)
            reason = Path(d) / "reason.txt"
            reason.write_text("fake-green found: the cell never executed. P0 task: restore-cell-x")
            r = subprocess.run([sys.executable, str(TOOL), "lint", "--old", str(old),
                                "--new", str(new), "--reason", str(reason)], capture_output=True, text=True)
            self.assertEqual(r.returncode, 0)
            self.assertIn("ALLOWED", r.stdout)

    def test_a_weak_reason_does_NOT_unlock_it(self):
        """Partner: the escape hatch must not accept arbitrary prose."""
        with tempfile.TemporaryDirectory() as d:
            old = self._write(d, "old.csv", 1); new = self._write(d, "new.csv", 0)
            reason = Path(d) / "reason.txt"; reason.write_text("flaky, will fix later")
            r = subprocess.run([sys.executable, str(TOOL), "lint", "--old", str(old),
                                "--new", str(new), "--reason", str(reason)], capture_output=True, text=True)
            self.assertEqual(r.returncode, 1)

    def test_strong_reason_without_a_P0_does_NOT_unlock_it(self):
        with tempfile.TemporaryDirectory() as d:
            old = self._write(d, "old.csv", 1); new = self._write(d, "new.csv", 0)
            reason = Path(d) / "reason.txt"; reason.write_text("fake-green found in this cell")
            r = subprocess.run([sys.executable, str(TOOL), "lint", "--old", str(old),
                                "--new", str(new), "--reason", str(reason)], capture_output=True, text=True)
            self.assertEqual(r.returncode, 1)

    def test_no_change_passes(self):
        with tempfile.TemporaryDirectory() as d:
            old = self._write(d, "old.csv", 1); new = self._write(d, "new.csv", 1)
            r = subprocess.run([sys.executable, str(TOOL), "lint", "--old", str(old), "--new", str(new)],
                               capture_output=True, text=True)
            self.assertEqual(r.returncode, 0)


class TestAggregate(unittest.TestCase):
    def test_table_has_buckets_total_and_total_of_totals(self):
        rows = [{"bucket": "b1", "test": "t", "mode": "run", "backend": "ptrace",
                 "strict": 1, "detlog_stack": 0, "detlog_heap": 0, "chaos": 0,
                 "determinism": 1, "parity": ""},
                {"bucket": "b2", "test": "t", "mode": "run", "backend": "dbi",
                 "strict": 1, "detlog_stack": 0, "detlog_heap": 0, "chaos": 0,
                 "determinism": 0, "parity": ""}]
        t = aggregate(rows)
        self.assertIn("ptrace", t); self.assertIn("dbi", t)
        self.assertIn("b1", t); self.assertIn("b2", t)
        self.assertIn("TOTAL", t); self.assertIn("TOTAL-OF-TOTALS: 1/2", t)

    def test_ptrace_is_the_leftmost_backend_column(self):
        rows = [{"bucket": "b", "test": "t", "mode": "run", "backend": be,
                 "strict": 1, "detlog_stack": 0, "detlog_heap": 0, "chaos": 0,
                 "determinism": 1, "parity": ""} for be in ("kvm", "ptrace", "dbi")]
        header = aggregate(rows).splitlines()[0]
        self.assertLess(header.index("ptrace"), header.index("dbi"))
        self.assertLess(header.index("ptrace"), header.index("kvm"))


class TestRealProducerCasing(unittest.TestCase):
    """Regression: the harness emits UPPERCASE outcomes. A case-sensitive compare
    scored every passing cell 0, and the fixtures hid it by using lowercase."""

    def test_uppercase_PASS_counts_as_determinism(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "r.jsonl"; p.write_text(rec(outcome="PASS") + "\n")
            self.assertEqual(read_rows([p])[0]["determinism"], 1)

    def test_uppercase_FAIL_counts_as_zero(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "r.jsonl"; p.write_text(rec(outcome="FAIL") + "\n")
            self.assertEqual(read_rows([p])[0]["determinism"], 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
