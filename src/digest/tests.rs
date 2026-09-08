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

/// A bare digest item: what `build` produces before any bullet pass runs.
fn item(pin: Pin, millis: i64, sender: &str, subject: &str, thread_id: &str) -> DigestItem {
    DigestItem {
        pin,
        date: ts(millis),
        sender: sender.to_string(),
        subject: subject.to_string(),
        thread_id: thread_id.to_string(),
        ask: None,
        bullets: Vec::new(),
    }
}

#[test]
fn test_build_starred_wins_when_both() {
    let t = thread("t1", vec![msg("m1", "t1", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&["t1"]));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].pin, Pin::Starred);
}

/// A thread appears exactly ONCE even when both pins apply, and Starred wins.
#[test]
fn test_build_double_pinned_thread_yields_one_item() {
    let t = thread("t1", vec![msg("m1", "t1", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&["t1"]));
    assert_eq!(items.len(), 1, "a thread must produce exactly one item");
    assert_eq!(items[0].pin, Pin::Starred);
}

/// The rendered digest must place a triple-pinned thread in one section only.
#[test]
fn test_format_places_a_thread_in_exactly_one_section() {
    let t = thread("T1", vec![msg("m1", "T1", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["T1"]), &ids(&["T1"]));
    let out = format(&items, 0, None);
    assert_eq!(
        out.matches("#all/T1|").count(),
        1,
        "one deep link, one section:\n{}",
        out
    );
    assert!(out.contains("*:star: Starred (1)*"));
    assert!(!out.contains("*:exclamation: Important"));
    assert!(
        !out.contains("Needs Reply"),
        "the machine-chosen section is gone for good:\n{}",
        out
    );
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
    assert_eq!(items[0].pin, Pin::Important);
}

#[test]
fn test_build_leaves_ask_and_bullets_empty() {
    let t = thread("t1", vec![msg("m1", "t1", "A <a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&[]));
    assert_eq!(items[0].ask, None);
    assert!(items[0].bullets.is_empty());
}

#[test]
fn test_build_sender_falls_back_to_email_without_display_name() {
    let t = thread("t1", vec![msg("m1", "t1", "<a@x.com>", "subj", 1_000)]);
    let items = build(&[t], &ids(&["t1"]), &ids(&[]));
    assert_eq!(items[0].sender, "a@x.com");
}

#[test]
fn test_attach_bullets_folds_the_pass_in_by_thread_id() {
    let mut items = vec![
        item(Pin::Starred, 1_000, "A", "s", "T1"),
        item(Pin::Important, 2_000, "B", "t", "T2"),
    ];
    let mut by_thread = HashMap::new();
    by_thread.insert(
        "T1".to_string(),
        ThreadBullets {
            ask: Some("send the letter".to_string()),
            bullets: vec!["one".to_string(), "two".to_string()],
        },
    );
    attach_bullets(&mut items, &by_thread);

    assert_eq!(items[0].ask.as_deref(), Some("send the letter"));
    assert_eq!(items[0].bullets, vec!["one", "two"]);
    // A thread the model said nothing about stays bare rather than failing.
    assert_eq!(items[1].ask, None);
    assert!(items[1].bullets.is_empty());
}

#[test]
fn test_format_empty_set_is_positive_with_signature() {
    let out = format(&[], 0, None);
    assert!(out.contains("Inbox clear - 0 starred, 0 important"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

#[test]
fn test_format_basic_shape() {
    let items = vec![
        item(
            Pin::Starred,
            1_717_459_200_000, // 2024-06-04
            "Mark Weiler",
            "pentest proposal",
            "THREADA",
        ),
        item(
            Pin::Important,
            1_717_286_400_000, // 2024-06-02
            "JP Ciceri",
            "annual attestation",
            "THREADB",
        ),
    ];
    let out = format(&items, 0, None);

    assert!(out.contains("*Pinned inbox digest* - 1 starred, 1 important"));
    assert!(out.contains("*:star: Starred (1)*"));
    assert!(out.contains("*:exclamation: Important (1)*"));
    assert!(out.contains("*Mark Weiler*"));
    assert!(out.contains("https://mail.google.com/mail/u/0/#all/THREADA|pentest proposal"));
    assert!(out.contains("https://mail.google.com/mail/u/0/#all/THREADB|annual attestation"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

/// Section order is fixed and most-actionable-first, independent of dates.
#[test]
fn test_format_section_order_is_starred_then_important() {
    let items = vec![
        item(Pin::Important, 9_000, "I", "i", "IMP"),
        item(Pin::Starred, 8_000, "S", "s", "STAR"),
    ];
    let out = format(&items, 0, None);
    let star = out.find(":star: Starred").unwrap();
    let imp = out.find("Important (").unwrap();
    assert!(star < imp, "{}", out);
}

#[test]
fn test_format_signature_always_last_line() {
    let items = vec![item(Pin::Starred, 1_000, "A", "s", "T")];
    let out = format(&items, 0, None);
    let last = out.lines().last().unwrap();
    assert_eq!(last, SIGNATURE);
}

#[test]
fn test_format_browser_index_in_deep_link() {
    let items = vec![item(Pin::Starred, 1_000, "A", "s", "T")];
    let out = format(&items, 3, None);
    assert!(out.contains("https://mail.google.com/mail/u/3/#all/T|s"));
}

#[test]
fn test_format_escapes_mrkdwn_specials() {
    let mut it = item(Pin::Starred, 1_000, "Foo & Bar", "a < b > c & d", "T");
    it.bullets = vec!["bullet with < and > and &".to_string()];
    it.ask = Some("reply to <a@x.com>".to_string());
    let out = format(&[it], 0, None);
    assert!(out.contains("Foo &amp; Bar"));
    assert!(out.contains("a &lt; b &gt; c &amp; d"));
    assert!(out.contains("bullet with &lt; and &gt; and &amp;"));
    assert!(out.contains("reply to &lt;a@x.com&gt;"));
    // Raw specials must not survive in the rendered line.
    assert!(!out.contains("a < b"));
}

#[test]
fn test_format_empty_subject_fallback() {
    let items = vec![item(Pin::Starred, 1_000, "A", "   ", "T")];
    let out = format(&items, 0, None);
    assert!(out.contains("|(no subject)>"));
}

#[test]
fn test_format_sorts_each_section_newest_first() {
    let items = vec![
        item(Pin::Starred, 1_000, "Old", "old", "OLD"),
        item(Pin::Starred, 9_000, "New", "new", "NEW"),
    ];
    let out = format(&items, 0, None);
    let new_pos = out.find("NEW").unwrap();
    let old_pos = out.find("OLD").unwrap();
    assert!(new_pos < old_pos, "newest item should appear first");
}

// ---------------------------------------------------------------------------
// Ask rendering: the ask is a TYPED FIELD, marked at render, never a bullet.
// ---------------------------------------------------------------------------

#[test]
fn test_format_renders_the_ask_first_and_marked() {
    let mut it = item(Pin::Starred, 1_000, "Mark", "pentest", "T");
    it.ask = Some("confirm the scope by Friday".to_string());
    it.bullets = vec![
        "scope covers two apps".to_string(),
        "quote attached".to_string(),
    ];
    let out = format(&[it], 0, None);

    let lines: Vec<&str> = out.lines().collect();
    let line_idx = lines.iter().position(|l| l.contains("#all/T|")).unwrap();
    assert_eq!(
        lines[line_idx + 1],
        "  - *Reply needed:* confirm the scope by Friday",
        "the ask is the FIRST line under the thread, visibly marked:\n{}",
        out
    );
    assert_eq!(lines[line_idx + 2], "  - scope covers two apps");
    assert_eq!(lines[line_idx + 3], "  - quote attached");
}

/// The ask is NOT one of the 3-7 bullets: an ask-bearing thread renders its
/// ask line PLUS its bullets, up to 8 lines under the digest line.
#[test]
fn test_format_ask_is_not_one_of_the_bullets() {
    let mut it = item(Pin::Starred, 1_000, "Mark", "pentest", "T");
    it.ask = Some("confirm the scope".to_string());
    it.bullets = (0..7).map(|i| format!("bullet {}", i)).collect();
    let out = format(&[it], 0, None);

    assert_eq!(
        out.matches("  - ").count(),
        8,
        "1 ask + 7 bullets:\n{}",
        out
    );
    for i in 0..7 {
        assert!(out.contains(&format!("  - bullet {}\n", i)), "{}", out);
    }
    assert!(out.contains("  - *Reply needed:* confirm the scope\n"));
}

/// Scott declined a "No action needed" placeholder: absence of the marker IS
/// the signal, so a pure-FYI thread renders bullets and nothing else.
#[test]
fn test_format_no_ask_renders_no_marker_and_no_placeholder() {
    let mut it = item(Pin::Starred, 1_000, "Bot", "build green", "T");
    it.bullets = vec!["nightly build passed".to_string(), "no flakes".to_string()];
    let out = format(&[it], 0, None);

    assert!(!out.contains("Reply needed"), "{}", out);
    assert!(!out.to_lowercase().contains("no action"), "{}", out);
    assert_eq!(out.matches("  - ").count(), 2, "{}", out);
}

/// EVERY pinned thread gets bullets, in all both sections -- not just the
/// Needs Reply one.
#[test]
fn test_format_bullets_render_in_both_sections() {
    let items: Vec<DigestItem> = [Pin::Starred, Pin::Important]
        .into_iter()
        .enumerate()
        .map(|(i, pin)| {
            let mut it = item(pin, 1_000 + i as i64, "S", "s", &format!("T{}", i));
            it.bullets = vec![format!("point {}", i)];
            it
        })
        .collect();
    let out = format(&items, 0, None);
    for i in 0..2 {
        assert!(out.contains(&format!("  - point {}\n", i)), "{}", out);
    }
}

#[test]
fn test_format_banner_is_rendered_when_bullets_were_expected_and_failed() {
    let items = vec![item(Pin::Starred, 1_000, "A", "s", "T")];
    let banner = bullets::banner(crate::triage::claude::FailureClass::Auth);
    let out = format(&items, 0, Some(&banner));

    assert!(
        out.contains("_bullets unavailable: claude not authenticated_"),
        "{}",
        out
    );
    // Named CLASS, not a generic line: an auth outage must not read like a blip.
    assert!(!out.contains("claude transport failure"));
    // The digest still carries its links.
    assert!(out.contains("#all/T|s"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

/// No `triage:` block means bullets were never expected, so there is no banner
/// and nothing to report as a failure.
#[test]
fn test_format_unenriched_digest_carries_no_banner() {
    let items = vec![item(Pin::Starred, 1_000, "A", "s", "T")];
    let out = format(&items, 0, None);
    assert!(!out.contains("bullets unavailable"), "{}", out);
}

// ---------------------------------------------------------------------------
// Budget fixtures. Both are synthetic and their string lengths are LOAD
// BEARING: short senders and subjects drop the whole set under budget and
// silently defeat the ladder test (design doc, Phase 6, AC 2).
// ---------------------------------------------------------------------------

/// Newest first, so index 0 renders at the top of its section and the highest
/// indices are the trailing items rung 3 sheds.
const FIXTURE_BASE_MILLIS: i64 = 1_717_459_200_000; // 2024-06-04

fn pad(prefix: &str, len: usize) -> String {
    let mut out = prefix.to_string();
    while out.chars().count() < len {
        out.push('x');
    }
    out.chars().take(len).collect()
}

/// One fixture thread, sized to the REAL-mail figures the budget arithmetic
/// assumes: a 123-char rendered line (incl. newline) and bullets at the
/// 80-char cap.
fn fixture_item(index: usize, pin: Pin, ask: bool, bullets: usize) -> DigestItem {
    let mut it = DigestItem {
        pin,
        date: ts(FIXTURE_BASE_MILLIS - index as i64 * 1_000),
        sender: pad(&format!("S{}", index), 20),
        subject: pad(&format!("J{}", index), 33),
        thread_id: std::format!("{:016x}", index),
        ask: None,
        bullets: (0..bullets)
            .map(|k| pad(&format!("t{}b{}", index, k), bullets::MAX_BULLET_CHARS))
            .collect(),
    };
    if ask {
        it.ask = Some(pad(&format!("t{}ask", index), bullets::MAX_BULLET_CHARS));
    }
    it
}

/// The fixture's own assumption, asserted rather than trusted: 122 chars plus
/// the newline `render` appends is the 123-char line the sweep measured.
#[test]
fn test_fixture_line_is_123_chars_including_the_newline() {
    let it = fixture_item(0, Pin::Starred, false, 0);
    assert_eq!(line(&it, 0).len() + 1, 123, "{}", line(&it, 0));
    assert_eq!(it.thread_id.len(), 16);
}

/// AC (1): 10 threads (3 Needs Reply / 4 Starred / 3 Important), MIXED asks,
/// 7 bullets each at the 80-char cap. Everything renders and it fits. This
/// pins the RENDERER and the typed `ask` field; it does NOT exercise the
/// ladder and does not claim to.
#[test]
fn test_ac1_ten_mixed_threads_with_seven_bullets_render_whole_under_budget() {
    let plan = [(Pin::Starred, 7usize), (Pin::Important, 3)];
    let mut items = Vec::new();
    let mut index = 0usize;
    for (pin, count) in plan {
        for _ in 0..count {
            // Mixed: every other thread carries an ask.
            items.push(fixture_item(index, pin, index.is_multiple_of(2), 7));
            index += 1;
        }
    }
    assert_eq!(items.len(), 10);
    let asks = items.iter().filter(|i| i.ask.is_some()).count();
    assert_eq!(asks, 5, "the set must be MIXED or ask-marking is untested");

    let out = format(&items, 0, None);

    assert!(
        out.len() <= BUDGET,
        "AC(1) must fit the budget, got {}",
        out.len()
    );
    assert!(!out.contains("... +"), "no thread may be dropped:\n{}", out);
    assert!(out.contains("*Pinned inbox digest* - 7 starred, 3 important"));
    for item in &items {
        assert!(
            out.contains(&std::format!("#all/{}|", item.thread_id)),
            "thread {} must render",
            item.thread_id
        );
        for bullet in &item.bullets {
            assert!(
                out.contains(&std::format!("  - {}\n", bullet)),
                "bullet {} must render in full",
                bullet
            );
        }
        if let Some(ask) = &item.ask {
            assert!(
                out.contains(&std::format!("  - {} {}\n", ASK_MARKER, ask)),
                "ask on {} must render marked",
                item.thread_id
            );
        }
    }
    assert_eq!(
        out.matches(ASK_MARKER).count(),
        asks,
        "exactly the ask-bearing threads carry the marker"
    );
}

/// Rung 1 -> rung 2: bullets are shed before ANY thread is dropped. 70 pure-FYI
/// threads at 7 bullets are far over budget, but their rung-2 floor is under
/// it, so the ladder must halt with all 70 threads present.
#[test]
fn test_ladder_sheds_bullets_before_dropping_any_thread() {
    let items: Vec<DigestItem> = (0..70)
        .map(|i| fixture_item(i, Pin::Starred, false, 7))
        .collect();
    let full = items.iter().map(|i| i.bullets.len()).sum::<usize>();
    assert_eq!(full, 490);

    let out = format(&items, 0, None);

    assert!(out.len() <= BUDGET, "got {}", out.len());
    assert!(
        !out.contains("... +"),
        "no thread may be dropped while bullets remain to shed:\n{}",
        out
    );
    for item in &items {
        assert!(
            out.contains(&std::format!("#all/{}|", item.thread_id)),
            "thread {} must survive",
            item.thread_id
        );
    }
    assert!(
        out.matches("  - ").count() < full,
        "bullets must have been shed"
    );
}

/// AC (2): the ladder, deliberately over budget at EVERY rung including the
/// last. 70 MIXED threads (45 Starred / 25 Important), 35 ask-bearing spread
/// across both, 7 bullets each at the 80-char cap. The rung-2 floor is ~12300
/// against `BUDGET` 10000, so rung 3 must fire.
#[test]
fn test_ac2_seventy_mixed_threads_drive_the_ladder_to_rung_three() {
    let plan = [(Pin::Starred, 45usize, 23usize), (Pin::Important, 25, 12)];
    let mut items = Vec::new();
    let mut index = 0usize;
    for (pin, count, asks) in plan {
        for n in 0..count {
            items.push(fixture_item(index, pin, n < asks, 7));
            index += 1;
        }
    }
    assert_eq!(items.len(), 70);
    assert_eq!(
        items.iter().filter(|i| i.ask.is_some()).count(),
        35,
        "35 ask-bearing, spread across all both sections"
    );

    let out = format(&items, 0, None);

    assert!(
        out.len() <= BUDGET,
        "final body must fit, got {}",
        out.len()
    );
    // Header counts stay exact even though threads are hidden.
    assert!(out.contains("*Pinned inbox digest* - 45 starred, 25 important"));

    // Rung 2 floor reached: every thread that still renders is at ask-or-nothing.
    let descriptive: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("  - ") && !l.starts_with("  - *Reply needed:*"))
        .collect();
    assert!(
        descriptive.is_empty(),
        "at the rung-2 floor no descriptive bullet may survive: {:?}",
        descriptive
    );

    // ... and an ask-bearing thread that renders STILL carries its ask, while a
    // pure-FYI thread renders its digest line alone. That is the intended
    // shape, not a dropped thread.
    let lines: Vec<&str> = out.lines().collect();
    let mut rendered = 0usize;
    for item in &items {
        let needle = std::format!("#all/{}|", item.thread_id);
        let Some(idx) = lines.iter().position(|l| l.contains(&needle)) else {
            continue;
        };
        rendered += 1;
        let next = lines.get(idx + 1).copied().unwrap_or("");
        match &item.ask {
            Some(ask) => assert_eq!(
                next,
                std::format!("  - {} {}", ASK_MARKER, ask),
                "ask-bearing thread {} lost its ask",
                item.thread_id
            ),
            None => assert!(
                !next.starts_with("  - "),
                "pure-FYI thread {} must render its line alone at the floor, got {:?}",
                item.thread_id,
                next
            ),
        }
    }
    assert!(rendered < 70, "rung 3 must have dropped some threads");

    // Rung 3 sheds by ACTIONABILITY, least first: a Needs Reply thread is never
    // dropped while any Starred or Important thread remains.
    assert!(
        out.contains("... +"),
        "rung 3 must emit a '... +N more' line:\n{}",
        out
    );
    for item in items.iter().filter(|i| i.pin != Pin::Important) {
        assert!(
            out.contains(&std::format!("#all/{}|", item.thread_id)),
            "{:?} thread {} must not be shed while Important threads remain",
            item.pin,
            item.thread_id
        );
    }
    let hidden_lines: Vec<&str> = out.lines().filter(|l| l.starts_with("... +")).collect();
    assert_eq!(
        hidden_lines.len(),
        1,
        "only the Important section sheds here: {:?}",
        hidden_lines
    );
    let important_header = out.find(":exclamation: Important (25)").unwrap();
    assert!(
        out.find("... +").unwrap() > important_header,
        "the '... +N more' line belongs to the Important section"
    );
    assert!(out.trim_end().ends_with(SIGNATURE));
}

/// The extreme tail: even with every section shed to nothing the digest still
/// posts, header counts intact, signature last. Guards the rung-3 loop's exit.
#[test]
fn test_ladder_terminates_when_everything_must_be_shed() {
    let items: Vec<DigestItem> = (0..400)
        .map(|i| fixture_item(i, Pin::Starred, true, 7))
        .collect();
    let out = format(&items, 0, None);
    assert!(out.contains("*Pinned inbox digest* - 400 starred"));
    assert!(out.contains("... +"));
    assert!(out.trim_end().ends_with(SIGNATURE));
}

/// Audit C3: the subject arrives from a message header, so it is unbounded and
/// attacker-controlled. Cap it in CHARS with the marker inside the cap.
#[test]
fn test_line_caps_a_pathological_subject() {
    let it = item(Pin::Starred, 1_000, "A", &"S".repeat(50_000), "t1");
    let rendered = line(&it, 0);
    let shown = rendered
        .rsplit_once('|')
        .expect("link display text")
        .1
        .trim_end_matches('>');
    assert_eq!(shown.chars().count(), MAX_SUBJECT_CHARS);
    assert!(shown.ends_with(LINE_TRUNCATION_MARKER), "{}", shown);
}

#[test]
fn test_line_caps_a_pathological_sender() {
    let it = item(Pin::Starred, 1_000, &"N".repeat(5_000), "subj", "t1");
    let rendered = line(&it, 0);
    let sender = rendered
        .split_once("` *")
        .expect("sender field")
        .1
        .split_once("* <")
        .expect("sender field end")
        .0;
    assert_eq!(sender.chars().count(), MAX_SENDER_CHARS);
    assert!(sender.ends_with(LINE_TRUNCATION_MARKER), "{}", sender);
}

/// The consequence the cap exists to prevent: uncapped, ONE 50k-char subject
/// blew `BUDGET` on its own, drove the ladder to its floor, and shed every
/// other thread -- a 124-byte digest reading `... +N more`. Capped, all ten
/// threads still render.
#[test]
fn test_a_pathological_subject_does_not_collapse_the_digest() {
    let mut items: Vec<DigestItem> = (0..9)
        .map(|i| fixture_item(i, Pin::Starred, false, 3))
        .collect();
    let mut poison = fixture_item(9, Pin::Starred, false, 3);
    poison.subject = "S".repeat(50_000);
    items.push(poison);

    let out = format(&items, 0, None);
    assert!(out.len() <= BUDGET, "over budget: {} chars", out.len());
    assert!(!out.contains("... +"), "threads were shed:\n{}", out);
    for i in 0..9 {
        let id = std::format!("{:016x}", i);
        assert!(out.contains(&id), "thread {} missing:\n{}", id, out);
    }
}

/// Both caps sit ABOVE the real-mail figures the budget arithmetic assumes, so
/// the cap is a backstop against pathology, not a routine truncation of
/// ordinary mail. Asserted rather than trusted, because tightening either
/// constant under the fixture would silently start clipping normal subjects.
#[test]
fn test_line_caps_exceed_the_fixture_figures() {
    let it = fixture_item(0, Pin::Starred, false, 0);
    assert!(it.sender.chars().count() < MAX_SENDER_CHARS);
    assert!(it.subject.chars().count() < MAX_SUBJECT_CHARS);
}
