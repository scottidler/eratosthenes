//! The triage archive write path (inbox-gate Phase 4), driven through the
//! public `triage::execute` against wiremock, with a stub `claude` script
//! standing in for the LLM (the config's `claude-binary`), as in
//! tests/triage_isolation.rs.
//!
//! What is pinned here: INBOX removal rides the message-level seen batch over
//! the classified ids only; the code guard holds a sole-to-me human thread
//! whatever the model says; a bot in an `awaits-reply` bucket is archived;
//! the message-filter pass runs first with its header guards fetched; mail
//! dated after the filter pass started is left for the next run; and a dry
//! run writes nothing.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{client_with_labels, requests_to};
use eratosthenes::cfg::config::{Config, parse_config};
use eratosthenes::triage::{self, CANDIDATE_QUERY};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param, query_param_contains};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ME: &str = "me@x.com";
const MESSAGES: &str = "/gmail/v1/users/me/messages";
const BATCH_MODIFY: &str = "/gmail/v1/users/me/messages/batchModify";
const THREADS: &str = "/gmail/v1/users/me/threads";
const NEEDS_REPLY_ID: &str = "Label_R";
const NOISE_ID: &str = "Label_N";
const SEEN_ID: &str = "Label_S";
const MARKER_ID: &str = "Label_T";
const LABELS: &[(&str, &str)] = &[
    (NEEDS_REPLY_ID, "llm/needs-reply"),
    (NOISE_ID, "llm/noise"),
    (SEEN_ID, "llm/seen"),
    (MARKER_ID, "Triaged"),
];

/// 2023-11-14: long before any run of this suite.
const PAST_MS: i64 = 1_700_000_000_000;
/// 2100-01-01: after any filter pass start this suite can record.
const FUTURE_MS: i64 = 4_102_444_800_000;

const BOT_TO_LIST: &[(&str, &str)] = &[
    ("From", "Greenhouse <no-reply@greenhouse.io>"),
    ("To", "eng@x.com"),
    ("List-Id", "<eng.x.com>"),
    ("Subject", "pipeline update"),
];
const HUMAN_TO_ME: &[(&str, &str)] = &[
    ("From", "Nick Andolina <nick@x.com>"),
    ("To", ME),
    ("Subject", "quick question"),
];
const HUMAN_WITH_CC: &[(&str, &str)] = &[
    ("From", "Nick Andolina <nick@x.com>"),
    ("To", ME),
    ("Cc", "boss@x.com"),
    ("Subject", "status"),
];
const NO_REPLY_TO_ME: &[(&str, &str)] = &[
    ("From", "Greenhouse <no-reply@greenhouse.io>"),
    ("To", ME),
    ("Subject", "please review a candidate"),
];

/// One mailbox message: id, thread, Gmail `internalDate`, headers.
struct Msg {
    id: &'static str,
    thread: &'static str,
    date_ms: i64,
    headers: &'static [(&'static str, &'static str)],
}

impl Msg {
    fn json(&self) -> Value {
        let headers: Vec<Value> = self
            .headers
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect();
        json!({
            "id": self.id,
            "threadId": self.thread,
            "labelIds": ["INBOX", "UNREAD"],
            "internalDate": self.date_ms.to_string(),
            "payload": { "mimeType": "text/plain", "headers": headers, "body": { "size": 0 } }
        })
    }
}

