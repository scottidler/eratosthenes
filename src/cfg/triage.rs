use std::path::PathBuf;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml::{Value, from_value};

/// A single classification bucket: an LLM-assigned category that becomes a Gmail
/// label. Buckets are config, not code, so adding one is a YAML edit (design doc,
/// Data Model): the `description` is folded directly into the classifier prompt.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TriageBucket {
    pub name: String,
    pub label: String,
    pub description: String,
    /// Whether this bucket gets a reply draft (Phase 7). Most buckets don't.
    #[serde(default)]
    pub draft: bool,
}

/// Optional per-account `triage:` block: the LLM classification pass in front of
/// the deterministic aging engine (design doc, Data Model). Absent -> `triage` is
/// a no-op and no triage timer is generated.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TriageConfig {
    /// Path to the `claude` binary. Unset -> resolved on PATH at call time
    /// (Phase 4/5); Phase 1 only carries the config value.
    #[serde(
        default,
        deserialize_with = "crate::cfg::deserialize_tilde_pathbuf_opt"
    )]
    pub claude_binary: Option<PathBuf>,

    /// systemd OnCalendar string driving the triage timer. REQUIRED when this
    /// block is present -- no default, deliberately, same reasoning as
    /// `slack.schedule`: a silent weekday guess would clobber the installed
    /// timer, so a missing schedule is a named config-load error instead.
    pub schedule: String,

    /// Per-run cap on candidate threads. Hitting it logs loudly; it never
    /// truncates silently (design doc, Data Model: Cap order).
    #[serde(default = "default_max_threads")]
    pub max_threads: u32,

    /// Per-thread char budget for body extraction, newest messages first.
    #[serde(default = "default_body_chars")]
    pub body_chars: usize,

    /// Model used for the batched classify call.
    #[serde(default = "default_classify_model")]
    pub classify_model: String,

    /// Model used for reply-draft generation (Phase 7).
    #[serde(default = "default_draft_model")]
    pub draft_model: String,

    /// Path to Scott's voice profile, consumed by draft prompts only
    /// (Phase 7). No default: drafting is skipped loudly if unset.
    #[serde(
        default,
        deserialize_with = "crate::cfg::deserialize_tilde_pathbuf_opt"
    )]
    pub voice_profile: Option<PathBuf>,

    /// Classification taxonomy. Defaults to the five-bucket set shipped in
    /// `eratosthenes.example.yml` so a bare `triage:` block (schedule only)
    /// still has a working taxonomy.
    #[serde(default = "default_buckets", deserialize_with = "deserialize_buckets")]
    pub buckets: Vec<TriageBucket>,
}

impl TriageConfig {
    /// Reject a `body-chars` too small to carry a truncation marker.
    ///
    /// `body::truncate` cuts bare below that floor, and a bare sliver reads to
    /// the model as a COMPLETE short message. `budget_messages` exempts the
    /// newest message from the floor so a thread is never sent empty, which
    /// means the only way to reach a bare cut is a `body-chars` configured
    /// below it. Validating here makes that unreachable by construction instead
    /// of defended by a comment.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.body_chars < crate::triage::body::MIN_MARKED_FRAGMENT_CHARS {
            eyre::bail!(
                "triage body-chars is {} but must be at least {}: below that a truncated body \
cannot carry its truncation marker, and an unmarked fragment reads to the model as a \
complete message",
                self.body_chars,
                crate::triage::body::MIN_MARKED_FRAGMENT_CHARS
            );
        }
        Ok(())
    }
}

fn default_max_threads() -> u32 {
    50
}

fn default_body_chars() -> usize {
    4000
}

fn default_classify_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}

fn default_draft_model() -> String {
    "claude-sonnet-5".to_string()
}

fn default_buckets() -> Vec<TriageBucket> {
    vec![
        TriageBucket {
            name: "needs-reply".to_string(),
            label: "llm/needs-reply".to_string(),
            description: "a real human wrote to Scott and expects a reply or action".to_string(),
            draft: true,
        },
        TriageBucket {
            name: "fyi-work".to_string(),
            label: "llm/fyi-work".to_string(),
            description: "work-relevant notifications -- AWS, security, CI, vendors".to_string(),
            draft: false,
        },
        TriageBucket {
            name: "recruiting".to_string(),
            label: "llm/recruiting".to_string(),
            description: "LinkedIn, recruiters, job alerts".to_string(),
            draft: false,
        },
        TriageBucket {
            name: "receipts".to_string(),
            label: "llm/receipts".to_string(),
            description: "invoices, payments, renewals, order confirmations".to_string(),
            draft: false,
        },
        TriageBucket {
            name: "noise".to_string(),
            label: "llm/noise".to_string(),
            description: "everything else -- marketing, newsletters, social".to_string(),
            draft: false,
        },
    ]
}

