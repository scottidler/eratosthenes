//! The `format=full` thread shape triage works on.
//!
//! Deliberately a separate type from `GmailMessage`/`GmailThread`: those model
//! the metadata fetch the aging engine runs on every inbox thread every five
//! minutes, and giving them an always-`None` body field would put a triage
//! concern in the hot path of a subsystem that has no use for it.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use log::trace;

use crate::gmail::message::parse_address_header;
use crate::triage::body::{extract_body, strip_quotes};

/// The header set the design doc's Data Plumbing section extends to. The
/// aging engine's default set is To/Cc/From/Subject; the five additions are
/// the RFC822 reply builder's inputs (Phase 7) and the thread ordering signal.
///
/// `format=full` returns EVERY header, so this list is not a request filter
/// the way `metadata_headers` is: it is the projection triage keeps, which
/// bounds what a prompt payload can ever carry out of a message.
pub const TRIAGE_HEADERS: &[&str] = &[
    "From",
    "To",
    "Cc",
    "Subject",
    "Date",
    "Message-ID",
    "In-Reply-To",
    "References",
    "Reply-To",
];

/// One message inside a triage candidate thread: headers, labels, and the
/// extracted, quote-stripped body.
#[derive(Debug, Clone)]
pub struct TriageMessage {
    pub id: String,
    pub label_ids: Vec<String>,
    pub internal_date: DateTime<Utc>,
    /// Keyed by LOWERCASED header name. Mail servers disagree on the casing of
    /// `Message-ID` / `Message-Id`, and a case-sensitive lookup silently
    /// returns `None` for half of them.
    headers: HashMap<String, String>,
    pub body: String,
}

