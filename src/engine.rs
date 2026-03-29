use eyre::Result;
use log::{debug, info, warn};

use crate::cfg::config::Config;
use crate::cfg::filter::{FilterAction, MessageFilter};
use crate::cfg::label::Label;
use crate::cfg::state::{Clock, StateAction, StateFilter, Ttl};
use crate::gmail::client::GmailClient;
use crate::gmail::label::create_label_if_missing;
use crate::gmail::message::GmailThread;
use crate::gmail::query::compile_query;

pub async fn execute(client: &mut GmailClient, config: &Config, dry_run: bool) -> Result<()> {
    ensure_labels(client, config).await?;

    info!("=== Phase 1: Message Filters ===");
    for filter in &config.message_filters {
        execute_message_filter(client, filter, dry_run).await?;
    }

    info!("=== Phase 2: State Filters (Thread Age-Off) ===");
    execute_state_filters(client, &config.state_filters, dry_run).await?;

    info!("=== Execution complete ===");
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

    let hub = client.hub().clone();
    for name in &needed {
        create_label_if_missing(&hub, &mut client.resolver, name).await?;
    }

    Ok(())
}

async fn execute_message_filter(client: &GmailClient, filter: &MessageFilter, dry_run: bool) -> Result<()> {
    let query = compile_query(filter);
    if query.is_empty() {
        warn!("Filter '{}' compiles to empty query, skipping", filter.name);
        return Ok(());
    }

    info!("[filter:{}] query: {}", filter.name, query);
    let message_ids = client.search_messages(&query).await?;
    info!("[filter:{}] {} candidates from server", filter.name, message_ids.len());

    let mut matched_ids = Vec::new();
    for id in &message_ids {
        let msg = client.get_message(id).await?;
        let labels = msg.labels();
        if filter.matches(&msg.to, &msg.cc, &msg.from, &msg.subject, &labels, &msg.headers) {
            matched_ids.push(id.clone());
        }
    }

    info!(
        "[filter:{}] {} matched after local validation",
        filter.name,
        matched_ids.len()
    );

    if matched_ids.is_empty() {
        return Ok(());
    }

    for action in &filter.actions {
        apply_filter_action(client, &matched_ids, action, &filter.name, dry_run).await?;
    }

    Ok(())
}

async fn apply_filter_action(
    client: &GmailClient,
    ids: &[String],
    action: &FilterAction,
    filter_name: &str,
    dry_run: bool,
) -> Result<()> {
    match action {
        FilterAction::Star => {
            info!("[filter:{}] Starring {} messages", filter_name, ids.len());
            if !dry_run {
                let add = vec!["STARRED".to_string()];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
        FilterAction::Flag => {
            info!("[filter:{}] Flagging {} messages as important", filter_name, ids.len());
            if !dry_run {
                let add = vec!["IMPORTANT".to_string()];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
        FilterAction::Move(dest) => {
            let dest_id = client.resolver.resolve_name(dest).unwrap_or(dest.as_str()).to_string();
            info!("[filter:{}] Moving {} messages to {}", filter_name, ids.len(), dest);
            if !dry_run {
                let add = vec![dest_id];
                client.batch_modify(ids, &add, &[]).await?;
            }
        }
    }
    Ok(())
}

async fn execute_state_filters(client: &GmailClient, state_filters: &[StateFilter], dry_run: bool) -> Result<()> {
    let active_query = build_active_threads_query(state_filters);
    if active_query.is_empty() {
        info!("No state filter labels to query, skipping Phase 2");
        return Ok(());
    }

    info!("[state] query: {}", active_query);
    let thread_ids = client.list_threads(&active_query).await?;
    info!("[state] {} active threads", thread_ids.len());

    let clock = crate::cfg::state::RealClock;

    for thread_id in &thread_ids {
        let thread = client.get_thread(thread_id).await?;
        evaluate_thread(client, &thread, state_filters, &clock, dry_run).await?;
    }

    Ok(())
}

async fn evaluate_thread<C: Clock>(
    client: &GmailClient,
    thread: &GmailThread,
    state_filters: &[StateFilter],
    clock: &C,
    dry_run: bool,
) -> Result<()> {
    let thread_labels = thread.labels();

    for state_filter in state_filters {
        if !state_filter.matches_labels(&thread_labels) {
            continue;
        }

        let Some(last_activity) = thread.last_activity() else {
            warn!("Thread {} has no messages, skipping", thread.id);
            return Ok(());
        };

        let is_read = thread.is_read();

        match state_filter.evaluate_ttl(last_activity, is_read, clock)? {
            Some(action) => {
                apply_state_action(client, thread, state_filter, &action, dry_run).await?;
                break;
            }
            None => {
                if state_filter.ttl == Ttl::Keep {
                    debug!("[thread:{}] protected by '{}'", thread.id, state_filter.name);
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn apply_state_action(
    client: &GmailClient,
    thread: &GmailThread,
    state_filter: &StateFilter,
    action: &StateAction,
    dry_run: bool,
) -> Result<()> {
    let msg_ids = thread.all_message_ids();

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

            info!(
                "[state:{}] thread {} -> {} ({} msgs)",
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
            info!(
                "[state:{}] trashing thread {} ({} msgs)",
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
                labels: vec![Label::Important, Label::Starred],
                ttl: Ttl::Keep,
                action: StateAction::Move(String::new()),
            },
            StateFilter {
                name: "Cull".to_string(),
                labels: vec![],
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
        assert!(query.contains("is:important"));
        assert!(query.contains("is:starred"));
        assert!(query.contains("label:purgatory"));
        assert!(query.contains("label:oblivion"));
    }
}
