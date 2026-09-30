use std::cell::Cell;
use std::time::Duration;

use serde_json::json;
use sqlx::migrate::MigrateError;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use super::owner_support::{connection, harness, is_live, Harness};
use super::{retry_open, twice, Command, Engine, EngineError, Queued, LOG_TICK};
use crate::driver::test_server::{Mode, ScriptedServer};
use crate::driver::test_support::{
    config, credentials, eventually, jump, no_credentials, seeded_db, SwitchableCredentials, WAIT,
};
use crate::engine::list_merge::{ListMergePolicy, PathPattern};
use crate::engine::machine::{
    ConnectionView, HaltReason, Input, Lifecycle, SettleOutcome, TimerId,
};
use crate::store::test_support::{count, exec, v012_db, v1_doc, v1_queue_row, v1_user, ME};
use crate::store::StoreError;
use crate::transport::socket::SocketEvent;
use crate::transport::wire::user_agent;

const OTHER: Uuid = Uuid::from_u128(0xB);

/// Drops every socket from the server side and waits for the reconnect (backoff ≤ 2 s here).
async fn reconnect_after_drop(h: &mut Harness, server: &ScriptedServer) {
    server.drop_connections();
    h.turn_until("disconnected", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    jump(Duration::from_millis(2100)).await;
    h.turn_until("live again", is_live).await;
}

#[tokio::test]
async fn effects_run_in_order_and_answers_are_fifo() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let first = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    let second = h
        .controls
        .store
        .create_document(None, json!({"n": 2}))
        .await
        .unwrap();
    h.owner
        .apply_step(Input::PendingDocs(vec![first, second]))
        .await;
    let gen = h.owner.socket_gen;
    let answered: Vec<(Option<u64>, Uuid)> = h
        .owner
        .queue
        .iter()
        .map(|queued| match &queued.input {
            Input::UploadBuilt { doc_id, .. } => (queued.epoch, *doc_id),
            other => panic!("unexpected answer {other:?}"),
        })
        .collect();
    assert_eq!(answered, vec![(Some(gen), first), (Some(gen), second)]);
}

#[tokio::test]
async fn send_failure_mid_step_is_fed_after_the_steps_effects() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    server.drop_connections();
    // Take the socket's Closed event off the channel without giving it to the core, as when
    // the core sends before the owner has read the event.
    loop {
        let event = timeout(WAIT, h.owner.connection.next_event())
            .await
            .expect("the socket reports its end");
        if matches!(event, SocketEvent::Closed { .. }) {
            break;
        }
    }
    timeout(WAIT, async {
        while !h.owner.connection.socket_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the socket task ends");
    h.owner.feed(Input::Timer(TimerId::Heartbeat)).await;
    assert_eq!(connection(&h.owner), ConnectionView::Disconnected);
    assert!(
        !h.owner.timers.is_scheduled(&TimerId::Heartbeat),
        "the step's own Schedule ran before the SocketClosed it caused"
    );
    assert!(h.owner.timers.is_scheduled(&TimerId::Reconnect));
}

#[tokio::test]
async fn joined_is_fed_only_after_the_identity_check() {
    let previous = Uuid::from_u128(0x5EED);
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), previous, false).await;
    let doc_id = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    h.live().await;
    assert_eq!(h.owner.me, ME);
    assert_eq!(h.controls.store.user_id().await.unwrap(), ME);
    let restamped = count(
        &h.controls.store,
        &format!("SELECT COUNT(*) FROM documents WHERE id = '{doc_id}' AND user_id = '{ME}'"),
    )
    .await;
    assert_eq!(restamped, 1);
    assert_eq!(
        server.user_agents(),
        vec![Some(user_agent("Test Host", "1.0"))]
    );
}

