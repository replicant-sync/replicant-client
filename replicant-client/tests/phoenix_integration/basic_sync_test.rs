//! Basic sync flow tests: connect, create, update, delete
//!
//! Ported from the original Rust server integration tests.

use super::{serial, skip_if_no_server, TestClient, TEST_EMAIL};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn test_connect_and_authenticate() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await;
    assert!(client.is_ok(), "Failed to connect: {:?}", client.err());
}

#[tokio::test]
#[serial]
async fn test_create_document() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();
    let content = json!({"title": "Test Document", "body": "Hello, World!"});

    let result = client.create_document(content.clone()).await;
    assert!(result.is_ok(), "Create failed: {:?}", result.err());

    let response = result.unwrap();
    assert!(response.get("document_id").is_some());
    assert!(response.get("sync_revision").is_some());
    assert!(response.get("content_hash").is_some());
}

#[tokio::test]
#[serial]
async fn test_create_and_update_document() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Original Title", "count": 0});

    // Create
    let create_result = client.create_document_with_id(doc_id, content).await;
    assert!(
        create_result.is_ok(),
        "Create failed: {:?}",
        create_result.err()
    );

    let create_response = create_result.unwrap();
    let content_hash = create_response
        .get("content_hash")
        .and_then(|v| v.as_str())
        .expect("Missing content_hash");

    // Update
    let patch = json!([{"op": "replace", "path": "/title", "value": "Updated Title"}]);
    let update_result = client.update_document(doc_id, patch, content_hash).await;
    assert!(
        update_result.is_ok(),
        "Update failed: {:?}",
        update_result.err()
    );

    let update_response = update_result.unwrap();
    let new_revision = update_response
        .get("sync_revision")
        .and_then(|v| v.as_i64());
    assert_eq!(new_revision, Some(2), "Revision should be 2 after update");
}

#[tokio::test]
#[serial]
async fn test_create_and_delete_document() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "To Be Deleted"});

    // Create
    let create_result = client.create_document_with_id(doc_id, content).await;
    assert!(
        create_result.is_ok(),
        "Create failed: {:?}",
        create_result.err()
    );

    // Delete
    let delete_result = client.delete_document(doc_id).await;
    assert!(
        delete_result.is_ok(),
        "Delete failed: {:?}",
        delete_result.err()
    );

    // Verify document is not in full sync results
    let sync_result = client.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let found = documents.iter().any(|d| {
        d.get("document_id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    assert!(!found, "Deleted document should not appear in full sync");
}

#[tokio::test]
#[serial]
async fn test_full_sync() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document first
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Sync Test Doc"});
    client
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();

    // Request full sync
    let result = client.request_full_sync().await;
    assert!(result.is_ok(), "Full sync failed: {:?}", result.err());

    let response = result.unwrap();
    assert!(response.get("documents").is_some());
    assert!(response.get("latest_sequence").is_some());

    // Should find our document
    let documents = response
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let found = documents.iter().any(|d| {
        d.get("document_id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    assert!(found, "Created document should appear in full sync");
}

#[tokio::test]
#[serial]
async fn test_get_changes_since() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Get current sequence
    let sync_result = client.request_full_sync().await.unwrap();
    let sequence_before = sync_result
        .get("latest_sequence")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Create a document
    let content = json!({"title": "Changes Test"});
    client.create_document(content).await.unwrap();

    // Get changes since previous sequence
    let result = client.get_changes_since(sequence_before).await;
    assert!(result.is_ok(), "Get changes failed: {:?}", result.err());

    let response = result.unwrap();
    let events = response.get("events").and_then(|v| v.as_array()).unwrap();
    assert!(!events.is_empty(), "Should have at least one change event");

    // Verify the create event
    let create_event = events.iter().find(|e| {
        e.get("event_type")
            .and_then(|v| v.as_str())
            .map(|s| s == "create")
            .unwrap_or(false)
    });
    assert!(create_event.is_some(), "Should have a create event");
}

/// Test from original suite: test_large_document_sync
/// Verifies that large documents sync correctly without corruption
#[tokio::test]
#[serial]
async fn test_large_document_sync() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a large document with 1000 items
    let large_array: Vec<serde_json::Value> = (0..1000)
        .map(|i| {
            json!({
                "index": i,
                "data": format!("Item number {} with some content", i),
                "nested": {
                    "field1": "value1",
                    "field2": i * 2
                }
            })
        })
        .collect();

    let content = json!({
        "title": "Large Document",
        "items": large_array,
        "metadata": {
            "count": 1000,
            "created": chrono::Utc::now().to_rfc3339()
        }
    });

    // Create the large document
    let result = client.create_document(content).await;
    assert!(
        result.is_ok(),
        "Failed to create large document: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(response.get("document_id").is_some());

    // Verify via full sync that the document exists and has correct structure
    let sync_result = client.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();

    let doc_id = response
        .get("document_id")
        .and_then(|v| v.as_str())
        .unwrap();
    let doc = documents.iter().find(|d| {
        d.get("document_id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id)
            .unwrap_or(false)
    });

    assert!(doc.is_some(), "Large document should be in sync results");
    let doc = doc.unwrap();
    let items = doc
        .get("content")
        .and_then(|c| c.get("items"))
        .and_then(|i| i.as_array());
    assert!(items.is_some(), "Document should have items array");
    assert_eq!(items.unwrap().len(), 1000, "Should have all 1000 items");
}

/// Test from original suite: test_array_duplication_bug
/// Verifies that array operations don't duplicate items
#[tokio::test]
#[serial]
async fn test_array_operations_no_duplication() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();
    let doc_id = Uuid::new_v4();

    // Create document with array
    let content = json!({"title": "Array Test Doc", "tags": ["existing"]});
    let create_result = client
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();
    let content_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap();

    // Add item to array via patch
    let patch = json!([{"op": "add", "path": "/tags/-", "value": "new_tag"}]);
    let update_result = client.update_document(doc_id, patch, content_hash).await;
    assert!(
        update_result.is_ok(),
        "Update failed: {:?}",
        update_result.err()
    );

    // Fetch and verify no duplication
    let sync_result = client.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let doc = documents.iter().find(|d| {
        d.get("document_id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });

    assert!(doc.is_some(), "Document should exist");
    let doc = doc.unwrap();
    let tags = doc
        .get("content")
        .and_then(|c| c.get("tags"))
        .and_then(|t| t.as_array())
        .unwrap();

    assert_eq!(tags.len(), 2, "Should have exactly 2 tags, got: {:?}", tags);
    assert_eq!(tags[0], "existing");
    assert_eq!(tags[1], "new_tag");
}
