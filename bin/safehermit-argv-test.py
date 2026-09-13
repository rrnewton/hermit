#!/usr/bin/env python3
"""Check argument and stdio preservation through a real safehermit user unit.

Requires working systemd user units. An optional wrapper path permits testing
an earlier implementation with the same assertions.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    wrapper = (
        Path(sys.argv[1]).resolve()
        if len(sys.argv) == 2
        else Path(__file__).resolve().with_name("safehermit")
    )
    arguments = [
        "",
        "two words",
        "${SAFEHERMIT_ARG_SENTINEL-UNSET}",
        "$SAFEHERMIT_ARG_SENTINEL",
        "$$",
        "$(printf unintended)",
        "literal%u",
        'quote"slash\\',
        "line1\nline2",
    ]
    probe = (
        "import json, os, sys; "
        "json.dump([sys.argv[1:], sys.stdin.read(), "
        "os.environ['SAFEHERMIT_ARG_SENTINEL']], sys.stdout); "
        "sys.stderr.write('probe-stderr\\n'); sys.exit(37)"
    )
    with tempfile.TemporaryDirectory(prefix="safehermit-argv-test-") as temporary:
        report = Path(temporary) / "report"
        environment = os.environ.copy()
        environment["SAFEHERMIT_ARG_SENTINEL"] = "must-remain-an-environment-value"
        environment["SAFEHERMIT_LOG_ROOT"] = str(Path(temporary) / "logs")
        result = subprocess.run(
            [
                str(wrapper),
                "--sh-deadline", "10s",
                "--sh-report", str(report),
                sys.executable, "-c", probe, *arguments,
            ],
            input="stdin survives\n",
            capture_output=True,
            text=True,
            env=environment,
            timeout=30,
        )
        bounds = report.read_text()
        assert "safehermit: bound.cgroup=APPLIED:" in bounds, (
            "this test requires a working systemd user unit"
        )
        assert result.returncode == 37, result
        assert result.stderr == "probe-stderr\n", result.stderr
        assert json.loads(result.stdout) == [
            arguments,
            "stdin survives\n",
            environment["SAFEHERMIT_ARG_SENTINEL"],
        ], result.stdout
        assert "safehermit: unit_result=exit-code\n" in bounds, bounds
        assert "safehermit: truncated=false\n" in bounds, bounds
    print("PASS: literal arguments, environment, stdin, stderr and exit 37")


if __name__ == "__main__":
    main()
