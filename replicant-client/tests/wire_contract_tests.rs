//! Contract test: every recorded server v2 wire frame round-trips through the client's real
//! wire types and codec.

use std::collections::BTreeSet;

use replicant_client::engine::machine::{Request, Response};
use replicant_client::engine::types::{ChangeKind, DocEnvelope, ServerError, Upload, UploadKind};
use replicant_client::transport::codec::{Codec, Incoming};
use replicant_client::transport::wire::{socket_url, JoinAuth, ReplyEnvelope};
use replicant_core::patches::calculate_checksum;
use serde_json::{json, Map, Value};
use uuid::Uuid;

const FRAMES_JSON: &str = include_str!("fixtures/server_v2/v2_frames.json");
const FATAL_CODES: &[&str] = &["update_required", "auth_invalid", "account_disabled"];
const JOIN_TIMESTAMP: i64 = 1_767_225_600;

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

fn load_frames() -> Map<String, Value> {
    let root: Value = serde_json::from_str(FRAMES_JSON).expect("v2_frames.json must parse");
    root.as_object()
        .expect("v2_frames.json must be a JSON object of named frames")
        .clone()
}

fn auth() -> JoinAuth {
    JoinAuth {
        email: "wire@example.com".into(),
        api_key: "<api_key>".into(),
        api_secret: "rps_test".into(),
    }
}

/// A codec that has joined with ref "1", as every recorded channel frame assumes.
fn joined_codec() -> Codec {
    let mut codec = Codec::new();
    codec.encode(1, &Request::Join, &auth(), JOIN_TIMESTAMP);
    codec
}

fn encode(codec: &mut Codec, req: u64, request: &Request) -> Value {
    serde_json::from_str(&codec.encode(req, request, &auth(), JOIN_TIMESTAMP))
        .expect("codec writes JSON")
}

/// Sends `request` with ref "2", as recorded, and decodes the named reply to it.
fn reply_to(
    frames: &Map<String, Value>,
    name: &str,
    request: &Request,
) -> Result<Response, ServerError> {
    let mut codec = joined_codec();
    codec.encode(2, request, &auth(), JOIN_TIMESTAMP);
    match codec.decode(&frames[name].to_string()) {
        Some(Incoming::Reply { req: 2, result }) => result,
        other => panic!("{name}: expected a reply to ref 2, got {other:?}"),
    }
}

fn expect_error(name: &str, result: Result<Response, ServerError>) -> ServerError {
    let error = result.expect_err(name);
    assert_eq!(
        error.is_fatal,
        FATAL_CODES.contains(&error.code.as_str()),
        "{name}: is_fatal mismatch for code {}",
        error.code
    );
    error
}

/// The raw `response` of a recorded reply, for keys `ServerError` does not keep.
fn raw_response(frames: &Map<String, Value>, name: &str) -> Value {
    let envelope: ReplyEnvelope = serde_json::from_value(frames[name][4].clone())
        .unwrap_or_else(|e| panic!("{name} payload must be a reply envelope: {e}"));
    envelope.response
}

fn changes_request() -> Request {
    Request::GetChangesSince {
        scope: "own".into(),
        cursor: 0,
        limit: 500,
    }
}

fn snapshot_request() -> Request {
    Request::GetSnapshot {
        scope: "own".into(),
        page_token: None,
    }
}

fn document_request() -> Request {
    Request::GetDocument {
        doc_id: Uuid::nil(),
    }
}

fn upload(kind: UploadKind, base_hash: Option<String>, payload: Value) -> Request {
    Request::Upload(Upload {
        upload_id: Uuid::nil(),
        doc_id: Uuid::nil(),
        kind,
        base_hash,
        payload,
    })
}

fn create_upload() -> Request {
    upload(UploadKind::Create, None, json!({"title": "Wire"}))
}

#[test]
fn every_expected_frame_is_present_and_no_others() {
    let frames = load_frames();
    let actual: BTreeSet<&str> = frames.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = EXPECTED_FRAME_NAMES.iter().copied().collect();
    assert_eq!(actual, expected, "frame set changed on the server side");
}

#[test]
fn change_pushes_decode_into_changes() {
    let frames = load_frames();
    let mut codec = joined_codec();

    let Some(Incoming::Push(upsert)) = codec.decode(&frames["change_push"].to_string()) else {
        panic!("change_push must decode as a push");
    };
    assert_eq!(upsert.kind, ChangeKind::Upsert);
    assert!(upsert.doc.is_some());
    assert!(upsert.prev_seq < upsert.seq);

    let Some(Incoming::Push(delete)) = codec.decode(&frames["change_push_delete"].to_string())
    else {
        panic!("change_push_delete must decode as a push");
    };
    assert_eq!(delete.kind, ChangeKind::Delete);
    assert_eq!(delete.doc, None);
    assert!(delete.prev_seq < delete.seq);
}

