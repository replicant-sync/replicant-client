use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{Engine, EngineEvent};
use crate::driver::test_server::ScriptedServer;
use crate::driver::test_support::{config, credentials, eventually, jump, seeded_db, wait_for};
use crate::engine::doc::DocEvent;
use crate::engine::doc_upload::MAX_DIVERGENT_REPLIES;
use crate::engine::machine::Lifecycle;
use crate::store::change_log::{ChangeOrigin, DocChange};
use crate::store::test_support::{count, snapshot, ME};
use crate::store::{DocNotice, Store, StoreError};

/// Starts an engine on `path` and waits for its first SyncCompleted.
async fn live_engine(
    server: &ScriptedServer,
    path: &Path,
) -> (Engine, mpsc::UnboundedReceiver<EngineEvent>) {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    wait_for(&mut events, "SyncCompleted", |event| {
        *event == EngineEvent::Lifecycle(Lifecycle::SyncCompleted)
    })
    .await;
    (engine, events)
}

async fn outbox_rows(store: &Store) -> i64 {
    count(store, "SELECT COUNT(*) FROM outbox").await
}

async fn recovered_rows(store: &Store, doc_id: Uuid) -> i64 {
    count(
        store,
        &format!("SELECT COUNT(*) FROM recovered WHERE doc_id = '{doc_id}'"),
    )
    .await
}

fn server_content(server: &ScriptedServer, doc_id: Uuid) -> Option<Value> {
    server.doc(doc_id).map(|(content, _, _)| content)
}

fn conflicts(events: &mut mpsc::UnboundedReceiver<EngineEvent>) -> usize {
    let mut conflicts = 0;
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::Doc(DocNotice {
            event: DocEvent::ConflictDetected | DocEvent::FieldConflict { .. },
            ..
        }) = event
        {
            conflicts += 1;
        }
    }
    conflicts
}

/// No rows left and the shadow is at the server's latest seq for the document.
async fn in_sync(store: &Store, server: &ScriptedServer, doc_id: Uuid) -> bool {
    outbox_rows(store).await == 0
        && snapshot(store, doc_id).await.shadow.map(|s| s.seq)
            == server.doc(doc_id).map(|(_, seq, _)| seq)
}

/// Waits for the document's `uploads`-th upload to be held, then releases it: a released
/// upload's push reaches the client before its reply.
async fn release_when_held(server: &ScriptedServer, doc_id: Uuid, uploads: usize) {
    eventually("the upload is held", || async {
        server.uploads_for(doc_id).len() == uploads
    })
    .await;
    server.release_held();
}

#[tokio::test]
async fn startup_emits_succeeded_started_completed_in_order() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    let mut lifecycles = Vec::new();
    while lifecycles.last() != Some(&Lifecycle::SyncCompleted) {
        if let EngineEvent::Lifecycle(lifecycle) =
            wait_for(&mut events, "a lifecycle event", |event| {
                matches!(event, EngineEvent::Lifecycle(_))
            })
            .await
        {
            lifecycles.push(lifecycle);
        }
    }
    assert_eq!(
        lifecycles,
        vec![
            Lifecycle::ConnectionAttempted,
            Lifecycle::ConnectionSucceeded,
            Lifecycle::SyncStarted,
            Lifecycle::SyncCompleted
        ]
    );
    engine.stop().await;
}

#[tokio::test]
async fn push_gap_triggers_catch_up() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    let (first, second) = (Uuid::from_u128(0xD1), Uuid::from_u128(0xD2));
    server.drop_next_push();
    server.put_doc(first, json!({"a": 1}));
    server.put_doc(second, json!({"b": 1}));
    let store = engine.store();
    eventually("both documents arrive", || async {
        snapshot(&store, first).await.exists && snapshot(&store, second).await.exists
    })
    .await;
    let own_catch_ups = server
        .frames()
        .iter()
        .filter(|frame| frame.event == "get_changes_since" && frame.payload["scope"] == "own")
        .count();
    assert!(own_catch_ups >= 2, "the gap started a second catch-up");
    engine.stop().await;
}

