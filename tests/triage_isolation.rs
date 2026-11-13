//! Phase 4 of per-thread error isolation: triage skips a thread whose
//! `threads.get` (full) or bucket write fails with a thread-scoped Gmail error,
//! and still fails on an account-scoped one. Driven through the public
//! `triage::execute` against wiremock, with a stub `claude` script standing in
//! for the LLM (via the config's `claude-binary`).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{client_with_labels, failed_precondition, gmail_error_body, requests_to, thread_body};
use eratosthenes::cfg::config::{Config, parse_config};
use eratosthenes::triage;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MESSAGES: &str = "/gmail/v1/users/me/messages";
const BATCH_MODIFY: &str = "/gmail/v1/users/me/messages/batchModify";
const THREADS: &str = "/gmail/v1/users/me/threads";
const NOISE_ID: &str = "Label_N";
const SEEN_ID: &str = "Label_S";
const MARKER_ID: &str = "Label_T";
/// `Triaged` is the default message-filter marker: live triage runs the filter
/// pass first, and `ensure_labels` would otherwise create it.
const LABELS: &[(&str, &str)] = &[
    (NOISE_ID, "llm/noise"),
    (SEEN_ID, "llm/seen"),
    (MARKER_ID, "Triaged"),
];

/// A stub `claude`: `--version` answers, any other call records its stdin
/// payload and classifies t1 and t3 into `noise` in the real envelope shape.
fn write_stub_claude(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("claude");
    let payload = dir.join("payload.txt");
    let answer = json!({
        "is_error": false,
        "result": r#"{"threads":[{"id":"t1","bucket":"noise"},{"id":"t3","bucket":"noise"}]}"#
    });
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 2.1.300; exit 0; fi\ncat > '{}'\ncat <<'EOF_ENVELOPE'\n{}\nEOF_ENVELOPE\n",
            payload.display(),
            answer
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn config(claude: &Path, max_skipped: usize) -> Config {
    parse_config(&format!(
        r#"
auth:
  creds-path: /tmp/creds
max-skipped-threads: {max_skipped}
triage:
  schedule: "Mon..Fri 06:30:00"
  claude-binary: {}
  buckets:
    - name: noise
      label: llm/noise
      description: everything else
"#,
        claude.display()
    ))
    .expect("triage config parses")
}

fn message_body(id: &str, thread_id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "threadId": thread_id,
        "labelIds": ["INBOX"],
        "internalDate": "1700000000000",
        "payload": { "headers": [{ "name": "Subject", "value": "hello" }] }
    })
}

/// Three candidate messages, one per thread t1..t3; every thread is
/// fetchable at format=full except those in `failing`, which answer `fail`.
async fn mount_mailbox(
    server: &MockServer,
    failing: &[&str],
    failing_modify: &[&str],
    fail: impl Fn() -> ResponseTemplate,
) {
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": "me@x.com" })),
        )
        .mount(server)
        .await;
    let refs: Vec<_> = (1..=3)
        .map(|n| json!({ "id": format!("m{n}"), "threadId": format!("t{n}") }))
        .collect();
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "messages": refs })))
        .mount(server)
        .await;
    for n in 1..=3 {
        let (m, t) = (format!("m{n}"), format!("t{n}"));
        Mock::given(method("GET"))
            .and(path(format!("{MESSAGES}/{m}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(message_body(&m, &t)))
            .mount(server)
            .await;
        let response = if failing.contains(&t.as_str()) {
            fail()
        } else {
            ResponseTemplate::new(200).set_body_json(thread_body(&t))
        };
        Mock::given(method("GET"))
            .and(path(format!("{THREADS}/{t}")))
            .respond_with(response)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{THREADS}/{t}/modify")))
            .respond_with(if failing_modify.contains(&t.as_str()) {
                fail()
            } else {
                ResponseTemplate::new(200).set_body_json(json!({ "id": t }))
            })
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(BATCH_MODIFY))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// Ids carried by every `batchModify` the run issued.
async fn marked_ids(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == BATCH_MODIFY)
        .flat_map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Success criterion: one failing `get_thread_full` -> the rest are
/// classified and labeled, the failed one is skipped, the run is `Ok`.
#[tokio::test]
async fn failing_thread_fetch_skips_one_and_classifies_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_mailbox(&server, &["t2"], &[], failed_precondition).await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, 10), "", false)
        .await
        .expect("a thread-scoped fetch failure must not fail triage");

    let payload = std::fs::read_to_string(dir.path().join("payload.txt")).unwrap();
    assert!(
        payload.contains("\"t1\"") && payload.contains("\"t3\""),
        "{payload}"
    );
    assert!(
        !payload.contains("\"t2\""),
        "the skipped thread was classified: {payload}"
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        1
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t3/modify")).await,
        1
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t2/modify")).await,
        0
    );
    let mut marked = marked_ids(&server).await;
    marked.sort();
    assert_eq!(
        marked,
        vec!["t1-m1", "t3-m1"],
        "only landed buckets earn a marker"
    );
}

