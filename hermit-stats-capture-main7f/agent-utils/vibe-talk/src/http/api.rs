//! Request handlers.
//!
//! Every handler that touches a channel goes through the same three steps, in this order:
//! authorize the caller, resolve the channel against the configured allowlist, then act. Doing
//! them in that order means an unauthenticated caller cannot use error messages to discover which
//! channel snowflakes are configured.

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::auth::{self, AuthError, Scope};
use crate::chat::ChatError;
use crate::conversation::{VoiceSession, VoiceSessionError};
use crate::elevenlabs::{SignedUrl, SignedUrlError};
use crate::model::{ChannelId, ChannelInfo, Message, MessageId, UserId};
use crate::ops::{self, OpError};
use crate::retrieval::Resolution;
use crate::speech::SpeechError;
use crate::state::AppState;
use crate::store::{ConversationId, ConversationSummary, ReadMark, Speaker, StoreError, Turn};
use crate::summary::DigestEntry;
use crate::untrusted;

use crate::contract::LiveDelivery;
pub use crate::contract::{ApiErrorBody, ClientConfigResponse, MessagesResponse, TimelineResponse};

/// An error that can be returned to a caller.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    detail: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
        }
    }

    /// A request this server understood and will not carry out as written.
    ///
    /// One code, `bad_request`, because a client branches on the code and there is nothing useful
    /// to branch on here: the detail says what to send instead, and every use of this is a shape
    /// the caller can fix by reading it.
    fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.code,
                detail: self.detail,
            }),
        )
            .into_response()
    }
}

impl From<AuthError> for ApiError {
    fn from(value: AuthError) -> Self {
        match value {
            AuthError::Unauthenticated => Self::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                value.to_string(),
            ),
            AuthError::Forbidden => {
                Self::new(StatusCode::FORBIDDEN, "forbidden", value.to_string())
            }
        }
    }
}

impl From<ChatError> for ApiError {
    fn from(value: ChatError) -> Self {
        match value.cause() {
            ChatError::Refused(_) => {
                Self::new(StatusCode::BAD_REQUEST, "refused", value.to_string())
            }
            // `#203 incremental-refresh`. 410 rather than 400: nothing about the request was wrong
            // when it was issued; the position it names is gone, and a newest read issues another.
            ChatError::CursorExpired(_) => {
                Self::new(StatusCode::GONE, "cursor_expired", value.to_string())
            }
            _ => Self::new(StatusCode::BAD_GATEWAY, "chat_error", value.to_string()),
        }
    }
}

impl From<SignedUrlError> for ApiError {
    fn from(value: SignedUrlError) -> Self {
        let code = value.code();
        let status = match value {
            // Not the caller's fault and not fixable by retrying: the operator has to set a value
            // and restart. 503 says "this server is not able to do this right now", which is the
            // truth, and it is distinguishable from ElevenLabs having refused us.
            SignedUrlError::NotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
            // An upstream failure, reported as one. Not flattened into a 500, and never into a
            // 200 with a URL that would fail later inside the browser.
            SignedUrlError::Transport(_)
            | SignedUrlError::Status { .. }
            | SignedUrlError::Shape(_) => StatusCode::BAD_GATEWAY,
        };
        // `Display` for every variant is already redacted at construction; see
        // `SignedUrlError::from_response`.
        Self::new(status, code, value.to_string())
    }
}

impl From<VoiceSessionError> for ApiError {
    fn from(value: VoiceSessionError) -> Self {
        let code = value.code();
        let status = match &value {
            VoiceSessionError::NotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
            VoiceSessionError::Provider(SignedUrlError::NotConfigured(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            VoiceSessionError::Provider(_) => StatusCode::BAD_GATEWAY,
        };
        Self::new(status, code, value.to_string())
    }
}

impl From<SpeechError> for ApiError {
    fn from(value: SpeechError) -> Self {
        let code = value.code();
        let status = match value {
            // A missing provider setting requires operator action rather than a retry.
            SpeechError::NotConfigured { .. } => StatusCode::SERVICE_UNAVAILABLE,
            // The CALLER asked for something impossible, and no vendor call was made. 422 rather
            // than 502: nothing upstream went wrong, and a reader must not be told the voice
            // service is broken because they tapped a message with no words in it.
            SpeechError::Empty => StatusCode::UNPROCESSABLE_ENTITY,
            SpeechError::Backend { .. } => StatusCode::BAD_GATEWAY,
            SpeechError::BrowserPlaybackRequired => StatusCode::CONFLICT,
        };
        // Provider adapters redact their errors before crossing the shared speech boundary.
        Self::new(status, code, value.to_string())
    }
}

impl From<OpError> for ApiError {
    fn from(value: OpError) -> Self {
        let code = value.code();
        let status = match value {
            OpError::UnknownChannel | OpError::UnknownMessage => StatusCode::NOT_FOUND,
            OpError::EmptyQuery
            | OpError::InvalidCursor
            | OpError::InvalidRange
            | OpError::InvalidHours
            | OpError::HoursWithSpan
            | OpError::CursorMismatch => StatusCode::BAD_REQUEST,
            OpError::ChannelNotWritable => StatusCode::FORBIDDEN,
            // A fallback only: the reply route handles this itself, because an `ApiError` body
            // carries a code and prose and this case has to carry the UNSENT TEXT. Reaching here
            // would mean some other caller grew a split path without deciding what to do with the
            // remainder, so it is a loud upstream failure rather than a quiet 500.
            OpError::PartiallyPosted { .. } => StatusCode::BAD_GATEWAY,
            // Only ever one channel's part of a catch-up, never a whole answer; a gateway timeout
            // is what it would be if it were.
            OpError::NotReadInTime => StatusCode::GATEWAY_TIMEOUT,
            OpError::Chat(inner) => return Self::from(inner),
            OpError::Store(inner) => return Self::from(inner),
            OpError::Summarizer(inner) => {
                let code = inner.code();
                // 503 for both: a summariser that is not configured and one that cannot be
                // reached are equally "this server cannot do this right now", and neither is the
                // caller's fault. The CODE tells them apart, which is what a client branches on.
                return Self::new(StatusCode::SERVICE_UNAVAILABLE, code, inner.to_string());
            }
        };
        Self::new(status, code, value.to_string())
    }
}

impl From<StoreError> for ApiError {
    fn from(value: StoreError) -> Self {
        let code = value.code();
        let status = match value {
            // Not the caller's fault and not fixable by retrying: the operator has to configure a
            // path and restart. Same reasoning, and the same status, as a missing ElevenLabs key.
            StoreError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            StoreError::NotFound => StatusCode::NOT_FOUND,
            StoreError::BadId(_) => StatusCode::BAD_REQUEST,
            // 413 rather than 400: the request was well formed, it is the amount that is refused,
            // and the client's correct response is to stop appending rather than to retry.
            StoreError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            StoreError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, code, value.to_string())
    }
}

fn require(headers: &HeaderMap, state: &AppState, scope: Scope) -> Result<Scope, ApiError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    Ok(auth::authorize(header, &state.config.auth, scope)?)
}

/// The write scope, taken from the `Authorization` header and nothing else.
///
/// `#41 post-gate-scope-first`. Axum runs extractors in argument order, and `Json`, `Query` and
/// `Path` each refuse a malformed request with their own 4xx. A handler that checked scope in its
/// body therefore told a read-scope or anonymous caller "your JSON is malformed" (422) before it
/// could say "you may not do this at all" (403/401). Taken as the first extractor after `State`,
/// this answers the scope question first, so a caller without write scope gets the same refusal
/// whatever it sent, and the parsers tell it nothing about what these routes accept.
pub struct WriteScope;

impl axum::extract::FromRequestParts<AppState> for WriteScope {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        require(&parts.headers, state, Scope::Write)?;
        Ok(Self)
    }
}

/// The read scope, taken from the `Authorization` header and nothing else, carrying the scope the
/// credential actually holds — a write token passes too, and a handler may still tell them apart.
///
/// `#42 api-scope-first`. The same argument as [`WriteScope`], for every route a read token may
/// use: taken straight after `State`, it answers 401 before `Path`, `Query` or `Json` can answer
/// 400 or 422, so the parsers tell an anonymous caller nothing about what the route accepts.
pub struct ReadScope(pub Scope);

impl axum::extract::FromRequestParts<AppState> for ReadScope {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(require(&parts.headers, state, Scope::Read)?))
    }
}

/// The speech ticket in the path, checked before anything else in the request is read.
///
/// `#42 api-scope-first`. On the two ticket routes the ticket IS the credential — it stands in for
/// the bearer token an `<audio src>` cannot send — so it is checked first for the same reason the
/// scopes are: a caller without a live ticket gets the same 404 whatever query or body it sent. A
/// ticket that does not even decode is just another unknown one.
pub struct SpeechTicket(pub String);

impl axum::extract::FromRequestParts<AppState> for SpeechTicket {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Ok(Path(ticket)) =
            <Path<String> as axum::extract::FromRequestParts<AppState>>::from_request_parts(
                parts, state,
            )
            .await
        else {
            return Err(unknown_speech_ticket());
        };
        if !state.speech_tickets.knows(&ticket) {
            return Err(unknown_speech_ticket());
        }
        Ok(Self(ticket))
    }
}

/// The SAME answer for expired and never-existed. Distinguishing them would let a caller probe for
/// the difference, and neither is something the reader can act on differently.
fn unknown_speech_ticket() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_speech_ticket",
        "that speech ticket is unknown or has expired; turn read-aloud on again",
    )
}

fn require_ingest(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let presented = auth::bearer_token(header).ok_or(AuthError::Unauthenticated)?;
    // When ingestion is disabled there is no credential that can authorize this route. Keep that
    // indistinguishable from a wrong credential instead of revealing the route/configuration to
    // an anonymous caller with a special 404.
    let configured = state
        .config
        .ingest
        .token
        .as_ref()
        .ok_or(AuthError::Unauthenticated)?;
    if !configured.matches(presented) {
        return Err(AuthError::Unauthenticated.into());
    }
    Ok(())
}

/// A normalized event delivered by an external chat adapter.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngestChange {
    /// A new message, in the same normalized form returned by channel reads.
    Create {
        /// The newly created message.
        message: Message,
    },
    /// A complete replacement for an existing message.
    Update {
        /// The message after the update.
        message: Message,
    },
    /// Removal of a message.
    Delete {
        /// Channel containing the removed message.
        channel_id: ChannelId,
        /// Provider-normalized id of the removed message.
        message_id: MessageId,
    },
}

/// One adapter delivery, de-duplicated within this process's bounded recent-event window.
///
/// Producers MUST send at most one request per channel at a time and wait for its response before
/// sending the next event for that channel. `event_id` is opaque, so arrival order is the only
/// ordering information this endpoint has; concurrent requests can arrive in either order.
#[derive(Debug, Deserialize)]
pub struct IngestEventRequest {
    /// Stable retry key for this one provider event.
    pub event_id: String,
    /// True for catch-up/history delivered while a subscription resumes, false only for an event
    /// the adapter knows arrived live.
    pub historical: bool,
    /// The normalized change.
    #[serde(flatten)]
    pub change: IngestChange,
}

/// Acknowledgement returned to an adapter.
#[derive(Debug, Serialize)]
pub struct IngestEventResponse {
    /// True only when this request entered the live hub.
    pub accepted: bool,
    /// True when the same event id was already accepted for this channel.
    pub duplicate: bool,
}

/// A content-free signal that one allowlisted channel may have changed.
///
/// The server deliberately ignores the upstream event body. Its configured provider performs the
/// authoritative read, through the same cursor the periodic reconciliation poll uses.
#[derive(Debug, Deserialize)]
pub struct IngestHintRequest {
    /// Allowlisted channel to read now.
    pub channel_id: ChannelId,
}

/// Maximum decoded JSON request size for one event.
pub const MAX_INGEST_BODY_BYTES: usize = 64 * 1024;

/// Maximum decoded JSON body for one content-free hint.
pub const MAX_HINT_BODY_BYTES: usize = 1_024;

/// `POST /api/v1/live/events` — authenticated provider-neutral push ingestion.
///
/// This credential is deliberately separate from both browser tokens. The route accepts only
/// configured channels, caps the body before JSON extraction, and retains no message history
/// beyond the live hub's bounded reconnect tail. A repeated event id is acknowledged without
/// broadcasting it again while its id remains in this process's bounded de-duplication window.
/// The memory is not durable: a restart, or more than the window's worth of later channel events,
/// permits the retry again. Producers and consumers must therefore tolerate an occasional duplicate.
/// A producer must keep at most one request per channel in flight and settle an uncertain result
/// before sending a later event, because the opaque event id carries no ordering information.
pub async fn ingest_event(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    // Authenticate before reading or parsing the body. Besides bounding unauthorized work, this
    // keeps a caller without the adapter credential from using parse errors to probe the schema.
    require_ingest(request.headers(), &state)?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "live events require Content-Type: application/json",
        ));
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_INGEST_BODY_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("a live event may be at most {MAX_INGEST_BODY_BYTES} bytes"),
            )
        })?;
    let request: IngestEventRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("the live event body is not valid JSON for this API"))?;

    let IngestEventRequest {
        event_id,
        historical,
        change,
    } = request;
    let (kind, channel_id, mut message) = match change {
        IngestChange::Create { message } => (
            crate::live::LiveKind::Create,
            message.channel_id.clone(),
            message,
        ),
        IngestChange::Update { message } => (
            crate::live::LiveKind::Update,
            message.channel_id.clone(),
            message,
        ),
        IngestChange::Delete {
            channel_id,
            message_id,
        } => {
            let tombstone = Message {
                thread: None,
                id: message_id,
                channel_id: channel_id.clone(),
                author: String::new(),
                author_id: UserId(String::new()),
                author_is_bot: false,
                timestamp: String::new(),
                spoken_time: String::new(),
                reply_to: None,
                content: String::new(),
                spoken_content: String::new(),
                content_html: String::new(),
                noise: false,
                reactions: None,
                heading: None,
            };
            (crate::live::LiveKind::Delete, channel_id, tombstone)
        }
    };

    validate_ingest_id("event_id", &event_id, crate::live::MAX_EVENT_ID_BYTES)?;
    validate_ingest_id("message id", message.id.as_str(), 512)?;
    let Some(channel) = state.channel(channel_id.as_str()) else {
        return Err(OpError::UnknownChannel.into());
    };
    // One live source per channel. A channel whose provider this server polls is published by
    // the poller; accepting an adapter's copy too would deliver every change twice.
    if state
        .providers
        .key_for(&channel.id)
        .as_deref()
        .and_then(|key| state.providers.entry(key))
        .is_some_and(|entry| entry.live_poll_seconds > 0)
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "channel_is_polled",
            "this server polls that channel's provider itself, so it does not accept pushed events for it",
        ));
    }
    // The configured zone and the spoken forms are application concerns, not something every
    // adapter should have to reproduce — and an adapter that tried would be choosing how this
    // server's voice reads, which is not an adapter's call. Whatever arrived in these two fields is
    // OVERWRITTEN rather than trusted. Deletes carry neither a timestamp nor a body and render
    // neither field.
    //
    // This is the live path, so it is the one `read new` relays from: a message reaches the page
    // already carrying the body a voice should say, with no second round trip on the leg that is
    // most latency-sensitive.
    //
    // `#217 markdown-blocks`. Nor is the rendered body an adapter's to supply, and of the three it
    // matters most: it is HTML the page inserts as it stands, so only this server's renderer and
    // sanitizer may write it. Cleared here, for every kind, and rendered again on the way out to
    // each page by `live::judge`; no subscriber of the hub ever sees what an adapter put there.
    // The heading goes with it: it names the words of `content_html` the page hides (`#242
    // rich-headers`).
    message.content_html = String::new();
    message.heading = None;
    if kind == crate::live::LiveKind::Delete {
        message.spoken_time = String::new();
        message.spoken_content = String::new();
    } else {
        message.spoken_time = crate::clock::spoken(&message.timestamp, &state.config.timezone);
        message.spoken_content = ops::prepared_body(
            &state,
            &message.content,
            jiff::Timestamp::now().as_millisecond(),
        );
    }
    // `#191 read-aloud-first-lag`: a recently fetched copy no longer says what the message says.
    if kind != crate::live::LiveKind::Create {
        state.read_ahead.forget(&channel_id, &message.id);
    }
    let accepted = state
        .live
        .publish_ingested(event_id, kind, historical, &channel_id, message);
    let status = if accepted {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(IngestEventResponse {
            accepted,
            duplicate: !accepted,
        }),
    )
        .into_response())
}

/// `POST /api/v1/live/hints` — wake one configured provider's existing poll cursor.
///
/// A hint is not a message and carries no chat content. It is safe beside periodic polling because
/// both paths wake the SAME provider reader and advance the SAME in-memory cursor. Repeated hints
/// coalesce before that reader runs; a missed hint is recovered by the provider's configured poll
/// interval. Only channels whose provider actually polls are accepted, because an unconsumed hint
/// would otherwise be acknowledged and silently do nothing.
pub async fn ingest_hint(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    // As for full event ingestion, authenticate before reading or parsing any part of the body.
    require_ingest(request.headers(), &state)?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "live hints require Content-Type: application/json",
        ));
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_HINT_BODY_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("a live hint may be at most {MAX_HINT_BODY_BYTES} bytes"),
            )
        })?;
    let request: IngestHintRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("the live hint body is not valid JSON for this API"))?;
    let Some(channel) = state.channel(request.channel_id.as_str()) else {
        return Err(OpError::UnknownChannel.into());
    };
    let Some(provider_key) = state.providers.key_for(&channel.id) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "channel_has_no_provider",
            "that channel is not routed to a configured provider",
        ));
    };
    let Some(provider) = state.providers.entry(&provider_key) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "channel_has_no_provider",
            "that channel is not routed to a configured provider",
        ));
    };
    if provider.live_poll_seconds == 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "channel_is_not_polled",
            "that channel's provider has no polling cursor for a change hint to wake",
        ));
    }
    let queue_key = state.providers.is_multi().then_some(provider_key.as_str());
    let accepted = state.live.queue_hint(queue_key, &channel.id);
    let status = if accepted {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(IngestEventResponse {
            accepted,
            duplicate: !accepted,
        }),
    )
        .into_response())
}

