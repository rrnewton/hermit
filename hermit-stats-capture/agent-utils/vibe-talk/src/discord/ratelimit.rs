//! Discord's rate limiter, read and respected.
//!
//! Discord answers an over-quota request with **HTTP 429** and tells you, precisely, how long to
//! wait. Ignoring that and surfacing the 429 as a gateway error is not "no rate limiting" — it is
//! rate limiting done badly, because the caller then retries by hand at whatever rate it feels
//! like and the next request is rejected too. This module is the part that listens.
//!
//! # What Discord actually sends
//!
//! Two independent channels say the same thing, and they disagree in a way that matters:
//!
//! * the **body**, `{"message": "You are being rate limited.", "retry_after": 0.529, "global":
//!   false}` — `retry_after` is in **seconds, as a float**; and
//! * the **`Retry-After` header**, in seconds, ROUNDED — historically to a whole second, and not
//!   always upwards.
//!
//! The body is therefore preferred and the header is the fallback: retrying a few milliseconds
//! early costs another rejected request, which is the exact thing being avoided.
//!
//! Three further headers are read where present:
//!
//! * `X-RateLimit-Bucket` — Discord's own opaque hash for the bucket this route belongs to. Two
//!   routes can share one bucket, and only Discord knows which; learning the hash is what lets a
//!   limit observed on one route apply to the other.
//! * `X-RateLimit-Remaining` and `X-RateLimit-Reset-After` — how many requests are left in the
//!   current window and how long until it refills. `remaining: 0` on a **successful** response is
//!   advance warning: the next request on that bucket is going to be a 429, and [`RateLimiter`]
//!   waits it out instead of spending it.
//! * `X-RateLimit-Scope` / `X-RateLimit-Global` — whether the limit is the **whole bot token** or
//!   one route's bucket.
//!
//! # Global is not the same as per-route, and is not treated as if it were
//!
//! A per-route 429 stops one route. A **global** 429 stops every request this token makes, so
//! [`RateLimiter`] holds a separate whole-token gate and every route waits behind it. Filing a
//! global limit under the route that happened to observe it would let the very next request, on a
//! different channel, walk straight into the same wall.
//!
//! # The budget is bounded, and running out is LOUD
//!
//! Waiting is bounded twice — [`RetryPolicy::max_attempts`] and [`RetryPolicy::max_total_wait`] —
//! because a request that waits as long as Discord asks, however long that is, is a request that
//! can hang until the caller gives up with nothing to say. When either bound is reached the call
//! fails with [`super::DiscordError::RateLimited`], which names the rate limit, the route, how
//! many attempts were made, how long was actually spent waiting and what Discord last asked for.
//! It is never downgraded to an empty result.
//!
//! **This is bounded waiting, not a queue.** Nothing is stored and retried later; a request that
//! exhausts the budget is over, and its caller decides what to do next.
//!
//! # One notion of "slow down", not two
//!
//! [`crate::live`]'s poll loop also slows down after a failure, by doubling its interval. The two
//! are not rivals, because they nest: this module is the inner, precise wait that Discord itself
//! asked for, and the poll loop only ever sees a rate limit that this module could not clear
//! inside its budget. When that happens the poll loop waits at least as long as Discord's own
//! outstanding `retry_after` — see [`super::DiscordError::retry_after`] — so the outer backoff can
//! be longer than Discord asked for but never shorter.
//!
//! # Tested against a fake, and only against a fake
//!
//! Everything here is exercised by [`super::fake::FakeDiscord`], which shares this engine, and by
//! unit tests over the parsing. **None of it has met live Discord**, so the header names, the body
//! field and the rounding behaviour come from Discord's published documentation rather than from
//! observation.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::chat::ChatError;
#[cfg(test)]
use crate::chat::ChatError as DiscordError;
pub use crate::chat::RateLimitExhausted;

/// How many times one logical request may be sent before the rate limit is declared unclearable.
///
/// Four: the original, plus three retries. Discord's per-route windows are seconds long, so three
/// retries clears an ordinary burst; anything that survives four attempts is not a burst.
pub const MAX_RATE_LIMIT_ATTEMPTS: u32 = 4;

