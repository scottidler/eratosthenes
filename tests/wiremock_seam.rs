//! Phase 0 proof: the production `GmailClient` can be driven against a
//! wiremock server, and the errors it surfaces carry the Gmail body the
//! error classifier will read.

mod common;

use common::{client_for, failed_precondition, rate_limited, thread_body, thread_get_requests};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const THREAD_PATH: &str = "/gmail/v1/users/me/threads/t1";

#[tokio::test]
async fn seam_serves_a_healthy_thread() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(THREAD_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t1")))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let thread = client.get_thread("t1").await.expect("healthy thread");

    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread_get_requests(&server).await, 1);
}

#[tokio::test]
async fn failed_precondition_surfaces_body_after_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(THREAD_PATH))
        .respond_with(failed_precondition())
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let err = client
        .get_thread("t1")
        .await
        .err()
        .expect("400 must surface as an error");

    let body = err
        .chain()
        .find_map(|e| match e.downcast_ref::<google_gmail1::Error>() {
            Some(google_gmail1::Error::BadRequest(body)) => Some(body.clone()),
            _ => None,
        })
        .expect("a BadRequest body somewhere in the error chain");
    assert_eq!(body["error"]["code"], 400);
    assert_eq!(body["error"]["status"], "FAILED_PRECONDITION");
    assert_eq!(
        thread_get_requests(&server).await,
        1,
        "a 400 is permanent: exactly one request, no retry"
    );
}

#[tokio::test]
async fn rate_limit_is_retried_under_the_paused_clock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(THREAD_PATH))
        .respond_with(rate_limited())
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    // Pause only now: construction did real I/O; the backoff ladder is what
    // must run on virtual time.
    tokio::time::pause();
    let err = client
        .get_thread("t1")
        .await
        .err()
        .expect("a permanent 429 exhausts the ladder");

    assert!(
        thread_get_requests(&server).await > 1,
        "a 429 must be retried, got error: {err:#}"
    );
}
