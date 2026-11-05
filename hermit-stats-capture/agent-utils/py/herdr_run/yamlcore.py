"""The YAML subset ``.herdr-run.yaml`` is written in, parsed by herdr-run itself.

A policy file must mean the same thing to every reader. Two general-purpose YAML libraries do not
agree with each other -- on tabs, directives, tags, explicit-tag semantics, aliases, nesting depth,
and every error message -- so herdr-run reads its configuration with this small parser, and the
native edition implements the identical rules. Shared golden cases are run by both editions'
unit tests.

What is accepted
----------------
* UTF-8 text. A leading byte-order mark is ignored; CRLF and CR line ends read as LF.
* One document: optional ``%YAML 1.1``/``%YAML 1.2`` directive lines, an optional ``---`` line, the
  content, and an optional ``...`` line. Comments start with ``#`` at a line start or after a space.
* Block mappings (``key: value``), block sequences (``- item``, including the compact forms
  ``key:`` followed by ``- item`` at the key's own indentation, ``- - item``, and ``- key: value``),
  and flow collections (``[a, b]``, ``{a: [b], c}``), which may span lines.
* Single-line plain, single-quoted, and double-quoted scalars (with every YAML double-quote escape).
* The tags ``!!str``, ``!!int``, ``!!float``, ``!!bool``, ``!!null`` on scalars, ``!!seq`` on a
  sequence and ``!!map`` on a mapping. ``!!int``, ``!!bool``, and ``!!null`` accept exactly the
  spellings plain scalars resolve to below; ``!!float`` accepts any decimal number, including an
  integer spelling with leading zeros (``!!float 007`` is ``7.0``); ``!!str`` accepts anything.
* Plain scalars resolve by herdr-run's own rule. It is close to the YAML 1.2 core schema but not
  the same, and this is the whole of it:

  - null: empty, ``~``, ``null``, ``Null``, ``NULL``;
  - bool: ``true``, ``True``, ``TRUE``, ``false``, ``False``, ``FALSE``;
  - integer: ``[-+]?(0|[1-9][0-9]*)`` (decimal WITHOUT leading zeros), ``[-+]?0o[0-7]+``,
    ``[-+]?0x[0-9a-fA-F]+``, and ``[-+]?0b[01]+``. The core schema reads ``007`` as 7, has no
    ``0b`` form, and does not sign ``0o``/``0x``; here ``007`` is a string and ``0b101`` is 5;
  - float: ``[-+]?(\\.[0-9]+|[0-9]+\\.[0-9]*)([eE][-+]?[0-9]+)?`` or ``[-+]?[0-9]+[eE][-+]?[0-9]+``;
    the ``.inf`` and ``.nan`` spellings are refused rather than read;
  - anything else -- ``yes``, ``on``, ``1_000``, ``007`` -- is a string.

What is refused, each with its own message and a line and column
-----------------------------------------------------------------
Anchors and aliases, block scalars (``|``, ``>``), explicit keys (``?``), multi-line plain and
quoted scalars, every other tag, ``%TAG`` and unknown directives, other ``%YAML`` versions, a second
document, non-string and duplicate mapping keys, merge keys (``<<``), non-finite numbers, a float
that overflows, a tab anywhere outside a quoted scalar or a comment, characters YAML does not allow
in a stream, a raw U+0085, U+2028, or U+2029 anywhere (YAML 1.1 reads each as a line break and YAML
1.2 does not), and collections nested more than 128 levels deep. An integer whose magnitude exceeds
10**38 reads as 10**38: it exceeds every bound the configuration checks.
"""

from __future__ import annotations

import math
import re
from dataclasses import dataclass

__all__ = ["MAX_DEPTH", "YamlError", "core_load"]

#: Deepest collection nesting accepted.
MAX_DEPTH = 128

#: Integers are clamped to this magnitude; larger values exceed every configuration bound anyway.
INTEGER_CLAMP = 10**38