/// A failed bucket write skips that thread and withholds its marker, so it
/// resurfaces next run; the other thread's bucket and marker land.
#[tokio::test]
async fn failing_bucket_write_skips_the_thread_and_withholds_its_marker() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_mailbox(&server, &[], &["t3"], failed_precondition).await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, 10), "", false)
        .await
        .expect("a thread-scoped bucket-write failure must not fail triage");

    assert_eq!(
        marked_ids(&server).await,
        vec!["t1-m1"],
        "t3 landed no bucket, so no marker"
    );
}

/// Negative: an account-scoped error on the same call still fails triage, and
/// nothing is written.
#[tokio::test]
async fn account_scoped_fetch_error_still_fails_triage() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_mailbox(&server, &["t2"], &[], || {
        ResponseTemplate::new(401).set_body_json(gmail_error_body(
            401,
            "UNAUTHENTICATED",
            "authError",
        ))
    })
    .await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = triage::execute(&mut client, &config(&claude, 10), "", false)
        .await
        .expect_err("an account-scoped error must fail triage");

    assert!(format!("{err:#}").contains("t2"), "{err:#}");
    assert!(marked_ids(&server).await.is_empty());
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        0
    );
}

/// The ceiling applies to triage too: more distinct skips than
/// `max-skipped-threads` fails the run.
#[tokio::test]
async fn skip_ceiling_fails_triage() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_mailbox(&server, &["t1", "t2"], &[], failed_precondition).await;
    let mut client = client_with_labels(&server, LABELS).await;

    let err = triage::execute(&mut client, &config(&claude, 1), "", false)
        .await
        .expect_err("2 skips over a ceiling of 1 must fail");

    assert!(
        err.to_string().contains("over max-skipped-threads 1"),
        "{err:#}"
    );
}

const RELABEL_ID: &str = "Label_R";

fn draft_config(claude: &Path) -> Config {
    parse_config(&format!(
        r#"
auth:
  creds-path: /tmp/creds
triage:
  schedule: "Mon..Fri 06:30:00"
  claude-binary: {}
  buckets:
    - name: needs-reply
      label: llm/needs-reply
      description: expects a reply
      draft: true
"#,
        claude.display()
    ))
    .expect("draft config parses")
}

/// t1 is the only candidate AND sits in the draft bucket, already answered by
/// the owner (so the refresh would remove its bucket label). Its first
/// `threads.get` fails with `FAILED_PRECONDITION`; every later one succeeds.
async fn mount_skipped_then_healthy_draft_thread(server: &MockServer) {
    let me = "me@x.com";
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": me })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "messages": [{ "id": "t1-m1", "threadId": "t1" }] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/t1-m1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(message_body("t1-m1", "t1")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "threads": [{ "id": "t1" }] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/t1")))
        .respond_with(failed_precondition())
        .up_to_n_times(1)
        .mount(server)
        .await;
    let answered = json!({
        "id": "t1",
        "messages": [{
            "id": "t1-m1",
            "threadId": "t1",
            "labelIds": ["INBOX", RELABEL_ID],
            "internalDate": "1700000000000",
            "payload": { "headers": [
                { "name": "From", "value": me },
                { "name": "To", "value": "them@x.com" },
                { "name": "Subject", "value": "hello" }
            ]}
        }]
    });
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/t1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(answered))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{THREADS}/t1/modify")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "t1" })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(BATCH_MODIFY))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// Audit finding: a thread skipped in the classify pass must get no further
/// reads or writes from the draft refresh. Also the all-candidates-skipped
/// path: the run is Ok and writes nothing.
#[tokio::test]
async fn skipped_thread_gets_no_draft_refresh_write() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_skipped_then_healthy_draft_thread(&server).await;
    let mut client = client_with_labels(
        &server,
        &[
            (RELABEL_ID, "llm/needs-reply"),
            (SEEN_ID, "llm/seen"),
            (MARKER_ID, "Triaged"),
        ],
    )
    .await;

    triage::execute(&mut client, &draft_config(&claude), "", false)
        .await
        .expect("the only candidate was skipped on a thread-scoped error");

    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        0,
        "a skipped thread must get no draft-refresh write"
    );
    assert_eq!(
        requests_to(&server, "GET", &format!("{THREADS}/t1")).await,
        1,
        "a skipped thread must not be refetched"
    );
}

/// `needs-reply` with `draft` as given and `awaits-reply` as given, so the
/// answered sweep is the only thing that can read t1.
fn sweep_config(claude: &Path, awaits_reply: bool) -> Config {
    parse_config(&format!(
        r#"
auth:
  creds-path: /tmp/creds
triage:
  schedule: "Mon..Fri 06:30:00"
  claude-binary: {}
  buckets:
    - name: needs-reply
      label: llm/needs-reply
      description: expects a reply
      draft: false
      awaits-reply: {awaits_reply}
"#,
        claude.display()
    ))
    .expect("sweep config parses")
}

