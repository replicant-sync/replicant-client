//! Contract test: every recorded server v2 wire frame must deserialize into the
//! client's engine types. Proves wire compatibility before the client driver exists.

use std::collections::BTreeSet;

use replicant_client::engine::types::{Change, ChangeKind, DocEnvelope, Scope, Seq};
use replicant_core::patches::calculate_checksum;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const FRAMES_JSON: &str = include_str!("fixtures/server_v2/v2_frames.json");
const HASH_FIXTURE_JSON: &str = include_str!("fixtures/server_v2/content_hash_fixture.json");

/// Mirrors `machine::Response::Changes`, which is not a serde type.
#[derive(Debug, Deserialize)]
struct WireChangesResponse {
    changes: Vec<Change>,
    next_cursor: Seq,
    has_more: bool,
}

/// Mirrors `machine::Response::SnapshotPage`, which is not a serde type.
#[derive(Debug, Deserialize)]
struct WireSnapshotResponse {
    docs: Vec<DocEnvelope>,
    snapshot_seq: Seq,
    next_page_token: Option<String>,
}

/// Mirrors `types::ServerError` plus the `scope`/`doc_id` keys some error replies carry,
/// which `ServerError` itself does not store.
#[derive(Debug, Deserialize)]
struct WireError {
    code: String,
    is_fatal: bool,
    retry_after_ms: Option<u64>,
    current_hash: Option<String>,
    current_seq: Option<Seq>,
    existing_owner: Option<Uuid>,
    scope: Option<Scope>,
    doc_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct WireJoinReply {
    user_id: Uuid,
    protocol_version: u32,
}

#[derive(Debug, Deserialize)]
struct ReplyEnvelope {
    status: String,
    response: Value,
}

const FATAL_CODES: &[&str] = &["update_required", "auth_invalid", "account_disabled"];

/// Every frame name the server fixture is expected to contain. Keeping this list explicit
/// means a frame added on the server fails this test until the client covers it too.
const EXPECTED_FRAME_NAMES: &[&str] = &[
    "change_push",
    "change_push_delete",
    "changes_reply",
    "changes_reply_populated",
    "cursor_too_old_reply",
    "deleted_reply",
    "document_reply",
    "exists_reply",
    "float_document_reply",
    "float_upload_reply",
    "hash_mismatch_reply",
    "join_error_reply",
    "join_reply",
    "publish_reply",
    "publish_request",
    "publish_update_reply",
    "publish_update_request",
    "snapshot_reply",
    "socket_refusal",
    "subscription_forbidden_reply",
    "too_large_reply",
    "unpublish_reply",
    "unpublish_request",
    "upload_reply",
    "validation_reply",
];

fn load_frames() -> serde_json::Map<String, Value> {
    let root: Value = serde_json::from_str(FRAMES_JSON).expect("v2_frames.json must parse");
    root.as_object()
        .expect("v2_frames.json must be a JSON object of named frames")
        .clone()
}

/// Parses a named entry as a `[join_ref, ref, topic, event, payload]` frame and returns its
/// payload, asserting the topic and event match.
fn frame_payload(frames: &serde_json::Map<String, Value>, name: &str, event: &str) -> Value {
    let frame = frames[name]
        .as_array()
        .unwrap_or_else(|| panic!("{name} must be a 5-element phoenix frame array"));
    assert_eq!(frame.len(), 5, "{name} must have 5 frame elements");
    assert_eq!(
        frame[2],
        Value::String("sync:v2".to_string()),
        "{name} has unexpected topic"
    );
    assert_eq!(
        frame[3],
        Value::String(event.to_string()),
        "{name} has unexpected event"
    );
    frame[4].clone()
}

/// Parses a named entry as a `phx_reply` frame and returns its `response`, asserting `status`.
fn reply_response(frames: &serde_json::Map<String, Value>, name: &str, status: &str) -> Value {
    let payload = frame_payload(frames, name, "phx_reply");
    let envelope: ReplyEnvelope = serde_json::from_value(payload)
        .unwrap_or_else(|e| panic!("{name} payload must be a reply envelope: {e}"));
    assert_eq!(envelope.status, status, "{name} has unexpected status");
    envelope.response
}

fn assert_error_shape(name: &str, response: Value) -> WireError {
    let err: WireError = serde_json::from_value(response)
        .unwrap_or_else(|e| panic!("{name} must parse as WireError: {e}"));
    assert_eq!(
        err.is_fatal,
        FATAL_CODES.contains(&err.code.as_str()),
        "{name}: is_fatal mismatch for code {}",
        err.code
    );
    err
}

#[test]
fn every_expected_frame_is_present_and_no_others() {
    let frames = load_frames();
    let actual: BTreeSet<&str> = frames.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = EXPECTED_FRAME_NAMES.iter().copied().collect();
    assert_eq!(actual, expected, "frame set changed on the server side");
}

#[test]
fn change_push_upsert_deserializes_into_change() {
    let frames = load_frames();
    let payload = frame_payload(&frames, "change_push", "change");
    let change: Change = serde_json::from_value(payload).expect("change_push must be a Change");
    assert_eq!(change.kind, ChangeKind::Upsert);
    assert!(change.doc.is_some());
    assert!(change.prev_seq < change.seq);
}

#[test]
fn change_push_delete_has_no_doc_and_advances_seq() {
    let frames = load_frames();
    let payload = frame_payload(&frames, "change_push_delete", "change");
    let change: Change =
        serde_json::from_value(payload).expect("change_push_delete must be a Change");
    assert_eq!(change.kind, ChangeKind::Delete);
    assert_eq!(change.doc, None);
    assert!(change.prev_seq < change.seq);
}

#[test]
fn changes_reply_empty_deserializes_into_changes_response() {
    let frames = load_frames();
    let response = reply_response(&frames, "changes_reply", "ok");
    let page: WireChangesResponse =
        serde_json::from_value(response).expect("changes_reply must match Response::Changes");
    assert!(page.changes.is_empty());
    assert!(!page.has_more);
    assert_eq!(page.next_cursor, 2);
}

#[test]
fn changes_reply_populated_deserializes_every_changes_item() {
    let frames = load_frames();
    let response = reply_response(&frames, "changes_reply_populated", "ok");
    let page: WireChangesResponse = serde_json::from_value(response)
        .expect("changes_reply_populated must match Response::Changes");
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].kind, ChangeKind::Delete);
    assert_eq!(page.changes[0].doc, None);
}

