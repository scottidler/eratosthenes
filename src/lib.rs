#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

pub mod cfg;
pub mod engine;
pub mod gmail;

use crate::cfg::config::{Config, load_config};
use eyre::{Context, Result};
use std::path::Path;

pub fn load(config_path: &Path) -> Result<Config> {
    load_config(config_path).context("Failed to load configuration")
}

pub async fn run(config: &Config, dry_run: bool) -> Result<()> {
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

    let mut client = gmail::client::GmailClient::new(hub)
        .await
        .context("Failed to initialize Gmail client")?;

    engine::execute(&mut client, config, dry_run).await
}
