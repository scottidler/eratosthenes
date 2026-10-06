# Implementation Notes: Per-Thread Error Isolation

## Phase 0: Prove the wiremock seam
### Design decisions
- Harness lives in `tests/common/mod.rs` (`client_for`, `failed_precondition`, `rate_limited`, `thread_body`, `thread_get_requests`) so later phases reuse it; proof tests in `tests/wiremock_seam.rs`. Integration tests drive only the public API, so the seam is proven without touching production code.
- Hub is built with the same `hyper_rustls::HttpsConnector<HttpConnector>` type as production but `https_or_http()` so it can speak plain http to the mock; `Hub::base_url` and `root_url` both point at `server.uri()`. Auth is a `String` token (`GetToken` is implemented for `String`).
- `labels.list` is mounted inside `client_for` because `GmailClient::new` calls it.
- Clock is paused with `tokio::time::pause()` after client construction, not `start_paused`, so construction does real I/O and only the backoff ladder runs on virtual time.

### Deviations
- None. (`tokio::time::pause()` mid-test instead of `#[tokio::test(start_paused = true)]`: same effect, avoids virtual time during real socket I/O at setup.)

### Tradeoffs
- Integration test crate with its own connector vs a `#[cfg(test)]` constructor in production code: the former keeps Phase 0 at zero production code, as specified; `init_tls` is already `pub`.
- The design said "throwaway test"; kept as permanent tests because later phases depend on the seam and it costs 0.1s.

### Open questions
- None.

## Phase 1: Retry hygiene
### Design decisions
- `with_retry` keeps the last retryable error and returns `Err(last_err.wrap_err(RetryExhausted { op, attempts }))` - `src/gmail/rate.rs:with_retry` - the cause stays in the chain, so `{:#}` shows it and `downcast_ref::<RetryExhausted>()` finds the marker.
- `RetryExhausted` Display is `{op} exhausted {attempts} attempts` - `src/gmail/rate.rs` - the old "failed after {} retries" wording is gone, as the success criterion's `rg` requires.
- The impossible no-attempts case (`MAX_RETRIES == 0`) bails with its own message rather than `unwrap`/`unreachable!` - fail loudly without a panic.
- Inverted the old `test_with_retry_times_out_a_hanging_call_and_retries_it` assertion that pinned `"after 5 retries"`; it now asserts `RetryExhausted` plus the timeout cause surviving.
- New tests: DNS-error exhaustion (cause + marker + attempt count) and permanent error is not marked exhausted. Bite verified: restoring the `eyre::bail!` made the DNS test and the inverted timeout test fail.

### Deviations
- Backoff log reads `[retry] backing off {n}s after failed attempt {k}` instead of the doc's `before attempt {k}`: `backoff` also sleeps after the final failed attempt, where no next attempt exists, so "before attempt 6" would be false. Same intent (drop "Rate limited").
- DNS test is a unit test in rate.rs with a `google_gmail1::Error::Io` carrying a "dns error" message, not wiremock: a real DNS failure cannot be produced through the mock server.

### Tradeoffs
- Unit test with a synthetic retryable error vs the wiremock harness: wiremock cannot emit transport-level DNS errors; the unit test hits exactly the code under change.

### Open questions
- None.
