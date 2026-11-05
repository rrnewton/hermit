#!/usr/bin/env python3
"""Re-derive the interrupt numbers the core allocator's ranking rule rests on.

``cgroup.pick_least_busy_free_cores`` ranks cores by measured device-interrupt
rate. Its docstring quotes figures from one host; this tool regenerates them
anywhere, so the rule can be checked rather than believed. Run it before
trusting those figures on a new machine.

Three questions, kept separate because conflating them is how a placement
change gets credited with an improvement that is really sampling noise:

``distribution``
    Is there a signal at all? Reports device rows and architectural/IPI rows
    separately. The allocator counts only device rows, and this is why: on the
    reference host the architectural rows spanned 49-4846/s across CPUs (a 3.6x
    spread, zero CPUs above 4x median) while device rows spanned 0.0-1100.6/s
    with 45.6% of CPUs at exactly zero.

``placement``
    Does IRQ awareness change the selection, and in the right direction?
    Compares the shipped ranking against the previous idle-only ranking on
    IDENTICAL snapshots, so burstiness cannot contaminate the comparison.

``drift``
    How much does an independent window disagree? This bounds what the signal
    can promise. On the reference host the IRQ-aware selection still admitted
    0.30 hot cores per k=16 draw on average, versus 1.30 for idle-only ranking
    and 1.74 for a uniform draw -- better, but not a guarantee, because device
    interrupts are bursty.

Usage::

    python3 scripts/irq_survey.py distribution [--window 5]
    python3 scripts/irq_survey.py placement [--trials 10] [--window 0.3]
    python3 scripts/irq_survey.py drift [--trials 10] [--hot 10]
"""

from __future__ import annotations

import argparse
import os
import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "py"))

from dagrun.cgroup import parse_device_irq_counts  # noqa: E402

PROG = "irq-survey"


def _read_interrupts() -> str:
    return Path("/proc/interrupts").read_text()


def _arch_counts(text: str) -> dict[int, int]:
    """Architectural / IPI rows only -- the complement of the device rows.

    Present so the "device rows only" decision stays checkable instead of being
    a claim in a comment.
    """
    lines = text.splitlines()
    if not lines:
        return {}
    header = lines[0].split()
    ncpu = len(header)
    totals = [0] * ncpu
    for line in lines[1:]:
        colon = line.find(":")
        if colon <= 0:
            continue
        name = line[:colon].strip()
        if not name or name.isdigit():
            continue
        fields = line[colon + 1 :].split(maxsplit=ncpu)
        if len(fields) < ncpu:
            continue
        for index in range(ncpu):
            if fields[index].isdigit():
                totals[index] += int(fields[index])
    out: dict[int, int] = {}
    for index, name in enumerate(header):
        lowered = name.lower()
        if lowered.startswith("cpu") and lowered[3:].isdigit():
            out[int(lowered[3:])] = totals[index]
    return out


def _sample(window: float) -> tuple[dict[int, float], dict[int, float], dict[int, float]]:
    """(device rate, arch rate, idle fraction) per CPU over one shared window."""

    def stat_snap() -> dict[int, tuple[int, int]]:
        out: dict[int, tuple[int, int]] = {}
        with open("/proc/stat") as fh:
            for line in fh:
                if line.startswith("cpu") and len(line) > 3 and line[3].isdigit():
                    p = line.split()
                    out[int(p[0][3:])] = (int(p[4]) + int(p[5]), sum(int(x) for x in p[1:]))
        return out

    text_a = _read_interrupts()
    dev_a, arch_a, stat_a = parse_device_irq_counts(text_a), _arch_counts(text_a), stat_snap()
    started = time.monotonic()
    time.sleep(window)
    text_b = _read_interrupts()
    dev_b, arch_b, stat_b = parse_device_irq_counts(text_b), _arch_counts(text_b), stat_snap()
    elapsed = max(time.monotonic() - started, 1e-9)

    dev = {c: max(0, dev_b.get(c, v) - v) / elapsed for c, v in dev_a.items()}
    arch = {c: max(0, arch_b.get(c, v) - v) / elapsed for c, v in arch_a.items()}
    idle: dict[int, float] = {}
    for cpu, (bi, bt) in stat_b.items():
        if cpu in stat_a:
            delta = bt - stat_a[cpu][1]
            idle[cpu] = (bi - stat_a[cpu][0]) / delta if delta else 1.0
    return dev, arch, idle


def _describe(label: str, values: list[float]) -> None:
    ordered = sorted(values)
    n = len(ordered)
    if n == 0:
        print(f"{label}: no data")
        return

    def pct(fraction: float) -> float:
        return ordered[min(n - 1, int(fraction * n))]

    median = statistics.median(ordered)
    above = sum(1 for v in ordered if median > 0 and v > 4 * median)
    print(
        f"{label}: n={n} min={ordered[0]:.1f} median={median:.1f} p90={pct(0.90):.1f} "
        f"p99={pct(0.99):.1f} max={ordered[-1]:.1f} zero={sum(1 for v in ordered if v == 0)} "
        f">4x-median={above}"
    )


