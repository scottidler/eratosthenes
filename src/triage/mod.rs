//! The triage engine: classify new inbox threads into config-defined buckets
//! with one batched LLM call, then record the result as Gmail labels.
//!
//! Control flow is deterministic Rust; the LLM is called exactly where judgment
//! is needed and nowhere else. Idempotency, the cap, dry-run, and every
//! mutation decision live here, not in a prompt.

pub mod body;
pub mod classify;
pub mod claude;
pub mod draft;
pub mod thread;

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use log::{debug, error, info, warn};

use crate::cfg::config::Config;
use crate::cfg::triage::{TriageBucket, TriageConfig};
use crate::engine;
use crate::gmail::client::GmailClient;
use crate::gmail::label::{LabelResolver, LabelVisibility, create_label_if_missing};
use crate::skip::SkipLedger;
use crate::triage::claude::{ClaudeCli, TRIAGE_TIMEOUT};
use crate::triage::draft::RefreshPlan;
use crate::triage::thread::{Protection, TriageThread};

/// Message-level idempotency marker. Message-level, deliberately: Gmail labels
/// attach to messages and new messages never inherit them, so
/// `-label:llm/seen` finds both brand-new threads AND new inbound messages
/// landing in an already-classified thread. Scott's own replies carry SENT, not
/// INBOX, so they never re-trigger.
pub const SEEN_LABEL: &str = "llm/seen";

/// The candidate query. Threads with no new messages match nothing, which is
/// what makes a rerun a no-op.
pub const CANDIDATE_QUERY: &str = "in:inbox -label:llm/seen";

/// Gmail's system inbox label id. Archiving is removing it, nothing more.
pub const INBOX_LABEL: &str = "INBOX";

/// The bucket whose threads become the digest's Needs Reply section. Matched by
/// bucket NAME so the Gmail label itself stays config: an account that renames
/// `llm/needs-reply` keeps its section, and an account with no such bucket
/// simply has no Needs Reply section.
pub const NEEDS_REPLY_BUCKET: &str = "needs-reply";

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

/// One thread's BUCKET mutation: one `threads.modify` so a thread is never
/// half-written.
///
/// The seen marker is deliberately NOT in `add`. `SEEN_LABEL`'s contract is
/// message-level, and `threads.modify` is a thread-level operation: it labels
/// every message the thread holds AT THE TIME OF THE CALL, including one that
/// arrived after the classifier's snapshot was taken. That message was never
/// classified, yet `-label:llm/seen` then hides it forever, so the loss was
/// permanent rather than merely racy (audit S1/MF4). The marker is applied
/// separately, over `classified_message_ids` and nothing else.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadWrite {
    pub thread_id: String,
    pub bucket: String,
    pub add: Vec<String>,
    pub remove: Vec<String>,
    /// Exactly the messages the classifier actually saw. The seen marker goes
    /// on these and no others, which is what makes a mid-pass arrival resurface
    /// on the next run instead of vanishing.
    pub classified_message_ids: Vec<String>,
    /// Whether `classified_message_ids` also lose INBOX. Never applied by this
    /// thread-level write: INBOX removal rides the message-level seen batch,
    /// for the same mid-pass-arrival reason as the marker.
    pub archive: ArchiveDecision,
}

/// Build the add/remove sets for one classified thread: add the new bucket,
/// remove any OTHER `llm/*` bucket the thread already carries.
///
/// Removing the previous bucket is what makes reclassification work: a noise
/// thread a human replies into becomes needs-reply, and it must not end up
/// carrying both.
///
/// `seen_label` is still validated here even though it is not applied here: a
/// missing marker label must fail the PLAN, not the trailing write, or the run
/// would mutate buckets and only then discover it cannot record what it saw.
pub fn plan_write(
    thread: &TriageThread,
    bucket: &TriageBucket,
    buckets: &[TriageBucket],
    resolver: &LabelResolver,
    seen_label: &str,
    archive: ArchiveDecision,
) -> Result<ThreadWrite> {
    let bucket_id = resolver
        .resolve_name(&bucket.label)
        .ok_or_else(|| eyre::eyre!("label '{}' is not in the resolver", bucket.label))?
        .to_string();
    resolver
        .resolve_name(seen_label)
        .ok_or_else(|| eyre::eyre!("label '{}' is not in the resolver", seen_label))?;

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
        add: vec![bucket_id],
        remove,
        classified_message_ids: thread.messages.iter().map(|m| m.id.clone()).collect(),
        archive,
    })
}

