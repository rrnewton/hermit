//! Turning one channel message into something worth listening to.
//!
//! # Why this exists
//!
//! Read-aloud sent the message's raw text to the vendor, so the voice said "asterisk asterisk
//! deploy asterisk asterisk", spelled out nineteen-digit snowflakes, read ISO timestamps character
//! by character, and began every address with "h t t p s colon slash slash". All of it is noise
//! that a listener cannot use and cannot skip.
//!
//! # Why it is code and not a prompt
//!
//! Every one of these transformations is deterministic, and asking a model to do them would cost a
//! round trip and tokens per message to get a *less* reliable answer. Nothing here calls anything.
//!
//! # The three jobs, in order
//!
//! 1. **Markdown comes off.** Discord's flavour: fences, inline code, bold, italic, underline,
//!    strikethrough, spoilers, headers, quotes, links, mentions and custom emoji. A link reads as
//!    its words, and an address with none is said as what it is (`#187 speakable-ids-urls`): a
//!    page with a short label as that page, "widgets pull request 41"; anything else by its host,
//!    "a link on example.com". Never "https", and never "colon slash slash".
//! 2. **Times become relative.** A listener wants "three hours ago", not "2026-08-24T04:31:00Z".
//! 3. **Opaque strings become useful short names.** A hash is not information when spoken; it is
//!    forty seconds of alphabet, so it becomes "hash code A". A long numeric identifier becomes
//!    "UID ending in 009", while a diff `D1234567890` becomes "diff ending in 890" and a task
//!    `T1234567` "task ending in 567".
//!
//! Only what is SAID changes: the text a reader sees and the text Copy copies are the message as
//! written.
//!
//! Order matters and is not arbitrary. Addresses are said BEFORE emphasis comes off, which would
//! otherwise take the underscores out of the middle of one, and before times and opaque strings,
//! which would otherwise find a date or a commit inside one. Times are resolved BEFORE opaque
//! strings, because a Unix epoch and a snowflake are both long runs of digits and only one of them
//! is a time.
//!
//! Every pass is linear in the length of the message (`#201 speech-prep-linear`): a stranger's
//! text must not cost the square of its length before a word of it is spoken.

use std::collections::BTreeMap;

use jiff::{Timestamp, Zoned};

use crate::clock::Zone;

/// Hex runs at least this long are hashes rather than words.
///
/// Twelve, not eight: a short git prefix is often quoted as seven or eight characters and is
/// occasionally still readable, while `deadbeef` is a word people actually write. Twelve is past
/// anything anybody says out loud on purpose.
const HASH_MIN_CHARS: usize = 12;

/// Digit runs at least this long are identifiers rather than quantities.
///
/// Ten covers a Unix epoch and a Discord snowflake. It deliberately does NOT cover a year, a port,
/// a build number or a count of anything — those are quantities a listener wants to hear.
pub(crate) const BIG_NUMBER_MIN_DIGITS: usize = 10;

/// Digits after the `D` of a diff or the `T` of a task before it is said by its ending.
///
/// Six: a reference that short is one somebody can say, and `D42` or `T3` in prose may be
/// anything at all. The ids a tracker of any size hands out are seven digits and more.
const TRACKER_ID_MIN_DIGITS: usize = 6;

/// Past this, a message is old enough that a relative reading stops helping.
const RELATIVE_LIMIT_DAYS: i64 = 365;

/// A fenced block this short is worth hearing; longer is worth NAMING.
///
/// Three lines is about a command and its output, or a one-line diff with context — the kind of
/// snippet whose whole point is the two words inside it. Past that, reading it aloud is a minute of
/// punctuation, and what the listener actually wants to know is that it is there and how big.
const CODE_SPOKEN_MAX_LINES: usize = 3;

/// How many first-column keys a table announcement will read out before it stops.
///
/// The keys are the useful part of a table when you cannot see it — they say what the table is
/// ABOUT. All of them would be the same wall of text the placeholder exists to remove.
const TABLE_KEYS_SPOKEN: usize = 4;

/// How many distinct hashes will be given a letter before the table stops growing.
///
/// This is a memory bound on a table that otherwise lives as long as the server, not a claim about
/// how many letters are useful — that number is far smaller. Past the cap an unseen value is
/// spoken as its kind with no letter at all, which is audibly different from a named one: the
/// listener hears "a hash code" and knows this one is not being tracked, instead of hearing a
/// letter that has quietly stopped meaning one value.
const NAMES_MAX: usize = 4_096;

/// Every hash letter handed out so far, and the value it stands for.
///
/// Held by the CALLER, because the useful lifetime of a letter is not the lifetime of a call. A
/// table built inside `for_speech` can only make "the same one" audible within a single message:
/// hear "hash code A" in one message and "hash code A" in the next and they are the same value
/// only by coincidence of ordering. Carried across the messages of one session, the letter becomes
/// a name the listener can actually use — which is the whole point of naming it.
///
/// **How wide the table may be shared is a property of the deployment, not of this type.** A letter
/// is only meaningful beside the transcript that introduced it, and it must never let one reader
/// hear an id they could not otherwise see. On this server both hold for a table shared by every
/// reader — there is one channel allowlist and every reader reads through it, so a named value is
/// always one already on their screen. A deployment where two readers see different channels would
/// have to give them different tables.
#[derive(Debug, Default, Clone)]
pub struct Names {
    hashes: BTreeMap<String, String>,
}

impl Names {
    /// An empty table, at the start of a session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct opaque strings have been named.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Whether nothing has been named yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One [`Names`] table that every handler on a server names into.
///
/// The lock is held only across the rewrite of a single message body, which touches no I/O, so it
/// is a `std::sync::Mutex` rather than an async one and is never held across an await. A poisoned
/// lock degrades to naming in a throwaway table: the reader loses letter continuity for that one
/// message and still hears no digits, which is strictly better than the alternative of a handler
/// that panics because a different handler did.
#[derive(Debug, Default)]
pub struct SharedNames(std::sync::Mutex<Names>);

impl SharedNames {
    /// An empty shared table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// [`for_speech`], naming into the shared table.
    #[must_use]
    pub fn for_speech(&self, content: &str, now_ms: i64, zone: &Zone) -> String {
        match self.0.lock() {
            Ok(mut names) => for_speech_with(content, now_ms, zone, &mut names),
            Err(_) => for_speech(content, now_ms, zone),
        }
    }

    /// How many distinct opaque strings have been named, or zero if the lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().map(|names| names.len()).unwrap_or_default()
    }

    /// Whether nothing has been named yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Rewrite a message so it can be read aloud, naming opaque strings in a fresh table.
///
/// `now_ms` is the instant to measure "ago" against, passed in rather than read from the clock so
/// the behaviour is testable without waiting for time to pass. `zone` is the operator's, and is
/// used only for the ABSOLUTE fallback — a relative reading is the same in every zone.
///
/// Use [`for_speech_with`] wherever the letters should outlive the one message.
#[must_use]
pub fn for_speech(content: &str, now_ms: i64, zone: &Zone) -> String {
    for_speech_with(content, now_ms, zone, &mut Names::new())
}

/// [`for_speech`], but naming into a table the caller keeps.
#[must_use]
pub fn for_speech_with(content: &str, now_ms: i64, zone: &Zone, names: &mut Names) -> String {
    let text = strip_markdown(content);
    let text = speak_times(&text, now_ms, zone);
    let text = name_opaque_strings(&text, names);
    collapse_whitespace(&text)
}

// --- markdown -----------------------------------------------------------------------------------

/// Take Discord's markdown off, keeping the words inside it.
fn strip_markdown(content: &str) -> String {
    let without_fences = drop_fenced_blocks(content);
    let without_tables = describe_tables(&without_fences);
    let mut out = String::with_capacity(without_tables.len());
    for (index, line) in without_tables.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&strip_line(line));
    }
    out
}

