#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

use clap::{CommandFactory, FromArgMatches};
use eyre::Result;
use log::info;

mod cli;
mod logging;
mod service;

use cli::{AuthCommand, Cli, Command, ConfigCommand, ServiceCommand};
use eratosthenes::cfg::account::{Account, discovered_account_names, resolve_accounts};

const ENV_LOG_LEVEL: &str = "ERATOSTHENES_LOG_LEVEL";

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

/// Resolve log level from accounts - use the first account's level as a reasonable default.
fn log_level_from_accounts(cli_level: Option<&str>, accounts: &[Account]) -> String {
    let config_level = accounts.first().map(|a| a.config.log_level.as_str()).unwrap_or("info");
    resolve_log_level(cli_level, config_level)
}

/// Determine if we're running in multi-account mode (for output prefixing).
fn account_prefix(name: &str, multi: bool) -> String {
    if multi { format!("[{}] ", name) } else { String::new() }
}

async fn cmd_run(cli: &Cli, names: Vec<String>) -> Result<()> {
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let level = log_level_from_accounts(cli.log_level.as_deref(), &accounts);
    let account_names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    logging::setup(&level, &account_names)?;

    let multi = accounts.len() > 1;
    let mut join_set = tokio::task::JoinSet::new();

    for account in accounts {
        let dry_run = cli.dry_run;
        join_set.spawn(async move {
            let name = account.name;
            let config = account.config;
            logging::ACCOUNT.scope(name.clone(), async move {
                let prefix = account_prefix(&name, multi);
                info!("{}Starting account '{}'", prefix, &name);
                let result = eratosthenes::run(&name, &config, dry_run, multi).await;
                (name, result)
            }).await
        });
    }

    let mut errors: Vec<String> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((name, Ok(()))) => {
                let prefix = account_prefix(&name, multi);
                println!("{}Completed successfully", prefix);
            }
            Ok((name, Err(e))) => {
                let prefix = account_prefix(&name, multi);
                eprintln!("{}FAILED: {:#}", prefix, e);
                errors.push(format!("{}: {:#}", name, e));
            }
            Err(e) => {
                eprintln!("Task panicked: {:#}", e);
                errors.push(format!("task panic: {:#}", e));
            }
        }
    }

    if !errors.is_empty() {
        eyre::bail!("{} account(s) failed:\n  {}", errors.len(), errors.join("\n  "));
    }
    Ok(())
}

async fn cmd_auth_login(cli: &Cli, account: Option<String>) -> Result<()> {
    let names = account.as_ref().map(|a| vec![a.clone()]).unwrap_or_default();
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let level = log_level_from_accounts(cli.log_level.as_deref(), &accounts);
    let account_names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    logging::setup(&level, &account_names)?;

    if accounts.len() > 1 {
        let available: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
        eyre::bail!(
            "auth login requires a single account when multiple exist.\nAvailable accounts: {:?}",
            available
        );
    }

    let account = accounts
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("No accounts found"))?;
    let name = account.name;
    let config = account.config;
    logging::ACCOUNT.scope(name.clone(), async move {
        let auth = eratosthenes::gmail::auth::build_authenticator(&config.auth).await?;
        eratosthenes::gmail::auth::get_token(&auth).await?;
        println!("Login successful for '{}'", name);
        Ok(())
    }).await
}

async fn cmd_auth_logout(cli: &Cli, names: Vec<String>) -> Result<()> {
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let level = log_level_from_accounts(cli.log_level.as_deref(), &accounts);
    let account_names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    logging::setup(&level, &account_names)?;

    for account in accounts {
        let name = account.name;
        let config = account.config;
        logging::ACCOUNT.scope(name.clone(), async move {
            eratosthenes::gmail::auth::logout(&config.auth).await?;
            println!("Logged out '{}' (token cache cleared)", name);
            Ok::<(), eyre::Error>(())
        }).await?;
    }
    Ok(())
}

fn cmd_auth_status(cli: &Cli, names: Vec<String>) -> Result<()> {
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let multi = accounts.len() > 1;

    for account in &accounts {
        if multi {
            println!("=== {} ===", account.name);
        }
        service::auth_status(&account.name, &account.config.auth)?;
        if multi {
            println!();
        }
    }
    Ok(())
}

fn cmd_config_validate(cli: &Cli, names: Vec<String>) -> Result<()> {
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let multi = accounts.len() > 1;

    for account in &accounts {
        if multi {
            println!("=== {} ===", account.name);
        }
        service::config_validate(&account.name, &account.config)?;
        if multi {
            println!();
        }
    }
    Ok(())
}

fn cmd_config_show(cli: &Cli, names: Vec<String>) -> Result<()> {
    let accounts = resolve_accounts(cli.config.as_ref(), &names)?;
    let multi = accounts.len() > 1;

    for account in &accounts {
        if multi {
            println!("=== {} ===", account.name);
        }
        service::config_show(&account.name, &account.config)?;
        if multi {
            println!();
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let discovered = discovered_account_names();
    let account_help = if discovered.is_empty() {
        "Discovered accounts: (none)".to_string()
    } else {
        format!("Discovered accounts: {}", discovered.join(", "))
    };

    let cmd = Cli::command()
        .mut_subcommand("run", |cmd| cmd.after_help(&account_help))
        .mut_subcommand("auth", |cmd| {
            cmd.mut_subcommand("login", |cmd| cmd.after_help(&account_help))
                .mut_subcommand("logout", |cmd| cmd.after_help(&account_help))
                .mut_subcommand("status", |cmd| cmd.after_help(&account_help))
        })
        .mut_subcommand("config", |cmd| {
            cmd.mut_subcommand("validate", |cmd| cmd.after_help(&account_help))
                .mut_subcommand("show", |cmd| cmd.after_help(&account_help))
        });

    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).map_err(|e| eyre::eyre!(e))?;
    eratosthenes::init_tls()?;

    match &cli.command {
        None => cmd_run(&cli, Vec::new()).await,
        Some(Command::Run { accounts }) => cmd_run(&cli, accounts.clone()).await,
        Some(Command::Auth(opts)) => match &opts.command {
            AuthCommand::Login { account } => cmd_auth_login(&cli, account.clone()).await,
            AuthCommand::Logout { accounts } => cmd_auth_logout(&cli, accounts.clone()).await,
            AuthCommand::Status { accounts } => cmd_auth_status(&cli, accounts.clone()),
        },
        Some(Command::Service(opts)) => match &opts.command {
            ServiceCommand::Install { interval } => service::install(interval),
            ServiceCommand::Uninstall => service::uninstall(),
            ServiceCommand::Reinstall { interval } => service::reinstall(interval),
            ServiceCommand::Status => service::status(),
            ServiceCommand::Start => service::start(),
            ServiceCommand::Stop => service::stop(),
        },
        Some(Command::Config(opts)) => match &opts.command {
            ConfigCommand::Validate { accounts } => cmd_config_validate(&cli, accounts.clone()),
            ConfigCommand::Show { accounts } => cmd_config_show(&cli, accounts.clone()),
        },
    }
}