/// What happens to a classified thread's INBOX label. "Stay" is today's
/// behavior: the thread keeps its bucket label, stays in INBOX, and ages on the
/// bucket TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveDecision {
    /// Row 6: the bucket archives and nothing protects the thread.
    Archive,
    /// Row 4: an `awaits-reply` bucket whose newest inbound sender is a bot.
    ArchiveAutomated,
    /// Row 5: the bucket has `archive: false`.
    Stay,
    /// Rows 1 to 3: the thread is protected whatever the bucket says.
    StayProtected(Protection),
}

impl ArchiveDecision {
    /// Whether the thread's classified messages lose INBOX.
    pub fn archives(self) -> bool {
        matches!(
            self,
            ArchiveDecision::Archive | ArchiveDecision::ArchiveAutomated
        )
    }
}

/// The dry-run column: `archive` | `archive (automated)` | `stay` |
/// `stay (pinned|no-inbound|sole-human)`. Padded through `Formatter::pad`, so
/// `{:<22}` lines the column up.
impl fmt::Display for ArchiveDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let column = match self {
            ArchiveDecision::Archive => "archive",
            ArchiveDecision::ArchiveAutomated => "archive (automated)",
            ArchiveDecision::Stay => "stay",
            ArchiveDecision::StayProtected(Protection::Pinned) => "stay (pinned)",
            ArchiveDecision::StayProtected(Protection::NoInbound) => "stay (no-inbound)",
            ArchiveDecision::StayProtected(Protection::SoleHuman) => "stay (sole-human)",
        };
        f.pad(column)
    }
}

/// The archive decision table, first match wins. Pure: headers and labels in,
/// a decision out. The model picks the bucket and nothing else.
///
/// | # | Condition                                                   | Result               |
/// |---|-------------------------------------------------------------|----------------------|
/// | 1 | a message carries `STARRED` or `IMPORTANT`                  | stay (pinned)        |
/// | 2 | no inbound message                                          | stay (no-inbound)    |
/// | 3 | an inbound non-automated message solely to me, or no To     | stay (sole-human)    |
/// | 4 | `awaits-reply` bucket, newest inbound sender automated      | archive (automated)  |
/// | 5 | `archive: false`                                            | stay                 |
/// | 6 | otherwise                                                   | archive              |
///
/// Row 4 sits ahead of row 5 on purpose: it is the only way a bot loses its
/// place in an `archive: false` bucket.
pub fn plan_archive(
    thread: &TriageThread,
    bucket: &TriageBucket,
    self_address: &str,
) -> ArchiveDecision {
    let decision = if let Some(protection) = thread.protection(self_address) {
        ArchiveDecision::StayProtected(protection)
    } else if bucket.awaits_reply
        && thread
            .inbound(self_address)
            .last()
            .is_some_and(|m| m.is_automated())
    {
        ArchiveDecision::ArchiveAutomated
    } else if !bucket.archive {
        ArchiveDecision::Stay
    } else {
        ArchiveDecision::Archive
    };
    debug!(
        "plan_archive: thread={}, bucket={}, decision={:?}",
        thread.id, bucket.name, decision
    );
    decision
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

/// One classified thread, the bucket the model put it in, and what the archive
/// decision says about its INBOX label.
#[derive(Debug)]
pub struct Assignment<'a> {
    pub thread: &'a TriageThread,
    pub bucket: &'a TriageBucket,
    pub archive: ArchiveDecision,
}

