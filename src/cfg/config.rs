use eyre::{Result, eyre};
use log::{debug, error};
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml::{Value, from_value};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cfg::filter::{FilterAction, MessageFilter};
use crate::cfg::state::{StateAction, StateFilter};

/// XDG config dir, honoring `$XDG_CONFIG_HOME` and falling back to `$HOME/.config`.
pub fn xdg_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

/// XDG data dir, honoring `$XDG_DATA_HOME` and falling back to `$HOME/.local/share`.
///
/// We deliberately do NOT use the `dirs` config/data helpers: those honor
/// `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` only on Linux. On macOS they resolve via system
/// APIs and return `~/Library/...`, ignoring the env vars. These helpers resolve to the
/// same XDG layout on every platform.
pub fn xdg_data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::home_dir().map(|h| h.join(".local").join("share"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AuthConfig {
    pub creds_path: PathBuf,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
}

impl AuthConfig {
    pub fn client_secret_path(&self) -> PathBuf {
        self.creds_path.join("client-secret.json")
    }

    pub fn token_cache_path(&self) -> PathBuf {
        self.creds_path.join("tokencache.json")
    }
}

fn default_callback_port() -> u16 {
    13131
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SlackConfig {
    /// Env var NAME holding the Slack token, never the token itself.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// Self-DM Dxxxx (user token) or Uxxxx/Cxxxx destination.
    pub channel: String,
    /// Gmail multi-login slot for deep links (/u/N), default 0.
    #[serde(default)]
    pub browser_index: u8,
    /// systemd OnCalendar string that drives the digest timer. Required, like
    /// `channel`: a digest with no cadence is meaningless, and a silent default
    /// would clobber the live timer on every `service` regenerate.
    pub schedule: String,
}

fn default_token_env() -> String {
    "SLACK_XOXP_TOKEN".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub auth: AuthConfig,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    #[serde(
        rename = "message-filters",
        default,
        deserialize_with = "deserialize_named_filters"
    )]
    pub message_filters: Vec<MessageFilter>,

    #[serde(
        rename = "state-filters",
        default,
        deserialize_with = "deserialize_named_states"
    )]
    pub state_filters: Vec<StateFilter>,

    /// Optional per-account Slack digest config. `digest` is a no-op if absent.
    #[serde(default)]
    pub slack: Option<SlackConfig>,

    /// Label recording that a message-filter HANDLED a message. Every message-filter
    /// excludes it, so a handled message is never acted on again. Defaulted, so no
    /// config change is required to adopt it.
    #[serde(default = "default_marker_label")]
    pub marker_label: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_marker_label() -> String {
    "Triaged".to_string()
}

impl Config {
    /// Reject configs whose marker or action order the engine cannot honor. Both checks
    /// fail at LOAD, not at runtime: each one is silent and destructive if it slips
    /// through to a live mailbox.
    fn validate(&self) -> Result<()> {
        self.validate_marker_label()?;
        self.validate_move_position()?;
        Ok(())
    }

    /// The marker must name no state-filter label and no state-filter destination.
    /// `derive_stages` harvests state Move destinations and `ensure_labels` harvests
    /// custom names out of state filters' `labels` blocks, so a colliding marker would
    /// reach `sanitize_stages` and be stripped off the very messages it marks.
    fn validate_marker_label(&self) -> Result<()> {
        if self.marker_label.trim().is_empty() {
            return Err(eyre!("marker-label must not be empty"));
        }

        for state in &self.state_filters {
            for label in &state.labels {
                if label.to_gmail_id().eq_ignore_ascii_case(&self.marker_label) {
                    return Err(eyre!(
                        "marker-label '{}' collides with state-filter '{}' label '{}': \
                         the marker must name no state-filter label and no state-filter \
                         destination, or stage sanitization strips it",
                        self.marker_label,
                        state.name,
                        label.to_gmail_id()
                    ));
                }
            }
            if let StateAction::Move(dest) = &state.action
                && dest.eq_ignore_ascii_case(&self.marker_label)
            {
                return Err(eyre!(
                    "marker-label '{}' collides with state-filter '{}' destination '{}': \
                     the marker must name no state-filter label and no state-filter \
                     destination, or stage sanitization strips it",
                    self.marker_label,
                    state.name,
                    dest
                ));
            }
        }

        Ok(())
    }

