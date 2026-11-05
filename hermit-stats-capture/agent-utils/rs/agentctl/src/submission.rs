//! Verified prompt submission for terminal agent composers.
//!
//! A terminal agent accepts a prompt in two separate steps: the text is staged in
//! its composer, and a submission key moves it into the conversation (or into the
//! agent's own queue while it is busy). Sending the key is not proof that the
//! second step happened. An agent can drop a key while it is redrawing or running
//! its own hooks, and the text then stays in the composer indefinitely.
//!
//! [`submit_verified`] drives both steps against the rendered screen and reports
//! one of three outcomes:
//!
//! * nothing was typed ([`Submission::NotStaged`]), so the prompt is safe to retry;
//! * the text left the composer and the screen corroborates a submission
//!   ([`Submission::Verified`]);
//! * an error: either the text is still staged after the deadline (it is left
//!   visible and must never be typed again automatically) or the outcome is
//!   unknown. Callers treat both as possibly submitted.

use std::time::Duration;

use crate::agent::AgentRuntime;
use crate::error::{AdapterError, Result};

/// Harnesses whose composer layout this module can read.
pub const VERIFIED_HARNESSES: [&str; 2] = ["claude", "codex"];
/// Screen rows requested for every composer read.
pub const SCREEN_LINES: usize = 200;
/// Maximum wait for pasted text to appear in the composer before any key is sent.
///
/// The wait ends at the first read that shows the paste, about 0.1 s after it in the
/// usual case. It is long because giving up leaves the paste unsubmitted in the
/// composer, where the draft check then holds every later prompt, and a paste typed
/// into a busy agent's pane has gone unseen for 3 s and then been found in its composer.
/// A paste that shows late or never costs the rest of the wait: the queue drain that
/// typed it keeps its queue and pane locks meanwhile, so other agentctl senders to the
/// pane wait too, and any new paste placeholder drawn in that time, even another
/// sender's, counts as this paste.
pub const STAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time spent retrying the submission key and waiting for corroboration.
pub const SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time spent waiting, before typing anything, for a recognisable composer.
///
/// Herdr can report a freshly launched harness ready before it has drawn its composer.
/// Nothing is typed while waiting, so the wait cannot duplicate a prompt.
pub const COMPOSER_WAIT: Duration = Duration::from_secs(15);
/// First wait before a still-staged prompt receives another submission key.
pub const FIRST_RETRY: Duration = Duration::from_millis(500);
/// Upper bound on the doubling wait between submission keys.
pub const MAX_RETRY: Duration = Duration::from_secs(4);
/// Screen polling interval while waiting.
pub const POLL: Duration = Duration::from_millis(100);
/// Longest extra wait for printed evidence once weaker evidence has proved a submission.
///
/// A busy agent shows its running-turn marker before the key and after it, so that marker proves
/// only that the prompt left the composer. The screen is then read every [`POLL`] for at most
/// this long, without sending another key, for something the pane printed after the key. The
/// submission deadline does not shorten this wait.
pub const PRINT_GRACE: Duration = Duration::from_secs(2);

const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";
const QUEUE_MARKERS: [&str; 3] = [
    "Press up to edit queued messages",
    "edit last queued message",
    "Queued follow-up inputs",
];
const WORKING_MARKER: &str = "esc to interrupt";
/// Rows, ending at the last row with text, that `running_turn` searches for a marker.
const RUNNING_TURN_ROWS: usize = 16;
/// The hint Claude Code draws in place of the status row under its input box for a while after
/// a paste, so that row's `esc to interrupt` is missing even while a turn runs.
const PASTE_HINT: &str = "paste again to expand";
const CODEX_QUEUE_HINT: &str = "tab to queue message";
/// Glyphs Codex draws in column 0 of its composer's first row. Earlier releases
/// draw `›`; v0.159.1 draws `»`, and keeps `›` for selection lists such as its
/// folder-trust prompt.
const CODEX_COMPOSER_MARKERS: [char; 2] = ['›', '»'];

/// Positive evidence that a staged prompt left the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionReceipt {
    /// Submission key used for the final press.
    pub key: &'static str,
    /// Number of submission keys sent, including retries.
    pub key_presses: u32,
    /// Time from the paste to the observed evidence.
    pub elapsed: Duration,
    /// The screen observation that established the submission.
    pub evidence: String,
    /// Whether `evidence` is something the pane printed after the submission key: the prompt
    /// or its paste placeholder above the composer, or a queued-message or running-turn marker
    /// that no screen read before the key showed. A running-turn marker that was already
    /// showing, as it always is on a busy agent, proves only that the prompt left the composer.
    pub printed: bool,
    /// Whether the evidence held: the wait for printed evidence ran its full course and its last
    /// read showed a recognisable composer without the prompt, or printed evidence was found.
    /// False when the wait ended early, because the caller stopped or a read failed, or ended on
    /// a screen with no recognisable composer: then nothing showed that the prompt did not come
    /// back into the composer after a key the agent dropped.
    pub settled: bool,
}

/// How a prompt submission was established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    /// The screen proved that the prompt left the composer.
    Verified(SubmissionReceipt),
    /// The prompt was handed to a primitive that gives no proof; confirm it another way.
    Unconfirmed,
    /// Nothing was typed; the reason explains why the prompt remains pending.
    NotStaged(String),
}

/// Terminal operations used by [`submit_verified`].
pub trait PromptTerminal {
    /// Return visible rows with SGR styling retained.
    fn read_screen(&self, pane_id: &str) -> Result<String>;
    /// Insert literal bytes without a submission key.
    fn send_text(&self, pane_id: &str, text: &str) -> Result<()>;
    /// Send one named key.
    fn send_keys(&self, pane_id: &str, keys: &str) -> Result<()>;
}

/// A prompt terminal that runs each input effect between recipient checks.
///
/// Every write, including each submission-key retry inside [`submit_verified`],
/// goes through one of these methods, so the implementation can prove the pane
/// still holds the intended program immediately before and after it.
pub trait GuardedInput {
    /// Return visible rows with SGR styling retained.
    fn read_screen(&self, pane_id: &str) -> Result<String>;
    /// Insert literal bytes without a submission key.
    fn send_text(&self, pane_id: &str, text: &str) -> Result<()>;
    /// Send one named key.
    fn send_keys(&self, pane_id: &str, keys: &str) -> Result<()>;
    /// Submit text plus Enter through Herdr's native prompt primitive.
    fn native_prompt(&self, pane_id: &str, text: &str) -> Result<()>;
}

/// The [`PromptTerminal`] view of a [`GuardedInput`], for [`submit_verified`].
pub struct GuardedPrompt<'a>(pub &'a dyn GuardedInput);

impl PromptTerminal for GuardedPrompt<'_> {
    fn read_screen(&self, pane_id: &str) -> Result<String> {
        self.0.read_screen(pane_id)
    }
    fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.0.send_text(pane_id, text)
    }
    fn send_keys(&self, pane_id: &str, keys: &str) -> Result<()> {
        self.0.send_keys(pane_id, keys)
    }
}

/// Deadlines for the two phases of one verified submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitTimeouts {
    /// Wait for the pasted text to become visible.
    pub stage: Duration,
    /// Wait for the submission to be proven.
    pub submit: Duration,
    /// Wait, before typing, for a recognisable composer.
    pub composer: Duration,
}

impl Default for SubmitTimeouts {
    fn default() -> Self {
        Self {
            stage: STAGE_TIMEOUT,
            submit: SUBMIT_TIMEOUT,
            composer: COMPOSER_WAIT,
        }
    }
}

/// One screen split around the agent's composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerView {
    /// Rows above the composer.
    pub transcript: String,
    /// Composer rows as displayed, marker removed.
    pub composer: String,
    /// Composer rows with faint and reverse-video cells blanked.
    pub composer_solid: String,
    /// Rows below the composer.
    pub footer: String,
}

/// Return whether `text` for `harness` uses the verified path.
///
/// Slash commands open harness menus and confirmation dialogs rather than
/// submitting through the composer, so they keep the native primitive.
pub fn verifies(harness: &str, text: &str) -> bool {
    VERIFIED_HARNESSES.contains(&harness) && !text.trim_start().starts_with('/')
}

/// Return `(plain, solid)` per row; `solid` blanks faint and reverse-video cells.
///
/// Agents draw composer placeholders and the cursor cell in faint or reverse
/// video. Blanking those cells lets an empty composer be told apart from one
/// holding a typed draft without a list of placeholder sentences.
pub fn render_screen(screen: &str) -> Vec<(Vec<char>, Vec<char>)> {
    let mut rows = Vec::new();
    let mut faint = false;
    let mut reverse = false;
    for raw in screen.replace('\r', "").split('\n') {
        let chars: Vec<char> = raw.chars().collect();
        let mut plain = Vec::new();
        let mut solid = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            let character = chars[index];
            if character == '\u{1b}' {
                index = skip_escape(&chars, index, &mut faint, &mut reverse);
                continue;
            }
            index += 1;
            if character < ' ' && character != '\t' {
                continue;
            }
            plain.push(character);
            solid.push(if faint || reverse { ' ' } else { character });
        }
        rows.push((plain, solid));
    }
    rows
}

/// Consume one escape sequence starting at `start`, applying SGR attributes.
fn skip_escape(chars: &[char], start: usize, faint: &mut bool, reverse: &mut bool) -> usize {
    let Some(&kind) = chars.get(start + 1) else {
        return start + 1;
    };
    match kind {
        '[' => {
            let mut index = start + 2;
            let parameters_start = index;
            while index < chars.len()
                && matches!(chars[index], '0'..='9' | ';' | ':' | '?' | '<' | '=' | '>')
            {
                index += 1;
            }
            let parameters_end = index;
            while index < chars.len() && (' '..='/').contains(&chars[index]) {
                index += 1;
            }
            match chars.get(index) {
                Some(&last) if ('@'..='~').contains(&last) => {
                    let parameters: String =
                        chars[parameters_start..parameters_end].iter().collect();
                    if last == 'm'
                        && parameters_end == index
                        && parameters
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == ';' || c == ':')
                    {
                        apply_sgr(&parameters, faint, reverse);
                    }
                    index + 1
                }
                _ => start + 1,
            }
        }
        ']' => {
            let mut index = start + 2;
            while index < chars.len() {
                if chars[index] == '\u{7}' {
                    return index + 1;
                }
                if chars[index] == '\u{1b}' {
                    return if chars.get(index + 1) == Some(&'\\') {
                        index + 2
                    } else {
                        start + 1
                    };
                }
                index += 1;
            }
            start + 1
        }
        'P' | 'X' | '^' | '_' => {
            let mut index = start + 2;
            while index < chars.len() {
                if chars[index] == '\u{1b}' {
                    return if chars.get(index + 1) == Some(&'\\') {
                        index + 2
                    } else {
                        start + 1
                    };
                }
                index += 1;
            }
            start + 1
        }
        '@'..='Z' | '\\' => start + 2,
        _ => start + 1,
    }
}

fn apply_sgr(parameters: &str, faint: &mut bool, reverse: &mut bool) {
    let codes: Vec<&str> = if parameters.is_empty() {
        vec!["0"]
    } else {
        parameters
            .split(';')
            .map(|part| part.split(':').next().unwrap_or(""))
            .collect()
    };
    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            "" | "0" => {
                *faint = false;
                *reverse = false;
            }
            "2" => *faint = true,
            "22" => *faint = false,
            "7" => *reverse = true,
            "27" => *reverse = false,
            "38" | "48" | "58" if index + 1 < codes.len() => {
                // Extended colours carry operands (5;N or 2;R;G;B) that are not attributes.
                index += match codes[index + 1] {
                    "5" => 2,
                    "2" => 4,
                    _ => 0,
                };
            }
            _ => {}
        }
        index += 1;
    }
}

fn text_of(row: &[char]) -> String {
    row.iter().collect()
}

fn is_rule_char(character: char) -> bool {
    matches!(character, '─' | '━' | '═')
}

fn is_rule(line: &[char]) -> bool {
    let text = text_of(line);
    let stripped = text.trim();
    stripped.chars().count() >= 3 && stripped.chars().all(is_rule_char)
}

/// Return whether `line` is a rule, possibly with a label drawn into it.
///
/// Claude can draw session state into the top border of its composer, as in
/// `──────── ultracode ─`. Such a row still starts and ends with rule
/// characters; whatever lies between the two runs is the label.
fn is_labelled_rule(line: &[char]) -> bool {
    let text = text_of(line);
    let stripped: Vec<char> = text.trim().chars().collect();
    let leading = stripped.iter().take_while(|c| is_rule_char(**c)).count();
    let trailing = stripped
        .iter()
        .rev()
        .take_while(|c| is_rule_char(**c))
        .count();
    stripped.len() >= 3 && leading >= 1 && trailing >= 1 && leading + trailing >= 3
}

fn join_rows(rows: &[(Vec<char>, Vec<char>)]) -> String {
    rows.iter()
        .map(|(plain, _)| text_of(plain))
        .collect::<Vec<_>>()
        .join("\n")
}

fn composer_from(body: &[(Vec<char>, Vec<char>)], marker_end: usize) -> (String, String) {
    let mut plain_lines: Vec<String> = body.iter().map(|(plain, _)| text_of(plain)).collect();
    let mut solid_lines: Vec<String> = body.iter().map(|(_, solid)| text_of(solid)).collect();
    plain_lines[0] = text_of(&body[0].0[marker_end.min(body[0].0.len())..]);
    solid_lines[0] = text_of(&body[0].1[marker_end.min(body[0].1.len())..]);
    (plain_lines.join("\n"), solid_lines.join("\n"))
}

fn claude_view(rows: &[(Vec<char>, Vec<char>)]) -> Option<ComposerView> {
    // A labelled top border is tried only when two plain rules do not already
    // frame a composer, so a draft row that happens to look like a labelled
    // rule cannot change how an unlabelled screen is split.
    claude_view_framed(rows, is_rule).or_else(|| claude_view_framed(rows, is_labelled_rule))
}

/// Split `rows` at the last plain rule and the nearest row above it accepted by `is_top`.
fn claude_view_framed(
    rows: &[(Vec<char>, Vec<char>)],
    is_top: fn(&[char]) -> bool,
) -> Option<ComposerView> {
    let bottom = rows.iter().rposition(|(plain, _)| is_rule(plain))?;
    let top = rows[..bottom]
        .iter()
        .rposition(|(plain, _)| is_top(plain))?;
    let body = &rows[top + 1..bottom];
    let first = body.first()?;
    let offset = first.0.iter().take_while(|c| c.is_whitespace()).count();
    if first.0.get(offset) != Some(&'❯') {
        return None;
    }
    let (composer, composer_solid) = composer_from(body, offset + 1);
    Some(ComposerView {
        transcript: join_rows(&rows[..top]),
        composer,
        composer_solid,
        footer: join_rows(&rows[bottom + 1..]),
    })
}

fn codex_view(rows: &[(Vec<char>, Vec<char>)]) -> Option<ComposerView> {
    let populated: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (plain, _))| plain.iter().any(|c| !c.is_whitespace()))
        .map(|(index, _)| index)
        .collect();
    if populated.len() < 2 {
        return None;
    }
    let footer = populated[populated.len() - 1];
    if !text_of(&rows[footer].0).starts_with("  ") {
        return None;
    }
    let mut start = None;
    for index in (0..footer).rev() {
        let plain = text_of(&rows[index].0);
        if plain.starts_with(&CODEX_COMPOSER_MARKERS[..]) {
            start = Some(index);
            break;
        }
        if !plain.trim().is_empty() && !plain.starts_with("  ") {
            return None;
        }
    }
    let start = start?;
    let (composer, composer_solid) = composer_from(&rows[start..footer], 1);
    Some(ComposerView {
        transcript: join_rows(&rows[..start]),
        composer,
        composer_solid,
        footer: join_rows(&rows[footer..]),
    })
}

/// Locate the composer of a supported harness, or `None` when it is not recognisable.
pub fn composer_view(harness: &str, screen: &str) -> Option<ComposerView> {
    let rows = render_screen(screen);
    match harness {
        "claude" => claude_view(&rows),
        "codex" => codex_view(&rows),
        _ => None,
    }
}

