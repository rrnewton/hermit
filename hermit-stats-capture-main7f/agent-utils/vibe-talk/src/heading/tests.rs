//! The heading rules, one shape at a time. `#242 rich-headers`.
//!
//! Every example is neutral text in the SHAPE an agent writes: a who-am-i tag, sometimes a time,
//! a title, then the body.

use super::{extract, paren_candidate, Extracted, HeadingTags, TagStyle, MAX_AUTHORS};
use crate::model::{ChannelId, MessageId, UserId};

/// Every paren tag counts, as if each had been confirmed.
fn any(_: &str) -> bool {
    true
}

/// No paren tag counts.
fn none(_: &str) -> bool {
    false
}

type Read = Option<(String, Option<String>, Option<String>, String)>;

/// The heading of `content` from a bot account with every paren tag confirmed, as (tag, time,
/// title, restated words).
fn read(content: &str) -> Read {
    read_with(content, &any)
}

fn read_with(content: &str, confirmed: &dyn Fn(&str) -> bool) -> Read {
    let found = extract(content, true, confirmed)?;
    assert!(
        content.is_char_boundary(found.cut),
        "{content:?}: {found:?}"
    );
    let restated = content[..found.cut].to_owned();
    Some((found.tag, found.time, found.title, restated))
}

fn heading(tag: &str, time: Option<&str>, title: Option<&str>, restated: &str) -> Read {
    Some((
        tag.to_owned(),
        time.map(str::to_owned),
        title.map(str::to_owned),
        restated.to_owned(),
    ))
}

// ---- the owner's six shapes ----------------------------------------------------------------

#[test]
fn a_bracket_tag_a_title_and_a_time_in_parentheses_before_a_colon() {
    assert_eq!(
        read("[build-bot coordinator] Report (11:58 AM ET):\nAll four shards are synced."),
        heading(
            "build-bot coordinator",
            Some("11:58 AM ET"),
            Some("Report"),
            "[build-bot coordinator] Report (11:58 AM ET):"
        )
    );
}

#[test]
fn a_title_with_a_comma_in_it_and_the_time_in_parentheses_at_its_end() {
    assert_eq!(
        read(
            "[build-bot coordinator] Cache warm-up and cold starts, first figures (3:40 PM ET).\n\
             Figures follow."
        ),
        heading(
            "build-bot coordinator",
            Some("3:40 PM ET"),
            Some("Cache warm-up and cold starts, first figures"),
            "[build-bot coordinator] Cache warm-up and cold starts, first figures (3:40 PM ET)."
        )
    );
}

#[test]
fn the_owners_worked_example_splits_tag_time_title_and_body() {
    // The example the owner wrote out: tag, time, title, and the rest of the line is body.
    let content = "[build-bot coordinator] Backup check, 10:52 PM ET. All 4 shards are synced, \
                   ... Nothing needed fixing.";
    assert_eq!(
        read(content),
        heading(
            "build-bot coordinator",
            Some("10:52 PM ET"),
            Some("Backup check"),
            "[build-bot coordinator] Backup check, 10:52 PM ET. "
        )
    );
    let found = extract(content, true, &any).expect("a heading");
    assert_eq!(
        &content[found.cut..],
        "All 4 shards are synced, ... Nothing needed fixing."
    );
}

#[test]
fn a_tag_and_a_time_alone_have_no_title() {
    assert_eq!(
        read("[build-bot coordinator] 3:20 PM ET:\n- shard one is green"),
        heading(
            "build-bot coordinator",
            Some("3:20 PM ET"),
            None,
            "[build-bot coordinator] 3:20 PM ET:"
        )
    );
}

#[test]
fn a_paren_tag_with_a_title_and_a_24_hour_zulu_time() {
    assert_eq!(
        read("(ops2) Midday summary, 15:05Z.\nNothing is stuck."),
        heading(
            "ops2",
            Some("15:05Z"),
            Some("Midday summary"),
            "(ops2) Midday summary, 15:05Z."
        )
    );
}

