"""Command-line interface for pr-landing-planner.

Subcommands:
  plan        build the conflict graph + CI/freshness fusion and print the landing PLAN (default)
  graph       just the conflict/ordering graph view
  status      just per-PR CI/label health
  quickstart  print a self-contained getting-started guide (no repo/network needed)
  --userguide print the full embedded user guide (the complete reference)

Data comes from a live GitHub host (``gh`` + a local ``git`` clone) unless ``--fixture FILE`` selects
a deterministic in-memory :class:`~pr_landing_planner.fakehost.FakeHost` (used by the demo + tests).
The planner is ADVISORY ONLY: it recommends actions; it never arms, refires, or merges anything.
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import sys
from collections.abc import Sequence
from datetime import datetime, timezone
from importlib.resources import files
from pathlib import Path

from pr_landing_planner import __version__
from pr_landing_planner.classify import (
    ClassifyConfig,
    FlakySignature,
    flaky_signatures_from_objs,
)
from pr_landing_planner.collect import CollectionError, collect_graph
from pr_landing_planner.emit import (
    render_actions,
    render_clusters_human,
    render_clusters_json,
    render_graph_human,
    render_graph_json,
    render_human,
    render_json,
    render_status_human,
    render_status_json,
)
from pr_landing_planner.fakehost import FakeHost, FixtureError, load_fixture_text
from pr_landing_planner.githubhost import GitHubHost, HostCommandError
from pr_landing_planner.host import VcsHost
from pr_landing_planner.landing_context import parse_landing_context
from pr_landing_planner.model import DEFAULT_BASE, DEFAULT_GATE_CHECK, DEFAULT_REPO, PlanResult
from pr_landing_planner.plan import DEFAULT_FRESHNESS_MAX_BEHIND, assemble_result
from pr_landing_planner.priority import (
    DEFAULT_LABEL_PATTERN,
    NonePriority,
    PriorityProvider,
    make_priority_provider,
)

_I64_TOKEN = re.compile(r"[+-]?[0-9]+\Z")
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


def _i64_arg(value: str) -> int:
    if _I64_TOKEN.fullmatch(value) is None:
        raise argparse.ArgumentTypeError(f"expected a signed 64-bit integer, got {value!r}")
    parsed = int(value)
    if not _I64_MIN <= parsed <= _I64_MAX:
        raise argparse.ArgumentTypeError(f"integer outside signed 64-bit range: {value!r}")
    return parsed


def _nonnegative_i64_arg(value: str) -> int:
    parsed = _i64_arg(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError(f"expected a nonnegative integer, got {value!r}")
    return parsed

PROG = "pr-landing-planner"
DEFAULT_WARN_THRESHOLD = 8


# --------------------------------------------------------------------------- colors
class Palette:
    """Minimal ANSI colorizer (stdlib only). Disabled for non-ttys and when NO_COLOR is set."""

    def __init__(self, enabled: bool) -> None:
        self.enabled = enabled

    def _wrap(self, code: str, text: str) -> str:
        return f"\033[{code}m{text}\033[0m" if self.enabled else text

    def bold(self, text: str) -> str:
        """Render *text* in bold when color output is enabled."""

        return self._wrap("1", text)

    def dim(self, text: str) -> str:
        """Render *text* dimly when color output is enabled."""

        return self._wrap("2", text)

    def red(self, text: str) -> str:
        """Render *text* in red when color output is enabled."""

        return self._wrap("31", text)

    def green(self, text: str) -> str:
        """Render *text* in green when color output is enabled."""

        return self._wrap("32", text)

    def yellow(self, text: str) -> str:
        """Render *text* in yellow when color output is enabled."""

        return self._wrap("33", text)

    def cyan(self, text: str) -> str:
        """Render *text* in cyan when color output is enabled."""

        return self._wrap("36", text)

    def style(self, name: str, text: str) -> str:
        """Apply a named palette style, leaving text unchanged for unknown names."""

        fn = {
            "bold": self.bold,
            "dim": self.dim,
            "red": self.red,
            "green": self.green,
            "yellow": self.yellow,
            "cyan": self.cyan,
        }.get(name)
        return fn(text) if fn is not None else text


def _color_enabled(stream: object) -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    isatty = getattr(stream, "isatty", None)
    return bool(callable(isatty) and isatty())


def _banner(c: Palette) -> str:
    return (
        f"{c.bold(PROG)} {c.dim('v' + __version__)}\n"
        "A conflict-graph + CI-aware, ADVISORY pull-request landing planner. It builds the real\n"
        "merge-conflict graph over the open PRs, classifies each red CI into one of five failure\n"
        "modes, exact-head validation evidence, caller authority, freshness + holds, and\n"
        "mechanism overlaps, then recommends a per-PR action. It NEVER mutates a PR."
    )


def _epilog(c: Palette) -> str:
    ex = c.cyan
    return (
        f"{c.bold('examples')}\n"
        f"  {ex(f'{PROG} quickstart')}                              {c.dim('# get started (model + runnable demo)')}\n"
        f"  {ex(f'{PROG} plan --fixture demo.yaml')}                {c.dim('# a full plan from a fixture (no network)')}\n"
        f"  {ex(f'{PROG} plan --repo OWNER/NAME --git-dir /path/to/clone')} {c.dim('# plan a live repository')}\n"
        f"  {ex(f'{PROG} graph --repo OWNER/NAME --base main')}          {c.dim('# just the conflict graph')}\n"
        f"  {ex(f'{PROG} status --repo OWNER/NAME')}                {c.dim('# just per-PR CI health')}\n\n"
        f"{c.dim('Advisory only. Nothing project-specific is baked in: the gate-check name, flaky')}\n"
        f"{c.dim('signatures, and priority source are all flags/config.')}"
    )


# --------------------------------------------------------------------------- quickstart
def _quickstart(c: Palette) -> str:
    h = c.bold
    k = c.cyan
    return f"""{_banner(c)}