    /// The marker rides the filter's LAST write, and a `Move` is the only action that
    /// destroys future eligibility (it strips INBOX and UNREAD), so the marker must fold
    /// into the `Move` write itself. That is knowable from `filter.actions` alone only
    /// while every filter has at most one `Move`, in final position.
    fn validate_move_position(&self) -> Result<()> {
        for filter in &self.message_filters {
            let moves: Vec<usize> = filter
                .actions
                .iter()
                .enumerate()
                .filter(|(_, a)| matches!(a, FilterAction::Move(_)))
                .map(|(i, _)| i)
                .collect();

            if moves.len() > 1 {
                return Err(eyre!(
                    "message-filter '{}' declares {} Move actions ({:?}); at most one \
                     Move per filter is allowed, and one message cannot land in two \
                     destinations",
                    filter.name,
                    moves.len(),
                    filter.actions
                ));
            }

            if let Some(&index) = moves.first()
                && index + 1 != filter.actions.len()
            {
                return Err(eyre!(
                    "message-filter '{}' declares a Move at position {} of {} actions \
                     ({:?}); a Move must be the LAST action, because the marker folds \
                     into the Move write and an action after it would be applied to an \
                     already-archived message",
                    filter.name,
                    index + 1,
                    filter.actions.len(),
                    filter.actions
                ));
            }
        }

        Ok(())
    }
}

/// Parse and VALIDATE a config from YAML text. `load_config` is this plus file IO and
/// creds-path resolution, so a config that parses here loads, and one that fails here
/// never reaches a mailbox.
pub fn parse_config(content: &str) -> Result<Config> {
    let cfg: Config = serde_yaml::from_str(content).map_err(|e| {
        error!("Failed to parse YAML: {}", e);
        eyre!("Failed to parse YAML: {}", e)
    })?;
    cfg.validate()?;
    Ok(cfg)
}

pub fn load_config(config_path: &Path) -> Result<Config> {
    debug!("Loading configuration from {:?}", config_path);

    let content = fs::read_to_string(config_path).map_err(|e| {
        error!(
            "Failed to read config file {}: {}",
            config_path.display(),
            e
        );
        eyre!(
            "Failed to read config file {}: {}",
            config_path.display(),
            e
        )
    })?;

    let mut cfg = parse_config(&content)?;

    // Resolve relative creds-path against the config file's directory
    let creds_str = cfg.auth.creds_path.to_str().unwrap_or_default();
    if !cfg.auth.creds_path.is_absolute() && !creds_str.starts_with("~/") {
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        cfg.auth.creds_path = config_dir.join(&cfg.auth.creds_path);
    }

    debug!("Successfully loaded configuration");
    Ok(cfg)
}

fn deserialize_named_filters<'de, D>(deserializer: D) -> Result<Vec<MessageFilter>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    let seq = match v {
        Value::Sequence(s) => s,
        _ => return Err(de::Error::custom("`message-filters` must be a sequence")),
    };
    let mut out = Vec::new();
    for entry in seq {
        if let Value::Mapping(map) = entry {
            if map.len() != 1 {
                return Err(de::Error::custom(
                    "Each filter must have exactly one name->body",
                ));
            }
            let (k, v) = map.into_iter().next().expect("checked len");
            let name = match k {
                Value::String(s) => s,
                _ => return Err(de::Error::custom("Filter name must be a string")),
            };
            let mut filt: MessageFilter = from_value(v).map_err(de::Error::custom)?;
            filt.name = name;
            out.push(filt);
        } else {
            return Err(de::Error::custom("Invalid entry in filters list"));
        }
    }
    Ok(out)
}

