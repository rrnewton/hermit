//! A summariser that is ONE plain HTTP call to a Messages API: no agent harness, no conversation,
//! and no tools. `#233 llm-summarizer`.
//!
//! # Why a direct call
//!
//! Summarising a message, or naming a thread, is "here is some text, say what it is". It needs no
//! tool, no memory of the previous answer and no session; every one of those is something that can
//! go wrong and something a prompt injection in other people's channel text could reach for. So
//! this is the smallest thing that can work: one request carrying a system instruction and one
//! user turn, and one response carrying text.
//!
//! # No tools, structurally
//!
//! [`MessagesRequest`] HAS NO `tools` OR `tool_choice` FIELD. Not "an empty list", not "omitted when
//! empty": the type cannot express a tool, so no configuration, prompt or code path can attach
//! one, and a test asserts that the serialized body carries neither key. A model that is offered
//! no tool cannot call one, whatever the fenced text asks of it.
//!
//! # One wire format, two dialects
//!
//! * **anthropic** — `POST {base_url}/v1/messages` with `x-api-key` (read from a file) and
//!   `anthropic-version: 2023-06-01`; the body names the model.
//! * **vertex** — `POST {base_url}/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict`;
//!   the same body without `model` and with `anthropic_version: "vertex-2023-10-16"`. Authenticated
//!   by an optional bearer token (public Google Vertex) and/or an mTLS client identity (a gateway
//!   that speaks the Vertex path and authenticates by client certificate). `x-api-key` is sent on
//!   this dialect only when `api_key_file` is configured explicitly.
//!
//! Both answer `{"content":[{"type":"text","text":"..."}],"stop_reason":"end_turn",...}`. A gateway
//! may answer an error in PLAIN TEXT with an error-code header rather than as JSON; both shapes are
//! reported, briefly and without anything this server sent.
//!
//! # Credentials
//!
//! Read at call time, from files or a command, never from the configuration text, and never
//! logged. A client identity is re-read whenever its file's modification time changes, because the
//! identities a gateway accepts are short-lived and rotate on disk under a running server.
//!
//! # Pace
//!
//! Every call goes through one [`RateLimiter`] shared by summaries and thread names; a 429 pauses
//! all of them for the `Retry-After` the provider asked for. See [`super::rate_limit`].

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::agent::{plain_uncut, PLAIN_TEXT_RULE};
use super::rate_limit::{Priority, RateLimiter};
use super::threads::{parse_thread_answer, ThreadNaming, ThreadRequest, THREAD_RULE};
use super::{Summarizer, Summary, SummaryError, SummaryRequest, PROMPT};
use crate::config::{LlmConfig, LlmDialect, Secret};

/// The slug this backend contributes to [`super::policy_version`].
pub const BACKEND: &str = "messages-api";
/// The `anthropic-version` header the anthropic dialect sends.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The `anthropic_version` body field the vertex dialect sends.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Attempts per call, counting the first: a 429 or an overload is retried, after the wait the
/// provider asked for, at most this many times in all.
pub const MAX_ATTEMPTS: u32 = 3;
/// How long a background call (a thread name) may wait for a rate-limit slot. An interactive call
/// waits at most `timeout_secs`, because somebody is looking at a spinner.
pub const BACKGROUND_MAX_WAIT: Duration = Duration::from_secs(15 * 60);
/// The longest pause a `Retry-After` may impose on every caller. A gateway answering "retry in a
/// day" is believed for this long and no longer.
pub const MAX_PAUSE: Duration = Duration::from_secs(15 * 60);
/// How long a bearer token printed by `bearer_token_command` is reused before the command is run
/// again. Well inside the hour a cloud access token typically lives; dropped early on a 401.
pub const TOKEN_COMMAND_REUSE: Duration = Duration::from_secs(5 * 60);
/// How long `bearer_token_command` may take.
pub const TOKEN_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// The largest credential file read, in bytes. A key or a token is a few hundred bytes; anything
/// past this is the wrong file.
const MAX_SECRET_BYTES: u64 = 16 * 1024;
/// The largest response body read, in bytes. A one-sentence answer is well under a kilobyte.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// How much of a provider's error message is repeated in an error.
const MAX_DETAIL_CHARS: usize = 200;

