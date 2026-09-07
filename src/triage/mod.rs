//! The triage engine: classify new inbox threads into config-defined buckets
//! with one batched LLM call, then record the result as Gmail labels.
//!
//! Control flow is deterministic Rust; the LLM is called exactly where judgment
//! is needed and nowhere else. Idempotency, the cap, dry-run, and every
//! mutation decision live here, not in a prompt.

pub mod body;
pub mod classify;
pub mod claude;
pub mod thread;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use log::{debug, info, warn};

use crate::cfg::config::Config;
use crate::cfg::triage::{TriageBucket, TriageConfig};
use crate::gmail::client::GmailClient;
use crate::gmail::label::{LabelResolver, LabelVisibility, create_label_if_missing};
use crate::triage::claude::{ClaudeCli, TRIAGE_TIMEOUT};
use crate::triage::thread::TriageThread;

/// Message-level idempotency marker. Message-level, deliberately: Gmail labels
/// attach to messages and new messages never inherit them, so
/// `-label:llm/seen` finds both brand-new threads AND new inbound messages
/// landing in an already-classified thread. Scott's own replies carry SENT, not
/// INBOX, so they never re-trigger.
pub const SEEN_LABEL: &str = "llm/seen";

/// The candidate query. Threads with no new messages match nothing, which is
/// what makes a rerun a no-op.
pub const CANDIDATE_QUERY: &str = "in:inbox -label:llm/seen";

/// A candidate message reduced to what the client-side sort needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMessage {
    pub thread_id: String,
    pub internal_date: DateTime<Utc>,
}

/// The capped, newest-first candidate set plus what the cap cost.
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub thread_ids: Vec<String>,
    pub total_threads: usize,
}

impl Selection {
    pub fn dropped(&self) -> usize {
        self.total_threads.saturating_sub(self.thread_ids.len())
    }
}

/// Collapse candidate MESSAGES into distinct THREADS, newest first, capped.
///
/// Sorted client-side on purpose: `messages.list` ordering is undocumented, so
/// relying on it would be an environmental assumption instead of a decision. A
/// thread's rank is its NEWEST candidate message, so a thread that just got a
/// reply outranks one whose only unseen message is old.
pub fn select_candidates(candidates: &[CandidateMessage], cap: usize) -> Selection {
    debug!(
        "select_candidates: candidates={}, cap={}",
        candidates.len(),
        cap
    );

    let mut newest: HashMap<&str, DateTime<Utc>> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for candidate in candidates {
        let id = candidate.thread_id.as_str();
        match newest.get_mut(id) {
            Some(existing) => {
                if candidate.internal_date > *existing {
                    *existing = candidate.internal_date;
                }
            }
            None => {
                newest.insert(id, candidate.internal_date);
                order.push(id);
            }
        }
    }

    // Ties break on the discovery order rather than arbitrarily, so a run over
    // an unchanged mailbox selects the same set every time.
    order.sort_by(|a, b| newest.get(b).cmp(&newest.get(a)).then_with(|| a.cmp(b)));

    let total_threads = order.len();
    let thread_ids: Vec<String> = order.into_iter().take(cap).map(|s| s.to_string()).collect();

    Selection {
        thread_ids,
        total_threads,
    }
}

/// The loud line for a cap that bit, or `None` when it did not. A message
/// rather than a bare bool so the numbers that justify raising `max-threads`
/// are in the journal, not just the fact that something was dropped.
pub fn cap_message(selection: &Selection, cap: usize) -> Option<String> {
    if selection.dropped() == 0 {
        return None;
    }
    Some(format!(
        "max-threads cap HIT: {} candidate threads, classifying the newest {}, \
{} left unseen for the next run (raise max-threads if this repeats)",
        selection.total_threads,
        cap,
        selection.dropped()
    ))
}

