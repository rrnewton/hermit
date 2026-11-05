//! A client-side ceiling on calls to a model provider: a token bucket with a waiting line.
//! `#233 llm-summarizer`.
//!
//! # The problem
//!
//! A thread list can ask for dozens of thread names at once, and a page full of long messages can
//! ask for a dozen summaries at once. The provider's own rate limit is the operator's to spend,
//! shared with whatever else uses the same key, and a 429 storm from this server would spend it on
//! retries. So calls are paced HERE, before they leave: at most `burst` back to back, then one per
//! `60 / requests_per_minute` seconds.
//!
//! # A line, served in order
//!
//! A caller over the limit WAITS its turn rather than being refused, in a first-come first-served
//! line: only the caller at the head of the line watches the bucket, and the next one moves up
//! when it has been served (or has given up). Each caller brings a deadline. One whose turn, as
//! the line stands when it joins, is past that deadline is turned away at once and told how long
//! it would have waited; one that joins and then runs out of patience leaves the line without
//! taking anything, so a refused caller costs the others nothing.
//!
//! # A 429 is obeyed by everyone, and never spends more than the limit
//!
//! When the provider answers 429 with `Retry-After`, [`RateLimiter::pause_for`] stops EVERY caller
//! until then. A pause only ever TAKES allowance away: the bucket keeps at most one call (the retry
//! the provider invited, when the bucket still had it), and nothing accrues while the provider is
//! refusing us. So a provider answering 429 to everything — even with `Retry-After: 0` — is still
//! called no more often than `burst` plus the steady pace allows.
//!
//! # Interactive work goes first
//!
//! Thread names are produced in the background; a message summary is produced while somebody
//! waits for it. They queue in separate lines, and [`Priority::Background`] takes a token only
//! when nobody is waiting in the interactive line AND one more token would still be left for an
//! interactive caller (when the burst allows a spare at all), so a backlog of thread names never
//! makes a reader wait behind it.
//!
//! Time is `tokio::time`, so a test with a paused clock can assert exactly when each call went.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Who is asking, which decides which line it waits in and whether a token must be left over
/// for somebody else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Somebody is waiting for this answer.
    Interactive,
    /// Nobody is waiting: a thread name refreshed in the background.
    Background,
}

impl Priority {
    fn line(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Background => 1,
        }
    }
}

/// The pace, the burst before it applies, and the two lines of callers waiting for it.
#[derive(Debug)]
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    bucket: Mutex<Bucket>,
    /// One per [`Priority`]. Holding a line's lock is being at its head; `tokio`'s mutex hands
    /// itself over in the order it was asked for, which is what makes the line first-come
    /// first-served.
    lines: [tokio::sync::Mutex<()>; 2],
    /// How many callers are in each line, the head included.
    waiting: [AtomicUsize; 2],
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    /// When `tokens` was last brought up to date. Never earlier than the end of a pause, so no
    /// token accrues during one.
    updated: Instant,
    paused_until: Option<Instant>,
}

/// A place in a line, given up however the caller leaves it: served, refused or dropped.
struct Place<'a>(&'a AtomicUsize);

