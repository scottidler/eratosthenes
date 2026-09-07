pub mod bullets;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use log::debug;

use crate::digest::bullets::ThreadBullets;
use crate::gmail::message::{GmailMessage, GmailThread};

/// Readability budget for the Slack message body. `chat.postMessage` accepts a
/// much larger `text` (~40k chars), but a digest beyond this is unreadable, so
/// `format` degrades past this: bullets first, whole threads only as a last
/// resort.
///
/// 10000, raised from 3500 when threads gained 3-7 bullets each (design doc,
/// Phase 6, panel finding M1). Sized to the bullet cap, never the reverse: a
/// 10-thread set at 7 bullets of 80 chars measures ~7300 including the `  - `
/// prefixes, section headers and signature.
const BUDGET: usize = 10000;

/// Signature marking the post as automated. Posted AS the user (xoxp token), so
/// this must never be dropped. Always the message's own last line.
const SIGNATURE: &str = ":giga-claude:";

/// Applied at RENDER time to `DigestItem::ask`. Presentation only: the ladder
/// reads the typed field and never re-parses this marker back out of a string.
const ASK_MARKER: &str = "*Reply needed:*";

/// Per-line caps, in CHARS, on the two fields that arrive straight from a
/// message header and are therefore attacker-controlled and unbounded. Bullets
/// and asks are capped where they are built (`bullets::MAX_BULLET_CHARS`);
/// these were not (audit C3), so ONE pathological subject could eat the whole
/// `BUDGET`, drive the shrink ladder to its floor, and collapse its section to
/// a bare `... +N more` -- a 50k-char subject yielded a 124-byte digest.
///
/// Applied BEFORE `escape_mrkdwn`, so the cap counts the characters a reader
/// sees. Escaping can still expand a capped field up to 5x (`&` -> `&amp;`),
/// which is bounded and small next to `BUDGET`.
const MAX_SUBJECT_CHARS: usize = 120;
const MAX_SENDER_CHARS: usize = 40;

/// Counted INSIDE the caps above, for the reason `bullets`' marker is: the cap
/// is what the budget arithmetic assumes.
const LINE_TRUNCATION_MARKER: &str = "...";

/// Section header emoji + title, and the word used in the count line, in
/// DISPLAY order: most actionable first.
const SECTIONS: [(&str, &str); 3] = [
    (":speech_balloon: Needs Reply", "needs reply"),
    (":star: Starred", "starred"),
    (":exclamation: Important", "important"),
];

/// Which section a thread lands in. Highest wins: a thread appears exactly
/// once, in `Needs Reply` > `Starred` > `Important` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    NeedsReply,
    Starred,
    Important,
}

impl Pin {
    /// Index into `SECTIONS`, which is also the actionability rank.
    fn index(self) -> usize {
        match self {
            Pin::NeedsReply => 0,
            Pin::Starred => 1,
            Pin::Important => 2,
        }
    }
}

/// One digest line: exactly one per pinned thread.
#[derive(Debug, Clone)]
pub struct DigestItem {
    pub pin: Pin,
    /// Thread's latest-message time.
    pub date: DateTime<Utc>,
    /// Latest message's display name, falling back to email.
    pub sender: String,
    /// Latest message's subject.
    pub subject: String,
    /// For the Gmail deep link; one per thread.
    pub thread_id: String,
    /// What this thread asks of the account owner, when it asks anything.
    ///
    /// A SEPARATE TYPED FIELD, deliberately, not `bullets[0]` carrying a
    /// prefix: the shrink ladder must never drop it, and with a bare string
    /// the ladder would have to re-parse a presentation marker out of the text
    /// to know that, making a rendering detail load-bearing for a correctness
    /// rule (design doc, Phase 6, panel finding M3).
    pub ask: Option<String>,
    /// 3-7 summary bullets. The ask is NOT one of them: an ask-bearing thread
    /// renders its ask line PLUS these, up to 8 lines.
    pub bullets: Vec<String>,
}

