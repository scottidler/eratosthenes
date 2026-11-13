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

/// Audit S1/MF4: the thread-level write carries the BUCKET ONLY. The seen
/// marker is message-level by contract, and `threads.modify` would put it on
/// every message the thread holds at call time -- including one that arrived
/// after the snapshot and was never classified, which `-label:llm/seen` then
/// hides forever.
#[test]
fn test_plan_write_adds_the_bucket_and_not_the_seen_marker() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX"]);
    let bucket = &config.buckets[0];

    let write = plan_write(
        &thread,
        bucket,
        &config.buckets,
        &resolver,
        SEEN_LABEL,
        ArchiveDecision::Stay,
    )
    .unwrap();
    assert_eq!(write.thread_id, "t1");
    assert_eq!(write.bucket, "needs-reply");
    assert_eq!(
        write.add,
        vec!["Label_1".to_string()],
        "the bucket label, and nothing else, goes through threads.modify"
    );
    let seen_id = resolver
        .resolve_name(SEEN_LABEL)
        .expect("the fixture resolves the marker");
    assert!(
        !write.add.iter().any(|id| id == seen_id),
        "the seen marker must not ride along on a thread-level write"
    );
    assert!(write.remove.is_empty());
}

/// The marker's target set: exactly the messages the classifier saw.
#[test]
fn test_plan_write_records_the_classified_message_ids() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX"]);
    let bucket = &config.buckets[0];

    let write = plan_write(
        &thread,
        bucket,
        &config.buckets,
        &resolver,
        SEEN_LABEL,
        ArchiveDecision::Stay,
    )
    .unwrap();
    assert_eq!(
        write.classified_message_ids,
        vec!["m1".to_string()],
        "the marker targets the snapshot's messages, not the thread"
    );
}

/// A missing marker label must fail the PLAN, not the trailing write: otherwise
/// the run mutates every bucket and only then discovers it cannot record what
/// it saw, leaving buckets written and nothing marked.
#[test]
fn test_plan_write_fails_when_the_seen_label_is_unresolvable() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX"]);
    let bucket = &config.buckets[0];

    let err = plan_write(
        &thread,
        bucket,
        &config.buckets,
        &resolver,
        "llm/never-created",
        ArchiveDecision::Stay,
    )
    .expect_err("an unresolvable marker must not plan a write");
    assert!(format!("{err:#}").contains("llm/never-created"), "{err:#}");
}

/// Reclassification: a noise thread a human replies into becomes needs-reply
/// and must not end up carrying both bucket labels.
#[test]
fn test_plan_write_removes_the_previous_bucket_label() {
    let config = triage_config();
    let resolver = resolver_with_llm_labels();
    let thread = thread_with_labels("t1", &["INBOX", "Label_2"]);
    let bucket = &config.buckets[0];

    let write = plan_write(
        &thread,
        bucket,
        &config.buckets,
        &resolver,
        SEEN_LABEL,
        ArchiveDecision::Stay,
    )
    .unwrap();
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
        ArchiveDecision::Stay,
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
        ArchiveDecision::Stay,
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
        ArchiveDecision::Stay,
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

    let assignments = plan_assignments(&threads, &classification, &config.buckets, ME);

    let wet = plan_writes(&assignments, &config.buckets, &resolver, false).unwrap();
    assert_eq!(wet.len(), 1, "precondition: a real run would write");

    let dry = plan_writes(&assignments, &config.buckets, &resolver, true).unwrap();
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

    let assignments = plan_assignments(&threads, &classification, &config.buckets, ME);
    let writes = plan_writes(&assignments, &config.buckets, &resolver, false).unwrap();
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

    let assignments = plan_assignments(&threads, &classification, &config.buckets, ME);
    let writes = plan_writes(&assignments, &config.buckets, &resolver, false).unwrap();
    assert!(writes.is_empty());
}