#[tokio::test]
async fn identity_drift_halts_without_retry() {
    let server = ScriptedServer::start(OTHER).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.start().await;
    h.turn_until("halted", |o| {
        matches!(connection(o), ConnectionView::Halted(_))
    })
    .await;
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::Other("identity_drift".into()))
    );
    assert!(h.lifecycles().contains(&Lifecycle::SyncError {
        code: "identity_drift".into(),
        scope: None,
        doc_id: None,
        fatal: true
    }));
    jump(Duration::from_secs(301)).await;
    h.turns(4).await;
    assert_eq!(
        server.stats.upgrades(),
        1,
        "a drifted identity never redials by itself"
    );
    assert_eq!(h.controls.store.user_id().await.unwrap(), ME);
}

#[tokio::test]
async fn new_credentials_apply_to_the_next_join_only() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.live().await;
    keys.set("k2");
    assert!(h
        .controls
        .commands
        .try_send(Command::CredentialsChanged)
        .is_ok());
    h.turns(2).await;
    assert!(
        is_live(&h.owner),
        "a credential change never replaces the open socket"
    );
    assert_eq!(server.stats.upgrades(), 1);
    reconnect_after_drop(&mut h, &server).await;
    assert_eq!(server.join_keys(), vec!["k1".to_string(), "k2".to_string()]);
}

#[tokio::test]
async fn halted_auth_invalid_does_not_redial_with_the_same_credentials() {
    let server = ScriptedServer::start(ME).await;
    server.reject_join("auth_invalid");
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    h.turn_until("halted", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::AuthInvalid)
    })
    .await;
    for _ in 0..5 {
        jump(Duration::from_millis(3100)).await;
        h.turns(3).await;
    }
    assert_eq!(
        server.stats.upgrades(),
        1,
        "the 3 s check must not redial with credentials the server rejected"
    );
    assert!(h.owner.timers.is_scheduled(&TimerId::HaltRetry));
    keys.set("k2");
    server.accept_joins();
    jump(Duration::from_millis(3100)).await;
    h.turn_until("live with new credentials", is_live).await;
    assert_eq!(server.join_keys(), vec!["k1".to_string(), "k2".to_string()]);
}

#[tokio::test]
async fn credentials_changed_while_a_join_is_outstanding_rejects_the_signer_not_the_new_ones() {
    // A silent server never auto-replies, so the join stays genuinely outstanding: turning
    // past "the join is sent" cannot also race past its (nonexistent) reply, unlike a live
    // scripted server, which answers fast enough that both would land in the same turn.
    let server = ScriptedServer::start(ME).await;
    server.set_mode(Mode::SilentAfterUpgrade);
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    h.turn_until("the join is sent", |_| server.frames().len() == 1)
        .await;
    keys.set("k2");
    assert!(h
        .controls
        .commands
        .try_send(Command::CredentialsChanged)
        .is_ok());
    h.turns(2).await;
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Connecting,
        "still waiting on the join k1 signed; the command must not touch it"
    );
    // Reject that outstanding, k1-signed join (req 1, the core's first request ever).
    let gen = h.owner.socket_gen;
    let rejection = json!([
        null,
        "1",
        "sync:v2",
        "phx_reply",
        {"status": "error", "response": {"code": "auth_invalid", "is_fatal": true}}
    ])
    .to_string();
    h.owner.connection.inject(SocketEvent::Text {
        gen,
        text: rejection,
    });
    h.turn_until("halted", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::AuthInvalid)
    })
    .await;
    server.set_mode(Mode::Normal);
    jump(Duration::from_secs(301)).await;
    h.turn_until("live with k2", is_live).await;
    assert_eq!(
        server.join_keys(),
        vec!["k1".to_string(), "k2".to_string()],
        "k1 signed the rejected join, so only k1 is remembered as rejected"
    );
}

#[tokio::test]
async fn sign_out_while_live_halts_and_never_joins_with_the_old_credentials() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.live().await;
    keys.sign_out();
    assert!(h
        .controls
        .commands
        .try_send(Command::CredentialsChanged)
        .is_ok());
    h.turn_until("halted as not enrolled", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::NotEnrolled)
    })
    .await;
    jump(Duration::from_secs(2)).await;
    h.turn_until("the dial cooldown has passed", |o| {
        !o.timers.is_scheduled(&TimerId::DialCooldown)
    })
    .await;
    assert!(h.controls.commands.try_send(Command::Reconnect).is_ok());
    h.turn_until("one more dial, halted before its join", |o| {
        server.stats.upgrades() == 2
            && connection(o) == ConnectionView::Halted(HaltReason::NotEnrolled)
    })
    .await;
    assert_eq!(
        server.join_keys(),
        vec!["k1".to_string()],
        "nothing joined after the sign-out"
    );
    keys.set("k2");
    assert!(h
        .controls
        .commands
        .try_send(Command::CredentialsChanged)
        .is_ok());
    h.turn_until("live with the new credentials", is_live).await;
    assert_eq!(server.join_keys(), vec!["k1".to_string(), "k2".to_string()]);
}

