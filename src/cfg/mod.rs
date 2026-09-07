pub mod account;
pub mod config;
pub mod filter;
pub mod label;
pub mod state;
pub mod triage;

use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

/// Expands a leading `~` (bare, or followed by `/...`) to the current user's
/// home directory. Any other path, including `~user` (someone else's home),
/// passes through unchanged: eratosthenes never resolves another user's home,
/// so supporting it would mean carrying a `/etc/passwd` lookup crate for a
/// case that never fires. A path that can't be expanded (no home directory
/// found) also passes through unchanged, deferring the failure to whatever
/// tries to use the path next.
///
/// Shape copied from otto (`otto/src/executor/layout.rs`), which is the
/// in-house home-rolled version. `strip_prefix` works on path COMPONENTS, so
/// `~otheruser` is correctly rejected where a naive `strip_prefix("~/")`
/// string match would also miss a bare `~`.
pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    match path.strip_prefix("~") {
        Ok(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

/// `#[serde(deserialize_with = "crate::cfg::deserialize_tilde_pathbuf")]` for
/// `PathBuf` config fields. Runs the deserialized value through
/// [`expand_tilde`] so a literal `~/...` in YAML becomes a real absolute path
/// the moment the config loads.
///
/// Every `PathBuf` that originates in user config goes through this. `~` is
/// shell syntax, not something the OS resolves: handed to `Command::new` it
/// fails NotFound, and handed to a filesystem call it creates a directory
/// literally named `~` in the process CWD.
pub fn deserialize_tilde_pathbuf<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = PathBuf::deserialize(deserializer)?;
    Ok(expand_tilde(raw))
}

/// [`deserialize_tilde_pathbuf`] for `Option<PathBuf>` fields.
pub fn deserialize_tilde_pathbuf_opt<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<PathBuf>::deserialize(deserializer)?;
    Ok(raw.map(expand_tilde))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde_expands_bare_and_prefixed_forms() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(
            expand_tilde("~/.local/bin/claude"),
            home.join(".local/bin/claude")
        );
    }

    #[test]
    fn test_expand_tilde_leaves_non_tilde_and_other_user_paths_alone() {
        assert_eq!(
            expand_tilde("/usr/bin/claude"),
            PathBuf::from("/usr/bin/claude")
        );
        assert_eq!(expand_tilde("claude"), PathBuf::from("claude"));
        // `~otheruser` is not a bare `~` component, so strip_prefix rejects it.
        assert_eq!(
            expand_tilde("~otheruser/bin/claude"),
            PathBuf::from("~otheruser/bin/claude")
        );
    }
}
