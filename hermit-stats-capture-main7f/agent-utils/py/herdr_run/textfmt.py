"""Text rules shared by both editions of herdr-run, written out rather than borrowed.

Everything in this module exists because the obvious language built-in gives an answer that
depends on the language, or on the version of the language's Unicode tables: ``repr()`` and
``str.isprintable`` follow the interpreter's Unicode database, ``str.strip`` treats the
information separators U+001C..U+001F as whitespace, ``int()`` accepts underscores and every
script's decimal digits, and ``json.loads`` accepts ``NaN``. Each function here states its rule
explicitly so the same input produces the same bytes on every interpreter, and the native edition
implements the identical rule. Shared golden cases, loaded by both editions' unit tests, hold the
two to it, down to the name rule's verdict on every one of the 1,114,112 code points.
"""

from __future__ import annotations

import json
import math
import re
from bisect import bisect_right
from decimal import Decimal

__all__ = [
    "ASCII_WHITESPACE",
    "BLANK_CHARACTERS",
    "DEFAULT_IGNORABLE_RANGES",
    "FORMAT_CHARACTER_RANGES",
    "JSON_MAX_DEPTH",
    "PRIVATE_USE_RANGES",
    "UNASSIGNED_RANGES",
    "UNICODE_VERSION",
    "UNICODE_WHITESPACE",
    "display",
    "format_round3",
    "is_blank",
    "is_default_ignorable",
    "is_format_character",
    "is_invisible",
    "is_noncharacter",
    "is_private_use",
    "is_unassigned",
    "json_text",
    "name_violation",
    "number_text",
    "os_error_text",
    "parse_exit_marker",
    "quote",
    "quote_optional",
    "rstrip_space",
    "strict_json_loads",
    "strip_ascii",
    "strip_space",
]

#: The six ASCII whitespace characters POSIX ``isspace`` names, trimmed from an exit-code marker.
#: Information separators and every non-ASCII space are kept, so they make a marker invalid rather
#: than being silently removed.
ASCII_WHITESPACE = " \t\n\r\x0b\x0c"

#: The Unicode ``White_Space`` property, written out. Used where text that came from a terminal is
#: judged blank or trimmed. The list has been stable since Unicode 6.3; writing it out keeps it from
#: following whichever Unicode version an interpreter or compiler happens to ship.
UNICODE_WHITESPACE = frozenset(
    "\t\n\x0b\x0c\r \x85\xa0\u1680"
    "\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a"
    "\u2028\u2029\u202f\u205f\u3000"
)

_UNICODE_WHITESPACE_TEXT = "".join(sorted(UNICODE_WHITESPACE))

#: The Unicode version every table below is written out from. A character a later version assigns
#: is still unassigned here, so a name holding one is refused until the tables are brought forward,
#: in both editions at once.
UNICODE_VERSION = "16.0.0"

#: Unicode general category ``Cf`` (format characters), written out as of Unicode 16.0, whose list
#: is Unicode 15.0's: the soft hyphen, zero-width spaces and joiners, bidirectional marks,
#: embeddings, overrides and isolates, the byte-order mark, interlinear annotation, tag characters,
#: and the rest. None of them draws anything, so a name that contains one looks exactly like a name
#: that does not.
FORMAT_CHARACTER_RANGES: tuple[tuple[int, int], ...] = (
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
)


#: Unicode ``Default_Ignorable_Code_Point``, written out as of Unicode 16.0: characters a renderer
#: shows as nothing at all unless it supports them specially. Besides most format characters it
#: holds the combining grapheme joiner (U+034F), the Khmer inherent vowels (U+17B4, U+17B5), the
#: Mongolian free variation selectors (U+180B..U+180D, U+180F), the variation selectors
#: (U+FE00..U+FE0F, U+E0100..U+E01EF), the Hangul fillers, and code points reserved for more of
#: the same (U+2065, U+FFF0..U+FFF8, and the unassigned rest of U+E0000..U+E0FFF), so a
#: character a later Unicode version assigns there is refused too.
DEFAULT_IGNORABLE_RANGES: tuple[tuple[int, int], ...] = (
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
)

#: Assigned, ordinary characters that nevertheless draw as empty space: the Hangul fillers
#: (U+115F, U+1160, U+3164, U+FFA0, which are also default-ignorable), and three that are neither
#: default-ignorable nor white space and so need naming on their own: U+2800 BRAILLE PATTERN BLANK,
#: U+16FE4 KHITAN SMALL SCRIPT FILLER (a nonspacing mark that draws nothing), and U+1D159 MUSICAL
#: SYMBOL NULL NOTEHEAD (a symbol that draws nothing).
BLANK_CHARACTERS: frozenset[str] = frozenset(
    "\u115f\u1160\u2800\u3164\uffa0\U00016fe4\U0001d159"
)

#: Unicode general category ``Co`` (private use), unchanged since Unicode 2.0: the private use area
#: U+E000..U+F8FF and planes 15 and 16 except the last two code points of each, which are
#: noncharacters. What a private-use character draws depends on the font in use, if it draws at
#: all, so the same name can look different, or empty, from one terminal to the next.
PRIVATE_USE_RANGES: tuple[tuple[int, int], ...] = (
    (0xE000, 0xF8FF),
    (0xF0000, 0xFFFFD),
    (0x100000, 0x10FFFD),
)