/// Whether Codex shows an empty idle composer with a recognized status footer.
///
/// This screen observation establishes readiness only after the caller independently
/// verifies the live recipient. A retained composer during a turn or dialog is not idle.
pub(crate) fn codex_idle_composer(screen: &str) -> bool {
    let Some(view) = composer_view("codex", screen) else {
        return false;
    };
    if !view.composer_solid.trim().is_empty()
        || view.composer.contains("[Pasted text #")
        || view.composer.contains("[Pasted Content ")
    {
        return false;
    }
    let footer = view.footer.to_ascii_lowercase();
    let words: Vec<&str> = footer.split_whitespace().collect();
    let context = words.windows(3).any(|words| {
        words[0].strip_suffix('%').is_some_and(|number| {
            let digits = |value: &str| {
                !value.is_empty() && value.bytes().all(|value| value.is_ascii_digit())
            };
            match number.split_once('.') {
                Some((whole, fraction)) => digits(whole) && digits(fraction),
                None => digits(number),
            }
        }) && words[1] == "context"
            && matches!(words[2], "left" | "remaining")
    });
    let path_footer = footer.contains('·')
        && words
            .iter()
            .any(|word| word.starts_with('/') || word.starts_with("~/"));
    if !path_footer && !footer.contains("? for shortcuts") && !context {
        return false;
    }
    let mut active_timer = false;
    for line in view.transcript.lines() {
        if codex_timer_row(line) {
            active_timer = true;
        } else if line.starts_with(['•', '◦', '■', '✗', '✓'])
            && ![
                "queued follow-up inputs",
                "messages to be submitted after next tool call",
                "messages to be submitted at end of turn",
            ]
            .iter()
            .any(|marker| line.to_ascii_lowercase().contains(marker))
        {
            active_timer = false;
        }
    }
    if active_timer {
        return false;
    }
    let rows = render_screen(screen);
    let Some(last) = rows
        .iter()
        .rposition(|(plain, _)| plain.iter().any(|character| !character.is_whitespace()))
    else {
        return false;
    };
    const REFUSALS: [&str; 22] = [
        "esc to interrupt",
        " to interrupt",
        "tab to queue message",
        "press up to edit queued messages",
        "edit last queued message",
        "queued follow-up inputs",
        "messages to be submitted after next tool call",
        "messages to be submitted at end of turn",
        "paste again to expand",
        "trust this folder?",
        "trust and continue",
        "do you trust the contents of this directory?",
        "approval required",
        "requires approval",
        "approve this",
        "would you like to run",
        "press enter to continue",
        "enter to confirm",
        "esc to cancel",
        "enter to submit answer",
        "enter to submit all",
        "allow command?",
    ];
    let active = &rows[(last + 1).saturating_sub(RUNNING_TURN_ROWS)..=last];
    if ["all results", "filesystem only", "plugins"]
        .iter()
        .all(|marker| {
            active
                .iter()
                .any(|(plain, _)| text_of(plain).to_ascii_lowercase().contains(marker))
        })
    {
        return false;
    }
    !active.iter().any(|(plain, _)| {
        let line = text_of(plain);
        if codex_timer_row(&line) {
            return false;
        }
        let line = line.to_ascii_lowercase();
        REFUSALS.iter().any(|refusal| line.contains(refusal))
    })
}

fn codex_timer_row(line: &str) -> bool {
    let line = line.trim_end();
    let line = if let Some(rest) = line.strip_prefix('•').or_else(|| line.strip_prefix('◦')) {
        if !rest.starts_with([' ', '\t']) {
            return false;
        }
        rest.trim_start_matches([' ', '\t'])
    } else {
        line
    };
    if line.chars().next().is_none_or(|first| {
        first.is_whitespace() || matches!(first, '›' | '•' | '◦' | '■' | '✗' | '✓' | '─')
    }) {
        return false;
    }
    line.match_indices(" (")
        .any(|(index, _)| codex_elapsed_suffix(&line[index + 2..]))
}

fn codex_elapsed_suffix(mut elapsed: &str) -> bool {
    loop {
        let digits = elapsed.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        elapsed = &elapsed[digits..];
        if let Some(rest) = elapsed
            .strip_prefix("h ")
            .or_else(|| elapsed.strip_prefix("m "))
        {
            elapsed = rest;
            continue;
        }
        let Some(rest) = elapsed.strip_prefix('s') else {
            return false;
        };
        let suffix_valid = |suffix: &str| {
            suffix.is_empty()
                || suffix
                    .strip_prefix(" · ")
                    .is_some_and(|text| !text.is_empty())
        };
        if let Some(suffix) = rest.strip_prefix(')') {
            return suffix_valid(suffix);
        }
        let Some(hint) = rest.strip_prefix(" • ") else {
            return false;
        };
        return hint
            .match_indices(" to interrupt)")
            .any(|(end, marker)| end > 0 && suffix_valid(&hint[end + marker.len()..]));
    }
}

/// Whether the bottom of `screen` shows that the agent is running a turn: `Some(true)` if it
/// does, `Some(false)` if it shows none, and `None` if it cannot tell.
///
/// Claude Code ends the status row under its input box with `esc to interrupt` while a turn
/// runs, and Codex prints the same words in the progress row above its composer. Both show a
/// queued-message hint only while a turn runs. Only the `RUNNING_TURN_ROWS` rows that end at
/// the last row with text are searched, so the same words higher up in the conversation do not
/// count. A status rule that looks for the input box cannot tell this: Claude Code keeps that
/// box on screen while it works, so such a rule can report a working pane as idle.
///
/// For a while after a paste, Claude Code draws `PASTE_HINT` in place of its status row, whether
/// or not a turn runs. A screen whose last row with text is that hint, and which shows no marker,
/// gives `None`.
pub(crate) fn running_turn(screen: &str) -> Option<bool> {
    let rows = render_screen(screen);
    let Some(last) = rows
        .iter()
        .rposition(|(plain, _)| plain.iter().any(|character| !character.is_whitespace()))
    else {
        return Some(false);
    };
    let marked = rows[(last + 1).saturating_sub(RUNNING_TURN_ROWS)..=last]
        .iter()
        .any(|(plain, _)| {
            let row = text_of(plain);
            row.contains(WORKING_MARKER) || QUEUE_MARKERS.iter().any(|marker| row.contains(marker))
        });
    if !marked && text_of(&rows[last].0).contains(PASTE_HINT) {
        return None;
    }
    Some(marked)
}

/// The queued-message marker among the rows `running_turn` searches, if `screen` shows one.
fn queue_marker(screen: &str) -> Option<&'static str> {
    let rows = render_screen(screen);
    let last = rows
        .iter()
        .rposition(|(plain, _)| plain.iter().any(|character| !character.is_whitespace()))?;
    rows[(last + 1).saturating_sub(RUNNING_TURN_ROWS)..=last]
        .iter()
        .find_map(|(plain, _)| {
            let row = text_of(plain);
            QUEUE_MARKERS
                .iter()
                .copied()
                .find(|marker| row.contains(marker))
        })
}

/// Whether the `rows` rows that end at the last row with text show a queued-message marker, and
/// whether they show any marker of a running turn.
fn markers_within(screen: &str, rows: usize) -> (bool, bool) {
    let rendered = render_screen(screen);
    let Some(last) = rendered
        .iter()
        .rposition(|(plain, _)| plain.iter().any(|character| !character.is_whitespace()))
    else {
        return (false, false);
    };
    let mut queued = false;
    let mut running = false;
    for (plain, _) in &rendered[(last + 1).saturating_sub(rows)..=last] {
        let row = text_of(plain);
        let queue = QUEUE_MARKERS.iter().any(|marker| row.contains(marker));
        queued |= queue;
        running |= queue || row.contains(WORKING_MARKER);
    }
    (queued, running)
}

/// What the screen reads before the submission key took effect showed: their markers, the most
/// copies of the whole prompt above the composer, and, for each paste placeholder the composer
/// showed for this prompt, the most copies of it above the composer.
#[derive(Clone, Debug, Default)]
struct ShownBefore {
    queue_marker: bool,
    running_turn: bool,
    /// The prompt without its blanks.
    prompt: String,
    prompt_copies: usize,
    /// Each numbered placeholder label the composer showed for this paste, without its blanks,
    /// such as `[Pastedtext#3+12lines]`, and its most copies above the composer.
    labels: Vec<(String, usize)>,
    /// The text above the composer before the paste, without its blanks.
    before_paste: String,
}

impl ShownBefore {
    fn new(text: &str) -> Self {
        Self {
            prompt: compact(text),
            ..Self::default()
        }
    }

    /// Note the screen read before the paste.
    fn note_before_paste(&mut self, view: &ComposerView) {
        self.before_paste = compact(&view.transcript);
        self.prompt_copies = self.before_paste.matches(self.prompt.as_str()).count();
    }

    /// Note what a screen read after the paste and before the key took effect shows. A tall
    /// composer can push a marker above the rows `running_turn` searches, and the marker comes
    /// back into them once the prompt leaves the composer, so the rows the composer takes are
    /// searched as well. The composer was empty before the paste, so a numbered placeholder it
    /// shows stands for this prompt, unless the prompt's own text holds that label.
    fn note(&mut self, screen: &str, view: Option<&ComposerView>) {
        let composer_rows = view.map_or(0, |view| view.composer.lines().count());
        let (queued, running) = markers_within(screen, RUNNING_TURN_ROWS + composer_rows);
        self.queue_marker |= queued || queue_marker(screen).is_some();
        self.running_turn |= running || running_turn(screen) == Some(true);
        let Some(view) = view else {
            return;
        };
        for label in placeholder_labels(&view.composer) {
            if !self.prompt.contains(label.as_str())
                && !self.labels.iter().any(|(known, _)| *known == label)
            {
                let copies = self.before_paste.matches(label.as_str()).count();
                self.labels.push((label, copies));
            }
        }
        let transcript = compact(&view.transcript);
        self.prompt_copies = self
            .prompt_copies
            .max(transcript.matches(self.prompt.as_str()).count());
        for (label, copies) in &mut self.labels {
            *copies = (*copies).max(transcript.matches(label.as_str()).count());
        }
    }
}

/// The numbered paste placeholders in `text`, `[Pasted text #N ...]`, each whole and without its
/// blanks, so that `#12` is not found inside `#123`. Claude Code numbers each paste, so the label
/// names one paste. `[Pasted Content N chars]` names only a length that another paste can share,
/// so it is not returned.
fn placeholder_labels(text: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = text;
    while let Some(position) = rest.find("[Pasted ") {
        let tail = &rest[position..];
        if let Some(close) = tail.char_indices().take(80).find(|(_, c)| *c == ']') {
            let label = &tail[..=close.0];
            if label.starts_with("[Pasted text #") && paste_placeholders(label) == 1 {
                labels.push(compact(label));
            }
        }
        rest = &tail["[Pasted ".len()..];
    }
    labels
}

pub(crate) fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

pub(crate) fn suffix(text: &str, length: usize) -> String {
    let chars: Vec<char> = compact(text).chars().collect();
    chars[chars.len().saturating_sub(length)..].iter().collect()
}

/// Count `[Pasted text #N` and `[Pasted Content N chars` placeholders.
fn paste_placeholders(text: &str) -> usize {
    let mut count = 0;
    let mut rest = text;
    while let Some(position) = rest.find("[Pasted ") {
        let tail = &rest[position + "[Pasted ".len()..];
        let digits = |value: &str| value.chars().take_while(char::is_ascii_digit).count();
        if let Some(after) = tail.strip_prefix("text #") {
            if digits(after) > 0 {
                count += 1;
            }
        } else if let Some(after) = tail.strip_prefix("Content ") {
            let length = digits(after);
            if length > 0 && after[length..].starts_with(" chars") {
                count += 1;
            }
        }
        rest = tail;
    }
    count
}

fn staged(view: &ComposerView, text: &str, placeholders_before: usize) -> bool {
    // Faint placeholder text is excluded so a short prompt is not "found" inside it.
    let composer = compact(&view.composer_solid);
    let wanted = compact(text);
    if wanted.is_empty() {
        return false;
    }
    composer.contains(&wanted)
        || composer.contains(&suffix(text, 80))
        || paste_placeholders(&view.composer) > placeholders_before
}

fn transcript_count(view: &ComposerView, text: &str) -> usize {
    compact(&view.transcript).matches(&suffix(text, 40)).count()
}

fn corroborated(before: &ComposerView, after: &ComposerView, text: &str) -> Option<String> {
    if transcript_count(after, text) > transcript_count(before, text) {
        return Some("prompt text appeared above the composer".to_owned());
    }
    if paste_placeholders(&after.transcript) > paste_placeholders(&before.transcript) {
        return Some("pasted prompt appeared above the composer".to_owned());
    }
    let screen = format!("{}\n{}", after.transcript, after.footer);
    if let Some(marker) = QUEUE_MARKERS.iter().find(|marker| screen.contains(*marker)) {
        return Some(format!("agent queue marker is visible ({marker:?})"));
    }
    if screen.contains(WORKING_MARKER) {
        return Some("agent reports an active turn".to_owned());
    }
    None
}

/// Describe what the pane printed after the submission key, if `after` shows any of it: more
/// copies above the composer of the whole prompt, or of a numbered placeholder the composer
/// showed for it, than any earlier read showed, or a marker that no earlier read showed. Each of
/// these names this prompt: another prompt's copy, even one built from the same template, is not
/// the whole of this one, and another paste has another number. `shown` says what the earlier
/// reads showed.
fn printed(after: &ComposerView, after_screen: &str, shown: &ShownBefore) -> Option<String> {
    let transcript = compact(&after.transcript);
    if transcript.matches(shown.prompt.as_str()).count() > shown.prompt_copies {
        return Some("prompt text appeared above the composer".to_owned());
    }
    if shown
        .labels
        .iter()
        .any(|(label, copies)| transcript.matches(label.as_str()).count() > *copies)
    {
        return Some("pasted prompt appeared above the composer".to_owned());
    }
    if !shown.queue_marker {
        if let Some(marker) = queue_marker(after_screen) {
            return Some(format!("agent queue marker appeared ({marker:?})"));
        }
    }
    if !shown.running_turn && running_turn(after_screen) == Some(true) {
        return Some("agent started reporting an active turn".to_owned());
    }
    None
}

fn submit_key(harness: &str, view: &ComposerView) -> &'static str {
    // A busy Codex steers the active turn on Enter and queues on Tab. It shows
    // the Tab hint only while it is busy and holds staged text.
    if harness == "codex" && view.footer.contains(CODEX_QUEUE_HINT) {
        "Tab"
    } else {
        "Enter"
    }
}

