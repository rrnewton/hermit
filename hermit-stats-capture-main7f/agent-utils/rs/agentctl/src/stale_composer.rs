//! Clearing text left unsubmitted in the coordinator's composer, for
//! `agentctl chat run --stale-composer-clear-after DURATION`.
//!
//! A verified submission never types into a composer that already holds text, so a draft left
//! in the coordinator's input box holds every chat request behind it for as long as it stays
//! there. With this option the bridge watches such a draft while it blocks a request. Once the
//! draft has stayed unchanged for the configured duration, and the agent is not working, the
//! bridge saves it to a private file in the state directory, deletes it from the composer with
//! editing keys, proves the composer empty, and posts it back to the chat once, through the
//! bridge's own outbound helper, behind a `[chat-bridge WARNING]` notice. Delivery then resumes.
//!
//! The draft's text goes only to the backup file and to that notice: every log line describes
//! it by its length.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::AgentRuntime;
use crate::chat_runtime::{OutboundOutcome, RootMessageSubmission, RootMessageTransport};
use crate::client::HerdrClient;
use crate::submission::{self, PromptTerminal};

/// The start of every notice the bridge posts about a cleared draft, so a reader can tell it
/// from a message the agent wrote.
pub const NOTICE_PREFIX: &str = "[chat-bridge WARNING] ";
/// The directory, under the bridge state directory, that holds one file for each cleared draft.
pub const BACKUP_DIRECTORY: &str = "composer-backups";
/// The shortest accepted wait: a draft is the owner's own text, so it is never cleared quickly.
pub const MIN_WINDOW: Duration = Duration::from_secs(60);
/// The longest accepted wait.
pub const MAX_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The longest wait for the pane's host-wide delivery lock before a clear is put off.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// The largest notice body the outbound helper accepts.
const MAX_NOTICE_BYTES: usize = 30_000;
/// Attempts to post one notice, all under the same operation ID, while the helper reports a
/// failure that did not apply and may succeed if repeated.
const NOTICE_ATTEMPTS: usize = 3;

/// Parse a duration such as `30m`, `90s`, `2h` or `1800` (seconds), between [`MIN_WINDOW`] and
/// [`MAX_WINDOW`].
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => value.split_at(index),
        None => (value, "s"),
    };
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return Err("expected a whole number with unit s, m or h, such as 30m".to_owned()),
    };
    let count: u64 = number
        .parse()
        .map_err(|_| "expected a whole number with unit s, m or h, such as 30m".to_owned())?;
    let duration = Duration::from_secs(count.saturating_mul(multiplier));
    if duration < MIN_WINDOW || duration > MAX_WINDOW {
        return Err("the duration must be at least 1m and at most 168h".to_owned());
    }
    Ok(duration)
}

/// `duration` in words, as the notice says it: `30 minutes`, `1 hour`, `90 seconds`.
#[must_use]
pub fn duration_words(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (count, unit) = if seconds % 3_600 == 0 {
        (seconds / 3_600, "hour")
    } else if seconds % 60 == 0 {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// The notice posted for a cleared draft: the warning line, then the draft with each line
/// quoted. A draft too long for one message is cut at a line, and the notice says so.
#[must_use]
pub fn notice_text(window: Duration, text: &str) -> String {
    let mut notice = format!(
        "{NOTICE_PREFIX}The following text was left in the input box and cleared out \
         automatically after not changing for {}:",
        duration_words(window)
    );
    const CUT: &str = "\n> [cut here; the whole text is in the bridge's local backup file]";
    for line in text.lines() {
        let quoted = format!("\n> {line}");
        if notice.len() + quoted.len() + CUT.len() > MAX_NOTICE_BYTES {
            notice.push_str(CUT);
            return notice;
        }
        notice.push_str(&quoted);
    }
    notice
}

/// The text the composer of `harness` shows in `screen`, one string with a line for each
/// composer row, or `None` when the screen shows no recognisable composer or an empty one.
/// Faint hint text and the cursor cell do not make a composer hold text: see
/// [`submission::composer_is_empty`].
#[must_use]
pub fn composer_text(harness: &str, screen: &str) -> Option<String> {
    let view = submission::composer_view(harness, screen)?;
    if submission::composer_is_empty(&view) {
        return None;
    }
    // The first row starts after the marker column, and the harness draws the rows below it
    // from column 0, indented to line up with the first row's text: the marker column plus the
    // gap after the marker.
    let rows: Vec<&str> = view.composer.lines().map(str::trim_end).collect();
    let gap = rows.first().map_or(0, |row| {
        row.chars().take_while(|c| c.is_whitespace()).count()
    });
    let unindent = |row: &str, width: usize| {
        let blank = row
            .chars()
            .take(width)
            .take_while(|c| c.is_whitespace())
            .count();
        row.chars().skip(blank).collect::<String>()
    };
    let rows: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| unindent(row, if index == 0 { gap } else { gap + 1 }))
        .collect();
    let first = rows.iter().position(|row| !row.trim().is_empty())?;
    let last = rows.iter().rposition(|row| !row.trim().is_empty())?;
    Some(rows[first..=last].join("\n"))
}

/// What the bridge read from the coordinator's pane: Herdr's status, the agent Herdr recognises
/// there, and the visible screen with its styling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneObservation {
    /// Herdr's native agent status, such as `idle`, `done` or `working`.
    pub status: String,
    /// The harness Herdr recognises in the pane, such as `claude` or `codex`.
    pub harness: String,
    /// The visible rows, with SGR styling, as a composer read takes them.
    pub screen: String,
}

