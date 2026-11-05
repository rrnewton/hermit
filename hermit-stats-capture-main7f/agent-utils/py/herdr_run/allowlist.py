"""Apply cooperative command policy and render an admitted argument vector safely.

Two separate jobs, deliberately not conflated:

1. **Admission** — is this program on the project's allowlist? Answered on the SPLIT argv, never on
   the raw string, so quoting tricks cannot change which program the policy thinks it is admitting.
2. **Rendering** — turn the admitted argv back into a shell word list. Every token is passed through
   :func:`shlex.quote`, so the rendered command contains exactly the argv that was admitted. This is
   why there is no metacharacter blocklist here: injection is prevented by CONSTRUCTION rather than
   by trying to enumerate ``;``, ``&&``, backticks, ``$()``, newlines, and so on. A blocklist of
   shell metacharacters is a proxy for "cannot start a second command"; re-quoting is the property
   itself.

A leading ``~`` in an argument is expanded BEFORE the policy looks at it, so the policy judges the
argument exactly as it will run: ``git ~ foo`` with ``HOME=--exec-path=/tmp/evil`` is judged as
``git --exec-path=/tmp/evil foo`` and refused, rather than admitted as a harmless-looking path and
then executed as an option. The wrapper prefixes and the program are never expanded; a program
spelled with a leading ``~`` is refused outright.

The remaining privilege is whatever the allowlisted program can do on its own. The user guide also
explains why same-user access to Herdr makes this a safety rail rather than a containment boundary.
"""

from __future__ import annotations

import os
import pwd
import shlex
from dataclasses import dataclass

from herdr_run.config import CARGO_ALWAYS_DENIED, Config
from herdr_run.errors import ConfigError, Refused
from herdr_run.textfmt import quote

__all__ = [
    "Admission",
    "admit",
    "expand_tilde",
    "quote_word",
    "reject_terminal_controls",
    "render",
]

#: Stands for "read ``HOME`` from this process's environment" where a caller may also pass a value.
_FROM_ENVIRONMENT = object()


def reject_terminal_controls(value: str, what: str) -> None:
    """Refuse bytes a terminal driver can act on before shell quoting has any effect."""
    for char in value:
        codepoint = ord(char)
        if codepoint < 32 or 127 <= codepoint <= 159:
            raise Refused(
                f"{what} contains terminal control U+{codepoint:04X}; control characters cannot "
                "be injected into a shared pane"
            )


@dataclass(frozen=True)
class Admission:
    """A command the policy accepted, with the reasoning kept for the audit record."""

    #: Full argv as split, including any allowed prefix wrappers, before ``~`` expansion.
    argv: tuple[str, ...]
    #: The allowed wrapper chain that preceded the program (e.g. ``("with-proxy",)``).
    prefix: tuple[str, ...]
    #: The allowlisted program itself (e.g. ``"git"``).
    program: str
    #: The program's first non-option argument, when it has one (e.g. ``"ls-remote"``).
    subcommand: str | None
    #: Validated shell text safe to embed in the POSIX result-capture wrapper: the argv the policy
    #: judged, after ``~`` expansion, each token quoted.
    rendered: str


def _has_surrogates(value: str) -> bool:
    return any(0xD800 <= ord(character) <= 0xDFFF for character in value)


def _account_home_text(user: str | None) -> str | None:
    """The account database's home for ``user`` (the current uid when ``None``), if usable."""
    try:
        record = pwd.getpwuid(os.getuid()) if user is None else pwd.getpwnam(user)
    except (KeyError, OSError, ValueError):
        return None
    home = record.pw_dir
    if _has_surrogates(home):
        return None
    return home


