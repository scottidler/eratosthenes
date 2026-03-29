use eyre::{Context, Result};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod, read_application_secret};

use crate::cfg::config::AuthConfig;

const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

pub async fn build_authenticator(
    config: &AuthConfig,
) -> Result<
    yup_oauth2::authenticator::Authenticator<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    >,
> {
    let secret_path = shellexpand(config.client_secret_path.to_str().unwrap_or_default());
    let token_path = shellexpand(config.token_cache_path.to_str().unwrap_or_default());

    let secret = read_application_secret(&secret_path)
        .await
        .context(format!("Failed to read client secret from {}", secret_path))?;

    let auth = InstalledFlowAuthenticator::builder(
        secret,
        InstalledFlowReturnMethod::HTTPPortRedirect(config.callback_port),
    )
    .persist_tokens_to_disk(&token_path)
    .build()
    .await
    .context("Failed to build OAuth2 authenticator")?;

    Ok(auth)
}

pub async fn get_token(
    auth: &yup_oauth2::authenticator::Authenticator<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    >,
) -> Result<String> {
    let token = auth
        .token(&[GMAIL_MODIFY_SCOPE])
        .await
        .context("Failed to obtain OAuth2 token")?;

    token
        .token()
        .map(|t| t.to_string())
        .ok_or_else(|| eyre::eyre!("OAuth2 token response contained no access token"))
}

pub async fn logout(config: &AuthConfig) -> Result<()> {
    let token_path = shellexpand(config.token_cache_path.to_str().unwrap_or_default());
    if std::path::Path::new(&token_path).exists() {
        std::fs::remove_file(&token_path).context("Failed to remove token cache")?;
        log::info!("Token cache removed: {}", token_path);
    }
    Ok(())
}

fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}
