use eyre::Result;
use std::path::Path;

use eratosthenes::cfg::config::{AuthConfig, Config};

pub fn install(config_path: &Path, interval: &str) -> Result<()> {
    let _ = (config_path, interval);
    eyre::bail!("service install is not yet implemented")
}

pub fn uninstall() -> Result<()> {
    eyre::bail!("service uninstall is not yet implemented")
}

pub fn reinstall(config_path: &Path, interval: &str) -> Result<()> {
    let _ = (config_path, interval);
    eyre::bail!("service reinstall is not yet implemented")
}

pub fn status() -> Result<()> {
    eyre::bail!("service status is not yet implemented")
}

pub fn start() -> Result<()> {
    eyre::bail!("service start is not yet implemented")
}

pub fn stop() -> Result<()> {
    eyre::bail!("service stop is not yet implemented")
}

pub fn auth_status(auth: &AuthConfig) -> Result<()> {
    let _ = auth;
    eyre::bail!("auth status is not yet implemented")
}

pub fn config_validate(config: &Config) -> Result<()> {
    let _ = config;
    eyre::bail!("config validate is not yet implemented")
}

pub fn config_show(config_path: &Path) -> Result<()> {
    let _ = config_path;
    eyre::bail!("config show is not yet implemented")
}
