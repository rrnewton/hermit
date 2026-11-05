"""IRQ-aware core selection: it still allocates, AND it changes placement.

A change that satisfies only one of those is invisible, so every behaviour here
is asserted from both sides -- the allocator keeps returning the right cores in
the right shape, and the interrupt signal demonstrably moves the selection,
verified by flipping which cores are hot and requiring the answer to follow.

The interrupt source is synthetic ``/proc`` text rather than the live host,
because the live signal is bursty (measured: an extreme core appears in the
top-32 of only 2 of 8 independent 0.3s samples) and a test keyed on it would be
flaky. Burstiness is a property of the machine; the ranking rule is what these
tests pin down. ``scripts/irq_survey.py`` measures the live host.
"""

from __future__ import annotations

import os

import pytest

from dagrun import cgroup


def interrupts_text(rates: dict[int, int], *, ncpu: int, arch_per_cpu: int = 99_000) -> str:
    """A ``/proc/interrupts`` whose DEVICE rows carry ``rates``.

    The architectural rows carry a deliberately huge uniform count: if the
    parser ever summed them they would swamp the device signal and the ordering
    assertions below would fail. That is what makes the device-rows-only rule
    observable rather than merely documented. Mirrors the Rust fixture in
    ``rs/dagrun/src/cgroup.rs``.
    """
    joined = " ".join(f"CPU{index}" for index in range(ncpu))
    device = " ".join(str(rates.get(index, 0)) for index in range(ncpu))
    zeros = " ".join("0" for _ in range(ncpu))
    arch = " ".join(str(arch_per_cpu) for _ in range(ncpu))
    return (
        f"      {joined}\n"
        f"  17: {device}   PCI-MSI  nvme0q1\n"
        f" 130: {zeros}   IR-PCI-MSI  eth0-tx\n"
        f" LOC: {arch}   Local timer interrupts\n"
        f" RES: {arch}   Rescheduling interrupts\n"
        f" ERR: 0\n"
    )


def stat_pair(idle: dict[int, float], *, ncpu: int) -> tuple[str, str]:
    """Two ``/proc/stat`` snapshots whose DELTA is the requested idle fraction.

    Idle fraction is a rate, so a single snapshot cannot express it -- two
    identical snapshots produce a zero denominator and every core ties, which
    is exactly the bug this helper exists to avoid.
    """
    before = "\n".join(f"cpu{cpu} 0 0 0 0 0 0 0 0 0 0" for cpu in range(ncpu)) + "\n"
    lines = []
    for cpu in range(ncpu):
        total = 1000
        idle_jiffies = int(idle.get(cpu, 1.0) * total)
        busy = total - idle_jiffies
        lines.append(f"cpu{cpu} {busy} 0 0 {idle_jiffies} 0 0 0 0 0 0")
    return before, "\n".join(lines) + "\n"


def install_proc(
    monkeypatch: pytest.MonkeyPatch,
    *,
    interrupts: list[str | None],
    stat: list[str | None],
) -> None:
    """Serve fixture text through the module's own ``/proc`` seam."""
    irq_iter = iter(interrupts)
    stat_iter = iter(stat)

    def fake_read(path: str) -> str | None:
        if path == "/proc/interrupts":
            return next(irq_iter)
        if path == "/proc/stat":
            return next(stat_iter)
        return None

    monkeypatch.setattr(cgroup, "read_proc_text", fake_read)


# --------------------------------------------------------------------------- #
# Parsing: which rows count, and what "unavailable" means.                      #
# --------------------------------------------------------------------------- #


def test_device_rows_are_counted_and_architectural_rows_are_not() -> None:
    """The measured reason for the rule: arch rows do not discriminate."""
    counts = cgroup.parse_device_irq_counts(
        interrupts_text({0: 7, 1: 0, 2: 300}, ncpu=3, arch_per_cpu=99_000)
    )

    assert counts == {0: 7, 1: 0, 2: 300}