#[tokio::test]
async fn a_dial_after_the_stored_credentials_are_gone_halts_without_joining() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.live().await;
    // Cleared by another process: this engine is never told.
    keys.sign_out();
    server.drop_connections();
    h.turn_until("disconnected", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    jump(Duration::from_millis(2100)).await;
    h.turn_until("halted at the next dial", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::NotEnrolled)
    })
    .await;
    assert_eq!(server.join_keys(), vec!["k1".to_string()]);
    assert_eq!(
        server.stats.upgrades(),
        2,
        "the dial happened, the join did not"
    );
}

#[tokio::test]
async fn unreadable_credentials_are_retried_and_never_read_as_a_sign_out() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.live().await;
    keys.set_unreadable(true);
    server.drop_connections();
    jump(Duration::from_millis(2100)).await;
    h.turn_until("a dial abandoned before its join, backing off", |o| {
        server.stats.upgrades() == 2 && connection(o) == ConnectionView::Disconnected
    })
    .await;
    assert_eq!(server.join_keys(), vec!["k1".to_string()]);
    keys.set_unreadable(false);
    jump(Duration::from_secs(10)).await;
    h.turn_until("live again with k1", is_live).await;
    assert_eq!(server.join_keys(), vec!["k1".to_string(), "k1".to_string()]);
}

#[tokio::test]
async fn a_sign_out_in_another_process_ends_a_live_connection_within_a_second() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.live().await;
    // This engine is never told. The tick count at sign-out is unknown, so the halt must land
    // within a second of ticks.
    keys.sign_out();
    for _ in 0..(1000 / LOG_TICK.as_millis()) {
        jump(LOG_TICK).await;
        h.turns(2).await;
    }
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::NotEnrolled)
    );
}

#[tokio::test]
async fn a_sign_in_in_another_process_is_picked_up_within_three_seconds() {
    let server = ScriptedServer::start(ME).await;
    let keys = SwitchableCredentials::new("k1");
    keys.sign_out();
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::NotEnrolled)
    );
    // Stored by another process: this engine is never told.
    keys.set("k1");
    for _ in 0..(3250 / LOG_TICK.as_millis()) {
        jump(LOG_TICK).await;
        h.turns(2).await;
    }
    assert_ne!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::NotEnrolled)
    );
    h.turn_until("live", is_live).await;
    assert_eq!(server.join_keys(), vec!["k1".to_string()]);
}

