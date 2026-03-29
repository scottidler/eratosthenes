use serde::Deserialize;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Label {
    Inbox,
    Important,
    Starred,
    Sent,
    Draft,
    Trash,
    Spam,
    Unread,
    Custom(String),
}

impl Label {
    pub fn new(raw: &str) -> Self {
        let trimmed = raw.trim_start_matches('\\');
        let up = trimmed.to_uppercase();
        match up.as_str() {
            "INBOX" => Label::Inbox,
            "IMPORTANT" => Label::Important,
            "FLAGGED" | "STARRED" => Label::Starred,
            "SENT" => Label::Sent,
            "DRAFT" => Label::Draft,
            "TRASH" => Label::Trash,
            "SPAM" => Label::Spam,
            "UNREAD" => Label::Unread,
            _ => Label::Custom(trimmed.to_string()),
        }
    }

    pub fn to_gmail_id(&self) -> &str {
        match self {
            Label::Inbox => "INBOX",
            Label::Important => "IMPORTANT",
            Label::Starred => "STARRED",
            Label::Sent => "SENT",
            Label::Draft => "DRAFT",
            Label::Trash => "TRASH",
            Label::Spam => "SPAM",
            Label::Unread => "UNREAD",
            Label::Custom(s) => s.as_str(),
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_gmail_id())
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Label::new(&raw))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_label_inbox() {
        assert_eq!(Label::new("INBOX"), Label::Inbox);
        assert_eq!(Label::new("inbox"), Label::Inbox);
        assert_eq!(Label::new("Inbox"), Label::Inbox);
    }

    #[test]
    fn test_label_important() {
        assert_eq!(Label::new("IMPORTANT"), Label::Important);
        assert_eq!(Label::new("important"), Label::Important);
        assert_eq!(Label::new("\\Important"), Label::Important);
    }

    #[test]
    fn test_label_starred() {
        assert_eq!(Label::new("STARRED"), Label::Starred);
        assert_eq!(Label::new("starred"), Label::Starred);
        assert_eq!(Label::new("FLAGGED"), Label::Starred);
        assert_eq!(Label::new("\\Flagged"), Label::Starred);
    }

    #[test]
    fn test_label_unread() {
        assert_eq!(Label::new("UNREAD"), Label::Unread);
        assert_eq!(Label::new("unread"), Label::Unread);
    }

    #[test]
    fn test_label_custom() {
        assert_eq!(Label::new("MyLabel"), Label::Custom("MyLabel".to_string()));
        assert_eq!(Label::new("work/projects"), Label::Custom("work/projects".to_string()));
    }

    #[test]
    fn test_label_strips_backslash() {
        assert_eq!(Label::new("\\Seen"), Label::Custom("Seen".to_string()));
    }

    #[test]
    fn test_label_deserialize() {
        let yaml = "\"INBOX\"";
        let label: Label = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(label, Label::Inbox);

        let yaml2 = "\"CustomLabel\"";
        let label2: Label = serde_yaml::from_str(yaml2).unwrap();
        assert_eq!(label2, Label::Custom("CustomLabel".to_string()));
    }

    #[test]
    fn test_to_gmail_id() {
        assert_eq!(Label::Inbox.to_gmail_id(), "INBOX");
        assert_eq!(Label::Starred.to_gmail_id(), "STARRED");
        assert_eq!(Label::Custom("Purgatory".to_string()).to_gmail_id(), "Purgatory");
    }
}
