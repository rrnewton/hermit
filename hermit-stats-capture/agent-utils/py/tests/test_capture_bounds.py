"""The IN-MEMORY step capture and the live-stream buffer are bounded, not just the disk.

#81 runner-capture-memory-bound. The disk ceiling stops a runaway step from filling the device;
it does nothing about the copy the runner holds in its own address space, and that copy is the
one that kills the runner first — taking the run's verdict, its profile rows and its evidence
with it.
"""

from __future__ import annotations

import tracemalloc

import pytest

from dagrun.attribution import (
    CAPTURE_MAX_BYTES_ENV,
    DEFAULT_CAPTURE_MAX_BYTES,
    capture_max_bytes,
)
from dagrun.cgroup import NoopCgroups
from dagrun.model import DagConfig, Step
from dagrun.scheduler import Runner, _BoundedCapture


def test_the_ring_never_allocates_more_than_its_ceiling_however_much_is_fed() -> None:
    """A step's output size must not appear anywhere in the runner's memory cost.

    The measurement is the point: with the previous ``list[bytes]`` capture, feeding 16 MiB cost
    16 MiB of live allocation, so this peak scaled with the step rather than with the ceiling.
    """
    limit = 64 * 1024
    chunk = b"x" * 8192
    capture = _BoundedCapture(limit)

    tracemalloc.start()
    try:
        tracemalloc.reset_peak()
        for _ in range(2048):  # 16 MiB, 256 times the ceiling
            capture.feed(chunk)
        _current, peak = tracemalloc.get_traced_memory()
    finally:
        tracemalloc.stop()

    assert capture.total == 2048 * 8192
    assert capture.kept == limit
    assert peak < 4 * limit, (
        f"feeding 16 MiB through a {limit}-byte ring peaked at {peak} bytes; the ceiling is "
        "supposed to be the whole memory cost, independent of how much the step emitted"
    )


def test_the_ring_keeps_the_tail_and_reports_how_much_it_dropped() -> None:
    capture = _BoundedCapture(10)
    capture.feed(b"0123456789abcdef")
    assert capture.tail() == b"6789abcdef", "the TAIL is what a failure dump needs"
    assert capture.kept == 10
    assert capture.total == 16
    assert capture.dropped


def test_the_ring_is_byte_exact_across_a_wrap() -> None:
    """Wrap-around is where a ring silently corrupts; pin the exact bytes, not just the length."""
    capture = _BoundedCapture(8)
    for chunk in (b"abc", b"de", b"fghij"):  # 10 bytes total through an 8-byte ring
        capture.feed(chunk)
    assert capture.tail() == b"cdefghij"
    assert capture.total == 10

    # A single read LARGER than the whole ring must also leave exactly its own tail.
    big = _BoundedCapture(4)
    big.feed(b"0123456789")
    assert big.tail() == b"6789"
    assert big.total == 10


def test_an_unwrapped_ring_is_exactly_what_was_fed() -> None:
    """Below the ceiling nothing is a tail: the capture must be lossless and say so."""
    capture = _BoundedCapture(1024)
    capture.feed(b"first\n")
    capture.feed(b"second\n")
    assert capture.tail() == b"first\nsecond\n"
    assert not capture.dropped
    assert capture.kept == 13
    assert list(capture.iter_lines()) == ["first", "second"]
    assert capture.last_line() == "second"


def test_the_last_line_survives_a_step_that_overran_the_ceiling() -> None:
    """Keeping the TAIL is what makes the one-line summary work on a runaway step."""
    capture = _BoundedCapture(32)
    capture.feed(b"early noise that will be dropped\n" * 4)
    capture.feed(b"the real verdict\n")
    assert capture.last_line() == "the real verdict"


def test_last_line_skips_trailing_blank_lines_and_tolerates_bad_utf8() -> None:
    capture = _BoundedCapture(4096)
    capture.feed(b"real line\n\n   \n\n")
    assert capture.last_line() == "real line"

    invalid = _BoundedCapture(4096)
    invalid.feed(b"before\n\xff\xfe\n")
    assert invalid.last_line() == "��"


