"""Writing a project's ``.herdr-run.yaml`` from the annotated template, and upgrading one.

The template carries EVERY knob, each set to the value the tool would use anyway, so a project can
adopt the file and change nothing. That is deliberate: the reference for what is configurable is
the file the tool writes into the project, not a block of documentation that restates it and then
drifts from it.

The cost of writing every knob out is that the file keeps the defaults of the release that wrote
it. A configured deny list replaces the built-in one, so an entry added to the built-in defaults
later does not reach a file written earlier. :func:`upgrade_config_file` is the way forward for
such a file: it rewrites each setting the file still holds exactly as an earlier release wrote it,
and leaves every other byte alone.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, replace

from herdr_run.config import (
    CONFIG_FILENAMES,
    MissingDefault,
    _has_surrogates,
    _lossy,
    missing_defaults,
    parse_config_text,
    read_config_file,
)
from herdr_run.errors import ConfigError
from herdr_run.textfmt import display, os_error_text, quote

__all__ = [
    "UPGRADES",
    "Upgrade",
    "UpgradeResult",
    "config_template",
    "upgrade_config_file",
    "upgrade_text",
    "write_config_template",
]


@dataclass(frozen=True)
class Upgrade:
    """One setting as an earlier release wrote it, and the same setting as this release writes it."""

    #: The setting, as a dotted path (``deny_anywhere``, ``value_options.git``).
    setting: str
    #: Whole lines, exactly as an earlier release wrote them.
    earlier: str
    #: The lines this release writes in their place.
    current: str
    #: The entries :attr:`current` has that :attr:`earlier` does not, in the order it lists them.
    added: tuple[str, ...]


#: Every setting an earlier release wrote differently from this one, in the order they are applied.
#:
#: * ``deny_anywhere`` without ``--exec``: written by ``herdr-run init`` in 0.2.0 and 0.3.0, and
#:   printed in the example configuration of the 0.1.0 and 0.2.0 user guides.
#: * ``value_options`` with seven git entries: written by ``herdr-run init`` in 0.2.0 and 0.3.0.
#:   The built-in table applies whatever a file says, so this one changes how the file reads, not
#:   what it does.
#:
#: The other edition holds the same table, and the shared corpus pins what each earlier release's
#: file becomes.
UPGRADES: tuple[Upgrade, ...] = (
    Upgrade(
        setting="deny_anywhere",
        earlier='deny_anywhere: ["--upload-pack", "--receive-pack"]\n',
        current='deny_anywhere: ["--upload-pack", "--receive-pack", "--exec"]\n',
        added=("--exec",),
    ),
    Upgrade(
        setting="value_options.git",
        earlier=(
            "value_options:\n"
            '  git: ["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path", '
            '"--config-env"]\n'
        ),
        current=(
            "value_options:\n"
            '  git: ["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path", '
            '"--config-env",\n'
            '        "--super-prefix", "--shallow-file", "--attr-source"]\n'
        ),
        added=("--super-prefix", "--shallow-file", "--attr-source"),
    ),
)


@dataclass(frozen=True)
class UpgradeResult:
    """What :func:`upgrade_config_file` did to one file, and what it left for a human."""

    #: The file upgraded.
    path: str
    #: Each setting rewritten, in :data:`UPGRADES` order, with the entries that gave it.
    upgraded: tuple[Upgrade, ...]
    #: The built-in deny entries the file still leaves out after the upgrade.
    missing: tuple[MissingDefault, ...]

#: Package resource holding the complete annotated ``.herdr-run.yaml``.
TEMPLATE_RESOURCE = "config_template.yaml"


def config_template() -> str:
    """Return the annotated ``.herdr-run.yaml`` this installation ships."""
    try:
        from importlib.resources import files

        return (files("herdr_run") / TEMPLATE_RESOURCE).read_text(encoding="utf-8")
    except (OSError, ModuleNotFoundError) as exc:  # pragma: no cover - damaged installation
        raise ConfigError(
            "the packaged configuration template is not available in this installation. "
            "Repair this herdr-run installation."
        ) from exc


def write_config_template(directory: str, *, force: bool) -> str:
    """Write the annotated configuration template into ``directory``; return the path written.

    Refuses to overwrite an existing configuration unless ``force``. It also refuses when only the
    ``.yml`` spelling exists, because the ``.yaml`` this would write silently takes precedence over
    it: a caller who ends up with two files and no warning has been given a puzzle, not a config.

    A directory whose name is not valid UTF-8 is refused, as every other subcommand refuses it: the
    file written there could never be loaded.
    """
    if _has_surrogates(directory):
        raise ConfigError(f"current directory is not valid UTF-8: {quote(_lossy(directory))}")
    destination = os.path.join(directory, CONFIG_FILENAMES[0])
    if not force:
        for filename in CONFIG_FILENAMES:
            existing = os.path.join(directory, filename)
            if not os.path.exists(existing):
                continue
            precedence = (
                ""
                if filename == CONFIG_FILENAMES[0]
                else f", and the {CONFIG_FILENAMES[0]} this would write takes precedence over it"
            )
            raise ConfigError(
                f"{display(existing)} already exists{precedence}; pass --force to overwrite"
            )
    text = config_template()
    # "x" without --force, so a file created between the check above and this open is still not
    # clobbered; the check exists to produce the useful message, not to be the guard.
    try:
        with open(destination, "w" if force else "x", encoding="utf-8") as handle:
            handle.write(text)
    except OSError as exc:
        raise ConfigError(f"cannot write {display(destination)}: {os_error_text(exc)}") from exc
    return destination


def upgrade_text(text: str) -> tuple[str, tuple[Upgrade, ...]]:
    """Rewrite every setting ``text`` still holds exactly as an earlier release wrote it.

    An :class:`Upgrade` applies where its earlier lines appear exactly once, starting at the start
    of a line; anything else -- a list somebody reformatted or edited, a file with CRLF line ends
    -- is left exactly as it is. A last line without a line feed is matched as if it had one, and
    still has none afterwards.
    """
    unterminated = not text.endswith("\n")
    working = "\n" + text + ("\n" if unterminated else "")
    applied: list[Upgrade] = []
    for upgrade in UPGRADES:
        needle = "\n" + upgrade.earlier
        if working.count(needle) != 1:
            continue
        working = working.replace(needle, "\n" + upgrade.current)
        applied.append(upgrade)
    upgraded = working[1:]
    if unterminated:
        upgraded = upgraded[:-1]
    return upgraded, tuple(applied)


def _covers(new: object, old: object) -> bool:
    """Whether the setting value ``new`` holds every entry the value ``old`` holds."""
    if isinstance(old, dict) and isinstance(new, dict):
        return all(key in new and _covers(new[key], value) for key, value in old.items())
    if isinstance(old, tuple) and isinstance(new, tuple):
        return all(entry in new for entry in old)
    return new == old


def _upgrade_target(directory: str) -> str:
    """The configuration file in ``directory`` that ``init --upgrade`` works on."""
    for filename in CONFIG_FILENAMES:
        candidate = os.path.join(directory, filename)
        if os.path.isfile(candidate):
            return candidate
    raise ConfigError(
        f"there is no {CONFIG_FILENAMES[0]} or {CONFIG_FILENAMES[1]} in "
        f"{display(directory)} to upgrade; run 'herdr-run init' to write one"
    )


def upgrade_config_file(directory: str) -> UpgradeResult:
    """Upgrade the configuration file in ``directory`` in place, and say what it did.

    The file is the ``.herdr-run.yaml`` there, else the ``.herdr-run.yml``: the one ``init`` would
    refuse to overwrite. It must load. :func:`upgrade_text` rewrites what it can; the result is
    loaded again and must read exactly as the file did except for the settings rewritten, which
    may only have gained entries -- otherwise nothing is written. The file is written in place, so
    its mode, its owner, and a symbolic link to it are kept, and it is not written at all when
    nothing changed.
    """
    if _has_surrogates(directory):
        raise ConfigError(f"current directory is not valid UTF-8: {quote(_lossy(directory))}")
    path = _upgrade_target(directory)
    text = read_config_file(path)
    before = parse_config_text(text, source_path=path, project_root=directory)
    upgraded, applied = upgrade_text(text)
    if not applied:
        return UpgradeResult(path=path, upgraded=(), missing=missing_defaults(before))
    after = parse_config_text(upgraded, source_path=path, project_root=directory)
    touched = {upgrade.setting.split(".", 1)[0] for upgrade in applied}
    unchanged = replace(after, **{name: getattr(before, name) for name in touched}) == before
    gained_only = all(_covers(getattr(after, name), getattr(before, name)) for name in touched)
    if not unchanged or not gained_only:
        raise ConfigError(
            f"{display(path)}: cannot upgrade it automatically, because the rewritten file would "
            "not read as this one does with entries added; nothing was written. Add the missing "
            "built-in entries by hand ('herdr-run status' lists them)"
        )
    try:
        with open(path, "w", encoding="utf-8", newline="") as handle:
            handle.write(upgraded)
    except OSError as exc:
        raise ConfigError(f"cannot write {display(path)}: {os_error_text(exc)}") from exc
    return UpgradeResult(path=path, upgraded=applied, missing=missing_defaults(after))