{h('The idea')}  {c.dim('- fuse the conflict graph with LIVE CI health into one landing plan')}
  Given the open PRs targeting a base branch, the planner answers, in one shot:
    * which PRs truly conflict (real git merge-tree, not just shared files),
    * which reds are REAL vs. benign (flaky / stale gate / evaluate-once race / runner outage),
    * how stale each green PR is (commits behind the base),
    * which PRs are held (draft / base-conflict / depends-on-held), and
    * exact-head local validation + caller hard/soft-green authority and gate-policy disposition,
    * shared mechanism tags that require coordinator review, and
    * a recommended per-PR ACTION with an assigned agent, ordered priority -> size -> age.
  It is ADVISORY: it recommends; a landing skill / coordinator executes the mutations.

{h('1. Install')}
  python -m pip install pr-landing-planner

{h('2. Try it with the bundled fixture')}  {c.dim('- no repo, no network')}
  {k(f'{PROG} plan --fixture <(printf %s "$({PROG} quickstart --emit-demo)")')}
  {c.dim('or save the demo fixture below and point --fixture at it.')}

{h('3. Run against a real repo')}
  {k(f'{PROG} plan --repo OWNER/NAME --base main --git-dir /path/to/clone')}
  {c.dim('gh lists the PRs (with CI rollup + labels); git merge-tree finds real conflicts.')}

{h('The five red classifications')}  {c.dim('(the headline value)')}
  real                   a genuine regression        -> hold-fix
  flaky                  matches --flaky-signatures   -> refire-ci
  stale-required-check   CI green, gate frozen        -> refire-stale-gate (unless local evidence)
  evaluate-once-race     gate fired while CI queued   -> wait (benign)
  runner-outage          gate job never ran           -> escalate-runner-outage

{h('Per-PR actions')}
  land-now | rebase-then-land | refire-stale-gate | escalate-runner-outage |
  escalate-gate-policy | refire-ci | hold-fix | wait

{h('Output formats')}  {c.dim('--format {{human,json,actions}}')}
  human    a readable landing summary (default)
  json     the full machine schema (deterministic; 2-space indent, sorted keys)
  actions  a stable {k('key=value')} summary block followed by one ACTION/ERROR/NOTE line per PR

{h('Key flags')}
  --conflict-detector {{merge-tree,file-overlap}}   merge-tree (real conflicts) is the default
  --gate-check NAME                               required-check name (default: {DEFAULT_GATE_CHECK})
  --flaky-signatures FILE                         name/text regexes marking a red as flaky
  --freshness-max-behind N                        caller override (default: none; being behind is not a rebase reason)
  --priority-source {{none,labels,command}}         ordering priority (default: none)
  --batch                                         also propose one green, conflict/dependency-free batch
  --archive-dir DIR / --no-archive                archive the plan JSON to disk (on by default for
                                                  live runs; path printed as a NOTE on stderr)
  --landing-context FILE                          exact validation/review evidence / authority / policy / agent