#[tokio::test]
async fn nothing_but_the_join_is_sent_while_connecting() {
    let server = ScriptedServer::start(ME).await;
    server.set_mode(Mode::SilentAfterUpgrade);
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.start().await;
    h.turn_until("the join is sent", |_| server.frames().len() == 1)
        .await;
    h.controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    h.controls.outbox.notify_one();
    h.turns(4).await;
    jump(Duration::from_secs(16)).await;
    h.turn_until("the connect timeout", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    jump(Duration::from_secs(2)).await;
    h.turn_until("the second join is sent", |_| server.frames().len() == 2)
        .await;
    h.turns(4).await;
    let sent: Vec<(usize, String)> = server
        .frames()
        .into_iter()
        .map(|frame| (frame.connection, frame.event))
        .collect();
    assert_eq!(
        sent,
        vec![(0, "phx_join".to_string()), (1, "phx_join".to_string())]
    );
}

#[tokio::test]
async fn heartbeat_reply_wins_over_a_timeout_due_at_the_same_instant() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    jump(Duration::from_secs(30)).await;
    // req 6 = join + four feeds; DialCooldown is still pending when `live()` returns, and
    // waiting on frames also takes in the heartbeat reply, so wait on the owner's own state.
    h.turn_until("a heartbeat is sent", |o| {
        o.timers.is_scheduled(&TimerId::HeartbeatTimeout(6))
    })
    .await;
    timeout(WAIT, async {
        while h.owner.connection.pending_events() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the heartbeat reply arrives");
    // The reply is queued and the heartbeat timeout is now due as well.
    jump(Duration::from_secs(30)).await;
    h.turns(4).await;
    assert!(is_live(&h.owner));
    assert!(!h.lifecycles().contains(&Lifecycle::ConnectionLost));
}

#[tokio::test]
async fn stale_settle_after_reconnect_is_dropped() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let stale_gen = h.owner.socket_gen;
    reconnect_after_drop(&mut h, &server).await;
    let doc_id = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    let settled = Input::Settled {
        doc_id,
        outcome: SettleOutcome::Done {
            rows_remain: true,
            diverged: false,
        },
    };
    h.owner.queue.push_back(Queued {
        epoch: Some(stale_gen),
        input: settled.clone(),
    });
    h.owner.drain().await;
    assert_eq!(
        h.owner.dropped_stale, 1,
        "an answer from the previous socket never reaches the core"
    );
    let current = Some(h.owner.socket_gen);
    h.owner.queue.push_back(Queued {
        epoch: current,
        input: settled,
    });
    h.owner.drain().await;
    assert_eq!(
        h.owner.dropped_stale, 1,
        "the current socket's answer is stepped"
    );
}

#[tokio::test]
async fn stale_generation_frames_never_reach_the_core() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let old_gen = h.owner.socket_gen;
    reconnect_after_drop(&mut h, &server).await;
    let phx_close = json!(["1", null, "sync:v2", "phx_close", {}]).to_string();
    h.owner.connection.inject(SocketEvent::Text {
        gen: old_gen,
        text: phx_close,
    });
    h.owner
        .connection
        .inject(SocketEvent::Closed { gen: old_gen });
    h.turns(3).await;
    assert!(is_live(&h.owner));
    assert!(h.owner.connection.has_socket());
    assert_eq!(h.owner.socket_gen, old_gen + 1);
}

#[tokio::test]
async fn apply_db_error_retries_then_forces_reconnect() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    exec(&h.controls.store, "DROP TABLE doc_scopes").await;
    server.put_doc(Uuid::from_u128(0xD1), json!({"n": 1}));
    h.turn_until("the connection is dropped", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    assert!(h.lifecycles().contains(&Lifecycle::ConnectionLost));
    assert!(!h.owner.connection.has_socket());
    eventually("the server sees the socket close", || async {
        server.stats.live() == 0
    })
    .await;
}

