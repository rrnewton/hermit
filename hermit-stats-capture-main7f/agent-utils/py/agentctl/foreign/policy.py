"""Explicit consumer policy loading, separate from the portable worker runtime."""

from __future__ import annotations

import importlib.util
import os
from functools import lru_cache
from pathlib import Path
from typing import Protocol, cast


class Policy(Protocol):
    """Consumer hooks selecting a backend and admitting or refusing worker launches."""
    def backend(self) -> str | None:
        """Return a presentation-backend override, or None to allow automatic detection."""
        ...

    def check_launch(self, harness: str, model: str | None, purpose: str) -> None:
        """Approve a worker launch or raise an operation error describing the policy refusal."""
        ...


class DefaultPolicy:
    """Default policy that leaves backend detection and harness launch unrestricted."""
    def backend(self) -> str | None:
        """Return a presentation-backend override, or None to allow automatic detection."""
        return None

    def check_launch(self, harness: str, model: str | None, purpose: str) -> None:
        """Allow this worker launch without imposing consumer-specific restrictions."""
        return None


@lru_cache(maxsize=16)
def _load(path: str) -> Policy:
    from .lib import AgentOperationError

    source = Path(path).expanduser().resolve()
    try:
        spec = importlib.util.spec_from_file_location("herdr_foreign_policy", source)
        if spec is None or spec.loader is None:
            raise ValueError("expected a Python source file")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        if not callable(getattr(module, "backend", None)) or not callable(
            getattr(module, "check_launch", None)
        ):
            raise ValueError("policy must define backend() and check_launch(harness, model, purpose)")
        return cast(Policy, module)
    except (OSError, ImportError, ValueError) as exc:
        raise AgentOperationError("policy_invalid", f"cannot load subagent policy {source}: {exc}") from exc


def configured_policy() -> Policy:
    """Load only a policy explicitly selected by the operator or consumer adapter."""
    path = os.environ.get("HERDR_SUBAGENTS_POLICY")
    return _load(path) if path else DefaultPolicy()