/// No new mail to classify; t1 sits in INBOX under `llm/needs-reply`, and its
/// newest message is my reply.
async fn mount_answered_inbox_thread(server: &MockServer) {
    let me = "me@x.com";
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": me })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "threads": [{ "id": "t1" }] })),
        )
        .mount(server)
        .await;
    let answered = json!({
        "id": "t1",
        "messages": [
            {
                "id": "t1-m1",
                "threadId": "t1",
                "labelIds": ["INBOX", RELABEL_ID],
                "internalDate": "1700000000000",
                "payload": { "headers": [{ "name": "From", "value": "them@x.com" }] }
            },
            {
                "id": "t1-m2",
                "threadId": "t1",
                "labelIds": ["SENT"],
                "internalDate": "1700000100000",
                "payload": { "headers": [{ "name": "From", "value": me }] }
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/t1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(answered))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{THREADS}/t1/modify")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "t1" })))
        .mount(server)
        .await;
}

const SWEEP_LABELS: &[(&str, &str)] = &[
    (RELABEL_ID, "llm/needs-reply"),
    (SEEN_ID, "llm/seen"),
    (MARKER_ID, "Triaged"),
];

/// D1: with `draft: false` the draft pass returns early, so the answered rule
/// never ran. The sweep runs ahead of it for `awaits-reply` buckets.
#[tokio::test]
async fn answered_awaits_reply_thread_loses_label_with_draft_false() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_answered_inbox_thread(&server).await;
    let mut client = client_with_labels(&server, SWEEP_LABELS).await;

    triage::execute(&mut client, &sweep_config(&claude, true), "", false)
        .await
        .expect("the answered sweep runs clean");

    let modifies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/modify"))
        .map(|r| {
            assert_eq!(r.url.path(), format!("{THREADS}/t1/modify"));
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect();
    assert_eq!(
        modifies.len(),
        1,
        "exactly one threads.modify: {modifies:?}"
    );
    assert_eq!(modifies[0]["removeLabelIds"], json!([RELABEL_ID]));
    assert!(
        modifies[0].get("addLabelIds").is_none_or(|v| v.is_null()),
        "the sweep adds nothing: {modifies:?}"
    );
    assert!(
        !modifies[0]["removeLabelIds"]
            .as_array()
            .unwrap()
            .contains(&json!("INBOX")),
        "the thread stays in INBOX under Cull"
    );
    let fetch = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method.as_str() == "GET" && r.url.path() == format!("{THREADS}/t1"))
        .expect("the sweep read t1");
    assert!(
        fetch
            .url
            .query_pairs()
            .any(|(k, v)| k == "format" && v == "metadata"),
        "metadata is enough: {}",
        fetch.url
    );
}

/// A bucket without `awaits-reply` is never swept: same answered thread, no
/// read and no write.
#[tokio::test]
async fn answered_thread_in_plain_bucket_keeps_label() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_answered_inbox_thread(&server).await;
    let mut client = client_with_labels(&server, SWEEP_LABELS).await;

    triage::execute(&mut client, &sweep_config(&claude, false), "", false)
        .await
        .expect("triage runs clean");

    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        0,
        "a plain bucket keeps its label"
    );
    assert_eq!(
        requests_to(&server, "GET", &format!("{THREADS}/t1")).await,
        0,
        "a plain bucket's threads are not even read"
    );
}

/// A dry run reads the sweep's candidates and writes nothing.
#[tokio::test]
async fn answered_sweep_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    mount_answered_inbox_thread(&server).await;
    let mut client = client_with_labels(&server, SWEEP_LABELS).await;

    triage::execute(&mut client, &sweep_config(&claude, true), "", true)
        .await
        .expect("a dry-run sweep runs clean");

    let requests = server.received_requests().await.unwrap();
    let writes: Vec<String> = requests
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(writes.is_empty(), "a dry run wrote: {writes:?}");
    assert_eq!(
        requests_to(&server, "GET", &format!("{THREADS}/t1")).await,
        1,
        "the dry run still reads the candidate"
    );
}

/// The sweep's own unanswered case: newest message inbound, label kept.
#[tokio::test]
async fn unanswered_awaits_reply_thread_keeps_label() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": "me@x.com" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "threads": [{ "id": "t1" }] })),
        )
        .mount(&server)
        .await;
    let waiting = json!({
        "id": "t1",
        "messages": [{
            "id": "t1-m1",
            "threadId": "t1",
            "labelIds": ["INBOX", RELABEL_ID],
            "internalDate": "1700000000000",
            "payload": { "headers": [{ "name": "From", "value": "them@x.com" }] }
        }]
    });
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/t1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .mount(&server)
        .await;
    let mut client = client_with_labels(&server, SWEEP_LABELS).await;

    triage::execute(&mut client, &sweep_config(&claude, true), "", false)
        .await
        .expect("triage runs clean");

    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        0
    );
}
