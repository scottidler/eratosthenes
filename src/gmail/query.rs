use crate::cfg::filter::MessageFilter;

fn join_patterns(patterns: &[String]) -> String {
    patterns
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compile one message filter to a Gmail search query. `marker` is the account's marker
/// label (`marker-label`); every compiled query excludes it, so a message a filter has
/// already HANDLED is never fetched again. Pass `""` to omit the exclusion (tests only).
///
/// `-label:<marker>` is sound here where a thread-level negation would not be: this query
/// goes to `users.messages.list`, so predicate and returned unit are both message-level
/// and there is no existential projection.
pub fn compile_query(filter: &MessageFilter, marker: &str) -> String {
    let mut parts = Vec::new();

    // Multiple patterns compile to a single Gmail brace-OR term (`field:{a b}`),
    // never one `field:(...)` term per pattern joined by AND-at-top-level space.
    if let Some(ref af) = filter.to {
        if af.patterns.len() == 1 {
            parts.push(format!("to:{}", af.patterns[0]));
        } else if af.patterns.len() > 1 {
            parts.push(format!("to:{{{}}}", join_patterns(&af.patterns)));
        }
    }

    if let Some(ref af) = filter.from {
        if af.patterns.len() == 1 {
            parts.push(format!("from:({})", af.patterns[0]));
        } else if af.patterns.len() > 1 {
            parts.push(format!("from:{{{}}}", join_patterns(&af.patterns)));
        }
    }

    for pat in &filter.subject {
        let clean = pat.trim_matches('*');
        if !clean.is_empty() {
            parts.push(format!("subject:({})", clean));
        }
    }

    if !filter.labels.included.is_empty() {
        for label in &filter.labels.included {
            parts.push(format!("label:{}", label.to_gmail_id().to_lowercase()));
        }
    }

    // An empty filter compiles to an empty query and the caller skips it, so neither the
    // marker exclusion nor the read scope is worth emitting on its own.
    if !parts.is_empty() {
        // Exclude anything a message-filter already HANDLED. This, not `is:unread`, is
        // what makes acting idempotent.
        if !marker.is_empty() {
            parts.push(format!("-label:{}", marker.to_lowercase()));
        }
        // SCOPE constraint, not an idempotency guard: it declares that filters act on
        // unread mail only. Un-starring from the thread list leaves a message UNREAD, so
        // it does NOT stop re-labeling.
        parts.push("is:unread".to_string());
    }

    parts.join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cfg::filter::{AddressFilter, FilterAction, LabelsFilter};
    use crate::cfg::label::Label;
    use std::collections::HashMap;

    #[test]
    fn test_compile_only_me_star() {
        let filter = MessageFilter {
            name: "only-me-star".to_string(),
            to: Some(AddressFilter {
                patterns: vec!["scott@example.com".to_string()],
            }),
            cc: Some(AddressFilter { patterns: vec![] }),
            from: Some(AddressFilter {
                patterns: vec!["*@example.com".to_string()],
            }),
            subject: vec![],
            labels: LabelsFilter {
                included: vec![Label::Inbox],
                excluded: vec![],
            },
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let query = compile_query(&filter, "Triaged");
        assert!(query.contains("to:scott@example.com"));
        assert!(query.contains("from:(*@example.com)"));
        assert!(query.contains("label:inbox"));
        assert!(query.contains("is:unread"));
        // cc: [] cannot be expressed in Gmail query - not present
        assert!(!query.contains("cc"));
    }

    #[test]
    fn test_compile_minimal() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: None,
            cc: None,
            from: Some(AddressFilter {
                patterns: vec!["*@company.com".to_string()],
            }),
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Flag],
        };

        let query = compile_query(&filter, "Triaged");
        assert_eq!(query, "from:(*@company.com) -label:triaged is:unread");
    }

    #[test]
    fn test_compile_with_subject() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: None,
            cc: None,
            from: None,
            subject: vec!["*urgent*".to_string()],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Flag],
        };

        let query = compile_query(&filter, "Triaged");
        assert_eq!(query, "subject:(urgent) -label:triaged is:unread");
    }

    #[test]
    fn test_compile_multi_pattern_from_is_brace_or() {
        let filter = MessageFilter {
            name: "leadership".to_string(),
            to: None,
            cc: None,
            from: Some(AddressFilter {
                patterns: vec![
                    "philip@tatari.tv".to_string(),
                    "mark.weiler@tatari.tv".to_string(),
                ],
            }),
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let query = compile_query(&filter, "Triaged");
        assert!(query.contains("from:{philip@tatari.tv mark.weiler@tatari.tv}"));
        // Never one from:(...) term per pattern set joined by AND-space.
        assert!(!query.contains("from:(philip@tatari.tv mark.weiler@tatari.tv)"));
    }

    #[test]
    fn test_compile_multi_pattern_to_is_single_brace_or_term() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: Some(AddressFilter {
                patterns: vec!["a@example.com".to_string(), "b@example.com".to_string()],
            }),
            cc: None,
            from: None,
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let query = compile_query(&filter, "Triaged");
        // Exactly one to: term, not two ANDed to: terms.
        assert_eq!(query.matches("to:").count(), 1);
        assert!(query.contains("to:{a@example.com b@example.com}"));
    }

    #[test]
    fn test_compile_empty_filter() {
        let filter = MessageFilter {
            name: "test".to_string(),
            to: None,
            cc: None,
            from: None,
            subject: vec![],
            labels: LabelsFilter::default(),
            headers: HashMap::new(),
            actions: vec![FilterAction::Star],
        };

        let query = compile_query(&filter, "Triaged");
        assert!(query.is_empty());
    }

    /// Every non-empty compiled query excludes the marker, whatever the filter's shape.
    #[test]
    fn test_marker_exclusion_present_in_every_compiled_query() {
        let shapes = vec![
            ("to-only", Some(vec!["a@example.com"]), None, vec![]),
            ("from-only", None, Some(vec!["*@example.com"]), vec![]),
            ("subject-only", None, None, vec!["*urgent*"]),
            (
                "combined",
                Some(vec!["a@example.com", "b@example.com"]),
                Some(vec!["*@example.com"]),
                vec!["*urgent*"],
            ),
        ];

        for (name, to, from, subject) in shapes {
            let filter = MessageFilter {
                name: name.to_string(),
                to: to.map(|p| AddressFilter {
                    patterns: p.iter().map(|s| s.to_string()).collect(),
                }),
                cc: None,
                from: from.map(|p| AddressFilter {
                    patterns: p.iter().map(|s| s.to_string()).collect(),
                }),
                subject: subject.iter().map(|s| s.to_string()).collect(),
                labels: LabelsFilter::default(),
                headers: HashMap::new(),
                actions: vec![FilterAction::Star],
            };

            let query = compile_query(&filter, "Triaged");
            assert!(
                query.contains("-label:triaged"),
                "filter '{}' compiled without the marker exclusion: {}",
                name,
                query
            );
        }
    }
}