#: Unicode general category ``Cn`` (unassigned), written out as of Unicode 16.0, sorted. It holds
#: the noncharacters and the reserved default-ignorable code points too, which
#: :func:`name_violation` names by those more specific descriptions first. A code point with no
#: character draws as a box, as nothing, or -- once a later Unicode version assigns it and a font
#: catches up -- as something this table has no way to know about.
UNASSIGNED_RANGES: tuple[tuple[int, int], ...] = (
    (0x0378, 0x0379), (0x0380, 0x0383), (0x038B, 0x038B), (0x038D, 0x038D),
    (0x03A2, 0x03A2), (0x0530, 0x0530), (0x0557, 0x0558), (0x058B, 0x058C),
    (0x0590, 0x0590), (0x05C8, 0x05CF), (0x05EB, 0x05EE), (0x05F5, 0x05FF),
    (0x070E, 0x070E), (0x074B, 0x074C), (0x07B2, 0x07BF), (0x07FB, 0x07FC),
    (0x082E, 0x082F), (0x083F, 0x083F), (0x085C, 0x085D), (0x085F, 0x085F),
    (0x086B, 0x086F), (0x088F, 0x088F), (0x0892, 0x0896), (0x0984, 0x0984),
    (0x098D, 0x098E), (0x0991, 0x0992), (0x09A9, 0x09A9), (0x09B1, 0x09B1),
    (0x09B3, 0x09B5), (0x09BA, 0x09BB), (0x09C5, 0x09C6), (0x09C9, 0x09CA),
    (0x09CF, 0x09D6), (0x09D8, 0x09DB), (0x09DE, 0x09DE), (0x09E4, 0x09E5),
    (0x09FF, 0x0A00), (0x0A04, 0x0A04), (0x0A0B, 0x0A0E), (0x0A11, 0x0A12),
    (0x0A29, 0x0A29), (0x0A31, 0x0A31), (0x0A34, 0x0A34), (0x0A37, 0x0A37),
    (0x0A3A, 0x0A3B), (0x0A3D, 0x0A3D), (0x0A43, 0x0A46), (0x0A49, 0x0A4A),
    (0x0A4E, 0x0A50), (0x0A52, 0x0A58), (0x0A5D, 0x0A5D), (0x0A5F, 0x0A65),
    (0x0A77, 0x0A80), (0x0A84, 0x0A84), (0x0A8E, 0x0A8E), (0x0A92, 0x0A92),
    (0x0AA9, 0x0AA9), (0x0AB1, 0x0AB1), (0x0AB4, 0x0AB4), (0x0ABA, 0x0ABB),
    (0x0AC6, 0x0AC6), (0x0ACA, 0x0ACA), (0x0ACE, 0x0ACF), (0x0AD1, 0x0ADF),
    (0x0AE4, 0x0AE5), (0x0AF2, 0x0AF8), (0x0B00, 0x0B00), (0x0B04, 0x0B04),
    (0x0B0D, 0x0B0E), (0x0B11, 0x0B12), (0x0B29, 0x0B29), (0x0B31, 0x0B31),
    (0x0B34, 0x0B34), (0x0B3A, 0x0B3B), (0x0B45, 0x0B46), (0x0B49, 0x0B4A),
    (0x0B4E, 0x0B54), (0x0B58, 0x0B5B), (0x0B5E, 0x0B5E), (0x0B64, 0x0B65),
    (0x0B78, 0x0B81), (0x0B84, 0x0B84), (0x0B8B, 0x0B8D), (0x0B91, 0x0B91),
    (0x0B96, 0x0B98), (0x0B9B, 0x0B9B), (0x0B9D, 0x0B9D), (0x0BA0, 0x0BA2),
    (0x0BA5, 0x0BA7), (0x0BAB, 0x0BAD), (0x0BBA, 0x0BBD), (0x0BC3, 0x0BC5),
    (0x0BC9, 0x0BC9), (0x0BCE, 0x0BCF), (0x0BD1, 0x0BD6), (0x0BD8, 0x0BE5),
    (0x0BFB, 0x0BFF), (0x0C0D, 0x0C0D), (0x0C11, 0x0C11), (0x0C29, 0x0C29),
    (0x0C3A, 0x0C3B), (0x0C45, 0x0C45), (0x0C49, 0x0C49), (0x0C4E, 0x0C54),
    (0x0C57, 0x0C57), (0x0C5B, 0x0C5C), (0x0C5E, 0x0C5F), (0x0C64, 0x0C65),
    (0x0C70, 0x0C76), (0x0C8D, 0x0C8D), (0x0C91, 0x0C91), (0x0CA9, 0x0CA9),
    (0x0CB4, 0x0CB4), (0x0CBA, 0x0CBB), (0x0CC5, 0x0CC5), (0x0CC9, 0x0CC9),
    (0x0CCE, 0x0CD4), (0x0CD7, 0x0CDC), (0x0CDF, 0x0CDF), (0x0CE4, 0x0CE5),
    (0x0CF0, 0x0CF0), (0x0CF4, 0x0CFF), (0x0D0D, 0x0D0D), (0x0D11, 0x0D11),
    (0x0D45, 0x0D45), (0x0D49, 0x0D49), (0x0D50, 0x0D53), (0x0D64, 0x0D65),
    (0x0D80, 0x0D80), (0x0D84, 0x0D84), (0x0D97, 0x0D99), (0x0DB2, 0x0DB2),
    (0x0DBC, 0x0DBC), (0x0DBE, 0x0DBF), (0x0DC7, 0x0DC9), (0x0DCB, 0x0DCE),
    (0x0DD5, 0x0DD5), (0x0DD7, 0x0DD7), (0x0DE0, 0x0DE5), (0x0DF0, 0x0DF1),
    (0x0DF5, 0x0E00), (0x0E3B, 0x0E3E), (0x0E5C, 0x0E80), (0x0E83, 0x0E83),
    (0x0E85, 0x0E85), (0x0E8B, 0x0E8B), (0x0EA4, 0x0EA4), (0x0EA6, 0x0EA6),
    (0x0EBE, 0x0EBF), (0x0EC5, 0x0EC5), (0x0EC7, 0x0EC7), (0x0ECF, 0x0ECF),
    (0x0EDA, 0x0EDB), (0x0EE0, 0x0EFF), (0x0F48, 0x0F48), (0x0F6D, 0x0F70),
    (0x0F98, 0x0F98), (0x0FBD, 0x0FBD), (0x0FCD, 0x0FCD), (0x0FDB, 0x0FFF),
    (0x10C6, 0x10C6), (0x10C8, 0x10CC), (0x10CE, 0x10CF), (0x1249, 0x1249),
    (0x124E, 0x124F), (0x1257, 0x1257), (0x1259, 0x1259), (0x125E, 0x125F),
    (0x1289, 0x1289), (0x128E, 0x128F), (0x12B1, 0x12B1), (0x12B6, 0x12B7),
    (0x12BF, 0x12BF), (0x12C1, 0x12C1), (0x12C6, 0x12C7), (0x12D7, 0x12D7),
    (0x1311, 0x1311), (0x1316, 0x1317), (0x135B, 0x135C), (0x137D, 0x137F),
    (0x139A, 0x139F), (0x13F6, 0x13F7), (0x13FE, 0x13FF), (0x169D, 0x169F),
    (0x16F9, 0x16FF), (0x1716, 0x171E), (0x1737, 0x173F), (0x1754, 0x175F),
    (0x176D, 0x176D), (0x1771, 0x1771), (0x1774, 0x177F), (0x17DE, 0x17DF),
    (0x17EA, 0x17EF), (0x17FA, 0x17FF), (0x181A, 0x181F), (0x1879, 0x187F),
    (0x18AB, 0x18AF), (0x18F6, 0x18FF), (0x191F, 0x191F), (0x192C, 0x192F),
    (0x193C, 0x193F), (0x1941, 0x1943), (0x196E, 0x196F), (0x1975, 0x197F),
    (0x19AC, 0x19AF), (0x19CA, 0x19CF), (0x19DB, 0x19DD), (0x1A1C, 0x1A1D),
    (0x1A5F, 0x1A5F), (0x1A7D, 0x1A7E), (0x1A8A, 0x1A8F), (0x1A9A, 0x1A9F),
    (0x1AAE, 0x1AAF), (0x1ACF, 0x1AFF), (0x1B4D, 0x1B4D), (0x1BF4, 0x1BFB),
    (0x1C38, 0x1C3A), (0x1C4A, 0x1C4C), (0x1C8B, 0x1C8F), (0x1CBB, 0x1CBC),
    (0x1CC8, 0x1CCF), (0x1CFB, 0x1CFF), (0x1F16, 0x1F17), (0x1F1E, 0x1F1F),
    (0x1F46, 0x1F47), (0x1F4E, 0x1F4F), (0x1F58, 0x1F58), (0x1F5A, 0x1F5A),
    (0x1F5C, 0x1F5C), (0x1F5E, 0x1F5E), (0x1F7E, 0x1F7F), (0x1FB5, 0x1FB5),
    (0x1FC5, 0x1FC5), (0x1FD4, 0x1FD5), (0x1FDC, 0x1FDC), (0x1FF0, 0x1FF1),
    (0x1FF5, 0x1FF5), (0x1FFF, 0x1FFF), (0x2065, 0x2065), (0x2072, 0x2073),
    (0x208F, 0x208F), (0x209D, 0x209F), (0x20C1, 0x20CF), (0x20F1, 0x20FF),
    (0x218C, 0x218F), (0x242A, 0x243F), (0x244B, 0x245F), (0x2B74, 0x2B75),
    (0x2B96, 0x2B96), (0x2CF4, 0x2CF8), (0x2D26, 0x2D26), (0x2D28, 0x2D2C),
    (0x2D2E, 0x2D2F), (0x2D68, 0x2D6E), (0x2D71, 0x2D7E), (0x2D97, 0x2D9F),
    (0x2DA7, 0x2DA7), (0x2DAF, 0x2DAF), (0x2DB7, 0x2DB7), (0x2DBF, 0x2DBF),
    (0x2DC7, 0x2DC7), (0x2DCF, 0x2DCF), (0x2DD7, 0x2DD7), (0x2DDF, 0x2DDF),
    (0x2E5E, 0x2E7F), (0x2E9A, 0x2E9A), (0x2EF4, 0x2EFF), (0x2FD6, 0x2FEF),
    (0x3040, 0x3040), (0x3097, 0x3098), (0x3100, 0x3104), (0x3130, 0x3130),
    (0x318F, 0x318F), (0x31E6, 0x31EE), (0x321F, 0x321F), (0xA48D, 0xA48F),
    (0xA4C7, 0xA4CF), (0xA62C, 0xA63F), (0xA6F8, 0xA6FF), (0xA7CE, 0xA7CF),
    (0xA7D2, 0xA7D2), (0xA7D4, 0xA7D4), (0xA7DD, 0xA7F1), (0xA82D, 0xA82F),
    (0xA83A, 0xA83F), (0xA878, 0xA87F), (0xA8C6, 0xA8CD), (0xA8DA, 0xA8DF),
    (0xA954, 0xA95E), (0xA97D, 0xA97F), (0xA9CE, 0xA9CE), (0xA9DA, 0xA9DD),
    (0xA9FF, 0xA9FF), (0xAA37, 0xAA3F), (0xAA4E, 0xAA4F), (0xAA5A, 0xAA5B),
    (0xAAC3, 0xAADA), (0xAAF7, 0xAB00), (0xAB07, 0xAB08), (0xAB0F, 0xAB10),
    (0xAB17, 0xAB1F), (0xAB27, 0xAB27), (0xAB2F, 0xAB2F), (0xAB6C, 0xAB6F),
    (0xABEE, 0xABEF), (0xABFA, 0xABFF), (0xD7A4, 0xD7AF), (0xD7C7, 0xD7CA),
    (0xD7FC, 0xD7FF), (0xFA6E, 0xFA6F), (0xFADA, 0xFAFF), (0xFB07, 0xFB12),
    (0xFB18, 0xFB1C), (0xFB37, 0xFB37), (0xFB3D, 0xFB3D), (0xFB3F, 0xFB3F),
    (0xFB42, 0xFB42), (0xFB45, 0xFB45), (0xFBC3, 0xFBD2), (0xFD90, 0xFD91),
    (0xFDC8, 0xFDCE), (0xFDD0, 0xFDEF), (0xFE1A, 0xFE1F), (0xFE53, 0xFE53),
    (0xFE67, 0xFE67), (0xFE6C, 0xFE6F), (0xFE75, 0xFE75), (0xFEFD, 0xFEFE),
    (0xFF00, 0xFF00), (0xFFBF, 0xFFC1), (0xFFC8, 0xFFC9), (0xFFD0, 0xFFD1),
    (0xFFD8, 0xFFD9), (0xFFDD, 0xFFDF), (0xFFE7, 0xFFE7), (0xFFEF, 0xFFF8),
    (0xFFFE, 0xFFFF), (0x1000C, 0x1000C), (0x10027, 0x10027), (0x1003B, 0x1003B),
    (0x1003E, 0x1003E), (0x1004E, 0x1004F), (0x1005E, 0x1007F), (0x100FB, 0x100FF),
    (0x10103, 0x10106), (0x10134, 0x10136), (0x1018F, 0x1018F), (0x1019D, 0x1019F),
    (0x101A1, 0x101CF), (0x101FE, 0x1027F), (0x1029D, 0x1029F), (0x102D1, 0x102DF),
    (0x102FC, 0x102FF), (0x10324, 0x1032C), (0x1034B, 0x1034F), (0x1037B, 0x1037F),
    (0x1039E, 0x1039E), (0x103C4, 0x103C7), (0x103D6, 0x103FF), (0x1049E, 0x1049F),
    (0x104AA, 0x104AF), (0x104D4, 0x104D7), (0x104FC, 0x104FF), (0x10528, 0x1052F),
    (0x10564, 0x1056E), (0x1057B, 0x1057B), (0x1058B, 0x1058B), (0x10593, 0x10593),
    (0x10596, 0x10596), (0x105A2, 0x105A2), (0x105B2, 0x105B2), (0x105BA, 0x105BA),
    (0x105BD, 0x105BF), (0x105F4, 0x105FF), (0x10737, 0x1073F), (0x10756, 0x1075F),
    (0x10768, 0x1077F), (0x10786, 0x10786), (0x107B1, 0x107B1), (0x107BB, 0x107FF),
    (0x10806, 0x10807), (0x10809, 0x10809), (0x10836, 0x10836), (0x10839, 0x1083B),
    (0x1083D, 0x1083E), (0x10856, 0x10856), (0x1089F, 0x108A6), (0x108B0, 0x108DF),
    (0x108F3, 0x108F3), (0x108F6, 0x108FA), (0x1091C, 0x1091E), (0x1093A, 0x1093E),
    (0x10940, 0x1097F), (0x109B8, 0x109BB), (0x109D0, 0x109D1), (0x10A04, 0x10A04),
    (0x10A07, 0x10A0B), (0x10A14, 0x10A14), (0x10A18, 0x10A18), (0x10A36, 0x10A37),
    (0x10A3B, 0x10A3E), (0x10A49, 0x10A4F), (0x10A59, 0x10A5F), (0x10AA0, 0x10ABF),
    (0x10AE7, 0x10AEA), (0x10AF7, 0x10AFF), (0x10B36, 0x10B38), (0x10B56, 0x10B57),
    (0x10B73, 0x10B77), (0x10B92, 0x10B98), (0x10B9D, 0x10BA8), (0x10BB0, 0x10BFF),
    (0x10C49, 0x10C7F), (0x10CB3, 0x10CBF), (0x10CF3, 0x10CF9), (0x10D28, 0x10D2F),
    (0x10D3A, 0x10D3F), (0x10D66, 0x10D68), (0x10D86, 0x10D8D), (0x10D90, 0x10E5F),
    (0x10E7F, 0x10E7F), (0x10EAA, 0x10EAA), (0x10EAE, 0x10EAF), (0x10EB2, 0x10EC1),
    (0x10EC5, 0x10EFB), (0x10F28, 0x10F2F), (0x10F5A, 0x10F6F), (0x10F8A, 0x10FAF),
    (0x10FCC, 0x10FDF), (0x10FF7, 0x10FFF), (0x1104E, 0x11051), (0x11076, 0x1107E),
    (0x110C3, 0x110CC), (0x110CE, 0x110CF), (0x110E9, 0x110EF), (0x110FA, 0x110FF),
    (0x11135, 0x11135), (0x11148, 0x1114F), (0x11177, 0x1117F), (0x111E0, 0x111E0),
    (0x111F5, 0x111FF), (0x11212, 0x11212), (0x11242, 0x1127F), (0x11287, 0x11287),
    (0x11289, 0x11289), (0x1128E, 0x1128E), (0x1129E, 0x1129E), (0x112AA, 0x112AF),
    (0x112EB, 0x112EF), (0x112FA, 0x112FF), (0x11304, 0x11304), (0x1130D, 0x1130E),
    (0x11311, 0x11312), (0x11329, 0x11329), (0x11331, 0x11331), (0x11334, 0x11334),
    (0x1133A, 0x1133A), (0x11345, 0x11346), (0x11349, 0x1134A), (0x1134E, 0x1134F),
    (0x11351, 0x11356), (0x11358, 0x1135C), (0x11364, 0x11365), (0x1136D, 0x1136F),
    (0x11375, 0x1137F), (0x1138A, 0x1138A), (0x1138C, 0x1138D), (0x1138F, 0x1138F),
    (0x113B6, 0x113B6), (0x113C1, 0x113C1), (0x113C3, 0x113C4), (0x113C6, 0x113C6),
    (0x113CB, 0x113CB), (0x113D6, 0x113D6), (0x113D9, 0x113E0), (0x113E3, 0x113FF),
    (0x1145C, 0x1145C), (0x11462, 0x1147F), (0x114C8, 0x114CF), (0x114DA, 0x1157F),
    (0x115B6, 0x115B7), (0x115DE, 0x115FF), (0x11645, 0x1164F), (0x1165A, 0x1165F),
    (0x1166D, 0x1167F), (0x116BA, 0x116BF), (0x116CA, 0x116CF), (0x116E4, 0x116FF),
    (0x1171B, 0x1171C), (0x1172C, 0x1172F), (0x11747, 0x117FF), (0x1183C, 0x1189F),
    (0x118F3, 0x118FE), (0x11907, 0x11908), (0x1190A, 0x1190B), (0x11914, 0x11914),
    (0x11917, 0x11917), (0x11936, 0x11936), (0x11939, 0x1193A), (0x11947, 0x1194F),
    (0x1195A, 0x1199F), (0x119A8, 0x119A9), (0x119D8, 0x119D9), (0x119E5, 0x119FF),
    (0x11A48, 0x11A4F), (0x11AA3, 0x11AAF), (0x11AF9, 0x11AFF), (0x11B0A, 0x11BBF),
    (0x11BE2, 0x11BEF), (0x11BFA, 0x11BFF), (0x11C09, 0x11C09), (0x11C37, 0x11C37),
    (0x11C46, 0x11C4F), (0x11C6D, 0x11C6F), (0x11C90, 0x11C91), (0x11CA8, 0x11CA8),
    (0x11CB7, 0x11CFF), (0x11D07, 0x11D07), (0x11D0A, 0x11D0A), (0x11D37, 0x11D39),
    (0x11D3B, 0x11D3B), (0x11D3E, 0x11D3E), (0x11D48, 0x11D4F), (0x11D5A, 0x11D5F),
    (0x11D66, 0x11D66), (0x11D69, 0x11D69), (0x11D8F, 0x11D8F), (0x11D92, 0x11D92),
    (0x11D99, 0x11D9F), (0x11DAA, 0x11EDF), (0x11EF9, 0x11EFF), (0x11F11, 0x11F11),
    (0x11F3B, 0x11F3D), (0x11F5B, 0x11FAF), (0x11FB1, 0x11FBF), (0x11FF2, 0x11FFE),
    (0x1239A, 0x123FF), (0x1246F, 0x1246F), (0x12475, 0x1247F), (0x12544, 0x12F8F),
    (0x12FF3, 0x12FFF), (0x13456, 0x1345F), (0x143FB, 0x143FF), (0x14647, 0x160FF),
    (0x1613A, 0x167FF), (0x16A39, 0x16A3F), (0x16A5F, 0x16A5F), (0x16A6A, 0x16A6D),
    (0x16ABF, 0x16ABF), (0x16ACA, 0x16ACF), (0x16AEE, 0x16AEF), (0x16AF6, 0x16AFF),
    (0x16B46, 0x16B4F), (0x16B5A, 0x16B5A), (0x16B62, 0x16B62), (0x16B78, 0x16B7C),
    (0x16B90, 0x16D3F), (0x16D7A, 0x16E3F), (0x16E9B, 0x16EFF), (0x16F4B, 0x16F4E),
    (0x16F88, 0x16F8E), (0x16FA0, 0x16FDF), (0x16FE5, 0x16FEF), (0x16FF2, 0x16FFF),
    (0x187F8, 0x187FF), (0x18CD6, 0x18CFE), (0x18D09, 0x1AFEF), (0x1AFF4, 0x1AFF4),
    (0x1AFFC, 0x1AFFC), (0x1AFFF, 0x1AFFF), (0x1B123, 0x1B131), (0x1B133, 0x1B14F),
    (0x1B153, 0x1B154), (0x1B156, 0x1B163), (0x1B168, 0x1B16F), (0x1B2FC, 0x1BBFF),
    (0x1BC6B, 0x1BC6F), (0x1BC7D, 0x1BC7F), (0x1BC89, 0x1BC8F), (0x1BC9A, 0x1BC9B),
    (0x1BCA4, 0x1CBFF), (0x1CCFA, 0x1CCFF), (0x1CEB4, 0x1CEFF), (0x1CF2E, 0x1CF2F),
    (0x1CF47, 0x1CF4F), (0x1CFC4, 0x1CFFF), (0x1D0F6, 0x1D0FF), (0x1D127, 0x1D128),
    (0x1D1EB, 0x1D1FF), (0x1D246, 0x1D2BF), (0x1D2D4, 0x1D2DF), (0x1D2F4, 0x1D2FF),
    (0x1D357, 0x1D35F), (0x1D379, 0x1D3FF), (0x1D455, 0x1D455), (0x1D49D, 0x1D49D),
    (0x1D4A0, 0x1D4A1), (0x1D4A3, 0x1D4A4), (0x1D4A7, 0x1D4A8), (0x1D4AD, 0x1D4AD),
    (0x1D4BA, 0x1D4BA), (0x1D4BC, 0x1D4BC), (0x1D4C4, 0x1D4C4), (0x1D506, 0x1D506),
    (0x1D50B, 0x1D50C), (0x1D515, 0x1D515), (0x1D51D, 0x1D51D), (0x1D53A, 0x1D53A),
    (0x1D53F, 0x1D53F), (0x1D545, 0x1D545), (0x1D547, 0x1D549), (0x1D551, 0x1D551),
    (0x1D6A6, 0x1D6A7), (0x1D7CC, 0x1D7CD), (0x1DA8C, 0x1DA9A), (0x1DAA0, 0x1DAA0),
    (0x1DAB0, 0x1DEFF), (0x1DF1F, 0x1DF24), (0x1DF2B, 0x1DFFF), (0x1E007, 0x1E007),
    (0x1E019, 0x1E01A), (0x1E022, 0x1E022), (0x1E025, 0x1E025), (0x1E02B, 0x1E02F),
    (0x1E06E, 0x1E08E), (0x1E090, 0x1E0FF), (0x1E12D, 0x1E12F), (0x1E13E, 0x1E13F),
    (0x1E14A, 0x1E14D), (0x1E150, 0x1E28F), (0x1E2AF, 0x1E2BF), (0x1E2FA, 0x1E2FE),
    (0x1E300, 0x1E4CF), (0x1E4FA, 0x1E5CF), (0x1E5FB, 0x1E5FE), (0x1E600, 0x1E7DF),
    (0x1E7E7, 0x1E7E7), (0x1E7EC, 0x1E7EC), (0x1E7EF, 0x1E7EF), (0x1E7FF, 0x1E7FF),
    (0x1E8C5, 0x1E8C6), (0x1E8D7, 0x1E8FF), (0x1E94C, 0x1E94F), (0x1E95A, 0x1E95D),
    (0x1E960, 0x1EC70), (0x1ECB5, 0x1ED00), (0x1ED3E, 0x1EDFF), (0x1EE04, 0x1EE04),
    (0x1EE20, 0x1EE20), (0x1EE23, 0x1EE23), (0x1EE25, 0x1EE26), (0x1EE28, 0x1EE28),
    (0x1EE33, 0x1EE33), (0x1EE38, 0x1EE38), (0x1EE3A, 0x1EE3A), (0x1EE3C, 0x1EE41),
    (0x1EE43, 0x1EE46), (0x1EE48, 0x1EE48), (0x1EE4A, 0x1EE4A), (0x1EE4C, 0x1EE4C),
    (0x1EE50, 0x1EE50), (0x1EE53, 0x1EE53), (0x1EE55, 0x1EE56), (0x1EE58, 0x1EE58),
    (0x1EE5A, 0x1EE5A), (0x1EE5C, 0x1EE5C), (0x1EE5E, 0x1EE5E), (0x1EE60, 0x1EE60),
    (0x1EE63, 0x1EE63), (0x1EE65, 0x1EE66), (0x1EE6B, 0x1EE6B), (0x1EE73, 0x1EE73),
    (0x1EE78, 0x1EE78), (0x1EE7D, 0x1EE7D), (0x1EE7F, 0x1EE7F), (0x1EE8A, 0x1EE8A),
    (0x1EE9C, 0x1EEA0), (0x1EEA4, 0x1EEA4), (0x1EEAA, 0x1EEAA), (0x1EEBC, 0x1EEEF),
    (0x1EEF2, 0x1EFFF), (0x1F02C, 0x1F02F), (0x1F094, 0x1F09F), (0x1F0AF, 0x1F0B0),
    (0x1F0C0, 0x1F0C0), (0x1F0D0, 0x1F0D0), (0x1F0F6, 0x1F0FF), (0x1F1AE, 0x1F1E5),
    (0x1F203, 0x1F20F), (0x1F23C, 0x1F23F), (0x1F249, 0x1F24F), (0x1F252, 0x1F25F),
    (0x1F266, 0x1F2FF), (0x1F6D8, 0x1F6DB), (0x1F6ED, 0x1F6EF), (0x1F6FD, 0x1F6FF),
    (0x1F777, 0x1F77A), (0x1F7DA, 0x1F7DF), (0x1F7EC, 0x1F7EF), (0x1F7F1, 0x1F7FF),
    (0x1F80C, 0x1F80F), (0x1F848, 0x1F84F), (0x1F85A, 0x1F85F), (0x1F888, 0x1F88F),
    (0x1F8AE, 0x1F8AF), (0x1F8BC, 0x1F8BF), (0x1F8C2, 0x1F8FF), (0x1FA54, 0x1FA5F),
    (0x1FA6E, 0x1FA6F), (0x1FA7D, 0x1FA7F), (0x1FA8A, 0x1FA8E), (0x1FAC7, 0x1FACD),
    (0x1FADD, 0x1FADE), (0x1FAEA, 0x1FAEF), (0x1FAF9, 0x1FAFF), (0x1FB93, 0x1FB93),
    (0x1FBFA, 0x1FFFF), (0x2A6E0, 0x2A6FF), (0x2B73A, 0x2B73F), (0x2B81E, 0x2B81F),
    (0x2CEA2, 0x2CEAF), (0x2EBE1, 0x2EBEF), (0x2EE5E, 0x2F7FF), (0x2FA1E, 0x2FFFF),
    (0x3134B, 0x3134F), (0x323B0, 0xE0000), (0xE0002, 0xE001F), (0xE0080, 0xE00FF),
    (0xE01F0, 0xEFFFF), (0xFFFFE, 0xFFFFF), (0x10FFFE, 0x10FFFF),
)