/// The coordinator's pane, as the stale-composer clear uses it.
pub trait ComposerPane {
    /// Read Herdr's status for the pane and its screen.
    fn observe(&self) -> Result<PaneObservation, String>;
    /// Hold the pane's host-wide delivery lock, which every agentctl sender takes while it types
    /// into the pane, until the returned value is dropped.
    fn lock(&self) -> Result<Box<dyn std::any::Any>, String>;
    /// The pane's terminal, for the keys that clear the composer.
    fn terminal(&self) -> &dyn PromptTerminal;
    /// The Herdr pane identifier.
    fn pane_id(&self) -> &str;
}

/// What one [`StaleComposer::step`] did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// No request is held by a draft, or the composer is empty: nothing is watched.
    Idle,
    /// A draft is watched and has not stayed unchanged for the whole wait yet.
    Watching,
    /// The draft is due, but was not cleared at this step; the reason says why. A later step
    /// tries again.
    Held(String),
    /// The draft was saved and cleared, and the composer proved empty.
    Cleared {
        /// The private file that holds the draft.
        backup: PathBuf,
        /// The provider message ID of the notice, or `None` when posting it failed.
        notice: Option<String>,
    },
    /// Saving or clearing failed: no notice was posted. The draft may be partly deleted, but
    /// once a backup was written it holds the whole draft. The wait starts again.
    Failed(String),
}

/// A draft being watched: its text and when it was first seen with that text.
#[derive(Clone, Debug)]
struct Watched {
    text: String,
    since: Instant,
}

/// The stale-composer clear for one `chat run`.
#[derive(Debug)]
pub struct StaleComposer {
    window: Duration,
    backup_directory: PathBuf,
    channel_id: String,
    watched: Option<Watched>,
}

fn is_ready(status: &str) -> bool {
    matches!(status, "idle" | "done")
}

/// Why `observation` shows an agent that must not have its composer cleared now, if it does: it
/// is working by Herdr's report or by its screen, or its screen cannot tell.
fn busy(observation: &PaneObservation) -> Option<String> {
    if !is_ready(&observation.status) {
        return Some(format!("Herdr reports the agent {}", observation.status));
    }
    match submission::running_turn(&observation.screen) {
        Some(false) => None,
        Some(true) => Some("the screen shows a running turn".to_owned()),
        None => Some("the screen cannot tell whether a turn runs".to_owned()),
    }
}

fn describe(text: &str) -> String {
    format!(
        "{} characters on {} line(s)",
        text.chars().count(),
        text.lines().count()
    )
}

impl StaleComposer {
    /// Clear a draft after it has stayed unchanged for `window`, saving it under
    /// `backup_directory` and posting the notice to `channel_id`.
    #[must_use]
    pub fn new(window: Duration, backup_directory: PathBuf, channel_id: String) -> Self {
        Self {
            window,
            backup_directory,
            channel_id,
            watched: None,
        }
    }

