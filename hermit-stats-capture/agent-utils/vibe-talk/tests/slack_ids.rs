//! Slack identifiers, links and text, without a network: the pure half of `vibe_talk::slack`.

use vibe_talk::chat::{ChatClient as _, SourceClaim};
use vibe_talk::config::Secret;
use vibe_talk::model::MessageId;
use vibe_talk::slack::ids::{is_conversation_id, ChannelRef};
use vibe_talk::slack::mrkdwn::{escape_outgoing, to_markdown};
use vibe_talk::slack::{
    message_id_from_ts, parse_source, ts_from_message_id, HttpSlackClient, SlackConfig,
    DEFAULT_SLACK_API_BASE,
};

fn id(ts: &str) -> MessageId {
    message_id_from_ts(ts).unwrap_or_else(|| panic!("{ts} encodes"))
}

#[test]
fn a_ts_round_trips_exactly_through_its_message_id() {
    for ts in [
        "1700000000.000100",
        "1700000000.000000",
        "1700000000.999999",
        "1420070400.000000",
        "1420070400.000001",
        "4102444800.123456",
    ] {
        let encoded = id(ts);
        assert_eq!(
            ts_from_message_id(&encoded).as_deref(),
            Some(ts),
            "{ts} did not round-trip through {encoded}"
        );
    }
}

#[test]
fn a_short_fraction_is_normalised_to_six_digits() {
    assert_eq!(id("1700000000.0001"), id("1700000000.000100"));
    assert_eq!(
        ts_from_message_id(&id("1700000000.5")).as_deref(),
        Some("1700000000.500000")
    );
    assert_eq!(
        ts_from_message_id(&id("1700000000")).as_deref(),
        Some("1700000000.000000")
    );
}

#[test]
fn malformed_and_unrepresentable_ts_values_are_refused() {
    for bad in [
        "",
        ".",
        "1700000000.",
        ".000100",
        "1700000000.0000001",
        "1700000000.00a100",
        "-1700000000.000100",
        "+1700000000.000100",
        "1700000000.000100.1",
        " 1700000000.000100",
        "9999999999999.000000",
        // Before Discord's epoch: a snowflake cannot be negative.
        "1420070399.999999",
        "0.000000",
        // Past the 42 timestamp bits (2154).
        "5820000000.000000",
    ] {
        assert_eq!(message_id_from_ts(bad), None, "{bad:?} was accepted");
    }
}

#[test]
fn ids_this_backend_did_not_issue_are_refused_when_decoded() {
    for bad in ["", "abc", "+5", "-5", "12x", "99999999999999999999999"] {
        assert_eq!(
            ts_from_message_id(&MessageId(bad.to_owned())),
            None,
            "{bad:?}"
        );
    }
    // Low bits above 999 cannot come from a microsecond remainder.
    let foreign = MessageId(((1_u64 << 22) | 1000).to_string());
    assert_eq!(ts_from_message_id(&foreign), None);
}

#[test]
fn encoded_ids_order_exactly_as_their_ts_values_do() {
    let ordered = [
        "1420070400.000000",
        "1420070400.000001",
        "1420070400.000999",
        "1420070400.001000",
        "1699999999.999999",
        "1700000000.000000",
        "1700000000.000100",
        "1700000000.000101",
        "1700000000.001100",
        "1700000001.000000",
        "1800000000.000000",
    ];
    let numeric: Vec<u64> = ordered
        .iter()
        .map(|ts| id(ts).numeric().expect("numeric"))
        .collect();
    assert!(
        numeric.windows(2).all(|pair| pair[0] < pair[1]),
        "encoded ids are not strictly increasing: {numeric:?}"
    );
}

#[test]
fn the_creation_instant_is_the_ts_millisecond() {
    assert_eq!(
        id("1700000000.123456").created_at_ms(),
        Some(1_700_000_000_123)
    );
    assert_eq!(
        id("1700000000.000999").created_at_ms(),
        Some(1_700_000_000_000)
    );
}

#[test]
fn a_time_boundary_decodes_to_the_instant_it_names_and_sorts_between_messages() {
    let boundary = MessageId::at_time_ms(1_700_000_000_123);
    assert_eq!(
        ts_from_message_id(&boundary).as_deref(),
        Some("1700000000.123000")
    );
    let n = |id: &MessageId| id.numeric().expect("numeric");
    assert!(n(&id("1700000000.122999")) < n(&boundary));
    assert!(n(&boundary) <= n(&id("1700000000.123000")));
    assert!(n(&boundary) < n(&id("1700000000.123001")));
    // The live poller seeds an empty channel at 0: the epoch, which is still a valid bound.
    assert_eq!(
        ts_from_message_id(&MessageId("0".to_owned())).as_deref(),
        Some("1420070400.000000")
    );
}

