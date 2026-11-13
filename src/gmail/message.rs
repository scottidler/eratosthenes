use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use std::collections::{HashMap, HashSet};

use crate::cfg::label::Label;
use crate::gmail::label::LabelResolver;

pub struct GmailMessage {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub internal_date: DateTime<Utc>,
    pub headers: HashMap<String, String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub from: Vec<String>,
    pub subject: String,
}

impl GmailMessage {
    pub fn from_api(msg: google_gmail1::api::Message) -> Result<Self> {
        let headers: HashMap<String, String> = msg
            .payload
            .and_then(|p| p.headers)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| Some((h.name?, h.value?)))
            .collect();

        let internal_date_millis = msg
            .internal_date
            .ok_or_else(|| eyre!("missing internal_date"))?;

        Ok(Self {
            id: msg.id.ok_or_else(|| eyre!("message missing id"))?,
            thread_id: msg
                .thread_id
                .ok_or_else(|| eyre!("message missing thread_id"))?,
            label_ids: msg.label_ids.unwrap_or_default(),
            internal_date: DateTime::from_timestamp_millis(internal_date_millis)
                .ok_or_else(|| eyre!("invalid timestamp: {}", internal_date_millis))?,
            to: parse_address_header(headers.get("To")),
            cc: parse_address_header(headers.get("Cc")),
            from: parse_address_header(headers.get("From")),
            subject: headers.get("Subject").cloned().unwrap_or_default(),
            headers,
        })
    }

    pub fn labels(&self) -> Vec<Label> {
        self.label_ids.iter().map(|id| Label::new(id)).collect()
    }

    pub fn labels_resolved(&self, resolver: &LabelResolver) -> Vec<Label> {
        self.label_ids
            .iter()
            .map(|id| Label::new(resolver.resolve_id(id).unwrap_or(id.as_str())))
            .collect()
    }

    pub fn is_read(&self) -> bool {
        !self.label_ids.iter().any(|l| l == "UNREAD")
    }
}

pub struct GmailThread {
    pub id: String,
    /// The thread's `historyId` as of this fetch; `None` if the API omitted it.
    pub history_id: Option<u64>,
    pub messages: Vec<GmailMessage>,
    /// Malformed messages `get_thread` dropped. Nonzero means `messages` is a
    /// partial view and the derived facts must not be cached.
    pub skipped_messages: usize,
}

impl GmailThread {
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.messages.last().map(|m| m.internal_date)
    }

    pub fn label_ids(&self) -> HashSet<String> {
        self.messages
            .iter()
            .flat_map(|m| m.label_ids.iter().cloned())
            .collect()
    }

    pub fn labels(&self) -> Vec<Label> {
        self.label_ids()
            .into_iter()
            .map(|id| Label::new(&id))
            .collect()
    }

    pub fn labels_resolved(&self, resolver: &LabelResolver) -> Vec<Label> {
        self.label_ids()
            .into_iter()
            .map(|id| Label::new(resolver.resolve_id(&id).unwrap_or(id.as_str())))
            .collect()
    }

    pub fn is_read(&self) -> bool {
        self.messages.last().map(|m| m.is_read()).unwrap_or(false)
    }

    pub fn all_message_ids(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.id.clone()).collect()
    }
}

