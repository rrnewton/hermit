"""Bounded, strict JSON artifacts used by the Chat bridge."""

from __future__ import annotations

import json
import os
import stat
from dataclasses import dataclass
from pathlib import Path

import agentctl.agent as agent_module
from agentctl.agent import (
    _read_bounded_queue_json,
    _reject_duplicate_json_keys, _reject_json_constant, _validate_json_depth,
)
from agentctl.errors import AgentDeliveryError
from agentctl.jsonx import as_mapping


@dataclass(frozen=True)
class ArtifactClass:
    """One encoded-file bound and diagnostic identity."""

    name: str
    max_bytes: int

    def __post_init__(self) -> None:
        if not self.name or type(self.max_bytes) is not int or self.max_bytes <= 0:
            raise ValueError("artifact class needs a name and positive byte limit")


CONFIG = ArtifactClass("Chat configuration", 512 << 10)
BRIDGE = ArtifactClass("saved Chat bridge", 512 << 10)
INPUT = ArtifactClass("saved Chat input observer", 32 << 10)
OUTPUT = ArtifactClass("saved Chat output observer", 8 << 10)
DIAGNOSTIC = ArtifactClass("saved Chat refusal diagnostic", 64 << 10)
REQUEST = ArtifactClass("saved request record", 512 << 10)
FEEDBACK = ArtifactClass("saved feedback record", 512 << 10)
DEFERRED = ArtifactClass("saved deferred message", 512 << 10)
QUEUE = ArtifactClass("saved queue artifact", 512 << 10)
REPLY = ArtifactClass("saved reply artifact", 256 << 10)
SUBMISSION = ArtifactClass("saved reply submission", 256 << 10)
RECEIPT = ArtifactClass("saved reply receipt", 8 << 10)
LEGACY_REPLY = ArtifactClass("embedded reply outbox", 256 << 20)
REST_RESPONSE = ArtifactClass("Google Chat response", 8 << 20)
REPLY_INPUT = ArtifactClass("Chat reply input", 30_000)


def artifact_for_path(path: Path) -> ArtifactClass:
    """Classify every JSON destination owned by one Chat state directory."""
    if path.name == "bridge.json":
        return BRIDGE
    if path.name == "input.json":
        return INPUT
    if path.name == "output.json":
        return OUTPUT
    if path.name in ("request-limit.json", "population-limit.json"):
        return DIAGNOSTIC
    parts = path.parts
    # Work from the file toward the filesystem root so a state directory whose
    # ancestors happen to use names such as ``items`` or ``queue`` cannot alter
    # the artifact class of the bridge-owned subtree nearest the file.
    for index in range(len(parts) - 2, -1, -1):
        component = parts[index]
        if component == "requests":
            return REQUEST
        if component == "feedback":
            return FEEDBACK
        if component == "deferred":
            return DEFERRED
        if component == "submissions":
            return SUBMISSION
        if component == "reply-receipts":
            return RECEIPT
        if component == "replies":
            return REPLY if parts[index + 1] == "items" else LEGACY_REPLY
        if component == "queue":
            return QUEUE
    return DIAGNOSTIC


def encoded_json(document: object, artifact: ArtifactClass) -> bytes:
    """Pre-serialize one finite document and enforce its encoded artifact cap."""
    try:
        _validate_json_depth(document)
        encoded = (json.dumps(
            document, indent=2, sort_keys=True, allow_nan=False,
        ) + "\n").encode("utf-8")
    except (TypeError, ValueError, UnicodeError, RecursionError) as exc:
        raise ValueError(f"{artifact.name} must be finite JSON data") from exc
    if len(encoded) > artifact.max_bytes:
        raise ValueError(
            f"{artifact.name} exceeds its {artifact.max_bytes}-byte encoded file limit")
    return encoded


