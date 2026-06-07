#![allow(clippy::unwrap_used)]

use super::*;
use std::collections::HashMap;

use crate::gmail::message::{GmailMessage, GmailThread};

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis).unwrap()
}

fn msg(id: &str, thread_id: &str, from_header: &str, subject: &str, millis: i64) -> GmailMessage {
    let mut headers = HashMap::new();
    headers.insert("From".to_string(), from_header.to_string());
    headers.insert("Subject".to_string(), subject.to_string());
    GmailMessage {
        id: id.to_string(),
        thread_id: thread_id.to_string(),
        label_ids: vec![],
        internal_date: ts(millis),
        headers,
        to: vec![],
        cc: vec![],
        from: vec![extract_email(from_header)],
        subject: subject.to_string(),
    }
}

fn extract_email(from_header: &str) -> String {
    if let Some(lt) = from_header.find('<')
        && let Some(gt) = from_header.find('>')
    {
        return from_header[lt + 1..gt].to_lowercase();
    }
    from_header.to_lowercase()
}

fn thread(id: &str, messages: Vec<GmailMessage>) -> GmailThread {
    GmailThread {
        id: id.to_string(),
        messages,
    }
}

fn ids(values: &[&str]) -> HashSet<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_build_starred_wins_when_both() {
    let t = thread("t1", vec![msg("m1", "t1", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&["t1"]));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].pin, Pin::Starred);
}

#[test]
fn test_build_one_item_per_thread_even_with_many_messages() {
    // A thread with several (potentially starred) messages must yield ONE line.
    let t = thread(
        "t1",
        vec![
            msg("m1", "t1", "A <a@x.com>", "first", 1_000),
            msg("m2", "t1", "B <b@x.com>", "second", 2_000),
            msg("m3", "t1", "C <c@x.com>", "latest", 3_000),
        ],
    );
    let items = build(&[t], &ids(&["t1"]), &ids(&[]));
    assert_eq!(items.len(), 1);
    // Sender/subject/date come from the LATEST message.
    assert_eq!(items[0].sender, "C");
    assert_eq!(items[0].subject, "latest");
    assert_eq!(items[0].date, ts(3_000));
}

#[test]
fn test_build_skips_unpinned_threads() {
    let t = thread("t9", vec![msg("m1", "t9", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&[]), &ids(&[]));
    assert!(items.is_empty());
}

#[test]
fn test_build_important_when_only_important() {
    let t = thread("t2", vec![msg("m1", "t2", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&[]), &ids(&["t2"]));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].pin, Pin::Important);
}

#[test]
fn test_build_sender_falls_back_to_email_without_display_name() {
    let t = thread("t1", vec![msg("m1", "t1", "bare@x.com", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&[]));
    assert_eq!(items[0].sender, "bare@x.com");
}

#[test]
fn test_format_empty_set_is_positive_with_signature() {
    let out = format(&[], 0);
    assert!(out.contains("Inbox clear - 0 starred, 0 important"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

#[test]
fn test_format_basic_shape() {
    let items = vec![
        DigestItem {
            pin: Pin::Starred,
            date: ts(1_717_459_200_000), // 2024-06-04
            sender: "Mark Weiler".to_string(),
            subject: "pentest proposal".to_string(),
            thread_id: "THREADA".to_string(),
        },
        DigestItem {
            pin: Pin::Important,
            date: ts(1_717_286_400_000), // 2024-06-02
            sender: "JP Ciceri".to_string(),
            subject: "annual attestation".to_string(),
            thread_id: "THREADB".to_string(),
        },
    ];
    let out = format(&items, 0);

    assert!(out.contains("*Pinned inbox digest* - 1 starred, 1 important"));
    assert!(out.contains("*:star: Starred (1)*"));
    assert!(out.contains("*:exclamation: Important (1)*"));
    assert!(out.contains("*Mark Weiler*"));
    assert!(out.contains("https://mail.google.com/mail/u/0/#all/THREADA|pentest proposal"));
    assert!(out.contains("https://mail.google.com/mail/u/0/#all/THREADB|annual attestation"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

#[test]
fn test_format_signature_always_last_line() {
    let items = vec![DigestItem {
        pin: Pin::Starred,
        date: ts(1_000),
        sender: "A".to_string(),
        subject: "s".to_string(),
        thread_id: "T".to_string(),
    }];
    let out = format(&items, 0);
    let last = out.lines().last().unwrap();
    assert_eq!(last, SIGNATURE);
}

#[test]
fn test_format_browser_index_in_deep_link() {
    let items = vec![DigestItem {
        pin: Pin::Starred,
        date: ts(1_000),
        sender: "A".to_string(),
        subject: "s".to_string(),
        thread_id: "T".to_string(),
    }];
    let out = format(&items, 3);
    assert!(out.contains("https://mail.google.com/mail/u/3/#all/T|s"));
}

#[test]
fn test_format_escapes_mrkdwn_specials() {
    let items = vec![DigestItem {
        pin: Pin::Starred,
        date: ts(1_000),
        sender: "Foo & Bar".to_string(),
        subject: "a < b > c & d".to_string(),
        thread_id: "T".to_string(),
    }];
    let out = format(&items, 0);
    assert!(out.contains("Foo &amp; Bar"));
    assert!(out.contains("a &lt; b &gt; c &amp; d"));
    // Raw specials must not survive in the rendered line.
    assert!(!out.contains("a < b"));
}

#[test]
fn test_format_empty_subject_fallback() {
    let items = vec![DigestItem {
        pin: Pin::Starred,
        date: ts(1_000),
        sender: "A".to_string(),
        subject: "   ".to_string(),
        thread_id: "T".to_string(),
    }];
    let out = format(&items, 0);
    assert!(out.contains("|(no subject)>"));
}

#[test]
fn test_format_truncates_over_budget_keeping_exact_counts() {
    let mut items = Vec::new();
    for i in 0..100 {
        items.push(DigestItem {
            pin: Pin::Starred,
            date: ts(1_000 + i as i64),
            sender: format!("Sender Number {}", i),
            subject: format!("A reasonably long subject line number {} to burn characters", i),
            thread_id: format!("THREAD{:04}", i),
        });
    }
    let out = format(&items, 0);

    assert!(out.len() <= BUDGET, "body must fit budget, got {}", out.len());
    // Header count stays exact even though items are truncated.
    assert!(out.contains("*Pinned inbox digest* - 100 starred, 0 important"));
    assert!(out.contains("*:star: Starred (100)*"));
    assert!(out.contains("... +"), "truncated digest must show a '... +N more' line");
    assert!(out.trim_end().ends_with(SIGNATURE));
}

#[test]
fn test_format_sorts_each_section_newest_first() {
    let items = vec![
        DigestItem {
            pin: Pin::Starred,
            date: ts(1_000),
            sender: "Old".to_string(),
            subject: "old".to_string(),
            thread_id: "OLD".to_string(),
        },
        DigestItem {
            pin: Pin::Starred,
            date: ts(9_000),
            sender: "New".to_string(),
            subject: "new".to_string(),
            thread_id: "NEW".to_string(),
        },
    ];
    let out = format(&items, 0);
    let new_pos = out.find("NEW").unwrap();
    let old_pos = out.find("OLD").unwrap();
    assert!(new_pos < old_pos, "newest item should appear first");
}