/// The bucket a thread is drafted under is the config's order, not Gmail's: a
/// thread carrying two `draft: true` bucket labels is handled once, by the
/// first bucket in config.
#[test]
fn test_plan_draft_targets_dedupes_a_thread_across_buckets() {
    let per_bucket = vec![
        (
            "needs-reply".to_string(),
            "Label_1".to_string(),
            vec!["t1".to_string(), "t2".to_string()],
        ),
        (
            "escalations".to_string(),
            "Label_9".to_string(),
            vec!["t2".to_string(), "t3".to_string()],
        ),
    ];

    let (targets, total) = plan_draft_targets(per_bucket, 10);
    let planned: Vec<(&str, &str)> = targets
        .iter()
        .map(|t| (t.thread_id.as_str(), t.bucket.as_str()))
        .collect();
    assert_eq!(
        planned,
        vec![
            ("t1", "needs-reply"),
            ("t2", "needs-reply"),
            ("t3", "escalations")
        ]
    );
    assert_eq!(total, 3);
}

/// The cap bounds the draft pass too: one `claude` call per draft, so an
/// unbounded needs-reply set would be an unbounded number of subprocesses.
#[test]
fn test_plan_draft_targets_caps_and_reports_the_total() {
    let per_bucket = vec![(
        "needs-reply".to_string(),
        "Label_1".to_string(),
        vec!["t1".to_string(), "t2".to_string(), "t3".to_string()],
    )];

    let (targets, total) = plan_draft_targets(per_bucket, 2);
    assert_eq!(targets.len(), 2);
    assert_eq!(total, 3, "the total is what makes the cap message honest");
}

#[test]
fn test_plan_draft_targets_on_an_empty_mailbox_plans_nothing() {
    let (targets, total) = plan_draft_targets(Vec::new(), 50);
    assert!(targets.is_empty());
    assert_eq!(total, 0);
}

#[test]
fn test_summary_line_reports_skips_when_nothing_was_classified() {
    let skipped_only = Tally {
        skipped: 1,
        ..Tally::default()
    };
    assert_eq!(
        summary_line("[a] ", &skipped_only, false),
        "[a] Triage: 0 threads classified, 0 labeled, 0 archived, 0 protected, 1 skipped"
    );
}

#[test]
fn test_summary_line_reports_archived_and_protected_counts() {
    let tally = Tally {
        classified: 76,
        labeled: 76,
        archived: 61,
        protected: 4,
        skipped: 0,
    };
    assert_eq!(
        summary_line("", &tally, false),
        "Triage: 76 threads classified, 76 labeled, 61 archived, 4 protected, 0 skipped"
    );
    assert_eq!(
        summary_line("", &tally, true),
        "Triage: 76 threads classified, 76 labeled, 61 archived, 4 protected, 0 skipped (dry run)"
    );
}

// ---- plan_archive: one test per row of the archive decision table ----------

const ME: &str = "scott.idler@tatari.tv";

fn bucket(archive: bool, awaits_reply: bool) -> TriageBucket {
    TriageBucket {
        name: "b".to_string(),
        label: "llm/b".to_string(),
        description: "d".to_string(),
        draft: false,
        archive,
        awaits_reply,
    }
}

