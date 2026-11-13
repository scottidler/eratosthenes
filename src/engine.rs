use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use eyre::Result;
use log::{debug, info, trace, warn};

use crate::cache::{CachedThread, ThreadCache};
use crate::cfg::config::Config;
use crate::cfg::filter::{FilterAction, MessageFilter};
use crate::cfg::label::Label;
use crate::cfg::state::{Clock, StateAction, StateFilter, Ttl};
use crate::gmail::client::{GmailClient, ThreadRef};
use crate::gmail::label::{LabelResolver, LabelVisibility, create_label_if_missing};
use crate::gmail::message::GmailMessage;
use crate::gmail::query::compile_query;
use crate::skip::SkipLedger;

/// What one account run did, the numbers its `Done:` line reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    /// Messages the message filters matched (marked, in `--mark-only`).
    pub messages_matched: usize,
    /// Threads a state filter moved or trashed (0 in `--mark-only`).
    pub threads_transitioned: usize,
    /// Distinct threads/messages skipped on a thread-scoped Gmail error.
    pub skipped: usize,
}

pub async fn execute(
    client: &mut GmailClient,
    config: &Config,
    prefix: &str,
    dry_run: bool,
    mark_only: bool,
    cache: &mut ThreadCache,
) -> Result<RunSummary> {
    debug!(
        "{}execute: dry_run={}, mark_only={}, message_filters={}, state_filters={}, max_skipped_threads={}",
        prefix,
        dry_run,
        mark_only,
        config.message_filters.len(),
        config.state_filters.len(),
        config.max_skipped_threads
    );

    // One ledger for the whole account run, created before the `--mark-only`
    // early return so no path can bypass the ceiling.
    let mut skipped = SkipLedger::new(config.max_skipped_threads);

    if mark_only {
        // One-shot backfill mode, not a permanent verb: reuses account discovery and
        // the message-filter matching path, stamps the marker on every message a
        // normal run would HANDLE, and applies no Star/Flag/Move. Stage sanitization
        // and state filters are unrelated to the marker and do not run here.
        info!(
            "{}=== MARK-ONLY: stamping marker on handled messages, no actions applied ===",
            prefix
        );
        ensure_labels(client, config).await?;
        let total_marked = execute_message_filters(
            client,
            &config.message_filters,
            &config.marker_label,
            &mut skipped,
            prefix,
            dry_run,
            mark_only,
        )
        .await?;
        info!(
            "{}Done: {} messages marked (mark-only), {} skipped{}",
            prefix,
            total_marked,
            skipped.len(),
            if dry_run { " (dry run)" } else { "" }
        );
        return Ok(RunSummary {
            messages_matched: total_marked,
            threads_transitioned: 0,
            skipped: skipped.len(),
        });
    }

    if dry_run {
        // Truthful, because `ensure_labels` below is NOT behind the dry-run guard:
        // labels_create is the one unguarded Gmail write in a dry run.
        info!(
            "{}=== DRY RUN - no message or thread changes; missing labels may be created ===",
            prefix
        );
    }

    ensure_labels(client, config).await?;

    info!("{}=== Phase 0: Stage Sanitization ===", prefix);
    let sanitized =
        sanitize_stages(client, &config.state_filters, &mut skipped, prefix, dry_run).await?;
    if sanitized > 0 {
        info!(
            "{}[sanitize] cleaned {} threads with conflicting stage labels",
            prefix, sanitized
        );
    }

    info!("{}=== Phase 1: Message Filters ===", prefix);
    let total_matched = execute_message_filters(
        client,
        &config.message_filters,
        &config.marker_label,
        &mut skipped,
        prefix,
        dry_run,
        mark_only,
    )
    .await?;

    info!("{}=== Phase 2: State Filters (Thread Age-Off) ===", prefix);
    let total_transitioned = execute_state_filters(
        client,
        &config.state_filters,
        &mut skipped,
        cache,
        prefix,
        dry_run,
    )
    .await?;

    info!(
        "{}Done: {} messages matched filters, {} threads transitioned, {} skipped{}",
        prefix,
        total_matched,
        total_transitioned,
        skipped.len(),
        if dry_run { " (dry run)" } else { "" }
    );

    Ok(RunSummary {
        messages_matched: total_matched,
        threads_transitioned: total_transitioned,
        skipped: skipped.len(),
    })
}

async fn ensure_labels(client: &mut GmailClient, config: &Config) -> Result<()> {
    let mut needed: Vec<String> = Vec::new();

    for filter in &config.message_filters {
        for action in &filter.actions {
            match action {
                FilterAction::Move(dest) | FilterAction::Tag(dest) => needed.push(dest.clone()),
                FilterAction::Star | FilterAction::Flag => {}
            }
        }
    }
    for state in &config.state_filters {
        if let StateAction::Move(dest) = &state.action
            && !dest.is_empty()
        {
            needed.push(dest.clone());
        }
        for label in &state.labels {
            if let Label::Custom(name) = label {
                needed.push(name.clone());
            }
        }
    }

    needed.sort();
    needed.dedup();
    debug!(
        "ensure_labels: needed={:?}, marker={}",
        needed, config.marker_label
    );

    let hub = client.hub().clone();
    for name in &needed {
        create_label_if_missing(
            &hub,
            &client.limiter,
            &mut client.resolver,
            name,
            LabelVisibility::Shown,
        )
        .await?;
    }

    // The marker must be in the resolver before any action folds its ID into an add-list:
    // unregistered, `resolve_name` returns None and the `unwrap_or(name)` fallback sends a
    // label NAME where Gmail wants an ID, which is a 400 rather than a clean error.
    // `ensure_labels` is the first thing `execute` does, so this ordering already holds.
    create_label_if_missing(
        &hub,
        &client.limiter,
        &mut client.resolver,
        &config.marker_label,
        LabelVisibility::Hidden,
    )
    .await?;

    Ok(())
}

/// Derive the ordered stage progression from state filter config.
/// Walks state filters and collects Move destinations in declaration order.
/// INBOX is always the first (implicit) stage.
fn derive_stages(state_filters: &[StateFilter]) -> Vec<String> {
    let mut stages = vec!["INBOX".to_string()];
    for filter in state_filters {
        if let StateAction::Move(dest) = &filter.action
            && !dest.is_empty()
            && !stages.contains(dest)
        {
            stages.push(dest.clone());
        }
    }
    stages
}

/// Phase 0: Sanitize conflicting stage labels on threads.
/// If a thread has labels from multiple stages (e.g., INBOX + Purgatory),
/// keep only the earliest stage and remove later ones.
async fn sanitize_stages(
    client: &GmailClient,
    state_filters: &[StateFilter],
    skipped: &mut SkipLedger,
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    let stages = derive_stages(state_filters);
    debug!("sanitize_stages: stages={:?}", stages);

    if stages.len() < 2 {
        return Ok(0);
    }

    let mut total_cleaned = 0usize;

    for i in 0..stages.len() {
        for j in (i + 1)..stages.len() {
            let early = &stages[i];
            let late = &stages[j];

            // Resolve stage names to Gmail label IDs.
            // labelIds in threads.list is evaluated at thread level: a thread matches if any
            // message has label A AND any message has label B (even across different messages).
            // This correctly finds threads where a reply arrived (new msg = INBOX, old msgs = Purgatory).
            let early_id = client
                .resolver
                .resolve_name(early)
                .unwrap_or(early.as_str())
                .to_string();
            let late_id = client
                .resolver
                .resolve_name(late)
                .unwrap_or(late.as_str())
                .to_string();

            debug!(
                "{}[sanitize] checking conflict: {} ({}) + {} ({})",
                prefix, early, early_id, late, late_id
            );
            let thread_ids = client
                .list_threads_by_label_ids(&[&early_id, &late_id])
                .await?;

            if thread_ids.is_empty() {
                continue;
            }

            debug!(
                "{}[sanitize] {} threads have both {} and {} - removing {}",
                prefix,
                thread_ids.len(),
                early,
                late,
                late
            );

            for tid in &thread_ids {
                if skipped.contains_thread(tid) {
                    continue;
                }
                if !dry_run
                    && let Err(err) = client
                        .modify_thread(tid, &[], std::slice::from_ref(&late_id))
                        .await
                {
                    skipped.skip_thread(tid, "threads.modify", err, prefix)?;
                    continue;
                }
                total_cleaned += 1;
            }
        }
    }

    Ok(total_cleaned)
}

/// A filter paired with the candidate message ids ITS OWN Gmail query returned.
struct FilterCandidates<'a> {
    filter: &'a MessageFilter,
    ids: Vec<String>,
}

/// Phase 1: ACL-style message filter execution.
/// Each filter's candidate scope is its OWN query; message FETCHES are deduped across
/// filters, scope is not. Filters are then evaluated in declaration order and the first
/// matching filter claims the message - it is excluded from further filters.
async fn execute_message_filters(
    client: &GmailClient,
    filters: &[MessageFilter],
    marker: &str,
    skipped: &mut SkipLedger,
    prefix: &str,
    dry_run: bool,
    mark_only: bool,
) -> Result<usize> {
    debug!(
        "{}execute_message_filters: count={}, marker={}, dry_run={}, mark_only={}, skipped_so_far={}",
        prefix,
        filters.len(),
        marker,
        dry_run,
        mark_only,
        skipped.len()
    );

    // Each filter keeps the ids its OWN query returned; `all_ids` is only the deduped
    // union used for the single fetch pass, never a filter's candidate scope.
    let mut scoped: Vec<FilterCandidates> = Vec::new();
    let mut all_ids: Vec<String> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for filter in filters {
        let query = compile_query(filter, marker);
        if query.is_empty() {
            warn!("Filter '{}' compiles to empty query, skipping", filter.name);
            continue;
        }
        debug!("{}[filter:{}] searching: {}", prefix, filter.name, query);
        let ids = client.search_messages(&query).await?;
        debug!(
            "{}[filter:{}] query returned {} candidates",
            prefix,
            filter.name,
            ids.len()
        );
        for id in &ids {
            if seen_ids.insert(id.clone()) {
                all_ids.push(id.clone());
            }
        }
        scoped.push(FilterCandidates { filter, ids });
    }

    debug!(
        "{}[phase1] {} unique candidates, fetching metadata...",
        prefix,
        all_ids.len()
    );

    // Fetch all messages once
    let total = all_ids.len();
    let mut messages: HashMap<String, GmailMessage> = HashMap::new();
    for (i, id) in all_ids.iter().enumerate() {
        trace!("{}[phase1] [{}/{}] fetching {}", prefix, i + 1, total, id);
        if (i + 1) % 50 == 0 {
            trace!("{}[phase1] [{}/{}] fetching...", prefix, i + 1, total);
        }
        // A skipped message never enters `messages`, so it never matches and
        // gets no write. Its siblings are unaffected: star suppression reads
        // the whole thread through `threads.get`, not this map.
        match client.get_message(id).await {
            Ok(msg) => {
                messages.insert(id.clone(), msg);
            }
            Err(err) => skipped.skip_message(id, "messages.get", err, prefix)?,
        }
    }

    let matched_per_filter =
        match_message_filters(&scoped, &messages, &client.resolver, marker, prefix);

    let mut total_matched = 0usize;
    // A matched id is claimed the moment it matches, so the per-filter matched sets are
    // disjoint and the running sum IS the size of the claimed set.
    let mut claimed = 0usize;
    // Thread label unions, one `threads.get` per distinct matched thread, cached for the
    // whole run and only ever populated for filters that PIN.
    let mut thread_labels: HashMap<String, HashSet<String>> = HashMap::new();

    for (candidates, matched_ids) in scoped.iter().zip(&matched_per_filter) {
        let filter = candidates.filter;
        claimed += matched_ids.len();

        debug!(
            "{}[filter:{}] {} matched (total claimed: {})",
            prefix,
            filter.name,
            matched_ids.len(),
            claimed
        );

        if matched_ids.is_empty() {
            continue;
        }

        // Mark-only never pins and never suppresses, so the suppression check's
        // `threads.get` round trip buys it nothing -- skip it.
        if !mark_only && pins(filter) {
            fetch_thread_labels(
                client,
                matched_ids,
                &messages,
                &mut thread_labels,
                skipped,
                prefix,
            )
            .await?;
        }

        // The drop-writes seam: a thread skipped anywhere earlier this run
        // (sanitize, or the label fetch just above) gets no star, Tag, Move or
        // marker. Without it, a thread missing from `thread_labels` reads as
        // unpinned and gets a duplicate star, and the marker freezes its
        // message unhandled forever.
        let writable_ids = drop_skipped_threads(matched_ids, &messages, skipped);
        if writable_ids.len() < matched_ids.len() {
            debug!(
                "{}[filter:{}] dropped {} matched messages on skipped threads",
                prefix,
                filter.name,
                matched_ids.len() - writable_ids.len()
            );
        }
        if writable_ids.is_empty() {
            continue;
        }
        total_matched += writable_ids.len();

        let writes = plan_filter_writes(
            filter,
            &writable_ids,
            &messages,
            &thread_labels,
            &client.resolver,
            marker,
            mark_only,
        );
        debug!(
            "{}[filter:{}] {} planned writes",
            prefix,
            filter.name,
            writes.len()
        );
        // Before issuing them, fold the planned pins back into the cached unions: two
        // filters can match two DIFFERENT messages in one thread (claiming is per
        // message), and without this the second filter would see a stale union and add a
        // second star to a thread the first filter just pinned. Unconditional, including
        // under --dry-run, so the preview is what a real run would do. A no-op in
        // mark-only mode, since its writes are never Star/Flag.
        record_planned_pins(&writes, &messages, &mut thread_labels);

        for write in &writes {
            if mark_only {
                // The stamp is irreversible in effect (a stamped message is never
                // handled again), so the operator needs a list to undo a wrong freeze
                // by hand: one INFO line per stamped message, before the write issues.
                log_mark_only_stamps(&write.ids, &messages, prefix);
            }
            apply_planned_write(client, write, &filter.name, prefix, dry_run, mark_only).await?;
        }
    }

    Ok(total_matched)
}

