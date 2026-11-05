//! Discord's mention syntax, as the page's renderer reads it. `#217 markdown-blocks`.
//!
//! `<@123>` notifies a user and `<#456>` names a channel. To CommonMark both are text — neither is
//! an autolink or a tag — so without this they would be drawn as the angle brackets they are.
//! [`crate::render`] cuts them out of the text and draws each as a chip: the id with `@` or `#`,
//! never a display name, because this page has no user directory and inventing a name is exactly
//! the kind of thing the channel view exists to catch.
//!
//! Every provider uses this form. Slack's reader keeps `<@U…>` verbatim
//! ([`crate::slack::mrkdwn`]), and a Discord-protocol bridge writes Discord's.

/// The longest id read as a mention. A Discord snowflake is at most twenty digits and a Slack id
/// a dozen characters; anything longer is text.
const MAX_ID: usize = 32;

/// The mention at the start of `text`, as its length in bytes and the chip's text.
///
/// `<@id>` and `<@!id>` (the nickname form) are a user, `<#id>` a channel, where an id is one to
/// [`MAX_ID`] ASCII letters and digits — Discord's are digits, Slack's start with a capital. A
/// role, `<@&id>`, is left as text, as the page has always drawn it.
///
/// The chip's characters are exactly the sigil and the id, so it needs no escaping in HTML.
///
/// The closing `>` is looked for only as far as the longest id could reach. A search to the end of
/// the text would read the whole rest of the message for every `<@` in it, and a message of
/// nothing but `<@` would take time that grew with the square of its length.
#[must_use]
pub fn mention_at(text: &str) -> Option<(usize, String)> {
    let (sigil, after) = if let Some(rest) = text.strip_prefix("<@!") {
        ('@', rest)
    } else if let Some(rest) = text.strip_prefix("<@") {
        ('@', rest)
    } else {
        ('#', text.strip_prefix("<#")?)
    };
    let end = after
        .bytes()
        .take(MAX_ID + 1)
        .position(|byte| byte == b'>')?;
    let id = &after[..end];
    if id.is_empty() || id.len() > MAX_ID || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some((text.len() - after.len() + end + 1, format!("{sigil}{id}")))
}

#[cfg(test)]
mod tests {
    use super::mention_at;

    #[test]
    fn a_user_a_nickname_and_a_channel_are_mentions() {
        assert_eq!(mention_at("<@123> hi"), Some((6, "@123".to_owned())));
        assert_eq!(mention_at("<@!123>"), Some((7, "@123".to_owned())));
        assert_eq!(mention_at("<#456>."), Some((6, "#456".to_owned())));
        assert_eq!(
            mention_at("<@U0123ABC>"),
            Some((11, "@U0123ABC".to_owned()))
        );
    }

    #[test]
    fn anything_else_in_angle_brackets_is_text() {
        for text in [
            "<@&123>",
            "<@>",
            "<#>",
            "<@12 3>",
            "<@123",
            "<b>",
            "<https://example.com>",
            "<@<script>>",
            &format!("<@{}>", "1".repeat(33)),
        ] {
            assert_eq!(mention_at(text), None, "{text:?} was read as a mention");
        }
    }

    #[test]
    fn a_message_of_unclosed_mentions_is_read_in_time_that_grows_with_its_length() {
        // Each `<@` once searched the whole rest of the text for its `>`. That search is fast enough
        // that it takes a megabyte of them to show, which is why the text is so long.
        let text = "<@".repeat(512 * 1024);
        let started = std::time::Instant::now();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            assert_eq!(mention_at(rest), None);
            rest = &rest[2..];
        }
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "a megabyte of unclosed mentions took {took:?}"
        );
        // The longest id still closes.
        let longest = format!("<@{}>", "9".repeat(32));
        assert_eq!(
            mention_at(&longest),
            Some((35, format!("@{}", "9".repeat(32))))
        );
    }
}
