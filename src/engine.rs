use std::collections::{HashMap, HashSet};

use eyre::Result;
use log::{debug, info, trace, warn};

use crate::cfg::config::Config;
use crate::cfg::filter::{FilterAction, MessageFilter};
use crate::cfg::label::Label;
use crate::cfg::state::{Clock, StateAction, StateFilter, Ttl};
use crate::gmail::client::GmailClient;
use crate::gmail::label::create_label_if_missing;
use crate::gmail::message::{GmailMessage, GmailThread};
use crate::gmail::query::compile_query;

pub async fn execute(client: &mut GmailClient, config: &Config, prefix: &str, dry_run: bool) -> Result<()> {
    debug!(
        "{}execute: dry_run={}, message_filters={}, state_filters={}",
        prefix,
        dry_run,
        config.message_filters.len(),
        config.state_filters.len()
    );

    if dry_run {
        println!("{}=== DRY RUN - no changes will be made ===", prefix);
    }

    ensure_labels(client, config).await?;

    info!("{}=== Phase 0: Stage Sanitization ===", prefix);
    let sanitized = sanitize_stages(client, &config.state_filters, prefix, dry_run).await?;
    if sanitized > 0 {
        println!(
            "{}[sanitize] cleaned {} threads with conflicting stage labels",
            prefix, sanitized
        );
    }

    info!("{}=== Phase 1: Message Filters ===", prefix);
    let total_matched = execute_message_filters(client, &config.message_filters, prefix, dry_run).await?;

    info!("{}=== Phase 2: State Filters (Thread Age-Off) ===", prefix);
    let total_transitioned = execute_state_filters(client, &config.state_filters, prefix, dry_run).await?;

    println!(
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
            if let FilterAction::Move(dest) = action {
                needed.push(dest.clone());
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
    debug!("ensure_labels: needed={:?}", needed);

    let hub = client.hub().clone();
    for name in &needed {
        create_label_if_missing(&hub, &mut client.resolver, name).await?;
    }

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

            // Gmail search: "in:inbox" for INBOX, "label:x" for custom
            let early_query = if early == "INBOX" {
                "in:inbox".to_string()
            } else {
                format!("label:{}", early.to_lowercase())
            };
            let late_query = format!("label:{}", late.to_lowercase());
            let query = format!("{} {}", early_query, late_query);

            debug!(
                "{}[sanitize] checking conflict: {} + {} -> query: {}",
                prefix, early, late, query
            );
            let thread_ids = client.list_threads(&query).await?;

            if thread_ids.is_empty() {
                continue;
            }

            println!(
                "{}[sanitize] {} threads have both {} and {} - removing {}",
                prefix,
                thread_ids.len(),
                early,
                late,
                late
            );

            if !dry_run {
                // Collect all message IDs from conflicting threads
                let mut all_msg_ids = Vec::new();
                for tid in &thread_ids {
                    let thread = client.get_thread(tid).await?;
                    all_msg_ids.extend(thread.all_message_ids());
                }

                let late_label_id = client.resolver.resolve_name(late).unwrap_or(late.as_str()).to_string();

                client.batch_modify(&all_msg_ids, &[], &[late_label_id]).await?;
            }

            total_cleaned += thread_ids.len();
        }
    }

    Ok(total_cleaned)
}

