use crate::cfg::filter::MessageFilter;

pub fn compile_query(filter: &MessageFilter) -> String {
    let mut parts = Vec::new();

    if let Some(ref af) = filter.to {
        for pat in &af.patterns {
            parts.push(format!("to:{}", pat));
        }
    }

    if let Some(ref af) = filter.from {
        if af.patterns.len() == 1 {
            parts.push(format!("from:({})", af.patterns[0]));
        } else if af.patterns.len() > 1 {
            let joined = af.patterns.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" ");
            parts.push(format!("from:({})", joined));
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

    // Only match unread messages - prevents re-labeling read emails
    if !parts.is_empty() {
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

        let query = compile_query(&filter);
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

        let query = compile_query(&filter);
        assert_eq!(query, "from:(*@company.com) is:unread");
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

        let query = compile_query(&filter);
        assert_eq!(query, "subject:(urgent) is:unread");
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

        let query = compile_query(&filter);
        assert!(query.is_empty());
    }
}
