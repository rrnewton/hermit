#!/usr/bin/env python3
"""Show reproducible Linux task evolution through zero-Heisenberg drgn reads."""

import os
from pathlib import Path
import sys

from drgn.helpers.linux.list import list_for_each_entry

DEMO_DIR = Path(__file__).resolve().parent
ROOT = DEMO_DIR.parent
sys.path.insert(0, str(DEMO_DIR / "lib"))

from drgn_hermit import GuestConfig, program_from_hermit  # noqa: E402


ADVANCE_MARKER = b"__HERMIT_DEMO07_ADVANCE_DONE__"
DEFAULT_ADVANCE_COMMAND = (
    "for n in 1 2; do sleep 1000 & done; "
    # Split the marker in the command text so terminal echo cannot satisfy the
    # wait; only the shell's post-timer output contains the complete marker.
    'usleep 1000; echo __HERMIT_DEMO07_ADVANCE_"DONE__"'
)


def _required_path(variable: str) -> Path:
    value = os.environ.get(variable)
    if not value:
        raise RuntimeError("{} is not set; run demos/07-drgn-kernel.sh".format(variable))
    return Path(value).resolve()


def _optional_path(variable: str):
    value = os.environ.get(variable)
    return Path(value).resolve() if value else None


def _config() -> GuestConfig:
    return GuestConfig(
        root=ROOT,
        hermit=_required_path("HERMIT_RELEASE"),
        qemu=_required_path("QEMU_BIN"),
        kernel=_required_path("DEMO07_KERNEL"),
        initrd=_required_path("DEMO07_INITRD"),
        vmlinux=_required_path("DEMO07_VMLINUX"),
        snapshot_disk=_required_path("DEMO07_SNAPSHOT_DISK"),
        snapshot_name=os.environ.get("DEMO07_SNAPSHOT_NAME", "hermit-boot"),
        artifact_dir=_required_path("DEMO07_ARTIFACTS"),
        qemu_bios=_optional_path("DEMO07_QEMU_BIOS"),
        qemu_library_path=_optional_path("DEMO07_QEMU_LIBRARY_PATH"),
        timeout=float(os.environ.get("DEMO07_TIMEOUT", "240")),
    )


def _task_list(program):
    init_task = program["init_task"]
    rows = [
        (
            init_task.pid.value_(),
            init_task.comm.string_().decode("utf-8", "replace"),
        )
    ]
    rows.extend(
        (task.pid.value_(), task.comm.string_().decode("utf-8", "replace"))
        for task in list_for_each_entry(
            "struct task_struct", init_task.tasks.address_of_(), "tasks"
        )
    )
    return rows


def _canonical(rows):
    return "\n".join("{} {}".format(pid, comm) for pid, comm in rows)


def _task_diff(before, after):
    before_set = set(before)
    after_set = set(after)
    removed = sorted(before_set - after_set)
    added = sorted(after_set - before_set)
    return removed, added


def _run_once(config, command):
    with program_from_hermit(config) as guest:
        with guest.observation() as program:
            before = _task_list(program)
        before_metrics = guest.metrics[-1]

        guest.advance(command, ADVANCE_MARKER)

        with guest.observation() as program:
            after = _task_list(program)
        after_metrics = guest.metrics[-1]

    removed, added = _task_diff(before, after)
    if not removed and not added:
        raise RuntimeError("task list did not change during deterministic advance")
    return before, after, removed, added, before_metrics, after_metrics


def _print_tasks(label, rows, limit):
    shown = rows[:limit]
    print(
        "{} tasks ({} total; first {} shown, pid comm):".format(
            label, len(rows), len(shown)
        )
    )
    for pid, comm in shown:
        print("  {:5d} {}".format(pid, comm))
    if len(rows) > limit:
        print("  ... {} unchanged rows omitted from display".format(len(rows) - limit))


def _print_diff(removed, added):
    print("task-list diff (- before, + after):")
    for pid, comm in removed:
        print("  - {:5d} {}".format(pid, comm))
    for pid, comm in added:
        print("  + {:5d} {}".format(pid, comm))


def main() -> int:
    config = _config()
    runs = int(os.environ.get("DEMO07_RUNS", "2"))
    task_limit = int(os.environ.get("DEMO07_TASK_LIMIT", "16"))
    command = DEFAULT_ADVANCE_COMMAND
    if runs < 2:
        raise ValueError("DEMO07_RUNS must be at least 2 to prove reproducibility")
    if task_limit < 1:
        raise ValueError("DEMO07_TASK_LIMIT must be positive")

    baseline = None
    all_metrics = []
    for run in range(1, runs + 1):
        current = _run_once(config, command)
        before, after, removed, added, before_metrics, after_metrics = current
        canonical = (
            _canonical(before),
            _canonical(after),
            _canonical(removed),
            _canonical(added),
        )
        if baseline is None:
            baseline = current
            expected = canonical
        elif canonical != expected:
            raise RuntimeError(
                "before/after task snapshots or their diff changed between restores"
            )
        all_metrics.extend((before_metrics, after_metrics))
        print(
            "evolution {}: before_tasks={} after_tasks={} removed={} added={} "
            "read_states={}/{},{}/{} serial_delta=0/0".format(
                run,
                len(before),
                len(after),
                len(removed),
                len(added),
                before_metrics.qemu_state,
                before_metrics.tracer_state,
                after_metrics.qemu_state,
                after_metrics.tracer_state,
            ),
            flush=True,
        )

    before, after, removed, added, _, _ = baseline
    _print_tasks("before", before, task_limit)
    _print_tasks("after", after, task_limit)
    _print_diff(removed, added)
    if any(item.serial_bytes_delta != 0 for item in all_metrics):
        raise RuntimeError("guest serial output advanced during a drgn read")
    print(
        "RESULT: restored phase-5 snapshot; fixed_virtual_advance_us=1000; "
        "task_lists_differ=yes; evolution_reproducible=yes; "
        "read_virtual_time_advanced=no"
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("Demo 07 failed: {}".format(error), file=sys.stderr)
        sys.exit(1)
