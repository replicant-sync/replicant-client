//! Live sync broadcast tests: verify clients receive real-time push events.
//!
//! Unlike multi_client_test.rs which verifies via request_full_sync (polling),
//! these tests verify that broadcast events actually arrive via the channel
//! event stream.

use super::{serial, skip_if_no_server, TestClient, TEST_EMAIL};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn test_broadcast_on_create() {
    if skip_if_no_server() {
        return;
    }

    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let mut client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Drain any stale events
    client_b.drain_broadcasts().await;

    let doc_id = Uuid::new_v4();
    client_a
        .create_document_with_id(doc_id, json!({"title": "Broadcast Create Test"}))
        .await
        .unwrap();

    let event = client_b.recv_broadcast(Duration::from_secs(5)).await;

    let event = event.expect("Client B should receive a broadcast event");
    assert_eq!(
        event.event, "document_created",
        "Event should be document_created"
    );
    let event_id = event
        .payload
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        event_id,
        doc_id.to_string(),
        "Broadcast should contain correct document ID"
    );
}

#[tokio::test]
#[serial]
async fn test_broadcast_on_update() {
    if skip_if_no_server() {
        return;
    }

    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let mut client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document
    let doc_id = Uuid::new_v4();
    let create_result = client_a
        .create_document_with_id(doc_id, json!({"title": "Original", "version": 1}))
        .await
        .unwrap();
    let content_hash = create_result
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap();

    // Drain the create broadcast
    client_b.drain_broadcasts().await;

    // Update the document
    let patch = json!([{"op": "replace", "path": "/title", "value": "Updated by A"}]);
    client_a
        .update_document(doc_id, patch, content_hash)
        .await
        .unwrap();

    let event = client_b.recv_broadcast(Duration::from_secs(5)).await;

    let event = event.expect("Client B should receive update broadcast");
    assert_eq!(
        event.event, "document_updated",
        "Event should be document_updated"
    );
    let event_id = event
        .payload
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        event_id,
        doc_id.to_string(),
        "Broadcast should contain correct document ID"
    );
}

#[tokio::test]
#[serial]
async fn test_broadcast_on_delete() {
    if skip_if_no_server() {
        return;
    }

    let client_a = TestClient::connect(TEST_EMAIL).await.unwrap();
    let mut client_b = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Create a document
    let doc_id = Uuid::new_v4();
    client_a
        .create_document_with_id(doc_id, json!({"title": "To Be Deleted"}))
        .await
        .unwrap();

    // Drain the create broadcast
    client_b.drain_broadcasts().await;

    // Delete the document
    client_a.delete_document(doc_id).await.unwrap();

    let event = client_b.recv_broadcast(Duration::from_secs(5)).await;

    let event = event.expect("Client B should receive delete broadcast");
    assert_eq!(
        event.event, "document_deleted",
        "Event should be document_deleted"
    );
    let event_id = event
        .payload
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        event_id,
        doc_id.to_string(),
        "Broadcast should contain correct document ID"
    );
}

#[tokio::test]
#[serial]
async fn test_sender_does_not_receive_own_broadcast() {
    if skip_if_no_server() {
        return;
    }

    let mut client = TestClient::connect(TEST_EMAIL).await.unwrap();

    // Drain any stale events
    client.drain_broadcasts().await;

    // Create a document
    let doc_id = Uuid::new_v4();
    client
        .create_document_with_id(doc_id, json!({"title": "No Self-Broadcast"}))
        .await
        .unwrap();

    // The sender should NOT receive their own broadcast (broadcast_from! excludes sender)
    let event = client.recv_broadcast(Duration::from_millis(500)).await;

    assert!(
        event.is_none(),
        "Sender should not receive their own broadcast event, but got: {:?}",
        event.map(|e| e.event)
    );
}
