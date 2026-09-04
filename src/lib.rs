#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

pub mod cfg;
pub mod digest;
pub mod engine;
pub mod gmail;
pub mod slack;

use crate::cfg::config::{Config, load_config};
use crate::cfg::state::StateAction;
use crate::slack::SlackPoster;
use eyre::{Context, Result};
use log::{debug, warn};
use std::collections::HashSet;
use std::path::Path;

pub fn load(config_path: &Path) -> Result<Config> {
    load_config(config_path).context("Failed to load configuration")
}

pub fn init_tls() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| eyre::eyre!("Failed to install rustls crypto provider"))
}

/// Authenticate and build a `GmailClient` over the shared hyper + hyper-rustls
/// stack. Used by both `run` and `digest` so they share one auth/transport path.
async fn build_gmail_client(config: &Config, prefix: &str) -> Result<gmail::client::GmailClient> {
    let auth = gmail::auth::build_authenticator(&config.auth)
        .await
        .context("OAuth2 authentication failed")?;

    let hub = google_gmail1::Gmail::new(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .context("Failed to load native TLS roots")?
                .https_or_http()
                .enable_http1()
                .build(),
        ),
        auth,
    );

    gmail::client::GmailClient::new(hub, prefix)
        .await
        .context("Failed to initialize Gmail client")
}

pub async fn run(
    account: &str,
    config: &Config,
    dry_run: bool,
    mark_only: bool,
    multi: bool,
) -> Result<()> {
    let prefix = if multi {
        format!("[{}] ", account)
    } else {
        String::new()
    };

    let mut client = build_gmail_client(config, &prefix).await?;

    // A header-based filter guard only works if the header is actually fetched.
    // Request the standard parsing headers plus every header any filter references.
    let mut metadata_headers: Vec<String> = ["To", "Cc", "From", "Subject"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for filter in &config.message_filters {
        for header_name in filter.headers.keys() {
            if !metadata_headers.contains(header_name) {
                metadata_headers.push(header_name.clone());
            }
        }
    }
    client.set_metadata_headers(metadata_headers);

    engine::execute(&mut client, config, &prefix, dry_run, mark_only).await
}

/// Build and post the pinned-inbox digest for one account. The caller only
/// invokes this when `config.slack` is `Some`; the channel and Gmail browser
/// slot for deep links come from that block. Queries the pinned set at the
/// THREAD level so each thread yields exactly one digest line.
pub async fn digest<P: SlackPoster>(account: &str, config: &Config, poster: &P) -> Result<()> {
    debug!("digest: account={}", account);

    let slack = config.slack.as_ref().ok_or_else(|| {
        eyre::eyre!(
            "digest called for account '{}' without a slack config",
            account
        )
    })?;

    let prefix = format!("[{}] ", account);

    let client = build_gmail_client(config, &prefix).await?;

    // Threads already aged into a state-filter's destination stage (e.g.
    // Purgatory, Oblivion) are excluded even if Gmail's own is:starred /
    // is:important still matches them (a stale classifier tag, or a later
    // reply that re-added INBOX without clearing the stage label). See
    // docs/design/2026-06-06-slack-digest.md's known-noise note.
    let stage_exclusions: String = config
        .state_filters
        .iter()
        .filter_map(|f| match &f.action {
            StateAction::Move(dest) if !dest.is_empty() => {
                Some(format!(" -label:{}", dest.to_lowercase()))
            }
            _ => None,
        })
        .collect();

    let starred_ids = client
        .list_threads(&format!("in:inbox is:starred{}", stage_exclusions))
        .await
        .context("listing starred threads")?;
    let important_ids = client
        .list_threads(&format!("in:inbox is:important{}", stage_exclusions))
        .await
        .context("listing important threads")?;

    let starred_set: HashSet<String> = starred_ids.iter().cloned().collect();
    let important_set: HashSet<String> = important_ids.iter().cloned().collect();

    // Fetch each unique thread once (a thread can be both starred and important).
    let mut unique: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for id in starred_ids.iter().chain(important_ids.iter()) {
        if seen.insert(id.clone()) {
            unique.push(id.clone());
        }
    }

    let mut threads = Vec::new();
    for id in &unique {
        match client.get_thread(id).await {
            Ok(thread) => threads.push(thread),
            Err(e) => warn!("{}skipping unreadable thread {}: {:#}", prefix, id, e),
        }
    }

    let items = digest::build(&threads, &starred_set, &important_set);
    let text = digest::format(&items, slack.browser_index);

    debug!(
        "digest: posting to channel={}, items={}",
        slack.channel,
        items.len()
    );
    poster
        .post(&slack.channel, &text)
        .await
        .context("posting digest to Slack")?;

    println!(
        "{}Digest posted: {} starred, {} important",
        prefix,
        starred_set.len(),
        important_set.len()
    );
    Ok(())
}