fn validate_ingest_id(field: &str, value: &str, max_bytes: usize) -> Result<(), ApiError> {
    if value.is_empty() || value.trim() != value {
        return Err(ApiError::bad_request(format!(
            "{field} must be non-empty and have no surrounding whitespace"
        )));
    }
    if value.len() > max_bytes {
        return Err(ApiError::bad_request(format!(
            "{field} exceeds the {max_bytes}-byte limit"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// `GET /healthz` — liveness only. Deliberately says nothing about the configuration.
pub async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "vibe-talk",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Fallback for unrouted paths.
pub async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// Channel listing.
#[derive(Debug, Serialize)]
pub struct ChannelsResponse {
    /// Configured channels, each carrying the operator's local alias when he has set one.
    pub channels: Vec<ChannelInfo>,
}

/// `GET /api/v1/channels`
pub async fn list_channels(
    State(state): State<AppState>,
    _scope: ReadScope,
) -> Result<Json<ChannelsResponse>, ApiError> {
    Ok(Json(ChannelsResponse {
        channels: ops::channels(&state).await,
    }))
}

/// The name the operator wants to call a channel. `#39 channel-alias`.
#[derive(Debug, Deserialize)]
pub struct SetAliasRequest {
    /// What to call it. Trimmed and length-checked by [`crate::store::validate_alias`]; blank is
    /// refused rather than treated as a clear, because DELETE is the clear.
    pub alias: String,
}

/// What this server is now calling a channel, and what it is called elsewhere.
#[derive(Debug, Serialize)]
pub struct AliasResponse {
    /// The channel, with `alias` set or cleared as the call asked.
    pub channel: ChannelInfo,
    /// Said on every alias answer for the same reason [`crate::store::INBOX_NOTICE`] is said on
    /// every inbox answer: this is the place a person expects the name to have changed in Discord.
    pub alias_notice: &'static str,
}

/// The standing statement that a channel alias is local and ours.
pub const ALIAS_NOTICE: &str = "A channel alias is vibe-talk's own name for the channel. It is \
                                stored on this server only: the source chat service is not told, the channel is \
                                not renamed there, and nobody outside this deployment sees it. \
                                Clearing it puts the configured label back.";

/// `PUT /api/v1/channels/{channel_id}/alias`
///
/// WRITE scope, like every other durable write here. **The scope is not what keeps the agent
/// out**, and saying so would be a comfortable overstatement: a hosted voice agent is routinely
/// given the write token, because `post_reply` is a write tool it is meant to use. What keeps a
/// model from renaming the channels it is also reporting on is that there is NO TOOL for it —
/// [`crate::mcp::tool_manifest`] offers none, and [`crate::mcp::protocol::dispatch`] refuses any
/// name it cannot find there. The model is handed the alias and cannot choose it.
pub async fn set_alias(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<SetAliasRequest>,
) -> Result<Response, ApiError> {
    let channel = ops::set_channel_alias(&state, &channel_id, &request.alias).await?;
    Ok(no_store(Json(AliasResponse {
        channel,
        alias_notice: ALIAS_NOTICE,
    })))
}

/// `DELETE /api/v1/channels/{channel_id}/alias` — go back to the configured label.
pub async fn clear_alias(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let channel = ops::clear_channel_alias(&state, &channel_id).await?;
    Ok(no_store(Json(AliasResponse {
        channel,
        alias_notice: ALIAS_NOTICE,
    })))
}

/// `GET /api/v1/agent-tools` — the voice agent's tool manifest.
pub async fn agent_tools(
    State(state): State<AppState>,
    _scope: ReadScope,
) -> Result<Json<serde_json::Value>, ApiError> {
    let approval = crate::send_approval::current(&state).await;
    let manifest = crate::mcp::tool_manifest(&ops::channels(&state).await, approval);
    Ok(Json(serde_json::json!({ "tools": manifest })))
}

/// `GET /api/v1/client-config`
pub async fn client_config(
    State(state): State<AppState>,
    ReadScope(scope): ReadScope,
) -> Result<Json<ClientConfigResponse>, ApiError> {
    // Only a deployment with several providers describes them one by one. With one, the
    // deployment-wide fields below ARE that provider's, and the answer keeps the shape it had
    // before multi-provider support.
    let described = if state.providers.is_multi() {
        state.providers.entries()
    } else {
        &[]
    };
    let providers: Vec<crate::contract::ProviderDescription> = described
        .iter()
        .map(|entry| crate::contract::ProviderDescription {
            key: entry.key.clone(),
            name: entry.client.provider_name().to_owned(),
            channel_registration_supported: entry.client.supports_channel_registration(),
            channel_discovery_supported: entry.client.supports_channel_discovery(),
            upstream_read_mark_supported: entry.client.supports_upstream_read_mark(),
            upstream_clear_supported: entry.client.supports_upstream_clear(),
            threading_supported: entry.client.supports_threading(),
            replies_in_thread: entry.client.replies_in_thread(),
            idempotent_posts_supported: entry.client.supports_idempotent_posts(),
            live_delivery: live_delivery(&state, entry.live_poll_seconds),
            live_poll_seconds: entry.live_poll_seconds,
            self_author_id: entry.client.self_author_id(),
            owner_author_id: entry.client.owner_author_id(),
        })
        .collect();
    let live_poll_seconds = state.config.live_poll_seconds();
    Ok(Json(ClientConfigResponse {
        chat_provider_name: state.chat.provider_name().to_owned(),
        channels: ops::channels(&state).await,
        hidden_channels: if scope >= Scope::Write {
            ops::hidden_channels(&state).await
        } else {
            Vec::new()
        },
        elevenlabs_agent_id: state.config.elevenlabs.agent_id.clone(),
        conversational_voice: state.conversation.describe(),
        read_aloud: state.speech.describe(),
        version: env!("CARGO_PKG_VERSION"),
        live_poll_seconds,
        live_delivery: live_delivery(&state, live_poll_seconds),
        channel_registration_supported: state.chat.supports_channel_registration(),
        channel_discovery_supported: state.chat.supports_channel_discovery(),
        upstream_read_mark_supported: state.chat.supports_upstream_read_mark(),
        upstream_clear_supported: state.chat.supports_upstream_clear(),
        threading_supported: state.chat.supports_threading(),
        replies_in_thread: state.chat.replies_in_thread(),
        idempotent_posts_supported: state.chat.supports_idempotent_posts(),
        speech_prep_enabled: state.config.speakable.enabled,
        replay_enabled: crate::replay::offered(&state.config.replay, &state.config.conversation),
        self_author_id: state.chat.self_author_id(),
        owner_author_id: state.chat.owner_author_id(),
        providers,
        summaries_unavailable: state.summarizer.unavailable_reason(),
        token_scope: scope.into(),
        noise_rules: noise_rules_view(&state).await,
        voice_settings: Some(crate::send_approval::view(&state).await),
        voice_prompt: Some(crate::voice_prompt::view(&state).await),
    }))
}

/// `GET /api/v1/voice-prompt` — the owner's additional instructions and his edited system prompt.
/// `#272 voice-agent-instructions`.
///
/// READ scope, because the prompt they are assembled into is served at read scope too, and it
/// rides on client-config. `no-store`, because it changes when he saves.
pub async fn voice_prompt(
    State(state): State<AppState>,
    _scope: ReadScope,
) -> Result<Response, ApiError> {
    Ok(no_store(Json(crate::voice_prompt::view(&state).await)))
}

/// The largest `PUT /api/v1/voice-prompt` body accepted: both texts at their limits, four bytes a
/// character at worst, escaped, with room for the field names.
pub const VOICE_PROMPT_BODY_BYTES: usize = 8
    * (crate::voice_agent::MAX_INSTRUCTIONS_CHARS + crate::voice_agent::MAX_SYSTEM_PROMPT_CHARS)
    + 1_024;

/// A save from Settings' Instructions editor. Each field absent keeps that text as it is.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoicePromptRequest {
    /// The additional instructions; empty removes them.
    #[serde(default)]
    pub instructions: Option<String>,
    /// The base prompt; empty, or the shipped prompt itself, restores the shipped one.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// `PUT /api/v1/voice-prompt` — save the owner's instructions, his system prompt, or both.
/// `#272 voice-agent-instructions`.
///
/// WRITE scope, and — as with [`set_voice_settings`] — **the scope is not what keeps the voice
/// agent out**: no tool reaches this route. Neither text can change what the agent may do; that
/// section is chosen from the token and the send setting when the prompt is assembled. 413
/// `too_large` for a text over its limit, 503 `storage_not_configured` with no store.
pub async fn set_voice_prompt(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<VoicePromptRequest>,
) -> Result<Response, ApiError> {
    if request.instructions.is_none() && request.system_prompt.is_none() {
        return Err(ApiError::bad_request(
            "send `instructions`, `system_prompt`, or both; an empty request would report success \
             for having changed nothing",
        ));
    }
    let settings = crate::voice_prompt::set(
        &state,
        crate::voice_prompt::Change {
            instructions: request.instructions,
            system_prompt: request.system_prompt,
        },
    )
    .await?;
    Ok(no_store(Json(settings)))
}

/// `GET /api/v1/voice-settings` — whether the voice agent asks before it sends.
/// `#264 voice-send-approval`.
///
/// READ scope: it says how the voice agent behaves, which a reader may know, and it rides on
/// client-config too. `no-store`, because it changes when the owner flips it.
pub async fn voice_settings(
    State(state): State<AppState>,
    _scope: ReadScope,
) -> Result<Response, ApiError> {
    Ok(no_store(Json(crate::send_approval::view(&state).await)))
}

/// The owner's answer to "Ask me before the voice agent sends".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSettingsRequest {
    /// True: `post_reply` proposes and the owner confirms. False: it posts at once.
    pub ask_before_send: bool,
}

/// `PUT /api/v1/voice-settings` — turn the voice agent's confirmation on or off.
/// `#264 voice-send-approval`.
///
/// WRITE scope, and — as with [`set_noise_rules`] — **the scope is not what keeps the voice agent
/// out**: a hosted agent is routinely given the write token for `post_reply`. What keeps a model
/// from turning its own confirmation off is that NO TOOL reaches this route —
/// [`crate::mcp::tool_manifest`] offers none and [`crate::mcp::protocol::dispatch`] refuses any name
/// it cannot find there. Settings is the only caller. With no store configured it answers 503
/// `storage_not_configured`, and the server keeps asking.
pub async fn set_voice_settings(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<VoiceSettingsRequest>,
) -> Result<Response, ApiError> {
    let settings = crate::send_approval::set(&state, request.ask_before_send).await?;
    Ok(no_store(Json(settings)))
}

/// The noise rules for client-config, or `None` when a configured store cannot say what they are.
///
/// Absent rather than an error: client-config is what signs the page in, and a store fault must
/// not lock the owner out of a page that can still read every channel. The editor hides itself
/// instead of showing a list that may not be the one in force.
async fn noise_rules_view(state: &AppState) -> Option<crate::contract::NoiseRules> {
    match ops::noise_rules(state).await {
        Ok(rules) => Some(crate::contract::NoiseRules {
            rules,
            matching: crate::noise::MATCHING_RULE,
        }),
        Err(error) => {
            tracing::warn!(%error, "could not read the noise rules for client-config");
            None
        }
    }
}

/// A replacement list of noise rules. `#196 auto-read-noise`.
#[derive(Debug, Deserialize)]
pub struct NoiseRulesRequest {
    /// Every rule, in order. The whole list: what is not here is removed. Validated by
    /// [`crate::noise::validate_rules`]; empty is allowed and turns the feature off.
    pub rules: Vec<String>,
}

/// `PUT /api/v1/noise-rules` — replace the rules that decide what is read automatically.
///
/// WRITE scope, because the list outlives the process and every device reads it back. And, as
/// with [`set_alias`], **the scope is not what keeps the voice agent out**: a hosted agent is
/// routinely given the write token for `post_reply`. What keeps a model from hiding messages from
/// the owner it reports to is that NO TOOL edits this list — [`crate::mcp::tool_manifest`] offers
/// none and [`crate::mcp::protocol::dispatch`] refuses any name it cannot find there.
///
/// Reading the list needs no route of its own: it rides on client-config.
pub async fn set_noise_rules(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<NoiseRulesRequest>,
) -> Result<Response, ApiError> {
    let rules = ops::set_noise_rules(&state, &request.rules).await?;
    Ok(no_store(Json(crate::contract::NoiseRules {
        rules,
        matching: crate::noise::MATCHING_RULE,
    })))
}

/// Messages the owner says are not noise. `#196 auto-read-noise`.
#[derive(Debug, Deserialize)]
pub struct NotNoiseRequest {
    /// The ids, as the page was served them. At most one page's worth.
    pub messages: Vec<String>,
}

/// `POST /api/v1/channels/{channel_id}/not-noise` — rescue a message a rule caught by mistake.
///
/// WRITE scope, like every durable write. Recorded per message rather than by editing the rules,
/// because a false positive is ONE message: the rule is still right about the other nine, and the
/// owner should not have to weaken it to see the tenth. There is no MCP tool for this either.
pub async fn not_noise(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<NotNoiseRequest>,
) -> Result<Response, ApiError> {
    if request.messages.is_empty() {
        return Err(ApiError::bad_request(
            "send the `messages` that are not noise; an empty request would report success for \
             having done nothing",
        ));
    }
    let wanted: Vec<MessageId> = request.messages.into_iter().map(MessageId).collect();
    Ok(answered(
        ops::exempt_from_noise(&state, &channel_id, &wanted).await?,
    ))
}

/// How changes reach the live stream for a provider polled every `poll_seconds`.
///
/// A polled provider is polled whether or not an adapter may push; push applies to the rest.
fn live_delivery(state: &AppState, poll_seconds: u64) -> LiveDelivery {
    if poll_seconds > 0 {
        LiveDelivery::Poll
    } else if state.config.ingest.enabled() {
        LiveDelivery::Push
    } else {
        LiveDelivery::Off
    }
}

/// `GET /api/v1/diagnostics` — re-run every configuration check and say what is wrong.
///
/// Four decisions about this route, each of which could plausibly have gone the other way.
///
/// **READ scope.** It reports configuration health and mints nothing, spends nothing, and writes
/// nothing durable — the storage check takes the write lock and rolls it back precisely so that
/// this stays true for a read-scope caller. What it *does* is name settings and vendor accounts,
/// which is why it is authenticated at all rather than sitting beside `/healthz`.
///
/// **It always answers 200.** The report IS the answer. Returning 503 when a check fails would
/// make the interesting case — six red rows, each with a remedy — arrive at a client as an error
/// with a body it may not even read, and would make this route unusable as the thing you call
/// precisely *because* something is broken.
///
/// **No credential can appear in it.** Every string goes through the deployment's whole secret
/// list on the way out; see [`crate::diagnostics::redacted`]. A test asserts it against a vendor
/// that quotes the key back in its own error body, which is a thing vendors do.
///
/// **`no-store`.** It names accounts, agent ids and file paths, and it is a point-in-time answer;
/// neither a proxy nor a browser should keep it.
pub async fn diagnostics(
    State(state): State<AppState>,
    _scope: ReadScope,
) -> Result<Response, ApiError> {
    let report = crate::diagnostics::run(&state).await;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(report)).into_response())
}

/// `GET /api/v1/signed-url` — mint a short-lived signed conversation URL for the voice agent.
///
/// Two things about this route are deliberate.
///
/// **It requires the WRITE scope**, even though it reads nothing. What it hands back is a working
/// conversation with the agent, and that agent is configured with a credential of its own: if it
/// holds the write token, then anyone holding a signed URL can ask it to post in the owner's name.
/// The gate on this route therefore has to be at least as strong as the strongest thing the
/// conversation can do. An unauthenticated version of this route would be strictly worse than the
/// public `talk-to` link that enabling authentication just closed.
///
/// **The answer is `no-store`.** The minted URL is itself a bearer credential for the next fifteen
/// minutes; it must not sit in a proxy or a browser cache.
pub async fn signed_url(
    State(state): State<AppState>,
    _scope: WriteScope,
) -> Result<Response, ApiError> {
    let minted: SignedUrl = state
        .elevenlabs
        .signed_url(&state.config.elevenlabs)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(minted)).into_response())
}

/// `GET /api/v1/voice-session` — open a session through the configured voice provider.
///
/// Like the compatibility signed-URL endpoint, this requires WRITE scope because a conversation
/// may hold tools with that authority. The descriptor is never cached: some providers return a
/// short-lived bearer URL, and a generic route must honor the strictest provider it can select.
///
/// A session also starts reading every listed channel, in the background. `#210
/// voice-catchup-speed`: the agent's first question comes after its greeting, five to six seconds
/// later, and its catch-up is answered from those reads instead of waiting for them. Single-flight
/// and skipped for a channel read recently enough, so a burst of call starts adds no reads.
pub async fn voice_session(
    State(state): State<AppState>,
    _scope: WriteScope,
) -> Result<Response, ApiError> {
    // `#272 voice-agent-instructions`. Beside the mint rather than after it: asking a hosted agent
    // whether it takes a prompt override is a round trip of its own, and the call is waiting.
    let (session, agent_prompt) = tokio::join!(
        state.conversation.open_session(),
        crate::voice_prompt::for_session(&state)
    );
    let mut session: VoiceSession = session?;
    session.agent_prompt = agent_prompt;
    let warming = crate::recent::warm(&state);
    tracing::info!(
        channels = warming,
        "voice session opened; reading channels ahead of the agent"
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(session)).into_response())
}

/// `GET /api/v1/voice-agent` — the versioned system prompt and what this caller's agent can do.
///
/// Provider-neutral on purpose: a hosted agent's operator reads the same prompt the QUICKSTART
/// points at, and a deployment-managed bridge fetches it here so the instructions it opens a
/// session with are the reviewed ones rather than a private copy. The tool list is filtered by the
/// same rules as the MCP endpoint's `tools/list`, so the two cannot disagree.
pub async fn voice_agent(
    State(state): State<AppState>,
    ReadScope(scope): ReadScope,
) -> Result<Response, ApiError> {
    // One read of the setting for both, so the prompt and `post_reply`'s confirmation flag cannot
    // straddle a change made between two reads. `#264 voice-send-approval`.
    let approval = crate::send_approval::current(&state).await;
    let manifest = crate::mcp::tool_manifest(&ops::channels(&state).await, approval);
    // `#272 voice-agent-instructions`: the owner's words go into the prompt here, which is how a
    // bridge that fetches this as each call opens carries them.
    let owner = crate::voice_prompt::current(&state).await;
    let profile = crate::voice_agent::profile(
        &manifest,
        scope,
        state.chat.provider_name(),
        state.config.timezone.name(),
        approval,
        &owner,
    );
    Ok(no_store(Json(profile)))
}

/// `POST /api/v1/voice-timing` — record one call's startup phases.
///
/// The body is a closed set of phase offsets (see [`crate::voice_agent::StartupTiming`]); an
/// unknown field is refused, so nothing a person said can reach the log through it. It is logged
/// and not stored: the point is to read the dominant delay off a live deployment, and a durable
/// record would be one more thing to retain and purge.
pub async fn voice_timing(
    _scope: WriteScope,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let timing: crate::voice_agent::StartupTiming =
        serde_json::from_slice(&body).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_timing",
                "expected a startup timing record with only the documented phase fields",
            )
        })?;
    timing
        .validate()
        .map_err(|detail| ApiError::new(StatusCode::BAD_REQUEST, "invalid_timing", detail))?;
    tracing::info!("voice_startup {}", timing.log_fields());
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/v1/voice-health` — the page's verdict that the voice service stopped answering.
///
/// The body is closed (see [`crate::voice_health::HealthReport`]): two closed enums, a flag and
/// integers, so an unknown field or value is a 400 and nothing free-form reaches the log. Logged
/// and not stored, like the timing record. The page sends each cause at most once per call; the
/// shared [`crate::voice_health::LogBudget`] is what bounds a page that does not.
pub async fn voice_health(
    State(state): State<AppState>,
    _scope: WriteScope,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let report: crate::voice_health::HealthReport =
        serde_json::from_slice(&body).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_health",
                "expected a voice health record with only the documented fields and values",
            )
        })?;
    report
        .validate()
        .map_err(|detail| ApiError::new(StatusCode::BAD_REQUEST, "invalid_health", detail))?;
    let spend = state.voice_health_budget.spend();
    if spend == crate::voice_health::Spend::FirstRefusal {
        tracing::warn!(
            "voice_health_throttled max_lines={} window_s={}",
            crate::voice_health::MAX_LINES_PER_WINDOW,
            crate::voice_health::BUDGET_WINDOW.as_secs()
        );
    }
    if !spend.allowed() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "voice_health_throttled",
            "too many voice health records this minute; this one was not logged",
        ));
    }
    if report.is_recovery() {
        tracing::info!("voice_recovered {}", report.log_fields());
    } else {
        tracing::warn!("voice_unresponsive {}", report.log_fields());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/v1/voice-end` — how a call that opened ended, as the page saw it. `#268
/// voice-call-drops`.
///
/// Closed like the health record (see [`crate::voice_health::EndReport`]), logged and not stored,
/// one line per call: `voice_call_ended`, at info when the reader hung up and at warn when anything
/// else ended the call. Bounded by its own [`crate::voice_health::LogBudget`].
pub async fn voice_end(
    State(state): State<AppState>,
    _scope: WriteScope,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let report: crate::voice_health::EndReport = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_end",
            "expected a call-end record with only the documented fields and values",
        )
    })?;
    report
        .validate()
        .map_err(|detail| ApiError::new(StatusCode::BAD_REQUEST, "invalid_end", detail))?;
    let spend = state.voice_end_budget.spend();
    if spend == crate::voice_health::Spend::FirstRefusal {
        tracing::warn!(
            "voice_end_throttled max_lines={} window_s={}",
            crate::voice_health::MAX_LINES_PER_WINDOW,
            crate::voice_health::BUDGET_WINDOW.as_secs()
        );
    }
    if !spend.allowed() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "voice_end_throttled",
            "too many call-end records this minute; this one was not logged",
        ));
    }
    if report.by_the_reader() {
        tracing::info!("voice_call_ended {}", report.log_fields());
    } else {
        tracing::warn!("voice_call_ended {}", report.log_fields());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/v1/channels/{channel_id}/messages/{message_id}/speak`
///
/// Read a message aloud, and hand back the AUDIO rather than a URL to it.
///
/// A POST even though it changes nothing here: it spends money at a vendor, and a GET is fair
/// game for a browser, a proxy or a crawler to fetch speculatively. The key never leaves this
/// process -- the browser could not call ElevenLabs itself without being given an account
/// credential, which is the whole reason this route exists rather than a signed URL.
///
/// The message is looked up in the fetched window by id, so a message this server cannot see is a
/// 404 rather than an invented request. The text sent is the message's OWN text, never anything
/// the caller supplied: this route must not become a way to spend the operator's ElevenLabs
/// balance on arbitrary strings.
pub async fn speak(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path((channel_id, message_id)): Path<(String, String)>,
    Query(query): Query<SpeakQuery>,
) -> Result<Response, ApiError> {
    // TIMED IN THREE PARTS, because "reading a message aloud is slow" has three candidate causes
    // and they are fixed by completely different work. Looking up the message is a round trip to
    // DISCORD — this route asks for a whole window and finds one message in it. Generating the
    // audio is a round trip to a VENDOR whose cost grows with how much there is to say. The
    // transfer is neither, and on a phone it is not small: nothing here streams, so the reader
    // waits for the last byte of the whole file before the first word is spoken.
    //
    // Reported as `Server-Timing`, which a browser shows per request in its own network panel, so
    // the split can be read off a real deployment against real messages rather than guessed at
    // from a local run against fakes where both round trips are free.
    let began = std::time::Instant::now();
    let (_channel, message) = ops::message_by_id_scoped(
        &state,
        &channel_id,
        &message_id,
        query.limit,
        query.thread_id.as_deref(),
    )
    .await?;
    let looked_up_ms = began.elapsed().as_millis();
    // WHAT IS SENT IS NOT THE RAW MESSAGE. Markdown, hashes, snowflakes and ISO timestamps are all
    // noise when spoken — the voice said "asterisk asterisk deploy asterisk asterisk" and spelled
    // out nineteen-digit ids. `speakable::for_speech` is deterministic and costs nothing; see it
    // for why this is code rather than a prompt.
    //
    // It used to run HERE, on a table of its own. It now reads the body `ops::stamp` already
    // prepared, which matters for the letters and not for the words: a hash named "hash code A" by
    // the page a minute ago is still "hash code A" when this route reads the same message aloud,
    // because both went through the one table on the state.
    let said = message.spoken_body();
    let characters = said.chars().count();
    let asked = std::time::Instant::now();
    let spoken = state.speech.speak(said, query.speed).await?;
    let generated_ms = asked.elapsed().as_millis();
    let bytes = spoken.audio.len();
    // At the vendor's default mp3 bitrate this is roughly sixteen kilobytes per second of speech,
    // so the size is also the plainest available statement of how long the reader is about to
    // listen for — and of how much has to cross a phone connection before they hear any of it.
    tracing::info!(
        message = %message_id,
        characters,
        bytes,
        chat_ms = %looked_up_ms,
        vendor_ms = %generated_ms,
        "read a message aloud"
    );
    let timing = format!(
        "chat;dur={looked_up_ms}, tts;dur={generated_ms}, \
         audio;desc=\"{bytes} bytes for {characters} characters\""
    );
    Ok((
        [
            (header::CONTENT_TYPE, spoken.content_type.as_str()),
            // Audio for one message never goes in a shared cache: it is derived from a channel
            // this server authenticates access to, and the reader's own browser re-asking is
            // cheaper to reason about than a proxy holding somebody else's message.
            (header::CACHE_CONTROL, "no-store"),
            // Not in `http`'s constant list, so named here. One place, one spelling.
            (
                header::HeaderName::from_static("server-timing"),
                timing.as_str(),
            ),
        ],
        spoken.audio,
    )
        .into_response())
}

/// Query parameters for reading a message aloud.
#[derive(Debug, Default, Deserialize)]
pub struct SpeakQuery {
    /// Thread containing the message, when reading a thread or a flattened timeline.
    pub thread_id: Option<String>,
    /// How many recent messages to look through for the id.
    pub limit: Option<u16>,
    /// How fast to read, overriding the agent's own pace for this one request.
    ///
    /// Clamped rather than validated: a slider is a blunt instrument and a request refused for
    /// being slightly out of range would be a tap that did nothing. See
    /// [`crate::speech::clamp_speed`].
    pub speed: Option<f64>,
}

/// Query parameters for the read endpoints.
#[derive(Debug, Default, Deserialize)]
pub struct LimitQuery {
    /// Thread containing the message, when the operation addresses a thread message.
    pub thread_id: Option<String>,
    /// How many recent messages to consider.
    pub limit: Option<u16>,
    /// Width of a digest line, in characters.
    pub width: Option<u16>,
    /// The OTHER messages drawn as part of the same row, comma-separated, oldest first.
    ///
    /// A post over Discord's length limit arrives as a message plus a short remainder and the page
    /// draws them as one row; this is how it says so, so the summary describes the whole piece of
    /// writing rather than the half that stops mid-sentence. Absent for an ordinary single row.
    pub with: Option<String>,
}

/// `GET /api/v1/channels/{channel_id}/messages`
pub async fn messages(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<MessagesResponse>, ApiError> {
    let window = ops::messages(&state, &channel_id, query.limit).await?;
    let dismissed = ops::dismissed_within(&state, &window.channel.id, &window.messages).await?;
    Ok(Json(MessagesResponse {
        complete: window.is_whole_channel(),
        channel: window.channel,
        messages: window.messages,
        dismissed,
        as_of: window.as_of,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// Query parameters for one step of a walk.
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// How many messages this step should return.
    pub limit: Option<u16>,
    /// Step back from this message id, exclusive.
    pub before: Option<String>,
    /// Start of a time span, inclusive, ISO-8601.
    pub since: Option<String>,
    /// End of a time span, exclusive, ISO-8601. Requires `since`.
    pub until: Option<String>,
    /// The span from this many hours ago to now, on this server's clock. Stands alone.
    /// `#190 voice-agent-tools`.
    pub hours: Option<u64>,
    /// `html` to have each message carry its rendered body, `content_html`. `#217 markdown-blocks`.
    pub render: Option<RenderQuery>,
}

