"""A strict YAML subset for ``.wrkslots.yml``: reader and literate emitter.

The package depends on the Python standard library only, so it cannot use a
YAML library. Configuration needs a small, predictable subset, and anything
outside it is refused with its line number instead of being guessed at.

Accepted:

* comments: full-line, and trailing after whitespace (never inside quotes);
* block mappings, nested by indentation (spaces only);
* block sequences of scalars, of flow collections, and of mappings;
* one-line flow sequences ``[a, "b", 3]`` and flow mappings ``{a: 1}``,
  including the empty ``[]`` and ``{}``;
* scalars: ``null``/``~``/empty, ``true``/``false``, integers (decimal,
  ``0x``, ``0o``), floats, double-quoted strings with JSON escapes,
  single-quoted strings (``''`` is a quote), and one-line plain strings;
* an optional leading ``---`` document marker.

Refused, each with its line: tab indentation, anchors, aliases, tags,
directives, block scalars (``|``/``>``), multi-line plain or flow values,
complex keys (``?``), multiple documents, and duplicate keys.

Scalars follow YAML 1.2: ``yes``/``no``/``on``/``off`` are strings, not booleans
(so ``protect_system: no`` is refused by type validation rather than read as
false), ``0755`` is the integer 755, and ``1:30`` is a string.

A document whose first non-blank character is ``{`` is JSON (also valid YAML)
and is decoded by :mod:`json`, still refusing duplicate keys.
"""

from __future__ import annotations

import json
import math
import re
from collections.abc import Callable, Iterable, Mapping, Sequence

FORMATS = ("yaml", "json")

