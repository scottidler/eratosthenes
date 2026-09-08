use std::time::Duration;

use bytes::Bytes;
use eyre::{Context, Result, eyre};
use http_body_util::{BodyExt, Full};
use log::{debug, warn};
use serde::Deserialize;

const POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";

/// Per-request ceiling on the Slack post, covering the request AND the body
/// read: `into_body().collect()` is a second await on the network and a stalled
/// response body hangs just as completely as a stalled request.
///
/// Same defect as the Gmail path had (`gmail::rate::REQUEST_TIMEOUT`): the
/// hyper client applies no timeout, so this call could not fail, only hang, and
/// it would have hung the whole digest unit. Tighter than Gmail's 30s because
/// this is one small JSON POST, not a thread fetch. No retry here on purpose --
/// the digest is idempotent-ish but not idempotent, and a retried post risks a
/// double message; a missed digest is the better failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// HTTP client over the SAME hyper + hyper-rustls stack the Gmail client uses,
/// so there is no second TLS/crypto provider and no `reqwest` dependency.
type HyperRustlsClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// Transport port for posting a Slack message. Generic DI: digest callers take
/// `P: SlackPoster` so tests can substitute an in-memory fake.
///
/// Native `async fn` in trait (stable on edition 2024); no `async-trait`. The
/// `async_fn_in_trait` lint is allowed deliberately: this trait is consumed only
/// by generic code in this crate, never via `dyn`, so the missing auto-trait
/// bound the lint warns about does not apply.
#[allow(async_fn_in_trait)]
pub trait SlackPoster {
    /// Post `text`, optionally rendered as Block Kit `blocks`.
    ///
    /// `blocks` is the BODY when present and `text` becomes the notification and accessibility
    /// fallback, because Slack renders blocks and demotes `text` to notification-only. Without
    /// this parameter the digest's block renderer was unreachable: it existed, was tested, and
    /// nothing called it, so the mrkdwn path shipped instead and its `  - ` prefixes rendered as
    /// literal hyphens.
    async fn post(
        &self,
        channel: &str,
        text: &str,
        blocks: Option<&serde_json::Value>,
    ) -> Result<()>;
}

/// Slack response envelope for `chat.postMessage`.
#[derive(Debug, Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Posts to Slack via `chat.postMessage` using a Bearer token.
pub struct HttpSlackPoster {
    token: String,
    http: HyperRustlsClient,
}

impl HttpSlackPoster {
    /// Build a poster, reading the token from the env var NAMED by `token_env`.
    /// Errors clearly if that variable is unset so the service fails visibly
    /// rather than silently posting nothing.
    pub fn from_env(token_env: &str) -> Result<Self> {
        debug!("HttpSlackPoster::from_env: token_env={}", token_env);

        let token = std::env::var(token_env).map_err(|_| {
            eyre!(
                "Slack token env var '{}' is not set; cannot post digest",
                token_env
            )
        })?;

        let http =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(crate::https_connector()?);

        Ok(Self { token, http })
    }
}

impl SlackPoster for HttpSlackPoster {
    async fn post(
        &self,
        channel: &str,
        text: &str,
        blocks: Option<&serde_json::Value>,
    ) -> Result<()> {
        debug!(
            "HttpSlackPoster::post: channel={}, text_len={}, blocks={}",
            channel,
            text.len(),
            blocks.is_some()
        );

        let mut payload = serde_json::json!({
            "channel": channel,
            "text": text,
        });
        if let Some(blocks) = blocks {
            payload["blocks"] = blocks.clone();
        }
        let body = serde_json::to_vec(&payload).context("Failed to serialize Slack payload")?;

        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(POST_MESSAGE_URL)
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(
                http::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .body(Full::new(Bytes::from(body)))
            .context("Failed to build Slack request")?;

        // One bound over BOTH awaits: the request and the body read are each a
        // network wait, and bounding only the first leaves the same hang one
        // step later.
        let (status, bytes) = tokio::time::timeout(REQUEST_TIMEOUT, async {
            let resp = self
                .http
                .request(req)
                .await
                .context("Slack chat.postMessage request failed")?;
            let status = resp.status();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .context("Failed to read Slack response body")?
                .to_bytes();
            Ok::<_, eyre::Report>((status, bytes))
        })
        .await
        .map_err(|_| {
            // The bound covers the body read as well as the request, so an
            // elapsed timeout does NOT mean the message failed to post. Say so:
            // nothing retries this in-process, but a human re-running `digest`
            // on a bare "returned nothing" would double-post.
            eyre!(
                "Slack chat.postMessage returned nothing within {}s; the message MAY \
already have posted, so check the channel before re-running",
                REQUEST_TIMEOUT.as_secs()
            )
        })??;

        if !status.is_success() {
            let preview = String::from_utf8_lossy(&bytes);
            warn!("Slack HTTP {} from chat.postMessage: {}", status, preview);
            eyre::bail!(
                "Slack chat.postMessage returned HTTP {}: {}",
                status,
                preview
            );
        }

        let parsed: SlackResponse =
            serde_json::from_slice(&bytes).context("Failed to parse Slack response JSON")?;

        if !parsed.ok {
            let err = parsed.error.unwrap_or_else(|| "unknown error".to_string());
            warn!("Slack chat.postMessage ok=false: {}", err);
            eyre::bail!("Slack chat.postMessage failed: {}", err);
        }

        debug!(
            "HttpSlackPoster::post: posted to {} ({} chars)",
            channel,
            text.len()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
