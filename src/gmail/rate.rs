use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::time::{Duration, Instant, sleep, timeout};

const MAX_TOKENS: u32 = 15000;
const REFILL_PER_SEC: u32 = 250;
const MAX_BACKOFF_SECS: u64 = 60;

pub struct RateLimiter {
    tokens: AtomicU32,
    last_refill: Mutex<Instant>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            tokens: AtomicU32::new(MAX_TOKENS),
            last_refill: Mutex::new(Instant::now()),
        }
    }

    fn refill(&self) {
        let mut last = self.last_refill.lock().expect("lock poisoned");
        let elapsed = last.elapsed();
        let new_tokens = (elapsed.as_secs_f64() * REFILL_PER_SEC as f64) as u32;
        if new_tokens > 0 {
            let current = self.tokens.load(Ordering::Relaxed);
            let refilled = (current + new_tokens).min(MAX_TOKENS);
            self.tokens.store(refilled, Ordering::Relaxed);
            *last = Instant::now();
        }
    }

    pub async fn acquire(&self, cost: u32) {
        loop {
            self.refill();
            let current = self.tokens.load(Ordering::Relaxed);
            if current >= cost {
                self.tokens.fetch_sub(cost, Ordering::Relaxed);
                return;
            }
            let deficit = cost - current;
            let wait_ms = (deficit as f64 / REFILL_PER_SEC as f64 * 1000.0) as u64;
            sleep(Duration::from_millis(wait_ms.max(10))).await;
        }
    }

    pub async fn backoff(&self, attempt: u32) {
        let wait = backoff_secs(attempt);
        log::warn!(
            "Rate limited, backing off for {}s (attempt {})",
            wait,
            attempt
        );
        sleep(Duration::from_secs(wait)).await;
    }
}

/// The backoff wait for one attempt, extracted as a pure function so the sleep
/// and the worst-case arithmetic below cannot disagree about it.
///
/// The clamp is applied LAST. It used to clamp `base` and then add the spread
/// on top, which returns 76s at attempt 6 for a `MAX_BACKOFF_SECS` that says
/// 60: unreachable at `MAX_RETRIES = 5`, but a trap for whoever raises it.
///
/// The spread is deterministic, and the name says so. It is a fixed 25% of the
/// exponential step, NOT jitter: there is no randomness here, so several
/// accounts retrying in parallel align their attempts perfectly rather than
/// spreading out. That is acceptable only because runs are sequential per
/// process; adding real jitter needs an RNG dependency and is a separate
/// decision, so the name is honest instead of aspirational.
fn backoff_secs(attempt: u32) -> u64 {
    let base = 1u64 << attempt.min(6);
    let spread = base / 4;
    base.saturating_add(spread).min(MAX_BACKOFF_SECS)
}

const MAX_RETRIES: u32 = 5;

/// Per-request ceiling on ONE Gmail API call.
///
/// The transport under this had no bound of any kind, which meant a Gmail call
/// could not fail, it could only hang: a black-holed connection during
/// `threads.list` or `drafts.create` wedged the run forever, and the retry
/// machinery below never saw an error to classify. Generous on purpose -- it is
/// a hang detector, not a latency target, and the largest call here fetches one
/// full thread.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Planted in a timeout's error text and matched by `is_retryable`, which
/// classifies by substring over the whole context chain. A named const keeps
/// the producer and the matcher from drifting apart; two bare literals in two
/// files would not.
pub const TIMEOUT_MARKER: &str = "transport timeout";

/// HTTP statuses worth another attempt: Gmail's rate limit plus the transient
/// server-side family. Anything else is a permanent error, and retrying it now
/// costs five `REQUEST_TIMEOUT`s plus the whole backoff ladder.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Reasons Google puts in a JSON error body for a condition that will clear on
/// its own. Matched exactly, against the `reason` field, never as loose text.
const RETRYABLE_REASONS: &[&str] = &[
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "backendError",
    "internalError",
];

