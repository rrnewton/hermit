"""Command-line interface for tick-hub.

Subcommands:
  tick   --config FILE [--state FILE]   run one tick: emit HEALTH/ACTION/NOTE/ERROR lines
  list   --config FILE                  list the reminders + health checks
  json   --config FILE                  re-emit the config as canonical JSON
  yaml   --config FILE                  re-emit the config as YAML
  state  --state FILE                   validate + show the ops-state's own lines
  quickstart                            print a self-contained getting-started guide
  --userguide                           print the full embedded user guide (the complete reference)

A config is a JSON or YAML file (see `quickstart` for the schema + a runnable example). `--config`
auto-detects the format by extension: `.yaml`/`.yml` load as YAML, everything else as JSON.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from collections.abc import Sequence
from importlib.resources import files
from pathlib import Path

from tick_hub import __version__
from tick_hub.cadence import load_fired_state, persist_fired_state
from tick_hub.engine import run_tick
from tick_hub.io import (
    TickConfigError,
    config_from_json,
    config_from_yaml,
    config_to_json,
    config_to_yaml,
)
from tick_hub.model import EVERY_TICK, TickConfig
from tick_hub.probes import GlobFileAgeProbe, SubprocessGateRunner, wall_clock_now
from tick_hub.state import OpsState, StateError, state_lines

PROG = "tick-hub"

#: Environment variable overriding the default fired-state file location.
STATE_FILE_ENV = "TICK_HUB_STATE"

#: Default fired-state file, RELATIVE TO THE CURRENT WORKING DIRECTORY, used when neither
#: ``--fired-state`` nor ``$TICK_HUB_STATE`` is set. Created on demand.
DEFAULT_FIRED_STATE = os.path.join(".tick-hub", "state")


# --------------------------------------------------------------------------- colors
class Palette:
    """Minimal ANSI colorizer (stdlib only). Disabled for non-ttys and when NO_COLOR is set."""

    def __init__(self, enabled: bool) -> None:
        self.enabled = enabled

    def _wrap(self, code: str, text: str) -> str:
        return f"\033[{code}m{text}\033[0m" if self.enabled else text

    def bold(self, text: str) -> str:
        """Render ``text`` with bold emphasis when colors are enabled."""
        return self._wrap("1", text)

    def dim(self, text: str) -> str:
        """Render ``text`` with dim emphasis when colors are enabled."""
        return self._wrap("2", text)

    def green(self, text: str) -> str:
        """Render ``text`` in green when colors are enabled."""
        return self._wrap("32", text)

    def yellow(self, text: str) -> str:
        """Render ``text`` in yellow when colors are enabled."""
        return self._wrap("33", text)

    def cyan(self, text: str) -> str:
        """Render ``text`` in cyan when colors are enabled."""
        return self._wrap("36", text)


def _color_enabled(stream: object) -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    isatty = getattr(stream, "isatty", None)
    return bool(callable(isatty) and isatty())


def _banner(c: Palette) -> str:
    return (
        f"{c.bold(PROG)} {c.dim('v' + __version__)}\n"
        "A single scheduled tick that funnels many recurring responsibilities, each on its own\n"
        "cadence, and emits machine-readable HEALTH/ACTION/NOTE/ERROR lines for a coordinator or\n"
        "automation to dispatch. Reminders, gates, and health checks are all caller config."
    )


def _epilog(c: Palette) -> str:
    ex = c.cyan
    return (
        f"{c.bold('examples')}\n"
        f"  {ex(f'{PROG} quickstart')}                          {c.dim('# get started (model + runnable demo)')}\n"
        f"  {ex(f'{PROG} tick --config ops.yaml')}              {c.dim('# one tick (dry-run: no state write)')}\n"
        f"  {ex(f'{PROG} tick --config ops.yaml --flush')}      {c.dim('# ...and persist the fired-state')}\n"
        f"  {ex(f'{PROG} list --config ops.yaml')}              {c.dim('# reminders + health checks')}\n"
        f"  {ex(f'{PROG} state --state host.yaml')}             {c.dim('# validate + show the ops-state lines')}\n\n"
        f"{c.dim('The fired-state (cadence epochs + reserved retry diagnostics) lives at ./.tick-hub/state by')}\n"
        f"{c.dim(f'default (override with --fired-state or ${STATE_FILE_ENV}); it is written only on --flush.')}"
    )


# --------------------------------------------------------------------------- quickstart
def _quickstart(c: Palette) -> str:
    h = c.bold
    k = c.cyan
    return f"""{_banner(c)}

