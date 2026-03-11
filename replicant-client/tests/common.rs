use chrono::Utc;
use replicant_client::ClientDatabase;
use replicant_core::models::Document;
use uuid::Uuid;

/// Creates a new in-memory test sqlite database and runs migrations.
#[allow(dead_code)]
pub async fn setup_test_db() -> ClientDatabase {
    let db_url = "sqlite::memory:";
    let db = ClientDatabase::new(db_url).await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

/// Creates a sample document for a given user.
#[allow(dead_code)]
pub fn make_document(user_id: Uuid, title: &str, text: &str, sync_revision: i64) -> Document {
    let content = serde_json::json!({
        "title": title,
        "text": text
    });

    Document {
        id: Uuid::new_v4(),
        user_id: Some(user_id),
        content: content.clone(),
        sync_revision,
        content_hash: None,
        title: None, // Will be extracted when saved to database
        created_at: Utc::now(),
        updated_at: Utc::now(),
        deleted_at: None,
    }
}

/// Helper to get the sync_status for a document.
#[allow(dead_code)]
pub async fn get_sync_status(db: &ClientDatabase, doc_id: Uuid) -> String {
    use sqlx::Row;
    sqlx::query("SELECT sync_status FROM documents WHERE id = ?")
        .bind(doc_id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get::<String, _>(0)
}
