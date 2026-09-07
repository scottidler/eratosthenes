use super::*;
use crate::triage::classify::Classification;
use crate::triage::thread::tests::api_message;

fn triage_config() -> TriageConfig {
    let yaml = r#"
schedule: "Mon..Fri 06:30:00"
max-threads: 3
buckets:
  - name: needs-reply
    label: llm/needs-reply
    description: a real human expects a reply
    draft: true
  - name: noise
    label: llm/noise
    description: everything else
"#;
    serde_yaml::from_str(yaml).expect("test triage config parses")
}

/// A resolver that already knows the `llm/*` labels, i.e. a mailbox where a
/// previous run (or `ensure_triage_labels`) has created them.
fn resolver_with_llm_labels() -> LabelResolver {
    let labels = ["llm/needs-reply", "llm/noise", SEEN_LABEL]
        .iter()
        .enumerate()
        .map(|(i, name)| google_gmail1::api::Label {
            id: Some(format!("Label_{}", i + 1)),
            name: Some(name.to_string()),
            ..Default::default()
        })
        .collect();
    LabelResolver::from_api_labels(labels)
}

fn thread_with_labels(id: &str, labels: &[&str]) -> TriageThread {
    TriageThread::from_api(google_gmail1::api::Thread {
        id: Some(id.to_string()),
        messages: Some(vec![api_message(
            "m1",
            id,
            1_000,
            vec![("Subject", "a subject")],
            "body",
            labels,
        )]),
        ..Default::default()
    })
    .expect("test thread parses")
}

fn candidate(thread_id: &str, millis: i64) -> CandidateMessage {
    CandidateMessage {
        thread_id: thread_id.to_string(),
        internal_date: DateTime::from_timestamp_millis(millis).expect("valid timestamp"),
    }
}

/// The query is the whole idempotency story: a thread with no new messages
/// matches nothing, so a rerun is a no-op.
#[test]
fn test_candidate_query_excludes_the_seen_marker() {
    assert_eq!(CANDIDATE_QUERY, "in:inbox -label:llm/seen");
    assert!(CANDIDATE_QUERY.contains(&format!("-label:{}", SEEN_LABEL)));
}

#[test]
fn test_select_candidates_collapses_messages_into_distinct_threads() {
    let candidates = vec![
        candidate("t1", 1_000),
        candidate("t1", 5_000),
        candidate("t2", 2_000),
    ];
    let selection = select_candidates(&candidates, 50);
    assert_eq!(selection.total_threads, 2);
    assert_eq!(selection.thread_ids, vec!["t1", "t2"]);
}

/// A thread ranks on its NEWEST unseen message, so a stale thread that just
/// got a reply beats a thread whose only unseen message is older.
#[test]
fn test_select_candidates_sorts_newest_first() {
    let candidates = vec![
        candidate("old", 1_000),
        candidate("newest", 9_000),
        candidate("middle", 5_000),
    ];
    let selection = select_candidates(&candidates, 50);
    assert_eq!(selection.thread_ids, vec!["newest", "middle", "old"]);
}

#[test]
fn test_select_candidates_caps_at_max_threads_keeping_the_newest() {
    let candidates = vec![
        candidate("t1", 1_000),
        candidate("t2", 2_000),
        candidate("t3", 3_000),
        candidate("t4", 4_000),
    ];
    let selection = select_candidates(&candidates, 2);
    assert_eq!(selection.thread_ids, vec!["t4", "t3"]);
    assert_eq!(selection.total_threads, 4);
    assert_eq!(selection.dropped(), 2);
}

#[test]
fn test_select_candidates_empty_input() {
    let selection = select_candidates(&[], 50);
    assert!(selection.thread_ids.is_empty());
    assert_eq!(selection.dropped(), 0);
}

/// The cap never truncates silently: when it bites, the line carries the
/// numbers that justify raising `max-threads`.
#[test]
fn test_cap_message_is_loud_and_carries_the_numbers() {
    let selection = Selection {
        thread_ids: vec!["a".to_string(), "b".to_string()],
        total_threads: 57,
    };
    let message = cap_message(&selection, 2).expect("a bitten cap must report");
    assert!(message.contains("max-threads cap HIT"), "{}", message);
    assert!(message.contains("57"), "{}", message);
    assert!(message.contains("2"), "{}", message);
    assert!(message.contains("55"), "{}", message);
}

#[test]
fn test_cap_message_is_silent_when_the_cap_did_not_bite() {
    let selection = Selection {
        thread_ids: vec!["a".to_string()],
        total_threads: 1,
    };
    assert_eq!(cap_message(&selection, 50), None);
}

#[test]
fn test_plan_write_adds_the_bucket_and_the_seen_marker() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX"]);
    let bucket = &config.buckets[0];

    let write = plan_write(&thread, bucket, &config.buckets, &resolver, SEEN_LABEL).unwrap();
    assert_eq!(write.thread_id, "t1");
    assert_eq!(write.bucket, "needs-reply");
    assert_eq!(
        write.add,
        vec!["Label_1".to_string(), "Label_3".to_string()],
        "bucket label then the seen marker"
    );
    assert!(write.remove.is_empty());
}

