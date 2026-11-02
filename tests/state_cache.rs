//! Phase 3 of the thread history cache: Phase 2 (state filters) evaluates an
//! unchanged thread from the cache and fetches only threads whose `historyId`
//! stamp moved. Driven through the public `engine::execute` and
//! `execute_with_cache` against wiremock, so the production `GmailClient`,
//! list pagination and retry ladder are all real.
//!
//! Each run gets its own `MockServer`, so request counts are per run while the
//! `ThreadCache` carries across runs exactly as the on-disk file would.

mod common;

use chrono::{Duration, Utc};
use common::{
    client_with_labels, drive_clock_manually, rate_limited, requests_to, thread_get_only_requests,
};
use eratosthenes::cache::ThreadCache;
use eratosthenes::cfg::config::{Config, parse_config};
use eratosthenes::engine::{self, RunSummary};
use eratosthenes::execute_with_cache;
use eratosthenes::gmail::client::GmailClient;
use serde_json::json;
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const THREADS: &str = "/gmail/v1/users/me/threads";
const PURGATORY_ID: &str = "Label_P";
const NOISE_ID: &str = "Label_N";

const LABELS: &[(&str, &str)] = &[
    (PURGATORY_ID, "Purgatory"),
    (NOISE_ID, "llm/noise"),
    ("Label_T", "Triaged"),
];

/// 2023-11-14: every day-based TTL has long expired.
const OLD_MS: i64 = 1_700_000_000_000;

/// An hour ago: inside every day-based TTL, so the thread is left alone.
fn recent_ms() -> i64 {
    (Utc::now() - Duration::hours(1)).timestamp_millis()
}

/// One thread as the mock mailbox holds it: what `threads.list` stamps it with
/// and what `threads.get` returns for it.
#[derive(Clone)]
struct Fixture {
    id: &'static str,
    history_id: u64,
    labels: Vec<&'static str>,
    internal_date_ms: i64,
}

fn fixture(id: &'static str, labels: &[&'static str], internal_date_ms: i64) -> Fixture {
    Fixture {
        id,
        history_id: 1000,
        labels: labels.to_vec(),
        internal_date_ms,
    }
}

impl Fixture {
    fn with_history(mut self, history_id: u64) -> Self {
        self.history_id = history_id;
        self
    }

    fn get_body(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "historyId": self.history_id.to_string(),
            "messages": [{
                "id": format!("{}-m1", self.id),
                "threadId": self.id,
                "labelIds": self.labels,
                "internalDate": self.internal_date_ms.to_string(),
                "payload": { "headers": [{ "name": "Subject", "value": "hello" }] }
            }]
        })
    }
}

/// `Cull`: every thread older than 1d moves to Purgatory. A thread already in
/// Purgatory is a no-op; a recent one is left alone.
fn cull_config() -> Config {
    parse_config(
        r#"
auth:
  creds-path: /tmp/creds
state-filters:
  - Cull:
      ttl: 1d
      action: Purgatory
"#,
    )
    .expect("cull config parses")
}

/// A ladder that produces both writes Phase 2 can make: a bucket Move, a
/// catch-all Move, and a Delete out of Purgatory.
fn ladder_config() -> Config {
    parse_config(
        r#"
auth:
  creds-path: /tmp/creds
state-filters:
  - age-noise:
      label: llm/noise
      ttl: 1d
      action: Purgatory
  - Cull:
      ttl: 1d
      action: Purgatory
  - Purge:
      label: Purgatory
      ttl: 1d
      action: { Delete: "" }
"#,
    )
    .expect("ladder config parses")
}

