//! The `format=full` thread shape triage works on.
//!
//! Deliberately a separate type from `GmailMessage`/`GmailThread`: those model
//! the metadata fetch the aging engine runs on every inbox thread every five
//! minutes, and giving them an always-`None` body field would put a triage
//! concern in the hot path of a subsystem that has no use for it.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use log::{debug, trace};

use crate::gmail::message::parse_address_header;
use crate::triage::body::{extract_body, strip_quotes};

/// The header set the design doc's Data Plumbing section extends to. The
/// aging engine's default set is To/Cc/From/Subject; the five additions are
/// the RFC822 reply builder's inputs (Phase 7) and the thread ordering signal.
/// The last four are the automated-sender facts the archive decision reads
/// (`TriageMessage::is_automated`); `PayloadMessage` never carries them, so
/// they stay out of the prompt.
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
    "List-Id",
    "List-Unsubscribe",
    "Auto-Submitted",
    "Precedence",
];

/// Gmail ignores dots in the local part of these domains, so
/// `scott.a.idler@gmail.com` and `scottaidler@gmail.com` are one mailbox.
const DOTLESS_DOMAINS: &[&str] = &["gmail.com", "googlemail.com"];

/// `Precedence` values that mark mail as sent to many.
const BULK_PRECEDENCE: &[&str] = &["bulk", "list", "junk"];

/// One mailbox, one string: lowercase, drop a `+tag`, and drop dots in the
/// local part for `gmail.com` | `googlemail.com`. `is_from_self`,
/// `is_solely_to` and the archive guard all compare through this, so they
/// cannot disagree on whether an address is the account owner.
pub fn normalize_address(addr: &str) -> String {
    let lower = addr.trim().to_lowercase();
    let Some((local, domain)) = lower.rsplit_once('@') else {
        return lower;
    };
    let local = local.split_once('+').map_or(local, |(base, _)| base);
    let local = if DOTLESS_DOMAINS.contains(&domain) {
        local.replace('.', "")
    } else {
        local.to_string()
    };
    format!("{}@{}", local, domain)
}

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

    /// A body-less `TriageMessage` from a metadata fetch, so a caller that never
    /// reads bodies (`senders`) asks the guard's own `is_automated` instead of
    /// keeping a second copy of the predicate.
    pub fn from_metadata(msg: &crate::gmail::message::GmailMessage) -> Self {
        let wanted: Vec<String> = TRIAGE_HEADERS.iter().map(|h| h.to_lowercase()).collect();
        let headers = msg
            .headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.to_lowercase();
                wanted.contains(&name).then(|| (name, value.clone()))
            })
            .collect();
        Self {
            id: msg.id.clone(),
            label_ids: msg.label_ids.clone(),
            internal_date: msg.internal_date,
            headers,
            body: String::new(),
        }
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
        let me = normalize_address(self_address);
        let from = self.header("From").map(|s| s.to_string());
        parse_address_header(from.as_ref())
            .iter()
            .any(|addr| normalize_address(addr) == me)
    }

    /// True only when `To` is exactly the account owner and `Cc` is empty. A
    /// list, group alias, extra recipient, or Bcc (owner absent from `To`) all
    /// fail, and so does a missing `To`: fail closed.
    pub fn is_solely_to(&self, self_address: &str) -> bool {
        let to = self.header("To").map(|s| s.to_string());
        let cc = self.header("Cc").map(|s| s.to_string());
        let to = parse_address_header(to.as_ref());
        let cc = parse_address_header(cc.as_ref());
        let sole = cc.is_empty()
            && to.len() == 1
            && normalize_address(&to[0]) == normalize_address(self_address);
        trace!(
            "is_solely_to: id={}, to={:?}, cc={:?}, sole={}",
            self.id, to, cc, sole
        );
        sole
    }

    /// The archive guard's addressing test, which fails closed the OTHER way
    /// from `is_solely_to`: a To that is missing or yields no address counts
    /// as addressed to me. `is_solely_to` gates drafting, where "not sole" is
    /// the safe answer; for archiving the safe answer is "keep it".
    fn is_solely_to_or_has_no_to(&self, self_address: &str) -> bool {
        let to = self.header("To").map(|s| s.to_string());
        parse_address_header(to.as_ref()).is_empty() || self.is_solely_to(self_address)
    }

    /// True when a header alone says a machine sent this message. Any of:
    /// `List-Id` present, `List-Unsubscribe` present, `Auto-Submitted` present
    /// and not `no`, `Precedence` is `bulk` | `list` | `junk`, or the From
    /// local part, with `-` `_` `.` removed, contains `noreply` or
    /// `donotreply` or equals `mailerdaemon`. The body is never read.
    pub fn is_automated(&self) -> bool {
        let list_id = self.header("List-Id").is_some();
        let list_unsubscribe = self.header("List-Unsubscribe").is_some();
        let auto_submitted = self
            .header("Auto-Submitted")
            .is_some_and(|v| !v.trim().eq_ignore_ascii_case("no"));
        let precedence = self
            .header("Precedence")
            .is_some_and(|v| BULK_PRECEDENCE.contains(&v.trim().to_lowercase().as_str()));
        let from_local = self.sender_local_part();
        let robot_from = from_local.contains("noreply")
            || from_local.contains("donotreply")
            || from_local == "mailerdaemon";
        let automated = list_id || list_unsubscribe || auto_submitted || precedence || robot_from;
        trace!(
            "is_automated: id={}, list_id={}, list_unsubscribe={}, auto_submitted={}, precedence={}, robot_from={}, automated={}",
            self.id, list_id, list_unsubscribe, auto_submitted, precedence, robot_from, automated
        );
        automated
    }

    /// The From address's local part, lowercased, with `-` `_` `.` removed.
    /// Empty when From is missing or carries no address.
    fn sender_local_part(&self) -> String {
        let from = self.header("From").map(|s| s.to_string());
        parse_address_header(from.as_ref())
            .first()
            .and_then(|addr| addr.rsplit_once('@').map(|(local, _)| local.to_string()))
            .unwrap_or_default()
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | '.'))
            .collect()
    }

    fn is_draft(&self) -> bool {
        self.label_ids.iter().any(|l| l == DRAFT_LABEL)
    }

    fn is_pinned(&self) -> bool {
        self.label_ids
            .iter()
            .any(|l| PIN_LABELS.contains(&l.as_str()))
    }
}

