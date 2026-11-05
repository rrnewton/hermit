"""Per-project ``.herdr-run.yaml``: which workspace, which tab, and what may run.

The tool is generic; the POLICY is per-project. A project states its desired Herdr workspace name,
its tab-name schema, and its command allowlist in one tracked file next to its source, so the
allowlist is reviewable in that project's history rather than baked into this shared utility.

Every field has a working default, so a project with no config file at all still gets the intended
conservative behaviour (workspace ``agent-cmds``, one tab per agent, and only ``git``/``gh``,
optionally prefixed with ``with-proxy``). The file is read with herdr-run's own YAML-subset parser
(:mod:`herdr_run.yamlcore`), so there is no third-party dependency to be missing.

Every message in this module is part of a catalogue the native edition repeats word for word:
type names use the JSON vocabulary (``null``, ``bool``, ``number``, ``string``, ``array``,
``object``) because they name the configuration language rather than an implementation language,
values are quoted with :func:`herdr_run.textfmt.quote`, and keys are validated in
:data:`KNOWN_KEYS` order, each completely at its own position, so a file with two mistakes reports
the same first mistake in both editions.
"""

from __future__ import annotations

import math
import os
from dataclasses import dataclass, field, replace

from herdr_run.errors import ConfigError
from herdr_run.retention import MAX_RETENTION_DAYS
from herdr_run.textfmt import display, name_violation, os_error_text, quote
from herdr_run.yamlcore import YamlError, core_load

__all__ = [
    "ALLOW_ANY_PROGRAM",
    "CARGO_ALWAYS_DENIED",
    "CONFIG_FILENAMES",
    "DEFAULT_MAX_PANES",
    "KNOWN_KEYS",
    "MAX_PANE_CAP",
    "MAX_RETENTION_DAYS",
    "MAX_TIMEOUT_SECONDS",
    "Config",
    "MissingDefault",
    "describe_missing",
    "find_config_file",
    "load_config",
    "missing_defaults",
    "missing_defaults_warning",
    "parse_config_text",
    "read_config_file",
    "render_tab_name",
    "type_name",
    "whole_milliseconds",
]

#: Accepted config basenames, in search order, looked up from the working directory upward.
CONFIG_FILENAMES: tuple[str, ...] = (".herdr-run.yaml", ".herdr-run.yml")

#: The single ``allow`` entry that turns the allowlist off: any bare program name is admitted.
#:
#: This is a named mode rather than something to be worked out, because the alternative -- writing
#: out every program a project might ever want -- is the kind of list people abandon halfway and
#: then widen by accident. It is deliberately the only entry permitted when it is present: an
#: ``allow`` reading ``["*", "git"]`` looks narrower than it is.
#:
#: Everything except the program-name check still applies in this mode: terminal control characters
#: are still refused, the program must still be a bare name resolved from the pane's ``PATH``,
#: wrapper prefixes must still be declared, and every ``deny_*`` rule still bites.
ALLOW_ANY_PROGRAM = "*"

#: Default ceiling on how many panes the command workspace may hold before herdr-run replaces the
#: least-recently-used idle tab (or, when every tab is busy, refuses to open ANOTHER one). Every
#: agent that ever runs a command leaves a tab behind and nothing else closes it, so without a
#: ceiling the workspace grows for as long as agents are coined. The number is not
#: arbitrary: measured on devbig014 2026-08-10, a session with 260 panes drove the Herdr server to
#: >1000% CPU with every control call timing out (see ``client.SERVER_WORKER_THREADS``, which is the
#: in-tree mitigation for that same convoy). 32 keeps an eightfold margin below that and is far more
#: tabs than one project's agents legitimately need. Be honest about what the margin is worth,
#: though: the measurement is SERVER-scoped and this cap is WORKSPACE-scoped, so several projects
#: each configured with their own ``workspace`` and each sitting just under the cap reproduce the
#: measured condition without any of them breaching it. The cap bounds one project's contribution
#: to the leak, not the host's total, which is the other reason not to set it generously.
DEFAULT_MAX_PANES = 32

#: Largest accepted ``max_panes``. A shared finite bound keeps the two implementations identical and
#: keeps an absurd integer out of the comparison.
MAX_PANE_CAP = 1_000_000

#: Largest command/readiness timeout accepted from either configuration or the CLI. Keeping this
#: comfortably below platform ``Instant``/``time.monotonic`` limits means a finite value can never
#: overflow into an accidental infinite wait in either implementation.
MAX_TIMEOUT_SECONDS = 31_536_000.0

