//! Basic sync flow tests: connect, create, update, delete
//!
//! Ported from the original Rust server integration tests.

use super::{
    connect_subject, open_offline_subject, remove_temp_db, serial, skip_if_no_server, temp_db_path,
    TestClient, TEST_EMAIL,
};
use serde_json::json;
use std::time::Duration;
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
    assert!(response.get("id").is_some());
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
        d.get("id")
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
        d.get("id")
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
    assert!(response.get("id").is_some());

    // Verify via full sync that the document exists and has correct structure
    let sync_result = client.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();

    let doc_id = response.get("id").and_then(|v| v.as_str()).unwrap();
    let doc = documents.iter().find(|d| {
        d.get("id")
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
        d.get("id")
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

/// Poll the subject's local database until the document settles as synced.
async fn wait_for_local_synced(db_url: &str, id: Uuid) -> bool {
    let db = replicant_client::ClientDatabase::new(db_url).await.unwrap();
    for _ in 0..100 {
        if let Ok(Some(replicant_core::models::SyncStatus::Synced)) = db.get_sync_status(&id).await
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Three edits made while offline must reach the server as ONE patch against
/// the revision the server actually holds. Each edit adds a distinct key, so
/// uploading only the newest fragment would land the last key and silently drop
/// the other two — and would be rejected as a `hash_mismatch` first, costing an
/// extra round trip.
#[tokio::test]
#[serial]
async fn offline_edits_flush_as_one_cumulative_patch() {
    if skip_if_no_server() {
        return;
    }

    std::fs::create_dir_all("databases").ok();
    let db_file = temp_db_path("offline_flush");
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    let driver = TestClient::connect(TEST_EMAIL).await.unwrap();
    let doc_id = Uuid::new_v4();
    let base = json!({"title": "base"});

    let created_revision = {
        let subject = connect_subject(&db_url).await;
        subject
            .create_document_with_id(doc_id, base.clone())
            .await
            .unwrap();
        assert!(
            wait_for_local_synced(&db_url, doc_id).await,
            "the created document should be acknowledged before going offline"
        );
        let created = driver.get_document(doc_id).await.unwrap();
        created
            .get("sync_revision")
            .and_then(|v| v.as_i64())
            .expect("server holds the created document")
    };

    // Offline: three edits, each building on the previous one.
    let final_content = json!({"title": "base", "a": 1, "b": 2, "c": 3});
    {
        let offline = open_offline_subject(&db_url).await;
        offline
            .update_document(doc_id, json!({"title": "base", "a": 1}))
            .await
            .unwrap();
        offline
            .update_document(doc_id, json!({"title": "base", "a": 1, "b": 2}))
            .await
            .unwrap();
        offline
            .update_document(doc_id, final_content.clone())
            .await
            .unwrap();
    }

    // Reconnecting flushes the queue; the subject settles once the server acks.
    let subject = connect_subject(&db_url).await;
    assert!(
        wait_for_local_synced(&db_url, doc_id).await,
        "the flushed edits should be acknowledged, not left pending"
    );

    let server_doc = driver.get_document(doc_id).await.unwrap();
    assert_eq!(
        server_doc.get("content"),
        Some(&final_content),
        "the flush must carry all three offline edits, not the newest fragment"
    );
    assert_eq!(
        server_doc.get("sync_revision").and_then(|v| v.as_i64()),
        Some(created_revision + 1),
        "three offline edits must cost exactly one server revision"
    );

    drop(subject);
    remove_temp_db(&db_file);
}

/// A document created while offline and then edited offline has never reached
/// the server, so its flush must be a CREATE carrying the final content.
/// Uploading it as an update draws `not_found` — the document then sits Pending
/// forever, across restarts, because nothing ever consumes its queued create.
#[tokio::test]
#[serial]
async fn an_offline_created_document_flushes_as_a_create() {
    if skip_if_no_server() {
        return;
    }

    std::fs::create_dir_all("databases").ok();
    let db_file = temp_db_path("offline_create_flush");
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    let driver = TestClient::connect(TEST_EMAIL).await.unwrap();
    let doc_id = Uuid::new_v4();
    let final_content = json!({"title": "draft", "a": 1, "b": 2});

    // Offline from the start: create, then two edits. Nothing reaches the server.
    {
        let offline = open_offline_subject(&db_url).await;
        offline
            .create_document_with_id(doc_id, json!({"title": "draft"}))
            .await
            .unwrap();
        offline
            .update_document(doc_id, json!({"title": "draft", "a": 1}))
            .await
            .unwrap();
        offline
            .update_document(doc_id, final_content.clone())
            .await
            .unwrap();
    }
    assert!(
        driver.get_document(doc_id).await.is_err(),
        "the server must not hold a document that was only ever created offline"
    );

    // Reconnecting flushes the queue; the subject settles once the server acks.
    let subject = connect_subject(&db_url).await;
    assert!(
        wait_for_local_synced(&db_url, doc_id).await,
        "the offline-created document should settle as synced, not retry forever"
    );

    let server_doc = driver.get_document(doc_id).await.unwrap();
    assert_eq!(
        server_doc.get("content"),
        Some(&final_content),
        "the create must carry the edits made after it, not the content it was created with"
    );
    assert_eq!(
        server_doc.get("sync_revision").and_then(|v| v.as_i64()),
        Some(1),
        "a create plus two offline edits must land as one document at the first revision"
    );

    drop(subject);
    remove_temp_db(&db_file);
}

/// Proves envelope attribution flows end-to-end: server stamps author_name
/// (email local-part fallback), visibility, and provenance on create, and
/// the same fields arrive on full sync — outside the content JSON.
#[tokio::test]
#[serial]
async fn test_envelope_attribution_end_to_end() {
    if skip_if_no_server() {
        return;
    }

    let client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create -> reply carries server-stamped attribution.
    let content = json!({"title": "Envelope check"});
    let reply = client.create_document(content).await.unwrap();
    assert_eq!(reply["author_name"], json!("integration-test"));
    assert_eq!(reply["visibility"], json!("private"));

    let doc_id = reply.get("id").and_then(|v| v.as_str()).unwrap();

    // Full sync -> the doc arrives with the same attribution, outside content.
    let sync_result = client.request_full_sync().await.unwrap();
    let documents = sync_result
        .get("documents")
        .and_then(|v| v.as_array())
        .unwrap();
    let doc = documents
        .iter()
        .find(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == doc_id)
                .unwrap_or(false)
        })
        .expect("Created document should appear in full sync");

    assert_eq!(doc["author_name"], json!("integration-test"));
    assert_eq!(doc["visibility"], json!("private"));
    assert!(
        doc["content"].get("author").is_none(),
        "attribution must not ride in content"
    );
}

/// An edit made straight after a create, before the server has acknowledged
/// the create, must reach the server, and the create's acknowledgement must
/// not discard it.
#[tokio::test]
#[serial]
async fn an_edit_right_after_a_create_reaches_the_server() {
    if skip_if_no_server() {
        return;
    }

    std::fs::create_dir_all("databases").ok();
    let db_file = temp_db_path("create_then_edit");
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    let driver = TestClient::connect(TEST_EMAIL).await.unwrap();
    let subject = connect_subject(&db_url).await;
    let doc_id = Uuid::new_v4();
    let edited = json!({"title": "edited", "body": "second write"});

    subject
        .create_document_with_id(doc_id, json!({"title": "draft"}))
        .await
        .unwrap();
    subject
        .update_document(doc_id, edited.clone())
        .await
        .unwrap();

    let mut server_content = None;
    for _ in 0..100 {
        server_content = driver
            .get_document(doc_id)
            .await
            .ok()
            .and_then(|doc| doc.get("content").cloned());
        if server_content.as_ref() == Some(&edited) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        server_content.as_ref(),
        Some(&edited),
        "the server must end up with the edit"
    );
    assert!(
        wait_for_local_synced(&db_url, doc_id).await,
        "the document should settle as synced"
    );
    assert_eq!(subject.count_pending_sync().await.unwrap(), 0);

    subject.shutdown().await;
    remove_temp_db(&db_file);
}