def decode_json(encoded: bytes, artifact: ArtifactClass) -> dict[str, object]:
    """Decode one already-bounded strict JSON object."""
    if len(encoded) > artifact.max_bytes:
        raise ValueError(
            f"{artifact.name} exceeds its {artifact.max_bytes}-byte encoded limit")
    try:
        decoded = json.loads(
            encoded.decode("utf-8"), object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_json_constant,
        )
        _validate_json_depth(decoded)
    except (UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as exc:
        raise ValueError(f"{artifact.name} is not a valid bounded JSON object: {exc}") from exc
    return as_mapping(decoded, artifact.name)


def read_json(path: Path, artifact: ArtifactClass | None = None) -> tuple[dict[str, object], int]:
    """Read one strict private artifact through one bounded descriptor."""
    selected = artifact or artifact_for_path(path)
    decoded, encoded_size = _read_bounded_queue_json(
        str(path), selected.name, require_private=True,
        max_artifact_bytes=selected.max_bytes,
    )
    document = as_mapping(decoded, selected.name)
    return document, encoded_size


def read_text(
    path: Path, artifact: ArtifactClass, *, require_private: bool = False,
) -> str:
    """Read one stable UTF-8 regular file through one bounded descriptor."""
    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = -1
    try:
        try:
            descriptor = os.open(path, flags)
        except OSError as exc:
            raise AgentDeliveryError(
                f"cannot open {artifact.name} {path}: {exc}") from exc
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
            raise AgentDeliveryError(f"unsafe {artifact.name}: {path}")
        if stat.S_IMODE(metadata.st_mode) & 0o022:
            raise AgentDeliveryError(
                f"{artifact.name} must not be writable by another account: {path}")
        if require_private and stat.S_IMODE(metadata.st_mode) & 0o077:
            raise AgentDeliveryError(f"{artifact.name} is not private: {path}")
        if metadata.st_nlink != 1:
            raise AgentDeliveryError(f"{artifact.name} must not be hard-linked: {path}")
        if metadata.st_size > artifact.max_bytes:
            raise AgentDeliveryError(
                f"{artifact.name} exceeds its {artifact.max_bytes}-byte limit: {path}")

        encoded = bytearray()
        while True:
            remaining = artifact.max_bytes + 1 - len(encoded)
            if remaining <= 0:
                raise AgentDeliveryError(
                    f"{artifact.name} exceeds its {artifact.max_bytes}-byte limit: {path}")
            block = os.read(descriptor, min(64 << 10, remaining))
            if not block:
                break
            encoded.extend(block)
            if len(encoded) > artifact.max_bytes:
                raise AgentDeliveryError(
                    f"{artifact.name} exceeds its {artifact.max_bytes}-byte limit: {path}")

        final_metadata = os.fstat(descriptor)
        if (
            final_metadata.st_dev != metadata.st_dev
            or final_metadata.st_ino != metadata.st_ino
            or final_metadata.st_size != len(encoded)
            or final_metadata.st_mode != metadata.st_mode
            or final_metadata.st_uid != metadata.st_uid
            or final_metadata.st_mtime_ns != metadata.st_mtime_ns
            or final_metadata.st_nlink != 1
            or final_metadata.st_ctime_ns != metadata.st_ctime_ns
        ):
            raise AgentDeliveryError(f"{artifact.name} changed while it was read: {path}")
        return bytes(encoded).decode("utf-8")
    except AgentDeliveryError:
        raise
    except (OSError, UnicodeError) as exc:
        raise AgentDeliveryError(
            f"cannot read {artifact.name} {path}: {exc}") from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def write_json(
    path: Path, document: dict[str, object], artifact: ArtifactClass | None = None,
    *, create_only: bool = False,
) -> None:
    """Preflight strict serialization, then use the shared atomic writer seam."""
    selected = artifact or artifact_for_path(path)
    encoded_json(document, selected)
    writer = agent_module._atomic_json_create if create_only else agent_module._atomic_json
    writer(str(path), document, max_artifact_bytes=selected.max_bytes)