/// Google's structured error body: `error.code` is the status, `error.status`
/// is the canonical code, and `error.errors[].reason` is the machine-readable
/// reason. All three are read from their own fields rather than from the
/// rendered text.
fn json_error_is_retryable(body: &serde_json::Value) -> bool {
    let error = &body["error"];

    if let Some(code) = error["code"].as_u64()
        && is_retryable_status(code as u16)
    {
        return true;
    }
    if error["status"].as_str() == Some("RESOURCE_EXHAUSTED") {
        return true;
    }
    if let Some(entries) = error["errors"].as_array() {
        return entries.iter().any(|entry| {
            entry["reason"]
                .as_str()
                .is_some_and(|reason| RETRYABLE_REASONS.contains(&reason))
        });
    }
    false
}

/// Classify an error as a transient Gmail rate/availability failure worth
/// retrying.
///
/// Classified STRUCTURALLY, off the typed `google_gmail1::Error` in the source
/// chain. It used to be a bare substring sweep over the whole rendered chain,
/// which is unsound in a way that bit: `create_label_if_missing` interpolates
/// the LABEL NAME into its context, and a bare `contains("rate")` matches
/// "Corporate", "Separate", "moderate", "generate", "accurate". A permanent 400
/// on any such label was classified as retryable. That was survivable while a
/// retry was cheap; bounding the transport made every false positive cost five
/// 30s attempts plus 38s of ladder, so the classifier had to get precise.
///
/// A transport timeout stays a text match, because it is the one error this
/// module GENERATES rather than receives -- see `TIMEOUT_MARKER`. It is
/// retryable: it is precisely the transient class this function exists for.
/// Note that retrying multiplies the worst-case wall clock by `MAX_RETRIES`,
/// which is why a unit-level bound has to be derived from
/// `worst_case_call_duration()` and not from `REQUEST_TIMEOUT` alone.
pub fn is_retryable(report: &eyre::Report) -> bool {
    if format!("{report:#}").contains(TIMEOUT_MARKER) {
        return true;
    }

    for source in report.chain() {
        if let Some(err) = source.downcast_ref::<google_gmail1::Error>() {
            return match err {
                google_gmail1::Error::Failure(response) => {
                    is_retryable_status(response.status().as_u16())
                }
                google_gmail1::Error::BadRequest(body) => json_error_is_retryable(body),
                // A dropped connection or a truncated stream is transient in
                // exactly the way a retry is for.
                google_gmail1::Error::HttpError(_) | google_gmail1::Error::Io(_) => true,
                _ => false,
            };
        }
    }
    false
}

/// The real ceiling on one `with_retry` call: every attempt timing out, plus
/// every backoff between them. This is the number a `TimeoutStartSec` must be
/// derived from -- `REQUEST_TIMEOUT` alone understates it by the retry factor,
/// and sizing a unit bound below this would SIGKILL a run that was about to
/// succeed on its last attempt.
pub fn worst_case_call_duration() -> Duration {
    let attempts = REQUEST_TIMEOUT * MAX_RETRIES;
    let backoffs: u64 = (0..MAX_RETRIES).map(backoff_secs).sum();
    attempts + Duration::from_secs(backoffs)
}

