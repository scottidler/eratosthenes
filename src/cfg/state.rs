use chrono::{self, DateTime, Utc};
use eyre::eyre;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml::Value;

use crate::cfg::label::Label;

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
}

pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ttl {
    Keep,
    Days(chrono::Duration),
    Detailed {
        read: chrono::Duration,
        unread: chrono::Duration,
    },
}

impl<'de> Deserialize<'de> for Ttl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TtlVisitor;

        impl<'de> de::Visitor<'de> for TtlVisitor {
            type Value = Ttl;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("Keep, '<n>d', or { read: '<n>d', unread: '<n>d' }")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "Keep" {
                    Ok(Ttl::Keep)
                } else {
                    parse_days(value)
                        .map(Ttl::Days)
                        .map_err(|e| E::custom(format!("Invalid TTL '{}': {}", value, e)))
                }
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: de::MapAccess<'de>,
            {
                let mut read = None;
                let mut unread = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "read" => {
                            let v: String = map.next_value()?;
                            read = Some(parse_days(&v).map_err(|e| de::Error::custom(e.to_string()))?);
                        }
                        "unread" => {
                            let v: String = map.next_value()?;
                            unread = Some(parse_days(&v).map_err(|e| de::Error::custom(e.to_string()))?);
                        }
                        other => return Err(de::Error::unknown_field(other, &["read", "unread"])),
                    }
                }

                let read = read.ok_or_else(|| de::Error::missing_field("read"))?;
                let unread = unread.ok_or_else(|| de::Error::missing_field("unread"))?;
                Ok(Ttl::Detailed { read, unread })
            }
        }

        deserializer.deserialize_any(TtlVisitor)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub enum StateAction {
    Move(String),
    Delete,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StateFilter {
    #[serde(skip_deserializing, default)]
    pub name: String,

    #[serde(default, alias = "label", deserialize_with = "deserialize_labels_vec")]
    pub labels: Vec<Label>,

    pub ttl: Ttl,

    #[serde(
        default = "default_action",
        alias = "action",
        deserialize_with = "deserialize_state_action"
    )]
    pub action: StateAction,
}

impl StateFilter {
    pub fn matches_labels(&self, labels: &[Label]) -> bool {
        if self.labels.is_empty() {
            return true;
        }
        labels.iter().any(|l| self.labels.contains(l))
    }

    pub fn evaluate_ttl<C: Clock>(
        &self,
        date: DateTime<Utc>,
        is_read: bool,
        clock: &C,
    ) -> eyre::Result<Option<StateAction>> {
        let now = clock.now();
        let age = now.signed_duration_since(date);

        let ttl_duration = match &self.ttl {
            Ttl::Keep => return Ok(None),
            Ttl::Days(dur) => *dur,
            Ttl::Detailed { read, unread } => {
                if is_read {
                    *read
                } else {
                    *unread
                }
            }
        };

        if age >= ttl_duration { Ok(Some(self.action.clone())) } else { Ok(None) }
    }
}

pub fn parse_days(s: &str) -> eyre::Result<chrono::Duration> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('d') {
        let days: i64 = num.parse().map_err(|_| eyre!("Invalid day count: {}", num))?;
        Ok(chrono::Duration::days(days))
    } else {
        Err(eyre!("TTL must end with 'd' (e.g. '7d'), got: {}", s))
    }
}

fn deserialize_labels_vec<'de, D>(deserializer: D) -> Result<Vec<Label>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    match v {
        Value::String(s) => Ok(vec![Label::new(&s)]),
        Value::Sequence(seq) => seq
            .into_iter()
            .map(|val| {
                if let Value::String(s) = val {
                    Ok(Label::new(&s))
                } else {
                    Err(de::Error::custom("Invalid label entry"))
                }
            })
            .collect(),
        _ => Err(de::Error::custom("Invalid `labels` value")),
    }
}

