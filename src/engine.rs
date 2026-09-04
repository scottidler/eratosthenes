use std::collections::{HashMap, HashSet};

use eyre::Result;
use log::{debug, info, trace, warn};

use crate::cfg::config::Config;
use crate::cfg::filter::{FilterAction, MessageFilter};
use crate::cfg::label::Label;
use crate::cfg::state::{Clock, StateAction, StateFilter, Ttl};
use crate::gmail::client::GmailClient;
use crate::gmail::label::{LabelResolver, LabelVisibility, create_label_if_missing};
use crate::gmail::message::{GmailMessage, GmailThread};
use crate::gmail::query::compile_query;

pub async fn execute(
    client: &mut GmailClient,
    config: &Config,
    prefix: &str,
    dry_run: bool,
) -> Result<()> {
    debug!(
        "{}execute: dry_run={}, message_filters={}, state_filters={}",
        prefix,
        dry_run,
        config.message_filters.len(),
        config.state_filters.len()
    );

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
    let sanitized = sanitize_stages(client, &config.state_filters, prefix, dry_run).await?;
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
        prefix,
        dry_run,
    )
    .await?;

    info!("{}=== Phase 2: State Filters (Thread Age-Off) ===", prefix);
    let total_transitioned =
        execute_state_filters(client, &config.state_filters, prefix, dry_run).await?;

    info!(
        "{}Done: {} messages matched filters, {} threads transitioned{}",
        prefix,
        total_matched,
        total_transitioned,
        if dry_run { " (dry run)" } else { "" }
    );

    Ok(())
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
        create_label_if_missing(&hub, &mut client.resolver, name, LabelVisibility::Shown).await?;
    }

    // The marker must be in the resolver before any action folds its ID into an add-list:
    // unregistered, `resolve_name` returns None and the `unwrap_or(name)` fallback sends a
    // label NAME where Gmail wants an ID, which is a 400 rather than a clean error.
    // `ensure_labels` is the first thing `execute` does, so this ordering already holds.
    create_label_if_missing(
        &hub,
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

            if !dry_run {
                for tid in &thread_ids {
                    client
                        .modify_thread(tid, &[], std::slice::from_ref(&late_id))
                        .await?;
                }
            }

            total_cleaned += thread_ids.len();
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
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    debug!(
        "{}execute_message_filters: count={}, marker={}, dry_run={}",
        prefix,
        filters.len(),
        marker,
        dry_run
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
        let msg = client.get_message(id).await?;
        if (i + 1) % 50 == 0 {
            trace!("{}[phase1] [{}/{}] fetching...", prefix, i + 1, total);
        }
        messages.insert(id.clone(), msg);
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
        total_matched += matched_ids.len();

        if pins(filter) {
            fetch_thread_labels(client, matched_ids, &messages, &mut thread_labels, prefix).await?;
        }

        let writes = plan_filter_writes(
            filter,
            matched_ids,
            &messages,
            &thread_labels,
            &client.resolver,
            marker,
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
        // under --dry-run, so the preview is what a real run would do.
        record_planned_pins(&writes, &messages, &mut thread_labels);

        for write in &writes {
            apply_planned_write(client, write, &filter.name, prefix, dry_run).await?;
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
    prefix: &str,
) -> Result<()> {
    for id in matched_ids {
        let Some(msg) = messages.get(id) else {
            continue;
        };
        if thread_labels.contains_key(&msg.thread_id) {
            continue;
        }
        trace!(
            "{}[phase1] fetching thread {} for suppression check",
            prefix, msg.thread_id
        );
        let thread = client.get_thread(&msg.thread_id).await?;
        thread_labels.insert(msg.thread_id.clone(), thread.label_ids());
    }
    Ok(())
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
fn plan_filter_writes(
    filter: &MessageFilter,
    matched_ids: &[String],
    messages: &HashMap<String, GmailMessage>,
    thread_labels: &HashMap<String, HashSet<String>>,
    resolver: &LabelResolver,
    marker: &str,
) -> Vec<PlannedWrite> {
    if matched_ids.is_empty() {
        return Vec::new();
    }

    let marker_id = resolve_label_id(resolver, marker);
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
/// thread for pinning filters before this runs.
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
) -> Result<()> {
    debug!(
        "{}apply_planned_write: filter={}, action={:?}, count={}, add={:?}, remove={:?}, dry_run={}",
        prefix,
        filter_name,
        write.action,
        write.ids.len(),
        write.add,
        write.remove,
        dry_run
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
            info!(
                "{}[filter:{}] tagging {} messages with {}",
                prefix, filter_name, count, label
            );
        }
    }

    if !dry_run {
        client
            .batch_modify(&write.ids, &write.add, &write.remove)
            .await?;
    }
    Ok(())
}

async fn execute_state_filters(
    client: &GmailClient,
    state_filters: &[StateFilter],
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    debug!(
        "{}execute_state_filters: count={}, dry_run={}",
        prefix,
        state_filters.len(),
        dry_run
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
    let thread_ids = client.list_threads(&active_query).await?;
    debug!(
        "{}[state] {} active threads to evaluate",
        prefix,
        thread_ids.len()
    );

    let clock = crate::cfg::state::RealClock;
    let total = thread_ids.len();
    let mut transitioned = 0usize;

    for (i, thread_id) in thread_ids.iter().enumerate() {
        if (i + 1) % 50 == 0 {
            trace!("{}[state] [{}/{}] evaluating...", prefix, i + 1, total);
        }
        trace!(
            "{}[state] [{}/{}] fetching thread {}",
            prefix,
            i + 1,
            total,
            thread_id
        );
        let thread = client.get_thread(thread_id).await?;
        if evaluate_thread(client, &thread, state_filters, prefix, &clock, dry_run).await? {
            transitioned += 1;
        }
    }

    Ok(transitioned)
}

async fn evaluate_thread<C: Clock>(
    client: &GmailClient,
    thread: &GmailThread,
    state_filters: &[StateFilter],
    prefix: &str,
    clock: &C,
    dry_run: bool,
) -> Result<bool> {
    let thread_labels = thread.labels_resolved(&client.resolver);
    // Per-record: fires once per active thread every run. TRACE, not DEBUG --
    // at the default level this single line grew tatari.log ~48 MiB/day.
    trace!(
        "{}evaluate_thread: id={}, msgs={}, labels={:?}, is_read={}",
        prefix,
        thread.id,
        thread.messages.len(),
        thread_labels,
        thread.is_read()
    );

    for state_filter in state_filters {
        if !state_filter.matches_labels(&thread_labels) {
            trace!(
                "{}[thread:{}] filter '{}' labels don't match, skipping",
                prefix, thread.id, state_filter.name
            );
            continue;
        }

        let Some(last_activity) = thread.last_activity() else {
            warn!("{}Thread {} has no messages, skipping", prefix, thread.id);
            return Ok(false);
        };

        let is_read = thread.is_read();
        // Per-record: once per matched thread every run -> TRACE, not DEBUG.
        trace!(
            "{}[thread:{}] matched filter '{}': last_activity={}, is_read={}",
            prefix, thread.id, state_filter.name, last_activity, is_read
        );

        match state_filter.evaluate_ttl(last_activity, is_read, clock)? {
            Some(action) => {
                apply_state_action(client, thread, state_filter, &action, prefix, dry_run).await?;
                return Ok(true);
            }
            None => {
                if state_filter.ttl == Ttl::Keep {
                    // Per-record: once per protected thread every run -> TRACE.
                    trace!(
                        "{}[thread:{}] protected by '{}'",
                        prefix, thread.id, state_filter.name
                    );
                    return Ok(false);
                }
            }
        }
    }

    Ok(false)
}

async fn apply_state_action(
    client: &GmailClient,
    thread: &GmailThread,
    state_filter: &StateFilter,
    action: &StateAction,
    prefix: &str,
    dry_run: bool,
) -> Result<()> {
    debug!(
        "{}apply_state_action: filter={}, thread={}, action={:?}, dry_run={}",
        prefix, state_filter.name, thread.id, action, dry_run
    );

    match action {
        StateAction::Move(dest) => {
            let remove_labels: Vec<String> = state_filter
                .labels
                .iter()
                .map(|l| {
                    client
                        .resolver
                        .resolve_name(l.to_gmail_id())
                        .unwrap_or(l.to_gmail_id())
                        .to_string()
                })
                .collect();

            let remove = if remove_labels.is_empty() {
                vec!["INBOX".to_string()]
            } else {
                remove_labels
            };

            let dest_id = client
                .resolver
                .resolve_name(dest)
                .unwrap_or(dest.as_str())
                .to_string();

            debug!(
                "{}[state:{}] thread {} -> {}",
                prefix, state_filter.name, thread.id, dest,
            );

            if !dry_run {
                let add = vec![dest_id];
                client.modify_thread(&thread.id, &add, &remove).await?;
            }
        }
        StateAction::Delete => {
            debug!(
                "{}[state:{}] trashing thread {}",
                prefix, state_filter.name, thread.id,
            );
            if !dry_run {
                client.trash_thread(&thread.id).await?;
            }
        }
    }

    Ok(())
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
}