/// Add each planned pin to the cached thread label union, so a later filter's suppression
/// check sees the pins this run already planned.
fn record_planned_pins(
    writes: &[PlannedWrite],
    messages: &HashMap<String, GmailMessage>,
    thread_labels: &mut HashMap<String, HashSet<String>>,
) {
    for write in writes {
        if !matches!(write.action, FilterAction::Star | FilterAction::Flag) {
            continue;
        }
        for id in &write.ids {
            let Some(msg) = messages.get(id) else {
                continue;
            };
            let union = thread_labels.entry(msg.thread_id.clone()).or_default();
            for label in &write.add {
                union.insert(label.clone());
            }
        }
    }
}

/// Does this filter apply a thread-scoped pin? Only pins need the thread label union, so
/// only pins pay for a `threads.get`.
fn pins(filter: &MessageFilter) -> bool {
    filter
        .actions
        .iter()
        .any(|a| matches!(a, FilterAction::Star | FilterAction::Flag))
}

/// Fill `thread_labels` with the label UNION of every distinct thread in `matched_ids`.
///
/// A `GmailMessage` carries only its OWN labels, and the residual star may sit on a
/// sibling this phase never fetched, because that sibling is read or fails the address
/// criteria. So the suppression check genuinely needs the thread, and one `threads.get`
/// per distinct thread id (cached across filters for the run) is the cost of it.
async fn fetch_thread_labels(
    client: &GmailClient,
    matched_ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    thread_labels: &mut HashMap<String, HashSet<String>>,
    skipped: &mut SkipLedger,
    prefix: &str,
) -> Result<()> {
    debug!(
        "{}fetch_thread_labels: matched={}, cached={}, skipped_so_far={}",
        prefix,
        matched_ids.len(),
        thread_labels.len(),
        skipped.len()
    );
    for id in matched_ids {
        let Some(msg) = messages.get(id) else {
            continue;
        };
        if thread_labels.contains_key(&msg.thread_id) || skipped.contains_thread(&msg.thread_id) {
            continue;
        }
        trace!(
            "{}[phase1] fetching thread {} for suppression check",
            prefix, msg.thread_id
        );
        match client.get_thread(&msg.thread_id).await {
            Ok(thread) => {
                thread_labels.insert(msg.thread_id.clone(), thread.label_ids());
            }
            Err(err) => skipped.skip_thread(&msg.thread_id, "threads.get", err, prefix)?,
        }
    }
    Ok(())
}

/// `matched_ids` minus every message whose thread is skipped this run, order kept.
/// Pure, so the planner downstream stays pure: the skipped set is an input, not a
/// lookup the planner makes.
fn drop_skipped_threads(
    matched_ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    skipped: &SkipLedger,
) -> Vec<String> {
    matched_ids
        .iter()
        .filter(|id| {
            messages
                .get(id.as_str())
                .is_none_or(|msg| !skipped.contains_thread(&msg.thread_id))
        })
        .cloned()
        .collect()
}

/// One intended `batch_modify(ids, add, remove)` call.
#[derive(Debug, PartialEq)]
struct PlannedWrite {
    /// The action this write implements. The marker-only write is `Tag(<marker>)`.
    action: FilterAction,
    ids: Vec<String>,
    add: Vec<String>,
    remove: Vec<String>,
}

/// Plan every write one filter's matched set implies, as DATA, before any of them is
/// issued. Pure: no Gmail calls, so "no STARRED add is issued" is assertable without a
/// live mailbox.
///
/// The scoping rule is PER ACTION, and `Move` is the trap:
///
/// - `Star` | `Flag` are thread-scoped pins. Every consumer (digest, state filters) reads
///   them at thread level, so a second pin in a thread carries no information and only
///   costs another gesture to clear. Skip the thread entirely when its label union already
///   carries the label; else pin exactly the newest matched message in it.
/// - `Move` is message-scoped and applies to the FULL matched set, never suppressed and
///   never one-per-thread. It is the only action that removes the labels its own filter
///   scopes on, so skipping it strands a new message in the inbox permanently AND stamped.
/// - `Tag` (user-configured) adds a label and removes nothing, so it is message-scoped
///   like `Move` but destroys no eligibility.
///
/// The marker is the filter's LAST write. With a `Move` present (validated at config load
/// to be at most one, in final position) the marker folds INTO the `Move` write, which
/// cannot half-apply: a `Move` archives and reads the message, so there would be no next
/// run to retry a separate stamp. With no `Move`, one marker-only write follows the pins;
/// no pin destroys eligibility, so a failed marker write just retries next run.
///
/// `mark_only` (the `--mark-only` backfill mode) collapses ALL of the above to exactly
/// one marker-only `Tag(<marker>)` write over the FULL matched set: no pin write (so no
/// `STARRED`/`IMPORTANT` add), no `Move` write, and crucially no `INBOX`/`UNREAD` removal.
/// Without a `Move`, nothing destroys eligibility, so the marker must not carry one either
/// -- a mark-only run that archived the `bots` candidates would be a serious defect.
fn plan_filter_writes(
    filter: &MessageFilter,
    matched_ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    thread_labels: &HashMap<String, HashSet<String>>,
    resolver: &LabelResolver,
    marker: &str,
    mark_only: bool,
) -> Vec<PlannedWrite> {
    if matched_ids.is_empty() {
        return Vec::new();
    }

    let marker_id = resolve_label_id(resolver, marker);

    if mark_only {
        return vec![PlannedWrite {
            action: FilterAction::Tag(marker.to_string()),
            ids: matched_ids.to_vec(),
            add: vec![marker_id],
            remove: Vec::new(),
        }];
    }

    let mut writes: Vec<PlannedWrite> = Vec::new();
    let mut has_move = false;

    for action in &filter.actions {
        match action {
            FilterAction::Star | FilterAction::Flag => {
                let label = if matches!(action, FilterAction::Star) {
                    "STARRED"
                } else {
                    "IMPORTANT"
                };
                let ids = plan_pin_ids(matched_ids, messages, thread_labels, label);
                if !ids.is_empty() {
                    writes.push(PlannedWrite {
                        action: action.clone(),
                        ids,
                        add: vec![label.to_string()],
                        remove: Vec::new(),
                    });
                }
            }
            FilterAction::Tag(label) => writes.push(PlannedWrite {
                action: action.clone(),
                ids: matched_ids.to_vec(),
                add: vec![resolve_label_id(resolver, label)],
                remove: Vec::new(),
            }),
            FilterAction::Move(dest) => {
                has_move = true;
                writes.push(PlannedWrite {
                    action: action.clone(),
                    ids: matched_ids.to_vec(),
                    add: vec![resolve_label_id(resolver, dest), marker_id.clone()],
                    // A "move" must actually leave the inbox: add the destination label
                    // AND remove INBOX (+ mark read). Without the removes this only tags
                    // the message, leaving it sitting in the inbox.
                    remove: vec!["INBOX".to_string(), "UNREAD".to_string()],
                });
            }
        }
    }

    if !has_move {
        writes.push(PlannedWrite {
            action: FilterAction::Tag(marker.to_string()),
            ids: matched_ids.to_vec(),
            add: vec![marker_id],
            remove: Vec::new(),
        });
    }

    writes
}

/// The ids a thread-scoped pin actually writes to: at most one per thread, and none at all
/// for a thread whose label union already carries the pin. A thread missing from
/// `thread_labels` counts as unpinned; `fetch_thread_labels` populates every matched
/// thread for pinning filters before this runs, and `drop_skipped_threads` removes the
/// ones it could not fetch, so none reaches here missing.
fn plan_pin_ids(
    matched_ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    thread_labels: &HashMap<String, HashSet<String>>,
    label: &str,
) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut newest: HashMap<String, (String, chrono::DateTime<chrono::Utc>)> = HashMap::new();

    for id in matched_ids {
        let Some(msg) = messages.get(id) else {
            continue;
        };
        let already_pinned = thread_labels
            .get(&msg.thread_id)
            .is_some_and(|labels| labels.contains(label));
        if already_pinned {
            continue;
        }
        match newest.get(&msg.thread_id) {
            Some((_, when)) if *when >= msg.internal_date => {}
            Some(_) => {
                newest.insert(msg.thread_id.clone(), (id.clone(), msg.internal_date));
            }
            None => {
                order.push(msg.thread_id.clone());
                newest.insert(msg.thread_id.clone(), (id.clone(), msg.internal_date));
            }
        }
    }

    order
        .iter()
        .filter_map(|tid| newest.get(tid).map(|(id, _)| id.clone()))
        .collect()
}

/// Gmail wants label IDs; config names labels. System labels' ids equal their names, so
/// the fallback is correct for those and a registration bug for anything else -- which is
/// why `ensure_labels` registers every destination and the marker before this is reached.
fn resolve_label_id(resolver: &LabelResolver, name: &str) -> String {
    resolver.resolve_name(name).unwrap_or(name).to_string()
}