pub async fn with_retry<F, Fut, T>(
    limiter: &RateLimiter,
    op_name: &str,
    mut f: F,
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<T>>,
{
    for attempt in 0..MAX_RETRIES {
        // The timeout is what puts the TRANSPORT inside this retry loop. An
        // unbounded call yields no error, so `is_retryable` never runs and the
        // backoff below never fires: the run just stops, forever. Turning
        // elapsed time into a marked error is the whole point.
        //
        // The limiter's `acquire` sits inside `f` and so inside this bound. It
        // is a local token bucket sized 15000 at 250/s against costs of at most
        // 50, so its wait is sub-second next to `REQUEST_TIMEOUT`.
        let outcome = match timeout(REQUEST_TIMEOUT, f()).await {
            Ok(result) => result,
            Err(_) => Err(eyre::eyre!(
                "{}: {} returned nothing within {}s",
                TIMEOUT_MARKER,
                op_name,
                REQUEST_TIMEOUT.as_secs()
            )),
        };
        match outcome {
            Ok(val) => return Ok(val),
            Err(e) => {
                if is_retryable(&e) {
                    log::warn!(
                        "[retry] {} failed (attempt {}): {:#}",
                        op_name,
                        attempt + 1,
                        e
                    );
                    limiter.backoff(attempt).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    eyre::bail!("{} failed after {} retries", op_name, MAX_RETRIES)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acquire_within_budget() {
        let limiter = RateLimiter::new();
        limiter.acquire(10).await;
        let remaining = limiter.tokens.load(Ordering::Relaxed);
        assert!(remaining < MAX_TOKENS);
    }

    #[tokio::test]
    async fn test_acquire_multiple() {
        let limiter = RateLimiter::new();
        limiter.acquire(100).await;
        limiter.acquire(100).await;
        let remaining = limiter.tokens.load(Ordering::Relaxed);
        assert!(remaining <= MAX_TOKENS - 200);
    }

    /// The typed error lives in the SOURCE; the top context is generic. This is
    /// the case a `to_string()` check would miss, and it is now answered by
    /// walking the chain to the real `google_gmail1::Error` rather than by
    /// pattern-matching rendered text.
    #[test]
    fn test_is_retryable_finds_the_rate_limit_in_the_typed_source() {
        use eyre::Context;
        let source = google_gmail1::Error::BadRequest(serde_json::json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "errors": [{ "reason": "rateLimitExceeded" }]
            }
        }));
        let report = Err::<(), _>(source)
            .context("threads.get(abc123) failed")
            .unwrap_err();

        assert!(!report.to_string().contains("429"));
        assert!(is_retryable(&report));
    }

    /// Audit MF2, the regression that motivated going structural. A bare
    /// `contains("rate")` over the rendered chain matched the LABEL NAME that
    /// `create_label_if_missing` interpolates into its context, so a permanent
    /// 400 on a label called "Corporate" was retried five times at up to 30s
    /// each. The bait is asserted present so this test cannot pass by accident.
    #[test]
    fn test_is_retryable_ignores_a_label_name_that_merely_contains_rate() {
        use eyre::Context;
        for label in [
            "Corporate",
            "Separate",
            "moderate",
            "generate",
            "accurate",
            "Corporate/rate-cards",
        ] {
            let source = google_gmail1::Error::BadRequest(serde_json::json!({
                "error": { "code": 400, "message": "Invalid label name" }
            }));
            let report = Err::<(), _>(source)
                .context(format!("Failed to create label '{}'", label))
                .unwrap_err();

            assert!(
                format!("{report:#}").contains("rate"),
                "the substring bait must be present for '{}' or this test proves nothing",
                label
            );
            assert!(
                !is_retryable(&report),
                "a permanent 400 must not retry because '{}' contains 'rate'",
                label
            );
        }
    }

    /// The transient server-side family retries; a permanent client error does
    /// not. Driven off the typed `Failure` status, not off text.
    #[test]
    fn test_is_retryable_splits_transient_from_permanent_statuses() {
        for status in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(status), "{} should retry", status);
        }
        for status in [400u16, 401, 403, 404, 409, 412, 422] {
            assert!(!is_retryable_status(status), "{} must not retry", status);
        }
    }

    /// A duplicate-label 409 is NOT retryable: retrying cannot help, because
    /// the label already exists. `create_label_if_missing` handles it by
    /// re-checking existence, not by retrying (audit MF1).
    #[test]
    fn test_is_retryable_does_not_retry_a_duplicate() {
        use eyre::Context;
        let source = google_gmail1::Error::BadRequest(serde_json::json!({
            "error": {
                "code": 409,
                "errors": [{ "reason": "duplicate" }]
            }
        }));
        let report = Err::<(), _>(source)
            .context("Failed to create label 'llm/needs-reply'")
            .unwrap_err();
        assert!(!is_retryable(&report));
    }

    #[test]
    fn test_is_retryable_ignores_non_transient_errors() {
        use eyre::Context;
        let source = eyre::eyre!("thread missing id");
        let report = Err::<(), _>(source)
            .context("threads.get(abc123) failed")
            .unwrap_err();
        assert!(!is_retryable(&report));
    }

    /// An untyped error carries no status to classify, so it must not retry.
    /// This is what keeps the structural matcher from silently falling back to
    /// the old text sweep.
    #[test]
    fn test_is_retryable_ignores_an_untyped_error_mentioning_a_status() {
        use eyre::Context;
        let source = eyre::eyre!("server said 503 once upon a time");
        let report = Err::<(), _>(source).context("threads.list").unwrap_err();
        assert!(!is_retryable(&report));
    }

    /// The marker has to survive being wrapped in context, because that is how
    /// `with_retry`'s caller will have wrapped it by the time anyone reads it.
    #[test]
    fn test_is_retryable_classifies_a_transport_timeout_through_context() {
        use eyre::Context;
        let source = eyre::eyre!(
            "{}: threads.list returned nothing within 30s",
            TIMEOUT_MARKER
        );
        let report = Err::<(), _>(source)
            .context("listing candidate threads failed")
            .unwrap_err();
        assert!(!report.to_string().contains(TIMEOUT_MARKER));
        assert!(is_retryable(&report));
    }

    /// A hang is the failure mode an unbounded transport actually has, and the
    /// one the old code could not see. Virtual time, so this costs no wall
    /// clock while still exercising the REAL `REQUEST_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn test_with_retry_times_out_a_hanging_call_and_retries_it() {
        use std::sync::atomic::AtomicU32;

        let limiter = RateLimiter::new();
        let attempts = AtomicU32::new(0);

        let result: eyre::Result<()> = with_retry(&limiter, "threads.list", || {
            attempts.fetch_add(1, Ordering::Relaxed);
            async {
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .await;

        let err = result.expect_err("a call that never answers must not succeed");
        assert!(format!("{err:#}").contains("after 5 retries"), "{err:#}");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            MAX_RETRIES,
            "every attempt should have been made"
        );
    }

    /// A call that answers is not slowed down by the bound.
    #[tokio::test(start_paused = true)]
    async fn test_with_retry_passes_through_a_call_that_answers() {
        let limiter = RateLimiter::new();
        let value = with_retry(&limiter, "threads.get", || async { Ok(7u32) })
            .await
            .unwrap();
        assert_eq!(value, 7);
    }

    /// A NON-retryable error still returns immediately: adding the timeout must
    /// not have turned every failure into five attempts.
    #[tokio::test(start_paused = true)]
    async fn test_with_retry_does_not_retry_a_permanent_error() {
        use std::sync::atomic::AtomicU32;

        let limiter = RateLimiter::new();
        let attempts = AtomicU32::new(0);

        let result: eyre::Result<()> = with_retry(&limiter, "threads.get", || {
            attempts.fetch_add(1, Ordering::Relaxed);
            async { Err(eyre::eyre!("thread missing id")) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    /// `backoff_secs` is the shared derivation; if it drifts from what
    /// `backoff` sleeps, `worst_case_call_duration` silently lies.
    #[test]
    fn test_backoff_secs_is_the_documented_ladder() {
        let ladder: Vec<u64> = (0..MAX_RETRIES).map(backoff_secs).collect();
        assert_eq!(ladder, vec![1, 2, 5, 10, 20]);
    }

    /// The clamp is applied last, so `MAX_BACKOFF_SECS` is a real ceiling
    /// rather than a number the function exceeds. Unreachable at the current
    /// `MAX_RETRIES`, asserted so raising it cannot reintroduce the trap.
    #[test]
    fn test_backoff_secs_never_exceeds_its_stated_ceiling() {
        for attempt in 0..32 {
            assert!(
                backoff_secs(attempt) <= MAX_BACKOFF_SECS,
                "attempt {} waits {}s, over the {}s ceiling",
                attempt,
                backoff_secs(attempt),
                MAX_BACKOFF_SECS
            );
        }
    }

    /// The number a unit-level `TimeoutStartSec` must be derived from. Pinned
    /// so that changing `REQUEST_TIMEOUT` or `MAX_RETRIES` forces a look at
    /// whatever was sized against it.
    #[test]
    fn test_worst_case_call_duration_includes_the_retry_factor() {
        let expected = REQUEST_TIMEOUT * MAX_RETRIES + Duration::from_secs(1 + 2 + 5 + 10 + 20);
        assert_eq!(worst_case_call_duration(), expected);
        assert_eq!(worst_case_call_duration(), Duration::from_secs(188));
        assert!(
            worst_case_call_duration() > REQUEST_TIMEOUT,
            "a bound sized from REQUEST_TIMEOUT alone would kill a run about to succeed"
        );
    }
}