/// Pair each classification with its fetched thread and bucket, and decide
/// archive for each. The one place `plan_archive` runs: the dry-run column and
/// the live write path both read these, so they cannot disagree.
pub fn plan_assignments<'a>(
    threads: &'a [TriageThread],
    classification: &classify::Classification,
    buckets: &'a [TriageBucket],
    self_address: &str,
) -> Vec<Assignment<'a>> {
    let by_name: HashMap<&str, &TriageBucket> =
        buckets.iter().map(|b| (b.name.as_str(), b)).collect();
    let by_id: HashMap<&str, &TriageThread> = threads.iter().map(|t| (t.id.as_str(), t)).collect();

    let mut assignments = Vec::new();
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
        assignments.push(Assignment {
            thread,
            bucket,
            archive: plan_archive(thread, bucket, self_address),
        });
    }
    debug!(
        "plan_assignments: classified={}, assigned={}",
        classification.assignments.len(),
        assignments.len()
    );
    assignments
}

/// Every thread mutation this run will perform. Same rule as `plan_labels`: a
/// dry run plans nothing, so it mutates nothing.
pub fn plan_writes(
    assignments: &[Assignment],
    buckets: &[TriageBucket],
    resolver: &LabelResolver,
    dry_run: bool,
) -> Result<Vec<ThreadWrite>> {
    if dry_run {
        return Ok(Vec::new());
    }
    assignments
        .iter()
        .map(|a| plan_write(a.thread, a.bucket, buckets, resolver, SEEN_LABEL, a.archive))
        .collect()
}

/// The seen-marker write, split by the archive decision. Both sets get
/// `llm/seen`; the archive set also loses INBOX. Message-level, over the
/// classified ids only: a message that arrived after the fetch is in neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeenBatches {
    pub stay: Vec<String>,
    pub archive: Vec<String>,
}

/// Split the landed writes' classified message ids into the stay set and the
/// archive set. Callers pass only writes whose bucket label landed, so a
/// thread is archived only together with its bucket and its seen marker.
pub fn plan_seen_batches<'a>(landed: impl IntoIterator<Item = &'a ThreadWrite>) -> SeenBatches {
    let mut batches = SeenBatches::default();
    for write in landed {
        let target = if write.archive.archives() {
            &mut batches.archive
        } else {
            &mut batches.stay
        };
        target.extend(write.classified_message_ids.iter().cloned());
    }
    batches
}

/// Candidates that existed when the filter pass started. A message dated at or
/// after `filter_pass_start` may have landed after the filter queries ran, so
/// no Star or Flag filter has judged it; the next run takes it.
pub fn candidates_before(
    candidates: Vec<CandidateMessage>,
    filter_pass_start: DateTime<Utc>,
) -> Vec<CandidateMessage> {
    candidates
        .into_iter()
        .filter(|c| c.internal_date < filter_pass_start)
        .collect()
}

/// Drop the messages of a fetched thread dated at or after `filter_pass_start`,
/// returning how many went. Same reason as `candidates_before`, one level down:
/// a candidate thread's snapshot can hold a reply that landed during the pass.
/// A dropped message is never classified, marked seen, or archived, so it is
/// still a candidate next run.
pub fn drop_messages_from(thread: &mut TriageThread, filter_pass_start: DateTime<Utc>) -> usize {
    let before = thread.messages.len();
    thread
        .messages
        .retain(|m| m.internal_date < filter_pass_start);
    before - thread.messages.len()
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

    // The filter pass below matches on header guards (`List-Id: []`), and an
    // unfetched header reads as absent.
    client.set_metadata_headers(engine::metadata_headers(&config.message_filters));

    ensure_triage_labels(client, triage, prefix, dry_run).await?;

    // Triage is its own invocation (`eratosthenes triage`), not a phase of
    // `run`, so it owns its ledger, shared by the filter pass and the classify
    // pass. The ceiling is the same per-account `max-skipped-threads` the
    // engine uses.
    let mut skipped = SkipLedger::new(config.max_skipped_threads);

    let filter_pass_start = if dry_run {
        // No filter pass: `ensure_labels` creates labels and the filters write.
        // A fresh mail a Star filter would catch can read `archive` here and
        // `stay (pinned)` live.
        Utc::now()
    } else {
        run_message_filters(client, config, &mut skipped, prefix).await?
    };

    let self_address = client
        .profile_email()
        .await
        .context("resolving the account's own address")?;

    classify_and_label(
        client,
        triage,
        &self_address,
        prefix,
        dry_run,
        filter_pass_start,
        &mut skipped,
    )
    .await?;

    // Runs on EVERY invocation, including one that classified nothing: the
    // refresh is what retries a thread whose previous run died between the
    // label write and the draft, and that thread carries no new message, so
    // classification will never look at it again.
    refresh_drafts(client, triage, &self_address, prefix, dry_run, &skipped).await
}

