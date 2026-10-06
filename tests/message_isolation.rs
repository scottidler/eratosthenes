//! Phase 3 of per-thread error isolation: message filters drop every write to
//! a thread or message skipped on a thread-scoped Gmail error. Driven through
//! the public `engine::execute` against wiremock, so the production
//! `GmailClient`, its request building and error decoding are all real.

mod common;

use common::{client_with_labels, failed_precondition, gmail_error_body, requests_to};
use eratosthenes::cfg::config::{Config, parse_config};
use eratosthenes::engine;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MESSAGES: &str = "/gmail/v1/users/me/messages";
const BATCH_MODIFY: &str = "/gmail/v1/users/me/messages/batchModify";
const THREADS: &str = "/gmail/v1/users/me/threads";
const KEEP_ID: &str = "Label_K";
const BOTS_ID: &str = "Label_B";
const MARKER_ID: &str = "Label_T";

/// Every label the configs below name, pre-registered so `ensure_labels`
/// creates nothing.
const LABELS: &[(&str, &str)] = &[(KEEP_ID, "Keep"), (BOTS_ID, "Bots"), (MARKER_ID, "Triaged")];

/// One unread inbox message from `from`, in thread `thread_id`.
fn message_body(id: &str, thread_id: &str, from: &str) -> serde_json::Value {
    json!({
        "id": id,
        "threadId": thread_id,
        "labelIds": ["INBOX", "UNREAD"],
        "internalDate": "1700000000000",
        "payload": { "headers": [
            { "name": "From", "value": from },
            { "name": "Subject", "value": "hello" }
        ]}
    })
}

/// Every filter's `messages.list` returns the `(message, thread)` refs; the
/// matcher, which is authoritative, splits them between filters by sender.
/// Also answers the default state filters' `threads.list` with no threads, so
/// Phases 0 and 2 run and touch nothing.
async fn mount_candidates(server: &MockServer, refs: &[(&str, &str)]) {
    let listed: Vec<_> = refs
        .iter()
        .map(|(id, thread)| json!({ "id": id, "threadId": thread }))
        .collect();
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "messages": listed })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
}

async fn mount_message(server: &MockServer, id: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/{id}")))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_thread(server: &MockServer, id: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/{id}")))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_batch_modify(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(BATCH_MODIFY))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// One `batchModify` the engine issued: its ids and label adds/removes.
#[derive(Debug, PartialEq)]
struct Write {
    ids: Vec<String>,
    add: Vec<String>,
    remove: Vec<String>,
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn batch_modify_writes(server: &MockServer) -> Vec<Write> {
    server
        .received_requests()
        .await
        .expect("request recording is on by default")
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == BATCH_MODIFY)
        .map(|r| {
            let body: serde_json::Value =
                serde_json::from_slice(&r.body).expect("batchModify body is json");
            Write {
                ids: strings(&body["ids"]),
                add: strings(&body["addLabelIds"]),
                remove: strings(&body["removeLabelIds"]),
            }
        })
        .collect()
}

fn write(ids: &[&str], add: &[&str], remove: &[&str]) -> Write {
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    Write {
        ids: own(ids),
        add: own(add),
        remove: own(remove),
    }
}

fn written_ids(writes: &[Write]) -> Vec<String> {
    writes.iter().flat_map(|w| w.ids.iter().cloned()).collect()
}

/// `pin-tag` stars, tags, then stamps a marker-only write; `pin-move` stars,
/// then moves with the marker folded in. Between them every write kind a
/// message filter issues: star, Tag, Move, and both marker forms.
fn pinning_config() -> Config {
    parse_config(
        r#"
auth:
  creds-path: /tmp/creds
message-filters:
  - pin-tag:
      from: 'a@x.com'
      action: [Star, {Tag: Keep}]
  - pin-move:
      from: 'b@x.com'
      action: [Star, Bots]
"#,
    )
    .expect("pinning config parses")
}

fn tag_config() -> Config {
    parse_config(
        r#"
auth:
  creds-path: /tmp/creds
message-filters:
  - tag:
      from: 'a@x.com'
      action: {Tag: Keep}
"#,
    )
    .expect("tag config parses")
}