def _merged(ranges: list[tuple[int, int]]) -> tuple[tuple[int, int], ...]:
    """Sort ``ranges`` and join those that overlap or touch, for a binary search."""
    out: list[tuple[int, int]] = []
    for low, high in sorted(ranges):
        if out and low <= out[-1][1] + 1:
            out[-1] = (out[-1][0], max(out[-1][1], high))
        else:
            out.append((low, high))
    return tuple(out)


def _in_sorted(ranges: tuple[tuple[int, int], ...], starts: list[int], code: int) -> bool:
    """True when ``code`` is in ``ranges`` (sorted and disjoint, ``starts`` their lower ends)."""
    index = bisect_right(starts, code) - 1
    return index >= 0 and code <= ranges[index][1]


def is_format_character(character: str) -> bool:
    """True for a character in :data:`FORMAT_CHARACTER_RANGES`."""
    code = ord(character)
    return any(low <= code <= high for low, high in FORMAT_CHARACTER_RANGES)


def is_default_ignorable(character: str) -> bool:
    """True for a character in :data:`DEFAULT_IGNORABLE_RANGES`."""
    code = ord(character)
    return any(low <= code <= high for low, high in DEFAULT_IGNORABLE_RANGES)


#: Every invisible character (:func:`is_invisible`) as sorted, disjoint ranges.
_INVISIBLE_RANGES = _merged(
    [
        *FORMAT_CHARACTER_RANGES,
        *DEFAULT_IGNORABLE_RANGES,
        *((ord(character), ord(character)) for character in BLANK_CHARACTERS),
        *(
            (ord(character), ord(character))
            for character in UNICODE_WHITESPACE
            if ord(character) > 0x7F
        ),
    ]
)
_INVISIBLE_STARTS = [low for low, _ in _INVISIBLE_RANGES]
_UNASSIGNED_STARTS = [low for low, _ in UNASSIGNED_RANGES]