/// The account's message filters, the same pass `run` does, ahead of
/// classification: a Star or Flag filter must see every mail the archive
/// decision reads, or a mail it would pin is archived first. Returns the
/// moment the pass started; discovery classifies only mail older than that.
async fn run_message_filters(
    client: &mut GmailClient,
    config: &Config,
    skipped: &mut SkipLedger,
    prefix: &str,
) -> Result<DateTime<Utc>> {
    // Filter destinations and the marker must be in the resolver, or Gmail
    // gets a label name where it wants an id.
    engine::ensure_labels(client, config)
        .await
        .context("ensuring message-filter labels before the triage filter pass")?;
    let filter_pass_start = Utc::now();
    debug!(
        "{}run_message_filters: filters={}, filter_pass_start={}",
        prefix,
        config.message_filters.len(),
        filter_pass_start
    );
    let matched = engine::execute_message_filters(
        client,
        &config.message_filters,
        &config.marker_label,
        skipped,
        prefix,
        false,
        false,
    )
    .await
    .context("running message filters ahead of triage")?;
    info!(
        "{}triage filter pass: {} messages matched message filters",
        prefix, matched
    );
    Ok(filter_pass_start)
}

/// Every count the triage summary line carries.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    classified: usize,
    labeled: usize,
    archived: usize,
    protected: usize,
    skipped: usize,
}