/// A stub `claude`: `--version` answers, any other call records its stdin
/// payload and answers `assignments` in the real envelope shape.
fn write_stub_claude(dir: &Path, assignments: &[(&str, &str)]) -> PathBuf {
    let script = dir.join("claude");
    let payload = dir.join("payload.txt");
    let threads: Vec<Value> = assignments
        .iter()
        .map(|(id, bucket)| json!({ "id": id, "bucket": bucket }))
        .collect();
    let answer = json!({
        "is_error": false,
        "result": json!({ "threads": threads }).to_string()
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

/// `needs-reply` is an `awaits-reply` bucket that stays; `noise` archives.
/// `message_filters` is spliced in as the account's `message-filters:` block.
fn config(claude: &Path, message_filters: &str) -> Config {
    parse_config(&format!(
        r#"
auth:
  creds-path: /tmp/creds
{message_filters}
triage:
  schedule: "*-*-* *:30:00"
  claude-binary: {}
  buckets:
    - name: needs-reply
      label: llm/needs-reply
      description: a human expects a reply
      archive: false
      awaits-reply: true
    - name: noise
      label: llm/noise
      description: everything else
      archive: true
"#,
        claude.display()
    ))
    .expect("triage config parses")
}

/// The `leadership` shape from the live config: Star the CEO's mail unless it
/// came through a mailing list.
const LEADERSHIP_FILTER: &str = r#"
message-filters:
  - leadership:
      from: 'ceo@x.com'
      headers:
        List-Id: []
      label: INBOX
      action: Star
"#;

/// Profile, candidate listing (`candidates`), one metadata get per candidate,
/// one format=full get and one `threads.modify` per thread in `threads`, and a
/// 204 for every `batchModify`. Any other `messages.list` query (a filter
/// query) returns nothing unless the test mounts its own.
async fn mount_mailbox(server: &MockServer, candidates: &[&Msg], threads: &[(&str, &[&Msg])]) {
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": ME })))
        .mount(server)
        .await;
    let refs: Vec<Value> = candidates
        .iter()
        .map(|m| json!({ "id": m.id, "threadId": m.thread }))
        .collect();
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("q", CANDIDATE_QUERY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "messages": refs })))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .with_priority(10)
        .mount(server)
        .await;
    for m in candidates {
        Mock::given(method("GET"))
            .and(path(format!("{MESSAGES}/{}", m.id)))
            .respond_with(ResponseTemplate::new(200).set_body_json(m.json()))
            .mount(server)
            .await;
    }
    for (thread_id, messages) in threads {
        let body = json!({
            "id": thread_id,
            "historyId": "1000",
            "messages": messages.iter().map(|m| m.json()).collect::<Vec<_>>()
        });
        Mock::given(method("GET"))
            .and(path(format!("{THREADS}/{thread_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{THREADS}/{thread_id}/modify")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": thread_id })))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(BATCH_MODIFY))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// One `batchModify` the run issued, as Gmail received it.
#[derive(Debug, PartialEq)]
struct Batch {
    ids: Vec<String>,
    add: Vec<String>,
    remove: Vec<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

async fn batches(server: &MockServer) -> Vec<Batch> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == BATCH_MODIFY)
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            Batch {
                ids: strings(&body["ids"]),
                add: strings(&body["addLabelIds"]),
                remove: strings(&body["removeLabelIds"]),
            }
        })
        .collect()
}

/// Bodies of every `threads.modify` the run issued.
async fn thread_modifies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/modify"))
        .filter(|r| r.url.path().starts_with(THREADS))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn inbox_removals(batches: &[Batch]) -> Vec<&Batch> {
    batches
        .iter()
        .filter(|b| b.remove.iter().any(|l| l == "INBOX"))
        .collect()
}

/// An archiving bucket: the classified ids leave INBOX in the seen batch, the
/// stay thread's ids get only the marker, and a message dated after the pass
/// started (a reply that landed mid-run, already in the snapshot) is in
/// neither batch. The bucket write itself never removes INBOX.
#[tokio::test]
async fn archiving_bucket_removes_inbox_from_the_classified_ids_only() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t1", "noise"), ("t2", "needs-reply")]);
    let server = MockServer::start().await;
    let t1_m1 = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: BOT_TO_LIST,
    };
    let t1_late = Msg {
        id: "t1-late",
        thread: "t1",
        date_ms: FUTURE_MS,
        headers: HUMAN_TO_ME,
    };
    let t2_m1 = Msg {
        id: "t2-m1",
        thread: "t2",
        date_ms: PAST_MS,
        headers: HUMAN_WITH_CC,
    };
    mount_mailbox(
        &server,
        &[&t1_m1, &t2_m1],
        &[("t1", &[&t1_m1, &t1_late]), ("t2", &[&t2_m1])],
    )
    .await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, ""), "", false)
        .await
        .expect("triage runs");

    let sent = batches(&server).await;
    assert_eq!(
        sent,
        vec![
            Batch {
                ids: vec!["t2-m1".to_string()],
                add: vec![SEEN_ID.to_string()],
                remove: vec![],
            },
            Batch {
                ids: vec!["t1-m1".to_string()],
                add: vec![SEEN_ID.to_string()],
                remove: vec!["INBOX".to_string()],
            },
        ],
        "stay set gets the marker, archive set the marker and INBOX removal"
    );
    assert!(
        sent.iter().all(|b| !b.ids.iter().any(|id| id == "t1-late")),
        "a message dated after the filter pass started is in neither batch: {sent:?}"
    );
    for body in thread_modifies(&server).await {
        assert!(
            !strings(&body["removeLabelIds"]).contains(&"INBOX".to_string()),
            "the thread-level bucket write never removes INBOX: {body}"
        );
        assert!(
            !strings(&body["removeLabelIds"]).contains(&"UNREAD".to_string()),
            "archive never marks read: {body}"
        );
    }
    let payload = std::fs::read_to_string(dir.path().join("payload.txt")).unwrap();
    assert!(
        !payload.contains("quick question"),
        "the late message was classified: {payload}"
    );
}

