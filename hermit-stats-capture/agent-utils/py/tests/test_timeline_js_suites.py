"""Run the archive's JavaScript suites from the Python gate that actually runs.

`tests/js/` holds the plain-Node suites over `static/app.js` and `static/timeline-core.js`.
This file discovers all of them and is owned by the wrkviz component in the root validation graph;
the separate browser fragment owns Playwright and benchmark suites.

The suites are discovered by glob rather than listed, for the reason `scripts/validate.py` gives
about naming one page suite instead of the pattern: a list is how the next suite comes to exist
without ever running. Each file is a separate parametrised case so a failure names the suite
rather than "the JavaScript".

Node is not a dependency of this package -- the archive it builds is served as static files and
read by a browser -- so a host without it skips rather than fails. That is a real weakness and
it is stated rather than hidden: on a host with no Node these tests prove nothing, and CI is
where they are load-bearing.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


_SUITES = sorted((Path(__file__).parent / "js").glob("test_*.js"))


def test_the_glob_found_the_suites() -> None:
    """A glob that matches nothing is a green test run that checked nothing."""

    assert len(_SUITES) >= 6, [path.name for path in _SUITES]


@pytest.mark.parametrize("suite", _SUITES, ids=[path.stem for path in _SUITES])
def test_javascript_suite_passes(suite: Path) -> None:
    node = shutil.which("node")
    if node is None:
        pytest.skip("node is not installed; the browser suites cannot run here")
    result = subprocess.run(
        [node, str(suite)],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
