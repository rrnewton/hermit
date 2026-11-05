"""The ``wrkslots image`` and ``wrkslots run`` commands.

Kept apart from ``cli.py`` so the representation and sandbox surface stays
readable; handlers import ``cli`` lazily to reuse its locks, state readers,
refusals, and process-use checks.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import secrets
import shutil
from pathlib import Path
from typing import TYPE_CHECKING

from wrkslots import sandbox, slotimage

if TYPE_CHECKING:
    from wrkslots import cli

GIB = 1024**3


def register(subparsers: argparse._SubParsersAction[argparse.ArgumentParser], formatter: type[argparse.HelpFormatter]) -> None:
    """Add the image, run, limits, and shell-command subcommands to the parser."""

    image = subparsers.add_parser(
        "image",
        help="inspect and manage disk-image slot storage",
        description=(
            "Manage the image representation. An image-backed slot's directory is the mount "
            "point of one sparse ext4 image under <control>/slot-images/<type>/<slot>/, with a "
            "second small image holding the sandbox's private HOME and /tmp. Representation is "
            "a storage choice only: ownership, heartbeats, salvage, and reclaim are unchanged. "
            "Every wrkslots command remounts image-backed slots that are not mounted (after a "
            "reboot, for example) before doing anything else."
        ),
        formatter_class=formatter,
    )
    actions = image.add_subparsers(dest="image_command", metavar="ACTION")

    status = actions.add_parser(
        "status",
        help="show each slot image, its mount, and how much host space it costs",
        description=(
            "List image-backed slots with apparent ceiling, host-allocated bytes, used bytes "
            "and inodes inside the image, and mount state. Also prints the representation "
            "new slots will get."
        ),
        formatter_class=formatter,
    )
    status.add_argument(
        "--format",
        choices=("human", "json"),
        default="human",
        help="human prints one block per slot; json prints {new_slot_representation, images} (default: human)",
    )
    status.set_defaults(handler=_cmd_image_status)

    mount = actions.add_parser(
        "mount",
        help="mount every image-backed slot that is not mounted",
        description="Mount all image-backed slots at their recorded locations (idempotent).",
        formatter_class=formatter,
    )
    mount.set_defaults(handler=_cmd_image_mount)

    unmount = actions.add_parser(
        "unmount",
        help="unmount one slot's images while nothing uses the slot",
        description=(
            "Unmount a slot's images after proving no process uses the slot. The next "
            "wrkslots command mounts them again; use this before copying an image file."
        ),
        formatter_class=formatter,
    )
    _slot_arguments(unmount)
    unmount.set_defaults(handler=_cmd_image_unmount)

    trim = actions.add_parser(
        "trim",
        help="return blocks freed inside a slot image to the host now",
        description=(
            "Run fstrim on a slot's images. Kernel mounts discard freed blocks online; FUSE "
            "mounts return them only on trim or unmount."
        ),
        formatter_class=formatter,
    )
    _slot_arguments(trim)
    trim.set_defaults(handler=_cmd_image_trim)

    grow = actions.add_parser(
        "grow",
        help="raise a slot image's ceiling",
        description=(
            "Enlarge the sparse slot image and its file system. Kernel mounts grow online; "
            "FUSE mounts are briefly unmounted. Growing costs no host space until used."
        ),
        formatter_class=formatter,
    )
    _slot_arguments(grow)
    grow.add_argument("--ceiling-gib", type=int, required=True, metavar="GIB", help="new apparent size in GiB")
    grow.set_defaults(handler=_cmd_image_grow)

    set_default = actions.add_parser(
        "set-default",
        help="choose the representation for slots created from now on",
        description=(
            "Change configuration.slot_representation. Only slots created afterwards are "
            "affected; existing slots keep their representation (convert them explicitly). The "
            "configuration keeps its format (literate YAML is regenerated; JSON stays JSON) and "
            "the previous bytes are saved to .wrkslots.yml.bak."
        ),
        formatter_class=formatter,
    )
    set_default.add_argument("representation", choices=("worktree", "image"))
    set_default.set_defaults(handler=_cmd_image_set_default)

    convert = actions.add_parser(
        "convert",
        help="migrate one idle slot between plain worktrees and an image, in place",
        description=(
            "Copy a slot's content into a new image (or out of its image into plain "
            "directories), prove the copy matches file by file, swap it in at the same path, "
            "and re-verify every checkout's Git identity. Refuses while any process uses the "
            "slot. The slot path, branches, and registry record are unchanged."
        ),
        epilog=(
            "Converting to worktree discards the state image's private HOME directories "
            "(home_private). "
            "--keep-original leaves the pre-conversion copy under the slot's image directory "
            "(or beside it) for inspection instead of deleting it."
        ),
        formatter_class=formatter,
    )
    _slot_arguments(convert)
    convert.add_argument("--to", choices=("image", "worktree"), required=True, dest="target")
    convert.add_argument("--keep-original", action="store_true", help="keep the pre-conversion copy")
    convert.set_defaults(handler=_cmd_image_convert)

    image.set_defaults(handler=_cmd_image_help, image_parser=image)

    run = subparsers.add_parser(
        "run",
        help="run a command boxed to one slot (limits + confined file-system view)",
        description=(
            "Run COMMAND boxed to one slot. The calling process joins a transient systemd user "
            "scope in the slot's own slice (limits shared by everything run against the slot) "
            "and then execs, so the PID never changes. With isolation userns or root, COMMAND "
            "also sees a confined file-system view (an accident boundary for cooperative agents, "
            "not containment of a hostile process; see the user guide's \"Limits of the box\"): "
            "a fresh /tmp per launch; $HOME as the slot's "
            "persistent private layer, in which every real top-level entry is bound read-only "
            "(nothing is copied), home_private_files are private copies seeded once, "
            "home_private directories are per-slot and writable, home_shared paths are the real "
            "ones and writable, and home_hidden credentials are masked; the slot, its Git "
            "directories, the project's outputs, and read_write paths writable; the wrkslots "
            "control directory (registry and other slots) always read-only, so registry commands "
            "such as heartbeat run outside the box; and, with protect_system, every other mount "
            "read-only. The box is the "
            "same for plain-worktree and image-backed slots. Defaults come from "
            "configuration.sandbox (see `wrkslots sandbox show-config`); list options ADD to "
            "the configured lists."
        ),
        epilog=(
            "Isolation: userns (default) builds the view in an unprivileged user namespace, where "
            "setuid programs gain nothing and host processes cannot be ptraced; root builds the "
            "identical view through a short-lived `sudo -n` launcher in a plain mount namespace "
            "and drops to your uid, gid, and groups, so setuid helpers (sudo, a harness launcher "
            "that enters a site sandbox) work inside the box, sudo can do whatever it allows, and "
            "your processes outside the box stay reachable through /proc; cgroup applies limits "
            "only. HOME modes: ro (default) keeps the real $HOME visible "
            "read-only; hidden covers it with an empty tmpfs and binds back only home_expose "
            "paths, read-only.\n\n"
            "Examples:\n"
            "  wrkslots run slot01 -- make test\n"
            "  wrkslots run slot01 --memory-max 32G --cpu-quota 800% -- claude\n"
            "  wrkslots run slot01 --isolation root -- claude\n"
            "  wrkslots run slot01 --home hidden --home-expose .gitconfig -- bash\n"
            "  wrkslots run slot01 --print -- bash"
        ),
        formatter_class=formatter,
    )
    run.add_argument("slot", help="registered slot name")
    run.add_argument("--slot-type", choices=("agent", "validate"), default="agent", help="slot type (default: agent)")
    _view_arguments(run)
    run.add_argument("--cwd", metavar="PATH", help="working directory (default: the slot directory)")
    run.add_argument("--print", action="store_true", dest="print_only", help="print the scope, slice limits, environment additions, and helper command or view; run nothing")
    run.add_argument("command", nargs="*", help="command to run, after a literal -- (default: $SHELL)")
    run.set_defaults(handler=_cmd_run)

    coordinator = subparsers.add_parser(
        "box",
        help="run a (sub)coordinator boxed to the project: all slots and the registry writable",
        description=(
            "Run COMMAND in a coordinator box: the same file-system view and limits as `run`, "
            "but with no slot. It is for an agent that creates slots and launches its own "
            "subagents into them. What it may write besides the usual home_shared, outputs, "
            "and read_write paths is chosen by --writable: worktrees (default) is the managed "
            "worktrees directory (every slot, the registry, slot images and state, validate "
            "slots) plus the Git directories the project's slots commit into; project is the "
            "whole project root. The agentctl registry (.agentctl in the working directory) "
            "is writable too, so the coordinator can launch agents. The registry being "
            "writable is the one deliberate exception to \"the registry is read-only inside a "
            "box\". $HOME is the box's own persistent private layer (<control>/box-state/NAME), "
            "/tmp is fresh per launch, and limits apply to the box's own slice."
        ),
        epilog=(
            "Image slots created from inside the box are mounted in the host's mount namespace "
            "through the user's systemd manager, so the host, workers launched into those slots, "
            "and the box itself all see them; mounts made on the host after the box started "
            "appear inside it under the worktrees directory. Subagents launched through a "
            "terminal multiplexer start outside this box (their pane shells belong to the "
            "multiplexer's server) and box themselves into their own slot.\n\n"
            "Examples:\n"
            "  wrkslots box -- claude\n"
            "  wrkslots box --isolation root --cwd worktrees -- claude\n"
            "  wrkslots box --writable project --name planner -- codex\n"
            "  wrkslots box --print -- bash"
        ),
        formatter_class=formatter,
    )
    coordinator.add_argument(
        "--name",
        default="coordinator",
        metavar="NAME",
        help="box name: its slice, scope, and persistent private $HOME layer (default: coordinator)",
    )
    coordinator.add_argument(
        "--writable",
        choices=sandbox.COORDINATOR_SCOPES,
        default=None,
        help=(
            "worktrees: the managed worktrees directory and the slots' Git directories; project: "
            "the whole project root (default: configuration.sandbox.coordinator_writable, else "
            "worktrees)"
        ),
    )
    _view_arguments(coordinator)
    coordinator.add_argument("--cwd", metavar="PATH", help="working directory (default: the project root)")
    coordinator.add_argument("--print", action="store_true", dest="print_only", help="print the scope, slice limits, environment additions, and helper command or view; run nothing")
    coordinator.add_argument("command", nargs="*", help="command to run, after a literal -- (default: $SHELL)")
    coordinator.set_defaults(handler=_cmd_box)

    box = subparsers.add_parser(
        "sandbox",
        help="show or complete the configuration.sandbox section",
        description=(
            "configuration.sandbox holds the defaults for `wrkslots run`, `wrkslots "
            "shell-command`, and agent launchers that box agents to slots. `init` writes the "
            "full section for a new project; unknown keys are refused."
        ),
        epilog=(
            "Keys: isolation (userns|cgroup|root), home (ro|hidden), home_shared, home_private, "
            "home_hidden, home_expose ($HOME-relative lists), home_private_files (top-level "
            "names), outputs (project-root-relative list), read_write (absolute or ~/ paths; "
            "$USER and $HOME expanded), env (NAME: value; leading ~ is $HOME), "
            "tmp_size, protect_system, coordinator_writable (worktrees|project: what `wrkslots box` "
            "may write), limits {memory_max, memory_high, cpu_quota, tasks_max, "
            "io_weight, all_slots_memory_max, all_slots_cpu_quota}.\n\n"
            "Examples:\n"
            "  wrkslots sandbox show-config\n"
            "  wrkslots sandbox write-defaults"
        ),
        formatter_class=formatter,
    )
    box_actions = box.add_subparsers(dest="sandbox_command", metavar="ACTION")
    show = box_actions.add_parser(
        "show-config",
        help="print the effective sandbox configuration as JSON",
        description=(
            "Print configuration.sandbox with every default filled in, exactly as `wrkslots "
            "run` will use it (before per-run options)."
        ),
        formatter_class=formatter,
    )
    show.set_defaults(handler=_cmd_sandbox_show)
    write_defaults = box_actions.add_parser(
        "write-defaults",
        help="add every missing sandbox default to the project configuration",
        description=(
            "Write each missing configuration.sandbox key (and missing limits key) with its "
            "default value. Present keys, and every other configuration key, are left as they "
            "are; the result is validated before it is written. A YAML file is rewritten in the "
            "canonical literate form (hand-written comments are regenerated) and a JSON file "
            "stays JSON; the previous bytes are saved to .wrkslots.yml.bak."
        ),
        formatter_class=formatter,
    )
    write_defaults.set_defaults(handler=_cmd_sandbox_write_defaults)
    box.set_defaults(handler=_cmd_sandbox_help, sandbox_parser=box)

    limits = subparsers.add_parser(
        "limits",
        help="show or apply the machine-wide guard on everything this user runs",
        description=(
            "Apply runtime (reboot-cleared) memory and CPU ceilings to this user's whole "
            "user-UID.slice with `sudo -n systemctl set-property --runtime`, so that every "
            "process the user runs, including agents that a harness moves into its own "
            "slice, shares one outer bound. Per-slot limits (wrkslots run) sit inside it. "
            "Nothing is written to host configuration."
        ),
        epilog="Example: wrkslots limits apply --memory-fraction 0.8 --cpu-fraction 0.9",
        formatter_class=formatter,
    )
    limits.add_argument("action", choices=("show", "apply", "clear"), help="show current values, apply new ones, or remove them")
    limits.add_argument("--memory-fraction", type=float, default=0.8, metavar="F", help="fraction of physical RAM for MemoryMax (default: 0.8)")
    limits.add_argument("--cpu-fraction", type=float, default=0.9, metavar="F", help="fraction of all CPUs for CPUQuota (default: 0.9)")
    limits.set_defaults(handler=_cmd_limits)

    shell_command = subparsers.add_parser(
        "shell-command",
        help="print an exec-only shell command line that enters a slot's box",
        description=(
            "Print one shell command line that replaces the current interactive shell with "
            "`wrkslots run SLOT -- SHELL -i`, using absolute interpreter paths and exec at "
            "every step so the process ID never changes. A terminal multiplexer that only "
            "starts agents in a pane whose own shell is at its prompt (Herdr) keeps working. "
            "An agent launcher runs it in a fresh pane before starting a harness. With a COMMAND "
            "after --, the line runs that program boxed instead of the shell (used for root "
            "isolation, where sudo stays in front of the program and the multiplexer cannot start "
            "an agent in the pane itself). The box is the same for plain-worktree and image-backed "
            "slots. With --box and no SLOT, the line enters a coordinator box (`wrkslots box`) "
            "instead, starting in --cwd."
        ),
        epilog=(
            "Examples:\n"
            "  wrkslots shell-command slot01 --format json\n"
            "  wrkslots shell-command slot01 --isolation root --format json -- /usr/local/bin/HARNESS ARGS\n"
            "  wrkslots shell-command --box --cwd worktrees --format json\n"
            "  wrkslots shell-command --box --isolation root --format json -- /usr/local/bin/HARNESS ARGS"
        ),
        formatter_class=formatter,
    )
    shell_command.add_argument("slot", nargs="?", help="registered slot name (omitted with --box)")
    shell_command.add_argument("--slot-type", choices=("agent", "validate"), default="agent", help="slot type (default: agent)")
    shell_command.add_argument(
        "--box",
        action="store_true",
        help="enter a coordinator box (`wrkslots box`) instead of a slot's box; no SLOT",
    )
    shell_command.add_argument(
        "--name",
        default="coordinator",
        metavar="NAME",
        help="with --box: the box name (default: coordinator)",
    )
    shell_command.add_argument(
        "--writable",
        choices=sandbox.COORDINATOR_SCOPES,
        default=None,
        help="with --box: worktrees or project (default: configuration.sandbox.coordinator_writable)",
    )
    shell_command.add_argument(
        "--cwd",
        metavar="PATH",
        help="with --box: the box's working directory (default: the project root)",
    )
    shell_command.add_argument(
        "--isolation",
        choices=sandbox.ISOLATIONS,
        default=None,
        help="passed to run (default: configuration.sandbox.isolation, else userns)",
    )
    shell_command.add_argument("--shell", default=None, metavar="PATH", help="interactive shell (default: $SHELL, else /bin/bash)")
    shell_command.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        help="text prints the command line; json prints {command, slot_path, isolation}, plus box with --box, where slot_path is the box's working directory (default: text)",
    )
    shell_command.add_argument(
        "command",
        nargs="*",
        help="program to run boxed instead of the interactive shell, after a literal -- (default: SHELL -i)",
    )
    shell_command.set_defaults(handler=_cmd_shell_command)
    for name in ("image", "run", "box", "sandbox", "limits", "shell-command"):
        _fill_help(subparsers.choices[name])


def _view_arguments(parser: argparse.ArgumentParser) -> None:
    """The view and limit options shared by `run` and `box` (and passed through by shell-command)."""

    parser.add_argument(
        "--isolation",
        choices=sandbox.ISOLATIONS,
        default=None,
        help=(
            "userns: limits plus the file-system view in an unprivileged user namespace; root: "
            "the same view built through sudo -n, then privileges dropped (setuid helpers, sudo "
            "included, work inside); cgroup: limits only, no file-system "
            "view (default: configuration.sandbox.isolation, else userns). Every mode is an "
            "accident boundary, not containment of a hostile process"
        ),
    )
    parser.add_argument(
        "--home",
        choices=sandbox.HOME_MODES,
        help="ro: every real $HOME entry visible read-only in the slot's layer; hidden: only home_expose paths (default: configuration.sandbox.home, else ro)",
    )
    parser.add_argument(
        "--home-shared",
        action="append",
        metavar="REL",
        help="$HOME-relative path bound read-write onto itself, shared with the host (repeatable; added to configuration.sandbox.home_shared)",
    )
    parser.add_argument(
        "--home-private",
        action="append",
        metavar="REL",
        help="$HOME-relative directory replaced by a persistent per-slot writable directory in the slot's layer (repeatable; added to configuration.sandbox.home_private)",
    )
    parser.add_argument(
        "--home-private-file",
        action="append",
        metavar="NAME",
        help="top-level $HOME file seeded once into the slot's layer as a private writable copy (repeatable; added to configuration.sandbox.home_private_files)",
    )
    parser.add_argument(
        "--home-hidden",
        action="append",
        metavar="REL",
        help="$HOME-relative path masked in the box: empty read-only directory or /dev/null (repeatable; added to configuration.sandbox.home_hidden)",
    )
    parser.add_argument(
        "--home-expose",
        action="append",
        metavar="REL",
        help="with --home hidden: $HOME-relative path bound back read-only (repeatable; added to configuration.sandbox.home_expose)",
    )
    parser.add_argument(
        "--output",
        action="append",
        metavar="REL",
        help="project-root-relative directory in the primary checkout left writable, skipped if missing (repeatable; added to configuration.sandbox.outputs)",
    )
    parser.add_argument(
        "--read-write",
        action="append",
        metavar="PATH",
        help="extra absolute or ~/ path left writable, skipped if missing; $USER and $HOME are expanded (repeatable; added to configuration.sandbox.read_write)",
    )
    parser.add_argument(
        "--env",
        action="append",
        metavar="NAME=VALUE",
        help="extra environment variable for COMMAND; a leading ~ in VALUE is your $HOME (repeatable; overrides configuration.sandbox.env)",
    )
    parser.add_argument("--tmp-size", metavar="SIZE", help="size limit of the per-launch /tmp tmpfs, such as 16G or 25%% (default: configuration.sandbox.tmp_size, else 16G)")
    parser.add_argument("--memory-max", metavar="SIZE", help="slot memory limit, such as 32G (swap is disabled when set)")
    parser.add_argument("--memory-high", metavar="SIZE", help="slot memory throttling threshold, such as 24G")
    parser.add_argument("--cpu-quota", metavar="PERCENT", help="slot CPU limit, such as 800%% for eight CPUs")
    parser.add_argument("--tasks-max", type=int, metavar="N", help=f"slot process and thread limit (default: configuration, else {sandbox.DEFAULT_TASKS_MAX})")
    parser.add_argument("--no-protect-system", action="store_true", help="leave mounts outside the slot writable ($HOME stays read-only; limits and masks still apply)")


def _fill(text: str | None) -> str | None:
    """Wrap prose paragraphs for the raw-description help formatter; keep indented lines."""

    import textwrap

    if not text:
        return text
    blocks = []
    for block in text.split("\n\n"):
        lines = block.split("\n")
        prose = " ".join(line for line in lines if not line.startswith("  "))
        indented = [line for line in lines if line.startswith("  ")]
        parts = [textwrap.fill(prose, width=96)] if prose else []
        blocks.append("\n".join([*parts, *indented]))
    return "\n\n".join(blocks)


def _fill_help(parser: argparse.ArgumentParser) -> None:
    parser.description = _fill(parser.description)
    parser.epilog = _fill(parser.epilog)
    for action in parser._actions:
        if isinstance(action, argparse._SubParsersAction):
            for child in action.choices.values():
                _fill_help(child)


def _slot_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("slot", help="registered slot name")
    parser.add_argument("--slot-type", choices=("agent", "validate"), default="agent", help="slot type (default: agent)")


def _cmd_image_help(args: argparse.Namespace) -> int:
    parser = args.image_parser
    assert isinstance(parser, argparse.ArgumentParser)
    parser.print_help()
    return 0


def _config(args: argparse.Namespace) -> "cli.Config":
    from wrkslots import cli

    return cli._load_config(args.project_root, args.machine)


def _refuse(message: str) -> "cli.Refusal":
    from wrkslots import cli

    return cli.Refusal(message)


def _load_image(config: "cli.Config", slot_type: str, slot: str) -> slotimage.SlotImage:
    try:
        image = slotimage.load(config.control, slot_type, slot)
    except slotimage.ImageError as exc:
        raise _refuse(str(exc)) from exc
    if image is None:
        raise _refuse(f"{slot_type} slot {slot!r} is not image-backed")
    return image


def _cmd_image_status(args: argparse.Namespace) -> int:
    config = _config(args)
    try:
        reports = [slotimage.usage(image) for image in slotimage.all_images(config.control)]
    except slotimage.ImageError as exc:
        raise _refuse(str(exc)) from exc
    if args.format == "json":
        print(json.dumps({"new_slot_representation": config.slot_representation, "images": reports}, indent=2))
        return 0
    print(f"new slots: {config.slot_representation}")
    if not reports:
        print("no image-backed slots")
    for report in reports:
        print(f"{report['slot_type']}/{report['name']} at {report['location']} ({report['phase']})")
        for label in ("slot", "state"):
            entry = report.get(label)
            if not isinstance(entry, dict):
                continue
            print(
                f"  {label:5s} mounted={entry.get('mounted')} backend={entry.get('backend', '-')} "
                f"host={_gib(entry.get('host_allocated_bytes'))} used={_gib(entry.get('used_bytes'))} "
                f"ceiling={_gib(entry.get('ceiling_bytes'))} inodes={entry.get('used_inodes', '-')}"
            )
    return 0


def _gib(value: object) -> str:
    if not isinstance(value, int):
        return "-"
    return f"{value / GIB:.2f}G"


def _cmd_image_mount(args: argparse.Namespace) -> int:
    config = _config(args)
    try:
        actions = slotimage.ensure_all_mounted(config.control)
    except slotimage.ImageError as exc:
        raise _refuse(str(exc)) from exc
    print("\n".join(actions) if actions else "all image-backed slots are mounted")
    return 0


def _cmd_image_unmount(args: argparse.Namespace) -> int:
    from wrkslots import cli

    config = _config(args)
    image = _load_image(config, args.slot_type, args.slot)
    with cli._mutation_locks(config, args.wait_lock):
        cli._assert_slot_unused(image.location)
        try:
            slotimage.unmount(image.state_image, image.state_mount)
            slotimage.unmount(image.slot_image, image.location)
        except slotimage.ImageError as exc:
            raise _refuse(str(exc)) from exc
    print(f"unmounted {args.slot_type}/{args.slot}; the next wrkslots command remounts it")
    return 0


def _cmd_image_trim(args: argparse.Namespace) -> int:
    config = _config(args)
    image = _load_image(config, args.slot_type, args.slot)
    for line in slotimage.trim(image):
        print(line)
    return 0


def _cmd_image_grow(args: argparse.Namespace) -> int:
    from wrkslots import cli

    config = _config(args)
    image = _load_image(config, args.slot_type, args.slot)
    with cli._mutation_locks(config, args.wait_lock):
        try:
            grown = slotimage.grow(image, args.ceiling_gib * GIB)
        except slotimage.ImageError as exc:
            raise _refuse(str(exc)) from exc
    print(f"{args.slot_type}/{args.slot} ceiling is now {grown.ceiling_bytes // GIB} GiB")
    return 0


def _cmd_image_set_default(args: argparse.Namespace) -> int:
    from wrkslots import cli

    config = _config(args)
    with cli._locked_config(config.config_path, args.wait_lock):
        raw = cli._read_config(config.config_path)
        updated = dict(raw)
        if args.representation == "worktree":
            updated.pop("slot_representation", None)
        else:
            updated["slot_representation"] = args.representation
        cli._write_config(config.config_path, updated)
    print(
        f"new slots will use the {args.representation} representation; "
        "existing slots are unchanged (see `wrkslots image convert`)"
    )
    return 0


def _checkout_paths(config: "cli.Config", record: "cli.ActiveRecord") -> list[tuple[Path, Path, str]]:
    from wrkslots import cli

    rows: list[tuple[Path, Path, str]] = []
    for checkout in record.checkouts:
        _relative, repository = cli._stored_repository_path(config, checkout.repository)
        path = cli._stored_path(config, checkout.path, "checkout path")
        rows.append((repository, path, checkout.head))
    return rows


def _verify_checkouts(config: "cli.Config", record: "cli.ActiveRecord") -> None:
    from wrkslots import cli

    vcs = cli._GitVcs()
    for repository, path, _head in _checkout_paths(config, record):
        vcs.verify_existing_worktree(repository, path)


def _cmd_image_convert(args: argparse.Namespace) -> int:
    from wrkslots import cli

    config = _config(args)
    slot_path = cli._slot_directory(config, args.slot, args.slot_type)
    with cli._mutation_locks(config, args.wait_lock):
        cli._refuse_partial_state(config)
        state = cli._load_active(config)
        record = cli._find_record(state, args.slot)
        if record.slot_type != args.slot_type:
            raise _refuse(f"slot {args.slot!r} is a {record.slot_type} slot, not {args.slot_type}")
        current = cli._slot_representation_of(config, args.slot, args.slot_type)
        if current == args.target:
            print(f"{args.slot_type}/{args.slot} is already {args.target}-backed")
            return 0
        if not slot_path.is_dir() or slot_path.is_symlink():
            raise _refuse(f"slot directory is missing or unsafe: {slot_path}")
        # Path use, not owner liveness: an idle agent may stay registered and
        # alive; what must not happen is copying files out from under a process
        # that has them open or is working inside the slot.
        cli._assert_slot_unused(slot_path)
        _verify_checkouts(config, record)
        if args.target == "image":
            _convert_to_image(config, record, slot_path, keep_original=args.keep_original)
        else:
            _convert_to_worktree(config, record, slot_path, keep_original=args.keep_original)
        cli._refresh_owned_mounts(config)
        _verify_checkouts(config, record)
    print(f"converted {args.slot_type}/{args.slot} to {args.target} at {slot_path}")
    return 0


def _convert_to_image(
    config: "cli.Config", record: "cli.ActiveRecord", slot_path: Path, *, keep_original: bool
) -> None:
    from wrkslots import cli

    staging = slotimage.image_directory(config.control, record.slot_type, record.slot).with_name(
        f".{record.slot}.convert-{secrets.token_hex(4)}"
    )
    image = cli._provision_slot_image(config, record.slot, record.slot_type, staging)
    original = image.directory / "pre-image-original"
    try:
        slotimage.copy_tree(slot_path, image.location)
        cli._assert_slot_unused(slot_path)
    except (slotimage.ImageError, cli.Refusal) as exc:
        slotimage.destroy(image, allow_content=True)
        raise _refuse(f"conversion to image abandoned; the slot is unchanged: {exc}") from exc
    os.rename(slot_path, original)
    try:
        slotimage.relocate(image, slot_path)
    except slotimage.ImageError as exc:
        os.rename(original, slot_path)
        raise _refuse(f"conversion to image failed; the original was restored: {exc}") from exc
    if keep_original:
        print(f"kept the pre-conversion copy at {original}")
    else:
        shutil.rmtree(original)


def _convert_to_worktree(
    config: "cli.Config", record: "cli.ActiveRecord", slot_path: Path, *, keep_original: bool
) -> None:
    image = _load_image(config, record.slot_type, record.slot)
    staging = image.directory.with_name(f".{record.slot}.convert-{secrets.token_hex(4)}")
    staging.mkdir(mode=0o755)
    try:
        slotimage.copy_tree(slot_path, staging)
    except slotimage.ImageError as exc:
        shutil.rmtree(staging)
        raise _refuse(f"conversion to worktree abandoned; the slot is unchanged: {exc}") from exc
    if keep_original:
        kept = image.directory.with_name(f".{record.slot}.pre-worktree-image")
        slotimage.unmount(image.state_image, image.state_mount)
        slotimage.unmount(image.slot_image, image.location)
        os.rename(image.directory, kept)
        print(f"kept the pre-conversion image directory at {kept}")
    else:
        slotimage.discard_files(image)
    slotimage.remove_mount_point(slot_path, image.directory.parent)
    os.rename(staging, slot_path)


def slot_view(config: "cli.Config", slot: str, slot_type: str) -> sandbox.SlotView:
    """Describe a registered slot for the sandbox, whatever its representation."""

    from wrkslots import cli

    state = cli._load_active(config)
    record = cli._find_record(state, slot)
    if record.slot_type != slot_type:
        raise _refuse(f"slot {slot!r} is a {record.slot_type} slot, not {slot_type}")
    slot_path = cli._slot_directory(config, slot, slot_type)
    try:
        image = slotimage.load(config.control, slot_type, slot)
    except slotimage.ImageError as exc:
        raise _refuse(str(exc)) from exc
    # Representation decides only where the slot's private state lives.
    if image is not None:
        state_directory = image.state_mount
    else:
        state_directory = config.control / "slot-state" / slot_type / slot
    vcs = cli._GitVcs()
    git_directories: list[Path] = []
    for repository, path, _head in _checkout_paths(config, record):
        common = vcs.common_directory(path if path.exists() else repository)
        if common not in git_directories:
            git_directories.append(common)
    return sandbox.SlotView(
        slot=slot,
        slot_type=slot_type,
        slot_path=slot_path,
        state_directory=state_directory,
        git_directories=tuple(git_directories),
        control_directory=config.control,
        representation="image" if image is not None else "worktree",
        project_root=config.root,
    )


def _project_git_directories(config: "cli.Config", state: "cli.ActiveState") -> list[Path]:
    """Git common directories a coordinator commits into or creates slots from.

    The project root's own repository (with its modules/ and worktrees/), every
    repository directly below the project root, and every repository an active
    slot's checkouts belong to.
    """

    from wrkslots import cli

    vcs = cli._GitVcs()
    found: list[Path] = []

    def add(repository: Path) -> None:
        try:
            common = vcs.common_directory(repository)
        except (cli.Refusal, cli.StateError, OSError):
            return
        if common not in found:
            found.append(common)

    managed = cli._managed_worktrees_root(config)
    if (config.root / ".git").exists():
        add(config.root)
    try:
        children = sorted(config.root.iterdir())
    except OSError:
        children = []
    for child in children:
        if child in (managed, config.control) or child.is_symlink() or not child.is_dir():
            continue
        if (child / ".git").exists():
            add(child)
    for record in state.slots:
        for repository, path, _head in _checkout_paths(config, record):
            add(path if path.exists() else repository)
    return found


def box_view(config: "cli.Config", name: str, scope: str) -> sandbox.SlotView:
    """Describe a coordinator box: no slot; the registry and slots (or the project) writable."""

    from wrkslots import cli

    cli._validate_name(name, "box name")
    if scope not in sandbox.COORDINATOR_SCOPES:
        raise _refuse(f"--writable must be one of {', '.join(sandbox.COORDINATOR_SCOPES)}")
    state = cli._load_active(config)
    managed = cli._managed_worktrees_root(config)
    roots = cli._slot_roots(config)
    if scope == "project":
        primary = config.root
        writable = [config.root]
    else:
        primary = managed
        writable = [managed, config.control, *roots.values()]
    for directory in _project_git_directories(config, state):
        writable.append(directory)
    # Image slots are mounted at the slot directories, their state images under
    # the control directory: those are the mounts the box must see arrive.
    propagate = [config.control, *roots.values()]
    # A root below another writable root is already writable: one bind each.
    unique = [
        path
        for index, path in enumerate(writable)
        if path not in writable[:index] and not any(other in path.parents for other in writable)
    ]
    return sandbox.SlotView(
        slot=name,
        slot_type="box",
        slot_path=primary,
        state_directory=config.control / "box-state" / name,
        git_directories=(),
        control_directory=config.control,
        representation=scope,
        project_root=config.root,
        coordinator=True,
        writable=tuple(path for path in unique if path != primary),
        propagate=tuple(dict.fromkeys(propagate)),
    )


def _agent_registry(view: sandbox.SlotView, cwd: Path, *, create: bool = True) -> sandbox.SlotView:
    """Make ``cwd/.agentctl`` (agentctl's default registry) writable when it lies in the project."""

    registry = cwd / ".agentctl"
    root = view.project_root
    if root is None or not (registry == root / ".agentctl" or root in registry.parents):
        return view
    covered = [view.slot_path, *view.writable]
    if any(registry == path or path in registry.parents for path in covered):
        return view
    if registry.is_symlink():
        raise _refuse(f"the agent registry {registry} is a symbolic link; remove it or use a real directory")
    if not registry.exists():
        if not create:
            return dataclasses.replace(view, writable=(*view.writable, registry))
        registry.mkdir(mode=0o700)
    if registry.is_symlink() or not registry.is_dir():
        raise _refuse(f"the agent registry {registry} is not a real directory")
    return dataclasses.replace(view, writable=(*view.writable, registry))


def _cmd_box(args: argparse.Namespace) -> int:
    config = _config(args)
    try:
        settings = _override_settings(config.sandbox_settings, args)
        view = box_view(config, args.name, settings.coordinator_writable)
        cwd = Path(args.cwd).absolute() if args.cwd else config.root
        if not cwd.is_dir():
            raise _refuse(f"--cwd {cwd} is not a directory")
        if settings.isolation != "cgroup":
            view = _agent_registry(view, cwd, create=not args.print_only)
        command = list(args.command)
        if not command:
            command = [os.environ.get("SHELL", "/bin/sh")]
        return sandbox.run(view, settings, command, cwd=cwd, print_only=args.print_only)
    except sandbox.SandboxError as exc:
        raise _refuse(str(exc)) from exc


def _cmd_run(args: argparse.Namespace) -> int:
    config = _config(args)
    view = slot_view(config, args.slot, args.slot_type)
    try:
        settings = _override_settings(config.sandbox_settings, args)
        command = list(args.command)
        if not command:
            command = [os.environ.get("SHELL", "/bin/sh")]
        return sandbox.run(
            view,
            settings,
            command,
            cwd=Path(args.cwd).absolute() if args.cwd else None,
            print_only=args.print_only,
        )
    except sandbox.SandboxError as exc:
        raise _refuse(str(exc)) from exc


def _cmd_sandbox_help(args: argparse.Namespace) -> int:
    parser = args.sandbox_parser
    assert isinstance(parser, argparse.ArgumentParser)
    parser.print_help()
    return 0


def _cmd_sandbox_show(args: argparse.Namespace) -> int:
    config = _config(args)
    print(json.dumps(sandbox.settings_to_obj(config.sandbox_settings), indent=2))
    return 0


def _cmd_sandbox_write_defaults(args: argparse.Namespace) -> int:
    from wrkslots import cli

    config = _config(args)
    with cli._locked_config(config.config_path, args.wait_lock):
        raw = cli._read_config(config.config_path)
        existing = raw.get("sandbox", {})
        if not isinstance(existing, dict):
            raise _refuse("configuration.sandbox must be an object")
        try:
            merged, added = sandbox.merge_defaults(existing)
        except sandbox.SandboxError as exc:
            raise _refuse(str(exc)) from exc
        if added:
            updated = dict(raw)
            updated["sandbox"] = merged
            cli._write_config(config.config_path, updated)
    if added:
        print(f"added to configuration.sandbox in {config.config_path}: {', '.join(added)}")
    else:
        print(f"configuration.sandbox in {config.config_path} already has every key")
    return 0


def _cmd_shell_command(args: argparse.Namespace) -> int:
    import shlex
    import sys

    from wrkslots import cli

    config = _config(args)
    command = list(args.command)
    if args.box:
        if args.slot is not None:
            # With --box there is no SLOT: argparse hands the first word after -- to it.
            # A word before -- is a mistaken SLOT, not a program to run boxed.
            raw = list(getattr(args, "raw_arguments", []) or [])
            after = raw[raw.index("--") + 1 :] if "--" in raw else []
            if not after or after[0] != args.slot:
                raise _refuse(
                    f"shell-command --box takes no SLOT ({args.slot!r}); put a program to run "
                    "boxed after a literal --"
                )
            command = [args.slot, *command]
        cli._validate_name(args.name, "box name")
    else:
        if args.slot is None:
            raise _refuse("shell-command needs a SLOT, or --box for a coordinator box")
        state = cli._load_active(config)
        cli._find_record(state, args.slot)
    if not sandbox.scope_in_place_available():
        # The line must keep the pane's process ID (a multiplexer checks it); the
        # fallback that `wrkslots run` uses without D-Bus cannot.
        raise _refuse(
            "shell-command needs the systemd user manager's D-Bus API (busctl --user) so the "
            "boxed shell keeps the pane's process ID; it is unavailable here"
        )
    shell = args.shell or os.environ.get("SHELL") or "/bin/bash"
    # The real interpreter (a wrapper script that forks would change the PID)
    # running the entry script in isolated mode: nothing is added to the
    # environment the boxed shell inherits (no PYTHONPATH).
    argv = [
        "exec",
        os.path.realpath(sys.executable),
        "-I",
        str(Path(__file__).resolve().with_name("__main__.py")),
        "--project-root",
        str(config.root),
    ]
    if args.box:
        cwd = Path(args.cwd).absolute() if args.cwd else config.root
        argv += ["box", "--name", args.name, "--cwd", str(cwd)]
        if args.writable is not None:
            argv += ["--writable", args.writable]
    else:
        argv += ["run", args.slot, "--slot-type", args.slot_type]
    if args.isolation is not None:
        argv += ["--isolation", args.isolation]
    if command:
        # A command given after -- replaces the interactive shell: the pane then
        # runs that program boxed, and closes when it exits.
        argv += ["--", *command]
    else:
        argv += ["--", os.path.realpath(shutil.which(shell) or shell), "-i"]
    line = " ".join(shlex.quote(part) if index else part for index, part in enumerate(argv))
    if args.format == "json":
        isolation = args.isolation or config.sandbox_settings.isolation
        if args.box:
            # slot_path is where the pane lands: the box's working directory.
            payload = {"command": line, "slot_path": str(cwd), "isolation": isolation, "box": args.name}
        else:
            slot_path = cli._slot_directory(config, args.slot, args.slot_type)
            payload = {"command": line, "slot_path": str(slot_path), "isolation": isolation}
        print(json.dumps(payload))
    else:
        print(line)
    return 0


def _cmd_limits(args: argparse.Namespace) -> int:
    import subprocess

    unit = f"user-{os.getuid()}.slice"
    keys = ("MemoryMax", "MemorySwapMax", "CPUQuotaPerSecUSec")
    if args.action in ("apply", "clear"):
        if args.action == "apply":
            for label, value in (("--memory-fraction", args.memory_fraction), ("--cpu-fraction", args.cpu_fraction)):
                if not 0 < value <= 1:
                    raise _refuse(f"{label} must be greater than 0 and at most 1")
            memory = int(os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE") * args.memory_fraction)
            cpu = max(1, int((os.cpu_count() or 1) * 100 * args.cpu_fraction))
            properties = [f"MemoryMax={memory}", "MemorySwapMax=0", f"CPUQuota={cpu}%"]
        else:
            properties = ["MemoryMax=infinity", "MemorySwapMax=infinity", "CPUQuota="]
        result = subprocess.run(
            ["sudo", "-n", "systemctl", "set-property", "--runtime", unit, *properties],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise _refuse(f"cannot set {unit} limits (needs sudo -n): {result.stderr.strip()}")
    shown = subprocess.run(
        ["systemctl", "show", unit, *(f"--property={key}" for key in keys)],
        capture_output=True,
        text=True,
        check=False,
    )
    print(f"{unit}:")
    for line in shown.stdout.splitlines():
        print(f"  {line}")
    return 0 if shown.returncode == 0 else 1


def _override_settings(settings: sandbox.SandboxSettings, args: argparse.Namespace) -> sandbox.SandboxSettings:
    """Apply per-run options on top of the configured settings, then re-validate."""

    merged = sandbox.settings_to_obj(settings)
    if getattr(args, "writable", None) is not None:
        merged["coordinator_writable"] = args.writable
    if args.isolation is not None:
        merged["isolation"] = args.isolation
    if args.home is not None:
        merged["home"] = args.home
    for option, key in (
        ("home_shared", "home_shared"),
        ("home_private", "home_private"),
        ("home_private_file", "home_private_files"),
        ("home_hidden", "home_hidden"),
        ("home_expose", "home_expose"),
        ("output", "outputs"),
        ("read_write", "read_write"),
    ):
        extra = getattr(args, option)
        if extra:
            current = merged[key]
            assert isinstance(current, list)
            merged[key] = [*current, *extra]
    if args.env:
        env = merged["env"]
        assert isinstance(env, dict)
        for item in args.env:
            name, separator, value = item.partition("=")
            if not separator:
                raise sandbox.SandboxError(f"--env needs NAME=VALUE, not {item!r}")
            env[name] = value
    if args.tmp_size is not None:
        merged["tmp_size"] = args.tmp_size
    if args.no_protect_system:
        merged["protect_system"] = False
    limits = merged["limits"]
    assert isinstance(limits, dict)
    for option in ("memory_max", "memory_high", "cpu_quota"):
        value = getattr(args, option)
        if value is not None:
            limits[option] = sandbox.validate_limit(value, "--" + option.replace("_", "-"))
    if args.tasks_max is not None:
        limits["tasks_max"] = args.tasks_max
    return sandbox.settings_from_obj(merged)
