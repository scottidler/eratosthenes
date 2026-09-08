#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

pub mod cfg;
pub mod digest;
pub mod engine;
pub mod gmail;
pub mod slack;
pub mod triage;

use crate::cfg::config::{Config, load_config};
use crate::cfg::state::StateAction;
use crate::cfg::triage::TriageConfig;
use crate::digest::{DigestItem, bullets};
use crate::slack::SlackPoster;
use crate::triage::claude::ClaudeCli;
use crate::triage::thread::TriageThread;
use eyre::{Context, Result};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

pub fn load(config_path: &Path) -> Result<Config> {
    load_config(config_path).context("Failed to load configuration")
}

/// Ceiling on establishing a TCP connection, distinct from the per-request
/// ceilings in `gmail::rate` and `slack`. A connect hang is the black-hole case
/// (a SYN into a void) and should fail fast; a request that has already
/// connected may legitimately take longer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared HTTPS connector for BOTH outbound paths, Gmail and Slack.
///
/// Built by hand rather than via `HttpsConnectorBuilder::build()` for one
/// reason: that convenience method hands the client a default `HttpConnector`
/// with NO connect timeout, and `hyper_util`'s legacy client adds no request,
/// response, or connect timeout of its own. The result was that every Gmail
/// call and the Slack post could not fail, only hang. Shared rather than
/// duplicated so the two paths cannot drift apart on the bound.
///
/// `enforce_http(false)` is not optional: `HttpConnector` rejects any
/// non-`http` scheme by default and would refuse every `https` URI here.
/// hyper-rustls's own `build()` does exactly this, for exactly this reason.
pub(crate) fn https_connector()
-> Result<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>> {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.enforce_http(false);

    Ok(hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("Failed to load native TLS roots")?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http))
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
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https_connector()?),
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

    // A pin is thread-scoped here: ANY starred message makes the whole thread
    // starred, even one that is not itself in the inbox. That is why the pin and
    // the inbox membership are two SEPARATE queries intersected by thread id,
    // rather than one `in:inbox is:starred` query.
    //
    // Gmail applies a conjunction of message-level predicates to the SAME
    // message, then returns the thread if any one message satisfies the whole
    // thing. So `in:inbox is:starred` misses a thread whose only star sits on a
    // SENT reply while its other messages are the ones in the inbox -- which is
    // exactly what happens when you reply to a thread and star your own reply.
    // Gmail's own Starred view uses the single predicate and shows it; the
    // digest must agree with that view.
    let inbox_ids = client
        .list_threads(&format!("in:inbox{}", stage_exclusions))
        .await
        .context("listing inbox threads")?;
    let inbox_set: HashSet<String> = inbox_ids.into_iter().collect();

    // NO machine-chosen section here, deliberately. A `Needs Reply` section fed
    // from the triage layer's `llm/needs-reply` bucket shipped and was removed
    // after one live run: it contributed 15 rows against the 5 the human had
    // pinned, 9 of them Greenhouse pipeline notifications, and the digest
    // stopped being "what I pinned" and became "what a model picked". The
    // pinned set below is curated by a human, which is the entire point.
    let starred_ids: Vec<String> = client
        .list_threads(&format!("is:starred{}", stage_exclusions))
        .await
        .context("listing starred threads")?
        .into_iter()
        .filter(|id| inbox_set.contains(id))
        .collect();
    let important_ids: Vec<String> = client
        .list_threads(&format!("is:important{}", stage_exclusions))
        .await
        .context("listing important threads")?
        .into_iter()
        .filter(|id| inbox_set.contains(id))
        .collect();

    let starred_set: HashSet<String> = starred_ids.iter().cloned().collect();
    let important_set: HashSet<String> = important_ids.iter().cloned().collect();

    // Fetch each unique thread once (a thread can be pinned by more than one of
    // the three signals; it still gets exactly one digest line).
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

    let mut items = digest::build(&threads, &starred_set, &important_set);

    // Bullets are EXPECTED only when the account has a `triage:` block. Without
    // one the digest posts un-enriched and carries NO banner: that is the
    // feature working as designed, not a degraded run.
    let banner: Option<String> = match config.triage.as_ref() {
        Some(triage) if !items.is_empty() => enrich_digest(&client, triage, &mut items, &prefix)
            .await?
            .map(|reason| bullets::banner_reason(&reason)),
        _ => None,
    };

    // Block Kit is the BODY; the mrkdwn `format` output is the notification fallback.
    //
    // Both are built, deliberately. `blocks` is what a reader sees -- real bulleted lists, an
    // `emoji` element per section header, literal text that needs no escaping. `fallback_text`
    // would be the honest fallback, but `format` carries the whole digest, so a client that
    // cannot render blocks still gets the content rather than a bare header.
    let text = digest::format(&items, slack.browser_index, banner.as_deref());
    let blocks = digest::blocks::format_blocks(&items, slack.browser_index, banner.as_deref());

    debug!(
        "digest: posting to channel={}, items={}, degraded={}, blocks={}",
        slack.channel,
        items.len(),
        banner.is_some(),
        blocks.as_array().map_or(0, Vec::len)
    );
    poster
        .post(&slack.channel, &text, Some(&blocks))
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

