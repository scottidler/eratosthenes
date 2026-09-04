use crate::cfg::label::Label;
use globset::Glob;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml::{Value, from_value};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct AddressFilter {
    pub patterns: Vec<String>,
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub enum FilterAction {
    Star,
    Flag,
    /// Add the destination label AND remove INBOX + UNREAD: the only action that
    /// destroys a message's future eligibility, which is why the marker folds into it.
    Move(String),
    /// Add a label, remove nothing. The mechanism the engine stamps the marker with.
    Tag(String),
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LabelsFilter {
    pub included: Vec<Label>,
    pub excluded: Vec<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageFilter {
    #[serde(skip_deserializing)]
    pub name: String,

    #[serde(default, deserialize_with = "deserialize_opt_address_filter")]
    pub to: Option<AddressFilter>,

    #[serde(default, deserialize_with = "deserialize_opt_address_filter")]
    pub cc: Option<AddressFilter>,

    #[serde(default, deserialize_with = "deserialize_opt_address_filter")]
    pub from: Option<AddressFilter>,

    #[serde(default)]
    pub subject: Vec<String>,

    #[serde(
        default,
        alias = "label",
        deserialize_with = "deserialize_labels_filter"
    )]
    pub labels: LabelsFilter,

    #[serde(default)]
    pub headers: HashMap<String, Vec<String>>,

    #[serde(default, alias = "action", deserialize_with = "deserialize_actions")]
    pub actions: Vec<FilterAction>,
}

impl AddressFilter {
    pub fn matches(&self, emails: &[String]) -> bool {
        for pat in &self.patterns {
            let matcher = Glob::new(pat).expect("invalid glob").compile_matcher();
            for email in emails {
                if matcher.is_match(email) {
                    return true;
                }
            }
        }
        false
    }
}

impl MessageFilter {
    /// `marker` is the account's marker label (`marker-label`), excluded by EVERY filter.
    /// The compiled Gmail query already carries `-label:<marker>`, but that query is only
    /// a prefilter, so the marker is enforced here too, where the matcher is
    /// authoritative. Pass `""` to disable (tests only; the config always has one).
    ///
    /// A marked message fails to match, so it is never CLAIMED either, and falls through
    /// to later filters. That fall-through is exactly what made per-filter
    /// `labels.excluded` harmful; it is safe here only because the exclusion is UNIFORM,
    /// so every later filter rejects the message for the same reason.
    pub fn matches(
        &self,
        to: &[String],
        cc: &[String],
        from: &[String],
        subject: &str,
        labels: &[Label],
        headers: &HashMap<String, String>,
        marker: &str,
    ) -> bool {
        if !marker.is_empty() && labels.contains(&Label::new(marker)) {
            return false;
        }

        if let Some(ref af) = self.to {
            if af.patterns.is_empty() {
                if !to.is_empty() {
                    return false;
                }
            } else if !af.matches(to) {
                return false;
            }
        }

        if let Some(ref af) = self.cc {
            if af.patterns.is_empty() {
                if !cc.is_empty() {
                    return false;
                }
            } else if !af.matches(cc) {
                return false;
            }
        }

        if let Some(ref af) = self.from {
            if af.patterns.is_empty() {
                if !from.is_empty() {
                    return false;
                }
            } else if !af.matches(from) {
                return false;
            }
        }

        if !self.subject.is_empty() {
            let mut found = false;
            for pat in &self.subject {
                let matcher = Glob::new(pat).expect("invalid glob").compile_matcher();
                if matcher.is_match(subject) {
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }

        if !self.labels.included.is_empty()
            && !labels.iter().any(|l| self.labels.included.contains(l))
        {
            return false;
        }
        if !self.labels.excluded.is_empty()
            && labels.iter().any(|l| self.labels.excluded.contains(l))
        {
            return false;
        }

        for (header_name, patterns) in &self.headers {
            if let Some(header_value) = headers.get(header_name) {
                if patterns.is_empty() {
                    return false;
                }
                let mut matched = false;
                for pat in patterns {
                    let matcher = Glob::new(pat).expect("invalid glob").compile_matcher();
                    if matcher.is_match(header_value) {
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    return false;
                }
            } else if !patterns.is_empty() {
                return false;
            }
        }

        true
    }
}

fn deserialize_opt_address_filter<'de, D>(
    deserializer: D,
) -> Result<Option<AddressFilter>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    match v {
        Value::Null => Ok(None),
        Value::Sequence(seq) => {
            let mut patterns = Vec::new();
            for val in seq {
                if let Value::String(s) = val {
                    patterns.push(s);
                } else {
                    return Err(de::Error::custom("Invalid entry in address filter"));
                }
            }
            Ok(Some(AddressFilter { patterns }))
        }
        Value::String(s) => Ok(Some(AddressFilter { patterns: vec![s] })),
        other @ Value::Mapping(_) => {
            let af: AddressFilter = from_value(other).map_err(de::Error::custom)?;
            Ok(Some(af))
        }
        _ => Err(de::Error::custom("Invalid address filter format")),
    }
}

fn deserialize_labels_filter<'de, D>(deserializer: D) -> Result<LabelsFilter, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    match v {
        Value::String(s) => Ok(LabelsFilter {
            included: vec![Label::new(&s)],
            excluded: vec![],
        }),
        Value::Sequence(seq) => {
            let mut included = Vec::new();
            for val in seq {
                match val {
                    Value::String(s) => included.push(Label::new(&s)),
                    _ => return Err(de::Error::custom("Invalid label entry")),
                }
            }
            Ok(LabelsFilter {
                included,
                excluded: vec![],
            })
        }
        Value::Mapping(map) => {
            let mut included = Vec::new();
            let mut excluded = Vec::new();
            for (k, v) in map {
                let key = match k {
                    Value::String(s) => s,
                    _ => return Err(de::Error::custom("Non-string key in labels map")),
                };
                match key.as_str() {
                    "included" => {
                        if let Value::Sequence(seq) = v {
                            for inner in seq {
                                if let Value::String(s) = inner {
                                    included.push(Label::new(&s));
                                } else {
                                    return Err(de::Error::custom("Invalid included label"));
                                }
                            }
                        } else {
                            return Err(de::Error::custom("`included` must be a sequence"));
                        }
                    }
                    "excluded" => {
                        if let Value::Sequence(seq) = v {
                            for inner in seq {
                                if let Value::String(s) = inner {
                                    excluded.push(Label::new(&s));
                                } else {
                                    return Err(de::Error::custom("Invalid excluded label"));
                                }
                            }
                        } else {
                            return Err(de::Error::custom("`excluded` must be a sequence"));
                        }
                    }
                    other => {
                        return Err(de::Error::unknown_field(other, &["included", "excluded"]));
                    }
                }
            }
            Ok(LabelsFilter { included, excluded })
        }
        _ => Err(de::Error::custom("Invalid `labels` value")),
    }
}

