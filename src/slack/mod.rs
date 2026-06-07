use bytes::Bytes;
use eyre::{Context, Result, eyre};
use http_body_util::{BodyExt, Full};
use log::{debug, warn};
use serde::Deserialize;

const POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";

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
    async fn post(&self, channel: &str, text: &str) -> Result<()>;
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

        let token = std::env::var(token_env)
            .map_err(|_| eyre!("Slack token env var '{}' is not set; cannot post digest", token_env))?;

        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("Failed to load native TLS roots for Slack client")?
            .https_or_http()
            .enable_http1()
            .build();

        let http = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

        Ok(Self { token, http })
    }
}

impl SlackPoster for HttpSlackPoster {
    async fn post(&self, channel: &str, text: &str) -> Result<()> {
        debug!("HttpSlackPoster::post: channel={}, text_len={}", channel, text.len());

        let payload = serde_json::json!({
            "channel": channel,
            "text": text,
        });
        let body = serde_json::to_vec(&payload).context("Failed to serialize Slack payload")?;

        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(POST_MESSAGE_URL)
            .header(http::header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(http::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(body)))
            .context("Failed to build Slack request")?;

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

        if !status.is_success() {
            let preview = String::from_utf8_lossy(&bytes);
            warn!("Slack HTTP {} from chat.postMessage: {}", status, preview);
            eyre::bail!("Slack chat.postMessage returned HTTP {}: {}", status, preview);
        }

        let parsed: SlackResponse = serde_json::from_slice(&bytes).context("Failed to parse Slack response JSON")?;

        if !parsed.ok {
            let err = parsed.error.unwrap_or_else(|| "unknown error".to_string());
            warn!("Slack chat.postMessage ok=false: {}", err);
            eyre::bail!("Slack chat.postMessage failed: {}", err);
        }

        debug!("HttpSlackPoster::post: posted to {} ({} chars)", channel, text.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
