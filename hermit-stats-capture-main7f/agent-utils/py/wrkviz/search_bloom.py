"""Deterministic trigram Bloom filters for transcript-search shard catalogs.

The filter is deliberately a prefilter only: a positive result still requires
the normal transcript matcher.  Its portable normalization uses one explicit
cross-runtime whitespace table followed by ASCII-only lowercasing of UTF-8 bytes.
Search callers must scan a shard for non-ASCII or sub-trigram terms, and exact
matchers must use ASCII-only case folding for eligible terms.  Those rules keep
catalog rejection false-negative-free across Python and browser runtimes.
"""

from __future__ import annotations

import base64
import re
from collections.abc import Iterable
from dataclasses import dataclass

from wrkviz.archive import JsonValue, as_int, as_string


TRIGRAM_BLOOM_ALGORITHM = "ascii-lower-utf8-trigram-fnv1a32-double-v1"
TRIGRAM_BLOOM_HASH_COUNT = 7
_BITS_PER_TRIGRAM = 10
_MINIMUM_BIT_COUNT = 64
_FNV_OFFSET = 2_166_136_261
_FNV_PRIME = 16_777_619
_SECOND_HASH_SEED = 0x9E37_79B9
_UINT32_MASK = (1 << 32) - 1
_WHITESPACE = re.compile(
    "[\u0009-\u000d\u001c-\u0020\u0085\u00a0\u1680"
    "\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+"
)
_ASCII_LOWER_TRANSLATION = bytes.maketrans(
    bytes(range(256)),
    bytes(value + 32 if 65 <= value <= 90 else value for value in range(256)),
)


@dataclass(frozen=True)
class TrigramBloom:
    """One immutable Bloom filter plus its portable catalog parameters."""

    bit_count: int
    hash_count: int
    bits: bytes
    trigram_count: int

    def catalog_obj(self) -> dict[str, JsonValue]:
        """Return deterministic JSON metadata suitable for a shard catalog."""

        return {
            "algorithm": TRIGRAM_BLOOM_ALGORITHM,
            "bit_count": self.bit_count,
            "hash_count": self.hash_count,
            "bits_base64": base64.b64encode(self.bits).decode("ascii"),
            "trigram_count": self.trigram_count,
        }


def compact_search_text(value: str) -> str:
    """Apply the transcript searcher's whitespace normalization."""

    return _WHITESPACE.sub(" ", value).strip()


def ascii_lower_utf8(value: str) -> bytes:
    """Encode UTF-8 while lowercasing ASCII ``A`` through ``Z`` only."""

    return value.encode("utf-8").translate(_ASCII_LOWER_TRANSLATION)


def search_text_trigrams(value: str) -> tuple[bytes, ...]:
    """Return distinct sorted byte trigrams for normalized search text."""

    encoded = ascii_lower_utf8(compact_search_text(value))
    if len(encoded) < 3:
        return ()
    return tuple(
        sorted({encoded[index : index + 3] for index in range(len(encoded) - 2)})
    )


def query_is_bloom_eligible(value: str) -> bool:
    """Return whether a query may safely use this ASCII trigram prefilter."""

    compact = compact_search_text(value)
    return compact.isascii() and len(compact.encode("utf-8")) >= 3


def _next_power_of_two(value: int) -> int:
    return 1 << (max(1, value) - 1).bit_length()


def _fnv1a32(value: bytes, seed: int) -> int:
    digest = seed
    for byte in value:
        digest ^= byte
        digest = (digest * _FNV_PRIME) & _UINT32_MASK
    return digest


def _bit_positions(trigram: bytes, bit_count: int, hash_count: int) -> tuple[int, ...]:
    first = _fnv1a32(trigram, _FNV_OFFSET)
    second = _fnv1a32(trigram, _FNV_OFFSET ^ _SECOND_HASH_SEED) | 1
    mask = bit_count - 1
    return tuple((first + index * second) & mask for index in range(hash_count))


def build_trigram_bloom(texts: Iterable[str]) -> TrigramBloom:
    """Build a deterministic filter without creating cross-record trigrams."""

    trigrams: set[bytes] = set()
    for text in texts:
        trigrams.update(search_text_trigrams(text))
    bit_count = _next_power_of_two(
        max(_MINIMUM_BIT_COUNT, len(trigrams) * _BITS_PER_TRIGRAM)
    )
    mutable = bytearray(bit_count // 8)
    for trigram in sorted(trigrams):
        for position in _bit_positions(trigram, bit_count, TRIGRAM_BLOOM_HASH_COUNT):
            mutable[position // 8] |= 1 << (position % 8)
    return TrigramBloom(
        bit_count=bit_count,
        hash_count=TRIGRAM_BLOOM_HASH_COUNT,
        bits=bytes(mutable),
        trigram_count=len(trigrams),
    )


def trigram_bloom_from_catalog(
    value: dict[str, JsonValue], where: str = "trigram bloom"
) -> TrigramBloom:
    """Validate and decode one catalog filter."""

    algorithm = as_string(value.get("algorithm"), where + ".algorithm")
    if algorithm != TRIGRAM_BLOOM_ALGORITHM:
        raise ValueError(f"{where}.algorithm: unsupported value {algorithm!r}")
    bit_count = as_int(value.get("bit_count"), where + ".bit_count")
    if bit_count < _MINIMUM_BIT_COUNT or bit_count & (bit_count - 1):
        raise ValueError(f"{where}.bit_count: expected a power of two of at least 64")
    hash_count = as_int(value.get("hash_count"), where + ".hash_count")
    if hash_count != TRIGRAM_BLOOM_HASH_COUNT:
        raise ValueError(f"{where}.hash_count: expected {TRIGRAM_BLOOM_HASH_COUNT}")
    trigram_count = as_int(value.get("trigram_count"), where + ".trigram_count")
    if trigram_count < 0:
        raise ValueError(f"{where}.trigram_count: expected a non-negative integer")
    encoded = as_string(value.get("bits_base64"), where + ".bits_base64")
    try:
        bits = base64.b64decode(encoded, validate=True)
    except ValueError as error:
        raise ValueError(f"{where}.bits_base64: invalid base64") from error
    if len(bits) * 8 != bit_count:
        raise ValueError(
            f"{where}.bits_base64: decoded {len(bits) * 8} bits, expected {bit_count}"
        )
    return TrigramBloom(bit_count, hash_count, bits, trigram_count)


def bloom_might_contain(filter_value: TrigramBloom, query: str) -> bool:
    """Return false only when an eligible query is definitely absent."""

    if not query_is_bloom_eligible(query):
        return True
    for trigram in search_text_trigrams(query):
        for position in _bit_positions(
            trigram, filter_value.bit_count, filter_value.hash_count
        ):
            if not filter_value.bits[position // 8] & (1 << (position % 8)):
                return False
    return True


__all__ = [
    "TRIGRAM_BLOOM_ALGORITHM",
    "TRIGRAM_BLOOM_HASH_COUNT",
    "TrigramBloom",
    "ascii_lower_utf8",
    "bloom_might_contain",
    "build_trigram_bloom",
    "compact_search_text",
    "query_is_bloom_eligible",
    "search_text_trigrams",
    "trigram_bloom_from_catalog",
]