/// Assemble one `DigestItem` per pinned thread. A thread in more than one set
/// appears once, in its highest section. Sender/subject/date come from the
/// thread's latest message. Threads in no set are skipped.
///
/// Bullets are attached separately by `attach_bullets`: an un-enriched digest
/// (no `triage:` block, or a failed bullet pass) is this function's output
/// rendered as-is.
pub fn build(
    threads: &[GmailThread],
    needs_reply_ids: &HashSet<String>,
    starred_ids: &HashSet<String>,
    important_ids: &HashSet<String>,
) -> Vec<DigestItem> {
    debug!(
        "build: threads={}, needs_reply_ids={}, starred_ids={}, important_ids={}",
        threads.len(),
        needs_reply_ids.len(),
        starred_ids.len(),
        important_ids.len()
    );

    let mut items = Vec::new();
    for thread in threads {
        let pin = if needs_reply_ids.contains(&thread.id) {
            Pin::NeedsReply
        } else if starred_ids.contains(&thread.id) {
            Pin::Starred
        } else if important_ids.contains(&thread.id) {
            Pin::Important
        } else {
            continue;
        };

        let Some(last) = thread.messages.last() else {
            continue;
        };

        items.push(DigestItem {
            pin,
            date: last.internal_date,
            sender: sender_display(last),
            subject: last.subject.clone(),
            thread_id: thread.id.clone(),
            ask: None,
            bullets: Vec::new(),
        });
    }

    debug!("build: produced {} items", items.len());
    items
}

/// Fold the bullet pass's answer into the items, keyed by thread id. A thread
/// the model said nothing about keeps its empty ask/bullets and renders as a
/// bare digest line.
pub fn attach_bullets(items: &mut [DigestItem], by_thread: &HashMap<String, ThreadBullets>) {
    let mut enriched = 0usize;
    for item in items.iter_mut() {
        if let Some(data) = by_thread.get(&item.thread_id) {
            item.ask = data.ask.clone();
            item.bullets = data.bullets.clone();
            enriched += 1;
        }
    }
    debug!(
        "attach_bullets: {}/{} items enriched",
        enriched,
        items.len()
    );
}

/// Format the items into a Slack `mrkdwn` message: three grouped sections with
/// per-item date / sender / subject, the subject deep-linked to the Gmail thread
/// at `/u/{browser_index}/`, and each thread's ask line plus summary bullets
/// under it. Header counts are always exact and the signature is always the
/// last line.
///
/// `banner` is the degradation line for a bullet pass that was expected and
/// failed; `None` covers both "bullets are there" and "bullets were never
/// expected", which are the two cases that must not be reported as failures.
///
/// Over `BUDGET`, degrade in this order (design doc, Phase 6):
/// 1. every thread keeps its bullets;
/// 2. shrink the per-thread bullet count, NEVER dropping an ask;
/// 3. only with every thread at its ask-or-nothing floor, drop trailing
///    THREADS, least actionable section first: Important, then Starred, then
///    Needs Reply, each with its own `... +N more`.
pub fn format(items: &[DigestItem], browser_index: u8, banner: Option<&str>) -> String {
    let mut sections: [Vec<&DigestItem>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for item in items {
        sections[item.pin.index()].push(item);
    }
    // Most-actionable first: newest at the top of each section.
    for section in sections.iter_mut() {
        section.sort_by_key(|i| std::cmp::Reverse(i.date));
    }

    let totals = [sections[0].len(), sections[1].len(), sections[2].len()];
    debug!(
        "format: needs_reply={}, starred={}, important={}, browser_index={}, banner={}",
        totals[0],
        totals[1],
        totals[2],
        browser_index,
        banner.is_some()
    );

    if totals.iter().all(|t| *t == 0) {
        let mut out = "Inbox clear - 0 needs reply, 0 starred, 0 important\n".to_string();
        if let Some(text) = banner {
            out.push_str(&format!("{}\n", text));
        }
        out.push_str(&format!("\n{}", SIGNATURE));
        return out;
    }

    // Rungs 1-2: shed BULLETS before threads. `cap` is a per-thread ceiling on
    // SUMMARY bullets only -- an ask always renders, so at cap 0 an ask-bearing
    // thread still shows its ask and a pure-FYI thread shows its line alone.
    let max_bullets = items.iter().map(|i| i.bullets.len()).max().unwrap_or(0);
    let mut rendered = String::new();
    for cap in (0..=max_bullets).rev() {
        rendered = render(&sections, &totals, cap, banner, browser_index);
        if rendered.len() <= BUDGET {
            debug!(
                "format: fits at bullet cap {} ({} chars)",
                cap,
                rendered.len()
            );
            return rendered;
        }
    }

    // Rung 3: every thread is at its floor and the body is STILL over budget.
    // Shed by ACTIONABILITY, least first, so a Needs Reply thread is never
    // dropped while a Starred or Important one remains. This replaces the old
    // longest-section-first rule outright.
    let mut shows = totals;
    loop {
        let Some(idx) = [2usize, 1, 0].into_iter().find(|i| shows[*i] > 0) else {
            debug!("format: rung 3 exhausted; nothing left to shed");
            return rendered;
        };
        shows[idx] -= 1;
        rendered = render(&sections, &shows, 0, banner, browser_index);
        if rendered.len() <= BUDGET {
            debug!(
                "format: fits after shedding threads to {:?} ({} chars)",
                shows,
                rendered.len()
            );
            return rendered;
        }
    }
}

fn render(
    sections: &[Vec<&DigestItem>; 3],
    shows: &[usize; 3],
    bullet_cap: usize,
    banner: Option<&str>,
    browser_index: u8,
) -> String {
    let counts: Vec<String> = SECTIONS
        .iter()
        .enumerate()
        .map(|(idx, (_, word))| format!("{} {}", sections[idx].len(), word))
        .collect();
    let mut out = format!("*Pinned inbox digest* - {}\n", counts.join(", "));
    if let Some(text) = banner {
        out.push_str(&format!("{}\n", text));
    }

    for (idx, (header, _)) in SECTIONS.iter().enumerate() {
        let total = sections[idx].len();
        if total == 0 {
            continue;
        }
        out.push_str(&format!("\n*{} ({})*\n", header, total));
        for item in sections[idx].iter().take(shows[idx]) {
            out.push_str(&line(item, browser_index));
            out.push('\n');
            out.push_str(&bullet_lines(item, bullet_cap));
        }
        let hidden = total - shows[idx];
        if hidden > 0 {
            out.push_str(&format!("... +{} more\n", hidden));
        }
    }

    out.push_str(&format!("\n{}", SIGNATURE));
    out
}

/// The indented lines under one thread's digest line: the marked ask first when
/// there is one, then up to `cap` summary bullets.
///
/// No ask means no marker line and NO placeholder -- absence of the marker is
/// itself the signal that nothing is being asked (design doc, Phase 6; Scott
/// declined a "No action needed" line explicitly).
fn bullet_lines(item: &DigestItem, cap: usize) -> String {
    let mut out = String::new();
    if let Some(ask) = &item.ask {
        out.push_str(&format!("  - {} {}\n", ASK_MARKER, escape_mrkdwn(ask)));
    }
    for bullet in item.bullets.iter().take(cap) {
        out.push_str(&format!("  - {}\n", escape_mrkdwn(bullet)));
    }
    out
}

/// Hard cap in CHARS with the marker counted inside it. Same shape as
/// `bullets::cap_bullet`, kept separate because the caps differ per field.
fn cap_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let marker = LINE_TRUNCATION_MARKER.chars().count();
    if max <= marker {
        return text.chars().take(max).collect();
    }
    let head: String = text.chars().take(max - marker).collect();
    format!("{}{}", head.trim_end(), LINE_TRUNCATION_MARKER)
}

