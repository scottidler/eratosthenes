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

## Phase 2: Classifier, state-filter boundary, ceiling
### Design decisions
- `ErrorScope` + `error_scope` in `src/gmail/rate.rs`, beside `is_retryable`: `RetryExhausted` via `downcast_ref` first (-> `Account`), then the first `google_gmail1::Error` in the chain; `Thread` only for `BadRequest` bodies with `code 400 + status FAILED_PRECONDITION` or `code 404 + some errors[].reason notFound`. Everything else, untyped errors included, is `Account`.
- Skip accounting is `SkipLedger` in a new `src/skip.rs` (crate-private): one `HashSet<String>` plus the ceiling, created at the top of `engine::execute` before the `--mark-only` early return. `skip_thread(id, op, err, prefix)` classifies; `Account` returns the error unchanged, `Thread` logs WARN `skipping thread {id}: {op} failed: {err:#}`, inserts, and bails with `skipped {n} distinct threads/messages, over max-skipped-threads {m}` when `len > max`. The WARN body is a pure `skip_warning` fn pinned by a test against the operator `rg -z` shape.
- `execute_state_filters` boundary: `get_thread` + `evaluate_thread` run as one async unit yielding `Result<bool, (op, Report)>`, matched once; op is `threads.get` or `state-filter action`. `apply_state_action` keeps its `?`s, so a failed Move cannot fall through to a later Delete.
- `sanitize_stages`: per-tid `modify_thread` failures go through the same ledger (op `threads.modify`). A tid already in the ledger is not touched again in later stage pairs, and `execute_state_filters` skips (neither fetches nor writes) any thread already in the ledger. `sanitized` now counts tids actually cleaned (or that would be, in dry run) rather than every listed tid.
- `max-skipped-threads` is `Config::max_skipped_threads: usize`, serde default 10, kebab-case via the struct's `rename_all`; example entry with comment in `eratosthenes.example.yml`. 0 is valid (any skip fails the run); negatives fail to parse.
- Corrected the `plan_state_move` comment (was `engine.rs:979-983`): the flip-flop caused the 429 storm, not the 400 failedPrecondition.
- Tests: `tests/error_scope.rs` (classifier on wiremock-produced errors: 400 FP, 400 other, 404 notFound, 429, 503, `RetryExhausted`, transport), `tests/state_isolation.rs` (all three success criteria plus sanitize skip carry-over and an account-scoped sanitize failure), unit tests in `rate.rs`, `skip.rs`, `config.rs`.
- Bites run: (1) boundary arm `Err((op, err)) => skipped.skip_thread(..)?` replaced with `return Err(err)` -> 4 of 7 `state_isolation` tests fail (1-of-N, Move-then-Delete, both ceiling tests). (2) Move failure swallowed inside `apply_state_action` as `Ok(false)` -> Move-then-Delete fails with 1 `threads.trash` request reaching wiremock. (3) sanitize arm reverted to `return Err(err)` -> sanitize carry-over test fails. All restored before CI.

### Deviations
- `engine::execute` now returns `Result<RunSummary>` (`messages_matched`, `threads_transitioned`, `skipped`; public) instead of `Result<()>`. The doc names no return type; integration tests through the public API need the skip count, and the `Done:` line's numbers are the same data. `lib::run` discards it.
- Integration tests rather than in-crate tests: `engine::execute` and `config::parse_config` are already public, so tests drive the real entry point against wiremock with no new public surface beyond `RunSummary`.
- `tests/common/mod.rs` grew `client_with_labels`, `thread_body_with_labels`, `requests_to`, and `pause_on_first_hit`. The last exists because a paused tokio clock auto-advances while an HTTP response is in flight, so pausing up front turns every request into a `REQUEST_TIMEOUT` (first draft of the classifier tests passed 429/503 cases via timeouts and failed the 400 cases outright). The responder pauses the test clock at the first hit, so everything before it is real I/O. Retryable 429/503 bodies are classified from one direct `hub()` call (deterministic), and exhaustion is tested separately.
- Phase 0's `rate_limit_is_retried_under_the_paused_clock` pauses up front; its `> 1 requests` assertion still holds because the server records each timed-out attempt, but its attempts after the first are timeouts, not 429 reads. Left as is (Phase 0's test, criterion still true); noted for awareness.

### Tradeoffs
- `SkipLedger` in its own module vs inline in `engine.rs`: `engine.rs` is ~2.3k lines and Phase 4 (triage) needs the same thread-scoped skip, so a crate-level module is the shared seam.
- One `(op, Report)` match vs two matches (one per call): one match is what the doc specifies and what the bite reverts; the tuple keeps the op in the WARN.
- `RunSummary` vs log capture to assert the skip count: a returned value is deterministic and needs no logger plumbing in tests.

### Open questions
- None.