def is_invisible(character: str) -> bool:
    """True for a character a reader cannot see.

    That is a format character, a default-ignorable code point, a blank character
    (:data:`BLANK_CHARACTERS`), or a non-ASCII ``White_Space`` character: a no-break space looks
    like a space, a line separator like a line break or nothing, a zero-width space, a byte-order
    mark, a variation selector, or a Hangul filler like nothing at all, and a bidirectional
    override reorders what follows it.
    """
    return _in_sorted(_INVISIBLE_RANGES, _INVISIBLE_STARTS, ord(character))


def is_noncharacter(character: str) -> bool:
    """True for a Unicode noncharacter: U+FDD0..U+FDEF and the last two code points of every
    plane (U+FFFE, U+FFFF, U+1FFFE, U+1FFFF, ... U+10FFFE, U+10FFFF), set aside permanently for a
    program's internal use and never assigned a character.
    """
    code = ord(character)
    return 0xFDD0 <= code <= 0xFDEF or (code & 0xFFFE) == 0xFFFE


def is_private_use(character: str) -> bool:
    """True for a character in :data:`PRIVATE_USE_RANGES`."""
    code = ord(character)
    return any(low <= code <= high for low, high in PRIVATE_USE_RANGES)


def is_unassigned(character: str) -> bool:
    """True for a code point in :data:`UNASSIGNED_RANGES`: no character as of Unicode 16.0."""
    return _in_sorted(UNASSIGNED_RANGES, _UNASSIGNED_STARTS, ord(character))