def test_short_rows_are_skipped_whole_so_counts_are_not_misattributed() -> None:
    """Matches the Rust parser: a prefix-counted short row would shift CPUs."""
    text = "      CPU0 CPU1 CPU2\n  17: 5 5\n  18: 1 2 3   PCI-MSI  dev\n"

    assert cgroup.parse_device_irq_counts(text) == {0: 1, 1: 2, 2: 3}


def test_empty_input_yields_an_absent_signal_not_zeroes() -> None:
    assert cgroup.parse_device_irq_counts("") == {}
    assert cgroup.parse_device_irq_counts("\n") == {}


def test_unreadable_interrupts_is_absent_not_zero(monkeypatch: pytest.MonkeyPatch) -> None:
    """"Could not look" must not render as "this host has no interrupts"."""
    monkeypatch.setattr(cgroup, "read_proc_text", lambda _path: None)

    assert cgroup.device_irq_counts() == {}
    assert cgroup.device_irq_rates(sample_s=0.0) == {}


def test_rates_are_derived_from_the_delta_not_the_absolute_count(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    install_proc(
        monkeypatch,
        interrupts=[
            interrupts_text({0: 1_000_000, 1: 5}, ncpu=2),
            interrupts_text({0: 1_000_000, 1: 105}, ncpu=2),
        ],
        stat=[],
    )

    rates = cgroup.device_irq_rates(sample_s=0.001)

    # cpu0 has a huge cumulative count but no delta: it is quiet NOW.
    assert rates[0] == 0.0
    assert rates[1] > 0.0


# --------------------------------------------------------------------------- #
# Placement: the signal must actually move the answer.                          #
# --------------------------------------------------------------------------- #


def pick(
    monkeypatch: pytest.MonkeyPatch,
    *,
    irq: dict[int, int] | None,
    idle: dict[int, float],
    k: int,
    ncpu: int = 8,
    max_irq_rate: float | None = None,
) -> list[int]:
    """Run the real ranking against a controlled interrupt and idle picture."""
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: set(range(ncpu)))
    before = None if irq is None else interrupts_text({}, ncpu=ncpu)
    after = None if irq is None else interrupts_text(irq, ncpu=ncpu)
    stat_before, stat_after = stat_pair(idle, ncpu=ncpu)
    # Order of reads inside pick_least_busy_free_cores: stat, interrupts, sleep,
    # stat, interrupts.
    install_proc(
        monkeypatch,
        interrupts=[before, after],
        stat=[stat_before, stat_after],
    )
    sample_s = 0.001 if max_irq_rate is not None else 0.0
    return cgroup.pick_least_busy_free_cores(
        k, sample_s=sample_s, max_irq_rate=max_irq_rate
    )


