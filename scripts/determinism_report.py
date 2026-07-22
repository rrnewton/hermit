#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Audit Hermit/Detcore per-syscall determinism coverage.

The tool parses the syscall dispatch `match` in ``detcore/src/lib.rs`` to find
every syscall Detcore intercepts and classifies each handler, then cross-
references those against the syscalls that real applications issue (captured
with ``strace -f -c``) to produce a coverage matrix and a gap list.

Classification (heuristic, derived from the dispatch arm text):

* ``FULL``        - a dedicated ``handle_*`` handler, unconditional. Detcore
                    emulates/sanitizes the call for determinism.
* ``PARTIAL``     - handled, but the arm/comment flags it as not fully
                    deterministic yet, or the handler is gated on a config flag
                    (e.g. ``if virtualize_time``) so it only determinizes in the
                    default configuration.
* ``PASSTHROUGH`` - forwarded to the host via ``self.passthrough(..)``. Result
                    is whatever the kernel returns; deterministic only if the
                    underlying operation is.
* ``MISSING``     - not in the dispatch (falls through to the ``_`` arm and
                    returns ``ENOSYS`` unless ``--allow-passthrough``), or an
                    explicit inline ``ENOSYS``/``panic!`` stub.

Usage::

    scripts/determinism_report.py \\
        --lib detcore/src/lib.rs \\
        --strace redis=redis.strace --strace nginx=nginx.strace \\
        --strace python=py.strace --strace go=go.strace \\
        --out docs/syscall-determinism-report.md

Each ``--strace NAME=PATH`` file is the output of ``strace -f -c`` for that
application.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field


# Detcore variant PascalCase -> raw kernel syscall name overrides, for the few
# cases where the mechanical PascalCase->snake_case conversion does not match
# the name strace prints.
VARIANT_ALIASES = {
    "Newfstatat": "newfstatat",
    "EpollWaitOld": "epoll_wait_old",
    "EpollCtlOld": "epoll_ctl_old",
    "InotifyInit1": "inotify_init1",
    "InotifyInit": "inotify_init",
}

# strace sometimes prints an alternate name for the same syscall number; map the
# alternates back to the canonical raw name Detcore's variant converts to.
STRACE_ALIASES = {
    "fstatat64": "newfstatat",
    "fstatat": "newfstatat",
    "epoll_pwait2": "epoll_pwait",
    "prlimit": "prlimit64",
    "pread": "pread64",
    "pwrite": "pwrite64",
    "sigreturn": "rt_sigreturn",
    "select": "select",
    "_newselect": "select",
    "getrlimit": "prlimit64",  # glibc getrlimit routes through prlimit64
}

CATS = ["FULL", "PARTIAL", "PASSTHROUGH", "MISSING"]


def pascal_to_snake(name: str) -> str:
    """Convert a Detcore ``Syscall::Variant`` name to a raw kernel name."""
    if name in VARIANT_ALIASES:
        return VARIANT_ALIASES[name]
    # Insert '_' before each uppercase run boundary, then lowercase. Digits stay
    # attached to the preceding token: Getdents64 -> getdents64.
    s = re.sub(r"(?<!^)(?=[A-Z])", "_", name)
    return s.lower()


@dataclass
class Handler:
    raw: str
    variant: str
    category: str
    note: str = ""


