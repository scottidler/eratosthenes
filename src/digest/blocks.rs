//! Block Kit `rich_text` rendering for the digest.
//!
//! Replaces a single `text` blob for two reasons that the 2026-09-08 live run
//! made obvious.
//!
//! **Real lists.** Slack `mrkdwn` has no list syntax, so the old `  - ` prefix
//! rendered as a literal hyphen and the digest read as a text dump. Block Kit
//! `rich_text_list` with `style: "bullet"` produces actual bullets, and
//! `"ordered"` produces numbered ones. That element is the only way to get a
//! list in Slack; the fix was never a different `mrkdwn` string.
//!
//! **No escaping, therefore no escaping bugs.** Inside `rich_text`, a `text`
//! element's content is LITERAL and its formatting comes from a `style` object,
//! not from in-band markers. So `<`, `>` and `&` need no mangling. The old path
//! escaped them for `mrkdwn`'s `<url|text>` form, and Slack does not decode
//! entities in link display text, which is why a subject reading
//! `Mark <> Scott Sync` reached the digest as a visible `Mark &lt;&gt; Scott Sync`.
//!
//! The message still carries a short `text` alongside the blocks. That field is
//! the notification and accessibility fallback, not the body, which also means
//! the body no longer has to fit Slack's 4,000-character `text` limit -- the
//! limit that silently split the first enriched digest into two messages
//! mid-thread.

use serde_json::{Value, json};

use super::{DigestItem, Pin, SECTIONS, gmail_url};

/// Bold-styled text element.
fn bold(text: &str) -> Value {
    json!({ "type": "text", "text": text, "style": { "bold": true } })
}

/// Monospace text element, used for the date so lines align.
fn code(text: &str) -> Value {
    json!({ "type": "text", "text": text, "style": { "code": true } })
}

