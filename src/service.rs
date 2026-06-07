use eyre::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use eratosthenes::cfg::account::{Account, discover_accounts};
use eratosthenes::cfg::config::{AuthConfig, Config};

fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}

const SERVICE_NAME: &str = "eratosthenes";

fn service_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| eyre::eyre!("Cannot determine XDG config directory"))?
        .join("systemd")
        .join("user");
    Ok(dir)
}

fn service_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{SERVICE_NAME}.service")))
}

fn timer_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{SERVICE_NAME}.timer")))
}

const DIGEST_NAME: &str = "eratosthenes-digest";

fn digest_service_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{DIGEST_NAME}.service")))
}

fn digest_timer_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{DIGEST_NAME}.timer")))
}

/// Path to the EnvironmentFile holding the Slack token(s) for the digest service.
fn digest_env_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| eyre::eyre!("Cannot determine XDG config directory"))?
        .join("eratosthenes");
    Ok(dir.join("digest.env"))
}

fn cargo_bin_dir() -> String {
    dirs::home_dir()
        .map(|h| h.join(".cargo").join("bin").display().to_string())
        .unwrap_or_else(|| "/usr/local/bin".to_string())
}

fn validate_interval(interval: &str) -> Result<()> {
    let normalized = interval.trim().to_lowercase();

    let (value, unit) = if let Some(rest) = normalized.strip_suffix("min") {
        (rest, "min")
    } else if let Some(rest) = normalized.strip_suffix('h') {
        (rest, "h")
    } else if let Some(rest) = normalized.strip_suffix('s') {
        (rest, "s")
    } else {
        eyre::bail!(
            "Invalid interval '{}'. Use a systemd duration like 5min, 1h, 30s",
            interval
        );
    };

    let num: u64 = value
        .parse()
        .map_err(|_| eyre::eyre!("Invalid interval '{}': not a valid number", interval))?;

    let total_seconds = match unit {
        "min" => num * 60,
        "h" => num * 3600,
        "s" => num,
        _ => unreachable!(),
    };

    if total_seconds < 60 {
        eyre::bail!("Interval too short (minimum 1 minute): {}", interval);
    }
    if total_seconds > 86400 {
        eyre::bail!("Interval too long (maximum 24 hours): {}", interval);
    }

    Ok(())
}

fn generate_service(binary: &Path) -> String {
    format!(
        "\
[Unit]
Description=Eratosthenes Gmail Inbox Zero Engine

[Service]
Type=oneshot
ExecStart={binary} run
Environment=PATH={cargo_bin}:/usr/local/bin:/usr/bin:/bin
",
        binary = binary.display(),
        cargo_bin = cargo_bin_dir(),
    )
}

fn generate_timer(interval: &str) -> String {
    format!(
        "\
[Unit]
Description=Eratosthenes Periodic Timer

[Timer]
OnBootSec=2min
OnUnitActiveSec={interval}
Persistent=true

[Install]
WantedBy=timers.target
"
    )
}

fn generate_digest_service(binary: &Path) -> String {
    format!(
        "\
[Unit]
Description=Eratosthenes Slack Pinned-Inbox Digest

[Service]
Type=oneshot
ExecStart={binary} digest
Environment=PATH={cargo_bin}:/usr/local/bin:/usr/bin:/bin
EnvironmentFile=-%h/.config/eratosthenes/digest.env
",
        binary = binary.display(),
        cargo_bin = cargo_bin_dir(),
    )
}

fn generate_digest_timer(schedule: &str) -> String {
    format!(
        "\
[Unit]
Description=Eratosthenes Digest Timer

[Timer]
OnCalendar={schedule}
Persistent=true

[Install]
WantedBy=timers.target
"
    )
}

/// Validate a systemd `OnCalendar` schedule using `systemd-analyze calendar`.
fn validate_schedule(schedule: &str) -> Result<()> {
    let output = Command::new("systemd-analyze")
        .arg("calendar")
        .arg(schedule)
        .output()
        .context("Failed to run `systemd-analyze calendar` to validate the schedule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eyre::bail!("Invalid OnCalendar schedule '{}': {}", schedule, stderr.trim());
    }
    Ok(())
}

