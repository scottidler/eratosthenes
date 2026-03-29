use eyre::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn generate_service(binary: &Path, config_path: &Path) -> String {
    format!(
        "\
[Unit]
Description=Eratosthenes Gmail Inbox Zero Engine

[Service]
Type=oneshot
ExecStart={binary} run --config {config}
Environment=PATH={cargo_bin}:/usr/local/bin:/usr/bin:/bin
",
        binary = binary.display(),
        config = config_path.display(),
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

pub fn install(config_path: &Path, interval: &str) -> Result<()> {
    validate_interval(interval)?;

    let binary = std::env::current_exe().context("Failed to get current executable path")?;
    let config_path = config_path.canonicalize().context("Failed to resolve config path")?;

    // Warn about non-standard binary paths
    let binary_str = binary.display().to_string();
    if binary_str.contains("target/debug") || binary_str.contains("target/release") {
        eprintln!("Warning: binary path contains target/ directory: {}", binary_str);
        eprintln!("  Consider running `cargo install --path .` first for a stable path.");
    }

    // Check if token cache exists (warn, don't block)
    if let Ok(config) = eratosthenes::load(&config_path) {
        let token_path_str = shellexpand(config.auth.token_cache_path.to_str().unwrap_or_default());
        if !Path::new(&token_path_str).exists() {
            eprintln!("Warning: no token cache found. Run `eratosthenes auth login` first.");
            eprintln!("  The browser OAuth flow cannot work inside a systemd timer context.");
        }
    }

    let dir = service_dir()?;
    std::fs::create_dir_all(&dir).context("Failed to create systemd user directory")?;

    let svc = generate_service(&binary, &config_path);
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
    println!("Hint: run `loginctl enable-linger $USER` for timer to run when not logged in");

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

    if removed {
        systemctl(&["daemon-reload"])?;
        println!("Service uninstalled");
    } else {
        println!("Service not installed (nothing to remove)");
    }

    Ok(())
}

pub fn reinstall(config_path: &Path, interval: &str) -> Result<()> {
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

    install(config_path, interval)
}

pub fn status() -> Result<()> {
    let svc_path = service_path()?;
    let tmr_path = timer_path()?;

    if !svc_path.exists() || !tmr_path.exists() {
        println!("Service not installed. Run: eratosthenes service install");
        return Ok(());
    }

    let output = Command::new("systemctl")
        .arg("--user")
        .arg("status")
        .arg(format!("{SERVICE_NAME}.timer"))
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

pub fn auth_status(auth: &AuthConfig) -> Result<()> {
    let token_path_str = shellexpand(auth.token_cache_path.to_str().unwrap_or_default());
    let token_path = Path::new(&token_path_str);

    println!("Token cache: {}", token_path.display());

    if !token_path.exists() {
        println!("Status: NOT AUTHENTICATED");
        println!("  No token cache found. Run: eratosthenes auth login");
        return Ok(());
    }

    let content = std::fs::read_to_string(token_path).context("Failed to read token cache")?;

    // yup-oauth2 token cache is JSON; check if it parses and has content
    let parsed: serde_json::Value = serde_json::from_str(&content).context("Token cache is not valid JSON")?;

    if parsed.as_object().is_some_and(|obj| obj.is_empty()) {
        println!("Status: EMPTY (no tokens cached)");
        println!("  Run: eratosthenes auth login");
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

pub fn config_validate(config: &Config) -> Result<()> {
    println!("Config is valid.");
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

pub fn config_show(config_path: &Path) -> Result<()> {
    let canonical = config_path.canonicalize().unwrap_or_else(|_| config_path.to_path_buf());
    println!("Config path: {}", canonical.display());
    println!();

    let content = std::fs::read_to_string(config_path)
        .context(format!("Failed to read config file: {}", config_path.display()))?;
    print!("{}", content);

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_service() {
        let binary = PathBuf::from("/home/user/.cargo/bin/eratosthenes");
        let config = PathBuf::from("/home/user/.config/eratosthenes/eratosthenes.yml");
        let output = generate_service(&binary, &config);

        assert!(output.contains("Type=oneshot"));
        assert!(output.contains(
            "ExecStart=/home/user/.cargo/bin/eratosthenes run --config /home/user/.config/eratosthenes/eratosthenes.yml"
        ));
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

    #[test]
    fn test_config_show_missing_file() {
        let result = config_show(Path::new("/nonexistent/config.yml"));
        assert!(result.is_err());
    }
}