/// One fixture message: id, headers, label ids. Dated in list order.
type FixtureMessage<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a [&'a str]);

fn archive_thread(messages: Vec<FixtureMessage>) -> TriageThread {
    TriageThread::from_api(google_gmail1::api::Thread {
        id: Some("t1".to_string()),
        messages: Some(
            messages
                .into_iter()
                .enumerate()
                .map(|(i, (id, headers, labels))| {
                    api_message(id, "t1", (i as i64 + 1) * 1_000, headers, "b", labels)
                })
                .collect(),
        ),
        ..Default::default()
    })
    .expect("test thread parses")
}

const BOT_TO_LIST: &[(&str, &str)] = &[
    ("From", "Greenhouse <no-reply@greenhouse.io>"),
    ("To", "eng@tatari.tv"),
    ("List-Id", "<eng.tatari.tv>"),
];

#[test]
fn plan_archive_row_1() {
    let thread = archive_thread(vec![("m1", BOT_TO_LIST.to_vec(), &["INBOX", "STARRED"])]);
    assert_eq!(
        plan_archive(&thread, &bucket(true, false), ME),
        ArchiveDecision::StayProtected(Protection::Pinned)
    );
}

#[test]
fn plan_archive_row_2() {
    let thread = archive_thread(vec![(
        "m1",
        vec![("From", ME), ("To", "bob@x.com")],
        &["INBOX", "SENT"],
    )]);
    assert_eq!(
        plan_archive(&thread, &bucket(true, false), ME),
        ArchiveDecision::StayProtected(Protection::NoInbound)
    );
}

/// The model filed a human mail to me alone as noise. Code keeps it.
#[test]
fn plan_archive_row_3() {
    let thread = archive_thread(vec![(
        "m1",
        vec![
            ("From", "Nick Andolina <nick.andolina@tatari.tv>"),
            ("To", ME),
        ],
        &["INBOX"],
    )]);
    assert_eq!(
        plan_archive(&thread, &bucket(true, false), ME),
        ArchiveDecision::StayProtected(Protection::SoleHuman)
    );
}

/// A bot in an `awaits-reply` bucket is archived even though the bucket has
/// `archive: false`: row 4 is ahead of row 5.
#[test]
fn plan_archive_row_4() {
    let thread = archive_thread(vec![(
        "m1",
        vec![("From", "Greenhouse <no-reply@greenhouse.io>"), ("To", ME)],
        &["INBOX"],
    )]);
    assert_eq!(
        plan_archive(&thread, &bucket(false, true), ME),
        ArchiveDecision::ArchiveAutomated
    );
}

/// A human with a Cc in an `awaits-reply`, `archive: false` bucket: row 3 does
/// not hold it (not solely to me), row 4 does not take it (not automated).
#[test]
fn plan_archive_row_5() {
    let thread = archive_thread(vec![(
        "m1",
        vec![
            ("From", "Nick Andolina <nick.andolina@tatari.tv>"),
            ("To", ME),
            ("Cc", "boss@tatari.tv"),
        ],
        &["INBOX"],
    )]);
    assert_eq!(
        plan_archive(&thread, &bucket(false, true), ME),
        ArchiveDecision::Stay
    );
}

#[test]
fn plan_archive_row_6() {
    let thread = archive_thread(vec![
        ("m1", vec![("From", ME), ("To", "eng@tatari.tv")], &["SENT"]),
        ("m2", BOT_TO_LIST.to_vec(), &["INBOX", "UNREAD"]),
    ]);
    assert_eq!(
        plan_archive(&thread, &bucket(true, false), ME),
        ArchiveDecision::Archive
    );
}

// ---- the archive write path (Phase 4) ---------------------------------------

#[test]
fn archive_decision_column_names_every_variant() {
    let columns: Vec<String> = [
        ArchiveDecision::Archive,
        ArchiveDecision::ArchiveAutomated,
        ArchiveDecision::Stay,
        ArchiveDecision::StayProtected(Protection::Pinned),
        ArchiveDecision::StayProtected(Protection::NoInbound),
        ArchiveDecision::StayProtected(Protection::SoleHuman),
    ]
    .iter()
    .map(|d| d.to_string())
    .collect();
    assert_eq!(
        columns,
        vec![
            "archive",
            "archive (automated)",
            "stay",
            "stay (pinned)",
            "stay (no-inbound)",
            "stay (sole-human)",
        ]
    );
}

#[test]
fn archive_decision_column_pads_for_alignment() {
    assert_eq!(format!("{:<9}|", ArchiveDecision::Stay), "stay     |");
}

#[test]
fn only_archive_variants_archive() {
    assert!(ArchiveDecision::Archive.archives());
    assert!(ArchiveDecision::ArchiveAutomated.archives());
    assert!(!ArchiveDecision::Stay.archives());
    assert!(!ArchiveDecision::StayProtected(Protection::SoleHuman).archives());
}

fn landed_write(thread_id: &str, ids: &[&str], archive: ArchiveDecision) -> ThreadWrite {
    ThreadWrite {
        thread_id: thread_id.to_string(),
        bucket: "noise".to_string(),
        add: vec!["Label_2".to_string()],
        remove: Vec::new(),
        classified_message_ids: ids.iter().map(|s| s.to_string()).collect(),
        archive,
    }
}

#[test]
fn plan_seen_batches_splits_classified_ids_by_the_archive_decision() {
    let writes = [
        landed_write("t1", &["t1-m1", "t1-m2"], ArchiveDecision::Archive),
        landed_write(
            "t2",
            &["t2-m1"],
            ArchiveDecision::StayProtected(Protection::SoleHuman),
        ),
        landed_write("t3", &["t3-m1"], ArchiveDecision::ArchiveAutomated),
        landed_write("t4", &["t4-m1"], ArchiveDecision::Stay),
    ];
    assert_eq!(
        plan_seen_batches(writes.iter()),
        SeenBatches {
            stay: vec!["t2-m1".to_string(), "t4-m1".to_string()],
            archive: vec![
                "t1-m1".to_string(),
                "t1-m2".to_string(),
                "t3-m1".to_string()
            ],
        }
    );
}

#[test]
fn plan_seen_batches_of_nothing_is_empty() {
    assert_eq!(plan_seen_batches([].iter()), SeenBatches::default());
}

#[test]
fn candidates_before_drops_a_message_dated_at_or_after_the_cutoff() {
    let cutoff = DateTime::from_timestamp_millis(5_000).unwrap();
    let kept = candidates_before(
        vec![
            candidate("old", 4_999),
            candidate("at", 5_000),
            candidate("after", 9_000),
        ],
        cutoff,
    );
    assert_eq!(kept, vec![candidate("old", 4_999)]);
}

#[test]
fn drop_messages_from_trims_a_reply_that_landed_during_the_pass() {
    let mut thread = archive_thread(vec![
        ("m1", vec![("From", "bob@x.com")], &["INBOX"]),
        ("m2", vec![("From", "bob@x.com")], &["INBOX"]),
    ]);
    // archive_thread dates m1 at 1000 ms and m2 at 2000 ms.
    let dropped = drop_messages_from(&mut thread, DateTime::from_timestamp_millis(2_000).unwrap());
    assert_eq!(dropped, 1);
    let ids: Vec<&str> = thread.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["m1"]);
}