def parse_dispatch(lib_path: str) -> dict[str, Handler]:
    """Parse the syscall dispatch match in lib.rs into raw_name -> Handler."""
    with open(lib_path, encoding="utf-8") as fh:
        lines = fh.readlines()

    # Find the dispatch match block: from "let res = match call {" to the top
    # level "_ => {" default arm at the same nesting.
    start = None
    end = None
    for i, ln in enumerate(lines):
        if start is None and "let res = match call {" in ln:
            start = i
            continue
        if start is not None and re.match(r"\s{12}_ => \{", ln):
            end = i
            break
    if start is None or end is None:
        sys.exit("could not locate the syscall dispatch match in lib.rs")

    handlers: dict[str, Handler] = {}
    block = lines[start + 1 : end]

    # Group each arm: an arm begins on a line that contains "Syscall::" at the
    # LHS and runs until the next such line (arms may span multiple lines).
    arm_starts = [
        idx for idx, ln in enumerate(block) if re.search(r"\bSyscall::", ln)
    ]
    for pos, idx in enumerate(arm_starts):
        nxt = arm_starts[pos + 1] if pos + 1 < len(arm_starts) else len(block)
        arm_text = "".join(block[idx:nxt])
        # Preceding comment (immediately above) can carry a PARTIAL marker.
        comment = ""
        j = idx - 1
        while j >= 0 and block[j].strip().startswith("//"):
            comment = block[j] + comment
            j -= 1

        lhs = arm_text.split("=>", 1)[0]
        rhs = arm_text.split("=>", 1)[1] if "=>" in arm_text else ""

        variants = re.findall(r"Syscall::([A-Za-z0-9]+)", lhs)
        # Handle `Syscall::Other(Sysno::faccessat2, args)` style arms.
        others = re.findall(r"Sysno::([a-z0-9_]+)", lhs)

        guard = None
        gm = re.search(r"\bif\s+([a-z_]+)", lhs)
        if gm:
            guard = gm.group(1)

        blob = (comment + rhs).lower()
        if "passthrough(" in rhs:
            category, note = "PASSTHROUGH", ""
        elif "panic!" in rhs:
            category, note = "MISSING", "explicit panic (unsupported)"
        elif "errno::enosys" in blob:
            category, note = "MISSING", "explicit ENOSYS stub"
        elif (
            "deterministic yet" in blob
            or "not fully deterministic" in blob
            or "partial" in blob
            or "fixme" in blob
        ):
            category, note = "PARTIAL", "flagged not-fully-deterministic in source"
        elif guard:
            category, note = "PARTIAL", f"conditional on --{guard.replace('_', '-')}"
        else:
            category, note = "FULL", ""

        names = [pascal_to_snake(v) for v in variants] + list(others)
        for raw in names:
            # Keep the strongest classification if a raw name appears twice
            # (e.g. Open/Openat both -> openat handler is per-variant; names are
            # distinct so this rarely collides).
            handlers[raw] = Handler(raw=raw, variant="|".join(variants) or raw,
                                    category=category, note=note)
    return handlers


def parse_strace(path: str) -> dict[str, int]:
    """Parse `strace -f -c` output -> {raw_syscall_name: call_count}."""
    out: dict[str, int] = {}
    with open(path, encoding="utf-8") as fh:
        for ln in fh:
            parts = ln.split()
            if not parts:
                continue
            name = parts[-1]
            if not re.match(r"^[a-z_][a-z0-9_]*$", name):
                continue
            if name in ("syscall", "total", "seconds", "usecs", "errors",
                        "time", "calls"):
                continue
            # Column before the name is a count when the row is a real syscall.
            calls = 0
            for tok in parts[:-1]:
                if tok.isdigit():
                    calls = int(tok)
            name = STRACE_ALIASES.get(name, name)
            out[name] = out.get(name, 0) + max(calls, 1)
    return out


def category_for(raw: str, handlers: dict[str, Handler]) -> tuple[str, str]:
    h = handlers.get(raw)
    if h is None:
        raw2 = STRACE_ALIASES.get(raw, raw)
        h = handlers.get(raw2)
    if h is None:
        return "MISSING", "not in dispatch (ENOSYS unless --allow-passthrough)"
    return h.category, h.note