/// Match every filter against the candidates ITS OWN query returned, in declaration
/// order, first-match-wins across filters. Returns the matched message ids per filter,
/// parallel to `scoped`. Pure: no Gmail calls and no writes, so the plan is inspectable
/// and testable before anything is applied. Takes the `LabelResolver` as a plain
/// parameter (not a `GmailClient`) so the function stays pure and injectable.
///
/// The Gmail query is an intentional PREFILTER and this matcher is authoritative. Gmail
/// cannot express `cc: []`, `headers.List-Id: []`, or globset semantics -- it does not
/// glob at all, so `from:(*@tatari.tv)` returns every unread inbox message by treating
/// the `*` as noise. So per-filter scope does NOT mean trusting a filter's query to be
/// precise: every candidate is re-checked against `MessageFilter::matches` here, which is
/// what makes `matched <= query returned` hold for every filter by construction.
fn match_message_filters(
    scoped: &[FilterCandidates<'_>],
    messages: &HashMap<String, GmailMessage>,
    resolver: &LabelResolver,
    marker: &str,
    prefix: &str,
) -> Vec<Vec<String>> {
    let mut claimed: HashSet<String> = HashSet::new();
    let mut matched_per_filter: Vec<Vec<String>> = Vec::with_capacity(scoped.len());

    for candidates in scoped {
        let filter = candidates.filter;
        let mut matched_ids: Vec<String> = Vec::new();

        for id in &candidates.ids {
            if claimed.contains(id) {
                continue;
            }
            let Some(msg) = messages.get(id) else {
                continue;
            };
            // Filters act on unread mail only. This is a SCOPE constraint mirroring
            // `is:unread` in the query, NOT an idempotency guard: un-starring from the
            // thread list leaves a message unread, so read-state never stopped
            // re-labeling. The marker is what does that.
            if msg.is_read() {
                trace!("{}[filter:{}] skipping {} (read)", prefix, filter.name, id);
                continue;
            }
            let labels = msg.labels_resolved(resolver);
            trace!(
                "{}[filter:{}] checking {} to={:?} cc={:?} from={:?}",
                prefix, filter.name, id, msg.to, msg.cc, msg.from
            );
            if filter.matches(
                &msg.to,
                &msg.cc,
                &msg.from,
                &msg.subject,
                &labels,
                &msg.headers,
                marker,
            ) {
                // Per-record: TRACE, not DEBUG/println. This single line, emitted
                // unconditionally to stdout every 5 minutes, was the syslog flood.
                trace!(
                    "{}[filter:{}] MATCH: {} (from: {})",
                    prefix,
                    filter.name,
                    msg.subject,
                    msg.from.first().map(|s| s.as_str()).unwrap_or("?")
                );
                // Claim on match - excluded from every later filter.
                claimed.insert(id.clone());
                matched_ids.push(id.clone());
            }
        }

        matched_per_filter.push(matched_ids);
    }

    matched_per_filter
}

/// Issue one planned write. All the decisions were made in `plan_filter_writes`; this is
/// the thin shell that talks to Gmail and logs what it did.
async fn apply_planned_write(
    client: &GmailClient,
    write: &PlannedWrite,
    filter_name: &str,
    prefix: &str,
    dry_run: bool,
    mark_only: bool,
) -> Result<()> {
    debug!(
        "{}apply_planned_write: filter={}, action={:?}, count={}, add={:?}, remove={:?}, \
         dry_run={}, mark_only={}",
        prefix,
        filter_name,
        write.action,
        write.ids.len(),
        write.add,
        write.remove,
        dry_run,
        mark_only
    );

    let count = write.ids.len();
    match &write.action {
        FilterAction::Star => {
            info!(
                "{}[filter:{}] starring {} messages",
                prefix, filter_name, count
            );
        }
        FilterAction::Flag => {
            info!(
                "{}[filter:{}] flagging {} messages as important",
                prefix, filter_name, count
            );
        }
        FilterAction::Move(dest) => {
            info!(
                "{}[filter:{}] moving {} messages to {}",
                prefix, filter_name, count, dest
            );
        }
        FilterAction::Tag(label) => {
            // `--mark-only` already logged one line per stamped message
            // (`log_mark_only_stamps`); an aggregate line here would just be noise on
            // top of the per-message rollback list the operator actually needs.
            if !mark_only {
                info!(
                    "{}[filter:{}] tagging {} messages with {}",
                    prefix, filter_name, count, label
                );
            }
        }
    }

    if !dry_run {
        client
            .batch_modify(&write.ids, &write.add, &write.remove)
            .await?;
    }
    Ok(())
}

/// `--mark-only` needs an undo list: the stamp is irreversible in effect (a message
/// carrying the marker is never handled again by any filter), so every message it stamps
/// gets exactly one INFO line naming it. Returns the number of lines emitted, which must
/// equal `ids.len()` for every id whose metadata was actually fetched -- that equality IS
/// the "stamps N messages while logging exactly N lines" criterion.
fn log_mark_only_stamps(
    ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    prefix: &str,
) -> usize {
    let mut logged = 0usize;
    for id in ids {
        let Some(msg) = messages.get(id) else {
            warn!(
                "{}[mark-only] stamping {} but its metadata was never fetched, cannot log \
                 date/from/subject",
                prefix, id
            );
            continue;
        };
        info!(
            "{}[mark-only] stamped {} date={} from={} subject={:?}",
            prefix,
            id,
            msg.internal_date.to_rfc3339(),
            msg.from.first().map(|s| s.as_str()).unwrap_or("?"),
            msg.subject
        );
        logged += 1;
    }
    logged
}

/// The one input a state filter evaluates: a thread's aging facts, whether they
/// came from a cache hit or a fresh `threads.get`. Same struct either way, so a
/// hit and a fetch go down the same evaluation path.
pub struct ThreadState<'a> {
    pub id: &'a str,
    /// Raw Gmail label ids, resolved to names at evaluation time.
    pub label_ids: &'a [String],
    /// Newest message `internal_date`.
    pub last_activity: DateTime<Utc>,
    /// Newest message lacks `UNREAD`.
    pub is_read: bool,
}

impl ThreadState<'_> {
    /// Same resolution `GmailThread::labels_resolved` does: an id the resolver
    /// does not know stands for itself (system labels like `INBOX`).
    fn labels_resolved(&self, resolver: &LabelResolver) -> Vec<Label> {
        self.label_ids
            .iter()
            .map(|id| Label::new(resolver.resolve_id(id).unwrap_or(id.as_str())))
            .collect()
    }
}

/// Phase 2's cache counters for one run, logged as one INFO line.
#[derive(Debug, Default)]
struct CacheStats {
    /// Thread refs the active list returned.
    active: usize,
    /// Refs evaluated from the cache with no `threads.get`.
    hits: usize,
    /// `threads.get` calls made.
    fetched: usize,
    /// Cache entries removed: absent from the list, written, or skipped.
    dropped: usize,
    /// List pages walked.
    pages: usize,
}

/// The cache entry for `thread_ref` if its `historyId` stamp still matches the
/// list's. Anything else (not cached, a moved stamp, no stamp on the list) is a
/// miss and gets fetched.
fn fresh_entry<'c>(cache: &'c ThreadCache, thread_ref: &ThreadRef) -> Option<&'c CachedThread> {
    let listed = thread_ref.history_id?;
    cache
        .get(&thread_ref.id)
        .filter(|entry| entry.history_id == listed)
}

/// Why a miss is being fetched, for the per-fetch DEBUG line.
fn fetch_reason(thread_ref: &ThreadRef, cached: Option<&CachedThread>) -> &'static str {
    match (thread_ref.history_id, cached) {
        (None, _) => "no-history-id",
        (Some(_), None) => "new",
        (Some(_), Some(_)) => "changed",
    }
}

fn drop_entry(cache: &mut ThreadCache, stats: &mut CacheStats, thread_id: &str) {
    if cache.remove(thread_id).is_some() {
        stats.dropped += 1;
    }
}

async fn execute_state_filters(
    client: &GmailClient,
    state_filters: &[StateFilter],
    skipped: &mut SkipLedger,
    cache: &mut ThreadCache,
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    debug!(
        "{}execute_state_filters: count={}, dry_run={}, cached={}",
        prefix,
        state_filters.len(),
        dry_run,
        cache.len()
    );

    let active_query = build_active_threads_query(state_filters);
    if active_query.is_empty() {
        info!(
            "{}No state filter labels to query, skipping Phase 2",
            prefix
        );
        return Ok(0);
    }

    debug!("{}[state] searching active threads...", prefix);
    debug!("{}[state] query: {}", prefix, active_query);
    // `?` on the whole walk: a list that fails on any page fails the run here,
    // before `retain_ids`, so a partial list can never drop live entries.
    let list = match client.list_thread_refs(&active_query).await {
        Ok(list) => list,
        Err(e) => {
            // No counters exist yet; this line stands in for the INFO stats line.
            warn!(
                "{}[state] active list failed, cache untouched: {:#}",
                prefix, e
            );
            return Err(e);
        }
    };
    debug!(
        "{}[state] {} active threads to evaluate",
        prefix,
        list.refs.len()
    );

    let mut stats = CacheStats {
        active: list.refs.len(),
        pages: list.pages,
        ..CacheStats::default()
    };
    let live: HashSet<&str> = list.refs.iter().map(|r| r.id.as_str()).collect();
    stats.dropped += cache.retain_ids(&live);

    let clock = crate::cfg::state::RealClock;
    let stages = derive_stages(state_filters);
    debug!("{}[state] stages={:?}", prefix, stages);

    // Logged on the error path too: the counters up to an account-scoped
    // failure are exactly what the next run's cache was saved with.
    let result = evaluate_active_threads(
        client,
        &list.refs,
        state_filters,
        &stages,
        skipped,
        cache,
        &mut stats,
        prefix,
        &clock,
        dry_run,
    )
    .await;

    info!(
        "{}[state] active={} hits={} fetched={} dropped={} pages={}",
        prefix, stats.active, stats.hits, stats.fetched, stats.dropped, stats.pages
    );
    result
}

/// Per ref: a fresh cache entry is a hit, anything else is a `threads.get`,
/// then one evaluation path for both. Entries are dropped on a real write, a
/// skip, or a fetch error; kept on a no-op, a Keep, and a dry-run would-write.
async fn evaluate_active_threads<C: Clock>(
    client: &GmailClient,
    refs: &[ThreadRef],
    state_filters: &[StateFilter],
    stages: &[String],
    skipped: &mut SkipLedger,
    cache: &mut ThreadCache,
    stats: &mut CacheStats,
    prefix: &str,
    clock: &C,
    dry_run: bool,
) -> Result<usize> {
    let total = refs.len();
    let mut transitioned = 0usize;

    for (i, thread_ref) in refs.iter().enumerate() {
        let thread_id = thread_ref.id.as_str();
        if (i + 1) % 50 == 0 {
            trace!("{}[state] [{}/{}] evaluating...", prefix, i + 1, total);
        }
        if skipped.contains_thread(thread_id) {
            // Skipped earlier this run (Phase 0): its entry goes, same as a
            // Phase 2 skip, so the next run fetches it fresh.
            drop_entry(cache, stats, thread_id);
            trace!(
                "{}[state] thread {} already skipped this run, no further writes",
                prefix, thread_id
            );
            continue;
        }

        let hit = fresh_entry(cache, thread_ref).cloned();
        if hit.is_some() {
            stats.hits += 1;
            trace!(
                "{}[state] [{}/{}] cache hit {}",
                prefix,
                i + 1,
                total,
                thread_id
            );
        } else {
            stats.fetched += 1;
            debug!(
                "{}[state] fetch id={} reason={}",
                prefix,
                thread_id,
                fetch_reason(thread_ref, cache.get(thread_id))
            );
        }

        // The per-thread boundary: the fetch AND the whole filter walk are one
        // unit. Catching around the write instead would turn a failed Move into
        // "not acted", and a caller that kept walking would try later filters, a
        // Delete among them.
        let outcome: std::result::Result<bool, (&str, eyre::Report)> = async {
            let (label_ids, last_activity, is_read) = match hit {
                Some(CachedThread {
                    label_ids,
                    last_activity,
                    is_read,
                    ..
                }) => (label_ids, last_activity, is_read),
                None => {
                    let thread = client
                        .get_thread(thread_id)
                        .await
                        .map_err(|err| ("threads.get", err))?;
                    match CachedThread::from_thread(&thread) {
                        Some(entry) => {
                            cache.insert(thread_id.to_string(), entry.clone());
                            (entry.label_ids, entry.last_activity, entry.is_read)
                        }
                        None => {
                            // Not cacheable (empty, no `historyId`, or a partial
                            // view): any older entry is stale now, and the thread
                            // is evaluated as before, from the same definitions.
                            drop_entry(cache, stats, thread_id);
                            let Some(last_activity) = thread.last_activity() else {
                                warn!("{}Thread {} has no messages, skipping", prefix, thread_id);
                                return Ok(false);
                            };
                            let mut label_ids: Vec<String> =
                                thread.label_ids().into_iter().collect();
                            label_ids.sort();
                            (label_ids, last_activity, thread.is_read())
                        }
                    }
                }
            };
            let state = ThreadState {
                id: thread_id,
                label_ids: &label_ids,
                last_activity,
                is_read,
            };
            evaluate_thread(
                client,
                &state,
                state_filters,
                stages,
                prefix,
                clock,
                dry_run,
            )
            .await
            .map_err(|err| ("state-filter action", err))
        }
        .await;
        match outcome {
            Ok(true) => {
                transitioned += 1;
                // `Ok(true)` in a dry run is a would-write: Gmail is unchanged,
                // so the entry is still true.
                if !dry_run {
                    drop_entry(cache, stats, thread_id);
                }
            }
            Ok(false) => {}
            Err((op, err)) => {
                // Before `skip_thread`: it returns early on an account-scoped
                // error or a blown ceiling, and the drop must survive into the save.
                drop_entry(cache, stats, thread_id);
                skipped.skip_thread(thread_id, op, err, prefix)?;
            }
        }
    }

    Ok(transitioned)
}

async fn evaluate_thread<C: Clock>(
    client: &GmailClient,
    thread: &ThreadState<'_>,
    state_filters: &[StateFilter],
    stages: &[String],
    prefix: &str,
    clock: &C,
    dry_run: bool,
) -> Result<bool> {
    let thread_labels = thread.labels_resolved(&client.resolver);
    // Per-record: fires once per active thread every run. TRACE, not DEBUG --
    // at the default level this single line grew tatari.log ~48 MiB/day.
    trace!(
        "{}evaluate_thread: id={}, labels={:?}, is_read={}",
        prefix, thread.id, thread_labels, thread.is_read
    );

    let verdict = plan_thread(
        &thread_labels,
        thread.last_activity,
        thread.is_read,
        state_filters,
        stages,
        &client.resolver,
        clock,
    )?;

    match verdict {
        ThreadVerdict::Move {
            filter,
            dest,
            planned,
        } => {
            debug!(
                "{}[state:{}] thread {} -> {} (add={:?}, remove={:?}, dry_run={})",
                prefix, filter.name, thread.id, dest, planned.add, planned.remove, dry_run,
            );
            if !dry_run {
                client
                    .modify_thread(thread.id, &planned.add, &planned.remove)
                    .await?;
            }
            Ok(true)
        }
        ThreadVerdict::Delete { filter } => {
            debug!(
                "{}[state:{}] trashing thread {} (dry_run={})",
                prefix, filter.name, thread.id, dry_run,
            );
            if !dry_run {
                client.trash_thread(thread.id).await?;
            }
            Ok(true)
        }
        ThreadVerdict::Held { filter } => {
            // Per-record: once per owned thread every run -> TRACE.
            trace!(
                "{}[thread:{}] owned by '{}', TTL running",
                prefix, thread.id, filter.name
            );
            Ok(false)
        }
        ThreadVerdict::Unowned => Ok(false),
    }
}