fn deserialize_named_states<'de, D>(deserializer: D) -> Result<Vec<StateFilter>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    let seq = match v {
        Value::Sequence(s) => s,
        _ => return Err(de::Error::custom("`state-filters` must be a sequence")),
    };
    let mut out = Vec::new();
    for entry in seq {
        if let Value::Mapping(map) = entry {
            if map.len() != 1 {
                return Err(de::Error::custom(
                    "Each state must have exactly one name->body",
                ));
            }
            let (k, v) = map.into_iter().next().expect("checked len");
            let name = match k {
                Value::String(s) => s,
                _ => return Err(de::Error::custom("State name must be a string")),
            };
            let mut st: StateFilter = from_value(v).map_err(de::Error::custom)?;
            st.name = name;
            out.push(st);
        } else {
            return Err(de::Error::custom("Invalid entry in states list"));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cfg::filter::FilterAction;
    use crate::cfg::label::Label;
    use crate::cfg::state::{StateAction, Ttl};

    #[test]
    fn test_load_full_config() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds

message-filters:
  - only-me-star:
      to: ['scott@example.com']
      cc: []
      from: '*@example.com'
      label: INBOX
      action: Star

  - only-me:
      to: ['scott@example.com']
      from: '*@example.com'
      label: INBOX
      action: Flag

state-filters:
  - Starred:
      labels: [Important, Starred]
      ttl: Keep

  - Cull:
      ttl:
        read: 7d
        unread: 21d
      action: Purgatory

  - Purge:
      label: Purgatory
      ttl: 3d
      action:
        Move: Oblivion
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.auth.callback_port, 13131);
        assert_eq!(config.log_level, "info"); // default
        assert_eq!(config.message_filters.len(), 2);
        assert_eq!(config.message_filters[0].name, "only-me-star");
        assert_eq!(config.message_filters[0].actions, vec![FilterAction::Star]);
        assert_eq!(config.message_filters[1].name, "only-me");
        assert_eq!(config.message_filters[1].actions, vec![FilterAction::Flag]);

        assert_eq!(config.state_filters.len(), 3);
        assert_eq!(config.state_filters[0].name, "Starred");
        assert_eq!(config.state_filters[0].ttl, Ttl::Keep);
        assert!(config.state_filters[0].labels.contains(&Label::Important));
        assert!(config.state_filters[0].labels.contains(&Label::Starred));

        assert_eq!(config.state_filters[1].name, "Cull");
        assert_eq!(
            config.state_filters[1].action,
            StateAction::Move("Purgatory".to_string())
        );

        assert_eq!(config.state_filters[2].name, "Purge");
        assert_eq!(
            config.state_filters[2].action,
            StateAction::Move("Oblivion".to_string())
        );
    }

    #[test]
    fn test_default_callback_port() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.auth.callback_port, 13131);
    }

    #[test]
    fn test_custom_callback_port() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
  callback-port: 9999
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.auth.callback_port, 9999);
    }

    #[test]
    fn test_log_level_from_config() {
        let yaml = r#"
log-level: debug
auth:
  creds-path: /tmp/creds
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_log_level_default() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_slack_absent_is_none() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.slack.is_none());
    }

    #[test]
    fn test_slack_block_with_defaults() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
slack:
  channel: D01G4Q7AWLV
  schedule: "Mon,Thu 07:00:00"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let slack = config.slack.expect("slack block present");
        assert_eq!(slack.channel, "D01G4Q7AWLV");
        assert_eq!(slack.token_env, "SLACK_XOXP_TOKEN");
        assert_eq!(slack.browser_index, 0);
        assert_eq!(slack.schedule, "Mon,Thu 07:00:00");
    }

    #[test]
    fn test_slack_block_requires_schedule() {
        // schedule has no serde default: omitting it must fail to parse rather
        // than silently fall back to a cadence that would clobber the timer.
        let yaml = r#"
auth:
  creds-path: /tmp/creds
slack:
  channel: D01G4Q7AWLV
"#;

        let result: Result<Config, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "missing schedule must be a hard error");
    }

    #[test]
    fn test_marker_label_defaults_to_triaged() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
"#;

        let config = parse_config(yaml).unwrap();
        assert_eq!(config.marker_label, "Triaged");
    }

    #[test]
    fn test_marker_label_override() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
