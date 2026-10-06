//! Phase 2 of per-thread error isolation: a thread-scoped Gmail error skips
//! that one thread, account-scoped errors and the skip ceiling still fail the
//! run. Driven through the public `engine::execute` against wiremock, so the
//! production `GmailClient`, retry ladder and error decoding are all real.

mod common;

use common::{
    client_with_labels, failed_precondition, pause_on_first_hit, rate_limited, requests_to,
    thread_body_with_labels, thread_get_requests,
};
use eratosthenes::cfg::config::parse_config;
use eratosthenes::engine::{self, RunSummary};
use eratosthenes::gmail::client::GmailClient;
use eratosthenes::gmail::rate::RetryExhausted;
use serde_json::json;
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

const THREADS: &str = "/gmail/v1/users/me/threads";
const PURGATORY_ID: &str = "Label_P";
const NOISE_ID: &str = "Label_N";

/// Every label the configs below name, pre-registered so `ensure_labels`
/// creates nothing and no `labels.create` mock is needed.
const LABELS: &[(&str, &str)] = &[
    (PURGATORY_ID, "Purgatory"),
    (NOISE_ID, "llm/noise"),
    ("Label_T", "Triaged"),
];

fn thread_path(id: &str) -> String {
    format!("{THREADS}/{id}")
}

fn ids(prefix: &str, n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("{prefix}{i}")).collect()
}

fn thread_list(ids: &[String]) -> serde_json::Value {
    json!({ "threads": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>() })
}

/// One state filter, `Cull`: every inbox thread older than 1d moves to Purgatory.
fn cull_config(max_skipped: usize) -> eratosthenes::cfg::config::Config {
    parse_config(&format!(
        r#"
auth:
  creds-path: /tmp/creds
max-skipped-threads: {max_skipped}
state-filters:
  - Cull:
      ttl: 1d
      action: Purgatory
"#
    ))
    .expect("cull config parses")
}

/// Stage sanitization's `threads.list` (labelIds, no `q`) finds no conflicts.
async fn mount_no_stage_conflicts(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(THREADS))
        .and(query_param_is_missing("q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
}

/// Phase 2's active-threads `threads.list` (it carries `q`) returns `ids`.
/// Mounted after the sanitize mock, which only matches requests without `q`.
async fn mount_active_threads(server: &MockServer, ids: &[String]) {
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(ResponseTemplate::new(200).set_body_json(thread_list(ids)))
        .mount(server)
        .await;
}

async fn mount_get(server: &MockServer, id: &str, response: impl Respond + 'static) {
    Mock::given(method("GET"))
        .and(path(thread_path(id)))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_healthy_inbox_thread(server: &MockServer, id: &str) {
    mount_get(
        server,
        id,
        ResponseTemplate::new(200).set_body_json(thread_body_with_labels(id, &["INBOX"])),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("{}/modify", thread_path(id))))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": id })))
        .mount(server)
        .await;
}

async fn modify_requests(server: &MockServer, id: &str) -> usize {
    requests_to(server, "POST", &format!("{}/modify", thread_path(id))).await
}

async fn run(
    client: &mut GmailClient,
    config: &eratosthenes::cfg::config::Config,
) -> eyre::Result<RunSummary> {
    engine::execute(client, config, "", false, false).await
}

/// Success criterion 1: one of N threads answers `threads.get` with
/// FAILED_PRECONDITION -> the run is `Ok`, the other N-1 are evaluated and
/// moved, the failing one is skipped and gets no write.
#[tokio::test]
async fn one_failed_precondition_get_skips_only_that_thread() {
    let server = MockServer::start().await;
    let threads = ids("t", 5);
    mount_no_stage_conflicts(&server).await;
    mount_active_threads(&server, &threads).await;
    for id in &threads {
        if id == "t3" {
            mount_get(&server, id, failed_precondition()).await;
        } else {
            mount_healthy_inbox_thread(&server, id).await;
        }
    }
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = run(&mut client, &cull_config(10))
        .await
        .expect("one thread-scoped failure must not fail the run");

    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.threads_transitioned, 4);
    for id in &threads {
        let expected = if id == "t3" { 0 } else { 1 };
        assert_eq!(modify_requests(&server, id).await, expected, "thread {id}");
    }
}

/// Success criterion 2: a thread whose first matching filter is a Move that
/// fails with FAILED_PRECONDITION, and whose later matching filter is a
/// Delete, is skipped whole: no `threads.trash` ever reaches Gmail.
#[tokio::test]
async fn failed_move_never_falls_through_to_a_later_delete() {
    let server = MockServer::start().await;
    let config = parse_config(
        r#"
auth:
  creds-path: /tmp/creds
state-filters:
  - age-noise:
      label: llm/noise
      ttl: 1d
      action: Purgatory
  - nuke-noise:
      label: llm/noise
      ttl: 1d
      action: { Delete: "" }
"#,
    )
    .expect("move-then-delete config parses");
    let threads = vec!["n1".to_string()];
    mount_no_stage_conflicts(&server).await;
    mount_active_threads(&server, &threads).await;
    mount_get(
        &server,
        "n1",
        ResponseTemplate::new(200)
            .set_body_json(thread_body_with_labels("n1", &["INBOX", NOISE_ID])),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("{}/modify", thread_path("n1"))))
        .respond_with(failed_precondition())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{}/trash", thread_path("n1"))))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "n1" })))
        .mount(&server)
        .await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = run(&mut client, &config)
        .await
        .expect("a thread-scoped Move failure must not fail the run");

    assert_eq!(
        modify_requests(&server, "n1").await,
        1,
        "the Move was tried"
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{}/trash", thread_path("n1"))).await,
        0,
        "a failed Move must not fall through to the Delete"
    );
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.threads_transitioned, 0);
}