#[tokio::test]
async fn cancel_stops_without_waiting_on_the_network() {
    let server = ScriptedServer::start(ME).await;
    server.set_mode(Mode::SilentAfterUpgrade);
    let (_dir, path) = seeded_db(ME, true).await;
    let (events_tx, _events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    eventually("the join is sent", || async { !server.frames().is_empty() }).await;
    timeout(Duration::from_secs(1), engine.stop())
        .await
        .expect("stop never waits on the network");
    eventually("the socket is closed", || async {
        server.stats.live() == 0
    })
    .await;
}

#[tokio::test]
async fn twice_retries_a_failed_feed_effect_once_then_succeeds() {
    let attempts = Cell::new(0);
    let result = twice(|| {
        attempts.set(attempts.get() + 1);
        let attempt = attempts.get();
        async move {
            if attempt == 1 {
                Err(StoreError::NoUserConfig)
            } else {
                Ok(attempt)
            }
        }
    })
    .await;
    assert_eq!(result.unwrap(), 2);
    assert_eq!(
        attempts.get(),
        2,
        "exactly one retry after the first failure"
    );
}

#[tokio::test(start_paused = true)]
async fn an_open_race_is_retried_until_the_fourth_attempt() {
    let attempts = Cell::new(0);
    let opened = retry_open(|| {
        attempts.set(attempts.get() + 1);
        let attempt = attempts.get();
        async move {
            if attempt < 4 {
                Err(StoreError::Migrate(MigrateError::Dirty(13)))
            } else {
                Ok(attempt)
            }
        }
    })
    .await;
    assert_eq!(opened.unwrap(), 4);
    let failing: Result<u32, StoreError> =
        retry_open(|| async { Err(StoreError::Migrate(MigrateError::Dirty(13))) }).await;
    assert!(failing.is_err(), "four attempts, then the error");
}

#[tokio::test(start_paused = true)]
async fn a_newer_schema_is_not_retried() {
    let attempts = Cell::new(0);
    let opened: Result<u32, StoreError> = retry_open(|| {
        attempts.set(attempts.get() + 1);
        async { Err(StoreError::Migrate(MigrateError::VersionMissing(99))) }
    })
    .await;
    assert!(opened.unwrap_err().is_newer_schema());
    assert_eq!(attempts.get(), 1);
}

#[tokio::test]
async fn other_open_errors_are_not_retried() {
    let attempts = Cell::new(0);
    let opened: Result<u32, StoreError> = retry_open(|| {
        attempts.set(attempts.get() + 1);
        async { Err(StoreError::NoUserConfig) }
    })
    .await;
    assert!(matches!(opened, Err(StoreError::NoUserConfig)));
    assert_eq!(attempts.get(), 1);
}

#[tokio::test]
async fn a_busy_open_is_retried() {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Connection};

    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("locked.sqlite3"))
        .create_if_missing(true)
        .busy_timeout(Duration::from_millis(1));
    let mut lock_holder = options.connect().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut lock_holder)
        .await
        .unwrap();
    let lock_holder = tokio::sync::Mutex::new(Some(lock_holder));
    let attempts = Cell::new(0);
    let busy_errors = Cell::new(0);
    let opened = retry_open(|| {
        attempts.set(attempts.get() + 1);
        let (lock_holder, options, busy_errors) = (&lock_holder, &options, &busy_errors);
        let release_lock = attempts.get() == 2;
        async move {
            if release_lock {
                let mut released = lock_holder.lock().await.take().unwrap();
                sqlx::query("ROLLBACK").execute(&mut released).await?;
            }
            let mut contender = options.connect().await?;
            let locked = sqlx::query("BEGIN IMMEDIATE").execute(&mut contender).await;
            contender.close().await?;
            locked.map(|_| ()).map_err(|error| {
                let error = StoreError::from(error);
                assert!(error.is_busy(), "{error}");
                busy_errors.set(busy_errors.get() + 1);
                error
            })
        }
    })
    .await;
    opened.unwrap();
    assert_eq!(attempts.get(), 2);
    assert_eq!(busy_errors.get(), 1);
}

#[tokio::test]
async fn start_creates_the_user_config_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replicant.sqlite3");
    let (events_tx, _events) = mpsc::unbounded_channel();
    let engine = Engine::start(
        &path,
        config("ws://127.0.0.1:9", no_credentials()),
        events_tx,
    )
    .await
    .unwrap();
    engine.store().user_id().await.unwrap();
    assert_eq!(
        count(&engine.store(), "SELECT COUNT(*) FROM user_config").await,
        1
    );
    engine.stop().await;
}

#[tokio::test]
async fn a_bad_server_url_is_refused_before_the_database_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replicant.sqlite3");
    let (events_tx, _events) = mpsc::unbounded_channel();
    let started = Engine::start(
        &path,
        config("wss://sync.example.com/?vsn=1", no_credentials()),
        events_tx,
    )
    .await;
    assert!(matches!(started, Err(EngineError::Config(_))));
    assert!(!path.exists(), "nothing opened, nothing migrated");
}

#[tokio::test]
async fn silent_server_is_detected_by_the_heartbeat_timeout() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    server.set_mode(Mode::SilentAfterUpgrade);
    jump(Duration::from_secs(30)).await;
    h.turn_until("a heartbeat is sent", |_| {
        server
            .frames()
            .iter()
            .any(|frame| frame.event == "heartbeat")
    })
    .await;
    jump(Duration::from_secs(30)).await;
    h.turn_until("the heartbeat timeout disconnects", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    assert!(h.lifecycles().contains(&Lifecycle::ConnectionLost));
}

