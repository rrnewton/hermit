//! `search_text`: what the full-text index holds for one message.
//!
//! The page searches what a row SAYS: its author, the text it draws (emphasis removed, link
//! labels such as `widgets#123`), and the addresses its links go to. The index chooses the
//! candidates, so it has to contain every one of those; re-matching in Rust cannot recover a
//! message the index never offered. So the column is
//!
//! ```text
//! lowercase(author + "\n" + content + "\n" + rendered text + "\n" + link addresses, one a line)
//! ```
//!
//! with the rendered text left out when it is the content exactly: every substring of it is
//! already there.
//!
//! capped at [`MAX_BYTES`]. How a body is rendered belongs to `crate::render`, which this module
//! does not depend on: the rendering comes in through [`SearchTextSource`], so the store can be
//! built and tested before the renderer is wired to it, and a renderer change is a new
//! [`SearchTextSource::version`] that the writer rebuilds the column for, in the background.

use crate::model::Message;

/// The most bytes of `search_text` kept per message. Slack allows 40,000 characters in one post;
/// past this cap a search can miss the message, and says its matching was by substring only.
pub const MAX_BYTES: usize = 8 * 1024;

/// A body as the page would draw it: its visible text and the addresses of its links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// The text content of the drawn body: what a reader sees, with markup gone.
    pub text: String,
    /// Every link address in the drawn body, in order.
    pub hrefs: Vec<String>,
}

/// How a message body is rendered for the index.
pub trait SearchTextSource: Send + Sync {
    /// A label for the rendering rules. When it differs from the one a cache file was built
    /// with, the writer rebuilds every row's `search_text` with these rules.
    fn version(&self) -> String;

    /// Render one message from the provider `provider_key` the way the page draws it.
    fn rendered(&self, provider_key: &str, message: &Message) -> Rendered;
}

/// The rendering before `crate::render` is wired in: the body exactly as written, and no links.
/// Every word of the content is still indexed; what it lacks is link labels and addresses that
/// appear only in the drawn form.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainText;

impl SearchTextSource for PlainText {
    fn version(&self) -> String {
        "plain-1".to_owned()
    }

    fn rendered(&self, _provider_key: &str, message: &Message) -> Rendered {
        Rendered {
            text: message.content.clone(),
            hrefs: Vec::new(),
        }
    }
}

/// Build the `search_text` column for one message from its rendering.
#[must_use]
pub fn build(author: &str, content: &str, rendered: &Rendered) -> String {
    let mut text = String::with_capacity(author.len() + content.len() * 2 + 16);
    text.push_str(author);
    text.push('\n');
    text.push_str(content);
    // The drawn text is very often the content itself; once is enough for every substring of
    // it to match, and the index is the larger half of the file.
    if rendered.text != content {
        text.push('\n');
        text.push_str(&rendered.text);
    }
    for href in &rendered.hrefs {
        text.push('\n');
        text.push_str(href);
    }
    cap(text.to_lowercase())
}

/// Cut to at most [`MAX_BYTES`], at a character boundary.
fn cap(mut text: String) -> String {
    if text.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Whether a `search_text` was cut at [`MAX_BYTES`], so a search that relied on it can say its
/// matching past that point was not complete.
#[must_use]
pub fn was_capped(search_text: &str) -> bool {
    search_text.len() + 4 > MAX_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_the_page_searches_is_in_the_text_lowercased() {
        let rendered = Rendered {
            text: "Fixed widgets#123 Today".to_owned(),
            hrefs: vec!["https://Example.test/Issue/123".to_owned()],
        };
        let text = build("Ada Lovelace", "Fixed **[#123](x)** today", &rendered);
        for needle in [
            "ada lovelace",
            "**[#123](x)**",
            "widgets#123",
            "https://example.test/issue/123",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from {text:?}");
        }
        assert_eq!(text, text.to_lowercase());
    }

    #[test]
    fn the_text_is_capped_at_a_character_boundary() {
        let long = "é".repeat(MAX_BYTES);
        let text = build("a", &long, &Rendered::default());
        assert!(text.len() <= MAX_BYTES);
        assert!(text.len() > MAX_BYTES - 4);
        assert!(was_capped(&text));
        assert!(!was_capped(&build("a", "short", &Rendered::default())));
    }

    #[test]
    fn a_rendering_identical_to_the_content_is_not_indexed_twice() {
        let same = build(
            "a",
            "hello",
            &Rendered {
                text: "hello".to_owned(),
                hrefs: Vec::new(),
            },
        );
        assert_eq!(same, "a\nhello");
    }

    #[test]
    fn plain_text_renders_the_body_as_written() {
        let message = crate::cache::fixtures::message("1", "u", "Hello *there*", 1_000);
        let rendered = PlainText.rendered("p", &message);
        assert_eq!(rendered.text, "Hello *there*");
        assert!(rendered.hrefs.is_empty());
        assert_eq!(PlainText.version(), "plain-1");
    }
}