#[test]
fn only_the_first_time_is_taken_and_an_aside_keeps_its_own() {
    // An aside with a time of its own stays whole in the title; only the first time is taken.
    assert_eq!(
        read(
            "(ops2) Weekly tally, 11:45Z (plan filed 02:57Z; expect 3-5 merges).\n\
             Body."
        ),
        heading(
            "ops2",
            Some("11:45Z"),
            Some("Weekly tally (plan filed 02:57Z; expect 3-5 merges)"),
            "(ops2) Weekly tally, 11:45Z (plan filed 02:57Z; expect 3-5 merges)."
        )
    );
}

// ---- the other forms measured in the corpus --------------------------------------------------

#[test]
fn a_trailing_paren_tag_with_a_time_before_the_end_of_the_sentence() {
    let found =
        extract("Release notes (ops2, 4:05 PM ET). Three fixes.", true, &any).expect("found");
    assert_eq!(found.style, TagStyle::Trailing);
    assert_eq!(
        read("Release notes (ops2, 4:05 PM ET). Three fixes."),
        heading(
            "ops2",
            Some("4:05 PM ET"),
            Some("Release notes"),
            "Release notes (ops2, 4:05 PM ET). "
        )
    );
    assert_eq!(
        read("Release notes (ops2): three fixes."),
        heading(
            "ops2",
            None,
            Some("Release notes"),
            "Release notes (ops2): "
        )
    );
}

#[test]
fn a_trailing_paren_tag_keeps_a_title_that_ends_in_a_full_stop_mid_line() {
    // The conversational rule is for a sentence after a leading tag; the trailing form IS a title.
    assert_eq!(
        read("Shard report (ops2). Everything is green."),
        heading("ops2", None, Some("Shard report"), "Shard report (ops2). ")
    );
}

#[test]
fn a_time_straight_after_the_tag_then_a_topic() {
    assert_eq!(
        read("[build-bot] 8:45 PM ET, Shard report: all green"),
        heading(
            "build-bot",
            Some("8:45 PM ET"),
            Some("Shard report"),
            "[build-bot] 8:45 PM ET, Shard report: "
        )
    );
}

#[test]
fn emphasis_around_the_whole_sentence_is_unwrapped() {
    assert_eq!(
        read("[build-bot] **Shard report, 8:45 PM ET.** All green."),
        heading(
            "build-bot",
            Some("8:45 PM ET"),
            Some("Shard report"),
            "[build-bot] **Shard report, 8:45 PM ET.** "
        )
    );
    assert_eq!(
        read("[build-bot] *Shard report*: all green"),
        heading(
            "build-bot",
            None,
            Some("Shard report"),
            "[build-bot] *Shard report*: "
        )
    );
    assert_eq!(
        read("[build-bot] **Shard report**\nall green"),
        heading(
            "build-bot",
            None,
            Some("Shard report"),
            "[build-bot] **Shard report**"
        )
    );
}

#[test]
fn twelve_and_twenty_four_hour_clocks_are_kept_exactly_as_written() {
    for (written, time) in [
        ("Status, 8 PM.", "8 PM"),
        ("Status, 8pm.", "8pm"),
        ("Status, 11:58 a.m. ET:", "11:58 a.m. ET"),
        ("Status, 13:58:", "13:58"),
        ("Status, 07:30 UTC.", "07:30 UTC"),
        ("Status at 9:15 PT:", "9:15 PT"),
        ("Status @ 9:15 PM:", "9:15 PM"),
    ] {
        let content = format!("[build-bot] {written} body");
        let found = extract(&content, true, &any).unwrap_or_else(|| panic!("{content:?}"));
        assert_eq!(found.time.as_deref(), Some(time), "{content:?}");
        assert_eq!(found.title.as_deref(), Some("Status"), "{content:?}");
    }
}

#[test]
fn a_time_in_the_middle_of_the_sentence_leaves_the_tag_alone() {
    // Neither the time nor the title: which time the sentence is ABOUT is not clear.
    assert_eq!(
        read("[build-bot] The 3 PM run failed on shard two: details below"),
        heading("build-bot", None, None, "[build-bot] ")
    );
}

#[test]
fn a_parenthesis_that_holds_more_than_the_time_stays_in_the_title() {
    assert_eq!(
        read("[build-bot] Status (see 3 PM): fine"),
        heading(
            "build-bot",
            None,
            Some("Status (see 3 PM)"),
            "[build-bot] Status (see 3 PM): "
        )
    );
}