    /// Look at the composer once, at `now`. `pane` is the coordinator's pane while some request
    /// is waiting because the composer held text when the bridge last tried to type it, and
    /// `None` otherwise; only while one waits is the draft watched. Each line `log` receives
    /// describes a step without the draft's text.
    pub fn step(
        &mut self,
        now: Instant,
        pane: Option<&dyn ComposerPane>,
        transport: &mut dyn RootMessageTransport,
        runtime: &dyn AgentRuntime,
        log: &mut dyn FnMut(String),
    ) -> Step {
        let Some(pane) = pane else {
            if self.watched.take().is_some() {
                log(
                    "agentctl: stale composer: no request is held by the draft any more; \
                     it is no longer watched"
                        .to_owned(),
                );
            }
            return Step::Idle;
        };
        let observation = match pane.observe() {
            Ok(observation) => observation,
            Err(error) => {
                return Step::Held(format!("the pane could not be read: {error}"));
            }
        };
        let Some(text) = composer_text(&observation.harness, &observation.screen) else {
            if self.watched.take().is_some() {
                log(
                    "agentctl: stale composer: the composer no longer holds the draft; \
                     it is no longer watched"
                        .to_owned(),
                );
            }
            return Step::Idle;
        };
        match &self.watched {
            Some(watched) if watched.text == text => {}
            previous => {
                let changed = previous.is_some();
                log(format!(
                    "agentctl: stale composer: pane {} composer {} unsubmitted text ({}); it is \
                     cleared if it stays unchanged for {} while a request waits",
                    pane.pane_id(),
                    if changed {
                        "changed; it now holds"
                    } else {
                        "holds"
                    },
                    describe(&text),
                    duration_words(self.window)
                ));
                self.watched = Some(Watched { text, since: now });
                return Step::Watching;
            }
        }
        let watched = self.watched.clone().expect("a draft is watched");
        if now.saturating_duration_since(watched.since) < self.window {
            return Step::Watching;
        }
        if let Some(reason) = busy(&observation) {
            return Step::Held(format!("the draft is due, but {reason}"));
        }
        let lock = match pane.lock() {
            Ok(lock) => lock,
            Err(error) => {
                return Step::Held(format!(
                    "the draft is due, but the pane's delivery lock is held: {error}"
                ))
            }
        };
        // Under the lock no agentctl sender types into the pane, so what this read shows is
        // what the keys below act on, unless the owner types at the same moment.
        let observation = match pane.observe() {
            Ok(observation) => observation,
            Err(error) => {
                return Step::Held(format!(
                    "the draft is due, but the pane could not be read again: {error}"
                ))
            }
        };
        if let Some(reason) = busy(&observation) {
            return Step::Held(format!("the draft is due, but {reason}"));
        }
        if composer_text(&observation.harness, &observation.screen).as_deref()
            != Some(watched.text.as_str())
        {
            self.watched = None;
            log(format!(
                "agentctl: stale composer: pane {} composer changed just before it was due; \
                 nothing was cleared",
                pane.pane_id()
            ));
            return Step::Held("the composer changed just before the draft was due".to_owned());
        }
        let backup = match write_backup(&self.backup_directory, &watched.text) {
            Ok(path) => path,
            Err(error) => {
                self.restart(now);
                let detail =
                    format!("the backup could not be written, so nothing was cleared: {error}");
                log(format!("agentctl: stale composer: {detail}"));
                return Step::Failed(detail);
            }
        };
        log(format!(
            "agentctl: stale composer: saved the draft ({}) to {}; clearing pane {} composer",
            describe(&watched.text),
            backup.display(),
            pane.pane_id()
        ));
        let cleared = submission::clear_composer(
            pane.terminal(),
            pane.pane_id(),
            &observation.harness,
            runtime,
        );
        drop(lock);
        match cleared {
            Ok(keys) => log(format!(
                "agentctl: stale composer: pane {} composer is empty after {keys} key(s)",
                pane.pane_id()
            )),
            Err(error) => {
                self.restart(now);
                let detail = format!(
                    "the composer could not be cleared, so no notice was posted; the draft is \
                     saved in {}: {error}",
                    backup.display()
                );
                log(format!("agentctl: stale composer: {detail}"));
                return Step::Failed(detail);
            }
        }
        self.watched = None;
        let notice = self.post_notice(&watched.text, transport, log);
        Step::Cleared { backup, notice }
    }

    /// Start the wait again for the draft being watched, after a failed clear, so a draft that
    /// cannot be cleared is not tried at every step.
    fn restart(&mut self, now: Instant) {
        if let Some(watched) = self.watched.as_mut() {
            watched.since = now;
        }
    }