/// What the state-filter walk decided for one thread, as data: the first filter that
/// owns the thread, and what it does to it now.
#[derive(Debug, PartialEq)]
enum ThreadVerdict<'a> {
    /// The owning filter's TTL has expired and its move changes something.
    Move {
        filter: &'a StateFilter,
        dest: &'a str,
        planned: PlannedMove,
    },
    /// The owning filter's TTL has expired and it trashes the thread.
    Delete { filter: &'a StateFilter },
    /// The owning filter is `Keep`, or its TTL is still running: nothing happens and
    /// no later filter is consulted.
    Held { filter: &'a StateFilter },
    /// No filter owns the thread.
    Unowned,
}

/// Walk the state filters in declaration order and decide the thread's fate. Pure: no
/// Gmail calls, so ownership is assertable without a mailbox.
///
/// The first filter whose labels match OWNS the thread (D5): while its TTL runs it holds
/// the thread, and when the TTL expires it acts. Later filters are never consulted. That
/// is what lets a bucket filter ahead of the catch-all `Cull` apply a looser TTL, and
/// what keeps an action-less category filter's thread from ever getting a stage label.
/// Before this, only `Keep` ended the walk, so a read `llm/receipts` thread 4 days old
/// skipped its 7d bucket filter and `Cull` (read 3d) took it.
///
/// One exception, before and after expiry: a `Move` whose plan is already a no-op (the
/// thread is at or past the destination stage) does not own the thread, and the walk
/// falls through. A bucket label survives every move by design, so a bucket filter still
/// matches the thread it parked in Purgatory; if it owned that thread, `Purge` would
/// never get it on `Purge`'s own TTL.
fn plan_thread<'a, C: Clock>(
    thread_labels: &[Label],
    last_activity: DateTime<Utc>,
    is_read: bool,
    state_filters: &'a [StateFilter],
    stages: &[String],
    resolver: &LabelResolver,
    clock: &C,
) -> Result<ThreadVerdict<'a>> {
    for state_filter in state_filters {
        if !state_filter.matches_labels(thread_labels) {
            trace!(
                "plan_thread: filter '{}' labels don't match, skipping",
                state_filter.name
            );
            continue;
        }

        // Per-record: once per matched thread every run -> TRACE, not DEBUG.
        trace!(
            "plan_thread: matched filter '{}': last_activity={}, is_read={}",
            state_filter.name, last_activity, is_read
        );

        if state_filter.ttl == Ttl::Keep {
            return Ok(ThreadVerdict::Held {
                filter: state_filter,
            });
        }

        let planned = match &state_filter.action {
            StateAction::Move(dest) => {
                let planned = plan_state_move(thread_labels, stages, dest, resolver);
                if planned.is_noop() {
                    debug!(
                        "plan_thread: filter '{}' move to '{}' is a no-op (thread at or past it); continuing to later filters",
                        state_filter.name, dest
                    );
                    continue;
                }
                Some((dest.as_str(), planned))
            }
            StateAction::Delete => None,
        };

        let verdict = match (
            state_filter.evaluate_ttl(last_activity, is_read, clock)?,
            planned,
        ) {
            (None, _) => ThreadVerdict::Held {
                filter: state_filter,
            },
            (Some(_), Some((dest, planned))) => ThreadVerdict::Move {
                filter: state_filter,
                dest,
                planned,
            },
            (Some(_), None) => ThreadVerdict::Delete {
                filter: state_filter,
            },
        };
        return Ok(verdict);
    }

    Ok(ThreadVerdict::Unowned)
}

/// One intended `modify_thread(add, remove)` for a state-filter `Move`.
#[derive(Debug, PartialEq)]
struct PlannedMove {
    add: Vec<String>,
    remove: Vec<String>,
}

impl PlannedMove {
    /// Nothing to add and nothing to remove: the thread already occupies the
    /// destination stage. Writing this would be a `threads.modify` that changes
    /// nothing, and -- worse -- treating it as "the filter fired" ends filter
    /// evaluation, so every later filter (notably `Purge`) is never reached.
    fn is_noop(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Plan the label writes a `Move` implies, as DATA: no Gmail calls, so the semantics are
/// assertable without a live mailbox.
///
/// A state filter's `labels:` are MATCH CRITERIA, not the thread's location, and this used
/// to remove them. That is only accidentally right for the two filter shapes that happen to
/// scope on a stage (`INBOX -> Purgatory`, `Purgatory -> Oblivion`) and wrong for every
/// other shape: an `llm/noise -> Purgatory` filter stripped `llm/noise`, left the thread
/// sitting in the inbox, and destroyed the very classification that selected it -- so the
/// next run reclassified it and stripped it again, forever, and the thread never aged.
///
/// A Move means "advance the thread one stage": remove `INBOX` plus every stage label
/// (`derive_stages`) the thread currently carries, add the destination, remove nothing
/// else. `INBOX` goes unconditionally -- a later-stage thread can be back in the inbox
/// because a reply arrived, and leaving it there is the same stranding bug in a different
/// costume. The destination is never removed, so a filter that moves a thread to the stage
/// it already occupies is a no-op instead of a self-cancelling write.
fn plan_state_move(
    thread_labels: &[Label],
    stages: &[String],
    dest: &str,
    resolver: &LabelResolver,
) -> PlannedMove {
    let dest_label = Label::new(dest);

    // The ladder only runs forward. A bucket filter (`llm/recruiting -> Purgatory`) still
    // matches a thread `Purge` already sent to Oblivion, because the bucket label survives
    // by design; without this guard it dragged the thread back to Purgatory, `Purge` sent
    // it on to Oblivion the next run, and 643 threads flipped every run (2026-10-05),
    // about 130k `threads.modify` calls a day and the 429 storm that came with them. The
    // occasional 400 failedPrecondition is NOT this bug's doing: it hits quiescent,
    // never-written threads before and after this guard, and is handled per thread by the
    // skip boundary in `execute_state_filters`.
    if let Some(dest_index) = stages.iter().position(|s| Label::new(s) == dest_label)
        && let Some(current_index) = current_stage_index(thread_labels, stages)
        && current_index > dest_index
    {
        debug!(
            "plan_state_move: refusing backward move to '{}' from '{}'",
            dest, stages[current_index]
        );
        return PlannedMove {
            add: Vec::new(),
            remove: Vec::new(),
        };
    }

    let mut remove: Vec<Label> = Vec::new();
    if dest_label != Label::Inbox && thread_labels.contains(&Label::Inbox) {
        remove.push(Label::Inbox);
    }
    for stage in stages {
        let stage = Label::new(stage);
        if stage != dest_label && thread_labels.contains(&stage) && !remove.contains(&stage) {
            remove.push(stage);
        }
    }

    // An empty destination is the `action:`-less default: age the thread out of the inbox
    // and out of its stage, with nowhere to land. Adding "" would be a Gmail 400.
    let add = if dest.is_empty() || thread_labels.contains(&dest_label) {
        Vec::new()
    } else {
        vec![resolve_label_id(resolver, dest)]
    };

    PlannedMove {
        add,
        remove: remove
            .iter()
            .map(|l| resolve_label_id(resolver, l.to_gmail_id()))
            .collect(),
    }
}

/// The thread's position on the ladder. `INBOX` wins over any later stage label it also
/// carries: a reply pulls a thread back into the inbox, and from there it ages forward
/// again rather than being pinned at its old stage.
fn current_stage_index(thread_labels: &[Label], stages: &[String]) -> Option<usize> {
    if thread_labels.contains(&Label::Inbox) {
        return stages.iter().position(|s| Label::new(s) == Label::Inbox);
    }
    stages
        .iter()
        .rposition(|s| thread_labels.contains(&Label::new(s)))
}

fn build_active_threads_query(state_filters: &[StateFilter]) -> String {
    let mut label_queries: Vec<String> = Vec::new();

    label_queries.push("in:inbox".to_string());

    for filter in state_filters {
        for label in &filter.labels {
            let query = match label {
                Label::Inbox => "in:inbox".to_string(),
                Label::Starred => "is:starred".to_string(),
                Label::Important => "is:important".to_string(),
                Label::Unread => "is:unread".to_string(),
                Label::Trash => "in:trash".to_string(),
                Label::Spam => "in:spam".to_string(),
                _ => format!("label:{}", label.to_gmail_id().to_lowercase()),
            };
            if !label_queries.contains(&query) {
                label_queries.push(query);
            }
        }

        if let StateAction::Move(dest) = &filter.action
            && !dest.is_empty()
        {
            let query = format!("label:{}", dest.to_lowercase());
            if !label_queries.contains(&query) {
                label_queries.push(query);
            }
        }
    }

    if label_queries.len() == 1 {
        return label_queries.into_iter().next().unwrap_or_default();
    }

    label_queries.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfg::filter::{AddressFilter, LabelsFilter};
    use crate::cfg::state::{StateAction, Ttl};
    use crate::gmail::message::GmailThread;
    use chrono::DateTime;

    fn from_filter(name: &str, patterns: &[&str]) -> MessageFilter {
        MessageFilter {
            name: name.to_string(),
            to: None,
            cc: None,
            from: Some(AddressFilter {
                patterns: patterns.iter().map(|p| p.to_string()).collect(),
            }),
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        }
    }

    fn message(id: &str, from: &str, label_ids: &[&str]) -> GmailMessage {
        GmailMessage {
            id: id.to_string(),
            thread_id: format!("thread-{}", id),
            label_ids: label_ids.iter().map(|l| l.to_string()).collect(),
            internal_date: DateTime::UNIX_EPOCH,
            headers: HashMap::new(),
            to: vec!["scott@example.com".to_string()],
            cc: vec![],
            from: vec![from.to_string()],
            subject: format!("subject {}", id),
        }
    }

    fn unread(id: &str, from: &str) -> GmailMessage {
        message(id, from, &["INBOX", "UNREAD"])
    }

    fn message_map(msgs: Vec<GmailMessage>) -> HashMap<String, GmailMessage> {
        msgs.into_iter().map(|m| (m.id.clone(), m)).collect()
    }

    /// A resolver with no custom labels registered, for tests that don't exercise
    /// label resolution.
    fn empty_resolver() -> LabelResolver {
        LabelResolver::from_api_labels(vec![])
    }

    const MARKER: &str = "Triaged";
    /// The Gmail-side id of the marker, as `ensure_labels` would have registered it.
    const MARKER_ID: &str = "Label_99";

    fn custom_label(id: &str, name: &str) -> google_gmail1::api::Label {
        google_gmail1::api::Label {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    /// A resolver holding the marker and the `Bots` destination, i.e. the state
    /// `ensure_labels` leaves behind before the message-filter stage runs.
    fn live_resolver() -> LabelResolver {
        LabelResolver::from_api_labels(vec![
            custom_label(MARKER_ID, MARKER),
            custom_label("Label_5", "Bots"),
        ])
    }

    /// A message with an explicit thread and delivery time, for thread-scoping tests.
    fn msg_at(id: &str, thread: &str, from: &str, label_ids: &[&str], millis: i64) -> GmailMessage {
        GmailMessage {
            id: id.to_string(),
            thread_id: thread.to_string(),
            label_ids: label_ids.iter().map(|l| l.to_string()).collect(),
            internal_date: DateTime::from_timestamp_millis(millis).expect("valid timestamp"),
            headers: HashMap::new(),
            to: vec!["scott@example.com".to_string()],
            cc: vec![],
            from: vec![from.to_string()],
            subject: format!("subject {}", id),
        }
    }

    fn thread_label_map(threads: &[(&str, &[&str])]) -> HashMap<String, HashSet<String>> {
        threads
            .iter()
            .map(|(tid, labels)| {
                (
                    tid.to_string(),
                    labels.iter().map(|l| l.to_string()).collect(),
                )
            })
            .collect()
    }

    /// The recording seam: match every filter against its own candidates, then plan the
    /// writes, exactly as `execute_message_filters` does, and hand back the writes instead
    /// of issuing them. No Gmail, no async. `thread_labels` stands in for the
    /// `threads.get` results `fetch_thread_labels` would have cached.
    fn plan_all_writes(
        scoped: &[FilterCandidates<'_>],
        messages: &HashMap<String, GmailMessage>,
        thread_labels: &HashMap<String, HashSet<String>>,
        resolver: &LabelResolver,
    ) -> Vec<PlannedWrite> {
        plan_all_writes_mode(scoped, messages, thread_labels, resolver, false)
    }

    /// Same as `plan_all_writes`, with `mark_only` exposed for the `--mark-only` tests.
    fn plan_all_writes_mode(
        scoped: &[FilterCandidates<'_>],
        messages: &HashMap<String, GmailMessage>,
        thread_labels: &HashMap<String, HashSet<String>>,
        resolver: &LabelResolver,
        mark_only: bool,
    ) -> Vec<PlannedWrite> {
        let matched_per_filter = match_message_filters(scoped, messages, resolver, MARKER, "");
        let mut unions = thread_labels.clone();
        let mut all_writes: Vec<PlannedWrite> = Vec::new();
        for (candidates, matched_ids) in scoped.iter().zip(&matched_per_filter) {
            let writes = plan_filter_writes(
                candidates.filter,
                matched_ids,
                messages,
                &unions,
                resolver,
                MARKER,
                mark_only,
            );
            record_planned_pins(&writes, messages, &mut unions);
            all_writes.extend(writes);
        }
        all_writes
    }

    fn adds(writes: &[PlannedWrite], label: &str) -> Vec<String> {
        writes
            .iter()
            .filter(|w| w.add.iter().any(|l| l == label))
            .flat_map(|w| w.ids.clone())
            .collect()
    }

    fn scope_one<'a>(filter: &'a MessageFilter, ids: &[&str]) -> Vec<FilterCandidates<'a>> {
        vec![FilterCandidates {
            filter,
            ids: ids.iter().map(|i| i.to_string()).collect(),
        }]
    }

    /// THE reported bug: a message the engine already handled, which the user then
    /// UNSTARRED. It still matches the filter's address criteria and it is still unread,
    /// so nothing but the marker stops it. No STARRED add may be issued.
    fn skipped_threads(ids: &[&str]) -> SkipLedger {
        let mut ledger = SkipLedger::new(10);
        for id in ids {
            let err = eyre::Report::new(google_gmail1::Error::BadRequest(serde_json::json!({
                "error": { "code": 400, "status": "FAILED_PRECONDITION" }
            })));
            ledger
                .skip_thread(id, "threads.get", err, "")
                .expect("thread-scoped skip under the ceiling");
        }
        ledger
    }

    #[test]
    fn test_drop_skipped_threads_removes_every_message_on_a_skipped_thread() {
        let messages = message_map(vec![
            msg_at("a1", "ta", "x@y.com", &["UNREAD"], 1),
            msg_at("b1", "tb", "x@y.com", &["UNREAD"], 2),
            msg_at("b2", "tb", "x@y.com", &["UNREAD"], 3),
            msg_at("c1", "tc", "x@y.com", &["UNREAD"], 4),
        ]);
        let matched: Vec<String> = ["a1", "b1", "b2", "c1"].map(String::from).to_vec();
        let kept = drop_skipped_threads(&matched, &messages, &skipped_threads(&["tb"]));
        assert_eq!(kept, vec!["a1".to_string(), "c1".to_string()]);
    }

    #[test]
    fn test_drop_skipped_threads_keeps_everything_with_no_skips() {
        let messages = message_map(vec![msg_at("a1", "ta", "x@y.com", &["UNREAD"], 1)]);
        let matched = vec!["a1".to_string()];
        let kept = drop_skipped_threads(&matched, &messages, &SkipLedger::new(10));
        assert_eq!(kept, matched);
    }

    /// Gmail gives a thread its first message's id: a skipped MESSAGE with that id
    /// must not drop the thread's other messages.
    #[test]
    fn test_drop_skipped_threads_ignores_a_message_skip_sharing_the_thread_id() {
        let messages = message_map(vec![msg_at("m2", "m1", "x@y.com", &["UNREAD"], 2)]);
        let mut ledger = SkipLedger::new(10);
        let err = eyre::Report::new(google_gmail1::Error::BadRequest(serde_json::json!({
            "error": { "code": 400, "status": "FAILED_PRECONDITION" }
        })));
        ledger
            .skip_message("m1", "messages.get", err, "")
            .expect("thread-scoped skip under the ceiling");
        let matched = vec!["m2".to_string()];
        assert_eq!(drop_skipped_threads(&matched, &messages, &ledger), matched);
    }

    #[test]
    fn test_marked_and_unstarred_message_is_never_re_starred() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let msg = msg_at(
            "id-1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD", MARKER_ID],
            1_000,
        );
        let messages = message_map(vec![msg]);
        let scoped = scope_one(&filter, &["id-1"]);

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD", MARKER_ID])]),
            &live_resolver(),
        );

        assert!(
            adds(&writes, "STARRED").is_empty(),
            "a marked, unstarred message must not be re-starred: {:?}",
            writes
        );
        assert!(writes.is_empty(), "nothing matched, so nothing is written");
    }

    /// The IMPORTANT twin. `Flag` is a separate action arm with its own suppression path,
    /// so `Star` coverage does not imply it.
    #[test]
    fn test_marked_and_unflagged_message_is_never_re_flagged() {
        let mut filter = from_filter("only-me-ttv", &["*@example.com"]);
        filter.actions = vec![FilterAction::Flag];
        let msg = msg_at(
            "id-1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD", MARKER_ID],
            1_000,
        );
        let messages = message_map(vec![msg]);
        let scoped = scope_one(&filter, &["id-1"]);

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD", MARKER_ID])]),
            &live_resolver(),
        );