/// Parse ONE action. `Star` and `Flag` are bare names, a bare name that is neither is a
/// Move destination, and `Tag`/`Move` take a label via a single-key mapping. `Tag` gets an
/// explicit arm ahead of the Move catch-all: without it, `action: Tag` parses silently as
/// a Move to a label literally named "Tag".
fn parse_action(v: Value) -> Result<FilterAction, String> {
    match v {
        Value::String(s) => match s.as_str() {
            "Star" => Ok(FilterAction::Star),
            "Flag" => Ok(FilterAction::Flag),
            "Tag" => Err("`Tag` needs a label: write `action: {Tag: <label>}`".to_string()),
            "Move" => Err("`Move` needs a destination: write `action: <destination>`".to_string()),
            other => Ok(FilterAction::Move(other.to_string())),
        },
        Value::Mapping(map) => {
            if map.len() != 1 {
                return Err("An action mapping must have exactly one key".to_string());
            }
            let (k, v) = map.into_iter().next().ok_or("checked len")?;
            let (Value::String(key), Value::String(label)) = (k, v) else {
                return Err("An action mapping must be `<Tag|Move>: <label>`".to_string());
            };
            match key.as_str() {
                "Tag" => Ok(FilterAction::Tag(label)),
                "Move" => Ok(FilterAction::Move(label)),
                other => Err(format!(
                    "Unknown action '{}', expected one of `Tag`, `Move`",
                    other
                )),
            }
        }
        _ => Err("Invalid `action` value".to_string()),
    }
}