#[tokio::test]
async fn local_edit_uploads_and_settles() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"title": "Scale"}))
        .await
        .unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "the local change", |event| {
        *event
            == EngineEvent::Changed(DocChange {
                doc_id,
                deleted: false,
                origin: ChangeOrigin::Local,
            })
    })
    .await;
    eventually("uploaded and settled", || async {
        outbox_rows(&store).await == 0
    })
    .await;
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"title": "Scale"}))
    );
    engine.stop().await;
}

#[tokio::test]
async fn killed_mid_upload_resends_same_upload_id() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    server.hold_uploads();
    let (first, _first_events) = live_engine(&server, &path).await;
    let doc_id = first
        .store()
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    first.notify_outbox();
    eventually("the first upload is held", || async {
        server.uploads_for(doc_id).len() == 1
    })
    .await;
    // Its in-memory in-flight state dies with it, as after kill -9; only the outbox survives.
    first.stop().await;
    let (second, _second_events) = live_engine(&server, &path).await;
    eventually("the restarted engine resends", || async {
        server.uploads_for(doc_id).len() == 2
    })
    .await;
    let uploads = server.uploads_for(doc_id);
    assert_eq!(
        uploads[0], uploads[1],
        "same upload_id, base_hash and payload"
    );
    server.release_held();
    let store = second.store();
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    assert_eq!(server.commits_for(doc_id), 1, "the server applied it once");
    second.stop().await;
}

