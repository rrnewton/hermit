"""Tests for stable, evidence-backed timeline glossary records."""

from __future__ import annotations

from wrkviz.terminology import (
    GlossaryTerm,
    TermSource,
    glossary_catalog_markdown,
    glossary_term_id,
    plain_language_context_text,
    scan_terminology,
)


def test_term_ids_are_stable_url_safe_and_collision_resistant() -> None:
    first = glossary_term_id("exact-head")

    assert first == glossary_term_id("exact-head")
    assert first.startswith("term-exact-head-")
    assert first != glossary_term_id("exact head")
    assert first != glossary_term_id("EXACT-HEAD")
    assert all(character.islower() or character.isdigit() or character == "-" for character in first)


def test_scanned_terms_and_catalog_carry_the_same_stable_ids() -> None:
    terms = scan_terminology(
        (
            TermSource(1_775_000_000_000, "Use `exact-head` validation before release."),
            TermSource(1_775_000_001_000, "The exact-head check protects the release."),
        ),
        "America/New_York",
    )

    exact = next(term for term in terms if term.term == "exact-head")
    catalog = glossary_catalog_markdown("codex-test", terms)
    assert exact.term_id == glossary_term_id(exact.term)
    assert exact.term_id in catalog
    assert "# codex-test project glossary" in catalog
    assert "## Project overview" in catalog
    assert "### Definition" in catalog
    assert "### First-use evidence" in catalog


def test_plain_language_context_excludes_unsupported_definitions() -> None:
    supported = GlossaryTerm(
        term="exact-head",
        introduced_at_ms=1,
        occurrences=2,
        context="The exact-head check binds a release to one revision.",
        week="2026-W31",
        term_id=glossary_term_id("exact-head"),
        definition="A release check that requires the tested and landed revision to match.",
        definition_status="supported",
    )
    unsupported = GlossaryTerm(
        term="DBI",
        introduced_at_ms=2,
        occurrences=2,
        context="DBI remained blocked.",
        week="2026-W31",
        term_id=glossary_term_id("DBI"),
        definition="Insufficient evidence: the source never expands DBI.",
        definition_status="insufficient-evidence",
    )

    context = plain_language_context_text(
        "Widget runs guest software deterministically.", (supported, unsupported)
    )

    assert "Widget runs guest software deterministically" in context
    assert "exact-head" in context
    assert "DBI" not in context


def test_uppercase_prose_is_not_mistaken_for_project_acronyms() -> None:
    noise = "THEN SAME PARENT NOT IF OR"
    real = "KVM PMU IPC CI"
    terms = scan_terminology(
        (
            TermSource(1_775_000_000_000, f"{noise}. The system uses {real}."),
            TermSource(1_775_000_001_000, f"{noise}. Tests cover {real}."),
        ),
        "America/New_York",
    )
    names = {term.term for term in terms}

    assert names.isdisjoint(noise.split())
    assert names.issuperset(real.split())


def test_operational_backticks_do_not_leak_nested_terms() -> None:
    sources = (
        TermSource(
            1_775_000_000_000,
            "Use `tg --db widget`, `tg --help`, `agents_v17.db`, "
            "`~/.orc/sessions/<session-id>/`, and `~/temp/orc_transcripts/`. "
            "Keep `dagrun`, `conversation_state`, `Node.js`, and `ALL`. "
            "The prose says ALL work is ready. KVM PMU IPC CI.",
        ),
        TermSource(
            1_775_000_001_000,
            "ALL commands remain operational details. KVM PMU IPC CI.",
        ),
    )

    names = {term.term for term in scan_terminology(sources, "America/New_York")}

    assert names.issuperset(
        {"dagrun", "conversation_state", "Node.js", "KVM", "PMU", "IPC", "CI"}
    )
    assert names.isdisjoint(
        {
            "tg --db widget",
            "tg --help",
            "agents_v17.db",
            "~/.orc/sessions/<session-id>/",
            "~/temp/orc_transcripts/",
            "session-id",
            "ALL",
        }
    )


def test_short_inline_code_spans_do_not_turn_connectives_into_terms() -> None:
    terms = scan_terminology(
        (
            TermSource(
                1_775_000_000_000,
                r"Escape `[` and `]`, or write `\[` or `\]`; keep `recording_metadata`.",
            ),
        ),
        "America/New_York",
    )

    assert {term.term for term in terms} == {"recording_metadata"}


def test_explicit_candidate_filter_rejects_literals_and_prose_fragments() -> None:
    terms = scan_terminology(
        (
            TermSource(
                1_775_000_000_000,
                "Keep `dagrun`, `Guest::ppid`, and `Node.js`; reject "
                "`and`, `or`, `true`, `10m`, `4c70658e`, `.env.dbi`, "
                "`CARGO_BUILD_JOBS=16`, `(hours),`, `check the deploy`, and "
                "`cells, not among the 70`.",
            ),
        ),
        "America/New_York",
    )
    names = {term.term for term in terms}

    assert names == {"dagrun", "Guest::ppid", "Node.js"}


def test_term_cap_is_selected_by_eligibility_not_first_mention() -> None:
    terms = scan_terminology(
        (
            TermSource(100, "EARLY appears once but is not eligible yet."),
            TermSource(200, "Use stable-one and stable-two."),
            TermSource(300, "EARLY now appears a second time."),
        ),
        "America/New_York",
        limit=2,
    )

    assert [term.term for term in terms] == ["stable-one", "stable-two"]


def test_one_off_project_slug_is_preserved_with_first_use_chronology() -> None:
    terms = scan_terminology(
        (
            TermSource(100, "in-guest is project terminology."),
        ),
        "America/New_York",
    )

    assert [term.term for term in terms] == ["in-guest"]
    assert terms[0].introduced_at_ms == 100
    assert terms[0].summary_available_at_ms == 100


def test_unquoted_second_occurrence_sets_summary_availability() -> None:
    terms = scan_terminology(
        (
            TermSource(100, "DBI appears once."),
            TermSource(300, "DBI appears again."),
        ),
        "America/New_York",
    )

    dbi = next(term for term in terms if term.term == "DBI")
    assert dbi.introduced_at_ms == 100
    assert dbi.summary_available_at_ms == 300