fn line(item: &DigestItem, browser_index: u8) -> String {
    let date = item.date.format("%b %d");
    let sender = escape_mrkdwn(&cap_chars(&item.sender, MAX_SENDER_CHARS));
    let subject = if item.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        escape_mrkdwn(&cap_chars(&item.subject, MAX_SUBJECT_CHARS))
    };
    let url = format!(
        "https://mail.google.com/mail/u/{}/#all/{}",
        browser_index, item.thread_id
    );
    format!("`{}` *{}* <{}|{}>", date, sender, url, subject)
}

/// Escape the three characters Slack treats specially in `mrkdwn` text. A bare
/// `>` in the link display text would close the `<url|text>` link early.
fn escape_mrkdwn(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Display name from a message: parse the `From` header's name, falling back to
/// the email it wraps, then to the parsed sender email, then to a placeholder.
fn sender_display(msg: &GmailMessage) -> String {
    if let Some(raw) = msg.headers.get("From")
        && let Some(name) = parse_display_name(raw)
    {
        return name;
    }
    msg.from
        .first()
        .cloned()
        .unwrap_or_else(|| "(unknown sender)".to_string())
}

/// Extract the display name from a raw `From` header value. Returns the name for
/// `Name <addr>`, the address when no name is present, or the whole value when
/// there are no angle brackets. `None` only for an empty value.
fn parse_display_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(lt) = raw.find('<') {
        let name = raw[..lt].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
        if let Some(gt) = raw.find('>') {
            let email = raw[lt + 1..gt].trim();
            if !email.is_empty() {
                return Some(email.to_string());
            }
        }
        return None;
    }
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

#[cfg(test)]
mod tests;