def _character_class(ranges: list[tuple[int, int]]) -> str:
    return "".join(
        re.escape(chr(low)) if low == high else f"{re.escape(chr(low))}-{re.escape(chr(high))}"
        for low, high in ranges
    )


#: Every character JSON output escapes beyond what JSON itself requires: DEL, the C1 controls,
#: and every invisible character (:func:`is_invisible`). JSON escapes C0 controls already.
_JSON_UNSAFE = re.compile("[" + _character_class([(0x7F, 0x9F), *_INVISIBLE_RANGES]) + "]")


def _json_escape(match: re.Match[str]) -> str:
    code = ord(match.group())
    if code <= 0xFFFF:
        return f"\\u{code:04x}"
    code -= 0x10000
    return f"\\u{0xD800 + (code >> 10):04x}\\u{0xDC00 + (code & 0x3FF):04x}"


def json_text(document: object, *, indent: int | None = None) -> str:
    """Serialize ``document`` in the JSON text form both editions share.

    Keys sorted at every level, ``, `` and ``: `` separators (two-space indentation with
    ``indent=2``), non-ASCII text written as itself -- except that DEL, the C1 controls, and every
    invisible character are written as ``\\uXXXX`` escapes (a surrogate pair above U+FFFF), so a
    project path or a command holding U+009B, U+202E, U+2028, or U+200B cannot reach a terminal
    raw through ``herdr-run config`` or any other JSON herdr-run prints or stores. A JSON reader
    gets back exactly the same strings.
    """
    text = json.dumps(document, indent=indent, sort_keys=True, ensure_ascii=False)
    if text.isascii() and "\x7f" not in text:
        return text
    return _JSON_UNSAFE.sub(_json_escape, text)


