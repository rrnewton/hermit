"""Stable failure outcomes for persistent agent control."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, TypeVar

EXIT_UNAVAILABLE = 69
EXIT_BUSY = 75
EXIT_TIMEOUT = 76


@dataclass(frozen=True)
class RecoveryAction:
    """Recovery authority captured at the refusal, never from a later record."""

    command: Literal["doctor", "stop", "rename", "move", "revive", "retire-dead-adoption"]
    name: str | None = None
    token: str | None = None
    rename_to: str | None = None
    record_sha256: str | None = None


class AgentCtlError(Exception):
    """Base exception for session, adapter, and delivery failures."""
    exit_code = 1
    recovery_action: RecoveryAction | None = None
    stop_reason: str | None = None


_Error = TypeVar("_Error", bound=AgentCtlError)


def _with_recovery(
    error: _Error, action: RecoveryAction, *, stop_reason: str | None = None,
) -> _Error:
    """Annotate an existing failure without changing its class or exit status."""
    error.recovery_action = action
    error.stop_reason = stop_reason
    return error


# Kept for extracted library callers while internal imports migrate.
HerdrRunError = AgentCtlError


class HerdrUnavailable(AgentCtlError):
    """The Herdr transport or a verified target cannot be reached."""
    exit_code = EXIT_UNAVAILABLE


class AgentDeliveryError(AgentCtlError):
    """Input could not be delivered under the required identity and state."""
    exit_code = EXIT_BUSY


class InputExpectationFailed(HerdrUnavailable):
    """Herdr refused input because the pane no longer held the expected terminal; nothing was written."""


class RecipientChanged(HerdrUnavailable):
    """The pane stopped holding the verified recipient before an input effect."""


class MisrouteRecovered(AgentDeliveryError):
    """Input reached the wrong program, which was interrupted and told to ignore it.

    The message itself was not delivered to its recipient and may be retried.
    """


class ProbableMisroute(AgentDeliveryError):
    """Input was written, then the pane failed its recipient check: it may have reached another program."""


class AgentPending(AgentDeliveryError):
    """Nothing was injected; the durable prompt remains safe to retry."""

    exit_code = EXIT_BUSY

    def __init__(self, message: str, *, message_id: str, artifact: str) -> None:
        super().__init__(message)
        self.message_id = message_id
        self.artifact = artifact
        self.outcome = "pending"


class AgentPossiblySubmitted(AgentDeliveryError):
    """Injection may have succeeded; automatic retry could duplicate a turn."""

    exit_code = EXIT_TIMEOUT

    def __init__(self, message: str, *, message_id: str, artifact: str) -> None:
        super().__init__(message)
        self.message_id = message_id
        self.artifact = artifact
        self.outcome = "possibly_submitted"
