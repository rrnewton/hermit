#!/usr/bin/env python3
"""Check the refusal predicate used by the validate stop-path test.

This check is deliberately about behavior, not Python spelling.  The defect in
https://github.com/rrnewton/hermit/pull/2637 was that `_looks_refused()` searched
the complete captured channel for refusal-shaped text.  That turned ordinary
failures which merely mentioned that text into `ValidateChildRefused`.

The stop-path test cannot run inside `make lint-checks`: it starts a full
validate, and a validate started from inside validation is refused.  Importing
the file is safe, however, because its process-spawning work is guarded by
`if __name__ == "__main__"`.  This check therefore drives the real
`wait_for_text()` path with both genuine refusal summaries and ordinary failure
output.

Before pull request 2637 lands, neither `ValidateChildRefused` nor
`_looks_refused` exists and there is no refusal-classification path to check.
Once either name appears, `ValidateChildRefused` and `wait_for_text` must exist,
and the consumer must classify every case correctly.  A source-only check cannot
decide whether arbitrary rewritten Python has the same behavior; executing the
real consumer on the required inputs can.

⚠️ AN ABSENT CLASSIFICATION IS NOT A PASS, AND THIS IS THE PART THAT WAS WRONG.
Measured on 2026-08-26 at e2bcf4d1a34a142abbae5ea8cf8d162bb33a3895 and again at
633d4a5b43d277ac72ed088d29cf4222bba9d032, this file printed
`PASS: ... refusal-classification path is not present` and exited 0 with ZERO of
its ten channel examples evaluated, because `scripts/test_validate_stop_paths.py`
contains neither name yet.  A check that reports success because it had nothing to
check is the defect this gate exists to remove, reproduced inside the gate.  The
same output appeared when both names were merely RENAMED in the ported file with
the whole-channel defect intact, so the reassuring word was printed over a real
miss.

It is reported as could-not-evaluate instead.  `ci/lint-checks-node.sh` already
reads a line beginning at column zero with `NO-RESULT-CASE:` and turns the node
into exit 75, which `scripts/validate.rs` classifies `no_result` rather than a
pass; `scripts/test_validate_stop_paths.py` already emits that marker.  This
reuses that channel rather than adding one.  The exit status stays 0 so `make`
continues to the remaining checkers, and a real failure still outranks the marker.

Exit status:
    0  wait_for_text classifies every case correctly, OR the classification could
       not be found -- in which case a NO-RESULT-CASE: line makes the node a
       no_result rather than a pass
    1  the path is incomplete, the corpus is one-sided, or a case is wrong
    2  usage or load error
"""

from __future__ import annotations

import runpy
import sys
import tempfile
from collections.abc import Callable, Sequence
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TARGET = ROOT / "scripts" / "test_validate_stop_paths.py"
SAMPLE_COMMIT = "0123456789abcdef0123456789abcdef01234567"

# These are channel contents, not fragments of an implementation.  The eight
# False cases are ordinary failures; seven contain one of the old predicate's
# substrings, while the complete-looking wrapper line checks the required line
# context.  The two True cases are final refusal summaries emitted by validate,
# including a channel shared with other writers.  Against this exact corpus the
# original predicate produces seven false refusals and misses the standalone
# genuine refusal.
CASES: tuple[tuple[str, bool], ...] = (
    ("thread 'main' panicked at src/x.rs:9: connection refused by: peer", False),
    ("guest: server said 'refused by: firewall'\nerror: compilation failed", False),
    ("error[E0433]: file /home/x/validate: REFUSED_cases/t.rs not found", False),
    ("refused by: guest firewall policy", False),
    ("   refused by: guest firewall policy", False),
    ("validate: REFUSED_cases is a guest label", False),
    ("another validate is already running in this documentation", False),
    (
        f"wrapper: 🚫 validate REFUSED (exit 3) — profile full @ {SAMPLE_COMMIT}",
        False,
    ),
    (
        "refused by: guest firewall policy\n"
        f"🚫 validate REFUSED (exit 3) — profile full @ {SAMPLE_COMMIT}\n"
        "   refused by: the per-checkout invocation lock\n"
        "   another validate is already running",
        True,
    ),
    (
        f"🚫 validate REFUSED (exit 2) — profile strict @ {SAMPLE_COMMIT}",
        True,
    ),
)


