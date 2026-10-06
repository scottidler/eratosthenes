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
