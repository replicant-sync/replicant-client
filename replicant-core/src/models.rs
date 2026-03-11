use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub content: serde_json::Value,
    pub sync_revision: i64,
    pub content_hash: Option<String>, // SHA256 hash for integrity verification
    pub title: Option<String>,        // Derived from content['title'] for query performance
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Document {
    /// Get the title from the content JSON, if present
    pub fn title(&self) -> Option<&str> {
        self.content.get("title").and_then(|v| v.as_str())
    }

    /// Get the title from content JSON, or return a default
    pub fn title_or_default(&self) -> &str {
        self.title().unwrap_or("Untitled")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_title_helpers() {
        // Test document with title
        let doc_with_title = Document {
            id: Uuid::new_v4(),
            user_id: Some(Uuid::new_v4()),
            content: serde_json::json!({"title": "My Document", "test": true}),
            sync_revision: 1,
            content_hash: None,
            title: Some("My Document".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        };

        assert_eq!(doc_with_title.title(), Some("My Document"));
        assert_eq!(doc_with_title.title_or_default(), "My Document");

        // Test document without title
        let doc_without_title = Document {
            id: Uuid::new_v4(),
            user_id: Some(Uuid::new_v4()),
            content: serde_json::json!({"test": true}),
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        };

        assert_eq!(doc_without_title.title(), None);
        assert_eq!(doc_without_title.title_or_default(), "Untitled");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentPatch {
    pub document_id: Uuid,
    pub patch: json_patch::Patch,
    pub content_hash: String, // SHA256 hash for integrity verification
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SyncStatus {
    Synced,
    Pending,
    Conflict,
}
