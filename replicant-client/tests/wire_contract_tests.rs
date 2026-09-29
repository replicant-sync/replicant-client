//! Contract test: every recorded server v2 wire frame must deserialize into the
//! client's engine types. Proves wire compatibility before the client driver exists.

use std::collections::BTreeSet;

use replicant_client::engine::types::{Change, ChangeKind, DocEnvelope, Scope, Seq, UploadKind};
use replicant_core::patches::calculate_checksum;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const FRAMES_JSON: &str = include_str!("fixtures/server_v2/v2_frames.json");

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

/// Mirrors `machine::Request::Join`: the `phx_join` payload.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Join {
    email: String,
    api_key: String,
    signature: String,
    timestamp: i64,
}

/// Mirrors `machine::Request::GetChangesSince`, which is not a serde type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetChangesSince {
    scope: Scope,
    cursor: Seq,
    limit: u32,
}

/// Mirrors `machine::Request::GetSnapshot`, which is not a serde type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSnapshot {
    scope: Scope,
    page_token: Option<String>,
}

/// Mirrors `machine::Request::Upload` (`types::Upload`), which is not a serde type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    upload_id: Uuid,
    doc_id: Uuid,
    kind: String,
    base_hash: Option<String>,
    payload: Value,
}

/// Mirrors `machine::Request::GetDocument`, which is not a serde type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetDocument {
    doc_id: Uuid,
}

/// The websocket connect query params.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketConnectParams {
    protocol_version: String,
    client_id: Uuid,
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
    "changes_request",
    "cursor_too_old_reply",
    "deleted_reply",
    "document_reply",
    "document_request",
    "exists_reply",
    "float_document_reply",
    "float_upload_reply",
    "hash_mismatch_reply",
    "join_error_reply",
    "join_reply",
    "join_request",
    "publish_reply",
    "publish_request",
    "publish_update_reply",
    "publish_update_request",
    "snapshot_page_request",
    "snapshot_reply",
    "snapshot_request",
    "socket_connect_params",
    "socket_refusal",
    "subscription_forbidden_reply",
    "too_large_reply",
    "unpublish_reply",
    "unpublish_request",
    "upload_create_request",
    "upload_delete_request",
    "upload_reply",
    "upload_update_request",
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

fn upload_kind(kind: &str) -> UploadKind {
    match kind {
        "create" => UploadKind::Create,
        "update" => UploadKind::Update,
        "delete" => UploadKind::Delete,
        other => panic!("unknown upload kind {other}"),
    }
}

fn upload_request(frames: &serde_json::Map<String, Value>, name: &str) -> (Value, Upload) {
    let payload = frame_payload(frames, name, "upload");
    let upload: Upload = serde_json::from_value(payload.clone())
        .unwrap_or_else(|e| panic!("{name} must match Request::Upload: {e}"));
    assert_eq!(upload.upload_id, Uuid::nil(), "{name}");
    assert_eq!(upload.doc_id, Uuid::nil(), "{name}");
    (payload, upload)
}

#[test]
fn socket_connect_and_join_requests_carry_the_contract_fields() {
    let frames = load_frames();

    let connect: SocketConnectParams =
        serde_json::from_value(frames["socket_connect_params"].clone())
            .expect("socket_connect_params must match the connect query params");
    assert_eq!(connect.protocol_version, "2");
    assert_eq!(connect.client_id, Uuid::nil());

    let join = frame_payload(&frames, "join_request", "phx_join");
    let join: Join = serde_json::from_value(join).expect("join_request must match Request::Join");
    assert!(join.email.contains('@'));
    assert!(!join.api_key.is_empty());
    assert_eq!(join.signature.len(), 64);
    assert!(join.timestamp > 0);
}

#[test]
fn feed_and_document_requests_match_their_request_variants() {
    let frames = load_frames();

    let changes: GetChangesSince = serde_json::from_value(frame_payload(
        &frames,
        "changes_request",
        "get_changes_since",
    ))
    .expect("changes_request must match Request::GetChangesSince");
    assert_eq!(changes.scope, "own");
    assert!(changes.cursor >= 0 && changes.limit > 0);

    let first_page = frame_payload(&frames, "snapshot_request", "get_snapshot");
    assert!(first_page.get("page_token").is_none());
    let first_page: GetSnapshot = serde_json::from_value(first_page)
        .expect("snapshot_request must match Request::GetSnapshot");
    assert_eq!(first_page.scope, "own");
    assert_eq!(first_page.page_token, None);

    let next_page: GetSnapshot = serde_json::from_value(frame_payload(
        &frames,
        "snapshot_page_request",
        "get_snapshot",
    ))
    .expect("snapshot_page_request must match Request::GetSnapshot");
    let token = next_page
        .page_token
        .expect("snapshot_page_request must carry a page_token");
    let (seq, id) = token
        .split_once(':')
        .expect("page_token is <snapshot_seq>:<doc_id>");
    assert!(seq.parse::<Seq>().is_ok());
    assert!(Uuid::parse_str(id).is_ok());

    let document: GetDocument =
        serde_json::from_value(frame_payload(&frames, "document_request", "get_document"))
            .expect("document_request must match Request::GetDocument");
    assert_eq!(document.doc_id, Uuid::nil());
}

#[test]
fn upload_requests_match_request_upload_per_kind() {
    let frames = load_frames();

    let (raw, create) = upload_request(&frames, "upload_create_request");
    assert_eq!(upload_kind(&create.kind), UploadKind::Create);
    assert!(raw.get("base_hash").is_none());
    assert!(create.payload.is_object());

    let (_raw, update) = upload_request(&frames, "upload_update_request");
    assert_eq!(upload_kind(&update.kind), UploadKind::Update);
    assert!(update.base_hash.is_some());
    assert!(update.payload.is_array());

    let (raw, delete) = upload_request(&frames, "upload_delete_request");
    assert_eq!(upload_kind(&delete.kind), UploadKind::Delete);
    assert!(raw.get("base_hash").is_none());
    assert!(delete.payload.is_null());
}

/// Never compare a client-computed hash to a server hash: the server hash is authoritative.
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

    assert_eq!(
        upload_doc.hash, document_doc.hash,
        "the server hash must survive the jsonb round trip"
    );
    assert_eq!(
        calculate_checksum(&upload_doc.content),
        upload_doc.hash,
        "client hash of the echoed upload content must match the server's hash"
    );
    assert_ne!(
        calculate_checksum(&document_doc.content),
        document_doc.hash,
        "client-computed hash of jsonb-rounded content must not equal the server's hash"
    );
}