def render(handlers: dict[str, Handler], apps: dict[str, dict[str, int]]) -> str:
    md: list[str] = []
    a = md.append
    a("# Hermit syscall determinism report\n")
    a("> Generated by `scripts/determinism_report.py`. Do not edit by hand; "
      "re-run the tool to refresh.\n")
    a("This report audits which Linux syscalls Detcore intercepts, how each is "
      "handled for determinism, and which syscalls real applications actually "
      "issue. Use it to prioritise determinism-envelope expansion: the **Gaps** "
      "section lists syscalls that target apps need but Hermit does not handle.\n")

    a("## Classification method\n")
    a("Classifications are parsed from the dispatch `match` in "
      "`detcore/src/lib.rs`:\n")
    a("| Category | Meaning |")
    a("| --- | --- |")
    a("| `FULL` | Dedicated `handle_*` handler, unconditional: Detcore "
      "emulates/sanitizes the call. |")
    a("| `PARTIAL` | Handled but source-flagged as not fully deterministic, or "
      "gated on a config flag (e.g. `--virtualize-time`). |")
    a("| `PASSTHROUGH` | Forwarded to the host via `passthrough()`; "
      "deterministic only if the underlying operation is. |")
    a("| `MISSING` | Not in dispatch (returns `ENOSYS` unless "
      "`--allow-passthrough`), or an explicit `ENOSYS`/`panic!` stub. |")
    a("")

    # Coverage totals
    counts = {c: 0 for c in CATS}
    for h in handlers.values():
        counts[h.category] = counts.get(h.category, 0) + 1
    a("## Detcore dispatch coverage\n")
    a(f"Detcore's dispatch names **{len(handlers)}** distinct syscalls "
      "(the `_` default arm returns `ENOSYS` for everything else):\n")
    a("| Category | Count |")
    a("| --- | --- |")
    for c in CATS:
        a(f"| `{c}` | {counts.get(c, 0)} |")
    a("")

    # Per-app matrix
    a("## Per-application coverage matrix\n")
    a("Each application's syscalls were captured with `strace -f -c` (see "
      "*Reproduction*). The table shows, per app, how many distinct syscalls it "
      "issues in each Hermit category.\n")
    header = "| App | distinct | " + " | ".join(f"`{c}`" for c in CATS) + " |"
    a(header)
    a("| " + " --- |" * (len(CATS) + 2))
    for app, sys in apps.items():
        row_counts = {c: 0 for c in CATS}
        for raw in sys:
            cat, _ = category_for(raw, handlers)
            row_counts[cat] += 1
        a(f"| {app} | {len(sys)} | "
          + " | ".join(str(row_counts[c]) for c in CATS) + " |")
    a("")

    # Gaps: syscalls apps need that are MISSING
    a("## Gaps: syscalls apps issue that Hermit does not handle\n")
    a("These are `MISSING` for at least one target app — the highest-value "
      "candidates for new handlers or determinization. “apps” lists which "
      "profiled apps issue the call.\n")
    gap: dict[str, list[str]] = {}
    for app, sys in apps.items():
        for raw in sys:
            cat, _ = category_for(raw, handlers)
            if cat == "MISSING":
                gap.setdefault(raw, []).append(app)
    if gap:
        a("| syscall | apps | note |")
        a("| --- | --- | --- |")
        for raw in sorted(gap):
            _, note = category_for(raw, handlers)
            a(f"| `{raw}` | {', '.join(gap[raw])} | {note} |")
    else:
        a("_No MISSING syscalls among the profiled apps._")
    a("")

    # Reproduction
    a("## Reproduction\n")
    a("Regenerate this report with::\n")
    a("```sh")
    a("# 1. Capture per-app syscall profiles (strace -f -c prints distinct")
    a("#    syscalls with call counts). Representative short runs:")
    a("strace -f -c -o redis.strace \\")
    a("  redis-server --port 7799 --save '' --appendonly no --daemonize no  # ^C after startup")
    a("strace -f -c -o nginx.strace nginx -c /path/to/minimal.conf -p /tmp   # daemon off")
    a("strace -f -c -o py.strace python3 -c \"import json,socket,os,threading,time; ...\"")
    a("strace -f -c -o go.strace ./a_small_go_binary")
    a("")
    a("# 2. Cross-reference against the Detcore dispatch and emit this report:")
    a("scripts/determinism_report.py --lib detcore/src/lib.rs \\")
    a("  --strace redis=redis.strace --strace nginx=nginx.strace \\")
    a("  --strace python=py.strace --strace go=go.strace \\")
    a("  --out docs/syscall-determinism-report.md")
    a("```")
    a("The syscall *sets* are host- and version-dependent (libc, kernel, app "
      "build); call *counts* vary run to run. The Detcore classifications come "
      "directly from `detcore/src/lib.rs` and are stable for a given checkout.\n")

    # Full per-syscall table per app (union), for reference.
    a("## Full syscall x app detail\n")
    all_syscalls = sorted(set().union(*[set(s) for s in apps.values()]))
    a("| syscall | category | " + " | ".join(apps) + " | note |")
    a("| --- | --- | " + " | ".join("---" for _ in apps) + " | --- |")
    for raw in all_syscalls:
        cat, note = category_for(raw, handlers)
        marks = []
        for app in apps:
            marks.append(str(apps[app].get(raw, "")) if raw in apps[app] else "")
        a(f"| `{raw}` | `{cat}` | " + " | ".join(marks) + f" | {note} |")
    a("")

    return "\n".join(md) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lib", required=True, help="path to detcore/src/lib.rs")
    ap.add_argument("--strace", action="append", default=[], metavar="NAME=PATH",
                    help="app strace -f -c file, e.g. redis=redis.strace")
    ap.add_argument("--out", help="output markdown path (default: stdout)")
    args = ap.parse_args()

    handlers = parse_dispatch(args.lib)
    apps: dict[str, dict[str, int]] = {}
    for spec in args.strace:
        name, _, path = spec.partition("=")
        apps[name] = parse_strace(path)

    report = render(handlers, apps)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(report)
        print(f"wrote {args.out}", file=sys.stderr)
    else:
        sys.stdout.write(report)


if __name__ == "__main__":
    main()
