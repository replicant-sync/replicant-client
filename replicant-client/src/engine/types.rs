use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn change_round_trips_through_json() {
        let change = Change {
            scope: SCOPE_OWN.to_string(),
            seq: 12,
            prev_seq: 9,
            doc_id: Uuid::from_u128(1),
            kind: ChangeKind::Upsert,
            doc: Some(DocEnvelope {
                doc_id: Uuid::from_u128(1),
                owner_id: Some(Uuid::from_u128(7)),
                author_id: None,
                read_only: false,
                source_doc_id: None,
                derived_from: None,
                title: Some("Just".into()),
                content: json!({"title": "Just"}),
                hash: "abc".into(),
                seq: 12,
            }),
            client_id: Some(Uuid::from_u128(3)),
            upload_id: None,
        };
        let text = serde_json::to_string(&change).unwrap();
        assert!(text.contains("\"kind\":\"upsert\""));
        let back: Change = serde_json::from_str(&text).unwrap();
        assert_eq!(back, change);
    }

    #[test]
    fn server_error_new_defaults_to_transient() {
        let e = ServerError::new("internal");
        assert_eq!(e.code, "internal");
        assert!(!e.is_fatal);
        assert_eq!(e.retry_after_ms, None);
    }
}

pub type Seq = i64;
pub type Scope = String;

pub const SCOPE_OWN: &str = "own";
pub const SCOPE_CURATED: &str = "collection:curated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Upsert,
    /// The document was deleted; sent in every scope it belonged to.
    Delete,
    /// The document left this scope only.
    Leave,
}

/// Document as sent by the server in changes, snapshots, fetches and upload replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocEnvelope {
    pub doc_id: Uuid,
    pub owner_id: Option<Uuid>,
    pub author_id: Option<Uuid>,
    pub read_only: bool,
    pub source_doc_id: Option<Uuid>,
    pub derived_from: Option<Uuid>,
    pub title: Option<String>,
    pub content: Value,
    pub hash: String,
    pub seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub scope: Scope,
    pub seq: Seq,
    pub prev_seq: Seq,
    pub doc_id: Uuid,
    pub kind: ChangeKind,
    /// Present for `Upsert`.
    pub doc: Option<DocEnvelope>,
    pub client_id: Option<Uuid>,
    pub upload_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadKind {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub upload_id: Uuid,
    pub doc_id: Uuid,
    pub kind: UploadKind,
    /// Required for `Update`, `None` for `Create` and `Delete`.
    pub base_hash: Option<String>,
    /// Full content for `Create`, serialized JSON Patch for `Update`, `Null` for `Delete`.
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServerError {
    pub code: String,
    #[serde(default)]
    pub is_fatal: bool,
    pub retry_after_ms: Option<u64>,
    /// Set on `hash_mismatch`.
    pub current_hash: Option<String>,
    pub current_seq: Option<Seq>,
    /// Set on a rejected create (`exists`).
    pub existing_owner: Option<Uuid>,
    /// Set on `clock_skew`: the server's clock, unix seconds.
    pub server_time: Option<i64>,
}

impl ServerError {
    pub fn new(code: &str) -> Self {
        ServerError {
            code: code.to_string(),
            is_fatal: false,
            retry_after_ms: None,
            current_hash: None,
            current_seq: None,
            existing_owner: None,
            server_time: None,
        }
    }
}
