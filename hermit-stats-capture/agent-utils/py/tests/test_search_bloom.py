from __future__ import annotations

import base64

import pytest

from wrkviz.archive import JsonValue
from wrkviz.query import _bloom_might_match
from wrkviz.search_bloom import (
    TRIGRAM_BLOOM_ALGORITHM,
    ascii_lower_utf8,
    bloom_might_contain,
    build_trigram_bloom,
    query_is_bloom_eligible,
    search_text_trigrams,
    trigram_bloom_from_catalog,
)


def test_normalization_lowercases_only_ascii_and_compacts_whitespace() -> None:
    assert ascii_lower_utf8("AZ az Ä") == b"az az \xc3\x84"
    assert search_text_trigrams("  Alpha\n\tBETA  ") == search_text_trigrams(
        "alpha beta"
    )
    assert search_text_trigrams("ab") == ()


def test_filter_has_no_false_negatives_for_ascii_substrings() -> None:
    texts = (
        "Backend B3 means more than 50% of ptrace tests pass.",
        "Ancestry-gating\nReverie pin bumps shipped.",
    )
    bloom = build_trigram_bloom(texts)

    for query in (
        "backend",
        "B3 means",
        "50% of ptrace",
        "ancestry-gating reverie",
        "PIN BUMPS",
    ):
        assert bloom_might_contain(bloom, query), query

    assert not bloom_might_contain(bloom, "zzzzzzzzzz")
    assert bloom_might_contain(bloom, "B3")
    assert bloom_might_contain(bloom, "Réverie")
    assert not query_is_bloom_eligible("B3")
    assert not query_is_bloom_eligible("Réverie")


def test_filter_is_order_independent_and_catalog_round_trips() -> None:
    first = build_trigram_bloom(("alpha beta", "gamma delta"))
    second = build_trigram_bloom(("gamma delta", "alpha beta", "alpha beta"))
    assert first == second

    catalog = first.catalog_obj()
    assert catalog["algorithm"] == TRIGRAM_BLOOM_ALGORITHM
    assert trigram_bloom_from_catalog(catalog) == first

    assert build_trigram_bloom(("abc",)).catalog_obj() == {
        "algorithm": TRIGRAM_BLOOM_ALGORITHM,
        "bit_count": 64,
        "hash_count": 7,
        "bits_base64": "AQoQgAAEIAA=",
        "trigram_count": 1,
    }


def test_archive_local_query_implementation_matches_catalog_filter() -> None:
    catalog = build_trigram_bloom(
        ("Backend B3 means more than 50% of ptrace tests pass.",)
    ).catalog_obj()

    assert _bloom_might_match(catalog, ("backend", "ptrace"), "filter")
    assert not _bloom_might_match(catalog, ("backend", "zzzzzz"), "filter")
    assert _bloom_might_match(catalog, ("B3",), "filter")
    assert _bloom_might_match(catalog, ("Réverie",), "filter")


@pytest.mark.parametrize(
    ("replacement", "message"),
    (
        ({"algorithm": "unknown"}, "unsupported value"),
        ({"bit_count": 65}, "power of two"),
        ({"hash_count": 0}, "expected 7"),
        ({"trigram_count": -1}, "non-negative integer"),
        ({"bits_base64": "!"}, "invalid base64"),
        ({"bits_base64": base64.b64encode(b"short").decode("ascii")}, "expected"),
    ),
)
def test_catalog_decoder_rejects_invalid_metadata(
    replacement: dict[str, JsonValue], message: str
) -> None:
    catalog = build_trigram_bloom(("some searchable text",)).catalog_obj()
    catalog.update(replacement)
    with pytest.raises(ValueError, match=message):
        trigram_bloom_from_catalog(catalog)
