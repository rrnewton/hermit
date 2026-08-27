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

The check refuses when the refusal-classification path is absent.  Pull request
2637 must therefore be underneath this change before it can land: once the gate
is scheduled, deleting or renaming both observed names cannot turn it green.
`ValidateChildRefused` and `wait_for_text` must exist, and the consumer must
classify every case correctly.  A source-only check cannot decide whether
arbitrary rewritten Python has the same behavior; executing the real consumer
on the required inputs can.

Exit status:
    0  wait_for_text classifies every case correctly
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

# These are channel contents plus the observed child status, not fragments of an
# implementation.  The eleven False cases are ordinary failures or refusals that
# reveal a broken fixture; eight contain one of the old predicate's substrings.
# The two True cases are the specific re-entrancy refusal that prevents the
# stop-test seam from running, including a channel shared with other writers.
# Against this exact corpus the original predicate produces eight false refusals
# and misses the standalone genuine refusal.
CASES: tuple[tuple[str, int, bool], ...] = (
    ("thread 'main' panicked at src/x.rs:9: connection refused by: peer", 1, False),
    ("guest: server said 'refused by: firewall'\nerror: compilation failed", 1, False),
    ("error[E0433]: file /home/x/validate: REFUSED_cases/t.rs not found", 1, False),
    ("refused by: guest firewall policy", 1, False),
    ("   refused by: guest firewall policy", 1, False),
    ("validate: REFUSED_cases is a guest label", 1, False),
    ("another validate is already running in this documentation", 1, False),
    (
        f"wrapper: 🚫 validate REFUSED (exit 3) — profile full @ {SAMPLE_COMMIT}",
        3,
        False,
    ),
    ("🚫 validate REFUSED is quoted documentation, not a final summary", 2, False),
    (
        f"🚫 validate REFUSED (exit 2) — profile full @ {SAMPLE_COMMIT}\n"
        "   refused by: argument parsing",
        2,
        False,
    ),
    (
        f"🚫 validate REFUSED (exit 2) — profile full @ {SAMPLE_COMMIT}\n"
        "   refused by: the re-entrancy guard",
        101,
        False,
    ),
    (
        "refused by: guest firewall policy\n"
        f"🚫 validate REFUSED (exit 2) — profile full @ {SAMPLE_COMMIT}\n"
        "   refused by: the re-entrancy guard\n"
        "   another validate is already running",
        2,
        True,
    ),
    (
        f"🚫 validate REFUSED (exit 2) — profile strict @ {SAMPLE_COMMIT}\n"
        "   refused by: the re-entrancy guard",
        2,
        True,
    ),
)


class CheckFailed(RuntimeError):
    """The refusal classification or its required corpus is wrong."""


class ExitedProcess:
    """The part of subprocess.Popen that wait_for_text observes after exit."""

    def __init__(self, returncode: int) -> None:
        self.returncode = returncode

    def poll(self) -> int:
        return self.returncode


def check_wait_for_text(
    wait_for_text: Callable[[Path, str, object], object],
    refusal_type: type[BaseException],
    cases: Sequence[tuple[str, int, bool]],
    *,
    subject: str,
) -> None:
    expected = {want for _, _, want in cases}
    if expected != {False, True}:
        raise CheckFailed(
            f"{subject}: refusal predicate corpus must contain refusing and "
            "non-refusing cases"
        )

    wrong: list[str] = []
    for sample, returncode, want in cases:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "validate.log"
            log.write_text(sample, encoding="utf-8")
            try:
                wait_for_text(
                    log,
                    "REFUSAL-CHECK-READY-MARKER",
                    ExitedProcess(returncode),
                )
            except BaseException as exc:  # target exit 0 must not bypass the gate
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


def check_target(path: Path) -> None:
    """Check the real wait_for_text refusal-classification path."""
    try:
        namespace = runpy.run_path(str(path), run_name="validate_stop_paths_check")
    except (OSError, SyntaxError) as exc:
        raise RuntimeError(f"could not load {path}: {exc}") from exc
    except BaseException as exc:  # target exit 0 must not bypass the gate
        raise RuntimeError(
            f"{path}: loading the stop-path test raised {type(exc).__name__}: {exc}"
        ) from exc

    has_exception = "ValidateChildRefused" in namespace
    has_predicate = "_looks_refused" in namespace
    if not has_exception and not has_predicate:
        raise CheckFailed(
            f"{path}: refusal-classification path is absent; this gate cannot "
            "pass without observing it"
        )
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


def self_test() -> None:
    import re

    summary = re.compile(
        r"🚫 validate REFUSED \(exit (?P<exit>[1-9][0-9]*)\) — profile .+ @ [0-9a-f]{40}"
    )

    reentrancy_reason = "   refused by: the re-entrancy guard"

    def correct(output: str, returncode: int) -> bool:
        lines = output.splitlines()
        for index, line in enumerate(lines):
            match = summary.fullmatch(line)
            if match is None or int(match.group("exit")) != returncode:
                continue
            if lines[index + 1 : index + 2] == [reentrancy_reason]:
                return True
        return False

    shapes = ("refused by:", "validate: REFUSED", "another validate is already running")

    def original_defect(output: str, _returncode: int) -> bool:
        return any(shape in output for shape in shapes)

    class Refused(RuntimeError):
        pass

    def wait_with(
        predicate: Callable[[str, int], bool],
    ) -> Callable[[Path, str, object], None]:
        def wait(log: Path, _text: str, process: object) -> None:
            if predicate(
                log.read_text(encoding="utf-8"), int(getattr(process, "returncode"))
            ):
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
        (
            "original whole-channel predicate",
            wait_with(original_defect),
            CASES,
            "misclassified",
        ),
        (
            "predicate that misses every refusal",
            wait_with(lambda _output, _returncode: False),
            CASES,
            "misclassified",
        ),
        (
            "empty corpus",
            correct_wait,
            (),
            "must contain refusing and non-refusing cases",
        ),
        (
            "non-refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if not case[2]),
            "must contain refusing and non-refusing cases",
        ),
        (
            "refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if case[2]),
            "must contain refusing and non-refusing cases",
        ),
        (
            "consumer exits zero",
            lambda _log, _text, _process: sys.exit(0),
            CASES,
            "raised SystemExit: 0",
        ),
    )
    for name, wait_for_text, cases, required_message in expected_failures:
        try:
            check_wait_for_text(
                wait_for_text,
                Refused,
                cases,
                subject=f"self-test {name}",
            )
        except CheckFailed as exc:
            if required_message not in str(exc):
                raise AssertionError(
                    f"self-test refused {name} for the wrong reason: {exc}"
                ) from exc
        else:
            raise AssertionError(f"self-test did not refuse {name}")

    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory) / "test_validate_stop_paths.py"
        fixture.write_text("value = 1\n", encoding="utf-8")
        try:
            check_target(fixture)
        except CheckFailed:
            pass
        else:
            raise AssertionError("self-test accepted an absent refusal path")

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

        fixture.write_text("raise SystemExit(0)\n", encoding="utf-8")
        try:
            check_target(fixture)
        except RuntimeError:
            pass
        else:
            raise AssertionError("self-test accepted a target that exits zero while loading")


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
        check_target(target)
    except CheckFailed as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    print(
        f"PASS: {target}: wait_for_text refusal classification passed "
        f"{len(CASES)} channel examples"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
