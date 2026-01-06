//! Multi-client sync tests: broadcasts, real-time updates
//!
//! Ported from the original Rust server integration tests.

use super::{serial, skip_if_no_server, TestClient, TEST_EMAIL};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn test_two_clients_same_user() {
    if skip_if_no_server() {
        return;
    }

    // Connect two clients as the same user
    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Client A creates a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Shared Document", "owner": "client_a"});
    let create_result = client_a.create_document_with_id(doc_id, content).await;
    assert!(
        create_result.is_ok(),
        "Create failed: {:?}",
        create_result.err()
    );

    // Give broadcasts time to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client B should see the document via full sync
    let sync_result = client_b.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let found = documents.iter().any(|d| {
        d.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    assert!(found, "Client B should see document created by Client A");
}

#[tokio::test]
#[serial]
async fn test_update_propagates_to_other_client() {
    if skip_if_no_server() {
        return;
    }

    // Connect two clients
    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Client A creates a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "Original", "version": 1});
    let create_result = client_a
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();
    let content_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap();

    // Client A updates the document
    let patch = json!([{"op": "replace", "path": "/title", "value": "Updated by A"}]);
    client_a
        .update_document(doc_id, patch, content_hash)
        .await
        .unwrap();

    // Give broadcasts time to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client B fetches the document via full sync
    let sync_result = client_b.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let doc = documents.iter().find(|d| {
        d.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });

    assert!(doc.is_some(), "Client B should see the document");
    let doc = doc.unwrap();
    let title = doc
        .get("content")
        .and_then(|c| c.get("title"))
        .and_then(|v| v.as_str());
    assert_eq!(
        title,
        Some("Updated by A"),
        "Client B should see updated title"
    );
}

#[tokio::test]
#[serial]
async fn test_delete_propagates_to_other_client() {
    if skip_if_no_server() {
        return;
    }

    // Connect two clients
    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Client A creates a document
    let doc_id = Uuid::new_v4();
    let content = json!({"title": "To Be Deleted"});
    client_a
        .create_document_with_id(doc_id, content)
        .await
        .unwrap();

    // Verify Client B can see it
    let sync_result = client_b.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let found_before = documents.iter().any(|d| {
        d.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    assert!(found_before, "Client B should initially see the document");

    // Client A deletes the document
    client_a.delete_document(doc_id).await.unwrap();

    // Give broadcasts time to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client B should no longer see it
    let sync_result = client_b.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let found_after = documents.iter().any(|d| {
        d.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    assert!(!found_after, "Client B should not see deleted document");
}

#[tokio::test]
#[serial]
async fn test_incremental_sync_across_clients() {
    if skip_if_no_server() {
        return;
    }

    // Connect two clients
    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Get Client B's current sequence
    let sync_result = client_b.request_full_sync().await.unwrap();
    let sequence_before = sync_result
        .get("latest_sequence")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Client A creates a document
    let content = json!({"title": "Incremental Sync Test"});
    client_a.create_document(content).await.unwrap();

    // Client B gets changes since its last sequence
    let changes_result = client_b.get_changes_since(sequence_before).await.unwrap();
    let events = changes_result
        .get("events")
        .and_then(|v| v.as_array())
        .unwrap();

    assert!(
        !events.is_empty(),
        "Client B should see at least one change event"
    );
}

/// Test from original suite: test_bidirectional_sync
/// Both clients create documents, both should see all documents
#[tokio::test]
#[serial]
async fn test_bidirectional_sync() {
    if skip_if_no_server() {
        return;
    }

    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Both clients create documents
    let doc_a_id = Uuid::new_v4();
    let doc_b_id = Uuid::new_v4();

    client_a
        .create_document_with_id(doc_a_id, json!({"title": "Doc from Client A"}))
        .await
        .unwrap();
    client_b
        .create_document_with_id(doc_b_id, json!({"title": "Doc from Client B"}))
        .await
        .unwrap();

    // Give time to sync
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both should see both documents
    let sync_a = client_a.request_full_sync().await.unwrap();
    let sync_b = client_b.request_full_sync().await.unwrap();

    let docs_a = sync_a.get("documents").and_then(|v| v.as_array()).unwrap();
    let docs_b = sync_b.get("documents").and_then(|v| v.as_array()).unwrap();

    let has_doc_a_id = |docs: &[serde_json::Value]| {
        docs.iter().any(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_a_id.to_string())
                .unwrap_or(false)
        })
    };

    let has_doc_b_id = |docs: &[serde_json::Value]| {
        docs.iter().any(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_b_id.to_string())
                .unwrap_or(false)
        })
    };

    assert!(has_doc_a_id(docs_a), "Client A should see doc_a");
    assert!(has_doc_b_id(docs_a), "Client A should see doc_b");
    assert!(has_doc_a_id(docs_b), "Client B should see doc_a");
    assert!(has_doc_b_id(docs_b), "Client B should see doc_b");
}