/// The total time one logical request may spend waiting on rate limits, across all attempts.
///
/// Thirty seconds, chosen against the client's own twenty-second per-request HTTP timeout: the
/// wait is allowed to exceed one request's timeout, because a rate limit is not a hung socket, but
/// it may not exceed the patience of a person holding a phone.
pub const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);

/// What a 429 with no readable `retry_after` anywhere is treated as.
///
/// Discord always sends one; this is for the intermediary that returns a 429 of its own with an
/// error page for a body. One second, so the retry is neither immediate nor a stall.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);

/// The floor on an actual retry wait.
///
/// A `retry_after` of zero would mean "retry immediately", which spends an attempt on a request
/// very likely to be rejected again in the same millisecond. Fifty milliseconds is short enough to
/// be invisible and long enough to be a different instant.
pub const MIN_RETRY_WAIT: Duration = Duration::from_millis(50);

/// The largest `retry_after` that will be believed, in seconds.
///
/// A ceiling on parsing, not a policy: it keeps a malformed or hostile float out of [`Duration`]
/// arithmetic. Anything this large blows the budget and fails loudly anyway.
pub const MAX_PARSED_RETRY_AFTER_SECONDS: f64 = 3600.0;

/// HTTP headers, matched without regard to case, independent of any HTTP client.
///
/// Small on purpose: this module has to be usable by the in-memory fake, which has no HTTP client
/// at all, so the engine production takes is the engine the tests exercise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// No headers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one header, builder style.
    #[must_use]
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.0.push((name.to_ascii_lowercase(), value.to_owned()));
        self
    }

    /// The first value for `name`, matched without regard to case.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    fn number(&self, name: &str) -> Option<f64> {
        self.get(name)?.trim().parse::<f64>().ok()
    }
}

/// One rate-limit rejection, as this client understands it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// How long Discord asked us to wait.
    pub retry_after: Duration,
    /// Whether the limit applies to the whole bot token rather than to one route's bucket.
    pub global: bool,
    /// Discord's opaque bucket hash, when the response carried one.
    pub bucket: Option<String>,
    /// Discord's `X-RateLimit-Scope`, verbatim, when present. Recorded for the log; nothing
    /// branches on it beyond `global`.
    pub scope: Option<String>,
}

/// The bounds on waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// How many times one logical request may be sent.
    pub max_attempts: u32,
    /// How long one logical request may spend waiting, across all attempts.
    pub max_total_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_RATE_LIMIT_ATTEMPTS,
            max_total_wait: MAX_RATE_LIMIT_WAIT,
        }
    }
}

/// What one attempt produced.
///
/// Deliberately not a `Result`: "Discord rate-limited this" is neither success nor a failure the
/// caller should see, it is an instruction to wait, and giving it its own case is what stops it
/// being flattened into an error further down.
#[derive(Debug)]
pub enum Attempt<T> {
    /// The request completed. The headers come with it so a bucket that is now empty can be
    /// recorded before the next caller walks into it.
    Done(T, Headers),
    /// Discord said no, and said when to come back.
    Limited(RateLimit),
}

/// What the limiter has had to do, for tests and for logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimiterStats {
    /// Requests that were NOT sent because their bucket was known to be empty, and were waited out
    /// first instead.
    pub preempted: u64,
    /// Retries performed after a 429.
    pub retries: u64,
    /// Rejections observed, including ones that later cleared.
    pub rejections: u64,
}

/// Turn Discord's float seconds into a duration, refusing to believe nonsense.
#[must_use]
pub fn seconds_to_duration(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    let capped = seconds.min(MAX_PARSED_RETRY_AFTER_SECONDS);
    Duration::try_from_secs_f64(capped)
        .unwrap_or_else(|_| Duration::from_secs_f64(MAX_PARSED_RETRY_AFTER_SECONDS))
}