#[test]
fn not_a_clock_a_version_a_ratio_or_a_time_with_seconds() {
    for content in [
        "[build-bot] Release 1.10:30 builds: done",
        "[build-bot] Ratio 3:2 holds: fine",
        "[build-bot] Run at 10:52:33 UTC: fine",
    ] {
        let found = extract(content, true, &any).unwrap_or_else(|| panic!("{content:?}"));
        assert_eq!(found.time, None, "{content:?}");
    }
}

#[test]
fn a_sentence_runs_past_a_clock_an_abbreviation_and_a_decimal() {
    assert_eq!(
        read("[build-bot] Latency is 1.5x, e.g. shard two: details"),
        heading(
            "build-bot",
            None,
            Some("Latency is 1.5x, e.g. shard two"),
            "[build-bot] Latency is 1.5x, e.g. shard two: "
        )
    );
}

// ---- titles that are dropped -----------------------------------------------------------------

#[test]
fn a_reply_with_a_full_stop_mid_line_is_not_a_title() {
    assert_eq!(
        read("[build-bot] Yes. The run is green."),
        heading("build-bot", None, None, "[build-bot] ")
    );
    // ...but a sentence that IS the whole line is one.
    assert_eq!(
        read("[build-bot] Done.\nThe run is green."),
        heading("build-bot", None, Some("Done"), "[build-bot] Done.")
    );
}

#[test]
fn a_title_longer_than_eighty_characters_leaves_the_tag_alone() {
    let long = "word ".repeat(20);
    let content = format!("[build-bot] {}: body", long.trim_end());
    assert_eq!(
        read(&content),
        heading("build-bot", None, None, "[build-bot] ")
    );
}

#[test]
fn markup_inside_the_title_leaves_the_tag_alone() {
    for content in [
        "[build-bot] Fixed the `parser` crash: details",
        "[build-bot] See [the run](https://example.com): green",
        "[build-bot] Done, https://example.com/run: green",
        "[build-bot] Ask <@123> about it: soon",
        "[build-bot] **Shard** report: all green",
        "[build-bot] Fish &amp; chips: lunch",
        "[build-bot] snake_case names: renamed",
    ] {
        assert_eq!(
            read(content),
            heading("build-bot", None, None, "[build-bot] "),
            "{content:?}"
        );
    }
}

#[test]
fn a_line_longer_than_the_window_gives_no_title() {
    let content = format!("[build-bot] {}", "a".repeat(400));
    assert_eq!(
        read(&content),
        heading("build-bot", None, None, "[build-bot] ")
    );
    // Seventy robots are under eighty characters but past the window's bytes: the sentence was cut
    // where the reading stopped, and a title cut mid-sentence is not one.
    let robots = format!("[build-bot] {} and more", "\u{1F916}".repeat(70));
    assert_eq!(
        read(&robots),
        heading("build-bot", None, None, "[build-bot] ")
    );
}

// ---- not headings at all ---------------------------------------------------------------------

#[test]
fn what_starts_like_a_tag_but_is_not_one_is_drawn_as_today() {
    for content in [
        "[the run](https://example.com) is green",
        "[the run][1] is green\n\n[1]: https://example.com",
        "[1/3] first part of a long answer",
        "[2] see the footnote",
        "[ ] a task left open",
        "[x] a task done",
        "[^1] the footnote",
        "[!NOTE] an alert",
        "[build-bot]: https://example.com",
        "[build-bot]no space after the tag",
        "[a tag with far too many words in it] body",
        "[*emphasis*] body",
    ] {
        assert_eq!(read(content), None, "{content:?}");
    }
    // A tag and nothing else is still a tag.
    assert_eq!(
        read("[build-bot] "),
        heading("build-bot", None, None, "[build-bot] ")
    );
}

#[test]
fn a_parenthesis_that_is_an_aside_or_a_stop_word_is_drawn_as_today() {
    for content in [
        "(edited) fixed the typo",
        "(cont.) and the rest of it",
        "(Sorry) wrong channel",
        "(answering your earlier question) yes, it is green",
        "(1/3) first part",
        "(ops2): status",
    ] {
        assert_eq!(read(content), None, "{content:?}");
    }
}

