use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "eratosthenes",
    about = "Gmail API-native inbox zero engine",
    version = env!("GIT_DESCRIBE"),
    after_help = "\
REQUIRED CREDENTIALS:
  Google Cloud OAuth2 client secret (Desktop app type)
  Default: ~/.config/eratosthenes/<account>/client-secret.json

Logs are written to: ~/.local/share/eratosthenes/logs/<account>.log"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to config file (bypass account discovery)
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, global = true)]
    pub log_level: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the inbox zero engine (default when no subcommand given)
    Run {
        /// Account(s) to run (default: all discovered)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,

        /// Dry run - no message or thread changes; missing labels may be created
        ///
        /// Lives on `run`, not on the top-level command: `run` is the only subcommand
        /// that honors it, and a top-level flag would be ACCEPTED and silently ignored
        /// by `digest` (which posts to Slack) and `service install` (which writes unit
        /// files and calls systemctl).
        #[arg(long)]
        dry_run: bool,

        /// Marker backfill mode: stamp the marker label on every message a normal run
        /// would HANDLE, applying no Star/Flag/Move. One-shot rollout tool, not a
        /// permanent verb: reuses account discovery and the message-filter matching
        /// path, so it lives here rather than as its own subcommand. Logs id, date,
        /// from, subject for every message it stamps, since the stamp is irreversible
        /// in effect and that log is the only way to undo a wrong freeze by hand.
        #[arg(long)]
        mark_only: bool,
    },

    /// Post the pinned-inbox (Starred + Important) digest to Slack
    Digest {
        /// Account(s) to post a digest for (default: all discovered)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },

    /// Manage OAuth2 authentication
    Auth(AuthOpts),

    /// Manage systemd timer service
    Service(ServiceOpts),

    /// Config utilities
    Config(ConfigOpts),
}

#[derive(Args)]
pub struct AuthOpts {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Force re-authentication (clear token cache, open browser)
    Login {
        /// Account to login (required when multiple exist)
        account: Option<String>,
    },
    /// Clear cached OAuth2 tokens
    Logout {
        /// Account(s) to logout (default: all)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
    /// Show current authentication status
    Status {
        /// Account(s) to show status for (default: all)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
}

#[derive(Args)]
pub struct ServiceOpts {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Install systemd user timer and service
    Install {
        /// Timer interval (default: 5min)
        #[arg(long, default_value = "5min")]
        interval: String,
    },
    /// Remove systemd user timer and service
    Uninstall,
    /// Reinstall (uninstall then install)
    Reinstall {
        /// Timer interval (default: 5min)
        #[arg(long, default_value = "5min")]
        interval: String,
    },
    /// Show service and timer status
    Status,
    /// Start the timer
    Start,
    /// Stop the timer
    Stop,
}

#[derive(Args)]
pub struct ConfigOpts {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Validate config file and show resolved filters
    Validate {
        /// Account(s) to validate (default: all)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
    /// Show resolved config path
    Show {
        /// Account(s) to show (default: all)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
}