/// Stage `text` in an empty composer, submit it, and prove that it left the composer.
///
/// The submission key is repeated with a doubling wait while the exact text is
/// still staged, so a key the agent dropped is retried without typing the
/// prompt twice. A key is never repeated once the text has left the composer.
///
/// When only a marker that was already showing proves the submission, the screen is read for
/// up to [`PRINT_GRACE`] more for printed evidence, and the receipt says which kind it got.
pub fn submit_verified(
    terminal: &dyn PromptTerminal,
    pane_id: &str,
    harness: &str,
    text: &str,
    timeouts: SubmitTimeouts,
    runtime: &dyn AgentRuntime,
) -> Result<Submission> {
    let refuse = |reason: String| Ok(Submission::NotStaged(reason));
    if !VERIFIED_HARNESSES.contains(&harness) {
        return refuse(format!(
            "no composer model for harness {harness:?}; nothing was typed"
        ));
    }
    if text.trim().is_empty() {
        return refuse("prompt text is empty; nothing was typed".to_owned());
    }
    if text.contains(['\u{1b}', '\0']) {
        return refuse(
            "prompt text contains NUL or terminal escape characters; nothing was typed".to_owned(),
        );
    }
    if runtime.cancelled() {
        return refuse(format!(
            "pane {pane_id}: delivery was cancelled before typing"
        ));
    }
    let composer_deadline = runtime.monotonic() + timeouts.composer;
    let (screen, before) = loop {
        let screen = match terminal.read_screen(pane_id) {
            Ok(screen) => screen,
            Err(error) => {
                return refuse(format!(
                    "pane {pane_id}: composer read failed before typing: {error}"
                ))
            }
        };
        if let Some(before) = composer_view(harness, &screen) {
            break (screen, before);
        }
        if runtime.monotonic() >= composer_deadline || runtime.cancelled() {
            return refuse(format!(
                "pane {pane_id} does not show a recognisable {harness} composer after {}s \
                 (a dialog or menu may be open); nothing was typed",
                timeouts.composer.as_secs_f64()
            ));
        }
        runtime.sleep(POLL);
    };
    let mut shown = ShownBefore {
        queue_marker: queue_marker(&screen).is_some(),
        // A screen that cannot tell, such as one that shows the paste hint in place of the status
        // row, may be hiding a running turn.
        running_turn: running_turn(&screen) != Some(false),
        ..ShownBefore::new(text)
    };
    shown.note_before_paste(&before);
    let draft = before.composer_solid.trim();
    if !draft.is_empty() {
        let preview: String = draft
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(120)
            .collect();
        return refuse(format!(
            "pane {pane_id} composer already holds unsubmitted text {preview:?}; \
             refusing to append to it; nothing was typed"
        ));
    }
    let placeholders_before = paste_placeholders(&before.composer);
    terminal.send_text(
        pane_id,
        &format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}"),
    )?;

    let cancelled_after_typing = || {
        Err(AdapterError::unavailable(format!(
            "pane {pane_id}: delivery was cancelled after the prompt was pasted; outcome is unknown"
        )))
    };
    let started = runtime.monotonic();
    let stage_deadline = started.saturating_add(timeouts.stage);
    let view = loop {
        if runtime.cancelled() {
            return cancelled_after_typing();
        }
        let screen = terminal.read_screen(pane_id)?;
        let view = composer_view(harness, &screen);
        shown.note(&screen, view.as_ref());
        if let Some(view) = view {
            if staged(&view, text, placeholders_before) {
                break view;
            }
        }
        if runtime.monotonic() >= stage_deadline {
            return Err(AdapterError::unavailable(format!(
                "pane {pane_id}: pasted prompt was not observed in the {harness} composer within \
                 {:.1}s; no submission key was sent",
                timeouts.stage.as_secs_f64()
            )));
        }
        runtime.sleep(POLL);
    };

    let mut key = submit_key(harness, &view);
    terminal.send_keys(pane_id, key)?;
    let mut presses: u32 = 1;
    let mut wait = FIRST_RETRY;
    let mut next_press = runtime.monotonic().saturating_add(wait);
    let submit_deadline = runtime.monotonic().saturating_add(timeouts.submit);
    let mut left_composer = false;
    // A read taken while waiting for printed evidence that showed the prompt staged again. It is
    // handled next as if this loop had just read it, so its bookkeeping, the deadline and the key
    // schedule all apply to it.
    let mut carried: Option<String> = None;
    'submitting: loop {
        let screen = match carried.take() {
            Some(screen) => {
                if runtime.cancelled() {
                    return cancelled_after_typing();
                }
                screen
            }
            None => {
                runtime.sleep(POLL);
                if runtime.cancelled() {
                    return cancelled_after_typing();
                }
                terminal.read_screen(pane_id)?
            }
        };
        let now = runtime.monotonic();
        let current = composer_view(harness, &screen);
        match current {
            Some(current) if staged(&current, text, placeholders_before) => {
                shown.note(&screen, Some(&current));
                left_composer = false;
                if now >= submit_deadline && runtime.withdraw_unaccepted_prompt() {
                    return withdraw_unaccepted(
                        terminal, pane_id, harness, &shown, presses, key, started, runtime,
                    );
                }
                if now >= submit_deadline {
                    return Err(AdapterError::unavailable(format!(
                        "pane {pane_id}: prompt is still staged in the {harness} composer after \
                         {presses} {key} key press(es) over {:.1}s; it was NOT submitted and is \
                         left visible so it is never typed twice",
                        now.saturating_sub(started).as_secs_f64()
                    )));
                }
                if now >= next_press {
                    key = submit_key(harness, &current);
                    terminal.send_keys(pane_id, key)?;
                    presses += 1;
                    wait = (wait * 2).min(MAX_RETRY);
                    next_press = now.saturating_add(wait);
                }
                continue;
            }
            Some(current) => {
                left_composer = true;
                if let Some(evidence) = corroborated(&before, &current, text) {
                    let mut receipt = SubmissionReceipt {
                        key,
                        key_presses: presses,
                        elapsed: now.saturating_sub(started),
                        evidence,
                        printed: false,
                        settled: false,
                    };
                    // No key is sent from here on: the prompt has left the composer.
                    let grace_deadline = now.saturating_add(PRINT_GRACE);
                    let mut observed = Some((current, screen, now));
                    // Whether the last read showed a recognisable composer without the prompt.
                    let mut clear = true;
                    loop {
                        if let Some((view, screen, read_at)) = &observed {
                            if let Some(evidence) = printed(view, screen, &shown) {
                                receipt.elapsed = read_at.saturating_sub(started);
                                receipt.evidence = evidence;
                                receipt.printed = true;
                                receipt.settled = true;
                                break;
                            }
                        }
                        // A wait that has run its course is complete even if a stop arrives with
                        // its last read; only a stop before that cuts it short.
                        if runtime.monotonic() >= grace_deadline {
                            receipt.settled = clear;
                            break;
                        }
                        if runtime.cancelled() {
                            break;
                        }
                        runtime.sleep(POLL);
                        let Ok(screen) = terminal.read_screen(pane_id) else {
                            break;
                        };
                        let read_at = runtime.monotonic();
                        let view = composer_view(harness, &screen);
                        // The prompt is staged again: the empty composer that seemed to prove the
                        // submission was a transient redraw, and the key may have been dropped.
                        // Without printed evidence nothing is proved, so this read goes back to
                        // the staged path, which notes what it shows, keeps the deadline, and
                        // presses the key again on its schedule.
                        if view
                            .as_ref()
                            .is_some_and(|view| staged(view, text, placeholders_before))
                        {
                            carried = Some(screen);
                            continue 'submitting;
                        }
                        clear = view.is_some();
                        observed = view.map(|view| (view, screen, read_at));
                    }
                    return Ok(Submission::Verified(receipt));
                }
            }
            None => {}
        }
        if now >= submit_deadline {
            let state = if left_composer {
                "left the composer without visible submission evidence"
            } else {
                "is no longer in a recognisable composer"
            };
            return Err(AdapterError::unavailable(format!(
                "pane {pane_id}: prompt {state} after {presses} {key} key press(es); outcome is unknown"
            )));
        }
    }
}

/// Keys that move to the end of the composer's current row and delete back to its start. Each
/// pair removes at most one row; a pair sent to a row that is already empty removes the line
/// break before it in Codex, and the screen is read after each pair to see what it did.
pub const CLEAR_ROW_KEYS: [&str; 2] = ["ctrl+e", "ctrl+u"];
/// The key [`clear_composer`] sends when a pair of [`CLEAR_ROW_KEYS`] changed nothing, as on an
/// empty row whose line break the row keys do not remove.
pub const CLEAR_CHARACTER_KEY: &str = "backspace";
/// Most pairs of [`CLEAR_ROW_KEYS`] one [`clear_composer`] sends.
pub const MAX_CLEAR_ROUNDS: usize = 64;