/// Resolve the single digest schedule from the slack-enabled accounts. The digest
/// is one timer running one `eratosthenes digest`; if accounts disagree, use the
/// first and warn.
fn resolve_digest_schedule(slack_accounts: &[&Account]) -> String {
    let first = slack_accounts
        .first()
        .and_then(|a| a.config.slack.as_ref())
        .map(|s| s.schedule.clone())
        .unwrap_or_default();

    for account in slack_accounts.iter().skip(1) {
        if let Some(slack) = account.config.slack.as_ref()
            && slack.schedule != first
        {
            eprintln!(
                "Warning: account '{}' requests digest schedule '{}' but the single digest timer uses '{}' (from the first slack-enabled account)",
                account.name, slack.schedule, first
            );
        }
    }
    first
}

/// Write the digest EnvironmentFile (mode 600) containing one line per DISTINCT
/// `token-env` name across slack-enabled accounts that is set in the current
/// environment. Warns for any referenced env var that is unset.
fn write_digest_env(slack_accounts: &[&Account]) -> Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for account in slack_accounts {
        if let Some(slack) = account.config.slack.as_ref()
            && !seen.contains(&slack.token_env)
        {
            seen.push(slack.token_env.clone());
        }
    }

    let mut lines = String::new();
    for name in &seen {
        match std::env::var(name) {
            Ok(value) => lines.push_str(&format!("{}={}\n", name, value)),
            Err(_) => eprintln!(
                "Warning: Slack token env var '{}' is not set; the digest service will fail until it is provided in {}",
                name,
                digest_env_path()?.display()
            ),
        }
    }

    let path = digest_env_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create config directory for digest.env")?;
    }
    std::fs::write(&path, lines).context("Failed to write digest.env")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .context("Failed to set 600 permissions on digest.env")?;

    println!("Wrote digest token env file (600): {}", path.display());
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("Failed to run systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eyre::bail!("systemctl --user {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

fn systemctl_ignore_errors(args: &[&str]) {
    let _ = Command::new("systemctl").arg("--user").args(args).output();
}

pub fn install(interval: &str) -> Result<()> {
    validate_interval(interval)?;

    let binary = std::env::current_exe().context("Failed to get current executable path")?;

    // Warn about non-standard binary paths
    let binary_str = binary.display().to_string();
    if binary_str.contains("target/debug") || binary_str.contains("target/release") {
        eprintln!("Warning: binary path contains target/ directory: {}", binary_str);
        eprintln!("  Consider running `cargo install --path .` first for a stable path.");
    }

    // Scan and validate all accounts
    let accounts = discover_accounts()?;
    if accounts.is_empty() {
        eprintln!("Warning: no account configs found in ~/.config/eratosthenes/");
        eprintln!("  Create *.yml config files before the timer fires.");
    } else {
        let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
        println!("Found accounts: {}", names.join(", "));

        for account in &accounts {
            let token_path_str = shellexpand(account.config.auth.token_cache_path().to_str().unwrap_or_default());
            if !Path::new(&token_path_str).exists() {
                eprintln!(
                    "Warning: no token cache for '{}'. Run `eratosthenes auth login {}` first.",
                    account.name, account.name
                );
            }
        }
    }

    let dir = service_dir()?;
    std::fs::create_dir_all(&dir).context("Failed to create systemd user directory")?;

    let svc = generate_service(&binary);
    let tmr = generate_timer(interval);

    let svc_path = service_path()?;
    let tmr_path = timer_path()?;

    std::fs::write(&svc_path, svc).context("Failed to write service file")?;
    std::fs::write(&tmr_path, tmr).context("Failed to write timer file")?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &format!("{SERVICE_NAME}.timer")])?;

    println!("Installed: {}", svc_path.display());
    println!("Installed: {}", tmr_path.display());
    println!("Timer enabled and started (interval: {})", interval);

    // The digest units are installed ONLY if at least one account opts in via a
    // `slack` block, so the timer never fires a no-op binary. The run units above
    // install unconditionally.
    install_digest_units(&binary, &accounts)?;

    println!("Hint: run `loginctl enable-linger $USER` for timer to run when not logged in");

    Ok(())
}