{h('The idea')}  {c.dim('- one heartbeat, many cadenced reminders, machine-readable output')}
  A "tick" is one heartbeat (a cron job, a coordinator loop, a systemd timer). On each tick the
  hub checks every DUE reminder and prints a stable, line-oriented report. A slow reminder (say a
  6-hourly sync) only appears when its cadence has elapsed; a reminder that is not due prints
  nothing. This lets ONE loop carry every recurring responsibility instead of N separate timers.

{h('1. Install')}
  python3 -m pip install tick-hub

{h('2. Write a reminder set (YAML)')}  {c.dim('- the caller plug-in; JSON works too')}
  Save as ops.yaml:
  reminders:
    - name: git_sync                 {c.dim('# a plain timed reminder')}
      cadence_secs: 21600            {c.dim('# ~6h; 0 = every tick')}
      emit: {{kind: action, skill: git-sync, title: "fetch origin and reconcile"}}
    - name: backlog                  {c.dim('# a GATED reminder that CAPTURES a live value')}
      cadence_secs: 0
      gate: {{cmd: "echo count=42", when: always, capture: true}}
      emit:
        kind: action
        skill: backlog-triage
        fields: {{threshold: "20"}}
        title: "backlog has {{count}} ready items (threshold {{threshold}})"
    - name: benchmark                {c.dim('# only fires when a state flag is truthy')}
      requires_flags: [benchmark_enabled]
      emit: {{kind: action, skill: run-benchmark, title: "refresh benchmarks"}}
  health_checks:
    - name: db_backup
      glob: "/var/backups/db-*.sql"
      threshold_secs: 93600
      detail: "newest DB snapshot"

{h('3. Run a tick')}
  {k(f'{PROG} tick --config ops.yaml')}          {c.dim('# dry-run: prints the lines, does NOT persist state')}
  {k(f'{PROG} tick --config ops.yaml --flush')}  {c.dim('# persist per-reminder last-fired epochs')}

{h('The output contract')}  {c.dim('(parse by the leading token; lines are independent)')}
  HEALTH: <check> <ok|stale|missing> age_secs=<N|NA> threshold_secs=<N> detail="..."
  ACTION: <skill> [key=value ...] title="..."
  NOTE:   <free text>
  ERROR:  <text>
  {c.dim('A caller dispatches each ACTION to the named skill/handler; HEALTH is a freshness')}
  {c.dim('signal to investigate (not a work item); NOTE is informational; ERROR needs attention.')}

{h('Reminders')}  {c.dim('(each: WHEN to check, WHETHER to fire, WHAT to emit)')}
  cadence_secs    how often to CHECK the reminder (0 = every tick; else seconds since last fire)
  requires_flags  names of ops-state flags that must ALL be truthy, or the reminder is suppressed
  gate            optional shell check: {{cmd, when: success|failure|nonempty|always, capture}}
                  {c.dim('when=success fires on exit 0; failure on non-zero; nonempty on stdout;')}
                  {c.dim('capture=true parses stdout key=value lines into fields + {{placeholders}}')}
  emit            action ({{skill, fields, title}}) or note ({{title}})

{h('The ops-state (per-host runtime state)')}  {c.dim('- YAML; strict + typed')}
  {k(f'{PROG} tick --config ops.yaml --state host.yaml')}
  enabled: true          {c.dim('# master switch; false => summary + health only, no reminders')}
  tick_frequency_min: 30 {c.dim('# desired cadence; --current-tick-min N triggers an actualize ACTION')}
  label: my-host         {c.dim('# optional identity shown in the state summary')}
  flags:                 {c.dim('# caller-defined toggles read by requires_flags (bool/int/str)')}
    benchmark_enabled: true

{h('Cadence + state files')}
  Last-fired epochs and reserved __tick_hub_internal__.* retry diagnostics share one atomic file:
    {k('./.tick-hub/state')}   {c.dim('(created on demand; override with --fired-state or $' + STATE_FILE_ENV + ')')}
  It is written ONLY on {k('--flush')}; the default dry-run mutates nothing.

