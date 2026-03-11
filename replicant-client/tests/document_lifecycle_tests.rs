//! # Document Lifecycle Tests
//!
//! Tests for the core document lifecycle: creation, retrieval, and deletion.
//! These tests verify that documents transition through sync states correctly.
//!
//! Tests cover:
//! - New local documents start as pending
//! - Pending documents can be queried
//! - Delete operations mark documents as pending until synced

mod common;

use common::*;
use replicant_core::models::Document;
use uuid::Uuid;

/// Verifies that newly created local documents start with "pending" sync status.
/// This is critical for offline-first functionality - documents created while
/// offline must be tracked for later sync.
#[tokio::test]
async fn test_new_local_documents_are_pending() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();
    let doc = make_document(user_id, "Local Document", "Local content", 1);

    // Save without specifying status (should default to pending)
    db.save_document(&doc).await.unwrap();

    let sync_status = get_sync_status(&db, doc.id).await;
    assert_eq!(
        sync_status, "pending",
        "New local documents should be marked as pending"
    );
}

/// Verifies that the get_pending_documents query correctly filters and retrieves
/// only documents with pending sync status.
#[tokio::test]
async fn test_pending_documents_can_be_retrieved() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();
    let mut doc_ids = Vec::new();

    // Create mix of pending and synced documents
    for i in 0..5 {
        let doc = make_document(
            user_id,
            &format!("Document {}", i),
            &format!("Content {}", i),
            1,
        );

        doc_ids.push(doc.id);

        // Save every other document as synced, rest as pending
        if i % 2 == 0 {
            db.save_document(&doc).await.unwrap();
            db.mark_synced(&doc.id).await.unwrap();
        } else {
            db.save_document(&doc).await.unwrap(); // defaults to pending
        }
    }

    // Get pending documents
    let pending_docs = db.get_pending_documents().await.unwrap();

    // Should have 2 pending documents (indices 1, 3)
    assert_eq!(pending_docs.len(), 2, "Should have 2 pending documents");

    // Verify the pending ones are the odd indices
    assert!(
        pending_docs.iter().any(|p| p.id == doc_ids[1]),
        "Document 1 should be pending"
    );
    assert!(
        pending_docs.iter().any(|p| p.id == doc_ids[3]),
        "Document 3 should be pending"
    );
    assert!(
        !pending_docs.iter().any(|p| p.id == doc_ids[0]),
        "Document 0 should not be pending"
    );
    assert!(
        !pending_docs.iter().any(|p| p.id == doc_ids[2]),
        "Document 2 should not be pending"
    );
    assert!(
        !pending_docs.iter().any(|p| p.id == doc_ids[4]),
        "Document 4 should not be pending"
    );
}

/// Verifies the complete deletion workflow:
/// 1. Document starts synced
/// 2. Delete operation marks it pending
/// 3. After server confirmation, it's marked synced again
///
/// This ensures deletes are tracked for sync just like creates and updates.
#[tokio::test]
async fn test_delete_marks_document_as_pending_then_synced() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();
    let doc = make_document(user_id, "Test Document", "Test content", 1);

    // Create and sync a document
    db.save_document(&doc).await.unwrap();
    db.mark_synced(&doc.id).await.unwrap();

    // Verify it's synced
    let sync_status = get_sync_status(&db, doc.id).await;
    assert_eq!(sync_status, "synced", "Document should start as synced");

    // Delete the document
    db.delete_document(&doc.id).await.unwrap();

    // Verify deletion marks it as pending
    let sync_status = get_sync_status(&db, doc.id).await;
    assert_eq!(
        sync_status, "pending",
        "Deleted document should be marked as pending sync"
    );

    // Simulate server confirmation
    db.mark_synced(&doc.id).await.unwrap();

    // Verify it's now synced
    let sync_status = get_sync_status(&db, doc.id).await;
    assert_eq!(
        sync_status, "synced",
        "Deleted document should be marked as synced after confirmation"
    );
}

#[tokio::test]
async fn test_count_documents() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();
    let doc = make_document(user_id, "Test Document", "Test content", 1);
    let doc1 = make_document(user_id, "Test Document 1", "Test content 1", 1);
    let doc2 = make_document(user_id, "Test Document 2", "Test content 2", 1);
    // Create and sync a document
    db.save_document(&doc).await.unwrap();
    db.mark_synced(&doc.id).await.unwrap();
    assert_eq!(db.count_documents().await.unwrap(), 1);
    db.save_document(&doc1).await.unwrap();
    assert_eq!(db.count_documents().await.unwrap(), 2);
    db.save_document(&doc2).await.unwrap();
    assert_eq!(db.count_documents().await.unwrap(), 3);
}

#[tokio::test]
async fn test_count_documents_excludes_deleted() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();

    // Create 3 documents
    let doc1 = make_document(user_id, "Document 1", "Content 1", 1);
    let doc2 = make_document(user_id, "Document 2", "Content 2", 1);
    let doc3 = make_document(user_id, "Document 3", "Content 3", 1);

    db.save_document(&doc1).await.unwrap();
    db.save_document(&doc2).await.unwrap();
    db.save_document(&doc3).await.unwrap();

    assert_eq!(db.count_documents().await.unwrap(), 3);

    // Delete one document
    db.delete_document(&doc2.id).await.unwrap();
    assert_eq!(
        db.count_documents().await.unwrap(),
        2,
        "Deleted documents should not be included in count"
    );

    // Delete another
    db.delete_document(&doc1.id).await.unwrap();
    assert_eq!(
        db.count_documents().await.unwrap(),
        1,
        "Count should decrease as documents are deleted"
    );

    // Delete the last one
    db.delete_document(&doc3.id).await.unwrap();
    assert_eq!(
        db.count_documents().await.unwrap(),
        0,
        "Count should be 0 when all documents are deleted"
    );
}

/// Verifies that the title field is properly extracted from document content
/// when documents are saved to the client database.
#[tokio::test]
async fn test_title_extraction() {
    let db = setup_test_db().await;
    let user_id = Uuid::new_v4();

    // Test 1: Document with title in content
    let doc_with_title = make_document(user_id, "My Document", "Test content", 1);
    db.save_document(&doc_with_title).await.unwrap();

    let retrieved = db.get_document(&doc_with_title.id).await.unwrap();
    assert_eq!(retrieved.title, Some("My Document".to_string()));

    // Test 2: Document without title (should use datetime fallback)
    let content_no_title = serde_json::json!({"text": "No title content"});
    let doc_no_title = Document {
        id: Uuid::new_v4(),
        user_id: Some(user_id),
        content: content_no_title,
        sync_revision: 1,
        content_hash: None,
        title: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
    };

    db.save_document(&doc_no_title).await.unwrap();

    let retrieved = db.get_document(&doc_no_title.id).await.unwrap();
    assert!(retrieved.title.is_some());
    let title = retrieved.title.unwrap();
    // Should have datetime format: YYYY-MM-DD|HH:MM:SS.mmm
    assert!(title.contains('|'), "Title should contain pipe separator");
    assert!(title.contains('-'), "Title should contain date separator");

    // Test 3: Very long title (should be truncated to 128 chars)
    let long_title = "a".repeat(200);
    let doc_long_title = make_document(user_id, &long_title, "Long title content", 1);
    db.save_document(&doc_long_title).await.unwrap();

    let retrieved = db.get_document(&doc_long_title.id).await.unwrap();
    assert_eq!(retrieved.title.as_ref().unwrap().len(), 128);
    assert_eq!(retrieved.title, Some("a".repeat(128)));
}
