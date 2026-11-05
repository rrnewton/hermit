"""An operator's build width is theirs; the containment default is not allowed to eat it silently.

``derive_build_jobs`` computes ``CARGO_BUILD_JOBS`` from the resource envelope and used to write
it unconditionally at three sites. An operator who deliberately set ``CARGO_BUILD_JOBS=K`` — and
sized a memory cap against exactly that pool — had it replaced with no word said.

Reading ``CARGO_BUILD_JOBS`` back as "operator intent" is NOT the fix, and this file pins why: the
runner SETS that variable itself, so the in-scope process would read the runner's own scope-wide
derivation as an instruction and stop refining downward per step, which is precisely the 284-wide
condition against an 8 GiB cap that ``test_build_job_cap.py`` exists for. Intent is resolved once,
in the outermost process, and forwarded under its own name.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from dagrun.cgroup import Cgroups
from dagrun.sizing import (
    BUILD_JOBS_ENV,
    OPERATOR_BUILD_JOBS_ENV,
    choose_build_jobs,
    derive_build_jobs,
    parse_build_jobs,
    resolve_operator_build_jobs,
)

GIB = 1024**3


def test_a_positive_integer_is_intent() -> None:
    assert parse_build_jobs("12") == 12
    assert parse_build_jobs(" 12 ") == 12


def test_nothing_else_is_intent() -> None:
    # Each case named, because "return None for everything" would pass a single-case test and
    # would also throw away the real value above.
    assert parse_build_jobs(None) is None
    assert parse_build_jobs("") is None
    assert parse_build_jobs("   ") is None
    assert parse_build_jobs("0") is None
    assert parse_build_jobs("-4") is None
    assert parse_build_jobs("8.5") is None
    assert parse_build_jobs("many") is None
    assert parse_build_jobs("8 jobs") is None


def test_the_awkward_digit_strings_answer_exactly_as_the_rust_twin_does() -> None:
    # THE DIVERGENCE `make cross` CAUGHT IN SPIRIT AND COULD NOT REACH. `str.isdigit()` is not
    # `is_ascii_digit`, and a Python int is not an i64. This table is hand-written here and
    # duplicated verbatim in `sizing::tests::the_awkward_digit_strings_are_not_intent` on the
    # Rust side; the two must agree case for case.
    #
    # A full-width digit: int() parses it, so Python HONOURED it as operator intent while Rust
    # ignored it — one input, two build widths, in the one function the differential exists to
    # keep aligned.
    assert parse_build_jobs("８") is None
    # Superscript two is isdigit() but not int()-parseable. This raised ValueError, and because
    # the capture below happens at IMPORT it killed `import dagrun` outright.
    assert parse_build_jobs("8²") is None
    # Arabic-Indic digits: isdigit(), int()-parseable, not ASCII.
    assert parse_build_jobs("٨") is None
    # Wider than an i64 is not a width. Rust's parse fails; Python's int() would have succeeded
    # and exported CARGO_BUILD_JOBS=99999999999999999999999.
    assert parse_build_jobs("99999999999999999999999") is None
    assert parse_build_jobs("9223372036854775808") is None
    # A string long enough that CPython's int_max_str_digits refuses to convert it at all.
    assert parse_build_jobs("1" * 5000) is None
    # And the boundaries that must still be honoured, so "reject the awkward ones" cannot become
    # "reject everything large".
    assert parse_build_jobs("9223372036854775807") == 9223372036854775807
    # Rust's i64 parse accepts leading zeros, so Python must too.
    assert parse_build_jobs("000000008") == 8


def test_the_outermost_process_reads_the_ambient_variable() -> None:
    # Nothing forwarded: we ARE the outermost process, so CARGO_BUILD_JOBS is the operator's.
    assert resolve_operator_build_jobs(None, "200") == 200
    assert resolve_operator_build_jobs(None, None) is None


def test_a_forwarded_answer_beats_the_runners_own_write() -> None:
    # THE DEFECT THIS PREVENTS. In-scope, CARGO_BUILD_JOBS=8 is the runner's own derivation.
    # An empty forwarded value says "already asked; the operator wanted nothing", and that must
    # win over the runner's own number, or per-step refinement stops.
    assert resolve_operator_build_jobs("", "8") is None
    # And a forwarded real choice survives the re-exec.
    assert resolve_operator_build_jobs("200", "8") == 200


def test_the_containment_default_governs_when_nothing_was_stated() -> None:
    choice = choose_build_jobs(None, 284, 8 * GIB)
    assert choice.jobs == 8
    assert choice.derived == 8
    assert choice.source == "containment"
    assert choice.jobs == derive_build_jobs(284, 8 * GIB)


def test_a_stated_width_wins_and_the_derivation_is_still_recorded() -> None:
    choice = choose_build_jobs(200, 284, 8 * GIB)
    assert choice.jobs == 200
    assert choice.derived == 8, "the number that lost must survive, or an OOM is inexplicable"
    assert choice.source == "operator"


def test_the_notice_names_the_winner_the_loser_and_the_risk() -> None:
    said = choose_build_jobs(200, 284, 8 * GIB).describe()
    assert f"honouring {BUILD_JOBS_ENV}=200" in said
    assert "would have chosen 8" in said
    assert "can still OOM" in said


def test_the_notice_also_says_when_nothing_was_overridden() -> None:
    # "Told it was overridden, or not overridden" — silence in the second case would leave an
    # operator unable to tell a honoured setting from an ignored one.
    said = choose_build_jobs(None, 284, 8 * GIB).describe()
    assert f"no {BUILD_JOBS_ENV} in the environment" in said
    assert "governs at 8" in said
    assert "refined downward per step" in said


def _captured_in_subprocess(env: dict[str, str]) -> str:
    """What a FRESH interpreter captures at import, under exactly this environment."""
    import os
    import subprocess
    import sys

    child = dict(os.environ)
    for key in (BUILD_JOBS_ENV, OPERATOR_BUILD_JOBS_ENV):
        child.pop(key, None)
    child.update(env)
    child["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])
    out = subprocess.run(
        [
            sys.executable,
            "-c",
            "from dagrun.sizing import operator_build_jobs; "
            "print(operator_build_jobs())",
        ],
        env=child,
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip()


def test_the_capture_happens_at_import_from_the_real_environment() -> None:
    # A separate interpreter is the only honest way to observe an import-time capture; patching
    # the module attribute would prove the patch, not the wiring.
    assert _captured_in_subprocess({BUILD_JOBS_ENV: "200"}) == "200"
    assert _captured_in_subprocess({}) == "None"


def test_a_malformed_variable_cannot_take_the_package_down_at_import() -> None:
    # The capture is a module-level statement, so anything it raises is raised by
    # `import dagrun` — and then `capabilities`, `--help` and every other subcommand die with
    # a traceback because of an environment variable. `_captured_in_subprocess` runs a fresh
    # interpreter with `check=True`, so a raising import fails this test rather than being
    # swallowed.
    assert _captured_in_subprocess({BUILD_JOBS_ENV: "8²"}) == "None"
    assert _captured_in_subprocess({BUILD_JOBS_ENV: "1" * 5000}) == "None"
    assert _captured_in_subprocess({OPERATOR_BUILD_JOBS_ENV: "8²"}) == "None"


def test_an_inherited_scope_value_is_not_mistaken_for_intent() -> None:
    # Exactly the in-scope environment the runner creates: it wrote CARGO_BUILD_JOBS itself and
    # forwarded "the operator asked for nothing". Reading the first back as intent is the defect.
    assert (
        _captured_in_subprocess({BUILD_JOBS_ENV: "284", OPERATOR_BUILD_JOBS_ENV: ""}) == "None"
    )
    assert (
        _captured_in_subprocess({BUILD_JOBS_ENV: "284", OPERATOR_BUILD_JOBS_ENV: "12"}) == "12"
    )


def test_the_systemd_free_fallback_still_has_no_caller() -> None:
    """`enter_delegated_scope` is one of the "three sites" this work claimed to fix, and it is
    dead: nothing calls it, so its build-width announcement cannot print and the import-time
    capture's original justification ("it mutates os.environ later in the same process") names an
    event that never happens. Its docstring now says NOT CALLED in as many words. This is what
    stops that sentence rotting: wire the function up and this fails, which is the prompt to
    rewrite the docstring and the two comments in `sizing.py` that depend on it."""
    import re

    package = Path(__file__).resolve().parents[1] / "dagrun"
    call = re.compile(r"(?<![\w.])enter_delegated_scope\s*\(")
    callers = [
        f"{path.name}:{n}"
        for path in sorted(package.rglob("*.py"))
        for n, line in enumerate(path.read_text().splitlines(), 1)
        if call.search(line) and not line.lstrip().startswith("def ")
    ]
    assert callers == [], (
        "enter_delegated_scope has acquired a caller, so it is no longer dead: update its "
        f"NOT CALLED docstring and the sizing.py comments that cite it. Callers: {callers}"
    )


def _boxed(root: Path, *, cpu_max: str, memory_max: str) -> Cgroups:
    (root / "cpu.max").write_text(cpu_max)
    (root / "memory.max").write_text(memory_max)
    cg = Cgroups()
    cg.enabled = True
    cg.root = root
    return cg


def _wrapped_jobs(cmd: str) -> int:
    import re

    match = re.search(r"export CARGO_BUILD_JOBS=(\d+)", cmd)
    assert match, f"no {BUILD_JOBS_ENV} export in wrapped command:\n{cmd}"
    return int(match.group(1))


def test_a_stated_width_reaches_the_step_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("dagrun.sizing._OPERATOR_BUILD_JOBS", 12)
    cg = _boxed(tmp_path, cpu_max="28400000 100000", memory_max=str(8 * GIB))
    assert _wrapped_jobs(cg.prepare_command("build.dbi_release", "cargo build")) == 12


def test_with_nothing_stated_the_step_is_still_refined_downward(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The leg that must not regress: an unpinned step under a 284-core scope and an 8 GiB cap
    # gets 8, not 284. If operator intent were sniffed from CARGO_BUILD_JOBS this would break
    # in-scope, where the runner has already written that variable.
    monkeypatch.setattr("dagrun.sizing._OPERATOR_BUILD_JOBS", None)
    monkeypatch.setenv(BUILD_JOBS_ENV, "284")
    monkeypatch.setenv(OPERATOR_BUILD_JOBS_ENV, "")
    cg = _boxed(tmp_path, cpu_max="28400000 100000", memory_max=str(8 * GIB))
    assert _wrapped_jobs(cg.prepare_command("build.dbi_release", "cargo build")) == 8
