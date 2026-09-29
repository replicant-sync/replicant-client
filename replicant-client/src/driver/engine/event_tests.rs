use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use super::{Engine, EngineEvent};
use crate::driver::test_support::{config, no_credentials, seeded_db};
use crate::engine::types::SCOPE_OWN;
use crate::store::change_log::{ChangeOrigin, DocChange};
use crate::store::test_support::{count, envelope, upsert_change, ME};
use crate::store::Store;

/// An engine that never touches the network (halted as not enrolled).
async fn offline_engine(path: &Path) -> (Engine, mpsc::UnboundedReceiver<EngineEvent>) {
    let (events_tx, events) = mpsc::unbounded_channel();
    let engine = Engine::start(
        path,
        config("ws://127.0.0.1:9", no_credentials()),
        events_tx,
    )
    .await
    .unwrap();
    (engine, events)
}

/// Every `Changed` event for `doc_id` within `window` of real time.
async fn changes_for(
    events: &mut mpsc::UnboundedReceiver<EngineEvent>,
    doc_id: Uuid,
    window: Duration,
) -> Vec<DocChange> {
    let deadline = tokio::time::Instant::now() + window;
    let mut changes = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let EngineEvent::Changed(change) = event {
            if change.doc_id == doc_id {
                changes.push(change);
            }
        }
    }
    changes
}

#[tokio::test]
async fn own_write_is_emitted_once_as_local() {
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, mut events) = offline_engine(&path).await;
    let doc_id = engine
        .store()
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    engine.notify_outbox();
    assert_eq!(
        changes_for(&mut events, doc_id, Duration::from_millis(800)).await,
        vec![DocChange {
            doc_id,
            deleted: false,
            origin: ChangeOrigin::Local
        }]
    );
    engine.stop().await;
}

#[tokio::test]
async fn other_process_edit_is_emitted_within_a_tick() {
    let (_dir, path) = seeded_db(ME, true).await;
    let (app, mut app_events) = offline_engine(&path).await;
    let (daw, _daw_events) = offline_engine(&path).await;
    let doc_id = daw
        .store()
        .create_document(ME, None, json!({"n": 1}))
        .await
        .unwrap();
    // No notify reaches the app: only its change-log tick can see the other process's write.
    assert_eq!(
        changes_for(&mut app_events, doc_id, Duration::from_secs(1)).await,
        vec![DocChange {
            doc_id,
            deleted: false,
            origin: ChangeOrigin::OtherProcess
        }]
    );
    app.stop().await;
    daw.stop().await;
}

#[tokio::test]
async fn server_write_by_either_process_is_reported_as_server() {
    let (_dir, path) = seeded_db(ME, true).await;
    let (app, mut app_events) = offline_engine(&path).await;
    let (daw, mut daw_events) = offline_engine(&path).await;
    let doc_id = Uuid::from_u128(0xD1);
    let push = upsert_change(SCOPE_OWN, envelope(doc_id, Some(ME), json!({"n": 1}), 5));
    // Both processes receive the same push; the second apply is a no-op (content guard).
    daw.store()
        .apply_changes(ME, SCOPE_OWN, std::slice::from_ref(&push), 5)
        .await
        .unwrap();
    app.store()
        .apply_changes(ME, SCOPE_OWN, &[push], 5)
        .await
        .unwrap();
    let from_server = DocChange {
        doc_id,
        deleted: false,
        origin: ChangeOrigin::Server,
    };
    assert_eq!(
        changes_for(&mut app_events, doc_id, Duration::from_secs(1)).await,
        vec![from_server.clone()]
    );
    assert_eq!(
        changes_for(&mut daw_events, doc_id, Duration::from_secs(1)).await,
        vec![from_server]
    );
    app.stop().await;
    daw.stop().await;
}

#[tokio::test]
async fn reader_heartbeat_uses_wall_clock_seconds() {
    let (_dir, path) = seeded_db(ME, true).await;
    let (engine, _events) = offline_engine(&path).await;
    let heartbeat = count(
        &engine.store(),
        "SELECT MAX(heartbeat_at) FROM change_log_readers",
    )
    .await;
    let now = chrono::Utc::now().timestamp();
    assert!(
        (now - heartbeat).abs() <= 5,
        "reader heartbeat {heartbeat} is not in wall-clock seconds (now {now})"
    );
    engine.stop().await;
}

#[tokio::test]
async fn engine_start_stop_cycles_leave_no_reader_rows() {
    let (_dir, path) = seeded_db(ME, true).await;
    for _ in 0..5 {
        let (engine, _events) = offline_engine(&path).await;
        timeout(Duration::from_secs(2), engine.stop())
            .await
            .expect("a stop is bounded");
    }
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM change_log_readers").await,
        0,
        "a stopped engine's reader would hold back change-log trimming"
    );
    store.close().await;
}
