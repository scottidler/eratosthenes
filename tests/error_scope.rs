//! `error_scope` on errors the production `GmailClient` actually produces
//! against wiremock, so the classifier is proven on `google_gmail1`'s own
//! error decoding rather than on hand-built values.

mod common;

use common::{
    client_at, client_for, drive_clock_manually, failed_precondition, gmail_error_body,
    gmail_error_code, rate_limited, thread_get_requests,
};
use eratosthenes::gmail::client::GmailClient;
use eratosthenes::gmail::rate::{
    ErrorScope, RetryExhausted, TIMEOUT_MARKER, error_scope, is_retryable,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const THREAD_PATH: &str = "/gmail/v1/users/me/threads/t1";

async fn server_answering(response: ResponseTemplate) -> (MockServer, GmailClient) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(THREAD_PATH))
        .respond_with(response)
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    (server, client)
}

/// A permanent answer through the real `get_thread`: one attempt, no retry.
async fn get_thread_error(response: ResponseTemplate) -> eyre::Report {
    let (server, client) = server_answering(response).await;
    let err = client
        .get_thread("t1")
        .await
        .err()
        .expect("the canned answer is an error");
    assert_eq!(thread_get_requests(&server).await, 1, "{err:#}");
    err
}

/// ONE attempt straight through the hub, bypassing `with_retry`, so a
/// retryable answer's own decoded error can be classified deterministically.
async fn single_attempt_error(response: ResponseTemplate) -> eyre::Report {
    let (_server, client) = server_answering(response).await;
    let err = client
        .hub()
        .users()
        .threads_get("me", "t1")
        .add_scope(eratosthenes::gmail::auth::GMAIL_SCOPE)
        .doit()
        .await
        .expect_err("the canned answer is an error");
    eyre::Report::new(err)
}

#[tokio::test]
async fn failed_precondition_400_is_thread() {
    let err = get_thread_error(failed_precondition()).await;
    assert_eq!(error_scope(&err), ErrorScope::Thread, "{err:#}");
}

#[tokio::test]
async fn other_400_is_account() {
    let err = get_thread_error(ResponseTemplate::new(400).set_body_json(gmail_error_body(
        400,
        "INVALID_ARGUMENT",
        "invalidArgument",
    )))
    .await;
    assert_eq!(error_scope(&err), ErrorScope::Account, "{err:#}");
}

#[tokio::test]
async fn not_found_404_is_thread() {
    let err = get_thread_error(ResponseTemplate::new(404).set_body_json(gmail_error_body(
        404,
        "NOT_FOUND",
        "notFound",
    )))
    .await;
    assert_eq!(error_scope(&err), ErrorScope::Thread, "{err:#}");
}

#[tokio::test]
async fn rate_limit_429_is_account() {
    let err = single_attempt_error(rate_limited()).await;
    assert!(format!("{err:#}").contains("rateLimitExceeded"), "{err:#}");
    assert!(is_retryable(&err));
    assert_eq!(error_scope(&err), ErrorScope::Account, "{err:#}");
}

#[tokio::test]
async fn backend_error_503_is_account() {
    let err = single_attempt_error(ResponseTemplate::new(503).set_body_json(gmail_error_body(
        503,
        "UNAVAILABLE",
        "backendError",
    )))
    .await;
    assert!(format!("{err:#}").contains("backendError"), "{err:#}");
    assert!(is_retryable(&err));
    assert_eq!(error_scope(&err), ErrorScope::Account, "{err:#}");
}

/// The ladder exhausting on a real 429 yields `RetryExhausted`, which is
/// `Account`. The clock is driven by hand so every attempt reads its 429.
#[tokio::test]
async fn retry_exhausted_is_account() {
    let (server, client) = server_answering(rate_limited()).await;
    let clock = drive_clock_manually();
    let err = client
        .get_thread("t1")
        .await
        .err()
        .expect("a permanent 429 exhausts the ladder");
    clock.abort();
    let exhausted = err
        .downcast_ref::<RetryExhausted>()
        .expect("exhaustion is marked");
    assert_eq!(exhausted.op, "threads.get");
    assert_eq!(
        thread_get_requests(&server).await,
        exhausted.attempts as usize,
        "{err:#}"
    );
    assert_eq!(gmail_error_code(&err), Some(429), "{err:#}");
    assert!(!format!("{err:#}").contains(TIMEOUT_MARKER), "{err:#}");
    assert_eq!(error_scope(&err), ErrorScope::Account, "{err:#}");
}

/// A client whose `GmailClient::new` succeeded against a one-shot listener
/// that answered `labels.list` with `Connection: close` and then exited, so the
/// port it built against now has nothing listening.
async fn client_on_dead_port() -> GmailClient {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = conn.read(&mut buf);
        let body = r#"{"labels":[{"id":"INBOX","name":"INBOX","type":"system"}]}"#;
        write!(
            conn,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let client = client_at(&format!("http://127.0.0.1:{port}")).await;
    serving.join().unwrap();
    client
}

/// Transport: connection refused on every attempt (a real transport failure,
/// not the 30s request timeout), so the ladder exhausts and the classification
/// is `Account`.
#[tokio::test]
async fn transport_failure_is_account() {
    let client = client_on_dead_port().await;
    tokio::time::pause();
    let err = client
        .get_thread("t1")
        .await
        .err()
        .expect("nothing is listening, so no thread");
    assert!(err.downcast_ref::<RetryExhausted>().is_some(), "{err:#}");
    assert_eq!(error_scope(&err), ErrorScope::Account, "{err:#}");
    assert!(
        !format!("{err:#}").contains(TIMEOUT_MARKER),
        "must not be a timeout: {err:#}"
    );
}