/// The addresses in a To/Cc/From header, lowercased. RFC 5322 comments
/// (`scott@x.com (Scott)`) are stripped first, so the bare address is what gets
/// compared; a kept comment made `To: me (Me)` miss the owner match.
pub(crate) fn parse_address_header(value: Option<&String>) -> Vec<String> {
    let Some(raw) = value else {
        return vec![];
    };
    strip_comments(raw)
        .split(',')
        .filter_map(|addr| {
            let addr = addr.trim();
            if let Some(start) = addr.rfind('<') {
                let end = addr.rfind('>')?;
                Some(addr[start + 1..end].to_lowercase())
            } else if addr.contains('@') {
                Some(addr.to_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// Remove RFC 5322 comments: parenthesized text, nestable, with `\` escapes,
/// outside quoted strings. An unclosed comment runs to the end of the header,
/// which drops the address after it rather than misreading it.
fn strip_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    let mut quoted = false;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if depth > 0 || quoted => {
                let escaped = chars.next();
                if depth == 0 {
                    out.push(c);
                    out.extend(escaped);
                }
            }
            '(' if !quoted => depth += 1,
            ')' if depth > 0 => depth -= 1,
            '"' if depth == 0 => {
                quoted = !quoted;
                out.push(c);
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_address_header_simple() {
        let raw = "user@example.com".to_string();
        let result = parse_address_header(Some(&raw));
        assert_eq!(result, vec!["user@example.com"]);
    }

    #[test]
    fn test_parse_address_header_with_name() {
        let raw = "John Doe <john@example.com>".to_string();
        let result = parse_address_header(Some(&raw));
        assert_eq!(result, vec!["john@example.com"]);
    }

    #[test]
    fn test_parse_address_header_multiple() {
        let raw = "alice@a.com, Bob <bob@b.com>, charlie@c.com".to_string();
        let result = parse_address_header(Some(&raw));
        assert_eq!(result, vec!["alice@a.com", "bob@b.com", "charlie@c.com"]);
    }

    #[test]
    fn test_parse_address_header_strips_trailing_comment() {
        let raw = "scott.idler@tatari.tv (Scott)".to_string();
        assert_eq!(
            parse_address_header(Some(&raw)),
            vec!["scott.idler@tatari.tv"]
        );
    }

    #[test]
    fn test_parse_address_header_strips_comments_around_angle_addr() {
        let raw = "Scott (work) <scott.idler@tatari.tv> (Scott)".to_string();
        assert_eq!(
            parse_address_header(Some(&raw)),
            vec!["scott.idler@tatari.tv"]
        );
    }

    #[test]
    fn test_parse_address_header_comment_with_comma_does_not_split() {
        let raw = "alice@a.com (Idler, Scott), bob@b.com (nested (x) y)".to_string();
        assert_eq!(
            parse_address_header(Some(&raw)),
            vec!["alice@a.com", "bob@b.com"]
        );
    }

    #[test]
    fn test_parse_address_header_keeps_parens_inside_quoted_name() {
        let raw = r#""Bob (eng)" <bob@b.com>"#.to_string();
        assert_eq!(parse_address_header(Some(&raw)), vec!["bob@b.com"]);
    }

    #[test]
    fn test_parse_address_header_unclosed_comment_drops_rest() {
        let raw = "(oops alice@a.com".to_string();
        assert!(parse_address_header(Some(&raw)).is_empty());
    }

    #[test]
    fn test_parse_address_header_none() {
        assert!(parse_address_header(None).is_empty());
    }

    #[test]
    fn test_gmail_thread_is_read() {
        let thread = GmailThread {
            id: "t1".to_string(),
            history_id: None,
            skipped_messages: 0,
            messages: vec![
                GmailMessage {
                    id: "m1".to_string(),
                    thread_id: "t1".to_string(),
                    label_ids: vec!["UNREAD".to_string()],
                    internal_date: Utc::now(),
                    headers: HashMap::new(),
                    to: vec![],
                    cc: vec![],
                    from: vec![],
                    subject: String::new(),
                },
                GmailMessage {
                    id: "m2".to_string(),
                    thread_id: "t1".to_string(),
                    label_ids: vec!["INBOX".to_string()],
                    internal_date: Utc::now(),
                    headers: HashMap::new(),
                    to: vec![],
                    cc: vec![],
                    from: vec![],
                    subject: String::new(),
                },
            ],
        };

        assert!(thread.is_read());
    }

    #[test]
    fn test_gmail_thread_last_activity() {
        let earlier = DateTime::from_timestamp_millis(1_000_000_000_000).unwrap();
        let later = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();

        let thread = GmailThread {
            id: "t1".to_string(),
            history_id: None,
            skipped_messages: 0,
            messages: vec![
                GmailMessage {
                    id: "m1".to_string(),
                    thread_id: "t1".to_string(),
                    label_ids: vec![],
                    internal_date: earlier,
                    headers: HashMap::new(),
                    to: vec![],
                    cc: vec![],
                    from: vec![],
                    subject: String::new(),
                },
                GmailMessage {
                    id: "m2".to_string(),
                    thread_id: "t1".to_string(),
                    label_ids: vec![],
                    internal_date: later,
                    headers: HashMap::new(),
                    to: vec![],
                    cc: vec![],
                    from: vec![],
                    subject: String::new(),
                },
            ],
        };

        assert_eq!(thread.last_activity(), Some(later));
    }
}