/// One thread's label mutation: exactly one `threads.modify` so a thread is
/// never half-written.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadWrite {
    pub thread_id: String,
    pub bucket: String,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// Build the add/remove sets for one classified thread: add the new bucket plus
/// the seen marker, remove any OTHER `llm/*` bucket the thread already carries.
///
/// Removing the previous bucket is what makes reclassification work: a noise
/// thread a human replies into becomes needs-reply, and it must not end up
/// carrying both.
pub fn plan_write(
    thread: &TriageThread,
    bucket: &TriageBucket,
    buckets: &[TriageBucket],
    resolver: &LabelResolver,
    seen_label: &str,
) -> Result<ThreadWrite> {
    let bucket_id = resolver
        .resolve_name(&bucket.label)
        .ok_or_else(|| eyre::eyre!("label '{}' is not in the resolver", bucket.label))?
        .to_string();
    let seen_id = resolver
        .resolve_name(seen_label)
        .ok_or_else(|| eyre::eyre!("label '{}' is not in the resolver", seen_label))?
        .to_string();

    let present = thread.label_ids();
    let remove: Vec<String> = buckets
        .iter()
        .filter(|other| other.label != bucket.label)
        .filter_map(|other| resolver.resolve_name(&other.label))
        .filter(|id| present.iter().any(|p| p == id))
        .map(|id| id.to_string())
        .collect();

    Ok(ThreadWrite {
        thread_id: thread.id.clone(),
        bucket: bucket.name.clone(),
        add: vec![bucket_id, seen_id],
        remove,
    })
}

/// Every label this run needs that the mailbox does not already have.
///
/// A dry run plans NOTHING, so the create loop has nothing to create. Zero
/// mutations is a property of the PLAN rather than a branch inside the write
/// loop, which is what keeps a later edit from reintroducing a write that a
/// dry run performs. Note this is deliberately stricter than `run --dry-run`,
/// which does create missing labels.
pub fn plan_labels(
    triage: &TriageConfig,
    resolver: &LabelResolver,
    dry_run: bool,
) -> Vec<(String, LabelVisibility)> {
    if dry_run {
        return Vec::new();
    }

    let mut needed: Vec<(String, LabelVisibility)> = triage
        .buckets
        .iter()
        .map(|b| (b.label.clone(), LabelVisibility::Shown))
        .collect();
    // Hidden, like the message-filter marker: the seen marker lands on nearly
    // every message, and shown it would put a chip on all of them.
    needed.push((SEEN_LABEL.to_string(), LabelVisibility::Hidden));

    needed
        .into_iter()
        .filter(|(name, _)| resolver.resolve_name(name).is_none())
        .collect()
}

/// Every thread mutation this run will perform. Same rule as `plan_labels`: a
/// dry run plans nothing, so it mutates nothing.
pub fn plan_writes(
    threads: &[TriageThread],
    classification: &classify::Classification,
    triage: &TriageConfig,
    resolver: &LabelResolver,
    dry_run: bool,
) -> Result<Vec<ThreadWrite>> {
    if dry_run {
        return Ok(Vec::new());
    }

    let by_name: HashMap<&str, &TriageBucket> = triage
        .buckets
        .iter()
        .map(|b| (b.name.as_str(), b))
        .collect();
    let by_id: HashMap<&str, &TriageThread> = threads.iter().map(|t| (t.id.as_str(), t)).collect();

    let mut writes = Vec::new();
    for (thread_id, bucket_name) in &classification.assignments {
        let (Some(bucket), Some(thread)) = (
            by_name.get(bucket_name.as_str()),
            by_id.get(thread_id.as_str()),
        ) else {
            warn!(
                "classification for thread {} referenced unknown data; skipping",
                thread_id
            );
            continue;
        };
        writes.push(plan_write(
            thread,
            bucket,
            &triage.buckets,
            resolver,
            SEEN_LABEL,
        )?);
    }
    Ok(writes)
}

