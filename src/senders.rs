//! `eratosthenes senders`: who still reaches the inbox (inbox-gate G5).
//!
//! Read-only: one `messages.list` plus one metadata `messages.get` per inbox
//! message in the window, and nothing else. The ranking is a pure function
//! (`aggregate`) over per-message facts so it is testable without Gmail.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use eyre::{Context, Result};
use log::debug;
use serde::Serialize;

use crate::cfg::config::Config;
use crate::gmail::client::GmailClient;
use crate::gmail::message::parse_address_header;
use crate::triage::thread::{TRIAGE_HEADERS, TriageMessage};

/// The facts `aggregate` needs from one inbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderMessage {
    pub thread_id: String,
    /// Lowercased From address; empty when From carries no address.
    pub address: String,
    pub unread: bool,
    /// The triage guard's own predicate (`TriageMessage::is_automated`).
    pub automated: bool,
    /// Names of the triage buckets whose label this message carries.
    pub buckets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SenderRow {
    pub address: String,
    pub messages: usize,
    pub threads: usize,
    /// Unread messages, not threads.
    pub unread: usize,
    /// True when ANY of the sender's messages in the window is automated.
    pub automated: bool,
    /// Threads per triage bucket name. A thread carrying two bucket labels
    /// counts under both. Empty when the account has no triage block.
    pub buckets: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct AccountSenders {
    pub account: String,
    pub days: u32,
    pub senders: Vec<SenderRow>,
}

/// Group messages by sender and rank: most messages first, then most threads,
/// then address ascending, so equal senders always come out in one order.
/// A sender's bucket split counts each of its threads once per bucket label
/// that thread carries on any of the sender's messages.
pub fn aggregate(messages: &[SenderMessage], limit: usize) -> Vec<SenderRow> {
    struct Acc<'a> {
        messages: usize,
        unread: usize,
        automated: bool,
        threads: BTreeSet<&'a str>,
        thread_buckets: BTreeSet<(&'a str, &'a str)>,
    }

    let mut by_sender: HashMap<&str, Acc> = HashMap::new();
    for msg in messages {
        let acc = by_sender.entry(msg.address.as_str()).or_insert(Acc {
            messages: 0,
            unread: 0,
            automated: false,
            threads: BTreeSet::new(),
            thread_buckets: BTreeSet::new(),
        });
        acc.messages += 1;
        acc.unread += usize::from(msg.unread);
        acc.automated |= msg.automated;
        acc.threads.insert(msg.thread_id.as_str());
        for bucket in &msg.buckets {
            acc.thread_buckets
                .insert((msg.thread_id.as_str(), bucket.as_str()));
        }
    }

    let mut rows: Vec<SenderRow> = by_sender
        .into_iter()
        .map(|(address, acc)| {
            let mut buckets: BTreeMap<String, usize> = BTreeMap::new();
            for (_, bucket) in acc.thread_buckets {
                *buckets.entry(bucket.to_string()).or_default() += 1;
            }
            SenderRow {
                address: address.to_string(),
                messages: acc.messages,
                threads: acc.threads.len(),
                unread: acc.unread,
                automated: acc.automated,
                buckets,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.messages
            .cmp(&a.messages)
            .then(b.threads.cmp(&a.threads))
            .then(a.address.cmp(&b.address))
    });
    rows.truncate(limit);
    rows
}

/// Fetch the account's inbox messages from the last `days` days and rank their
/// senders. GET requests only.
pub async fn collect(
    client: &mut GmailClient,
    config: &Config,
    days: u32,
    limit: usize,
) -> Result<Vec<SenderRow>> {
    debug!("senders::collect: days={}, limit={}", days, limit);
    client.set_metadata_headers(TRIAGE_HEADERS.iter().map(|h| h.to_string()).collect());

    let bucket_labels: Vec<(&str, &str)> = config
        .triage
        .as_ref()
        .map(|t| {
            t.buckets
                .iter()
                .map(|b| (b.label.as_str(), b.name.as_str()))
                .collect()
        })
        .unwrap_or_default();

    let query = format!("in:inbox newer_than:{days}d");
    let refs = client
        .search_message_refs(&query)
        .await
        .context("listing inbox messages")?;

    let mut facts: Vec<SenderMessage> = Vec::with_capacity(refs.len());
    for msg_ref in &refs {
        let msg = client
            .get_message(&msg_ref.id)
            .await
            .with_context(|| format!("fetching message {}", msg_ref.id))?;
        let label_names: Vec<&str> = msg
            .label_ids
            .iter()
            .filter_map(|id| client.resolver.resolve_id(id))
            .collect();
        let buckets = bucket_labels
            .iter()
            .filter(|(label, _)| label_names.contains(label))
            .map(|(_, name)| name.to_string())
            .collect();
        let address = parse_address_header(msg.headers.get("From"))
            .into_iter()
            .next()
            .unwrap_or_default();
        facts.push(SenderMessage {
            thread_id: msg.thread_id.clone(),
            address,
            unread: !msg.is_read(),
            automated: TriageMessage::from_metadata(&msg).is_automated(),
            buckets,
        });
    }
    debug!("senders::collect: {} messages fetched", facts.len());
    Ok(aggregate(&facts, limit))
}

/// yaml or json text for the reports; one document either way.
pub fn render(reports: &[AccountSenders], json: bool) -> Result<String> {
    if json {
        Ok(serde_json::to_string_pretty(reports)?)
    } else {
        Ok(serde_yaml::to_string(reports)?)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn msg(
        thread: &str,
        from: &str,
        unread: bool,
        automated: bool,
        buckets: &[&str],
    ) -> SenderMessage {
        SenderMessage {
            thread_id: thread.to_string(),
            address: from.to_string(),
            unread,
            automated,
            buckets: buckets.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn ranks_by_messages_then_threads_then_address() {
        let msgs = vec![
            msg("t1", "b@x.com", false, false, &[]),
            msg("t2", "b@x.com", false, false, &[]),
            msg("t3", "a@x.com", false, false, &[]),
            msg("t3", "a@x.com", false, false, &[]),
            msg("t4", "c@x.com", false, false, &[]),
            msg("t5", "c@x.com", false, false, &[]),
            msg("t6", "c@x.com", false, false, &[]),
        ];
        let rows = aggregate(&msgs, 10);
        let order: Vec<&str> = rows.iter().map(|r| r.address.as_str()).collect();
        // c has 3 messages; a and b tie on messages, b has 2 threads to a's 1.
        assert_eq!(order, vec!["c@x.com", "b@x.com", "a@x.com"]);
    }

    #[test]
    fn full_ties_break_on_address() {
        let msgs = vec![
            msg("t1", "z@x.com", false, false, &[]),
            msg("t2", "m@x.com", false, false, &[]),
            msg("t3", "a@x.com", false, false, &[]),
        ];
        let first = aggregate(&msgs, 10);
        let mut reversed = msgs.clone();
        reversed.reverse();
        assert_eq!(first, aggregate(&reversed, 10), "input order never matters");
        assert_eq!(first[0].address, "a@x.com");
    }

    #[test]
    fn limit_truncates_after_ranking() {
        let msgs = vec![
            msg("t1", "a@x.com", false, false, &[]),
            msg("t2", "b@x.com", false, false, &[]),
            msg("t3", "b@x.com", false, false, &[]),
        ];
        let rows = aggregate(&msgs, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].address, "b@x.com");
        assert!(aggregate(&msgs, 0).is_empty());
    }

    #[test]
    fn counts_unread_messages_and_distinct_threads() {
        let msgs = vec![
            msg("t1", "a@x.com", true, false, &[]),
            msg("t1", "a@x.com", false, false, &[]),
            msg("t2", "a@x.com", true, false, &[]),
        ];
        let row = &aggregate(&msgs, 10)[0];
        assert_eq!(row.messages, 3);
        assert_eq!(row.threads, 2);
        assert_eq!(row.unread, 2);
    }

    #[test]
    fn automated_when_any_message_is() {
        let msgs = vec![
            msg("t1", "a@x.com", false, false, &[]),
            msg("t2", "a@x.com", false, true, &[]),
            msg("t3", "h@x.com", false, false, &[]),
        ];
        let rows = aggregate(&msgs, 10);
        assert!(
            rows.iter()
                .find(|r| r.address == "a@x.com")
                .unwrap()
                .automated
        );
        assert!(
            !rows
                .iter()
                .find(|r| r.address == "h@x.com")
                .unwrap()
                .automated
        );
    }

    #[test]
    fn bucket_split_counts_each_thread_once_per_bucket() {
        let msgs = vec![
            msg("t1", "a@x.com", false, false, &["noise"]),
            msg("t1", "a@x.com", false, false, &["noise"]),
            msg("t2", "a@x.com", false, false, &["noise", "receipts"]),
            msg("t3", "a@x.com", false, false, &[]),
        ];
        let row = &aggregate(&msgs, 10)[0];
        assert_eq!(row.buckets.get("noise"), Some(&2));
        assert_eq!(row.buckets.get("receipts"), Some(&1));
        assert_eq!(row.buckets.len(), 2);
    }

    #[test]
    fn no_buckets_gives_empty_split() {
        let rows = aggregate(&[msg("t1", "a@x.com", false, false, &[])], 10);
        assert!(rows[0].buckets.is_empty());
    }

    #[test]
    fn render_json_and_yaml_carry_kebab_fields() {
        let reports = vec![AccountSenders {
            account: "tatari".into(),
            days: 7,
            senders: aggregate(&[msg("t1", "a@x.com", true, false, &["noise"])], 10),
        }];
        let json = render(&reports, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["senders"][0]["address"], "a@x.com");
        assert_eq!(parsed[0]["senders"][0]["buckets"]["noise"], 1);
        let yaml = render(&reports, false).unwrap();
        assert!(yaml.contains("account: tatari"), "{yaml}");
    }
}