#[tokio::test]
async fn cursor_too_old_resync_sweeps_missing_docs() {
    let server = ScriptedServer::start(ME).await;
    let (kept, gone, edited) = (
        Uuid::from_u128(0xD1),
        Uuid::from_u128(0xD2),
        Uuid::from_u128(0xD3),
    );
    server.put_doc(kept, json!({"k": 1}));
    server.put_doc(gone, json!({"g": 1}));
    server.put_doc(edited, json!({"w": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    let store = engine.store();
    for doc_id in [kept, gone, edited] {
        assert!(snapshot(&store, doc_id).await.exists);
    }
    // Let the catch-up's pump timers run out, so the edit below stays pending when we stop.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    store
        .update_document(ME, edited, json!({"w": 2}))
        .await
        .unwrap();
    engine.stop().await;

    server.delete_doc(gone);
    server.delete_doc(edited);
    server.trim_log_through(server.head());

    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    assert!(
        !snapshot(&store, gone).await.exists,
        "the resync swept the deleted document"
    );
    assert!(snapshot(&store, kept).await.exists);
    wait_for(
        &mut events,
        "ConflictDetected for the edited document",
        |event| {
            *event
                == EngineEvent::Doc(DocNotice {
                    doc_id: edited,
                    event: DocEvent::ConflictDetected,
                })
        },
    )
    .await;
    assert_eq!(
        recovered_rows(&store, edited).await,
        1,
        "the offline edit is kept"
    );
    assert!(!snapshot(&store, edited).await.exists);
    engine.stop().await;
}

#[tokio::test]
async fn integral_float_create_uploads_once_after_a_lost_reply() {
    let server = ScriptedServer::start(ME).await;
    server.put_doc(Uuid::from_u128(0xF0), json!({"seed": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    server.drop_after_next_upload();
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1200.0}))
        .await
        .unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "the dropped connection", |event| {
        *event == EngineEvent::Lifecycle(Lifecycle::ConnectionLost)
    })
    .await;
    jump(Duration::from_millis(1100)).await;
    eventually("in sync after the reconnect", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;
    let uploads = server.uploads_for(doc_id);
    assert!(
        uploads.iter().all(|upload| *upload == uploads[0]),
        "only the create, resent at most: {uploads:#?}"
    );
    assert_eq!(
        uploads[0]["payload"],
        json!({"n": 1200}),
        "the create carries the integer"
    );
    assert_eq!(snapshot(&store, doc_id).await.content, json!({"n": 1200}));
    assert_eq!(conflicts(&mut events), 0);
    engine.stop().await;
}

/// Creates {"n": 1200.0, "m": 1}, then edits "m" alone. With `push_first`, each upload's push
/// reaches the client before its reply.
async fn integral_float_edits_upload_once_per_edit(push_first: bool) {
    let server = ScriptedServer::start(ME).await;
    server.put_doc(Uuid::from_u128(0xF0), json!({"seed": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    if push_first {
        server.hold_uploads();
    }
    let doc_id = store
        .create_document(ME, None, json!({"n": 1200.0, "m": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    if push_first {
        release_when_held(&server, doc_id, 1).await;
    }
    eventually("the create settles", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    if push_first {
        server.hold_uploads();
    }
    store
        .update_document(ME, doc_id, json!({"n": 1200.0, "m": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    if push_first {
        release_when_held(&server, doc_id, 2).await;
    }
    eventually("the edit settles", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    assert_eq!(conflicts(&mut events), 0);
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"n": 1200, "m": 2}))
    );
    assert_eq!(
        snapshot(&store, doc_id).await.content,
        json!({"n": 1200, "m": 2})
    );
    let uploads = server.uploads_for(doc_id);
    assert_eq!(uploads.len(), 2, "one upload per edit: {uploads:#?}");
    assert_eq!(uploads[0]["payload"], json!({"n": 1200, "m": 1}));
    assert_eq!(
        uploads[1]["payload"],
        json!([{"op": "replace", "path": "/m", "value": 2}]),
        "the patch never touches the float"
    );
    engine.stop().await;
}

#[tokio::test]
async fn integral_float_edits_upload_once_per_edit_reply_first() {
    integral_float_edits_upload_once_per_edit(false).await;
}

#[tokio::test]
async fn integral_float_edits_upload_once_per_edit_push_first() {
    integral_float_edits_upload_once_per_edit(true).await;
}

/// The client settles {"n": 1200.0, "m": 2} and edits "n" while that upload is held; another
/// device changes "m" meanwhile. With `push_first`, the create's push arrives before its reply.
async fn local_float_edit_survives_another_devices_edit(push_first: bool) {
    let server = ScriptedServer::start(ME).await;
    server.put_doc(Uuid::from_u128(0xF0), json!({"seed": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    if push_first {
        server.hold_uploads();
    }
    let doc_id = store
        .create_document(ME, None, json!({"n": 1200.0, "m": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    if push_first {
        release_when_held(&server, doc_id, 1).await;
    }
    eventually("the create settles", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    server.hold_uploads();
    store
        .update_document(ME, doc_id, json!({"n": 1300.0, "m": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the edit is held", || async {
        server.uploads_for(doc_id).len() == 2
    })
    .await;
    // Another device changes "m"; the server patched the row it read back, so "n" is 1200.
    server.put_doc(doc_id, json!({"n": 1200, "m": 3}));
    eventually(
        "the other device's edit merges under the local one",
        || async { snapshot(&store, doc_id).await.content == json!({"n": 1300, "m": 3}) },
    )
    .await;
    // The held edit's base is stale now: hash_mismatch, then a retry on the new version.
    server.release_held();
    eventually("the local edit lands", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    assert_eq!(conflicts(&mut events), 0);
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"n": 1300, "m": 3}))
    );
    let uploads = server.uploads_for(doc_id);
    assert_eq!(
        uploads.len(),
        3,
        "the create, the held edit and its retry: {uploads:#?}"
    );
    for upload in &uploads[1..] {
        assert_eq!(
            upload["payload"],
            json!([{"op": "replace", "path": "/n", "value": 1300}])
        );
    }
    engine.stop().await;
}

#[tokio::test]
async fn local_float_edit_survives_another_devices_edit_reply_first() {
    local_float_edit_survives_another_devices_edit(false).await;
}

#[tokio::test]
async fn local_float_edit_survives_another_devices_edit_push_first() {
    local_float_edit_survives_another_devices_edit(true).await;
}

#[tokio::test]
async fn another_devices_integral_float_does_not_collide_with_a_local_edit() {
    let server = ScriptedServer::start(ME).await;
    server.put_doc(Uuid::from_u128(0xF0), json!({"seed": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = Uuid::from_u128(0xD1);
    // A writer that does not canonicalise: the push carries the float as written.
    server.put_doc(doc_id, json!({"n": 1200.0, "m": 1}));
    eventually("the other device's document arrives", || async {
        snapshot(&store, doc_id).await.exists
    })
    .await;
    assert_eq!(
        snapshot(&store, doc_id).await.content,
        json!({"n": 1200, "m": 1})
    );

    server.hold_uploads();
    store
        .update_document(ME, doc_id, json!({"n": 1300, "m": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the edit is held", || async {
        server.uploads_for(doc_id).len() == 1
    })
    .await;
    server.put_doc(doc_id, json!({"n": 1200, "m": 2}));
    eventually(
        "the other device's edit merges under the local one",
        || async { snapshot(&store, doc_id).await.content == json!({"n": 1300, "m": 2}) },
    )
    .await;
    server.release_held();
    eventually("the local edit lands", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    assert_eq!(conflicts(&mut events), 0);
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"n": 1300, "m": 2}))
    );
    engine.stop().await;
}

#[tokio::test]
async fn lost_upload_reply_then_new_edit_appends_once() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"items": []}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the document is synced", || async {
        outbox_rows(&store).await == 0
    })
    .await;

    server.lose_next_upload();
    store
        .update_document(ME, doc_id, json!({"items": ["a"]}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the first append is applied", || async {
        server_content(&server, doc_id) == Some(json!({"items": ["a"]}))
    })
    .await;
    // The reply never comes: the request times out and the document backs off for 1 s.
    jump(Duration::from_secs(31)).await;
    store
        .update_document(ME, doc_id, json!({"items": ["a", "b"]}))
        .await
        .unwrap();
    engine.notify_outbox();
    jump(Duration::from_millis(1100)).await;
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"items": ["a", "b"]}))
    );
    assert_eq!(
        snapshot(&store, doc_id).await.content,
        json!({"items": ["a", "b"]})
    );
    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    assert_eq!(conflicts(&mut events), 0);
    engine.stop().await;
}

#[tokio::test]
async fn lost_create_reply_then_edit_settles_without_conflict() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    server.lose_next_upload();
    let doc_id = store
        .create_document(ME, None, json!({"items": ["a"]}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the create is applied", || async {
        server.doc(doc_id).is_some()
    })
    .await;
    jump(Duration::from_secs(31)).await;
    store
        .update_document(ME, doc_id, json!({"items": ["a", "b"]}))
        .await
        .unwrap();
    engine.notify_outbox();
    jump(Duration::from_millis(1100)).await;
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    assert_eq!(
        recovered_rows(&store, doc_id).await,
        0,
        "nothing was in conflict"
    );
    assert_eq!(conflicts(&mut events), 0);
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"items": ["a", "b"]}))
    );
    engine.stop().await;
}

#[tokio::test]
async fn snapshot_of_a_doc_with_only_a_pending_create_surfaces_conflict_detected() {
    let server = ScriptedServer::start(ME).await;
    let doc_id = Uuid::from_u128(0xD0C);
    server.put_doc(doc_id, json!({"v": "server"}));
    let (_dir, path) = seeded_db(ME, true).await;
    let store = Store::open(&path).await.unwrap();
    store
        .create_document(ME, Some(doc_id), json!({"v": "local"}))
        .await
        .unwrap();
    store.close().await;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    wait_for(&mut events, "ConflictDetected", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::ConflictDetected,
            })
    })
    .await;
    let store = engine.store();
    assert_eq!(
        snapshot(&store, doc_id).await.content,
        json!({"v": "server"})
    );
    assert_eq!(
        count(
            &store,
            &format!(
                "SELECT COUNT(*) FROM recovered WHERE doc_id = '{doc_id}' AND content LIKE '%local%'"
            ),
        )
        .await,
        1
    );
    engine.stop().await;
}