const DRAFT_LABEL: &str = "DRAFT";

/// Gmail system labels that mean Scott, or a Star/Flag filter, already said
/// this thread matters.
const PIN_LABELS: &[&str] = &["STARRED", "IMPORTANT"];

/// Why the archive decision keeps a thread in INBOX whatever its bucket says.
/// Rows 1 to 3 of the design doc's archive decision table, in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// A message carries `STARRED` or `IMPORTANT`.
    Pinned,
    /// Every non-draft message is from me: nobody else is in the thread.
    NoInbound,
    /// An inbound message from a non-automated sender is solely to me, or has
    /// no parseable To.
    SoleHuman,
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

    /// The answered rule: the newest non-draft message is from me. Drafts are
    /// skipped because a draft is FROM me and newest, and reading it as my
    /// answer would strip a bucket label the run after the draft was made.
    /// `false` for a thread with no non-draft message.
    pub fn is_answered(&self, self_address: &str) -> bool {
        self.messages
            .iter()
            .rev()
            .find(|m| !m.is_draft())
            .is_some_and(|newest| newest.is_from_self(self_address))
    }

    /// Non-draft messages not from me, oldest first. Distinct from
    /// `plan_refresh`'s newest message, which counts my own replies.
    pub fn inbound<'a>(
        &'a self,
        self_address: &'a str,
    ) -> impl Iterator<Item = &'a TriageMessage> + 'a {
        self.messages
            .iter()
            .filter(move |m| !m.is_draft() && !m.is_from_self(self_address))
    }

    /// The first archive-table row (1 to 3) that keeps this thread in INBOX,
    /// or `None` when the bucket gets to decide. Row 3 reads every inbound
    /// message, not only the newest, so an older human mail to me alone still
    /// holds the thread under a newer bot notice.
    pub fn protection(&self, self_address: &str) -> Option<Protection> {
        let protection = if self.messages.iter().any(|m| m.is_pinned()) {
            Some(Protection::Pinned)
        } else if self.inbound(self_address).next().is_none() {
            Some(Protection::NoInbound)
        } else if self
            .inbound(self_address)
            .any(|m| !m.is_automated() && m.is_solely_to_or_has_no_to(self_address))
        {
            Some(Protection::SoleHuman)
        } else {
            None
        };
        debug!(
            "protection: thread={}, messages={}, protection={:?}",
            self.id,
            self.messages.len(),
            protection
        );
        protection
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
    // ---- normalize_address -------------------------------------------------

    #[test]
    fn normalize_address_drops_a_plus_tag() {
        assert_eq!(
            normalize_address("scott.idler+news@tatari.tv"),
            "scott.idler@tatari.tv"
        );
    }

    #[test]
    fn normalize_address_drops_dots_for_gmail() {
        assert_eq!(
            normalize_address("scott.a.idler@gmail.com"),
            "scottaidler@gmail.com"
        );
    }

    #[test]
    fn normalize_address_drops_dots_for_googlemail() {
        assert_eq!(
            normalize_address("scott.a.idler+x@googlemail.com"),
            "scottaidler@googlemail.com"
        );
    }

    #[test]
    fn normalize_address_keeps_dots_outside_gmail() {
        assert_eq!(
            normalize_address("scott.idler@tatari.tv"),
            "scott.idler@tatari.tv"
        );
    }

    #[test]
    fn normalize_address_lowercases() {
        assert_eq!(
            normalize_address(" Scott.A.Idler@GMail.COM "),
            "scottaidler@gmail.com"
        );
    }

    #[test]
    fn normalize_address_without_at_is_only_lowercased() {
        assert_eq!(normalize_address("Not.An+Address"), "not.an+address");
    }

    // ---- fixtures for the guard --------------------------------------------

    const ME: &str = "scott.idler@tatari.tv";

    fn msg(id: &str, millis: i64, headers: Vec<(&str, &str)>, labels: &[&str]) -> TriageMessage {
        TriageMessage::from_api(api_message(id, "t1", millis, headers, "b", labels)).unwrap()
    }

    fn human(headers: &[(&'static str, &'static str)]) -> TriageMessage {
        let mut all = vec![
            ("From", "Nick Andolina <nick.andolina@tatari.tv>"),
            ("To", ME),
        ];
        for (name, value) in headers {
            all.retain(|(n, _)| n != name);
            all.push((name, value));
        }
        msg("m1", 1_000, all, &["INBOX"])
    }

    fn thread_of(messages: Vec<TriageMessage>) -> TriageThread {
        TriageThread {
            id: "t1".to_string(),
            messages,
        }
    }

    // ---- is_from_self / is_solely_to normalization -------------------------

    #[test]
    fn is_from_self_matches_through_normalization() {
        let m = msg(
            "m1",
            1,
            vec![("From", "Scott <Scott.A.Idler+news@Gmail.com>")],
            &[],
        );
        assert!(m.is_from_self("scott.a.idler@gmail.com"));
        assert!(m.is_from_self("scottaidler@gmail.com"));
        assert!(!m.is_from_self("scott.b.idler@gmail.com"));
    }

    #[test]
    fn is_solely_to_matches_through_normalization() {
        let m = msg(
            "m1",
            1,
            vec![("From", "a@x.com"), ("To", "ScottAIdler+tag@gmail.com")],
            &[],
        );
        assert!(m.is_solely_to("scott.a.idler@gmail.com"));
        assert!(!m.is_solely_to("other@gmail.com"));
    }

    // ---- is_automated: one positive and one negative per fact --------------

    #[test]
    fn list_id_marks_sender_automated() {
        assert!(human(&[("List-Id", "<eng.tatari.tv>")]).is_automated());
    }

    #[test]
    fn missing_list_id_does_not_mark_sender_automated() {
        assert!(!human(&[]).is_automated());
    }

    #[test]
    fn list_unsubscribe_marks_sender_automated() {
        assert!(human(&[("List-Unsubscribe", "<mailto:u@x.com>")]).is_automated());
    }

    #[test]
    fn missing_list_unsubscribe_does_not_mark_sender_automated() {
        assert!(!human(&[("Reply-To", "nick.andolina@tatari.tv")]).is_automated());
    }

    #[test]
    fn auto_submitted_marks_sender_automated() {
        assert!(human(&[("Auto-Submitted", "auto-generated")]).is_automated());
    }

    #[test]
    fn auto_submitted_no_does_not_mark_sender_automated() {
        assert!(!human(&[("Auto-Submitted", " No ")]).is_automated());
    }

    #[test]
    fn precedence_bulk_list_junk_mark_sender_automated() {
        for value in ["bulk", "List", " junk "] {
            assert!(
                human(&[("Precedence", value)]).is_automated(),
                "Precedence: {value}"
            );
        }
    }

    #[test]
    fn precedence_first_class_does_not_mark_sender_automated() {
        assert!(!human(&[("Precedence", "first-class")]).is_automated());
    }

    #[test]
    fn robot_from_local_part_marks_sender_automated() {
        for from in [
            "Greenhouse <no-reply@greenhouse.io>",
            "do_not.reply@x.com",
            "NoReply@x.com",
            "MAILER-DAEMON@x.com",
        ] {
            assert!(human(&[("From", from)]).is_automated(), "From: {from}");
        }
    }

    #[test]
    fn human_from_local_part_does_not_mark_sender_automated() {
        for from in [
            "reply@x.com",
            "mailer-daemon-ops@x.com",
            "bob@noreply.example.com",
        ] {
            assert!(!human(&[("From", from)]).is_automated(), "From: {from}");
        }
    }

    /// Measured 2026-11-11: the top `needs-reply` senders that should never
    /// hold a place there, and a colleague who should.
    #[test]
    fn greenhouse_no_reply_is_automated() {
        assert!(human(&[("From", "Greenhouse <no-reply@greenhouse.io>")]).is_automated());
    }

    #[test]
    fn drive_shares_noreply_is_automated() {
        assert!(
            human(&[("From", "Google Drive <drive-shares-dm-noreply@google.com>")]).is_automated()
        );
    }

    #[test]
    fn colleague_without_list_headers_is_not_automated() {
        let m = human(&[("From", "Nick Andolina <nick.andolina@tatari.tv>")]);
        assert!(!m.is_automated());
    }

    // ---- inbound -----------------------------------------------------------

    #[test]
    fn inbound_skips_my_own_messages_and_drafts() {
        let thread = thread_of(vec![
            msg("m1", 1, vec![("From", "bob@x.com"), ("To", ME)], &["INBOX"]),
            msg("m2", 2, vec![("From", ME), ("To", "bob@x.com")], &["SENT"]),
            msg("m3", 3, vec![("From", "bob@x.com")], &["DRAFT"]),
        ]);
        let ids: Vec<&str> = thread.inbound(ME).map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m1"]);
    }

    #[test]
    fn inbound_is_empty_when_every_message_is_mine() {
        let thread = thread_of(vec![msg(
            "m1",
            1,
            vec![("From", ME), ("To", "bob@x.com")],
            &["SENT"],
        )]);
        assert_eq!(thread.inbound(ME).count(), 0);
    }

    // ---- protection: one positive and one negative per variant -------------

    fn bot_to_list(id: &str, millis: i64, labels: &[&str]) -> TriageMessage {
        msg(
            id,
            millis,
            vec![
                ("From", "no-reply@greenhouse.io"),
                ("To", "eng@tatari.tv"),
                ("List-Id", "<eng.tatari.tv>"),
            ],
            labels,
        )
    }

    #[test]
    fn starred_or_important_message_pins_thread() {
        for pin in ["STARRED", "IMPORTANT"] {
            let thread = thread_of(vec![
                bot_to_list("m1", 1, &["INBOX", pin]),
                bot_to_list("m2", 2, &["INBOX"]),
            ]);
            assert_eq!(thread.protection(ME), Some(Protection::Pinned), "{pin}");
        }
    }

    #[test]
    fn unpinned_bot_thread_is_not_protected() {
        let thread = thread_of(vec![bot_to_list("m1", 1, &["INBOX", "UNREAD"])]);
        assert_eq!(thread.protection(ME), None);
    }

    #[test]
    fn thread_with_only_my_messages_has_no_inbound() {
        let thread = thread_of(vec![
            msg("m1", 1, vec![("From", ME), ("To", "bob@x.com")], &["SENT"]),
            msg("m2", 2, vec![("From", "bob@x.com")], &["DRAFT"]),
        ]);
        assert_eq!(thread.protection(ME), Some(Protection::NoInbound));
    }

    #[test]
    fn thread_with_an_inbound_bot_message_is_not_no_inbound() {
        let thread = thread_of(vec![
            msg(
                "m1",
                1,
                vec![("From", ME), ("To", "eng@tatari.tv")],
                &["SENT"],
            ),
            bot_to_list("m2", 2, &["INBOX"]),
        ]);
        assert_eq!(thread.protection(ME), None);
    }

    #[test]
    fn human_message_solely_to_me_protects_thread() {
        let thread = thread_of(vec![human(&[])]);
        assert_eq!(thread.protection(ME), Some(Protection::SoleHuman));
    }

    /// Implementation audit F2: an RFC 5322 comment on the owner's address
    /// (`To: me (Me)`) must not defeat the owner match and let human mail archive.
    #[test]
    fn human_message_to_me_with_address_comment_protects_thread() {
        let thread = thread_of(vec![human(&[("To", "scott.idler@tatari.tv (Scott)")])]);
        assert_eq!(thread.protection(ME), Some(Protection::SoleHuman));
    }

    #[test]
    fn human_message_with_cc_does_not_protect_thread() {
        let thread = thread_of(vec![human(&[("Cc", "boss@tatari.tv")])]);
        assert_eq!(thread.protection(ME), None);
    }

    #[test]
    fn automated_message_solely_to_me_does_not_protect_thread() {
        let thread = thread_of(vec![human(&[("From", "no-reply@greenhouse.io")])]);
        assert_eq!(thread.protection(ME), None);
    }

    /// Row 3 fails closed: `is_solely_to` reads a missing To as "not sole"
    /// because it gates drafting. For archiving that would flip the safety.
    #[test]
    fn non_automated_message_without_to_protects_thread() {
        let m = msg(
            "m1",
            1,
            vec![("From", "Nick Andolina <nick.andolina@tatari.tv>")],
            &["INBOX"],
        );
        assert!(!m.is_solely_to(ME), "drafting semantics stay fail-closed");
        assert_eq!(
            thread_of(vec![m]).protection(ME),
            Some(Protection::SoleHuman)
        );
    }

    #[test]
    fn non_automated_message_with_unparseable_to_protects_thread() {
        let thread = thread_of(vec![human(&[("To", "undisclosed-recipients:;")])]);
        assert_eq!(thread.protection(ME), Some(Protection::SoleHuman));
    }

    #[test]
    fn automated_message_without_to_does_not_protect_thread() {
        let m = msg(
            "m1",
            1,
            vec![
                ("From", "x@y.com"),
                ("List-Unsubscribe", "<mailto:u@y.com>"),
            ],
            &["INBOX"],
        );
        assert_eq!(thread_of(vec![m]).protection(ME), None);
    }

    #[test]
    fn older_sole_human_message_protects_thread_under_newer_bot_message() {
        let mut older = human(&[]);
        older.internal_date = DateTime::from_timestamp_millis(1).unwrap();
        let thread = thread_of(vec![older, bot_to_list("m2", 2, &["INBOX"])]);
        assert_eq!(thread.protection(ME), Some(Protection::SoleHuman));
    }

    // ---- is_answered -------------------------------------------------------

    #[test]
    fn thread_whose_newest_message_is_mine_is_answered() {
        let thread = thread_of(vec![
            msg("m1", 1, vec![("From", "bob@x.com"), ("To", ME)], &["INBOX"]),
            msg("m2", 2, vec![("From", ME), ("To", "bob@x.com")], &["SENT"]),
        ]);
        assert!(thread.is_answered(ME));
    }

    #[test]
    fn thread_whose_newest_message_is_inbound_is_not_answered() {
        let thread = thread_of(vec![
            msg("m1", 1, vec![("From", ME), ("To", "bob@x.com")], &["SENT"]),
            msg("m2", 2, vec![("From", "bob@x.com"), ("To", ME)], &["INBOX"]),
        ]);
        assert!(!thread.is_answered(ME));
    }

    #[test]
    fn my_own_draft_is_not_an_answer() {
        let thread = thread_of(vec![
            msg("m1", 1, vec![("From", "bob@x.com"), ("To", ME)], &["INBOX"]),
            msg("m2", 2, vec![("From", ME), ("To", "bob@x.com")], &["DRAFT"]),
        ]);
        assert!(!thread.is_answered(ME));
    }

    #[test]
    fn thread_of_drafts_only_is_not_answered() {
        let thread = thread_of(vec![msg(
            "m1",
            1,
            vec![("From", ME), ("To", "bob@x.com")],
            &["DRAFT"],
        )]);
        assert!(!thread.is_answered(ME));
    }
}
