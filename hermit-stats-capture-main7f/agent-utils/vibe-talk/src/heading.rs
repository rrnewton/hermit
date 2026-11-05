//! A message's heading: the who-am-i tag, the time and the short title an agent writes at the head
//! of its first line, lifted out so the page can show them in the row's header. `#242
//! rich-headers`.
//!
//! # Why
//!
//! Every agent in a channel posts through one shared bot account, so the account says nothing about
//! WHICH agent wrote a message. The owner asks his agents to say so themselves, at the head of the
//! message: `[build-bot coordinator] Backup check, 10:52 PM ET. All four shards are synced.`, or
//! `(ops2) Midday summary, 15:05Z.`. Read as a sentence, that is a bracketed name, a time and a
//! title run into the body. On a desk the row's header line is mostly empty space between the time
//! and the row's buttons, and those three pieces fit there.
//!
//! # The rules
//!
//! [`extract`] reads only the first line, at most [`MAX_LINE_BYTES`] of it, and answers `None`
//! whenever it is unsure: the row is then drawn exactly as it was before this module existed.
//!
//! 1. **A tag is required.** Without one nothing tells an agent's heading apart from a person's
//!    first sentence.
//!    - A leading `[tag]`: one to [`MAX_TAG_CHARS`] characters and one to [`MAX_TAG_WORDS`]
//!      words, no markup inside, followed by a space. Not a checkbox, a footnote, an alert or a
//!      number such as `[1/3]`; never a link, because `]` must be followed by a space.
//!    - A leading `(tag)`, or a trailing `Title (tag):` / `Title (tag, TIME).`: a single token. A
//!      parenthesis opens plenty of ordinary sentences, so a paren tag counts only once the same
//!      author has started at least two different messages in the channel with it
//!      ([`HeadingTags`]), and never when it is a word such as `edited`, `cont.` or `thanks`
//!      ([`STOP`]). The trailing form is taken only from a bot account: people end sentences with
//!      asides in parentheses far more often than they sign them, and every agent measured posts
//!      through a bot account.
//! 2. **The first sentence** runs to the first `:` `.` `?` `!` that is followed by a space or the
//!    end of the line and is outside parentheses. A clock's colon, `e.g.` and a decimal point do
//!    not end it. Emphasis around the whole sentence is unwrapped; any other markup in it — code,
//!    a link, a mention, an emoji shortcode such as `:tada:`, emphasis over part of it — leaves the
//!    heading the tag alone.
//! 3. **A time** is one clock expression, kept exactly as written, and only from three places:
//!    straight after the tag, at the end of the sentence, or in parentheses inside it. A time
//!    anywhere else means neither a time nor a title. Other parenthesised text stays in the title,
//!    times and all: `Tally, 11:45Z (plan filed 02:57Z; three merges).`.
//! 4. **The title** is what is left, with spacing and dangling punctuation tidied. It is dropped
//!    when it is longer than [`MAX_TITLE_CHARS`], and when a `.` `?` `!` in mid-line ends it with no
//!    time — `Yes. The run…` is a reply, not a title.
//!
//! # Nothing is moved and nothing is lost
//!
//! [`Extracted::cut`] is how much of the text the heading RESTATES: always a prefix of the first
//! line. The body is not cut. [`crate::render`] marks that prefix in the rendered body, and the page
//! hides it only while it shows the heading, so with rich headers off the row is the same, byte for
//! byte, as before.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::model::{ChannelId, MessageId, UserId};

/// How much of the first line is read, in bytes. A heading sits at the very start; a line longer
/// than this is read only this far, and a sentence that runs past it gives no title.
pub const MAX_LINE_BYTES: usize = 300;

/// The longest `[tag]`, in characters. The longest measured was 26.
pub const MAX_TAG_CHARS: usize = 40;

/// The most words a `[tag]` may have: a name and a role, with room to spare.
pub const MAX_TAG_WORDS: usize = 4;

/// The longest `(tag)`, in characters. A paren tag is one token.
pub const MAX_PAREN_TAG_CHARS: usize = 24;