def name_violation(value: str) -> str | None:
    """Describe the first character a name may not contain, or return ``None``.

    A name -- an agent, a workspace, an allowed program, a denied option -- is compared with what
    somebody typed and shown to somebody reading, so it may contain no terminal control (C0, DEL,
    C1), no surrogate, no invisible character (:func:`is_invisible`), no noncharacter, no
    private-use character, and no code point Unicode 16.0 leaves unassigned: each of those last
    three draws as whatever the font in use makes of it, often nothing. A character that is more
    than one of these is described by the first that applies, in that order.
    """
    for character in value:
        code = ord(character)
        if 0x20 <= code <= 0x7E:
            continue
        if code < 0x20 or 0x7F <= code <= 0x9F:
            return f"control character U+{code:04X}"
        if 0xD800 <= code <= 0xDFFF:
            return f"surrogate U+{code:04X}"
        if is_invisible(character):
            return f"invisible character U+{code:04X}"
        if is_noncharacter(character):
            return f"noncharacter U+{code:04X}"
        if is_private_use(character):
            return f"private-use character U+{code:04X}"
        if is_unassigned(character):
            return f"unassigned code point U+{code:04X}"
    return None


def display(value: str) -> str:
    """Show ``value`` in a message as it is, or :func:`quote` it when it holds an unsafe character.

    A path or a name that is made of ordinary characters reads best unquoted; one that holds any
    character a name may not contain (:func:`name_violation`) -- a terminal control, a surrogate,
    an invisible character, a noncharacter, a private-use character, or an unassigned code point
    -- is quoted, so nothing can reach a terminal raw from a herdr-run message.
    """
    return value if name_violation(value) is None else quote(value)


