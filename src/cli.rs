use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "eratosthenes",
    about = "Gmail API-native inbox zero engine",
    version = env!("GIT_DESCRIBE"),
    after_help = "Logs are written to: ~/.local/share/eratosthenes/logs/eratosthenes.log\n\nRequires: Google Cloud OAuth2 client credentials (client-secret.json)"
)]
pub struct Cli {
    /// Path to config file
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// Dry run - show what would be done without making changes
    #[arg(long)]
    pub dry_run: bool,

    /// Force re-authentication (clear token cache, open browser)
    #[arg(long)]
    pub login: bool,

    /// Clear cached OAuth2 tokens
    #[arg(long)]
    pub logout: bool,
}