    fn post_notice(
        &self,
        text: &str,
        transport: &mut dyn RootMessageTransport,
        log: &mut dyn FnMut(String),
    ) -> Option<String> {
        let body = notice_text(self.window, text);
        let request_id = match crate::chat_runtime::random_operation_uuid() {
            Ok(request_id) => request_id,
            Err(error) => {
                log(format!(
                    "agentctl: stale composer: the notice was not posted: no operation ID: {error}"
                ));
                return None;
            }
        };
        for attempt in 1..=NOTICE_ATTEMPTS {
            match transport.publish_root(RootMessageSubmission {
                channel_id: &self.channel_id,
                body: &body,
                request_id: &request_id,
            }) {
                Ok(message_id) => {
                    log(format!(
                        "agentctl: stale composer: posted the notice to {} as {message_id}",
                        self.channel_id
                    ));
                    return Some(message_id);
                }
                Err(failure)
                    if attempt < NOTICE_ATTEMPTS
                        && failure.retryable
                        && failure.outcome == OutboundOutcome::NotApplied => {}
                Err(failure) => {
                    log(format!(
                        "agentctl: stale composer: the notice was not posted ({failure}); the \
                         draft is only in the local backup"
                    ));
                    return None;
                }
            }
        }
        None
    }
}

/// Save `text` in a new file of mode 0600 in `directory`, which is created with mode 0700 when
/// missing, and flush it to disk.
fn write_backup(directory: &Path, text: &str) -> std::io::Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let stamp: String = crate::chat_runtime::log_timestamp()
        .chars()
        .filter(|c| *c != ':' && *c != '-')
        .collect();
    let millis = crate::chat_runtime::unix_millis() % 1_000;
    for suffix in 0..100_u32 {
        let name = if suffix == 0 {
            format!("{stamp}-{millis:03}.txt")
        } else {
            format!("{stamp}-{millis:03}-{suffix}.txt")
        };
        let path = directory.join(name);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::File::open(directory)?.sync_all()?;
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free backup file name",
    ))
}

/// The coordinator's pane through Herdr.
pub struct HerdrComposerPane<'a> {
    client: &'a HerdrClient,
    pane_id: String,
    runtime: &'a dyn AgentRuntime,
    take_lock: bool,
    expect_terminal: Option<String>,
}

impl<'a> HerdrComposerPane<'a> {
    /// The pane `pane_id`. With `take_lock`, [`ComposerPane::lock`] takes the host-wide delivery
    /// lock that agentctl senders take; with `expect_terminal`, each key is refused by Herdr
    /// unless the pane still holds that terminal.
    #[must_use]
    pub fn new(
        client: &'a HerdrClient,
        pane_id: String,
        runtime: &'a dyn AgentRuntime,
        take_lock: bool,
        expect_terminal: Option<String>,
    ) -> Self {
        Self {
            client,
            pane_id,
            runtime,
            take_lock,
            expect_terminal,
        }
    }
}

impl ComposerPane for HerdrComposerPane<'_> {
    fn observe(&self) -> Result<PaneObservation, String> {
        let cancelled = || self.runtime.cancelled();
        let info = self
            .client
            .pane_info_with_cancellation(&self.pane_id, &cancelled)
            .map_err(|error| error.to_string())?;
        if let (Some(expected), Some(actual)) = (&self.expect_terminal, &info.terminal_id) {
            if expected != actual {
                return Err(format!(
                    "pane {} now holds terminal {actual:?}, not {expected:?}",
                    self.pane_id
                ));
            }
        }
        let screen = self
            .client
            .read_screen_with_cancellation(&self.pane_id, &cancelled)
            .map_err(|error| error.to_string())?;
        Ok(PaneObservation {
            status: info.status,
            harness: info.agent.unwrap_or_default(),
            screen,
        })
    }

    fn lock(&self) -> Result<Box<dyn std::any::Any>, String> {
        if !self.take_lock {
            return Ok(Box::new(()));
        }
        crate::agent::lock_target_within(&self.pane_id, "host-wide target lock", LOCK_WAIT)
            .map(|lock| Box::new(lock) as Box<dyn std::any::Any>)
            .map_err(|error| error.to_string())
    }

    fn terminal(&self) -> &dyn PromptTerminal {
        self
    }

    fn pane_id(&self) -> &str {
        &self.pane_id
    }
}