/// Mount one run's mailbox: sanitization finds no conflicts, the active list
/// returns `listed` with their stamps, and each of `mailbox` answers
/// `threads.get`, `modify` and `trash`.
async fn mount_mailbox(server: &MockServer, listed: &[Fixture], mailbox: &[Fixture]) {
    Mock::given(method("GET"))
        .and(path(THREADS))
        .and(query_param_is_missing("q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    let refs: Vec<serde_json::Value> = listed
        .iter()
        .map(|f| json!({ "id": f.id, "historyId": f.history_id.to_string() }))
        .collect();
    Mock::given(method("GET"))
        .and(path(THREADS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "threads": refs })))
        .mount(server)
        .await;
    for f in mailbox {
        Mock::given(method("GET"))
            .and(path(format!("{THREADS}/{}", f.id)))
            .respond_with(ResponseTemplate::new(200).set_body_json(f.get_body()))
            .mount(server)
            .await;
        for verb in ["modify", "trash"] {
            Mock::given(method("POST"))
                .and(path(format!("{THREADS}/{}/{verb}", f.id)))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": f.id })))
                .mount(server)
                .await;
        }
    }
}

/// A fresh mock server holding `mailbox` (all of it listed) and a client for it.
async fn mailbox_run(mailbox: &[Fixture]) -> (MockServer, GmailClient) {
    let server = MockServer::start().await;
    mount_mailbox(&server, mailbox, mailbox).await;
    let client = client_with_labels(&server, LABELS).await;
    (server, client)
}

async fn run(
    client: &mut GmailClient,
    config: &Config,
    dry_run: bool,
    cache: &mut ThreadCache,
) -> RunSummary {
    engine::execute(client, config, "", dry_run, false, cache)
        .await
        .expect("run succeeds")
}