/// The whole request body. THERE IS NO `tools` OR `tool_choice` FIELD, and there must never be:
/// see the module doc.
///
/// `temperature` is not sent either. Several current models refuse a sampling parameter outright,
/// and a summariser that broke when the operator named a newer model would be a trap; the default
/// sampling of a small model asked for one sentence is already all but deterministic.
#[derive(Debug, Serialize)]
pub struct MessagesRequest<'a> {
    /// The model, on the anthropic dialect. Absent on vertex, where the URL names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    /// The API version, on the vertex dialect. Absent on anthropic, where a header carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_version: Option<&'static str>,
    /// The output budget.
    pub max_tokens: u32,
    /// The instruction: what to produce and in what form. Trusted text only.
    pub system: &'a str,
    /// Exactly one user turn, carrying the fenced channel text.
    pub messages: [UserTurn<'a>; 1],
}

/// The one user turn.
#[derive(Debug, Serialize)]
pub struct UserTurn<'a> {
    /// Always `"user"`.
    pub role: &'static str,
    /// The prompt, with every word another party wrote inside the fence.
    pub content: &'a str,
}

impl<'a> MessagesRequest<'a> {
    /// The body `config`'s dialect sends for one instruction and one prompt.
    #[must_use]
    pub fn new(config: &'a LlmConfig, system: &'a str, prompt: &'a str) -> Self {
        let (model, anthropic_version) = match config.dialect {
            LlmDialect::Anthropic => (Some(config.model.as_str()), None),
            LlmDialect::Vertex => (None, Some(VERTEX_ANTHROPIC_VERSION)),
        };
        Self {
            model,
            anthropic_version,
            max_tokens: config.max_tokens,
            system,
            messages: [UserTurn {
                role: "user",
                content: prompt,
            }],
        }
    }
}

/// Where `config`'s dialect sends a request.
#[must_use]
pub fn endpoint(config: &LlmConfig) -> String {
    match config.dialect {
        LlmDialect::Anthropic => format!("{}/v1/messages", config.base_url),
        LlmDialect::Vertex => format!(
            "{}/projects/{}/locations/{}/publishers/anthropic/models/{}:rawPredict",
            config.base_url,
            config.project.as_deref().unwrap_or_default(),
            config.region.as_deref().unwrap_or_default(),
            config.model
        ),
    }
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

/// What this backend has done, for a test to assert on and an operator to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LlmStats {
    /// HTTP requests sent, retries included.
    pub requests: usize,
    /// Answers that produced text.
    pub answered: usize,
    /// Times a 429 or an overload was waited out and the call tried again.
    pub retried: usize,
    /// Times the HTTP client was built: once, then again whenever the client identity or the CA
    /// bundle changed on disk.
    pub client_builds: usize,
    /// Times `bearer_token_command` was run.
    pub token_commands: usize,
}

/// Summaries, and thread names, from one Messages API call apiece.
pub struct MessagesApiSummarizer {
    config: LlmConfig,
    policy_input: String,
    limiter: RateLimiter,
    client: Mutex<Option<(Fingerprint, reqwest::Client)>>,
    token: tokio::sync::Mutex<Option<(Instant, Secret)>>,
    stats: Mutex<LlmStats>,
}

impl std::fmt::Debug for MessagesApiSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The configuration holds paths, never secrets, but it is still not this type's to print:
        // the dialect and the model are what a reader of a debug line needs.
        f.debug_struct("MessagesApiSummarizer")
            .field("dialect", &self.config.dialect)
            .field("model", &self.config.model)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Which client identity and CA bundle the cached client was built from: each file's modification
/// time and length, or `None` when it is not configured.
type Fingerprint = (Option<(SystemTime, u64)>, Option<(SystemTime, u64)>);

/// One attempt's result.
enum Attempt {
    Answered(String),
    Retry {
        after: Option<Duration>,
        detail: String,
    },
    Failed(SummaryError),
}