impl PromptTerminal for HerdrComposerPane<'_> {
    fn read_screen(&self, pane_id: &str) -> crate::error::Result<String> {
        self.client
            .read_screen_with_cancellation(pane_id, &|| self.runtime.cancelled())
    }

    fn send_text(&self, pane_id: &str, _text: &str) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(format!(
            "pane {pane_id}: the stale-composer clear never types text"
        )))
    }

    fn send_keys(&self, pane_id: &str, keys: &str) -> crate::error::Result<()> {
        self.client
            .send_keys_expecting(pane_id, keys, self.expect_terminal.as_deref(), &|| {
                self.runtime.cancelled()
            })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::chat_runtime::OutboundFailure;
    use crate::submission::fake::FakeScreen;

    const DRAFT: &str = "muse-wonder-full-1.0";
    const CHANNEL: &str = "spaces/example";
    const WINDOW: Duration = Duration::from_secs(30 * 60);

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "agentctl-stale-{label}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create test directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct Clock(AtomicU64);

    impl AgentRuntime for Clock {
        fn monotonic(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) {
            self.0.fetch_add(
                u64::try_from(duration.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
        }
    }

    /// The coordinator's pane, drawn by a fake Claude Code composer.
    struct FakePane {
        screen: FakeScreen,
        status: Mutex<String>,
        locks: AtomicU64,
    }

    impl FakePane {
        fn new(draft: &str) -> Self {
            let screen = FakeScreen::new("claude", false);
            screen.state().composer = draft.to_owned();
            Self {
                screen,
                status: Mutex::new("idle".to_owned()),
                locks: AtomicU64::new(0),
            }
        }
        fn composer(&self) -> String {
            self.screen.state().composer.clone()
        }
        fn keys(&self) -> Vec<String> {
            self.screen.state().keys.clone()
        }
    }

    impl ComposerPane for FakePane {
        fn observe(&self) -> Result<PaneObservation, String> {
            Ok(PaneObservation {
                status: self.status.lock().unwrap().clone(),
                harness: "claude".to_owned(),
                screen: self
                    .screen
                    .read_screen("w1:p1")
                    .map_err(|error| error.to_string())?,
            })
        }
        fn lock(&self) -> Result<Box<dyn std::any::Any>, String> {
            self.locks.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()))
        }
        fn terminal(&self) -> &dyn PromptTerminal {
            &self.screen
        }
        fn pane_id(&self) -> &str {
            "w1:p1"
        }
    }

    /// The outbound helper: records each root message and answers with `result`.
    #[derive(Default)]
    struct FakeTransport {
        posted: Vec<(String, String, String)>,
        failures: Vec<OutboundFailure>,
    }

    impl RootMessageTransport for FakeTransport {
        fn publish_root(
            &mut self,
            submission: RootMessageSubmission<'_>,
        ) -> Result<String, OutboundFailure> {
            self.posted.push((
                submission.channel_id.to_owned(),
                submission.body.to_owned(),
                submission.request_id.to_owned(),
            ));
            if self.failures.is_empty() {
                Ok(format!("spaces/example/messages/{}", self.posted.len()))
            } else {
                Err(self.failures.remove(0))
            }
        }
    }

    struct Harness {
        directory: TestDirectory,
        guard: StaleComposer,
        transport: FakeTransport,
        clock: Clock,
        logged: Vec<String>,
        start: Instant,
    }

    impl Harness {
        fn new(label: &str) -> Self {
            let directory = TestDirectory::new(label);
            let guard = StaleComposer::new(
                WINDOW,
                directory.0.join(BACKUP_DIRECTORY),
                CHANNEL.to_owned(),
            );
            Self {
                directory,
                guard,
                transport: FakeTransport::default(),
                clock: Clock::default(),
                logged: Vec::new(),
                start: Instant::now(),
            }
        }

        /// One step `minutes` after the start, while a request is held by the draft.
        fn step(&mut self, pane: &dyn ComposerPane, minutes: u64) -> Step {
            self.step_at(Some(pane), Duration::from_secs(minutes * 60))
        }

        fn step_at(&mut self, pane: Option<&dyn ComposerPane>, after: Duration) -> Step {
            let mut logged = Vec::new();
            let step = self.guard.step(
                self.start + after,
                pane,
                &mut self.transport,
                &self.clock,
                &mut |line| logged.push(line),
            );
            self.logged.append(&mut logged);
            step
        }

        fn backups(&self) -> Vec<PathBuf> {
            match fs::read_dir(self.directory.0.join(BACKUP_DIRECTORY)) {
                Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
                Err(_) => Vec::new(),
            }
        }
    }

    fn expected_notice(text: &str) -> String {
        format!(
            "[chat-bridge WARNING] The following text was left in the input box and cleared out \
             automatically after not changing for 30 minutes:\n> {text}"
        )
    }

    #[test]
    fn an_unchanged_draft_is_saved_cleared_and_posted_once_after_the_wait() {
        let pane = FakePane::new(DRAFT);
        let mut harness = Harness::new("unchanged");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        assert_eq!(harness.step(&pane, 29), Step::Watching);
        assert_eq!(
            pane.composer(),
            DRAFT,
            "nothing is cleared before the wait ends"
        );
        assert!(pane.keys().is_empty());

        let Step::Cleared { backup, notice } = harness.step(&pane, 30) else {
            panic!("expected a clear: {:?}", harness.logged);
        };
        assert_eq!(notice.as_deref(), Some("spaces/example/messages/1"));
        assert_eq!(pane.composer(), "");
        assert_eq!(pane.locks.load(Ordering::SeqCst), 1);
        assert!(
            pane.screen.state().submitted.is_empty(),
            "nothing was submitted"
        );

        // The backup holds the draft, private to the owner.
        assert_eq!(fs::read_to_string(&backup).unwrap(), format!("{DRAFT}\n"));
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(backup.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(harness.backups(), [backup]);

        // The notice, exactly once, with the prefix, to the configured channel.
        assert_eq!(harness.transport.posted.len(), 1);
        let (channel, body, request_id) = &harness.transport.posted[0];
        assert_eq!(channel, CHANNEL);
        assert!(body.starts_with(NOTICE_PREFIX));
        assert_eq!(body, &expected_notice(DRAFT));
        assert!(crate::chat_runtime::validate_operation_uuid(request_id).is_ok());

        // Later steps find an empty composer and post nothing more.
        assert_eq!(harness.step(&pane, 31), Step::Idle);
        assert_eq!(harness.step(&pane, 90), Step::Idle);
        assert_eq!(
            harness.step_at(None, Duration::from_secs(100 * 60)),
            Step::Idle
        );
        assert_eq!(harness.transport.posted.len(), 1);

        // No log line holds the draft.
        assert!(!harness.logged.is_empty());
        assert!(
            harness.logged.iter().all(|line| !line.contains(DRAFT)),
            "{:?}",
            harness.logged
        );
    }

    #[test]
    fn a_changed_draft_starts_the_wait_again_and_is_not_cleared_early() {
        let pane = FakePane::new("draft a");
        let mut harness = Harness::new("changed");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        pane.screen.state().composer = "draft ab".to_owned();
        assert_eq!(harness.step(&pane, 20), Step::Watching);
        // Thirty minutes after the first sight, but only twenty after the change.
        assert_eq!(harness.step(&pane, 40), Step::Watching);
        assert_eq!(pane.composer(), "draft ab");
        assert!(pane.keys().is_empty());
        assert!(harness.transport.posted.is_empty());
        assert!(harness.backups().is_empty());
        assert!(matches!(harness.step(&pane, 50), Step::Cleared { .. }));
        assert_eq!(harness.transport.posted[0].1, expected_notice("draft ab"));
    }

    #[test]
    fn a_failed_clear_leaves_the_draft_posts_nothing_and_waits_again() {
        let pane = FakePane::new(DRAFT);
        pane.screen.state().ignore_clear_keys = true;
        let mut harness = Harness::new("failed");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        let Step::Failed(detail) = harness.step(&pane, 30) else {
            panic!("expected a failed clear");
        };
        assert!(detail.contains("could not be cleared"), "{detail}");
        assert_eq!(pane.composer(), DRAFT);
        assert!(harness.transport.posted.is_empty());
        let backups = harness.backups();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read_to_string(&backups[0]).unwrap(),
            format!("{DRAFT}\n")
        );
        // The wait starts again, so the keys are not pressed at every step.
        let keys = pane.keys().len();
        assert_eq!(harness.step(&pane, 31), Step::Watching);
        assert_eq!(harness.step(&pane, 59), Step::Watching);
        assert_eq!(pane.keys().len(), keys);
        assert!(harness.logged.iter().all(|line| !line.contains(DRAFT)));
    }

    #[test]
    fn a_working_agent_keeps_its_draft_until_it_is_idle() {
        let pane = FakePane::new(DRAFT);
        *pane.status.lock().unwrap() = "working".to_owned();
        let mut harness = Harness::new("working");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        let Step::Held(reason) = harness.step(&pane, 45) else {
            panic!("expected the draft to be held");
        };
        assert!(reason.contains("working"), "{reason}");
        // Herdr says idle, but the screen shows a running turn.
        *pane.status.lock().unwrap() = "idle".to_owned();
        pane.screen.state().busy = true;
        assert!(matches!(harness.step(&pane, 46), Step::Held(_)));
        assert_eq!(pane.composer(), DRAFT);
        assert!(pane.keys().is_empty());
        assert!(harness.transport.posted.is_empty());
        // Once it is idle, the draft that stayed unchanged all along is cleared.
        pane.screen.state().busy = false;
        assert!(matches!(harness.step(&pane, 47), Step::Cleared { .. }));
    }

    #[test]
    fn nothing_is_watched_while_no_request_is_held() {
        let pane = FakePane::new(DRAFT);
        let mut harness = Harness::new("unblocked");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        assert_eq!(
            harness.step_at(None, Duration::from_secs(10 * 60)),
            Step::Idle
        );
        // Held again: the wait starts from here.
        assert_eq!(harness.step(&pane, 20), Step::Watching);
        assert_eq!(harness.step(&pane, 45), Step::Watching);
        assert!(matches!(harness.step(&pane, 50), Step::Cleared { .. }));
    }

    #[test]
    fn a_failed_notice_is_retried_under_one_operation_id_and_then_given_up() {
        let pane = FakePane::new(DRAFT);
        let mut harness = Harness::new("notice");
        let retryable = OutboundFailure {
            code: "unavailable".to_owned(),
            detail: "try again".to_owned(),
            outcome: OutboundOutcome::NotApplied,
            retryable: true,
        };
        harness.transport.failures = vec![retryable.clone(), retryable];
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        let Step::Cleared { notice, .. } = harness.step(&pane, 30) else {
            panic!("expected a clear");
        };
        assert_eq!(notice.as_deref(), Some("spaces/example/messages/3"));
        let ids: Vec<&str> = harness
            .transport
            .posted
            .iter()
            .map(|(_, _, id)| id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| *id == ids[0]));

        // An outcome that may have applied is never posted again.
        let pane = FakePane::new(DRAFT);
        let mut harness = Harness::new("notice-unknown");
        harness.transport.failures = vec![OutboundFailure {
            code: "timeout".to_owned(),
            detail: "no answer".to_owned(),
            outcome: OutboundOutcome::Unknown,
            retryable: true,
        }];
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        assert!(matches!(
            harness.step(&pane, 30),
            Step::Cleared { notice: None, .. }
        ));
        assert_eq!(harness.transport.posted.len(), 1);
    }

    #[test]
    fn composer_text_keeps_every_row_without_the_gap_after_the_marker() {
        let screen = FakeScreen::new("claude", false);
        screen.state().composer = "first row\n  indented row\nlast".to_owned();
        assert_eq!(
            composer_text("claude", &screen.render()).as_deref(),
            Some("first row\n  indented row\nlast")
        );
        screen.state().composer.clear();
        assert_eq!(composer_text("claude", &screen.render()), None);
        screen.state().queued.push("queued".to_owned());
        assert_eq!(composer_text("claude", &screen.render()), None, "hint text");
        let codex = FakeScreen::new("codex", false);
        assert_eq!(composer_text("codex", &codex.render()), None, "placeholder");
        codex.state().composer = "a codex draft".to_owned();
        assert_eq!(
            composer_text("codex", &codex.render()).as_deref(),
            Some("a codex draft")
        );
    }

    #[test]
    fn durations_parse_and_read_as_the_notice_says_them() {
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_duration("1800"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_duration("1800s"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7_200)));
        assert_eq!(parse_duration("1m"), Ok(MIN_WINDOW));
        for bad in ["59s", "0", "", "m", "30 m", "1d", "-5m", "169h", "1.5h"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        assert_eq!(duration_words(Duration::from_secs(1_800)), "30 minutes");
        assert_eq!(duration_words(Duration::from_secs(60)), "1 minute");
        assert_eq!(duration_words(Duration::from_secs(3_600)), "1 hour");
        assert_eq!(duration_words(Duration::from_secs(5_400)), "90 minutes");
        assert_eq!(duration_words(Duration::from_secs(90)), "90 seconds");
        assert_eq!(
            notice_text(Duration::from_secs(7_200), "two\nlines"),
            "[chat-bridge WARNING] The following text was left in the input box and cleared out \
             automatically after not changing for 2 hours:\n> two\n> lines"
        );
        let long = "x".repeat(200);
        let text = vec![long.as_str(); 400].join("\n");
        let notice = notice_text(WINDOW, &text);
        assert!(notice.len() <= MAX_NOTICE_BYTES);
        assert!(
            notice.ends_with("local backup file]"),
            "{}",
            &notice[notice.len() - 80..]
        );
    }

    /// A Herdr stand-in for one Claude Code pane whose status, composer and key log are files
    /// beside it. `pane send-keys` records each key, deletes the composer on `ctrl+u`, and fails
    /// while a `fail-keys` file exists.
    const FAKE_HERDR: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