#[test]
fn conversation_ids_and_thread_scoped_channel_ids_parse_strictly() {
    for good in ["C0123ABCD", "G01234567", "D0ABCDEFGH", "C0123ABCDEFGHIJ"] {
        assert!(is_conversation_id(good), "{good}");
    }
    for bad in [
        "C0123ABC",
        "c0123ABCD",
        "U0123ABCD",
        "C0123-ABCD",
        "C0123abcd",
        "",
    ] {
        assert!(!is_conversation_id(bad), "{bad}");
    }
    let scoped = ChannelRef::parse("C0123ABCD~1700000000.000100").expect("scoped id");
    assert_eq!(scoped.conversation, "C0123ABCD");
    assert_eq!(scoped.thread_ts.as_deref(), Some("1700000000.000100"));
    assert_eq!(scoped.channel_id(), "C0123ABCD~1700000000.000100");
    assert!(vibe_talk::directory::path_safe_id(&scoped.channel_id()));
    // A channel id must already be canonical; it is a durable key.
    assert_eq!(ChannelRef::parse("C0123ABCD~1700000000.0001"), None);
    assert_eq!(ChannelRef::parse("C0123ABCD~"), None);
    assert_eq!(ChannelRef::parse("1532416065114607829"), None);
}

#[test]
fn every_supported_link_shape_parses_to_its_conversation_and_thread() {
    let thread = Some("1700000000.000100".to_owned());
    for (source, expected) in [
        ("C0123ABCD", ("C0123ABCD", None)),
        ("  G0123ABCD  ", ("G0123ABCD", None)),
        ("C0123ABCD~1700000000.000100", ("C0123ABCD", thread.clone())),
        ("C0123ABCD~1700000000.0001", ("C0123ABCD", thread.clone())),
        (
            "https://example.slack.com/archives/C0123ABCD",
            ("C0123ABCD", None),
        ),
        (
            "https://example.slack.com/archives/C0123ABCD/p1700000000000100",
            ("C0123ABCD", thread.clone()),
        ),
        (
            // A link to a REPLY names the thread it belongs to.
            "https://example.slack.com/archives/C0123ABCD/p1700000099000200?thread_ts=1700000000.000100&cid=C0123ABCD",
            ("C0123ABCD", thread.clone()),
        ),
        (
            "https://acme.enterprise.slack.com/archives/D0123ABCD/p1700000000000100",
            ("D0123ABCD", thread.clone()),
        ),
        (
            "https://app.slack.com/client/T0123ABCD/C0123ABCD",
            ("C0123ABCD", None),
        ),
        (
            "https://app.slack.com/client/E0123ABCD/C0123ABCD/thread/C0123ABCD-1700000000.000100",
            ("C0123ABCD", thread.clone()),
        ),
    ] {
        assert_eq!(
            parse_source(source),
            Some((expected.0.to_owned(), expected.1)),
            "{source}"
        );
    }
    for bad in [
        "",
        "general",
        "https://example.com/archives/C0123ABCD",
        "http://example.slack.com/archives/C0123ABCD",
        "https://example.slack.com/archives/C0123ABCD/p170000000000010",
        "https://example.slack.com/archives/U0123ABCD",
        "https://example.slack.com/messages/C0123ABCD",
        "https://app.slack.com/client/T0123ABCD/C0123ABCD/thread/C9999ABCD-1700000000.000100",
        "https://evil.example/slack.com/archives/C0123ABCD",
        "https://discord.com/channels/1/2",
        "1532416065114607829",
    ] {
        assert_eq!(parse_source(bad), None, "{bad}");
    }
}

fn offline_client() -> HttpSlackClient {
    HttpSlackClient::new(&SlackConfig {
        provider_name: "Slack".to_owned(),
        api_base: format!("{DEFAULT_SLACK_API_BASE}/"),
        token: Secret::new("xoxb-offline"),
        owner_user_id: Some(" W0123ABCD ".to_owned()),
        request_timeout_seconds: 5,
        channel_registration: true,
        registered_channels_writable: false,
        channel_discovery: false,
        upstream_clear: false,
        max_post_chars: None,
        max_post_bytes: None,
    })
    .expect("client")
}

