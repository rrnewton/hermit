"""Extract complete, request-scoped reply blocks from rendered terminal text."""

from __future__ import annotations

import re
from collections.abc import Collection

_NONCE = re.compile(r"[A-Za-z0-9_-]{22}\Z")
_SEQUENCED = re.compile(r"(?P<nonce>[A-Za-z0-9_-]{22})_(?P<ordinal>[1-9][0-9]{0,5})\Z")
_MARKER = re.compile(r"<(?P<close>/?)(?P<protocol>G?CHAT)_REPLY_(?P<nonce>[^<>\s]*)>")
_FENCE = re.compile(r"(?P<fence>`{3,}|~{3,})(?P<suffix>.*)")
_MAX_REPLY_BYTES = 30_000


def _undecorate(line: str) -> tuple[str, str]:
    """Return marker text and its margin, replacing a native leading bullet."""
    stripped = line.lstrip(" \t")
    margin = line[:len(line) - len(stripped)]
    if stripped.startswith(("• ", "⏺ ", "● ")):
        stripped = stripped[2:]
        margin += "  "
        extra = len(stripped) - len(stripped.lstrip(" \t"))
        margin += stripped[:extra]
        stripped = stripped[extra:]
    return stripped.rstrip(" \t"), margin


def _body(lines: list[str], opening_margin: str, closing_margin: str) -> str:
    """Remove only indentation shared by the markers and nonblank body lines."""
    margin_length = len(opening_margin)
    for line in [closing_margin, *(line for line in lines if line.strip())]:
        common = 0
        limit = min(margin_length, len(line))
        while common < limit and opening_margin[common] == line[common]:
            common += 1
        margin_length = common
        if not margin_length:
            break
    margin = opening_margin[:margin_length]
    result = "\n".join(line[len(margin):] if line.startswith(margin) else line for line in lines)
    size = len(result.encode("utf-8"))
    if not result.strip() or size > _MAX_REPLY_BYTES:
        raise ValueError("chat reply body must be nonempty and at most 30000 UTF-8 bytes")
    return result


def _scan(
    text: str, nonces: Collection[str], *, extract: bool, sequenced: bool = False,
) -> tuple[dict[str, list[str]], list[str], list[str], list[str]]:
    expected = set(nonces)
    expected_pattern = _SEQUENCED if sequenced else _NONCE
    if any(expected_pattern.fullmatch(nonce) is None for nonce in expected):
        description = ("22 base64url characters plus an underscore and ordinal"
                       if sequenced else "22 base64url characters")
        raise ValueError(f"chat reply nonce must contain {description}")
    text = text.replace("\r\n", "\n")
    if any((ord(char) < 32 and char not in "\t\n") or 127 <= ord(char) <= 159 for char in text):
        raise ValueError("chat reply capture must contain rendered text without ANSI or control sequences")
    try:
        text.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError("chat reply capture must be valid UTF-8") from error

    replies: dict[str, list[str]] = {}
    unknown: dict[str, None] = {}
    closing_unknown: dict[str, None] = {}
    raw_closing: dict[str, None] = {}
    active: tuple[str, str] | None = None
    opening_margin = ""
    body: list[str] = []
    fence = ""
    prompt_margin: int | None = None
    for line in text.split("\n"):
        normalized, margin = _undecorate(line)
        raw_marker = _MARKER.fullmatch(normalized)
        if raw_marker is not None and raw_marker["close"]:
            raw_closing[raw_marker["nonce"]] = None
        stripped = line.lstrip(" \t")
        decorated = stripped.startswith(("• ", "⏺ ", "● "))
        if active is None:
            # Native TUIs echo wrapped user prompts beneath ›/❯. Their hanging
            # indentation does not turn markers in user text into assistant output.
            if stripped.startswith(("› ", "❯ ")):
                prompt_margin = len(line) - len(stripped) + 2
                continue
            if prompt_margin is not None:
                # A user can type native-looking bullets in the echoed prompt.
                # Only returning to its base margin ends that continuation.
                if not stripped or len(line) - len(stripped) >= prompt_margin:
                    continue
                prompt_margin = None
        marker = _MARKER.fullmatch(normalized)
        if (active is None and marker is not None and not marker["close"]
                and marker["nonce"] in expected and decorated):
            fence = ""
        fence_match = _FENCE.fullmatch(normalized)
        if fence:
            if (fence_match is not None
                    and fence_match["fence"][0] == fence[0]
                    and len(fence_match["fence"]) >= len(fence)
                    and not fence_match["suffix"].strip()):
                fence = ""
            if active is not None and extract:
                body.append(line)
            continue
        if fence_match is not None and (
            fence_match["fence"][0] != "`" or "`" not in fence_match["suffix"]
        ):
            fence = fence_match["fence"]
            if active is not None and extract:
                body.append(line)
            continue

        if marker is None:
            if active is not None and extract:
                body.append(line)
            continue
        nonce = marker["nonce"]
        if nonce not in expected:
            unknown[nonce] = None
            if marker["close"]:
                closing_unknown[nonce] = None
        identity = marker["protocol"], nonce
        if active is None:
            if nonce in expected and not marker["close"]:
                active, opening_margin, body = identity, margin, []
            continue
        if not marker["close"]:
            if extract:
                raise ValueError("nested chat reply markers are ambiguous")
            continue
        if identity != active:
            if extract:
                raise ValueError("chat reply closing marker does not match its opening marker")
            continue
        if extract:
            replies.setdefault(nonce, []).append(_body(body, opening_margin, margin))
        active, body = None, []
    return replies, list(unknown), list(closing_unknown), list(raw_closing)