/// Mount 12 active threads whose first `failing` answer `threads.get` with
/// FAILED_PRECONDITION; the rest are healthy.
async fn mount_twelve_with_failures(server: &MockServer, failing: usize) -> Vec<String> {
    let threads = ids("c", 12);
    mount_no_stage_conflicts(server).await;
    mount_active_threads(server, &threads).await;
    for (i, id) in threads.iter().enumerate() {
        if i < failing {
            mount_get(server, id, failed_precondition()).await;
        } else {
            mount_healthy_inbox_thread(server, id).await;
        }
    }
    threads
}

/// Success criterion 3a: exactly `max-skipped-threads` distinct skips is fine.
#[tokio::test]
async fn ten_skips_at_a_ceiling_of_ten_is_ok() {
    let server = MockServer::start().await;
    mount_twelve_with_failures(&server, 10).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = run(&mut client, &cull_config(10))
        .await
        .expect("10 skips at max-skipped-threads 10 is within the ceiling");

    assert_eq!(summary.skipped, 10);
    assert_eq!(summary.threads_transitioned, 2);
}

/// Success criterion 3b: the 11th distinct skip fails the run on the spot,
/// before a 12th `threads.get` is spent, with the ceiling text and NOT the
/// per-thread cause.
#[tokio::test]
async fn eleventh_skip_fails_the_run_before_the_twelfth_get() {
    let server = MockServer::start().await;
    mount_twelve_with_failures(&server, 11).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = run(&mut client, &cull_config(10))
        .await
        .expect_err("11 skips at max-skipped-threads 10 must fail the run");

    assert_eq!(
        format!("{err:#}"),
        "skipped 11 distinct threads/messages, over max-skipped-threads 10"
    );
    assert_eq!(
        requests_to(&server, "GET", &thread_path("c12")).await,
        0,
        "the ceiling must trip before the 12th threads.get"
    );
    assert_eq!(thread_get_requests(&server).await, 11);
}

/// Success criterion 3c: a 429 on one thread is account-scoped (the ladder
/// exhausts), so it fails the run rather than being skipped.
#[tokio::test]
async fn rate_limit_on_one_thread_fails_the_run() {
    let server = MockServer::start().await;
    let threads = ids("r", 3);
    mount_no_stage_conflicts(&server).await;
    mount_active_threads(&server, &threads).await;
    mount_healthy_inbox_thread(&server, "r1").await;
    // Real I/O up to r2's first answer, virtual time for its backoff ladder.
    mount_get(&server, "r2", pause_on_first_hit(rate_limited())).await;
    mount_healthy_inbox_thread(&server, "r3").await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = run(&mut client, &cull_config(10))
        .await
        .expect_err("a 429 is account-scoped and must fail the run");

    let exhausted = err
        .downcast_ref::<RetryExhausted>()
        .expect("the 429 exhausted the ladder");
    assert_eq!(exhausted.op, "threads.get");
    assert_eq!(modify_requests(&server, "r1").await, 1, "r1 ran normally");
    assert!(requests_to(&server, "GET", &thread_path("r2")).await > 1);
    assert_eq!(
        requests_to(&server, "GET", &thread_path("r3")).await,
        0,
        "the run stops at the account-scoped failure"
    );
}

/// Phase 0 (stage sanitization) shares the boundary: a FAILED_PRECONDITION on
/// one tid's cleanup `threads.modify` skips it, and the skip carries into
/// Phase 2, which neither fetches nor writes that thread again this run.
#[tokio::test]
async fn sanitize_skip_carries_into_state_filters() {
    let server = MockServer::start().await;
    // INBOX + Purgatory conflict on s1 (cleanup fails) and s2 (cleanup works).
    Mock::given(method("GET"))
        .and(path(THREADS))
        .and(query_param_is_missing("q"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(thread_list(&["s1".to_string(), "s2".to_string()])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{}/modify", thread_path("s1"))))
        .respond_with(failed_precondition())
        .mount(&server)
        .await;
    mount_active_threads(&server, &["s1".to_string(), "s2".to_string()]).await;
    mount_healthy_inbox_thread(&server, "s2").await;
    mount_get(
        &server,
        "s1",
        ResponseTemplate::new(200).set_body_json(thread_body_with_labels("s1", &["INBOX"])),
    )
    .await;
    let mut client = client_with_labels(&server, LABELS).await;

    let summary = run(&mut client, &cull_config(10))
        .await
        .expect("a thread-scoped sanitize failure must not fail the run");

    assert_eq!(summary.skipped, 1);
    assert_eq!(
        modify_requests(&server, "s1").await,
        1,
        "only the failed cleanup reached s1"
    );
    assert_eq!(
        requests_to(&server, "GET", &thread_path("s1")).await,
        0,
        "a thread skipped in sanitization is not fetched again this run"
    );
    // s2: one sanitize cleanup plus one state-filter Move.
    assert_eq!(modify_requests(&server, "s2").await, 2);
}

/// An account-scoped failure in sanitization (a non-thread 400) still fails
/// the run: the boundary only swallows the allowlisted thread shapes.
#[tokio::test]
async fn account_scoped_sanitize_failure_fails_the_run() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(THREADS))
        .and(query_param_is_missing("q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(thread_list(&["s1".to_string()])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{}/modify", thread_path("s1"))))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(common::gmail_error_body(
                400,
                "INVALID_ARGUMENT",
                "invalidArgument",
            )),
        )
        .mount(&server)
        .await;
    mount_active_threads(&server, &[]).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = run(&mut client, &cull_config(10))
        .await
        .expect_err("a non-thread 400 is account-scoped");
    assert!(format!("{err:#}").contains("INVALID_ARGUMENT"), "{err:#}");
}