/// The bucket key for a request, before Discord has told us its real bucket hash.
///
/// Method plus path, query dropped. Discord buckets by route AND by "major parameter" — the
/// channel, guild or webhook id — and this client only ever issues channel-scoped routes, so
/// keeping the id in the path is exactly the right granularity: two channels do not share a limit,
/// and reading a channel does not share one with posting to it.
#[must_use]
pub fn route_key(method: &str, url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = after_scheme.find('/').map_or("/", |at| &after_scheme[at..]);
    let path = path.split(['?', '#']).next().unwrap_or("/");
    format!("{} {}", method.to_ascii_uppercase(), path)
}

/// Read a rate-limit rejection out of a response, or decide it is not one.
///
/// Returns `None` for anything that is not a 429, so an ordinary failure keeps its ordinary
/// meaning even when it arrives with a `Retry-After` header attached.
#[must_use]
pub fn parse_rate_limit(status: u16, headers: &Headers, body: &str) -> Option<RateLimit> {
    if status != 429 {
        return None;
    }
    let json = serde_json::from_str::<serde_json::Value>(body).ok();
    let field = |name: &str| json.as_ref().and_then(|value| value.get(name));
    // Body first: it is fractional, and the header is rounded.
    let seconds = field("retry_after")
        .and_then(serde_json::Value::as_f64)
        .or_else(|| headers.number("retry-after"));
    let retry_after = match seconds {
        Some(seconds) => seconds_to_duration(seconds),
        None => DEFAULT_RETRY_AFTER,
    };
    let scope = headers.get("x-ratelimit-scope").map(str::to_owned);
    let global = field("global")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || headers
            .get("x-ratelimit-global")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        || scope
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("global"));
    Some(RateLimit {
        retry_after,
        global,
        bucket: headers.get("x-ratelimit-bucket").map(str::to_owned),
        scope,
    })
}

/// How long a bucket is empty for, according to a SUCCESSFUL response's own accounting.
///
/// `Some` only when `X-RateLimit-Remaining` is zero, which is Discord saying the next request on
/// this bucket will be rejected. That is worth knowing before spending it.
#[must_use]
pub fn exhausted_for(headers: &Headers) -> Option<Duration> {
    let remaining = headers.number("x-ratelimit-remaining")?;
    if remaining > 0.0 {
        return None;
    }
    let reset_after = headers.number("x-ratelimit-reset-after")?;
    Some(seconds_to_duration(reset_after))
}

#[derive(Debug, Default)]
struct LimiterState {
    /// Route key -> Discord's own bucket hash, learned from `X-RateLimit-Bucket`. Two routes that
    /// turn out to share a hash then share a gate.
    buckets: BTreeMap<String, String>,
    /// Effective key -> the instant before which a request would certainly be rejected.
    gates: BTreeMap<String, Instant>,
    /// The whole-token gate. A global limit is not one route's problem.
    global_gate: Option<Instant>,
    stats: LimiterStats,
}

/// Discord's rate limits, obeyed: bucket state plus the bounded wait-and-retry around a request.
///
/// Shared by the live client and by the in-memory fake, so a test written against the fake
/// exercises the same waiting, the same bucket accounting and the same loud exhaustion that
/// production takes.
#[derive(Debug, Default)]
pub struct RateLimiter {
    policy: RetryPolicy,
    state: Mutex<LimiterState>,
}