def test_interrupt_hot_cores_are_ranked_below_quiet_ones(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """cpu0 is the MOST IDLE core but also interrupt-hot; it must lose."""
    idle = {cpu: 0.5 for cpu in range(8)}
    idle[0] = 1.0

    picked = pick(monkeypatch, irq={0: 5000}, idle=idle, k=3)

    assert 0 not in picked, "the hottest core was selected despite quiet alternatives"
    assert len(picked) == 3


def test_flipping_which_core_is_hot_flips_the_selection(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The strongest form of "it changes placement": invert the signal and
    require the answer to invert with it. Idle fractions are identical in both
    runs, so nothing but the interrupt rate can explain the difference."""
    flat = {cpu: 0.5 for cpu in range(8)}

    hot_low = pick(monkeypatch, irq={0: 900, 1: 900, 2: 900}, idle=flat, k=3)
    hot_high = pick(monkeypatch, irq={3: 900, 4: 900, 5: 900}, idle=flat, k=3)

    assert hot_low == [3, 4, 5]
    assert hot_high == [0, 1, 2]


def test_idle_fraction_still_breaks_ties_among_equally_quiet_cores(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """IRQ awareness must not discard the original signal, only outrank it."""
    idle = {0: 0.1, 1: 0.2, 2: 0.9, 3: 0.8, 4: 0.3, 5: 0.4, 6: 0.5, 7: 0.6}

    picked = pick(monkeypatch, irq={}, idle=idle, k=2)

    assert picked == [2, 3], "most-idle cores should win when interrupt rates tie"


def test_ranking_degrades_to_idle_only_when_interrupts_are_unreadable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A sandbox without /proc/interrupts still gets a working allocator."""
    idle = {0: 0.1, 1: 0.2, 2: 0.9, 3: 0.8, 4: 0.3, 5: 0.4, 6: 0.5, 7: 0.6}

    picked = pick(monkeypatch, irq=None, idle=idle, k=2)

    assert picked == [2, 3]


def test_transient_first_stat_snapshot_failure_refuses_instead_of_crashing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: {0, 1})
    _before, after = stat_pair({0: 0.5, 1: 0.5}, ncpu=2)
    install_proc(monkeypatch, interrupts=[None, None], stat=[None, after])

    assert cgroup.pick_least_busy_free_cores(1, sample_s=0.0) == []


def test_cpu_counter_rollback_excludes_that_core(monkeypatch: pytest.MonkeyPatch) -> None:
    """A hotplug/reset interval must not look like a perfectly idle CPU."""
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: {0, 1})
    before = "cpu0 10 0 0 90 0 0 0 0 0 0\ncpu1 10 0 0 90 0 0 0 0 0 0\n"
    # CPU0's total and idle counters both moved backwards; CPU1 is a valid interval.
    after = "cpu0 5 0 0 80 0 0 0 0 0 0\ncpu1 20 0 0 180 0 0 0 0 0 0\n"
    install_proc(monkeypatch, interrupts=[None, None], stat=[before, after])

    assert cgroup.pick_least_busy_free_cores(2, sample_s=0.0) == [1]


# --------------------------------------------------------------------------- #
# The opt-in budget.                                                            #
# --------------------------------------------------------------------------- #


def test_max_irq_rate_drops_cores_above_the_stated_budget(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    picked = pick(
        monkeypatch,
        irq={1: 50_000, 3: 500_000},
        idle={cpu: 0.5 for cpu in range(8)},
        k=8,
        max_irq_rate=10.0,
    )

    assert 1 not in picked and 3 not in picked
    assert len(picked) == 6, "shortfall must be observable, not padded with hot cores"


def test_max_irq_rate_refuses_when_the_signal_is_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A budget that cannot be checked must not report success.

    Returning the full set here would claim the caller's interrupt budget was
    honoured when nothing ever verified it.
    """
    picked = pick(
        monkeypatch,
        irq=None,
        idle={cpu: 0.5 for cpu in range(8)},
        k=4,
        max_irq_rate=10.0,
    )

    assert picked == []


def test_max_irq_rate_refuses_zero_length_measurement(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: {0})
    assert cgroup.pick_least_busy_free_cores(
        1, sample_s=0.0, max_irq_rate=10.0
    ) == []


def test_max_irq_rate_never_treats_an_unmeasured_core_as_zero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: {0, 1})
    stat_before, stat_after = stat_pair({0: 0.5, 1: 0.9}, ncpu=2)
    # The IRQ interface reports only CPU0. CPU1 is not certified quiet merely because its column
    # is absent, even though its idle fraction would otherwise make it the preferred core.
    irq_before = interrupts_text({}, ncpu=1)
    irq_after = interrupts_text({}, ncpu=1)
    install_proc(
        monkeypatch,
        interrupts=[irq_before, irq_after],
        stat=[stat_before, stat_after],
    )

    assert cgroup.pick_least_busy_free_cores(
        2, sample_s=0.001, max_irq_rate=10.0
    ) == [0]


def test_no_budget_is_the_default_so_allocation_never_fails_on_interrupts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Every core is hot; with no stated budget the allocator still returns k.

    Measurement drove this: interrupt hotness is bursty, so a default threshold
    would refuse allocations nondeterministically.
    """
    picked = pick(
        monkeypatch,
        irq={cpu: 999_999 for cpu in range(8)},
        idle={cpu: 0.5 for cpu in range(8)},
        k=4,
    )

    assert len(picked) == 4