#[tokio::test]
async fn new_edit_unparks_a_rejected_document() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    server.reject_next_upload("validation");
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "the park", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::SyncError {
                    code: "validation".into(),
                },
            })
    })
    .await;
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM outbox WHERE parked_error IS NOT NULL"
        )
        .await,
        1
    );
    store
        .update_document(ME, doc_id, json!({"n": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("uploaded after the new edit", || async {
        outbox_rows(&store).await == 0
    })
    .await;
    assert_eq!(server_content(&server, doc_id), Some(json!({"n": 2})));
    engine.stop().await;
}

#[tokio::test]
async fn delete_of_a_parked_document_is_uploaded() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("synced", || async { outbox_rows(&store).await == 0 }).await;
    server.reject_next_upload("validation");
    store
        .update_document(ME, doc_id, json!({"n": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("parked", || async {
        count(
            &store,
            "SELECT COUNT(*) FROM outbox WHERE parked_error IS NOT NULL",
        )
        .await
            == 1
    })
    .await;
    store.delete_document(ME, doc_id).await.unwrap();
    engine.notify_outbox();
    eventually("the delete is uploaded and settled", || async {
        server.doc(doc_id).is_some_and(|(_, _, deleted)| deleted) && outbox_rows(&store).await == 0
    })
    .await;
    assert!(!snapshot(&store, doc_id).await.exists);
    assert_eq!(
        count(
            &store,
            &format!("SELECT COUNT(*) FROM tombstones WHERE doc_id = '{doc_id}'")
        )
        .await,
        1
    );
    engine.stop().await;
}

#[tokio::test]
async fn update_after_local_delete_is_refused_and_the_delete_uploads() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("synced", || async { outbox_rows(&store).await == 0 }).await;
    store.delete_document(ME, doc_id).await.unwrap();
    assert!(matches!(
        store.update_document(ME, doc_id, json!({"n": 2})).await,
        Err(StoreError::NotFound(id)) if id == doc_id
    ));
    engine.notify_outbox();
    eventually("the delete is uploaded and settled", || async {
        server.doc(doc_id).is_some_and(|(_, _, deleted)| deleted) && outbox_rows(&store).await == 0
    })
    .await;
    engine.stop().await;
}

#[tokio::test]
async fn offline_create_then_delete_uploads_one_delete_and_tombstones_at_zero() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let store = Store::open(&path).await.unwrap();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    store.delete_document(ME, doc_id).await.unwrap();
    store.close().await;
    let (engine, _events) = live_engine(&server, &path).await;
    let store = engine.store();
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    let uploads = server.uploads_for(doc_id);
    assert_eq!(uploads.len(), 1);
    assert_eq!(
        uploads[0]["kind"], "delete",
        "a create never sent is not sent now"
    );
    assert!(!snapshot(&store, doc_id).await.exists);
    assert_eq!(
        count(
            &store,
            &format!(
                "SELECT COUNT(*) FROM tombstones WHERE doc_id = '{doc_id}' AND server_seq = 0"
            )
        )
        .await,
        1,
        "not_found on the delete settles locally with a tombstone at seq 0"
    );
    engine.stop().await;
}

#[tokio::test]
async fn app_and_daw_on_one_data_dir_upload_an_edit_once() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (app, _app_events) = live_engine(&server, &path).await;
    let (daw, _daw_events) = live_engine(&server, &path).await;
    let doc_id = app
        .store()
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    app.notify_outbox();
    let store = daw.store();
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        server.commits_for(doc_id),
        1,
        "both engines may upload; the server applies once"
    );
    assert_eq!(outbox_rows(&store).await, 0);
    app.stop().await;
    daw.stop().await;
}