#: Programs allowed by default. Deliberately tiny: this is a sandbox door, not a shell. Cargo is
#: NOT a default: even dependency-oriented subcommands can execute configured rustc wrappers,
#: credential providers, or fetch helpers. Projects may explicitly opt it in, accepting that trust
#: widening; :data:`_DEFAULT_ALLOW_SUBCOMMAND` still fails closed against compiling subcommands.
_DEFAULT_ALLOW: tuple[str, ...] = ("git", "gh")

#: Wrapper programs that may precede an allowlisted program. A wrapper takes a command and execs it,
#: so allowing it as a PREFIX (never as a program in its own right) keeps wrapped commands
#: expressible without widening the allowlist to "anything this wrapper can exec".
_DEFAULT_PREFIXES: tuple[str, ...] = ("with-proxy",)

#: Options that make an otherwise-allowlisted program run arbitrary code. Git's are matched among
#: GLOBAL options preceding its subcommand; Cargo accepts its global options on either side, so its
#: entries are matched everywhere. ``git -c core.pager=...`` / ``git -c alias.x='!sh'`` /
#: ``git --exec-path=/tmp/evil`` all turn "run git" into "run anything".
#:
#: Defense in depth for cooperative callers, not a same-user security boundary. A determined caller
#: with `git` can still reach a lot; see the trust model in the user guide.
_DEFAULT_DENY_GLOBAL: dict[str, tuple[str, ...]] = {
    "git": ("-c", "--config-env", "--exec-path", "--namespace"),
    "gh": (),
    # These are Cargo's CLI configuration-mutation surfaces: `--config` can inject source
    # replacement, compilers, wrappers, and credential providers; `-Z` unlocks unstable behavior.
    # They are unnecessary for the dependency-oriented commands admitted after explicit opt-in.
    # Ordinary selection/output flags (`--manifest-path`, `--target`, `--registry`, feature and
    # lock flags) do not themselves name a process or mutate Cargo configuration. Ambient Cargo
    # configuration can still execute helpers, which is why Cargo is not in `_DEFAULT_ALLOW`.
    "cargo": ("--config", "-Z"),
}

#: Cargo options refused in every argument position whatever ``deny_global`` says, because a
#: project that opts Cargo in must not be able to configure its way past them. The admission rule
#: (:func:`herdr_run.allowlist.admit`) adds these to Cargo's ``deny_global`` entries, and
#: :func:`missing_defaults` therefore never reports them as left out.
CARGO_ALWAYS_DENIED: tuple[str, ...] = ("--config", "-Z")

#: Per-program subcommand ALLOWLIST. When a program appears here its subcommand MUST be in the
#: list; anything else -- including no subcommand at all -- is refused. This limits an explicit
#: ``cargo`` opt-in, but does not make Cargo a no-code-execution boundary: Cargo may invoke ambient
#: rustc wrappers, credential providers, or fetch helpers even during dependency resolution.
#:
#: WHY: the pane runs OUTSIDE the sandbox, and `cargo build`/`test`/`run` execute build scripts and
#: proc macros from freshly downloaded third-party crates. Allowing those would run untrusted code
#: outside the very confinement this tool exists to cross in a controlled way. The subcommands below
#: are dependency-resolution-oriented and do not request compilation. A project that deliberately
#: accepts Cargo's ambient helper execution can fetch through the door and then build in-jail with
#: `--offline` against the warm cache.
#:
#: A deny-list is the wrong shape here: `build`, `test`, `run`, `bench`, `install`, `rustc`,
#: `clippy`, `doc`, `miri` and every third-party `cargo-*` subcommand execute code, and a new one
#: can appear at any time.
_DEFAULT_ALLOW_SUBCOMMAND: dict[str, tuple[str, ...]] = {
    "cargo": ("fetch", "update", "generate-lockfile", "vendor", "metadata", "tree", "search"),
}

#: Subcommands (the first non-option token) that define or execute arbitrary code.
_DEFAULT_DENY_SUBCOMMAND: dict[str, tuple[str, ...]] = {
    "git": ("filter-branch", "daemon", "instaweb"),
    "gh": ("alias", "extension", "ext", "codespace", "cs"),
}