        assert!(
            adds(&writes, "IMPORTANT").is_empty(),
            "a marked, unflagged message must not be re-flagged: {:?}",
            writes
        );
    }

    /// Pins are read at thread level by every consumer, so two matched messages in one
    /// thread get ONE star, on the newest of them. Both are still MARKED as handled: the
    /// marker means handled, not acted on, or the older sibling stays eligible and the
    /// loop returns one message along.
    #[test]
    fn test_two_matched_messages_in_one_thread_get_exactly_one_star() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let messages = message_map(vec![
            msg_at(
                "older",
                "t1",
                "boss@example.com",
                &["INBOX", "UNREAD"],
                1_000,
            ),
            msg_at(
                "newer",
                "t1",
                "boss@example.com",
                &["INBOX", "UNREAD"],
                2_000,
            ),
        ]);
        let scoped = scope_one(&filter, &["older", "newer"]);

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD"])]),
            &live_resolver(),
        );

        assert_eq!(
            adds(&writes, "STARRED"),
            vec!["newer"],
            "exactly one star, on the newest matched message: {:?}",
            writes
        );
        let mut marked = adds(&writes, MARKER_ID);
        marked.sort();
        assert_eq!(
            marked,
            vec!["newer", "older"],
            "every HANDLED message is marked, not just the acted-on one"
        );
    }

    /// The residual star sits on a sibling this phase never fetched: read, so the match
    /// loop would skip it, and not in `messages` at all. Only the THREAD's label union
    /// shows it, which is what makes the per-thread `get_thread` required rather than an
    /// optimization.
    #[test]
    fn test_pin_suppressed_when_the_star_sits_on_a_non_matched_sibling() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let messages = message_map(vec![msg_at(
            "id-1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD"],
            2_000,
        )]);
        let scoped = scope_one(&filter, &["id-1"]);

        // The union carries STARRED; the matched message itself does not.
        let thread_labels = thread_label_map(&[("t1", &["INBOX", "UNREAD", "STARRED"])]);
        let writes = plan_all_writes(&scoped, &messages, &thread_labels, &live_resolver());

        assert!(
            adds(&writes, "STARRED").is_empty(),
            "the thread is already pinned, so the pin is suppressed: {:?}",
            writes
        );
        assert_eq!(
            adds(&writes, MARKER_ID),
            vec!["id-1"],
            "suppressed still means HANDLED, so the marker is written"
        );
        assert_eq!(writes.len(), 1, "one marker-only write, no pin write");
    }

    /// Acceptance criterion: `Move` is NEITHER suppressed NOR limited to one message per
    /// thread. Two matched bot messages in a thread that already carries `Bots` must BOTH
    /// leave the inbox, or a new bot message is stranded in the inbox and stamped.
    #[test]
    fn test_move_is_neither_suppressed_nor_limited_to_one_per_thread() {
        let mut filter = from_filter("bots", &["*@example.com"]);
        filter.actions = vec![FilterAction::Move("Bots".to_string())];
        let messages = message_map(vec![
            msg_at("bot-1", "t1", "ci@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("bot-2", "t1", "ci@example.com", &["INBOX", "UNREAD"], 2_000),
        ]);
        let scoped = scope_one(&filter, &["bot-1", "bot-2"]);

        // The thread already carries the Bots label from an older message.
        let thread_labels = thread_label_map(&[("t1", &["INBOX", "UNREAD", "Label_5"])]);
        let writes = plan_all_writes(&scoped, &messages, &thread_labels, &live_resolver());

        assert_eq!(writes.len(), 1, "one write: the Move, carrying the marker");
        let write = &writes[0];
        assert_eq!(write.ids, vec!["bot-1", "bot-2"], "the FULL matched set");
        assert_eq!(write.add, vec!["Label_5", MARKER_ID]);
        assert_eq!(write.remove, vec!["INBOX", "UNREAD"]);
    }

    /// `[Star, Bots]`: the pin is thread-scoped and the Move takes the full matched set,
    /// in declared order, and only the Move carries the marker (it is the last write, and
    /// it is the write that destroys future eligibility).
    #[test]
    fn test_star_then_move_folds_the_marker_into_the_move_only() {
        let mut filter = from_filter("star-then-bots", &["*@example.com"]);
        filter.actions = vec![FilterAction::Star, FilterAction::Move("Bots".to_string())];
        let messages = message_map(vec![
            msg_at("m1", "t1", "ci@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("m2", "t1", "ci@example.com", &["INBOX", "UNREAD"], 2_000),
        ]);
        let scoped = scope_one(&filter, &["m1", "m2"]);

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD"])]),
            &live_resolver(),
        );

        assert_eq!(writes.len(), 2, "the Star write, then the Move write");
        assert_eq!(writes[0].action, FilterAction::Star);
        assert_eq!(writes[0].ids, vec!["m2"]);
        assert!(!writes[0].add.contains(&MARKER_ID.to_string()));
        assert_eq!(writes[1].action, FilterAction::Move("Bots".to_string()));
        assert_eq!(writes[1].ids, vec!["m1", "m2"]);
        assert_eq!(writes[1].add, vec!["Label_5", MARKER_ID]);
    }

    /// A pin-only filter stamps every matched message in ONE marker-only write, after the
    /// pins, and pins at most one message per thread.
    #[test]
    fn test_pin_only_filter_stamps_every_matched_message_in_one_write() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let messages = message_map(vec![
            msg_at("a1", "t1", "boss@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("a2", "t1", "boss@example.com", &["INBOX", "UNREAD"], 2_000),
            msg_at("b1", "t2", "boss@example.com", &["INBOX", "UNREAD"], 3_000),
        ]);
        let scoped = scope_one(&filter, &["a1", "a2", "b1"]);

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD"]), ("t2", &["INBOX", "UNREAD"])]),
            &live_resolver(),
        );

        assert_eq!(writes.len(), 2, "one pin write, then one marker write");
        assert_eq!(writes[0].action, FilterAction::Star);
        assert_eq!(writes[0].ids, vec!["a2", "b1"], "one star per thread");
        assert_eq!(writes[1].action, FilterAction::Tag(MARKER.to_string()));
        assert_eq!(writes[1].ids, vec!["a1", "a2", "b1"]);
        assert_eq!(writes[1].add, vec![MARKER_ID]);
        assert!(writes[1].remove.is_empty(), "a marker removes nothing");
    }

    /// Two DIFFERENT filters matching two DIFFERENT messages in the SAME thread still add
    /// only one star: claiming is per message, so both messages are matched, but the pin
    /// is per thread. Without folding each run's planned pins back into the thread union,
    /// eratosthenes would keep manufacturing the multi-star threads it is meant to stop.
    #[test]
    fn test_two_filters_matching_one_thread_still_add_only_one_star() {
        let alpha = from_filter("alpha", &["*@example.com"]);
        let beta = from_filter("beta", &["*@example.com"]);

        let messages = message_map(vec![
            msg_at("m1", "t1", "boss@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("m2", "t1", "boss@example.com", &["INBOX", "UNREAD"], 2_000),
        ]);
        let scoped = vec![
            FilterCandidates {
                filter: &alpha,
                ids: vec!["m1".to_string()],
            },
            FilterCandidates {
                filter: &beta,
                ids: vec!["m2".to_string()],
            },
        ];

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD"])]),
            &live_resolver(),
        );

        assert_eq!(
            adds(&writes, "STARRED"),
            vec!["m1"],
            "one star for the thread, not one per filter: {:?}",
            writes
        );
    }

    /// The Phase 0 fall-through, which per-filter `labels.excluded` caused: a message the
    /// first filter skips falls through to a broader filter and is newly FLAGGED. With one
    /// uniform marker there is no fall-through, because every filter rejects it for the
    /// same reason.
    #[test]
    fn test_uniform_marker_has_no_fall_through_to_a_broader_filter() {
        let leadership = from_filter("leadership", &["*@example.com"]);
        let mut only_me_ttv = from_filter("only-me-ttv", &["*"]);
        only_me_ttv.actions = vec![FilterAction::Flag];

        let messages = message_map(vec![msg_at(
            "id-1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD", "STARRED", MARKER_ID],
            1_000,
        )]);
        let scoped = vec![
            FilterCandidates {
                filter: &leadership,
                ids: vec!["id-1".to_string()],
            },
            FilterCandidates {
                filter: &only_me_ttv,
                ids: vec!["id-1".to_string()],
            },
        ];

        let writes = plan_all_writes(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD", "STARRED", MARKER_ID])]),
            &live_resolver(),
        );

        assert!(
            writes.is_empty(),
            "no filter may act on a marked message: {:?}",
            writes
        );
        assert!(adds(&writes, "IMPORTANT").is_empty());
    }

    /// Per-filter scope: two filters whose queries returned DISJOINT id sets, where each
    /// filter's criteria would happily match the other's message. Neither may touch the
    /// other's candidates. This is the pooled-candidates defect, pinned.
    #[test]
    fn test_scope_isolation_filters_never_match_another_filters_candidates() {
        let alpha = from_filter("alpha", &["*@example.com"]);
        let beta = from_filter("beta", &["*@example.com"]);

        let messages = message_map(vec![
            unread("id-alpha", "alice@example.com"),
            unread("id-beta", "bob@example.com"),
        ]);

        let scoped = vec![
            FilterCandidates {
                filter: &alpha,
                ids: vec!["id-alpha".to_string()],
            },
            FilterCandidates {
                filter: &beta,
                ids: vec!["id-beta".to_string()],
            },
        ];

        let matched = match_message_filters(&scoped, &messages, &empty_resolver(), MARKER, "");

        assert_eq!(matched, vec![vec!["id-alpha"], vec!["id-beta"]]);
    }

    /// `claimed` still spans filters: an id in BOTH queries goes to the first filter only.
    #[test]
    fn test_first_match_wins_still_spans_filters() {
        let alpha = from_filter("alpha", &["*@example.com"]);
        let beta = from_filter("beta", &["*@example.com"]);

        let messages = message_map(vec![
            unread("shared", "alice@example.com"),
            unread("id-beta", "bob@example.com"),
        ]);

        let scoped = vec![
            FilterCandidates {
                filter: &alpha,
                ids: vec!["shared".to_string()],
            },
            FilterCandidates {
                filter: &beta,
                ids: vec!["shared".to_string(), "id-beta".to_string()],
            },
        ];

        let matched = match_message_filters(&scoped, &messages, &empty_resolver(), MARKER, "");

        assert_eq!(matched, vec![vec!["shared"], vec!["id-beta"]]);
    }

    /// The query is a prefilter, so candidates get dropped here: a read message, one
    /// failing the glob Gmail could not enforce, and one never fetched.
    #[test]
    fn test_matched_is_a_subset_of_the_filters_own_candidates() {
        let filter = from_filter("only-example", &["*@example.com"]);

        let messages = message_map(vec![
            unread("keep", "alice@example.com"),
            message("read", "bob@example.com", &["INBOX"]),
            unread("other-domain", "carol@other.test"),
        ]);

        let scoped = vec![FilterCandidates {
            filter: &filter,
            ids: vec![
                "keep".to_string(),
                "read".to_string(),
                "other-domain".to_string(),
                "never-fetched".to_string(),
            ],
        }];

        let matched = match_message_filters(&scoped, &messages, &empty_resolver(), MARKER, "");

        assert_eq!(matched, vec![vec!["keep"]]);
        assert!(matched[0].len() <= scoped[0].ids.len());
    }

    /// Design doc 2026-09-03, Phase 3 defect #4: a custom label arrives from Gmail as a
    /// raw id (`Label_60`), not its name (`Oblivion`). A `labels.excluded: [Oblivion]`
    /// filter must resolve that id before comparing, or the exclusion is a silent no-op.
    #[test]
    fn test_labels_excluded_matches_resolved_custom_label_name() {
        let mut filter = from_filter("excludes-oblivion", &["*@example.com"]);
        filter.labels = LabelsFilter {
            included: vec![],
            excluded: vec![Label::Custom("Oblivion".to_string())],
        };

        let mut msg = unread("id-1", "alice@example.com");
        msg.label_ids.push("Label_60".to_string());
        let messages = message_map(vec![msg]);

        let resolver = LabelResolver::from_api_labels(vec![google_gmail1::api::Label {
            id: Some("Label_60".to_string()),
            name: Some("Oblivion".to_string()),
            ..Default::default()
        }]);

        let scoped = vec![FilterCandidates {
            filter: &filter,
            ids: vec!["id-1".to_string()],
        }];

        let matched = match_message_filters(&scoped, &messages, &resolver, MARKER, "");

        assert_eq!(matched, vec![Vec::<String>::new()]);
    }

    #[test]
    fn test_build_active_threads_query() {
        let filters = vec![
            StateFilter {
                name: "Starred".to_string(),
                labels: vec![Label::Starred],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            StateFilter {
                name: "Important".to_string(),
                labels: vec![Label::Important],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            StateFilter {
                name: "Cull".to_string(),
                labels: vec![Label::Inbox],
                ttl: Ttl::Days(chrono::Duration::days(7)),
                action: StateAction::Move("Purgatory".to_string()),
            },
            StateFilter {
                name: "Purge".to_string(),
                labels: vec![Label::Custom("Purgatory".to_string())],
                ttl: Ttl::Days(chrono::Duration::days(3)),
                action: StateAction::Move("Oblivion".to_string()),
            },
        ];

        let query = build_active_threads_query(&filters);
        assert!(query.contains("in:inbox"));
        assert!(query.contains("is:starred"));
        assert!(query.contains("is:important"));
        assert!(query.contains("label:purgatory"));
        assert!(query.contains("label:oblivion"));
        // The marker is NOT a state-filter label, so it never enters the active-thread
        // query. If it did, stage sanitization would reach the messages it marks.
        assert!(
            !query.to_lowercase().contains("triaged"),
            "marker leaked into the state-filter query: {}",
            query
        );
    }

    #[test]
    fn test_derive_stages() {
        let filters = vec![
            StateFilter {
                name: "Starred".to_string(),
                labels: vec![Label::Starred],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            StateFilter {
                name: "Important".to_string(),
                labels: vec![Label::Important],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            StateFilter {
                name: "Cull".to_string(),
                labels: vec![Label::Inbox],
                ttl: Ttl::Days(chrono::Duration::days(7)),
                action: StateAction::Move("Purgatory".to_string()),
            },
            StateFilter {
                name: "Purge".to_string(),
                labels: vec![Label::Custom("Purgatory".to_string())],
                ttl: Ttl::Days(chrono::Duration::days(3)),
                action: StateAction::Move("Oblivion".to_string()),
            },
        ];

        let stages = derive_stages(&filters);
        assert_eq!(stages, vec!["INBOX", "Purgatory", "Oblivion"]);
    }

    #[test]
    fn test_derive_stages_skips_keep_filters() {
        let filters = vec![StateFilter {
            name: "Starred".to_string(),
            labels: vec![Label::Starred],
            ttl: Ttl::Keep,
            action: StateAction::Move(String::new()),
        }];

        let stages = derive_stages(&filters);
        assert_eq!(stages, vec!["INBOX"]);
    }

    /// A resolver holding the stage and bucket labels the state-filter tests move threads
    /// between, i.e. what `ensure_labels` leaves registered before Phase 2 runs.
    fn state_resolver() -> LabelResolver {
        LabelResolver::from_api_labels(vec![
            custom_label("Label_7", "Purgatory"),
            custom_label("Label_8", "Oblivion"),
            custom_label("Label_9", "llm/noise"),
        ])
    }

    fn move_filter(name: &str, labels: &[&str], dest: &str) -> StateFilter {
        StateFilter {
            name: name.to_string(),
            labels: labels.iter().map(|l| Label::new(l)).collect(),
            ttl: Ttl::Days(chrono::Duration::days(3)),
            action: StateAction::Move(dest.to_string()),
        }
    }

    /// The production ladder (tatari.yml): Keep guards, an `INBOX` Cull into Purgatory,
    /// Purgatory draining to Oblivion, plus the Phase 3 bucket shape that scopes on a
    /// classification label instead of a stage.
    fn ladder_filters() -> Vec<StateFilter> {
        vec![
            StateFilter {
                name: "Starred".to_string(),
                labels: vec![Label::Starred],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            move_filter("Cull", &["INBOX"], "Purgatory"),
            move_filter("age-noise", &["llm/noise"], "Purgatory"),
            move_filter("Purge", &["Purgatory"], "Oblivion"),
        ]
    }

    fn labels_of(names: &[&str]) -> Vec<Label> {
        names.iter().map(|n| Label::new(n)).collect()
    }

    /// Existing shape 1, the stage transition: a thread sitting in Purgatory advances to
    /// Oblivion, shedding `INBOX` and the stage it came from. Bites against the old
    /// remove-own-labels behavior, which never removed `INBOX` here.
    #[test]
    fn test_state_move_stage_transition_sheds_inbox_and_the_current_stage() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["Purgatory", "UNREAD"]),
            &stages,
            "Oblivion",
            &state_resolver(),
        );

        assert_eq!(planned.add, vec!["Label_8".to_string()]);
        // This thread is NOT in INBOX, so INBOX is not in the plan. Removing it
        // unconditionally is what made a no-op look like a real write and broke the
        // ladder (implementation audit M1, 2026-09-07). What this test actually
        // guards is that the CURRENT stage is shed and the destination added.
        assert_eq!(planned.remove, vec!["Label_7".to_string()]);
    }

    /// Existing shape 2, the bare catch-all: a filter with no `labels:` at all still
    /// removes `INBOX` and nothing else.
    #[test]
    fn test_state_move_bare_catch_all_removes_only_inbox() {
        let filters = vec![move_filter("Cull", &[], "Purgatory")];
        let stages = derive_stages(&filters);
        let planned = plan_state_move(
            &labels_of(&["INBOX", "UNREAD", "CATEGORY_FORUMS"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert_eq!(planned.add, vec!["Label_7".to_string()]);
        assert_eq!(planned.remove, vec!["INBOX".to_string()]);
    }

    /// Phase 2 success criterion, the new shape: an `llm/noise -> Purgatory` filter removes
    /// `INBOX` and leaves `llm/noise` intact. Match labels are criteria, never removed --
    /// the old behavior stripped `llm/noise` and left the thread in the inbox.
    #[test]
    fn test_state_move_leaves_the_bucket_label_intact() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["INBOX", "UNREAD", "llm/noise"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert_eq!(planned.add, vec!["Label_7".to_string()]);
        assert_eq!(planned.remove, vec!["INBOX".to_string()]);
        assert!(
            !planned.remove.contains(&"Label_9".to_string()),
            "bucket label llm/noise must survive the move: {:?}",
            planned.remove
        );
    }

    /// A stage the thread does not carry is not removed: the plan touches the thread's
    /// actual location, not the whole ladder.
    #[test]
    fn test_state_move_ignores_stages_the_thread_does_not_carry() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["INBOX"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert_eq!(planned.remove, vec!["INBOX".to_string()]);
    }

    /// The destination is never in the remove set, so re-moving a thread to the stage it
    /// already occupies cannot cancel its own add.
    #[test]
    fn test_state_move_never_removes_its_own_destination() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["INBOX", "Purgatory"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        // The thread already carries Purgatory, so there is nothing to add. The
        // property this test guards -- the destination is never REMOVED -- still
        // holds, and the thread is still pulled out of the inbox it is sitting in.
        assert!(planned.add.is_empty(), "add={:?}", planned.add);
        assert_eq!(planned.remove, vec!["INBOX".to_string()]);
        assert!(
            !planned.remove.contains(&"Label_7".to_string()),
            "destination must never be removed: {:?}",
            planned.remove
        );
    }

    /// An `action:`-less filter has an empty Move destination: age the thread out of the
    /// inbox and its stage, add nothing. An empty label id would be a Gmail 400.
    #[test]
    fn test_state_move_with_no_destination_adds_nothing() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["INBOX", "Purgatory"]),
            &stages,
            "",
            &state_resolver(),
        );

        assert!(planned.add.is_empty(), "add={:?}", planned.add);
        assert_eq!(
            planned.remove,
            vec!["INBOX".to_string(), "Label_7".to_string()]
        );
    }

    /// THE FIXED-POINT REGRESSION (implementation audit, 2026-09-07).
    ///
    /// A `llm/noise -> Purgatory` filter still MATCHES a thread it already moved,
    /// because the bucket label is a match criterion and survives by design
    /// (Phase 2). Before this fix `plan_state_move` pushed `INBOX` into `remove`
    /// unconditionally, so the plan was never empty, the filter "fired" every run,
    /// `evaluate_thread` returned early, and `Purge` was never reached: the thread
    /// was rewritten every 5 minutes forever and Oblivion was dead config.
    #[test]
    fn test_state_move_is_a_true_noop_once_the_thread_is_at_the_destination() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            // Already moved: carries the bucket label AND the destination, no INBOX.
            &labels_of(&["UNREAD", "llm/noise", "Purgatory"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert!(
            planned.add.is_empty(),
            "destination already held, nothing to add: {:?}",
            planned.add
        );
        assert!(
            planned.remove.is_empty(),
            "thread is not in INBOX and is already at its destination, \
             so nothing may be removed: {:?}",
            planned.remove
        );
        assert!(
            planned.is_noop(),
            "this must report as a no-op so evaluate_thread keeps walking to Purge"
        );
    }

    /// INBOX is only shed when the thread actually carries it. Removing it
    /// unconditionally is what made the no-op above look like a real write.
    #[test]
    fn test_state_move_does_not_remove_inbox_when_the_thread_is_not_in_it() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["UNREAD", "Purgatory"]),
            &stages,
            "Oblivion",
            &state_resolver(),
        );

        assert!(
            !planned.remove.contains(&"INBOX".to_string()),
            "thread was not in INBOX: {:?}",
            planned.remove
        );
    }

    /// The ladder must TERMINATE: a thread parked in Purgatory by a bucket filter
    /// is still advanced to Oblivion by `Purge`. This is the property M1 broke.
    #[test]
    fn test_purge_still_advances_a_thread_a_bucket_filter_parked_in_purgatory() {
        let stages = derive_stages(&ladder_filters());
        let parked = labels_of(&["UNREAD", "llm/noise", "Purgatory"]);

        // The bucket filter that put it there is now inert.
        let bucket = plan_state_move(&parked, &stages, "Purgatory", &state_resolver());
        assert!(bucket.is_noop(), "bucket filter must no longer fire");

        // ...so Purge gets its turn, and it moves.
        let purge = plan_state_move(&parked, &stages, "Oblivion", &state_resolver());
        assert!(!purge.is_noop(), "Purge must still act on a parked thread");
        assert!(
            purge.remove.contains(&"Label_7".to_string()),
            "Purge sheds the Purgatory stage: {:?}",
            purge.remove
        );
    }

    /// THE PING-PONG REGRESSION (2026-10-05). A thread `Purge` already sent to Oblivion
    /// still carries its bucket label, so the bucket filter still matches it. Before this
    /// fix the plan removed Oblivion and re-added Purgatory, `Purge` reversed it the next
    /// run, and 643 threads flipped every run until Gmail 400'd one of the writes.
    #[test]
    fn test_bucket_filter_never_drags_an_oblivion_thread_back_to_purgatory() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["UNREAD", "llm/noise", "Oblivion"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert!(
            planned.is_noop(),
            "a backward move must be a no-op: add={:?} remove={:?}",
            planned.add,
            planned.remove
        );
    }

    /// The ladder reaches a fixed point: once in Oblivion, neither the bucket filter nor
    /// `Purge` has anything left to write, run after run.
    #[test]
    fn test_ladder_reaches_a_fixed_point_at_oblivion() {
        let stages = derive_stages(&ladder_filters());
        let resolver = state_resolver();
        let at_oblivion = labels_of(&["llm/noise", "Oblivion"]);

        for dest in ["Purgatory", "Oblivion"] {
            let planned = plan_state_move(&at_oblivion, &stages, dest, &resolver);
            assert!(
                planned.is_noop(),
                "move to {dest} must not write once the thread is in Oblivion: {planned:?}"
            );
        }
    }

    /// A reply puts an Oblivion thread back in INBOX. That thread is at stage 0 again, so
    /// `Cull` may still move it forward to Purgatory; the backward guard must not pin it.
    #[test]
    fn test_inbox_thread_carrying_a_later_stage_can_still_move_forward() {
        let stages = derive_stages(&ladder_filters());
        let planned = plan_state_move(
            &labels_of(&["INBOX", "Oblivion"]),
            &stages,
            "Purgatory",
            &state_resolver(),
        );

        assert_eq!(planned.add, vec!["Label_7".to_string()]);
        assert_eq!(
            planned.remove,
            vec!["INBOX".to_string(), "Label_8".to_string()]
        );
    }

    // ---- State filter ownership (D5) -----------------------------------------------

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_763_000_000, 0).expect("valid timestamp")
    }

    fn days_ago(days: i64) -> DateTime<Utc> {
        fixed_now() - chrono::Duration::days(days)
    }

    fn ttl_days(days: i64) -> Ttl {
        Ttl::Days(chrono::Duration::days(days))
    }

    fn ttl_read_unread(read: i64, unread: i64) -> Ttl {
        Ttl::Detailed {
            read: chrono::Duration::days(read),
            unread: chrono::Duration::days(unread),
        }
    }

    fn state_filter(name: &str, labels: &[&str], ttl: Ttl, dest: &str) -> StateFilter {
        StateFilter {
            name: name.to_string(),
            labels: labels.iter().map(|l| Label::new(l)).collect(),
            ttl,
            action: StateAction::Move(dest.to_string()),
        }
    }

    fn keep_filter(name: &str, label: &str) -> StateFilter {
        StateFilter {
            name: name.to_string(),
            labels: vec![Label::new(label)],
            ttl: Ttl::Keep,
            action: StateAction::Move(String::new()),
        }
    }

    /// The live work ladder's shape (tatari.yml): Keep guards, bucket filters ahead of
    /// a catch-all `Cull` on INBOX, and `Purge` draining Purgatory to Oblivion.
    fn work_filters() -> Vec<StateFilter> {
        vec![
            keep_filter("Starred", "Starred"),
            state_filter("age-receipts", &["llm/receipts"], ttl_days(7), "Purgatory"),
            state_filter("age-noise", &["llm/noise"], ttl_days(1), "Purgatory"),
            state_filter("Cull", &["INBOX"], ttl_read_unread(3, 21), "Purgatory"),
            state_filter("Purge", &["Purgatory"], ttl_days(3), "Oblivion"),
        ]
    }

    /// The personal starter config's shape (design doc `home.yml`).
    fn home_filters() -> Vec<StateFilter> {
        vec![
            keep_filter("Starred", "Starred"),
            keep_filter("Important", "Important"),
            state_filter(
                "personal",
                &["CATEGORY_PERSONAL"],
                ttl_read_unread(7, 30),
                "",
            ),
            state_filter(
                "promotions",
                &["CATEGORY_PROMOTIONS"],
                ttl_days(1),
                "Purgatory",
            ),
            state_filter("social", &["CATEGORY_SOCIAL"], ttl_days(1), "Purgatory"),
            state_filter("forums", &["CATEGORY_FORUMS"], ttl_days(3), "Purgatory"),
            state_filter(
                "updates",
                &["CATEGORY_UPDATES"],
                ttl_read_unread(3, 7),
                "Purgatory",
            ),
            state_filter("Cull", &["INBOX"], ttl_read_unread(3, 21), "Purgatory"),
            state_filter("Purge", &["Purgatory"], ttl_days(3), "Oblivion"),
        ]
    }

    fn ownership_resolver() -> LabelResolver {
        LabelResolver::from_api_labels(vec![
            custom_label("Label_7", "Purgatory"),
            custom_label("Label_8", "Oblivion"),
            custom_label("Label_9", "llm/noise"),
            custom_label("Label_10", "llm/receipts"),
        ])
    }

    fn verdict<'a>(
        labels: &[Label],
        age_days: i64,
        is_read: bool,
        filters: &'a [StateFilter],
    ) -> ThreadVerdict<'a> {
        plan_thread(
            labels,
            days_ago(age_days),
            is_read,
            filters,
            &derive_stages(filters),
            &ownership_resolver(),
            &FixedClock(fixed_now()),
        )
        .expect("plan_thread")
    }

    /// Apply a planned move to a thread's labels the way Gmail would, mapping the
    /// plan's wire ids back to names through the resolver.
    fn apply_move(labels: &[Label], planned: &PlannedMove, resolver: &LabelResolver) -> Vec<Label> {
        let name = |id: &String| Label::new(resolver.resolve_id(id).unwrap_or(id.as_str()));
        let removed: Vec<Label> = planned.remove.iter().map(name).collect();
        let mut out: Vec<Label> = labels
            .iter()
            .filter(|l| !removed.contains(l))
            .cloned()
            .collect();
        for added in planned.add.iter().map(name) {
            if !out.contains(&added) {
                out.push(added);
            }
        }
        out
    }

    fn held_by<'a>(v: &ThreadVerdict<'a>) -> Option<&'a str> {
        match v {
            ThreadVerdict::Held { filter } => Some(filter.name.as_str()),
            _ => None,
        }
    }

    /// D5, the live-config case: a read `llm/receipts` thread, 4 days old, is owned by
    /// `age-receipts` (7d) and must not be taken by `Cull` (read 3d) behind it.
    #[test]
    fn unexpired_bucket_filter_owns_thread_ahead_of_cull() {
        let filters = work_filters();
        let labels = labels_of(&["INBOX", "llm/receipts"]);

        let v = verdict(&labels, 4, true, &filters);

        assert_eq!(
            held_by(&v),
            Some("age-receipts"),
            "the unexpired bucket filter must own the thread, no move: {v:?}"
        );
    }

    /// Once the owner's own TTL expires it acts, on its own destination.
    #[test]
    fn expired_bucket_filter_moves_its_thread_to_purgatory() {
        let filters = work_filters();
        let labels = labels_of(&["INBOX", "llm/receipts"]);

        match verdict(&labels, 8, true, &filters) {
            ThreadVerdict::Move { filter, dest, .. } => {
                assert_eq!(filter.name, "age-receipts");
                assert_eq!(dest, "Purgatory");
            }
            other => panic!("expected age-receipts to move the thread, got {other:?}"),
        }
    }

    /// A thread no bucket filter matches is still `Cull`'s, on `Cull`'s TTL.
    #[test]
    fn unlabeled_inbox_thread_is_still_culled() {
        let filters = work_filters();
        match verdict(&labels_of(&["INBOX"]), 4, true, &filters) {
            ThreadVerdict::Move { filter, .. } => assert_eq!(filter.name, "Cull"),
            other => panic!("expected Cull to move the thread, got {other:?}"),
        }
    }

    /// Keep still owns unconditionally, ahead of everything.
    #[test]
    fn keep_filter_still_holds_a_starred_thread() {
        let filters = work_filters();
        let v = verdict(
            &labels_of(&["INBOX", "STARRED", "llm/noise"]),
            400,
            true,
            &filters,
        );
        assert_eq!(held_by(&v), Some("Starred"), "{v:?}");
    }

    /// D5 on the personal account: an action-less `personal` filter owns a read
    /// Personal-category thread. Run 1 at 4 days: no move. Run 2 at 8 days: INBOX is
    /// removed and no stage label is added. Before D5 run 1 fell through to `Cull`
    /// (read 3d) and the thread went to Purgatory, on its way to Oblivion.
    #[test]
    fn personal_category_thread_never_gets_a_stage_label() {
        let filters = home_filters();
        let resolver = ownership_resolver();
        let labels = labels_of(&["INBOX", "CATEGORY_PERSONAL"]);

        let run1 = verdict(&labels, 4, true, &filters);
        assert_eq!(
            held_by(&run1),
            Some("personal"),
            "run 1 must not move: {run1:?}"
        );

        let run2 = verdict(&labels, 8, true, &filters);
        let ThreadVerdict::Move {
            filter, planned, ..
        } = run2
        else {
            panic!("run 2 must move the thread out of the inbox: {run2:?}");
        };
        assert_eq!(filter.name, "personal");
        assert!(
            planned.add.is_empty(),
            "no stage label may be added: {planned:?}"
        );
        assert_eq!(planned.remove, vec!["INBOX".to_string()]);

        let after = apply_move(&labels, &planned, &resolver);
        assert_eq!(after, labels_of(&["CATEGORY_PERSONAL"]));
        for stage in ["Purgatory", "Oblivion"] {
            assert!(!after.contains(&Label::new(stage)), "{stage} on {after:?}");
        }

        // A third run over the archived thread: the personal move is now a no-op, it
        // falls through, and nothing else matches. It stays where it is.
        let run3 = verdict(&after, 40, true, &filters);
        assert_eq!(run3, ThreadVerdict::Unowned, "{run3:?}");
    }

    /// The no-op exception after expiry: a bucket-labeled thread parked in Purgatory,
    /// past both its bucket TTL and `Purge`'s, still reaches Oblivion. The bucket filter
    /// still matches (its label survives by design) but its move is already done.
    #[test]
    fn noop_move_still_falls_through_to_purge() {
        let filters = work_filters();
        let labels = labels_of(&["llm/noise", "Purgatory"]);

        match verdict(&labels, 10, false, &filters) {
            ThreadVerdict::Move {
                filter,
                dest,
                planned,
            } => {
                assert_eq!(filter.name, "Purge");
                assert_eq!(dest, "Oblivion");
                assert_eq!(planned.add, vec!["Label_8".to_string()]);
                assert_eq!(planned.remove, vec!["Label_7".to_string()]);
            }
            other => panic!("expected Purge to move the thread to Oblivion, got {other:?}"),
        }
    }

    /// The no-op exception before expiry (panel round 2): an unread Updates thread in
    /// Purgatory, 4 days old. `updates` (unread 7d) has not expired, but its move to
    /// Purgatory is already done, so it does not hold the thread there: `Purge` (3d)
    /// takes it to Oblivion.
    #[test]
    fn unexpired_filter_does_not_hold_a_thread_already_in_purgatory() {
        let filters = home_filters();
        let labels = labels_of(&["UNREAD", "CATEGORY_UPDATES", "Purgatory"]);

        match verdict(&labels, 4, false, &filters) {
            ThreadVerdict::Move { filter, dest, .. } => {
                assert_eq!(filter.name, "Purge");
                assert_eq!(dest, "Oblivion");
            }
            other => panic!("expected Purge to move the thread to Oblivion, got {other:?}"),
        }
    }

    /// A backward move is a no-op too: a bucket filter never owns a thread already in
    /// Oblivion, so the walk ends unowned instead of holding or dragging it back.
    #[test]
    fn bucket_filter_does_not_own_an_oblivion_thread() {
        let filters = work_filters();
        let v = verdict(&labels_of(&["llm/noise", "Oblivion"]), 0, false, &filters);
        assert_eq!(v, ThreadVerdict::Unowned, "{v:?}");
    }

    /// An unexpired `Delete` filter owns its thread like any other.
    #[test]
    fn unexpired_delete_filter_holds_its_thread() {
        let filters = vec![
            StateFilter {
                name: "trash-noise".to_string(),
                labels: vec![Label::new("llm/noise")],
                ttl: ttl_days(5),
                action: StateAction::Delete,
            },
            state_filter("Cull", &["INBOX"], ttl_days(1), "Purgatory"),
        ];
        let labels = labels_of(&["INBOX", "llm/noise"]);

        let v = verdict(&labels, 2, true, &filters);
        assert_eq!(held_by(&v), Some("trash-noise"), "{v:?}");

        match verdict(&labels, 6, true, &filters) {
            ThreadVerdict::Delete { filter } => assert_eq!(filter.name, "trash-noise"),
            other => panic!("expected the expired Delete to fire, got {other:?}"),
        }
    }

    /// The real data flow: thread label IDs off the wire resolve to names, the plan is
    /// computed over names, and the write comes back out as IDs.
    #[test]
    fn test_state_move_round_trips_wire_label_ids() {
        let resolver = state_resolver();
        let thread = GmailThread {
            id: "t1".to_string(),
            history_id: None,
            skipped_messages: 0,
            messages: vec![msg_at(
                "m1",
                "t1",
                "noreply@example.com",
                &["INBOX", "UNREAD", "Label_9"],
                1_000,
            )],
        };
        let stages = derive_stages(&ladder_filters());

        let planned = plan_state_move(
            &thread.labels_resolved(&resolver),
            &stages,
            "Purgatory",
            &resolver,
        );

        assert_eq!(planned.add, vec!["Label_7".to_string()]);
        assert_eq!(planned.remove, vec!["INBOX".to_string()]);
    }

    /// `--mark-only` success criterion: a pin filter's plan collapses to exactly one
    /// marker-only write over the FULL handled set (both threads' messages, not one per
    /// thread), with no `STARRED` add and no removal at all.
    #[test]
    fn test_mark_only_pin_filter_collapses_to_one_tag_write_over_the_full_handled_set() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let messages = message_map(vec![
            msg_at("a1", "t1", "boss@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("a2", "t1", "boss@example.com", &["INBOX", "UNREAD"], 2_000),
            msg_at("b1", "t2", "boss@example.com", &["INBOX", "UNREAD"], 3_000),
        ]);
        let scoped = scope_one(&filter, &["a1", "a2", "b1"]);

        let writes = plan_all_writes_mode(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD"]), ("t2", &["INBOX", "UNREAD"])]),
            &live_resolver(),
            true,
        );

        assert_eq!(
            writes.len(),
            1,
            "mark-only issues one write, no separate pin write: {:?}",
            writes
        );
        let write = &writes[0];
        assert_eq!(write.action, FilterAction::Tag(MARKER.to_string()));
        let mut ids = write.ids.clone();
        ids.sort();
        assert_eq!(
            ids,
            vec!["a1", "a2", "b1"],
            "every HANDLED message is stamped, not one per thread"
        );
        assert_eq!(write.add, vec![MARKER_ID.to_string()]);
        assert!(
            !write.add.contains(&"STARRED".to_string()),
            "mark-only must issue zero STARRED adds: {:?}",
            write
        );
        assert!(
            write.remove.is_empty(),
            "a marker-only write removes nothing"
        );
    }

    /// The trap named in the design doc: `plan_filter_writes` folds the marker into the
    /// `Move` write BECAUSE a `Move` destroys eligibility. In mark-only mode there is no
    /// `Move`, so nothing destroys eligibility, and the marker must NOT carry INBOX/UNREAD
    /// removal. A mark-only run that archived the `bots` candidates would be a serious
    /// defect, so this is tested explicitly rather than left to fall out of the collapse.
    #[test]
    fn test_mark_only_never_removes_inbox_or_unread_for_a_move_filter() {
        let mut filter = from_filter("bots", &["*@example.com"]);
        filter.actions = vec![FilterAction::Move("Bots".to_string())];
        let messages = message_map(vec![
            msg_at("bot-1", "t1", "ci@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("bot-2", "t1", "ci@example.com", &["INBOX", "UNREAD"], 2_000),
        ]);
        let scoped = scope_one(&filter, &["bot-1", "bot-2"]);

        // The thread already carries the Bots label from an older message, which under a
        // normal run would be irrelevant (Move is never suppressed); here it also proves
        // mark-only never touches the destination label at all.
        let writes = plan_all_writes_mode(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD", "Label_5"])]),
            &live_resolver(),
            true,
        );

        assert_eq!(
            writes.len(),
            1,
            "mark-only collapses to one marker-only write"
        );
        let write = &writes[0];
        assert_eq!(write.action, FilterAction::Tag(MARKER.to_string()));
        assert_eq!(write.ids, vec!["bot-1", "bot-2"], "the full HANDLED set");
        assert_eq!(write.add, vec![MARKER_ID.to_string()]);
        assert!(
            !write.add.contains(&"Label_5".to_string()),
            "no destination label added, no Move issued: {:?}",
            write
        );
        assert!(
            write.remove.is_empty(),
            "mark-only must not remove INBOX/UNREAD: archiving the bots candidates would \
             be a serious defect: {:?}",
            write
        );
    }

    /// A message already carrying the marker never re-enters the HANDLED set: it fails
    /// `matches()` just like a normal run, so mark-only stamps nothing for it either.
    #[test]
    fn test_mark_only_skips_an_already_marked_message() {
        let filter = from_filter("leadership", &["*@example.com"]);
        let msg = msg_at(
            "id-1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD", MARKER_ID],
            1_000,
        );
        let messages = message_map(vec![msg]);
        let scoped = scope_one(&filter, &["id-1"]);

        let writes = plan_all_writes_mode(
            &scoped,
            &messages,
            &thread_label_map(&[("t1", &["INBOX", "UNREAD", MARKER_ID])]),
            &live_resolver(),
            true,
        );

        assert!(
            writes.is_empty(),
            "nothing matched, so mark-only has nothing to stamp: {:?}",
            writes
        );
    }

    /// The logging half of the success criterion: "stamps N messages while logging
    /// exactly N lines". One INFO line per id whose metadata was fetched.
    #[test]
    fn test_log_mark_only_stamps_logs_exactly_one_line_per_stamped_message() {
        let messages = message_map(vec![
            msg_at("m1", "t1", "boss@example.com", &["INBOX", "UNREAD"], 1_000),
            msg_at("m2", "t2", "boss@example.com", &["INBOX", "UNREAD"], 2_000),
        ]);
        let ids = vec!["m1".to_string(), "m2".to_string()];

        let logged = log_mark_only_stamps(&ids, &messages, "");

        assert_eq!(logged, ids.len(), "one INFO line per stamped message");
    }

    /// An id whose metadata was never fetched cannot be logged with date/from/subject; it
    /// is skipped (and warned about) rather than silently miscounted as logged.
    #[test]
    fn test_log_mark_only_stamps_skips_ids_missing_from_the_fetched_set() {
        let messages = message_map(vec![msg_at(
            "m1",
            "t1",
            "boss@example.com",
            &["INBOX", "UNREAD"],
            1_000,
        )]);
        let ids = vec!["m1".to_string(), "missing".to_string()];

        let logged = log_mark_only_stamps(&ids, &messages, "");

        assert_eq!(
            logged, 1,
            "only the message actually fetched gets a log line"
        );
    }

    fn cached(history_id: u64) -> CachedThread {
        CachedThread {
            history_id,
            label_ids: vec!["INBOX".to_string()],
            last_activity: DateTime::from_timestamp_millis(1_000).expect("valid millis"),
            is_read: true,
        }
    }

    fn thread_ref(id: &str, history_id: Option<u64>) -> ThreadRef {
        ThreadRef {
            id: id.to_string(),
            history_id,
        }
    }

    #[test]
    fn test_fresh_entry_hits_only_on_a_matching_stamp() {
        let mut cache = ThreadCache::default();
        cache.insert("t1".to_string(), cached(7));

        assert_eq!(
            fresh_entry(&cache, &thread_ref("t1", Some(7))),
            Some(&cached(7))
        );
        assert_eq!(fresh_entry(&cache, &thread_ref("t1", Some(8))), None);
        assert_eq!(
            fresh_entry(&cache, &thread_ref("t1", None)),
            None,
            "a list item with no stamp is never trusted"
        );
        assert_eq!(fresh_entry(&cache, &thread_ref("t2", Some(7))), None);
    }

    #[test]
    fn test_fetch_reason_names_each_kind_of_miss() {
        let entry = cached(7);
        assert_eq!(fetch_reason(&thread_ref("t", Some(1)), None), "new");
        assert_eq!(
            fetch_reason(&thread_ref("t", Some(8)), Some(&entry)),
            "changed"
        );
        assert_eq!(
            fetch_reason(&thread_ref("t", None), Some(&entry)),
            "no-history-id"
        );
        assert_eq!(fetch_reason(&thread_ref("t", None), None), "no-history-id");
    }

    /// A cached `ThreadState` resolves its raw ids exactly as the fetched
    /// `GmailThread` does, so a hit and a fetch see the same labels.
    #[test]
    fn test_thread_state_resolves_labels_like_the_fetched_thread() {
        let resolver = state_resolver();
        let thread = GmailThread {
            id: "t1".to_string(),
            history_id: Some(3),
            skipped_messages: 0,
            messages: vec![msg_at(
                "m1",
                "t1",
                "noreply@example.com",
                &["INBOX", "UNREAD", "Label_9", "Label_unknown"],
                1_000,
            )],
        };
        let entry = CachedThread::from_thread(&thread).expect("cacheable");
        let state = ThreadState {
            id: "t1",
            label_ids: &entry.label_ids,
            last_activity: entry.last_activity,
            is_read: entry.is_read,
        };

        let mut from_state = state.labels_resolved(&resolver);
        let mut from_thread = thread.labels_resolved(&resolver);
        from_state.sort_by_key(|l| format!("{l:?}"));
        from_thread.sort_by_key(|l| format!("{l:?}"));
        assert_eq!(from_state, from_thread);
        assert!(from_state.contains(&Label::new("llm/noise")));
    }
}