#[test]
fn the_client_claims_slack_references_and_nothing_else() {
    let client = offline_client();
    for ours in [
        "C0123ABCD",
        "C0123ABCD~1700000000.000100",
        "https://example.slack.com/archives/C0123ABCD/p1700000000000100",
        "https://app.slack.com/client/T0123ABCD/C0123ABCD",
        // A Slack link this client cannot parse is still Slack's; registration explains why.
        "https://example.slack.com/huddle/T0123/C0123ABCD",
    ] {
        assert_eq!(client.claims_source(ours), SourceClaim::Certain, "{ours}");
    }
    for theirs in [
        "1532416065114607829",
        "https://discord.com/channels/1/2",
        "https://chat.google.com/room/AAAA",
        "general",
        "",
    ] {
        assert_eq!(client.claims_source(theirs), SourceClaim::Never, "{theirs}");
    }
    assert_eq!(client.owner_author_id().as_deref(), Some("W0123ABCD"));
    assert_eq!(
        client.self_author_id(),
        None,
        "unknown until auth.test or a post"
    );
    assert!(client.supports_threading());
    assert!(!client.supports_upstream_read_mark());
    assert!(
        !client.supports_upstream_clear(),
        "clearing unread in Slack is off unless configured"
    );
    assert!(client.supports_channel_registration());
    assert!(!client.supports_channel_discovery());
}

#[test]
fn a_non_http_api_base_is_refused_at_construction() {
    let mut config = SlackConfig {
        provider_name: "Slack".to_owned(),
        api_base: "slack.com/api".to_owned(),
        token: Secret::new("xoxb-offline"),
        owner_user_id: None,
        request_timeout_seconds: 5,
        channel_registration: false,
        registered_channels_writable: false,
        channel_discovery: false,
        upstream_clear: false,
        max_post_chars: None,
        max_post_bytes: None,
    };
    assert!(HttpSlackClient::new(&config).is_err());
    config.api_base = "ftp://slack.com/api".to_owned();
    assert!(HttpSlackClient::new(&config).is_err());
}

#[test]
fn mrkdwn_reads_as_the_markdown_the_rest_of_the_server_understands() {
    for (slack, expected) in [
        (
            "see <https://example.com|the docs>",
            "see [the docs](https://example.com)",
        ),
        ("see <https://example.com>", "see https://example.com"),
        (
            "<https://example.com|https://example.com>",
            "https://example.com",
        ),
        (
            "<mailto:a@example.com|a@example.com>",
            "[a@example.com](mailto:a@example.com)",
        ),
        ("in <#C0123ABCD|general>", "in #general"),
        ("in <#C0123ABCD>", "in #C0123ABCD"),
        ("<!here> <!channel> <!everyone>", "@here @channel @everyone"),
        ("<!here|@here> now", "@here now"),
        ("<!subteam^S0123|@oncall> look", "@oncall look"),
        ("<!subteam^S0123|oncall> look", "@oncall look"),
        (
            "at <!date^1700000000^{date_short}|Nov 14, 2023>",
            "at Nov 14, 2023",
        ),
        ("hi <@U0123ABCD>!", "hi <@U0123ABCD>!"),
        ("hi <@W0123ABCD|ada>", "hi <@W0123ABCD>"),
        ("a &amp; b &lt;c&gt;", "a & b <c>"),
        ("&amp;lt; stays one level", "&lt; stays one level"),
        (
            "<https://x.example/?a=1&amp;b=2|q &amp; a>",
            "[q & a](https://x.example/?a=1&b=2)",
        ),
        ("unclosed < bracket", "unclosed < bracket"),
        ("a <b <https://x.example>", "a <b https://x.example"),
        ("&unknown; entity", "&unknown; entity"),
        ("", ""),
        ("plain *bold* _it_", "plain *bold* _it_"),
    ] {
        assert_eq!(to_markdown(slack), expected, "{slack:?}");
    }
}

#[test]
fn outgoing_text_keeps_user_mentions_live_and_nothing_else() {
    assert_eq!(
        escape_outgoing("<@U0123ABCD> deploy is green & <!channel> <#C1> <https://x|y> a>b"),
        "<@U0123ABCD> deploy is green &amp; &lt;!channel&gt; &lt;#C1&gt; &lt;https://x|y&gt; a&gt;b"
    );
    assert_eq!(escape_outgoing("<@W0123ABCD>"), "<@W0123ABCD>");
    assert_eq!(escape_outgoing("<@here>"), "&lt;@here&gt;");
    assert_eq!(escape_outgoing("<@u0123>"), "&lt;@u0123&gt;");
    assert_eq!(escape_outgoing("plain @here"), "plain @here");
    // Reading back what was escaped gives the original text.
    let original = "q & a <with> brackets <@U0123ABCD>";
    assert_eq!(to_markdown(&escape_outgoing(original)), original);
}