/// The longest title, in characters. Past this it is a sentence, not a title.
pub const MAX_TITLE_CHARS: usize = 80;

/// How many paren tags [`HeadingTags`] keeps for one author in one channel.
pub const MAX_TAGS_PER_AUTHOR: usize = 32;

/// How many channel-and-author pairs [`HeadingTags`] keeps before it forgets the least recently
/// used.
pub const MAX_AUTHORS: usize = 256;

/// Words a parenthesis holds without naming anybody — a note about the message, or an aside such as
/// `(thanks)` or `(finally)` — whichever end of the sentence it is at. Compared without case.
pub const STOP: &[&str] = &[
    "again",
    "also",
    "aside",
    "btw",
    "bump",
    "cont",
    "cont.",
    "contd",
    "continued",
    "correction",
    "done",
    "draft",
    "edited",
    "finally",
    "fixed",
    "fyi",
    "haha",
    "hmm",
    "hopefully",
    "imho",
    "imo",
    "jk",
    "lol",
    "maybe",
    "nit",
    "no",
    "nope",
    "nvm",
    "oh",
    "ok",
    "okay",
    "oops",
    "optional",
    "p.s.",
    "phew",
    "ping",
    "please",
    "pls",
    "ps",
    "repost",
    "resend",
    "resent",
    "sic",
    "sigh",
    "sorry",
    "tbd",
    "thanks",
    "thx",
    "todo",
    "ty",
    "ugh",
    "unrelated",
    "update",
    "updated",
    "wip",
    "wow",
    "yay",
    "yep",
    "yes",
];

/// Where a heading's tag was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagStyle {
    /// `[tag] …`
    Bracket,
    /// `(tag) …`
    Paren,
    /// `Title (tag): …` or `Title (tag, TIME). …`
    Trailing,
}

/// A heading found at the head of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    /// Who wrote it, as the message names itself. Never empty.
    pub tag: String,
    /// Where the tag was written.
    pub style: TagStyle,
    /// The clock time the message states, exactly as written: `10:52 PM ET`, `15:05Z`.
    pub time: Option<String>,
    /// The title, with emphasis markers and the time taken out.
    pub title: Option<String>,
    /// How many BYTES at the start of the text the heading restates. Always on a character
    /// boundary and never past the end of the first line (its line ending excepted: a `\r` before
    /// the newline is counted when the heading uses the whole line).
    pub cut: usize,
}

impl Extracted {
    /// The heading as the page is sent it, with [`Extracted::cut`] counted in UTF-16 units, which
    /// is how the page indexes `content`.
    #[must_use]
    pub fn wire(&self, content: &str) -> crate::model::Heading {
        let units = content
            .get(..self.cut)
            .map_or(0, |prefix| prefix.encode_utf16().count());
        crate::model::Heading {
            tag: Some(self.tag.clone()),
            time: self.time.clone(),
            title: self.title.clone(),
            cut: u32::try_from(units).unwrap_or(u32::MAX),
        }
    }
}

