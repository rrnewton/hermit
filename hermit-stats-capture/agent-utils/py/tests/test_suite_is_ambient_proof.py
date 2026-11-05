"""A suite whose verdict depends on the developer's shell is not a suite.

``select_build_jobs`` consults an operator intent captured from ``$CARGO_BUILD_JOBS`` at import.
That made two pre-existing suites environment-dependent the moment it landed:
``CARGO_BUILD_JOBS=200 python3 -m pytest tests/test_build_job_cap.py tests/test_sizing.py``
reported ``2 failed`` — ``test_unpinned_step_is_bounded_not_284`` and
``test_legitimate_configs_pass_unharmed``, both with a wrapped command carrying
``export CARGO_BUILD_JOBS=200`` where the containment default should have refined the step's width
down to 4. Nothing scrubbed the variable, so ``make validate`` was green or red according to who
ran it.

Asserting "the fixture cleared it" would be worthless here: on a host where the variable is
already unset, such an assertion passes with or without the fixture. So this test creates the
condition itself, in a child interpreter that really has the variable exported, and requires the
affected suites to pass under it.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

#: The suites that read the captured width, directly or through ``prepare_command``.
_AFFECTED = (
    "tests/test_build_job_cap.py",
    "tests/test_sizing.py",
    "tests/test_operator_build_width.py",
)


def test_the_build_width_suites_pass_with_an_ambient_cargo_build_jobs() -> None:
    py_root = Path(__file__).resolve().parents[1]
    env = dict(os.environ)
    # Exactly the developer's shell that turned the suite red: a stated width, and no forwarded
    # answer, which is what makes the outermost process treat it as operator intent.
    env["CARGO_BUILD_JOBS"] = "200"
    env.pop("DAGRUN_OPERATOR_BUILD_JOBS", None)
    env["PYTHONPATH"] = str(py_root)
    out = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", *_AFFECTED],
        cwd=py_root,
        env=env,
        capture_output=True,
        text=True,
    )
    assert out.returncode == 0, (
        "an exported CARGO_BUILD_JOBS must not change what these suites conclude:\n"
        f"{out.stdout}\n{out.stderr}"
    )
