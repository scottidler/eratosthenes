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
    json!({
        "id": thread_id,
        "messages": [{
            "id": format!("{thread_id}-m1"),
            "threadId": thread_id,
            "labelIds": ["INBOX"],
            "internalDate": "1700000000000",
            "payload": { "headers": [{ "name": "Subject", "value": "hello" }] }
        }]
    })
}

/// Mount the `labels.list` response `GmailClient::new` needs at construction.
async fn mount_labels(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "labels": [{ "id": "INBOX", "name": "INBOX", "type": "system" }]
        })))
        .mount(server)
        .await;
}

/// Build the production `GmailClient` against `server`. A static token stands
/// in for OAuth; the connector is the same hyper-rustls type the hub uses in
/// production, allowed to speak plain http for the local mock.
pub async fn client_for(server: &MockServer) -> GmailClient {
    // Idempotent for tests: a second install in the same process just errors.
    let _ = eratosthenes::init_tls();
    mount_labels(server).await;

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
    hub.base_url(format!("{}/", server.uri()));
    hub.root_url(format!("{}/", server.uri()));

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