def extract_replies(text: str, nonces: Collection[str]) -> dict[str, list[str]]:
    """Return ordered complete reply bodies for each expected request nonce.

    Each reply uses standalone ``<CHAT_REPLY_NONCE>`` and
    ``</CHAT_REPLY_NONCE>`` lines. ``GCHAT_REPLY`` blocks are also accepted.
    Opening and closing tag spellings must match.
    Multiple blocks for one nonce remain separate, including identical bodies;
    callers provide durable occurrence-based delivery deduplication across captures.

    Inline examples, quoted markers, native terminal prompt echoes, and markers
    inside Markdown fences are ignored. A native leading ``• ``/``⏺ ``/``● `` on an
    expected opening marker starts a fresh assistant item, discarding an unrelated
    outside-block fence. Native decorations and shared terminal indentation are
    removed without stripping body bullets or meaningful code indentation.

    ``text`` must be rendered terminal text. CRLF is accepted; ANSI escapes,
    other control characters, invalid nonces/UTF-8, nested blocks, mismatched
    closing markers, and empty or oversized completed replies raise ``ValueError``.
    Incomplete blocks produce no reply. Bodies are limited to 30000 UTF-8 bytes.
    """
    return _scan(text, nonces, extract=True)[0]


def unknown_reply_ids(text: str, nonces: Collection[str]) -> list[str]:
    """Return first-seen unavailable IDs from standalone reply markers.

    Apply the same rendered-text validation and example/prompt filtering as
    :func:`extract_replies`, accepting both ``CHAT_REPLY`` and ``GCHAT_REPLY``.
    Both opening and closing markers are checked, including empty, malformed,
    or incorrectly sized IDs. IDs may contain punctuation; callers must escape
    and bound them when constructing feedback. Duplicate IDs appear once.

    Incomplete, nested, or mismatched blocks do not prevent diagnostics for an
    unavailable ID. Expected IDs still must contain 22 base64url characters.
    """
    return _scan(text, nonces, extract=False)[1]


def reply_marker_ids(text: str) -> list[str]:
    """Return first-seen standalone marker IDs after rendered-text filtering."""
    return _scan(text, (), extract=False)[1]


def reply_marker_sets(text: str) -> tuple[list[str], list[str]]:
    """Return filtered marker IDs and raw standalone closing IDs in one scan."""
    _, identifiers, _, raw_closing = _scan(text, (), extract=False)
    return identifiers, raw_closing


def sequenced_reply_id(nonce: str, ordinal: int) -> str:
    """Build one bounded protocol-v3 marker ID."""
    if _NONCE.fullmatch(nonce) is None:
        raise ValueError("chat reply nonce must contain 22 base64url characters")
    if type(ordinal) is not int or not 1 <= ordinal <= 999_999:
        raise ValueError("chat reply ordinal must be an integer between 1 and 999999")
    return f"{nonce}_{ordinal}"


def extract_sequenced_replies(
    text: str, nonces: Collection[str], *, marker_ids: Collection[str] | None = None,
) -> dict[str, list[tuple[int, str]]]:
    """Extract protocol-v3 replies, preserving occurrence order and ordinals."""
    expected_nonces = set(nonces)
    if any(_NONCE.fullmatch(nonce) is None for nonce in expected_nonces):
        raise ValueError("chat reply nonce must contain 22 base64url characters")
    visible = reply_marker_ids(text) if marker_ids is None else list(marker_ids)
    expected_ids: set[str] = set()
    for identifier in visible:
        match = _SEQUENCED.fullmatch(identifier)
        if match is not None and match["nonce"] in expected_nonces:
            expected_ids.add(identifier)
    raw, _, _, _ = _scan(text, expected_ids, extract=True, sequenced=True)
    result: dict[str, list[tuple[int, str]]] = {}
    for identifier, bodies in raw.items():
        match = _SEQUENCED.fullmatch(identifier)
        assert match is not None
        nonce, ordinal = match["nonce"], int(match["ordinal"])
        result.setdefault(nonce, []).extend((ordinal, body) for body in bodies)
    return result