def quote(value: str) -> str:
    """Quote ``value`` for a diagnostic, escaping everything that is not printable ASCII.

    Printable ASCII (U+0020..U+007E) passes through except ``"`` and ``\\``, which are escaped;
    newline, carriage return, and tab use their short escapes; every other code point is written
    as ``\\u{hex}``. No Unicode table is consulted, so a bidirectional override, a line separator,
    or an invisible character can never reach a terminal raw from a herdr-run message.
    """
    out = ['"']
    for character in value:
        code = ord(character)
        if character == '"':
            out.append('\\"')
        elif character == "\\":
            out.append("\\\\")
        elif character == "\n":
            out.append("\\n")
        elif character == "\r":
            out.append("\\r")
        elif character == "\t":
            out.append("\\t")
        elif 0x20 <= code <= 0x7E:
            out.append(character)
        else:
            out.append(f"\\u{{{code:x}}}")
    out.append('"')
    return "".join(out)


def quote_optional(value: str | None) -> str:
    """:func:`quote` a value that may be absent; an absent value reads ``None``."""
    return "None" if value is None else quote(value)


def os_error_text(exc: BaseException) -> str:
    """Describe an operating-system error as ``<strerror> (os error <errno>)``.

    That is the form the native edition's standard library prints, and it is easy to reproduce
    exactly here; reproducing Python's ``[Errno N] text: 'path'`` there would mean imitating
    ``repr`` of a path. An error without an errno falls back to its message.
    """
    errno = getattr(exc, "errno", None)
    strerror = getattr(exc, "strerror", None)
    if isinstance(errno, int) and isinstance(strerror, str) and strerror:
        return f"{strerror} (os error {errno})"
    return str(exc)