#[tokio::test]
async fn lost_create_reply_across_a_reconnect_settles_without_conflict() {
    // An empty own scope resyncs by snapshot, which carries no upload id; the equal-content
    // rule adopts the landed create instead of recovering an identical copy.
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    server.drop_after_next_upload();
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"items": ["a"]}))
        .await
        .unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "the dropped connection", |event| {
        *event == EngineEvent::Lifecycle(Lifecycle::ConnectionLost)
    })
    .await;
    jump(Duration::from_millis(1100)).await;
    eventually("settled after the resync", || async {
        outbox_rows(&store).await == 0
    })
    .await;
    assert_eq!(server.commits_for(doc_id), 1);
    assert_eq!(
        recovered_rows(&store, doc_id).await,
        0,
        "nothing was in conflict"
    );
    assert_eq!(conflicts(&mut events), 0);
    engine.stop().await;
}

#[tokio::test]
async fn returning_user_offline_edits_rebase_onto_a_changed_snapshot() {
    let server = ScriptedServer::start(ME).await;
    let doc_id = Uuid::from_u128(0xD1);
    server.put_doc(doc_id, json!({"a": 1, "b": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    engine.stop().await;

    // Months offline: an edit that was never sent, while another device changed the document
    // and the server trimmed the log past this data dir's cursor.
    let store = Store::open(&path).await.unwrap();
    store
        .update_document(ME, doc_id, json!({"a": 2, "b": 1}))
        .await
        .unwrap();
    store.close().await;
    server.put_doc(doc_id, json!({"a": 1, "b": 2}));
    server.trim_log_through(server.head());

    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    eventually("the offline edit is uploaded", || async {
        outbox_rows(&store).await == 0
    })
    .await;
    assert_eq!(
        server_content(&server, doc_id),
        Some(json!({"a": 2, "b": 2}))
    );
    assert_eq!(
        snapshot(&store, doc_id).await.content,
        json!({"a": 2, "b": 2})
    );
    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    assert_eq!(
        conflicts(&mut events),
        0,
        "a returning user sees no conflict"
    );
    engine.stop().await;
}

#[tokio::test]
async fn an_edit_from_another_device_supersedes_an_offline_delete() {
    let server = ScriptedServer::start(ME).await;
    let doc_id = Uuid::from_u128(0xDE1);
    server.put_doc(doc_id, json!({"title": "Scale", "n": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    engine.stop().await;

    // Deleted offline here while another device edits the document.
    let store = Store::open(&path).await.unwrap();
    store.delete_document(ME, doc_id).await.unwrap();
    store.close().await;
    server.put_doc(doc_id, json!({"title": "Scale", "n": 2}));

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    let store = engine.store();
    wait_for(&mut events, "DeleteSuperseded", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::DeleteSuperseded,
            })
    })
    .await;
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    let after = snapshot(&store, doc_id).await;
    assert!(
        after.exists && !after.soft_deleted,
        "the newer version is back"
    );
    assert_eq!(after.content, json!({"title": "Scale", "n": 2}));
    assert!(
        !server.doc(doc_id).unwrap().2,
        "the other device's edit survives"
    );
    assert_eq!(recovered_rows(&store, doc_id).await, 0);
    engine.stop().await;
}

#[tokio::test]
async fn an_offline_edit_then_delete_superseded_by_another_device_keeps_the_edit() {
    let server = ScriptedServer::start(ME).await;
    let doc_id = Uuid::from_u128(0xDE3);
    server.put_doc(doc_id, json!({"title": "Scale", "n": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    engine.stop().await;

    let edited = json!({"title": "Scale", "n": 1, "note": "mine"});
    let store = Store::open(&path).await.unwrap();
    store
        .update_document(ME, doc_id, edited.clone())
        .await
        .unwrap();
    store.delete_document(ME, doc_id).await.unwrap();
    store.close().await;
    server.put_doc(doc_id, json!({"title": "Scale", "n": 2}));

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    let store = engine.store();
    wait_for(&mut events, "DeleteSuperseded", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::DeleteSuperseded,
            })
    })
    .await;
    eventually("settled", || async { outbox_rows(&store).await == 0 }).await;
    let after = snapshot(&store, doc_id).await;
    assert!(after.exists && !after.soft_deleted);
    assert_eq!(after.content, json!({"title": "Scale", "n": 2}));
    assert!(!server.doc(doc_id).unwrap().2);
    let kept: Vec<_> = store
        .list_recovered()
        .await
        .unwrap()
        .into_iter()
        .filter(|copy| copy.doc_id == doc_id)
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].content, edited);
    assert_eq!(kept[0].reason, "delete_superseded");
    engine.stop().await;
}

#[tokio::test]
async fn a_delete_on_a_stale_version_is_refused_and_can_be_repeated() {
    let server = ScriptedServer::start(ME).await;
    let doc_id = Uuid::from_u128(0xDE2);
    server.put_doc(doc_id, json!({"n": 1}));
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    assert!(snapshot(&store, doc_id).await.exists);
    // Another device's edit this client has not heard of yet.
    server.drop_next_push();
    server.put_doc(doc_id, json!({"n": 2}));
    store.delete_document(ME, doc_id).await.unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "DeleteSuperseded", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::DeleteSuperseded,
            })
    })
    .await;
    let uploads = server.uploads_for(doc_id);
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0]["kind"], "delete");
    assert!(
        uploads[0]["base_hash"].is_string(),
        "the delete names the version it was made on"
    );
    assert!(!server.doc(doc_id).unwrap().2);
    assert_eq!(snapshot(&store, doc_id).await.content, json!({"n": 2}));

    store.delete_document(ME, doc_id).await.unwrap();
    engine.notify_outbox();
    eventually("the repeated delete lands", || async {
        server.doc(doc_id).is_some_and(|(_, _, deleted)| deleted) && outbox_rows(&store).await == 0
    })
    .await;
    assert!(!snapshot(&store, doc_id).await.exists);
    engine.stop().await;
}