/// Attach LLM bullets to the pinned items, in one batched `claude` call.
///
/// Returns the REASON the digest is degraded, or `None` when the items carry
/// their bullets. Every LLM failure lands here as a reason rather than an
/// error: the digest contract never depends on the Anthropic API, so the post
/// always goes out, subjects and deep links intact.
///
/// Stateless: nothing is cached, so the bullets always describe the mailbox as
/// it is at digest time.
async fn enrich_digest(
    client: &gmail::client::GmailClient,
    triage: &TriageConfig,
    items: &mut [DigestItem],
    prefix: &str,
) -> Result<Option<String>> {
    debug!(
        "{}enrich_digest: items={}, model={}",
        prefix,
        items.len(),
        triage.classify_model
    );

    // Bullets need BODIES, and the digest's own fetch is metadata-only. A
    // second, full fetch of the pinned set is the cost of that; the pinned set
    // is tens of threads, not thousands.
    let mut threads: Vec<TriageThread> = Vec::new();
    for item in items.iter() {
        match client.get_thread_full(&item.thread_id).await {
            Ok(raw) => match TriageThread::from_api(raw) {
                Ok(thread) => threads.push(thread),
                Err(e) => warn!(
                    "{}skipping unreadable thread {} in the bullet pass: {:#}",
                    prefix, item.thread_id, e
                ),
            },
            Err(e) => warn!(
                "{}fetching thread {} at format=full failed: {:#}",
                prefix, item.thread_id, e
            ),
        }
    }
    if threads.is_empty() {
        warn!(
            "{}no pinned thread could be read at format=full; posting without bullets",
            prefix
        );
        return Ok(Some("thread bodies unreadable".to_string()));
    }

    let self_address = client
        .profile_email()
        .await
        .context("resolving the account's own address")?;

    // Summarization runs on `classify-model`, not `draft-model`: drafting is a
    // different job with a different model.
    let cli =
        match ClaudeCli::resolve(triage.claude_binary.as_deref(), bullets::DIGEST_TIMEOUT).await {
            Ok(cli) => cli,
            Err(e) => {
                warn!("{}bullet pass unavailable: {}", prefix, e);
                return Ok(Some(e.class.as_str().to_string()));
            }
        };
    info!(
        "{}bullet pass: claude version={}, threads={}",
        prefix,
        cli.version(),
        threads.len()
    );

    let prompt = bullets::build_prompt();
    let payload = triage::classify::build_payload(&threads, triage.body_chars, &self_address)
        .context("building the bullet payload")?;
    let requested: Vec<String> = threads.iter().map(|t| t.id.clone()).collect();

    let raw = match cli.invoke(&triage.classify_model, &prompt, &payload).await {
        Ok(raw) => raw,
        Err(e) => {
            warn!("{}bullet pass failed: {}", prefix, e);
            return Ok(Some(e.class.as_str().to_string()));
        }
    };

    match bullets::parse_response(&raw, &requested) {
        Ok(by_thread) => {
            digest::attach_bullets(items, &by_thread);
            Ok(None)
        }
        Err(e) => {
            warn!("{}bullet response was unusable: {:#}", prefix, e);
            Ok(Some(
                crate::triage::claude::FailureClass::Protocol
                    .as_str()
                    .to_string(),
            ))
        }
    }
}

/// Classify one account's new inbox threads into `llm/*` bucket labels.
/// A no-op for an account with no `triage:` block; the caller skips those
/// before authenticating, and `triage::execute` re-checks.
pub async fn triage(account: &str, config: &Config, dry_run: bool, multi: bool) -> Result<()> {
    debug!("triage: account={}, dry_run={}", account, dry_run);

    let prefix = if multi {
        format!("[{}] ", account)
    } else {
        String::new()
    };

    let mut client = build_gmail_client(config, &prefix).await?;
    triage::execute(&mut client, config, &prefix, dry_run).await
}