#[test]
fn a_message_with_no_tag_is_drawn_as_today() {
    for content in [
        "# A heading",
        "```\ncode\n```",
        "> a quote",
        "- a list item",
        "Backup check, 10:52 PM ET. All good.",
        "**Shard report, 8:45 PM ET.** All green.",
        "",
        "   [build-bot] indented",
        "\n[build-bot] after a blank line",
    ] {
        assert_eq!(read(content), None, "{content:?}");
    }
}

// ---- paren tags need two messages from the same author -------------------------------------

#[test]
fn a_paren_tag_needs_confirming() {
    let content = "(ops2) Midday summary, 15:05Z.";
    assert_eq!(read_with(content, &none), None);
    assert!(read_with(content, &any).is_some());
    assert_eq!(read_with("Release notes (ops2): body", &none), None);
    // A bracket tag needs nothing.
    assert!(read_with("[ops2] Midday summary, 15:05Z.", &none).is_some());
}

#[test]
fn the_candidate_is_the_paren_tag_in_either_place() {
    assert_eq!(
        paren_candidate("(ops2) Midday summary.", true),
        Some("ops2")
    );
    assert_eq!(
        paren_candidate("Release notes (ops2): body", true),
        Some("ops2")
    );
    assert_eq!(
        paren_candidate("Release notes (ops2, 4:05 PM ET). body", true),
        Some("ops2")
    );
    assert_eq!(paren_candidate("(edited) typo", true), None);
    assert_eq!(paren_candidate("[ops2] Midday summary.", true), None);
    assert_eq!(paren_candidate("plain words", true), None);
}

#[test]
fn a_trailing_tag_is_taken_only_from_a_bot_account() {
    // A person ending a sentence with a word in parentheses is writing an aside, not signing it,
    // however often they do it: the trailing form is neither a heading nor a candidate for them.
    let trailing = "Release notes (ops2): three fixes.";
    assert!(extract(trailing, true, &any).is_some());
    assert_eq!(extract(trailing, false, &any), None);
    assert_eq!(paren_candidate(trailing, false), None);
    assert_eq!(
        paren_candidate("Release notes (ops2, 4:05 PM ET). body", false),
        None
    );
    // A person's leading tags are theirs to use: a confirmed `(tag)` and any `[tag]`.
    let leading = "(ops2) Midday summary, 15:05Z.";
    assert_eq!(paren_candidate(leading, false), Some("ops2"));
    assert_eq!(
        extract(leading, false, &any).map(|found| (found.tag, found.title)),
        Some(("ops2".to_owned(), Some("Midday summary".to_owned())))
    );
    assert!(extract("[human] Ship it.", false, &none).is_some());
}

#[test]
fn an_aside_in_parentheses_is_not_a_tag_at_either_end() {
    for word in [
        "thanks",
        "Thanks",
        "lol",
        "finally",
        "maybe",
        "hopefully",
        "yes",
        "ok",
        "done",
        "btw",
    ] {
        for content in [
            format!("({word}) it works now"),
            format!("It works now ({word})."),
            format!("It works now ({word}): details"),
        ] {
            assert_eq!(read(&content), None, "{content:?}");
            assert_eq!(paren_candidate(&content, true), None, "{content:?}");
        }
    }
}

#[test]
fn an_emoji_shortcode_is_not_cut_in_half_into_a_title() {
    // The closing colon would end the sentence and leave `:tada` as the title, where the provider
    // draws a picture. A shortcode is markup, so the heading is the tag alone.
    for content in [
        "[build-bot] :tada: shipped",
        "[build-bot] Shipped :rocket: today: details",
        "[build-bot] Review :+1: from both: merging",
        "[build-bot] :white_check_mark:",
    ] {
        assert_eq!(
            read(content),
            heading("build-bot", None, None, "[build-bot] "),
            "{content:?}"
        );
    }
    // A clock's colon is not one.
    assert_eq!(
        read("[build-bot] Status (see 9:05 AM ET): body"),
        heading(
            "build-bot",
            None,
            Some("Status (see 9:05 AM ET)"),
            "[build-bot] Status (see 9:05 AM ET): "
        )
    );
}