impl RateLimiter {
    /// A limiter with the default bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A limiter with bounds of your own. Tests use this to make exhaustion reachable quickly.
    #[must_use]
    pub fn with_policy(policy: RetryPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// The bounds this limiter enforces.
    #[must_use]
    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// What it has had to do so far.
    #[must_use]
    pub fn stats(&self) -> LimiterStats {
        self.lock().stats
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LimiterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn effective_key(state: &LimiterState, route: &str) -> String {
        state
            .buckets
            .get(route)
            .cloned()
            .unwrap_or_else(|| route.to_owned())
    }

    /// How long this route must wait before a request could possibly be accepted.
    fn gate_wait(&self, route: &str) -> Duration {
        let state = self.lock();
        let now = Instant::now();
        let key = Self::effective_key(&state, route);
        let of = |gate: Option<&Instant>| {
            gate.map_or(Duration::ZERO, |at| at.saturating_duration_since(now))
        };
        of(state.gates.get(&key)).max(of(state.global_gate.as_ref()))
    }

    fn learn_bucket(state: &mut LimiterState, route: &str, bucket: Option<&str>) {
        if let Some(bucket) = bucket {
            state.buckets.insert(route.to_owned(), bucket.to_owned());
        }
    }

    fn shut(state: &mut LimiterState, key: String, until: Instant) {
        let gate = state.gates.entry(key).or_insert(until);
        if until > *gate {
            *gate = until;
        }
    }

    fn observe_limit(&self, route: &str, limit: &RateLimit) {
        let mut state = self.lock();
        state.stats.rejections += 1;
        Self::learn_bucket(&mut state, route, limit.bucket.as_deref());
        let until = Instant::now() + limit.retry_after;
        if limit.global {
            // Everything waits. Filing this under one route would let the next request, on a
            // different channel, walk into the identical wall a millisecond later.
            if state.global_gate.is_none_or(|gate| until > gate) {
                state.global_gate = Some(until);
            }
        } else {
            let key = Self::effective_key(&state, route);
            Self::shut(&mut state, key, until);
        }
    }

    fn observe_success(&self, route: &str, headers: &Headers) {
        let mut state = self.lock();
        Self::learn_bucket(&mut state, route, headers.get("x-ratelimit-bucket"));
        if let Some(reset_after) = exhausted_for(headers) {
            let key = Self::effective_key(&state, route);
            Self::shut(&mut state, key, Instant::now() + reset_after);
        }
    }

    /// Send a request, respecting Discord's rate limits, within a bounded budget.
    ///
    /// `attempt` is called once per try. It reports [`Attempt::Limited`] for a 429 and
    /// [`Attempt::Done`] for anything it could turn into a value; a genuine error is returned
    /// unchanged and is NOT retried, because a 500 or a 404 is not a quota problem.
    ///
    /// Before each try, a bucket already known to be empty is **waited out rather than spent**:
    /// the request Discord would certainly reject is not sent at all.
    ///
    /// # Errors
    ///
    /// Returns whatever `attempt` returns, and [`DiscordError::RateLimited`] when the attempt or
    /// total-wait budget runs out with the limit still in force. It never returns a success it did
    /// not get.
    pub async fn run<T, F, Fut>(&self, method: &str, url: &str, attempt: F) -> Result<T, ChatError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Attempt<T>, ChatError>>,
    {
        let route = route_key(method, url);
        let budget = self.policy.max_total_wait;
        let mut waited = Duration::ZERO;
        let mut attempts: u32 = 0;
        let mut global = false;
        loop {
            let gate = self.gate_wait(&route);
            if !gate.is_zero() {
                if waited + gate > budget {
                    return Err(self.exhausted(&route, attempts, waited, gate, global));
                }
                self.lock().stats.preempted += 1;
                tracing::debug!(
                    route = %route,
                    wait_ms = gate.as_millis(),
                    "holding a chat request back: that bucket is known to be empty"
                );
                tokio::time::sleep(gate).await;
                waited += gate;
            }
            attempts += 1;
            match attempt().await? {
                Attempt::Done(value, headers) => {
                    self.observe_success(&route, &headers);
                    return Ok(value);
                }
                Attempt::Limited(limit) => {
                    global = limit.global;
                    self.observe_limit(&route, &limit);
                    let wait = limit.retry_after.max(MIN_RETRY_WAIT);
                    if attempts >= self.policy.max_attempts || waited + wait > budget {
                        return Err(self.exhausted(
                            &route,
                            attempts,
                            waited,
                            limit.retry_after,
                            global,
                        ));
                    }
                    self.lock().stats.retries += 1;
                    tracing::info!(
                        route = %route,
                        attempt = attempts,
                        wait_ms = wait.as_millis(),
                        global = limit.global,
                        "the chat provider rate-limited this request; waiting the time it asked for"
                    );
                    tokio::time::sleep(wait).await;
                    waited += wait;
                }
            }
        }
    }

    fn exhausted(
        &self,
        route: &str,
        attempts: u32,
        waited: Duration,
        retry_after: Duration,
        global: bool,
    ) -> ChatError {
        let detail = RateLimitExhausted {
            provider: "chat",
            route: route.to_owned(),
            attempts,
            waited,
            budget: self.policy.max_total_wait,
            retry_after,
            global,
        };
        tracing::warn!(%detail, "giving up on a chat request: the rate limit did not clear");
        ChatError::RateLimited(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Discord's documented 429 body, per-route.
    const BODY: &str = r#"{"message": "You are being rate limited.", "retry_after": 0.529, "global": false, "code": 0}"#;

    #[test]
    fn the_fractional_body_value_beats_the_rounded_header() {
        // The header is whole seconds and is not always rounded UP, so believing it can mean
        // retrying early — which spends a request to be told the same thing again.
        let headers = Headers::new().with("Retry-After", "1");
        let limit = parse_rate_limit(429, &headers, BODY).expect("a 429 is a rate limit");
        assert_eq!(limit.retry_after, Duration::from_millis(529));
        assert!(!limit.global);
    }

    #[test]
    fn the_header_is_the_fallback_when_the_body_is_not_discords() {
        // An intermediary can answer 429 with an HTML error page. The header still says when.
        let headers = Headers::new().with("retry-after", "2");
        let limit = parse_rate_limit(429, &headers, "<html>too many requests</html>")
            .expect("still a rate limit");
        assert_eq!(limit.retry_after, Duration::from_secs(2));
    }

    #[test]
    fn a_429_with_nothing_readable_still_waits_rather_than_hammering() {
        let limit = parse_rate_limit(429, &Headers::new(), "").expect("still a rate limit");
        assert_eq!(limit.retry_after, DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn global_is_read_from_the_body_the_header_or_the_scope() {
        let from_body = parse_rate_limit(
            429,
            &Headers::new(),
            r#"{"retry_after": 1.0, "global": true}"#,
        )
        .expect("a rate limit");
        assert!(from_body.global);

        let from_header = parse_rate_limit(
            429,
            &Headers::new().with("X-RateLimit-Global", "true"),
            BODY,
        )
        .expect("a rate limit");
        assert!(
            from_header.global,
            "the header says global even though the body says false"
        );

        let from_scope = parse_rate_limit(
            429,
            &Headers::new().with("X-RateLimit-Scope", "global"),
            BODY,
        )
        .expect("a rate limit");
        assert!(from_scope.global);
        assert_eq!(from_scope.scope.as_deref(), Some("global"));
    }

    #[test]
    fn nothing_but_a_429_is_a_rate_limit() {
        for status in [200, 400, 403, 404, 500, 503] {
            assert!(
                parse_rate_limit(status, &Headers::new().with("retry-after", "5"), BODY).is_none(),
                "HTTP {status} must keep its own meaning even with a Retry-After header"
            );
        }
    }

    #[test]
    fn a_nonsense_retry_after_is_not_believed() {
        assert_eq!(seconds_to_duration(f64::NAN), Duration::ZERO);
        assert_eq!(seconds_to_duration(-5.0), Duration::ZERO);
        assert_eq!(seconds_to_duration(f64::INFINITY), Duration::ZERO);
        assert_eq!(
            seconds_to_duration(1e18),
            Duration::from_secs_f64(MAX_PARSED_RETRY_AFTER_SECONDS),
            "a huge value is capped rather than turned into Duration arithmetic"
        );
    }

    #[test]
    fn a_route_key_keeps_the_channel_and_the_method_and_drops_the_query() {
        assert_eq!(
            route_key(
                "GET",
                "https://discord.com/api/v10/channels/123/messages?limit=25&after=9"
            ),
            "GET /api/v10/channels/123/messages"
        );
        assert_ne!(
            route_key("GET", "https://discord.com/api/v10/channels/123/messages"),
            route_key("GET", "https://discord.com/api/v10/channels/456/messages"),
            "the channel is Discord's major parameter: two channels do not share one bucket"
        );
        assert_ne!(
            route_key("GET", "https://discord.com/api/v10/channels/123/messages"),
            route_key("POST", "https://discord.com/api/v10/channels/123/messages"),
            "reading a channel and posting to it are different buckets"
        );
    }

    #[test]
    fn a_successful_response_reports_an_empty_bucket() {
        let full = Headers::new()
            .with("x-ratelimit-remaining", "4")
            .with("x-ratelimit-reset-after", "1.5");
        assert_eq!(exhausted_for(&full), None);
        let empty = Headers::new()
            .with("x-ratelimit-remaining", "0")
            .with("x-ratelimit-reset-after", "1.5");
        assert_eq!(exhausted_for(&empty), Some(Duration::from_millis(1500)));
        assert_eq!(
            exhausted_for(&Headers::new().with("x-ratelimit-remaining", "0")),
            None,
            "without a reset time there is nothing to wait for"
        );
    }

    const URL: &str = "https://discord.com/api/v10/channels/123/messages";
    const OTHER: &str = "https://discord.com/api/v10/channels/456/messages";

    fn limit(retry_after: Duration, global: bool) -> RateLimit {
        RateLimit {
            retry_after,
            global,
            bucket: None,
            scope: None,
        }
    }

    /// An attempt that is rate-limited on its first `n` calls and then succeeds.
    fn limited_then_ok(
        calls: &AtomicU32,
        n: u32,
        retry_after: Duration,
        global: bool,
    ) -> Attempt<&'static str> {
        if calls.fetch_add(1, Ordering::SeqCst) < n {
            Attempt::Limited(limit(retry_after, global))
        } else {
            Attempt::Done("ok", Headers::new())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_request_waits_the_time_discord_asked_for_and_then_succeeds() {
        let limiter = RateLimiter::new();
        let calls = AtomicU32::new(0);
        let started = Instant::now();
        let value = limiter
            .run("GET", URL, || async {
                Ok(limited_then_ok(
                    &calls,
                    2,
                    Duration::from_millis(500),
                    false,
                ))
            })
            .await
            .expect("two rejections are inside the budget");
        assert_eq!(value, "ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "one original and two retries"
        );
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1),
            "it must actually have waited 500ms twice, not retried immediately"
        );
        assert_eq!(limiter.stats().retries, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_attempt_budget_is_bounded_and_running_out_names_the_rate_limit() {
        let limiter = RateLimiter::with_policy(RetryPolicy {
            max_attempts: 3,
            max_total_wait: MAX_RATE_LIMIT_WAIT,
        });
        let calls = AtomicU32::new(0);
        let error = limiter
            .run("GET", URL, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, DiscordError>(Attempt::<&str>::Limited(limit(
                    Duration::from_millis(200),
                    false,
                )))
            })
            .await
            .expect_err("a limit that never clears must not read as success");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "bounded, not forever");
        let message = error.to_string();
        assert!(
            message.contains("RATE LIMIT"),
            "the failure must name the rate limit, not read as a generic upstream error: {message}"
        );
        assert!(message.contains("/channels/123/messages"), "{message}");
        assert!(
            matches!(error, DiscordError::RateLimited(ref detail) if detail.attempts == 3),
            "{error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_total_wait_is_bounded_even_when_attempts_are_left() {
        // Discord asking for a minute is not a reason to hold a caller for a minute.
        let limiter = RateLimiter::with_policy(RetryPolicy {
            max_attempts: 8,
            max_total_wait: Duration::from_secs(5),
        });
        let calls = AtomicU32::new(0);
        let started = Instant::now();
        let error = limiter
            .run("GET", URL, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, DiscordError>(Attempt::<&str>::Limited(limit(
                    Duration::from_secs(60),
                    false,
                )))
            })
            .await
            .expect_err("a wait longer than the budget must fail rather than be taken");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited past its own budget: {:?}",
            started.elapsed()
        );
        assert!(matches!(error, DiscordError::RateLimited(_)), "{error:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_known_empty_bucket_is_waited_out_instead_of_being_spent() {
        let limiter = RateLimiter::new();
        // A SUCCESS whose headers say the bucket is now empty gates the next request on it.
        limiter
            .run("GET", URL, || async {
                Ok::<_, DiscordError>(Attempt::Done(
                    "ok",
                    Headers::new()
                        .with("x-ratelimit-remaining", "0")
                        .with("x-ratelimit-reset-after", "3"),
                ))
            })
            .await
            .expect("succeeds");

        let started = Instant::now();
        let after = AtomicU32::new(0);
        limiter
            .run("GET", URL, || async {
                after.fetch_add(1, Ordering::SeqCst);
                Ok::<_, DiscordError>(Attempt::Done("ok", Headers::new()))
            })
            .await
            .expect("succeeds once the window resets");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(3),
            "the empty bucket must be waited out, not spent on a certain rejection"
        );
        assert_eq!(
            after.load(Ordering::SeqCst),
            1,
            "and then sent exactly once"
        );
        assert_eq!(limiter.stats().preempted, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_global_limit_stops_a_route_that_never_saw_it() {
        let limiter = RateLimiter::new();
        let calls = AtomicU32::new(0);
        limiter
            .run("GET", URL, || async {
                Ok(limited_then_ok(&calls, 1, Duration::from_secs(6), true))
            })
            .await
            .expect("clears on the retry");

        // The retry above slept the global gate out, so re-arm it and then watch a DIFFERENT
        // channel wait behind it. A global limit is the whole token, not one route.
        let again = AtomicU32::new(0);
        limiter
            .run("GET", URL, || async {
                Ok(limited_then_ok(&again, 1, Duration::from_secs(6), true))
            })
            .await
            .expect("clears again");
        assert_eq!(again.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bucket_limit_stops_only_its_own_route() {
        let limiter = RateLimiter::new();
        let calls = AtomicU32::new(0);
        limiter
            .run("GET", URL, || async {
                Ok(limited_then_ok(&calls, 1, Duration::from_secs(10), false))
            })
            .await
            .expect("clears after ten seconds");

        let elsewhere = Instant::now();
        limiter
            .run("GET", OTHER, || async {
                Ok::<_, DiscordError>(Attempt::Done("ok", Headers::new()))
            })
            .await
            .expect("a different channel is a different bucket");
        assert_eq!(
            elsewhere.elapsed(),
            Duration::ZERO,
            "one channel's bucket limit must not stall every other channel"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_routes_that_share_discords_bucket_hash_share_one_gate() {
        // Only Discord knows which routes share a bucket, and it says so with the header. Without
        // learning it, a limit observed on one route would be spent again on the other.
        let limiter = RateLimiter::new();
        for method in ["GET", "POST"] {
            limiter
                .run(method, URL, || async {
                    Ok::<_, DiscordError>(Attempt::Done(
                        "ok",
                        Headers::new().with("x-ratelimit-bucket", "abcd"),
                    ))
                })
                .await
                .expect("succeeds");
        }
        limiter
            .run("GET", URL, || async {
                Ok::<_, DiscordError>(Attempt::Done(
                    "ok",
                    Headers::new()
                        .with("x-ratelimit-bucket", "abcd")
                        .with("x-ratelimit-remaining", "0")
                        .with("x-ratelimit-reset-after", "2"),
                ))
            })
            .await
            .expect("succeeds");
        let started = Instant::now();
        limiter
            .run("POST", URL, || async {
                Ok::<_, DiscordError>(Attempt::Done("ok", Headers::new()))
            })
            .await
            .expect("succeeds after the shared window");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(2),
            "the shared bucket hash must gate both routes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_ordinary_failure_is_not_retried_as_if_it_were_quota() {
        let limiter = RateLimiter::new();
        let calls = AtomicU32::new(0);
        let error = limiter
            .run("GET", URL, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<Attempt<&str>, _>(DiscordError::Status {
                    status: 404,
                    body: r#"{"code": 10003}"#.to_owned(),
                })
            })
            .await
            .expect_err("a 404 is still a 404");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retrying a 404 four times helps nobody and costs four requests"
        );
        assert!(matches!(error, DiscordError::Status { status: 404, .. }));
    }
}