#[test]
fn changes_replies_decode_into_response_changes() {
    let frames = load_frames();
    assert_eq!(
        reply_to(&frames, "changes_reply", &changes_request()),
        Ok(Response::Changes {
            changes: vec![],
            next_cursor: 2,
            has_more: false
        })
    );
    let Ok(Response::Changes {
        changes,
        next_cursor: 4,
        has_more: false,
    }) = reply_to(&frames, "changes_reply_populated", &changes_request())
    else {
        panic!("changes_reply_populated must decode as a changes page");
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Delete);
    assert_eq!(changes[0].doc, None);
}

#[test]
fn snapshot_reply_decodes_into_snapshot_page() {
    let frames = load_frames();
    let Ok(Response::SnapshotPage {
        docs,
        snapshot_seq,
        next_page_token,
    }) = reply_to(&frames, "snapshot_reply", &snapshot_request())
    else {
        panic!("snapshot_reply must decode as a snapshot page");
    };
    assert_eq!(docs.len(), 1);
    assert_eq!(snapshot_seq, 3);
    assert_eq!(next_page_token, None);
}

#[test]
fn join_reply_decodes_as_joined_with_the_user_id() {
    let frames = load_frames();
    let mut codec = joined_codec();
    assert_eq!(
        codec.decode(&frames["join_reply"].to_string()),
        Some(Incoming::Joined {
            req: 1,
            user_id: Uuid::nil()
        })
    );
}