/// The heading at the head of `content`, or `None` when there is none or it is unclear.
///
/// `bot` is whether the provider flagged the author as a bot: only then is a trailing tag looked
/// for. `confirmed` answers whether a paren tag has started enough of this author's messages in the
/// channel to count ([`HeadingTags::confirmed`]); a `[tag]` needs no confirmation.
#[must_use]
pub fn extract(content: &str, bot: bool, confirmed: &dyn Fn(&str) -> bool) -> Option<Extracted> {
    let line = first_line(content)?;
    let window = window(line);
    let (mut tag, mut style, pos) = if window.starts_with('[') {
        let (tag, pos) = bracket_tag(window)?;
        (Some(tag), TagStyle::Bracket, pos)
    } else if window.starts_with('(') {
        let (tag, pos) = paren_tag(window)?;
        if stopped(tag) || !confirmed(tag) {
            return None;
        }
        (Some(tag), TagStyle::Paren, pos)
    } else {
        (None, TagStyle::Trailing, 0)
    };
    let sentence = sentence(window, pos);
    let mut time = None;
    let mut title = None;
    let mut cut = pos;
    if let Some(found) = &sentence {
        if !has_markup(found.text) && !has_shortcode(found.text) {
            let mut text = found.text;
            if tag.is_none() && bot {
                if let Some((start, token, clock)) = trailing_tag(text) {
                    if !stopped(token) && confirmed(token) {
                        tag = Some(token);
                        style = TagStyle::Trailing;
                        time = clock.map(str::to_owned);
                        text = &text[..start];
                    }
                }
            }
            let mut text = text.to_owned();
            if time.is_none() {
                if let Some((clock, rest)) = take_time(&text) {
                    time = Some(clock);
                    text = rest;
                }
            }
            let tidy = tidy(&text);
            let conversational = matches!(found.terminator, Some('.' | '?' | '!'))
                && !window[found.after..].trim().is_empty()
                && time.is_none()
                && style != TagStyle::Trailing;
            // A sentence that runs past what was read was cut there, mid-word perhaps.
            let unfinished = found.terminator.is_none() && window.len() < line.len();
            if find_clock(&without_parentheses(&tidy)).is_some()
                || tidy.chars().count() > MAX_TITLE_CHARS
                || unfinished
            {
                // A time left in the sentence, or a sentence too long to be a title: the tag alone.
                time = None;
            } else {
                if !conversational && !tidy.is_empty() {
                    title = Some(tidy);
                }
                if title.is_some() || time.is_some() {
                    cut = found.after;
                }
            }
        }
    }
    let tag = tag?;
    // A trailing tag is part of the sentence it ends, so it is only a heading together with it.
    if style == TagStyle::Trailing && cut == 0 {
        return None;
    }
    Some(Extracted {
        tag: tag.to_owned(),
        style,
        time,
        title,
        cut: extend_cut(content, cut),
    })
}

/// The paren tag `content` would be headed by if it were confirmed, for [`HeadingTags::record`]:
/// a leading `(tag)`, or, from a `bot`, a trailing one before the end of the first sentence.
#[must_use]
pub fn paren_candidate(content: &str, bot: bool) -> Option<&str> {
    let line = first_line(content)?;
    let window = window(line);
    if window.starts_with('[') {
        return None;
    }
    let token = if window.starts_with('(') {
        paren_tag(window)?.0
    } else if bot {
        let (end, _) = terminator(window, 0, None);
        trailing_tag(&window[..end])?.1
    } else {
        return None;
    };
    (!stopped(token)).then_some(token)
}

/// The first line of `content`, without its line ending, or `None` when it is blank or indented:
/// a heading starts at the very first character.
fn first_line(content: &str) -> Option<&str> {
    let end = content.find(['\n', '\r']).unwrap_or(content.len());
    let line = &content[..end];
    let first = line.chars().next()?;
    (!first.is_whitespace()).then_some(line)
}