impl Drop for Place<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RateLimiter {
    /// A limiter allowing `burst` calls at once and `per_minute` a minute after that. Both are
    /// clamped to at least one: a limiter that never admits anything is a configuration error,
    /// refused at load, not a state this type has to represent.
    #[must_use]
    pub fn new(per_minute: u32, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Self {
            per_second: f64::from(per_minute.max(1)) / 60.0,
            burst,
            bucket: Mutex::new(Bucket {
                tokens: burst,
                updated: Instant::now(),
                paused_until: None,
            }),
            lines: [tokio::sync::Mutex::new(()), tokio::sync::Mutex::new(())],
            waiting: [AtomicUsize::new(0), AtomicUsize::new(0)],
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait for a turn, for at most `max_wait`.
    ///
    /// # Errors
    ///
    /// The wait that WOULD have been needed, when it is longer than `max_wait`. Nothing is
    /// reserved in that case, so a refused caller costs the others nothing.
    pub async fn acquire(&self, priority: Priority, max_wait: Duration) -> Result<(), Duration> {
        let joined = Instant::now();
        let deadline = joined + max_wait;
        let line = priority.line();
        let ahead = self.waiting[line].fetch_add(1, Ordering::SeqCst);
        let _place = Place(&self.waiting[line]);
        // Everybody ahead is served first, so this caller needs the bucket to hold one call for
        // each of them as well as its own. Said now, not at the deadline, when the answer is no.
        let estimate = {
            let mut bucket = self.lock();
            self.refill(&mut bucket, joined);
            self.wait_for(&bucket, joined, priority, ahead)
        };
        if estimate > max_wait {
            return Err(estimate);
        }
        let Ok(_head) = tokio::time::timeout_at(deadline, self.lines[line].lock()).await else {
            // The line moved slower than it looked: callers ahead were held up by a pause.
            return Err(estimate
                .saturating_sub(joined.elapsed())
                .max(Duration::from_secs(1)));
        };
        loop {
            let wait = {
                let mut bucket = self.lock();
                let now = Instant::now();
                self.refill(&mut bucket, now);
                let wait = self.wait_for(&bucket, now, priority, 0);
                if wait.is_zero() {
                    bucket.tokens -= 1.0;
                    return Ok(());
                }
                wait
            };
            if Instant::now() + wait > deadline {
                return Err(wait);
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// How long until the bucket can serve a caller of `priority` with `ahead` callers in its own
    /// line in front of it. Zero means now.
    fn wait_for(
        &self,
        bucket: &Bucket,
        now: Instant,
        priority: Priority,
        ahead: usize,
    ) -> Duration {
        let needed = match priority {
            Priority::Interactive => ahead as f64 + 1.0,
            // Behind everybody in the interactive line, and leaving a spare for the next one.
            Priority::Background => {
                let interactive = self.waiting[Priority::Interactive.line()].load(Ordering::SeqCst);
                let spare = if self.burst >= 2.0 { 1.0 } else { 0.0 };
                ahead as f64 + 1.0 + interactive as f64 + spare
            }
        };
        let paused = bucket
            .paused_until
            .filter(|until| *until > now)
            .map_or(Duration::ZERO, |until| until - now);
        // Nothing accrues during a pause (`updated` is its end), so the refill starts after it.
        let deficit = (needed - bucket.tokens).max(0.0);
        // Rounded UP to the millisecond, so a waiter never wakes a hair early, finds the bucket a
        // fraction short and has to sleep again for nothing.
        let refill = Duration::from_millis((deficit / self.per_second * 1000.0).ceil() as u64);
        paused + refill
    }

    /// Stop every caller for `wait`, and start again at the steady pace rather than with a burst.
    ///
    /// What a 429's `Retry-After` turns into. A pause already longer than this one is kept.
    ///
    /// A pause NEVER adds allowance. The bucket keeps at most ONE call for the moment the pause
    /// ends — the retry the provider invited, if the bucket still had a call to give — and refill
    /// is counted from the END of the pause: tokens that "accrued" while the provider was refusing
    /// us are not tokens we may spend. A provider that answers every call with 429 therefore
    /// paces this server exactly as the configured limit does, and no faster, whatever its
    /// `Retry-After` says, zero included.
    pub fn pause_for(&self, wait: Duration) {
        let mut bucket = self.lock();
        let now = Instant::now();
        self.refill(&mut bucket, now);
        let until = bucket
            .paused_until
            .map_or(now + wait, |held| held.max(now + wait));
        bucket.paused_until = Some(until);
        bucket.tokens = bucket.tokens.min(1.0);
        bucket.updated = bucket.updated.max(until);
    }

    fn refill(&self, bucket: &mut Bucket, now: Instant) {
        if now > bucket.updated {
            let elapsed = (now - bucket.updated).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
            bucket.updated = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Every pair of grants (i, j) must satisfy: grants in [t_i, t_j] <= burst + rate * (t_j - t_i).
    /// That is the definition of "never exceeds the limit", stated without trusting the
    /// implementation's own arithmetic.
    fn assert_within_limit(grants: &[Duration], per_minute: u32, burst: u32) {
        let rate = f64::from(per_minute) / 60.0;
        for i in 0..grants.len() {
            for j in i..grants.len() {
                let window = (grants[j] - grants[i]).as_secs_f64();
                let allowed = f64::from(burst) + rate * window + 1e-6;
                let count = (j - i + 1) as f64;
                assert!(
                    count <= allowed,
                    "{count} calls went out in {window}s, more than the {allowed} the limit allows: \
                     {grants:?}"
                );
            }
        }
    }

    /// Let every runnable task run, so tasks spawned one after another join the line in order.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_callers_is_queued_and_never_exceeds_the_limit() {
        let limiter = Arc::new(RateLimiter::new(15, 3));
        let start = Instant::now();
        let grants = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let limiter = Arc::clone(&limiter);
            let grants = Arc::clone(&grants);
            tasks.push(tokio::spawn(async move {
                limiter
                    .acquire(Priority::Interactive, Duration::from_secs(3600))
                    .await
                    .expect("a long deadline admits everyone eventually");
                grants.lock().unwrap().push(start.elapsed());
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut grants = grants.lock().unwrap().clone();
        grants.sort();
        assert_eq!(grants.len(), 12, "every caller got its turn");
        assert_within_limit(&grants, 15, 3);
        // The control: the burst really went out at once, and the queue really did drain at the
        // configured pace rather than at some slower one that would pass the check above trivially.
        assert!(grants[2] < Duration::from_millis(10), "{grants:?}");
        assert!(
            grants[11] <= Duration::from_secs(37),
            "nine calls past the burst at 4s apiece should take about 36s: {grants:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_callers_are_served_in_the_order_they_arrived() {
        let limiter = Arc::new(RateLimiter::new(60, 1));
        limiter
            .acquire(Priority::Interactive, Duration::ZERO)
            .await
            .expect("the first is free");
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..6 {
            let limiter = Arc::clone(&limiter);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                limiter
                    .acquire(Priority::Interactive, Duration::from_secs(3600))
                    .await
                    .expect("admitted");
                order.lock().unwrap().push(n);
            }));
            settle().await;
            // Arrivals spread over time, so a later one would be due sooner if each computed its
            // own wait and raced for the token.
            tokio::time::advance(Duration::from_millis(150)).await;
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_whose_turn_is_past_its_deadline_is_told_how_long_it_would_have_waited() {
        let limiter = RateLimiter::new(6, 1);
        limiter
            .acquire(Priority::Interactive, Duration::ZERO)
            .await
            .expect("the first is free");
        let refused = limiter
            .acquire(Priority::Interactive, Duration::from_secs(1))
            .await
            .expect_err("the next slot is ten seconds away");
        assert!(refused >= Duration::from_secs(9), "{refused:?}");
        // ...and the refusal reserved nothing: the next caller with patience goes at ten seconds.
        let start = Instant::now();
        limiter
            .acquire(Priority::Interactive, Duration::from_secs(60))
            .await
            .expect("admitted");
        assert!(
            start.elapsed() <= Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_is_judged_by_its_place_in_the_line_and_refused_at_once() {
        let limiter = Arc::new(RateLimiter::new(60, 1));
        limiter
            .acquire(Priority::Interactive, Duration::ZERO)
            .await
            .expect("the first is free");
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let limiter = Arc::clone(&limiter);
            tasks.push(tokio::spawn(async move {
                limiter
                    .acquire(Priority::Interactive, Duration::from_secs(3600))
                    .await
                    .expect("admitted");
            }));
            settle().await;
        }
        // Three ahead at one a second: this caller's turn is about four seconds away, and it will
        // wait two. It is told so now, rather than after two seconds of waiting for nothing.
        let start = Instant::now();
        let refused = limiter
            .acquire(Priority::Interactive, Duration::from_secs(2))
            .await
            .expect_err("its turn is past its deadline");
        assert_eq!(start.elapsed(), Duration::ZERO, "refused at once");
        assert!(refused >= Duration::from_secs(3), "{refused:?}");
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_pause_holds_everyone_and_resumes_at_the_steady_pace() {
        let limiter = RateLimiter::new(60, 5);
        let start = Instant::now();
        limiter.pause_for(Duration::from_secs(30));
        limiter
            .acquire(Priority::Interactive, Duration::from_secs(120))
            .await
            .expect("admitted after the pause");
        let first = start.elapsed();
        assert!(
            first >= Duration::from_secs(30),
            "the pause was not obeyed: {first:?}"
        );
        assert!(
            first < Duration::from_millis(30_100),
            "the retry the provider invited should go the moment the pause ends: {first:?}"
        );
        limiter
            .acquire(Priority::Interactive, Duration::from_secs(120))
            .await
            .expect("admitted");
        assert!(
            start.elapsed() - first >= Duration::from_millis(900),
            "the pause ended in a burst rather than at the steady pace"
        );
    }

    /// A provider that answers EVERY call with 429 and `retry_after`, against `callers` callers
    /// each retrying as the summariser does. Returns when each call went out.
    async fn storm(
        per_minute: u32,
        burst: u32,
        retry_after: Duration,
        callers: usize,
    ) -> Vec<Duration> {
        let limiter = Arc::new(RateLimiter::new(per_minute, burst));
        let start = Instant::now();
        let grants = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for _ in 0..callers {
            let limiter = Arc::clone(&limiter);
            let grants = Arc::clone(&grants);
            tasks.push(tokio::spawn(async move {
                for _ in 0..10 {
                    limiter
                        .acquire(Priority::Interactive, Duration::from_secs(3600))
                        .await
                        .expect("a long deadline admits everyone eventually");
                    grants.lock().unwrap().push(start.elapsed());
                    // ...and the answer is a 429.
                    limiter.pause_for(retry_after);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut grants = grants.lock().unwrap().clone();
        grants.sort();
        grants
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_storm_never_spends_more_than_the_limit_whatever_retry_after_says() {
        // The regression: a pause used to REFILL the bucket to one call, so every 429 bought the
        // next call, and `Retry-After: 1` became one call a second against a limit of one every
        // four, while `Retry-After: 0` took the pacing away entirely.
        for (retry_after, callers) in [
            (Duration::from_secs(1), 1),
            (Duration::ZERO, 1),
            (Duration::from_secs(1), 4),
            (Duration::ZERO, 4),
        ] {
            let grants = storm(15, 1, retry_after, callers).await;
            assert_eq!(grants.len(), 10 * callers);
            assert_within_limit(&grants, 15, 1);
        }
        // With a burst to spend, too.
        let grants = storm(15, 3, Duration::ZERO, 3).await;
        assert_within_limit(&grants, 15, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn background_work_leaves_a_token_for_somebody_who_is_waiting() {
        let limiter = RateLimiter::new(60, 2);
        limiter
            .acquire(Priority::Background, Duration::ZERO)
            .await
            .expect("two tokens: background may take one");
        limiter
            .acquire(Priority::Background, Duration::ZERO)
            .await
            .expect_err("one token left: it is kept for an interactive caller");
        limiter
            .acquire(Priority::Interactive, Duration::ZERO)
            .await
            .expect("the kept token is there for an interactive caller");
    }

    #[tokio::test(start_paused = true)]
    async fn background_work_waits_behind_every_interactive_caller() {
        let limiter = Arc::new(RateLimiter::new(60, 1));
        limiter
            .acquire(Priority::Interactive, Duration::ZERO)
            .await
            .expect("the first is free");
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (who, priority) in [
            ("background", Priority::Background),
            ("interactive-1", Priority::Interactive),
            ("interactive-2", Priority::Interactive),
        ] {
            let limiter = Arc::clone(&limiter);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                limiter
                    .acquire(priority, Duration::from_secs(3600))
                    .await
                    .expect("admitted");
                order.lock().unwrap().push(who);
            }));
            settle().await;
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            vec!["interactive-1", "interactive-2", "background"],
            "a thread name that arrived first still waits for the summaries somebody is reading"
        );
    }
}