/// The classify pass: new inbox messages -> buckets -> one `threads.modify`
/// each -> the seen marker, with INBOX removed from the archive set.
async fn classify_and_label(
    client: &GmailClient,
    triage: &TriageConfig,
    self_address: &str,
    prefix: &str,
    dry_run: bool,
    filter_pass_start: DateTime<Utc>,
    skipped: &mut SkipLedger,
) -> Result<()> {
    debug!(
        "{}classify_and_label: dry_run={}, filter_pass_start={}, skip_ceiling_already_used={}",
        prefix,
        dry_run,
        filter_pass_start,
        skipped.len()
    );
    let selection = discover_candidates(client, triage, filter_pass_start, prefix).await?;
    if selection.thread_ids.is_empty() {
        info!("{}no new inbox messages to classify", prefix);
        println!("{}Triage: nothing to classify", prefix);
        return Ok(());
    }

    let mut threads: Vec<TriageThread> = Vec::new();
    for id in &selection.thread_ids {
        if skipped.contains_thread(id) {
            info!(
                "{}thread {} was skipped by the filter pass; not classified this run",
                prefix, id
            );
            continue;
        }
        let raw = match client.get_thread_full(id).await {
            Ok(raw) => raw,
            Err(e) => {
                // A thread-scoped failure drops this one thread from the batch;
                // an account-scoped one (or the skip ceiling) fails the run.
                skipped.skip_thread(
                    id,
                    "threads.get (full)",
                    e.wrap_err(format!("fetching thread {} at format=full", id)),
                    prefix,
                )?;
                continue;
            }
        };
        let mut thread = match TriageThread::from_api(raw) {
            Ok(thread) => thread,
            Err(e) => {
                warn!("{}skipping unreadable thread {}: {:#}", prefix, id, e);
                continue;
            }
        };
        // Same rule the draft pass follows: a message the filter pass could
        // not judge leaves its whole thread for the next run.
        if let Some(message) = thread
            .messages
            .iter()
            .find(|m| skipped.contains_message(&m.id))
        {
            info!(
                "{}thread {}: message {} was skipped by the filter pass; not classified this run",
                prefix, id, message.id
            );
            continue;
        }
        let deferred = drop_messages_from(&mut thread, filter_pass_start);
        if deferred > 0 {
            info!(
                "{}thread {}: {} message(s) dated after the filter pass started; left for the next run",
                prefix, id, deferred
            );
        }
        if thread.messages.is_empty() {
            continue;
        }
        threads.push(thread);
    }
    if threads.is_empty() {
        warn!(
            "{}no candidate thread was classifiable; nothing to classify",
            prefix
        );
        print_summary(
            prefix,
            &Tally {
                skipped: skipped.len(),
                ..Tally::default()
            },
            dry_run,
        );
        return Ok(());
    }

    let classification = classify_threads(triage, &threads, self_address, prefix).await?;
    let assignments = plan_assignments(&threads, &classification, &triage.buckets, self_address);

    if dry_run {
        for assignment in &assignments {
            println!(
                "{}{:<20} {:<14} {:<22} {}",
                prefix,
                assignment.thread.id,
                assignment.bucket.name,
                assignment.archive,
                assignment.thread.subject()
            );
        }
    }

    let writes = plan_writes(&assignments, &triage.buckets, &client.resolver, dry_run)?;

    // ORDER IS A CORRECTNESS CONSTRAINT, not a preference (audit MF4).
    //
    // Bucket first, then the marker. If the marker write fails after the bucket
    // write succeeded, the messages stay unmarked, the thread resurfaces on the
    // next run, reclassification is idempotent for the label write, and
    // `refresh_drafts` skips threads that already have a draft -- so it
    // self-heals at the cost of one extra LLM call.
    //
    // The reverse order does NOT self-heal: marker first, then a failed bucket
    // write, leaves every message marked seen with no bucket label, and
    // `-label:llm/seen` means the thread can never resurface. That is permanent
    // loss, strictly worse than the bug this fixes.
    let mut landed: Vec<&ThreadWrite> = Vec::new();
    for write in &writes {
        if let Err(e) = client
            .modify_thread(&write.thread_id, &write.add, &write.remove)
            .await
        {
            // Skipped BEFORE it joins `landed`, so a thread whose bucket did
            // not land never earns a seen marker or loses INBOX, and resurfaces.
            skipped.skip_thread(
                &write.thread_id,
                "threads.modify",
                e.wrap_err(format!(
                    "labeling thread {} as {}",
                    write.thread_id, write.bucket
                )),
                prefix,
            )?;
            continue;
        }
        info!(
            "{}[triage:{}] thread {} labeled ({} removed), {}",
            prefix,
            write.bucket,
            write.thread_id,
            write.remove.len(),
            write.archive
        );
        landed.push(write);
    }

    // The seen marker, message-level, in two batches: the archive set also
    // loses INBOX. INBOX removal rides this write and not the thread-level
    // bucket write above, because `threads.modify` would archive a reply that
    // landed after the fetch, unclassified, and `in:inbox` would never find it
    // again. Over the classified ids only, so a message is archived only
    // together with its seen marker. One call per set for the whole pass:
    // `batch_modify` chunks at 1000 ids for 50 quota units. UNREAD is never
    // touched: `Ttl::Detailed` keys on read state.
    let batches = plan_seen_batches(landed.iter().copied());
    if !batches.stay.is_empty() || !batches.archive.is_empty() {
        let seen_id = client
            .resolver
            .resolve_name(SEEN_LABEL)
            .ok_or_else(|| eyre::eyre!("label '{}' is not in the resolver", SEEN_LABEL))?
            .to_string();
        client
            .batch_modify(&batches.stay, std::slice::from_ref(&seen_id), &[])
            .await
            .with_context(|| {
                format!(
                    "marking {} classified messages as seen (buckets are already written; \
the next run will reclassify these threads)",
                    batches.stay.len()
                )
            })?;
        client
            .batch_modify(
                &batches.archive,
                std::slice::from_ref(&seen_id),
                &[INBOX_LABEL.to_string()],
            )
            .await
            .with_context(|| {
                format!(
                    "marking {} classified messages as seen and archiving them (buckets are \
already written; the next run will reclassify these threads)",
                    batches.archive.len()
                )
            })?;
        debug!(
            "{}marked {} classified messages seen, {} of them archived",
            prefix,
            batches.stay.len() + batches.archive.len(),
            batches.archive.len()
        );
    }

    let tally = Tally {
        classified: classification.assignments.len(),
        labeled: landed.len(),
        archived: landed.iter().filter(|w| w.archive.archives()).count(),
        protected: landed
            .iter()
            .filter(|w| matches!(w.archive, ArchiveDecision::StayProtected(_)))
            .count(),
        skipped: classification.unknown_buckets.len()
            + classification.missing_ids.len()
            + skipped.len(),
    };
    info!("{}", summary_line(prefix, &tally, dry_run));
    print_summary(prefix, &tally, dry_run);

    Ok(())
}

