#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

use clap::Parser;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod cli;

use cli::Cli;

fn setup_logging(level: &str) -> Result<()> {
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eratosthenes")
        .join("logs");

    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    let log_file = log_dir.join("eratosthenes.log");

    let target = Box::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .context("Failed to open log file")?,
    );

    env_logger::Builder::new()
        .parse_filters(level)
        .target(env_logger::Target::Pipe(target))
        .init();

    info!("Logging initialized, writing to: {}", log_file.display());
    Ok(())
}

fn resolve_config_path(cli_path: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(path) = cli_path {
        return Some(path.clone());
    }

    if let Some(config_dir) = dirs::config_dir() {
        let primary = config_dir.join("eratosthenes").join("eratosthenes.yml");
        if primary.exists() {
            return Some(primary);
        }
    }

    let fallback = PathBuf::from("eratosthenes.yml");
    if fallback.exists() {
        return Some(fallback);
    }

    None
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    setup_logging(&cli.log_level).context("Failed to setup logging")?;

    if cli.logout {
        info!("Logout requested - token cache clearing will be implemented in Phase 2");
        println!("Logged out (token cache cleared)");
        return Ok(());
    }

    let config_path = resolve_config_path(cli.config.as_ref()).ok_or_else(|| {
        eyre::eyre!("No config file found. Provide --config or create ~/.config/eratosthenes/eratosthenes.yml")
    })?;

    info!("Loading config from: {}", config_path.display());
    let config = eratosthenes::load(&config_path)?;

    info!(
        "Loaded {} message filters, {} state filters",
        config.message_filters.len(),
        config.state_filters.len()
    );

    if cli.dry_run {
        println!("Dry run mode - no changes will be made");
    }

    if cli.login {
        info!("Login requested - OAuth2 flow will be implemented in Phase 2");
        println!("Login flow will be implemented in Phase 2");
        return Ok(());
    }

    info!("Engine execution will be implemented in Phase 3/4");
    println!(
        "Config loaded: {} message filters, {} state filters",
        config.message_filters.len(),
        config.state_filters.len()
    );

    Ok(())
}