{h('Exit codes')}  0 = ok | 2 = bad usage / host or fixture error

{h('Demo fixture')}  {c.dim('(save as demo.yaml, then: ' + PROG + ' plan --fixture demo.yaml)')}
{_DEMO_FIXTURE}"""


#: A small, self-contained fixture used by `quickstart --emit-demo` and the bundled example.
_DEMO_FIXTURE = """repo: OWNER/NAME
base: main
prs:
  - number: 1043
    title: fast, fresh, all green
    head_ref: feat-a
    additions: 10
    labels: [validated-locally]
    changed_files: [src/a.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
  - number: 987
    title: green but 6 behind base
    head_ref: feat-b
    additions: 40
    commits_behind: 6
    changed_files: [src/b.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
  - number: 942
    title: CI green, gate stale
    head_ref: feat-c
    changed_files: [src/c.rs]
    checks:
      - {name: CI, status: COMPLETED, conclusion: SUCCESS}
      - {name: merge-gate, status: COMPLETED, conclusion: FAILURE, text: stale result}
  - number: 1050
    title: gate fired while CI queued
    head_ref: feat-d
    changed_files: [src/d.rs]
    checks:
      - {name: CI, status: IN_PROGRESS, conclusion: ""}
      - {name: merge-gate, status: COMPLETED, conclusion: FAILURE, text: "Full CI still queued; rerun after CI completes"}
  - number: 1049
    title: red on wasm-core (add a --flaky-signatures file to reclassify as flaky)
    head_ref: feat-e
    changed_files: [src/e.rs]
    checks:
      - {name: wasm-core, status: COMPLETED, conclusion: FAILURE, text: browser flake}
      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}
conflicts:
  - {a: 987, b: 942, paths: [src/shared.rs]}
"""


# --------------------------------------------------------------------------- user guide
def _load_userguide() -> str:
    """Return the full user guide, read from the guide EMBEDDED IN THIS PACKAGE.

    The guide is a generated package resource (``pr_landing_planner/USER_GUIDE.md``, declared as
    ``package-data``). Reading it via ``importlib.resources`` makes ``--userguide`` work from an
    installed wheel without relying on a source checkout."""
    return (files("pr_landing_planner") / "USER_GUIDE.md").read_text(encoding="utf-8")


# --------------------------------------------------------------------------- collection wiring
def _only_numbers(prs_arg: str | None) -> frozenset[int] | None:
    if not prs_arg:
        return None
    try:
        numbers = frozenset(_i64_arg(x.strip()) for x in prs_arg.split(",") if x.strip())
    except argparse.ArgumentTypeError as exc:
        raise ValueError(f"invalid --prs value: {exc}") from exc
    if any(number <= 0 for number in numbers):
        raise ValueError("--prs requires positive PR numbers")
    return numbers


def _load_doc(path_arg: str) -> object:
    path = Path(path_arg)
    text = path.read_text(encoding="utf-8")
    as_yaml = path.suffix.lower() in (".yaml", ".yml")
    return load_fixture_text(text, as_yaml=as_yaml)


def _classify_config(ns: argparse.Namespace) -> ClassifyConfig:
    gate = ns.gate_check if isinstance(ns.gate_check, str) else DEFAULT_GATE_CHECK
    if not gate.strip():
        raise ValueError("--gate-check must be non-empty")
    sigs: tuple[FlakySignature, ...] = ()
    flaky_arg = getattr(ns, "flaky_signatures", None)
    if isinstance(flaky_arg, str) and flaky_arg:
        sigs = flaky_signatures_from_objs(_load_doc(flaky_arg))
    outage_min = ns.outage_min_prs if isinstance(ns.outage_min_prs, int) else 2
    return ClassifyConfig(gate_check=gate, flaky_signatures=sigs, outage_min_prs=outage_min)


def _priority_provider(ns: argparse.Namespace) -> PriorityProvider:
    source = getattr(ns, "priority_source", None)
    if not isinstance(source, str):
        return NonePriority()
    wrapper = shlex.split(ns.net_wrapper) if isinstance(ns.net_wrapper, str) and ns.net_wrapper else []
    pattern = getattr(ns, "priority_label_pattern", DEFAULT_LABEL_PATTERN)
    command = getattr(ns, "priority_cmd", "")
    return make_priority_provider(
        source,
        label_pattern=pattern if isinstance(pattern, str) else DEFAULT_LABEL_PATTERN,
        command=command if isinstance(command, str) else "",
        wrapper=wrapper,
    )


def _build_host(ns: argparse.Namespace) -> tuple[VcsHost, str, str]:
    fixture = getattr(ns, "fixture", None)
    if isinstance(fixture, str) and fixture:
        host, repo, base = FakeHost.from_fixture(_load_doc(fixture))
        return host, repo, base
    wrapper = shlex.split(ns.net_wrapper) if isinstance(ns.net_wrapper, str) and ns.net_wrapper else []
    gh_host = GitHubHost(
        git_dir=ns.git_dir if isinstance(ns.git_dir, str) else ".",
        remote=ns.remote if isinstance(ns.remote, str) else "origin",
        net_wrapper=wrapper,
        gh_cmd=ns.gh_cmd if isinstance(ns.gh_cmd, str) else "gh",
    )
    repo = ns.repo if isinstance(ns.repo, str) else DEFAULT_REPO
    base = ns.base if isinstance(ns.base, str) else DEFAULT_BASE
    if not repo.strip():
        raise ValueError("--repo OWNER/NAME is required for live runs (or use --fixture FILE)")
    return gh_host, repo, base


def _build_result(ns: argparse.Namespace) -> PlanResult:
    host, repo, base = _build_host(ns)
    context_arg = getattr(ns, "landing_context", None)
    landing_context = (
        parse_landing_context(_load_doc(context_arg))
        if isinstance(context_arg, str) and context_arg
        else ()
    )
    priority = _priority_provider(ns)
    graph = collect_graph(
        host,
        repo=repo,
        base=base,
        only=_only_numbers(ns.prs if isinstance(ns.prs, str) else None),
        conflict_detector=ns.conflict_detector,
        classify_config=_classify_config(ns),
        priority_provider=priority,
        landing_context=landing_context,
    )
    if priority.last_error is not None:
        raise ValueError(priority.last_error)
    freshness = getattr(ns, "freshness_max_behind", DEFAULT_FRESHNESS_MAX_BEHIND)
    batch = bool(getattr(ns, "batch", False))
    outage_min = ns.outage_min_prs if isinstance(ns.outage_min_prs, int) else 2
    return assemble_result(
        graph,
        freshness_max_behind=(
            freshness if isinstance(freshness, int) else DEFAULT_FRESHNESS_MAX_BEHIND
        ),
        outage_min_prs=outage_min,
        batch=batch,
    )


# --------------------------------------------------------------------------- parser
class _ColorHelp(argparse.RawDescriptionHelpFormatter):
    """Raw formatter so our colored description/epilog render verbatim."""


def _add_collect_flags(sp: argparse.ArgumentParser) -> None:
    sp.add_argument(
        "--repo",
        default=DEFAULT_REPO,
        help="OWNER/NAME (required for live runs; omitted with --fixture)",
    )
    sp.add_argument("--base", default=DEFAULT_BASE, help=f"base branch (default: {DEFAULT_BASE})")
    sp.add_argument("--prs", default=None, help="comma-separated PR numbers to restrict to")
    sp.add_argument("--git-dir", default=".", help="path to a local clone of --repo")
    sp.add_argument("--remote", default="origin", help="git remote to fetch from")
    sp.add_argument(
        "--landing-context",
        default=None,
        metavar="FILE",
        help=(
            "JSON/YAML exact-head evidence, validation authority, policy class, and assigned agent"
        ),
    )
    sp.add_argument(
        "--net-wrapper",
        default="",
        help="optional command prefix for gh and git fetch (empty = none)",
    )
    sp.add_argument("--gh-cmd", default="gh", help="gh executable or wrapper command")
    sp.add_argument(
        "--conflict-detector",
        choices=["merge-tree", "file-overlap"],
        default="merge-tree",
        help="real merge-tree conflicts (default) or the fast file-overlap fallback",
    )
    sp.add_argument(
        "--fixture",
        default=None,
        metavar="FILE",
        help="use a deterministic FakeHost from a JSON/YAML fixture instead of the network",
    )
    sp.add_argument(
        "--gate-check",
        default=DEFAULT_GATE_CHECK,
        help=f"required gate check name (default: {DEFAULT_GATE_CHECK})",
    )
    sp.add_argument(
        "--flaky-signatures",
        default=None,
        metavar="FILE",
        help="JSON/YAML file of {name_regex,text_regex} signatures marking a red as flaky",
    )
    sp.add_argument(
        "--outage-min-prs",
        type=_nonnegative_i64_arg,
        default=2,
        help="min PRs showing the gate-never-ran signature to declare a systemic outage",
    )


def build_parser() -> argparse.ArgumentParser:
    """Build the command-line parser without parsing process arguments."""

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

    plan_p = sub.add_parser(
        "plan",
        allow_abbrev=False,
        help="build the graph + CI/freshness fusion and print the PLAN",
        description="build the graph + CI/freshness fusion and print the PLAN",
    )
    _add_collect_flags(plan_p)
    plan_p.add_argument(
        "--freshness-max-behind",
        type=_nonnegative_i64_arg,
        default=DEFAULT_FRESHNESS_MAX_BEHIND,
        help="caller override (default: none): a green PR more than N commits behind => rebase-then-land",
    )
    plan_p.add_argument(
        "--priority-source",
        choices=["none", "labels", "command"],
        default="none",
        help="ordering priority source (default: none => size then age)",
    )
    plan_p.add_argument(
        "--priority-label-pattern",
        default=DEFAULT_LABEL_PATTERN,
        help="regex whose first group is the priority integer (for --priority-source labels)",
    )
    plan_p.add_argument(
        "--priority-cmd",
        default="",
        help="required shell command with {pr} that prints one integer (for --priority-source command)",
    )
    plan_p.add_argument(
        "--batch",
        action="store_true",
        help="also propose one green, conflict- and dependency-free batch (off by default)",
    )
    plan_p.add_argument(
        "--format", choices=["human", "json", "actions"], default="human", help="output format"
    )
    plan_p.add_argument(
        "--archive-dir",
        default=None,
        metavar="DIR",
        help=(
            "directory to archive the emitted plan JSON into so past plans are readable on disk "
            "(default: $PR_LANDING_PLANNER_ARCHIVE_DIR, else $XDG_STATE_HOME/pr-landing-planner/plans, "
            "else ~/.local/state/pr-landing-planner/plans; skipped for --fixture runs unless set here). "
            "The archived path is printed as a NOTE on stderr."
        ),
    )
    plan_p.add_argument(
        "--no-archive",
        action="store_true",
        help="do not archive the emitted plan to disk (archiving is on by default for live runs)",
    )

    graph_p = sub.add_parser(
        "graph", allow_abbrev=False, help="just the conflict/ordering graph view",
        description="just the conflict/ordering graph view",
    )
    _add_collect_flags(graph_p)
    graph_p.add_argument("--format", choices=["human", "json"], default="human", help="output format")

    clusters_p = sub.add_parser(
        "clusters",
        allow_abbrev=False,
        help="cluster PRs by shared conflict set into stack-land lanes (rebases-avoided)",
        description="cluster PRs by shared conflict set into stack-land lanes (rebases-avoided)",
    )
    _add_collect_flags(clusters_p)
    clusters_p.add_argument(
        "--format", choices=["human", "json"], default="human", help="output format"
    )

    status_p = sub.add_parser(
        "status", allow_abbrev=False, help="just per-PR CI/label health",
        description="just per-PR CI/label health",
    )
    _add_collect_flags(status_p)
    status_p.add_argument("--format", choices=["human", "json"], default="human", help="output format")
    status_p.add_argument(
        "--warn-threshold",
        type=_nonnegative_i64_arg,
        default=DEFAULT_WARN_THRESHOLD,
        help=f"warn above this open-PR count (default: {DEFAULT_WARN_THRESHOLD})",
    )

    qs = sub.add_parser(
        "quickstart", allow_abbrev=False, help="print a self-contained getting-started guide",
        description="print a self-contained getting-started guide",
    )
    qs.add_argument(
        "--emit-demo",
        action="store_true",
        help="print ONLY the demo fixture (pipe into --fixture) and exit",
    )
    return parser


# --------------------------------------------------------------------------- archiving
def _resolve_archive_dir(ns: argparse.Namespace) -> Path | None:
    """Resolve where an emitted plan should be archived, or ``None`` to skip archiving.

    Precedence: explicit ``--archive-dir`` > ``$PR_LANDING_PLANNER_ARCHIVE_DIR`` >
    ``$XDG_STATE_HOME/pr-landing-planner/plans`` > ``~/.local/state/pr-landing-planner/plans``.
    Archiving is skipped when ``--no-archive`` is passed, and for ``--fixture`` runs unless
    ``--archive-dir`` is explicit — that keeps the demo and the test suite side-effect-free and
    their stdout byte-deterministic.
    """
    if bool(getattr(ns, "no_archive", False)):
        return None
    explicit = getattr(ns, "archive_dir", None)
    if isinstance(explicit, str) and explicit:
        return Path(explicit).expanduser()
    fixture = getattr(ns, "fixture", None)
    if isinstance(fixture, str) and fixture:
        return None  # demo/test run: do not archive unless --archive-dir is explicit
    env_dir = os.environ.get("PR_LANDING_PLANNER_ARCHIVE_DIR")
    if env_dir:
        return Path(env_dir).expanduser()
    xdg = os.environ.get("XDG_STATE_HOME")
    base = Path(xdg).expanduser() if xdg else Path.home() / ".local" / "state"
    return base / "pr-landing-planner" / "plans"


def _archive_plan(ns: argparse.Namespace, result: PlanResult) -> None:
    """Write the canonical plan JSON to a durable, timestamped file and NOTE its path.

    The archived artifact is ALWAYS the full machine schema (:func:`render_json`), independent of
    the on-screen ``--format`` — so ``plan --format human`` still leaves a machine-readable record
    on disk. The path is printed to STDERR so stdout stays pure output. A write failure is a loud
    WARN, never fatal: archiving must not break planning. The UTC timestamp lives only in the
    FILENAME (not in ``render_json``), keeping the JSON body byte-deterministic.
    """
    directory = _resolve_archive_dir(ns)
    if directory is None:
        return
    graph = result.graph
    repo_slug = graph.repository.replace("/", "_") or "repo"
    base_slug = graph.base.replace("/", "_") or "base"
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S_%fZ")
    path = directory / f"plan-{repo_slug}-{base_slug}-{stamp}.json"
    try:
        directory.mkdir(parents=True, exist_ok=True)
        path.write_text(render_json(result) + "\n", encoding="utf-8")
    except OSError as exc:
        print(f"{PROG}: WARN: could not archive plan to {path}: {exc}", file=sys.stderr)
        return
    print(f"{PROG}: NOTE: plan archived to {path}", file=sys.stderr)


# --------------------------------------------------------------------------- commands
def _cmd_plan(ns: argparse.Namespace, c: Palette) -> int:
    try:
        result = _build_result(ns)
    except (CollectionError, FixtureError, HostCommandError, OSError, ValueError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    fmt = ns.format
    if fmt == "json":
        print(render_json(result))
    elif fmt == "actions":
        print(render_actions(result))
    else:
        print(render_human(result, color=c.style))
    _archive_plan(ns, result)
    return 0


def _cmd_graph(ns: argparse.Namespace, c: Palette) -> int:
    try:
        result = _build_result(ns)
    except (CollectionError, FixtureError, HostCommandError, OSError, ValueError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    if ns.format == "json":
        print(render_graph_json(result))
    else:
        print(render_graph_human(result, color=c.style))
    return 0


def _cmd_clusters(ns: argparse.Namespace, c: Palette) -> int:
    try:
        result = _build_result(ns)
    except (CollectionError, FixtureError, HostCommandError, OSError, ValueError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    if ns.format == "json":
        print(render_clusters_json(result))
    else:
        print(render_clusters_human(result, color=c.style))
    return 0


def _cmd_status(ns: argparse.Namespace, c: Palette) -> int:
    try:
        result = _build_result(ns)
    except (CollectionError, FixtureError, HostCommandError, OSError, ValueError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    if ns.format == "json":
        print(render_status_json(result))
    else:
        threshold = ns.warn_threshold if isinstance(ns.warn_threshold, int) else DEFAULT_WARN_THRESHOLD
        print(render_status_human(result, threshold, color=c.style))
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the command-line interface and return its process exit status."""

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
        if bool(getattr(ns, "emit_demo", False)):
            sys.stdout.write(_DEMO_FIXTURE)
            return 0
        print(_quickstart(c))
        return 0
    if command == "plan":
        return _cmd_plan(ns, c)
    if command == "graph":
        return _cmd_graph(ns, c)
    if command == "clusters":
        return _cmd_clusters(ns, c)
    if command == "status":
        return _cmd_status(ns, c)
    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())


__all__ = ["main", "build_parser", "PROG", "DEFAULT_WARN_THRESHOLD"]