#[test]
fn snapshot_reply_deserializes_into_snapshot_response() {
    let frames = load_frames();
    let response = reply_response(&frames, "snapshot_reply", "ok");
    let page: WireSnapshotResponse =
        serde_json::from_value(response).expect("snapshot_reply must match Response::SnapshotPage");
    assert_eq!(page.docs.len(), 1);
    assert_eq!(page.snapshot_seq, 3);
    assert_eq!(page.next_page_token, None);
}

#[test]
fn join_reply_carries_protocol_version_two() {
    let frames = load_frames();
    let response = reply_response(&frames, "join_reply", "ok");
    let join: WireJoinReply =
        serde_json::from_value(response).expect("join_reply must parse as WireJoinReply");
    assert_eq!(join.protocol_version, 2);
    assert_eq!(join.user_id, Uuid::from_u128(0));
}

#[test]
fn document_shaped_replies_deserialize_into_doc_envelope() {
    let frames = load_frames();
    for name in [
        "document_reply",
        "publish_reply",
        "publish_update_reply",
        "unpublish_reply",
        "upload_reply",
    ] {
        let response = reply_response(&frames, name, "ok");
        let _doc: DocEnvelope =
            serde_json::from_value(response).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn error_replies_and_socket_refusal_parse_extra_keys() {
    let frames = load_frames();

    let cursor_too_old = assert_error_shape(
        "cursor_too_old_reply",
        reply_response(&frames, "cursor_too_old_reply", "error"),
    );
    assert_eq!(cursor_too_old.scope.as_deref(), Some("own"));
    assert_eq!(cursor_too_old.retry_after_ms, None);

    let deleted = assert_error_shape(
        "deleted_reply",
        reply_response(&frames, "deleted_reply", "error"),
    );
    assert_eq!(deleted.current_seq, Some(4));
    assert!(deleted.doc_id.is_some());

    let exists = assert_error_shape(
        "exists_reply",
        reply_response(&frames, "exists_reply", "error"),
    );
    assert!(exists.existing_owner.is_some());
    assert!(exists.doc_id.is_some());

    let hash_mismatch = assert_error_shape(
        "hash_mismatch_reply",
        reply_response(&frames, "hash_mismatch_reply", "error"),
    );
    assert!(hash_mismatch.current_hash.is_some());
    assert_eq!(hash_mismatch.current_seq, Some(3));

    let join_error = assert_error_shape(
        "join_error_reply",
        reply_response(&frames, "join_error_reply", "error"),
    );
    assert_eq!(join_error.code, "auth_invalid");
    assert!(join_error.is_fatal);

    let subscription_forbidden = assert_error_shape(
        "subscription_forbidden_reply",
        reply_response(&frames, "subscription_forbidden_reply", "error"),
    );
    assert_eq!(
        subscription_forbidden.scope.as_deref(),
        Some("collection:nope")
    );

    let too_large = assert_error_shape(
        "too_large_reply",
        reply_response(&frames, "too_large_reply", "error"),
    );
    assert!(too_large.doc_id.is_some());

    assert_error_shape(
        "validation_reply",
        reply_response(&frames, "validation_reply", "error"),
    );

    let socket_refusal = &frames["socket_refusal"];
    let refusal = assert_error_shape("socket_refusal", socket_refusal.clone());
    assert_eq!(refusal.code, "update_required");
    assert!(refusal.is_fatal);
}

#[test]
fn client_push_requests_carry_their_expected_ids() {
    let frames = load_frames();

    let publish = frame_payload(&frames, "publish_request", "publish");
    assert!(publish
        .get("source_doc_id")
        .and_then(Value::as_str)
        .is_some());

    let publish_update = frame_payload(&frames, "publish_update_request", "publish_update");
    assert!(publish_update
        .get("publication_id")
        .and_then(Value::as_str)
        .is_some());

    let unpublish = frame_payload(&frames, "unpublish_request", "unpublish");
    assert!(unpublish
        .get("publication_id")
        .and_then(Value::as_str)
        .is_some());
}

/// Never compare a client-computed hash to a server hash: the server hash is authoritative,
/// and jsonb round-tripping a float loses the distinction the client-side hasher preserves.
#[test]
fn float_content_deserializes_but_client_hash_is_not_the_server_hash() {
    let frames = load_frames();

    let upload_response = reply_response(&frames, "float_upload_reply", "ok");
    let upload_doc: DocEnvelope =
        serde_json::from_value(upload_response).expect("float_upload_reply must be a DocEnvelope");
    assert_eq!(upload_doc.seq, 5);

    let document_response = reply_response(&frames, "float_document_reply", "ok");
    let document_doc: DocEnvelope = serde_json::from_value(document_response)
        .expect("float_document_reply must be a DocEnvelope");

    assert_ne!(
        calculate_checksum(&document_doc.content),
        document_doc.hash,
        "client-computed hash of jsonb-rounded content must not equal the server's hash"
    );
}

#[test]
fn content_hash_fixture_matches_calculate_checksum() {
    #[derive(Debug, Deserialize)]
    struct Case {
        name: String,
        content: Value,
        hash: String,
    }

    let cases: Vec<Case> =
        serde_json::from_str(HASH_FIXTURE_JSON).expect("content_hash_fixture.json must parse");
    assert_eq!(cases.len(), 8, "expected 8 pinned hash cases");
    for case in &cases {
        assert_eq!(
            calculate_checksum(&case.content),
            case.hash,
            "hash mismatch for case {}",
            case.name
        );
    }
}
