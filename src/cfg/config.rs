use eyre::{Result, eyre};
use log::{debug, error};
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml::{Value, from_value};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cfg::filter::MessageFilter;
use crate::cfg::state::StateFilter;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AuthConfig {
    pub client_secret_path: PathBuf,
    pub token_cache_path: PathBuf,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
}

fn default_callback_port() -> u16 {
    13131
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub auth: AuthConfig,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    #[serde(rename = "message-filters", default, deserialize_with = "deserialize_named_filters")]
    pub message_filters: Vec<MessageFilter>,

    #[serde(rename = "state-filters", default, deserialize_with = "deserialize_named_states")]
    pub state_filters: Vec<StateFilter>,
}

fn default_log_level() -> String {
    "info".to_string()
}

pub fn load_config(config_path: &Path) -> Result<Config> {
    debug!("Loading configuration from {:?}", config_path);

    let content = fs::read_to_string(config_path).map_err(|e| {
        error!("Failed to read config file {}: {}", config_path.display(), e);
        eyre!("Failed to read config file {}: {}", config_path.display(), e)
    })?;

    let cfg: Config = serde_yaml::from_str(&content).map_err(|e| {
        error!("Failed to parse YAML: {}", e);
        eyre!("Failed to parse YAML: {}", e)
    })?;

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
                return Err(de::Error::custom("Each filter must have exactly one name->body"));
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
                return Err(de::Error::custom("Each state must have exactly one name->body"));
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
  client-secret-path: "/tmp/secret.json"
  token-cache-path: "/tmp/tokens.json"

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
  client-secret-path: "/tmp/secret.json"
  token-cache-path: "/tmp/tokens.json"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.auth.callback_port, 13131);
    }

    #[test]
    fn test_custom_callback_port() {
        let yaml = r#"
auth:
  client-secret-path: "/tmp/secret.json"
  token-cache-path: "/tmp/tokens.json"
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
  client-secret-path: "/tmp/secret.json"
  token-cache-path: "/tmp/tokens.json"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_log_level_default() {
        let yaml = r#"
auth:
  client-secret-path: "/tmp/secret.json"
  token-cache-path: "/tmp/tokens.json"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_level, "info");
    }
}