#[tokio::test]
async fn a_skewed_clock_joins_on_the_second_try_without_an_error() {
    let server = ScriptedServer::start(ME).await;
    server.clock_ahead_by(3600);
    let (_dir, path) = seeded_db(ME, true).await;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    wait_for(&mut events, "SyncCompleted", |event| {
        assert!(
            !matches!(event, EngineEvent::Lifecycle(Lifecycle::SyncError { code, .. }) if code == "clock_skew"),
            "a clock the server corrected is not an error"
        );
        *event == EngineEvent::Lifecycle(Lifecycle::SyncCompleted)
    })
    .await;
    let joins: Vec<i64> = server
        .frames()
        .into_iter()
        .filter(|frame| frame.event == "phx_join")
        .map(|frame| frame.payload["timestamp"].as_i64().unwrap())
        .collect();
    assert_eq!(joins.len(), 2, "one skewed join, one re-signed");
    assert!(
        (joins[1] - joins[0] - 3600).abs() <= 5,
        "the second join is signed with the server's clock: {joins:?}"
    );
    assert_eq!(server.stats.upgrades(), 1, "on the same socket");
    engine.stop().await;
}

#[tokio::test]
async fn a_one_off_divergent_reply_is_corrected_by_one_more_upload() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the create settles", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    server.stamp_updates(1);
    store
        .update_document(ME, doc_id, json!({"n": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the correction settles", || async {
        server.uploads_for(doc_id).len() == 3 && in_sync(&store, &server, doc_id).await
    })
    .await;

    assert_eq!(server_content(&server, doc_id), Some(json!({"n": 2})));
    assert_eq!(snapshot(&store, doc_id).await.content, json!({"n": 2}));
    let uploads = server.uploads_for(doc_id);
    assert_eq!(uploads.len(), 3, "create, the stamped edit, one correction");
    assert_eq!(
        uploads[2]["payload"],
        json!([{"op": "remove", "path": "/stamped"}])
    );
    engine.stop().await;
}

#[tokio::test]
async fn a_persistent_divergent_reply_parks_the_document_after_a_bounded_number_of_uploads() {
    let server = ScriptedServer::start(ME).await;
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = live_engine(&server, &path).await;
    let store = engine.store();
    let doc_id = store
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    eventually("the create settles", || async {
        in_sync(&store, &server, doc_id).await
    })
    .await;

    server.stamp_updates(u32::MAX);
    store
        .update_document(ME, doc_id, json!({"n": 2}))
        .await
        .unwrap();
    engine.notify_outbox();
    wait_for(&mut events, "the park", |event| {
        *event
            == EngineEvent::Doc(DocNotice {
                doc_id,
                event: DocEvent::SyncError {
                    code: "diverged".into(),
                },
            })
    })
    .await;
    let bound = 1 + MAX_DIVERGENT_REPLIES as usize + 1;
    assert_eq!(server.uploads_for(doc_id).len(), bound);
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM outbox WHERE parked_error = 'diverged'"
        )
        .await,
        1
    );
    assert_eq!(snapshot(&store, doc_id).await.content, json!({"n": 2}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        server.uploads_for(doc_id).len(),
        bound,
        "no uploads after the park"
    );
    engine.stop().await;
}