def test_the_default_capture_ceiling_is_four_mebibytes_stated_as_a_number() -> None:
    """The DEFAULT is pinned to a LITERAL, because every other assertion on it is circular.

    ``capture_max_bytes() == DEFAULT_CAPTURE_MAX_BYTES`` is true for every value the constant
    could ever hold, so shrinking the ceiling by three orders of magnitude — the difference
    between "the tail a human reads" and "a few lines" — passed the whole suite. The sibling
    engine pins the same literal (``the_default_capture_ceiling_is_four_mebibytes_literally``),
    so the two defaults cannot drift apart without one of the two failing by name; nothing else
    compares them, because the differential sets the environment override and never observes the
    default at all.
    """
    assert DEFAULT_CAPTURE_MAX_BYTES == 4194304
    assert capture_max_bytes() == 4194304


def test_the_console_line_bound_is_one_mebibyte_stated_as_a_number() -> None:
    """Same reasoning, for the ``-vv`` live-stream bound.

    Its own end-to-end test monkeypatches the constant to 3 KiB to stay fast, which means that
    test says nothing about the shipped value. Both engines carry a "MUST match" note beside this
    number; this is what turns that note into something that can fail.
    """
    from dagrun import scheduler

    assert scheduler._STREAM_LINE_MAX_BYTES == 1048576


