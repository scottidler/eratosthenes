//! `eratosthenes senders` against wiremock: read-only, and the ranking holds
//! over real metadata fetches.

mod common;

use common::client_with_labels;
use eratosthenes::cfg::config::parse_config;
use eratosthenes::senders;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MESSAGES: &str = "/gmail/v1/users/me/messages";

fn message(id: &str, thread: &str, labels: &[&str], headers: &[(&str, &str)]) -> Value {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    json!({
        "id": id,
        "threadId": thread,
        "labelIds": labels,
        "internalDate": "1700000000000",
        "payload": { "headers": headers }
    })
}

#[tokio::test]
async fn senders_is_read_only_and_ranks_the_fixture() {
    let server = MockServer::start().await;
    let mailbox = [
        message(
            "m1",
            "t1",
            &["INBOX", "UNREAD", "Label_N"],
            &[
                ("From", "Greenhouse <no-reply@greenhouse.io>"),
                ("To", "me@x.com"),
            ],
        ),
        message(
            "m2",
            "t2",
            &["INBOX", "UNREAD", "Label_N"],
            &[
                ("From", "Greenhouse <no-reply@greenhouse.io>"),
                ("To", "me@x.com"),
            ],
        ),
        message(
            "m3",
            "t3",
            &["INBOX"],
            &[
                ("From", "Greenhouse <no-reply@greenhouse.io>"),
                ("To", "me@x.com"),
            ],
        ),
        message(
            "m4",
            "t4",
            &["INBOX", "UNREAD"],
            &[("From", "Nick <Nick@x.com>"), ("To", "me@x.com")],
        ),
        message(
            "m5",
            "t4",
            &["INBOX"],
            &[("From", "Nick <nick@x.com>"), ("To", "me@x.com")],
        ),
        message(
            "m6",
            "t6",
            &["INBOX"],
            &[("From", "dev@x.com"), ("List-Id", "<eng.x.com>")],
        ),
    ];

    let refs: Vec<Value> = mailbox
        .iter()
        .map(|m| json!({ "id": m["id"], "threadId": m["threadId"] }))
        .collect();
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("q", "in:inbox newer_than:7d"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "messages": refs })))
        .mount(&server)
        .await;
    for m in &mailbox {
        let id = m["id"].as_str().expect("id");
        Mock::given(method("GET"))
            .and(path(format!("{MESSAGES}/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(m))
            .mount(&server)
            .await;
    }

    let config = parse_config(
        r#"
auth:
  creds-path: /tmp/creds
triage:
  schedule: "*-*-* *:30:00"
  buckets:
    - name: noise
      label: llm/noise
      description: everything else
"#,
    )
    .expect("config parses");
    let mut client = client_with_labels(&server, &[("Label_N", "llm/noise")]).await;

    let rows = senders::collect(&mut client, &config, 7, 2)
        .await
        .expect("collect");

    let order: Vec<&str> = rows.iter().map(|r| r.address.as_str()).collect();
    assert_eq!(order, vec!["no-reply@greenhouse.io", "nick@x.com"]);
    let greenhouse = &rows[0];
    assert_eq!(
        (greenhouse.messages, greenhouse.threads, greenhouse.unread),
        (3, 3, 2)
    );
    assert!(greenhouse.automated);
    assert_eq!(greenhouse.buckets.get("noise"), Some(&2));
    let nick = &rows[1];
    assert_eq!((nick.messages, nick.threads, nick.unread), (2, 1, 1));
    assert!(!nick.automated, "no list headers, a human address");
    assert!(nick.buckets.is_empty());

    let requests = server.received_requests().await.expect("recording is on");
    let writes: Vec<String> = requests
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(writes.is_empty(), "senders must be read-only: {writes:?}");
}