{h('Exit codes')}  0 = tick ran | 2 = bad usage / bad config or state file
"""


# --------------------------------------------------------------------------- user guide
def _load_userguide() -> str:
    """Return the full user guide, read from the guide EMBEDDED IN THIS PACKAGE.

    The guide is a generated package resource (``tick_hub/USER_GUIDE.md``, declared as
    ``package-data``). Reading it via ``importlib.resources`` makes ``--userguide`` work from an
    installed wheel without relying on a source checkout."""
    return (files("tick_hub") / "USER_GUIDE.md").read_text(encoding="utf-8")


# --------------------------------------------------------------------------- loading
def _load_config(config_arg: str) -> TickConfig:
    path = Path(config_arg)
    text = path.read_text(encoding="utf-8")
    if path.suffix.lower() in (".yaml", ".yml"):
        return config_from_yaml(text)
    return config_from_json(text)


def _resolve_fired_path(fired_arg: str | None) -> Path:
    if fired_arg:
        return Path(fired_arg)
    env = os.environ.get(STATE_FILE_ENV)
    if env:
        return Path(env)
    return Path(DEFAULT_FIRED_STATE)


# --------------------------------------------------------------------------- rendering
def _render_list(cfg: TickConfig, c: Palette) -> str:
    lines: list[str] = []
    if cfg.reminders:
        lines.append(c.bold("reminders:"))
        width = max(len(r.name) for r in cfg.reminders)
        for rem in cfg.reminders:
            cadence = "every-tick" if rem.cadence_secs == EVERY_TICK else f"{rem.cadence_secs}s"
            gate = ""
            if rem.gate is not None:
                cap = ",capture" if rem.gate.capture else ""
                gate = c.dim(f" gate[{rem.gate.when.value}{cap}]")
            flags = c.dim(f" needs={','.join(rem.requires_flags)}") if rem.requires_flags else ""
            name = c.bold(f"{rem.name:<{width}}")
            kind = c.yellow(f"[{rem.emit.kind.value}]")
            target = rem.emit.skill if rem.emit.skill else rem.emit.title
            lines.append(f"  {name}  {kind} {c.cyan(cadence)} {target}{gate}{flags}")
    else:
        lines.append(c.dim("(no reminders)"))
    if cfg.health_checks:
        lines.append(c.bold("health checks:"))
        for hc in cfg.health_checks:
            lines.append(
                f"  {c.bold(hc.name)}  {c.dim(hc.glob)} "
                f"{c.cyan(f'threshold={hc.threshold_secs}s')}"
            )
    return "\n".join(lines)


# --------------------------------------------------------------------------- parser
class _ColorHelp(argparse.RawDescriptionHelpFormatter):
    """Raw formatter so our colored description/epilog render verbatim."""


def _i64_arg(value: str) -> int:
    if re.fullmatch(r"[+-]?[0-9]+", value) is None:
        raise argparse.ArgumentTypeError(f"invalid int value: {value!r}")
    try:
        parsed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid int value: {value!r}") from exc
    if parsed < -(2**63) or parsed > 2**63 - 1:
        raise argparse.ArgumentTypeError(f"integer is outside the signed 64-bit range: {value!r}")
    return parsed


def _nonnegative_i64_arg(value: str) -> int:
    parsed = _i64_arg(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError(f"value must be non-negative: {value!r}")
    return parsed


def _positive_i64_arg(value: str) -> int:
    parsed = _i64_arg(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError(f"value must be positive: {value!r}")
    return parsed


def build_parser() -> argparse.ArgumentParser:
    """Build the complete command-line argument parser."""
    c = Palette(_color_enabled(sys.stdout))
    parser = argparse.ArgumentParser(
        prog=PROG,
        allow_abbrev=False,
        formatter_class=_ColorHelp,
        description=_banner(c),
        epilog=_epilog(c),
    )
    parser.add_argument("--version", action="version", version=f"{PROG} {__version__}")
    parser.add_argument(
        "--userguide",
        action="store_true",
        help="print the full embedded user guide (the complete reference) and exit",
    )
    sub = parser.add_subparsers(dest="command", metavar="<command>")

    tick_p = sub.add_parser(
        "tick", allow_abbrev=False, help="run one tick (emit HEALTH/ACTION/NOTE/ERROR lines)",
        description="run one tick (emit HEALTH/ACTION/NOTE/ERROR lines)",
    )
    tick_p.add_argument(
        "--config",
        required=True,
        metavar="FILE",
        help="reminder-set config file; .yaml/.yml load as YAML, else JSON",
    )
    tick_p.add_argument(
        "--state",
        metavar="FILE",
        default=None,
        help="per-host ops-state YAML (optional; a sensible enabled default is used when omitted)",
    )
    tick_p.add_argument(
        "--fired-state",
        metavar="FILE",
        default=None,
        help=f"cadence and reserved retry-diagnostic state file (default: ./{DEFAULT_FIRED_STATE} or "
        f"${STATE_FILE_ENV})",
    )
    tick_p.add_argument(
        "--now",
        type=_nonnegative_i64_arg,
        default=None,
        metavar="EPOCH",
        help="override the clock with an explicit epoch (seconds) for deterministic runs",
    )
    tick_p.add_argument(
        "--current-tick-min",
        type=_positive_i64_arg,
        default=None,
        metavar="N",
        help="the actually-running tick cadence in minutes; when it differs from the ops-state's "
        "tick_frequency_min, emit an actualize-tick-frequency ACTION",
    )
    tick_p.add_argument(
        "--flush",
        action="store_true",
        help="persist the advanced fired-state (default: dry-run, mutate nothing)",
    )
    tick_p.add_argument(
        "--dry-run",
        action="store_true",
        help=(
            "show EVERY reminder regardless of when it last fired, and print a line "
            "for the clean ones too (implies no state write; refuses --flush)"
        ),
    )
    tick_p.add_argument(
        "--report-pending",
        action="store_true",
        help=(
            "print a verdict for each due reminder, including CLEAN and SUPPRESSED; "
            "unlike --dry-run, preserves cadence selection and is compatible with --flush"
        ),
    )
    tick_p.add_argument(
        "--no-header",
        action="store_true",
        help="suppress the explanatory stderr banner (pure machine parsing)",
    )

    state_p = sub.add_parser(
        "state", allow_abbrev=False, help="validate + show the ops-state's own lines",
        description="validate + show the ops-state's own lines",
    )
    state_p.add_argument("--state", required=True, metavar="FILE", help="ops-state YAML file")
    state_p.add_argument(
        "--current-tick-min",
        type=_positive_i64_arg,
        default=None,
        metavar="N",
        help="the running tick cadence in minutes (drives the actualize-tick-frequency ACTION)",
    )

    for cmd, helptext in (
        ("list", "list the reminders + health checks"),
        ("json", "re-emit the config as canonical JSON"),
        ("yaml", "re-emit the config as YAML"),
    ):
        sp = sub.add_parser(cmd, allow_abbrev=False, help=helptext, description=helptext)
        sp.add_argument(
            "--config",
            required=True,
            metavar="FILE",
            help="config file; .yaml/.yml load as YAML, else JSON",
        )

    sub.add_parser(
        "quickstart", allow_abbrev=False, help="print a self-contained getting-started guide",
        description="print a self-contained getting-started guide",
    )
    return parser


# --------------------------------------------------------------------------- commands
def _cmd_tick(ns: argparse.Namespace, c: Palette) -> int:
    config_arg = ns.config if isinstance(ns.config, str) else None
    if config_arg is None:
        print(f"{PROG}: tick: --config FILE is required", file=sys.stderr)
        return 2
    try:
        cfg = _load_config(config_arg)
    except (OSError, TickConfigError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2

    state_arg = ns.state if isinstance(ns.state, str) else None
    state_note: str | None = None
    if state_arg is None:
        state = OpsState.default()
        state_note = "no --state file given; using the default enabled ops-state"
    elif not Path(state_arg).is_file():
        state = OpsState.default()
        state_note = f"ops-state file {state_arg} not found; using the default enabled ops-state"
    else:
        try:
            state = OpsState.load(state_arg)
        except (OSError, StateError) as exc:
            print(f"{PROG}: invalid ops-state {state_arg}: {exc}", file=sys.stderr)
            return 2

    now = int(ns.now) if isinstance(ns.now, int) else wall_clock_now()
    current_tick_min = ns.current_tick_min if isinstance(ns.current_tick_min, int) else None
    fired_path = _resolve_fired_path(ns.fired_state if isinstance(ns.fired_state, str) else None)
    dry_run = bool(ns.dry_run)
    report_pending = dry_run or bool(ns.report_pending)
    if dry_run and bool(ns.flush):
        # Refuse rather than silently ignore one of them. --dry-run reports what
        # WOULD run; --flush records what DID. Advancing the cadence from a report
        # that deliberately ignored the cadence would mark every reminder as just
        # fired, which is the one outcome that silently destroys the schedule.
        print(
            f"{PROG}: --dry-run and --flush are mutually exclusive: --dry-run ignores "
            "the fired-state, so persisting its result would reset every cadence",
            file=sys.stderr,
        )
        return 2

    # ⚠️ THE FIRED-STATE IS STILL READ AND STILL NOT WRITTEN. Passing {} to run_tick
    # is what makes this a real report of everything pending: is_due() treats a
    # reminder with no recorded epoch as never-run and therefore due, so an empty
    # mapping means "ask every reminder". The file on disk is untouched either way
    # -- run_tick is pure with respect to it and only --flush persists, which the
    # guard above has already excluded.
    fired = load_fired_state(fired_path)

    # ⚠️ THE HEADER GOES OUT BEFORE THE TICK RUNS, because the report now does too.
    # A tick can take minutes, and callers run it under an external time bound
    # that kills it. Everything this command has already determined must be on
    # stdout by then, so each line is written and FLUSHED as it is produced
    # rather than collected and printed after run_tick returns -- which, in the
    # case that matters, it never does.
    if not bool(ns.no_header):
        print(_banner(c), file=sys.stderr)
        if state_note is not None:
            print(f"{PROG}: {state_note}", file=sys.stderr)

    def emit_line(line: str) -> None:
        print(line)
        sys.stdout.flush()

    result = run_tick(
        cfg,
        state,
        now=now,
        fired={} if dry_run else fired,
        gate_runner=SubprocessGateRunner(),
        age_probe=GlobFileAgeProbe(),
        current_tick_min=current_tick_min,
        report_pending=report_pending,
        emit=emit_line,
    )

    if bool(ns.flush):
        try:
            persist_fired_state(fired_path, dict(result.fired))
        except (OSError, ValueError) as exc:
            print(f"{PROG}: cannot persist fired-state to {fired_path}: {exc}", file=sys.stderr)
            return 2
        print(f"{PROG}: fired-state persisted to {fired_path}", file=sys.stderr)
    else:
        print(
            f"{PROG}: no state write (fired-state NOT persisted; pass --flush to persist). "
            f"This still consults the fired-state, so it shows only what is DUE -- "
            f"pass --dry-run to ignore it and see everything outstanding.",
            file=sys.stderr,
        )
    return 0


def _cmd_state(ns: argparse.Namespace) -> int:
    state_arg = ns.state if isinstance(ns.state, str) else None
    if state_arg is None:
        print(f"{PROG}: state: --state FILE is required", file=sys.stderr)
        return 2
    try:
        state = OpsState.load(state_arg)
    except (OSError, StateError) as exc:
        print(f"{PROG}: invalid ops-state {state_arg}: {exc}", file=sys.stderr)
        return 2
    current_tick_min = ns.current_tick_min if isinstance(ns.current_tick_min, int) else None
    for line in state_lines(state, current_tick_min):
        print(line)
    return 0


def _cmd_config_render(ns: argparse.Namespace, command: str, c: Palette) -> int:
    config_arg = ns.config if isinstance(ns.config, str) else None
    if config_arg is None:
        print(f"{PROG}: {command}: --config FILE is required", file=sys.stderr)
        return 2
    try:
        cfg = _load_config(config_arg)
    except (OSError, TickConfigError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    if command == "list":
        print(_render_list(cfg, c))
    elif command == "json":
        print(config_to_json(cfg))
    elif command == "yaml":
        # config_to_yaml needs the declared PyYAML dependency; surface its absence as a clean
        # actionable message (exit 2) rather than letting TickConfigError escape as a traceback.
        try:
            text = config_to_yaml(cfg)
        except TickConfigError as exc:
            print(f"{PROG}: {exc}", file=sys.stderr)
            return 2
        sys.stdout.write(text)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the command-line application and return its process status."""
    parser = build_parser()
    ns = parser.parse_args(list(argv) if argv is not None else None)
    c = Palette(_color_enabled(sys.stdout))

    if bool(ns.userguide):
        sys.stdout.write(_load_userguide())
        return 0

    command = ns.command if isinstance(ns.command, str) else None
    if command is None:
        parser.print_help()
        return 0
    if command == "quickstart":
        print(_quickstart(c))
        return 0
    if command == "tick":
        return _cmd_tick(ns, c)
    if command == "state":
        return _cmd_state(ns)
    if command in ("list", "json", "yaml"):
        return _cmd_config_render(ns, command, c)
    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())


__all__ = ["main", "build_parser", "PROG", "STATE_FILE_ENV", "DEFAULT_FIRED_STATE"]
