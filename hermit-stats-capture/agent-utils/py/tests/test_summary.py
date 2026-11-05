"""Tests for the constant-sized, mergeable profile SUMMARY (dagrun.summary).

Covers the self-review properties that make the summary the "correctness core" of the sync feature:
bounded size, deterministic + commutative + associative merge, byte-identical serialization
round-trips, and recomputed estimates that match the raw-sample estimates (exactly under the
reservoir cap, closely once the reservoir subsamples a skewed set)."""

from __future__ import annotations

import pytest

from dagrun import summary as S
from dagrun.estimates import (
    bucketize_rows,
    step_samples_from_buckets,
)

MID = "m"
CC = "affinity8_cpu-max-max"


def _row(
    step: str,
    inner: int,
    elapsed: float,
    peak: int,
    *,
    pct_other: float = 0.0,
    workload_digest: str = "",
) -> dict[str, str]:
    row = {
        "step": step,
        "inner_jobs": str(inner),
        "elapsed_s": f"{elapsed:.3f}",
        "peak_bytes": str(peak),
        "pct_other": f"{pct_other:.3f}",
    }
    if workload_digest:
        row["workload_digest"] = workload_digest
    return row


def test_serialization_roundtrip_is_stable() -> None:
    rows = [_row("g.a", 1, 8.0, 1000), _row("g.a", 1, 20.0, 1000, pct_other=60.0)]
    summ = S.summary_from_rows(rows, MID, CC, 8)
    js = S.to_json(summ)
    reparsed = S.from_json(js)
    assert S.to_json(reparsed) == js  # round-trip is byte-stable
    # And the discount is transparent in the JSON (raw elapsed + contention kept separately).
    assert '"elapsed_s": "20.000"' in js
    assert '"contention": "0.600"' in js


def test_estimates_from_summary_match_raw_union_exactly_under_cap() -> None:
    rows = [
        _row("g.a", 1, 8.0, 6_000_000_000),
        _row("g.a", 1, 20.0, 6_000_000_000, pct_other=60.0),
        _row("g.b", 2, 5.0, 1_000_000_000),
        _row("g.b", 2, 5.2, 1_000_000_000),
        _row("g.b", 2, 5.1, 1_000_000_000),
    ]
    raw = step_samples_from_buckets(bucketize_rows(rows, 8))
    summ = S.summary_from_rows(rows, MID, CC, 8, reservoir_cap=64)
    got = S.step_samples_from_summary(summ)
    for tag in ("g.a", "g.b"):
        assert got[tag].est_duration_s == raw[tag].est_duration_s
        assert got[tag].rss_estimate_bytes == raw[tag].rss_estimate_bytes
        assert got[tag].samples == raw[tag].samples


def test_merge_is_commutative_and_associative() -> None:
    a = S.summary_from_rows([_row("g.a", 1, 1.0, 100), _row("g.a", 1, 2.0, 200)], MID, CC, 8)
    b = S.summary_from_rows([_row("g.a", 1, 3.0, 300), _row("g.b", 1, 4.0, 400)], MID, CC, 8)
    c = S.summary_from_rows([_row("g.b", 1, 5.0, 500), _row("g.c", 1, 6.0, 600)], MID, CC, 8)
    left = S.to_json(S.merge(S.merge(a, b), c))
    right = S.to_json(S.merge(a, S.merge(b, c)))
    perm = S.to_json(S.merge(S.merge(c, a), b))
    assert left == right == perm


def test_merge_with_empty_is_identity() -> None:
    a = S.summary_from_rows([_row("g.a", 1, 1.0, 100)], MID, CC, 8)
    e = S.empty(MID, CC)
    assert S.to_json(S.merge(a, e)) == S.to_json(a)
    assert S.to_json(S.merge(e, a)) == S.to_json(a)


def test_merge_rejects_identity_mismatch() -> None:
    a = S.summary_from_rows([_row("g.a", 1, 1.0, 100)], MID, CC, 8)
    b = S.summary_from_rows([_row("g.a", 1, 1.0, 100)], "other", CC, 8)
    with pytest.raises(S.SummaryError):
        S.merge(a, b)


def test_merge_is_bounded_across_many_runs() -> None:
    """Merging N runs never grows a bucket past the reservoir cap, nor the bucket count past the
    structural set — the constant-size guarantee, independent of N."""
    k = 8
    acc = S.empty(MID, CC)
    for run in range(200):
        rows = [_row("g.a", 1, float(run) + 0.5, 1000 + run), _row("g.b", 2, 3.0, 2000)]
        delta = S.summary_from_rows(rows, MID, CC, 8, reservoir_cap=k)
        acc = S.merge(acc, delta, reservoir_cap=k)
    buckets, total, largest = S.summary_stats(acc)
    assert buckets == 2  # only (g.a,1) and (g.b,2) ever appear, no matter how many runs
    assert largest <= k
    assert total <= buckets * k