def expand_tilde(token: str, home: object = _FROM_ENVIRONMENT) -> str:
    """Expand a LEADING ``~`` / ``~user`` in one token, the way a shell would.

    Quoting every token is what makes injection impossible, but it also suppresses the one
    expansion callers reasonably expect: ``git -C ~/work/repo`` arrived at git as a literal ``~``
    and failed with ``cannot change to '~/work/repo'``. Doing the expansion HERE keeps the security
    property intact -- the result is still quoted, so the shell never gets a chance to interpret
    anything.

    The rules, identical in both editions: ``~`` uses ``HOME`` when it is set (refusing a ``HOME``
    that is not valid UTF-8, because a lossy conversion would name a different directory), and the
    account database otherwise; ``~user`` asks the account database. A lookup that fails, or a home
    that is not valid UTF-8, leaves the token literal. Trailing slashes are dropped from the home,
    and an empty home expands to the remainder of the token, or ``/``. A tilde anywhere but the
    start is an ordinary character.

    ``home`` is the value of ``HOME`` to use (``None`` when it is unset); by default it is read from
    this process's environment.
    """
    if not token.startswith("~"):
        return token
    slash = token.find("/")
    if slash < 0:
        slash = len(token)
    user = token[1:slash]
    suffix = token[slash:]
    resolved: str | None
    if not user:
        value = os.environ.get("HOME") if home is _FROM_ENVIRONMENT else home
        if value is None:
            resolved = _account_home_text(None)
        elif not isinstance(value, str):
            raise TypeError("home must be a string or None")
        elif _has_surrogates(value):
            lossy = os.fsencode(value).decode("utf-8", errors="replace")
            raise ConfigError(f"HOME is not valid UTF-8: {quote(lossy)}")
        else:
            resolved = value
    else:
        resolved = _account_home_text(user)
    if resolved is None:
        return token
    trimmed = resolved.rstrip("/")
    if not trimmed:
        return suffix or "/"
    return trimmed + suffix


def quote_word(token: str) -> str:
    """Quote one word for a POSIX shell, by the rule both editions share.

    A non-empty word made only of ASCII letters, digits, and ``_@%+=:,./-`` is returned unchanged;
    anything else is wrapped in single quotes, with each ``'`` written as ``'"'"'``.
    """
    return shlex.quote(token)


def render(argv: tuple[str, ...], *, home: object = _FROM_ENVIRONMENT) -> str:
    """Expand a leading ``~`` in every token and quote each, refusing terminal controls.

    :func:`admit` does not call this: it expands only the arguments after the program, and judges
    them expanded. This is the same rendering for an argv that needs no policy.
    """
    rendered: list[str] = []
    for token in argv:
        expanded = expand_tilde(token, home)
        reject_terminal_controls(expanded, "command argument")
        rendered.append(quote_word(expanded))
    return " ".join(rendered)


def _split(command: str) -> tuple[str, ...]:
    try:
        # comments=False: a '#' is a literal argument character here (e.g. `gh issue view '#12'`),
        # not a comment introducer. Letting shlex strip from '#' onward would silently TRUNCATE a
        # command after admission, so the audited text and the executed text would disagree.
        tokens = shlex.split(command, comments=False, posix=True)
    except ValueError as exc:
        raise Refused(f"cannot parse command (unbalanced quoting?): {exc}") from exc
    if not tokens:
        raise Refused("empty command")
    return tuple(tokens)


def _matches_denied_option(token: str, denied: str) -> bool:
    """Whether ``token`` spells the denied option ``denied``. Every ``deny_*`` option rule uses this.

    Three spellings match:

    * the option itself, or with an attached value: ``--upload-pack``, ``--upload-pack=CMD``;
    * for a long option (``--`` and at least one more character), any abbreviation of it, with or
      without a value: ``--upload-pac=CMD``, ``--u``. Git's option parser accepts any prefix that
      is unambiguous among the options of the subcommand at hand, and which prefixes those are
      depends on the subcommand and the Git version, so every prefix is refused. A full option
      that merely starts the same way (``--exec-path`` against ``--exec``) is a different option;
    * for a one-letter short option, the letter anywhere in a short-option cluster: ``-qu/tmp/x``,
      ``-Zfoo``, ``-Z=foo``, ``-qZfoo``. Git and Cargo both read clusters and attached values, so
      looking only at ``token.split("=")[0]`` would leave a bypass. This is intentionally
      fail-closed: a value that starts with ``-`` and holds the letter is refused too.
    """
    if token == denied or token.startswith(f"{denied}="):
        return True
    if denied.startswith("--"):
        name = token.split("=", 1)[0]
        return len(denied) > 2 and len(name) > 2 and name.startswith("--") and denied.startswith(name)
    return (
        len(denied) == 2
        and denied.startswith("-")
        and not denied.startswith("--")
        and token.startswith("-")
        and not token.startswith("--")
        and denied[1] in token[1:]
    )