/// Install (or, if no account opts in, remove) the digest service + timer.
fn install_digest_units(binary: &Path, accounts: &[Account]) -> Result<()> {
    let slack_accounts: Vec<&Account> = accounts.iter().filter(|a| a.config.slack.is_some()).collect();

    if slack_accounts.is_empty() {
        remove_digest_units()?;
        println!("No slack-enabled accounts; digest timer not installed.");
        return Ok(());
    }

    let schedule = resolve_digest_schedule(&slack_accounts);
    validate_schedule(&schedule)?;
    write_digest_env(&slack_accounts)?;

    let svc_path = digest_service_path()?;
    let tmr_path = digest_timer_path()?;
    std::fs::write(&svc_path, generate_digest_service(binary)).context("Failed to write digest service file")?;
    std::fs::write(&tmr_path, generate_digest_timer(&schedule)).context("Failed to write digest timer file")?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &format!("{DIGEST_NAME}.timer")])?;

    println!("Installed: {}", svc_path.display());
    println!("Installed: {}", tmr_path.display());
    println!("Digest timer enabled and started (schedule: {})", schedule);
    Ok(())
}

/// Stop, disable, and remove the digest unit files. The digest.env (a
/// user-provided secret) is intentionally left in place.
fn remove_digest_units() -> Result<()> {
    systemctl_ignore_errors(&["stop", &format!("{DIGEST_NAME}.timer")]);
    systemctl_ignore_errors(&["disable", &format!("{DIGEST_NAME}.timer")]);

    let svc_path = digest_service_path()?;
    let tmr_path = digest_timer_path()?;

    let mut removed = false;
    if svc_path.exists() {
        std::fs::remove_file(&svc_path).context("Failed to remove digest service file")?;
        println!("Removed: {}", svc_path.display());
        removed = true;
    }
    if tmr_path.exists() {
        std::fs::remove_file(&tmr_path).context("Failed to remove digest timer file")?;
        println!("Removed: {}", tmr_path.display());
        removed = true;
    }
    if removed {
        systemctl(&["daemon-reload"])?;
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    systemctl_ignore_errors(&["stop", &format!("{SERVICE_NAME}.timer")]);
    systemctl_ignore_errors(&["disable", &format!("{SERVICE_NAME}.timer")]);

    let svc_path = service_path()?;
    let tmr_path = timer_path()?;

    let mut removed = false;
    if svc_path.exists() {
        std::fs::remove_file(&svc_path).context("Failed to remove service file")?;
        println!("Removed: {}", svc_path.display());
        removed = true;
    }
    if tmr_path.exists() {
        std::fs::remove_file(&tmr_path).context("Failed to remove timer file")?;
        println!("Removed: {}", tmr_path.display());
        removed = true;
    }

    // Tear down the digest units too (run + digest are managed together).
    remove_digest_units()?;

    if removed {
        systemctl(&["daemon-reload"])?;
        println!("Service uninstalled");
    } else {
        println!("Service not installed (nothing to remove)");
    }

    Ok(())
}

pub fn reinstall(interval: &str) -> Result<()> {
    // Suppress errors from uninstall (may not be installed)
    systemctl_ignore_errors(&["stop", &format!("{SERVICE_NAME}.timer")]);
    systemctl_ignore_errors(&["disable", &format!("{SERVICE_NAME}.timer")]);

    let svc_path = service_path()?;
    let tmr_path = timer_path()?;
    if svc_path.exists() {
        let _ = std::fs::remove_file(&svc_path);
    }
    if tmr_path.exists() {
        let _ = std::fs::remove_file(&tmr_path);
    }

    // Clear stale digest units too; install() re-lays them based on current config.
    systemctl_ignore_errors(&["stop", &format!("{DIGEST_NAME}.timer")]);
    systemctl_ignore_errors(&["disable", &format!("{DIGEST_NAME}.timer")]);
    if let Ok(p) = digest_service_path()
        && p.exists()
    {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(p) = digest_timer_path()
        && p.exists()
    {
        let _ = std::fs::remove_file(&p);
    }

    install(interval)
}

pub fn status() -> Result<()> {
    let svc_path = service_path()?;
    let tmr_path = timer_path()?;

    if !svc_path.exists() || !tmr_path.exists() {
        println!("Service not installed. Run: eratosthenes service install");
        return Ok(());
    }

    print_timer_status(&format!("{SERVICE_NAME}.timer"))?;

    // Show the digest timer too, when it is installed.
    if digest_service_path()?.exists() && digest_timer_path()?.exists() {
        println!();
        print_timer_status(&format!("{DIGEST_NAME}.timer"))?;
    }

    Ok(())
}

fn print_timer_status(unit: &str) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .arg("status")
        .arg(unit)
        .output()
        .context("Failed to run systemctl")?;

    // systemctl status exits non-zero if inactive, that's OK
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

pub fn start() -> Result<()> {
    systemctl(&["start", &format!("{SERVICE_NAME}.timer")])
}

pub fn stop() -> Result<()> {
    systemctl(&["stop", &format!("{SERVICE_NAME}.timer")])
}

pub fn auth_status(account_name: &str, auth: &AuthConfig) -> Result<()> {
    let token_path_str = shellexpand(auth.token_cache_path().to_str().unwrap_or_default());
    let token_path = Path::new(&token_path_str);

    println!("Account: {}", account_name);
    println!("Token cache: {}", token_path.display());

    if !token_path.exists() {
        println!("Status: NOT AUTHENTICATED");
        println!("  No token cache found. Run: eratosthenes auth login {}", account_name);
        return Ok(());
    }

    let content = std::fs::read_to_string(token_path).context("Failed to read token cache")?;

    // yup-oauth2 token cache is JSON; check if it parses and has content
    let parsed: serde_json::Value = serde_json::from_str(&content).context("Token cache is not valid JSON")?;

    if parsed.as_object().is_some_and(|obj| obj.is_empty()) {
        println!("Status: EMPTY (no tokens cached)");
        println!("  Run: eratosthenes auth login {}", account_name);
        return Ok(());
    }

    println!("Status: AUTHENTICATED");

    // Try to extract expiry info from the cached tokens
    if let Some(obj) = parsed.as_object() {
        for (scope, token_data) in obj {
            if let Some(expiry) = token_data.get("expiry_date") {
                println!("  Scope: {}", scope);
                println!("  Expiry: {}", expiry);
            }
        }
    }

    Ok(())
}

pub fn config_validate(account_name: &str, config: &Config) -> Result<()> {
    println!("Account '{}' config is valid.", account_name);
    println!();
    println!("Message filters: {} defined", config.message_filters.len());
    for filter in &config.message_filters {
        println!("  - {}", filter.name);
    }
    println!();
    println!("State filters: {} defined", config.state_filters.len());
    for filter in &config.state_filters {
        println!("  - {}", filter.name);
    }
    println!();
    println!("Log level: {}", config.log_level);

    Ok(())
}

pub fn config_show(account_name: &str, config: &Config) -> Result<()> {
    println!("Account: {}", account_name);
    println!("Creds path: {}", config.auth.creds_path.display());
    println!("Client secret: {}", config.auth.client_secret_path().display());
    println!("Token cache: {}", config.auth.token_cache_path().display());
    println!("Callback port: {}", config.auth.callback_port);
    println!("Log level: {}", config.log_level);
    println!();
    println!("Message filters: {}", config.message_filters.len());
    for filter in &config.message_filters {
        println!("  - {}", filter.name);
    }
    println!();
    println!("State filters: {}", config.state_filters.len());
    for filter in &config.state_filters {
        println!("  - {}", filter.name);
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_service() {
        let binary = PathBuf::from("/home/user/.cargo/bin/eratosthenes");
        let output = generate_service(&binary);

        assert!(output.contains("Type=oneshot"));
        assert!(output.contains("ExecStart=/home/user/.cargo/bin/eratosthenes run"));
        assert!(!output.contains("--config"));
        assert!(output.contains("Environment=PATH="));
        assert!(output.contains("Description=Eratosthenes Gmail Inbox Zero Engine"));
    }

    #[test]
    fn test_generate_timer() {
        let output = generate_timer("5min");

        assert!(output.contains("OnUnitActiveSec=5min"));
        assert!(output.contains("OnBootSec=2min"));
        assert!(output.contains("Persistent=true"));
        assert!(output.contains("WantedBy=timers.target"));
    }

    #[test]
    fn test_generate_digest_service() {
        let binary = PathBuf::from("/home/user/.cargo/bin/eratosthenes");
        let output = generate_digest_service(&binary);

        assert!(output.contains("Type=oneshot"));
        assert!(output.contains("ExecStart=/home/user/.cargo/bin/eratosthenes digest"));
        assert!(output.contains("EnvironmentFile=-%h/.config/eratosthenes/digest.env"));
        assert!(output.contains("Description=Eratosthenes Slack Pinned-Inbox Digest"));
    }

    #[test]
    fn test_generate_digest_timer() {
        let output = generate_digest_timer("Mon-Fri 08,13:00:00");

        assert!(output.contains("OnCalendar=Mon-Fri 08,13:00:00"));
        assert!(output.contains("Persistent=true"));
        assert!(output.contains("WantedBy=timers.target"));
        // The digest is a fixed-schedule timer, not an interval timer.
        assert!(!output.contains("OnUnitActiveSec"));
    }

    #[test]
    fn test_generate_timer_custom_interval() {
        let output = generate_timer("10min");
        assert!(output.contains("OnUnitActiveSec=10min"));

        let output = generate_timer("1h");
        assert!(output.contains("OnUnitActiveSec=1h"));
    }

    #[test]
    fn test_validate_interval_valid() {
        assert!(validate_interval("1min").is_ok());
        assert!(validate_interval("5min").is_ok());
        assert!(validate_interval("30min").is_ok());
        assert!(validate_interval("1h").is_ok());
        assert!(validate_interval("24h").is_ok());
        assert!(validate_interval("60s").is_ok());
    }

    #[test]
    fn test_validate_interval_too_short() {
        assert!(validate_interval("30s").is_err());
        assert!(validate_interval("0min").is_err());
    }

    #[test]
    fn test_validate_interval_too_long() {
        assert!(validate_interval("25h").is_err());
    }

    #[test]
    fn test_validate_interval_invalid_format() {
        assert!(validate_interval("abc").is_err());
        assert!(validate_interval("5x").is_err());
        assert!(validate_interval("").is_err());
    }

    #[test]
    fn test_service_file_paths() {
        // Just verify these don't panic
        let svc = service_path();
        let tmr = timer_path();
        assert!(svc.is_ok());
        assert!(tmr.is_ok());

        let svc = svc.unwrap();
        let tmr = tmr.unwrap();
        assert!(svc.to_string_lossy().contains("eratosthenes.service"));
        assert!(tmr.to_string_lossy().contains("eratosthenes.timer"));
    }

    #[test]
    fn test_shellexpand_tilde() {
        let expanded = shellexpand("~/some/path");
        assert!(!expanded.starts_with("~/"));
        assert!(expanded.ends_with("/some/path"));
    }

    #[test]
    fn test_shellexpand_no_tilde() {
        let expanded = shellexpand("/absolute/path");
        assert_eq!(expanded, "/absolute/path");
    }
}