/// Every write Phase 2 sent (`modify` / `trash`), as `(path, body)`, sorted so
/// two runs compare regardless of request order.
async fn writes(server: &MockServer) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = server
        .received_requests()
        .await
        .expect("request recording is on by default")
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with(THREADS))
        .map(|r| {
            (
                r.url.path().to_string(),
                String::from_utf8_lossy(&r.body).into_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// A mailbox where nothing gets written under `cull_config`: recent inbox
/// threads (kept) and an old thread already in Purgatory (a no-op Move).
fn quiet_mailbox() -> Vec<Fixture> {
    vec![
        fixture("k1", &["INBOX"], recent_ms()),
        fixture("k2", &["INBOX", "UNREAD"], recent_ms()),
        fixture("k3", &["INBOX"], recent_ms()),
        fixture("p1", &[PURGATORY_ID], OLD_MS),
    ]
}

/// AC1 / criterion 1: two identical runs sharing one cache; run 2 makes 0 GET
/// `threads/{id}` requests.
#[tokio::test]
async fn second_identical_run_makes_no_thread_gets() {
    let mut cache = ThreadCache::default();
    let mailbox = quiet_mailbox();

    let (server1, mut client1) = mailbox_run(&mailbox).await;
    let first = run(&mut client1, &cull_config(), false, &mut cache).await;
    assert_eq!(
        thread_get_only_requests(&server1).await,
        4,
        "run 1 fetches all"
    );
    assert_eq!(cache.len(), 4, "every fetched thread is cached");
    assert_eq!(first.threads_transitioned, 0);

    let (server2, mut client2) = mailbox_run(&mailbox).await;
    let second = run(&mut client2, &cull_config(), false, &mut cache).await;
    assert_eq!(
        thread_get_only_requests(&server2).await,
        0,
        "run 2 evaluates every unchanged thread from the cache"
    );
    assert_eq!(second, first, "same decisions from the cache");
    assert_eq!(cache.len(), 4);
}

/// Criterion 2: one thread's `historyId` moves in the list; run 2 makes exactly
/// one GET, for that thread, and re-stamps its entry.
#[tokio::test]
async fn changed_history_id_fetches_exactly_that_thread() {
    let mut cache = ThreadCache::default();
    let mailbox = quiet_mailbox();

    let (_server1, mut client1) = mailbox_run(&mailbox).await;
    run(&mut client1, &cull_config(), false, &mut cache).await;

    let changed: Vec<Fixture> = mailbox
        .iter()
        .cloned()
        .map(|f| {
            if f.id == "k2" {
                f.with_history(1001)
            } else {
                f
            }
        })
        .collect();
    let (server2, mut client2) = mailbox_run(&changed).await;
    run(&mut client2, &cull_config(), false, &mut cache).await;

    assert_eq!(thread_get_only_requests(&server2).await, 1);
    assert_eq!(
        requests_to(&server2, "GET", &format!("{THREADS}/k2")).await,
        1
    );
    assert_eq!(cache.get("k2").map(|e| e.history_id), Some(1001));
}

/// Criterion 3: a thread Moved in run 1 is fetched again in run 2 even with an
/// unmoved stamp (the write drop is the backstop), and a thread absent from
/// run 2's list is gone from the cache.
#[tokio::test]
async fn written_thread_is_refetched_and_unlisted_thread_is_dropped() {
    let mut cache = ThreadCache::default();
    let run1 = vec![
        fixture("m1", &["INBOX"], OLD_MS),
        fixture("k1", &["INBOX"], recent_ms()),
        fixture("gone", &["INBOX"], recent_ms()),
    ];

    let (server1, mut client1) = mailbox_run(&run1).await;
    let first = run(&mut client1, &cull_config(), false, &mut cache).await;
    assert_eq!(first.threads_transitioned, 1);
    assert_eq!(
        requests_to(&server1, "POST", &format!("{THREADS}/m1/modify")).await,
        1
    );
    assert!(cache.get("m1").is_none(), "a real write drops the entry");
    assert!(cache.get("gone").is_some());

    // m1 now sits in Purgatory; its stamp is deliberately left at 1000.
    let run2 = vec![
        fixture("m1", &[PURGATORY_ID], OLD_MS),
        fixture("k1", &["INBOX"], recent_ms()),
    ];
    let (server2, mut client2) = mailbox_run(&run2).await;
    run(&mut client2, &cull_config(), false, &mut cache).await;

    assert_eq!(
        requests_to(&server2, "GET", &format!("{THREADS}/m1")).await,
        1,
        "the written thread is fetched again"
    );
    assert_eq!(thread_get_only_requests(&server2).await, 1, "k1 is a hit");
    assert!(cache.get("gone").is_none(), "unlisted thread is dropped");
    assert!(cache.get("m1").is_some(), "the no-op m1 is cached again");
    assert_eq!(cache.len(), 2);
}

/// Criterion 4: a dry run writes nothing, and the would-move thread stays
/// cached (Gmail is unchanged, so its facts are still true).
#[tokio::test]
async fn dry_run_keeps_the_would_move_thread_cached() {
    let mut cache = ThreadCache::default();
    let mailbox = vec![fixture("d1", &["INBOX"], OLD_MS)];

    let (server, mut client) = mailbox_run(&mailbox).await;
    let summary = run(&mut client, &cull_config(), true, &mut cache).await;

    assert_eq!(summary.threads_transitioned, 1, "d1 would move");
    assert!(writes(&server).await.is_empty(), "a dry run writes nothing");
    assert!(cache.get("d1").is_some(), "would-write keeps the entry");
}

/// Criterion 5: the same facts, config and clock produce identical writes
/// whether every thread is evaluated from the cache or freshly fetched. The
/// thread label order on the wire is deliberately not the cache's sorted order.
#[tokio::test]
async fn cached_and_fetched_evaluation_write_identically() {
    let mailbox = vec![
        fixture("a", &["INBOX"], OLD_MS),
        fixture("b", &["UNREAD", NOISE_ID, "INBOX"], OLD_MS),
        fixture("c", &[PURGATORY_ID], OLD_MS),
        fixture("d", &["INBOX"], recent_ms()),
    ];

    // Populate the cache without changing anything: a dry run's reads fill it
    // and its would-writes keep every entry.
    let mut cache = ThreadCache::default();
    let (_warm, mut warm_client) = mailbox_run(&mailbox).await;
    run(&mut warm_client, &ladder_config(), true, &mut cache).await;
    assert_eq!(cache.len(), 4);

    let (cached_server, mut cached_client) = mailbox_run(&mailbox).await;
    let cached = run(&mut cached_client, &ladder_config(), false, &mut cache).await;
    assert_eq!(
        thread_get_only_requests(&cached_server).await,
        0,
        "every thread evaluated from the cache"
    );

    let (fetched_server, mut fetched_client) = mailbox_run(&mailbox).await;
    let fetched = run(
        &mut fetched_client,
        &ladder_config(),
        false,
        &mut ThreadCache::default(),
    )
    .await;
    assert_eq!(
        thread_get_only_requests(&fetched_server).await,
        4,
        "every thread fetched"
    );

    let cached_writes = writes(&cached_server).await;
    let fetched_writes = writes(&fetched_server).await;
    assert_eq!(
        cached_writes
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/gmail/v1/users/me/threads/a/modify",
            "/gmail/v1/users/me/threads/b/modify",
            "/gmail/v1/users/me/threads/c/trash",
        ],
        "the ladder exercises both Moves and the Delete"
    );
    assert_eq!(cached_writes, fetched_writes);
    assert_eq!(cached, fetched);
}

/// Criterion 6: a drop made before an account-scoped error (429 exhaustion)
/// is absent from the file `execute_with_cache` saved on the error path.
#[tokio::test]
async fn drops_before_an_account_scoped_error_reach_the_saved_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_path = dir.path().join("eratosthenes").join("acct.json");
    let run1 = vec![
        fixture("a", &["INBOX"], recent_ms()),
        fixture("b", &["INBOX"], recent_ms()),
        fixture("c", &["INBOX"], recent_ms()),
    ];

    let (_server1, mut client1) = mailbox_run(&run1).await;
    execute_with_cache(&mut client1, &cull_config(), "", false, false, &cache_path)
        .await
        .expect("run 1 succeeds");
    assert_eq!(ThreadCache::load(&cache_path).len(), 3);

    // Run 2: `a` left the active set (dropped by the diff), `b` is a hit, and
    // `c` changed, so it is fetched, and its `threads.get` exhausts on 429.
    let server2 = MockServer::start().await;
    let listed = vec![run1[1].clone(), run1[2].clone().with_history(1001)];
    mount_mailbox(&server2, &listed, &[]).await;
    Mock::given(method("GET"))
        .and(path(format!("{THREADS}/c")))
        .respond_with(rate_limited())
        .mount(&server2)
        .await;
    let mut client2 = client_with_labels(&server2, LABELS).await;

    let clock = drive_clock_manually();
    let err = execute_with_cache(&mut client2, &cull_config(), "", false, false, &cache_path)
        .await
        .expect_err("a 429 is account-scoped and fails the run");
    clock.abort();
    assert!(format!("{err:#}").contains("threads.get(c)"), "{err:#}");

    let saved = ThreadCache::load(&cache_path);
    assert!(saved.get("a").is_none(), "the unlisted drop was saved");
    assert!(
        saved.get("c").is_none(),
        "the failed fetch's drop was saved"
    );
    assert!(saved.get("b").is_some(), "the hit is untouched");
    assert_eq!(saved.len(), 1);
}

/// A failed save is a WARN, never a run failure: the run's result stands.
#[tokio::test]
async fn failed_save_leaves_the_run_result_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"file").expect("write blocker");
    let cache_path = blocker.join("acct.json");

    let (_server, mut client) = mailbox_run(&quiet_mailbox()).await;
    let summary = execute_with_cache(&mut client, &cull_config(), "", false, false, &cache_path)
        .await
        .expect("a failed save must not fail the run");

    assert_eq!(summary.threads_transitioned, 0);
    assert!(!cache_path.exists());
}

/// `mark_only` returns before Phase 2: the cache is loaded and saved untouched.
#[tokio::test]
async fn mark_only_saves_the_cache_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_path = dir.path().join("acct.json");
    let (_server1, mut client1) = mailbox_run(&quiet_mailbox()).await;
    execute_with_cache(&mut client1, &cull_config(), "", false, false, &cache_path)
        .await
        .expect("seed run succeeds");
    let before = ThreadCache::load(&cache_path);
    assert_eq!(before.len(), 4);

    let (server2, mut client2) = mailbox_run(&[]).await;
    execute_with_cache(&mut client2, &cull_config(), "", false, true, &cache_path)
        .await
        .expect("mark-only run succeeds");

    assert_eq!(thread_get_only_requests(&server2).await, 0);
    assert_eq!(ThreadCache::load(&cache_path), before);
}