class CheckFailed(RuntimeError):
    """The refusal classification or its required corpus is wrong."""


class ExitedProcess:
    """The part of subprocess.Popen that wait_for_text observes after exit."""

    returncode = 1

    def poll(self) -> int:
        return self.returncode


def check_wait_for_text(
    wait_for_text: Callable[[Path, str, object], object],
    refusal_type: type[BaseException],
    cases: Sequence[tuple[str, bool]],
    *,
    subject: str,
) -> None:
    expected = {want for _, want in cases}
    if expected != {False, True}:
        raise CheckFailed(
            f"{subject}: refusal predicate corpus must contain refusing and "
            "non-refusing cases"
        )

    wrong: list[str] = []
    for sample, want in cases:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "validate.log"
            log.write_text(sample, encoding="utf-8")
            try:
                wait_for_text(log, "REFUSAL-CHECK-READY-MARKER", ExitedProcess())
            except Exception as exc:  # noqa: BLE001 - classify the target exception
                if isinstance(exc, refusal_type):
                    got = True
                elif isinstance(exc, AssertionError):
                    got = False
                else:
                    raise CheckFailed(
                        f"{subject}: wait_for_text raised {type(exc).__name__}: {exc}"
                    ) from exc
            else:
                raise CheckFailed(
                    f"{subject}: wait_for_text returned after the process had exited"
                )
        if got is not want:
            direction = "must" if want else "must NOT"
            wrong.append(f"{direction} classify as refused: {sample!r}")

    if wrong:
        raise CheckFailed(
            f"{subject}: refusal classification misclassified {len(wrong)} of "
            f"{len(cases)} channel examples:\n  " + "\n  ".join(wrong)
        )


def check_target(path: Path) -> bool:
    """Check the real predicate.  Return False when the path is not present yet."""
    try:
        namespace = runpy.run_path(str(path), run_name="validate_stop_paths_check")
    except (OSError, SyntaxError) as exc:
        raise RuntimeError(f"could not load {path}: {exc}") from exc
    except Exception as exc:  # noqa: BLE001 - import failure must not become green
        raise RuntimeError(
            f"{path}: loading the stop-path test raised {type(exc).__name__}: {exc}"
        ) from exc

    has_exception = "ValidateChildRefused" in namespace
    has_predicate = "_looks_refused" in namespace
    if not has_exception and not has_predicate:
        # ⚠️ NOT A PASS. main() reports this as a no_result; see the module
        # docstring for why the reassuring word was the defect here.
        return False
    refusal_type = namespace.get("ValidateChildRefused")
    if not isinstance(refusal_type, type) or not issubclass(
        refusal_type, BaseException
    ):
        raise CheckFailed(
            f"{path}: refusal classification exists without a usable "
            "ValidateChildRefused exception"
        )
    wait_for_text = namespace.get("wait_for_text")
    if not callable(wait_for_text):
        raise CheckFailed(
            f"{path}: ValidateChildRefused exists but wait_for_text is not callable"
        )

    check_wait_for_text(
        wait_for_text,
        refusal_type,
        CASES,
        subject=f"{path}:wait_for_text refusal classification",
    )
    return True


