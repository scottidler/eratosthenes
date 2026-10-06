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

## Phase 2 follow-up: Phase 0 429 test
### Design decisions
- `rate_limit_is_retried_under_the_paused_clock` (`tests/wiremock_seam.rs`) now runs on `drive_clock_manually` (`tests/common/mod.rs`). That helper pauses the clock and spawns a task that stays runnable in a `yield_now` loop, so tokio never auto-advances while a response is in flight, and steps virtual time 1s per 50ms of real time. Every attempt reads its 429; the backoffs (38s virtual) take about 2.2s of real time.
- The test now asserts: more than 1 request; `RetryExhausted` present; recorded requests == `RetryExhausted.attempts`; the chain's `BadRequest` body has `error.code == 429`; no `TIMEOUT_MARKER` in `{:#}`.
- Bites: (1) 429 made non-retryable in `json_error_is_retryable` -> the test fails on "a 429 must be retried" after 1 request. (2) The old up-front `tokio::time::pause()` put back -> the test fails with `left: None, right: Some(429)` and the timeout message in the chain, which proves the new assertion catches the old defect. Both restored.

### Deviations
- None.

### Tradeoffs
- Clock driven by hand vs `pause_on_first_hit`: `pause_on_first_hit` still turns attempts 2-5 into timeouts once paused, so it cannot prove every attempt read a 429. Hand-driving costs about 2s of wall clock per test. The 50ms step gives a localhost answer 30 steps (1.5s real) before `REQUEST_TIMEOUT` could fire.
- `pause_on_first_hit` is kept for the Phase 2 tests (`retry_exhausted_is_account`, `rate_limit_on_one_thread_fails_the_run`). Those assert only Account scope / `RetryExhausted`, which holds whatever the last attempt's cause was.

### Open questions
- None.

## Phase 2 follow-up: Phase 2 429 tests
### Design decisions
- `retry_exhausted_is_account` (`tests/error_scope.rs`) and `rate_limit_on_one_thread_fails_the_run` (`tests/state_isolation.rs`) now run on `drive_clock_manually`. Both assert: requests == `RetryExhausted.attempts`; `gmail_error_code(&err) == Some(429)` (new helper in `tests/common/mod.rs`, the first `BadRequest` body's `error.code` in the chain); no `TIMEOUT_MARKER`. The engine test also asserts the cause names `threads.get(r2)`.
- `pause_on_first_hit` had no users left and is deleted (it supersedes the Phase 2 entry's mention of it).
- Bites: (1) up-front `tokio::time::pause()` in `retry_exhausted_is_account` -> fails on the 429 assertion (`left: None, right: Some(429)`, chain reads "transport timeout"). (2) Same in `rate_limit_on_one_thread_fails_the_run` -> fails earlier, on `exhausted.op` (`"threads.list (by label IDs)"` vs `"threads.get"`), because the up-front pause makes sanitize's `threads.list` time out first. Both restored.

### Deviations
- None.

### Tradeoffs
- Each hand-driven test adds about 2.2s of wall clock; accepted for proving every attempt reads a real 429.

### Open questions
- None.

## Phase 3: Message-filter drop-writes
### Design decisions
- `execute_message_filters` takes `&mut SkipLedger`; both call sites in `engine::execute` (normal and `--mark-only`) pass the run's one ledger.
- `get_message` boundary - `src/engine.rs:execute_message_filters` - a failure goes through `SkipLedger::skip_message(id, "messages.get", ..)`; `Thread` scope logs `skipping message {id}: messages.get failed: ...`, records, ceiling-checks, and the message never enters `messages`, so it never matches. `Account` scope propagates unchanged.
- `fetch_thread_labels` takes the ledger, skips a thread whose `threads.get` fails with `Thread` scope (`skip_thread(.., "threads.get", ..)`), and does not refetch a thread already in the ledger.
- The seam is a pure `drop_skipped_threads(matched_ids, messages, skipped) -> Vec<String>` - `src/engine.rs` - called after `fetch_thread_labels` and before `plan_filter_writes`, for every filter (pinning or not, mark-only included). So a thread skipped by sanitize (Phase 0) or by the label fetch gets no star, Tag, Move or marker. `plan_filter_writes` is unchanged and stays pure.
- `SkipLedger` keys are now `(Skipped::{Thread, Message}, id)`, and `contains` became `contains_thread` - `src/skip.rs` - because Gmail gives a thread the id of its first message: with bare ids, a skipped first message would read as a skipped thread and drop its healthy siblings' writes (and skip the thread in Phase 2), which the doc's "skip that one message" rules out. The ceiling still counts distinct skipped ids, now distinct per kind.
- `messages_matched` (and the `Done:` line's matched/marked count) counts messages actually planned for writing, after the drop, so `--mark-only`'s "N messages marked" stays true. `claimed` still counts every match.
- Tests: `tests/message_isolation.rs` (label-fetch failure drops all five write kinds for the skipped threads with the healthy threads' writes pinned exactly; `messages.get` skip under normal and `--mark-only`; account-scoped `messages.get` error fails the run with no write), unit tests for `drop_skipped_threads` and the new ledger API.
- Bites run: (1) `drop_skipped_threads` call replaced with `matched_ids.to_vec()` -> `failed_label_fetch_drops_every_write_to_that_thread` fails with `ma2 is on a skipped thread but was written`, every batchModify carrying `ma2`/`mb2` (duplicate star, Tag, marker, Move). (2) `get_message` arm reverted to `return Err(err)` -> both `messages.get` skip tests fail with `messages.get(m2) failed ... FAILED_PRECONDITION`. Both restored.
- Acceptance `rg -nU '(get_thread|get_message|modify_thread|trash_thread)\([^;]*?\)\s*\.await\?' src/engine.rs` now prints only the two `apply_state_action` writes (`modify_thread` at :1206-1207, `trash_thread` at :1217).

### Deviations
- `fetch_thread_labels` records skips into the shared ledger and returns `Result<()>` instead of returning `(labels, skipped_thread_ids)`: same effect, correct seam. The run already has one ledger (Phase 2), and a second skipped set would have to be merged back into it for the ceiling and for Phase 2's no-further-writes check.
- Ledger keys carry their kind (thread vs message), not bare ids as the Data Model's single `HashSet<String>` reads. Reason above (thread id == first message id).

### Tradeoffs
- Filter `matched_ids` for every filter vs only pinning filters: the doc names the drop "before `plan_filter_writes`"; applying it everywhere also covers sanitize skips on non-pinning filters at no extra calls.
- No integration test for a sanitize-stage skip carrying into message filters; the ledger lookup it relies on is the same `contains_thread` the unit tests and the label-fetch test exercise.

### Open questions
- None.
