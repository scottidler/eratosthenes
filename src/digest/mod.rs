use std::collections::HashSet;

use chrono::{DateTime, Utc};
use log::debug;

use crate::gmail::message::{GmailMessage, GmailThread};

/// Readability budget for the Slack message body. `chat.postMessage` accepts a
/// much larger `text` (~40k chars), but a digest beyond this is unreadable, so
/// `format` truncates the longer section with a `... +N more` line past this.
const BUDGET: usize = 3500;

/// Signature marking the post as automated. Posted AS the user (xoxp token), so
/// this must never be dropped. Always the message's own last line.
const SIGNATURE: &str = ":giga-claude:";

/// Which pin a thread carries in the digest. `Starred` wins if a thread is both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    Starred,
    Important,
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
}

/// Assemble one `DigestItem` per pinned thread. A thread present in both the
/// starred and important sets appears once, under Starred. Sender/subject/date
/// come from the thread's latest message. Threads in neither set are skipped.
pub fn build(
    threads: &[GmailThread],
    starred_ids: &HashSet<String>,
    important_ids: &HashSet<String>,
) -> Vec<DigestItem> {
    debug!(
        "build: threads={}, starred_ids={}, important_ids={}",
        threads.len(),
        starred_ids.len(),
        important_ids.len()
    );

    let mut items = Vec::new();
    for thread in threads {
        let pin = if starred_ids.contains(&thread.id) {
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
        });
    }

    debug!("build: produced {} items", items.len());
    items
}

/// Format the items into a Slack `mrkdwn` message: two grouped sections with
/// per-item date / sender / subject, the subject deep-linked to the Gmail thread
/// at `/u/{browser_index}/`. Header counts are always exact; if the body exceeds
/// `BUDGET`, the longer section is truncated with a `... +N more` line. The
/// signature is always the last line.
pub fn format(items: &[DigestItem], browser_index: u8) -> String {
    let mut starred: Vec<&DigestItem> = items.iter().filter(|i| i.pin == Pin::Starred).collect();
    let mut important: Vec<&DigestItem> = items.iter().filter(|i| i.pin == Pin::Important).collect();

    // Most-actionable first: newest at the top of each section.
    starred.sort_by_key(|i| std::cmp::Reverse(i.date));
    important.sort_by_key(|i| std::cmp::Reverse(i.date));

    let s_total = starred.len();
    let i_total = important.len();
    debug!(
        "format: starred={}, important={}, browser_index={}",
        s_total, i_total, browser_index
    );

    if s_total == 0 && i_total == 0 {
        return format!("Inbox clear - 0 starred, 0 important\n\n{}", SIGNATURE);
    }

    // Show everything, then drop trailing items from the longer section until the
    // body fits the budget. Pinned sets are tens of threads, so the linear shrink
    // is cheap.
    let mut s_show = s_total;
    let mut i_show = i_total;
    loop {
        let msg = render(&starred, &important, s_show, i_show, s_total, i_total, browser_index);
        if msg.len() <= BUDGET {
            return msg;
        }
        if s_show >= i_show && s_show > 0 {
            s_show -= 1;
        } else if i_show > 0 {
            i_show -= 1;
        } else {
            return msg;
        }
    }
}

fn render(
    starred: &[&DigestItem],
    important: &[&DigestItem],
    s_show: usize,
    i_show: usize,
    s_total: usize,
    i_total: usize,
    browser_index: u8,
) -> String {
    let mut out = format!("*Pinned inbox digest* - {} starred, {} important\n", s_total, i_total);

    if s_total > 0 {
        out.push_str(&format!("\n*:star: Starred ({})*\n", s_total));
        for item in starred.iter().take(s_show) {
            out.push_str(&line(item, browser_index));
            out.push('\n');
        }
        let hidden = s_total - s_show;
        if hidden > 0 {
            out.push_str(&format!("... +{} more\n", hidden));
        }
    }

    if i_total > 0 {
        out.push_str(&format!("\n*:exclamation: Important ({})*\n", i_total));
        for item in important.iter().take(i_show) {
            out.push_str(&line(item, browser_index));
            out.push('\n');
        }
        let hidden = i_total - i_show;
        if hidden > 0 {
            out.push_str(&format!("... +{} more\n", hidden));
        }
    }

    out.push_str(&format!("\n{}", SIGNATURE));
    out
}

fn line(item: &DigestItem, browser_index: u8) -> String {
    let date = item.date.format("%b %d");
    let sender = escape_mrkdwn(&item.sender);
    let subject = if item.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        escape_mrkdwn(&item.subject)
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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
    if raw.is_empty() { None } else { Some(raw.to_string()) }
}

#[cfg(test)]
mod tests;