fn summary_line(prefix: &str, tally: &Tally, dry_run: bool) -> String {
    format!(
        "{}Triage: {} threads classified, {} labeled, {} archived, {} protected, {} skipped{}",
        prefix,
        tally.classified,
        tally.labeled,
        tally.archived,
        tally.protected,
        tally.skipped,
        if dry_run { " (dry run)" } else { "" }
    )
}

fn print_summary(prefix: &str, tally: &Tally, dry_run: bool) {
    println!("{}", summary_line(prefix, tally, dry_run));
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
        create_label_if_missing(
            &hub,
            &client.limiter,
            &mut client.resolver,
            &name,
            visibility,
        )
        .await
        .with_context(|| format!("ensuring triage label '{}'", name))?;
    }
    Ok(())
}

/// Find threads carrying at least one message the classifier has never seen.
async fn discover_candidates(
    client: &GmailClient,
    triage: &TriageConfig,
    filter_pass_start: DateTime<Utc>,
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

    let listed = candidates.len();
    let candidates = candidates_before(candidates, filter_pass_start);
    if candidates.len() < listed {
        info!(
            "{}{} candidate message(s) dated at or after the filter pass start; left for the next run",
            prefix,
            listed - candidates.len()
        );
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

/// One thread still carrying a `draft: true` bucket label, and which label that
/// is: the answered-rule needs the label id to take it back off.
#[derive(Debug, PartialEq, Eq)]
pub struct DraftTarget {
    pub thread_id: String,
    pub bucket: String,
    pub label_id: String,
}

/// Distinct threads still sitting in the inbox under a `draft: true` bucket,
/// capped.
///
/// Deduped across buckets because a thread can only be drafted into once, and
/// first bucket wins so the config's own bucket order decides -- not the order
/// Gmail happened to return threads in.
pub fn plan_draft_targets(
    per_bucket: Vec<(String, String, Vec<String>)>,
    cap: usize,
) -> (Vec<DraftTarget>, usize) {
    let mut targets: Vec<DraftTarget> = Vec::new();
    for (bucket, label_id, thread_ids) in per_bucket {
        for thread_id in thread_ids {
            if targets.iter().any(|t| t.thread_id == thread_id) {
                continue;
            }
            targets.push(DraftTarget {
                thread_id,
                bucket: bucket.clone(),
                label_id: label_id.clone(),
            });
        }
    }

    let total = targets.len();
    targets.truncate(cap);
    (targets, total)
}

/// Find every inbox thread under a `draft: true` bucket.
///
/// Matched on label IDs rather than a `label:llm/needs-reply` text query: a
/// nested label name needs quoting in Gmail's query syntax, and `labelIds` is
/// evaluated thread-level, which is the level bucket labels live at.
async fn collect_draft_targets(
    client: &GmailClient,
    draft_buckets: &[&TriageBucket],
    cap: usize,
    prefix: &str,
) -> Result<Vec<DraftTarget>> {
    let mut per_bucket: Vec<(String, String, Vec<String>)> = Vec::new();
    for bucket in draft_buckets {
        let Some(label_id) = client.resolver.resolve_name(&bucket.label) else {
            // Only reachable on a dry run, which creates no labels: a real run
            // has already ensured them.
            debug!(
                "{}label '{}' does not exist yet; nothing to refresh",
                prefix, bucket.label
            );
            continue;
        };
        let label_id = label_id.to_string();
        let thread_ids = client
            .list_threads_by_label_ids(&["INBOX", label_id.as_str()])
            .await
            .with_context(|| format!("listing inbox threads labeled '{}'", bucket.label))?;
        per_bucket.push((bucket.name.clone(), label_id, thread_ids));
    }

    let (targets, total) = plan_draft_targets(per_bucket, cap);
    if total > targets.len() {
        let message = format!(
            "max-threads cap HIT in the reply-draft pass: {} threads await a draft, \
handling the first {}, {} left for the next run (raise max-threads if this repeats)",
            total,
            targets.len(),
            total - targets.len()
        );
        warn!("{}{}", prefix, message);
        println!("{}{}", prefix, message);
    }
    info!(
        "{}reply-draft candidates: {} threads",
        prefix,
        targets.len()
    );

    Ok(targets)
}

/// The needs-reply refresh: answered threads lose their bucket label, and
/// unanswered ones without a draft get one.
///
/// Nothing here ever SENDS, and nothing modifies or deletes an existing draft:
/// Scott may have edited it, and his edits are sacred. The cost of that is a
/// draft going stale when the counterparty replies again, which he sees as the
/// newer message in the same thread. Accepted (design doc, Data Model).
async fn refresh_drafts(
    client: &GmailClient,
    triage: &TriageConfig,
    self_address: &str,
    prefix: &str,
    dry_run: bool,
    skipped_threads: &SkipLedger,
) -> Result<()> {
    let draft_buckets: Vec<&TriageBucket> = triage.buckets.iter().filter(|b| b.draft).collect();
    if draft_buckets.is_empty() {
        debug!(
            "{}no bucket sets draft: true; skipping the reply-draft pass",
            prefix
        );
        return Ok(());
    }
    debug!(
        "{}refresh_drafts: buckets={}, model={}, dry_run={}",
        prefix,
        draft_buckets.len(),
        triage.draft_model,
        dry_run
    );

    let targets =
        collect_draft_targets(client, &draft_buckets, triage.max_threads as usize, prefix).await?;
    if targets.is_empty() {
        return Ok(());
    }

    // Loaded once and LOUDLY: a missing profile disables DRAFTING only. The
    // answered-rule below is a label decision with no voice in it, and
    // classification already happened.
    let voice = match draft::load_voice_profile(triage.voice_profile.as_deref()) {
        Ok(profile) => Some(profile),
        Err(e) => {
            error!("{}reply drafting SKIPPED: {:#}", prefix, e);
            println!("{}Triage: reply drafting SKIPPED: {:#}", prefix, e);
            None
        }
    };

    // Resolved on first need, not up front: an account whose needs-reply
    // threads are all answered or already drafted never shells out at all.
    let mut cli: Option<ClaudeCli> = None;
    let mut drafted = 0usize;
    let mut answered = 0usize;
    let mut skipped = 0usize;

    for target in &targets {
        // A thread skipped earlier this run gets no further reads or writes.
        if skipped_threads.contains_thread(&target.thread_id) {
            debug!(
                "{}refresh_drafts: thread {} was skipped this run; leaving it alone",
                prefix, target.thread_id
            );
            skipped += 1;
            continue;
        }
        let raw = match client.get_thread_full(&target.thread_id).await {
            Ok(raw) => raw,
            Err(e) => {
                warn!(
                    "{}fetching thread {} at format=full failed: {:#}",
                    prefix, target.thread_id, e
                );
                skipped += 1;
                continue;
            }
        };
        let thread = match TriageThread::from_api(raw) {
            Ok(thread) => thread,
            Err(e) => {
                warn!(
                    "{}skipping unreadable thread {}: {:#}",
                    prefix, target.thread_id, e
                );
                skipped += 1;
                continue;
            }
        };

        match draft::plan_refresh(&thread, self_address) {
            RefreshPlan::Answered => {
                if dry_run {
                    println!(
                        "{}[dry-run] thread {} is answered; would remove '{}'",
                        prefix, target.thread_id, target.bucket
                    );
                } else {
                    client
                        .modify_thread(
                            &target.thread_id,
                            &[],
                            std::slice::from_ref(&target.label_id),
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "clearing '{}' from answered thread {}",
                                target.bucket, target.thread_id
                            )
                        })?;
                    info!(
                        "{}[triage:{}] thread {} answered; bucket label removed",
                        prefix, target.bucket, target.thread_id
                    );
                }
                answered += 1;
            }
            RefreshPlan::DraftExists => {
                debug!(
                    "{}thread {} already has a draft; leaving it untouched",
                    prefix, target.thread_id
                );
                skipped += 1;
            }
            RefreshPlan::NotSoleRecipient => {
                info!(
                    "{}thread {} not addressed to the owner alone; no draft",
                    prefix, target.thread_id
                );
                skipped += 1;
            }
            RefreshPlan::Empty => {
                warn!(
                    "{}thread {} has no message to reply to; skipping",
                    prefix, target.thread_id
                );
                skipped += 1;
            }
            RefreshPlan::Draft { target_id } => {
                let Some(voice) = voice.as_deref() else {
                    skipped += 1;
                    continue;
                };
                if dry_run {
                    println!(
                        "{}[dry-run] thread {} would get a reply draft",
                        prefix, target.thread_id
                    );
                    skipped += 1;
                    continue;
                }

                let Some(message) = thread.messages.iter().find(|m| m.id == target_id) else {
                    warn!(
                        "{}thread {} lost message {} between plan and build; skipping",
                        prefix, target.thread_id, target_id
                    );
                    skipped += 1;
                    continue;
                };
                let headers = match draft::reply_headers(message) {
                    Ok(headers) => headers,
                    Err(e) => {
                        error!(
                            "{}no reply draft for thread {}: {:#}",
                            prefix, target.thread_id, e
                        );
                        skipped += 1;
                        continue;
                    }
                };

                if cli.is_none() {
                    let resolved =
                        ClaudeCli::resolve(triage.claude_binary.as_deref(), draft::DRAFT_TIMEOUT)
                            .await
                            .map_err(|e| eyre::eyre!("{}", e))?;
                    debug!(
                        "{}reply drafts: claude version={}",
                        prefix,
                        resolved.version()
                    );
                    cli = Some(resolved);
                }
                let Some(cli) = cli.as_ref() else {
                    unreachable!("the claude CLI was just resolved or the run bailed")
                };

                let prompt = draft::build_prompt(voice);
                let payload = classify::build_payload(
                    std::slice::from_ref(&thread),
                    triage.body_chars,
                    self_address,
                )
                .context("building the draft payload")?;
                let raw = cli
                    .invoke(&triage.draft_model, &prompt, &payload)
                    .await
                    .map_err(|e| eyre::eyre!("{}", e))?;

                let body = match draft::parse_response(&raw) {
                    Ok(body) => body,
                    Err(e) => {
                        error!(
                            "{}draft response for thread {} was unusable: {:#}",
                            prefix, target.thread_id, e
                        );
                        skipped += 1;
                        continue;
                    }
                };

                let rfc822 = draft::build_rfc822(&headers, &body);
                let draft_id = client
                    .create_draft(&thread.id, &rfc822)
                    .await
                    .with_context(|| format!("drafting a reply in thread {}", thread.id))?;
                info!(
                    "{}[triage:{}] thread {} -> draft {} to {}",
                    prefix, target.bucket, thread.id, draft_id, headers.to
                );
                drafted += 1;
            }
        }
    }

    info!(
        "{}Reply drafts done: {} drafted, {} answered, {} skipped{}",
        prefix,
        drafted,
        answered,
        skipped,
        if dry_run { " (dry run)" } else { "" }
    );
    println!(
        "{}Triage: {} reply drafts, {} answered, {} skipped{}",
        prefix,
        drafted,
        answered,
        skipped,
        if dry_run { " (dry run)" } else { "" }
    );

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