/// The model filed a human mail to me alone as `noise` (`archive: true`). The
/// code guard keeps it: no INBOX removal anywhere, the marker still lands.
#[tokio::test]
async fn sole_to_me_human_thread_classified_noise_is_not_archived() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t1", "noise")]);
    let server = MockServer::start().await;
    let human = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: HUMAN_TO_ME,
    };
    mount_mailbox(&server, &[&human], &[("t1", &[&human])]).await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, ""), "", false)
        .await
        .expect("triage runs");

    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        1,
        "the noise label still lands"
    );
    let sent = batches(&server).await;
    assert!(
        inbox_removals(&sent).is_empty(),
        "a sole-to-me human thread must not leave INBOX: {sent:?}"
    );
    assert_eq!(
        sent,
        vec![Batch {
            ids: vec!["t1-m1".to_string()],
            add: vec![SEEN_ID.to_string()],
            remove: vec![],
        }]
    );
}

/// A `no-reply` sender classified into the `awaits-reply` bucket, which has
/// `archive: false`: row 4 archives it anyway.
#[tokio::test]
async fn no_reply_thread_in_an_awaits_reply_bucket_is_archived() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t1", "needs-reply")]);
    let server = MockServer::start().await;
    let bot = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: NO_REPLY_TO_ME,
    };
    mount_mailbox(&server, &[&bot], &[("t1", &[&bot])]).await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, ""), "", false)
        .await
        .expect("triage runs");

    let sent = batches(&server).await;
    assert_eq!(
        sent,
        vec![Batch {
            ids: vec!["t1-m1".to_string()],
            add: vec![SEEN_ID.to_string()],
            remove: vec!["INBOX".to_string()],
        }],
        "a bot cannot hold a place in the inbox through an awaits-reply bucket"
    );
    let modifies = thread_modifies(&server).await;
    assert_eq!(modifies.len(), 1);
    assert_eq!(strings(&modifies[0]["addLabelIds"]), vec![NEEDS_REPLY_ID]);
}

/// M2: a message filter guarded on `List-Id: []` must see the header in
/// triage's filter pass. Two CEO messages: f1 came through a list (its
/// `List-Id` is returned only when the fetch asks for it), f2 did not. f2 is
/// starred, proving the pass ran; f1 is not.
#[tokio::test]
async fn list_mail_is_not_starred_by_a_list_id_guarded_filter_in_triage() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[]);
    let server = MockServer::start().await;
    mount_mailbox(&server, &[], &[]).await;

    // The leadership filter's own query returns both CEO messages.
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param_contains("q", "from:(ceo@x.com)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "messages": [
                { "id": "f1", "threadId": "tf1" },
                { "id": "f2", "threadId": "tf2" }
            ]})),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    let ceo = |id: &str, thread: &str, list_id: Option<&str>| {
        let mut headers = vec![
            json!({ "name": "From", "value": "CEO <ceo@x.com>" }),
            json!({ "name": "To", "value": "all@x.com" }),
            json!({ "name": "Subject", "value": "all hands" }),
        ];
        if let Some(list_id) = list_id {
            headers.push(json!({ "name": "List-Id", "value": list_id }));
        }
        json!({
            "id": id,
            "threadId": thread,
            "labelIds": ["INBOX", "UNREAD"],
            "internalDate": PAST_MS.to_string(),
            "payload": { "headers": headers }
        })
    };
    // Gmail's metadata format returns only the headers asked for.
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/f1")))
        .and(query_param("metadataHeaders", "List-Id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ceo(
            "f1",
            "tf1",
            Some("<all.x.com>"),
        )))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/f1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(ceo("f1", "tf1", None)))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/f2")))
        .respond_with(ResponseTemplate::new(200).set_body_json(ceo("f2", "tf2", None)))
        .mount(&server)
        .await;
    for thread in ["tf1", "tf2"] {
        Mock::given(method("GET"))
            .and(path(format!("{THREADS}/{thread}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": thread,
                "messages": [ceo(&format!("{thread}-x"), thread, None)]
            })))
            .mount(&server)
            .await;
    }
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, LEADERSHIP_FILTER), "", false)
        .await
        .expect("triage runs");

    let starred: Vec<String> = batches(&server)
        .await
        .into_iter()
        .filter(|b| b.add.iter().any(|l| l == "STARRED"))
        .flat_map(|b| b.ids)
        .collect();
    assert_eq!(
        starred,
        vec!["f2".to_string()],
        "list mail must not be starred; non-list CEO mail must be"
    );
}