/// Phase 1: ACL-style message filter execution.
/// Fetches all candidate messages once, then evaluates filters in order.
/// First matching filter claims the message - it is excluded from further filters.
async fn execute_message_filters(
    client: &GmailClient,
    filters: &[MessageFilter],
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    debug!(
        "{}execute_message_filters: count={}, dry_run={}",
        prefix,
        filters.len(),
        dry_run
    );

    // Collect unique candidate IDs across all filters
    let mut all_ids: Vec<String> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for filter in filters {
        let query = compile_query(filter);
        if query.is_empty() {
            warn!("Filter '{}' compiles to empty query, skipping", filter.name);
            continue;
        }
        println!("{}[filter:{}] searching: {}", prefix, filter.name, query);
        let ids = client.search_messages(&query).await?;
        debug!(
            "{}[filter:{}] query returned {} candidates",
            prefix,
            filter.name,
            ids.len()
        );
        for id in ids {
            if seen_ids.insert(id.clone()) {
                all_ids.push(id);
            }
        }
    }

    println!(
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
            println!("{}[phase1] [{}/{}] fetching...", prefix, i + 1, total);
        }
        messages.insert(id.clone(), msg);
    }

    // ACL evaluation: first matching filter claims each message
    let mut claimed: HashSet<String> = HashSet::new();
    let mut total_matched = 0usize;

    for filter in filters {
        let mut matched_ids: Vec<String> = Vec::new();

        for id in &all_ids {
            if claimed.contains(id) {
                continue;
            }
            let Some(msg) = messages.get(id) else {
                continue;
            };
            // Only match unread messages (belt and suspenders with is:unread in query)
            if msg.is_read() {
                trace!("{}[filter:{}] skipping {} (read)", prefix, filter.name, id);
                continue;
            }
            let labels = msg.labels();
            trace!(
                "{}[filter:{}] checking {} to={:?} cc={:?} from={:?}",
                prefix, filter.name, id, msg.to, msg.cc, msg.from
            );
            if filter.matches(&msg.to, &msg.cc, &msg.from, &msg.subject, &labels, &msg.headers) {
                debug!(
                    "{}[filter:{}] MATCH: {} (from: {})",
                    prefix,
                    filter.name,
                    msg.subject,
                    msg.from.first().map(|s| s.as_str()).unwrap_or("?")
                );
                println!(
                    "{}[filter:{}] MATCH: {} (from: {})",
                    prefix,
                    filter.name,
                    msg.subject,
                    msg.from.first().map(|s| s.as_str()).unwrap_or("?")
                );
                matched_ids.push(id.clone());
            }
        }

        // Claim matched messages - excluded from further filters
        for id in &matched_ids {
            claimed.insert(id.clone());
        }

        println!(
            "{}[filter:{}] {} matched (total claimed: {})",
            prefix,
            filter.name,
            matched_ids.len(),
            claimed.len()
        );

        if !matched_ids.is_empty() {
            total_matched += matched_ids.len();
            for action in &filter.actions {
                apply_filter_action(client, &matched_ids, action, &filter.name, prefix, dry_run).await?;
            }
        }
    }

    Ok(total_matched)
}

async fn apply_filter_action(
    client: &GmailClient,
    ids: &[String],
    action: &FilterAction,
    filter_name: &str,
    prefix: &str,
    dry_run: bool,
) -> Result<()> {
    debug!(
        "{}apply_filter_action: filter={}, action={:?}, count={}, dry_run={}",
        prefix,
        filter_name,
        action,
        ids.len(),
        dry_run
    );

    match action {
        FilterAction::Star => {
            println!("{}[filter:{}] starring {} messages", prefix, filter_name, ids.len());
            if !dry_run {
                let add = vec!["STARRED".to_string()];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
        FilterAction::Flag => {
            println!(
                "{}[filter:{}] flagging {} messages as important",
                prefix,
                filter_name,
                ids.len()
            );
            if !dry_run {
                let add = vec!["IMPORTANT".to_string()];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
        FilterAction::Move(dest) => {
            let dest_id = client.resolver.resolve_name(dest).unwrap_or(dest.as_str()).to_string();
            println!(
                "{}[filter:{}] moving {} messages to {}",
                prefix,
                filter_name,
                ids.len(),
                dest
            );
            if !dry_run {
                let add = vec![dest_id];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
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
        info!("{}No state filter labels to query, skipping Phase 2", prefix);
        return Ok(0);
    }

    println!("{}[state] searching active threads...", prefix);
    debug!("{}[state] query: {}", prefix, active_query);
    let thread_ids = client.list_threads(&active_query).await?;
    println!("{}[state] {} active threads to evaluate", prefix, thread_ids.len());

    let clock = crate::cfg::state::RealClock;
    let total = thread_ids.len();
    let mut transitioned = 0usize;

    for (i, thread_id) in thread_ids.iter().enumerate() {
        if (i + 1) % 50 == 0 {
            println!("{}[state] [{}/{}] evaluating...", prefix, i + 1, total);
        }
        trace!("{}[state] [{}/{}] fetching thread {}", prefix, i + 1, total, thread_id);
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
    let thread_labels = thread.labels();
    debug!(
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
        debug!(
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
                    debug!("{}[thread:{}] protected by '{}'", prefix, thread.id, state_filter.name);
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
    let msg_ids = thread.all_message_ids();
    debug!(
        "{}apply_state_action: filter={}, thread={}, action={:?}, msgs={}, dry_run={}",
        prefix,
        state_filter.name,
        thread.id,
        action,
        msg_ids.len(),
        dry_run
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

            let remove = if remove_labels.is_empty() { vec!["INBOX".to_string()] } else { remove_labels };

            let dest_id = client.resolver.resolve_name(dest).unwrap_or(dest.as_str()).to_string();

            println!(
                "{}[state:{}] thread {} -> {} ({} msgs)",
                prefix,
                state_filter.name,
                thread.id,
                dest,
                msg_ids.len()
            );

            if !dry_run {
                let add = vec![dest_id];
                client.batch_modify(&msg_ids, &add, &remove).await?;
            }
        }
        StateAction::Delete => {
            println!(
                "{}[state:{}] trashing thread {} ({} msgs)",
                prefix,
                state_filter.name,
                thread.id,
                msg_ids.len()
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
    use crate::cfg::state::{StateAction, Ttl};

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