_INT_RE = re.compile(r"^[-+]?[0-9]+$")
_HEX_RE = re.compile(r"^0x[0-9a-fA-F]+$")
_OCT_RE = re.compile(r"^0o[0-7]+$")
_FLOAT_RE = re.compile(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$")
_INF_RE = re.compile(r"^[-+]?\.(inf|Inf|INF)$")
_NAN_RE = re.compile(r"^\.(nan|NaN|NAN)$")
_NULLS = frozenset({"null", "Null", "NULL", "~", ""})
_TRUES = frozenset({"true", "True", "TRUE"})
_FALSES = frozenset({"false", "False", "FALSE"})
#: Plain words other YAML readers (YAML 1.1) turn into booleans; always quoted when written.
_YAML11_WORDS = frozenset(
    word
    for base in ("yes", "no", "on", "off", "y", "n")
    for word in (base, base.capitalize(), base.upper())
)
#: Characters that may not begin a plain scalar or plain key.
_PLAIN_FORBIDDEN_START = frozenset("-?:,[]{}#&*!|>'\"%@`")


class YamlError(ValueError):
    """A document is outside the accepted subset or malformed; ``line`` is 1-based."""

    def __init__(self, message: str, line: int | None = None) -> None:
        super().__init__(message if line is None else f"line {line}: {message}")
        self.reason = message
        self.line = line


# ------------------------------------------------------------------ reading


def detect_format(text: str) -> str:
    """``json`` when the first non-blank character is ``{``, else ``yaml``."""

    stripped = text.lstrip()
    return "json" if stripped.startswith("{") else "yaml"


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate key {key!r}")
        value[key] = item
    return value


def loads_json(text: str) -> object:
    """Decode JSON, refusing duplicate keys at every depth."""

    try:
        return json.loads(text, object_pairs_hook=_unique_object)
    except json.JSONDecodeError as exc:
        raise YamlError(f"malformed JSON: {exc.msg} (column {exc.colno})", exc.lineno) from exc
    except (ValueError, RecursionError) as exc:
        raise YamlError(f"malformed JSON: {exc}") from exc


def load_document(text: str) -> tuple[object, str]:
    """Decode a configuration document; return ``(value, format)``."""

    if detect_format(text) == "json":
        return loads_json(text), "json"
    return loads(text), "yaml"


class _Line:
    __slots__ = ("number", "indent", "text")

    def __init__(self, number: int, indent: int, text: str) -> None:
        self.number = number
        self.indent = indent
        self.text = text


def _strip_comment(text: str, number: int) -> str:
    """Remove a trailing comment, honoring quotes; return the right-stripped content."""

    quote: str | None = None
    index = 0
    while index < len(text):
        character = text[index]
        if quote == '"':
            if character == "\\":
                index += 2
                continue
            if character == '"':
                quote = None
        elif quote == "'":
            if character == "'":
                if index + 1 < len(text) and text[index + 1] == "'":
                    index += 2
                    continue
                quote = None
        elif character == "#" and (index == 0 or text[index - 1] in " \t"):
            return text[:index].rstrip()
        elif character in "\"'" and (index == 0 or text[index - 1] in " \t[{,:-"):
            quote = character
        index += 1
    if quote is not None:
        raise YamlError("unterminated quoted string", number)
    return text.rstrip()


def _logical_lines(text: str) -> list[_Line]:
    lines: list[_Line] = []
    seen_content = False
    for number, raw in enumerate(text.splitlines(), start=1):
        body = raw.lstrip(" ")
        leading = raw[: len(raw) - len(body)]
        if body.startswith("\t") or "\t" in leading:
            if body.strip() == "":
                continue
            raise YamlError("tabs are not allowed for indentation", number)
        content = _strip_comment(body, number)
        if not content:
            continue
        if content.startswith("%"):
            raise YamlError("directives are not supported", number)
        if content in ("---", "...") or content.startswith("--- ") or content.startswith("... "):
            if content == "---" and not seen_content and not leading:
                continue
            raise YamlError("multiple documents and document markers are not supported", number)
        seen_content = True
        lines.append(_Line(number, len(leading), content))
    return lines


def _split_key(text: str, number: int) -> tuple[str, str] | None:
    """Split ``key: value`` at the first mapping colon outside quotes, or return None."""

    if text.startswith("? ") or text == "?":
        raise YamlError("complex keys (?) are not supported", number)
    if text[0] in "\"'":
        quote = text[0]
        index = 1
        while index < len(text):
            if quote == '"' and text[index] == "\\":
                index += 2
                continue
            if text[index] == quote:
                if quote == "'" and index + 1 < len(text) and text[index + 1] == "'":
                    index += 2
                    continue
                break
            index += 1
        else:
            raise YamlError("unterminated quoted key", number)
        rest = text[index + 1 :]
        if rest == ":" or rest.startswith(": "):
            key = _quoted_scalar(text[: index + 1], number)
            assert isinstance(key, str)
            return key, rest[1:].strip()
        return None
    if text[0] in _PLAIN_FORBIDDEN_START:
        return None
    match = re.search(r":(?: |$)", text)
    if match is None:
        return None
    key = text[: match.start()].rstrip()
    if not key or key != key.strip():
        return None
    if key[0] in "&*!":
        raise YamlError("anchors, aliases, and tags are not supported", number)
    return key, text[match.end() :].strip()


def _quoted_scalar(text: str, number: int) -> str:
    if text[0] == '"':
        try:
            value = json.loads(text)
        except json.JSONDecodeError as exc:
            raise YamlError(f"invalid double-quoted string: {exc.msg}", number) from exc
        if not isinstance(value, str):
            raise YamlError("invalid double-quoted string", number)
        return value
    if len(text) < 2 or not text.endswith("'"):
        raise YamlError("unterminated single-quoted string", number)
    inner = text[1:-1]
    if re.search(r"(?<!')'(?!')", inner.replace("''", "")):
        raise YamlError("stray quote in single-quoted string", number)
    return inner.replace("''", "'")


def _plain_scalar(text: str, number: int) -> object:
    if text in _NULLS:
        return None
    if text in _TRUES:
        return True
    if text in _FALSES:
        return False
    if _INT_RE.match(text):
        return int(text, 10)
    if _HEX_RE.match(text):
        return int(text[2:], 16)
    if _OCT_RE.match(text):
        return int(text[2:], 8)
    if _FLOAT_RE.match(text):
        return float(text)
    if _INF_RE.match(text):
        return -math.inf if text.startswith("-") else math.inf
    if _NAN_RE.match(text):
        return math.nan
    first = text[0]
    if first in "&*!":
        raise YamlError("anchors, aliases, and tags are not supported", number)
    if first in "|>":
        raise YamlError("block scalars (| and >) are not supported", number)
    if first in "%@`":
        raise YamlError(f"a plain value may not begin with {first!r}; quote it", number)
    if first in "?:-," and (len(text) == 1 or text[1] == " "):
        raise YamlError(f"unexpected {first!r}; quote the value", number)
    if ": " in text or text.endswith(":"):
        raise YamlError("a plain value may not contain ': '; quote it", number)
    if first in "]}":
        raise YamlError(f"unexpected {first!r}", number)
    return text


def _scalar(text: str, number: int) -> object:
    if text[0] in "\"'":
        return _quoted_scalar(text, number)
    if text[0] in "[{":
        return _flow(text, number)
    return _plain_scalar(text, number)


def _flow(text: str, number: int) -> object:
    """Parse one complete, one-line flow collection."""

    value, end = _flow_value(text, 0, number)
    if text[end:].strip():
        raise YamlError(f"unexpected text after a flow collection: {text[end:].strip()!r}", number)
    return value


def _skip_spaces(text: str, index: int) -> int:
    while index < len(text) and text[index] == " ":
        index += 1
    return index


def _flow_value(text: str, index: int, number: int) -> tuple[object, int]:
    index = _skip_spaces(text, index)
    if index >= len(text):
        raise YamlError("multi-line flow collections are not supported; close it on one line", number)
    opener = text[index]
    if opener == "[":
        items: list[object] = []
        index = _skip_spaces(text, index + 1)
        if index < len(text) and text[index] == "]":
            return items, index + 1
        while True:
            item, index = _flow_value(text, index, number)
            items.append(item)
            index = _skip_spaces(text, index)
            if index >= len(text):
                raise YamlError("multi-line flow collections are not supported; close it on one line", number)
            if text[index] == "]":
                return items, index + 1
            if text[index] != ",":
                raise YamlError(f"expected ',' or ']' in a flow sequence, found {text[index]!r}", number)
            index += 1
    if opener == "{":
        mapping: dict[str, object] = {}
        index = _skip_spaces(text, index + 1)
        if index < len(text) and text[index] == "}":
            return mapping, index + 1
        while True:
            key_value, index = _flow_value(text, index, number)
            index = _skip_spaces(text, index)
            if index >= len(text) or text[index] != ":":
                raise YamlError("expected ':' after a flow mapping key", number)
            if not isinstance(key_value, str):
                raise YamlError("flow mapping keys must be strings", number)
            if key_value in mapping:
                raise YamlError(f"duplicate key {key_value!r}", number)
            item, index = _flow_value(text, index + 1, number)
            mapping[key_value] = item
            index = _skip_spaces(text, index)
            if index >= len(text):
                raise YamlError("multi-line flow collections are not supported; close it on one line", number)
            if text[index] == "}":
                return mapping, index + 1
            if text[index] != ",":
                raise YamlError(f"expected ',' or '}}' in a flow mapping, found {text[index]!r}", number)
            index += 1
    if opener in "\"'":
        end = index + 1
        while end < len(text):
            if opener == '"' and text[end] == "\\":
                end += 2
                continue
            if text[end] == opener:
                if opener == "'" and end + 1 < len(text) and text[end + 1] == "'":
                    end += 2
                    continue
                break
            end += 1
        else:
            raise YamlError("unterminated quoted string", number)
        return _quoted_scalar(text[index : end + 1], number), end + 1
    end = index
    while end < len(text) and text[end] not in ",]}" and not (
        text[end] == ":" and (end + 1 >= len(text) or text[end + 1] in " ,]}")
    ):
        end += 1
    token = text[index:end].strip()
    if not token:
        raise YamlError("empty entry in a flow collection", number)
    return _plain_scalar(token, number), end


class _Parser:
    def __init__(self, lines: list[_Line]) -> None:
        self.lines = lines
        self.position = 0

    def peek(self) -> _Line | None:
        return self.lines[self.position] if self.position < len(self.lines) else None

    def parse_document(self) -> object:
        first = self.peek()
        if first is None:
            return None
        if first.indent != 0:
            raise YamlError("the document must start at column 1", first.number)
        value = self.parse_block(0)
        leftover = self.peek()
        if leftover is not None:
            raise YamlError("unexpected indentation or content", leftover.number)
        return value

    def parse_block(self, indent: int) -> object:
        line = self.peek()
        assert line is not None and line.indent == indent
        if line.text == "-" or line.text.startswith("- "):
            return self.parse_sequence(indent)
        if _split_key(line.text, line.number) is not None:
            return self.parse_mapping(indent)
        self.position += 1
        value = _scalar(line.text, line.number)
        following = self.peek()
        if following is not None and following.indent > indent:
            raise YamlError("multi-line values are not supported", following.number)
        return value

    def parse_mapping(self, indent: int, first_text: str | None = None, first_number: int = 0) -> dict[str, object]:
        mapping: dict[str, object] = {}
        pending = (first_text, first_number) if first_text is not None else None
        while True:
            if pending is not None:
                text, number = pending
                pending = None
            else:
                line = self.peek()
                if line is None or line.indent < indent:
                    return mapping
                if line.indent > indent:
                    raise YamlError("unexpected indentation", line.number)
                if line.text == "-" or line.text.startswith("- "):
                    raise YamlError("a sequence item cannot follow mapping keys at the same level", line.number)
                text, number = line.text, line.number
                self.position += 1
            split = _split_key(text, number)
            if split is None:
                raise YamlError(f"expected 'key: value', found {text!r}", number)
            key, rest = split
            if key in mapping:
                raise YamlError(f"duplicate key {key!r}", number)
            mapping[key] = self.parse_value(rest, indent, number, allow_same_indent_sequence=True)

    def parse_value(self, rest: str, indent: int, number: int, *, allow_same_indent_sequence: bool) -> object:
        following = self.peek()
        if rest:
            value = _scalar(rest, number)
            if following is not None and following.indent > indent:
                raise YamlError("multi-line values are not supported", following.number)
            return value
        if following is not None and following.indent > indent:
            return self.parse_block(following.indent)
        if (
            allow_same_indent_sequence
            and following is not None
            and following.indent == indent
            and (following.text == "-" or following.text.startswith("- "))
        ):
            return self.parse_sequence(indent)
        return None

    def parse_sequence(self, indent: int) -> list[object]:
        items: list[object] = []
        while True:
            line = self.peek()
            if line is None or line.indent < indent:
                return items
            if line.indent > indent:
                raise YamlError("unexpected indentation", line.number)
            if not (line.text == "-" or line.text.startswith("- ")):
                return items
            self.position += 1
            rest = line.text[1:].lstrip(" ")
            if not rest:
                following = self.peek()
                if following is not None and following.indent > indent:
                    items.append(self.parse_block(following.indent))
                else:
                    items.append(None)
                continue
            item_indent = indent + (len(line.text) - len(rest))
            if rest == "-" or rest.startswith("- "):
                raise YamlError("nested sequences on one line are not supported", line.number)
            if rest[0] not in "[{" and _split_key(rest, line.number) is not None:
                items.append(self.parse_mapping(item_indent, rest, line.number))
                continue
            items.append(self.parse_value(rest, indent, line.number, allow_same_indent_sequence=False))


def loads(text: str) -> object:
    """Decode a YAML-subset document (see the module docstring)."""

    if text.startswith("\ufeff"):
        text = text[1:]
    try:
        return _Parser(_logical_lines(text)).parse_document()
    except RecursionError as exc:
        raise YamlError("document is nested too deeply") from exc


# ------------------------------------------------------------------ writing

Comments = Mapping[tuple[str, ...], str]


def _wrap(text: str, prefix: str, width: int = 96) -> list[str]:
    lines: list[str] = []
    for paragraph in text.split("\n"):
        words = paragraph.split()
        if not words:
            lines.append(prefix.rstrip())
            continue
        current = prefix
        for word in words:
            if current != prefix and len(current) + 1 + len(word) > width:
                lines.append(current)
                current = prefix
            current = current + ("" if current == prefix else " ") + word
        lines.append(current)
    return lines


def comment_block(text: str, indent: int = 0) -> list[str]:
    """Render ``text`` as ``#`` comment lines at ``indent``."""

    return _wrap(text, " " * indent + "# ")


def scalar_text(value: object, *, flow: bool = False) -> str:
    """The canonical YAML text of one scalar; parsing it returns an equal value.

    ``flow`` quotes strings that would split a flow collection.
    """

    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if math.isnan(value):
            return ".nan"
        if math.isinf(value):
            return "-.inf" if value < 0 else ".inf"
        text = repr(value)
        return text if _FLOAT_RE.match(text) else json.dumps(value)
    if isinstance(value, str):
        if (
            value
            and value == value.strip()
            and value not in _YAML11_WORDS
            and not (flow and any(character in ",[]{}" for character in value))
            and not any(ord(character) < 32 or character in "#\"'" for character in value)
        ):
            try:
                if _plain_scalar(value, 0) == value and value[0] not in _PLAIN_FORBIDDEN_START:
                    return value
            except YamlError:
                pass
        return json.dumps(value, ensure_ascii=False)
    raise TypeError(f"cannot write {type(value).__name__} as a YAML scalar")


def _key_text(key: str) -> str:
    text = scalar_text(key)
    return text if text == key and ":" not in key else json.dumps(key, ensure_ascii=False)


def _is_scalar(value: object) -> bool:
    return value is None or isinstance(value, (bool, int, float, str))


def emit(
    value: Mapping[str, object],
    *,
    header: str = "",
    comments: Comments | None = None,
    order: Callable[[tuple[str, ...], Iterable[str]], list[str]] | None = None,
    footer: str = "",
) -> str:
    """Write ``value`` as literate YAML: a header, and a comment above each documented key.

    ``comments`` maps a key path (``("sandbox", "limits", "memory_max")``) to its
    comment. ``order`` sorts one mapping's keys given its path. The output
    parses back (:func:`loads`) to a value equal to ``value``.
    """

    notes = comments or {}
    arrange = order or (lambda _path, keys: list(keys))
    out: list[str] = []
    if header:
        out.extend(comment_block(header))
        out.append("")

    def mapping(node: Mapping[str, object], path: tuple[str, ...], indent: int) -> None:
        keys = arrange(path, node.keys())
        for position, key in enumerate(keys):
            item = node[key]
            here = (*path, key)
            note = notes.get(here)
            if note:
                if position and indent == 0:
                    out.append("")
                out.extend(comment_block(note, indent))
            prefix = " " * indent + _key_text(key) + ":"
            entry(item, here, indent, prefix)

    def entry(item: object, here: tuple[str, ...], indent: int, prefix: str) -> None:
        if _is_scalar(item):
            out.append(f"{prefix} {scalar_text(item)}")
        elif isinstance(item, Mapping):
            if not item:
                out.append(f"{prefix} {{}}")
            else:
                out.append(prefix)
                mapping({str(key): value for key, value in item.items()}, here, indent + 2)
        elif isinstance(item, Sequence):
            if not item:
                out.append(f"{prefix} []")
            else:
                out.append(prefix)
                sequence(item, here, indent + 2)
        else:
            raise TypeError(f"cannot write {type(item).__name__} to YAML")

    def sequence(items: Sequence[object], path: tuple[str, ...], indent: int) -> None:
        for item in items:
            if _is_scalar(item):
                out.append(" " * indent + "- " + scalar_text(item))
            elif isinstance(item, Mapping) and item:
                keys = list(item.keys())
                first, rest = keys[0], keys[1:]
                inner = indent + 2
                entry(item[first], (*path, str(first)), inner, " " * indent + "- " + _key_text(str(first)) + ":")
                for key in rest:
                    entry(item[key], (*path, str(key)), inner, " " * inner + _key_text(str(key)) + ":")
            elif isinstance(item, Mapping):
                out.append(" " * indent + "- {}")
            elif isinstance(item, Sequence) and not item:
                out.append(" " * indent + "- []")
            elif isinstance(item, Sequence) and all(_is_scalar(part) for part in item):
                out.append(
                    " " * indent + "- [" + ", ".join(scalar_text(part, flow=True) for part in item) + "]"
                )
            else:
                raise TypeError("nested non-scalar sequences cannot be written")

    mapping({str(key): item for key, item in value.items()}, (), 0)
    if footer:
        out.append("")
        out.extend(comment_block(footer))
    return "\n".join(out) + "\n"