def _rank_aware(cores: list[int], dev: dict[int, float], idle: dict[int, float]) -> list[int]:
    return sorted(cores, key=lambda c: (dev.get(c, 0.0), -idle.get(c, 1.0), c))


def _rank_idle_only(cores: list[int], idle: dict[int, float]) -> list[int]:
    return sorted(cores, key=lambda c: (-idle.get(c, 1.0), c))


def cmd_distribution(args: argparse.Namespace) -> int:
    dev, arch, _idle = _sample(float(args.window))
    print(f"{PROG}: host={os.uname().nodename} cpus={len(dev)} window={args.window}s")
    _describe("device IRQ /s     ", list(dev.values()))
    _describe("architectural /s  ", list(arch.values()))
    hottest = sorted(dev.items(), key=lambda kv: -kv[1])[:8]
    print("hottest: " + ", ".join(f"cpu{c}={v:.1f}/s" for c, v in hottest))
    print(
        "device rows carry the signal; architectural rows track how busy the CPU already is, "
        "which /proc/stat sampling already measures."
    )
    return 0


def cmd_placement(args: argparse.Namespace) -> int:
    hot_at = float(args.hot)
    ks = [1, 4, 16, 32]
    changed = {k: 0 for k in ks}
    hot_aware = {k: 0 for k in ks}
    hot_idle = {k: 0 for k in ks}
    for _ in range(int(args.trials)):
        dev, _arch, idle = _sample(float(args.window))
        cores = sorted(dev)
        hot = {c for c, v in dev.items() if v > hot_at}
        for k in ks:
            aware = set(_rank_aware(cores, dev, idle)[:k])
            blind = set(_rank_idle_only(cores, idle)[:k])
            changed[k] += aware != blind
            hot_aware[k] += len(aware & hot)
            hot_idle[k] += len(blind & hot)
    print(f"{PROG}: same-window comparison, trials={args.trials} hot>{hot_at}/s")
    for k in ks:
        print(
            f"  k={k:<3} selection differs {changed[k]}/{args.trials} | "
            f"hot selected: IRQ-aware={hot_aware[k]} idle-only={hot_idle[k]}"
        )
    print(
        "A zero difference at every k would mean the change is invisible; a nonzero "
        "'IRQ-aware' column would mean it is not working."
    )
    return 0


def cmd_drift(args: argparse.Namespace) -> int:
    hot_at = float(args.hot)
    k = int(args.k)
    aware_leak: list[int] = []
    idle_leak: list[int] = []
    hot_sizes: list[int] = []
    total = 0
    for _ in range(int(args.trials)):
        dev1, _a1, idle1 = _sample(float(args.window))
        dev2, _a2, _i2 = _sample(float(args.window))
        cores = sorted(dev1)
        total = len(cores)
        hot2 = {c for c, v in dev2.items() if v > hot_at}
        hot_sizes.append(len(hot2))
        aware_leak.append(len(set(_rank_aware(cores, dev1, idle1)[:k]) & hot2))
        idle_leak.append(len(set(_rank_idle_only(cores, idle1)[:k]) & hot2))
    expected = k * statistics.fmean(hot_sizes) / total if total else 0.0
    print(f"{PROG}: cross-window drift, k={k} trials={args.trials} hot>{hot_at}/s")
    print(f"  IRQ-aware leaked hot cores: mean={statistics.fmean(aware_leak):.2f} max={max(aware_leak)}")
    print(f"  idle-only leaked hot cores: mean={statistics.fmean(idle_leak):.2f} max={max(idle_leak)}")
    print(f"  uniform-draw expectation:   {expected:.2f}")
    print(
        "A nonzero IRQ-aware mean is burstiness, not a defect: it bounds what a single "
        "sample can promise about the next interval."
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog=PROG, description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    dist = sub.add_parser("distribution", help="is there an interrupt signal on this host?", description="is there an interrupt signal on this host?")
    dist.add_argument("--window", type=float, default=5.0)
    dist.set_defaults(handler=cmd_distribution)

    place = sub.add_parser("placement", help="does IRQ awareness change the selection?", description="does IRQ awareness change the selection?")
    place.add_argument("--trials", type=int, default=10)
    place.add_argument("--window", type=float, default=0.3)
    place.add_argument("--hot", type=float, default=10.0)
    place.set_defaults(handler=cmd_placement)

    drift = sub.add_parser("drift", help="how much does an independent window disagree?", description="how much does an independent window disagree?")
    drift.add_argument("--trials", type=int, default=10)
    drift.add_argument("--window", type=float, default=0.3)
    drift.add_argument("--hot", type=float, default=10.0)
    drift.add_argument("--k", type=int, default=16)
    drift.set_defaults(handler=cmd_drift)

    args = parser.parse_args(argv)
    handler = getattr(args, "handler")
    if not callable(handler):  # pragma: no cover - argparse guarantees this
        parser.error("no handler bound")
    result = handler(args)
    return int(result) if isinstance(result, int) else 0


if __name__ == "__main__":
    raise SystemExit(main())