/// Classify and label one account's new inbox threads.
pub async fn execute(
    client: &mut GmailClient,
    config: &Config,
    prefix: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(triage) = config.triage.as_ref() else {
        info!("{}no triage block; skipping triage", prefix);
        println!("{}no triage config; skipping", prefix);
        return Ok(());
    };

    debug!(
        "{}triage::execute: dry_run={}, max_threads={}, body_chars={}, buckets={}",
        prefix,
        dry_run,
        triage.max_threads,
        triage.body_chars,
        triage.buckets.len()
    );

    if dry_run {
        // Stricter than `run --dry-run`, which creates missing labels. A triage
        // dry run is the eval gate's instrument: it must be able to run against
        // a live mailbox and leave NOTHING behind, label creation included.
        info!(
            "{}=== DRY RUN - zero Gmail mutations, labels included ===",
            prefix
        );
    }

    ensure_triage_labels(client, triage, prefix, dry_run).await?;

    let selection = discover_candidates(client, triage, prefix).await?;
    if selection.thread_ids.is_empty() {
        info!("{}no new inbox messages to classify", prefix);
        println!("{}Triage: nothing to classify", prefix);
        return Ok(());
    }

    let mut threads: Vec<TriageThread> = Vec::new();
    for id in &selection.thread_ids {
        let raw = client
            .get_thread_full(id)
            .await
            .with_context(|| format!("fetching thread {} at format=full", id))?;
        match TriageThread::from_api(raw) {
            Ok(thread) => threads.push(thread),
            Err(e) => warn!("{}skipping unreadable thread {}: {:#}", prefix, id, e),
        }
    }
    if threads.is_empty() {
        warn!(
            "{}every candidate thread was unreadable; nothing to classify",
            prefix
        );
        return Ok(());
    }

    let self_address = client
        .profile_email()
        .await
        .context("resolving the account's own address")?;

    let classification = classify_threads(triage, &threads, &self_address, prefix).await?;

    if dry_run {
        let by_id: HashMap<&str, &TriageThread> =
            threads.iter().map(|t| (t.id.as_str(), t)).collect();
        for (thread_id, bucket_name) in &classification.assignments {
            let subject = by_id.get(thread_id.as_str()).map_or("", |t| t.subject());
            println!(
                "{}{:<20} {:<14} {}",
                prefix, thread_id, bucket_name, subject
            );
        }
    }

    let writes = plan_writes(&threads, &classification, triage, &client.resolver, dry_run)?;

    let mut applied = 0usize;
    for write in &writes {
        client
            .modify_thread(&write.thread_id, &write.add, &write.remove)
            .await
            .with_context(|| format!("labeling thread {} as {}", write.thread_id, write.bucket))?;
        info!(
            "{}[triage:{}] thread {} labeled ({} removed)",
            prefix,
            write.bucket,
            write.thread_id,
            write.remove.len()
        );
        applied += 1;
    }

    let skipped = classification.unknown_buckets.len() + classification.missing_ids.len();
    info!(
        "{}Triage done: {} classified, {} labeled, {} skipped{}",
        prefix,
        classification.assignments.len(),
        applied,
        skipped,
        if dry_run { " (dry run)" } else { "" }
    );
    println!(
        "{}Triage: {} threads classified, {} labeled, {} skipped{}",
        prefix,
        classification.assignments.len(),
        applied,
        skipped,
        if dry_run { " (dry run)" } else { "" }
    );

    Ok(())
}