def test_an_unlimited_capture_is_an_explicit_opt_out_not_a_fallback(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(CAPTURE_MAX_BYTES_ENV, "0")
    assert capture_max_bytes() is None
    capture = _BoundedCapture(None)
    capture.feed(b"a" * 5000)
    assert capture.kept == 5000
    assert not capture.dropped


def test_an_unparseable_ceiling_is_reported_and_falls_back_to_the_default(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A misread ceiling that quietly became "unlimited" is the failure this ceiling prevents."""
    monkeypatch.setenv(CAPTURE_MAX_BYTES_ENV, "lots")
    assert capture_max_bytes() == DEFAULT_CAPTURE_MAX_BYTES
    err = capsys.readouterr().err
    assert CAPTURE_MAX_BYTES_ENV in err
    assert "WARNING" in err

    monkeypatch.setenv(CAPTURE_MAX_BYTES_ENV, "-1")
    assert capture_max_bytes() == DEFAULT_CAPTURE_MAX_BYTES


def test_a_failing_step_dumps_the_tail_and_says_how_much_it_dropped(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """End to end: the dump must not present a partial tail as if it were the whole output."""
    monkeypatch.setenv(CAPTURE_MAX_BYTES_ENV, "300")
    noisy = Step(
        "g",
        "noisy",
        "",
        "for i in $(seq 1 200); do echo \"line$i\"; done; exit 1",
    )
    runner = Runner(
        DagConfig(steps=(noisy,)),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=1,
    )
    assert not runner.run()

    out = capsys.readouterr().out
    assert "EARLIER OUTPUT DROPPED" in out
    assert "DAGRUN_CAPTURE_MAX_BYTES" in out
    # The numbers must be real, not decorative: 200 lines of "lineN" is well over 300 bytes.
    assert "the last 300 were kept in memory" in out
    assert "line200" in out, "the TAIL is what is kept"
    assert "line1\n" not in out, "the head is what is dropped"


def test_a_short_failing_step_dumps_everything_without_a_dropped_notice(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The notice must be evidence of an actual drop, not boilerplate on every failure."""
    monkeypatch.setenv(CAPTURE_MAX_BYTES_ENV, "65536")
    quiet = Step("g", "quiet", "", "echo only-line; exit 1")
    runner = Runner(
        DagConfig(steps=(quiet,)),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=1,
    )
    assert not runner.run()

    out = capsys.readouterr().out
    assert "only-line" in out
    assert "EARLIER OUTPUT DROPPED" not in out


def test_the_live_stream_buffer_does_not_grow_without_a_newline(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The ``-vv`` buffer is drained only when it CONTAINS a newline.

    Bounding the capture alone leaves this hole open on exactly the path that streams a runaway to
    a human. The forced flush is asserted through what reaches the console: the bytes appear
    BEFORE any newline does, which can only happen if something other than a newline drained the
    buffer.
    """
    from dagrun import scheduler

    # A 3 KiB bound keeps the test fast; the production constant is a display bound, not a policy,
    # so overriding it here changes only how quickly the same code path is reached.
    monkey = pytest.MonkeyPatch()
    monkey.setattr(scheduler, "_STREAM_LINE_MAX_BYTES", 3072)
    try:
        # 30 KiB with NO newline at all, then a sleep so the pump must flush before EOF.
        gusher = Step(
            "g",
            "gusher",
            "",
            "printf 'y%.0s' $(seq 1 30000); sleep 0.4; echo; exit 1",
        )
        runner = Runner(
            DagConfig(steps=(gusher,)),
            max_steps=1,
            max_cpus=1,
            cgroups=NoopCgroups(),
            verbosity=2,
        )
        runner.run()
    finally:
        monkey.undo()

    # Only the LIVE stream, not the end-of-step failure dump, which replays the same bytes from
    # the (separately bounded) capture and would otherwise mask what the live path did.
    lines = capsys.readouterr().out.splitlines()
    detail_at = next(
        (i for i, line in enumerate(lines) if line.endswith("----- detail -----")),
        len(lines),
    )
    streamed = [
        line for line in lines[:detail_at] if line.startswith("[g.gusher] y")
    ]
    assert streamed, "the newline-free output must reach the console at all"
    assert len(streamed) >= 9, (
        "30 KiB of newline-free output through a 3 KiB bound must be flushed in pieces; a single "
        f"piece means the buffer grew unbounded (saw {len(streamed)} pieces)"
    )
    # The buffer is trimmed once per READ, so the widest a piece can be is the bound plus the
    # 8 KiB read that carried the newline. What must never happen is a piece the size of the
    # step's whole output.
    widest = max(len(line) for line in streamed)
    assert widest <= len("[g.gusher] ") + 3072 + 8192, (
        f"widest streamed piece was {widest} bytes; the live buffer is not being trimmed"
    )


def test_a_forced_split_says_it_was_forced_and_that_nothing_was_lost(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The guard has to be legible at the one moment it acts.

    A forced flush that looks like an ordinary console line means the cap firing and the cap
    never firing produce the SAME output, so nobody can tell whether the guard has ever worked.
    The bound also cannot fire on healthy output -- the longest newline-free run measured over a
    real corpus was about 27 KiB against a 1 MiB bound -- so the notice is the only evidence a
    reader will ever get that it acted.

    Two things are asserted because either alone is misleading: that the notice appears at all,
    and that it says nothing was discarded. "SPLIT" on its own reads as data loss.
    """
    from dagrun import scheduler

    monkey = pytest.MonkeyPatch()
    monkey.setattr(scheduler, "_STREAM_LINE_MAX_BYTES", 3072)
    try:
        gusher = Step(
            "g",
            "gusher",
            "",
            "printf 'y%.0s' $(seq 1 30000); sleep 0.4; echo; exit 1",
        )
        runner = Runner(
            DagConfig(steps=(gusher,)),
            max_steps=1,
            max_cpus=1,
            cgroups=NoopCgroups(),
            verbosity=2,
        )
        runner.run()
    finally:
        monkey.undo()

    lines = capsys.readouterr().out.splitlines()
    detail_at = next(
        (i for i, line in enumerate(lines) if line.endswith("----- detail -----")),
        len(lines),
    )
    notices = [
        line
        for line in lines[:detail_at]
        if scheduler._STREAM_SPLIT_MARKER in line
    ]
    assert notices, (
        "the cap fired but said nothing: a forced flush is indistinguishable from an "
        "ordinary console line, so a reader cannot tell the guard acted"
    )
    joined = " ".join(notices)
    assert "NOTHING WAS DISCARDED" in joined, (
        "the notice must state that splitting a console line dropped no output; 'SPLIT' "
        "alone reads as data loss"
    )
    assert "3072" in joined, "the notice must name the threshold it enforced"


def test_ordinary_output_produces_no_split_notice(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The negative control, without which the assertion above proves nothing.

    If the notice were emitted unconditionally it would satisfy the test above while carrying no
    information. Newline-delimited output of the same total size must produce none.
    """
    from dagrun import scheduler

    monkey = pytest.MonkeyPatch()
    monkey.setattr(scheduler, "_STREAM_LINE_MAX_BYTES", 3072)
    try:
        tidy = Step("g", "tidy", "", "for i in $(seq 1 400); do printf 'y%.0s' $(seq 1 70); echo; done")
        runner = Runner(
            DagConfig(steps=(tidy,)),
            max_steps=1,
            max_cpus=1,
            cgroups=NoopCgroups(),
            verbosity=2,
        )
        runner.run()
    finally:
        monkey.undo()

    out = capsys.readouterr().out
    assert "[g.tidy] y" in out, "the control must actually stream output"
    assert scheduler._STREAM_SPLIT_MARKER not in out, (
        "newline-delimited output must not trip the split notice; a notice that always "
        "appears distinguishes nothing"
    )