#[tokio::test]
async fn a_new_users_empty_snapshot_still_unlocks_uploads() {
    let server = ScriptedServer::start(ME).await;
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let doc_id = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    h.controls.outbox.notify_one();
    h.turn_until("the upload reaches the server", |_| {
        server.doc(doc_id).is_some()
    })
    .await;
}

#[tokio::test]
async fn upload_is_marked_sent_before_it_is_sent() {
    let server = ScriptedServer::start(ME).await;
    server.hold_uploads();
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let doc_id = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    h.controls.outbox.notify_one();
    h.turn_until("the upload reaches the server", |_| {
        server.uploads_for(doc_id).len() == 1
    })
    .await;
    let upload_id = server.uploads_for(doc_id)[0]["upload_id"]
        .as_str()
        .unwrap()
        .to_string();
    let marked = count(
        &h.controls.store,
        &format!(
            "SELECT COUNT(*) FROM outbox WHERE doc_id = '{doc_id}' AND sent_upload_id = '{upload_id}'"
        ),
    )
    .await;
    assert_eq!(
        marked, 1,
        "a held (unacknowledged) upload's rows are marked sent"
    );
}

/// A live owner with one unsent document; nothing has asked the owner to upload it yet.
async fn live_with_unsent_doc(server: &ScriptedServer) -> (Harness, Uuid) {
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.live().await;
    let doc_id = h
        .controls
        .store
        .create_document(None, json!({"n": 1}))
        .await
        .unwrap();
    (h, doc_id)
}

#[tokio::test]
async fn failed_build_backs_the_doc_off_and_uploads_on_retry() {
    let server = ScriptedServer::start(ME).await;
    let (mut h, doc_id) = live_with_unsent_doc(&server).await;
    exec(
        &h.controls.store,
        "ALTER TABLE tombstones RENAME TO tombstones_off",
    )
    .await;
    h.owner.feed(Input::PendingDocs(vec![doc_id])).await;
    assert!(h.owner.timers.is_scheduled(&TimerId::DocRetry(doc_id)));
    exec(
        &h.controls.store,
        "ALTER TABLE tombstones_off RENAME TO tombstones",
    )
    .await;
    jump(Duration::from_millis(1100)).await;
    h.turn_until("the retried upload reaches the server", |_| {
        server.doc(doc_id).is_some()
    })
    .await;
}

#[tokio::test]
async fn failed_pending_load_retries_the_pump() {
    let server = ScriptedServer::start(ME).await;
    let (mut h, doc_id) = live_with_unsent_doc(&server).await;
    exec(&h.controls.store, "ALTER TABLE outbox RENAME TO outbox_off").await;
    h.owner.feed(Input::OutboxChanged).await;
    // A fired pump cancels its cap; only the retry can schedule the pump again.
    h.turn_until("the failed pump is rescheduled", |o| {
        o.timers.is_scheduled(&TimerId::Pump) && !o.timers.is_scheduled(&TimerId::PumpCap)
    })
    .await;
    exec(&h.controls.store, "ALTER TABLE outbox_off RENAME TO outbox").await;
    jump(Duration::from_millis(1100)).await;
    h.turn_until("the retried pump's upload reaches the server", |_| {
        server.doc(doc_id).is_some()
    })
    .await;
}