/// Test from original suite: test_three_clients_full_crud
/// Three clients perform CRUD operations, all should converge
#[tokio::test]
#[serial]
async fn test_three_clients_full_crud() {
    if skip_if_no_server() {
        return;
    }

    let client_1 = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_2 = TestClient::connect(TEST_EMAIL).await.unwrap();
    let client_3 = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Give clients time to connect
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client 1 creates a document
    let doc_id = Uuid::new_v4();
    let create_result = client_1
        .create_document_with_id(
            doc_id,
            json!({
                "title": "Shared Task",
                "status": "pending",
                "priority": "high"
            }),
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify all clients see the document
    for (name, client) in [
        ("Client 1", &client_1),
        ("Client 2", &client_2),
        ("Client 3", &client_3),
    ] {
        let sync = client.request_full_sync().await.unwrap();
        let docs = sync.get("documents").and_then(|v| v.as_array()).unwrap();
        let found = docs.iter().any(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_id.to_string())
                .unwrap_or(false)
        });
        assert!(found, "{} should see the document after create", name);
    }

    // Client 2 updates the document
    let content_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap();
    client_2
        .update_document(
            doc_id,
            json!([{"op": "replace", "path": "/status", "value": "in_progress"}]),
            content_hash,
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify update propagated
    let sync_3 = client_3.request_full_sync().await.unwrap();
    let docs_3 = sync_3.get("documents").and_then(|v| v.as_array()).unwrap();
    let doc = docs_3.iter().find(|d| {
        d.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == doc_id.to_string())
            .unwrap_or(false)
    });
    let status = doc
        .and_then(|d| d.get("content"))
        .and_then(|c| c.get("status"))
        .and_then(|s| s.as_str());
    assert_eq!(
        status,
        Some("in_progress"),
        "Client 3 should see updated status"
    );

    // Client 3 deletes the document
    client_3.delete_document(doc_id).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify deletion propagated
    for (name, client) in [("Client 1", &client_1), ("Client 2", &client_2)] {
        let sync = client.request_full_sync().await.unwrap();
        let docs = sync.get("documents").and_then(|v| v.as_array()).unwrap();
        let found = docs.iter().any(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_id.to_string())
                .unwrap_or(false)
        });
        assert!(!found, "{} should not see deleted document", name);
    }
}

/// Test from original suite: test_no_duplicate_broadcast_to_sender
/// Sender should not receive their own broadcast back
#[tokio::test]
#[serial]
async fn test_no_duplicate_broadcast_to_sender() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a document
    let doc_id = Uuid::new_v4();
    client
        .create_document_with_id(doc_id, json!({"title": "Test No Duplicates"}))
        .await
        .unwrap();

    // Wait for any potential duplicate broadcasts
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify only one document exists
    let sync = client.request_full_sync().await.unwrap();
    let docs = sync.get("documents").and_then(|v| v.as_array()).unwrap();

    // Filter to only docs with our specific ID (avoid interference from other tests)
    let matching_docs: Vec<_> = docs
        .iter()
        .filter(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_id.to_string())
                .unwrap_or(false)
        })
        .collect();

    assert_eq!(
        matching_docs.len(),
        1,
        "Should have exactly 1 document with our ID, not duplicates"
    );
}