def test_reservoirs_are_partitioned_by_workload_before_subsampling() -> None:
    rows = [
        _row("g.a", 1, float(index + 100), 1000, workload_digest="old")
        for index in range(100)
    ]
    rows += [
        _row("g.a", 1, 10.0, 2000, workload_digest="new"),
        _row("g.a", 2, 5.0, 3000, workload_digest="new"),
    ]
    summary = S.summary_from_rows(rows, MID, CC, 8, reservoir_cap=2)

    # The old cohort cannot consume the current cohort's two-sample reservoir. Selection happens
    # after bounded storage without losing either current width.
    assert S.summary_stats(summary) == (3, 4, 2)
    model = S.step_speedups_from_summary(
        summary, workload_digests={"g.a": "new"}
    )["g.a"]
    assert [level.wall_s for level in model.levels] == [10.0, 5.0]


def test_version_one_summary_is_read_as_blank_workload_and_rewritten_as_v2() -> None:
    old = (
        '{"version": 1, "machine_id": "m", "container_class": "c", '
        '"buckets": [{"step": "g.a", "inner_jobs": 1, "samples": '
        '[{"observation_id": "one", "workload_digest": "ignored-v1-extension", '
        '"elapsed_s": "2.000", '
        '"contention": "0.000", "cpu_s": null, "effective_cores": null, '
        '"throttled_s": null, "peak_bytes": 1000}]}]}'
    )
    parsed = S.from_json(old)
    rendered = S.to_json(parsed)
    assert parsed.version == S.SUMMARY_VERSION == 2
    assert '"version": 2' in rendered
    assert '"workload_digest": ""' in rendered
    assert S.step_samples_from_summary(parsed)["g.a"].est_duration_s == 2.0


def test_reservoir_does_not_badly_bias_median_on_skewed_set() -> None:
    """A skewed set (mostly 5.0 with a minority of 50.0 outliers) far larger than the reservoir:
    the subsampled summary's robust median must stay near the true robust median (self-review c)."""
    rows = [_row("g.a", 1, 5.0, 1000) for _ in range(90)]
    rows += [_row("g.a", 1, 50.0, 1000) for _ in range(30)]
    raw = step_samples_from_buckets(bucketize_rows(rows, 8))["g.a"]
    summ = S.summary_from_rows(rows, MID, CC, 8, reservoir_cap=64)
    got = S.step_samples_from_summary(summ)["g.a"]
    assert got.samples == 64  # bounded
    assert raw.est_duration_s is not None and got.est_duration_s is not None
    # The subsample keeps the majority value's dominance, so the MAD-trimmed median stays ~5.0.
    assert 4.5 <= got.est_duration_s <= 5.5
    assert abs(got.est_duration_s - raw.est_duration_s) <= 0.5


def test_reservoir_ranks_observations_not_distinct_values() -> None:
    """A minority value must not fill the reservoir merely because its content hash sorts first."""
    rows = [_row("g.a", 1, 5.0, 1000) for _ in range(936)]
    rows += [_row("g.a", 1, 50.0, 1000) for _ in range(64)]
    raw = step_samples_from_buckets(bucketize_rows(rows, 8))["g.a"]
    summ = S.summary_from_rows(rows, MID, CC, 8, reservoir_cap=64)
    got = S.step_samples_from_summary(summ)["g.a"]
    assert raw.est_duration_s == 5.0
    assert got.est_duration_s == 5.0


def test_from_json_rejects_unknown_version() -> None:
    bad = '{"version": 999, "machine_id": "m", "container_class": "c", "buckets": []}'
    with pytest.raises(S.SummaryError):
        S.from_json(bad)


def test_from_json_rejects_non_finite_and_malformed() -> None:
    with pytest.raises(S.SummaryError):
        S.from_json('{"version": 1, "machine_id": "m", "container_class": "c", '
                    '"buckets": [{"step": "s", "inner_jobs": 1, '
                    '"samples": [{"contention": "0.0", "elapsed_s": NaN}]}]}')
    with pytest.raises(S.SummaryError):
        S.from_json("not json at all")
    with pytest.raises(S.SummaryError):
        S.from_json('{"version": 1, "machine_id": "m"}')  # missing container_class