#: Options that are dangerous wherever they appear. ``--upload-pack``/``--receive-pack`` name a
#: program to execute for the "remote" side, which for a local path is simply local execution, and
#: ``--exec`` is the older spelling of the same thing (``git push --exec``, ``git ls-remote
#: --exec``, ``git fetch-pack --exec``, ``git send-pack --exec``) and also names the program
#: ``git archive --remote`` runs. Every rule also refuses an abbreviation of a long option it
#: names (``--upload-pac``), because Git's option parser accepts one.
_DEFAULT_DENY_ANYWHERE: tuple[str, ...] = ("--upload-pack", "--receive-pack", "--exec")

#: Options denied after one particular subcommand, where the same letters mean something harmless
#: elsewhere: ``git clone -u CMD`` is ``--upload-pack``, ``git rebase -x CMD`` runs ``CMD`` after
#: each commit, and ``git difftool -x CMD`` (``--extcmd``) runs ``CMD`` as the diff tool, while
#: ``git push -u``, ``git add -u``, and ``git cherry-pick -x`` are ordinary options that must stay
#: usable.
_DEFAULT_DENY_SUBCOMMAND_OPTION: dict[str, dict[str, tuple[str, ...]]] = {
    "git": {"clone": ("-u",), "difftool": ("-x", "--extcmd"), "rebase": ("-x",)},
}

#: Global options that CONSUME the following token as their value, when written space-separated.
#: Needed to find the subcommand correctly: in ``git -C /tmp/repo log``, ``/tmp/repo`` is a value,
#: not the subcommand, and misreading it would point every subcommand-level rule at the wrong token
#: -- ``git --attr-source HEAD daemon`` read ``HEAD`` as the subcommand and so ran a denied one.
#: Git's list is the set of top-level options its own ``handle_options`` (``git.c``) gives the
#: next argument to: ``-C``, ``-c``, ``--git-dir``, ``--work-tree``, ``--namespace``,
#: ``--config-env``, ``--super-prefix`` (removed from Git 2.41, still read by older Gits),
#: ``--shallow-file``, and ``--attr-source``. ``--exec-path`` takes its value only as
#: ``--exec-path=PATH`` and is listed for the sake of older configurations; it is denied anyway.
#:
#: This table records how each program reads its own command line, which is a fact about the
#: program and not a policy choice, so a configured ``value_options`` ADDS to it and can never take
#: an entry away (:func:`_with_builtin_value_options`). Leaving an entry out could only make the
#: policy misread a command -- and a file written by an older ``herdr-run init`` lists the older,
#: shorter table, which used to replace this one and so reopened ``git --attr-source HEAD daemon``.
_DEFAULT_VALUE_OPTIONS: dict[str, tuple[str, ...]] = {
    "git": (
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--exec-path",
        "--config-env",
        "--super-prefix",
        "--shallow-file",
        "--attr-source",
    ),
    "gh": ("-R", "--repo"),
    "cargo": ("--manifest-path", "--config", "-Z", "-p", "--package", "--target-dir"),
}

#: Process names accepted as "this pane is sitting at a shell prompt".
_DEFAULT_SHELLS: tuple[str, ...] = ("bash", "zsh", "sh", "dash", "fish", "ksh")


