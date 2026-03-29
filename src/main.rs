#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

use clap::Parser;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod cli;
mod service;

use cli::{AuthCommand, Cli, Command, ConfigCommand, ServiceCommand};

const ENV_LOG_LEVEL: &str = "ERATOSTHENES_LOG_LEVEL";

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

    // Filter noisy third-party crates to warn, apply user level to eratosthenes
    let filter = format!("warn,eratosthenes={}", level);

    env_logger::Builder::new()
        .parse_filters(&filter)
        .target(env_logger::Target::Pipe(target))
        .init();

    info!("------------------------------------------------------------");
    info!(
        "eratosthenes run started at {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    info!("------------------------------------------------------------");
    info!("Log level: {}, writing to: {}", level, log_file.display());
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

fn require_config_path(cli: &Cli) -> Result<PathBuf> {
    resolve_config_path(cli.config.as_ref()).ok_or_else(|| {
        eyre::eyre!("No config file found. Provide --config or create ~/.config/eratosthenes/eratosthenes.yml")
    })
}

fn load_config_or_exit(cli: &Cli) -> Result<eratosthenes::cfg::config::Config> {
    let config_path = require_config_path(cli)?;
    let config = eratosthenes::load(&config_path)?;
    info!("Config loaded from: {}", config_path.display());
    Ok(config)
}

/// Resolve log level with precedence: CLI flag > env var > config file > default ("info")
fn resolve_log_level(cli_level: Option<&str>, config_level: &str) -> String {
    if let Some(level) = cli_level {
        return level.to_string();
    }
    if let Ok(level) = std::env::var(ENV_LOG_LEVEL) {
        return level;
    }
    config_level.to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    eratosthenes::init_tls()?;

    match &cli.command {
        None | Some(Command::Run) => {
            let config = load_config_or_exit(&cli)?;
            setup_logging(&resolve_log_level(cli.log_level.as_deref(), &config.log_level))?;
            info!(
                "Loaded {} message filters, {} state filters",
                config.message_filters.len(),
                config.state_filters.len()
            );
            eratosthenes::run(&config, cli.dry_run).await
        }
        Some(Command::Auth(opts)) => {
            let config = load_config_or_exit(&cli)?;
            setup_logging(&resolve_log_level(cli.log_level.as_deref(), &config.log_level))?;
            match &opts.command {
                AuthCommand::Login => {
                    let auth = eratosthenes::gmail::auth::build_authenticator(&config.auth).await?;
                    eratosthenes::gmail::auth::get_token(&auth).await?;
                    println!("Login successful");
                    Ok(())
                }
                AuthCommand::Logout => {
                    eratosthenes::gmail::auth::logout(&config.auth).await?;
                    println!("Logged out (token cache cleared)");
                    Ok(())
                }
                AuthCommand::Status => service::auth_status(&config.auth),
            }
        }
        Some(Command::Service(opts)) => match &opts.command {
            ServiceCommand::Install { interval } => {
                let config_path = require_config_path(&cli)?;
                service::install(&config_path, interval)
            }
            ServiceCommand::Uninstall => service::uninstall(),
            ServiceCommand::Reinstall { interval } => {
                let config_path = require_config_path(&cli)?;
                service::reinstall(&config_path, interval)
            }
            ServiceCommand::Status => service::status(),
            ServiceCommand::Start => service::start(),
            ServiceCommand::Stop => service::stop(),
        },
        Some(Command::Config(opts)) => match &opts.command {
            ConfigCommand::Validate => {
                let config = load_config_or_exit(&cli)?;
                service::config_validate(&config)
            }
            ConfigCommand::Show => {
                let config_path = require_config_path(&cli)?;
                service::config_show(&config_path)
            }
        },
    }
}