/// What a composer holds, as [`clear_composer`] compares it between reads: its rows as drawn,
/// each without trailing blanks, so an empty last row still counts as a row.
fn composer_content(view: &ComposerView) -> String {
    view.composer
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `view` shows an empty composer: no solid text and no paste placeholder. Faint
/// placeholder and hint text, such as Claude Code's `Press up to edit queued messages`, and the
/// reverse-video cursor cell do not count.
#[must_use]
pub fn composer_is_empty(view: &ComposerView) -> bool {
    view.composer_solid.trim().is_empty() && paste_placeholders(&view.composer) == 0
}

/// Delete everything in the composer of `harness` in `pane_id` without submitting it, and prove
/// from the screen that it is empty.
///
/// Sends the [`CLEAR_ROW_KEYS`] pair, reads the screen, and repeats while the composer still
/// holds text and the last pair changed it, for at most [`MAX_CLEAR_ROUNDS`] pairs. A pair that
/// changed nothing is followed by one [`CLEAR_CHARACTER_KEY`]; if that changes nothing either,
/// the clear fails. It also fails when the composer stops being recognisable, as when a dialog
/// opens, or when the caller cancels. No key that submits, interrupts or opens a menu is sent.
/// Returns the number of keys sent; an `Err` says what the composer was left holding, which may
/// be less than it held before.
pub fn clear_composer(
    terminal: &dyn PromptTerminal,
    pane_id: &str,
    harness: &str,
    runtime: &dyn AgentRuntime,
) -> std::result::Result<u32, String> {
    let read = || -> std::result::Result<ComposerView, String> {
        let screen = terminal
            .read_screen(pane_id)
            .map_err(|error| format!("pane {pane_id}: composer read failed: {error}"))?;
        composer_view(harness, &screen).ok_or_else(|| {
            format!("pane {pane_id} no longer shows a recognisable {harness} composer")
        })
    };
    let send = |key: &str| -> std::result::Result<(), String> {
        if runtime.cancelled() {
            return Err(format!("pane {pane_id}: composer clear was cancelled"));
        }
        terminal
            .send_keys(pane_id, key)
            .map_err(|error| format!("pane {pane_id}: key {key} failed: {error}"))
    };
    let mut view = read()?;
    let mut keys = 0_u32;
    for _ in 0..MAX_CLEAR_ROUNDS {
        if composer_is_empty(&view) {
            return Ok(keys);
        }
        let before = composer_content(&view);
        for key in CLEAR_ROW_KEYS {
            send(key)?;
            keys += 1;
        }
        runtime.sleep(POLL);
        view = read()?;
        if composer_is_empty(&view) {
            return Ok(keys);
        }
        if composer_content(&view) == before {
            send(CLEAR_CHARACTER_KEY)?;
            keys += 1;
            runtime.sleep(POLL);
            view = read()?;
            if !composer_is_empty(&view) && composer_content(&view) == before {
                return Err(format!(
                    "pane {pane_id}: the {harness} composer did not change after {keys} clearing \
                     key(s); it still holds text"
                ));
            }
        }
    }
    if composer_is_empty(&view) {
        return Ok(keys);
    }
    Err(format!(
        "pane {pane_id}: the {harness} composer still holds text after {keys} clearing key(s)"
    ))
}

/// Take a prompt that the agent never accepted back out of its composer, for
/// [`AgentRuntime::withdraw_unaccepted_prompt`]. Nothing was submitted when the composer is
/// proved empty and the screen shows nothing the pane printed after the submission key: the
/// prompt is then reported as not typed, so it stays pending. Any other outcome is an error,
/// which the caller treats as possibly submitted, as it treats a prompt left staged.
#[allow(clippy::too_many_arguments)]
fn withdraw_unaccepted(
    terminal: &dyn PromptTerminal,
    pane_id: &str,
    harness: &str,
    shown: &ShownBefore,
    presses: u32,
    key: &str,
    started: Duration,
    runtime: &dyn AgentRuntime,
) -> Result<Submission> {
    let elapsed = runtime.monotonic().saturating_sub(started).as_secs_f64();
    let left = |detail: String| {
        Err(AdapterError::unavailable(format!(
            "pane {pane_id}: prompt is still staged in the {harness} composer after {presses} \
             {key} key press(es) over {elapsed:.1}s; taking it back out failed, so it is left \
             there and treated as possibly submitted: {detail}"
        )))
    };
    if let Err(detail) = clear_composer(terminal, pane_id, harness, runtime) {
        return left(detail);
    }
    let screen = match terminal.read_screen(pane_id) {
        Ok(screen) => screen,
        Err(error) => return left(format!("the screen read after the clear failed: {error}")),
    };
    let Some(view) = composer_view(harness, &screen) else {
        return left(format!(
            "pane {pane_id} no longer shows a recognisable {harness} composer"
        ));
    };
    if !composer_is_empty(&view) {
        return left("the composer is not empty after the clear".to_owned());
    }
    if let Some(evidence) = printed(&view, &screen, shown) {
        return Err(AdapterError::unavailable(format!(
            "pane {pane_id}: the prompt may have been accepted while it was taken back out of the \
             {harness} composer ({evidence}); outcome is unknown"
        )));
    }
    Ok(Submission::NotStaged(format!(
        "pane {pane_id}: the {harness} agent did not accept the prompt after {presses} {key} key \
         press(es) over {elapsed:.1}s; it was taken back out of the composer, which is empty \
         again, so nothing was submitted"
    )))
}

/// Scriptable agent composer shared by the submission and queue tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::{PromptTerminal, BRACKETED_PASTE_END, BRACKETED_PASTE_START};
    use crate::error::Result;

    const RULE: &str = "────────────────────────────────────────";

    fn dim(text: &str) -> String {
        format!("\u{1b}[2m{text}\u{1b}[22m")
    }

    /// Observable and controllable state of one fake agent pane.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct State {
        pub harness: &'static str,
        pub busy: bool,
        pub composer: String,
        pub transcript: Vec<String>,
        pub queued: Vec<String>,
        pub submitted: Vec<String>,
        pub steered: Vec<String>,
        pub pastes: Vec<String>,
        pub keys: Vec<String>,
        pub drop_keys: u32,
        /// Reads that still render the composer empty after a paste; `u32::MAX` never shows it.
        pub paste_visible_after_reads: u32,
        pub redraw_lag_reads: u32,
        pub dialog: bool,
        /// Reads that still show the dialog before the composer is drawn, as a freshly
        /// launched harness does; the dialog closes on the last of them.
        pub dialog_reads: u32,
        /// Label Claude draws into the composer's top border, such as a mode name.
        pub top_border_label: Option<String>,
        /// Glyph Codex draws before its composer: `›` before v0.159.1, `»` from it.
        pub codex_marker: &'static str,
        /// A busy Claude queues a prompt without showing any of it above the composer, as it does
        /// for a prompt queued behind another that it already holds.
        pub queued_hidden: bool,
        /// The clearing keys `ctrl+u` and `backspace` change nothing, as in a composer that
        /// binds them to something else.
        pub ignore_clear_keys: bool,
        /// Every submission key, Enter or Tab, is dropped; editing keys still work.
        pub drop_submit_keys: bool,
        stale_screen: Option<String>,
        pending_reads: u32,
        paste_counter: u32,
        placeholder: Option<String>,
    }

    /// A terminal agent composer with a controllable key drop and redraw lag.
    #[derive(Debug)]
    pub(crate) struct FakeScreen(pub Mutex<State>);

    impl FakeScreen {
        pub(crate) fn new(harness: &'static str, busy: bool) -> Self {
            Self(Mutex::new(State {
                harness,
                busy,
                transcript: vec!["• earlier output".to_owned()],
                codex_marker: "›",
                ..State::default()
            }))
        }

        pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().expect("fake screen state")
        }

        pub(crate) fn render(&self) -> String {
            render(&self.state(), false)
        }
    }

    fn render(state: &State, hide_composer: bool) -> String {
        let composer = if hide_composer {
            ""
        } else {
            state.composer.as_str()
        };
        let mut lines = state.transcript.clone();
        if state.dialog {
            lines.extend(["Replace goal?", "  › 1. Replace", "  2. Cancel"].map(str::to_owned));
            return lines.join("\n");
        }
        let composer_lines = |marker: &str, lines: &mut Vec<String>| {
            let mut rows = composer.split('\n');
            lines.push(format!("{marker} {}", rows.next().unwrap_or("")));
            lines.extend(rows.map(|row| format!("  {row}")));
        };
        if state.harness == "claude" {
            if state.busy && !lines.iter().any(|line| line.contains("Working")) {
                lines.push("✻ Working… (running PreToolUse hooks…)".to_owned());
            }
            lines.push(match &state.top_border_label {
                // Coloured as Claude draws it: grey rule, violet label, grey tail.
                Some(label) => format!(
                    "\u{1b}[38;5;244m{RULE}\u{1b}[0m \u{1b}[38;5;147m{label} \u{1b}[38;5;244m─\u{1b}[0m"
                ),
                None => RULE.to_owned(),
            });
            match (&state.placeholder, composer.is_empty()) {
                (Some(placeholder), false) => lines.push(format!("❯ {placeholder}")),
                (None, false) => composer_lines("❯", &mut lines),
                (_, true) if !state.queued.is_empty() => lines.push(format!(
                    "❯ \u{1b}[7mP\u{1b}[0m{}",
                    dim("ress up to edit queued messages")
                )),
                (_, true) => lines.push("❯\u{a0}".to_owned()),
            }
            lines.push(RULE.to_owned());
            let mut footer = "  ⏵⏵ auto mode on".to_owned();
            if state.busy {
                footer.push_str(" · esc to interrupt");
            }
            lines.push(footer);
        } else {
            if composer.is_empty() {
                lines.push(format!(
                    "{} {}",
                    state.codex_marker,
                    dim("Ask Codex to do anything")
                ));
            } else {
                composer_lines(state.codex_marker, &mut lines);
            }
            lines.push(String::new());
            lines.push(if state.busy && !composer.is_empty() {
                "  tab to queue message                 99% context".to_owned()
            } else {
                "  GPT default · /tmp/project".to_owned()
            });
        }
        lines.join("\n") + "\n"
    }

    impl PromptTerminal for FakeScreen {
        fn read_screen(&self, _pane_id: &str) -> Result<String> {
            let mut state = self.state();
            if state.dialog_reads > 0 {
                state.dialog_reads -= 1;
                state.dialog = state.dialog_reads > 0;
            }
            if state.pending_reads > 0 {
                if let Some(stale) = state.stale_screen.clone() {
                    state.pending_reads -= 1;
                    return Ok(stale);
                }
            }
            state.stale_screen = None;
            if state.paste_visible_after_reads > 0 {
                if state.paste_visible_after_reads != u32::MAX {
                    state.paste_visible_after_reads -= 1;
                }
                return Ok(render(&state, true));
            }
            Ok(render(&state, false))
        }

        fn send_text(&self, _pane_id: &str, text: &str) -> Result<()> {
            let body = text
                .strip_prefix(BRACKETED_PASTE_START)
                .and_then(|rest| rest.strip_suffix(BRACKETED_PASTE_END))
                .expect("prompt text is sent as one bracketed paste");
            let mut state = self.state();
            state.pastes.push(body.to_owned());
            state.composer.push_str(body);
            let newlines = body.matches('\n').count();
            if state.harness == "claude" && newlines >= 3 {
                state.paste_counter += 1;
                state.placeholder = Some(format!(
                    "[Pasted text #{} +{newlines} lines]",
                    state.paste_counter
                ));
            }
            Ok(())
        }

        fn send_keys(&self, _pane_id: &str, keys: &str) -> Result<()> {
            let mut state = self.state();
            state.keys.push(keys.to_owned());
            if state.drop_keys > 0 {
                state.drop_keys -= 1;
                return Ok(());
            }
            if state.drop_submit_keys && matches!(keys, "Enter" | "Tab") {
                return Ok(());
            }
            let before = render(&state, false);
            let text = state.composer.clone();
            if text.is_empty() {
                return Ok(());
            }
            // Editing keys, modelled with the cursor at the end of the composer: `ctrl+e` moves
            // nothing, `ctrl+u` deletes the last row, or on an empty last row the line break
            // before it in Codex only, and `backspace` deletes one character. A paste
            // placeholder goes as a whole.
            match keys {
                "ctrl+e" => return Ok(()),
                "ctrl+u" | "backspace" if state.ignore_clear_keys => return Ok(()),
                "ctrl+u" | "backspace" if state.placeholder.is_some() => {
                    state.composer.clear();
                    state.placeholder = None;
                    return Ok(());
                }
                "ctrl+u" => {
                    let start = text.rfind('\n').map_or(0, |index| index + 1);
                    if start < text.len() {
                        state.composer.truncate(start);
                    } else if state.harness == "codex" {
                        state.composer.truncate(start - 1);
                    }
                    return Ok(());
                }
                "backspace" => {
                    state.composer.pop();
                    return Ok(());
                }
                _ => {}
            }
            match (state.harness, keys) {
                ("claude", "Enter") => {
                    if !(state.busy && state.queued_hidden) {
                        state.transcript.push(format!("❯ {text}"));
                    }
                    if state.busy {
                        state.queued.push(text);
                    } else {
                        state.submitted.push(text);
                        state
                            .transcript
                            .push("✻ Working… (running UserPromptSubmit hooks…)".to_owned());
                    }
                    state.busy = true;
                }
                ("codex", "Enter") => {
                    state.transcript.push(format!("› {text}"));
                    if state.busy {
                        state.steered.push(text);
                    } else {
                        state.submitted.push(text);
                        state
                            .transcript
                            .push("• Working (0s • esc to interrupt)".to_owned());
                    }
                    state.busy = true;
                }
                ("codex", "Tab") if state.busy => {
                    state.queued.push(text.clone());
                    state
                        .transcript
                        .push("• Queued follow-up inputs".to_owned());
                    state.transcript.push(format!("  ↳ {text}"));
                }
                _ => return Ok(()),
            }
            state.composer.clear();
            state.placeholder = None;
            if state.redraw_lag_reads > 0 {
                state.stale_screen = Some(before);
                state.pending_reads = state.redraw_lag_reads;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::fake::FakeScreen;
    use super::*;

    #[derive(Default)]
    struct Clock {
        millis: AtomicU64,
        cancel: AtomicBool,
        withdraw: AtomicBool,
    }

    impl AgentRuntime for Clock {
        fn monotonic(&self) -> Duration {
            Duration::from_millis(1_000_000 + self.millis.load(Ordering::SeqCst))
        }

        fn sleep(&self, duration: Duration) {
            self.millis.fetch_add(
                u64::try_from(duration.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
        }

        fn cancelled(&self) -> bool {
            self.cancel.load(Ordering::SeqCst)
        }

        fn withdraw_unaccepted_prompt(&self) -> bool {
            self.withdraw.load(Ordering::SeqCst)
        }
    }

    fn submit_with(
        screen: &FakeScreen,
        text: &str,
        timeouts: SubmitTimeouts,
    ) -> (Result<Submission>, Duration) {
        let clock = Clock::default();
        let harness = screen.state().harness;
        let outcome = submit_verified(screen, "w1:p1", harness, text, timeouts, &clock);
        (
            outcome,
            clock.monotonic() - Duration::from_millis(1_000_000),
        )
    }

    fn submit(screen: &FakeScreen, text: &str) -> Result<Submission> {
        submit_with(screen, text, SubmitTimeouts::default()).0
    }

    fn receipt(outcome: Result<Submission>) -> SubmissionReceipt {
        match outcome {
            Ok(Submission::Verified(receipt)) => receipt,
            other => panic!("expected a verified submission, got {other:?}"),
        }
    }

    fn refusal(outcome: Result<Submission>) -> String {
        match outcome {
            Ok(Submission::NotStaged(reason)) => reason,
            other => panic!("expected a refusal before typing, got {other:?}"),
        }
    }

    #[test]
    fn screen_parser_separates_placeholder_from_draft() {
        let claude = FakeScreen::new("claude", false);
        let view = composer_view("claude", &claude.render()).unwrap();
        assert_eq!(view.composer_solid.trim(), "");
        claude.state().queued.push("x".to_owned());
        let view = composer_view("claude", &claude.render()).unwrap();
        assert_eq!(view.composer_solid.trim(), "");
        assert!(view.composer.contains("ress up") && !view.composer_solid.contains("ress up"));
        claude.state().composer = "draft".to_owned();
        let view = composer_view("claude", &claude.render()).unwrap();
        assert_eq!(view.composer_solid.trim(), "draft");

        let codex = FakeScreen::new("codex", false);
        let view = composer_view("codex", &codex.render()).unwrap();
        assert_eq!(view.composer_solid.trim(), "");
        assert!(view.composer.contains("Ask Codex"));
        codex.state().composer = "draft".to_owned();
        let view = composer_view("codex", &codex.render()).unwrap();
        assert_eq!(view.composer_solid.trim(), "draft");
    }

    #[test]
    fn extended_colour_operands_are_not_attributes() {
        // 38;2;2;7;2 is an RGB colour whose operands spell the faint and reverse codes.
        let rows = render_screen("\u{1b}[38;2;2;7;2mA\u{1b}[48;5;2mB\u{1b}[2mC\u{1b}[0mD");
        assert_eq!(text_of(&rows[0].0), "ABCD");
        assert_eq!(text_of(&rows[0].1), "AB D");
    }

    #[test]
    fn slash_commands_and_unknown_harnesses_keep_the_native_path() {
        assert!(verifies("claude", "hello"));
        assert!(verifies("codex", "hello"));
        assert!(!verifies("codex", "  /goal ship it"));
        assert!(!verifies("custom", "hello"));
    }

    #[test]
    fn dropped_submit_key_is_retried_without_retyping() {
        for harness in ["claude", "codex"] {
            let screen = FakeScreen::new(harness, false);
            screen.state().drop_keys = 2;
            let receipt = receipt(submit(&screen, "run the tests"));
            assert_eq!(receipt.key, "Enter");
            assert_eq!(receipt.key_presses, 3, "{harness}");
            let state = screen.state();
            assert_eq!(state.pastes, ["run the tests"], "{harness}");
            assert_eq!(state.keys, ["Enter", "Enter", "Enter"], "{harness}");
            assert_eq!(state.submitted, ["run the tests"], "{harness}");
            assert!(state.composer.is_empty());
        }
    }

    #[test]
    fn already_accepted_prompt_gets_exactly_one_key_despite_redraw_lag() {
        for harness in ["claude", "codex"] {
            let screen = FakeScreen::new(harness, false);
            screen.state().redraw_lag_reads = 3;
            let receipt = receipt(submit(&screen, "only once please"));
            assert_eq!(receipt.key_presses, 1, "{harness}");
            let state = screen.state();
            assert_eq!(state.keys, ["Enter"], "{harness}");
            assert_eq!(state.submitted, ["only once please"], "{harness}");
        }
    }

    #[test]
    fn late_redraw_retry_lands_on_an_empty_composer_without_a_duplicate() {
        // The stale screen outlives the first retry interval, so a second key is
        // sent; it reaches an empty composer and cannot submit anything twice.
        let screen = FakeScreen::new("claude", false);
        screen.state().redraw_lag_reads = 8;
        let receipt = receipt(submit(&screen, "late redraw"));
        assert_eq!(receipt.key_presses, 2);
        let state = screen.state();
        assert_eq!(state.submitted, ["late redraw"]);
        assert_eq!(state.pastes.len(), 1);
        assert_eq!(
            state
                .transcript
                .iter()
                .filter(|line| line.contains("late redraw"))
                .count(),
            1
        );
    }

    #[test]
    fn busy_claude_queues_with_enter() {
        let screen = FakeScreen::new("claude", true);
        let receipt = receipt(submit(&screen, "after this turn"));
        assert_eq!(receipt.key, "Enter");
        assert_eq!(screen.state().queued, ["after this turn"]);
    }

    #[test]
    fn busy_codex_queues_with_tab_and_never_steers() {
        let screen = FakeScreen::new("codex", true);
        screen.state().drop_keys = 1;
        let receipt = receipt(submit(&screen, "follow up later"));
        assert_eq!(receipt.key, "Tab");
        let state = screen.state();
        assert_eq!(state.keys, ["Tab", "Tab"]);
        assert_eq!(state.queued, ["follow up later"]);
        assert!(state.steered.is_empty());
    }

    #[test]
    fn idle_codex_submits_with_enter() {
        let screen = FakeScreen::new("codex", false);
        let receipt = receipt(submit(&screen, "start now"));
        assert_eq!(receipt.key, "Enter");
        assert_eq!(screen.state().submitted, ["start now"]);
    }

    #[test]
    fn prompt_left_staged_times_out_as_not_submitted() {
        let screen = FakeScreen::new("claude", false);
        screen.state().drop_keys = u32::MAX;
        let (outcome, elapsed) = submit_with(
            &screen,
            "never accepted",
            SubmitTimeouts {
                stage: STAGE_TIMEOUT,
                submit: Duration::from_secs(20),
                ..SubmitTimeouts::default()
            },
        );
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("still staged"), "{message}");
        assert!(message.contains("NOT submitted"), "{message}");
        let state = screen.state();
        assert_eq!(state.pastes.len(), 1);
        // Presses at 0, 0.5, 1.5, 3.5, 7.5, 11.5, 15.5 and 19.5 s.
        assert!((5..=8).contains(&state.keys.len()), "{:?}", state.keys);
        assert_eq!(state.composer, "never accepted");
        assert!(elapsed >= Duration::from_secs(20) && elapsed < Duration::from_secs(21));
    }

    #[test]
    fn existing_draft_is_refused_before_typing() {
        let screen = FakeScreen::new("claude", false);
        screen.state().composer = "someone else's words".to_owned();
        let reason = refusal(submit(&screen, "mine"));
        assert!(reason.contains("refusing to append"), "{reason}");
        let state = screen.state();
        assert!(state.pastes.is_empty() && state.keys.is_empty());
    }

    #[test]
    fn late_composer_is_awaited_before_typing() {
        let screen = FakeScreen::new("claude", false);
        screen.state().dialog = true;
        screen.state().dialog_reads = 3;
        let receipt = receipt(submit(&screen, "the brief"));
        assert_eq!(receipt.key, "Enter");
        let state = screen.state();
        assert_eq!(state.pastes, ["the brief"]);
        assert_eq!(state.submitted, ["the brief"]);
    }

    #[test]
    fn composer_wait_is_bounded_and_types_nothing() {
        let screen = FakeScreen::new("claude", false);
        screen.state().dialog = true;
        let (outcome, elapsed) = submit_with(
            &screen,
            "the brief",
            SubmitTimeouts {
                composer: Duration::from_secs(2),
                ..SubmitTimeouts::default()
            },
        );
        let reason = refusal(outcome);
        assert!(reason.contains("after 2s"), "{reason}");
        assert!(reason.contains("nothing was typed"), "{reason}");
        assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
        let state = screen.state();
        assert!(state.pastes.is_empty() && state.keys.is_empty());
    }

    #[test]
    fn open_dialog_is_refused_before_typing() {
        let screen = FakeScreen::new("codex", false);
        screen.state().dialog = true;
        let reason = refusal(submit(&screen, "hello"));
        assert!(reason.contains("recognisable codex composer"), "{reason}");
        assert!(screen.state().pastes.is_empty());
    }

    #[test]
    fn labelled_claude_top_border_still_frames_the_composer() {
        let screen = FakeScreen::new("claude", false);
        screen.state().top_border_label = Some("ultracode".to_owned());
        let view = composer_view("claude", &screen.render())
            .expect("a label in the top border must not hide the composer");
        assert_eq!(view.composer_solid.trim(), "");
        assert!(!view.transcript.contains("ultracode"), "{view:?}");
        assert!(view.footer.contains("auto mode on"), "{view:?}");

        let submitted = receipt(submit(&screen, "reply to the owner"));
        assert_eq!(submitted.key_presses, 1);
        assert_eq!(screen.state().submitted, ["reply to the owner"]);

        let busy = FakeScreen::new("claude", true);
        busy.state().top_border_label = Some("ultracode".to_owned());
        receipt(submit(&busy, "after this turn"));
        assert_eq!(busy.state().queued, ["after this turn"]);

        let drafted = FakeScreen::new("claude", false);
        drafted.state().top_border_label = Some("ultracode".to_owned());
        drafted.state().composer = "someone else's words".to_owned();
        let reason = refusal(submit(&drafted, "mine"));
        assert!(reason.contains("refusing to append"), "{reason}");
        assert!(drafted.state().pastes.is_empty());
    }

    #[test]
    fn labelled_rules_only_frame_a_composer_that_plain_rules_do_not() {
        let rule = "─".repeat(40);
        // Plain rules win: a draft row shaped like a labelled rule stays part of the draft.
        let screen = format!("• earlier\n{rule}\n❯ first line\n  ── notes ──\n{rule}\n  footer\n");
        let view = composer_view("claude", &screen).unwrap();
        assert!(view.composer_solid.contains("── notes ──"), "{view:?}");
        assert_eq!(view.transcript, "• earlier");

        // A labelled rule with no composer marker below it frames nothing.
        let no_composer = format!("{rule} label ─\n  Replace goal?\n{rule}\n");
        assert!(composer_view("claude", &no_composer).is_none());
        // Edge runs of rule characters are required on both sides of a label.
        for row in ["label ───", "─── label", "─ x", "──"] {
            let chars: Vec<char> = row.chars().collect();
            assert!(!is_labelled_rule(&chars), "{row:?}");
        }
        for row in ["─── label ─", "── label ─────", "───"] {
            let chars: Vec<char> = row.chars().collect();
            assert!(is_labelled_rule(&chars), "{row:?}");
        }
    }

    #[test]
    fn escape_characters_empty_text_and_cancellation_are_refused_before_typing() {
        let screen = FakeScreen::new("claude", false);
        assert!(refusal(submit(&screen, "a\u{1b}[31mb")).contains("escape"));
        assert!(refusal(submit(&screen, "  \n")).contains("empty"));
        let clock = Clock::default();
        clock.cancel.store(true, Ordering::SeqCst);
        let outcome = submit_verified(
            &screen,
            "w1:p1",
            "claude",
            "x",
            SubmitTimeouts::default(),
            &clock,
        );
        assert!(refusal(outcome).contains("cancelled before typing"));
        assert!(screen.state().pastes.is_empty());
    }

    #[test]
    fn invisible_paste_sends_no_submission_key() {
        let screen = FakeScreen::new("claude", false);
        screen.state().paste_visible_after_reads = u32::MAX;
        let message = submit(&screen, "lost paste").unwrap_err().to_string();
        assert!(message.contains("no submission key was sent"), "{message}");
        assert!(screen.state().keys.is_empty());
    }

    #[test]
    fn slow_paste_is_waited_for_before_the_key() {
        let screen = FakeScreen::new("codex", false);
        screen.state().paste_visible_after_reads = 5;
        let receipt = receipt(submit(&screen, "slow paste"));
        assert_eq!(receipt.key_presses, 1);
        assert_eq!(screen.state().submitted, ["slow paste"]);
    }

    #[test]
    fn long_paste_placeholder_counts_as_staged_and_submitted() {
        let screen = FakeScreen::new("claude", false);
        let text = "line one\nline two\nline three\nline four\nline five";
        let receipt = receipt(submit(&screen, text));
        assert_eq!(receipt.key_presses, 1);
        assert_eq!(screen.state().submitted, [text]);
    }

    /// Rows Codex v0.159.1 drew at the bottom of its pane, read with
    /// `herdr pane read --source visible --format ansi`. Escape sequences, attributes and row
    /// layout are as captured; runs of padding are shortened, and the model name, working
    /// directory, links and prompt text were replaced with neutral words.
    mod codex_v0_159_1 {
        pub(super) const PROMPT: &str = "Reply with exactly READY and nothing else.";
        const SHADED_BLANK: &str = "\u{1b}[0m\u{1b}[48;2;30;30;30m                    \u{1b}[0m";
        const EMPTY_COMPOSER: &str = concat!(
            "\u{1b}[0m\u{1b}[1m\u{1b}[38;2;190;142;250m\u{1b}[48;2;30;30;30m»\u{1b}[0m",
            "\u{1b}[48;2;30;30;30m \u{1b}[0m\u{1b}[2m\u{1b}[48;2;30;30;30mAsk Codex to do anything",
            "\u{1b}[0m\u{1b}[48;2;30;30;30m          \u{1b}[0m",
        );
        const STAGED_COMPOSER: &str = concat!(
            "\u{1b}[0m\u{1b}[1m\u{1b}[38;2;190;142;250m\u{1b}[48;2;30;30;30m»\u{1b}[0m",
            "\u{1b}[48;2;30;30;30m Reply with exactly READY and nothing else.          \u{1b}[0m",
        );
        const FOOTER: &str = concat!(
            "  \u{1b}[0m\u{1b}[38;2;246;226;183mmodel high\u{1b}[0m",
            "\u{1b}[38;2;132;132;132m · \u{1b}[0m\u{1b}[38;2;171;223;167m~/project\u{1b}[0m",
            "          \u{1b}[0m\u{1b}[38;2;132;132;132m⚠ \u{1b}[0m",
            "\u{1b}[38;2;196;167;103m1 warning\u{1b}[0m\u{1b}[38;2;132;132;132m · \u{1b}[0m",
            "\u{1b}[1m\u{1b}[38;2;220;220;220mf2\u{1b}[0m\u{1b}[38;2;132;132;132m to view\u{1b}[0m",
        );
        const SESSION_FOOTER: &str = concat!(
            "  \u{1b}[0m\u{1b}[38;5;6msession\u{1b}[0m\u{1b}[2m · \u{1b}[0m",
            "\u{1b}[38;2;246;226;183mmodel high\u{1b}[0m\u{1b}[38;2;132;132;132m · \u{1b}[0m",
            "\u{1b}[38;2;171;223;167m~/project\u{1b}[0m\u{1b}[38;2;132;132;132m · \u{1b}[0m",
            "\u{1b}[38;2;242;205;205mReply READY\u{1b}[0m          \u{1b}[0m",
        );
        const NOTICE: &str = "\u{1b}[0m\u{1b}[2m• \u{1b}[0mTip: /status shows the session.";
        /// The submitted prompt as the transcript shows it: Codex still draws `›` there.
        const SUBMITTED_PROMPT: &str = concat!(
            "\u{1b}[0m\u{1b}[1m\u{1b}[2m\u{1b}[48;2;40;40;40m› \u{1b}[0m",
            "\u{1b}[48;2;40;40;40mReply with exactly READY and nothing else.          \u{1b}[0m",
        );
        const HOOK: &str = "\u{1b}[0m\u{1b}[2m↳ Hook · \u{1b}[0mDiscovered 3 skills";
        const WORKING: &str = concat!(
            "\u{1b}[0m\u{1b}[1m\u{1b}[38;2;151;151;151m•\u{1b}[0m \u{1b}[0m",
            "\u{1b}[38;2;167;167;167mW\u{1b}[0m\u{1b}[38;2;118;118;118mo\u{1b}[0m",
            "\u{1b}[38;2;110;110;110mrking\u{1b}[0m \u{1b}[0m\u{1b}[2m(0s • \u{1b}[0m",
            "\u{1b}[1m\u{1b}[38;2;220;220;220mesc\u{1b}[0m\u{1b}[2m to interrupt)\u{1b}[0m",
        );
        const REPLY: &str = "\u{1b}[0m\u{1b}[2m• \u{1b}[0mREADY";
        const WORKED: &str = "\u{1b}[0m\u{1b}[2m  Worked for 52s • 12:56 PM\u{1b}[0m";

        fn screen(rows: &[&str]) -> String {
            rows.join("\n") + "\n"
        }

        /// A freshly started session, before any prompt.
        pub(super) fn fresh_idle() -> String {
            screen(&[
                NOTICE,
                "",
                " ",
                SHADED_BLANK,
                EMPTY_COMPOSER,
                SHADED_BLANK,
                FOOTER,
            ])
        }

        /// The same session with [`PROMPT`] pasted into the composer, before the submission key.
        pub(super) fn staged() -> String {
            screen(&[
                NOTICE,
                "",
                " ",
                SHADED_BLANK,
                STAGED_COMPOSER,
                SHADED_BLANK,
                FOOTER,
            ])
        }

        /// The first redraw after Enter: the prompt moved into the transcript.
        pub(super) fn submitted() -> String {
            screen(&[
                NOTICE,
                "",
                SUBMITTED_PROMPT,
                "",
                " ",
                SHADED_BLANK,
                EMPTY_COMPOSER,
                SHADED_BLANK,
                FOOTER,
            ])
        }

        /// The turn is running; the composer stays drawn below the progress row.
        pub(super) fn working() -> String {
            screen(&[
                SUBMITTED_PROMPT,
                "",
                "",
                HOOK,
                " ",
                WORKING,
                " ",
                SHADED_BLANK,
                EMPTY_COMPOSER,
                SHADED_BLANK,
                FOOTER,
            ])
        }

        /// The working screen with its composer rows removed.
        pub(super) fn working_without_composer() -> String {
            screen(&[SUBMITTED_PROMPT, "", "", HOOK, " ", WORKING, " ", FOOTER])
        }

        /// Idle again after a finished turn, with the session footer.
        pub(super) fn session_idle() -> String {
            screen(&[
                SUBMITTED_PROMPT,
                "",
                REPLY,
                "",
                WORKED,
                "",
                " ",
                SHADED_BLANK,
                EMPTY_COMPOSER,
                SHADED_BLANK,
                SESSION_FOOTER,
            ])
        }

        /// The folder-trust question Codex asks before its first composer in a new directory.
        pub(super) fn trust_prompt() -> String {
            screen(&[
                SHADED_BLANK,
                "\u{1b}[0m\u{1b}[1m\u{1b}[48;2;30;30;30m  Folder access\u{1b}[0m\u{1b}[48;2;30;30;30m          \u{1b}[0m",
                "\u{1b}[0m\u{1b}[48;2;30;30;30m  \u{1b}[0m\u{1b}[2m\u{1b}[48;2;30;30;30m/tmp/project          \u{1b}[0m\u{1b}[48;2;30;30;30m  \u{1b}[0m",
                SHADED_BLANK,
                "\u{1b}[0m\u{1b}[48;2;30;30;30m  Trust this folder? Codex can read, edit, and run files here.   \u{1b}[0m",
                "\u{1b}[0m\u{1b}[48;2;30;30;30m  Continue only if you trust these files.          \u{1b}[0m",
                SHADED_BLANK,
                "\u{1b}[0m\u{1b}[1m\u{1b}[38;2;0;0;46m\u{1b}[48;2;99;168;248m› 1. Trust and continue          \u{1b}[0m",
                "\u{1b}[0m\u{1b}[48;2;30;30;30m  2. Quit          \u{1b}[0m",
                SHADED_BLANK,
                concat!(
                    "\u{1b}[0m\u{1b}[48;2;30;30;30m  \u{1b}[0m\u{1b}[1m\u{1b}[38;2;220;220;220m",
                    "\u{1b}[48;2;30;30;30menter\u{1b}[0m\u{1b}[2m\u{1b}[48;2;30;30;30m continue · \u{1b}[0m",
                    "\u{1b}[1m\u{1b}[38;2;220;220;220m\u{1b}[48;2;30;30;30mesc\u{1b}[0m\u{1b}[2m",
                    "\u{1b}[48;2;30;30;30m quit\u{1b}[0m\u{1b}[48;2;30;30;30m          \u{1b}[0m",
                ),
            ])
        }
    }

    /// Rows Claude Code drew at the bottom of its pane while a turn was running, read with
    /// `herdr pane read --source visible --format ansi`: before a paste, with an 11-line paste
    /// collapsed into a placeholder, and after one Enter queued that paste. The composer, rule
    /// and footer rows are as captured, escape sequences included, except that runs of padding
    /// and rule characters are shortened and the placeholder carries the paste number of the
    /// session the bridge failed on. The rows above them are simplified. Among the differences:
    /// their words are neutral, a 10-row file preview and a hint row are left out, the later
    /// frames reuse the first frame's transcript rows and the staged frame its spinner row, and
    /// the queued prompt rows lack their trailing reset.
    mod claude_busy {
        /// Eleven lines, which Claude Code collapses into `[Pasted text #N +10 lines]`.
        pub(super) const PROMPT: &str = concat!(
            "Reply with one line for each item below.\n",
            "item 2\nitem 3\nitem 4\nitem 5\nitem 6\nitem 7\nitem 8\nitem 9\nitem 10\nitem 11",
        );
        const TOOL_CALL: &str =
            "\u{1b}[0m\u{1b}[38;5;114m● \u{1b}[0m\u{1b}[1mRead\u{1b}[0m(notes.md)";
        const TOOL_RESULT: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;246m  ⎿ \u{a0}\u{1b}[0mRead \u{1b}[0m\u{1b}[1m45\u{1b}[0m",
            " lines\u{1b}[0m",
        );
        const RUNNING: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;246m  \u{1b}[0mRunning the test suite\u{1b}[0m",
            "\u{1b}[38;5;246m · 3s\u{1b}[0m",
        );
        const RUNNING_COMMAND: &str = "\u{1b}[0m\u{1b}[38;5;246m  ⎿  $ cargo test\u{1b}[0m";
        const SPINNER: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;174m·\u{1b}[0m \u{1b}[0m\u{1b}[38;5;180mFermenting…\u{1b}[0m",
            "\u{1b}[38;5;174m \u{1b}[0m\u{1b}[38;5;246m(16m 8s · ↓\u{1b}[0m \u{1b}[0m",
            "\u{1b}[38;5;246m12.3k tokens)\u{1b}[0m",
        );
        /// The spinner just after Enter, while Claude Code runs its prompt hooks.
        const SPINNER_HOOKS: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;174m✢\u{1b}[0m \u{1b}[0m\u{1b}[38;5;216mFermenting…\u{1b}[0m",
            "\u{1b}[38;5;174m \u{1b}[0m\u{1b}[38;5;246m(running UserPromptSubmit hooks… 8/10 · ",
            "16m 9s · ↓ 12.3k tokens)\u{1b}[0m",
        );
        const PADDING: &str = "                    ";
        const RULE: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;244m",
            "────────────────────────────────────────",
            "\u{1b}[0m",
        );
        const EMPTY_COMPOSER: &str = "\u{1b}[0m\u{1b}[38;5;246m❯\u{a0}\u{1b}[0m\u{1b}[7m \u{1b}[0m";
        const PASTED_COMPOSER: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;246m❯\u{a0}\u{1b}[0m[Pasted text #923 +10 lines]\u{1b}[0m",
            "\u{1b}[7m \u{1b}[0m",
        );
        const QUEUED_COMPOSER: &str = concat!(
            "\u{1b}[0m\u{1b}[38;5;246m❯\u{a0}\u{1b}[0m\u{1b}[7mP\u{1b}[0m",
            "\u{1b}[2mress up to edit queued messages\u{1b}[0m",
        );
        const WORKING_FOOTER: &str = concat!(
            "  \u{1b}[0m\u{1b}[38;5;220m⏵⏵ auto mode on\u{1b}[0m",
            "\u{1b}[38;5;246m (shift+tab to cycle) · esc to interrupt · ← 2 agents\u{1b}[0m",
        );
        /// While a placeholder is in the composer, this replaces the running-turn footer.
        const PASTE_FOOTER: &str = "  \u{1b}[0m\u{1b}[38;5;246mpaste again to expand\u{1b}[0m";

        fn screen(rows: &[&str]) -> String {
            rows.join("\n") + "\n"
        }

        /// The running turn with an empty composer, before the paste.
        pub(super) fn before() -> String {
            screen(&[
                TOOL_CALL,
                TOOL_RESULT,
                "",
                RUNNING,
                RUNNING_COMMAND,
                "",
                SPINNER,
                PADDING,
                RULE,
                EMPTY_COMPOSER,
                RULE,
                WORKING_FOOTER,
            ])
        }

        /// The same turn with [`PROMPT`] pasted: only its placeholder is drawn.
        pub(super) fn staged() -> String {
            screen(&[
                TOOL_CALL,
                TOOL_RESULT,
                "",
                RUNNING,
                RUNNING_COMMAND,
                "",
                SPINNER,
                PADDING,
                RULE,
                PASTED_COMPOSER,
                RULE,
                PASTE_FOOTER,
            ])
        }

        /// After one Enter: the prompt is queued above the spinner, in full.
        pub(super) fn queued() -> String {
            let mut rows = vec![
                TOOL_CALL.to_owned(),
                TOOL_RESULT.to_owned(),
                String::new(),
                RUNNING.to_owned(),
                RUNNING_COMMAND.to_owned(),
                "     ".to_owned(),
            ];
            for (index, line) in PROMPT.lines().enumerate() {
                let lead = if index == 0 {
                    "\u{1b}[0m\u{1b}[38;5;239m\u{1b}[48;5;237m❯ "
                } else {
                    "\u{1b}[0m\u{1b}[48;5;237m  "
                };
                rows.push(format!(
                    "{lead}\u{1b}[0m\u{1b}[38;5;246m\u{1b}[48;5;237m{line}\u{1b}[0m\u{1b}[48;5;237m{PADDING}"
                ));
            }
            rows.extend(
                [
                    "",
                    SPINNER_HOOKS,
                    PADDING,
                    RULE,
                    QUEUED_COMPOSER,
                    RULE,
                    PASTE_FOOTER,
                ]
                .map(str::to_owned),
            );
            rows.join("\n") + "\n"
        }
    }

    /// Plays captured screens back: one before the paste, one while the paste is staged,
    /// and one after the submission key. The staged screen can be held back for a number of
    /// reads, as when a busy agent draws a paste late. Every key press is recorded.
    struct Replay {
        frames: [String; 3],
        phase: std::sync::Mutex<usize>,
        /// Reads after the paste that still return the screen from before it.
        undrawn_reads: std::sync::Mutex<u32>,
        pastes: std::sync::Mutex<Vec<String>>,
        keys: std::sync::Mutex<Vec<String>>,
    }

    impl Replay {
        fn new(before: String, staged: String, after: String) -> Self {
            Self {
                frames: [before, staged, after],
                phase: std::sync::Mutex::new(0),
                undrawn_reads: std::sync::Mutex::new(0),
                pastes: std::sync::Mutex::default(),
                keys: std::sync::Mutex::default(),
            }
        }

        /// Draw the paste only on the read after `reads` reads that miss it.
        fn drawn_after(self, reads: u32) -> Self {
            *self.undrawn_reads.lock().unwrap() = reads;
            self
        }
    }

    impl PromptTerminal for Replay {
        fn read_screen(&self, _pane_id: &str) -> Result<String> {
            let phase = *self.phase.lock().unwrap();
            let mut undrawn = self.undrawn_reads.lock().unwrap();
            if phase == 1 && *undrawn > 0 {
                *undrawn -= 1;
                return Ok(self.frames[0].clone());
            }
            Ok(self.frames[phase].clone())
        }

        fn send_text(&self, _pane_id: &str, text: &str) -> Result<()> {
            self.pastes.lock().unwrap().push(text.to_owned());
            *self.phase.lock().unwrap() = 1;
            Ok(())
        }

        fn send_keys(&self, _pane_id: &str, keys: &str) -> Result<()> {
            self.keys.lock().unwrap().push(keys.to_owned());
            if keys == "Enter" {
                *self.phase.lock().unwrap() = 2;
            }
            Ok(())
        }
    }

    #[test]
    fn codex_0_159_1_idle_composer_is_recognised() {
        for screen in [codex_v0_159_1::fresh_idle(), codex_v0_159_1::session_idle()] {
            let view = composer_view("codex", &screen)
                .unwrap_or_else(|| panic!("no composer in {screen:?}"));
            assert_eq!(view.composer_solid.trim(), "", "{view:?}");
            assert!(
                view.composer.contains("Ask Codex to do anything"),
                "{view:?}"
            );
            assert!(view.footer.contains("~/project"), "{view:?}");
            assert!(!view.transcript.contains("Ask Codex"), "{view:?}");
        }
        let view = composer_view("codex", &codex_v0_159_1::session_idle()).unwrap();
        assert!(view.transcript.contains("Worked for 52s"), "{view:?}");
    }

    #[test]
    fn herdr_093_codex_idle_composer_accepts_only_recognized_empty_idle_frames() {
        for screen in [codex_v0_159_1::fresh_idle(), codex_v0_159_1::session_idle()] {
            assert!(codex_idle_composer(&screen), "{screen:?}");
        }
        for marker in ['›', '»'] {
            for footer in [
                "? for shortcuts",
                "100% context left",
                "82% context remaining",
                "82.5% context remaining",
                "model high · /work/project",
            ] {
                let screen = format!(
                    "Codex\n{marker} \u{1b}[2mAsk Codex to do anything\u{1b}[0m\n  {footer}\n"
                );
                assert!(codex_idle_composer(&screen), "{screen:?}");
            }
        }
        for screen in [
            codex_v0_159_1::working(),
            codex_v0_159_1::staged(),
            codex_v0_159_1::trust_prompt(),
            "Codex\n» draft text\n  ? for shortcuts\n".to_owned(),
            "Codex\n» \u{1b}[2m[Pasted text #4 +12 lines]\u{1b}[0m\n  ? for shortcuts\n".to_owned(),
            "Codex\n» \u{1b}[2m[Pasted Content 50 chars]\u{1b}[0m\n  ? for shortcuts\n".to_owned(),
            "shell$\n".to_owned(),
            "Codex\n»\n  arbitrary footer\n".to_owned(),
        ] {
            assert!(!codex_idle_composer(&screen), "{screen:?}");
        }
    }

    #[test]
    fn herdr_093_codex_idle_composer_refuses_current_attention_cues_without_scanning_history() {
        for cue in [
            "esc to interrupt",
            "tab to queue message",
            "Press up to edit queued messages",
            "edit last queued message",
            "Queued follow-up inputs",
            "paste again to expand",
            "Trust this folder?",
            "Trust and continue",
            "Approval required",
            "requires approval",
            "Approve this",
            "Would you like to run",
            "Press enter to continue",
            "Enter to confirm",
            "Esc to cancel",
        ] {
            let screen = format!("{cue}\n»\n  ? for shortcuts\n");
            assert!(!codex_idle_composer(&screen), "{cue}");
        }
        let screen = format!(
            "Approval required in an old quoted message\n{}»\n  ? for shortcuts\n",
            "prior answer\n".repeat(20)
        );
        assert!(codex_idle_composer(&screen));
    }

    #[test]
    fn herdr_093_unknown_codex_live_timers_follow_the_source_grammar_and_response_tail() {
        let timer = regex::Regex::new(
            r"^(?:[•◦][ \t]+)?[^\s›•◦■✗✓─][^\r\n]* \((?:[0-9]+[hm] )*[0-9]+s(?: • [^\r\n]+? to interrupt)?\)(?: · [^\r\n]*)?$"
        ).unwrap();
        for row in [
            "Working (7s)",
            "Working (7s)   ",
            "◦ Exploring (1m 2s • Ctrl+c to interrupt)",
            "• Working (7s • esc to interrupt)",
            "Processing inputs (7s) · command (background)",
            "Working (1h 2m 3s • remapped (key) to interrupt) · model",
            "• Thinking about changes (12s)",
            "› old prompt (7s)",
            "■ prior response (7s)",
            "Working (soon)",
            "Working (7s) extra",
            "Working (1m 2s • to interrupt)",
        ] {
            assert_eq!(
                codex_timer_row(row),
                timer.is_match(row.trim_end()),
                "{row:?}"
            );
        }
        for row in [
            "Working (7s)",
            "Working (7s)   ",
            "◦ Exploring (1m 2s • Ctrl+c to interrupt)",
            "• Working (7s • esc to interrupt)",
            "Processing inputs (7s) · command (background)",
            "Working (1h 2m 3s • remapped (key) to interrupt) · model",
            "Working (7s)\u{1b}[48;5;235m   \u{1b}[0m",
        ] {
            let screen = format!("{row}\n»\n  ? for shortcuts\n");
            assert!(!codex_idle_composer(&screen), "active {row:?}");
            let screen = format!("{row}\n■ completed response\n»\n  ? for shortcuts\n");
            assert!(codex_idle_composer(&screen), "completed {row:?}");
        }
        let queued = format!(
            "Working (7s)\n• Messages to be submitted after next tool call\n{}»\n  ? for shortcuts\n",
            "queued body\n".repeat(30)
        );
        assert!(!codex_idle_composer(&queued));
        let complete = format!(
            "Working (7s)\n• Messages to be submitted after next tool call\n{}✓ completed response\n»\n  ? for shortcuts\n",
            "queued body\n".repeat(30)
        );
        assert!(codex_idle_composer(&complete));
        let queued_uppercase =
            queued.replace("Messages to be submitted", "MESSAGES TO BE SUBMITTED");
        assert!(!codex_idle_composer(&queued_uppercase));
    }

    #[test]
    fn herdr_093_unknown_codex_refuses_current_source_dialogs_and_grouped_search_menu() {
        for cue in [
            "enter to submit answer",
            "enter to submit all",
            "allow command?",
            "Do you trust the contents of this directory?",
            "All Results   Filesystem Only   Plugins",
            "All Results\nFilesystem Only\nPlugins",
        ] {
            let screen = format!("{cue}\n»\n  ? for shortcuts\n");
            assert!(!codex_idle_composer(&screen), "{cue}");
        }
        for cue in ["All Results", "Plugins", "All Results   Filesystem Only"] {
            let screen = format!("{cue}\n»\n  ? for shortcuts\n");
            assert!(codex_idle_composer(&screen), "incomplete menu {cue}");
        }
        let history = format!(
            "All Results   Filesystem Only   Plugins\n{}»\n  ? for shortcuts\n",
            "prior answer\n".repeat(20)
        );
        assert!(codex_idle_composer(&history));
    }

    #[test]
    fn codex_0_159_1_prompt_is_staged_submitted_and_verified() {
        let replay = Replay::new(
            codex_v0_159_1::fresh_idle(),
            codex_v0_159_1::staged(),
            codex_v0_159_1::submitted(),
        );
        let clock = Clock::default();
        let outcome = submit_verified(
            &replay,
            "w1:p1",
            "codex",
            codex_v0_159_1::PROMPT,
            SubmitTimeouts::default(),
            &clock,
        );
        let receipt = receipt(outcome);
        assert_eq!(receipt.key, "Enter");
        assert_eq!(receipt.key_presses, 1);
        assert_eq!(receipt.evidence, "prompt text appeared above the composer");
        assert_eq!(
            *replay.pastes.lock().unwrap(),
            [format!(
                "{BRACKETED_PASTE_START}{}{BRACKETED_PASTE_END}",
                codex_v0_159_1::PROMPT
            )]
        );
        assert_eq!(*replay.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn codex_0_159_1_working_screen_keeps_its_progress_row_out_of_the_draft() {
        let view = composer_view("codex", &codex_v0_159_1::working()).unwrap();
        assert_eq!(view.composer_solid.trim(), "", "{view:?}");
        assert!(view.transcript.contains(WORKING_MARKER), "{view:?}");
        assert!(!view.footer.contains(CODEX_QUEUE_HINT), "{view:?}");
    }

    #[test]
    fn codex_0_159_1_screens_without_a_composer_are_refused_before_typing() {
        assert!(composer_view("codex", &codex_v0_159_1::working_without_composer()).is_none());
        for screen in [
            codex_v0_159_1::working_without_composer(),
            codex_v0_159_1::trust_prompt(),
        ] {
            let replay = Replay::new(screen.clone(), screen.clone(), screen);
            let clock = Clock::default();
            let outcome = submit_verified(
                &replay,
                "w1:p1",
                "codex",
                "hello",
                SubmitTimeouts::default(),
                &clock,
            );
            let reason = refusal(outcome);
            assert!(reason.contains("nothing was typed"), "{reason}");
            assert!(replay.pastes.lock().unwrap().is_empty());
            assert!(replay.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn codex_flows_hold_under_both_composer_markers() {
        for marker in ["›", "»"] {
            let idle = FakeScreen::new("codex", false);
            idle.state().codex_marker = marker;
            let view = composer_view("codex", &idle.render()).expect(marker);
            assert_eq!(view.composer_solid.trim(), "", "{marker}");
            assert_eq!(receipt(submit(&idle, "start now")).key, "Enter", "{marker}");
            assert_eq!(idle.state().submitted, ["start now"], "{marker}");

            let busy = FakeScreen::new("codex", true);
            busy.state().codex_marker = marker;
            busy.state().drop_keys = 1;
            assert_eq!(
                receipt(submit(&busy, "follow up later")).key,
                "Tab",
                "{marker}"
            );
            assert_eq!(busy.state().queued, ["follow up later"], "{marker}");
            assert!(busy.state().steered.is_empty(), "{marker}");

            let drafted = FakeScreen::new("codex", false);
            drafted.state().codex_marker = marker;
            drafted.state().composer = "someone else's words".to_owned();
            let reason = refusal(submit(&drafted, "mine"));
            assert!(reason.contains("refusing to append"), "{marker}: {reason}");
            assert!(drafted.state().pastes.is_empty(), "{marker}");

            let dialog = FakeScreen::new("codex", false);
            dialog.state().codex_marker = marker;
            dialog.state().dialog = true;
            let reason = refusal(submit(&dialog, "hello"));
            assert!(
                reason.contains("recognisable codex composer"),
                "{marker}: {reason}"
            );
            assert!(dialog.state().pastes.is_empty(), "{marker}");
        }
    }

    #[test]
    fn busy_claude_screens_show_the_paste_only_as_a_placeholder() {
        let before = composer_view("claude", &claude_busy::before()).expect("busy composer");
        assert_eq!(before.composer_solid.trim(), "", "{before:?}");
        assert_eq!(paste_placeholders(&before.composer), 0, "{before:?}");
        assert!(before.footer.contains(WORKING_MARKER), "{before:?}");

        let pasted = composer_view("claude", &claude_busy::staged()).expect("pasted composer");
        assert_eq!(
            pasted.composer_solid.trim(),
            "[Pasted text #923 +10 lines]",
            "{pasted:?}"
        );
        assert!(staged(&pasted, claude_busy::PROMPT, 0), "{pasted:?}");
        // The footer stops saying that a turn is running while the placeholder is drawn.
        assert!(!pasted.footer.contains(WORKING_MARKER), "{pasted:?}");
        assert!(
            pasted.footer.contains("paste again to expand"),
            "{pasted:?}"
        );

        let queued = composer_view("claude", &claude_busy::queued()).expect("queued composer");
        assert_eq!(queued.composer_solid.trim(), "", "{queued:?}");
        assert!(!staged(&queued, claude_busy::PROMPT, 0), "{queued:?}");
        assert_eq!(
            corroborated(&before, &queued, claude_busy::PROMPT).as_deref(),
            Some("prompt text appeared above the composer")
        );
    }

    #[test]
    fn placeholder_left_in_a_busy_claude_composer_holds_the_next_prompt() {
        let screen = claude_busy::staged();
        let replay = Replay::new(screen.clone(), screen.clone(), screen);
        let reason = refusal(submit_verified(
            &replay,
            "w1:p1",
            "claude",
            "the next prompt",
            SubmitTimeouts::default(),
            &Clock::default(),
        ));
        assert!(reason.contains("refusing to append"), "{reason}");
        assert!(replay.pastes.lock().unwrap().is_empty());
        assert!(replay.keys.lock().unwrap().is_empty());
    }

    // Verified submission once saw no paste in a busy Claude Code composer within 3 s of
    // typing it; the paste was there later, unsubmitted, and held later prompts:
    // https://github.com/rrnewton/agent-utils/issues/215
    #[test]
    fn busy_claude_paste_drawn_late_is_submitted_with_one_enter() {
        let replay = Replay::new(
            claude_busy::before(),
            claude_busy::staged(),
            claude_busy::queued(),
        )
        .drawn_after(45);
        let clock = Clock::default();
        let outcome = submit_verified(
            &replay,
            "w1:p1",
            "claude",
            claude_busy::PROMPT,
            SubmitTimeouts::default(),
            &clock,
        );
        let receipt = receipt(outcome);
        assert_eq!(receipt.key, "Enter");
        assert_eq!(receipt.key_presses, 1);
        assert_eq!(receipt.evidence, "prompt text appeared above the composer");
        // Reads 100 ms apart missed the placeholder for 4.5 s; the evidence came one poll after
        // the Enter.
        assert_eq!(receipt.elapsed, Duration::from_millis(4_600));
        assert_eq!(
            *replay.pastes.lock().unwrap(),
            [format!(
                "{BRACKETED_PASTE_START}{}{BRACKETED_PASTE_END}",
                claude_busy::PROMPT
            )]
        );
        assert_eq!(*replay.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn busy_claude_paste_never_drawn_gives_up_at_the_stage_timeout_without_a_key() {
        let replay = Replay::new(
            claude_busy::before(),
            claude_busy::staged(),
            claude_busy::queued(),
        )
        .drawn_after(u32::MAX);
        let clock = Clock::default();
        let message = submit_verified(
            &replay,
            "w1:p1",
            "claude",
            claude_busy::PROMPT,
            SubmitTimeouts::default(),
            &clock,
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("within 60.0s; no submission key was sent"),
            "{message}"
        );
        assert_eq!(
            clock.monotonic() - Duration::from_millis(1_000_000),
            STAGE_TIMEOUT
        );
        assert_eq!(replay.pastes.lock().unwrap().len(), 1);
        assert!(replay.keys.lock().unwrap().is_empty());
    }

    /// A clock whose stop request arrives once `stop_after` has passed.
    struct StoppingClock {
        clock: Clock,
        stop_after: Duration,
    }

    impl AgentRuntime for StoppingClock {
        fn monotonic(&self) -> Duration {
            self.clock.monotonic()
        }

        fn sleep(&self, duration: Duration) {
            self.clock.sleep(duration);
            if self.monotonic() - Duration::from_millis(1_000_000) >= self.stop_after {
                self.clock.cancel.store(true, Ordering::SeqCst);
            }
        }

        fn cancelled(&self) -> bool {
            self.clock.cancelled()
        }
    }

    #[test]
    fn stop_ends_the_wait_for_a_late_paste_at_the_next_poll() {
        let replay = Replay::new(
            claude_busy::before(),
            claude_busy::staged(),
            claude_busy::queued(),
        )
        .drawn_after(u32::MAX);
        let runtime = StoppingClock {
            clock: Clock::default(),
            stop_after: Duration::from_secs(1),
        };
        let message = submit_verified(
            &replay,
            "w1:p1",
            "claude",
            claude_busy::PROMPT,
            SubmitTimeouts::default(),
            &runtime,
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("cancelled after the prompt was pasted"),
            "{message}"
        );
        assert_eq!(
            runtime.monotonic() - Duration::from_millis(1_000_000),
            Duration::from_secs(1)
        );
        assert!(replay.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn a_running_turn_is_read_from_the_bottom_rows_of_the_screen() {
        // Claude Code ends its status row with the marker while it works, and shows the queued
        // hint as the placeholder of its empty input box.
        let claude = FakeScreen::new("claude", true);
        assert_eq!(
            running_turn(&claude.render()),
            Some(true),
            "{}",
            claude.render()
        );
        claude.state().busy = false;
        assert_eq!(
            running_turn(&claude.render()),
            Some(false),
            "{}",
            claude.render()
        );
        claude.state().queued.push("next".to_owned());
        assert_eq!(
            running_turn(&claude.render()),
            Some(true),
            "{}",
            claude.render()
        );

        // Codex prints the marker in its progress row, above the composer.
        assert_eq!(running_turn(&codex_v0_159_1::working()), Some(true));
        for idle in [
            codex_v0_159_1::fresh_idle(),
            codex_v0_159_1::submitted(),
            codex_v0_159_1::session_idle(),
        ] {
            assert_eq!(running_turn(&idle), Some(false), "{idle:?}");
        }
        let queued = concat!(
            "• Queued follow-up inputs\n",
            "  ↳ check the logs\n",
            "\n",
            "› Ask Codex to do anything\n",
            "\n",
            "  GPT default · /tmp/project\n",
        );
        assert_eq!(running_turn(queued), Some(true));

        assert_eq!(running_turn(""), Some(false));
        assert_eq!(running_turn(" \n\n \n"), Some(false));
    }

    #[test]
    fn running_turn_words_above_the_bottom_rows_do_not_count() {
        // The marker sits `above` rows over the last row with text; blank rows below that row
        // are not counted.
        let screen = |above: usize| {
            let mut rows = vec!["• Claude prints esc to interrupt while it works".to_owned()];
            rows.extend((1..above).map(|row| format!("• line {row}")));
            rows.push("  ⏵⏵ auto mode on".to_owned());
            rows.extend([String::new(), String::new()]);
            rows.join("\n")
        };
        assert_eq!(running_turn(&screen(RUNNING_TURN_ROWS - 1)), Some(true));
        assert_eq!(running_turn(&screen(RUNNING_TURN_ROWS)), Some(false));
    }

    #[test]
    fn a_screen_whose_last_row_is_the_paste_hint_cannot_tell_whether_a_turn_runs() {
        // A busy Claude Code as captured: the marker ends the status row before the paste, the
        // hint takes that row's place while the paste is staged, and the queued-message hint
        // shows once Enter has queued the paste.
        assert_eq!(running_turn(&claude_busy::before()), Some(true));
        assert_eq!(running_turn(&claude_busy::staged()), None);
        assert_eq!(running_turn(&claude_busy::queued()), Some(true));

        // The hint follows a paste into an idle pane too, so it says nothing either way.
        let rows = |above: &str, status: &str| {
            [
                "● Done.",
                above,
                "────────────────────────────────────────",
                "❯ [Pasted text #923 +10 lines]",
                "────────────────────────────────────────",
                status,
                "",
            ]
            .join("\n")
        };
        assert_eq!(running_turn(&rows("", "  paste again to expand")), None);
        // A marker still counts beside the hint, as in a spinner row that ends with it.
        let spinner = "✻ Fermenting… (16m 8s · ↓ 12.3k tokens · esc to interrupt)";
        assert_eq!(
            running_turn(&rows(spinner, "  paste again to expand")),
            Some(true)
        );
        // Only the last row with text is the status row: the hint's words above it do not count.
        let quoted = "  Claude Code prints paste again to expand after a paste";
        assert_eq!(
            running_turn(&rows(quoted, "  ⏵⏵ auto mode on")),
            Some(false)
        );
    }

    /// Plays a screen from before the paste, one while the paste is staged, and then each
    /// screen of `after` in turn, one per read after the submission key, repeating the last.
    struct Frames {
        before: String,
        staged: String,
        after: Vec<String>,
        phase: std::sync::Mutex<usize>,
        reads_after_key: std::sync::Mutex<usize>,
        keys: std::sync::Mutex<Vec<String>>,
    }

    impl Frames {
        fn new(before: String, staged: String, after: Vec<String>) -> Self {
            Self {
                before,
                staged,
                after,
                phase: std::sync::Mutex::new(0),
                reads_after_key: std::sync::Mutex::new(0),
                keys: std::sync::Mutex::default(),
            }
        }
    }

    impl PromptTerminal for Frames {
        fn read_screen(&self, _pane_id: &str) -> Result<String> {
            match *self.phase.lock().unwrap() {
                0 => Ok(self.before.clone()),
                1 => Ok(self.staged.clone()),
                _ => {
                    let mut reads = self.reads_after_key.lock().unwrap();
                    let frame = self.after[(*reads).min(self.after.len() - 1)].clone();
                    *reads += 1;
                    Ok(frame)
                }
            }
        }

        fn send_text(&self, _pane_id: &str, _text: &str) -> Result<()> {
            *self.phase.lock().unwrap() = 1;
            Ok(())
        }

        fn send_keys(&self, _pane_id: &str, keys: &str) -> Result<()> {
            self.keys.lock().unwrap().push(keys.to_owned());
            if keys == "Enter" {
                *self.phase.lock().unwrap() = 2;
            }
            Ok(())
        }
    }

    const IDLE_STATUS: &str = "  ⏵⏵ auto mode on";
    const BUSY_STATUS: &str = "  ⏵⏵ auto mode on · esc to interrupt";
    const PASTE_STATUS: &str = "  paste again to expand";
    /// The placeholder Claude Code draws in its empty input box while messages are queued.
    const QUEUED_PLACEHOLDER: &str =
        "\u{1b}[7mP\u{1b}[0m\u{1b}[2mress up to edit queued messages\u{1b}[22m";

    /// A plain Claude Code screen: `transcript` rows, the input box holding `composer`, and the
    /// status row.
    fn claude_screen(transcript: &[&str], composer: &str, status: &str) -> String {
        let rule = "─".repeat(40);
        let mut rows: Vec<String> = transcript.iter().map(|row| (*row).to_owned()).collect();
        rows.extend([
            rule.clone(),
            format!("❯\u{a0}{composer}"),
            rule,
            status.to_owned(),
        ]);
        rows.join("\n") + "\n"
    }

    /// Submit `prompt` to `frames` as Claude Code, returning the receipt and the time it took.
    fn submit_frames(frames: &Frames, prompt: &str) -> (SubmissionReceipt, Duration) {
        let clock = Clock::default();
        let outcome = submit_verified(
            frames,
            "w1:p1",
            "claude",
            prompt,
            SubmitTimeouts::default(),
            &clock,
        );
        (
            receipt(outcome),
            clock.monotonic() - Duration::from_millis(1_000_000),
        )
    }

    #[test]
    fn a_prompt_drawn_above_the_composer_is_printed_evidence() {
        for (harness, busy) in [
            ("claude", false),
            ("claude", true),
            ("codex", false),
            ("codex", true),
        ] {
            let screen = FakeScreen::new(harness, busy);
            let receipt = receipt(submit(&screen, "run the tests"));
            assert!(receipt.printed, "{harness} busy={busy}: {receipt:?}");
            assert_eq!(
                receipt.evidence, "prompt text appeared above the composer",
                "{harness} busy={busy}"
            );
        }

        // The captured busy Claude Code screens: Enter queued the paste and drew it in full.
        let replay = Replay::new(
            claude_busy::before(),
            claude_busy::staged(),
            claude_busy::queued(),
        );
        let receipt = receipt(submit_verified(
            &replay,
            "w1:p1",
            "claude",
            claude_busy::PROMPT,
            SubmitTimeouts::default(),
            &Clock::default(),
        ));
        assert!(receipt.printed, "{receipt:?}");
        assert_eq!(receipt.evidence, "prompt text appeared above the composer");
    }

    #[test]
    fn a_prompt_staged_again_after_a_transient_empty_composer_is_not_verified() {
        // The key was dropped: one redraw after it looks empty, so the running-turn marker that
        // was already showing seems to corroborate a submission, and then the prompt is staged
        // in the composer again on every read. Nothing proved the submission, so the key is
        // pressed again until the deadline, and the prompt is reported as not submitted.
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        let frames = Frames::new(busy.clone(), staged.clone(), vec![busy, staged]);
        let clock = Clock::default();
        let outcome = submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &clock,
        );
        match outcome {
            Err(error) => assert!(
                error.to_string().contains("it was NOT submitted"),
                "{error}"
            ),
            other => panic!("expected the prompt to be reported as not submitted, got {other:?}"),
        }
        assert!(frames.keys.lock().unwrap().len() > 1);
    }

    #[test]
    fn a_weak_verification_settles_only_when_the_whole_wait_shows_the_composer_clear() {
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        // The whole wait shows the composer clear: settled.
        let frames = Frames::new(busy.clone(), staged.clone(), vec![busy.clone()]);
        let (held, _) = submit_frames(&frames, "run the tests");
        assert!(!held.printed && held.settled, "{held:?}");
        // The wait ends on a screen with no recognisable composer: not settled.
        let frames = Frames::new(
            busy.clone(),
            staged.clone(),
            vec![
                busy.clone(),
                "Replace goal?\n  › 1. Replace\n  2. Cancel\n".to_owned(),
            ],
        );
        let (held, _) = submit_frames(&frames, "run the tests");
        assert!(!held.printed && !held.settled, "{held:?}");
        // Stopped during the wait, before the prompt could be seen back in the composer: not
        // settled.
        let frames = Frames::new(busy.clone(), staged, vec![busy.clone(), busy]);
        let runtime = StoppingClock {
            clock: Clock::default(),
            stop_after: Duration::from_millis(150),
        };
        let held = receipt(submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &runtime,
        ));
        assert!(!held.printed && !held.settled, "{held:?}");
    }

    #[test]
    fn a_stop_arriving_with_the_last_read_of_a_complete_wait_still_settles() {
        // Weak evidence at 100 ms; the wait's last read at 2,100 ms comes with a stop.
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        let frames = Frames::new(busy.clone(), staged, vec![busy]);
        let runtime = StoppingClock {
            clock: Clock::default(),
            stop_after: POLL + PRINT_GRACE,
        };
        let held = receipt(submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &runtime,
        ));
        assert!(!held.printed && held.settled, "{held:?}");
    }

    #[test]
    fn a_stop_before_a_carried_staged_read_sends_no_further_key() {
        // Empty-looking redraws from 100 to 400 ms, a stop at 500 ms, and the prompt staged
        // again on the read at 500 ms: that read is not handled, so the scheduled second key is
        // never sent.
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        let frames = Frames::new(
            busy.clone(),
            staged.clone(),
            vec![busy.clone(), busy.clone(), busy.clone(), busy, staged],
        );
        let runtime = StoppingClock {
            clock: Clock::default(),
            stop_after: Duration::from_millis(450),
        };
        let outcome = submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &runtime,
        );
        assert!(outcome.is_err(), "{outcome:?}");
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn redraws_alternating_with_the_staged_prompt_keep_the_key_schedule_and_the_deadline() {
        // The key was dropped, and the pane alternates between an empty-looking redraw and the
        // staged prompt. Each staged read goes through the staged path, so the key is pressed
        // again on its schedule and the prompt is reported as not submitted at the deadline.
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        let after = (0..4_000)
            .map(|read| {
                if read % 2 == 0 {
                    busy.clone()
                } else {
                    staged.clone()
                }
            })
            .collect();
        let frames = Frames::new(busy.clone(), staged.clone(), after);
        let clock = Clock::default();
        let started = clock.monotonic();
        let outcome = submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &clock,
        );
        match outcome {
            Err(error) => assert!(
                error.to_string().contains("it was NOT submitted"),
                "{error}"
            ),
            other => panic!("expected the prompt to be reported as not submitted, got {other:?}"),
        }
        assert!(frames.keys.lock().unwrap().len() > 2);
        assert!(clock.monotonic() - started <= SubmitTimeouts::default().submit + PRINT_GRACE);
    }

    #[test]
    fn a_row_first_seen_while_the_prompt_was_staged_again_is_not_printed_evidence() {
        // After a dropped key: an empty-looking redraw, then the prompt staged again with an
        // older identical row newly visible above it, then an empty-looking redraw with that same
        // row. The row was seen while the prompt was staged, so it proves nothing.
        let row = "❯ run the tests";
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(&["• earlier"], "run the tests", BUSY_STATUS);
        let staged_with_row = claude_screen(&["• earlier", row], "run the tests", BUSY_STATUS);
        let busy_with_row = claude_screen(&["• earlier", row], "", BUSY_STATUS);
        let frames = Frames::new(
            busy.clone(),
            staged,
            vec![
                busy,
                staged_with_row.clone(),
                busy_with_row,
                staged_with_row,
            ],
        );
        let clock = Clock::default();
        let outcome = submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &clock,
        );
        match outcome {
            Err(error) => assert!(
                error.to_string().contains("it was NOT submitted"),
                "{error}"
            ),
            other => panic!("expected the prompt to be reported as not submitted, got {other:?}"),
        }
    }

    #[test]
    fn a_running_turn_marker_that_was_already_showing_is_not_printed_evidence() {
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let frames = Frames::new(
            busy.clone(),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![busy],
        );
        let (receipt, took) = submit_frames(&frames, "run the tests");
        assert_eq!(receipt.evidence, "agent reports an active turn");
        assert!(!receipt.printed, "{receipt:?}");
        // The first read after the key proved the submission. The reads that then looked for
        // printed evidence, for `PRINT_GRACE`, sent no key.
        assert_eq!(receipt.elapsed, POLL);
        assert_eq!(took, POLL + PRINT_GRACE);
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn printed_evidence_that_follows_a_marker_already_showing_is_waited_for() {
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let queued = claude_screen(&["• earlier", "❯ run the tests"], "", BUSY_STATUS);
        let frames = Frames::new(
            busy.clone(),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![busy.clone(), busy.clone(), busy, queued],
        );
        let (receipt, took) = submit_frames(&frames, "run the tests");
        assert_eq!(receipt.evidence, "prompt text appeared above the composer");
        assert!(receipt.printed, "{receipt:?}");
        // Reads 100 ms apart: the fourth after the key showed the prompt.
        assert_eq!(receipt.elapsed, 4 * POLL);
        assert_eq!(took, 4 * POLL);
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn a_queued_message_marker_that_appears_after_the_key_is_printed_evidence() {
        let frames = Frames::new(
            claude_screen(&["• earlier"], "", BUSY_STATUS),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![claude_screen(
                &["• earlier"],
                QUEUED_PLACEHOLDER,
                BUSY_STATUS,
            )],
        );
        let (receipt, took) = submit_frames(&frames, "run the tests");
        assert_eq!(
            receipt.evidence,
            "agent queue marker appeared (\"Press up to edit queued messages\")"
        );
        assert!(receipt.printed, "{receipt:?}");
        assert_eq!((receipt.elapsed, took), (POLL, POLL));
    }

    #[test]
    fn a_queued_message_marker_that_was_already_showing_is_not_printed_evidence() {
        let queued = claude_screen(&["• earlier"], QUEUED_PLACEHOLDER, BUSY_STATUS);
        let frames = Frames::new(
            queued.clone(),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![queued],
        );
        let (receipt, took) = submit_frames(&frames, "run the tests");
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(took, POLL + PRINT_GRACE);
    }

    #[test]
    fn a_running_turn_marker_that_appears_after_the_key_is_printed_evidence() {
        let frames = Frames::new(
            claude_screen(&["• earlier"], "", IDLE_STATUS),
            claude_screen(&["• earlier"], "run the tests", IDLE_STATUS),
            vec![claude_screen(&["• earlier"], "", BUSY_STATUS)],
        );
        let (receipt, took) = submit_frames(&frames, "run the tests");
        assert_eq!(receipt.evidence, "agent started reporting an active turn");
        assert!(receipt.printed, "{receipt:?}");
        assert_eq!((receipt.elapsed, took), (POLL, POLL));
    }

    #[test]
    fn a_prompt_row_shown_while_the_prompt_was_staged_is_not_printed_evidence() {
        let before = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(
            &["• earlier", "❯ run the tests"],
            "run the tests",
            BUSY_STATUS,
        );
        let after = claude_screen(&["• earlier", "❯ run the tests"], "", BUSY_STATUS);
        let frames = Frames::new(before, staged, vec![after]);
        let (receipt, _) = submit_frames(&frames, "run the tests");
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn a_placeholder_row_shown_while_the_prompt_was_staged_is_not_printed_evidence() {
        let before = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let staged = claude_screen(
            &["• earlier", "❯ [Pasted text #923 +10 lines]"],
            "[Pasted text #924 +10 lines]",
            BUSY_STATUS,
        );
        let after = claude_screen(
            &["• earlier", "❯ [Pasted text #923 +10 lines]"],
            "",
            BUSY_STATUS,
        );
        let frames = Frames::new(before, staged, vec![after]);
        let (receipt, _) = submit_frames(&frames, claude_busy::PROMPT);
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn a_new_prompt_ending_like_an_older_row_is_printed_evidence_by_its_start() {
        // The older row scrolls away as the new one appears, so the count of rows ending like the
        // prompt does not grow, but the count of rows starting like it does.
        let tail = "x".repeat(40);
        let old_row = format!("❯ OLD unrelated request {tail}");
        let prompt = format!("NEW current request {tail}");
        let new_row = format!("❯ {prompt}");
        let frames = Frames::new(
            claude_screen(&["• earlier", old_row.as_str()], "", BUSY_STATUS),
            claude_screen(&["• earlier", old_row.as_str()], &prompt, BUSY_STATUS),
            vec![claude_screen(
                &["• earlier", new_row.as_str()],
                "",
                BUSY_STATUS,
            )],
        );
        let (receipt, _) = submit_frames(&frames, &prompt);
        assert!(receipt.printed, "{receipt:?}");
        assert_eq!(receipt.evidence, "prompt text appeared above the composer");
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn a_new_chat_prompt_sharing_its_edges_with_an_older_row_is_printed_evidence() {
        let header = "The user's request arrived through the configured chat bridge.";
        let trailer = "Complete this request using your normal instructions and tools. Use the \
                       same two lines for every reply, with no tool call between them.";
        let old_prompt =
            format!("{header}\nSource: spaces/example/messages/old\nOld request body\n\n{trailer}");
        let prompt =
            format!("{header}\nSource: spaces/example/messages/new\nNew request body\n\n{trailer}");
        let old_row = format!("❯ {old_prompt}");
        let new_row = format!("❯ {prompt}");
        let frames = Frames::new(
            claude_screen(&["• earlier", old_row.as_str()], "", BUSY_STATUS),
            claude_screen(&["• earlier", old_row.as_str()], &prompt, BUSY_STATUS),
            vec![claude_screen(
                &["• earlier", new_row.as_str()],
                "",
                BUSY_STATUS,
            )],
        );
        let (receipt, _) = submit_frames(&frames, &prompt);
        assert!(receipt.printed, "{receipt:?}");
        assert_eq!(*frames.keys.lock().unwrap(), ["Enter"]);
    }

    #[test]
    fn a_running_turn_marker_above_a_tall_staged_composer_is_not_printed_evidence() {
        /// Frames whose Tab key, which a busy Codex queues with, also takes effect.
        struct TabFrames(Frames);
        impl PromptTerminal for TabFrames {
            fn read_screen(&self, pane_id: &str) -> Result<String> {
                self.0.read_screen(pane_id)
            }
            fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
                self.0.send_text(pane_id, text)
            }
            fn send_keys(&self, pane_id: &str, keys: &str) -> Result<()> {
                self.0.send_keys(pane_id, keys)?;
                if keys == "Tab" {
                    *self.0.phase.lock().unwrap() = 2;
                }
                Ok(())
            }
        }
        let prompt = (0..20)
            .map(|n| format!("request line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let before = "• earlier\n› \u{1b}[2mAsk Codex to do anything\u{1b}[22m\n\n  99% context\n"
            .to_owned();
        // The turn started while the prompt was staged; its progress row sits above a composer
        // of 20 rows, out of the rows `running_turn` searches.
        let mut staged_rows = vec![
            "• earlier".to_owned(),
            "• Working (esc to interrupt)".to_owned(),
        ];
        staged_rows.extend(
            prompt
                .lines()
                .enumerate()
                .map(|(index, line)| format!("{} {line}", if index == 0 { "›" } else { " " })),
        );
        staged_rows.extend([
            String::new(),
            "  tab to queue message                 99% context".to_owned(),
        ]);
        let staged = staged_rows.join("\n") + "\n";
        assert_eq!(running_turn(&staged), Some(false));
        let after = "• earlier\n• Working (esc to interrupt)\n› \u{1b}[2mAsk Codex to do \
                     anything\u{1b}[22m\n\n  99% context\n"
            .to_owned();
        assert_eq!(
            composer_view("codex", &staged).unwrap().transcript,
            composer_view("codex", &after).unwrap().transcript
        );
        let frames = TabFrames(Frames::new(before, staged, vec![after]));
        let receipt = receipt(submit_verified(
            &frames,
            "w1:p1",
            "codex",
            &prompt,
            SubmitTimeouts::default(),
            &Clock::default(),
        ));
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(*frames.0.keys.lock().unwrap(), ["Tab"]);
    }

    #[test]
    fn a_running_turn_marker_shown_while_the_prompt_was_staged_is_not_printed_evidence() {
        // The turn started between the paste and the key, so the marker after the key is not
        // something the key caused.
        let frames = Frames::new(
            claude_screen(&["• earlier"], "", IDLE_STATUS),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![claude_screen(&["• earlier"], "", BUSY_STATUS)],
        );
        let (receipt, _) = submit_frames(&frames, "run the tests");
        assert_eq!(receipt.evidence, "agent reports an active turn");
        assert!(!receipt.printed, "{receipt:?}");
    }

    #[test]
    fn a_paste_hint_before_the_paste_may_hide_a_running_turn() {
        // Claude Code draws the hint in place of its status row for a while after any paste, so
        // the screen before this paste cannot tell whether a turn was already running.
        let frames = Frames::new(
            claude_screen(&["• earlier"], "", PASTE_STATUS),
            claude_screen(&["• earlier"], "[Pasted text #924 +10 lines]", PASTE_STATUS),
            vec![claude_screen(&["• earlier"], "", BUSY_STATUS)],
        );
        let (receipt, took) = submit_frames(&frames, claude_busy::PROMPT);
        assert_eq!(receipt.evidence, "agent reports an active turn");
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(took, POLL + PRINT_GRACE);
    }

    #[test]
    fn stop_ends_the_wait_for_printed_evidence_at_the_next_poll() {
        let busy = claude_screen(&["• earlier"], "", BUSY_STATUS);
        let frames = Frames::new(
            busy.clone(),
            claude_screen(&["• earlier"], "run the tests", BUSY_STATUS),
            vec![busy],
        );
        let runtime = StoppingClock {
            clock: Clock::default(),
            stop_after: Duration::from_millis(500),
        };
        let receipt = receipt(submit_verified(
            &frames,
            "w1:p1",
            "claude",
            "run the tests",
            SubmitTimeouts::default(),
            &runtime,
        ));
        assert!(!receipt.printed, "{receipt:?}");
        assert_eq!(
            runtime.monotonic() - Duration::from_millis(1_000_000),
            Duration::from_millis(500)
        );
    }

    /// A busy Claude Code screen captured from a live pane (task names removed): Claude Code
    /// holds a queued prompt, so its empty composer shows the reverse-video cursor on `P` and the
    /// faint hint `ress up to edit queued messages`. Rows end in CR LF, as Herdr returns them.
    fn captured_busy_claude_with_queue_hint() -> String {
        let rule = format!(
            "\u{1b}[0m\u{1b}[38;2;136;136;136m{}\u{1b}[0m",
            "─".repeat(152)
        );
        [
            "  \u{1b}[0m\u{1b}[38;2;153;153;153mearlier output\u{1b}[0m",
            "\u{1b}[0m\u{1b}[38;2;215;119;87m· \u{1b}[0m\u{1b}[38;2;225;139;107mQuantumizing…\u{1b}[0m\u{1b}[38;2;215;119;87m \u{1b}[0m\u{1b}[38;2;153;153;153m(running UserPromptSubmit hooks… 9/10 · 10m 8s · ↓\u{1b}[0m \u{1b}[0m\u{1b}[38;2;153;153;153m21.2k tokens)\u{1b}[0m",
            &rule,
            "\u{1b}[0m\u{1b}[38;2;153;153;153m❯\u{a0}\u{1b}[0m\u{1b}[7mP\u{1b}[0m\u{1b}[2mress up to edit queued messages\u{1b}[0m",
            &rule,
            "  \u{1b}[0m\u{1b}[38;2;255;107;128m⏵⏵ bypass permissions on\u{1b}[0m\u{1b}[38;2;153;153;153m (shift+tab to cycle) · esc to interrupt · ← 2 agents · ↓ to manage\u{1b}[0m",
            "",
        ]
        .join("\r\n")
    }

    /// An idle Claude Code screen captured from a live pane (transcript removed): the empty
    /// composer shows only the reverse-video cursor cell.
    fn captured_idle_claude() -> String {
        let rule = format!(
            "\u{1b}[0m\u{1b}[38;2;136;136;136m{}\u{1b}[0m",
            "─".repeat(152)
        );
        [
            "\u{1b}[0m\u{1b}[38;2;153;153;153m✻\u{1b}[0m \u{1b}[0m\u{1b}[38;2;153;153;153mCooked for 2m 8s · done 7:09 PM\u{1b}[0m",
            &rule,
            "❯\u{a0}\u{1b}[0m\u{1b}[7m \u{1b}[0m",
            &rule,
            "  \u{1b}[0m\u{1b}[38;2;255;193;7m⏵⏵ auto mode on\u{1b}[0m",
            "",
        ]
        .join("\r\n")
    }

    #[test]
    fn claude_hint_text_and_cursor_in_an_empty_composer_are_not_a_draft() {
        for (label, screen) in [
            ("queue hint", captured_busy_claude_with_queue_hint()),
            ("idle", captured_idle_claude()),
        ] {
            let view = composer_view("claude", &screen).expect(label);
            assert!(composer_is_empty(&view), "{label}: {view:?}");
        }
        let busy = composer_view("claude", &captured_busy_claude_with_queue_hint()).unwrap();
        assert!(busy.composer.contains("ress up to edit queued messages"));
        assert_eq!(
            running_turn(&captured_busy_claude_with_queue_hint()),
            Some(true)
        );
        assert_eq!(running_turn(&captured_idle_claude()), Some(false));
        // A typed draft in the same frame does count.
        let drafted = captured_idle_claude().replace(
            "❯\u{a0}\u{1b}[0m\u{1b}[7m \u{1b}[0m",
            "❯\u{a0}muse-wonder-full-1.0\u{1b}[0m\u{1b}[7m \u{1b}[0m",
        );
        let view = composer_view("claude", &drafted).unwrap();
        assert!(!composer_is_empty(&view));
        assert_eq!(view.composer_solid.trim(), "muse-wonder-full-1.0");
    }

    #[test]
    fn a_busy_claude_with_only_the_queue_hint_is_typed_into_and_the_prompt_is_queued() {
        // The hint the captured frame shows: the composer is empty, so the prompt is typed.
        let screen = FakeScreen::new("claude", true);
        screen.state().queued.push("an earlier prompt".to_owned());
        screen.state().queued_hidden = true;
        let receipt = receipt(submit(&screen, "check the build"));
        assert_eq!(receipt.key, "Enter");
        let state = screen.state();
        assert_eq!(state.queued, ["an earlier prompt", "check the build"]);
        assert!(state.composer.is_empty());
    }

    #[test]
    fn clear_composer_deletes_every_row_and_proves_the_composer_empty() {
        for harness in ["claude", "codex"] {
            let screen = FakeScreen::new(harness, false);
            screen.state().composer = "first row\nsecond row\nthird".to_owned();
            let clock = Clock::default();
            let keys = clear_composer(&screen, "w1:p1", harness, &clock).expect(harness);
            assert!(keys >= 6, "{harness}: {keys}");
            let state = screen.state();
            assert!(state.composer.is_empty(), "{harness}: {:?}", state.composer);
            assert!(state.submitted.is_empty() && state.queued.is_empty());
            assert!(
                state
                    .keys
                    .iter()
                    .all(|key| ["ctrl+e", "ctrl+u", "backspace"].contains(&key.as_str())),
                "{harness}: {:?}",
                state.keys
            );
        }
    }

    #[test]
    fn clear_composer_fails_without_progress_and_reports_the_text_left() {
        let screen = FakeScreen::new("claude", false);
        screen.state().composer = "keep me".to_owned();
        screen.state().ignore_clear_keys = true;
        let error = clear_composer(&screen, "w1:p1", "claude", &Clock::default()).unwrap_err();
        assert!(error.contains("did not change"), "{error}");
        assert_eq!(screen.state().composer, "keep me");
        assert_eq!(screen.state().keys, ["ctrl+e", "ctrl+u", "backspace"]);
    }

    #[test]
    fn a_prompt_a_working_claude_never_accepts_is_withdrawn_and_reported_not_typed() {
        let screen = FakeScreen::new("claude", true);
        screen.state().drop_submit_keys = true;
        let clock = Clock::default();
        clock.withdraw.store(true, Ordering::SeqCst);
        let outcome = submit_verified(
            &screen,
            "w1:p1",
            "claude",
            "check the build",
            SubmitTimeouts::default(),
            &clock,
        );
        let reason = refusal(outcome);
        assert!(
            reason.contains("taken back out of the composer"),
            "{reason}"
        );
        let state = screen.state();
        assert!(state.composer.is_empty(), "{:?}", state.composer);
        assert!(state.queued.is_empty() && state.submitted.is_empty());
        assert!(state.keys.iter().filter(|key| *key == "Enter").count() > 1);
        assert!(state.keys.iter().any(|key| key == "ctrl+u"));
    }

    #[test]
    fn a_prompt_never_accepted_is_left_staged_without_the_withdrawal_request() {
        let screen = FakeScreen::new("claude", true);
        screen.state().drop_keys = u32::MAX;
        let error = submit(&screen, "check the build").unwrap_err();
        assert!(error.to_string().contains("still staged"), "{error}");
        assert_eq!(screen.state().composer, "check the build");
        assert!(!screen.state().keys.iter().any(|key| key == "ctrl+u"));
    }

    #[test]
    fn a_prompt_that_cannot_be_withdrawn_stays_possibly_submitted() {
        let screen = FakeScreen::new("claude", true);
        screen.state().drop_keys = u32::MAX;
        screen.state().ignore_clear_keys = true;
        let clock = Clock::default();
        clock.withdraw.store(true, Ordering::SeqCst);
        let error = submit_verified(
            &screen,
            "w1:p1",
            "claude",
            "check the build",
            SubmitTimeouts::default(),
            &clock,
        )
        .unwrap_err();
        assert!(error.to_string().contains("possibly submitted"), "{error}");
        assert_eq!(screen.state().composer, "check the build");
    }
}