impl MessagesApiSummarizer {
    /// Build a summariser for `config`. Reads no credential and opens no connection: both happen
    /// per call, so a deployment whose key file is not there yet still starts, and says so.
    #[must_use]
    pub fn new(config: LlmConfig) -> Self {
        // What decides what a summary SAYS, and nothing else: see `LlmConfig`. Both instructions
        // are in, because one cache version covers summaries and thread names alike.
        let policy_input = format!(
            "{PROMPT}\n{PLAIN_TEXT_RULE}\n{THREAD_RULE}\nbackend={BACKEND}\ndialect={}\nmodel={}\n\
             max_tokens={}",
            config.dialect.as_str(),
            config.model,
            config.max_tokens
        );
        let limiter = RateLimiter::new(config.requests_per_minute, config.burst);
        Self {
            config,
            policy_input,
            limiter,
            client: Mutex::new(None),
            token: tokio::sync::Mutex::new(None),
            stats: Mutex::new(LlmStats::default()),
        }
    }

    /// The configuration this summariser was built from.
    #[must_use]
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// What this backend has done so far.
    #[must_use]
    pub fn stats(&self) -> LlmStats {
        *self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn note(&self, change: impl FnOnce(&mut LlmStats)) {
        change(
            &mut self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// The HTTP client, rebuilt whenever the client identity or the CA bundle changed on disk.
    ///
    /// Public because the reload is a guarantee, and a guarantee only reachable through a TLS
    /// handshake is one no test can state.
    ///
    /// # Errors
    ///
    /// [`SummaryError::NotConfigured`] naming the file that cannot be read or does not hold what
    /// it should.
    pub fn client(&self) -> Result<reqwest::Client, SummaryError> {
        let fingerprint = (
            stamp(
                self.config.client_identity_file.as_deref(),
                "summaries.llm.client_identity_file",
            )?,
            stamp(
                self.config.ca_bundle_file.as_deref(),
                "summaries.llm.ca_bundle_file",
            )?,
        );
        let mut slot = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((held, client)) = slot.as_ref() {
            if *held == fingerprint {
                return Ok(client.clone());
            }
        }
        let client = build_client(&self.config)?;
        *slot = Some((fingerprint, client.clone()));
        drop(slot);
        self.note(|s| s.client_builds += 1);
        if self.config.client_identity_file.is_some() {
            tracing::info!(
                setting = "summaries.llm.client_identity_file",
                "loaded the summariser's client identity (again, if it changed on disk)"
            );
        }
        Ok(client)
    }

    /// The bearer token for this call, if this deployment has one.
    async fn bearer(&self) -> Result<Option<Secret>, SummaryError> {
        if let Some(path) = &self.config.bearer_token_file {
            return read_secret(path, "summaries.llm.bearer_token_file").map(Some);
        }
        let Some(argv) = &self.config.bearer_token_command else {
            return Ok(None);
        };
        let mut held = self.token.lock().await;
        if let Some((fetched, token)) = held.as_ref() {
            if fetched.elapsed() < TOKEN_COMMAND_REUSE {
                return Ok(Some(token.clone()));
            }
        }
        self.note(|s| s.token_commands += 1);
        let token = run_token_command(argv).await?;
        *held = Some((Instant::now(), token.clone()));
        Ok(Some(token))
    }

    /// Forget a command-issued token, so the next call runs the command again. After a 401.
    async fn forget_token(&self) {
        *self.token.lock().await = None;
    }

    /// One call: wait for a slot, send, and retry what the provider asked to be retried.
    async fn complete(
        &self,
        priority: Priority,
        system: &str,
        prompt: &str,
    ) -> Result<String, SummaryError> {
        // ONE deadline for every wait this call does for a slot, retries included: a reader is
        // not asked to sit through `timeout_secs` of queueing once per attempt.
        let max_wait = match priority {
            Priority::Interactive => Duration::from_secs(self.config.timeout_secs),
            Priority::Background => BACKGROUND_MAX_WAIT,
        };
        let began = Instant::now();
        let deadline = began + max_wait;
        let mut last = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let patience = deadline.saturating_duration_since(Instant::now());
            if let Err(wait) = self.limiter.acquire(priority, patience).await {
                return Err(SummaryError::Transport(format!(
                    "the summariser's rate limit ({} a minute, burst {}) has no free call for \
                     another {}s{}",
                    self.config.requests_per_minute,
                    self.config.burst,
                    wait.as_secs(),
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!("; the provider last said {last}")
                    }
                )));
            }
            match self.send(system, prompt).await {
                Attempt::Answered(text) => {
                    tracing::debug!(
                        attempts = attempt + 1,
                        elapsed_ms = began.elapsed().as_millis(),
                        "the Messages API answered"
                    );
                    return Ok(text);
                }
                Attempt::Failed(error) => return Err(error),
                Attempt::Retry { after, detail } => {
                    // The provider's own wait when it gave one, else 1s, 2s, 4s.
                    let wait = after
                        .unwrap_or_else(|| Duration::from_secs(1 << attempt))
                        .min(MAX_PAUSE);
                    tracing::warn!(
                        backend = BACKEND,
                        dialect = self.config.dialect.as_str(),
                        wait_ms = wait.as_millis(),
                        attempt = attempt + 1,
                        %detail,
                        "the model provider asked us to slow down; every summariser call now waits"
                    );
                    self.limiter.pause_for(wait);
                    self.note(|s| s.retried += 1);
                    last = detail;
                }
            }
        }
        Err(SummaryError::Transport(format!(
            "the model provider was still refusing after {MAX_ATTEMPTS} attempts: {last}"
        )))
    }

    /// One HTTP request and its interpretation.
    async fn send(&self, system: &str, prompt: &str) -> Attempt {
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return Attempt::Failed(error),
        };
        let body = MessagesRequest::new(&self.config, system, prompt);
        let mut request = client
            .post(endpoint(&self.config))
            .timeout(Duration::from_secs(self.config.timeout_secs))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body);
        if self.config.dialect == LlmDialect::Anthropic {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
        }
        // `x-api-key` whenever a key file is configured: always on anthropic, where it is
        // required, and on vertex ONLY because somebody configured it there on purpose.
        if let Some(path) = &self.config.api_key_file {
            match read_secret(path, "summaries.llm.api_key_file") {
                Ok(key) => request = request.header("x-api-key", sensitive(&key)),
                Err(error) => return Attempt::Failed(error),
            }
        }
        let bearer = match self.bearer().await {
            Ok(bearer) => bearer,
            Err(error) => return Attempt::Failed(error),
        };
        if let Some(token) = &bearer {
            request = request.header(
                reqwest::header::AUTHORIZATION,
                sensitive(&Secret::new(format!("Bearer {}", token.expose()))),
            );
        }
        for (name, value) in &self.config.extra_headers {
            request = request.header(name.as_str(), value.as_str());
        }