def self_test() -> None:
    import re

    summary = re.compile(
        r"🚫 validate REFUSED \(exit [1-9][0-9]*\) — profile .+ @ [0-9a-f]{40}"
    )

    def correct(output: str) -> bool:
        return any(summary.fullmatch(line) for line in output.splitlines())

    shapes = ("refused by:", "validate: REFUSED", "another validate is already running")

    def original_defect(output: str) -> bool:
        return any(shape in output for shape in shapes)

    class Refused(RuntimeError):
        pass

    def wait_with(
        predicate: Callable[[str], bool],
    ) -> Callable[[Path, str, object], None]:
        def wait(log: Path, _text: str, _process: object) -> None:
            if predicate(log.read_text(encoding="utf-8")):
                raise Refused("refused")
            raise AssertionError("ordinary failure")

        return wait

    correct_wait = wait_with(correct)
    check_wait_for_text(
        correct_wait,
        Refused,
        CASES,
        subject="self-test correct wait_for_text",
    )

    expected_failures = (
        ("original whole-channel predicate", wait_with(original_defect), CASES),
        (
            "predicate that misses every refusal",
            wait_with(lambda _output: False),
            CASES,
        ),
        ("empty corpus", correct_wait, ()),
        (
            "non-refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if not case[1]),
        ),
        (
            "refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if case[1]),
        ),
    )
    for name, wait_for_text, cases in expected_failures:
        try:
            check_wait_for_text(
                wait_for_text,
                Refused,
                cases,
                subject=f"self-test {name}",
            )
        except CheckFailed:
            pass
        else:
            raise AssertionError(f"self-test did not refuse {name}")

    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory) / "test_validate_stop_paths.py"
        fixture.write_text("value = 1\n", encoding="utf-8")
        if check_target(fixture):
            raise AssertionError("self-test treated an absent refusal path as present")

        # ⚠️ AND THAT ABSENCE MUST REACH THE NODE AS A no_result, NOT AS A PASS.
        # This is the case that was wrong: zero of the ten examples evaluated, and
        # the word printed was PASS. The marker has to be on stdout at column zero
        # or ci/lint-checks-node.sh will not see it, and the exit status has to
        # stay 0 or the remaining checkers in the recipe never run.
        import subprocess

        absent = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), str(fixture)],
            capture_output=True,
            text=True,
        )
        if absent.returncode != 0:
            raise AssertionError(
                "self-test: an absent classification must not fail the recipe, got "
                f"rc={absent.returncode}"
            )
        if not any(
            line.startswith("NO-RESULT-CASE:") for line in absent.stdout.splitlines()
        ):
            raise AssertionError(
                "self-test: an absent classification must emit NO-RESULT-CASE: at "
                f"column zero on stdout, got {absent.stdout!r}"
            )
        if "PASS" in absent.stdout:
            raise AssertionError(
                "self-test: an absent classification must not print PASS, got "
                f"{absent.stdout!r}"
            )

        fixture.write_text(
            "class ValidateChildRefused(RuntimeError):\n    pass\n",
            encoding="utf-8",
        )
        try:
            check_target(fixture)
        except CheckFailed:
            pass
        else:
            raise AssertionError("self-test accepted a class without its predicate")


def usage() -> int:
    print(
        f"usage: {Path(sys.argv[0]).name} [--self-test | PATH]",
        file=sys.stderr,
    )
    return 2


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        try:
            self_test()
        except (AssertionError, CheckFailed, RuntimeError) as exc:
            print(f"FAIL: {exc}", file=sys.stderr)
            return 1
        print(f"PASS: {Path(argv[0]).name} self-test")
        return 0

    if len(argv) > 2 or (len(argv) == 2 and argv[1].startswith("-")):
        return usage()
    target = Path(argv[1]) if len(argv) == 2 else DEFAULT_TARGET
    if not target.is_file():
        print(f"error: target is not a file: {target}", file=sys.stderr)
        return 2

    try:
        present = check_target(target)
    except CheckFailed as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if present:
        print(
            f"PASS: {target}: wait_for_text refusal classification passed "
            f"{len(CASES)} channel examples"
        )
        return 0
    # ⚠️ COLUMN ZERO, AND FLUSHED. ci/lint-checks-node.sh matches this marker
    # anchored at the start of a line, and merges stdout and stderr with 2>&1, so
    # an unterminated earlier line could otherwise leave it mid-line and unseen.
    sys.stdout.flush()
    print(
        f"NO-RESULT-CASE: no refusal classification found in {target}, so none of "
        f"the {len(CASES)} channel examples were evaluated"
    )
    print(
        "  This is not a pass. Either "
        "https://github.com/rrnewton/hermit/pull/2637 has not landed yet, or the\n"
        "  classification was renamed past this check's discovery. Both leave it\n"
        "  unprotected, and neither is evidence that it is correct.",
        file=sys.stderr,
    )
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