@dataclass(frozen=True)
class Config:
    """Resolved configuration. Immutable; CLI flags produce a modified copy via :func:`override`."""

    #: Herdr workspace LABEL (not id) that holds this project's command tabs.
    workspace: str = "agent-cmds"

    #: Tab-label schema. ``{agent}`` expands to the invoking agent name, ``{project}`` to the
    #: basename of the project root. Default: one tab per agent, e.g. ``release-agent``.
    tab_name: str = "{agent}"

    #: Working directory for commands, relative to the project root when not absolute.
    #: ``None`` leaves command execution at the caller's current directory; session bootstrap uses
    #: the project root when it needs a concrete initial directory.
    cwd: str | None = None

    allow: tuple[str, ...] = _DEFAULT_ALLOW
    prefixes: tuple[str, ...] = _DEFAULT_PREFIXES
    deny_global: dict[str, tuple[str, ...]] = field(
        default_factory=lambda: dict(_DEFAULT_DENY_GLOBAL)
    )
    deny_subcommand: dict[str, tuple[str, ...]] = field(
        default_factory=lambda: dict(_DEFAULT_DENY_SUBCOMMAND)
    )
    deny_anywhere: tuple[str, ...] = _DEFAULT_DENY_ANYWHERE
    #: Options denied after one subcommand: program -> subcommand -> options.
    deny_subcommand_option: dict[str, dict[str, tuple[str, ...]]] = field(
        default_factory=lambda: {
            program: dict(entries)
            for program, entries in _DEFAULT_DENY_SUBCOMMAND_OPTION.items()
        }
    )
    allow_subcommand: dict[str, tuple[str, ...]] = field(
        default_factory=lambda: dict(_DEFAULT_ALLOW_SUBCOMMAND)
    )
    #: The built-in table plus whatever the configuration adds; see :data:`_DEFAULT_VALUE_OPTIONS`.
    value_options: dict[str, tuple[str, ...]] = field(
        default_factory=lambda: dict(_DEFAULT_VALUE_OPTIONS)
    )

    #: Where run spools (stdout/stderr/exit-code files) and the audit log live, relative to the
    #: project root. MUST be a git-ignored path: it holds command OUTPUT, not source. Add the
    #: default ``.herdr-run/`` directory to each adopting project's ``.gitignore``.
    spool_dir: str = ".herdr-run"

    #: Seconds to wait for the command's exit-code file after launching it.
    timeout_seconds: float = 900.0

    #: Days of run spools to keep. Pruned when a new run is written; see :mod:`herdr_run.retention`.
    retention_days: int = 4

    #: Ceiling on panes in :attr:`workspace` before a NEW tab is refused. ``0`` disables the cap.
    #: Checked only when a tab has to be created: an agent that already has its tab is never locked
    #: out of it, because a cap that could break work in progress would be turned off immediately.
    max_panes: int = DEFAULT_MAX_PANES

    #: Seconds to wait for the pane to become idle before giving up with :class:`PaneBusy`.
    ready_timeout_seconds: float = 0.0

    #: Readiness policy. ``both`` (default) requires the process signal to say idle AND the prompt
    #: signal to not veto. ``process`` drops the prompt veto. See :mod:`herdr_run.readiness`.
    readiness: str = "both"

    #: Explicit prompt tail (the literal text a clean prompt line ends with, e.g. ``"$ "``). When
    #: ``None`` it is inferred from the shell rc file; inference failure means the prompt signal
    #: ABSTAINS rather than guesses.
    prompt_tail: str | None = None

    shells: tuple[str, ...] = _DEFAULT_SHELLS

    #: Remote used by the ``net-doctor`` smoke test. It only needs to be a reachable repository
    #: that the caller's own network policy blocks and the pane's does not.
    probe_remote: str = "https://github.com/git/git"

    #: How herdr control calls reach the server. ``direct`` uses the server's Unix socket;
    #: ``systemd-run`` brokers each call through a transient user unit for hosts where that socket
    #: is not reachable from the caller.
    broker: str = "direct"

    #: Absolute path of the config file this came from, or ``None`` for pure defaults.
    source_path: str | None = None

    #: Project root: the directory holding the config file, else the starting directory.
    project_root: str = "."

    def allows_any_program(self) -> bool:
        """Report whether ``allow`` has been set to the :data:`ALLOW_ANY_PROGRAM` wildcard."""
        return ALLOW_ANY_PROGRAM in self.allow


#: Every configuration key the parser accepts.
#:
#: Public because it is also the definition of "every knob", which the generated ``.herdr-run.yaml``
#: has to cover for the user guide to be able to point at that file instead of restating it.
KNOWN_KEYS: tuple[str, ...] = (
    "workspace",
    "tab_name",
    "cwd",
    "allow",
    "prefixes",
    "deny_global",
    "deny_subcommand",
    "deny_anywhere",
    "deny_subcommand_option",
    "allow_subcommand",
    "value_options",
    "spool_dir",
    "timeout_seconds",
    "retention_days",
    "max_panes",
    "ready_timeout_seconds",
    "readiness",
    "prompt_tail",
    "shells",
    "broker",
    "probe_remote",
)



def find_config_file(start: str) -> str | None:
    """Search ``start`` and each ancestor for a config file, returning the first hit.

    Nearest-wins, like ``.gitignore`` or ``.editorconfig``: a slot worktree can carry its own policy
    without the parent's file leaking in, and a subdirectory inherits its project's file.
    """
    current = _absolute(start)
    while True:
        for name in CONFIG_FILENAMES:
            candidate = os.path.join(current, name)
            if os.path.isfile(candidate):
                return candidate
        parent = os.path.dirname(current)
        if parent == current:
            return None
        current = parent