        self.note(|s| s.requests += 1);
        let started = Instant::now();
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Failed(SummaryError::Transport(if error.is_timeout() {
                    format!(
                        "the model provider did not answer within {}s",
                        self.config.timeout_secs
                    )
                } else {
                    // The whole chain: reqwest's own line is "error sending request", and the
                    // reason — refused, unresolvable, a TLS handshake the gateway rejected — is
                    // underneath it.
                    format!(
                        "the model provider could not be reached: {}",
                        causes(&error)
                    )
                }));
            }
        };
        let status = response.status();
        let retry_after = retry_after(response.headers());
        let error_code = error_code(response.headers());
        let body = match read_bounded(response).await {
            Ok(body) => body,
            Err(error) => return Attempt::Failed(error),
        };
        if status.is_success() {
            return self.interpret(&body, started.elapsed());
        }
        let detail = describe_failure(status, error_code.as_deref(), &body);
        match status.as_u16() {
            // 529 is the Anthropic API's "overloaded"; 503 is a gateway's. Both mean "later".
            429 | 503 | 529 => Attempt::Retry {
                after: retry_after,
                detail,
            },
            401 | 403 => {
                if status.as_u16() == 401 && self.config.bearer_token_command.is_some() {
                    self.forget_token().await;
                }
                Attempt::Failed(SummaryError::Refused(detail))
            }
            400..=499 => Attempt::Failed(SummaryError::Refused(detail)),
            _ => Attempt::Failed(SummaryError::Transport(detail)),
        }
    }

    /// Turn a successful response into text, or say why it is not one.
    fn interpret(&self, body: &[u8], elapsed: Duration) -> Attempt {
        let Ok(parsed) = serde_json::from_slice::<MessagesResponse>(body) else {
            return Attempt::Failed(SummaryError::Shape(
                "the model provider answered 2xx with something that is not a Messages API \
                 response"
                    .to_owned(),
            ));
        };
        let usage = parsed.usage.unwrap_or_default();
        tracing::info!(
            backend = BACKEND,
            dialect = self.config.dialect.as_str(),
            model = %self.config.model,
            answer_ms = elapsed.as_millis(),
            stop_reason = parsed.stop_reason.as_deref().unwrap_or("(none)"),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            "the Messages API answered one summariser call"
        );
        if parsed.stop_reason.as_deref() == Some("refusal") {
            return Attempt::Failed(SummaryError::Refused(
                "the model declined to answer".to_owned(),
            ));
        }
        // Text blocks only. Anything else — a thinking block from a model that thinks by default —
        // is not part of the answer, and there is no tool block because no tool was offered.
        let text: String = parsed
            .content
            .iter()
            .filter(|block| block.kind == "text")
            .filter_map(|block| block.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            return Attempt::Failed(SummaryError::Shape(format!(
                "the model answered with no text (stop_reason {})",
                parsed.stop_reason.as_deref().unwrap_or("absent")
            )));
        }
        self.note(|s| s.answered += 1);
        if parsed.stop_reason.as_deref() == Some("max_tokens") {
            return Attempt::Answered(marked_as_cut(&text));
        }
        Attempt::Answered(text)
    }
}