/// At most [`MAX_LINE_BYTES`] of `line`, cut on a character boundary.
fn window(line: &str) -> &str {
    if line.len() <= MAX_LINE_BYTES {
        return line;
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Whether `token` is one of the [`STOP`] words.
fn stopped(token: &str) -> bool {
    STOP.iter().any(|word| word.eq_ignore_ascii_case(token))
}

/// A leading `[tag]` followed by a space: the tag and where the text after its spaces starts.
fn bracket_tag(line: &str) -> Option<(&str, usize)> {
    let close = line.find(']')?;
    let tag = &line[1..close];
    let after = &line[close + 1..];
    if !after.starts_with([' ', '\t']) {
        return None;
    }
    let characters = tag.chars().count();
    let words = tag.split_whitespace().count();
    let numeric = tag
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || "/.,:#%-".contains(c));
    if !(1..=MAX_TAG_CHARS).contains(&characters)
        || !(1..=MAX_TAG_WORDS).contains(&words)
        || tag.trim() != tag
        || tag.contains('[')
        || has_markup(tag)
        || matches!(tag, "x" | "X")
        || tag.starts_with(['^', '!'])
        || numeric
    {
        return None;
    }
    Some((tag, skip_blanks(line, close + 1)))
}

/// A leading `(tag)` followed by a space: the token and where the text after its spaces starts.
fn paren_tag(line: &str) -> Option<(&str, usize)> {
    let close = line.find(')')?;
    let token = &line[1..close];
    if !is_token(token) || !line[close + 1..].starts_with([' ', '\t']) {
        return None;
    }
    Some((token, skip_blanks(line, close + 1)))
}

/// Whether `text` is one paren-tag token: a letter, then letters, digits, `_`, `.` or `-`.
fn is_token(text: &str) -> bool {
    let mut characters = text.chars();
    characters.next().is_some_and(|c| c.is_ascii_alphabetic())
        && text.len() <= MAX_PAREN_TAG_CHARS
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// `at`, moved past any spaces and tabs in `line`.
fn skip_blanks(line: &str, at: usize) -> usize {
    at + (line[at..].len() - line[at..].trim_start_matches([' ', '\t']).len())
}

/// Whether `text` holds anything Markdown or the chat dialects would draw as other than its own
/// characters: emphasis, code, a link, a mention, an escape, an entity, a bare address.
fn has_markup(text: &str) -> bool {
    text.contains(['`', '<', '>', '*', '_', '~', '|', '[', ']', '\\'])
        || text.contains("://")
        || text.contains("www.")
        || has_entity(text)
}

/// Whether `text` holds what may be an emoji shortcode such as `:tada:` or `:+1:`: a colon run
/// straight into a letter or a `+`. A provider draws one as a picture, and the sentence rule would
/// end the sentence at its closing colon and put half of it in the title. A clock's colon is
/// followed by a digit, and a colon that ends a sentence by a space.
fn has_shortcode(text: &str) -> bool {
    text.match_indices(':')
        .any(|(at, _)| text[at + 1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '+'))
}

/// Whether `text` holds an HTML character reference, `&amp;` or `&#38;`, which is drawn as the
/// character it names.
fn has_entity(text: &str) -> bool {
    text.match_indices('&').any(|(at, _)| {
        let rest = &text[at + 1..];
        let name = rest.trim_start_matches('#');
        let length = name
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(name.len());
        length > 0 && name[length..].starts_with(';')
    })
}

/// The first sentence of a line, after its tag.
struct Sentence<'a> {
    /// The sentence, without its terminator or the emphasis around it.
    text: &'a str,
    /// The terminator that ended it, or `None` when it ran to the end of what was read.
    terminator: Option<char>,
    /// Where the line goes on after the sentence, its terminator and closing emphasis.
    after: usize,
}

/// The first sentence of `line` from `start`, or `None` when emphasis opens it and closes in the
/// middle of it.
fn sentence(line: &str, start: usize) -> Option<Sentence<'_>> {
    let rest = &line[start..];
    let wrap = ["**", "__", "*", "_"].into_iter().find(|marker| {
        rest.strip_prefix(marker)
            .is_some_and(|inner| !inner.starts_with([' ', '\t', '*', '_']) && !inner.is_empty())
    });
    let from = start + wrap.map_or(0, str::len);
    let (end, terminator) = terminator(line, from, wrap);
    let mut text = &line[from..end];
    let mut after = end + terminator.map_or(0, char::len_utf8);
    if let Some(marker) = wrap {
        if let Some(inner) = text.strip_suffix(marker) {
            text = inner;
        } else if line[after..].starts_with(marker) {
            after += marker.len();
        } else {
            return None;
        }
    }
    Some(Sentence {
        text,
        terminator,
        after,
    })
}

