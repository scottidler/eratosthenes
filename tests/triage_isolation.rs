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
const LABELS: &[(&str, &str)] = &[(NOISE_ID, "llm/noise"), (SEEN_ID, "llm/seen")];

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