marker-label: Handled
"#;

        let config = parse_config(yaml).unwrap();
        assert_eq!(config.marker_label, "Handled");
    }

    /// A marker naming a state-filter DESTINATION would be stripped by stage
    /// sanitization, silently un-marking every message it marked.
    #[test]
    fn test_marker_label_colliding_with_state_destination_fails_to_load() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
marker-label: Oblivion
state-filters:
  - Purge:
      label: Purgatory
      ttl: 3d
      action: Oblivion
"#;

        let err = parse_config(yaml).expect_err("colliding marker must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("marker-label 'Oblivion'"), "got: {}", msg);
        assert!(msg.contains("state-filter 'Purge'"), "got: {}", msg);
        assert!(msg.contains("destination"), "got: {}", msg);
    }

    /// The other half of the same rule: a state filter's `labels` block, which
    /// `ensure_labels` also harvests custom names from.
    #[test]
    fn test_marker_label_colliding_with_state_label_fails_to_load() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
marker-label: Purgatory
state-filters:
  - Purge:
      label: Purgatory
      ttl: 3d
      action: Oblivion
"#;

        let err = parse_config(yaml).expect_err("colliding marker must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("marker-label 'Purgatory'"), "got: {}", msg);
        assert!(msg.contains("label 'Purgatory'"), "got: {}", msg);
    }

    #[test]
    fn test_marker_label_not_colliding_loads() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
marker-label: Triaged
state-filters:
  - Purge:
      label: Purgatory
      ttl: 3d
      action: Oblivion
"#;

        let config = parse_config(yaml).unwrap();
        assert_eq!(config.marker_label, "Triaged");
    }

    fn filter_yaml(action: &str) -> String {
        format!(
            r#"
auth:
  creds-path: /tmp/creds
message-filters:
  - bots:
      from: '*@example.com'
      action: {}
"#,
            action
        )
    }

    /// The validator's full accept/reject table (design doc, Phase 4).
    #[test]
    fn test_move_must_be_the_last_action() {
        // ACCEPT: one Move, trivially final because it is the only action.
        let cfg = parse_config(&filter_yaml("Bots")).unwrap();
        assert_eq!(
            cfg.message_filters[0].actions,
            vec![FilterAction::Move("Bots".to_string())]
        );

        // ACCEPT: no Move at all.
        assert!(parse_config(&filter_yaml("Star")).is_ok());
        // ACCEPT: one Move, last.
        assert!(parse_config(&filter_yaml("[Star, Bots]")).is_ok());
        // ACCEPT: two pins, no Move, so the constraint does not apply.
        assert!(parse_config(&filter_yaml("[Star, Flag]")).is_ok());

        // REJECT: Move not last - the orphan case, where a later action fails after the
        // Move already archived the message.
        let err =
            parse_config(&filter_yaml("[Bots, Star]")).expect_err("[Bots, Star] must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("message-filter 'bots'"), "got: {}", msg);
        assert!(msg.contains("must be the LAST action"), "got: {}", msg);

        // REJECT: two Moves to the same destination.
        let err =
            parse_config(&filter_yaml("[Bots, Bots]")).expect_err("[Bots, Bots] must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("message-filter 'bots'"), "got: {}", msg);
        assert!(msg.contains("2 Move actions"), "got: {}", msg);

        // REJECT: two Moves to different destinations.
        let err = parse_config(&filter_yaml("[Bots, Purgatory]"))
            .expect_err("[Bots, Purgatory] must fail to load");
        assert!(
            format!("{}", err).contains("2 Move actions"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_slack_block_overrides() {
        let yaml = r#"
auth:
  creds-path: /tmp/creds
slack:
  token-env: MY_TOKEN
  channel: C12345
  browser-index: 2
  schedule: "Mon-Fri 09:00:00"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let slack = config.slack.expect("slack block present");
        assert_eq!(slack.token_env, "MY_TOKEN");
        assert_eq!(slack.channel, "C12345");
        assert_eq!(slack.browser_index, 2);
        assert_eq!(slack.schedule, "Mon-Fri 09:00:00");
    }
}
