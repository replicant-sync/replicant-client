use chrono::{DateTime, Utc};
use replicant_core::{
    models::{Document, SyncStatus},
    SyncResult,
};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use uuid::Uuid;

/// Type alias for document parameters tuple
pub type DocumentParams = (
    String,         // id
    Option<String>, // user_id
    String,         // content
    i64,            // sync_revision
    String,         // created_at
    String,         // updated_at
    Option<String>, // deleted_at
    String,         // sync_status
    String,         // title
);

/// SQL queries for client database operations
pub struct Queries;

impl Queries {
    /// Create the client database schema
    pub const SCHEMA: &'static str = r#"
        CREATE TABLE IF NOT EXISTS user_config (
            user_id TEXT PRIMARY KEY,
            server_url TEXT NOT NULL,
            last_sync_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS documents (
            id TEXT PRIMARY KEY,
            user_id TEXT,
            content JSON NOT NULL,
            sync_revision INTEGER NOT NULL DEFAULT 1,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            deleted_at TIMESTAMP,
            local_changes JSON,
            sync_status TEXT DEFAULT 'pending',
            CHECK (sync_status IN ('synced', 'pending', 'conflict'))
        );
        
        CREATE TABLE IF NOT EXISTS sync_queue (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            document_id TEXT NOT NULL,
            operation_type TEXT NOT NULL,
            patch JSON,
            old_content_hash TEXT,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            retry_count INTEGER DEFAULT 0,
            FOREIGN KEY (document_id) REFERENCES documents(id),
            CHECK (operation_type IN ('create', 'update', 'delete'))
        );
        
        CREATE INDEX IF NOT EXISTS idx_documents_user_id ON documents(user_id);
        CREATE INDEX IF NOT EXISTS idx_documents_sync_status ON documents(sync_status);
        CREATE INDEX IF NOT EXISTS idx_sync_queue_created_at ON sync_queue(created_at);
    "#;

    // User config queries
    pub const GET_USER_ID: &'static str = "SELECT user_id FROM user_config LIMIT 1";

    pub const GET_CLIENT_ID: &'static str = "SELECT client_id FROM user_config LIMIT 1";

    pub const GET_USER_AND_CLIENT_ID: &'static str =
        "SELECT user_id, client_id FROM user_config LIMIT 1";

    pub const INSERT_USER_CONFIG: &'static str =
        "INSERT INTO user_config (user_id, client_id, server_url) VALUES (?1, ?2, ?3)";

    pub const UPDATE_LAST_SYNC: &'static str =
        "UPDATE user_config SET last_sync_at = ?1 WHERE user_id = ?2";

    // Document queries
    pub const GET_DOCUMENT: &'static str = r#"
        SELECT id, user_id, content, sync_revision,
               created_at, updated_at, deleted_at, title
        FROM documents
        WHERE id = ?1
    "#;

    pub const UPSERT_DOCUMENT: &'static str = r#"
        INSERT INTO documents (
            id, user_id, content, sync_revision,
            created_at, updated_at, deleted_at, sync_status, title
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(id) DO UPDATE SET
            content = excluded.content,
            sync_revision = excluded.sync_revision,
            updated_at = excluded.updated_at,
            deleted_at = excluded.deleted_at,
            sync_status = excluded.sync_status,
            title = excluded.title
    "#;

    pub const LIST_USER_DOCUMENTS: &'static str = r#"
        SELECT id, sync_status, updated_at 
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
    "#;

    pub const GET_PENDING_DOCUMENTS: &'static str = r#"
        SELECT id, deleted_at FROM documents
        WHERE sync_status = ?
        ORDER BY updated_at ASC
    "#;

    pub const MARK_DOCUMENT_SYNCED: &'static str = r#"
        UPDATE documents
        SET sync_status = ?
        WHERE id = ?
    "#;

    pub const UPDATE_SYNC_STATUS: &'static str =
        "UPDATE documents SET sync_status = ?2 WHERE id = ?1";

    pub const COUNT_BY_SYNC_STATUS: &'static str =
        "SELECT COUNT(*) as count FROM documents WHERE sync_status = ?1";

    // Sync queue queries
    pub const INSERT_SYNC_QUEUE: &'static str = r#"
        INSERT INTO sync_queue (document_id, operation_type, patch)
        VALUES (?1, ?2, ?3)
    "#;

    pub const GET_SYNC_QUEUE: &'static str = r#"
        SELECT id, document_id, operation_type, patch, retry_count
        FROM sync_queue
        ORDER BY created_at ASC
        LIMIT 100
    "#;

    pub const DELETE_FROM_QUEUE: &'static str = "DELETE FROM sync_queue WHERE id = ?1";

    pub const INCREMENT_RETRY_COUNT: &'static str =
        "UPDATE sync_queue SET retry_count = retry_count + 1 WHERE id = ?1";

    // FTS (Full-Text Search) queries
    pub const HAS_SEARCH_CONFIG: &'static str =
        "SELECT EXISTS(SELECT 1 FROM search_config LIMIT 1)";

    pub const CLEAR_SEARCH_CONFIG: &'static str = "DELETE FROM search_config";

    pub const INSERT_SEARCH_PATH: &'static str = "INSERT INTO search_config (json_path) VALUES (?)";

    pub const DELETE_FTS_ENTRY: &'static str = "DELETE FROM documents_fts WHERE document_id = ?";

    pub const UPDATE_FTS_ENTRY: &'static str = r#"
        INSERT INTO documents_fts (document_id, title, body)
        SELECT
            d.id,
            COALESCE(d.title, ''),
            COALESCE(
                (SELECT GROUP_CONCAT(json_extract(d.content, sc.json_path), ' ')
                 FROM search_config sc
                 WHERE json_extract(d.content, sc.json_path) IS NOT NULL),
                ''
            )
        FROM documents d
        WHERE d.id = ? AND d.deleted_at IS NULL
    "#;

    pub const REBUILD_FTS_INDEX: &'static str = r#"
        INSERT INTO documents_fts (document_id, title, body)
        SELECT
            d.id,
            COALESCE(d.title, ''),
            COALESCE(
                (SELECT GROUP_CONCAT(json_extract(d.content, sc.json_path), ' ')
                 FROM search_config sc
                 WHERE json_extract(d.content, sc.json_path) IS NOT NULL),
                ''
            )
        FROM documents d
        WHERE d.deleted_at IS NULL
    "#;

    pub const CLEAR_FTS_INDEX: &'static str = "DELETE FROM documents_fts";

    pub const SEARCH_DOCUMENTS: &'static str = r#"
        SELECT d.id, d.user_id, d.content, d.sync_revision,
               d.created_at, d.updated_at, d.deleted_at, d.title
        FROM documents d
        JOIN documents_fts fts ON d.id = fts.document_id
        WHERE d.deleted_at IS NULL
          AND documents_fts MATCH ?
        ORDER BY rank
        LIMIT ?
    "#;
}

/// Helper functions for common database operations
pub struct DbHelpers;

impl DbHelpers {
    /// Initialize the database schema
    pub async fn init_schema(pool: &SqlitePool) -> SyncResult<()> {
        sqlx::query(Queries::SCHEMA).execute(pool).await?;
        Ok(())
    }

    /// Parse a document from a database row
    pub fn parse_document(row: &SqliteRow) -> SyncResult<Document> {
        let id: String = row.get("id");
        let user_id: Option<String> = row.get("user_id");
        let content: String = row.get("content");
        let sync_revision: i64 = row
            .try_get::<i64, _>("sync_revision")
            .or_else(|_| row.try_get::<i32, _>("sync_revision").map(|v| v as i64))?;
        let created_at: String = row.get("created_at");
        let updated_at: String = row.get("updated_at");
        let deleted_at: Option<String> = row.get("deleted_at");
        let title: Option<String> = row.try_get("title").ok();

        Ok(Document {
            id: Uuid::parse_str(&id)?,
            user_id: user_id.map(|s| Uuid::parse_str(&s)).transpose()?,
            content: serde_json::from_str(&content)?,
            sync_revision,
            content_hash: None, // Not stored in client database
            title,
            created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
            deleted_at: deleted_at
                .and_then(|dt| DateTime::parse_from_rfc3339(&dt).ok())
                .map(|dt| dt.with_timezone(&Utc)),
        })
    }

    /// Prepare document values for database insertion
    pub fn document_to_params(
        doc: &Document,
        sync_status: Option<SyncStatus>,
    ) -> SyncResult<DocumentParams> {
        let status = sync_status.unwrap_or(SyncStatus::Pending).to_string();
        let title = doc.title.clone().unwrap_or_else(|| {
            // Fallback to extracting from content if not set
            doc.content
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(128).collect::<String>())
                .unwrap_or_else(|| doc.created_at.format("%Y-%m-%d|%H:%M:%S%.3f").to_string())
        });

        Ok((
            doc.id.to_string(),
            doc.user_id.map(|id| id.to_string()),
            serde_json::to_string(&doc.content)?,
            doc.sync_revision,
            doc.created_at.to_rfc3339(),
            doc.updated_at.to_rfc3339(),
            doc.deleted_at.map(|dt| dt.to_rfc3339()),
            status,
            title,
        ))
    }

    /// Get count of documents by sync status
    pub async fn count_by_status(pool: &SqlitePool, status: SyncStatus) -> SyncResult<i64> {
        let row = sqlx::query(Queries::COUNT_BY_SYNC_STATUS)
            .bind(status.to_string())
            .fetch_one(pool)
            .await?;

        Ok(row.try_get("count")?)
    }
}