fn deserialize_state_action<'de, D>(deserializer: D) -> Result<StateAction, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    match v {
        Value::String(s) => Ok(StateAction::Move(s)),
        Value::Mapping(m) => {
            if m.len() != 1 {
                return Err(de::Error::custom("Expected single key in action map"));
            }
            let (k, v) = m.into_iter().next().expect("checked len");
            let key = if let Value::String(s) = k {
                s
            } else {
                return Err(de::Error::custom("Invalid action key"));
            };
            let target = if let Value::String(s) = v {
                s
            } else {
                return Err(de::Error::custom("Invalid action target"));
            };
            match key.as_str() {
                "Move" => Ok(StateAction::Move(target)),
                "Delete" => Ok(StateAction::Delete),
                other => Err(de::Error::unknown_field(other, &["Move", "Delete"])),
            }
        }
        _ => Err(de::Error::custom("Invalid `action` value")),
    }
}

fn default_action() -> StateAction {
    StateAction::Move(String::new())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Duration;

    struct FakeClock(DateTime<Utc>);

    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    #[test]
    fn test_ttl_keep_never_expires() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Keep,
            action: StateAction::Move("Archive".to_string()),
        };

        let now = Utc::now();
        let old_date = now - Duration::days(365);
        let clock = FakeClock(now);

        assert!(filter.evaluate_ttl(old_date, false, &clock).unwrap().is_none());
    }

    #[test]
    fn test_ttl_days_expired() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Days(Duration::days(7)),
            action: StateAction::Move("Archive".to_string()),
        };

        let now = Utc::now();
        let ten_days_ago = now - Duration::days(10);
        let clock = FakeClock(now);

        let result = filter.evaluate_ttl(ten_days_ago, false, &clock).unwrap();
        assert_eq!(result, Some(StateAction::Move("Archive".to_string())));
    }

    #[test]
    fn test_ttl_days_not_expired() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Days(Duration::days(7)),
            action: StateAction::Move("Archive".to_string()),
        };

        let now = Utc::now();
        let three_days_ago = now - Duration::days(3);
        let clock = FakeClock(now);

        assert!(filter.evaluate_ttl(three_days_ago, false, &clock).unwrap().is_none());
    }

    #[test]
    fn test_ttl_detailed_read_expired() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Detailed {
                read: Duration::days(7),
                unread: Duration::days(21),
            },
            action: StateAction::Move("Archive".to_string()),
        };

        let now = Utc::now();
        let ten_days_ago = now - Duration::days(10);
        let clock = FakeClock(now);

        assert!(filter.evaluate_ttl(ten_days_ago, true, &clock).unwrap().is_some());
    }

    #[test]
    fn test_ttl_detailed_unread_not_expired() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Detailed {
                read: Duration::days(7),
                unread: Duration::days(21),
            },
            action: StateAction::Move("Archive".to_string()),
        };

        let now = Utc::now();
        let ten_days_ago = now - Duration::days(10);
        let clock = FakeClock(now);

        assert!(filter.evaluate_ttl(ten_days_ago, false, &clock).unwrap().is_none());
    }

    #[test]
    fn test_matches_labels() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![Label::Starred, Label::Important],
            ttl: Ttl::Keep,
            action: StateAction::Move("Archive".to_string()),
        };

        assert!(filter.matches_labels(&[Label::Starred]));
        assert!(filter.matches_labels(&[Label::Important]));
        assert!(!filter.matches_labels(&[Label::Inbox]));
    }

    #[test]
    fn test_empty_labels_matches_all() {
        let filter = StateFilter {
            name: "test".to_string(),
            labels: vec![],
            ttl: Ttl::Keep,
            action: StateAction::Move("Archive".to_string()),
        };

        assert!(filter.matches_labels(&[Label::Custom("anything".to_string())]));
    }

    #[test]
    fn test_ttl_deserialize_keep() {
        let yaml = "Keep";
        let ttl: Ttl = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(ttl, Ttl::Keep);
    }

    #[test]
    fn test_ttl_deserialize_days() {
        let yaml = "7d";
        let ttl: Ttl = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(ttl, Ttl::Days(Duration::days(7)));
    }

    #[test]
    fn test_ttl_deserialize_detailed() {
        let yaml = "read: 7d\nunread: 21d";
        let ttl: Ttl = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            ttl,
            Ttl::Detailed {
                read: Duration::days(7),
                unread: Duration::days(21)
            }
        );
    }

    #[test]
    fn test_parse_days() {
        assert_eq!(parse_days("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_days("21d").unwrap(), Duration::days(21));
        assert!(parse_days("bad").is_err());
    }
}