_RE_INT_DEC = re.compile(r"[-+]?(?:0|[1-9][0-9]*)")
_RE_INT_OCT = re.compile(r"([-+]?)0o([0-7]+)")
_RE_INT_HEX = re.compile(r"([-+]?)0x([0-9a-fA-F]+)")
_RE_INT_BIN = re.compile(r"([-+]?)0b([01]+)")
_RE_FLOAT = re.compile(
    r"[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)(?:[eE][-+]?[0-9]+)?|[-+]?[0-9]+[eE][-+]?[0-9]+"
)
_RE_TAG_FLOAT = re.compile(r"[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?")
_NULL_TOKENS = frozenset({"", "~", "null", "Null", "NULL"})
_TRUE_TOKENS = frozenset({"true", "True", "TRUE"})
_FALSE_TOKENS = frozenset({"false", "False", "FALSE"})
_NONFINITE_TOKENS = frozenset(
    {
        ".inf",
        ".Inf",
        ".INF",
        "+.inf",
        "+.Inf",
        "+.INF",
        "-.inf",
        "-.Inf",
        "-.INF",
        ".nan",
        ".NaN",
        ".NAN",
    }
)
_SCALAR_TAGS = frozenset({"!!str", "!!int", "!!float", "!!bool", "!!null"})
_ALL_TAGS = _SCALAR_TAGS | {"!!seq", "!!map"}
_FLOW_INDICATORS = ",[]{}"
_ESCAPES = {
    "0": "\x00",
    "a": "\x07",
    "b": "\x08",
    "t": "\t",
    "\t": "\t",
    "n": "\n",
    "v": "\x0b",
    "f": "\x0c",
    "r": "\r",
    "e": "\x1b",
    " ": " ",
    '"': '"',
    "/": "/",
    "\\": "\\",
    "N": "\x85",
    "_": "\xa0",
    "L": "\u2028",
    "P": "\u2029",
}
_HEX_ESCAPES = {"x": 2, "u": 4, "U": 8}

_TAB_MESSAGE = "tab characters are only allowed inside quoted scalars and comments"

#: A ``:`` that starts a later line than its plain flow-mapping key and is followed by text.
_SPLIT_COLON_MESSAGE = (
    "a ':' on a later line than its plain key must be followed by a space: YAML readers "
    "disagree on whether it starts the value or continues the key"
)

#: NEXT LINE, LINE SEPARATOR, and PARAGRAPH SEPARATOR. YAML 1.1 breaks a line at each of them and
#: YAML 1.2 at none, so a file holding one raw means different things to different readers: a deny
#: entry written ``-c<U+2028>`` is ``-c`` to one and a longer name nothing matches to the other.
#: They are refused anywhere in the file, comments included; a double-quoted escape (``\N``,
#: ``\L``, ``\P``) still writes one explicitly, and the configuration rules then decide.
_AMBIGUOUS_BREAKS = frozenset({0x85, 0x2028, 0x2029})


def _ambiguous_break_message(code: int) -> str:
    return (
        f"U+{code:04X} is not allowed: YAML 1.1 reads it as a line break and YAML 1.2 does not"
    )


class YamlError(ValueError):
    """A configuration document outside the accepted subset, with a 1-based position."""

    def __init__(self, message: str, line: int, column: int) -> None:
        super().__init__(f"{message} at line {line} column {column}")
        self.message = message
        self.line = line
        self.column = column


def _quote(value: str) -> str:
    # Imported lazily so this module stays importable on its own in tests.
    from herdr_run.textfmt import quote

    return quote(value)


@dataclass(frozen=True)
class _Scalar:
    """A scalar before resolution: its text, whether it was plain, and where it started."""

    text: str
    plain: bool
    line: int
    column: int