fn deserialize_actions<'de, D>(deserializer: D) -> Result<Vec<FilterAction>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    let mut out = Vec::new();
    match v {
        Value::Sequence(seq) => {
            for val in seq {
                out.push(parse_action(val).map_err(de::Error::custom)?);
            }
        }
        single => out.push(parse_action(single).map_err(de::Error::custom)?),
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_address_filter_matches_exact() {
        let filter = AddressFilter {
            patterns: vec!["test@example.com".to_string()],
        };
        assert!(filter.matches(&["test@example.com".to_string()]));
        assert!(!filter.matches(&["other@example.com".to_string()]));
    }

    #[test]
    fn test_address_filter_matches_glob() {
        let filter = AddressFilter {
            patterns: vec!["*@example.com".to_string()],
        };
        assert!(filter.matches(&["test@example.com".to_string()]));
        assert!(filter.matches(&["anyone@example.com".to_string()]));
        assert!(!filter.matches(&["test@other.com".to_string()]));
    }

    #[test]
    fn test_message_filter_matches_to() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: Some(AddressFilter {
                patterns: vec!["me@example.com".to_string()],
            }),
            cc: None,
            from: None,
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let to = vec!["me@example.com".to_string()];
        let cc = vec![];
        let from = vec!["sender@example.com".to_string()];
        assert!(filter.matches(&to, &cc, &from, "Test", &[], &HashMap::new(), ""));

        let to2 = vec!["other@example.com".to_string()];
        assert!(!filter.matches(&to2, &cc, &from, "Test", &[], &HashMap::new(), ""));
    }

    #[test]
    fn test_message_filter_requires_empty_cc() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: None,
            cc: Some(AddressFilter { patterns: vec![] }),
            from: None,
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let empty: Vec<String> = vec![];
        assert!(filter.matches(&empty, &empty, &empty, "Test", &[], &HashMap::new(), ""));

        let with_cc = vec!["cc@example.com".to_string()];
        assert!(!filter.matches(&empty, &with_cc, &empty, "Test", &[], &HashMap::new(), ""));
    }

    #[test]
    fn test_message_filter_combined_only_me() {
        let filter = MessageFilter {
            name: "only-me".to_string(),
            to: Some(AddressFilter {
                patterns: vec!["me@example.com".to_string()],
            }),
            cc: Some(AddressFilter { patterns: vec![] }),
            from: Some(AddressFilter {
                patterns: vec!["*@company.com".to_string()],
            }),
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let to = vec!["me@example.com".to_string()];
        let from = vec!["boss@company.com".to_string()];
        let empty: Vec<String> = vec![];

        assert!(filter.matches(&to, &empty, &from, "Good", &[], &HashMap::new(), ""));

        let with_cc = vec!["other@example.com".to_string()];
        assert!(!filter.matches(&to, &with_cc, &from, "CC", &[], &HashMap::new(), ""));

        let wrong_from = vec!["spam@other.com".to_string()];
        assert!(!filter.matches(&to, &empty, &wrong_from, "Spam", &[], &HashMap::new(), ""));
    }

    #[test]
    fn test_header_must_not_exist() {
        let mut header_patterns = HashMap::new();
        header_patterns.insert("List-Id".to_string(), vec![]);

        let filter = MessageFilter {
            name: "no-list".to_string(),
            to: None,
            cc: None,
            from: None,
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: header_patterns,
            actions: vec![FilterAction::Star],
        };

        let empty: Vec<String> = vec![];
        let no_headers = HashMap::new();
        assert!(filter.matches(&empty, &empty, &empty, "Test", &[], &no_headers, ""));

        let mut with_list_id = HashMap::new();
        with_list_id.insert("List-Id".to_string(), "<repo.github.com>".to_string());
        assert!(!filter.matches(&empty, &empty, &empty, "Test", &[], &with_list_id, ""));
    }

    /// The marker is enforced in the Rust matcher, not just in the Gmail query: a marked
    /// message must not match even when every other criterion does.
    #[test]
    fn test_marker_label_suppresses_the_match() {
        let filter = MessageFilter {
            name: "leadership".to_string(),
            to: None,
            cc: None,
            from: Some(AddressFilter {
                patterns: vec!["*@example.com".to_string()],
            }),
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let empty: Vec<String> = vec![];
        let from = vec!["boss@example.com".to_string()];
        let unmarked = [Label::Inbox, Label::Unread];
        let marked = [
            Label::Inbox,
            Label::Unread,
            Label::Custom("Triaged".to_string()),
        ];

        assert!(filter.matches(
            &empty,
            &empty,
            &from,
            "Test",
            &unmarked,
            &HashMap::new(),
            "Triaged"
        ));
        assert!(!filter.matches(
            &empty,
            &empty,
            &from,
            "Test",
            &marked,
            &HashMap::new(),
            "Triaged"
        ));
    }

    /// `action: Tag` must not parse as `Move("Tag")` via the catch-all.
    #[test]
    fn test_tag_action_needs_a_label_and_is_never_a_move() {
        let err = serde_yaml::from_str::<MessageFilter>("action: Tag")
            .expect_err("bare `Tag` must be rejected");
        assert!(
            format!("{}", err).contains("`Tag` needs a label"),
            "{}",
            err
        );

        let filter: MessageFilter =
            serde_yaml::from_str("action: {Tag: Triaged}").expect("mapping form parses");
        assert_eq!(
            filter.actions,
            vec![FilterAction::Tag("Triaged".to_string())]
        );
    }

    #[test]
    fn test_action_sequence_and_move_mapping_parse() {
        let filter: MessageFilter =
            serde_yaml::from_str("action: [Star, Bots]").expect("sequence parses");
        assert_eq!(
            filter.actions,
            vec![FilterAction::Star, FilterAction::Move("Bots".to_string())]
        );

        let filter: MessageFilter =
            serde_yaml::from_str("action: {Move: Bots}").expect("Move mapping parses");
        assert_eq!(filter.actions, vec![FilterAction::Move("Bots".to_string())]);

        let err = serde_yaml::from_str::<MessageFilter>("action: {Nope: Bots}")
            .expect_err("unknown mapping key must be rejected");
        assert!(
            format!("{}", err).contains("Unknown action 'Nope'"),
            "{}",
            err
        );
    }
}
