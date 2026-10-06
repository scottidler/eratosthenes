//! Shared wiremock harness: the PRODUCTION `GmailClient` pointed at a local
//! mock server through `Hub::base_url`, so a test exercises `google_gmail1`'s
//! real request building and error decoding, and the real `with_retry` ladder.

#![allow(dead_code)]

use eratosthenes::gmail::client::GmailClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type Hub = google_gmail1::Gmail<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
>;

/// A Gmail-shaped error body, exactly as the API returns it.
pub fn gmail_error_body(code: u16, status: &str, reason: &str) -> serde_json::Value {
    json!({
        "error": {
            "code": code,
            "message": format!("canned {status}"),
            "status": status,
            "errors": [{ "message": format!("canned {status}"), "domain": "global", "reason": reason }]
        }
    })
}

/// The `400 FAILED_PRECONDITION` Gmail returns on one healthy thread.
pub fn failed_precondition() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(gmail_error_body(
        400,
        "FAILED_PRECONDITION",
        "failedPrecondition",
    ))
}

/// A `429 rateLimitExceeded`, the retryable class.
pub fn rate_limited() -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_json(gmail_error_body(
        429,
        "RESOURCE_EXHAUSTED",
        "rateLimitExceeded",
    ))
}

/// A thread with one message, enough for `GmailMessage::from_api`.
pub fn thread_body(thread_id: &str) -> serde_json::Value {
    thread_body_with_labels(thread_id, &["INBOX"])
}

/// A one-message thread carrying `label_ids` (Gmail label ids, not names),
/// last active 2023-11-14, so any day-based TTL has long expired.
pub fn thread_body_with_labels(thread_id: &str, label_ids: &[&str]) -> serde_json::Value {
    json!({
        "id": thread_id,
        "messages": [{
            "id": format!("{thread_id}-m1"),
            "threadId": thread_id,
            "labelIds": label_ids,
            "internalDate": "1700000000000",
            "payload": { "headers": [{ "name": "Subject", "value": "hello" }] }
        }]
    })
}

/// Mount the `labels.list` response `GmailClient::new` needs at construction:
/// INBOX plus each `(id, name)` user label, so `ensure_labels` creates nothing.
async fn mount_labels(server: &MockServer, user_labels: &[(&str, &str)]) {
    let mut labels = vec![json!({ "id": "INBOX", "name": "INBOX", "type": "system" })];
    labels.extend(
        user_labels
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name, "type": "user" })),
    );
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "labels": labels })))
        .mount(server)
        .await;
}

/// Build the production `GmailClient` against `server`. A static token stands
/// in for OAuth; the connector is the same hyper-rustls type the hub uses in
/// production, allowed to speak plain http for the local mock.
pub async fn client_for(server: &MockServer) -> GmailClient {
    client_with_labels(server, &[]).await
}

/// `client_for`, with `(id, name)` user labels already present in the mailbox.
pub async fn client_with_labels(server: &MockServer, user_labels: &[(&str, &str)]) -> GmailClient {
    mount_labels(server, user_labels).await;
    client_at(&server.uri()).await
}

/// The production `GmailClient` pointed at `base_uri`. The caller must have
/// arranged for `labels.list` to answer there (`GmailClient::new` calls it).
pub async fn client_at(base_uri: &str) -> GmailClient {
    // Idempotent for tests: a second install in the same process just errors.
    let _ = eratosthenes::init_tls();

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native roots")
        .https_or_http()
        .enable_http1()
        .build();
    let mut hub: Hub = google_gmail1::Gmail::new(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector),
        "test-token".to_string(),
    );
    hub.base_url(format!("{base_uri}/"));
    hub.root_url(format!("{base_uri}/"));

    GmailClient::new(hub, "")
        .await
        .expect("GmailClient::new against wiremock")
}

/// Requests the mock saw for `threads.get` (any thread id).
pub async fn thread_get_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording is on by default")
        .iter()
        .filter(|r| r.url.path().starts_with("/gmail/v1/users/me/threads/"))
        .count()
}

/// Requests the mock saw with `method` whose path is exactly `path`.
pub async fn requests_to(server: &MockServer, method: &str, path: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording is on by default")
        .iter()
        .filter(|r| r.method.as_str() == method && r.url.path() == path)
        .count()
}

/// Virtual time that only moves when the test says so, so EVERY retry attempt
/// gets its real HTTP answer. Pauses the clock, then spawns a task that stays
/// runnable (a `yield_now` loop), which keeps tokio from auto-advancing while a
/// response is in flight, and steps the clock 1s per `STEP_REAL` of real time.
/// A localhost answer lands in milliseconds, far inside the 30
/// steps a `REQUEST_TIMEOUT` would take, while the 1/2/5/10/20s backoffs
/// elapse in about 2s of real time. Must be called inside the test's
/// (current-thread) runtime, after any real-time setup.
pub fn drive_clock_manually() -> tokio::task::JoinHandle<()> {
    const STEP_REAL: std::time::Duration = std::time::Duration::from_millis(50);
    tokio::time::pause();
    tokio::spawn(async {
        loop {
            let started = std::time::Instant::now();
            while started.elapsed() < STEP_REAL {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
    })
}

/// `error.code` of the first `BadRequest` body in `err`'s chain: what the last
/// attempt actually read. `None` means it never got a Gmail answer (a timeout
/// or transport failure).
pub fn gmail_error_code(err: &eyre::Report) -> Option<u64> {
    err.chain()
        .find_map(|e| match e.downcast_ref::<google_gmail1::Error>() {
            Some(google_gmail1::Error::BadRequest(body)) => body["error"]["code"].as_u64(),
            _ => None,
        })
}