/// What a read may be asked to render besides the text it returns. `#217 markdown-blocks`.
///
/// ASKED FOR, on the one route that needs asking: `/page` is the page's history walk for a
/// provider without threads AND the voice agent's `read_page` tool, and a model given HTML beside
/// every message would be reading the same words twice in a form it has no use for. The routes
/// only the page reads — the timeline, the to-do list, the live stream, a post's answer and the
/// pins — render without being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderQuery {
    /// Fill each message's `content_html`.
    Html,
}

/// One step of a walk, saying plainly that it is one.
#[derive(Debug, Serialize)]
pub struct PageResponse {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// Messages, oldest first.
    pub messages: Vec<Message>,
    /// How many this step returned. Never a channel total.
    pub returned: usize,
    /// The page size that applied.
    pub limit: u16,
    /// Whether messages exist beyond this page in the direction of travel.
    pub has_more: bool,
    /// Hand this back as `before` to take the next step back. Absent when there is nothing more.
    pub next_before: Option<String>,
    /// Hand this back as `since` to take the next step of a range walk.
    pub next_since: Option<String>,
    /// Which of `messages` the reader has ARCHIVED, so the view can dim them.
    ///
    /// The ordinary channel view shows archived rows greyed rather than hidden -- hiding is what
    /// the To do filter is for -- and it can only do that if the payload says which they are.
    pub dismissed: Vec<MessageId>,
    /// The revision this channel's pins are at, as on a timeline read. `#206 pin-message`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pins_revision: Option<i64>,
    /// This channel's bookmark, as on a timeline read. `#240 reading-bookmark`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bookmark: Option<crate::store::Bookmark>,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/channels/{channel_id}/page`
pub async fn page(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<PageResponse>, ApiError> {
    let mut step = ops::page(
        &state,
        &channel_id,
        ops::PageRequest {
            limit: query.limit,
            before: query.before.as_deref(),
            since: query.since.as_deref(),
            until: query.until.as_deref(),
            hours: query.hours,
        },
    )
    .await?;
    if query.render == Some(RenderQuery::Html) {
        crate::render::fill(
            &state.config,
            &state.headings,
            &step.channel,
            &mut step.messages,
        );
    }
    let dismissed = ops::dismissed_within(&state, &step.channel.id, &step.messages).await?;
    let pins_revision = ops::pins_revision(&state, &step.channel.id).await;
    let bookmark = ops::bookmark_state(&state, &step.channel.id).await;
    Ok(Json(PageResponse {
        pins_revision,
        bookmark,
        returned: step.returned(),
        channel: step.channel,
        messages: step.messages,
        dismissed,
        limit: step.limit,
        has_more: step.has_more,
        next_before: step.next_before.map(|id| id.0),
        next_since: step.next_since,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// Query parameters for browsing a channel or its threads.
#[derive(Debug, Default, Deserialize)]
pub struct TimelineQuery {
    /// Main channel, thread list, flattened history, or a selected thread.
    #[serde(default)]
    pub view: crate::threads::TimelineView,
    /// Thread to read; required exactly when `view=thread`.
    pub thread_id: Option<String>,
    /// Opaque backward continuation returned by the previous page.
    pub before: Option<String>,
    /// Opaque forward cursor: the `next_after` of an earlier newest page or delta of this view.
    /// The answer then carries only what changed since. `#203 incremental-refresh`.
    pub after: Option<String>,
    /// Entries requested, clamped to the configured limit and 99.
    pub limit: Option<u16>,
}

/// `GET /api/v1/channels/{channel_id}/timeline` — all views share one allowlist boundary.
pub async fn timeline(
    State(state): State<AppState>,
    ReadScope(caller): ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<TimelineQuery>,
) -> Result<Response, ApiError> {
    if (query.view == crate::threads::TimelineView::Thread) != query.thread_id.is_some() {
        return Err(ApiError::bad_request(
            "thread_id is required exactly when view=thread",
        ));
    }
    if query.thread_id.as_ref().is_some_and(|id| {
        id.trim().is_empty() || id.len() > 1024 || id.chars().any(char::is_control)
    }) {
        return Err(ApiError::bad_request(
            "thread_id must be a non-empty thread identifier",
        ));
    }
    if query
        .before
        .as_ref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 16384)
    {
        return Err(ApiError::bad_request(
            "before must be a continuation returned by this view",
        ));
    }
    if query.before.is_some() && query.after.is_some() {
        return Err(ApiError::bad_request(
            "before and after cannot both be given",
        ));
    }
    if query.after.as_ref().is_some_and(|cursor| {
        cursor.is_empty() || cursor.len() > crate::timeline_forward::MAX_ENVELOPE
    }) {
        return Err(ApiError::bad_request(
            "after must be a next_after returned by this view",
        ));
    }
    let limit = state.effective_limit(query.limit).min(ops::MAX_PAGE);
    let request = crate::threads::TimelineRequest {
        view: query.view,
        thread_id: query.thread_id,
        before: query.before,
        after: query.after,
        limit,
    };
    let (channel, mut page) = ops::timeline(&state, &channel_id, &request).await?;
    // `#217 markdown-blocks`. Here and not in `ops::timeline`: this route's only reader is the
    // page, and the operation under it is shared with paths toward a model.
    crate::render::fill_page(&state.config, &state.headings, &channel, &mut page);
    // `#182 thread-display-names`. Known names and sentences ride along on every thread list and
    // thread heading; due threads are queued for the background, and only for a caller with write
    // scope, because a read token never spends the model or fills the cache.
    state
        .thread_names
        .annotate(
            &crate::summarize::threads::ThreadDeps::from_state(&state),
            &channel.id,
            &request,
            &mut page,
            caller >= crate::auth::Scope::Write,
        )
        .await;
    // `#222 unread-replies`. Main's newest page and its deltas also say which replies in its threads
    // are Done, which its own rows cannot: Main shows a root and none of its replies. Not on a step
    // back, whose roots are older than anything the reader is waiting on; not in a channel scoped
    // to one conversation, whose Main holds the replies themselves.
    let replies = request.view == crate::threads::TimelineView::Main
        && request.before.is_none()
        && page.has_threads
        && page.thread.is_none();
    let (dismissed, reply_dismissals) =
        ops::timeline_read_state(&state, &channel.id, &page.messages, replies).await?;
    // `#206 pin-message`. One indexed lookup on every read, delta included: this is how a refresh
    // that is happening anyway tells the page whether another device changed the pins.
    let pins_revision = ops::pins_revision(&state, &channel.id).await;
    // `#240 reading-bookmark`. One indexed lookup too, for the same reason: this is how a page
    // learns the owner moved the bookmark on his other device.
    let bookmark = ops::bookmark_state(&state, &channel.id).await;
    Ok(no_store(Json(TimelineResponse {
        pins_revision,
        bookmark,
        returned: page.messages.len() + page.threads.len(),
        channel,
        view: request.view,
        limit,
        page,
        dismissed,
        reply_dismissals,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// Query parameters for a bounded count.
#[derive(Debug, Default, Deserialize)]
pub struct CountQuery {
    /// Count only messages at or after this ISO-8601 instant.
    pub since: Option<String>,
    /// Count only messages from this many hours ago to now. Not with `since`.
    /// `#190 voice-agent-tools`.
    pub hours: Option<u64>,
    /// Stop after this many. Clamped by `discord.max_count_scan`.
    pub cap: Option<u32>,
}

/// A bounded count, with the honesty flag attached.
#[derive(Debug, Serialize)]
pub struct CountResponse {
    /// Channel that was counted.
    pub channel: ChannelInfo,
    /// How many messages were seen.
    pub counted: usize,
    /// When true, `counted` is a LOWER BOUND: the cap stopped the walk before the channel ran out.
    pub at_least: bool,
    /// The ceiling that applied.
    pub cap: u32,
    /// Oldest message the walk reached.
    pub oldest_seen: Option<String>,
    /// Newest message the walk started from.
    pub newest_seen: Option<String>,
    /// Messages the walk passed that the owner's noise rules call read, and so did NOT count.
    /// `#196 auto-read-noise`.
    pub noise: usize,
}

/// `GET /api/v1/channels/{channel_id}/count`
pub async fn count(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<CountQuery>,
) -> Result<Json<CountResponse>, ApiError> {
    let tally = ops::count_within(
        &state,
        &channel_id,
        query.since.as_deref(),
        query.hours,
        query.cap,
    )
    .await?;
    Ok(Json(CountResponse {
        channel: tally.channel,
        counted: tally.counted,
        at_least: tally.at_least,
        cap: tally.cap,
        oldest_seen: tally.oldest_seen.map(|id| id.0),
        newest_seen: tally.newest_seen.map(|id| id.0),
        noise: tally.noise,
    }))
}

/// One message, in full.
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// The message.
    pub message: Message,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/channels/{channel_id}/messages/{message_id}`
///
/// The lookup is within the recent window this server fetches, so a very old message answers 404
/// rather than silently returning something else.
pub async fn message_by_id(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path((channel_id, message_id)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<MessageResponse>, ApiError> {
    let (channel, message) = ops::message_by_id_scoped(
        &state,
        &channel_id,
        &message_id,
        query.limit,
        query.thread_id.as_deref(),
    )
    .await?;
    Ok(Json(MessageResponse {
        channel,
        message,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// A speakable channel digest.
#[derive(Debug, Serialize)]
pub struct DigestResponse {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// One line per message, oldest first.
    pub entries: Vec<DigestEntry>,
    /// Whether the fetch came back short, so `entries.len()` counts the CHANNEL rather than the
    /// window. See [`MessagesResponse::complete`].
    pub complete: bool,
    /// Messages in the window the owner's noise rules call read, which have no entry.
    /// `#196 auto-read-noise`.
    pub noise: usize,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/channels/{channel_id}/digest`
pub async fn digest(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<DigestResponse>, ApiError> {
    let digest = ops::digest(&state, &channel_id, query.limit, query.width).await?;
    Ok(Json(DigestResponse {
        channel: digest.channel,
        entries: digest.entries,
        complete: digest.complete,
        noise: digest.noise,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// Query parameters for a catch-up across every listed channel. `#190 voice-agent-tools`.
#[derive(Debug, Default, Deserialize)]
pub struct ActivityQuery {
    /// Hours back from now, on this server's clock. Default 24, clamped to 1..=168.
    pub hours: Option<u64>,
    /// How many of each channel's most recent lines to return. Default 8, clamped to 1..=30.
    pub per_channel_limit: Option<u16>,
}

/// The newest message any listed channel holds, as far as the reads saw.
#[derive(Debug, Serialize)]
pub struct ActivityNewest {
    /// Channel it is in.
    pub channel: ChannelInfo,
    /// The message, as a digest line.
    pub entry: DigestEntry,
    /// Whether it falls inside the window that was asked about.
    pub in_window: bool,
}

/// One channel's part of a catch-up.
#[derive(Debug, Serialize)]
pub struct ChannelActivityResponse {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// Why this channel could not be read, as an error code. Absent when it was read; the other
    /// channels are answered either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
    /// The most recent lines inside the window, oldest first, placeholders excluded.
    pub entries: Vec<DigestEntry>,
    /// Messages inside the window that are not placeholders.
    pub counted: usize,
    /// When true, `counted` is a LOWER BOUND: the read stopped before the start of the window.
    pub at_least: bool,
    /// Placeholders inside the window that the owner's noise rules call read.
    pub noise: usize,
    /// Present when this channel was not read again in time and is answered from this server's
    /// earlier copy: how many seconds ago that copy's read began. `#210 voice-catchup-speed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_age_seconds: Option<u64>,
}

/// Every listed channel's last few hours.
#[derive(Debug, Serialize)]
pub struct ActivityResponse {
    /// The window that was read, in hours, after clamping.
    pub hours: u64,
    /// Where the window starts, ISO-8601. Inclusive; it runs to the moment of the read.
    pub since: String,
    /// The newest message any channel holds, inside the window or not.
    pub newest: Option<ActivityNewest>,
    /// One entry per listed channel, in list order.
    pub channels: Vec<ChannelActivityResponse>,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/activity` — the route behind `recent_activity`. `#190 voice-agent-tools`.
pub async fn activity(
    State(state): State<AppState>,
    _scope: ReadScope,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<ActivityResponse>, ApiError> {
    let activity = ops::activity(
        &state,
        ops::ActivityRequest {
            hours: query.hours,
            per_channel_limit: query.per_channel_limit,
            now_ms: ops::now_ms(),
        },
    )
    .await?;
    let newest = activity.newest().map(|(channel, newest)| ActivityNewest {
        channel: channel.clone(),
        entry: newest.entry.clone(),
        in_window: activity.covers(newest.at_ms),
    });
    let channels = activity
        .channels
        .into_iter()
        .map(|part| match part.read {
            Ok(read) => ChannelActivityResponse {
                channel: part.channel,
                error: None,
                entries: read.entries,
                counted: read.counted,
                at_least: !read.complete,
                noise: read.noise,
                held_age_seconds: match part.currency {
                    ops::Currency::Current => None,
                    ops::Currency::Held { age, .. } => Some(age.as_secs()),
                },
            },
            Err(error) => ChannelActivityResponse {
                channel: part.channel,
                error: Some(error.code()),
                entries: Vec::new(),
                counted: 0,
                at_least: false,
                noise: 0,
                held_age_seconds: None,
            },
        })
        .collect();
    Ok(Json(ActivityResponse {
        hours: activity.hours,
        since: crate::clock::iso_from_ms(activity.since_ms),
        newest,
        channels,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// A semantic-random-access request: describe the message you want.
#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    /// The description, in the speaker's own words.
    pub query: String,
    /// How many recent messages to search.
    pub limit: Option<u16>,
    /// How many runners-up to return.
    pub max_alternatives: Option<u16>,
}

/// The answer to a resolve request.
#[derive(Debug, Serialize)]
pub struct ResolveResponse {
    /// Channel that was searched.
    pub channel: ChannelInfo,
    /// What was asked for.
    pub query: String,
    /// The match, its runners-up, and whether it was close.
    #[serde(flatten)]
    pub resolution: Resolution,
    /// How many messages were searched, so a caller can say "I only looked at the last 20".
    pub searched: usize,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `POST /api/v1/channels/{channel_id}/resolve` — semantic random access.
pub async fn resolve(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, ApiError> {
    let (channel, resolution, searched) = ops::resolve(
        &state,
        &channel_id,
        &request.query,
        request.limit,
        request.max_alternatives,
    )
    .await?;
    Ok(Json(ResolveResponse {
        channel,
        query: request.query,
        resolution,
        searched,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// A request to post into a channel.
#[derive(Debug, Deserialize)]
pub struct ReplyRequest {
    /// The text to post.
    pub text: String,
    /// Message being replied to, when any.
    pub reply_to: Option<String>,
    /// The destination thread; absent posts directly to the configured channel.
    pub thread_id: Option<String>,
    /// Names this post across attempts, so sending it again cannot post it twice where the
    /// provider honours that. `#195 send-resilience`; see [`ops::reply_scoped`].
    ///
    /// One key names one text: a caller sending different words mints a new key. The page uses
    /// its outbox entry's id, which it keeps for the life of the entry and across reloads.
    pub idempotency_key: Option<String>,
}

/// The result of posting.
#[derive(Debug, Serialize)]
pub struct ReplyResponse {
    /// The FIRST message as Discord accepted it.
    ///
    /// Kept as it was, and kept first: a long reply is one answer that ran over, so the part that
    /// carries the reply pointer is the one a caller means by "the message I posted".
    pub posted: Message,
    /// Every part, in order. One element for a reply that fitted.
    pub parts: Vec<Message>,
}

/// What came back when a split reply got halfway.
#[derive(Debug, Serialize)]
pub struct PartialReplyResponse {
    /// Stable machine-readable code, matching [`ApiErrorBody`] so one branch can read either.
    pub error: &'static str,
    /// Why the next part failed.
    pub detail: String,
    /// How many parts Discord accepted.
    pub posted: usize,
    /// The text that did NOT reach Discord.
    ///
    /// THE ONLY COPY. The caller is about to clear a draft on the strength of this answer, so the
    /// remainder travels in the body rather than being something they must reconstruct by
    /// re-splitting the original the same way this server did.
    pub unsent: String,
    /// Whether the failure was transient — a timeout, a lost connection, a provider's 5xx — so
    /// that sending `unsent` again later could succeed. False for a refusal, which would only be
    /// refused again. `#195 send-resilience`; see [`crate::chat::ChatError::is_transient`].
    pub retryable: bool,
    /// Whether sending `unsent` again — with the same `idempotency_key`, to `thread_id`, and with
    /// the reply target only when `thread_id` is the request's own thread or absent — asks the
    /// provider for the part that failed under the very words and key it had, and for nothing
    /// already posted. Only then can a provider that honours the key recognise a retry of the
    /// part whose arrival nobody knows, so only then may a caller retry on its own or say a retry
    /// cannot post twice. When nothing posted, `unsent` is the request's own text and this is
    /// always true. `#195 send-resilience`; see `ops::unsent_remainder`.
    pub resumable: bool,
    /// The thread `unsent` belongs in, when that is not the conversation itself: the request's
    /// own `thread_id`, or the thread the first part started when the provider continues a long
    /// post under its first part (`#263 post-split-per-provider`). Sending `unsent` there, and
    /// without the reply target when it is not the request's own thread, puts the rest where the
    /// parts already posted went. Absent for the conversation itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// `POST /api/v1/channels/{channel_id}/reply` — the only route that speaks in the owner's name.
pub async fn reply(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<ReplyRequest>,
) -> Result<Response, ApiError> {
    match ops::reply_scoped(
        &state,
        &channel_id,
        &request.text,
        request.reply_to.as_deref(),
        request.thread_id.as_deref(),
        request.idempotency_key.as_deref(),
    )
    .await
    {
        Ok((channel, mut posted, mut parts)) => {
            // The page draws the acknowledged parts before any read brings them back.
            crate::render::fill(
                &state.config,
                &state.headings,
                &channel,
                std::slice::from_mut(&mut posted),
            );
            crate::render::fill(&state.config, &state.headings, &channel, &mut parts);
            Ok(Json(ReplyResponse { posted, parts }).into_response())
        }
        // 207, not 200 and not 502. A machine caller reading only the status must not conclude
        // "sent" — nor "nothing happened", which would have them send the whole thing again and
        // post the first half twice. Multi-Status says exactly what is true: look inside.
        Err(OpError::PartiallyPosted {
            posted,
            unsent,
            cause,
            retryable,
            resumable,
            thread_id,
            ..
        }) => Ok((
            StatusCode::MULTI_STATUS,
            Json(PartialReplyResponse {
                error: "partially_posted",
                detail: cause,
                posted,
                unsent,
                retryable,
                resumable,
                thread_id,
            }),
        )
            .into_response()),
        Err(other) => Err(other.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// `#34 voice-chat-write-confirm`: the post gate's application routes.
//
// A model's `post_reply` only PROPOSES while the owner has the agent ask first (`#264
// voice-send-approval`, the shipped default); these are how the person it speaks for sees the proposal
// and sends or withdraws it. All four need the WRITE token, and NONE of them is a tool — the commit
// in particular must stay out of the manifest, because a model that could reach it would be
// confirming its own post. The handle travels in request bodies, never in a path, so the access
// log's path field cannot hold it. See `crate::post_gate`.
// ---------------------------------------------------------------------------------------------

/// The longest a proposal long-poll may hang, in seconds. Under the usual 30-second proxy idle
/// timeout, so a wait ends in an answer rather than in a proxy's 504.
pub const PROPOSAL_WAIT_MAX_SECONDS: u64 = 25;

/// A request to propose a post. The same fields as `post_reply`'s arguments.
#[derive(Debug, Deserialize)]
pub struct ProposeRequest {
    /// The channel to post to.
    pub channel_id: String,
    /// The exact text.
    pub text: String,
    /// The message it answers, when any.
    pub reply_to: Option<String>,
}

/// What `POST /api/v1/post-proposals` answers. It carries no handle: the proposing caller may be
/// a model's tool, and the handle is the owner's to spend, through `GET` and the commit route.
#[derive(Debug, Serialize)]
pub struct ProposedResponse {
    /// `false` when it was proposed, which posts nothing; `true` only when the owner has turned off
    /// "Ask me before the voice agent sends" and it was posted at once. `#264 voice-send-approval`.
    pub sent: bool,
    /// The proposal's number, as `GET /api/v1/post-proposals` will report it; for a direct send,
    /// the number its access-log line carries.
    pub serial: u64,
    /// The sentence MCP's `post_reply` answers with, for a caller that relays it to a model.
    pub result: String,
}

/// Query of the proposal long-poll.
#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    /// Seconds to wait for a change, at most [`PROPOSAL_WAIT_MAX_SECONDS`]. Absent answers at once.
    pub wait: Option<u64>,
    /// The serial the caller already shows, so the wait ends when the pending proposal differs.
    /// Absent means the caller shows none.
    pub seen: Option<u64>,
}

/// A commit: the handle, the binding restated, and who confirmed it.
#[derive(Debug, Deserialize)]
pub struct CommitRequest {
    /// The proposal's handle.
    pub handle: String,
    /// The channel, exactly as proposed.
    pub channel_id: String,
    /// The text, exactly as proposed.
    pub text: String,
    /// The reply target, exactly as proposed.
    pub reply_to: Option<String>,
    /// Which confirmation this is.
    pub confirmed_by: ops::ConfirmedBy,
}

/// A cancel: only the handle.
#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    /// The proposal's handle.
    pub handle: String,
}

impl From<crate::post_gate::Refusal> for ApiError {
    fn from(value: crate::post_gate::Refusal) -> Self {
        use crate::post_gate::Refusal;
        let status = match value {
            Refusal::Unknown => StatusCode::NOT_FOUND,
            Refusal::Expired => StatusCode::GONE,
            Refusal::Used | Refusal::Superseded | Refusal::Mismatch => StatusCode::CONFLICT,
        };
        Self::new(status, value.code(), value.sentence())
    }
}

async fn pending_view(
    state: &AppState,
    proposal: Option<crate::post_gate::Proposal>,
) -> crate::contract::PendingPostResponse {
    let Some(proposal) = proposal else {
        return crate::contract::PendingPostResponse { proposal: None };
    };
    let channel_id = ChannelId(proposal.binding.channel_id.clone());
    let channel_name = ops::channels(state)
        .await
        .into_iter()
        .find(|channel| channel.id == channel_id)
        .map_or_else(
            || proposal.binding.channel_id.clone(),
            |channel| channel.display_name().to_owned(),
        );
    crate::contract::PendingPostResponse {
        proposal: Some(crate::contract::PendingPost {
            serial: proposal.serial,
            expires_in_ms: proposal.expires_in_ms(tokio::time::Instant::now()),
            handle: proposal.handle,
            channel_id,
            channel_name,
            text: proposal.binding.text,
            reply_to: proposal.binding.reply_to.map(MessageId),
        }),
    }
}

/// `POST /api/v1/post-proposals` — what `post_reply` does, for a caller that speaks REST rather
/// than MCP: propose a post and post nothing, or — when the owner has turned off "Ask me before the
/// voice agent sends" — post it at once. `#264 voice-send-approval`.
///
/// A direct send that fails partway answers like the reply route: 207 with the unsent remainder.
pub async fn propose_post(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<ProposeRequest>,
) -> Result<Response, ApiError> {
    let outcome = crate::send_approval::voice_post(
        &state,
        &request.channel_id,
        &request.text,
        request.reply_to.as_deref(),
    )
    .await;
    match outcome {
        Ok(crate::send_approval::VoicePost::Proposed { channel, proposal }) => {
            Ok(Json(ProposedResponse {
                sent: false,
                serial: proposal.serial,
                result: ops::proposal_notice(&channel, &proposal),
            })
            .into_response())
        }
        Ok(crate::send_approval::VoicePost::Sent {
            channel,
            serial,
            parts,
            text,
            reply_to,
            ..
        }) => Ok(Json(ProposedResponse {
            sent: true,
            serial,
            result: crate::send_approval::sent_notice(&channel, &text, reply_to.as_deref(), &parts),
        })
        .into_response()),
        // The same words to the same place moments after an earlier direct send: nothing posted.
        // Answered as that send was when it is known to have posted — an idempotent replay — and
        // refused with the sentence otherwise, so no caller reads a repeat as a new post.
        Ok(crate::send_approval::VoicePost::Repeat {
            channel,
            earlier,
            text,
        }) => {
            let result = crate::send_approval::repeat_notice(&channel, &earlier, &text);
            if earlier.outcome == crate::post_gate::DirectOutcome::Sent {
                Ok(Json(ProposedResponse {
                    sent: true,
                    serial: earlier.serial,
                    result,
                })
                .into_response())
            } else {
                Err(ApiError::new(StatusCode::CONFLICT, "post_repeated", result))
            }
        }
        Err(OpError::PartiallyPosted {
            posted,
            unsent,
            cause,
            retryable,
            resumable,
            thread_id,
            ..
        }) => Ok((
            StatusCode::MULTI_STATUS,
            Json(PartialReplyResponse {
                error: "partially_posted",
                detail: cause,
                posted,
                unsent,
                retryable,
                resumable,
                thread_id,
            }),
        )
            .into_response()),
        Err(other) => Err(other.into()),
    }
}

/// `GET /api/v1/post-proposals` — the post waiting for confirmation, long-polled.
///
/// Without `wait` it answers at once. With it, the answer waits until the pending proposal is no
/// longer the one numbered `seen` — a new one, or none because it was sent, cancelled, or expired
/// — or until `wait` seconds pass.
pub async fn pending_post(
    State(state): State<AppState>,
    _scope: WriteScope,
    Query(query): Query<PendingQuery>,
) -> Result<Json<crate::contract::PendingPostResponse>, ApiError> {
    let proposal = match query.wait {
        None | Some(0) => state.post_gate.pending(),
        Some(seconds) => {
            let wait = std::time::Duration::from_secs(seconds.min(PROPOSAL_WAIT_MAX_SECONDS));
            state.post_gate.wait_for_change(query.seen, wait).await
        }
    };
    Ok(Json(pending_view(&state, proposal).await))
}

/// `POST /api/v1/post-proposals/commit` — the owner confirmed; post it, once.
///
/// Refused, posting nothing, with `proposal_unknown` (404), `proposal_expired` (410), or
/// `proposal_used`, `proposal_superseded`, `proposal_mismatch` (409). A post that fails after the
/// gate accepted answers like the reply route: 207 with the unsent remainder when part went out.
pub async fn commit_post(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<CommitRequest>,
) -> Result<Response, ApiError> {
    let restated = crate::post_gate::Binding {
        channel_id: request.channel_id,
        text: request.text,
        reply_to: request.reply_to,
    };
    match ops::commit_proposal(&state, &request.handle, &restated, request.confirmed_by).await {
        Ok((serial, mut posted, mut parts)) => {
            if let Some(channel) = state.channel(&restated.channel_id) {
                crate::render::fill(
                    &state.config,
                    &state.headings,
                    &channel,
                    std::slice::from_mut(&mut posted),
                );
                crate::render::fill(&state.config, &state.headings, &channel, &mut parts);
            }
            Ok(Json(crate::contract::CommittedPostResponse {
                serial,
                posted,
                parts,
            })
            .into_response())
        }
        Err(ops::CommitError::Refused(why)) => Err(why.into()),
        Err(ops::CommitError::Post(OpError::PartiallyPosted {
            posted,
            unsent,
            cause,
            retryable,
            resumable,
            thread_id,
            ..
        })) => Ok((
            StatusCode::MULTI_STATUS,
            Json(PartialReplyResponse {
                error: "partially_posted",
                detail: cause,
                posted,
                unsent,
                retryable,
                resumable,
                thread_id,
            }),
        )
            .into_response()),
        Err(ops::CommitError::Post(other)) => Err(other.into()),
    }
}

/// `POST /api/v1/post-proposals/cancel` — withdraw the pending proposal without posting it.
pub async fn cancel_post(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<CancelRequest>,
) -> Result<StatusCode, ApiError> {
    ops::cancel_proposal(&state, &request.handle)?;
    Ok(StatusCode::NO_CONTENT)
}

/// A slow-path question for the coding agents.
#[derive(Debug, Deserialize)]
pub struct AskRequest {
    /// The question.
    pub question: String,
}

/// `POST /api/v1/channels/{channel_id}/ask` — the slow-path seam. Not implemented in v0.
pub async fn ask(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<AskRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let info = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    match state.agent.ask(&info.id, &request.question).await {
        Ok(answer) => Ok(Json(serde_json::json!({ "answer": answer }))),
        Err(error) => Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "agent_backend_unavailable",
            error.to_string(),
        )),
    }
}

/// `GET /api/v1/channels/{channel_id}/stream` — Server-Sent Events for one channel.
///
/// Read scope, and the same allowlist as every other channel route: see [`ops::watch`] for why the
/// gate is there rather than here. A channel outside the configuration answers `404
/// unknown_channel`, never a 200 that streams nothing.
///
/// **`Last-Event-ID` is honoured.** A browser reconnecting sends back the last `id:` it saw and
/// gets only what came after it, out of the hub's bounded replay tail. That is what "reconnection
/// must not duplicate or drop messages" means in practice, and the two failure directions are
/// treated differently on purpose: a repeat is de-duplicated by id on the page, and a subscriber
/// that fell too far behind gets one `event: reset` and the stream ENDS, so the page re-reads
/// through `/messages` rather than silently skipping the gap.
///
/// **Every event says whether it is `replayed`.** An attach with no `Last-Event-ID` gets the whole
/// tail, and on the wire a replayed message is otherwise indistinguishable from one that arrived a
/// moment ago — so a page could not avoid announcing two hundred old messages to a live, billed
/// conversation as news. See [`crate::live::events`].
///
/// **`Cache-Control: no-store`** because the body is channel text belonging to one credential, and
/// **`X-Accel-Buffering: no`** because an nginx-shaped reverse proxy in front of this will
/// otherwise buffer the response and deliver nothing until it is large enough — which presents as
/// a stream that works locally and is dead behind the tunnel.
///
/// One more thing a reader of the access log needs: **this route leaves exactly one line, at
/// attach, with `millis=0`**. [`crate::http::access_layer::log_requests`] returns as soon as the
/// status is known, and for a streaming body that is before a single event has gone out — so a
/// stream held open for an hour still logs zero milliseconds. A test in `tests/logging.rs` pins
/// that rather than leaving it to be misread as an instant request.
pub async fn stream(
    State(state): State<AppState>,
    _scope: ReadScope,
    headers: HeaderMap,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::model::MessageId(value.to_owned()));
    let (channel, subscription) = ops::watch(&state, &channel_id, after.as_ref())?;
    let body =
        axum::response::sse::Sse::new(crate::live::events(subscription, state.clone(), channel))
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
            );
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                axum::http::HeaderName::from_static("x-accel-buffering"),
                "no",
            ),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use crate::http::assets::{APP_JS, VOICE_JS};

    #[test]
    fn the_voice_page_pluralizes_and_prefers_the_operators_own_clock() {
        // Both defects were fixed in web/app.js first and never carried across, and this guard
        // only covered app.js — so the page the owner actually uses kept printing "message(s)"
        // and UTC while the suite stayed green. Guarding one of two assets that render the same
        // two fields is not a guard; it is a coin flip about which file someone edits next.
        assert!(
            !VOICE_JS.contains("(s)"),
            "web/voice.js still renders a parenthesised plural"
        );
        assert!(
            VOICE_JS.contains("function channelSummary("),
            "the count helper is what keeps the fetch window from being read as a channel total"
        );
        assert!(
            VOICE_JS.contains("complete !== true"),
            "a server too old to send `complete` must be treated as unknown, not as complete"
        );
        assert!(
            VOICE_JS.contains("message.spoken_time ||"),
            "the channel view must prefer the zone-converted time the server already computed"
        );
    }

    #[test]
    fn the_relay_quotes_the_prepared_body_and_the_row_still_shows_the_raw_one() {
        // Two claims, and a guard that only made the first would let the fix undo itself. The
        // relay must say the PREPARED body — quoting `content` there is how `read new` came to
        // spell nineteen-digit ids out loud, and it happened because that turn is the one built
        // in the browser rather than on the server. And the row must keep showing the RAW body:
        // a letter is only recoverable because the id it stands for is on the screen beside it,
        // so a page that showed the prepared text in both places would have made the
        // substitution lossy rather than merely quiet.
        let relay = function_body(VOICE_JS, "function relayLine(");
        assert!(
            relay.contains("message.spoken_content ||"),
            "web/voice.js relays the raw body again, so ids are read aloud digit by digit"
        );
        // `combinedContent` is the single place a channel row's text comes from, so it is the
        // single place this half can be checked. It must read the raw field and nothing else.
        let row = VOICE_JS
            .split_once("const combinedContent =")
            .map(|(_, rest)| rest.split_once(";\n").map_or(rest, |(body, _)| body))
            .expect("web/voice.js no longer defines `combinedContent`");
        assert!(
            row.contains("m.content"),
            "the channel row must render what was actually written"
        );
        assert!(
            !row.contains("spoken_content"),
            "the channel row shows the spoken rewrite, so the id a letter stands for is nowhere \
             on screen and the substitution has become lossy"
        );
    }

    /// The text of one top-level function in a served asset, up to the next one.
    ///
    /// Crude on purpose: it is enough to ask "does THIS function call that one", which is the
    /// only question the guard below has, and it needs no parser to answer it.
    ///
    /// `async function` counts as the next one. It did not used to, and the consequence was that
    /// a body ran on through every `async` declaration after it until the next synchronous one —
    /// so an assertion about one function was quietly an assertion about three. web/app.js is
    /// mostly `async`, which is where that first mattered.
    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("the asset no longer defines `{signature}`"));
        let rest = &source[start + signature.len()..];
        let end = ["\nfunction ", "\nasync function "]
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min()
            .unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn both_message_lists_on_the_voice_page_fold_long_messages_the_same_way() {
        // `#47 scrollback-stability`. The page renders two message lists — the voice transcript
        // and the channel view — and the whole point of the folding work is that they behave
        // IDENTICALLY, because they sit one switch apart and a reader moves between them without
        // thinking about it. That is not a property of a shared helper existing; it is a property
        // of both call sites using it. A fix landed on one list and not the other is the exact
        // defect this file's other guard was written for, and it cost a round trip that time.
        assert!(
            VOICE_JS.contains("function foldable("),
            "the folding idiom must be ONE function, not a behaviour each list implements"
        );
        // `function line(` used to be the transcript's entry here. `#128 transcript-history`
        // split BUILDING a transcript row from PLACING one, for the same reason `#63` split the
        // seam: a row restored from the record is PREPENDED above the reader in one anchored
        // mutation, so it cannot go through the function that appends to the end and follows.
        // `turnNode` is now the thing that decides what a turn looks like, and therefore the
        // thing that has to fold. The property asserted is unchanged; the name moved.
        for renderer in ["function turnNode(", "function discordNode("] {
            assert!(
                function_body(VOICE_JS, renderer).contains("foldable("),
                "`{renderer}` in web/voice.js no longer folds long messages, so the two message \
                 lists on /voice disagree about what a long message looks like"
            );
        }
        // Which only means anything while both places that put a turn on screen go through it:
        // the live call's `line`, and the run of restored turns the walk back prepends. A second
        // row builder is how the record ends up folding differently from the call.
        for placer in ["function line(", "function restoredRun("] {
            assert!(
                function_body(VOICE_JS, placer).contains("turnNode("),
                "`{placer}` in web/voice.js builds a transcript row of its own instead of the \
                 one that folds"
            );
        }
        // And the anchoring that goes with it: an arrival must be conditional, never the old
        // unconditional jump.
        assert!(
            VOICE_JS.contains("function followIfPinned("),
            "an arrival must follow the newest line only when the reader was already there"
        );
        // `function seam(` used to be in this list. `#63 status-line-placement` split BUILDING a
        // seam from PLACING one — the channel's summary is a seam at the head of its own list, and
        // placing it at the end of the transcript would be wrong — so the thing that appends to
        // the transcript, and therefore the thing that has to decide whether to follow, is
        // `transcriptSeam`. The property asserted is unchanged; the name of the function that has
        // to satisfy it moved.
        for renderer in ["function line(", "function transcriptSeam("] {
            let body = function_body(VOICE_JS, renderer);
            assert!(
                body.contains("followIfPinned("),
                "`{renderer}` in web/voice.js appends without deciding whether to follow"
            );
            assert!(
                !body.contains("scrollToNewest("),
                "`{renderer}` in web/voice.js scrolls on every arrival again, which is the \
                 defect: a reader looking at older messages gets thrown to the bottom"
            );
        }
    }

    #[test]
    fn typed_input_on_the_voice_page_has_exactly_one_send_path() {
        // `#43 typed-input`. The same argument as the folding guard above, one issue later. Three
        // things now put a client event on the conversation socket — the composer's Send, the
        // Enter key, and the presence ping while somebody is typing — and `#59 text-entry-button`
        // and `#60 canned-prompt-buttons` add more. Each one that grows its own
        // `socket.send(JSON.stringify(...))` is a place the guards can drift: the readyState
        // check, the visible refusal when there is no call, the local echo of the turn and the
        // de-duplication of the vendor's echo all live in these two functions, and a second copy
        // gets some subset of them.
        //
        // So: one writer, and the composer's callers go through it.
        assert!(
            VOICE_JS.contains("function sendClientEvent("),
            "the one place a JSON client event reaches the conversation socket is gone"
        );
        for caller in ["function sendUserMessage(", "function noteComposing("] {
            assert!(
                function_body(VOICE_JS, caller).contains("sendClientEvent("),
                "`{caller}` in web/voice.js writes to the socket by some other route, so the \
                 readyState guard and the visible refusal are no longer on every send"
            );
        }
        // The audio path is the ONE deliberate exception, and it is documented as one in
        // web/voice.js: it is called every 4096 samples, holds the socket in a closure and does
        // its own readyState check. Pinned so that "the exception" cannot quietly become "the
        // rule" by a second hot-path send appearing beside it.
        assert!(
            function_body(VOICE_JS, "function startCapture(").contains("user_audio_chunk"),
            "the microphone frames no longer go out from startCapture"
        );
        assert_eq!(
            VOICE_JS.matches("user_audio_chunk:").count(),
            1,
            "web/voice.js writes microphone frames from more than one place"
        );
        // A typed turn is rendered locally because the vendor may never echo it back, which makes
        // suppressing the echo — if there is one — the other half of the same decision. Losing
        // either half puts the sentence on screen twice or not at all.
        let typed = function_body(VOICE_JS, "function sendUserMessage(");
        assert!(
            typed.contains("line(\"you\""),
            "a typed turn is no longer rendered when it is sent, so it exists only if the vendor \
             chooses to echo it"
        );
        assert!(
            VOICE_JS.contains("function isEchoOfTyped("),
            "nothing suppresses the vendor's echo of a typed turn, so it can render twice"
        );
    }

    #[test]
    fn the_phone_app_pluralizes_and_never_prints_the_placeholder_form() {
        // `#62 message-count-accuracy`. `message(s)` is the shape of the defect: it is what a
        // renderer writes when it has not decided whether it knows the number. The bytes asserted
        // on here are exactly the bytes `GET /app.js` serves.
        assert!(
            !APP_JS.contains("(s)"),
            "web/app.js still renders a parenthesised plural"
        );
        assert!(
            APP_JS.contains("function messageCount("),
            "the count helper is what keeps the two call sites honest and identical"
        );
        assert!(
            APP_JS.contains("complete !== true"),
            "a server too old to send `complete` must be treated as unknown, not as complete"
        );
    }

    #[test]
    fn the_phone_app_shows_the_time_the_server_already_converted() {
        // `#52 operator-timezone`. The phone and the voice agent must say the same thing about
        // when a message arrived; the only way to guarantee that is for both to render the one
        // string the server computed, rather than each slicing the ISO value their own way.
        assert!(
            APP_JS.contains("message.spoken_time ||"),
            "web/app.js must prefer the server-converted time, with the ISO slice as fallback"
        );
        assert!(
            APP_JS.contains("spoken_time: entry.spoken_time"),
            "a digest entry carries the converted time too; dropping it here would make the \
             digest list silently fall back to UTC while the scrollback shows local time"
        );
    }

    /// Every place either served page names a channel, and the one function that decides the name.
    ///
    /// The list is the point. `#52 operator-timezone` and `#62 message-count-accuracy` were each
    /// fixed on one of these two files and carried across afterwards, at the cost of a round trip
    /// both times, and `#39 channel-alias` walked into it a third time: the alias reached
    /// /voice and the voice agent while `/` went on printing the configured label, so one
    /// deployment showed two names for one channel.
    const NAMES_A_CHANNEL: [(&str, &str, &[&str]); 2] = [
        (
            "web/app.js",
            APP_JS,
            // Both pickers are filled by one function; the header is written on both read paths.
            &[
                "function fillChannelSelects(",
                "function loadDigest(",
                "function loadScrollback(",
            ],
        ),
        (
            "web/voice.js",
            VOICE_JS,
            // Both pickers again, the head of the channel on each of its three writers, and the
            // head of the to-do list. The older-page writer is `loadOlderPage`: `loadOlder` only
            // hands out the step in flight since `#204 reply-arrow`.
            &[
                "function fillChannelSelect(",
                "function loadDiscord(",
                "function loadOlderPage(",
                "function restateChannelSeam(",
                "function todoSummary(",
            ],
        ),
    ];

    #[test]
    fn both_served_pages_call_a_channel_what_the_operator_called_it() {
        // `#39 channel-alias`. `ChannelInfo::display_name` is the rule on this side of the wire;
        // `channelName` is the same rule in the browser, and it exists twice because two served
        // assets with no build step between them cannot share a module. That is exactly the
        // arrangement the guards above were written for, so it gets the same treatment: the RULE
        // is asserted over the bytes of BOTH files, and so is every call site of it.
        for (name, source, renderers) in NAMES_A_CHANNEL {
            let rule = function_body(source, "function channelName(");
            assert!(
                rule.contains("channel.alias"),
                "{name} defines channelName without consulting the operator's own name for the \
                 channel"
            );
            assert!(
                rule.contains("channel.label"),
                "{name} defines channelName without the configured label as its fallback, so \
                 clearing an alias would leave the channel with no name at all"
            );
            for renderer in renderers {
                let body = function_body(source, renderer);
                assert!(
                    body.contains("channelName("),
                    "`{renderer}` in {name} names a channel without going through channelName, \
                     so this deployment can show two different names for one channel"
                );
                assert!(
                    !body.contains(".label"),
                    "`{renderer}` in {name} reads the configured label directly again, which is \
                     the defect: the alias is ignored wherever that is done"
                );
            }
        }
    }

    #[test]
    fn no_served_page_grows_a_second_opinion_about_a_channels_name() {
        // The other half, and the half that keeps the list above honest: a call site added later
        // is not covered by a list written today. `.label` is the shape of the defect, so it is
        // allowed in exactly two places — the fallback inside `channelName`, and the alias editor
        // on /voice, which names the configured label deliberately so the owner can see what
        // clearing would go back to.
        assert_eq!(
            APP_JS.matches(".label").count(),
            1,
            "web/app.js reads a channel's configured label somewhere other than the fallback \
             inside channelName"
        );
        assert_eq!(
            channel_label_reads_in_voice_page(VOICE_JS),
            3,
            "web/voice.js reads a channel's configured label somewhere other than the fallback \
             inside channelName and the two sentences the alias editor shows"
        );
        assert_eq!(
            function_body(VOICE_JS, "function renderAliasEditor(")
                .matches(".label")
                .count(),
            2,
            "the alias editor on /voice must be where the other two readings of the configured \
             label are; if they moved, this count is guarding the wrong thing"
        );
    }

    fn channel_label_reads_in_voice_page(source: &str) -> usize {
        // Read-aloud metadata also has a label. Exempt only its accesses in the function that
        // binds `speech` to `config.read_aloud`; channel.label in that same function still counts.
        let config = function_body(source, "function applyClientConfig(");
        assert!(config.contains("const speech = config.read_aloud;"));
        let without_speech_label = config.replace("speech.label", "speech_provider_name");
        source
            .replacen(config, &without_speech_label, 1)
            .matches(".label")
            .count()
    }

    #[test]
    fn speech_metadata_exemption_does_not_hide_direct_channel_labels() {
        let changed = VOICE_JS.replacen(
            "function applyClientConfig(config) {",
            "function applyClientConfig(config) {\n  heading = channel.label;",
            1,
        );
        assert_eq!(channel_label_reads_in_voice_page(&changed), 4);
        let changed = format!("{VOICE_JS}\nheading = speech.label;\n");
        assert_eq!(channel_label_reads_in_voice_page(&changed), 4);
    }
}

// --- durable state: the /voice transcript, and this server's own inbox ------------------------
//
// These five conversation routes are the only handlers on this server that do not consult
// `ops::allowed`, because a conversation is not a channel: it is the owner's own session with
// the agent, held on this page, and there is no allowlist for it. What takes the place of that
// gate is `ConversationId::parse`, which every one of them runs first.
//
// ALL OF THEM REQUIRE THE WRITE SCOPE, including the reads. A transcript is the owner's own
// speech plus whatever channel text the agent read aloud to him, which is strictly more sensitive
// than a digest of a channel he already allowlisted — and /voice, the only thing that uses these,
// already holds the write token. A read-scope token deliberately gets 403 here, and a test
// asserts that rather than assuming it.
//
// NONE OF THEM IS AN MCP TOOL, and a test in tests/mcp.rs holds that line. The model does not
// need the owner's transcript to answer a question about a channel, and text that reaches a model
// is text that leaves this machine.

/// A stored conversation listing.
#[derive(Debug, Serialize)]
pub struct ConversationsResponse {
    /// Conversations, most recently active first.
    pub conversations: Vec<ConversationSummary>,
    /// Standing reminder that a transcript quotes third-party channel text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/conversations`
pub async fn list_conversations(
    State(state): State<AppState>,
    _scope: WriteScope,
) -> Result<Response, ApiError> {
    let conversations = state.store.conversations().await?;
    Ok(no_store(Json(ConversationsResponse {
        conversations,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// One stored conversation, in full.
#[derive(Debug, Serialize)]
pub struct ConversationResponse {
    /// The conversation's id.
    pub id: ConversationId,
    /// Its turns, oldest first.
    pub turns: Vec<Turn>,
    /// Standing reminder that a transcript quotes third-party channel text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/conversations/{conversation_id}`
pub async fn conversation(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(conversation_id): Path<String>,
) -> Result<Response, ApiError> {
    let id = ConversationId::parse(&conversation_id)?;
    let turns = state.store.turns(&id).await?;
    Ok(no_store(Json(ConversationResponse {
        id,
        turns,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// Query parameters for one step of a walk back through the transcript.
#[derive(Debug, Default, Deserialize)]
pub struct TranscriptQuery {
    /// How many turns this step should return. Clamped to
    /// [`crate::store::MAX_TRANSCRIPT_PAGE`]; defaults to
    /// [`crate::store::DEFAULT_TRANSCRIPT_PAGE`].
    pub limit: Option<u16>,
    /// The `next` token from the previous step. Absent means "start at the newest".
    pub before: Option<String>,
}

/// One turn of the record, with what a page needs to place it.
#[derive(Debug, Serialize)]
pub struct TranscriptTurn {
    /// Which conversation it was said in, so a reader can be shown where one call ended.
    pub conversation_id: ConversationId,
    /// Its position within that conversation.
    pub seq: i64,
    /// Who said it.
    pub speaker: Speaker,
    /// What was said. UNTRUSTED when the agent was reading a channel aloud.
    pub text: String,
    /// When the server recorded it.
    pub at_ms: i64,
}

/// One step backwards through everything that has been said.
#[derive(Debug, Serialize)]
pub struct TranscriptResponse {
    /// The turns, NEWEST FIRST. The page reverses what it draws; see
    /// [`crate::store::StateStore::transcript`] for why the limit is applied at this end.
    pub turns: Vec<TranscriptTurn>,
    /// Whether anything older than the last of them is still held.
    pub has_more: bool,
    /// Hand this back as `before` to take the next step. Absent only when nothing came back.
    pub next_before: Option<String>,
    /// The page size that actually applied, after clamping.
    pub limit: u16,
    /// Standing reminder that a transcript quotes third-party channel text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/transcript`
///
/// **Write scope, like every other route over the transcript**, and for the reason in the block
/// comment above them: this is the owner's own speech plus whatever channel text the agent read
/// out to him.
///
/// A SEPARATE ROUTE FROM `GET /api/v1/conversations/{id}`, not a `limit` bolted onto it. The two
/// answer different questions and a single route could not answer both honestly: that one returns
/// ONE conversation whole, which is what resuming needs and what a page cannot bound; this one
/// returns the newest N turns from ANYWHERE, which is what browsing needs and which no
/// conversation-shaped read can express — the newest forty turns routinely span two calls, and on
/// a long call they are a suffix of one.
pub async fn transcript(
    State(state): State<AppState>,
    _scope: WriteScope,
    Query(query): Query<TranscriptQuery>,
) -> Result<Response, ApiError> {
    // Parsed, never trusted, and a bad one is refused rather than quietly treated as "start from
    // the newest": silently rewinding a walk puts the reader somewhere other than where they were
    // with nothing on screen saying so.
    let before = query
        .before
        .as_deref()
        .map(crate::store::TranscriptCursor::parse)
        .transpose()?;
    let limit = query
        .limit
        .unwrap_or(crate::store::DEFAULT_TRANSCRIPT_PAGE)
        .clamp(1, crate::store::MAX_TRANSCRIPT_PAGE);
    let page = state.store.transcript(limit, before.as_ref()).await?;
    Ok(no_store(Json(TranscriptResponse {
        has_more: page.has_more,
        next_before: page
            .next
            .as_ref()
            .map(crate::store::TranscriptCursor::encode),
        limit,
        turns: page
            .turns
            .into_iter()
            .map(|entry| TranscriptTurn {
                conversation_id: entry.conversation,
                seq: entry.seq,
                speaker: entry.turn.speaker,
                text: entry.turn.text,
                at_ms: entry.turn.at_ms,
            })
            .collect(),
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// A reconstruction of an earlier conversation, and everything needed to describe it honestly.
#[derive(Debug, Serialize)]
pub struct ReplayResponse {
    /// The payload to send on the new socket. EMPTY when there is nothing to resume from.
    pub text: String,
    /// How many turns are in it.
    pub included: usize,
    /// How many older turns were left out. Non-zero means the interface must say "in part".
    pub dropped: usize,
    /// Whether the budget, rather than the transcript running out, is what stopped it.
    pub truncated: bool,
    /// The budget that produced it.
    pub policy: crate::replay::ReplayPolicy,
    /// How the page should hand `text` to the vendor.
    pub transport: crate::replay::Transport,
    /// Whether the operator has turned resuming on at all.
    ///
    /// Reported rather than answered with a 404, because "off" and "nothing to resume from" are
    /// different things the page has to say differently. A route that refused would collapse them.
    pub enabled: bool,
    /// Standing reminder that a transcript quotes third-party channel text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/conversations/{conversation_id}/replay`
///
/// **Write scope, like every other conversation route**, and for the same reason: what comes back
/// is the transcript itself, rendered. If a read token could fetch this it could fetch the
/// transcript, and the block comment above says why it may not.
///
/// Server-side rather than in the page, for two reasons. The budget policy is then testable in
/// Rust and shared with the billed harness that is the only thing able to say whether the vendor
/// honours the payload at all; and the transcript already lives here, so building it in the
/// browser would mean shipping the whole record to the page in order to throw most of it away.
///
/// **Disabled is a 200, not a refusal.** `enabled: false` with an empty `text` is a state the page
/// renders ("the agent starts fresh"); a 404 would be indistinguishable from a conversation that
/// was never stored, and the page would say the wrong one of those two things.
pub async fn replay(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(conversation_id): Path<String>,
) -> Result<Response, ApiError> {
    let id = ConversationId::parse(&conversation_id)?;
    let settings = &state.config.replay;
    // Not fetched at all when resuming is off. The point of the setting is that prior conversation
    // content is not re-sent to a vendor, and a server that assembled the payload anyway and let
    // the page decide would have already read the whole transcript to do it.
    let built = if settings.enabled {
        let turns = state.store.turns(&id).await?;
        crate::replay::build(&turns, &settings.policy)
    } else {
        crate::replay::build(&[], &settings.policy)
    };
    Ok(no_store(Json(ReplayResponse {
        text: built.text,
        included: built.included,
        dropped: built.dropped,
        truncated: built.truncated,
        policy: built.policy,
        transport: settings.transport,
        enabled: settings.enabled,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// One turn to record.
#[derive(Debug, Deserialize)]
pub struct AppendTurnRequest {
    /// Who said it.
    pub speaker: Speaker,
    /// What was said.
    pub text: String,
}

/// What the store did with it.
#[derive(Debug, Serialize)]
pub struct AppendTurnResponse {
    /// The conversation it went into.
    pub id: ConversationId,
    /// The turn as stored, including the SERVER's timestamp — the page must render this rather
    /// than its own clock, or a restored transcript is stamped with the moment it was reloaded.
    pub turn: Turn,
}

/// `POST /api/v1/conversations/{conversation_id}/turns`
pub async fn append_turn(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(conversation_id): Path<String>,
    Json(request): Json<AppendTurnRequest>,
) -> Result<Response, ApiError> {
    let id = ConversationId::parse(&conversation_id)?;
    let turn = Turn::now(request.speaker, request.text);
    state.store.append_turn(&id, &turn).await?;
    Ok(no_store(Json(AppendTurnResponse { id, turn })))
}

/// `DELETE /api/v1/conversations/{conversation_id}`
pub async fn forget_conversation(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(conversation_id): Path<String>,
) -> Result<Response, ApiError> {
    let id = ConversationId::parse(&conversation_id)?;
    state.store.forget_conversation(&id).await?;
    Ok(no_store(Json(serde_json::json!({ "forgotten": 1 }))))
}

/// `DELETE /api/v1/conversations`
pub async fn forget_conversations(
    State(state): State<AppState>,
    _scope: WriteScope,
) -> Result<Response, ApiError> {
    let forgotten = state.store.forget_all_conversations().await?;
    Ok(no_store(Json(
        serde_json::json!({ "forgotten": forgotten }),
    )))
}

/// `DELETE /api/v1/storage` — erase EVERYTHING this server holds in its store.
///
/// The operator's erase, and the only complete one of the store there is over HTTP. It is not a
/// complete erase of memory: see the end of this comment for the copies of channel text it leaves.
/// `DELETE /api/v1/conversations` clears transcripts and deliberately leaves the read marks
/// alone, because the two are different records and a control that quietly does more than it
/// says is the failure this project is written against. This route says it does all of it: the
/// transcripts, this server's read marks, the cached summaries — which are the one thing on
/// disk written by third parties — and the per-message inbox overlay `#50 todo-view` adds.
///
/// Write scope, obviously. It exists because [`crate::store::StateStore::purge_everything`] was
/// otherwise trait surface with three implementations and no caller: an erase nobody can invoke
/// is not an erase, it is a claim in a document.
///
/// It also turns every channel's clear-unread setting back to off (`#232 upstream-mark-read`), in
/// the running loop as well as in the store, so nothing more is marked read at a provider once
/// Settings says Off.
///
/// And it empties three of the copies of channel text held in memory: read-aloud's recently
/// fetched messages ([`crate::read_ahead::ReadAhead::clear`], `#191 read-aloud-first-lag`), the
/// voice catch-up's channel windows with their journalled pushes
/// ([`crate::recent::RecentWindows::clear`], `#210 voice-catchup-speed`), and the recent reads a
/// bridge's recent-view shadow holds waiting for their check ([`crate::recent_view::Shadow::clear`],
/// `#208 persistent-cache`). All three refuse what a read already running when the purge came
/// brings back, so none can be refilled from before it; a shadow check already running keeps only
/// counts from its read.
///
/// **It does not empty every copy in memory**, and nothing written about it may say it does. These
/// outlive it until they age out, are evicted, or the process restarts:
///
/// * the live stream's replay tail: up to [`crate::live::REPLAY_TAIL`] published changes per
///   channel, with their text, which a page that attaches after the purge is sent;
/// * read-aloud tickets: each holds one message's text as it will be spoken, at most
///   [`crate::speech_tickets::MAX_TICKETS`] of them, for up to
///   [`crate::speech_tickets::TICKET_TTL`];
/// * thread names and their one-sentence summaries already read from the summary cache: this route
///   erases the rows, but [`crate::summarize::threads::ThreadNames`] answers from what it read
///   before; and
/// * smaller records, such as the paren tags [`crate::heading::HeadingTags`] has seen.
///
/// Only a restart clears every copy held in memory.
pub async fn purge_storage(
    State(state): State<AppState>,
    _scope: WriteScope,
) -> Result<Response, ApiError> {
    let clearing = state.live.upstream_clear();
    let changing = clearing.changing().await;
    state.store.purge_everything().await?;
    let forgotten = clearing.forget_all();
    drop(changing);
    // Not durable, so not in the list below, but channel text all the same.
    state.read_ahead.clear();
    state.live.recent().clear();
    state.recent_view_shadow.clear();
    if forgotten > 0 {
        tracing::info!(
            channels = forgotten,
            "upstream-clear: the store was erased, so every channel is off and nothing more is \
             marked read at its provider"
        );
    }
    Ok(no_store(Json(serde_json::json!({
        "purged": ["conversations", "read_marks", "summaries", "dismissals", "upstream_clear"],
        "detail": "every durable record this server holds has been erased; the store is still \
                   open and usable",
    }))))
}

/// This server's own inbox state.
#[derive(Debug, Serialize)]
pub struct InboxResponse {
    /// Every configured channel, marked or not.
    pub channels: Vec<ops::InboxEntry>,
    /// Said on every inbox answer, not buried in a document: this read state is not Discord's,
    /// in either direction. See [`crate::store::INBOX_NOTICE`].
    pub read_state_notice: &'static str,
}

/// `GET /api/v1/inbox`
pub async fn inbox(State(state): State<AppState>, _scope: ReadScope) -> Result<Response, ApiError> {
    let channels = ops::inbox(&state).await?;
    Ok(no_store(Json(InboxResponse {
        channels,
        read_state_notice: crate::store::INBOX_NOTICE,
    })))
}

/// How far to move a read mark.
#[derive(Debug, Deserialize)]
pub struct MarkReadRequest {
    /// The newest message the owner has been shown.
    pub message_id: String,
    /// Selected thread, when the message came from a thread timeline.
    pub thread_id: Option<String>,
}

/// The mark after the move.
#[derive(Debug, Serialize)]
pub struct MarkReadResponse {
    /// The channel it is about.
    pub channel: ChannelInfo,
    /// The mark as it now stands. Not necessarily what was asked for: a mark never moves
    /// backwards, so a stale client is told where the mark really is.
    pub mark: ReadMark,
    /// See [`crate::store::INBOX_NOTICE`]. Repeated on the mutating route too, because this is
    /// exactly where someone expects the Discord badge to clear.
    pub read_state_notice: &'static str,
}

/// The result of moving the source provider's read cursor.
#[derive(Debug, Serialize)]
pub struct UpstreamReadResponse {
    /// Channel whose provider cursor moved.
    pub channel: ChannelInfo,
    /// Inclusive message boundary requested by the caller.
    pub through: MessageId,
    /// The contract common to every provider implementation.
    pub upstream_read_notice: &'static str,
}

/// What a successful source-provider read mark means.
pub const UPSTREAM_READ_NOTICE: &str = "The source chat provider's read cursor was moved through this message. Read cursors only move forward; thread-level behavior is defined by the provider.";

/// `POST /api/v1/channels/{channel_id}/upstream-read`
pub async fn mark_read_upstream(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<MarkReadRequest>,
) -> Result<Response, ApiError> {
    let channel = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    let through = MessageId(request.message_id);
    if let Some(thread_id) = request.thread_id.as_deref() {
        ops::message_by_id_scoped(&state, &channel_id, through.as_str(), None, Some(thread_id))
            .await?;
    }
    state.chat.mark_read_upstream(&channel.id, &through).await?;
    Ok(no_store(Json(UpstreamReadResponse {
        channel,
        through,
        upstream_read_notice: UPSTREAM_READ_NOTICE,
    })))
}

/// What the owner chose for clearing a channel's unread state at its provider.
/// `#232 upstream-mark-read`.
///
/// Only the mode. Pushing the newest message at once is its own act, `POST .../upstream-clear/now`
/// (`#253 upstream-clear-polish`), so that pressing it can never also change the mode a stale page
/// last saw.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamClearRequest {
    /// Off, a dry run, or on.
    pub mode: crate::store::UpstreamClearMode,
}

/// The longest `POST .../upstream-clear/now` holds the request, reading the channel's newest
/// message included, before answering that the push is still on its way. Long enough for a bridge
/// to answer one push, and under the 60 s read timeout common to reverse proxies, so a slow push is
/// answered `waiting` rather than cut off by the proxy as a gateway timeout.
/// `#253 upstream-clear-polish`.
pub const UPSTREAM_CLEAR_NOW_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// Whether `channel`'s own provider can clear its unread state.
fn upstream_clear_supported(state: &AppState, channel: &ChannelId) -> bool {
    state
        .providers
        .client_for(channel)
        .is_some_and(|client| client.supports_upstream_clear())
}

/// One channel's upstream-clear state, from the store and the running loop.
async fn upstream_clear_view(
    state: &AppState,
    channel: ChannelInfo,
) -> Result<crate::contract::UpstreamClearState, ApiError> {
    let supported = upstream_clear_supported(state, &channel.id);
    let setting = state
        .store
        .upstream_clear_settings()
        .await?
        .into_iter()
        .find(|setting| setting.channel == channel.id)
        .unwrap_or_else(|| crate::store::UpstreamClearSetting::unset(channel.id.clone()));
    let live = state.live.upstream_clear().status(&channel.id);
    let provider_name = state.providers.client_for(&channel.id).map_or_else(
        || state.chat.provider_name().to_owned(),
        |client| client.provider_name().to_owned(),
    );
    Ok(crate::contract::UpstreamClearState {
        channel,
        provider_name,
        supported,
        mode: if supported {
            setting.mode
        } else {
            crate::store::UpstreamClearMode::Off
        },
        enabled_at_ms: setting.enabled_at_ms,
        pushed: setting.pushed.as_ref().map(Into::into),
        previous_cursor: setting.previous_cursor,
        last: setting.last.as_ref().map(Into::into),
        failures: setting.failures,
        pending: live.pending,
        retry_at_ms: live.retry_at_ms,
        unavailable: live.unavailable,
    })
}

/// `GET /api/v1/channels/{channel_id}/upstream-clear` — whether this server clears the channel's
/// unread state at its provider, and what it last did. `#232 upstream-mark-read`.
pub async fn upstream_clear(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let channel = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    Ok(no_store(Json(upstream_clear_view(&state, channel).await?)))
}

/// `PUT /api/v1/channels/{channel_id}/upstream-clear` — turn clearing the channel's unread state
/// at its provider off, to a dry run, or on. `#232 upstream-mark-read`.
///
/// WRITE scope, and NO MCP TOOL: on, it marks every message in the channel read in the owner's
/// own client, people's messages included, which is a decision about his attention that only he
/// makes. Turning it on pushes nothing by itself; the next message that arrives is pushed, and
/// with it everything before it.
pub async fn set_upstream_clear(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<UpstreamClearRequest>,
) -> Result<Response, ApiError> {
    let channel = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    if request.mode != crate::store::UpstreamClearMode::Off
        && !upstream_clear_supported(&state, &channel.id)
    {
        return Err(ChatError::Refused(
            "this channel's chat provider cannot clear its unread state automatically".to_owned(),
        )
        .into());
    }
    let at_ms = jiff::Timestamp::now().as_millisecond();
    // The store and the running loop change together, never interleaved with another save or an
    // erase; see `Candidates::changing`.
    let changing = state.live.upstream_clear().changing().await;
    let setting = state
        .store
        .set_upstream_clear_mode(&channel.id, request.mode, at_ms)
        .await?;
    state.live.upstream_clear().set_mode(
        &channel.id,
        setting.mode,
        crate::upstream_clear::stored_floor(&setting),
    );
    drop(changing);
    tracing::info!(
        channel = %channel.id,
        mode = setting.mode.as_str(),
        "upstream-clear: channel {} ({}) set to {} in Settings",
        channel.id,
        channel.alias.as_deref().unwrap_or(&channel.label),
        setting.mode.as_str(),
    );
    Ok(no_store(Json(upstream_clear_view(&state, channel).await?)))
}

/// `POST /api/v1/channels/{channel_id}/upstream-clear/now` — Settings' "Mark read now" (or "Try it
/// now" in a dry run): push the channel's newest message at once, and answer what it came to.
/// `#253 upstream-clear-polish`.
///
/// WRITE scope and no MCP tool, like the setting itself. It changes no setting: a channel that is
/// off is refused, so a page that still shows On after another device turned it off cannot turn
/// it back on by pressing this. The push goes through the running loop like any other, so it is
/// never two provider calls at once and never moves the marker backwards; this request only waits,
/// at most [`UPSTREAM_CLEAR_NOW_WAIT`] in all, for the loop to record it. A channel waiting out a
/// failure keeps its wait: the message goes at the retry, and the answer says so.
///
/// The outcome is what answered THIS press (`Candidates::wait_for`): `pushed` only when the push
/// that covers the pressed message was taken by the provider, `failed` when it failed, `changed`
/// when the setting changed meanwhile, and the recorded attempt rides along so the page says what
/// it came to from that attempt rather than from whatever `state.last` holds by now.
pub async fn upstream_clear_now(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    use crate::contract::UpstreamClearNowOutcome as Outcome;
    use crate::store::UpstreamClearResult;
    use crate::upstream_clear::{PushNow, Settled};

    // One deadline for the whole request, the read of the newest message included.
    let deadline = tokio::time::Instant::now() + UPSTREAM_CLEAR_NOW_WAIT;
    let channel = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    let candidates = state.live.upstream_clear();
    let off = || {
        ApiError::from(ChatError::Refused(
            "clearing unread is off for this channel; turn it on, or to a dry run, and save first"
                .to_owned(),
        ))
    };
    if candidates.mode(&channel.id) == crate::store::UpstreamClearMode::Off {
        return Err(off());
    }
    let messages = tokio::time::timeout_at(deadline, state.chat.fetch_recent(&channel.id, 1))
        .await
        .map_err(|_elapsed| {
            ChatError::Transport(format!(
                "the channel's newest message could not be read within {} s, so nothing was pushed",
                UPSTREAM_CLEAR_NOW_WAIT.as_secs()
            ))
        })??;
    let newest = messages.last();
    let mut attempt = None;
    let outcome = match newest {
        None => Outcome::NoMessage,
        Some(newest) => match candidates.push_now(&channel.id, newest) {
            PushNow::Off => return Err(off()),
            PushNow::Unavailable => Outcome::Unavailable,
            PushNow::Unplaceable => Outcome::NoMessage,
            PushNow::Covered => Outcome::Covered,
            PushNow::RetryWaiting => Outcome::RetryWaiting,
            PushNow::Queued(ticket) => {
                match candidates.wait_for(&channel.id, ticket, deadline).await {
                    Settled::Recorded(recorded) => {
                        let outcome = match recorded.result {
                            UpstreamClearResult::Advanced
                            | UpstreamClearResult::AlreadyRead
                            | UpstreamClearResult::DryRun => Outcome::Pushed,
                            UpstreamClearResult::Failed => Outcome::Failed,
                            UpstreamClearResult::Skipped => Outcome::Skipped,
                            UpstreamClearResult::Unavailable => Outcome::Unavailable,
                        };
                        attempt = Some(crate::contract::UpstreamClearAttemptView::from(&recorded));
                        outcome
                    }
                    Settled::RetryWaiting => Outcome::RetryWaiting,
                    Settled::Unavailable => Outcome::Unavailable,
                    Settled::Changed => Outcome::Changed,
                    Settled::Waiting => Outcome::Waiting,
                }
            }
        },
    };
    tracing::info!(
        channel = %channel.id,
        outcome = outcome.as_str(),
        "upstream-clear: channel {} ({}) pushed now from Settings: {}",
        channel.id,
        channel.alias.as_deref().unwrap_or(&channel.label),
        outcome.as_str(),
    );
    Ok(no_store(Json(crate::contract::UpstreamClearNowResponse {
        outcome,
        message_id: newest.map(|newest| newest.id.clone()),
        message_at_ms: newest
            .and_then(crate::upstream_clear::position)
            .map(|position| position.0),
        attempt,
        state: upstream_clear_view(&state, channel).await?,
    })))
}

/// `POST /api/v1/channels/{channel_id}/read`
pub async fn mark_read(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<MarkReadRequest>,
) -> Result<Response, ApiError> {
    let (channel, mark) = ops::mark_read(
        &state,
        &channel_id,
        &crate::model::MessageId(request.message_id),
    )
    .await?;
    Ok(no_store(Json(MarkReadResponse {
        channel,
        mark,
        read_state_notice: crate::store::INBOX_NOTICE,
    })))
}

/// `DELETE /api/v1/channels/{channel_id}/read`
pub async fn forget_read_mark(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let channel = ops::forget_read_mark(&state, &channel_id).await?;
    Ok(no_store(Json(serde_json::json!({
        "channel": channel,
        "read_state_notice": crate::store::INBOX_NOTICE,
    }))))
}

/// The messages in one channel that have not been dealt with here.
#[derive(Debug, Serialize)]
pub struct TodoResponse {
    /// The to-do list itself: the channel, what is left, and how big the window was.
    #[serde(flatten)]
    pub todo: ops::TodoView,
    /// See [`crate::store::INBOX_NOTICE`]. On the LISTING as well as on the mutations, because
    /// this is the screen where "unread" means something to a Discord user and this is not that
    /// thing.
    pub read_state_notice: &'static str,
    /// The revision this channel's pins are at, as on a timeline read. `#206 pin-message`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pins_revision: Option<i64>,
    /// This channel's bookmark, as on a timeline read. `#240 reading-bookmark`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bookmark: Option<crate::store::Bookmark>,
    /// The messages are somebody else's words, exactly as on every other read.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/channels/{channel_id}/todo`
pub async fn todo(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, ApiError> {
    let mut todo = ops::todo(&state, &channel_id, query.limit).await?;
    crate::render::fill(
        &state.config,
        &state.headings,
        &todo.channel,
        &mut todo.messages,
    );
    let pins_revision = ops::pins_revision(&state, &todo.channel.id).await;
    let bookmark = ops::bookmark_state(&state, &todo.channel.id).await;
    Ok(no_store(Json(TodoResponse {
        pins_revision,
        bookmark,
        todo,
        read_state_notice: crate::store::INBOX_NOTICE,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// Which messages to mark as dealt with, or put back.
///
/// Exactly one of the two, and supplying both is a refusal rather than a precedence rule: a
/// caller that sent both meant one of them, and guessing which would be wrong half the time in a
/// way that quietly clears a backlog.
#[derive(Debug, Default, Deserialize)]
pub struct DismissRequest {
    /// The messages named one by one.
    #[serde(default)]
    pub messages: Vec<String>,
    /// Or: everything in the window at or before this one. The boundary is INCLUDED.
    #[serde(default)]
    pub through: Option<String>,
    /// The window the caller READ the list with, so `through` cannot clear more than was shown.
    ///
    /// The same number sent to `GET /todo?limit=`. Without it a client that paged three messages
    /// and gave up on them would clear the default fifty, because the boundary would be resolved
    /// against a window it never displayed — clearing work the owner never saw, which is the worst
    /// failure this view has. Ignored when `messages` names the ids one by one: there the window is
    /// only an existence check, so narrowing it could only refuse an id the caller really did see.
    #[serde(default)]
    pub limit: Option<u16>,
}

/// What a dismissal or a restoration actually did.
#[derive(Debug, Serialize)]
pub struct InboxChangeResponse {
    /// The channel and the exact set that changed.
    #[serde(flatten)]
    pub change: ops::InboxChange,
    /// How many messages changed state. The list beside it is what an undo needs; this is what a
    /// confirmation says out loud.
    pub count: usize,
    /// See [`crate::store::INBOX_NOTICE`]. Repeated on the mutating routes, because this is
    /// exactly where someone expects the Discord badge to move.
    pub read_state_notice: &'static str,
}

fn answered(change: ops::InboxChange) -> Response {
    let count = change.messages.len();
    no_store(Json(InboxChangeResponse {
        change,
        count,
        read_state_notice: crate::store::INBOX_NOTICE,
    }))
}

/// Why a dismissal that names both ways of choosing, or neither, is refused.
///
/// Held as constants so the match below stays one line per case and reads as the table of four it
/// is. Guessing between the two would be wrong half the time, and the wrong half CLEARS A BACKLOG.
const NAMES_BOTH_WAYS: &str = "send either `messages` or `through`, not both: they are different \
                               acts and guessing which one you meant would clear a backlog by \
                               accident";
const NAMES_NEITHER: &str = "send `messages` or `through`; an empty request would report success \
                             for having done nothing";

/// `POST /api/v1/channels/{channel_id}/dismiss`
pub async fn dismiss(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<DismissRequest>,
) -> Result<Response, ApiError> {
    let change = match (request.through, request.messages.is_empty()) {
        (Some(_), false) => return Err(ApiError::bad_request(NAMES_BOTH_WAYS)),
        (Some(through), true) => {
            // WITH the window the caller read the list with. See `DismissRequest::limit`.
            let boundary = crate::model::MessageId(through);
            ops::declare_bankruptcy(&state, &channel_id, &boundary, request.limit).await?
        }
        (None, false) => {
            let wanted: Vec<crate::model::MessageId> = request
                .messages
                .into_iter()
                .map(crate::model::MessageId)
                .collect();
            ops::dismiss(&state, &channel_id, &wanted).await?
        }
        (None, true) => return Err(ApiError::bad_request(NAMES_NEITHER)),
    };
    Ok(answered(change))
}

/// `POST /api/v1/channels/{channel_id}/restore`
pub async fn restore(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<DismissRequest>,
) -> Result<Response, ApiError> {
    if request.messages.is_empty() {
        return Err(ApiError::bad_request(
            "send the `messages` an earlier dismissal reported; restoring nothing is not an undo",
        ));
    }
    let wanted: Vec<crate::model::MessageId> = request
        .messages
        .into_iter()
        .map(crate::model::MessageId)
        .collect();
    Ok(answered(ops::restore(&state, &channel_id, &wanted).await?))
}

// --- pinned messages ------------------------------------------------------------------------------
//
// `#206 pin-message`. READ scope to list, WRITE scope to pin or unpin — the split every durable
// write here takes, because a pin outlives the process and another device reads it back. And, as
// with `#39 channel-alias` and `#196 auto-read-noise`, the scope is not what keeps a voice agent
// out: no MCP tool lists, pins or unpins, so a model is never handed the list and cannot change it.

/// The standing statement that a pin here is this server's own.
pub const PINS_NOTICE: &str =
    "Pins are vibe-talk's own. They are kept on this server and shown on \
                               every device signed in to it; the source chat service's own pinned \
                               messages are not read, and pinning here pins nothing there.";

/// What to keep of a message being pinned. Its id is the path's; everything else is the row the
/// page is showing, as it holds it. See [`crate::store::PinSnapshot`] for what each field is for.
#[derive(Debug, Deserialize)]
pub struct PinRequest {
    /// The author's display name.
    pub author: String,
    /// The author's id.
    pub author_id: String,
    /// Whether the provider flagged the author as a bot.
    #[serde(default)]
    pub author_is_bot: bool,
    /// The message's text as the page holds it. Cut to [`crate::store::MAX_PIN_TEXT_CHARS`]
    /// here, not refused.
    pub content: String,
    /// When the message was sent, ISO-8601, as the page was served it.
    pub timestamp: String,
    /// The thread the message is in, when it is in one.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Whether the message is that thread's root.
    #[serde(default)]
    pub thread_root: bool,
}

/// `GET /api/v1/channels/{channel_id}/pins` — every pin in the channel, oldest message first.
pub async fn pins(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let (channel, mut pins) = ops::pins(&state, &channel_id).await?;
    crate::render::fill_pins(&state.config, &state.headings, &channel, &mut pins.pins);
    Ok(no_store(Json(crate::contract::PinsResponse {
        channel,
        pins: pins.pins,
        revision: pins.revision,
        limit: crate::store::MAX_PINS_PER_CHANNEL,
        pins_notice: PINS_NOTICE,
        untrusted_content_notice: untrusted::NOTICE,
    })))
}

/// `PUT /api/v1/channels/{channel_id}/pins/{message_id}` — pin a message. Idempotent.
///
/// PUT because the act is "make this message pinned": sending it twice leaves the same state,
/// and the second answers `changed: false`. At [`crate::store::MAX_PINS_PER_CHANNEL`] the oldest
/// other pin in the channel is unpinned to make room and named in `unpinned`.
pub async fn pin_message(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path((channel_id, message_id)): Path<(String, String)>,
    Json(request): Json<PinRequest>,
) -> Result<Response, ApiError> {
    let snapshot = crate::store::PinSnapshot {
        message_id: MessageId(message_id),
        author: request.author,
        author_id: UserId(request.author_id),
        author_is_bot: request.author_is_bot,
        content: request.content,
        truncated: false,
        timestamp: request.timestamp,
        thread_id: request.thread_id,
        thread_root: request.thread_root,
    };
    let (channel, mut change) = ops::pin(&state, &channel_id, &snapshot).await?;
    crate::render::fill_pins(&state.config, &state.headings, &channel, &mut change.pin);
    Ok(no_store(Json(crate::contract::PinChangeResponse {
        channel,
        message_id: snapshot.message_id,
        pinned: true,
        changed: change.changed,
        pin: change.pin,
        unpinned: change.unpinned,
        revision: change.revision,
        pins_notice: PINS_NOTICE,
    })))
}

/// `DELETE /api/v1/channels/{channel_id}/pins/{message_id}` — unpin a message. Idempotent.
pub async fn unpin_message(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path((channel_id, message_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let message = MessageId(message_id);
    let (channel, change) = ops::unpin(&state, &channel_id, &message).await?;
    Ok(no_store(Json(crate::contract::PinChangeResponse {
        channel,
        message_id: message,
        pinned: false,
        changed: change.changed,
        pin: None,
        unpinned: change.unpinned,
        revision: change.revision,
        pins_notice: PINS_NOTICE,
    })))
}

// --- the reading bookmark ------------------------------------------------------------------------
//
// `#240 reading-bookmark`. READ scope to see it, WRITE scope to move or clear it, and NO MCP TOOL,
// for the `#39` reason the pins give: the scope is not what keeps a voice agent out.

/// The standing statement that the bookmark is this server's own and marks nothing read.
pub const BOOKMARK_NOTICE: &str = "The bookmark is vibe-talk's own. It is kept on this server \
                                   and shown on every device signed in to it; setting it marks \
                                   nothing read, and nothing is sent to the source chat service.";

/// Where to put a channel's bookmark. See [`crate::store::BookmarkAnchor`] for each field.
#[derive(Debug, Deserialize)]
pub struct BookmarkRequest {
    /// The message the line is drawn next to.
    pub message_id: String,
    /// Which side of it; above unless said.
    #[serde(default = "above")]
    pub place: crate::store::BookmarkPlace,
    /// When that message was sent, ISO-8601, as the page was served it.
    pub timestamp: String,
    /// The thread that message is in, when it is in one.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Only if this is the revision stored now; a stale one is answered 409. Absent: whatever is
    /// stored, which is the owner's own act.
    #[serde(default)]
    pub if_revision: Option<i64>,
}

fn above() -> crate::store::BookmarkPlace {
    crate::store::BookmarkPlace::Above
}

/// The condition a clear may carry, in the query string because a DELETE has no body.
#[derive(Debug, Default, Deserialize)]
pub struct BookmarkClearQuery {
    /// Only if this is the revision stored now, as on a PUT.
    #[serde(default)]
    pub if_revision: Option<i64>,
}

/// A conditional write that lost: [`crate::contract::BookmarkResponse`] with an
/// [`ApiErrorBody`]'s two fields beside it.
#[derive(Debug, Serialize)]
struct BookmarkConflict {
    error: &'static str,
    detail: &'static str,
    #[serde(flatten)]
    answer: crate::contract::BookmarkResponse,
}

/// The answer to a bookmark write: 200 with where it is, or 409 with where it is when the write's
/// `if_revision` was stale.
fn bookmark_answered(channel: ChannelInfo, change: crate::store::BookmarkChange) -> Response {
    let answer = crate::contract::BookmarkResponse {
        channel,
        bookmark: change.bookmark,
        changed: change.changed,
        bookmark_notice: BOOKMARK_NOTICE,
    };
    if change.applied {
        return no_store(Json(answer));
    }
    let mut response = no_store(Json(BookmarkConflict {
        error: "bookmark_moved",
        detail: "the bookmark was moved since the revision this write named; the answer says \
                 where it is now",
        answer,
    }));
    *response.status_mut() = StatusCode::CONFLICT;
    response
}

/// `GET /api/v1/channels/{channel_id}/bookmark`
pub async fn bookmark(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let (channel, bookmark) = ops::bookmark(&state, &channel_id).await?;
    Ok(no_store(Json(crate::contract::BookmarkResponse {
        channel,
        bookmark,
        changed: false,
        bookmark_notice: BOOKMARK_NOTICE,
    })))
}

/// `PUT /api/v1/channels/{channel_id}/bookmark` — put the bookmark next to a message.
///
/// PUT because the act is "make the bookmark be here": sent twice it leaves the same state, and the
/// second answers `changed: false` at the same revision.
pub async fn set_bookmark(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Json(request): Json<BookmarkRequest>,
) -> Result<Response, ApiError> {
    let anchor = crate::store::BookmarkAnchor {
        message_id: MessageId(request.message_id),
        place: request.place,
        timestamp: request.timestamp,
        thread_id: request.thread_id,
    };
    let (channel, change) =
        ops::set_bookmark(&state, &channel_id, Some(&anchor), request.if_revision).await?;
    Ok(bookmark_answered(channel, change))
}

/// `DELETE /api/v1/channels/{channel_id}/bookmark` — take the bookmark away. Idempotent.
pub async fn clear_bookmark(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
    Query(query): Query<BookmarkClearQuery>,
) -> Result<Response, ApiError> {
    let (channel, change) = ops::set_bookmark(&state, &channel_id, None, query.if_revision).await?;
    Ok(bookmark_answered(channel, change))
}

/// One message, summarised.
#[derive(Debug, Serialize)]
pub struct MessageSummaryResponse {
    /// The channel it came from.
    pub channel: ChannelInfo,
    /// The message it is about.
    pub message_id: String,
    /// Whether it was below the threshold, served from the cache, or generated now. A page shows
    /// the original for `below_threshold`; a shortened copy of something already short would be
    /// a claim that work was done.
    #[serde(flatten)]
    pub outcome: ops::SummaryOutcome,
    /// Which backend produced it, said out loud so a page can never imply a model summary it did
    /// not get.
    pub backend: &'static str,
    /// The cache key's policy component. Changes the moment any setting that decides what a
    /// summary says changes.
    pub version: String,
    /// The length below which nothing is summarised, so a client can avoid asking at all.
    pub threshold_chars: usize,
    /// How long the summariser took, in milliseconds, or `null` when none was called.
    ///
    /// Answered rather than merely logged because the question "how does a round trip to a hosted
    /// conversational agent compare with a full-size model" is one a deployment should be able to
    /// answer from its own traffic, on the first real run, without instrumenting anything.
    /// `null` for a cache hit or a message below the threshold: a zero would claim the vendor
    /// answered instantly, which is a different thing from not having asked it.
    pub generated_in_ms: Option<u64>,
    /// Standing reminder that a summary of third-party text is still third-party text.
    pub untrusted_content_notice: &'static str,
}

/// `GET /api/v1/channels/{channel_id}/messages/{message_id}/summary`
pub async fn message_summary(
    State(state): State<AppState>,
    ReadScope(caller): ReadScope,
    Path((channel_id, message_id)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    // The granted scope, not the required one: a read token may be served FROM the cache but
    // never writes to it. See `ops::summarize_message`.
    // Empty segments dropped rather than rejected: `with=` and a trailing comma both mean "no
    // others", and refusing them would turn a harmless way of writing the URL into a 400.
    let also: Vec<String> = query
        .with
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let answer = ops::summarize_message_scoped(
        &state,
        caller,
        &channel_id,
        &message_id,
        &also,
        query.limit,
        query.thread_id.as_deref(),
    )
    .await?;
    Ok(Json(MessageSummaryResponse {
        channel: answer.channel,
        message_id,
        outcome: answer.outcome,
        backend: state.summarizer.describe(),
        version: state.summary_version.to_string(),
        threshold_chars: state.config.summaries.threshold_chars,
        generated_in_ms: answer.generated_in_ms,
        untrusted_content_notice: untrusted::NOTICE,
    }))
}

/// Answer with `Cache-Control: no-store`.
///
/// Every durable-state answer is the owner's own speech or his reading position. Neither belongs
/// in a proxy cache or in a browser's back-forward cache, and the transcript routes are the first
/// on this server whose body is private to one person rather than merely credentialed.
fn no_store<T: IntoResponse>(body: T) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// What read-aloud mode asks for the moment it is turned on.
#[derive(Debug, Deserialize)]
pub struct PrepareSpeechRequest {
    /// Thread shared by these messages; absent uses the ordinary channel window.
    pub thread_id: Option<String>,
    /// The messages currently on screen, in the order they are drawn.
    pub ids: Vec<String>,
    /// The pace the reader has chosen, if they have chosen one.
    pub speed: Option<f64>,
    /// How many recent messages to look through for those ids.
    pub limit: Option<u16>,
}

/// One message, ready to play.
#[derive(Debug, Serialize)]
pub struct PreparedSpeech {
    /// The Discord message id, so the page can hang this on the right row.
    pub message_id: String,
    /// Where to play it from. Carries its own authority; see [`crate::speech_tickets`].
    pub url: String,
}

/// Everything the page needs to make a tap instant.
#[derive(Debug, Serialize)]
pub struct PrepareSpeechResponse {
    /// One entry per id that resolved. An id this server cannot see is simply absent.
    pub prepared: Vec<PreparedSpeech>,
    /// How long these stay playable, so the page knows when to ask again.
    pub expires_in_seconds: u64,
}

/// `POST /api/v1/channels/{channel_id}/speech/prepare`
///
/// DO THE WORK BEFORE THE TAP, NOT DURING IT.
///
/// A tap used to pay for four round trips in series: a window of fifty messages from Discord to
/// find one by id, the speech normalisation of its text, a lookup of which voice the agent speaks
/// in, and only then the audio. The reader sat through all of it having already been shown the very
/// text they were waiting to hear.
///
/// This does the first three ONCE, for everything on screen, at the moment read-aloud is switched
/// on — one Discord window covers every id instead of one window per tap — and hands back a ticket
/// per message. What is left for the tap is a map lookup and the vendor.
///
/// Asking the vendor for its voice here is not incidental: it is also what opens the TLS connection
/// that the audio request then reuses.
///
/// `#191 read-aloud-first-lag`. THE TAP MAY BE WAITING ON THIS, so it does as little as it can:
///
/// - the provider warm-up starts first and is not awaited (see
///   [`crate::read_ahead::ReadAhead::start_warm_up`]);
/// - messages the page fetched in the last thirty seconds are read from memory rather than fetched
///   again, and what is missing is fetched once per window, or concurrently in a thread (see
///   [`crate::read_ahead::messages_for_speech`]);
/// - one content-free line records how long each phase took.
pub async fn prepare_speech(
    State(state): State<AppState>,
    _scope: ReadScope,
    Path(channel_id): Path<String>,
    Json(request): Json<PrepareSpeechRequest>,
) -> Result<Json<PrepareSpeechResponse>, ApiError> {
    let preparation_began = std::time::Instant::now();
    if state.speech.describe().playback == crate::speech::Playback::Browser {
        return Err(SpeechError::BrowserPlaybackRequired.into());
    }
    let thread = request.thread_id.as_deref();
    if thread.is_some() && request.ids.len() > usize::from(ops::MAX_PAGE) {
        return Err(ApiError::bad_request(
            "too many messages in one speech preparation",
        ));
    }
    // The allowlist, before anything is started or read.
    let channel = state.channel(&channel_id).ok_or(OpError::UnknownChannel)?;
    // Best effort: a provider can move transport establishment and session readiness ahead of
    // the tap. Failure still belongs to the actual playback request, where it is actionable.
    let warm_up = if state
        .read_ahead
        .start_warm_up(std::sync::Arc::clone(&state.speech))
    {
        "started"
    } else {
        "already_running"
    };
    let found = crate::read_ahead::messages_for_speech(
        &state,
        &channel,
        thread,
        &request.ids,
        request.limit,
        preparation_began,
    )
    .await?;
    let normalizing = std::time::Instant::now();
    let now = jiff::Timestamp::now().as_millisecond();
    let mut resolved = Vec::new();
    for id in &request.ids {
        // Silently absent rather than a 404 for the batch. The page sends what it has on screen,
        // and a row that scrolled out from under the request is not an error the reader did
        // anything to cause — failing the whole batch over one id would lose the other forty.
        let Some(message) = found.messages.get(id) else {
            continue;
        };
        let said = crate::speakable::for_speech(&message.content, now, &state.config.timezone);
        if said.trim().is_empty() {
            continue;
        }
        resolved.push((message.id.0.clone(), said));
    }
    // Message preparation is lookup, fetch and normalization. The provider warm-up is a separate
    // phase, running beside this one and logged when it ends; its readiness reaches the tap
    // through the warmed stream.
    let preparation_ms = millis(preparation_began.elapsed());
    tracing::info!(
        ids = request.ids.len(),
        threaded = thread.is_some(),
        recent_hits = found.hits,
        fetches = found.fetches,
        joined_fetch = found.joined,
        resolved = resolved.len(),
        lookup_ms = millis(found.lookup),
        fetch_ms = millis(found.fetch),
        normalize_ms = millis(normalizing.elapsed()),
        preparation_ms,
        warm_up,
        "read-aloud preparation"
    );
    let mut prepared = Vec::new();
    for (message_id, said) in resolved {
        let ticket = state.speech_tickets.mint(crate::speech_tickets::Prepared {
            channel: channel.id.0.clone(),
            message: message_id.clone(),
            said,
            speed: crate::speech::clamp_speed(request.speed),
            observation: crate::speech_tickets::fresh_observation(),
            preparation_ms,
        });
        prepared.push(PreparedSpeech {
            message_id,
            url: format!("/api/v1/speech/{ticket}"),
        });
    }
    Ok(Json(PrepareSpeechResponse {
        prepared,
        expires_in_seconds: crate::speech_tickets::TICKET_TTL.as_secs(),
    }))
}

/// Query parameters for playing a prepared message.
#[derive(Debug, Default, Deserialize)]
pub struct PlayQuery {
    /// Opaque id the page gives one tap. A request naming a different selection from the read in
    /// progress interrupts it; parts of one selection play in order. Letters, digits, `-` and `_`,
    /// at most 64. Omitted, the read queues behind whatever is playing.
    pub selection: Option<String>,
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// `GET /api/v1/speech/{ticket}` — the whole of the tap.
///
/// NO `Authorization` HEADER, AND THAT IS THE POINT. An `<audio src>` cannot send one, and without
/// an `<audio src>` the browser cannot stream: it would have to fetch the whole file into a blob
/// first, which is the wait this change exists to remove. So the ticket carries its own authority.
/// It is 256 unguessable bits naming one already-resolved message and it expires in minutes; see
/// [`crate::speech_tickets`] for the whole of that argument.
///
/// A GET, unlike `/speak`, because this is now safe to re-issue: the ticket was minted deliberately
/// and names text this server chose. What a speculative fetch costs is one already-authorised
/// message being generated slightly early.
pub async fn play_speech(
    State(state): State<AppState>,
    SpeechTicket(ticket): SpeechTicket,
    Query(query): Query<PlayQuery>,
) -> Result<Response, ApiError> {
    let selection = query.selection.as_deref();
    if selection.is_some_and(|selection| {
        selection.is_empty()
            || selection.len() > 64
            || !selection
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return Err(ApiError::bad_request(
            "selection must be 1 to 64 letters, digits, '-' or '_'",
        ));
    }
    // `SpeechTicket` found it live a moment ago, but it can lapse in between.
    let Some(prepared) = state.speech_tickets.claim(&ticket) else {
        return Err(unknown_speech_ticket());
    };
    let requested = std::time::Instant::now();
    let mut stream = state
        .speech
        .speak_stream_for_selection(&prepared.said, prepared.speed, selection)
        .await?;
    let first_audio = match stream.chunks.next().await {
        Some(Ok(chunk)) => chunk,
        Some(Err(error)) => return Err(error.into()),
        None => {
            return Err(SpeechError::Backend {
                code: "speech_no_audio",
                detail: "the speech provider completed without returning audio".to_owned(),
            }
            .into())
        }
    };
    let request_to_first_audio = requested.elapsed();
    let session = stream.session;
    let (request_connection, request_session_start, yielded) =
        session.map_or(Default::default(), |session| {
            (
                session.waited.connection,
                session.waited.start,
                session.yielded,
            )
        });
    let server_timing = crate::speech_tickets::ServerTiming {
        request_to_first_audio_ms: millis(request_to_first_audio),
        request_connection_ms: millis(request_connection),
        request_session_start_ms: millis(request_session_start),
        yield_ms: millis(yielded),
        // A remainder rather than a separate stopwatch, so the four phases sum exactly.
        first_audio_ms: millis(
            request_to_first_audio
                .saturating_sub(request_connection)
                .saturating_sub(request_session_start)
                .saturating_sub(yielded),
        ),
        session,
    };
    state
        .speech_tickets
        .record_server_timing(&ticket, server_timing);
    tracing::info!(
        observation = prepared.observation,
        message_preparation_ms = prepared.preparation_ms,
        session = session.map(|session| session.generation),
        turn = session.map(|session| session.turn),
        session_reused = session.map(|session| session.reused),
        retried = session.map(|session| session.retried),
        session_connection_ms = session.map(|session| millis(session.setup.connection)),
        session_start_ms = session.map(|session| millis(session.setup.start)),
        request_connection_ms = server_timing.request_connection_ms,
        request_session_start_ms = server_timing.request_session_start_ms,
        yield_ms = server_timing.yield_ms,
        first_audio_ms = server_timing.first_audio_ms,
        request_to_first_audio_ms = server_timing.request_to_first_audio_ms,
        "read-aloud audio reached the server"
    );
    let mut timing = format!(
        "message_prepare;dur={}, request_connect;dur={}, session_start;dur={}, yield;dur={}, first_audio;dur={}, request_to_first_audio;dur={}",
        prepared.preparation_ms,
        server_timing.request_connection_ms,
        server_timing.request_session_start_ms,
        server_timing.yield_ms,
        server_timing.first_audio_ms,
        server_timing.request_to_first_audio_ms,
    );
    if let Some(session) = session {
        use std::fmt::Write as _;
        let _ = write!(
            timing,
            ", session;desc=\"generation={} turn={} reused={}\"",
            session.generation, session.turn, session.reused
        );
    }
    let prefix = stream
        .preamble
        .into_iter()
        .map(Ok::<_, SpeechError>)
        .chain(std::iter::once(Ok(first_audio)));
    let body =
        axum::body::Body::from_stream(futures_util::stream::iter(prefix).chain(stream.chunks));
    Ok((
        [
            (header::CONTENT_TYPE, stream.content_type.as_str()),
            (header::CACHE_CONTROL, "no-store"),
            // There is no length to seek within and none is claimed. A browser told it may range
            // over a stream that cannot will ask for one and stall.
            (header::ACCEPT_RANGES, "none"),
            (
                header::HeaderName::from_static("server-timing"),
                timing.as_str(),
            ),
        ],
        body,
    )
        .into_response())
}

/// Browser-side milestones for one prepared audio element. Durations contain no page or message
/// content and are capped before logging so malformed clients cannot create absurd observations.
#[derive(Debug, Deserialize)]
pub struct SpeechPlaybackTiming {
    /// Tap until `play()` was invoked, including any preparation join.
    pub tap_to_play_ms: u64,
    /// Audio-source creation until the browser had the first media frame (`loadeddata`).
    pub request_to_loaded_ms: u64,
    /// First media frame until the `playing` event, our closest browser-visible audible boundary.
    pub loaded_to_playing_ms: u64,
    /// Tap until `playing`, measured directly rather than reconstructed.
    pub tap_to_audible_ms: u64,
    /// Tap until the first sample loud enough to be speech was due at the speaker.
    ///
    /// `playing` counts the provider's leading silence as audible; this does not. Absent when the
    /// page could not hear its own samples (the `<audio>` fallback) or no speech arrived in time,
    /// and from pages older than it. `#191 read-aloud-first-lag`.
    #[serde(default)]
    pub tap_to_speech_ms: Option<u64>,
}

/// Record the browser half of an already-authorized read-aloud observation.
///
/// There is intentionally no bearer header: the random ticket is the same narrow capability the
/// audio element used for playback. The report adds only bounded durations and cannot select text.
pub async fn speech_playback_timing(
    State(state): State<AppState>,
    SpeechTicket(ticket): SpeechTicket,
    Json(timing): Json<SpeechPlaybackTiming>,
) -> Result<StatusCode, ApiError> {
    const MAX_TIMING_MS: u64 = 10 * 60 * 1_000;
    if [
        timing.tap_to_play_ms,
        timing.request_to_loaded_ms,
        timing.loaded_to_playing_ms,
        timing.tap_to_audible_ms,
        timing.tap_to_speech_ms.unwrap_or(0),
    ]
    .into_iter()
    .any(|value| value > MAX_TIMING_MS)
    {
        return Err(ApiError::bad_request(
            "read-aloud timing values must be at most 600000 milliseconds",
        ));
    }
    let Some((prepared, server)) = state.speech_tickets.observed_timing(&ticket) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_speech_ticket",
            "that speech timing ticket is unknown, expired, or has not produced audio",
        ));
    };
    // `loadeddata` happens after the server had its first audio chunk AND that chunk crossed the
    // network into the browser's decoder. Both intervals start at the same request, so what is
    // left after removing the server's share is delivery plus browser buffering — without having
    // to compare the host's clock with the phone's.
    let transfer_ms = timing
        .request_to_loaded_ms
        .saturating_sub(server.request_to_first_audio_ms);
    tracing::info!(
        observation = prepared.observation,
        message_preparation_ms = prepared.preparation_ms,
        session = server.session.map(|session| session.generation),
        turn = server.session.map(|session| session.turn),
        session_reused = server.session.map(|session| session.reused),
        request_connection_ms = server.request_connection_ms,
        request_session_start_ms = server.request_session_start_ms,
        yield_ms = server.yield_ms,
        first_audio_ms = server.first_audio_ms,
        request_to_first_audio_ms = server.request_to_first_audio_ms,
        transfer_ms,
        browser_playback_ms = timing.loaded_to_playing_ms,
        tap_to_play_ms = timing.tap_to_play_ms,
        tap_to_audible_ms = timing.tap_to_audible_ms,
        tap_to_speech_ms = timing.tap_to_speech_ms,
        "read-aloud reached audible playback"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug)]
struct Present<T> {
    present: bool,
    value: Option<T>,
}

impl<T> Default for Present<T> {
    fn default() -> Self {
        Self {
            present: false,
            value: None,
        }
    }
}

impl<'de, T> Deserialize<'de> for Present<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            present: true,
            value: Option::<T>::deserialize(deserializer)?,
        })
    }
}

/// One page of the channel directory, as the browser asks for it. `#19 channel-browser`.
#[derive(Debug, Deserialize)]
pub struct ChannelDirectoryQuery {
    /// Case-insensitive name filter. Trimmed; blank lists everything.
    #[serde(default)]
    pub q: Option<String>,
    /// Opaque continuation returned by the previous page of the same search.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Entries per page, `1..=50`, default 25.
    #[serde(default)]
    pub limit: Option<u16>,
}

/// A channel the bridge can see, and whether this app already tracks it.
#[derive(Debug, Serialize)]
pub struct ChannelDirectoryEntry {
    /// Opaque reference to send back as `source` when adding it.
    pub source: String,
    /// The channel's display name upstream.
    pub name: String,
    /// Whether a channel in this app's list already reads this source.
    pub tracked: bool,
    /// That channel's id, when tracked.
    pub channel_id: Option<ChannelId>,
}

/// One page of the channel directory.
#[derive(Debug, Serialize)]
pub struct ChannelDirectoryResponse {
    /// Entries in the bridge's order.
    pub entries: Vec<ChannelDirectoryEntry>,
    /// Continuation for the next page, or null on the last one.
    pub next_cursor: Option<String>,
    /// Whether the bridge stopped listing before the end of what its account can see.
    pub truncated: bool,
}

/// `GET /api/v1/channel-directory` — browse the channels the bridge can see. `#19 channel-browser`.
///
/// WRITE SCOPE, like adding. The list names every channel the bridge's account can see, not only
/// the ones this app reads, and its only purpose is choosing one to add, so a read-only token has
/// no business enumerating it. No MCP tool, for the reason `add_channel` has none.
///
/// Tracked entries are marked here rather than by the bridge alone: a source is tracked when the
/// bridge's existing registration for it, or the source itself, is a channel in this app's list.
pub async fn channel_directory(
    State(state): State<AppState>,
    _scope: WriteScope,
    Query(query): Query<ChannelDirectoryQuery>,
) -> Result<Response, ApiError> {
    use crate::directory::{
        DirectoryRequest, DEFAULT_LIMIT, MAX_LIMIT, MAX_QUERY_CHARS, MAX_TOKEN_BYTES,
    };

    if !state.chat.supports_channel_discovery() {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "channel_directory_unsupported",
            format!(
                "{} cannot list channels through this chat connector; add one by link or reference instead",
                state.chat.provider_name()
            ),
        ));
    }
    let search = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if search.is_some_and(|text| {
        text.chars().count() > MAX_QUERY_CHARS || text.chars().any(char::is_control)
    }) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_directory_query",
            format!("a search must be at most {MAX_QUERY_CHARS} characters of ordinary text"),
        ));
    }
    if query.cursor.as_deref().is_some_and(|cursor| {
        cursor.is_empty() || cursor.len() > MAX_TOKEN_BYTES || cursor.chars().any(char::is_control)
    }) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_directory_cursor",
            "cursor must be a continuation returned by the previous page",
        ));
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_directory_limit",
            format!("limit must be between 1 and {MAX_LIMIT}"),
        ));
    }
    let request = DirectoryRequest {
        query: search.map(str::to_owned),
        cursor: query.cursor,
        limit,
    };
    let page = state
        .chat
        .discover_channels(&request)
        .await
        .map_err(directory_error)?;
    let local: std::collections::BTreeSet<String> = state
        .all_channels()
        .into_iter()
        .map(|channel| channel.id.as_str().to_owned())
        .collect();
    let entries = page
        .entries
        .into_iter()
        .map(|entry| {
            let channel_id = entry
                .registered_channel_id
                .filter(|id| local.contains(id.as_str()))
                .or_else(|| {
                    local
                        .contains(&entry.source)
                        .then(|| ChannelId(entry.source.clone()))
                });
            ChannelDirectoryEntry {
                source: entry.source,
                name: entry.name,
                tracked: channel_id.is_some(),
                channel_id,
            }
        })
        .collect();
    Ok(no_store(Json(ChannelDirectoryResponse {
        entries,
        next_cursor: page.next_cursor,
        truncated: page.truncated,
    })))
}

/// Keep the three failures a person can act on apart: the bridge said no, the bridge does not
/// offer a directory, and the bridge refused this particular request. Everything else is the
/// ordinary upstream failure.
fn directory_error(error: ChatError) -> ApiError {
    let status = match error.cause() {
        ChatError::Status { status, .. } => Some(*status),
        _ => None,
    };
    match status {
        Some(401 | 403) => ApiError::new(
            StatusCode::FORBIDDEN,
            "channel_directory_denied",
            error.to_string(),
        ),
        Some(404 | 405 | 501) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "channel_directory_unsupported",
            error.to_string(),
        ),
        Some(400) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "channel_directory_rejected",
            error.to_string(),
        ),
        _ => error.into(),
    }
}

/// Adding a channel from inside the app.
#[derive(Debug, Deserialize)]
pub struct AddChannelRequest {
    /// A direct Discord snowflake. Used when provider-managed registration is disabled.
    #[serde(default)]
    id: Present<String>,
    /// A provider-neutral channel link or reference. Used when managed registration is enabled.
    #[serde(default)]
    source: Present<String>,
    /// What to call it. The configured channels get theirs from the file; this one gets it here.
    label: String,
    /// Whether the bridge may POST here. Omission defaults to false for direct targets. Managed
    /// targets must omit the field entirely because their bridge declares the policy.
    #[serde(default)]
    writable: Present<bool>,
}

/// What happened when a channel was added, removed, or shown again.
#[derive(Debug, Serialize)]
pub struct AddChannelResponse {
    /// The channel the change was about.
    pub channel: ChannelInfo,
    /// Every channel, so the page can redraw its pickers from one answer.
    pub channels: Vec<ChannelInfo>,
    /// Configured channels now off the list, so Settings can redraw what it offers to show again
    /// from the same answer. `#199 removable-config-channels`.
    pub hidden_channels: Vec<ChannelInfo>,
}

/// The answer every change to the channel list gives: the channel it was about, and the list as
/// it now stands both ways.
///
/// The channel is taken from those lists when it is in one, so it wears the owner's alias exactly
/// as the pickers will; a channel that has left both, an added one just removed, is answered as
/// given.
async fn channel_list_answer(state: &AppState, channel: ChannelInfo) -> Response {
    let channels = ops::channels(state).await;
    let hidden_channels = ops::hidden_channels(state).await;
    let channel = channels
        .iter()
        .chain(&hidden_channels)
        .find(|listed| listed.id == channel.id)
        .cloned()
        .unwrap_or(channel);
    no_store(Json(AddChannelResponse {
        channel,
        channels,
        hidden_channels,
    }))
}

/// Put a hidden configured channel back on the list, store first and then memory.
///
/// The store first for the reason [`add_channel`] writes it first: a store that refused would
/// otherwise leave the channel showing until the next restart and then silently gone again.
async fn unhide_configured_channel(state: &AppState, id: &ChannelId) -> Result<(), ApiError> {
    state.store.unhide_channel(id).await?;
    if let Ok(mut hidden) = state.hidden_channels.write() {
        hidden.remove(id);
    }
    state.refresh_routes();
    tracing::info!(channel = %id, "configured channel shown again in the app");
    Ok(())
}

/// The configured channel `id` when the owner has hidden it, so that asking to ADD it can put it
/// back instead of refusing a channel he can no longer see. `#199 removable-config-channels`.
fn hidden_configured(state: &AppState, id: &str) -> Option<ChannelInfo> {
    state
        .configured_channel(id)
        .filter(|channel| state.channel(channel.id.as_str()).is_none())
}

/// `POST /api/v1/channels` — add a channel without editing a file or redeploying.
///
/// WRITE SCOPE, AND NO MCP TOOL. Adding a channel widens what this bridge can read, and if
/// `writable` is set, what it can say. That is the operator's decision and nobody else's — least of
/// all the voice agent's, which would otherwise be able to grant itself a new place to post in the
/// middle of a conversation. The same reasoning that keeps `set_alias` out of the tool manifest
/// applies here with more force, so this route is reachable from the app and from nothing else.
///
/// THE CHANNEL IS PROBED BEFORE IT IS ACCEPTED. Adding the bot to a Discord SERVER is not the same
/// as adding it to a private CHANNEL, and a snowflake with a typo in it looks identical to a
/// correct one. So this reads a message before agreeing, and hands back the probe's own words when
/// it cannot — the same sentences the startup check uses, rather than a second opinion about the
/// same failure.
///
/// ADDING A CONFIGURED CHANNEL THE OWNER HID SHOWS IT AGAIN. `#199 removable-config-channels`. To
/// him it is not on the list, so the directory offers it and a pasted id names it; refusing it as
/// "already configured" would leave no way back through the act he reached for.
pub async fn add_channel(
    State(state): State<AppState>,
    _scope: WriteScope,
    Json(request): Json<AddChannelRequest>,
) -> Result<Response, ApiError> {
    let label = request.label.trim();
    if label.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_channel_label",
            "give the channel a name so it can be told apart in the picker",
        ));
    }

    let managed = state.chat.supports_channel_registration();
    // One transaction spans an upstream registration plus the local probe/store update. The two
    // systems cannot commit atomically, so serialize the whole managed lifecycle and make rollback
    // compensation unable to race another add or removal.
    let _managed_lifecycle = if managed {
        Some(state.channel_registration_lock.lock().await)
    } else {
        None
    };
    let registered = if managed {
        if request.id.present || request.writable.present {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_channel_registration",
                "this provider manages channel registration; send a source and label, without an id or writable field",
            ));
        }
        let source = request
            .source
            .value
            .as_deref()
            .map(str::trim)
            .filter(|source| !source.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_channel_source",
                    "give a channel link or provider reference",
                )
            })?;
        // A configured channel the owner hid is not "already a channel" to him: the directory
        // offers it as one he can add, and adding it shows it again. Nothing is registered
        // upstream for it, because the file, not this route, is what owns it.
        if let Some(configured) = hidden_configured(&state, source) {
            unhide_configured_channel(&state, &configured.id).await?;
            return Ok(channel_list_answer(&state, configured).await);
        }
        let source_is_static = state
            .config
            .channels
            .iter()
            .any(|channel| channel.id.as_str() == source);
        let source_is_direct = state.added_channels.read().ok().is_some_and(|added| {
            added.iter().any(|channel| {
                channel.channel.as_str() == source && channel.registration_provider.is_none()
            })
        });
        if source_is_static || source_is_direct {
            let (code, detail) = if source_is_static {
                (
                    "channel_is_configured",
                    "that source is already a channel owned by the static configuration",
                )
            } else {
                (
                    "channel_already_present",
                    "that source is already a directly managed local channel",
                )
            };
            return Err(ApiError::new(StatusCode::CONFLICT, code, detail));
        }
        Some(state.chat.register_channel(source, label).await?)
    } else {
        if request.source.present {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "channel_registration_unsupported",
                format!("{} requires a channel id through this chat adapter; channel links and provider references are not supported", state.chat.provider_name()),
            ));
        }
        None
    };

    let id = match registered.as_ref() {
        Some(channel) => channel.id.as_str(),
        None => request
            .id
            .value
            .as_deref()
            .map(str::trim)
            .unwrap_or_default(),
    };
    // Which provider owns the channel, and the namespace that records it across a restart. A
    // managed registration was bound to its provider by the router; a direct id belongs to the
    // deployment's default provider.
    let provider_key = match registered.as_ref() {
        Some(channel) => state.providers.key_for(&channel.id),
        None => state.config.default_provider_key().map(str::to_owned),
    };
    let registration_provider = if managed {
        state.registration_namespace(provider_key.as_deref())
    } else {
        None
    };
    let registration_provider = registration_provider.as_deref();
    if id.is_empty() || (!managed && !id.chars().all(|c| c.is_ascii_digit())) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_channel_id",
            format!(
                "{} requires a numeric channel id through this chat adapter. Ask the deployment operator for the channel ID.",
                state.chat.provider_name()
            ),
        ));
    }
    if let Some(configured) = hidden_configured(&state, id) {
        // The same as a hidden source above: a direct id, or a link that resolved to one. The
        // file's label and write policy stand, not the ones in this request, because the file is
        // what owns the channel. A registration the bridge just answered is left alone for the
        // reason given below: the id is owned outside this transaction, so nothing here may
        // delete it upstream. A direct add holds no lifecycle lock yet, and takes it here so that
        // showing the channel cannot interleave with a concurrent hide.
        let _hide_lifecycle = if managed {
            None
        } else {
            Some(state.channel_registration_lock.lock().await)
        };
        unhide_configured_channel(&state, &configured.id).await?;
        return Ok(channel_list_answer(&state, configured).await);
    }
    if state
        .config
        .channels
        .iter()
        .any(|channel| channel.id.as_str() == id)
    {
        // A colliding id is already owned outside this registration transaction. Never issue a
        // compensating DELETE for it, even if the bridge incorrectly claims it was just created.
        let (code, detail) = if managed {
            (
                "channel_is_configured",
                "this source resolves to a channel owned by the static configuration; managed registration cannot replace it",
            )
        } else {
            (
                "channel_already_present",
                "that channel is already in the list",
            )
        };
        return Err(ApiError::new(StatusCode::CONFLICT, code, detail));
    }
    let existing = state.added_channels.read().ok().and_then(|added| {
        added
            .iter()
            .find(|channel| channel.channel.as_str() == id)
            .cloned()
    });
    if existing
        .as_ref()
        .is_some_and(|channel| channel.registration_provider.as_deref() != registration_provider)
    {
        // As above, the colliding row belongs to another lifecycle. Do not let the new bridge's
        // `created` claim authorize deletion of that pre-existing channel id.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "channel_already_present",
            "that channel is already in the list under a different registration policy",
        ));
    }

    let candidate = ChannelInfo {
        id: crate::model::ChannelId(id.to_owned()),
        label: label.to_owned(),
        writable: registered.as_ref().map_or_else(
            || request.writable.value.unwrap_or(false),
            |channel| channel.writable,
        ),
        alias: None,
        added: true,
        provider: provider_key.clone(),
    };
    let rollback_if_failed = if existing.is_none() {
        registered.as_ref()
    } else {
        None
    };
    // The same read the startup check makes, against the one channel being added.
    let report =
        crate::probe::probe_channels(state.chat.as_ref(), std::slice::from_ref(&candidate)).await;
    let Some(outcome) = report.outcomes.first() else {
        rollback_registration(&state, rollback_if_failed).await;
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "channel_unprobed",
            "the channel could not be checked",
        ));
    };
    if !matches!(outcome.diagnosis, crate::probe::Diagnosis::Readable { .. }) {
        rollback_registration(&state, rollback_if_failed).await;
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "channel_unreadable",
            // The PROBE'S own remedy, not a sentence written here. One explanation of "the bot
            // cannot see this channel", used by the startup check and by this route alike.
            outcome.diagnosis.remedy_for(state.chat.provider_name()),
        ));
    }

    let added_at_ms = existing.as_ref().map_or_else(
        || jiff::Timestamp::now().as_millisecond(),
        |channel| channel.added_at_ms,
    );
    if let Err(error) = state
        .store
        .add_channel(
            &candidate.id,
            &candidate.label,
            candidate.writable,
            registration_provider,
            added_at_ms,
        )
        .await
    {
        rollback_registration(&state, rollback_if_failed).await;
        return Err(error.into());
    }
    // The store first, then the live list. Written the other way round, a store that refused would
    // leave a channel reachable until the next restart and then silently gone.
    let stored = crate::store::AddedChannel {
        channel: candidate.id.clone(),
        label: candidate.label.clone(),
        writable: candidate.writable,
        registration_provider: registration_provider.map(str::to_owned),
        added_at_ms,
    };
    if let Ok(mut added) = state.added_channels.write() {
        if let Some(current) = added
            .iter_mut()
            .find(|channel| channel.channel == stored.channel)
        {
            *current = stored;
        } else {
            added.push(stored);
        }
    }
    state.refresh_routes();
    tracing::info!(channel = %candidate.id, writable = candidate.writable, managed, provider = provider_key.as_deref().unwrap_or_default(), "channel added or reconciled in the app");
    Ok(channel_list_answer(&state, candidate).await)
}

async fn rollback_registration(
    state: &AppState,
    registered: Option<&crate::chat::RegisteredChannel>,
) {
    if let Some(registered) = registered.filter(|registration| registration.created) {
        if let Err(error) = state.chat.unregister_channel(&registered.id).await {
            tracing::warn!(channel = %registered.id, %error, "could not roll back channel registration");
        }
    }
    // A registration binds its channel to a provider before the probe can read it. Re-deriving
    // the routes from the allowlist drops that binding when the channel did not make it in.
    state.refresh_routes();
}

/// `DELETE /api/v1/channels/{channel_id}` — take a channel off the list.
///
/// TWO KINDS OF CHANNEL, TWO MEANINGS, ONE VERB FOR THE OWNER. `#199 removable-config-channels`.
/// One added in the app is forgotten: its row goes, and a managed registration is withdrawn
/// upstream. One from the configuration file is HIDDEN: this server never writes that file, so
/// deleting the channel would last until the next restart and then undo itself — the reason this
/// route used to refuse. Recording the hide in the store is what makes it last instead; the file
/// still says the channel exists, and `DELETE .../hidden` or adding it again brings it back.
///
/// Either way it stops being reachable, not only listed, because [`AppState::channel`] stops
/// answering for it. Hiding a configured channel touches nothing upstream. Without a store the
/// hide is refused by name rather than held in memory until the next restart, which would be the
/// same silent undo the old refusal existed to prevent.
///
/// Either way its cached summaries and thread sentences are deleted too, and its thread names
/// kept (`#208 persistent-cache`): a summary is a second at-rest copy of the channel's text, and
/// the channel is no longer on the list it was read under. A failure there is logged, not
/// returned: the channel is already off the list, and the age limit still collects them.
///
/// WRITE scope, and NO MCP TOOL, for the reason [`set_alias`] has none: the list is the owner's.
pub async fn remove_channel(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let _managed_lifecycle = state.channel_registration_lock.lock().await;
    if let Some(configured) = state.configured_channel(&channel_id) {
        // Idempotent: hiding one already hidden keeps the first instant and answers the same list,
        // so a retry after a lost answer is not an error.
        state
            .store
            .hide_channel(&configured.id, crate::store::now_ms())
            .await?;
        if let Ok(mut hidden) = state.hidden_channels.write() {
            hidden.insert(configured.id.clone());
        }
        state.refresh_routes();
        let summaries_deleted = forget_channel_summaries(&state, &configured.id).await;
        tracing::info!(
            channel = %channel_id,
            summaries_deleted,
            "configured channel hidden in the app"
        );
        return Ok(channel_list_answer(&state, configured).await);
    }
    let added_channel = state.added_channels.read().ok().and_then(|added| {
        added
            .iter()
            .find(|channel| channel.channel.as_str() == channel_id)
            .cloned()
    });
    let Some(added_channel) = added_channel else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_channel",
            "that channel is not in the added-channel list",
        ));
    };
    if let Some(provider) = added_channel.registration_provider.as_deref() {
        if !state.providers.knows_namespace(provider) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "channel_provider_mismatch",
                "that channel belongs to a different provider registration; restore its original bridge configuration before removing it",
            ));
        }
        // The bridge contract makes DELETE idempotent and answers 204 even when an earlier attempt
        // already removed the upstream registration. This ordering deliberately permits retry when
        // the upstream delete succeeds but the following local store write fails.
        state
            .chat
            .unregister_channel(&added_channel.channel)
            .await?;
    }
    state
        .store
        .remove_added_channel(&added_channel.channel)
        .await?;
    if let Ok(mut added) = state.added_channels.write() {
        added.retain(|channel| channel.channel != added_channel.channel);
    }
    state.refresh_routes();
    let summaries_deleted = forget_channel_summaries(&state, &added_channel.channel).await;
    tracing::info!(channel = %channel_id, summaries_deleted, "channel removed in the app");
    Ok(channel_list_answer(
        &state,
        ChannelInfo {
            id: crate::model::ChannelId(channel_id),
            label: String::new(),
            writable: false,
            alias: None,
            added: true,
            provider: None,
        },
    )
    .await)
}

/// Delete a channel's cached summaries and thread sentences, keeping its thread names, and answer
/// how many went: the count is all that is logged. `#208 persistent-cache`.
///
/// The sentences this server remembers go too, so a channel shown again in the same process is not
/// shown from memory what was just deleted from the store.
async fn forget_channel_summaries(state: &AppState, channel: &crate::model::ChannelId) -> u64 {
    state.thread_names.forget_channel_sentences(channel);
    match state.store.forget_channel_summaries(channel).await {
        Ok(deleted) => deleted,
        Err(crate::store::StoreError::Unavailable(_)) => 0,
        Err(error) => {
            tracing::warn!(%error, "a channel taken off the list kept its cached summaries");
            0
        }
    }
}

/// `DELETE /api/v1/channels/{channel_id}/hidden` — show a hidden configured channel again.
/// `#199 removable-config-channels`.
///
/// The one channel route that addresses a channel OUTSIDE the allowlist, and only ever one the
/// configuration file names: anything else is `unknown_channel`, so this cannot admit a channel
/// the operator never listed. Showing one that is not hidden answers the list as it stands.
///
/// WRITE scope, and NO MCP TOOL, the same as taking it off.
pub async fn show_channel(
    State(state): State<AppState>,
    _scope: WriteScope,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let _managed_lifecycle = state.channel_registration_lock.lock().await;
    let Some(configured) = state.configured_channel(&channel_id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_channel",
            "only a channel named in this server's configuration file can be hidden or shown again",
        ));
    };
    unhide_configured_channel(&state, &configured.id).await?;
    Ok(channel_list_answer(&state, configured).await)
}