#[test]
fn two_messages_from_one_author_confirm_a_tag_and_one_or_two_authors_do_not() {
    let tags = HeadingTags::new();
    let channel = ChannelId("c1".into());
    let (agent, person) = (UserId("u1".into()), UserId("u2".into()));
    tags.record(&channel, &agent, &MessageId("m1".into()), "ops2");
    assert!(!tags.confirmed(&channel, &agent, "ops2"), "one message");
    // The same message read again is still one message.
    tags.record(&channel, &agent, &MessageId("m1".into()), "ops2");
    assert!(
        !tags.confirmed(&channel, &agent, "ops2"),
        "one message, read twice"
    );
    // Another author's message with the same tag does not count for this one.
    tags.record(&channel, &person, &MessageId("m2".into()), "ops2");
    assert!(!tags.confirmed(&channel, &agent, "ops2"), "two authors");
    assert!(!tags.confirmed(&channel, &person, "ops2"), "two authors");
    tags.record(&channel, &agent, &MessageId("m3".into()), "ops2");
    assert!(tags.confirmed(&channel, &agent, "ops2"), "two messages");
    // Per channel.
    assert!(!tags.confirmed(&ChannelId("c2".into()), &agent, "ops2"));
}

#[test]
fn the_record_is_bounded() {
    let tags = HeadingTags::new();
    let channel = ChannelId("c1".into());
    for index in 0..MAX_AUTHORS + 50 {
        let author = UserId(format!("u{index}"));
        tags.record(&channel, &author, &MessageId(format!("m{index}")), "ops2");
    }
    assert_eq!(tags.authors(), MAX_AUTHORS);
    // The oldest was forgotten, the newest kept.
    let newest = UserId(format!("u{}", MAX_AUTHORS + 49));
    tags.record(&channel, &newest, &MessageId("again".into()), "ops2");
    assert!(tags.confirmed(&channel, &newest, "ops2"));
    let oldest = UserId("u0".into());
    tags.record(&channel, &oldest, &MessageId("again".into()), "ops2");
    assert!(
        !tags.confirmed(&channel, &oldest, "ops2"),
        "forgotten, so one message"
    );

    // At most so many tags per author: the first is forgotten when one too many arrive.
    let agent = UserId("tagger".into());
    for index in 0..=super::MAX_TAGS_PER_AUTHOR {
        tags.record(
            &channel,
            &agent,
            &MessageId("m".into()),
            &format!("t{index}"),
        );
    }
    tags.record(&channel, &agent, &MessageId("n".into()), "t0");
    assert!(!tags.confirmed(&channel, &agent, "t0"));
    tags.record(&channel, &agent, &MessageId("n".into()), "t5");
    assert!(tags.confirmed(&channel, &agent, "t5"));
}

// ---- where the cut falls ---------------------------------------------------------------------

#[test]
fn crlf_counts_the_carriage_return_into_the_cut_when_the_line_is_used_up() {
    assert_eq!(
        read("[build-bot] Status, 9:05 AM ET:\r\nAll green."),
        heading(
            "build-bot",
            Some("9:05 AM ET"),
            Some("Status"),
            "[build-bot] Status, 9:05 AM ET:\r"
        )
    );
    assert_eq!(
        read("[build-bot] Status: all green.\r\nMore."),
        heading("build-bot", None, Some("Status"), "[build-bot] Status: ")
    );
}

#[test]
fn the_cut_on_the_wire_is_in_utf16_units() {
    let content = "[b\u{fc}ld-b\u{f8}t \u{1F916}] R\u{e9}sum\u{e9}, 9:05 AM ET: body";
    let found: Extracted = extract(content, true, &any).expect("a heading");
    assert_eq!(&content[found.cut..], "body");
    let wire = found.wire(content);
    let units = content[..found.cut].encode_utf16().count();
    assert_eq!(wire.cut as usize, units);
    // The robot is two units and four bytes; each of the four accented letters one unit and two
    // bytes.
    assert_eq!(found.cut, units + 2 + 4);
    assert_eq!(wire.tag.as_deref(), Some("b\u{fc}ld-b\u{f8}t \u{1F916}"));
    assert_eq!(wire.title.as_deref(), Some("R\u{e9}sum\u{e9}"));
    assert_eq!(wire.time.as_deref(), Some("9:05 AM ET"));
}

#[test]
fn a_tag_alone_cuts_only_the_tag_and_its_spaces() {
    let content = "[build-bot]   Yes. The run is green.";
    let found = extract(content, true, &any).expect("a heading");
    assert_eq!(&content[found.cut..], "Yes. The run is green.");
}
