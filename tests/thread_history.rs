//! `historyId` plumbing: `threads.list` and `threads.get` both surface the
//! per-thread stamp the cache diffs on, and the list asks for 500 per page.

mod common;

use common::{client_for, thread_body, thread_get_only_requests};
use eratosthenes::gmail::client::ThreadRef;
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIST_PATH: &str = "/gmail/v1/users/me/threads";

#[tokio::test]
async fn list_thread_refs_returns_history_id_per_thread_at_500_per_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(LIST_PATH))
        .and(query_param("maxResults", "500"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [
                { "id": "t1", "historyId": "111" },
                { "id": "t2", "historyId": "222" },
                { "id": "t3" }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let list = client.list_thread_refs("in:inbox").await.expect("list");

    assert_eq!(
        list.refs,
        vec![
            ThreadRef {
                id: "t1".into(),
                history_id: Some(111)
            },
            ThreadRef {
                id: "t2".into(),
                history_id: Some(222)
            },
            ThreadRef {
                id: "t3".into(),
                history_id: None
            },
        ]
    );
    assert_eq!(list.pages, 1);
    assert_eq!(list.skipped_items, 0);
}

#[tokio::test]
async fn list_thread_refs_paginates_and_skips_items_without_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(LIST_PATH))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [{ "id": "t1", "historyId": "1" }, { "historyId": "9" }],
            "nextPageToken": "p2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(LIST_PATH))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [{ "id": "t2", "historyId": "2" }]
        })))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let list = client.list_thread_refs("in:inbox").await.expect("list");

    let ids: Vec<&str> = list.refs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["t1", "t2"]);
    assert_eq!(list.pages, 2);
    assert_eq!(list.skipped_items, 1);
}

#[tokio::test]
async fn list_thread_refs_errors_when_a_later_page_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(LIST_PATH))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [{ "id": "t1", "historyId": "1" }],
            "nextPageToken": "p2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(LIST_PATH))
        .and(query_param("pageToken", "p2"))
        .respond_with(common::failed_precondition())
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    assert!(client.list_thread_refs("in:inbox").await.is_err());
}

#[tokio::test]
async fn get_thread_returns_history_id_and_counts_no_skips() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads/t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t1")))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let thread = client.get_thread("t1").await.expect("thread");

    assert_eq!(thread.history_id, Some(1000));
    assert_eq!(thread.skipped_messages, 0);
    assert_eq!(thread_get_only_requests(&server).await, 1);
}

#[tokio::test]
async fn get_thread_counts_malformed_messages_it_skips() {
    let server = MockServer::start().await;
    let mut body = thread_body("t1");
    // No internalDate: `GmailMessage::from_api` rejects it.
    body["messages"]
        .as_array_mut()
        .expect("messages")
        .push(json!({ "id": "t1-bad", "threadId": "t1" }));
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads/t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let thread = client.get_thread("t1").await.expect("thread");

    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread.skipped_messages, 1);
}