/// Replace a pipe table with a sentence about it.
///
/// A table of results is excellent to LOOK at and unbearable to hear: every cell arrives with no
/// structure around it, and by the third row the listener has lost the header. So it is announced —
/// its shape, and the first column's keys when those look like words somebody would recognise,
/// because the keys are what the table is ABOUT.
///
/// The keys are only offered when they read as English. A first column of hashes or numbers says
/// nothing when spoken, and a placeholder that recites them is the wall of text this removes.
fn describe_tables(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let run = table_run_at(&lines, i);
        match run {
            Some(end) => {
                out.push(say_table(&lines[i..end]));
                i = end;
            }
            None => {
                out.push(lines[i].to_owned());
                i += 1;
            }
        }
    }
    out.join("\n")
}

/// How far a table starting at `start` runs, or `None` if there is not one there.
///
/// Two rows minimum, and every row has to carry the same pipe shape. One line with a pipe in it is
/// a sentence with a pipe in it.
fn table_run_at(lines: &[&str], start: usize) -> Option<usize> {
    let mut end = start;
    while end < lines.len() && is_table_row(lines[end]) {
        end += 1;
    }
    if end - start >= 2 {
        Some(end)
    } else {
        None
    }
}

fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.matches('|').count() >= 2
}

fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_owned())
        .collect()
}

/// Is this row the `|---|---|` separator rather than data?
fn is_table_rule(line: &str) -> bool {
    table_cells(line)
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
}

fn say_table(rows: &[&str]) -> String {
    let data: Vec<&str> = rows.iter().copied().filter(|r| !is_table_rule(r)).collect();
    let columns = data.first().map_or(0, |r| table_cells(r).len());
    // The header is not a row of data, when there was a rule under it saying so.
    let had_header = rows.iter().any(|r| is_table_rule(r));
    let body: &[&str] = if had_header && !data.is_empty() {
        &data[1..]
    } else {
        &data
    };
    let row_count = body.len();
    let col_plural = if columns == 1 { "" } else { "s" };
    let row_plural = if row_count == 1 { "" } else { "s" };
    let shape =
        format!("a table with {columns} column{col_plural} and {row_count} row{row_plural}");

    let keys: Vec<String> = body
        .iter()
        .filter_map(|r| table_cells(r).into_iter().next())
        .filter(|k| looks_like_a_word(k))
        .take(TABLE_KEYS_SPOKEN)
        .collect();
    if keys.len() < 2 || keys.len() < row_count.min(2) {
        return format!(" {shape}. ");
    }
    let more = if row_count > keys.len() {
        ", and others"
    } else {
        ""
    };
    format!(" {shape}, with keys {}{more}. ", keys.join(", "))
}

/// Does this cell read as something a person would recognise, rather than as an identifier?
fn looks_like_a_word(cell: &str) -> bool {
    let stripped: String = cell
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_' | '~'))
        .collect();
    let stripped = stripped.trim();
    !stripped.is_empty()
        && stripped.chars().any(|c| c.is_ascii_alphabetic())
        && !is_hashlike(stripped)
        && !is_big_number(stripped)
        && stripped.chars().filter(char::is_ascii_alphabetic).count() * 2 >= stripped.len()
}

/// Replace fenced blocks with a description, or keep the short ones.
///
/// A SHORT block is usually the point of the message — a command, an error line — and silently
/// dropping it would remove the thing the listener was told to look at. A long one is a minute of
/// spoken punctuation, so it is announced instead: its language, if the fence declared one, and how
/// long it is, so the listener knows what they are choosing not to hear.
fn drop_fenced_blocks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => {
                out.push_str(&say_code(&after[..end]));
                rest = &after[end + 3..];
            }
            // Unterminated: the rest is code, and reading it aloud is what this prevents.
            None => {
                out.push_str(&say_code(after));
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn say_code(block: &str) -> String {
    let mut lines = block.lines();
    // A fence's first line may be a language tag rather than code: ```rust
    let first = lines.next().unwrap_or_default().trim();
    let language = if !first.is_empty()
        && first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '#')
    {
        Some(first.to_owned())
    } else {
        None
    };
    let body: Vec<&str> = if language.is_some() {
        lines.collect()
    } else {
        block.lines().collect()
    };
    let count = body.iter().filter(|l| !l.trim().is_empty()).count();
    if count <= CODE_SPOKEN_MAX_LINES {
        // Short enough to be the message rather than an attachment to it. Spoken as words, with
        // the blank lines a fence always leaves behind dropped — joining them produces a spoken
        // full stop with nothing in front of it.
        let said: Vec<&str> = body
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        return format!(" {} ", said.join(". "));
    }
    let plural = if count == 1 { "" } else { "s" };
    match language {
        Some(name) => format!(" a {name} code snippet, {count} line{plural} long. "),
        None => format!(" a code snippet, {count} line{plural} long. "),
    }
}

fn strip_line(line: &str) -> String {
    let trimmed = line.trim_start();
    // Headers and quotes carry no sound. `- ` and `* ` bullets are left as words would be: the
    // pause between list items is what a reader hears anyway.
    let body = trimmed
        .trim_start_matches('>')
        .trim_start_matches('#')
        .trim_start();
    strip_inline(body)
}

/// Longest angle-bracket form read as one, in characters. Every Discord form is far shorter; the
/// bound keeps a message full of stray `<` from re-copying the rest of the line at each one.
const ANGLE_FORM_CHARS: usize = 128;

/// Whether `chars` begins with `marker`, without copying the rest of the line.
fn starts_with_chars(chars: &[char], marker: &str) -> bool {
    let mut held = chars.iter();
    marker.chars().all(|wanted| held.next() == Some(&wanted))
}

/// The next position at or after a cursor that holds one character, found without reading any
/// character twice.
///
/// `#201 speech-prep-linear`: a search to the end of the line from every `[` in a line of them is
/// the square of the line's length. Positions are asked for in increasing order, so the last
/// answer stands until the cursor passes it, and the next search starts where the last one ended.
struct NextOf {
    wanted: char,
    /// The last answer: the position found, or `None` when nothing is left to find.
    found: Option<usize>,
    /// Everything before this has been read.
    read: usize,
}

impl NextOf {
    fn new(wanted: char) -> Self {
        Self {
            wanted,
            found: None,
            read: 0,
        }
    }

    /// The first position at or after `at` holding the character. `at` never decreases.
    fn from(&mut self, chars: &[char], at: usize) -> Option<usize> {
        if let Some(found) = self.found.filter(|&found| found >= at) {
            return Some(found);
        }
        let start = at.max(self.read);
        if start >= chars.len() {
            self.read = chars.len();
            self.found = None;
            return None;
        }
        self.found = chars[start..]
            .iter()
            .position(|&c| c == self.wanted)
            .map(|offset| start + offset);
        self.read = self.found.map_or(chars.len(), |found| found + 1);
        self.found
    }
}