case "$1 $2" in
  "pane get")
    printf '{"result":{"pane":{"pane_id":"%s","workspace_id":"w1","cwd":"/work","agent":"claude","agent_status":"%s","terminal_id":"term-1"}}}\n' "$3" "$(cat "$dir/status")" ;;
  "pane read")
    rule='────────────────────────────────────────'
    printf '\033[0m\033[38;2;153;153;153m✻\033[0m Cooked for 2m 8s\r\n%s\r\n' "$rule"
    if [ -s "$dir/composer" ]; then
      printf '❯\302\240%s\033[0m\033[7m \033[0m\r\n' "$(cat "$dir/composer")"
    else
      printf '❯\302\240\033[0m\033[7m \033[0m\r\n'
    fi
    printf '%s\r\n  \033[0m\033[38;2;255;193;7m⏵⏵ auto mode on\033[0m\r\n' "$rule" ;;
  "pane send-keys")
    shift 2
    if [ "$1" = --expect-terminal ]; then shift 2; fi
    printf '%s\n' "$2" >> "$dir/keys"
    if [ -e "$dir/fail-keys" ]; then echo "fake herdr: send-keys refused" >&2; exit 1; fi
    if [ "$2" = ctrl+u ]; then : > "$dir/composer"; fi
    printf '{"result":{}}\n' ;;
  *)
    echo "fake herdr: unsupported $*" >&2; exit 2 ;;