/// Deserialize `buckets` one entry at a time so a bucket that fails to parse (a
/// missing `label`, most commonly) names ITSELF in the error, not just the field.
/// Plain `#[derive(Deserialize)]` on `Vec<TriageBucket>` would surface only
/// serde_yaml's own "missing field `label`" message, with no way to tell which of
/// several buckets it was.
fn deserialize_buckets<'de, D>(deserializer: D) -> Result<Vec<TriageBucket>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer).map_err(de::Error::custom)?;
    let seq = match v {
        Value::Sequence(s) => s,
        _ => return Err(de::Error::custom("`buckets` must be a sequence")),
    };

    let mut out = Vec::new();
    for (i, entry) in seq.into_iter().enumerate() {
        let ident = if let Value::Mapping(ref m) = entry {
            m.get(Value::String("name".to_string()))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
        .unwrap_or_else(|| format!("#{}", i + 1));

        let bucket: TriageBucket = from_value(entry)
            .map_err(|e| de::Error::custom(format!("triage bucket '{}': {}", ident, e)))?;
        out.push(bucket);
    }

    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_triage_requires_schedule() {
        let yaml = r#"
buckets:
  - name: noise
    label: llm/noise
    description: everything else
"#;
        let result: Result<TriageConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "missing schedule must be a hard error");
    }

    #[test]
    fn test_triage_minimal_uses_defaults() {
        let yaml = r#"
schedule: "Mon..Fri 06:30:00"
"#;
        let config: TriageConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.schedule, "Mon..Fri 06:30:00");
        assert_eq!(config.max_threads, 50);
        assert_eq!(config.body_chars, 4000);
        assert_eq!(config.classify_model, "claude-haiku-4-5-20251001");
        assert_eq!(config.draft_model, "claude-sonnet-5");
        assert!(config.claude_binary.is_none());
        assert!(config.voice_profile.is_none());
        assert_eq!(config.buckets.len(), 5);
        assert_eq!(config.buckets[0].name, "needs-reply");
        assert!(config.buckets[0].draft);
    }

    #[test]
    fn test_triage_overrides() {
        let yaml = r#"
claude-binary: /opt/claude/bin/claude
schedule: "Mon..Fri 06:30:00"
max-threads: 10
body-chars: 2000
classify-model: claude-opus-5
draft-model: claude-opus-5
voice-profile: ~/Claude/writing/VOICE.md
buckets:
  - name: noise
    label: llm/noise
    description: everything else
"#;
        let config: TriageConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.claude_binary,
            Some(PathBuf::from("/opt/claude/bin/claude"))
        );
        assert_eq!(config.max_threads, 10);
        assert_eq!(config.body_chars, 2000);
        assert_eq!(config.classify_model, "claude-opus-5");
        assert_eq!(config.draft_model, "claude-opus-5");
        // `~` is expanded at LOAD time by `cfg::deserialize_tilde_pathbuf_opt`.
        // Phase 7 opens this path directly, so a literal `~` here would mean the
        // voice profile is never found.
        assert_eq!(
            config.voice_profile,
            Some(
                dirs::home_dir()
                    .expect("home dir")
                    .join("Claude/writing/VOICE.md")
            )
        );
        assert_eq!(config.buckets.len(), 1);
        assert_eq!(config.buckets[0].name, "noise");
    }

    #[test]
    fn test_bucket_missing_label_names_bucket_and_field() {
        let yaml = r#"
schedule: "Mon..Fri 06:30:00"
buckets:
  - name: noise
    description: everything else
"#;
        let err = serde_yaml::from_str::<TriageConfig>(yaml)
            .expect_err("bucket missing label must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("triage bucket 'noise'"), "got: {}", msg);
        assert!(msg.contains("label"), "got: {}", msg);
    }

    #[test]
    fn test_bucket_missing_name_falls_back_to_position() {
        let yaml = r#"
schedule: "Mon..Fri 06:30:00"
buckets:
  - label: llm/noise
    description: everything else
"#;
        let err = serde_yaml::from_str::<TriageConfig>(yaml)
            .expect_err("bucket missing name must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("triage bucket '#1'"), "got: {}", msg);
    }

    /// A `body-chars` below the marked-fragment floor is rejected at LOAD, so
    /// `budget_messages`' newest-message exemption can never actually produce
    /// an unmarked sliver (audit follow-on to C4).
    #[test]
    fn test_body_chars_below_the_marker_floor_is_rejected() {
        let floor = crate::triage::body::MIN_MARKED_FRAGMENT_CHARS;
        let yaml = format!("schedule: 'Mon 07:00:00'\nbody-chars: {}\n", floor - 1);
        let config: TriageConfig = serde_yaml::from_str(&yaml).expect("parses");
        let err = config
            .validate()
            .expect_err("a body-chars below the floor must not load");
        let text = format!("{err:#}");
        assert!(text.contains("body-chars"), "{}", text);
        assert!(text.contains(&floor.to_string()), "{}", text);
    }

    #[test]
    fn test_body_chars_at_the_floor_loads() {
        let floor = crate::triage::body::MIN_MARKED_FRAGMENT_CHARS;
        let yaml = format!("schedule: 'Mon 07:00:00'\nbody-chars: {}\n", floor);
        let config: TriageConfig = serde_yaml::from_str(&yaml).expect("parses");
        assert!(config.validate().is_ok());
    }

    /// The shipped default must satisfy its own validator.
    #[test]
    fn test_the_default_body_chars_is_valid() {
        let config: TriageConfig =
            serde_yaml::from_str("schedule: 'Mon 07:00:00'\n").expect("parses");
        assert_eq!(config.body_chars, 4000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_buckets_must_be_a_sequence() {
        let yaml = r#"
schedule: "Mon..Fri 06:30:00"
buckets: not-a-list
"#;
        let err = serde_yaml::from_str::<TriageConfig>(yaml)
            .expect_err("non-sequence buckets must fail to load");
        let msg = format!("{}", err);
        assert!(msg.contains("must be a sequence"), "got: {}", msg);
    }
}