impl TriageMessage {
    pub fn from_api(msg: google_gmail1::api::Message) -> Result<Self> {
        let id = msg.id.clone().ok_or_else(|| eyre!("message missing id"))?;
        let internal_date_millis = msg
            .internal_date
            .ok_or_else(|| eyre!("message {} missing internal_date", id))?;
        let internal_date = DateTime::from_timestamp_millis(internal_date_millis)
            .ok_or_else(|| eyre!("invalid timestamp: {}", internal_date_millis))?;

        let payload = msg.payload;
        let wanted: Vec<String> = TRIAGE_HEADERS.iter().map(|h| h.to_lowercase()).collect();
        let headers: HashMap<String, String> = payload
            .as_ref()
            .and_then(|p| p.headers.as_ref())
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| {
                        let name = h.name.as_ref()?.to_lowercase();
                        if !wanted.contains(&name) {
                            return None;
                        }
                        Some((name, h.value.clone()?))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let body = payload
            .as_ref()
            .map(|p| strip_quotes(&extract_body(p)))
            .unwrap_or_default();

        trace!(
            "TriageMessage::from_api: id={}, headers={}, body_chars={}",
            id,
            headers.len(),
            body.chars().count()
        );

        Ok(Self {
            id,
            label_ids: msg.label_ids.unwrap_or_default(),
            internal_date,
            headers,
            body,
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }

    pub fn subject(&self) -> &str {
        self.header("Subject").unwrap_or("")
    }

    pub fn from(&self) -> &str {
        self.header("From").unwrap_or("")
    }

    pub fn date(&self) -> &str {
        self.header("Date").unwrap_or("")
    }

    /// True when this message was sent BY the authenticated account. Drives the
    /// answered-rule: a thread whose newest message is Scott's own reply is not
    /// waiting on him.
    pub fn is_from_self(&self, self_address: &str) -> bool {
        let from = self.header("From").map(|s| s.to_string());
        parse_address_header(from.as_ref())
            .iter()
            .any(|addr| addr == &self_address.to_lowercase())
    }
}

/// A candidate thread, messages in Gmail's own oldest-first order.
#[derive(Debug, Clone)]
pub struct TriageThread {
    pub id: String,
    pub messages: Vec<TriageMessage>,
}

impl TriageThread {
    pub fn from_api(thread: google_gmail1::api::Thread) -> Result<Self> {
        let id = thread
            .id
            .clone()
            .ok_or_else(|| eyre!("thread missing id"))?;
        let mut messages: Vec<TriageMessage> = Vec::new();
        for msg in thread.messages.unwrap_or_default() {
            match TriageMessage::from_api(msg) {
                Ok(m) => messages.push(m),
                Err(e) => log::warn!("skipping malformed message in thread {}: {:#}", id, e),
            }
        }
        messages.sort_by_key(|m| m.internal_date);
        Ok(Self { id, messages })
    }

    pub fn newest(&self) -> Option<&TriageMessage> {
        self.messages.last()
    }

    /// Newest first, which is the order the per-thread char budget is spent in:
    /// if something has to be dropped it is the oldest message, never the one
    /// that just arrived.
    pub fn newest_first(&self) -> impl Iterator<Item = &TriageMessage> {
        self.messages.iter().rev()
    }

    pub fn subject(&self) -> &str {
        self.newest().map(|m| m.subject()).unwrap_or("")
    }

    /// Every label id present on ANY message in the thread. Bucket labels are
    /// applied thread-wide, so the previous bucket can sit on any message.
    pub fn label_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for msg in &self.messages {
            for id in &msg.label_ids {
                if !out.contains(id) {
                    out.push(id.clone());
                }
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use google_gmail1::api::{Message, MessagePart, MessagePartBody, MessagePartHeader};

    fn header(name: &str, value: &str) -> MessagePartHeader {
        MessagePartHeader {
            name: Some(name.to_string()),
            value: Some(value.to_string()),
        }
    }

    /// Build an API message with a plain-text body, for tests in this module
    /// and in the triage engine's own.
    pub(crate) fn api_message(
        id: &str,
        thread_id: &str,
        millis: i64,
        headers: Vec<(&str, &str)>,
        body: &str,
        labels: &[&str],
    ) -> Message {
        Message {
            id: Some(id.to_string()),
            thread_id: Some(thread_id.to_string()),
            internal_date: Some(millis),
            label_ids: Some(labels.iter().map(|l| l.to_string()).collect()),
            payload: Some(MessagePart {
                mime_type: Some("text/plain".to_string()),
                headers: Some(headers.iter().map(|(n, v)| header(n, v)).collect()),
                body: Some(MessagePartBody {
                    data: Some(body.as_bytes().to_vec()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_from_api_keeps_the_extended_header_set() {
        let msg = api_message(
            "m1",
            "t1",
            1_700_000_000_000,
            vec![
                ("From", "Bob <bob@x.com>"),
                ("Subject", "hello"),
                ("Date", "Mon, 1 Sep 2026 09:00:00 -0700"),
                ("Message-ID", "<abc@x.com>"),
                ("In-Reply-To", "<prev@x.com>"),
                ("References", "<root@x.com> <prev@x.com>"),
                ("Reply-To", "replies@x.com"),
                ("X-Mailer", "should be dropped"),
            ],
            "body text",
            &["INBOX"],
        );

        let parsed = TriageMessage::from_api(msg).unwrap();
        assert_eq!(parsed.subject(), "hello");
        assert_eq!(parsed.header("Message-ID"), Some("<abc@x.com>"));
        assert_eq!(parsed.header("In-Reply-To"), Some("<prev@x.com>"));
        assert_eq!(parsed.header("Reply-To"), Some("replies@x.com"));
        assert_eq!(parsed.date(), "Mon, 1 Sep 2026 09:00:00 -0700");
        assert_eq!(parsed.header("X-Mailer"), None, "unlisted header kept");
    }

    /// `Message-Id` and `Message-ID` are the same header.
    #[test]
    fn test_header_lookup_is_case_insensitive() {
        let msg = api_message(
            "m1",
            "t1",
            1,
            vec![("message-id", "<lower@x.com>")],
            "b",
            &[],
        );
        let parsed = TriageMessage::from_api(msg).unwrap();
        assert_eq!(parsed.header("Message-ID"), Some("<lower@x.com>"));
    }

    #[test]
    fn test_from_api_strips_quotes_out_of_the_body() {
        let msg = api_message(
            "m1",
            "t1",
            1,
            vec![("Subject", "s")],
            "my answer\n\nOn Mon, Bob <bob@x.com> wrote:\n> the question",
            &[],
        );
        let parsed = TriageMessage::from_api(msg).unwrap();
        assert_eq!(parsed.body, "my answer");
    }

    #[test]
    fn test_from_api_rejects_a_message_with_no_internal_date() {
        let mut msg = api_message("m1", "t1", 1, vec![], "b", &[]);
        msg.internal_date = None;
        let err = TriageMessage::from_api(msg).expect_err("missing internal_date must fail");
        assert!(format!("{:#}", err).contains("internal_date"));
    }

    #[test]
    fn test_is_from_self_matches_the_account_address() {
        let msg = api_message(
            "m1",
            "t1",
            1,
            vec![("From", "Scott Idler <scott.idler@tatari.tv>")],
            "b",
            &[],
        );
        let parsed = TriageMessage::from_api(msg).unwrap();
        assert!(parsed.is_from_self("scott.idler@tatari.tv"));
        assert!(!parsed.is_from_self("bob@x.com"));
    }

    #[test]
    fn test_thread_orders_messages_oldest_first_and_exposes_newest() {
        let thread = google_gmail1::api::Thread {
            id: Some("t1".to_string()),
            messages: Some(vec![
                api_message("m2", "t1", 2_000, vec![("Subject", "newer")], "b2", &[]),
                api_message("m1", "t1", 1_000, vec![("Subject", "older")], "b1", &[]),
            ]),
            ..Default::default()
        };

        let parsed = TriageThread::from_api(thread).unwrap();
        assert_eq!(parsed.messages[0].id, "m1");
        assert_eq!(parsed.newest().map(|m| m.id.as_str()), Some("m2"));
        assert_eq!(parsed.subject(), "newer");
        let order: Vec<&str> = parsed.newest_first().map(|m| m.id.as_str()).collect();
        assert_eq!(order, vec!["m2", "m1"]);
    }

    #[test]
    fn test_thread_label_ids_union_across_messages() {
        let thread = google_gmail1::api::Thread {
            id: Some("t1".to_string()),
            messages: Some(vec![
                api_message("m1", "t1", 1, vec![], "b", &["INBOX", "Label_1"]),
                api_message("m2", "t1", 2, vec![], "b", &["INBOX", "Label_2"]),
            ]),
            ..Default::default()
        };

        let parsed = TriageThread::from_api(thread).unwrap();
        let mut ids = parsed.label_ids();
        ids.sort();
        assert_eq!(ids, vec!["INBOX", "Label_1", "Label_2"]);
    }
}