/// An answer the provider stopped at `max_tokens`, marked as cut the way one past
/// [`super::hard_cap_chars`] is: at the last whole word, ending "…".
///
/// `max_tokens` is a second cap, applied by the provider rather than here, and it stops mid-word as
/// readily as between words; unmarked, its cut reached the page as a sentence ending in half a word
/// with nothing to say so. `#257 summary-truncation`. The last word is dropped unless the text
/// already ends between words; with no whole word before it, the text is kept as it came.
fn marked_as_cut(text: &str) -> String {
    let trimmed = text.trim_end();
    let whole = if trimmed.len() < text.len() {
        trimmed
    } else {
        trimmed
            .rfind(char::is_whitespace)
            .map_or("", |at| trimmed[..at].trim_end())
    };
    let kept = if whole.is_empty() { trimmed } else { whole };
    format!("{kept}…")
}

/// The first configured credential or TLS file that is not there, as the setting naming it and
/// what is wrong with it. Existence only: the contents are read, and judged, per call.
///
/// For the startup log, `client-config` and diagnostics, which all want to say what is broken
/// before anybody asks for a summary.
#[must_use]
pub fn unusable_file(config: &LlmConfig) -> Option<(&'static str, String)> {
    [
        (&config.api_key_file, "summaries.llm.api_key_file"),
        (&config.bearer_token_file, "summaries.llm.bearer_token_file"),
        (
            &config.client_identity_file,
            "summaries.llm.client_identity_file",
        ),
        (&config.ca_bundle_file, "summaries.llm.ca_bundle_file"),
    ]
    .into_iter()
    .find_map(|(path, setting)| {
        let path = path.as_ref()?;
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => None,
            Ok(_) => Some((setting, format!("{} is not a file", path.display()))),
            Err(error) => Some((
                setting,
                format!("{} cannot be read: {error}", path.display()),
            )),
        }
    })
}

/// A header value that never reaches a debug print.
fn sensitive(secret: &Secret) -> reqwest::header::HeaderValue {
    // A value with a byte a header cannot carry is a corrupt credential file; sending an empty
    // header gets the provider's 401, which names the problem better than a panic here would.
    let mut value = reqwest::header::HeaderValue::from_str(secret.expose())
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
    value.set_sensitive(true);
    value
}

