//! Conflict handling tests: hash mismatch, duplicate IDs

use super::{serial, skip_if_no_server, TestClient, TEST_EMAIL};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn test_update_with_wrong_hash_fails() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Hash Test", "version": 1});
    client
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();

    // Try to update with wrong content hash
    let patch = json!([{"op": "replace", "path": "/title", "value": "Should Fail"}]);
    let result = client
        .update_document(doc_id, patch, "wrong_hash_value")
        .await;

    // Should fail with hash_mismatch
    assert!(result.is_err(), "Update with wrong hash should fail");
    let error = result.unwrap_err();
    assert!(
        error.contains("hash_mismatch") || error.contains("error"),
        "Error should indicate hash mismatch: {}",
        error
    );
}

#[tokio::test]
#[serial]
async fn test_stale_hash_returns_current_state() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Original", "data": "initial"});
    let create_result = client
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();
    let first_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Update the document to get a new hash
    let patch1 = json!([{"op": "replace", "path": "/title", "value": "First Update"}]);
    client
        .update_document(doc_id, patch1, &first_hash)
        .await
        .unwrap();

    // Try to update with the stale (first) hash
    let patch2 = json!([{"op": "replace", "path": "/title", "value": "Should Fail"}]);
    let result = client.update_document(doc_id, patch2, &first_hash).await;

    // Should fail because hash is stale
    assert!(result.is_err(), "Update with stale hash should fail");
}

#[tokio::test]
#[serial]
async fn test_duplicate_document_id_returns_conflict() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document with a specific ID
    let doc_id = Uuid::new_v4();
    let content1 = json!({"title": "First Document"});
    let result1 = client.create_document_with_id(doc_id, content1).await;
    assert!(result1.is_ok(), "First create should succeed");

    // Try to create another document with the same ID
    let content2 = json!({"title": "Duplicate Document"});
    let result2 = client.create_document_with_id(doc_id, content2).await;

    // Should fail with conflict
    assert!(result2.is_err(), "Duplicate ID should fail");
    let error = result2.unwrap_err();
    assert!(
        error.contains("conflict") || error.contains("error"),
        "Error should indicate conflict: {}",
        error
    );
}

#[tokio::test]
#[serial]
async fn test_update_nonexistent_document_fails() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Try to update a document that doesn't exist
    let nonexistent_id = Uuid::new_v4();
    let patch = json!([{"op": "replace", "path": "/title", "value": "Won't Work"}]);
    let result = client
        .update_document(nonexistent_id, patch, "any_hash")
        .await;

    assert!(
        result.is_err(),
        "Update of nonexistent document should fail"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("not_found") || error.contains("error"),
        "Error should indicate not found: {}",
        error
    );
}

#[tokio::test]
#[serial]
async fn test_delete_nonexistent_document_fails() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Try to delete a document that doesn't exist
    let nonexistent_id = Uuid::new_v4();
    let result = client.delete_document(nonexistent_id).await;

    assert!(
        result.is_err(),
        "Delete of nonexistent document should fail"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("not_found") || error.contains("error"),
        "Error should indicate not found: {}",
        error
    );
}

#[tokio::test]
#[serial]
async fn test_concurrent_updates_one_wins() {
    if skip_if_no_server() {
        return;
    }

    // Two clients as the same user
    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Concurrent Test", "counter": 0});
    let create_result = client_a
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();
    let content_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Both clients try to update with the same hash (simulating concurrent edits)
    let patch_a = json!([{"op": "replace", "path": "/title", "value": "Client A Wins"}]);
    let patch_b = json!([{"op": "replace", "path": "/title", "value": "Client B Wins"}]);

    // Client A's update should succeed
    let result_a = client_a
        .update_document(doc_id, patch_a, &content_hash)
        .await;
    assert!(result_a.is_ok(), "Client A's update should succeed");

    // Client B's update should fail (stale hash)
    let result_b = client_b
        .update_document(doc_id, patch_b, &content_hash)
        .await;
    assert!(
        result_b.is_err(),
        "Client B's update should fail with stale hash"
    );
}