/// Remove inline markers, resolve links to their text, say addresses as what they are, and name
/// the things that have no sound.
fn strip_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut closes = NextOf::new(']');
    let mut parens = NextOf::new(')');
    let mut i = 0;
    while i < chars.len() {
        // `[label](url)` reads as its label. A listener cannot use a URL and cannot write it down.
        if chars[i] == '[' {
            if let Some((label, consumed)) = link_at(&chars, i, &mut closes, &mut parens) {
                out.push_str(&label);
                i += consumed;
                continue;
            }
        }
        if chars[i] == '<' {
            // `<https://…>`, and a chat service's `<https://…|label>`, however long the address.
            if let Some((said, consumed)) = angle_address_at(&chars, i) {
                out.push_str(&said);
                i += consumed;
                continue;
            }
            // Discord's angle-bracket forms: `<@123>`, `<#123>`, `<@&123>`, `<:name:123>`,
            // `<t:…>`. `<t:…>` is a TIME and is left for `speak_times`; the rest have no useful
            // sound.
            let rest: String = chars[i..].iter().take(ANGLE_FORM_CHARS).collect();
            if let Some((said, consumed)) = angle_form_at(&rest) {
                out.push_str(&said);
                i += consumed;
                continue;
            }
        }
        // A bare web address is said as what it is. Before the emphasis markers, which would
        // otherwise take the underscores out of the middle of it.
        if let Some(end) = address_at(&chars, i) {
            let address: String = chars[i..end].iter().collect();
            out.push_str(&say_address(&address));
            i = end;
            continue;
        }
        // Emphasis markers, in descending length so `***` is not read as `*` twice. Spoilers and
        // strikethrough go the same way: the words inside them are still the message.
        let marker = ["***", "**", "~~", "||", "__", "*", "_", "`"]
            .into_iter()
            .find(|m| starts_with_chars(&chars[i..], m));
        if let Some(marker) = marker {
            let len = marker.chars().count();
            // `#200 reply-context`. An underscore INSIDE a word is part of a name — snake_case,
            // SCREAMING_SNAKE_CASE — not emphasis, as CommonMark has it. Dropping it ran the parts
            // together into one unsayable word; a space lets the voice say each part.
            let inside_word = marker.starts_with('_')
                && i.checked_sub(1)
                    .and_then(|j| chars.get(j))
                    .is_some_and(|c| c.is_alphanumeric())
                && chars.get(i + len).is_some_and(|c| c.is_alphanumeric());
            if inside_word {
                out.push(' ');
            }
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `[label](target)` at `start` → the label, and how many characters it took.
fn link_at(
    chars: &[char],
    start: usize,
    closes: &mut NextOf,
    parens: &mut NextOf,
) -> Option<(String, usize)> {
    let close = closes.from(chars, start + 1)?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = parens.from(chars, close + 2)?;
    let label: String = chars[start + 1..close].iter().collect();
    let said = if label.trim().is_empty() {
        // Nothing to say but where it goes: `[](https://…)`, or Discord's `[](<https://…>)`.
        let target: String = chars[close + 2..end].iter().collect();
        let target = target.split_whitespace().next().unwrap_or_default();
        let target = target.trim_start_matches('<').trim_end_matches('>');
        let chars: Vec<char> = target.chars().collect();
        if address_prefix_at(&chars, 0).is_some() {
            say_address(target)
        } else {
            "a link".to_owned()
        }
    } else {
        // The label wins over the address, and is prose like any other: emphasis comes off it,
        // and an address written as the label is said as what it is.
        strip_inline(&label)
    };
    Some((said, end + 1 - start))
}

/// `<https://…>`, or a chat service's `<https://…|label>`, at `start`: what to say for it, and how
/// many characters it took. A label wins; an address alone is said as what it is.
///
/// Its `>` is looked for only as far as the next `<` or the end of the line, so a line of
/// `<https://` with no `>` is read once, not once per bracket.
fn angle_address_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let inside = start + 1;
    scheme_at(chars, inside)?;
    let close = inside
        + chars[inside..]
            .iter()
            .position(|&c| matches!(c, '<' | '>' | '\n'))?;
    if chars[close] != '>' {
        return None;
    }
    let inner: String = chars[inside..close].iter().collect();
    let (address, label) = match inner.split_once('|') {
        Some((address, label)) => (address, label.trim()),
        None => (inner.as_str(), ""),
    };
    if address.chars().any(char::is_whitespace) {
        return None;
    }
    let said = if label.is_empty() {
        say_address(address)
    } else {
        strip_inline(label)
    };
    Some((said, close + 1 - start))
}

/// One of Discord's `<…>` forms, and how many characters it took.
fn angle_form_at(rest: &str) -> Option<(String, usize)> {
    let end = rest.find('>')?;
    let inner = &rest[1..end];
    // Characters, not bytes: the caller steps through characters.
    let consumed = rest[..=end].chars().count();
    // A timestamp is a TIME, not chrome. Left exactly as it is for the next pass.
    if inner.starts_with("t:") {
        return None;
    }
    let said = if inner.starts_with("@&") {
        "a role".to_owned()
    } else if inner.starts_with('@') {
        "a mention".to_owned()
    } else if inner.starts_with('#') {
        "a channel".to_owned()
    } else if inner.starts_with(':') || inner.starts_with("a:") {
        // `<:name:12345>` — the NAME is the only part with a sound.
        inner
            .split(':')
            .find(|part| !part.is_empty() && !part.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or("an emoji")
            .to_owned()
    } else if inner.starts_with("http://") || inner.starts_with("https://") {
        "a link".to_owned()
    } else {
        return None;
    };
    Some((said, consumed))
}

// --- addresses ----------------------------------------------------------------------------------

/// How a web address is said: what it is, and never how it is spelled. `#187 speakable-ids-urls`.
///
/// Read aloud, an address is "h t t p s colon slash slash" and a minute of path that a listener
/// can neither use nor write down. So a page [`crate::link_labels`] has a label for is said as that
/// page, "widgets pull request 41"; a diff or a task alone under a host is said as that id, "diff
/// ending in 567"; an address that is nothing but a host is said as the host, "example.com"; and
/// any other is said by its host alone, "a link on example.com". `www.` is dropped from a host,
/// as is a port, and the name and password an address may carry before its host are never said.
fn say_address(address: &str) -> String {
    let with_scheme;
    let address = if crate::link_labels::strip_scheme(address).is_some() {
        address
    } else {
        with_scheme = format!("https://{address}");
        &with_scheme
    };
    if let Some(said) = crate::link_labels::spoken_label(address) {
        return said;
    }
    let rest = crate::link_labels::strip_scheme(address).unwrap_or(address);
    let (authority, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if !plausible_host(host) {
        return "a link".to_owned();
    }
    if path.is_empty() || path == "/" {
        return host.to_owned();
    }
    lone_tracker_id(path).unwrap_or_else(|| format!("a link on {host}"))
}

/// A host name as a voice may say it: ASCII labels of letters, digits and hyphens, joined by dots,
/// and short enough to be one.
fn plausible_host(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// A path that is one diff or one task and nothing else, `/D1234567`, said as that id.
fn lone_tracker_id(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let (segment, after) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let alone = after.is_empty() || after == "/" || after.starts_with(['?', '#']);
    alone.then(|| say_tracker_id(segment)).flatten()
}

/// Whether `chars` holds `prefix` at `at`, in any case.
fn holds_at(chars: &[char], at: usize, prefix: &str) -> bool {
    let mut held = chars.get(at..).unwrap_or_default().iter();
    prefix
        .chars()
        .all(|wanted| held.next().is_some_and(|c| c.eq_ignore_ascii_case(&wanted)))
}

/// The length of `http://` or `https://` at `at`, in any case.
fn scheme_at(chars: &[char], at: usize) -> Option<usize> {
    ["https://", "http://"]
        .into_iter()
        .find(|scheme| holds_at(chars, at, scheme))
        .map(str::len)
}

/// The length of what begins a web address at `at`: a scheme, or `www.` or `github.com/` written
/// without one. Any other name followed by a path is too often a file to be read as an address.
fn address_prefix_at(chars: &[char], at: usize) -> Option<usize> {
    if let Some(length) = scheme_at(chars, at) {
        return Some(length);
    }
    ["www.", "github.com/"]
        .into_iter()
        .find(|prefix| holds_at(chars, at, prefix))
        .map(str::len)
}

/// Where a bare web address that begins at `start` ends, or `None` when none begins there.
///
/// One begins only at the start of a word, so `user@www.example.com` and `xhttps://` are left as
/// written. It runs to whitespace or to a character no address holds unescaped, and then gives
/// back what prose put after it rather than the address itself: a sentence's full stop, closing
/// emphasis, and a bracket it did not open — `(see https://example.com/a_(b))` keeps its own pair.
fn address_at(chars: &[char], start: usize) -> Option<usize> {
    if !matches!(chars[start], 'h' | 'H' | 'w' | 'W' | 'g' | 'G') {
        return None;
    }
    let follows_word = start
        .checked_sub(1)
        .and_then(|before| chars.get(before))
        .is_some_and(|&c| c.is_alphanumeric() || matches!(c, '.' | '@' | '/' | '-' | '+' | '%'));
    if follows_word {
        return None;
    }
    let prefix = address_prefix_at(chars, start)?;
    let mut end = start;
    let (mut parens, mut brackets) = (0_isize, 0_isize);
    while end < chars.len() && !ends_address(chars[end]) {
        match chars[end] {
            '(' => parens += 1,
            ')' => parens -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
        end += 1;
    }
    while end > start + prefix {
        match chars[end - 1] {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' | '_' | '~' => {}
            ')' if parens < 0 => parens += 1,
            ']' if brackets < 0 => brackets += 1,
            _ => break,
        }
        end -= 1;
    }
    (end > start + prefix).then_some(end)
}

/// A character that ends a bare address: whitespace, or one an address never holds unescaped.
fn ends_address(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')
}

// --- times --------------------------------------------------------------------------------------

/// The longest `<t:…>` form read as one, in bytes: `<t:`, an epoch in seconds, a style and `>` is
/// far shorter. The bound keeps a line of `<t:` with no `>` from searching the rest of the line at
/// each one.
const DISCORD_TIMESTAMP_BYTES: usize = 40;

/// The longest ISO-8601 instant read as one, in characters.
const ISO_INSTANT_CHARS: usize = 30;

/// Replace every instant we can recognise with something a listener can place.
///
/// `#201 speech-prep-linear`: linear in the length of `text`. It used to copy the rest of the text
/// at every character before looking at it, so a 210,000-character line cost the square of its
/// length — over two minutes — before a word was spoken. Each candidate is now read from a slice
/// of the text, and only where one can start: an instant at a digit, Discord's form at a `<`.
fn speak_times(text: &str, now_ms: i64, zone: &Zone) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        // Both forms begin with an ASCII byte, which is always the start of a character, so the
        // slice below is always on a boundary.
        let found = match bytes[at] {
            b'<' => discord_timestamp_at(&text[at..], now_ms, zone),
            b'0'..=b'9' => iso_timestamp_at(&text[at..], now_ms, zone),
            _ => None,
        };
        match found {
            Some((said, consumed)) => {
                out.push_str(&text[copied..at]);
                out.push_str(&said);
                at += consumed;
                copied = at;
            }
            None => at += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Discord's own `<t:1700000000:R>` form, and how many bytes it took. The style suffix is ignored:
/// this decides how to say it.
fn discord_timestamp_at(rest: &str, now_ms: i64, zone: &Zone) -> Option<(String, usize)> {
    if !rest.starts_with("<t:") {
        return None;
    }
    let end = rest
        .bytes()
        .take(DISCORD_TIMESTAMP_BYTES)
        .position(|byte| byte == b'>')?;
    let inner = &rest[3..end];
    let seconds: i64 = inner.split(':').next()?.parse().ok()?;
    // An epoch past what a timestamp can hold is not a time anybody wrote on purpose, and is left
    // as written rather than said as nothing.
    let then_ms = seconds.checked_mul(1000)?;
    Timestamp::from_millisecond(then_ms).ok()?;
    Some((say_instant(then_ms, now_ms, zone), end + 1))
}

/// A bare ISO-8601 instant sitting in the text, and how many bytes it took.
fn iso_timestamp_at(rest: &str, now_ms: i64, zone: &Zone) -> Option<(String, usize)> {
    // Cheap gate before the parse: an instant starts with a date.
    if !looks_like_a_date(rest.as_bytes()) {
        return None;
    }
    // Where each of the first characters ends, so every candidate is a slice and not a copy.
    let ends: Vec<usize> = rest
        .char_indices()
        .take(ISO_INSTANT_CHARS)
        .map(|(index, character)| index + character.len_utf8())
        .collect();
    // Longest first, so a full instant is not truncated to its date.
    for take in (10..=ends.len()).rev() {
        let candidate = &rest[..ends[take - 1]];
        let trimmed = candidate.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
        if let Ok(instant) = trimmed.parse::<Timestamp>() {
            return Some((
                say_instant(instant.as_millisecond(), now_ms, zone),
                trimmed.len(),
            ));
        }
    }
    None
}

/// Whether `head` begins with `YYYY-MM-DD`, as digits and dashes.
fn looks_like_a_date(head: &[u8]) -> bool {
    head.len() >= 10
        && head[0..4].iter().all(u8::is_ascii_digit)
        && head[4] == b'-'
        && head[5..7].iter().all(u8::is_ascii_digit)
        && head[7] == b'-'
        && head[8..10].iter().all(u8::is_ascii_digit)
}

/// How to say one instant, given when "now" is.
///
/// PRECISION IS PROPORTIONAL, and that is the whole rule. "Three hours and fifteen seconds ago" is
/// worse than "three hours ago" — the extra term is noise at that distance, and a listener has to
/// hold it in their head to discard it. So exactly ONE unit is ever spoken, the largest that
/// applies.
///
/// Past a year the relative reading stops helping: nobody hears "four hundred days ago" as a date.
/// Those are left as they were written, which the caller can still read, rather than converted
/// into something worse.
fn say_instant(then_ms: i64, now_ms: i64, zone: &Zone) -> String {
    let delta_ms = now_ms.saturating_sub(then_ms);
    let future = delta_ms < 0;
    let secs = (delta_ms / 1000).abs();

    let days = secs / 86_400;
    if days > RELATIVE_LIMIT_DAYS {
        // Older than a relative reading can carry. Say the date in full, year included, because at
        // that distance the year is the part that matters.
        return absolute(then_ms, zone, true);
    }
    // Far enough back that "N days" stops being a date anybody can place.
    if days >= 7 {
        return absolute(then_ms, zone, false);
    }
    let (count, unit) = if secs < 60 {
        (secs, "second")
    } else if secs < 3_600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3_600, "hour")
    } else {
        (days, "day")
    };
    let plural = if count == 1 { "" } else { "s" };
    if future {
        format!("in {count} {unit}{plural}")
    } else if count == 0 {
        "just now".to_owned()
    } else {
        format!("{count} {unit}{plural} ago")
    }
}

/// `August 3rd, a Tuesday` — and with the year when it is old enough to need one.
fn absolute(then_ms: i64, zone: &Zone, with_year: bool) -> String {
    let Ok(instant) = Timestamp::from_millisecond(then_ms) else {
        return String::new();
    };
    let local: Zoned = zone.at(instant);
    let month = local.strftime("%B").to_string();
    let day = local.day();
    let weekday = local.strftime("%A").to_string();
    let ordinal = ordinal_for(day);
    if with_year {
        let year = local.strftime("%Y").to_string();
        format!("{month} {day}{ordinal} {year}")
    } else {
        format!("{month} {day}{ordinal}, a {weekday}")
    }
}

fn ordinal_for(day: i8) -> &'static str {
    match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

// --- opaque strings -----------------------------------------------------------------------------

/// Replace hashes and long identifiers with placeholders, stable for as long as the table lives.
///
/// The SAME value gets the same letter every time it appears, which is the one thing about an
/// opaque string that survives being spoken: a listener can hear that two mentions are the same
/// commit without hearing the commit. How far "every time" reaches is the caller's choice and not
/// this function's — see [`Names`].
fn name_opaque_strings(text: &str, names: &mut Names) -> String {
    let Names { hashes } = names;
    let mut out = String::with_capacity(text.len());

    for token in split_keeping_separators(text) {
        let word = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if word.is_empty() {
            out.push_str(&token);
            continue;
        }
        let replacement = if let Some(said) = say_tracker_id(word) {
            Some(said)
        } else if is_big_number(word) {
            Some(format!("UID ending in {}", ending(word)))
        } else if is_hashlike(word) {
            Some(name_for(word, hashes, "hash code"))
        } else {
            None
        };
        match replacement {
            Some(said) => out.push_str(&token.replacen(word, &said, 1)),
            None => out.push_str(&token),
        }
    }
    out
}

fn name_for(value: &str, seen: &mut BTreeMap<String, String>, kind: &str) -> String {
    if let Some(held) = seen.get(value) {
        return held.clone();
    }
    if seen.len() >= NAMES_MAX {
        // Unnamed rather than mis-named. See `NAMES_MAX`.
        return format!("a {kind}");
    }
    let letter = letter_for(seen.len());
    let said = format!("{kind} {letter}");
    seen.insert(value.to_owned(), said.clone());
    said
}

/// A, B, … Z, AA, AB, … — spreadsheet columns, and like them it never repeats itself.
///
/// It used to double the letter past Z, on the reasoning that a message with twenty-seven distinct
/// hashes in it has other problems. That was true of a table that lived for one message and is
/// false of one that lives for a session: doubling gives `AA` to the twenty-seventh value and `AA`
/// again to the fifty-third, and two different ids answering to one name is worse than a long name,
/// because the listener has no way to hear that it happened.
fn letter_for(index: usize) -> String {
    let mut remaining = index + 1;
    let mut letters = Vec::new();
    while remaining > 0 {
        remaining -= 1;
        letters.push(char::from(b'A' + u8::try_from(remaining % 26).unwrap_or(0)));
        remaining /= 26;
    }
    letters.iter().rev().collect()
}

fn is_big_number(word: &str) -> bool {
    word.len() >= BIG_NUMBER_MIN_DIGITS && word.chars().all(|c| c.is_ascii_digit())
}

/// A Phabricator-style reference — `D` and a diff's number, `T` and a task's — said as its kind and
/// its last three digits: `D1234567890` is "diff ending in 890". `#187 speakable-ids-urls`.
///
/// Only with [`TRACKER_ID_MIN_DIGITS`] digits or more, so a short one in prose is left as written.
fn say_tracker_id(word: &str) -> Option<String> {
    let kind = match word.as_bytes().first()? {
        b'D' => "diff",
        b'T' => "task",
        _ => return None,
    };
    let digits = &word[1..];
    (digits.len() >= TRACKER_ID_MIN_DIGITS && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{kind} ending in {}", ending(digits)))
}

/// The short, speakable part of a numeric identifier: its last three digits.
fn ending(digits: &str) -> &str {
    &digits[digits.len().saturating_sub(3)..]
}

/// Hex-looking, long, and containing at least one digit.
///
/// The digit is what keeps an ordinary long word out: `unfortunately` is all letters and is a word;
/// `deadbeefcafe` is hex but so is `defaceable`, and requiring a digit costs almost no real hashes
/// while sparing the dictionary.
fn is_hashlike(word: &str) -> bool {
    word.len() >= HASH_MIN_CHARS
        && word.chars().all(|c| c.is_ascii_hexdigit())
        && word.chars().any(|c| c.is_ascii_digit())
}

fn split_keeping_separators(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            parts.push(ch.to_string());
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-24T12:00:00Z, so every "ago" below is measured from a fixed point.
    const NOW: i64 = 1_787_572_800_000;

    fn eastern() -> Zone {
        crate::clock::zone("America/New_York").expect("a bundled zone")
    }

    fn say(text: &str) -> String {
        for_speech(text, NOW, &eastern())
    }

    #[test]
    fn markdown_markers_are_not_read_out() {
        // The reported bug, verbatim: the voice said "asterisk asterisk" before every bold word.
        assert_eq!(say("the **deploy** is _done_"), "the deploy is done");
        assert_eq!(say("~~cancelled~~ and ||hidden||"), "cancelled and hidden");
        assert_eq!(say("## A heading"), "A heading");
        assert_eq!(say("> quoted text"), "quoted text");
        assert_eq!(say("run `make validate` now"), "run make validate now");
    }

    #[test]
    fn an_underscore_inside_a_name_is_a_pause_not_glue() {
        // `#200 reply-context`: identifiers were spoken as one run-together word.
        assert_eq!(
            say("set RUNNER_E2E_EMPTY_WORKDIR first"),
            "set RUNNER E2E EMPTY WORKDIR first"
        );
        assert_eq!(say("call parse_thread_id now"), "call parse thread id now");
        // Emphasis at word edges is still only emphasis.
        assert_eq!(say("this is _really_ done"), "this is really done");
        assert_eq!(say("open __init__.py"), "open init.py");
    }

    #[test]
    fn markers_and_brackets_are_stripped_in_linear_time() {
        // Every character used to copy the rest of the line, so a long line from a stranger cost
        // the square of its length before a word was spoken.
        let line = "a_<".repeat(70_000);
        let started = std::time::Instant::now();
        let stripped = strip_inline(&line);
        assert!(stripped.starts_with("a<a<"), "{}", &stripped[..12]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    /// `unit` repeated to exactly `length` characters. Every unit is ASCII, so a byte is a character.
    fn line_of(unit: &str, length: usize) -> String {
        assert!(unit.is_ascii());
        unit.repeat(length / unit.len() + 1)[..length].to_owned()
    }

    /// Every kind of thing preparation looks for, in one stretch of prose, and then each alone in
    /// the shape that made one stage search the rest of the line from every place it could start.
    const SCALING_UNITS: [&str; 13] = [
        "the **deploy** of parse_thread_id at 2026-08-24T11:30:00Z by <@1> in <#2> \
         <t:1787569200:R>, see [the runbook](https://example.com/x) about D1234567 \
         12345678901 a1b2c3d4e5f6a7 ",
        "[a",
        "[a](",
        "<t:1",
        "<a",
        "2026-08-24",
        "1234567890",
        "a_b*c~",
        "word ",
        "https://",
        "<https://a",
        "(https://x.test/a_b)",
        "[](https://x.test/",
    ];

    /// The least of three timings, which is the one least disturbed by whatever else the host was
    /// doing.
    fn least_cost(text: &str) -> std::time::Duration {
        let zone = eastern();
        (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                std::hint::black_box(for_speech(text, NOW, &zone));
                started.elapsed()
            })
            .min()
            .unwrap_or_default()
    }

    #[test]
    fn preparing_a_line_costs_its_length_and_not_its_square() {
        // `#201 speech-prep-linear`: a 210,000-character line took 142 s in a debug build, because
        // the time pass copied the rest of the line at every character and the link pass searched
        // the rest of it from every `[`. Real messages are short; a stranger's need not be.
        //
        // Compared as a ratio, so a slow or busy host slows both sides alike: four times the text
        // costs four times as much when the work is linear and sixteen times when it is quadratic,
        // and ten sits well between them. The small allowance keeps a pair of sub-millisecond
        // timings from failing on a scheduler's whim.
        for unit in SCALING_UNITS {
            let small = least_cost(&line_of(unit, 25_000));
            let large = least_cost(&line_of(unit, 100_000));
            assert!(
                large <= small * 10 + std::time::Duration::from_millis(20),
                "{unit:?}: 25,000 characters took {small:?} and 100,000 took {large:?}"
            );
        }
    }

    #[test]
    fn a_line_of_two_hundred_thousand_characters_is_prepared_promptly() {
        // The absolute bound beside the ratio above: generous, so that only a cost far out of
        // proportion to the text fails it, on a loaded host or in an unoptimised build.
        let limit = if cfg!(debug_assertions) { 30 } else { 5 };
        let line = line_of(SCALING_UNITS[0], 200_000);
        let started = std::time::Instant::now();
        let said = for_speech(&line, NOW, &eastern());
        let took = started.elapsed();
        assert!(said.starts_with("the deploy of parse thread id at 30 minutes ago"));
        assert!(
            took < std::time::Duration::from_secs(limit),
            "200,000 characters took {took:?}"
        );
    }

    #[test]
    fn a_link_reads_as_its_words_and_never_as_its_url() {
        assert_eq!(
            say("see [the runbook](https://example.com/x)"),
            "see the runbook"
        );
        assert_eq!(say("<@123456789012345678> look"), "a mention look");
        assert_eq!(say("in <#987654321098765432>"), "in a channel");
    }

    #[test]
    fn a_diff_or_a_task_is_said_by_its_ending() {
        // `#187 speakable-ids-urls`, the owner's own example.
        assert_eq!(say("landed D1234456789"), "landed diff ending in 789");
        assert_eq!(say("see D123456"), "see diff ending in 456");
        assert_eq!(say("filed T98765432."), "filed task ending in 432.");
        assert_eq!(
            say("(D1234567, T7654321)"),
            "(diff ending in 567, task ending in 321)"
        );
    }

    #[test]
    fn a_short_id_or_a_word_like_one_is_left_as_written() {
        for text in [
            "D42 and T7 and D12345 and T99999",
            "Dx1234567 d1234567 t1234567 D1234567a TODO T-1000000",
            "in 2026 on port 8080, 42 times",
        ] {
            assert_eq!(say(text), text);
        }
    }

    const WIDGETS: &str = "https://github.com/octo/widgets";
    const SHA: &str = "4f21ab0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a";

    #[test]
    fn a_github_address_is_said_as_the_page_it_names() {
        // Each row of the `#227 github-link-abbrev` table, in words.
        for (address, said) in [
            (format!("{WIDGETS}/pull/3871"), "widgets pull request 3871"),
            (format!("{WIDGETS}/issues/12"), "widgets issue 12"),
            (format!("{WIDGETS}/discussions/5"), "widgets discussion 5"),
            (
                format!("{WIDGETS}/pull/41#issuecomment-1234567"),
                "a comment on widgets pull request 41",
            ),
            (
                format!("{WIDGETS}/pull/41#pullrequestreview-7"),
                "a review of widgets pull request 41",
            ),
            (
                format!("{WIDGETS}/pull/41/files"),
                "the files of widgets pull request 41",
            ),
            (
                format!("{WIDGETS}/pull/41/checks"),
                "the checks of widgets pull request 41",
            ),
            (format!("{WIDGETS}/commit/{SHA}"), "widgets commit 4 F 2 1"),
            (
                format!("{WIDGETS}/commit/{SHA}#commitcomment-3"),
                "a comment on widgets commit 4 F 2 1",
            ),
            (
                format!("{WIDGETS}/pull/41/commits/{SHA}"),
                "widgets commit 4 F 2 1",
            ),
            (
                format!("{WIDGETS}/compare/main...feature"),
                "a comparison in widgets",
            ),
            (
                format!("{WIDGETS}/releases/tag/v1.2.0"),
                "widgets release v1.2.0",
            ),
            (format!("{WIDGETS}/tree/main"), "widgets branch main"),
            (format!("{WIDGETS}/tree/{SHA}"), "widgets commit 4 F 2 1"),
            (
                format!("{WIDGETS}/actions/runs/12345678901"),
                "widgets run ending in 901",
            ),
            (
                format!("{WIDGETS}/actions/runs/123456"),
                "widgets run 123456",
            ),
            (
                format!("{WIDGETS}/actions/runs/12345678901/job/98765432101"),
                "a job in widgets run ending in 901",
            ),
            (
                format!("{WIDGETS}/actions/runs/12345678901/attempts/2"),
                "attempt 2 of widgets run ending in 901",
            ),
            (
                format!("{WIDGETS}/blob/main/src/render.rs#L10"),
                "widgets file render.rs line 10",
            ),
            (
                format!("{WIDGETS}/blob/main/src/render.rs#L10-L20"),
                "widgets file render.rs lines 10 to 20",
            ),
            (
                format!("{WIDGETS}/blob/main/README.md"),
                "widgets file README.md",
            ),
            // Either scheme, `www.`, the host's case and a trailing slash change nothing, and the
            // host may be written with no scheme at all.
            (
                "HTTP://WWW.GitHub.com/octo/widgets/pull/41/".to_owned(),
                "widgets pull request 41",
            ),
            (
                "github.com/octo/widgets/issues/12".to_owned(),
                "widgets issue 12",
            ),
            // An underscore in a name is a pause, as in prose.
            (
                "https://github.com/octo/my_widgets/pull/3".to_owned(),
                "my widgets pull request 3",
            ),
        ] {
            assert_eq!(say(&address), said, "{address}");
        }
    }

    #[test]
    fn any_other_address_is_said_by_its_host_and_a_bare_host_as_itself() {
        for (address, said) in [
            (
                "https://example.com/some/long/path?q=1#part",
                "a link on example.com",
            ),
            ("https://www.foo-bar.com/a/b/c", "a link on foo-bar.com"),
            // The underscores inside an address are not emphasis, nor a pause.
            (
                "https://docs.example.org/a_b_c/d_e",
                "a link on docs.example.org",
            ),
            ("http://Example.COM:8080/x", "a link on example.com"),
            ("www.example.com/docs/start", "a link on example.com"),
            ("https://example.com", "example.com"),
            ("https://www.example.com/", "example.com"),
            // GitHub, but no page with a short form: said like any other address.
            ("https://github.com/octo/widgets", "a link on github.com"),
            (
                "https://github.com/octo/widgets/pull/41?notification_referrer_id=x",
                "a link on github.com",
            ),
            // A diff or a task alone on its host is said as the id it is.
            (
                "https://phabricator.example.com/D1234567",
                "diff ending in 567",
            ),
            (
                "https://tasks.example.com/T7654321?view=1",
                "task ending in 321",
            ),
            // A host a voice could not say is not said.
            ("https://[::1]:8080/status", "a link"),
            ("https://ex%61mple.com/x", "a link"),
        ] {
            let spoken = say(address);
            assert_eq!(spoken, said, "{address}");
            assert!(
                !spoken.contains("http") && !spoken.contains("//"),
                "{spoken}"
            );
        }
    }

    #[test]
    fn the_credentials_an_address_carries_are_never_said() {
        assert_eq!(
            say("try http://deploy:hunter2@example.com:8080/x now"),
            "try a link on example.com now"
        );
        assert_eq!(say("https://token@example.com"), "example.com");
    }

    #[test]
    fn emails_paths_and_short_bare_domains_are_left_as_written() {
        for text in [
            "mail octo@example.com about example.com",
            "see foo-bar.com and docs.example.org today",
            "edit src/speech.rs and docs/guide.md",
            "write to octo@www.example.com",
            "a git@github.com:octo/widgets.git remote",
            "the scheme is https:// alone",
        ] {
            assert_eq!(say(text), text);
        }
    }

    #[test]
    fn an_address_keeps_the_prose_around_it() {
        for (text, said) in [
            (
                "Fixed in https://github.com/octo/widgets/pull/41.",
                "Fixed in widgets pull request 41.",
            ),
            (
                "(see https://example.com/a_(b))",
                "(see a link on example.com)",
            ),
            ("**https://example.com/x**", "a link on example.com"),
            ("_https://example.com/x_", "a link on example.com"),
            ("\"https://example.com/x\"", "\"a link on example.com\""),
            (
                "https://example.com/x, https://example.org/y; done",
                "a link on example.com, a link on example.org; done",
            ),
            (
                "> quoted https://github.com/octo/widgets/issues/7",
                "quoted widgets issue 7",
            ),
        ] {
            assert_eq!(say(text), said, "{text}");
        }
    }

    #[test]
    fn markdown_link_text_wins_over_its_address() {
        for (text, said) in [
            (
                "[the fix](https://github.com/octo/widgets/pull/41)",
                "the fix",
            ),
            ("[the **real** fix](https://x.test/a)", "the real fix"),
            // A link whose text is its own address says the address as what it is.
            (
                "[https://github.com/octo/widgets/pull/41](https://github.com/octo/widgets/pull/41)",
                "widgets pull request 41",
            ),
            // With no text, where it goes is all there is to say.
            (
                "[](https://github.com/octo/widgets/issues/7)",
                "widgets issue 7",
            ),
            ("[](<https://example.com/x>)", "a link on example.com"),
            ("[ ](relative/page)", "a link"),
            // Angle brackets, and a chat service's labelled form.
            (
                "<https://github.com/octo/widgets/pull/41>",
                "widgets pull request 41",
            ),
            ("<https://example.com/x|the runbook>", "the runbook"),
            ("<https://example.com/x|https://example.com/x>", "a link on example.com"),
        ] {
            assert_eq!(say(text), said, "{text}");
        }
        // An address in angle brackets is said whatever its length.
        let long = format!("open <https://example.com/{}> now", "a".repeat(300));
        assert_eq!(say(&long), "open a link on example.com now");
    }

    #[test]
    fn a_code_span_keeps_its_words_and_still_never_spells_an_address() {
        // The words in code are the point of it, and are said as they were.
        assert_eq!(
            say("run `make validate` in `octo/widgets` on `src/speech.rs`"),
            "run make validate in octo/widgets on src/speech.rs"
        );
        // An address or an id in code is said as it is in prose: agents put both in backticks,
        // and code is no reason to hear "h t t p s".
        assert_eq!(
            say("`curl https://example.com/api/v1` for `D1234567`"),
            "curl a link on example.com for diff ending in 567"
        );
        assert_eq!(
            say("```\ncurl https://example.com/api\n```"),
            "curl a link on example.com"
        );
    }

    #[test]
    fn a_message_mixing_them_reads_as_one_sentence() {
        let message = "**Landed** D1234456789 (https://github.com/octo/widgets/pull/3871) at \
                       2026-08-24T11:30:00Z; see [the runbook](https://wiki.example.com/x/y) or \
                       https://www.foo-bar.com/a/b/c, and a1b2c3d4e5f6a7. Ping octo@example.com \
                       about T1234567 or <https://github.com/octo/widgets/issues/12>.";
        assert_eq!(
            say(message),
            "Landed diff ending in 789 (widgets pull request 3871) at 30 minutes ago; see the \
             runbook or a link on foo-bar.com, and hash code A. Ping octo@example.com about task \
             ending in 567 or widgets issue 12."
        );
    }

    #[test]
    fn a_short_snippet_is_spoken_and_a_long_one_is_named() {
        // Short: the snippet IS the message, and dropping it removes what the reader was shown.
        assert_eq!(say("```\ncargo test\n```"), "cargo test");
        // Long: a minute of spoken punctuation, so it is announced with its size instead.
        let long = "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\nlet e = 5;\n```";
        assert_eq!(say(long), "a rust code snippet, 5 lines long.");
        // ...and an unlabelled fence still says how big it is.
        let plain = "```\none\ntwo\nthree\nfour\n```";
        assert_eq!(say(plain), "a code snippet, 4 lines long.");
    }

    #[test]
    fn a_table_becomes_a_sentence_about_the_table() {
        let table = "\
| step | result |
|---|---|
| build | ok |
| test | ok |
| package | failed |";
        // Keys read, because they are what the table is ABOUT.
        assert_eq!(
            say(table),
            "a table with 2 columns and 3 rows, with keys build, test, package."
        );
    }

    #[test]
    fn a_table_of_identifiers_says_its_shape_and_stops() {
        // Reciting a first column of hashes is the wall of text the placeholder exists to remove.
        let table = "\
| commit | status |
|---|---|
| a1b2c3d4e5f6a7 | ok |
| f6e5d4c3b2a1f0 | ok |";
        assert_eq!(say(table), "a table with 2 columns and 2 rows.");
    }

    #[test]
    fn the_same_hash_twice_is_the_same_placeholder_and_a_different_one_is_not() {
        // The only thing about a hash that survives being spoken: whether it is the same one.
        let said = say("deployed a1b2c3d4e5f6a7 then reverted a1b2c3d4e5f6a7, not ffee0011223344");
        assert_eq!(
            said,
            "deployed hash code A then reverted hash code A, not hash code B"
        );
    }

    #[test]
    fn a_carried_table_makes_one_letter_mean_one_hash_across_messages() {
        let mut names = Names::new();
        let zone = eastern();
        let first = for_speech_with("from a1b2c3d4e5f6a7", NOW, &zone, &mut names);
        let second = for_speech_with("also from a1b2c3d4e5f6a7", NOW, &zone, &mut names);
        let other = for_speech_with("but ffee0011223344 replied", NOW, &zone, &mut names);

        assert_eq!(first, "from hash code A");
        assert_eq!(second, "also from hash code A");
        assert_eq!(other, "but hash code B replied");
        assert_eq!(names.len(), 2);

        // And a table that was NOT carried says A for the second hash, which is exactly the
        // confusion the carried one exists to remove.
        let restarted = for_speech("but ffee0011223344 replied", NOW, &zone);
        assert_eq!(restarted, "but hash code A replied");
    }

    #[test]
    fn a_session_long_table_runs_out_of_single_letters_without_reusing_one() {
        // Twenty-seven distinct ids in one MESSAGE is a pathology. Twenty-seven across an hour of
        // a busy channel is a Tuesday, so the sequence has to keep going honestly.
        let assigned: Vec<String> = (0..80).map(letter_for).collect();
        let distinct: std::collections::BTreeSet<&String> = assigned.iter().collect();
        assert_eq!(distinct.len(), assigned.len(), "two values share a name");
        assert_eq!(assigned[0], "A");
        assert_eq!(assigned[25], "Z");
        assert_eq!(assigned[26], "AA");
        assert_eq!(assigned[27], "AB");
        // The old doubling scheme said AA here too, and nothing in the audio would have said so.
        assert_eq!(assigned[52], "BA");
    }

    #[test]
    fn uids_do_not_consume_the_hash_letter_table() {
        let mut names = Names::new();
        assert!(names.is_empty());
        let said = for_speech_with(
            "1000000000000000009 touched a1b2c3d4e5f6a7",
            NOW,
            &eastern(),
            &mut names,
        );
        assert_eq!(said, "UID ending in 009 touched hash code A");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn a_table_that_has_filled_up_stops_naming_rather_than_reusing_a_name() {
        // A table on the server outlives any one conversation, so it needs a bound. The bound has
        // to be a bound on the TABLE and not on the meaning of a letter: a full table that started
        // handing out `A` again would make "hash code A" silently ambiguous, and nothing in the
        // audio would say so. Unnamed is audibly different, which is the property being asserted.
        let mut names = Names::new();
        let zone = eastern();
        for i in 0..NAMES_MAX {
            let value = format!("a{i:012x}");
            let _ = for_speech_with(&value, NOW, &zone, &mut names);
        }
        assert_eq!(names.len(), NAMES_MAX, "the cap is not where it says it is");

        let overflow = for_speech_with("b123456789abc", NOW, &zone, &mut names);
        assert_eq!(overflow, "a hash code");
        assert_eq!(names.len(), NAMES_MAX, "a full table grew anyway");

        // And a value already in the table is still answered from it: filling up must not cost the
        // letters already handed out, which are the ones a listener has heard.
        let known = for_speech_with("a000000000000", NOW, &zone, &mut names);
        assert_eq!(known, "hash code A");
    }

    #[test]
    fn a_shared_table_names_the_same_value_the_same_way_for_every_caller() {
        // What the server actually holds. Every handler names into this one, so a hash the page
        // heard about through the live stream is the same letter when the audio route reads the
        // same message a minute later.
        let shared = SharedNames::new();
        let zone = eastern();
        assert!(shared.is_empty());
        let first = shared.for_speech("from a1b2c3d4e5f6a7", NOW, &zone);
        let second = shared.for_speech("still a1b2c3d4e5f6a7", NOW, &zone);
        let other = shared.for_speech("but ffee0011223344", NOW, &zone);
        assert_eq!(first, "from hash code A");
        assert_eq!(second, "still hash code A");
        assert_eq!(other, "but hash code B");
        assert_eq!(shared.len(), 2);
    }

    #[test]
    fn long_identifiers_and_diff_tags_have_useful_endings() {
        assert_eq!(say("id 1000000000000000009"), "id UID ending in 009");
        assert_eq!(say("review D1234567890"), "review diff ending in 890");
        // A year, a port and a count are quantities somebody wants to hear.
        assert_eq!(
            say("in 2026 on port 8080, 42 times"),
            "in 2026 on port 8080, 42 times"
        );
    }

    #[test]
    fn precision_is_proportional_and_never_two_units_at_once() {
        // "3 hours and 15 seconds ago" is worse than "3 hours ago": the second term is noise at
        // that distance and the listener has to hold it to discard it.
        assert_eq!(say("at 2026-08-24T11:59:45Z"), "at 15 seconds ago");
        assert_eq!(say("at 2026-08-24T11:30:00Z"), "at 30 minutes ago");
        assert_eq!(say("at 2026-08-24T09:00:15Z"), "at 2 hours ago");
        assert_eq!(say("at 2026-08-22T12:00:00Z"), "at 2 days ago");
    }

    #[test]
    fn a_date_far_enough_back_is_said_as_a_date() {
        // Past a week, "N days ago" stops being something anybody can place.
        let said = say("at 2026-08-04T12:00:00Z");
        assert!(said.contains("August 4th"), "{said}");
        assert!(said.contains("a Tuesday"), "{said}");
        assert!(
            !said.contains("2026"),
            "the year is noise at this distance: {said}"
        );
    }

    #[test]
    fn past_a_year_the_year_comes_back() {
        let said = say("at 2024-03-05T12:00:00Z");
        assert!(said.contains("March 5th"), "{said}");
        assert!(
            said.contains("2024"),
            "a date this old needs its year: {said}"
        );
    }

    #[test]
    fn discords_own_timestamp_markup_is_resolved_too() {
        // <t:seconds:style>. The style is ignored: this decides how to say it.
        assert_eq!(say("shipped <t:1787569200:R>"), "shipped 1 hour ago");
    }

    #[test]
    fn an_epoch_too_large_for_a_time_is_left_as_written() {
        // Seconds that overflow when counted in milliseconds used to panic a debug build.
        assert_eq!(
            say("at <t:9223372036854776:R>"),
            "at <t:9223372036854776:R>"
        );
        // One that fits in milliseconds but past any calendar used to be said as nothing at all.
        assert_eq!(say("at <t:999999999999:R>"), "at <t:999999999999:R>");
        // And a `<t:` whose `>` is past any real form is not read as one.
        let far = format!("<t:1787569200{}>", " ".repeat(64));
        assert_eq!(say(&far), "<t:1787569200 >");
    }

    #[test]
    fn a_time_is_resolved_before_a_long_number_can_claim_it() {
        // A Unix epoch and a snowflake are both long runs of digits, and only one is a time. If the
        // order were reversed this would read "UID ending in 200" and the time would be gone.
        assert_eq!(say("<t:1787569200:f>"), "1 hour ago");
    }

    #[test]
    fn the_absolute_reading_uses_the_operators_zone() {
        // 03:00Z on the 5th is still the 4th in New York, and saying the wrong day is worse than
        // saying a bare instant.
        let said = for_speech("at 2026-08-05T03:00:00Z", NOW, &eastern());
        assert!(said.contains("August 4th"), "{said}");
    }

    #[test]
    fn a_message_with_nothing_speakable_in_it_does_not_become_nonsense() {
        assert_eq!(say(""), "");
        assert_eq!(say("   \n\n  "), "");
        assert_eq!(say("**"), "");
    }
}