/// Success criterion 1: a thread whose `threads.get` fails in
/// `fetch_thread_labels` gets no star, Tag, Move or marker write: no
/// `batchModify` carries any of its messages' ids. The healthy threads' writes
/// are exactly what they would be with no failure at all.
#[tokio::test]
async fn failed_label_fetch_drops_every_write_to_that_thread() {
    let server = MockServer::start().await;
    mount_candidates(
        &server,
        &[
            ("ma1", "ta1"),
            ("ma2", "ta2"),
            ("mb1", "tb1"),
            ("mb2", "tb2"),
        ],
    )
    .await;
    mount_message(
        &server,
        "ma1",
        ResponseTemplate::new(200).set_body_json(message_body("ma1", "ta1", "a@x.com")),
    )
    .await;
    mount_message(
        &server,
        "ma2",
        ResponseTemplate::new(200).set_body_json(message_body("ma2", "ta2", "a@x.com")),
    )
    .await;
    mount_message(
        &server,
        "mb1",
        ResponseTemplate::new(200).set_body_json(message_body("mb1", "tb1", "b@x.com")),
    )
    .await;
    mount_message(
        &server,
        "mb2",
        ResponseTemplate::new(200).set_body_json(message_body("mb2", "tb2", "b@x.com")),
    )
    .await;
    for healthy in ["ta1", "tb1"] {
        mount_thread(
            &server,
            healthy,
            ResponseTemplate::new(200).set_body_json(common::thread_body(healthy)),
        )
        .await;
    }
    for failing in ["ta2", "tb2"] {
        mount_thread(&server, failing, failed_precondition()).await;
    }
    mount_batch_modify(&server).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = engine::execute(&mut client, &pinning_config(), "", false, false)
        .await
        .expect("thread-scoped label-fetch failures must not fail the run");

    assert_eq!(summary.skipped, 2);
    let writes = batch_modify_writes(&server).await;
    let ids = written_ids(&writes);
    for skipped in ["ma2", "mb2"] {
        assert!(
            !ids.iter().any(|id| id == skipped),
            "{skipped} is on a skipped thread but was written: {writes:?}"
        );
    }
    assert_eq!(
        writes,
        vec![
            write(&["ma1"], &["STARRED"], &[]),
            write(&["ma1"], &[KEEP_ID], &[]),
            write(&["ma1"], &[MARKER_ID], &[]),
            write(&["mb1"], &["STARRED"], &[]),
            write(&["mb1"], &[BOTS_ID, MARKER_ID], &["INBOX", "UNREAD"]),
        ]
    );
    assert_eq!(summary.messages_matched, 2, "only written messages count");
}

/// Success criterion 2: a `messages.get` failing with FAILED_PRECONDITION
/// skips that one message: no write carries its id, the run is `Ok` with 1
/// skip, and its neighbour is written as normal.
#[tokio::test]
async fn failed_get_message_skips_only_that_message() {
    let server = MockServer::start().await;
    mount_candidates(&server, &[("m1", "t1"), ("m2", "t2")]).await;
    mount_message(
        &server,
        "m1",
        ResponseTemplate::new(200).set_body_json(message_body("m1", "t1", "a@x.com")),
    )
    .await;
    mount_message(&server, "m2", failed_precondition()).await;
    mount_batch_modify(&server).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = engine::execute(&mut client, &tag_config(), "", false, false)
        .await
        .expect("a thread-scoped messages.get failure must not fail the run");

    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.messages_matched, 1);
    assert_eq!(
        batch_modify_writes(&server).await,
        vec![
            write(&["m1"], &[KEEP_ID], &[]),
            write(&["m1"], &[MARKER_ID], &[]),
        ]
    );
}

/// `--mark-only` takes the same `messages.get` boundary: the skip is counted
/// in its summary and the skipped message is never stamped.
#[tokio::test]
async fn failed_get_message_is_skipped_under_mark_only() {
    let server = MockServer::start().await;
    mount_candidates(&server, &[("m1", "t1"), ("m2", "t2")]).await;
    mount_message(
        &server,
        "m1",
        ResponseTemplate::new(200).set_body_json(message_body("m1", "t1", "a@x.com")),
    )
    .await;
    mount_message(&server, "m2", failed_precondition()).await;
    mount_batch_modify(&server).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = engine::execute(&mut client, &tag_config(), "", false, true)
        .await
        .expect("a thread-scoped messages.get failure must not fail a mark-only run");

    assert_eq!(summary.skipped, 1);
    assert_eq!(
        batch_modify_writes(&server).await,
        vec![write(&["m1"], &[MARKER_ID], &[])]
    );
}

/// The negative case: a `messages.get` error the classifier calls
/// account-scoped still fails the run, before any write is issued.
#[tokio::test]
async fn account_scoped_get_message_error_fails_the_run() {
    let server = MockServer::start().await;
    mount_candidates(&server, &[("m1", "t1"), ("m2", "t2")]).await;
    mount_message(
        &server,
        "m1",
        ResponseTemplate::new(200).set_body_json(message_body("m1", "t1", "a@x.com")),
    )
    .await;
    mount_message(
        &server,
        "m2",
        ResponseTemplate::new(400).set_body_json(gmail_error_body(
            400,
            "INVALID_ARGUMENT",
            "invalidArgument",
        )),
    )
    .await;
    mount_batch_modify(&server).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = engine::execute(&mut client, &tag_config(), "", false, false)
        .await
        .expect_err("an account-scoped messages.get failure must fail the run");

    assert!(format!("{err:#}").contains("messages.get(m2)"), "{err:#}");
    assert_eq!(requests_to(&server, "POST", BATCH_MODIFY).await, 0);
}