/// Create the bucket labels and the seen marker if they are missing. Idempotent
/// and fail-loud: the operator never has to create a label by hand, and a
/// mailbox that refuses one is a hard error rather than a run that silently
/// classifies into nothing.
async fn ensure_triage_labels(
    client: &mut GmailClient,
    triage: &TriageConfig,
    prefix: &str,
    dry_run: bool,
) -> Result<()> {
    let missing = plan_labels(triage, &client.resolver, dry_run);
    debug!(
        "{}ensure_triage_labels: missing={}, dry_run={}",
        prefix,
        missing.len(),
        dry_run
    );

    if dry_run {
        // Reported against the same rule the plan uses, so the message cannot
        // claim one thing while the plan does another.
        for (name, _) in plan_labels(triage, &client.resolver, false) {
            println!("{}[dry-run] label '{}' does not exist yet", prefix, name);
        }
        return Ok(());
    }

    let hub = client.hub().clone();
    for (name, visibility) in missing {
        create_label_if_missing(&hub, &mut client.resolver, &name, visibility)
            .await
            .with_context(|| format!("ensuring triage label '{}'", name))?;
    }
    Ok(())
}

/// Find threads carrying at least one message the classifier has never seen.
async fn discover_candidates(
    client: &GmailClient,
    triage: &TriageConfig,
    prefix: &str,
) -> Result<Selection> {
    let refs = client
        .search_message_refs(CANDIDATE_QUERY)
        .await
        .with_context(|| format!("listing candidates with q=\"{}\"", CANDIDATE_QUERY))?;
    debug!("{}discover_candidates: messages={}", prefix, refs.len());

    // `messages.list` carries no date, so recency costs one metadata get per
    // candidate MESSAGE. Paid before the cap deliberately: the cap bounds the
    // classify call and the mutations, and capping on an undocumented list
    // order instead would be the environmental assumption this design refuses.
    let mut candidates: Vec<CandidateMessage> = Vec::new();
    for reference in &refs {
        match client.get_message(&reference.id).await {
            Ok(msg) => candidates.push(CandidateMessage {
                thread_id: msg.thread_id,
                internal_date: msg.internal_date,
            }),
            Err(e) => warn!(
                "{}skipping unreadable candidate message {}: {:#}",
                prefix, reference.id, e
            ),
        }
    }

    let cap = triage.max_threads as usize;
    let selection = select_candidates(&candidates, cap);
    if let Some(message) = cap_message(&selection, cap) {
        warn!("{}{}", prefix, message);
        println!("{}{}", prefix, message);
    }
    info!(
        "{}triage candidates: {} messages -> {} threads -> {} selected",
        prefix,
        refs.len(),
        selection.total_threads,
        selection.thread_ids.len()
    );

    Ok(selection)
}

/// One batched classify call, with exactly one retry on a malformed answer.
/// Retry the CALL, not the thread: a schema miss is the model, not the data,
/// and a per-thread retry would multiply a batched call back into N calls.
async fn classify_threads(
    triage: &TriageConfig,
    threads: &[TriageThread],
    self_address: &str,
    prefix: &str,
) -> Result<classify::Classification> {
    let cli = ClaudeCli::resolve(triage.claude_binary.as_deref(), TRIAGE_TIMEOUT)
        .await
        .map_err(|e| eyre::eyre!("{}", e))?;
    debug!(
        "{}classify_threads: claude version={}",
        prefix,
        cli.version()
    );

    let prompt = classify::build_prompt(&triage.buckets);
    let payload = classify::build_payload(threads, triage.body_chars, self_address)
        .context("building the classifier payload")?;
    let requested: Vec<String> = threads.iter().map(|t| t.id.clone()).collect();

    let mut last_error = None;
    for attempt in 1..=2 {
        let raw = cli
            .invoke(&triage.classify_model, &prompt, &payload)
            .await
            .map_err(|e| eyre::eyre!("{}", e))?;

        match classify::parse_response(&raw, &requested, &triage.buckets) {
            Ok(classification) => return Ok(classification),
            Err(e) => {
                warn!(
                    "{}classifier response was unusable on attempt {}/2: {:#}",
                    prefix, attempt, e
                );
                last_error = Some(e);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| eyre::eyre!("classifier produced no usable response"))
        .wrap_err("classifier response unusable after one retry; no threads were labeled"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