/// Where the sentence in `line` from `start` ends: the byte index of its terminator and the
/// terminator, or the end of the line and `None`.
fn terminator(line: &str, start: usize, wrap: Option<&str>) -> (usize, Option<char>) {
    let mut depth = 0_usize;
    let mut at = start;
    while at < line.len() {
        // A clock is read whole, so neither its colon nor the dots of `a.m.` end the sentence.
        if let Some(&end) = clock_ends(line, at).first() {
            at = end;
            continue;
        }
        let Some(c) = line[at..].chars().next() else {
            break;
        };
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ':' | '.' | '?' | '!' if depth == 0 => {
                let next = line[at + 1..].chars().next();
                let closes = |n: char| wrap.is_some_and(|marker| marker.starts_with(n));
                let ends = match next {
                    None => true,
                    Some(n) => n == ' ' || n == '\t' || closes(n),
                };
                if ends && !abbreviation(&line[start..at], c) {
                    return (at, Some(c));
                }
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    (line.len(), None)
}

/// Whether the terminator `c` after `before` belongs to an abbreviation or an address instead of
/// ending the sentence.
fn abbreviation(before: &str, c: char) -> bool {
    let words: &[&str] = match c {
        '.' => &[
            "e.g", "i.e", "vs", "etc", "approx", "no", "St", "Dr", "Mr", "Ms",
        ],
        ':' => &["http", "https"],
        _ => return false,
    };
    words.iter().any(|word| {
        before.strip_suffix(word).is_some_and(|head| {
            head.chars()
                .next_back()
                .is_none_or(|p| !(p.is_alphanumeric() || p == '_'))
        })
    })
}

/// A trailing `(tag)` or `(tag, TIME)` at the end of `text`: where it starts (with the spaces
/// before it), the token, and the time.
fn trailing_tag(text: &str) -> Option<(usize, &str, Option<&str>)> {
    let inner_end = text.strip_suffix(')')?.len();
    let open = text[..inner_end].rfind('(')?;
    let inner = &text[open + 1..inner_end];
    let (token, clock) = match inner.split_once(',') {
        Some((token, rest)) => {
            let rest = rest.trim_start_matches([' ', '\t']);
            if !clock_ends(rest, 0).contains(&rest.len()) {
                return None;
            }
            (token, Some(rest))
        }
        None => (inner, None),
    };
    if !is_token(token) {
        return None;
    }
    let start = text[..open].trim_end_matches([' ', '\t']).len();
    Some((start, token, clock))
}

/// The time in one of its three places in `text`, and `text` without it.
fn take_time(text: &str) -> Option<(String, String)> {
    if let Some((start, clock)) = time_at_end(text) {
        return Some((text[clock.clone()].to_owned(), text[..start].to_owned()));
    }
    if let Some((clock, end)) = time_at_start(text) {
        return Some((text[clock].to_owned(), text[end..].to_owned()));
    }
    if let Some((start, clock, end)) = time_in_parentheses(text) {
        return Some((
            text[clock].to_owned(),
            format!("{}{}", &text[..start], &text[end..]),
        ));
    }
    let (start, clock, end) = time_before_aside(text)?;
    Some((
        text[clock].to_owned(),
        format!("{} {}", &text[..start], &text[end..]),
    ))
}

/// Whether `c` separates a closing time from the title before it.
fn separates(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '(' | '@')
}

/// A time that ends the sentence — `Backup check, 10:52 PM ET`, `Report (11:58 AM ET)`, `Status at
/// 3 PM` — or is the whole of it: where the title ends, and the time.
fn time_at_end(text: &str) -> Option<(usize, std::ops::Range<usize>)> {
    let trimmed = text.trim_end();
    let bodies = [(trimmed.strip_suffix(')'), true), (Some(trimmed), false)];
    for (body, closed) in bodies {
        let Some(body) = body else {
            continue;
        };
        let Some(start) = char_starts(body).find(|&at| clock_ends(body, at).contains(&body.len()))
        else {
            continue;
        };
        let mut lead = start;
        if body[..lead].ends_with('(') {
            lead -= 1;
        } else if closed {
            // `Status (see 3 PM)`: the parenthesis holds more than the time, and stays whole.
            continue;
        }
        let before = |at: usize| body[..at].chars().next_back();
        let joined = |at: usize| at == 0 || before(at).is_some_and(separates);
        // `at` and the spaces after it, when a separator or the start comes before the `at`.
        let spaced = body[..lead].trim_end();
        let at_word = (spaced.len() < lead)
            .then(|| spaced.strip_suffix("at"))
            .flatten()
            .map(str::len)
            .filter(|&at| joined(at));
        let mut from = match at_word {
            Some(at) => at,
            None if joined(lead) => lead,
            None => continue,
        };
        while let Some(c) = before(from).filter(|&c| separates(c)) {
            from -= c.len_utf8();
        }
        return Some((from, start..body.len()));
    }
    None
}

/// A time straight after the tag, followed by a comma, a dash or nothing: `3:20 PM ET` or `8:45
/// PM ET, Topic`. The time, and where the title starts.
fn time_at_start(text: &str) -> Option<(std::ops::Range<usize>, usize)> {
    let open = usize::from(text.starts_with('('));
    let start = skip_spaces(text, open);
    for end in clock_ends(text, start) {
        let mut at = skip_spaces(text, end);
        if open == 1 && text[at..].starts_with(')') {
            at = skip_spaces(text, at + 1);
        }
        let rest = &text[at..];
        if rest.is_empty() {
            return Some((start..end, at));
        }
        if let Some(c) = rest
            .chars()
            .next()
            .filter(|&c| matches!(c, ',' | '-' | '–' | '—'))
        {
            return Some((start..end, skip_spaces(text, at + c.len_utf8())));
        }
    }
    None
}

/// `at`, moved past any whitespace in `text`.
fn skip_spaces(text: &str, at: usize) -> usize {
    at + (text[at..].len() - text[at..].trim_start().len())
}

/// A time in parentheses inside the sentence: `Report (11:58 AM ET) for the shards`. Where the
/// parenthesis starts (with the spaces before it), the time, and where it ends.
fn time_in_parentheses(text: &str) -> Option<(usize, std::ops::Range<usize>, usize)> {
    text.match_indices('(').find_map(|(open, _)| {
        let start = open + 1;
        clock_ends(text, start)
            .into_iter()
            .find(|&end| text[end..].starts_with(')'))
            .map(|end| {
                let lead = text[..open].trim_end().len();
                (lead, start..end, end + 1)
            })
    })
}

/// A time after a comma and before an aside in parentheses: `Tally, 11:45Z (plan filed …)`.
/// Where the comma is, the time, and where the aside starts.
fn time_before_aside(text: &str) -> Option<(usize, std::ops::Range<usize>, usize)> {
    text.match_indices(',').find_map(|(comma, _)| {
        let start = skip_spaces(text, comma + 1);
        clock_ends(text, start).into_iter().find_map(|end| {
            let aside = skip_spaces(text, end);
            text[aside..]
                .starts_with('(')
                .then_some((comma, start..end, aside))
        })
    })
}

/// `text` with its spacing collapsed and loose punctuation trimmed from both ends.
fn tidy(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches([' ', ',', ';', '-', '–', '—', '@'])
        .to_owned()
}

/// `text` without the innermost parenthesised asides, which may hold times of their own.
fn without_parentheses(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        let after = &rest[open + 1..];
        match after.find([')', '(']) {
            Some(close) if after[close..].starts_with(')') => {
                out.push_str(&rest[..open]);
                rest = &after[close + 1..];
            }
            Some(close) => {
                out.push_str(&rest[..open + 1 + close]);
                rest = &after[close..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Every character boundary in `text`.
fn char_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.char_indices().map(|(at, _)| at)
}

/// The first clock expression in `text`: where it starts and ends.
fn find_clock(text: &str) -> Option<(usize, usize)> {
    char_starts(text).find_map(|at| clock_ends(text, at).first().map(|&end| (at, end)))
}

/// The zones a clock may name after it. A zone glued to the digits is written `15:05Z`.
const ZONES: &[&str] = &[
    "UTC", "GMT", "EST", "EDT", "CST", "CDT", "MST", "MDT", "PST", "PDT", "BST", "CET", "CEST",
    "ET", "CT", "MT", "PT", "Z",
];

/// Where a clock expression starting at `at` in `text` may end, most preferred first; empty when
/// none starts there.
///
/// A 12-hour time (`10:52 PM`, `8pm`, `11:58 a.m.`) or a 24-hour one (`15:05`), either with an
/// optional zone, and neither run into a word or a longer number on either side: `v1.10:30`,
/// `10:52:33` and `13:58x` are not clocks.
fn clock_ends(text: &str, at: usize) -> Vec<usize> {
    let bytes = text.as_bytes();
    let before = text[..at].chars().next_back();
    if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.')) {
        return Vec::new();
    }
    let digit = |i: usize, range: std::ops::RangeInclusive<u8>| {
        bytes.get(i).is_some_and(|byte| range.contains(byte))
    };
    let mut ends = Vec::new();
    // 12-hour: an hour of 1 to 12, optional minutes, an optional space, AM or PM.
    let mut hours = Vec::new();
    if digit(at, b'1'..=b'1') && digit(at + 1, b'0'..=b'2') {
        hours.push(at + 2);
    }
    if digit(at, b'0'..=b'0') && digit(at + 1, b'1'..=b'9') {
        hours.push(at + 2);
    }
    if digit(at, b'1'..=b'9') {
        hours.push(at + 1);
    }
    for hour in hours {
        let mut minutes = Vec::new();
        if bytes.get(hour) == Some(&b':')
            && digit(hour + 1, b'0'..=b'5')
            && digit(hour + 2, b'0'..=b'9')
        {
            minutes.push(hour + 3);
        }
        minutes.push(hour);
        for minute in minutes {
            for spaced in optional_space(bytes, minute) {
                for meridiem in meridiem(bytes, spaced) {
                    zoned(text, meridiem, &mut ends);
                }
            }
        }
    }
    // 24-hour: HH:MM or H:MM.
    let mut hours = Vec::new();
    if digit(at, b'0'..=b'1') && digit(at + 1, b'0'..=b'9') {
        hours.push(at + 2);
    }
    if digit(at, b'0'..=b'9') {
        hours.push(at + 1);
    }
    if digit(at, b'2'..=b'2') && digit(at + 1, b'0'..=b'3') {
        hours.push(at + 2);
    }
    for hour in hours {
        if bytes.get(hour) == Some(&b':')
            && digit(hour + 1, b'0'..=b'5')
            && digit(hour + 2, b'0'..=b'9')
        {
            zoned(text, hour + 3, &mut ends);
        }
    }
    ends
}

/// `at`, past one space or tab when there is one, then not.
fn optional_space(bytes: &[u8], at: usize) -> Vec<usize> {
    match bytes.get(at) {
        Some(b' ' | b'\t') => vec![at + 1, at],
        _ => vec![at],
    }
}

/// Where `AM`, `pm`, `a.m.` and the like starting at `at` may end, longest first.
///
/// A dot after the `M` belongs to the clock only in the dotted form: in `8 PM.` it ends the
/// sentence, and in `8 a.m.` it is the abbreviation's own.
fn meridiem(bytes: &[u8], at: usize) -> Vec<usize> {
    let mut ends = Vec::new();
    if !matches!(bytes.get(at), Some(b'A' | b'a' | b'P' | b'p')) {
        return ends;
    }
    if bytes.get(at + 1) == Some(&b'.') && matches!(bytes.get(at + 2), Some(b'M' | b'm')) {
        if bytes.get(at + 3) == Some(&b'.') {
            ends.push(at + 4);
        }
        ends.push(at + 3);
    }
    if matches!(bytes.get(at + 1), Some(b'M' | b'm')) {
        ends.push(at + 2);
    }
    ends
}

/// Push the ends of a clock that ends at `at`: with a zone after it, then without, each only when
/// nothing runs on from it.
fn zoned(text: &str, at: usize, ends: &mut Vec<usize>) {
    for spaced in optional_space(text.as_bytes(), at) {
        for zone in ZONES {
            if text[spaced..].starts_with(zone) {
                let end = spaced + zone.len();
                if stands_alone(text, end) {
                    ends.push(end);
                }
            }
        }
    }
    if stands_alone(text, at) {
        ends.push(at);
    }
}

/// Whether nothing runs on from `at`: no letter or digit, and no `:` before a digit.
fn stands_alone(text: &str, at: usize) -> bool {
    let rest = &text[at..];
    let mut characters = rest.chars();
    match characters.next() {
        None => true,
        Some(':') => !characters.next().is_some_and(|c| c.is_ascii_digit()),
        Some(c) => !(c.is_alphanumeric() || c == '_'),
    }
}

/// `cut`, moved past the spaces after it, and past the line ending's `\r` when that leaves nothing
/// of the line: so a heading that uses the whole line hides the whole line.
fn extend_cut(content: &str, cut: usize) -> usize {
    let at = skip_blanks(content, cut);
    if content[at..].starts_with('\r') {
        at + 1
    } else {
        at
    }
}

/// The paren tags each author has started messages with, per channel: what confirms a `(tag)`.
/// `#242 rich-headers`.
///
/// A `(tag)` is only a heading once the same author has opened two different messages in the
/// channel with it, because one parenthesis at the start of a message is just as likely to be an
/// aside. The page's reads record every message they serve here before any of them is rendered,
/// so a window of messages confirms its own tags; the live stream records each new message as it
/// passes, and the pins route records the pins it serves.
///
/// IT ONLY EVER HEARS OF MESSAGES, never of their removal. A tag stays confirmed after the
/// messages that confirmed it are edited or deleted, until it is forgotten by the bound or a
/// restart. That is looser than "starts two messages the server holds", and it is accepted: the
/// author did open two messages with it, and all it can cost is a heading on a later message that
/// opens the same way, whose words are restated, never lost. An edit cannot confirm a tag on its
/// own, because the record counts message ids, not texts.
///
/// IN MEMORY AND BOUNDED: at most [`MAX_TAGS_PER_AUTHOR`] tags for each of at most
/// [`MAX_AUTHORS`] channel-and-author pairs, the least recently used forgotten first. Under a
/// megabyte at worst, and nothing in it outlives the process: after a restart a paren-tagged live
/// message is drawn as it always was until the next read of its channel confirms the tag again.
#[derive(Debug, Default)]
pub struct HeadingTags {
    inner: Mutex<Seen>,
}

/// What [`HeadingTags`] holds.
#[derive(Debug, Default)]
struct Seen {
    /// Tags by channel and author, each with when it was last used.
    authors: HashMap<(String, String), (u64, Vec<SeenTag>)>,
    /// A counter standing in for the time, for least-recently-used.
    clock: u64,
}

/// One paren tag one author has used.
#[derive(Debug)]
struct SeenTag {
    tag: String,
    /// The first message seen with it.
    first: String,
    /// Whether a second, different message has been seen with it.
    confirmed: bool,
}

impl HeadingTags {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `author` started `message` in `channel` with the paren tag `tag`.
    pub fn record(&self, channel: &ChannelId, author: &UserId, message: &MessageId, tag: &str) {
        let Ok(mut seen) = self.inner.lock() else {
            return;
        };
        seen.clock += 1;
        let now = seen.clock;
        let key = (channel.0.clone(), author.0.clone());
        if !seen.authors.contains_key(&key) && seen.authors.len() >= MAX_AUTHORS {
            let oldest = seen
                .authors
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                seen.authors.remove(&oldest);
            }
        }
        let (used, tags) = seen.authors.entry(key).or_default();
        *used = now;
        if let Some(at) = tags.iter().position(|seen| seen.tag == tag) {
            let mut held = tags.remove(at);
            held.confirmed |= held.first != message.0;
            tags.push(held);
        } else {
            if tags.len() >= MAX_TAGS_PER_AUTHOR {
                tags.remove(0);
            }
            tags.push(SeenTag {
                tag: tag.to_owned(),
                first: message.0.clone(),
                confirmed: false,
            });
        }
    }

    /// Whether `author` has started two different messages in `channel` with `tag`.
    #[must_use]
    pub fn confirmed(&self, channel: &ChannelId, author: &UserId, tag: &str) -> bool {
        let Ok(seen) = self.inner.lock() else {
            return false;
        };
        seen.authors
            .get(&(channel.0.clone(), author.0.clone()))
            .is_some_and(|(_, tags)| tags.iter().any(|seen| seen.tag == tag && seen.confirmed))
    }

    /// How many channel-and-author pairs are held. For the bound's test.
    #[must_use]
    pub fn authors(&self) -> usize {
        self.inner.lock().map_or(0, |seen| seen.authors.len())
    }
}

#[cfg(test)]
mod tests;