def _absolute(path: str) -> str:
    """``os.path.abspath`` with a POSIX ``//`` prefix collapsed to ``/``.

    POSIX lets a path that starts with exactly two slashes mean something implementation-defined,
    so ``abspath`` keeps them. On Linux the two spellings are the same directory, and every path
    derived from the project root (spool, audit log, session cache) must print the same way in
    both editions.
    """
    absolute = os.path.abspath(path)
    if absolute.startswith("//") and not absolute.startswith("///"):
        absolute = absolute[1:]
    return absolute


def type_name(value: object) -> str:
    """Name a decoded configuration value's type in the JSON vocabulary."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


def whole_milliseconds(value: float) -> bool:
    """True when ``value`` is a whole number of milliseconds; finer waits are meaningless."""
    return round(value * 1000) / 1000 == value


def _object(raw: object, what: str) -> dict[str, object]:
    if not isinstance(raw, dict):
        raise ConfigError(f"{what}: expected an object, got {type_name(raw)}")
    return raw


def _array(raw: object, what: str) -> list[object]:
    if not isinstance(raw, list):
        raise ConfigError(f"{what}: expected an array, got {type_name(raw)}")
    return raw


def _text(raw: object, what: str) -> str:
    if not isinstance(raw, str):
        raise ConfigError(f"{what}: must be a string, got {type_name(raw)}")
    _reject_controls(raw, what)
    return raw


def _reject_controls(value: str, what: str) -> None:
    if any(
        ord(char) < 32 or 127 <= ord(char) <= 159 or 0xD800 <= ord(char) <= 0xDFFF
        for char in value
    ):
        raise ConfigError(f"{what}: control characters and Unicode surrogates are not allowed")


def _policy_name(raw: object, what: str) -> str:
    """A name the policy compares with typed text: a program, a wrapper, an option, a subcommand,
    a workspace, a tab schema, or a shell.

    Besides control characters it may hold no invisible character -- a format character such as a
    zero-width space, a byte-order mark, or a bidirectional override, or a non-ASCII space such as
    U+00A0 or U+2028 -- because a name holding one looks exactly like the name without it, and a
    deny entry that looks like ``-c`` but is not would never match the ``-c`` it appears to deny.
    Nor may it hold a noncharacter, a private-use character, or a code point Unicode 16.0 leaves
    unassigned, which draw as whatever the font in use makes of them, often nothing
    (:func:`herdr_run.textfmt.name_violation`).
    """
    value = _text(raw, what)
    if not value:
        raise ConfigError(f"{what}: must not be empty")
    violation = name_violation(value)
    if violation is not None:
        raise ConfigError(
            f"{what}: {quote(value)} contains {violation}; a name may not contain one, "
            "because it would look like a name it is not"
        )
    return value


def _str_tuple(raw: object, what: str) -> tuple[str, ...]:
    items = _array(raw, what)
    out: list[str] = []
    for item in items:
        if not isinstance(item, str):
            raise ConfigError(f"{what}: every entry must be a string, got {type_name(item)}")
        out.append(_policy_name(item, f"{what}[{len(out)}]"))
    return tuple(out)


def _str_tuple_map(raw: object, what: str) -> dict[str, tuple[str, ...]]:
    mapping = _object(raw, what)
    out: dict[str, tuple[str, ...]] = {}
    for key, value in mapping.items():
        _policy_name(key, f"{what} key")
        out[key] = _str_tuple(value, f"{what}.{key}")
    return out


def _str_tuple_map_map(raw: object, what: str) -> dict[str, dict[str, tuple[str, ...]]]:
    mapping = _object(raw, what)
    out: dict[str, dict[str, tuple[str, ...]]] = {}
    for key, value in mapping.items():
        _policy_name(key, f"{what} key")
        out[key] = _str_tuple_map(value, f"{what}.{key}")
    return out


def _seconds(raw: object, what: str) -> float:
    if isinstance(raw, bool) or not isinstance(raw, (int, float)):
        raise ConfigError(f"{what}: must be a number, got {type_name(raw)}")
    if isinstance(raw, float) and not math.isfinite(raw):
        raise ConfigError(f"{what}: must be finite")
    if raw < 0:
        raise ConfigError(f"{what}: must not be negative")
    if raw > MAX_TIMEOUT_SECONDS:
        raise ConfigError(f"{what}: must not exceed {MAX_TIMEOUT_SECONDS:.0f} seconds")
    value = float(raw)
    if not whole_milliseconds(value):
        raise ConfigError(f"{what}: must be a whole number of milliseconds")
    return value


def _bounded_count(raw: object, what: str, limit: int, unit: str) -> int:
    if isinstance(raw, bool) or not isinstance(raw, int) or raw < 0:
        raise ConfigError(f"{what}: must be a non-negative integer, got {type_name(raw)}")
    if raw > limit:
        raise ConfigError(f"{what}: must not exceed {limit}{unit}")
    return raw


def _choice(raw: object, what: str, allowed: tuple[str, ...]) -> str:
    value = _text(raw, what)
    if value not in allowed:
        raise ConfigError(f"{what}: must be one of {', '.join(allowed)}; got {quote(value)}")
    return value


def _optional_text(raw: object, what: str) -> str | None:
    return None if raw is None else _text(raw, what)


def _template_error(schema: str) -> str | None:
    """Describe what is wrong with a tab-name schema, or return ``None`` when it is valid.

    The schema is literal text plus ``{{``, ``}}``, ``{agent}``, and ``{project}``. A ``{`` with
    no closing brace, or a lone ``}``, is malformed; any other ``{...}`` field is a placeholder this
    tool does not offer.
    """
    index = 0
    while index < len(schema):
        character = schema[index]
        if character == "{":
            if schema.startswith("{{", index):
                index += 2
                continue
            close = schema.find("}", index + 1)
            if close < 0:
                return "malformed: unmatched '{'"
            if schema[index + 1 : close] not in ("agent", "project"):
                return "placeholders"
            index = close + 1
            continue
        if character == "}":
            if schema.startswith("}}", index):
                index += 2
                continue
            return "malformed: unmatched '}'"
        index += 1
    return None


def _check_template(schema: str, what: str) -> None:
    problem = _template_error(schema)
    if problem == "placeholders":
        raise ConfigError(
            f"{what}: schema {quote(schema)} may use only plain {{agent}} and {{project}} "
            "placeholders"
        )
    if problem is not None:
        raise ConfigError(f"{what}: schema {quote(schema)} is {problem}")


def render_tab_name(schema: str, *, agent: str, project: str) -> str:
    """Render a tab schema restricted to literal text plus ``{agent}``/``{project}``."""
    _check_template(schema, "tab_name")
    out: list[str] = []
    index = 0
    while index < len(schema):
        if schema.startswith("{{", index):
            out.append("{")
            index += 2
        elif schema.startswith("}}", index):
            out.append("}")
            index += 2
        elif schema[index] == "{":
            close = schema.index("}", index + 1)
            out.append(agent if schema[index + 1 : close] == "agent" else project)
            index = close + 1
        else:
            out.append(schema[index])
            index += 1
    rendered = "".join(out)
    _reject_controls(rendered, "rendered tab_name")
    if not rendered:
        raise ConfigError("rendered tab_name: must not be empty")
    return rendered


def _with_builtin_value_options(
    configured: dict[str, tuple[str, ...]],
) -> dict[str, tuple[str, ...]]:
    """The built-in value-option table with ``configured`` added to it; nothing is taken away.

    Each program keeps its built-in options first, in their built-in order, followed by the
    configured ones it did not already have, in the order they were written; a program the table
    does not know gets its configured options with repeats dropped.
    """
    merged = dict(_DEFAULT_VALUE_OPTIONS)
    for program, options in configured.items():
        combined = list(merged.get(program, ()))
        for option in options:
            if option not in combined:
                combined.append(option)
        merged[program] = tuple(combined)
    return merged


def _parse_config(
    document: object, *, source_path: str | None, project_root: str
) -> Config:
    """Build a :class:`Config` from an already-decoded YAML document.

    Split out from :func:`load_config` so the whole validation surface is testable without touching
    the filesystem.
    """
    if document is None:
        return Config(source_path=source_path, project_root=project_root)

    # The path heads every message below; `display` keeps a control character in a directory name
    # from reaching the terminal raw.
    what = display(source_path) if source_path else "<config>"
    mapping = _object(document, what)

    known = set(KNOWN_KEYS)
    unknown = sorted(set(mapping) - known)
    if unknown:
        # Reject rather than ignore: a typo'd `allowlist:` key silently falling back to the default
        # allowlist is exactly the kind of quiet policy failure this tool must not have.
        raise ConfigError(
            f"{what}: unknown key(s): {', '.join(quote(key) for key in unknown)}. "
            f"Known keys: {', '.join(sorted(known))}"
        )

    config = Config(source_path=source_path, project_root=project_root)
    for key in KNOWN_KEYS:
        if key not in mapping:
            continue
        raw = mapping[key]
        where = f"{what}.{key}"
        if key == "workspace":
            config = replace(config, workspace=_policy_name(raw, where))
        elif key == "tab_name":
            schema = _policy_name(raw, where)
            _check_template(schema, where)
            config = replace(config, tab_name=schema)
        elif key == "cwd":
            config = replace(config, cwd=_optional_text(raw, where))
        elif key == "allow":
            config = replace(config, allow=_str_tuple(raw, where))
        elif key == "prefixes":
            config = replace(config, prefixes=_str_tuple(raw, where))
        elif key == "deny_global":
            config = replace(config, deny_global=_str_tuple_map(raw, where))
        elif key == "deny_subcommand":
            config = replace(config, deny_subcommand=_str_tuple_map(raw, where))
        elif key == "deny_anywhere":
            config = replace(config, deny_anywhere=_str_tuple(raw, where))
        elif key == "deny_subcommand_option":
            config = replace(config, deny_subcommand_option=_str_tuple_map_map(raw, where))
        elif key == "allow_subcommand":
            config = replace(config, allow_subcommand=_str_tuple_map(raw, where))
        elif key == "value_options":
            config = replace(
                config, value_options=_with_builtin_value_options(_str_tuple_map(raw, where))
            )
        elif key == "spool_dir":
            config = replace(config, spool_dir=_text(raw, where))
        elif key == "timeout_seconds":
            config = replace(config, timeout_seconds=_seconds(raw, where))
        elif key == "retention_days":
            config = replace(
                config, retention_days=_bounded_count(raw, where, MAX_RETENTION_DAYS, " days")
            )
        elif key == "max_panes":
            config = replace(config, max_panes=_bounded_count(raw, where, MAX_PANE_CAP, ""))
        elif key == "ready_timeout_seconds":
            config = replace(config, ready_timeout_seconds=_seconds(raw, where))
        elif key == "readiness":
            config = replace(config, readiness=_choice(raw, where, ("both", "process")))
        elif key == "prompt_tail":
            config = replace(config, prompt_tail=_optional_text(raw, where))
        elif key == "shells":
            config = replace(config, shells=_str_tuple(raw, where))
        elif key == "broker":
            config = replace(config, broker=_choice(raw, where, ("direct", "systemd-run")))
        elif key == "probe_remote":
            config = replace(config, probe_remote=_text(raw, where))

    if not config.allow:
        raise ConfigError(
            f"{what}.allow: refusing an EMPTY allowlist — no command could ever run"
        )
    if config.allows_any_program() and len(config.allow) > 1:
        raise ConfigError(
            f'{what}.allow: "{ALLOW_ANY_PROGRAM}" already admits every program, so it must be the '
            "only entry; listing programs beside it makes the policy look narrower than it is"
        )
    if "cargo" in config.allow and "cargo" not in config.allow_subcommand:
        raise ConfigError(
            f"{what}.allow_subcommand: cargo is allowed but has no positive subcommand list"
        )
    return config


def parse_config(
    document: object, *, source_path: str | None, project_root: str
) -> Config:
    """Validate a decoded configuration document."""
    return _parse_config(document, source_path=source_path, project_root=project_root)


def _lossy(path: str) -> str:
    """A path for a message, with bytes that are not UTF-8 shown as U+FFFD."""
    return os.fsencode(path).decode("utf-8", errors="replace")


def _has_surrogates(value: str) -> bool:
    return any(0xD800 <= ord(character) <= 0xDFFF for character in value)


def load_config(*, explicit_path: str | None, start_dir: str) -> Config:
    """Resolve the effective config: explicit path, else nearest ancestor file, else defaults."""
    start = _absolute(start_dir)
    if _has_surrogates(start):
        # Paths derived from this directory name the spool, the audit log, and the session cache.
        # Converting it lossily would retarget those writes into a different directory.
        raise ConfigError(f"current directory is not valid UTF-8: {quote(_lossy(start))}")
    path = explicit_path
    if path is not None:
        if not os.path.isfile(path):
            raise ConfigError(f"config file not found: {display(path)}")
        path = _absolute(path)
    else:
        path = find_config_file(start)

    if path is None:
        return Config(source_path=None, project_root=start)

    project_root = os.path.dirname(path) or "/"
    return parse_config_text(read_config_file(path), source_path=path, project_root=project_root)


def read_config_file(path: str) -> str:
    """The text of the configuration file at ``path``, which must be UTF-8."""
    try:
        with open(path, "rb") as handle:
            payload = handle.read()
    except OSError as exc:
        raise ConfigError(
            f"cannot read config file {display(path)}: {os_error_text(exc)}"
        ) from exc
    try:
        return payload.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ConfigError(
            f"cannot read config file {display(path)}: the file is not valid UTF-8"
        ) from exc


def parse_config_text(text: str, *, source_path: str, project_root: str) -> Config:
    """Read configuration text as the file ``source_path`` holds it, and validate it."""
    try:
        document = core_load(text)
    except YamlError as exc:
        raise ConfigError(f"{display(source_path)}: invalid YAML: {exc}") from exc
    return parse_config(document, source_path=source_path, project_root=project_root)


@dataclass(frozen=True)
class MissingDefault:
    """Built-in deny entries that one configured list leaves out, and so does not enforce."""

    #: The list, as a dotted path: ``deny_anywhere``, ``deny_global.git``,
    #: ``deny_subcommand_option.git.clone``.
    setting: str
    #: The entries it leaves out, in built-in order.
    entries: tuple[str, ...]


def missing_defaults(config: Config) -> tuple[MissingDefault, ...]:
    """Every built-in deny entry the configuration's deny lists leave out.

    A configured deny list REPLACES the built-in one, so a list written before an entry was added
    to the built-in defaults -- by an older ``herdr-run init``, say -- goes on without it, and
    nothing about the file says so. This names each such entry, so it can be said out loud.

    Entries for a program the allowlist can never admit are not reported, because they could never
    apply; nor are Cargo's :data:`CARGO_ALWAYS_DENIED`, which apply whatever the file says. Lists
    are reported in :data:`KNOWN_KEYS` order, programs and subcommands in sorted order, and entries
    in built-in order.
    """
    defaults = Config()
    found: list[MissingDefault] = []

    def admissible(program: str) -> bool:
        return config.allows_any_program() or program in config.allow

    def compare(
        setting: str,
        built_in: tuple[str, ...],
        configured: tuple[str, ...],
        exempt: tuple[str, ...] = (),
    ) -> None:
        absent = tuple(
            entry for entry in built_in if entry not in configured and entry not in exempt
        )
        if absent:
            found.append(MissingDefault(setting, absent))

    for program in sorted(defaults.deny_global):
        if admissible(program):
            compare(
                f"deny_global.{program}",
                defaults.deny_global[program],
                config.deny_global.get(program, ()),
                CARGO_ALWAYS_DENIED if program == "cargo" else (),
            )
    for program in sorted(defaults.deny_subcommand):
        if admissible(program):
            compare(
                f"deny_subcommand.{program}",
                defaults.deny_subcommand[program],
                config.deny_subcommand.get(program, ()),
            )
    compare("deny_anywhere", defaults.deny_anywhere, config.deny_anywhere)
    for program in sorted(defaults.deny_subcommand_option):
        if not admissible(program):
            continue
        configured = config.deny_subcommand_option.get(program, {})
        for subcommand in sorted(defaults.deny_subcommand_option[program]):
            compare(
                f"deny_subcommand_option.{program}.{subcommand}",
                defaults.deny_subcommand_option[program][subcommand],
                configured.get(subcommand, ()),
            )
    return tuple(found)


def describe_missing(missing: tuple[MissingDefault, ...]) -> str:
    """``deny_anywhere "--exec"; deny_global.git "-c", "--namespace"``: one line, every entry."""
    return "; ".join(
        f"{item.setting} {', '.join(quote(entry) for entry in item.entries)}" for item in missing
    )


def missing_defaults_warning(config: Config) -> str | None:
    """The one warning line every subcommand that reads ``config`` prints, or ``None``.

    The line is the same every time for the same file, so it can be recognised. It names the file,
    every left-out entry, and the remedy: ``herdr-run init --upgrade`` run where the file is, when
    the file has a name ``init`` works on, and otherwise an edit by hand.
    """
    missing = missing_defaults(config)
    if not missing:
        return None
    source = config.source_path
    shown = display(source) if source is not None else "<config>"
    if source is not None and os.path.basename(source) in CONFIG_FILENAMES:
        remedy = (
            f"run 'herdr-run init --upgrade' in {display(config.project_root)} "
            "or add them by hand"
        )
    else:
        remedy = "add them to the file by hand"
    return (
        f"herdr-run: WARNING: {shown} leaves out built-in deny entries, so they are not "
        f"enforced: {describe_missing(missing)}. To adopt them, {remedy}."
    )
