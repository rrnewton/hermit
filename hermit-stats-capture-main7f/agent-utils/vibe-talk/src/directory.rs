//! Provider-neutral channel discovery. `#19 channel-browser`.
//!
//! A registration-capable bridge may also list the named channels its account can see, so the
//! operator can pick one instead of pasting a link. Everything provider-specific stays behind the
//! bridge: a source is an opaque string that is handed back, unchanged, to channel registration,
//! and a cursor is an opaque token that is handed back, unchanged, for the next page.

use serde::Serialize;

use crate::chat::ChatError;
use crate::model::ChannelId;

/// Most characters a search may contain after trimming.
pub const MAX_QUERY_CHARS: usize = 100;
/// Most bytes an opaque source or cursor may contain.
pub const MAX_TOKEN_BYTES: usize = 2048;
/// Most characters a channel's display name may contain.
pub const MAX_NAME_CHARS: usize = 200;
/// Entries requested per page when the caller does not say.
pub const DEFAULT_LIMIT: u16 = 25;
/// Most entries one page may request.
pub const MAX_LIMIT: u16 = 50;

/// One page of the directory, as asked of the backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectoryRequest {
    /// Case-insensitive name filter, already trimmed; `None` lists everything.
    pub query: Option<String>,
    /// Opaque continuation from the previous page of the same query.
    pub cursor: Option<String>,
    /// Entries wanted, `1..=MAX_LIMIT`.
    pub limit: u16,
}

/// A channel the bridge's account can see.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirectoryEntry {
    /// Opaque reference accepted by channel registration.
    pub source: String,
    /// Stable display name.
    pub name: String,
    /// The bridge's existing whole-channel registration for this source, when it has one.
    pub registered_channel_id: Option<ChannelId>,
}

/// One page of discovered channels, in the bridge's order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectoryPage {
    /// Entries on this page.
    pub entries: Vec<DirectoryEntry>,
    /// Continuation for the next page; `None` on the last one.
    pub next_cursor: Option<String>,
    /// Whether the bridge stopped listing before it reached the end of what the account can see.
    pub truncated: bool,
}

fn shape(detail: &str) -> ChatError {
    ChatError::Shape(format!("channel directory answer {detail}"))
}

fn opaque_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TOKEN_BYTES && !value.chars().any(char::is_control)
}

/// Whether `id` is safe to place in one URL path segment, as registered channel ids must be.
#[must_use]
pub fn path_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

/// Read a bridge's answer to a directory request of at most `limit` entries.
///
/// Strict, because every string here is later drawn on screen or sent back upstream: a missing
/// name, an oversized token, a control character, or more entries than were asked for is a
/// malformed answer rather than something to repair.
///
/// # Errors
///
/// Returns [`ChatError::Shape`] when the answer does not follow the contract.
pub fn parse_directory_page(
    value: &serde_json::Value,
    limit: u16,
) -> Result<DirectoryPage, ChatError> {
    let raw_entries = value
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| shape("has no \"entries\" array"))?;
    if raw_entries.len() > usize::from(limit) {
        return Err(shape("has more entries than were requested"));
    }
    let mut entries = Vec::with_capacity(raw_entries.len());
    for raw in raw_entries {
        let source = raw
            .get("source")
            .and_then(serde_json::Value::as_str)
            .filter(|source| opaque_token(source))
            .ok_or_else(|| shape("has an entry without a usable \"source\""))?;
        let name = raw
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| {
                !name.is_empty()
                    && name.chars().count() <= MAX_NAME_CHARS
                    && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| shape("has an entry without a usable \"name\""))?;
        let registered_channel_id = match raw.get("registered_channel_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(id) => Some(
                id.as_str()
                    .filter(|id| path_safe_id(id))
                    .map(|id| ChannelId(id.to_owned()))
                    .ok_or_else(|| shape("has a malformed \"registered_channel_id\""))?,
            ),
        };
        entries.push(DirectoryEntry {
            source: source.to_owned(),
            name: name.to_owned(),
            registered_channel_id,
        });
    }
    let next_cursor = match value.get("next_cursor") {
        None | Some(serde_json::Value::Null) => None,
        Some(cursor) => Some(
            cursor
                .as_str()
                .filter(|cursor| opaque_token(cursor))
                .map(str::to_owned)
                .ok_or_else(|| shape("has a malformed \"next_cursor\""))?,
        ),
    };
    let truncated = match value.get("truncated") {
        None => false,
        Some(flag) => flag
            .as_bool()
            .ok_or_else(|| shape("has a non-boolean \"truncated\""))?,
    };
    Ok(DirectoryPage {
        entries,
        next_cursor,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_well_formed_page_is_read_in_the_bridges_order() {
        let page = parse_directory_page(
            &json!({
                "entries": [
                    {"source": "room/b", "name": "  Beta  ", "registered_channel_id": "reg-7"},
                    {"source": "room/a", "name": "Alpha", "registered_channel_id": null},
                    {"source": "room/c", "name": "Gamma"},
                ],
                "next_cursor": "opaque-2",
                "truncated": true,
            }),
            3,
        )
        .expect("valid page");
        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Beta", "Alpha", "Gamma"]
        );
        assert_eq!(
            page.entries[0].registered_channel_id,
            Some(ChannelId("reg-7".to_owned()))
        );
        assert_eq!(page.entries[1].registered_channel_id, None);
        assert_eq!(page.next_cursor.as_deref(), Some("opaque-2"));
        assert!(page.truncated);

        let last = parse_directory_page(&json!({"entries": []}), 25).expect("empty last page");
        assert_eq!(last, DirectoryPage::default());
    }

    #[test]
    fn malformed_answers_are_refused_rather_than_repaired() {
        let long_name = "n".repeat(MAX_NAME_CHARS + 1);
        let long_token = "t".repeat(MAX_TOKEN_BYTES + 1);
        for (bad, why) in [
            (json!({}), "entries missing"),
            (json!({"entries": {}}), "entries not an array"),
            (json!({"entries": [{"name": "x"}]}), "source missing"),
            (
                json!({"entries": [{"source": "", "name": "x"}]}),
                "source empty",
            ),
            (
                json!({"entries": [{"source": long_token, "name": "x"}]}),
                "source too long",
            ),
            (json!({"entries": [{"source": "s"}]}), "name missing"),
            (
                json!({"entries": [{"source": "s", "name": "   "}]}),
                "name blank",
            ),
            (
                json!({"entries": [{"source": "s", "name": "a\nb"}]}),
                "name has a control",
            ),
            (
                json!({"entries": [{"source": "s", "name": long_name}]}),
                "name too long",
            ),
            (
                json!({"entries": [{"source": "s", "name": "x", "registered_channel_id": "../x"}]}),
                "registered id not path-safe",
            ),
            (
                json!({"entries": [{"source": "s", "name": "x", "registered_channel_id": 7}]}),
                "registered id not a string",
            ),
            (json!({"entries": [], "next_cursor": ""}), "cursor empty"),
            (
                json!({"entries": [], "next_cursor": 3}),
                "cursor not a string",
            ),
            (
                json!({"entries": [], "truncated": "yes"}),
                "truncated not a boolean",
            ),
            (
                json!({"entries": [{"source": "a", "name": "a"}, {"source": "b", "name": "b"}]}),
                "more entries than requested",
            ),
        ] {
            assert!(
                matches!(parse_directory_page(&bad, 1), Err(ChatError::Shape(_))),
                "{why} must be refused"
            );
        }
    }
}
