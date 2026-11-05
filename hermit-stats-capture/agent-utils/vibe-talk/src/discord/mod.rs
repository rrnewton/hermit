//! Discord implementation of the provider-neutral chat interface.
//!
//! The Discord implementation reads recent messages and posts replies on demand. It has no
//! Gateway connection or Discord webhook receiver; provider-neutral push ingestion, when a
//! deployment enables it, enters through the separate adapter endpoint in [`crate::live`].

pub mod fake;
pub mod http;
pub mod markup;
pub mod ratelimit;
pub mod split;

// Compatibility re-exports for callers that used the original Discord-specific public names.
// New server code depends on `crate::chat` directly.
pub use crate::chat::{
    ChatClient as DiscordClient, ChatError as DiscordError, ChatIdentity as BotIdentity,
};

/// The bot's own Discord user id, read out of its token.
///
/// A bot token is three dot-separated parts, and the FIRST is the bot's user id in base64url. So
/// the account this server posts as is knowable without a single API call, without a new trait
/// method for every client and fake to implement, and without a startup step that can fail.
///
/// # Why this matters at all
///
/// The channel view colours a message by who sent it, and the one account it must recognise is
/// this bridge's own -- those messages are the OWNER's words, posted on his behalf. The page used
/// to learn that id only as a side effect of the reader replying from the app or of the live feed
/// delivering a `self_posted` message. A reader who had done neither saw every message fall
/// through to the same "somebody else" colour, including their own, which is exactly the bug this
/// closes: with the bridge unidentified, it also counts as a second bot, and the "the only bot
/// that is not us" guess for the coding agent becomes a coin toss it declines to call.
///
/// Returns `None` rather than guessing when the token is not in that shape. Degrading to the old
/// learned-later behaviour is correct; inventing an id would mislabel somebody else's messages as
/// the owner's.
#[must_use]
pub fn self_user_id_from_token(token: &str) -> Option<String> {
    let first = token.trim().split('.').next()?;
    if first.is_empty() {
        return None;
    }
    let decoded = base64_url_decode(first)?;
    let id = String::from_utf8(decoded).ok()?;
    // A snowflake is decimal digits and nothing else. Anything else means this was not the shape
    // assumed, and a wrong id is worse than no id.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

/// Decode unpadded base64url. Small and local: this is the only base64 this crate needs.
fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    const fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let six = value(byte)?;
        buffer = (buffer << 6) | u32::from(six);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod self_id_tests {
    use super::self_user_id_from_token;

    #[test]
    fn a_bot_token_carries_the_bots_own_user_id_in_its_first_segment() {
        // A real-shaped token: base64url("1000000000000000009") . timestamp . hmac. The id is what
        // lets the channel view tell the owner's own words from everybody else's on the first
        // render, with no API call and no waiting for him to reply.
        let token = "MTAwMDAwMDAwMDAwMDAwMDAwOQ.Gabcde.fghijklmnopqrstuvwxyz0123456789";
        assert_eq!(
            self_user_id_from_token(token).as_deref(),
            Some("1000000000000000009")
        );
    }

    #[test]
    fn anything_not_in_that_shape_is_none_rather_than_a_guess() {
        // A wrong id is worse than no id: it would paint somebody else's messages as the owner's.
        for token in [
            "",
            ".x.y",
            "not-base64!!!.x.y",
            // Decodes cleanly, but to letters rather than a snowflake.
            "aGVsbG8.x.y",
            // Decodes to something with a non-digit in the middle.
            "MTIzNDU2Nzg5YQ.x.y",
        ] {
            assert!(
                self_user_id_from_token(token).is_none(),
                "{token:?} produced an id"
            );
        }
    }

    #[test]
    fn the_dots_are_not_required_because_only_the_first_segment_is_read() {
        // Said out loud because it looks like an oversight and is not: a caller that hands over
        // just the first segment gets the same answer. Nothing here validates that a token is
        // WELL-FORMED -- that is Discord's job, and it answers 401. This only reads an id out of
        // one, and refuses when what it reads is not one.
        assert_eq!(
            self_user_id_from_token("MTAwMDAwMDAwMDAwMDAwMDAwOQ").as_deref(),
            Some("1000000000000000009")
        );
    }
}
