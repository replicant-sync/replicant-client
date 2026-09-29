use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{Engine, EngineEvent};
use crate::driver::test_server::ScriptedServer;
use crate::driver::test_support::{config, credentials, eventually, jump, seeded_db, wait_for};
use crate::engine::doc::DocEvent;
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
            event: DocEvent::ConflictDetected,
            ..
        }) = event
        {
            conflicts += 1;
        }
    }
    conflicts
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
async fn float_round_trip_converges_with_at_most_one_extra_upload() {
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
    eventually("settled after the reconnect", || async {
        outbox_rows(&store).await == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let uploads = server.uploads_for(doc_id);
    assert!(
        uploads.len() <= 2,
        "more than one extra upload: {uploads:#?}"
    );
    assert_eq!(outbox_rows(&store).await, 0);
    assert_eq!(snapshot(&store, doc_id).await.content, json!({"n": 1200.0}));
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
        server_content(&server, doc_id),
        Some(json!({"items": ["a", "b"]}))
    );
    assert_eq!(
        recovered_rows(&store, doc_id).await,
        0,
        "nothing was in conflict"
    );
    assert_eq!(conflicts(&mut events), 0);
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
