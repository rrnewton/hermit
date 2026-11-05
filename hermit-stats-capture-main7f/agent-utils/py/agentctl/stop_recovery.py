"""Render stop refusals from captured authority and argparse's own progress."""
from __future__ import annotations

import argparse
import os
import re
import shlex
import sys
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from typing import NoReturn, TypeVar, cast, overload

from agentctl.errors import AgentCtlError, RecoveryAction


def stop_refusal_message(
    error: BaseException, *, prefix: str, registry: str, herdr_bin: str,
    expected_token: str | None = None, expected_record_sha256: str | None = None,
) -> str:
    """Build one replayable command without reading any replacement state."""
    reason = error.stop_reason if isinstance(error, AgentCtlError) else None
    if reason is None:
        reason = str(error)
    if not reason.strip():
        reason = "stop refused without a reason"
    action = error.recovery_action if isinstance(error, AgentCtlError) else None
    if action is None:
        action = RecoveryAction("doctor")
    if ((expected_token is not None and expected_token != action.token)
            or (expected_record_sha256 is not None
                and expected_record_sha256 != action.record_sha256)):
        action = RecoveryAction("doctor")
    argv = ["agentctl", "--registry=" + os.path.abspath(registry), "--herdr-bin=" + herdr_bin]
    if (action.command == "retire-dead-adoption" and action.name and action.token
            and "\0" not in action.name and "\0" not in action.token
            and action.record_sha256 is not None
            and re.fullmatch(r"[0-9a-f]{64}", action.record_sha256) is not None):
        argv.extend([
            "stop", action.name, "--retire-dead-adoption",
            "--expected-token=" + action.token,
            "--expected-record-sha256=" + action.record_sha256,
        ])
    elif (action.command in ("stop", "revive") and action.name and action.token
            and "\0" not in action.name and "\0" not in action.token):
        argv.extend([action.command, action.name, "--expected-token=" + action.token])
        if action.command == "stop" and action.record_sha256 is not None:
            argv.extend([
                "--recover-legacy-adoption",
                "--expected-record-sha256=" + action.record_sha256,
            ])
    else:
        # Rename and move have no generation assertion in their CLI. A delayed
        # printed command could otherwise operate on a later replacement.
        argv.append("doctor")
    return f"{prefix}: {reason}\nRecovery command: {shlex.join(argv)}"


@dataclass
class _ParserContext:
    active: list[object] = field(default_factory=list)
    completed: object | None = None

    def namespace(self) -> argparse.Namespace:
        values: dict[str, object] = {}
        namespaces = self.active or ([self.completed] if self.completed is not None else [])
        for namespace in namespaces:
            values.update(vars(namespace))
        return argparse.Namespace(**values)


_Namespace = TypeVar("_Namespace")


class StopRecoveryParser(argparse.ArgumentParser):
    """Keep parsed stop context for syntax errors, without inspecting raw argv."""

    _stop_context: _ParserContext | None = None

    @property
    def _context(self) -> _ParserContext:
        if self._stop_context is None:
            self._stop_context = _ParserContext()
        return self._stop_context

    def share_stop_context(self, child: argparse.ArgumentParser) -> None:
        """Let a subparser retain its parent's already parsed global options."""
        if not isinstance(child, StopRecoveryParser):
            raise TypeError("stop recovery requires the same parser class for subcommands")
        child._stop_context = self._context

    @overload
    def parse_known_args(
        self, args: Iterable[str] | None = None, namespace: None = None,
    ) -> tuple[argparse.Namespace, list[str]]:
        """Parse arguments into a new namespace, retaining stop context."""
        ...

    @overload
    def parse_known_args(
        self, args: Iterable[str] | None, namespace: _Namespace,
    ) -> tuple[_Namespace, list[str]]:
        """Parse arguments into the supplied namespace, retaining stop context."""
        ...

    @overload
    def parse_known_args(self, *, namespace: _Namespace) -> tuple[_Namespace, list[str]]:
        """Parse process arguments into the supplied namespace with stop context."""
        ...

    def parse_known_args(
        self, args: Iterable[str] | None = None, namespace: object | None = None,
    ) -> tuple[object, list[str]]:
        """Track the namespace while argparse consumes the ordinary grammar."""
        current = namespace if namespace is not None else argparse.Namespace()
        context = self._context
        if not context.active:
            context.completed = None
        context.active.append(current)
        try:
            # argparse copies `args` with list(); typeshed versions differ on Sequence vs Iterable.
            result = super().parse_known_args(cast("Sequence[str] | None", args), current)
            context.completed = result[0]
            return result
        finally:
            context.active.pop()

    @overload
    def parse_known_intermixed_args(
        self, args: Iterable[str] | None = None, namespace: None = None,
    ) -> tuple[argparse.Namespace, list[str]]:
        """Parse intermixed arguments into a new namespace with stop context."""
        ...

    @overload
    def parse_known_intermixed_args(
        self, args: Iterable[str] | None, namespace: _Namespace,
    ) -> tuple[_Namespace, list[str]]:
        """Parse intermixed arguments into the supplied namespace with stop context."""
        ...

    @overload
    def parse_known_intermixed_args(
        self, *, namespace: _Namespace,
    ) -> tuple[_Namespace, list[str]]:
        """Parse intermixed process arguments into the supplied namespace."""
        ...

    def parse_known_intermixed_args(
        self, args: Iterable[str] | None = None, namespace: object | None = None,
    ) -> tuple[object, list[str]]:
        """Track options and positionals across supported intermixed parsers."""
        current = namespace if namespace is not None else argparse.Namespace()
        context = self._context
        if not context.active:
            context.completed = None
        context.active.append(current)
        try:
            result = super().parse_known_intermixed_args(cast("Sequence[str] | None", args), current)
            context.completed = result[0]
            return result
        finally:
            context.active.pop()

    def error(self, message: str) -> NoReturn:
        """Retain argparse's usage and status, adding advice for a parsed stop."""
        namespace = self._context.namespace()
        if getattr(namespace, "command", None) != "stop":
            super().error(message)
        registry = getattr(namespace, "registry", ".agentctl")
        herdr_bin = getattr(namespace, "herdr_bin", "herdr")
        self.print_usage(sys.stderr)
        self.exit(2, stop_refusal_message(
            ValueError(message), prefix=f"{self.prog}: error",
            registry=str(registry), herdr_bin=str(herdr_bin),
        ) + "\n")