/// Reclassification: a noise thread a human replies into becomes needs-reply
/// and must not end up carrying both bucket labels.
#[test]
fn test_plan_write_removes_the_previous_bucket_label() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX", "Label_2"]);
    let bucket = &config.buckets[0];

    let write = plan_write(&thread, bucket, &config.buckets, &resolver, SEEN_LABEL).unwrap();
    assert_eq!(write.remove, vec!["Label_2".to_string()]);
}

/// Only bucket labels are ever removed. INBOX, stage labels and the user's own
/// labels are none of triage's business.
#[test]
fn test_plan_write_leaves_non_bucket_labels_alone() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX", "STARRED", "Purgatory"]);

    let write = plan_write(
        &thread,
        &config.buckets[1],
        &config.buckets,
        &resolver,
        SEEN_LABEL,
    )
    .unwrap();
    assert!(write.remove.is_empty(), "remove={:?}", write.remove);
}

/// Re-labeling a thread with the bucket it already has does not remove it: the
/// same-bucket case is an add-only write, not a remove-then-add flicker.
#[test]
fn test_plan_write_does_not_remove_the_bucket_it_is_applying() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX", "Label_2"]);

    let write = plan_write(
        &thread,
        &config.buckets[1],
        &config.buckets,
        &resolver,
        SEEN_LABEL,
    )
    .unwrap();
    assert!(write.remove.is_empty());
    assert!(write.add.contains(&"Label_2".to_string()));
}

/// Fail loudly rather than sending a label NAME where Gmail wants an ID, which
/// is a 400 from the API instead of a readable error here.
#[test]
fn test_plan_write_fails_loudly_when_a_label_is_unresolved() {
    let config = triage_config();
    let resolver = LabelResolver::from_api_labels(vec![]);
    let thread = thread_with_labels("t1", &["INBOX"]);

    let err = plan_write(
        &thread,
        &config.buckets[0],
        &config.buckets,
        &resolver,
        SEEN_LABEL,
    )
    .expect_err("an unresolved label must fail");
    assert!(
        format!("{:#}", err).contains("llm/needs-reply"),
        "{:#}",
        err
    );
}

#[test]
fn test_plan_labels_lists_every_missing_label_with_its_visibility() {
    let config = triage_config();
    let resolver = LabelResolver::from_api_labels(vec![]);

    let plan = plan_labels(&config, &resolver, false);
    assert_eq!(
        plan,
        vec![
            ("llm/needs-reply".to_string(), LabelVisibility::Shown),
            ("llm/noise".to_string(), LabelVisibility::Shown),
            (SEEN_LABEL.to_string(), LabelVisibility::Hidden),
        ]
    );
}

#[test]
fn test_plan_labels_is_empty_when_every_label_exists() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    assert!(plan_labels(&config, &resolver, false).is_empty());
}

/// DRY RUN, MUTATION 1 OF 2: `labels.create`. Stricter than `run --dry-run`,
/// which does create missing labels: a triage dry run is the eval gate's
/// instrument and must leave NOTHING behind.
#[test]
fn test_dry_run_creates_no_labels_even_when_all_are_missing() {
    let config = triage_config();
    let resolver = LabelResolver::from_api_labels(vec![]);

    assert!(
        !plan_labels(&config, &resolver, false).is_empty(),
        "precondition: every label is missing, so a real run would create some"
    );
    assert!(
        plan_labels(&config, &resolver, true).is_empty(),
        "a dry run must plan zero label creations"
    );
}

/// DRY RUN, MUTATION 2 OF 2: `threads.modify`.
#[test]
fn test_dry_run_plans_zero_thread_writes() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let threads = vec![thread_with_labels("t1", &["INBOX"])];
    let classification = Classification {
        assignments: vec![("t1".to_string(), "needs-reply".to_string())],
        ..Default::default()
    };

    let wet = plan_writes(&threads, &classification, &config, &resolver, false).unwrap();
    assert_eq!(wet.len(), 1, "precondition: a real run would write");

    let dry = plan_writes(&threads, &classification, &config, &resolver, true).unwrap();
    assert!(dry.is_empty(), "a dry run must plan zero thread writes");
}

#[test]
fn test_plan_writes_covers_every_assignment() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let threads = vec![
        thread_with_labels("t1", &["INBOX"]),
        thread_with_labels("t2", &["INBOX"]),
    ];
    let classification = Classification {
        assignments: vec![
            ("t1".to_string(), "needs-reply".to_string()),
            ("t2".to_string(), "noise".to_string()),
        ],
        ..Default::default()
    };

    let writes = plan_writes(&threads, &classification, &config, &resolver, false).unwrap();
    let planned: Vec<(&str, &str)> = writes
        .iter()
        .map(|w| (w.thread_id.as_str(), w.bucket.as_str()))
        .collect();
    assert_eq!(planned, vec![("t1", "needs-reply"), ("t2", "noise")]);
}

/// A skipped thread is skipped end to end: no write, so it stays unseen and is
/// retried on the next run.
#[test]
fn test_plan_writes_skips_an_assignment_whose_thread_was_not_fetched() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let threads = vec![thread_with_labels("t1", &["INBOX"])];
    let classification = Classification {
        assignments: vec![("ghost".to_string(), "noise".to_string())],
        ..Default::default()
    };

    let writes = plan_writes(&threads, &classification, &config, &resolver, false).unwrap();
    assert!(writes.is_empty());
}