class _Parser:
    def __init__(self, text: str) -> None:
        if text.startswith("\ufeff"):
            text = text[1:]
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        self.lines = text.split("\n")
        self.count = len(self.lines)
        for number, line in enumerate(self.lines):
            for column, character in enumerate(line):
                code = ord(character)
                if code in _AMBIGUOUS_BREAKS:
                    raise YamlError(_ambiguous_break_message(code), number + 1, column + 1)
                if not (
                    character == "\t"
                    or 0x20 <= code <= 0x7E
                    or 0xA0 <= code <= 0xD7FF
                    or 0xE000 <= code <= 0xFFFD
                    or 0x10000 <= code <= 0x10FFFF
                ):
                    raise YamlError(f"unacceptable character U+{code:04X}", number + 1, column + 1)

    # ---- positions and small scanners -------------------------------------------------------

    def error(self, message: str, line: int, column: int) -> YamlError:
        return YamlError(message, line + 1, column + 1)

    def char(self, line: int, column: int) -> str:
        text = self.lines[line]
        return text[column] if column < len(text) else ""

    def is_blank(self, line: int) -> bool:
        stripped = self.lines[line].lstrip(" ")
        return stripped == "" or stripped.startswith("#")

    def indent(self, line: int) -> int:
        text = self.lines[line]
        return len(text) - len(text.lstrip(" "))

    def is_marker(self, line: int, marker: str) -> bool:
        text = self.lines[line]
        return text.startswith(marker) and (len(text) == 3 or text[3] in " \t")

    def is_document_boundary(self, line: int) -> bool:
        return self.is_marker(line, "---") or self.is_marker(line, "...")

    def next_content(self, line: int) -> int | None:
        while line < self.count:
            if not self.is_blank(line):
                return line
            line += 1
        return None

    def skip_spaces(self, line: int, column: int) -> int:
        text = self.lines[line]
        while column < len(text):
            if text[column] == " ":
                column += 1
            elif text[column] == "\t":
                raise self.error(_TAB_MESSAGE, line, column)
            else:
                break
        return column

    def check_trailing(self, line: int, column: int) -> None:
        column = self.skip_spaces(line, column)
        text = self.lines[line]
        if column >= len(text):
            return
        if text[column] == "#" and column > 0 and text[column - 1] == " ":
            return
        raise self.error("unexpected text after a value", line, column)

    def is_dash(self, line: int, column: int) -> bool:
        text = self.lines[line]
        return (
            column < len(text)
            and text[column] == "-"
            and (column + 1 == len(text) or text[column + 1] in " \t")
        )

    # ---- tags and scalars -------------------------------------------------------------------

    def parse_tag(self, line: int, column: int, flow: bool) -> tuple[str, int]:
        text = self.lines[line]
        end = column
        while end < len(text) and text[end] not in " \t" and not (
            flow and text[end] in _FLOW_INDICATORS
        ):
            end += 1
        tag = text[column:end]
        if tag not in _ALL_TAGS:
            raise self.error(f"unsupported tag {_quote(tag)}", line, column)
        return tag, end

    def quote_end(self, text: str, column: int) -> int | None:
        """Index just past the closing quote that matches ``text[column]``, on this line."""
        quote = text[column]
        index = column + 1
        while index < len(text):
            character = text[index]
            if quote == "'":
                if character == "'":
                    if index + 1 < len(text) and text[index + 1] == "'":
                        index += 2
                        continue
                    return index + 1
            else:
                if character == "\\":
                    index += 2
                    continue
                if character == '"':
                    return index + 1
            index += 1
        return None

    def parse_quoted(self, line: int, column: int) -> tuple[_Scalar, int]:
        text = self.lines[line]
        quote = text[column]
        index = column + 1
        out: list[str] = []
        while True:
            if index >= len(text):
                raise self.error("quoted scalars must not span lines", line, column)
            character = text[index]
            if quote == "'":
                if character == "'":
                    if index + 1 < len(text) and text[index + 1] == "'":
                        out.append("'")
                        index += 2
                        continue
                    index += 1
                    break
                out.append(character)
                index += 1
                continue
            if character == '"':
                index += 1
                break
            if character != "\\":
                out.append(character)
                index += 1
                continue
            if index + 1 >= len(text):
                raise self.error("quoted scalars must not span lines", line, column)
            code = text[index + 1]
            if code in _ESCAPES:
                out.append(_ESCAPES[code])
                index += 2
                continue
            width = _HEX_ESCAPES.get(code)
            if width is None:
                raise self.error(f"invalid escape {_quote(chr(92) + code)}", line, index)
            digits = text[index + 2 : index + 2 + width]
            sequence = text[index : index + 2 + width]
            if len(digits) != width or any(d not in "0123456789abcdefABCDEF" for d in digits):
                raise self.error(f"invalid escape {_quote(sequence)}", line, index)
            value = int(digits, 16)
            if 0xD800 <= value <= 0xDFFF or value > 0x10FFFF:
                raise self.error(
                    f"escape {_quote(sequence)} is not a valid Unicode scalar value", line, index
                )
            out.append(chr(value))
            index += 2 + width
        return _Scalar("".join(out), False, line, column), index

    def plain_start_error(self, line: int, column: int, flow: bool) -> None:
        """Raise for a character that cannot begin a plain scalar here."""
        character = self.char(line, column)
        following = self.char(line, column + 1)
        safe_follow = following not in ("", " ", "\t") and not (
            flow and following in _FLOW_INDICATORS
        )
        if character in "&*":
            raise self.error("anchors and aliases are not supported", line, column)
        if character in "|>":
            raise self.error("block scalars (| and >) are not supported", line, column)
        # In a flow collection `?x` is a plain scalar to a YAML 1.2 reader and an explicit key `x`
        # to a YAML 1.1 reader, so it is refused there rather than read one way.
        if character == "?" and (flow or not safe_follow):
            raise self.error("explicit mapping keys (?) are not supported", line, column)
        if (
            character in ",[]{}#%@`!"
            or (character in "-:" and not safe_follow)
            or (character == ":" and flow)
        ):
            raise self.error(f"a plain scalar cannot start with {_quote(character)}", line, column)

    def parse_plain(self, line: int, column: int, flow: bool) -> tuple[_Scalar, int]:
        self.plain_start_error(line, column, flow)
        text = self.lines[line]
        index = column
        while index < len(text):
            character = text[index]
            if character == "\t":
                raise self.error(_TAB_MESSAGE, line, index)
            if character == ":":
                following = text[index + 1] if index + 1 < len(text) else ""
                if following in ("", " ", "\t") or (flow and following in _FLOW_INDICATORS):
                    break
            if character == "#" and index > column and text[index - 1] == " ":
                break
            if flow and character in _FLOW_INDICATORS:
                break
            index += 1
        value = text[column:index].rstrip(" ")
        return _Scalar(value, True, line, column), column + len(value)

    # ---- resolution -------------------------------------------------------------------------

    def resolve(self, scalar: _Scalar, tag: str | None, tag_line: int, tag_column: int) -> object:
        text = scalar.text
        if tag is None:
            if not scalar.plain:
                return text
            return self.resolve_plain(scalar)
        if tag == "!!str":
            return text
        if tag == "!!null":
            if text in ("~", "null", "Null", "NULL") or (text == "" and scalar.plain):
                return None
        elif tag == "!!bool":
            if text in _TRUE_TOKENS:
                return True
            if text in _FALSE_TOKENS:
                return False
        elif tag == "!!int":
            value = _integer(text)
            if value is not None:
                return value
        elif tag == "!!float":
            if text in _NONFINITE_TOKENS:
                raise self.error(
                    "non-finite numbers are not supported", scalar.line, scalar.column
                )
            if _RE_TAG_FLOAT.fullmatch(text):
                return self.finite(float(text), scalar)
        else:
            raise self.error(f"{tag} can only tag a {_collection_kind(tag)}", tag_line, tag_column)
        raise self.error(f"{_quote(text)} is not a valid {tag} value", scalar.line, scalar.column)

    def resolve_plain(self, scalar: _Scalar) -> object:
        text = scalar.text
        if text in _NULL_TOKENS:
            return None
        if text in _TRUE_TOKENS:
            return True
        if text in _FALSE_TOKENS:
            return False
        value = _integer(text)
        if value is not None:
            return value
        if text in _NONFINITE_TOKENS:
            raise self.error("non-finite numbers are not supported", scalar.line, scalar.column)
        if _RE_FLOAT.fullmatch(text):
            return self.finite(float(text), scalar)
        return text

    def finite(self, value: float, scalar: _Scalar) -> float:
        if not math.isfinite(value):
            raise self.error("number out of range", scalar.line, scalar.column)
        return value

    def tag_collection(self, value: object, tag: str | None, line: int, column: int) -> object:
        if tag is None:
            return value
        kind = "seq" if isinstance(value, list) else "map"
        if tag == f"!!{kind}":
            return value
        if tag in _SCALAR_TAGS:
            raise self.error(f"{tag} can only tag a scalar", line, column)
        raise self.error(f"{tag} can only tag a {_collection_kind(tag)}", line, column)

    # ---- keys -------------------------------------------------------------------------------

    def find_key_colon(self, line: int, column: int) -> int | None:
        """Column of the ``:`` that makes ``line[column:]`` a ``key: value`` entry, if it is one."""
        text = self.lines[line]
        index = column
        if index < len(text) and text[index] == "!":
            while index < len(text) and text[index] not in " \t":
                index += 1
            while index < len(text) and text[index] == " ":
                index += 1
        if index >= len(text):
            return None
        character = text[index]
        if character == "!":
            return None
        if character in "\"'":
            end = self.quote_end(text, index)
            if end is None:
                return None
            while end < len(text) and text[end] == " ":
                end += 1
            if end < len(text) and text[end] == ":" and (
                end + 1 == len(text) or text[end + 1] in " \t"
            ):
                return end
            return None
        if character in "[{&*|>%@`#,]}":
            return None
        if character in "-?:" and (index + 1 == len(text) or text[index + 1] in " \t"):
            return None
        start = index
        while index < len(text):
            character = text[index]
            if character == ":" and (index + 1 == len(text) or text[index + 1] in " \t"):
                return index
            if character == "#" and index > start and text[index - 1] in " \t":
                return None
            index += 1
        return None

    def key_text(self, scalar: _Scalar, tag: str | None, tag_line: int, tag_column: int) -> str:
        if tag is not None and tag not in _SCALAR_TAGS:
            raise self.error(f"{tag} can only tag a {_collection_kind(tag)}", tag_line, tag_column)
        key = self.resolve(scalar, tag, tag_line, tag_column)
        if not isinstance(key, str):
            raise self.error("mapping keys must be strings", scalar.line, scalar.column)
        return key

    def parse_block_key(self, line: int, column: int, colon: int) -> tuple[str, int, int]:
        text = self.lines[line]
        tag: str | None = None
        tag_column = column
        if text[column] == "!":
            tag, column = self.parse_tag(line, column, flow=False)
            column = self.skip_spaces(line, column)
        if text[column] in "\"'":
            scalar, _end = self.parse_quoted(line, column)
        else:
            raw = text[column:colon]
            if "\t" in raw:
                raise self.error(_TAB_MESSAGE, line, column + raw.index("\t"))
            scalar = _Scalar(raw.rstrip(" "), True, line, column)
        key = self.key_text(scalar, tag, line, tag_column)
        return key, scalar.column, colon + 1

    def insert(self, mapping: dict[str, object], key: str, line: int, column: int) -> None:
        if key in mapping:
            raise self.error(f"duplicate mapping key {_quote(key)}", line, column)
        if key == "<<":
            raise self.error("YAML merge keys are not supported", line, column)

    # ---- block structure --------------------------------------------------------------------

    def check_depth(self, depth: int, line: int, column: int) -> None:
        if depth > MAX_DEPTH:
            raise self.error(
                f"collections nested more than {MAX_DEPTH} levels deep", line, column
            )

    def parse_block_node(
        self, line: int, column: int, owner: int, depth: int
    ) -> tuple[object, int]:
        """Parse the node starting at ``(line, column)``; return it and the next line to read."""
        if self.is_dash(line, column):
            return self.parse_block_sequence(line, column, depth)
        colon = self.find_key_colon(line, column)
        if colon is not None:
            return self.parse_block_mapping(line, column, colon, depth)
        return self.parse_inline_value(line, column, owner, depth, tag=None)

    def parse_inline_value(
        self, line: int, column: int, owner: int, depth: int, tag: tuple[str, int, int] | None
    ) -> tuple[object, int]:
        text = self.lines[line]
        tag_name = tag[0] if tag else None
        if tag is None and column < len(text) and text[column] == "!":
            tag_name, after = self.parse_tag(line, column, flow=False)
            tag = (tag_name, line, column)
            column = self.skip_spaces(line, after)
            if column >= len(text) or (text[column] == "#" and text[column - 1] == " "):
                value = self.resolve(_Scalar("", True, line, column), tag_name, line, tag[2])
                self.check_continuation(line, owner, plain=True)
                return value, line + 1
        value, end_line, end_column, plain = self.parse_inline(line, column, depth)
        if isinstance(value, _Scalar):
            resolved = self.resolve(value, tag_name, tag[1] if tag else 0, tag[2] if tag else 0)
        else:
            resolved = self.tag_collection(
                value, tag_name, tag[1] if tag else 0, tag[2] if tag else 0
            )
        self.check_trailing(end_line, end_column)
        self.check_continuation(end_line, owner, plain=plain)
        return resolved, end_line + 1

    def check_continuation(self, line: int, owner: int, *, plain: bool) -> None:
        following = self.next_content(line + 1)
        if following is None or self.is_document_boundary(following):
            return
        indent = self.indent(following)
        if indent > owner:
            message = "plain scalars must not span lines" if plain else "unexpected indentation"
            raise self.error(message, following, indent)

    def parse_value(
        self, line: int, column: int, owner: int, depth: int, *, in_sequence: bool
    ) -> tuple[object, int]:
        """Parse what follows ``key:`` or ``-``; ``owner`` is the indentation of that key or dash."""
        text = self.lines[line]
        column = self.skip_spaces(line, column)
        tag: tuple[str, int, int] | None = None
        if column < len(text) and text[column] == "!":
            tag_name, after = self.parse_tag(line, column, flow=False)
            tag = (tag_name, line, column)
            column = self.skip_spaces(line, after)
        if column < len(text) and text[column] in "&*":
            raise self.error("anchors and aliases are not supported", line, column)
        if column >= len(text) or (text[column] == "#" and text[column - 1] == " "):
            return self.parse_following(line, owner, depth, in_sequence=in_sequence, tag=tag)
        if self.is_dash(line, column):
            if not in_sequence:
                raise self.error("block sequence entries are not allowed here", line, column)
            items, following = self.parse_block_sequence(line, column, depth)
            return self.apply_collection_tag(items, tag), following
        colon = self.find_key_colon(line, column)
        if colon is not None:
            if not in_sequence:
                raise self.error("mapping values are not allowed here", line, colon)
            mapping, following = self.parse_block_mapping(line, column, colon, depth)
            return self.apply_collection_tag(mapping, tag), following
        return self.parse_inline_value(line, column, owner, depth, tag=tag)

    def apply_collection_tag(
        self, value: object, tag: tuple[str, int, int] | None
    ) -> object:
        if tag is None:
            return value
        return self.tag_collection(value, tag[0], tag[1], tag[2])

    def parse_following(
        self,
        line: int,
        owner: int,
        depth: int,
        *,
        in_sequence: bool,
        tag: tuple[str, int, int] | None,
    ) -> tuple[object, int]:
        """The value of a ``key:`` or ``-`` with nothing after it on its own line."""
        following = self.next_content(line + 1)
        if following is not None and not self.is_document_boundary(following):
            indent = self.indent(following)
            if indent > owner:
                if tag is not None and self.lines[following][indent] == "!":
                    raise self.error(
                        "a node can have only one tag", following, indent
                    )
                value, after = self.parse_block_node(following, indent, owner, depth)
                if tag is None:
                    return value, after
                if isinstance(value, (list, dict)):
                    return self.tag_collection(value, tag[0], tag[1], tag[2]), after
                raise self.error(
                    "a tag must be on the same line as its scalar", tag[1], tag[2]
                )
            if not in_sequence and indent == owner and self.is_dash(following, indent):
                value, after = self.parse_block_sequence(following, indent, depth)
                return self.apply_collection_tag(value, tag), after
        if tag is None:
            return None, line + 1
        return self.resolve(_Scalar("", True, line, tag[2]), tag[0], tag[1], tag[2]), line + 1

    def parse_block_mapping(
        self, line: int, column: int, colon: int, depth: int
    ) -> tuple[dict[str, object], int]:
        self.check_depth(depth, line, column)
        mapping: dict[str, object] = {}
        indent = column
        while True:
            key, key_column, after = self.parse_block_key(line, column, colon)
            self.insert(mapping, key, line, key_column)
            value, following = self.parse_value(
                line, after, indent, depth + 1, in_sequence=False
            )
            mapping[key] = value
            line = following
            next_line = self.next_content(line)
            if next_line is None or self.is_document_boundary(next_line):
                return mapping, line
            next_indent = self.indent(next_line)
            if next_indent < indent:
                return mapping, line
            if next_indent > indent:
                raise self.error("unexpected indentation", next_line, next_indent)
            if self.is_dash(next_line, indent):
                raise self.error(
                    "block sequence entries are not allowed here", next_line, indent
                )
            found = self.find_key_colon(next_line, indent)
            if found is None:
                raise self.error(
                    "expected a mapping key followed by ':'", next_line, indent
                )
            line, column, colon = next_line, indent, found

    def parse_block_sequence(
        self, line: int, column: int, depth: int
    ) -> tuple[list[object], int]:
        self.check_depth(depth, line, column)
        items: list[object] = []
        indent = column
        while True:
            value, following = self.parse_value(
                line, column + 1, indent, depth + 1, in_sequence=True
            )
            items.append(value)
            line = following
            next_line = self.next_content(line)
            if next_line is None or self.is_document_boundary(next_line):
                return items, line
            next_indent = self.indent(next_line)
            if next_indent < indent:
                return items, line
            if next_indent > indent:
                raise self.error("unexpected indentation", next_line, next_indent)
            if not self.is_dash(next_line, indent):
                return items, line
            line, column = next_line, indent

    # ---- inline and flow nodes ----------------------------------------------------------------

    def parse_inline(
        self, line: int, column: int, depth: int
    ) -> tuple[object, int, int, bool]:
        """A node written on one line, or a flow collection starting there.

        Returns ``(value, end_line, end_column, was_plain)``; a scalar comes back unresolved.
        """
        character = self.char(line, column)
        if character == "[":
            items, end_line, end_column = self.parse_flow_sequence(line, column, depth)
            return items, end_line, end_column, False
        if character == "{":
            mapping, end_line, end_column = self.parse_flow_mapping(line, column, depth)
            return mapping, end_line, end_column, False
        if character in "\"'":
            scalar, end = self.parse_quoted(line, column)
            return scalar, line, end, False
        scalar, end = self.parse_plain(line, column, flow=False)
        return scalar, line, end, True

    def skip_flow(self, line: int, column: int, opener: tuple[int, int, str]) -> tuple[int, int]:
        """Skip spaces, line breaks, and comments inside a flow collection."""
        while True:
            if line >= self.count:
                raise self.error(f"unterminated flow {opener[2]}", opener[0], opener[1])
            text = self.lines[line]
            if column >= len(text):
                line += 1
                column = 0
                if line < self.count and self.is_document_boundary(line):
                    raise self.error(f"unterminated flow {opener[2]}", opener[0], opener[1])
                continue
            character = text[column]
            if character == " ":
                column += 1
            elif character == "\t":
                raise self.error(_TAB_MESSAGE, line, column)
            elif character == "#" and (column == 0 or text[column - 1] == " "):
                column = len(text)
            else:
                return line, column

    def parse_flow_node(
        self, line: int, column: int, depth: int, opener: tuple[int, int, str]
    ) -> tuple[object, int, int, bool]:
        """One entry of a flow collection; returns ``(value, line, column, was_plain)``."""
        tag: str | None = None
        tag_line, tag_column = line, column
        if self.char(line, column) == "!":
            tag, column = self.parse_tag(line, column, flow=True)
            line, column = self.skip_flow(line, column, opener)
        character = self.char(line, column)
        if character in ",]}":
            if tag is None:
                raise self.error("expected a value in a flow collection", line, column)
            return self.resolve(_Scalar("", True, line, column), tag, tag_line, tag_column), line, column, False
        if character == "[":
            items, line, column = self.parse_flow_sequence(line, column, depth)
            return self.tag_collection(items, tag, tag_line, tag_column), line, column, False
        if character == "{":
            mapping, line, column = self.parse_flow_mapping(line, column, depth)
            return self.tag_collection(mapping, tag, tag_line, tag_column), line, column, False
        if character in "\"'":
            scalar, end = self.parse_quoted(line, column)
            return self.resolve(scalar, tag, tag_line, tag_column), line, end, False
        scalar, end = self.parse_plain(line, column, flow=True)
        return self.resolve(scalar, tag, tag_line, tag_column), line, end, True

    def flow_separator_error(
        self, line: int, column: int, crossed: bool, plain: bool, expected: str
    ) -> YamlError:
        if plain and crossed:
            return self.error("plain scalars must not span lines", line, column)
        return self.error(expected, line, column)

    def parse_flow_sequence(
        self, line: int, column: int, depth: int
    ) -> tuple[list[object], int, int]:
        self.check_depth(depth, line, column)
        opener = (line, column, "sequence")
        items: list[object] = []
        column += 1
        while True:
            line, column = self.skip_flow(line, column, opener)
            if self.char(line, column) == "]":
                return items, line, column + 1
            value, end_line, end_column, plain = self.parse_flow_node(
                line, column, depth + 1, opener
            )
            items.append(value)
            line, column = self.skip_flow(end_line, end_column, opener)
            character = self.char(line, column)
            if character == ",":
                column += 1
                continue
            if character == "]":
                return items, line, column + 1
            if character == ":":
                raise self.error("a flow sequence entry cannot be a mapping", line, column)
            raise self.flow_separator_error(
                line, column, line != end_line, plain, "expected ',' or ']' in a flow sequence"
            )

    def parse_flow_mapping(
        self, line: int, column: int, depth: int
    ) -> tuple[dict[str, object], int, int]:
        self.check_depth(depth, line, column)
        opener = (line, column, "mapping")
        mapping: dict[str, object] = {}
        column += 1
        while True:
            line, column = self.skip_flow(line, column, opener)
            character = self.char(line, column)
            if character == "}":
                return mapping, line, column + 1
            tag: str | None = None
            tag_line, tag_column = line, column
            if character == "!":
                tag, column = self.parse_tag(line, column, flow=True)
                line, column = self.skip_flow(line, column, opener)
                character = self.char(line, column)
            if character in "[{":
                raise self.error("mapping keys must be strings", line, column)
            if character in ",}":
                raise self.error("expected a value in a flow collection", line, column)
            if character in "\"'":
                scalar, key_end = self.parse_quoted(line, column)
                plain = False
            else:
                scalar, key_end = self.parse_plain(line, column, flow=True)
                plain = True
            key = self.key_text(scalar, tag, tag_line, tag_column)
            self.insert(mapping, key, scalar.line, scalar.column)
            key_line = line
            line, column = self.skip_flow(line, key_end, opener)
            character = self.char(line, column)
            value: object = None
            if character == ":":
                following = self.char(line, column + 1)
                if (
                    plain
                    and line != key_line
                    and following not in ("", " ", "\t")
                    and following not in _FLOW_INDICATORS
                ):
                    # `{x\n :y}`: other readers continue the plain key (`x :y`); refused rather
                    # than read as `x: y`.
                    raise self.error(_SPLIT_COLON_MESSAGE, line, column)
                line, column = self.skip_flow(line, column + 1, opener)
                if self.char(line, column) not in ",}":
                    value, end_line, end_column, plain = self.parse_flow_node(
                        line, column, depth + 1, opener
                    )
                    key_line = end_line
                    line, column = self.skip_flow(end_line, end_column, opener)
                else:
                    plain = False
                    key_line = line
            elif character not in ",}":
                raise self.flow_separator_error(
                    line, column, line != key_line, plain, "expected ':' after a flow mapping key"
                )
            mapping[key] = value
            character = self.char(line, column)
            if character == ",":
                column += 1
                continue
            if character == "}":
                return mapping, line, column + 1
            raise self.flow_separator_error(
                line, column, line != key_line, plain, "expected ',' or '}' in a flow mapping"
            )

    # ---- the document -----------------------------------------------------------------------

    def parse_directive(self, line: int, seen_yaml: bool) -> None:
        text = self.lines[line]
        end = 1
        while end < len(text) and text[end] not in " \t":
            end += 1
        name = text[1:end]
        if name == "TAG":
            raise self.error("%TAG directives are not supported", line, 0)
        if name != "YAML":
            raise self.error(f"unknown directive {_quote('%' + name)}", line, 0)
        if seen_yaml:
            raise self.error("duplicate %YAML directive", line, 0)
        column = self.skip_spaces(line, end)
        version_end = column
        while version_end < len(text) and text[version_end] not in " \t":
            version_end += 1
        version = text[column:version_end]
        if not version or version.startswith("#"):
            raise self.error("the %YAML directive needs a version", line, column)
        if version not in ("1.1", "1.2"):
            raise self.error(f"unsupported YAML version {_quote(version)}", line, column)
        self.check_trailing(line, version_end)

    def parse(self) -> object:
        line = 0
        seen_yaml = False
        while line < self.count:
            if self.is_blank(line):
                line += 1
                continue
            if self.lines[line].startswith("%"):
                self.parse_directive(line, seen_yaml)
                seen_yaml = True
                line += 1
                continue
            break
        if line < self.count and self.is_marker(line, "---"):
            self.check_marker_rest(line, "'---'")
            line += 1
        elif seen_yaml:
            at = line if line < self.count else self.count - 1
            raise self.error("a directive must be followed by '---'", at, 0)
        value: object = None
        first = self.next_content(line)
        after = line
        if first is not None and not self.is_document_boundary(first):
            value, after = self.parse_block_node(first, self.indent(first), -1, 1)
        rest = self.next_content(after)
        if rest is None:
            return value
        if self.is_marker(rest, "..."):
            self.check_marker_rest(rest, "'...'")
            trailing = self.next_content(rest + 1)
            if trailing is not None:
                raise self.error("multiple documents are not supported", trailing, 0)
            return value
        if self.is_marker(rest, "---"):
            raise self.error("multiple documents are not supported", rest, 0)
        raise self.error("expected the end of the document", rest, self.indent(rest))

    def check_marker_rest(self, line: int, marker: str) -> None:
        text = self.lines[line]
        column = self.skip_spaces(line, 3)
        if column >= len(text) or (text[column] == "#" and text[column - 1] == " "):
            return
        raise self.error(f"content after {marker} is not supported", line, column)