#[test]
fn document_shaped_replies_decode_into_doc_envelopes() {
    let frames = load_frames();
    assert!(matches!(
        reply_to(&frames, "document_reply", &document_request()),
        Ok(Response::Document(_))
    ));
    assert!(matches!(
        reply_to(&frames, "upload_reply", &create_upload()),
        Ok(Response::Uploaded(_))
    ));
    // The client sends no publish requests yet; their replies still share the envelope.
    for name in ["publish_reply", "publish_update_reply", "unpublish_reply"] {
        let _doc: DocEnvelope = serde_json::from_value(raw_response(&frames, name))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn error_replies_decode_into_server_errors() {
    let frames = load_frames();

    let cursor_too_old = expect_error(
        "cursor_too_old_reply",
        reply_to(&frames, "cursor_too_old_reply", &changes_request()),
    );
    assert_eq!(cursor_too_old.code, "cursor_too_old");
    assert_eq!(cursor_too_old.retry_after_ms, None);
    assert_eq!(
        raw_response(&frames, "cursor_too_old_reply")["scope"],
        "own"
    );

    let deleted = expect_error(
        "deleted_reply",
        reply_to(&frames, "deleted_reply", &document_request()),
    );
    assert_eq!(deleted.code, "deleted");
    assert_eq!(deleted.current_seq, Some(4));
    assert!(raw_response(&frames, "deleted_reply")["doc_id"].is_string());

    let exists = expect_error(
        "exists_reply",
        reply_to(&frames, "exists_reply", &create_upload()),
    );
    assert_eq!(exists.code, "exists");
    assert!(exists.existing_owner.is_some());

    let hash_mismatch = expect_error(
        "hash_mismatch_reply",
        reply_to(&frames, "hash_mismatch_reply", &create_upload()),
    );
    assert!(hash_mismatch.current_hash.is_some());
    assert_eq!(hash_mismatch.current_seq, Some(3));

    let forbidden = expect_error(
        "subscription_forbidden_reply",
        reply_to(&frames, "subscription_forbidden_reply", &snapshot_request()),
    );
    assert_eq!(forbidden.code, "subscription_forbidden");
    assert_eq!(
        raw_response(&frames, "subscription_forbidden_reply")["scope"],
        "collection:nope"
    );

    let too_large = expect_error(
        "too_large_reply",
        reply_to(&frames, "too_large_reply", &create_upload()),
    );
    assert_eq!(too_large.code, "too_large");
    assert!(raw_response(&frames, "too_large_reply")["doc_id"].is_string());

    let validation = expect_error(
        "validation_reply",
        reply_to(&frames, "validation_reply", &create_upload()),
    );
    assert_eq!(validation.code, "validation");
}

#[test]
fn join_errors_and_socket_refusal_are_fatal_server_errors() {
    let frames = load_frames();
    let mut codec = joined_codec();
    let Some(Incoming::Reply { req: 1, result }) =
        codec.decode(&frames["join_error_reply"].to_string())
    else {
        panic!("join_error_reply must decode as a reply to the join");
    };
    let join_error = expect_error("join_error_reply", result);
    assert_eq!(join_error.code, "auth_invalid");
    assert!(join_error.is_fatal);

    let refusal: ServerError = serde_json::from_value(frames["socket_refusal"].clone())
        .expect("socket_refusal must parse as ServerError");
    assert_eq!(refusal.code, "update_required");
    assert!(refusal.is_fatal);
}

#[test]
fn socket_url_carries_the_recorded_connect_params() {
    let frames = load_frames();
    let url = url::Url::parse(&socket_url("ws://localhost:4000", Uuid::nil()).unwrap()).unwrap();
    let params: Map<String, Value> = url
        .query_pairs()
        .filter(|(key, _)| key != "vsn")
        .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
        .collect();
    assert_eq!(Value::Object(params), frames["socket_connect_params"]);
}

#[test]
fn join_request_matches_the_recorded_frame_with_a_real_signature() {
    let frames = load_frames();
    let join = encode(&mut Codec::new(), 1, &Request::Join);
    let recorded = &frames["join_request"];
    for i in 0..4 {
        assert_eq!(join[i], recorded[i], "join frame element {i}");
    }
    let ours = join[4].as_object().unwrap();
    let theirs = recorded[4].as_object().unwrap();
    assert_eq!(
        ours.keys().collect::<BTreeSet<_>>(),
        theirs.keys().collect::<BTreeSet<_>>()
    );
    for key in ["email", "api_key", "timestamp"] {
        assert_eq!(ours[key], theirs[key], "{key}");
    }
    assert_eq!(
        ours["signature"],
        "c53b53ad283f08d4cd2e9261f8f65d08dc0815b8ad0ff41aa4574f293bb1d9c2"
    );
}

#[test]
fn feed_and_document_requests_match_the_recorded_frames() {
    let frames = load_frames();
    let mut codec = joined_codec();
    assert_eq!(
        encode(&mut codec, 2, &changes_request()),
        frames["changes_request"]
    );
    assert_eq!(
        encode(&mut codec, 2, &snapshot_request()),
        frames["snapshot_request"]
    );
    let next_page = Request::GetSnapshot {
        scope: "own".into(),
        page_token: Some(format!("3:{}", Uuid::nil())),
    };
    assert_eq!(
        encode(&mut codec, 2, &next_page),
        frames["snapshot_page_request"]
    );
    assert_eq!(
        encode(&mut codec, 2, &document_request()),
        frames["document_request"]
    );
}

#[test]
fn upload_requests_match_the_recorded_frames_per_kind() {
    let frames = load_frames();
    let mut codec = joined_codec();
    assert_eq!(
        encode(&mut codec, 2, &create_upload()),
        frames["upload_create_request"]
    );
    let recorded_update = &frames["upload_update_request"][4];
    let update = upload(
        UploadKind::Update,
        recorded_update["base_hash"].as_str().map(String::from),
        recorded_update["payload"].clone(),
    );
    assert_eq!(
        encode(&mut codec, 2, &update),
        frames["upload_update_request"]
    );
    assert_eq!(
        encode(
            &mut codec,
            2,
            &upload(UploadKind::Delete, None, Value::Null)
        ),
        frames["upload_delete_request"]
    );
}

#[test]
fn publish_requests_carry_their_ids() {
    let frames = load_frames();
    for (name, event, key) in [
        ("publish_request", "publish", "source_doc_id"),
        ("publish_update_request", "publish_update", "publication_id"),
        ("unpublish_request", "unpublish", "publication_id"),
    ] {
        assert_eq!(frames[name][2], "sync:v2", "{name}");
        assert_eq!(frames[name][3], event, "{name}");
        assert!(frames[name][4][key].is_string(), "{name}");
    }
}

/// Never compare a client-computed hash to a server hash: the server hash is authoritative.
#[test]
fn float_content_decodes_but_client_hash_is_not_the_server_hash() {
    let frames = load_frames();
    let Ok(Response::Uploaded(upload_doc)) =
        reply_to(&frames, "float_upload_reply", &create_upload())
    else {
        panic!("float_upload_reply must decode as an upload reply");
    };
    assert_eq!(upload_doc.seq, 5);
    let Ok(Response::Document(document_doc)) =
        reply_to(&frames, "float_document_reply", &document_request())
    else {
        panic!("float_document_reply must decode as a document reply");
    };

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
