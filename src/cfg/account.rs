use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use log::{debug, warn};

use crate::cfg::config::{Config, load_config};

/// An account is a named config - name derived from the config filename stem.
#[derive(Debug)]
pub struct Account {
    pub name: String,
    pub config: Config,
}

/// Discover all accounts by globbing *.yml in the config directory.
/// Each file is validated as an eratosthenes config (must have an `auth:` section).
/// Files that fail validation are skipped with a warning.
pub fn discover_accounts() -> Result<Vec<Account>> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| eyre::eyre!("Cannot determine XDG config directory"))?
        .join("eratosthenes");

    if !config_dir.exists() {
        return Ok(Vec::new());
    }

    let mut accounts = Vec::new();

    let entries =
        std::fs::read_dir(&config_dir).context(format!("Failed to read config directory: {}", config_dir.display()))?;

    for entry in entries {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        match load_config(&path) {
            Ok(config) => {
                debug!("Discovered account '{}' from {}", name, path.display());
                accounts.push(Account { name, config });
            }
            Err(e) => {
                warn!("Skipping {}: {}", path.display(), e);
            }
        }
    }

    // Sort by name for deterministic ordering
    accounts.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(accounts)
}

/// Wrap a single config file into a one-element account list.
/// Account name is derived from the filename stem.
pub fn account_from_config(config_path: &Path) -> Result<Vec<Account>> {
    let config = load_config(config_path).context("Failed to load configuration")?;
    let name = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default")
        .to_string();

    Ok(vec![Account { name, config }])
}

/// Validate that requested account names exist in the discovered set.
/// Returns filtered accounts matching the requested names, or all if names is empty.
pub fn filter_accounts(accounts: Vec<Account>, names: &[String]) -> Result<Vec<Account>> {
    if names.is_empty() {
        return Ok(accounts);
    }

    let available: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();

    for name in names {
        if !available.contains(&name.as_str()) {
            eyre::bail!("unknown account '{}', available accounts: {:?}", name, available);
        }
    }

    let requested: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    Ok(accounts
        .into_iter()
        .filter(|a| requested.contains(a.name.as_str()))
        .collect())
}

/// Validate startup constraints across all accounts.
/// Checks for duplicate callback ports.
pub fn validate_accounts(accounts: &[Account]) -> Result<()> {
    let mut seen_ports: Vec<(u16, &str)> = Vec::new();

    for account in accounts {
        let port = account.config.auth.callback_port;
        if let Some((_, other)) = seen_ports.iter().find(|(p, _)| *p == port) {
            eyre::bail!(
                "callback port {} is used by both '{}' and '{}' - each account must have a unique callback-port",
                port,
                other,
                account.name
            );
        }
        seen_ports.push((port, &account.name));
    }

    Ok(())
}

/// Return the list of discovered account names (for dynamic help text).
pub fn discovered_account_names() -> Vec<String> {
    discover_accounts()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name)
        .collect()
}

/// Resolve config path from CLI --config flag or default locations.
pub fn resolve_config_path(cli_path: Option<&PathBuf>) -> Option<PathBuf> {
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

/// Resolve accounts based on CLI flags.
/// If --config is provided, use that single file.
/// Otherwise, discover all accounts from the config directory.
pub fn resolve_accounts(cli_config: Option<&PathBuf>, names: &[String]) -> Result<Vec<Account>> {
    let accounts = if let Some(config_path) = cli_config {
        account_from_config(config_path)?
    } else {
        // Try discovery first
        let discovered = discover_accounts()?;
        if discovered.is_empty() {
            // Fall back to legacy single-file resolution
            if let Some(path) = resolve_config_path(None) {
                account_from_config(&path)?
            } else {
                eyre::bail!("No config files found. Create *.yml files in ~/.config/eratosthenes/ or provide --config");
            }
        } else {
            discovered
        }
    };

    validate_accounts(&accounts)?;
    filter_accounts(accounts, names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn write_valid_config(dir: &Path, name: &str, port: u16) {
        let content = format!(
            r#"
auth:
  creds-path: /tmp/creds
  callback-port: {}
"#,
            port
        );
        fs::write(dir.join(format!("{}.yml", name)), content).unwrap();
    }

    #[test]
    fn test_account_from_config() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);

        let accounts = account_from_config(&dir.path().join("work.yml")).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "work");
        assert_eq!(accounts[0].config.auth.callback_port, 13131);
    }

    #[test]
    fn test_filter_accounts_empty_names_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);
        write_valid_config(dir.path(), "home", 13132);

        let accounts = vec![
            account_from_config(&dir.path().join("work.yml")).unwrap().remove(0),
            account_from_config(&dir.path().join("home.yml")).unwrap().remove(0),
        ];

        let filtered = filter_accounts(accounts, &[]).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_accounts_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);
        write_valid_config(dir.path(), "home", 13132);

        let accounts = vec![
            account_from_config(&dir.path().join("work.yml")).unwrap().remove(0),
            account_from_config(&dir.path().join("home.yml")).unwrap().remove(0),
        ];

        let filtered = filter_accounts(accounts, &["work".to_string()]).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "work");
    }

    #[test]
    fn test_filter_accounts_unknown_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);

        let accounts = vec![account_from_config(&dir.path().join("work.yml")).unwrap().remove(0)];

        let result = filter_accounts(accounts, &["bogus".to_string()]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown account 'bogus'"));
        assert!(err.contains("work"));
    }

    #[test]
    fn test_validate_accounts_duplicate_ports() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);
        write_valid_config(dir.path(), "home", 13131);

        let accounts = vec![
            account_from_config(&dir.path().join("work.yml")).unwrap().remove(0),
            account_from_config(&dir.path().join("home.yml")).unwrap().remove(0),
        ];

        let result = validate_accounts(&accounts);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("callback port 13131"));
    }

    #[test]
    fn test_validate_accounts_unique_ports_ok() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_config(dir.path(), "work", 13131);
        write_valid_config(dir.path(), "home", 13132);

        let accounts = vec![
            account_from_config(&dir.path().join("work.yml")).unwrap().remove(0),
            account_from_config(&dir.path().join("home.yml")).unwrap().remove(0),
        ];

        assert!(validate_accounts(&accounts).is_ok());
    }
}