def admit(command: str, config: Config, *, home: object = _FROM_ENVIRONMENT) -> Admission:
    """Admit ``command`` under ``config``, or raise :class:`Refused` explaining exactly why.

    Raises before any pane is touched, so a refusal never leaves partial state anywhere. Every
    argument after the program is ``~``-expanded first (``home`` as for :func:`expand_tilde`), and
    every rule below judges the expanded argument, which is exactly what is rendered and run.
    """
    reject_terminal_controls(command, "command")
    argv = _split(command)
    for token in argv:
        reject_terminal_controls(token, "command argument")

    # Peel allowed wrapper prefixes. Each wrapper may appear at most once, so `with-proxy with-proxy
    # ... ` cannot be used to pad an argv into a shape the later checks read differently.
    prefix: list[str] = []
    rest = list(argv)
    while rest and rest[0] in config.prefixes:
        wrapper = rest.pop(0)
        if wrapper in prefix:
            raise Refused(f"prefix {quote(wrapper)} repeated; each wrapper may appear at most once")
        prefix.append(wrapper)

    if not rest:
        raise Refused(
            f"command is only wrapper prefixes ({' '.join(prefix)}) with no program. "
            f"Allowed programs: {', '.join(sorted(config.allow))}"
        )

    program = rest[0]

    # A bare name only. An explicit path (`/bin/sh`, `./git`, `../x/gh`) would let any binary be
    # presented under an allowlisted-looking basename, so the name must be resolved by the pane's
    # PATH rather than chosen by the caller.
    # A leading `~` counts as a path too: rendering expands it (`~root` becomes `/root`), so it
    # would pass this check as a name and run as a path.
    if "/" in program or program.startswith("~"):
        raise Refused(
            f"program must be a bare command name resolved from PATH, not a path: {quote(program)}"
        )

    # The wildcard turns off THIS check and nothing else: control characters, the bare-name rule
    # above, the wrapper-prefix rule, every `deny_*` rule, and the positive subcommand lists below
    # all still apply.
    if not config.allows_any_program() and program not in config.allow:
        allowed = ", ".join(sorted(config.allow))
        hint = ""
        if program in config.prefixes:
            hint = f" ({quote(program)} is a wrapper prefix, not a program)"
        raise Refused(f"program {quote(program)} is not allowlisted{hint}. Allowed: {allowed}")

    # Expanded BEFORE any rule reads them: `git ~ foo` with HOME=daemon is `git daemon foo`, and
    # must be judged as that. A HOME that is not valid UTF-8 is a configuration error, and a home
    # holding a control character is refused like any other argument that holds one.
    args: list[str] = []
    for token in rest[1:]:
        expanded = expand_tilde(token, home)
        reject_terminal_controls(expanded, "command argument")
        args.append(expanded)

    for token in args:
        # `--upload-pack=/tmp/evil`, the space-separated `--upload-pack /tmp/evil`, and the
        # abbreviation `--upload-pac=/tmp/evil` all name the option.
        if any(_matches_denied_option(token, denied) for denied in config.deny_anywhere):
            raise Refused(
                f"option {quote(token)} is denied: it names a program for {program} to execute"
            )

    # Git global options are the tokens BEFORE the first non-option token; that is where git puts
    # config/exec-path switches that turn it into an arbitrary-code runner. Cargo is scanned across
    # the full argv below because it accepts global flags after the subcommand too.
    deny_global = config.deny_global.get(program, ())
    value_options = config.value_options.get(program, ())

    # Cargo's "global" flags are accepted on EITHER side of the subcommand. Scan the entire argv
    # before finding the subcommand so `cargo fetch --config ...` and attached/clustered `-Z` forms
    # cannot bypass the same checks that reject `cargo --config ... fetch`.
    if program == "cargo":
        cargo_denied = tuple(dict.fromkeys((*deny_global, *CARGO_ALWAYS_DENIED)))
        for token in args:
            denied = next(
                (option for option in cargo_denied if _matches_denied_option(token, option)), None
            )
            if denied is not None:
                raise Refused(
                    f"option {quote(denied)} is denied everywhere for cargo: it can make cargo execute "
                    "arbitrary code"
                )

    subcommand: str | None = None
    subcommand_index = len(args)
    expect_value = False
    for index, token in enumerate(args):
        if expect_value:
            # This token is the VALUE of the preceding option (`git -C <path>`), not the
            # subcommand. Without this, `git -C /tmp/repo log` would read `/tmp/repo` as the
            # subcommand and every subcommand-level rule would be looking at the wrong token.
            expect_value = False
            continue
        if not token.startswith("-"):
            subcommand = token
            subcommand_index = index
            break
        base = token.split("=", 1)[0]
        denied = next(
            (option for option in deny_global if _matches_denied_option(token, option)), None
        )
        if denied is not None:
            raise Refused(
                f"global option {quote(denied)} is denied for {program}: it can make {program} execute "
                "arbitrary code"
            )
        if base in value_options and "=" not in token:
            expect_value = True

    # A per-program subcommand ALLOWLIST, when present, is fail-closed: it is checked before the
    # deny-list and refuses anything not named, including a bare program with no subcommand. This
    # is what makes `cargo` admissible at all -- `cargo fetch` only downloads, while `cargo build`
    # would execute build scripts from third-party crates OUTSIDE the sandbox.
    allowed_subcommands = config.allow_subcommand.get(program)
    if program == "cargo" and allowed_subcommands is None:
        raise Refused(
            "cargo requires an explicit allow_subcommand entry; omitting its positive list would "
            "silently admit compilation-oriented and unknown subcommands"
        )
    if allowed_subcommands is not None:
        if subcommand is None:
            raise Refused(
                f"{program} requires a subcommand, and only these are allowed: "
                f"{', '.join(sorted(allowed_subcommands))}"
            )
        if subcommand not in allowed_subcommands:
            raise Refused(
                f"subcommand {quote(program + ' ' + subcommand)} is not allowlisted. Allowed: "
                f"{', '.join(sorted(allowed_subcommands))}. "
                f"{program} compilation-oriented and unknown subcommands are deliberately "
                "excluded. Cargo must also be explicitly allowlisted because even dependency "
                "commands may execute ambiently configured helpers."
            )

    if subcommand is not None and subcommand in config.deny_subcommand.get(program, ()):
        raise Refused(
            f"subcommand {quote(program + ' ' + subcommand)} is denied: it defines or runs "
            "arbitrary code"
        )

    # Options that are harmless elsewhere but name a program after this one subcommand
    # (`git clone -u CMD`). Every token after the subcommand is judged.
    if subcommand is not None:
        scoped = config.deny_subcommand_option.get(program, {}).get(subcommand, ())
        for token in args[subcommand_index + 1 :]:
            if any(_matches_denied_option(token, denied) for denied in scoped):
                raise Refused(
                    f"option {quote(token)} is denied for {quote(program + ' ' + subcommand)}: "
                    f"it names a program for {program} to execute"
                )

    return Admission(
        argv=argv,
        prefix=tuple(prefix),
        program=program,
        subcommand=subcommand,
        rendered=" ".join(quote_word(token) for token in (*prefix, program, *args)),
    )