def strip_ascii(value: str) -> str:
    """Trim the six ASCII whitespace characters from both ends, and nothing else."""
    return value.strip(ASCII_WHITESPACE)


def strip_space(value: str) -> str:
    """Trim Unicode ``White_Space`` (the explicit list above) from both ends."""
    return value.strip(_UNICODE_WHITESPACE_TEXT)


def rstrip_space(value: str) -> str:
    """Trim Unicode ``White_Space`` (the explicit list above) from the end."""
    return value.rstrip(_UNICODE_WHITESPACE_TEXT)


def is_blank(value: str) -> bool:
    """True when ``value`` is empty or holds only Unicode ``White_Space``."""
    return all(character in UNICODE_WHITESPACE for character in value)


def number_text(value: float) -> str:
    """Render seconds for a message: integral values with no decimals, otherwise the shortest
    round-trip decimal, never in exponent notation (``0.00001``, not ``1e-05``)."""
    if math.isfinite(value) and value == math.floor(value):
        return f"{value:.0f}"
    if not math.isfinite(value):
        return repr(value)
    return format(Decimal(repr(value)), "f")


def format_round3(value: float) -> float:
    """Round to three decimal places by correctly rounded decimal formatting."""
    return float(f"{value:.3f}")


_EXIT_MARKER = re.compile(r"[+-]?[0-9]+")

#: Longest completion marker accepted. A real one is a few bytes; anything longer is not one.
MAX_EXIT_MARKER_BYTES = 128


def parse_exit_marker(payload: bytes) -> int | None:
    """Parse a run's ``exit_code`` completion marker, or return ``None`` when it is not one.

    ASCII bytes only, at most 128 of them, ASCII whitespace trimmed, then ``[+-]?[0-9]+`` within
    the signed 32-bit range. Anything else -- underscores, other scripts' digits, a byte that is
    not ASCII, a value out of range -- means "not a completed marker", never a crash and never a
    deletion the other edition would not make.
    """
    if len(payload) > MAX_EXIT_MARKER_BYTES or any(byte >= 0x80 for byte in payload):
        return None
    text = strip_ascii(payload.decode("ascii"))
    if not _EXIT_MARKER.fullmatch(text):
        return None
    value = int(text, 10)
    if not -(2**31) <= value <= 2**31 - 1:
        return None
    return value


def _reject_constant(token: str) -> object:
    raise ValueError(f"{token} is not valid JSON")


def _parse_float(token: str) -> float:
    value = float(token)
    if not math.isfinite(value):
        raise ValueError("number out of range")
    return value


def _parse_int(token: str) -> object:
    # Mirror a strict JSON reader with 64-bit integers: an integer literal outside the u64/i64
    # range, and ``-0``, are numbers but not integers.
    if token == "-0":
        return -0.0
    value = int(token)
    if -(2**63) <= value <= 2**64 - 1:
        return value
    try:
        as_float = float(value)
    except OverflowError:
        raise ValueError("number out of range") from None
    if not math.isfinite(as_float):
        raise ValueError("number out of range")
    return as_float


def _reject_surrogates(value: object) -> None:
    """Refuse a lone surrogate in ``value``: a string, or one inside arrays (nested any depth).

    Objects are not entered: every object was already checked, pair by pair, as it was parsed
    (:func:`_checked_object`), so each string is looked at once however deep it is.
    """
    stack = [value]
    while stack:
        current = stack.pop()
        if isinstance(current, str):
            if any(0xD800 <= ord(character) <= 0xDFFF for character in current):
                raise ValueError("lone surrogate in JSON string")
        elif isinstance(current, list):
            stack.extend(current)


def _checked_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    """Build one JSON object, checking EVERY key and value as parsed, not only the survivors.

    A strict reader refuses a lone surrogate escape wherever it appears, including in a value that
    a later duplicate key replaces; checking only the finished document would accept
    ``{"a": "\\ud800", "a": 1}``, which the native edition refuses. The last duplicate wins, as it
    does in a plain reading.
    """
    for key, value in pairs:
        _reject_surrogates(key)
        _reject_surrogates(value)
    return dict(pairs)


#: Deepest JSON nesting refused: a document whose arrays and objects nest this many levels deep is
#: not read, so 127 levels is the most accepted. The native edition's reader has exactly this limit.
JSON_MAX_DEPTH = 128

#: A JSON string (closed or not) or one bracket, so nesting can be measured without parsing.
_JSON_DEPTH_TOKENS = re.compile(r'"[^"\\]*(?:\\.[^"\\]*)*"?|[\[\]{}]', re.DOTALL)


def _json_too_deep(text: str) -> bool:
    depth = 0
    for match in _JSON_DEPTH_TOKENS.finditer(text):
        token = match.group()
        if token in ("[", "{"):
            depth += 1
            if depth >= JSON_MAX_DEPTH:
                return True
        elif token in ("]", "}"):
            depth -= 1
    return False


def strict_json_loads(text: str) -> object:
    """Parse RFC 8259 JSON the way a strict 64-bit reader does, raising ``ValueError`` otherwise.

    ``NaN``/``Infinity`` are refused, a float that overflows is refused, an integer that does not
    fit 64 bits becomes the nearest float (and is refused when even that overflows), a lone
    surrogate escape is refused wherever it appears (even in a value a later duplicate key
    replaces), and so is nesting :data:`JSON_MAX_DEPTH` levels deep. Every failure is a
    ``ValueError``; nothing else escapes.
    """
    if _json_too_deep(text):
        raise ValueError(f"JSON nested {JSON_MAX_DEPTH} or more levels deep")
    try:
        document: object = json.loads(
            text,
            object_pairs_hook=_checked_object,
            parse_constant=_reject_constant,
            parse_float=_parse_float,
            parse_int=_parse_int,
        )
    except (OverflowError, RecursionError) as exc:
        raise ValueError(str(exc)) from exc
    _reject_surrogates(document)
    return document