/// Plain text element. No escaping: `rich_text` content is literal.
fn plain(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn link(url: &str, text: &str) -> Value {
    json!({ "type": "link", "url": url, "text": text })
}

fn section(elements: Vec<Value>) -> Value {
    json!({ "type": "rich_text_section", "elements": elements })
}

/// One thread's bullets as a REAL bulleted list. Returns `None` for a thread
/// with nothing under it, so an empty `rich_text_list` is never emitted (Slack
/// rejects one).
fn bullet_list(item: &DigestItem) -> Option<Value> {
    let mut entries: Vec<Value> = Vec::new();

    // The ask leads, and is the only bolded entry, so it reads as the one thing
    // being asked rather than as another summary line.
    if let Some(ask) = &item.ask {
        entries.push(section(vec![bold("Reply needed: "), plain(ask)]));
    }
    for b in &item.bullets {
        entries.push(section(vec![plain(b)]));
    }

    if entries.is_empty() {
        return None;
    }
    Some(json!({
        "type": "rich_text_list",
        "style": "bullet",
        "indent": 0,
        "elements": entries,
    }))
}

/// The digest as Block Kit blocks: one `rich_text` block whose `elements` hold
/// every line, rather than one block per line.
///
/// One block, deliberately: a message is capped at 50 blocks, which 5 pinned
/// threads would approach and 20 would blow. A single block's `elements` array
/// has no such per-message ceiling.
pub fn format_blocks(items: &[DigestItem], browser_index: u8, banner: Option<&str>) -> Value {
    let mut elements: Vec<Value> = Vec::new();

    let mut by_section: [Vec<&DigestItem>; SECTIONS.len()] = Default::default();
    for item in items {
        by_section[item.pin.index()].push(item);
    }
    for bucket in by_section.iter_mut() {
        bucket.sort_by_key(|i| std::cmp::Reverse(i.date));
    }

    let counts: Vec<String> = SECTIONS
        .iter()
        .enumerate()
        .map(|(i, (_, word))| format!("{} {}", by_section[i].len(), word))
        .collect();
    elements.push(section(vec![bold(&format!(
        "Pinned inbox digest - {}\n",
        counts.join(", ")
    ))]));

    if let Some(text) = banner {
        elements.push(section(vec![plain(&format!("{}\n", text))]));
    }

    if items.is_empty() {
        elements.push(section(vec![plain("Inbox clear.")]));
        return json!([{ "type": "rich_text", "elements": elements }]);
    }

    for (idx, (header, _)) in SECTIONS.iter().enumerate() {
        let bucket = &by_section[idx];
        if bucket.is_empty() {
            continue;
        }
        elements.push(section(vec![bold(&format!(
            "\n{} ({})\n",
            header,
            bucket.len()
        ))]));

        for item in bucket {
            let subject = if item.subject.trim().is_empty() {
                "(no subject)"
            } else {
                item.subject.trim()
            };
            elements.push(section(vec![
                code(&item.date.format("%b %d").to_string()),
                plain(" "),
                bold(&format!("{} ", item.sender)),
                link(&gmail_url(browser_index, &item.thread_id), subject),
            ]));
            if let Some(list) = bullet_list(item) {
                elements.push(list);
            }
        }
    }

    json!([{ "type": "rich_text", "elements": elements }])
}

/// The notification/accessibility fallback that rides alongside the blocks.
///
/// Deliberately just the header: Slack shows this in notifications and in
/// clients that cannot render blocks, and duplicating the whole body here would
/// reintroduce the 4,000-character split it exists to avoid.
pub fn fallback_text(items: &[DigestItem]) -> String {
    let starred = items.iter().filter(|i| i.pin == Pin::Starred).count();
    let important = items.iter().filter(|i| i.pin == Pin::Important).count();
    if starred + important == 0 {
        return "Pinned inbox digest - inbox clear".to_string();
    }
    format!(
        "Pinned inbox digest - {} starred, {} important",
        starred, important
    )
}

/// Unused today, kept honest: the digest has no ordered list. Documented here
/// because "Slack cannot do numbered lists" was the wrong belief that produced
/// the literal-hyphen bug, and the next person should see the answer.
#[allow(dead_code)]
fn _ordered_list_example(entries: Vec<Value>) -> Value {
    json!({ "type": "rich_text_list", "style": "ordered", "elements": entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    fn item(pin: Pin, subject: &str, ask: Option<&str>, bullets: &[&str]) -> DigestItem {
        DigestItem {
            pin,
            date: DateTime::from_timestamp_millis(1_788_800_000_000).expect("valid"),
            sender: "Scott Idler".to_string(),
            subject: subject.to_string(),
            thread_id: "abc123".to_string(),
            ask: ask.map(String::from),
            bullets: bullets.iter().map(|b| b.to_string()).collect(),
        }
    }

    fn render(items: &[DigestItem]) -> String {
        serde_json::to_string(&format_blocks(items, 0, None)).expect("serializes")
    }

    /// The whole point: a REAL bulleted list, not a literal hyphen.
    #[test]
    fn test_bullets_are_a_rich_text_list_not_hyphens() {
        let out = render(&[item(Pin::Starred, "subj", None, &["one", "two"])]);
        assert!(out.contains(r#""type":"rich_text_list""#), "{}", out);
        assert!(out.contains(r#""style":"bullet""#), "{}", out);
        assert!(
            !out.contains(r#""text":"  - "#),
            "a literal hyphen prefix came back: {}",
            out
        );
    }

    /// `rich_text` content is literal, so angle brackets survive intact. The
    /// old mrkdwn path turned this subject into a visible `&lt;&gt;`.
    #[test]
    fn test_angle_brackets_in_a_subject_are_not_escaped() {
        let out = render(&[item(Pin::Starred, "Mark <> Scott Sync", None, &["x"])]);
        assert!(out.contains("Mark <> Scott Sync"), "{}", out);
        assert!(!out.contains("&lt;"), "entities leaked: {}", out);
        assert!(!out.contains("&gt;"), "entities leaked: {}", out);
    }

    /// An ampersand is the same story.
    #[test]
    fn test_ampersand_in_a_sender_is_not_escaped() {
        let mut it = item(Pin::Important, "subj", None, &["x"]);
        it.sender = "Ben & Jerry".to_string();
        let out = render(&[it]);
        assert!(out.contains("Ben & Jerry"), "{}", out);
        assert!(!out.contains("&amp;"), "entities leaked: {}", out);
    }

    /// Slack rejects an empty `rich_text_list`, so a bullet-less thread emits
    /// none at all.
    #[test]
    fn test_a_thread_with_no_bullets_emits_no_list() {
        let out = render(&[item(Pin::Starred, "subj", None, &[])]);
        assert!(!out.contains("rich_text_list"), "{}", out);
    }

    /// The ask leads its list and is the only bold entry.
    #[test]
    fn test_the_ask_leads_the_list() {
        let out = render(&[item(Pin::Starred, "subj", Some("do the thing"), &["ctx"])]);
        let ask_at = out.find("Reply needed").expect("ask rendered");
        let ctx_at = out.find("ctx").expect("bullet rendered");
        assert!(ask_at < ctx_at, "the ask must come first: {}", out);
    }

    /// One block, so the 50-block message cap cannot be reached by thread count.
    #[test]
    fn test_everything_lands_in_one_block() {
        let items: Vec<DigestItem> = (0..30)
            .map(|i| item(Pin::Starred, &format!("subj {}", i), None, &["a", "b", "c"]))
            .collect();
        let blocks = format_blocks(&items, 0, None);
        assert_eq!(
            blocks.as_array().expect("array").len(),
            1,
            "thread count must not multiply blocks"
        );
    }

    /// There is no machine-chosen section any more; only what the human pinned.
    #[test]
    fn test_only_starred_and_important_sections_exist() {
        let out = render(&[
            item(Pin::Starred, "s", None, &["a"]),
            item(Pin::Important, "i", None, &["b"]),
        ]);
        assert!(out.contains("Starred"), "{}", out);
        assert!(out.contains("Important"), "{}", out);
        assert!(!out.contains("Needs Reply"), "{}", out);
    }

    #[test]
    fn test_fallback_text_is_the_header_only() {
        let text = fallback_text(&[
            item(Pin::Starred, "s", None, &["a"]),
            item(Pin::Important, "i", None, &["b"]),
        ]);
        assert_eq!(text, "Pinned inbox digest - 1 starred, 1 important");
        assert!(
            text.len() < 200,
            "the fallback must stay short or it reintroduces the split"
        );
    }

    #[test]
    fn test_empty_set_still_renders_a_block() {
        let blocks = format_blocks(&[], 0, None);
        let out = serde_json::to_string(&blocks).expect("serializes");
        assert!(out.contains("Inbox clear"), "{}", out);
        assert_eq!(fallback_text(&[]), "Pinned inbox digest - inbox clear");
    }

    /// Guards the doc claim that an ordered list is available, so nobody
    /// re-derives "Slack has no numbered lists".
    #[test]
    fn test_ordered_style_is_a_real_option() {
        let v = _ordered_list_example(vec![section(vec![plain("first")])]);
        assert_eq!(v["style"], "ordered");
    }

    /// The date renders as a monospace `%b %d`, which is what keeps the lines
    /// visually aligned in Slack.
    #[test]
    fn test_dates_render_as_monospace_month_day() {
        let it = item(Pin::Starred, "subj", None, &["a"]);
        let expected = it.date.format("%b %d").to_string();
        let out = render(&[it]);
        assert!(out.contains(&expected), "{}", out);
        assert!(out.contains(r#""style":{"code":true}"#), "{}", out);
    }
}
