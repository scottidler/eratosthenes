use eyre::{Context, Result};
use std::collections::HashMap;

pub struct LabelResolver {
    name_to_id: HashMap<String, String>,
    id_to_name: HashMap<String, String>,
}

const SYSTEM_LABELS: &[(&str, &str)] = &[
    ("INBOX", "INBOX"),
    ("STARRED", "STARRED"),
    ("IMPORTANT", "IMPORTANT"),
    ("UNREAD", "UNREAD"),
    ("SENT", "SENT"),
    ("DRAFT", "DRAFT"),
    ("TRASH", "TRASH"),
    ("SPAM", "SPAM"),
    ("CATEGORY_PERSONAL", "CATEGORY_PERSONAL"),
    ("CATEGORY_SOCIAL", "CATEGORY_SOCIAL"),
    ("CATEGORY_PROMOTIONS", "CATEGORY_PROMOTIONS"),
    ("CATEGORY_UPDATES", "CATEGORY_UPDATES"),
    ("CATEGORY_FORUMS", "CATEGORY_FORUMS"),
];

impl LabelResolver {
    pub fn from_api_labels(labels: Vec<google_gmail1::api::Label>) -> Self {
        let mut name_to_id = HashMap::new();
        let mut id_to_name = HashMap::new();

        for (name, id) in SYSTEM_LABELS {
            name_to_id.insert(name.to_string(), id.to_string());
            id_to_name.insert(id.to_string(), name.to_string());
        }

        for label in labels {
            if let (Some(id), Some(name)) = (label.id, label.name) {
                name_to_id.insert(name.clone(), id.clone());
                id_to_name.insert(id, name);
            }
        }

        Self {
            name_to_id,
            id_to_name,
        }
    }

    pub fn resolve_name(&self, name: &str) -> Option<&str> {
        self.name_to_id.get(name).map(|s| s.as_str())
    }

    pub fn resolve_id(&self, id: &str) -> Option<&str> {
        self.id_to_name.get(id).map(|s| s.as_str())
    }

    pub fn ensure_label(&mut self, name: &str, id: String) {
        self.name_to_id.insert(name.to_string(), id.clone());
        self.id_to_name.insert(id, name.to_string());
    }
}

/// Where a created label shows up in Gmail's UI. Destinations (Bots, Purgatory) are
/// `Shown`; the marker is `Hidden`, or it puts a chip on nearly every message and a row
/// in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LabelVisibility {
    Shown,
    Hidden,
}

impl LabelVisibility {
    fn label_list(&self) -> &'static str {
        match self {
            LabelVisibility::Shown => "labelShow",
            LabelVisibility::Hidden => "labelHide",
        }
    }

    fn message_list(&self) -> &'static str {
        match self {
            LabelVisibility::Shown => "show",
            LabelVisibility::Hidden => "hide",
        }
    }
}

pub async fn create_label_if_missing(
    hub: &google_gmail1::Gmail<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    >,
    resolver: &mut LabelResolver,
    name: &str,
    visibility: LabelVisibility,
) -> Result<String> {
    if let Some(id) = resolver.resolve_name(name) {
        return Ok(id.to_string());
    }

    log::info!("Creating missing label: {} ({:?})", name, visibility);
    let label = google_gmail1::api::Label {
        name: Some(name.to_string()),
        label_list_visibility: Some(visibility.label_list().to_string()),
        message_list_visibility: Some(visibility.message_list().to_string()),
        ..Default::default()
    };

    let (_, created) = hub
        .users()
        .labels_create(label, "me")
        .add_scope(crate::gmail::auth::GMAIL_SCOPE)
        .doit()
        .await
        .context(format!("Failed to create label '{}'", name))?;

    let id = created
        .id
        .ok_or_else(|| eyre::eyre!("Created label '{}' has no ID", name))?;

    resolver.ensure_label(name, id.clone());
    Ok(id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_system_labels_resolved() {
        let resolver = LabelResolver::from_api_labels(vec![]);
        assert_eq!(resolver.resolve_name("INBOX"), Some("INBOX"));
        assert_eq!(resolver.resolve_name("STARRED"), Some("STARRED"));
        assert_eq!(resolver.resolve_name("UNREAD"), Some("UNREAD"));
    }

    #[test]
    fn test_custom_label_resolved() {
        let label = google_gmail1::api::Label {
            id: Some("Label_123".to_string()),
            name: Some("Purgatory".to_string()),
            ..Default::default()
        };

        let resolver = LabelResolver::from_api_labels(vec![label]);
        assert_eq!(resolver.resolve_name("Purgatory"), Some("Label_123"));
        assert_eq!(resolver.resolve_id("Label_123"), Some("Purgatory"));
    }

    #[test]
    fn test_unknown_label_returns_none() {
        let resolver = LabelResolver::from_api_labels(vec![]);
        assert_eq!(resolver.resolve_name("NonExistent"), None);
    }

    /// The marker must be created hidden; destinations stay shown.
    #[test]
    fn test_label_visibility_strings() {
        assert_eq!(LabelVisibility::Shown.label_list(), "labelShow");
        assert_eq!(LabelVisibility::Shown.message_list(), "show");
        assert_eq!(LabelVisibility::Hidden.label_list(), "labelHide");
        assert_eq!(LabelVisibility::Hidden.message_list(), "hide");
    }

    #[test]
    fn test_ensure_label() {
        let mut resolver = LabelResolver::from_api_labels(vec![]);
        assert!(resolver.resolve_name("NewLabel").is_none());

        resolver.ensure_label("NewLabel", "Label_456".to_string());
        assert_eq!(resolver.resolve_name("NewLabel"), Some("Label_456"));
        assert_eq!(resolver.resolve_id("Label_456"), Some("NewLabel"));
    }
}