#[tokio::test]
async fn failed_mark_sent_backs_the_doc_off_at_once() {
    let server = ScriptedServer::start(ME).await;
    let (mut h, doc_id) = live_with_unsent_doc(&server).await;
    exec(
        &h.controls.store,
        "CREATE TRIGGER refuse_mark BEFORE UPDATE OF sent_upload_id ON outbox \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await;
    h.owner.feed(Input::PendingDocs(vec![doc_id])).await;
    assert!(h.owner.timers.is_scheduled(&TimerId::DocRetry(doc_id)));
    assert!(server.uploads_for(doc_id).is_empty());
    exec(&h.controls.store, "DROP TRIGGER refuse_mark").await;
    jump(Duration::from_millis(1100)).await;
    h.turn_until("the retried upload reaches the server", |_| {
        server.doc(doc_id).is_some()
    })
    .await;
}

#[tokio::test]
async fn start_refuses_a_full_list_merge_policy() {
    let (_dir, path) = seeded_db(ME, true).await;
    let mut refused = config("ws://127.0.0.1:9", credentials("k1"));
    refused
        .list_merge
        .rules
        .push((PathPattern("/pitches".into()), ListMergePolicy::Full));
    let (events_tx, _events) = mpsc::unbounded_channel();
    match Engine::start(&path, refused, events_tx).await {
        Err(EngineError::Config(message)) => {
            assert_eq!(message, "Full list merge is not supported yet")
        }
        other => panic!("expected a config error, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn two_engines_starting_on_one_v1_database_both_start_and_migrate_it_once() {
    let (_dir, path, pool) = v012_db().await;
    v1_user(&pool, ME, true).await;
    let doc_id = Uuid::from_u128(0xD1).to_string();
    v1_doc(&pool, &doc_id, Some(ME), json!({"n": 1}), "pending", None).await;
    v1_queue_row(&pool, &doc_id, "update", Some(json!({"n": 0}))).await;
    pool.close().await;
    let (app_events, _app) = mpsc::unbounded_channel();
    let (daw_events, _daw) = mpsc::unbounded_channel();
    let (app, daw) = tokio::join!(
        Engine::start(
            &path,
            config("ws://127.0.0.1:9", no_credentials()),
            app_events
        ),
        Engine::start(
            &path,
            config("ws://127.0.0.1:9", no_credentials()),
            daw_events
        ),
    );
    let (app, daw) = (app.unwrap(), daw.unwrap());
    assert_eq!(
        count(&app.store(), "SELECT COUNT(*) FROM outbox").await,
        1,
        "one marker, not one per engine"
    );
    app.stop().await;
    daw.stop().await;
}

#[tokio::test]
async fn a_re_sign_in_after_auth_invalid_in_another_process_is_picked_up_within_three_seconds() {
    let server = ScriptedServer::start(ME).await;
    server.reject_join("auth_invalid");
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    h.turn_until("halted", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::AuthInvalid)
    })
    .await;
    // The user signs out and in again in another process (Studio): this engine is never told.
    keys.set("k2");
    server.accept_joins();
    for _ in 0..(3250 / LOG_TICK.as_millis()) {
        jump(LOG_TICK).await;
        h.turns(2).await;
    }
    assert_ne!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::AuthInvalid),
        "a cross-process re-sign-in after auth_invalid is not picked up within 3 s"
    );
}

#[tokio::test]
async fn a_sign_out_while_halted_auth_invalid_reports_not_enrolled() {
    let server = ScriptedServer::start(ME).await;
    server.reject_join("auth_invalid");
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    h.turn_until("halted", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::AuthInvalid)
    })
    .await;
    // replicant_clear_credentials in this process.
    keys.sign_out();
    assert!(h
        .controls
        .commands
        .try_send(Command::CredentialsChanged)
        .is_ok());
    h.turns(3).await;
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::NotEnrolled),
        "signed out, but the state still says the credentials were refused"
    );
}

#[tokio::test]
async fn a_sign_out_in_another_process_while_halted_auth_invalid_reports_not_enrolled_within_three_seconds(
) {
    let server = ScriptedServer::start(ME).await;
    server.reject_join("auth_invalid");
    let keys = SwitchableCredentials::new("k1");
    let mut h = harness(&server.url, keys.loader(), ME, true).await;
    h.start().await;
    h.turn_until("halted", |o| {
        connection(o) == ConnectionView::Halted(HaltReason::AuthInvalid)
    })
    .await;
    keys.sign_out();
    jump(Duration::from_millis(3100)).await;
    h.turns(3).await;
    assert_eq!(
        connection(&h.owner),
        ConnectionView::Halted(HaltReason::NotEnrolled)
    );
}
