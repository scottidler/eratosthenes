#![deny(clippy::unwrap_used)]
#![deny(dead_code)]
#![deny(unused_variables)]

pub mod cfg;
pub mod gmail;

use crate::cfg::config::{Config, load_config};
use eyre::{Context, Result};
use std::path::Path;

pub fn load(config_path: &Path) -> Result<Config> {
    load_config(config_path).context("Failed to load configuration")
}