#[test]
fn drop_messages_from_keeps_a_thread_that_predates_the_pass() {
    let mut thread = archive_thread(vec![("m1", vec![("From", "bob@x.com")], &["INBOX"])]);
    assert_eq!(
        drop_messages_from(
            &mut thread,
            DateTime::from_timestamp_millis(10_000).unwrap()
        ),
        0
    );
    assert_eq!(thread.messages.len(), 1);
}

#[test]
fn plan_assignments_decides_archive_once_per_assigned_thread() {
    let yaml = r#"
schedule: "Mon..Fri 06:30:00"
buckets:
  - name: noise
    label: llm/noise
    description: everything else
    archive: true
"#;
    let config: TriageConfig = serde_yaml::from_str(yaml).unwrap();
    let bot = archive_thread(vec![("m1", BOT_TO_LIST.to_vec(), &["INBOX"])]);
    let classification = Classification {
        assignments: vec![
            ("t1".to_string(), "noise".to_string()),
            ("ghost".to_string(), "noise".to_string()),
        ],
        ..Default::default()
    };
    let threads = [bot];
    let assignments = plan_assignments(&threads, &classification, &config.buckets, ME);
    assert_eq!(
        assignments.len(),
        1,
        "an unfetched thread gets no assignment"
    );
    assert_eq!(assignments[0].thread.id, "t1");
    assert_eq!(assignments[0].archive, ArchiveDecision::Archive);
}

#[test]
fn plan_writes_carries_the_archive_decision_but_never_removes_inbox() {
    let yaml = r#"
schedule: "Mon..Fri 06:30:00"
buckets:
  - name: noise
    label: llm/noise
    description: everything else
    archive: true
"#;
    let config: TriageConfig = serde_yaml::from_str(yaml).unwrap();
    let resolver = resolver_with_llm_labels();
    let threads = [archive_thread(vec![(
        "m1",
        BOT_TO_LIST.to_vec(),
        &["INBOX"],
    )])];
    let classification = Classification {
        assignments: vec![("t1".to_string(), "noise".to_string())],
        ..Default::default()
    };
    let assignments = plan_assignments(&threads, &classification, &config.buckets, ME);
    let writes = plan_writes(&assignments, &config.buckets, &resolver, false).unwrap();
    assert_eq!(writes[0].archive, ArchiveDecision::Archive);
    assert!(
        !writes[0].remove.iter().any(|id| id == INBOX_LABEL),
        "INBOX removal rides the seen batch, never the thread-level write"
    );
}