esac
"#;

    struct FakeHerdr {
        directory: TestDirectory,
        client: HerdrClient,
    }

    impl FakeHerdr {
        fn new(label: &str, draft: &str) -> Self {
            let directory = TestDirectory::new(label);
            let executable = directory.0.join("herdr");
            fs::write(&executable, FAKE_HERDR).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(directory.0.join("status"), "idle").unwrap();
            fs::write(directory.0.join("composer"), draft).unwrap();
            let client = HerdrClient::with_executable("direct", &executable).unwrap();
            Self { directory, client }
        }
        fn file(&self, name: &str) -> String {
            fs::read_to_string(self.directory.0.join(name)).unwrap_or_default()
        }
    }

    #[test]
    fn through_herdr_an_unchanged_draft_is_cleared_and_posted_once() {
        let herdr = FakeHerdr::new("herdr-clear", DRAFT);
        let clock = Clock::default();
        let pane = HerdrComposerPane::new(
            &herdr.client,
            "w1:p1".to_owned(),
            &clock,
            false,
            Some("term-1".to_owned()),
        );
        let observed = pane.observe().unwrap();
        assert_eq!(
            composer_text("claude", &observed.screen).as_deref(),
            Some(DRAFT)
        );

        let mut harness = Harness::new("herdr-clear-state");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        assert_eq!(harness.step(&pane, 10), Step::Watching);
        assert_eq!(herdr.file("keys"), "");
        let Step::Cleared { backup, notice } = harness.step(&pane, 30) else {
            panic!("expected a clear: {:?}", harness.logged);
        };
        assert!(notice.is_some());
        assert_eq!(herdr.file("composer"), "");
        assert_eq!(herdr.file("keys"), "ctrl+e\nctrl+u\n");
        assert_eq!(fs::read_to_string(&backup).unwrap(), format!("{DRAFT}\n"));
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(harness.transport.posted.len(), 1);
        assert_eq!(harness.transport.posted[0].1, expected_notice(DRAFT));
        assert_eq!(harness.step(&pane, 31), Step::Idle);
        assert_eq!(harness.transport.posted.len(), 1);
    }

    #[test]
    fn through_herdr_a_refused_clear_leaves_the_draft_and_posts_nothing() {
        let herdr = FakeHerdr::new("herdr-refused", DRAFT);
        fs::write(herdr.directory.0.join("fail-keys"), "").unwrap();
        let clock = Clock::default();
        let pane = HerdrComposerPane::new(&herdr.client, "w1:p1".to_owned(), &clock, false, None);
        let mut harness = Harness::new("herdr-refused-state");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        let Step::Failed(detail) = harness.step(&pane, 30) else {
            panic!("expected a failed clear: {:?}", harness.logged);
        };
        assert!(detail.contains("send-keys refused"), "{detail}");
        assert_eq!(herdr.file("composer"), DRAFT);
        assert_eq!(herdr.file("keys"), "ctrl+e\n");
        assert!(harness.transport.posted.is_empty());

        // A working pane is never touched.
        let herdr = FakeHerdr::new("herdr-working", DRAFT);
        fs::write(herdr.directory.0.join("status"), "working").unwrap();
        let pane = HerdrComposerPane::new(&herdr.client, "w1:p1".to_owned(), &clock, false, None);
        let mut harness = Harness::new("herdr-working-state");
        assert_eq!(harness.step(&pane, 0), Step::Watching);
        assert!(matches!(harness.step(&pane, 30), Step::Held(_)));
        assert_eq!(herdr.file("keys"), "");
        assert_eq!(herdr.file("composer"), DRAFT);
    }
}