/// Read one credential file: bounded, trimmed, non-empty. The path is logged on failure; the
/// contents never are.
fn read_secret(path: &std::path::Path, setting: &'static str) -> Result<Secret, SummaryError> {
    let refuse = |why: String| {
        tracing::warn!(setting, path = %path.display(), why, "a summariser credential file is unusable");
        SummaryError::NotConfigured(setting)
    };
    let metadata = std::fs::metadata(path).map_err(|error| refuse(error.to_string()))?;
    if metadata.len() > MAX_SECRET_BYTES {
        return Err(refuse(format!(
            "it is {} bytes, more than a credential is",
            metadata.len()
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|error| refuse(error.to_string()))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(refuse("it is empty".to_owned()));
    }
    Ok(Secret::new(trimmed))
}

/// Run `bearer_token_command` and take its standard output as the token.
async fn run_token_command(argv: &[String]) -> Result<Secret, SummaryError> {
    const SETTING: &str = "summaries.llm.bearer_token_command";
    let refuse = |why: String| {
        tracing::warn!(
            setting = SETTING,
            why,
            "the summariser's bearer token command failed"
        );
        SummaryError::NotConfigured(SETTING)
    };
    let Some((program, args)) = argv.split_first() else {
        return Err(refuse("it names no program".to_owned()));
    };
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TOKEN_COMMAND_TIMEOUT, child)
        .await
        .map_err(|_| {
            refuse(format!(
                "it did not finish within {}s",
                TOKEN_COMMAND_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| refuse(format!("{program:?} could not be run: {error}")))?;
    if !output.status.success() {
        // stderr, briefly, because it is where the command says why ("not logged in"). stdout is
        // never repeated: on a command that half-worked it may be the token.
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(refuse(format!(
            "it exited with {}: {}",
            output.status,
            excerpt(&stderr)
        )));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| refuse("its output is not text".to_owned()))?;
    let token = token.trim();
    if token.is_empty()
        || token.len() as u64 > MAX_SECRET_BYTES
        || token.contains(char::is_whitespace)
    {
        return Err(refuse(
            "its output is not a single token on one line".to_owned(),
        ));
    }
    Ok(Secret::new(token))
}

/// A file's modification time and length, or `None` when no file is configured.
fn stamp(
    path: Option<&std::path::Path>,
    setting: &'static str,
) -> Result<Option<(SystemTime, u64)>, SummaryError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(path).map_err(|error| {
        tracing::warn!(setting, path = %path.display(), %error, "a summariser TLS file is unreadable");
        SummaryError::NotConfigured(setting)
    })?;
    Ok(Some((
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        metadata.len(),
    )))
}

/// Build the HTTP client: the bundled public roots, plus the CA bundle and the client identity
/// when they are configured. Redirects are not followed: a credential header must not be carried
/// anywhere the operator did not name.
fn build_client(config: &LlmConfig) -> Result<reqwest::Client, SummaryError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("vibe-talk/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(path) = &config.ca_bundle_file {
        const SETTING: &str = "summaries.llm.ca_bundle_file";
        let refuse = |why: String| {
            tracing::warn!(setting = SETTING, path = %path.display(), why, "the summariser's CA bundle is unusable");
            SummaryError::NotConfigured(SETTING)
        };
        let pem = std::fs::read(path).map_err(|error| refuse(error.to_string()))?;
        let roots = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|error| refuse(error.to_string()))?;
        if roots.is_empty() {
            return Err(refuse("it holds no certificate".to_owned()));
        }
        for root in roots {
            builder = builder.add_root_certificate(root);
        }
    }
    if let Some(path) = &config.client_identity_file {
        const SETTING: &str = "summaries.llm.client_identity_file";
        let refuse = |why: String| {
            // reqwest's and rustls's messages name the problem ("private key or certificate not
            // found", "keys do not match") and never repeat key material.
            tracing::warn!(setting = SETTING, path = %path.display(), why, "the summariser's client identity is unusable");
            SummaryError::NotConfigured(SETTING)
        };
        let pem = std::fs::read(path).map_err(|error| refuse(error.to_string()))?;
        let identity =
            reqwest::Identity::from_pem(&pem).map_err(|error| refuse(error.to_string()))?;
        builder = builder.identity(identity);
        // The identity is only checked against its certificate when the client is built, so a
        // failure there is this file's too.
        return builder.build().map_err(|error| refuse(causes(&error)));
    }
    builder.build().map_err(|error| {
        SummaryError::Transport(format!(
            "the summariser's HTTP client could not be built: {error}"
        ))
    })
}

/// An error and every cause under it, on one line: reqwest's own message for a TLS failure is
/// "builder error", and the reason is two sources down.
fn causes(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        next = cause.source();
    }
    line
}

