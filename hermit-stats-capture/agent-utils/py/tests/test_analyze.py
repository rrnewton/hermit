"""Compatibility coverage for the profile-analysis library helper."""

from __future__ import annotations

from pathlib import Path

import dagrun
from dagrun import analyze, perflog


def test_summarize_remains_library_only_and_filters_ambient_rows(tmp_path: Path) -> None:
    """Preserve the public helper without recreating an undeclared command surface."""
    profile = tmp_path / "profiles.csv"
    profile.write_text(
        "step,inner_jobs,ambient_bucket,elapsed_s,effective_cores,peak_bytes\n"
        "build.unit,2,quiet,4.0,1.5,100\n"
        "build.unit,2,quiet,2.0,2.0,120\n"
        "build.unit,2,busy,20.0,1.0,500\n",
        encoding="utf-8",
    )

    assert dagrun.summarize is analyze.summarize
    assert not hasattr(analyze, "main")
    assert analyze.summarize([profile]) == [
        {
            "step": "build.unit",
            "inner_jobs": "2",
            "samples": 2,
            "duration_median_s": 3.0,
            "effective_cores_median": 1.75,
            "memory_peak_max_bytes": 120,
        }
    ]


def test_summarize_reads_the_column_the_store_actually_writes() -> None:
    """Bind the duration column to the emitted schema so the two cannot drift apart.

    ``analyze`` keyed on a duration column the profile store has never written, so every group was
    dropped as unparseable and the helper returned nothing at all against a real store. Asserting
    the constant against :data:`dagrun.perflog.STEP_PROFILE_COLUMNS` makes that
    divergence a test failure rather than a silent empty summary.
    """
    assert analyze.COL_DURATION_S in perflog.STEP_PROFILE_COLUMNS
    for column in (
        analyze.COL_STEP,
        analyze.COL_INNER_JOBS,
        analyze.COL_AMBIENT,
        analyze.COL_EFFECTIVE_CORES,
        analyze.COL_MEMORY_PEAK_BYTES,
    ):
        assert column in perflog.STEP_PROFILE_COLUMNS


def test_summarize_returns_rows_for_a_store_shaped_csv(tmp_path: Path) -> None:
    """Summarize a CSV carrying the real emitted header, not a hand-written subset."""
    profile = tmp_path / "store.csv"
    header = ",".join(perflog.STEP_PROFILE_COLUMNS)
    index = {name: position for position, name in enumerate(perflog.STEP_PROFILE_COLUMNS)}
    rows = []
    for elapsed in ("4.0", "2.0"):
        cells = [""] * len(perflog.STEP_PROFILE_COLUMNS)
        cells[index["step"]] = "build.unit"
        cells[index["inner_jobs"]] = "2"
        cells[index["elapsed_s"]] = elapsed
        cells[index["effective_cores"]] = "1.5"
        cells[index["peak_bytes"]] = "120"
        cells[index["ambient_bucket"]] = "quiet"
        rows.append(",".join(cells))
    profile.write_text(header + "\n" + "\n".join(rows) + "\n", encoding="utf-8")

    summary = analyze.summarize([profile])
    assert len(summary) == 1
    assert summary[0]["step"] == "build.unit"
    assert summary[0]["samples"] == 2
    assert summary[0]["duration_median_s"] == 3.0
    assert summary[0]["memory_peak_max_bytes"] == 120