def _collection_kind(tag: str) -> str:
    return {"!!seq": "sequence", "!!map": "mapping"}.get(tag, "scalar")


def _clamp(value: int) -> int:
    if value > INTEGER_CLAMP:
        return INTEGER_CLAMP
    if value < -INTEGER_CLAMP:
        return -INTEGER_CLAMP
    return value


def _integer(text: str) -> int | None:
    """Resolve an integer spelling (see the module docstring), clamped to +/-10**38, or ``None``."""
    if _RE_INT_DEC.fullmatch(text):
        sign = -1 if text.startswith("-") else 1
        digits = text.lstrip("+-")
        if len(digits) > 39:
            return sign * INTEGER_CLAMP
        return _clamp(sign * int(digits, 10))
    for pattern, base in ((_RE_INT_OCT, 8), (_RE_INT_HEX, 16), (_RE_INT_BIN, 2)):
        match = pattern.fullmatch(text)
        if match is not None:
            sign = -1 if match.group(1) == "-" else 1
            digits = match.group(2).lstrip("0") or "0"
            if len(digits) > 200:
                return sign * INTEGER_CLAMP
            return _clamp(sign * int(digits, base))
    return None


def core_load(text: str) -> object:
    """Parse one configuration document, raising :class:`YamlError` outside the subset."""
    return _Parser(text).parse()