/// `Retry-After`, in seconds. An HTTP-date form is not trusted against a clock that may disagree
/// with the provider's, and falls back to the backoff.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// A gateway's error code, from any response header whose name says it carries one.
fn error_code(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers.iter().find_map(|(name, value)| {
        let name = name.as_str();
        (name.ends_with("error-code") || name.ends_with("error_code"))
            .then(|| value.to_str().ok().map(excerpt))
            .flatten()
    })
}

/// Read a response body, refusing one larger than any answer this server asked for.
async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, SummaryError> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_RESPONSE_BYTES {
                    return Err(SummaryError::Shape(format!(
                        "the model provider's answer is larger than {MAX_RESPONSE_BYTES} bytes"
                    )));
                }
            }
            Ok(None) => return Ok(body),
            Err(error) => {
                return Err(SummaryError::Transport(format!(
                    "the model provider's answer was cut off: {error}"
                )))
            }
        }
    }
}

/// One line saying what a failed response said: the status, a gateway's error code, and the
/// provider's message — from a JSON error body when there is one, else the first words of a
/// plain-text one.
fn describe_failure(status: reqwest::StatusCode, code: Option<&str>, body: &[u8]) -> String {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            let error = json.get("error").unwrap_or(&json);
            error
                .get("message")
                .or_else(|| error.get("type"))
                .and_then(serde_json::Value::as_str)
                .map(excerpt)
        })
        .unwrap_or_else(|| excerpt(&String::from_utf8_lossy(body)));
    let mut line = format!("HTTP {}", status.as_u16());
    if let Some(code) = code {
        line.push_str(&format!(" [{code}]"));
    }
    if !message.is_empty() {
        line.push_str(": ");
        line.push_str(&message);
    }
    line
}

/// The first [`MAX_DETAIL_CHARS`] characters of `text`, on one line.
fn excerpt(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_DETAIL_CHARS {
        let cut: String = flat.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[async_trait]
impl Summarizer for MessagesApiSummarizer {
    fn describe(&self) -> &'static str {
        match self.config.dialect {
            LlmDialect::Anthropic => {
                "one Messages API call per summary, with no tools (anthropic dialect)"
            }
            LlmDialect::Vertex => {
                "one Messages API call per summary, with no tools (vertex dialect)"
            }
        }
    }

    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn policy_input(&self) -> &str {
        &self.policy_input
    }

    fn unavailable_reason(&self) -> Option<String> {
        unusable_file(&self.config).map(|(setting, why)| format!("{setting} {why}"))
    }

    fn names_threads(&self) -> bool {
        true
    }

    async fn summarize(&self, request: &SummaryRequest<'_>) -> Result<Summary, SummaryError> {
        let system = format!(
            "{PLAIN_TEXT_RULE}\nKeep the answer under {} characters.",
            request.target_chars
        );
        let reply = self
            .complete(Priority::Interactive, &system, request.prompt())
            .await?;
        // Asked for `target_chars`, stored uncut and cut only well past it when served: see
        // `plain_uncut`. `#257 summary-truncation`, `#208 persistent-cache`. A `max_tokens` stop
        // was already marked by `marked_as_cut`; it is the provider's cut, not ours, so it leaves
        // `cut_at` unset.
        let (summary, cut_at) = plain_uncut(&reply, request.target_chars);
        if summary.trim().is_empty() || summary == crate::summary::NO_TEXT {
            return Err(SummaryError::Shape(
                "the model answered with no text at all".to_owned(),
            ));
        }
        Ok(Summary {
            text: summary,
            cut_at,
        })
    }

    async fn name_thread(&self, request: &ThreadRequest<'_>) -> Result<ThreadNaming, SummaryError> {
        let system = format!(
            "{THREAD_RULE}\nKeep the sentence under {} characters.",
            request.sentence_chars
        );
        let reply = self
            .complete(Priority::Background, &system, request.prompt())
            .await?;
        parse_thread_answer(&reply, request.sentence_chars)
    }
}

#[cfg(test)]
mod tests;
