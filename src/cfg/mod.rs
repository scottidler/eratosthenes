pub mod account;
pub mod config;
pub mod filter;
pub mod label;
pub mod state;
pub mod triage;

/// Expand a leading `~/` against the user's home directory.
///
/// `~` is shell syntax, not a path the OS resolves: a config value like
/// `~/.local/bin/claude` handed straight to `Command::new` fails with
/// NotFound. Every config path that names a file on disk goes through here.
pub fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shellexpand_expands_leading_tilde() {
        let expanded = shellexpand("~/.local/bin/claude");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with("/.local/bin/claude"));
    }

    #[test]
    fn test_shellexpand_leaves_absolute_path_alone() {
        assert_eq!(shellexpand("/usr/bin/claude"), "/usr/bin/claude");
    }

    #[test]
    fn test_shellexpand_leaves_bare_name_alone() {
        assert_eq!(shellexpand("claude"), "claude");
    }
}