/// M4: a candidate dated at or after the filter pass start is not classified:
/// its thread is never fetched, never in the payload, never marked.
#[tokio::test]
async fn message_dated_after_filter_pass_start_is_not_classified() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t1", "noise")]);
    let server = MockServer::start().await;
    let old = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: BOT_TO_LIST,
    };
    let fresh = Msg {
        id: "t2-m1",
        thread: "t2",
        date_ms: FUTURE_MS,
        headers: BOT_TO_LIST,
    };
    mount_mailbox(
        &server,
        &[&old, &fresh],
        &[("t1", &[&old]), ("t2", &[&fresh])],
    )
    .await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, ""), "", false)
        .await
        .expect("triage runs");

    let payload = std::fs::read_to_string(dir.path().join("payload.txt")).unwrap();
    assert!(payload.contains("\"t1\""), "{payload}");
    assert!(
        !payload.contains("\"t2\""),
        "mail dated after the filter pass started was classified: {payload}"
    );
    assert_eq!(
        requests_to(&server, "GET", &format!("{THREADS}/t2")).await,
        0,
        "the fresh thread must not even be fetched"
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t2/modify")).await,
        0
    );
    let sent = batches(&server).await;
    assert!(
        sent.iter().all(|b| !b.ids.iter().any(|id| id == "t2-m1")),
        "{sent:?}"
    );
}

/// `triage --dry-run` promises zero Gmail mutations: no filter pass, no label
/// creation (every triage label and the marker are missing here), no bucket
/// write, no seen batch. A message filter that WOULD star a message is mounted,
/// so a dry run that ran the filter pass would show up as a POST.
#[tokio::test]
async fn dry_run_issues_zero_non_get_requests() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t1", "noise")]);
    let server = MockServer::start().await;
    let bot = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: BOT_TO_LIST,
    };
    mount_mailbox(&server, &[&bot], &[("t1", &[&bot])]).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param_contains("q", "from:(ceo@x.com)"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "messages": [{ "id": "f2", "threadId": "tf2" }] })),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "x" })))
        .with_priority(20)
        .mount(&server)
        .await;
    let mut client = client_with_labels(&server, &[]).await;

    triage::execute(&mut client, &config(&claude, LEADERSHIP_FILTER), "", true)
        .await
        .expect("a dry run succeeds");

    let payload = std::fs::read_to_string(dir.path().join("payload.txt")).unwrap();
    assert!(
        payload.contains("\"t1\""),
        "precondition: the dry run reached classification: {payload}"
    );
    let writes: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(
        writes.is_empty(),
        "a dry run issued Gmail writes: {writes:?}"
    );
}

/// A thread holding a message the filter pass skipped (a thread-scoped
/// `messages.get` failure) is not classified this run: no filter judged that
/// message, so nothing may archive it. Its sibling thread still is.
#[tokio::test]
async fn thread_with_a_message_the_filter_pass_skipped_is_not_classified() {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_stub_claude(dir.path(), &[("t2", "noise")]);
    let server = MockServer::start().await;
    let skipped = Msg {
        id: "t1-m1",
        thread: "t1",
        date_ms: PAST_MS,
        headers: BOT_TO_LIST,
    };
    let healthy = Msg {
        id: "t2-m1",
        thread: "t2",
        date_ms: PAST_MS,
        headers: BOT_TO_LIST,
    };
    // The filter pass reads t1-m1 first and gets a thread-scoped failure;
    // every later read (candidate discovery) succeeds.
    Mock::given(method("GET"))
        .and(path(format!("{MESSAGES}/t1-m1")))
        .respond_with(common::failed_precondition())
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param_contains("q", "from:(ceo@x.com)"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "messages": [{ "id": "t1-m1", "threadId": "t1" }] })),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    mount_mailbox(
        &server,
        &[&skipped, &healthy],
        &[("t1", &[&skipped]), ("t2", &[&healthy])],
    )
    .await;
    let mut client = client_with_labels(&server, LABELS).await;

    triage::execute(&mut client, &config(&claude, LEADERSHIP_FILTER), "", false)
        .await
        .expect("a thread-scoped filter-pass skip must not fail triage");

    let payload = std::fs::read_to_string(dir.path().join("payload.txt")).unwrap();
    assert!(payload.contains("\"t2\""), "{payload}");
    assert!(
        !payload.contains("\"t1\""),
        "a thread with a skipped message was classified: {payload}"
    );
    assert_eq!(
        requests_to(&server, "POST", &format!("{THREADS}/t1/modify")).await,
        0
    );
    let sent = batches(&server).await;
    assert!(
        sent.iter().all(|b| !b.ids.iter().any(|id| id == "t1-m1")),
        "{sent:?}"
    );
}
