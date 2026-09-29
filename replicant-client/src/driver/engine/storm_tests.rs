use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::owner_support::{connection, harness, Harness};
use super::{Command, Engine};
use crate::driver::test_server::{Mode, ScriptedServer};
use crate::driver::test_support::{config, credentials, eventually, jump, seeded_db};
use crate::engine::machine::{ConnectionView, HaltReason};
use crate::store::test_support::ME;
use crate::transport::test_server::refusing_server;

#[tokio::test]
async fn upgrade_then_immediate_drop_makes_at_most_six_attempts_in_30_s_with_one_open_socket() {
    let server = ScriptedServer::start(ME).await;
    server.set_mode(Mode::DropAfterUpgrade);
    let (_dir, path) = seeded_db(ME, true).await;
    let (events_tx, _events) = mpsc::unbounded_channel();
    let engine = Engine::start(&path, config(&server.url, credentials("k1")), events_tx)
        .await
        .unwrap();
    // Thirty one-second jumps; after each, real time lets the dial it released reach the server.
    for _ in 0..30 {
        jump(Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let upgrades = server.stats.upgrades();
    assert!(
        upgrades >= 2,
        "the engine stopped retrying after {upgrades} dials"
    );
    assert!(
        upgrades <= 6,
        "{upgrades} dials in about 31 s against a server that upgrades and drops"
    );
    assert!(server.stats.peak_live() <= 1);
    engine.stop().await;
}

/// After a failed connect: no socket left in the connection, and none open at the server.
async fn assert_no_socket_left(h: &Harness, server: &ScriptedServer) {
    assert!(
        !h.owner.connection.has_socket(),
        "the connection still holds a socket"
    );
    eventually("no open server connection", || async {
        server.stats.live() == 0
    })
    .await;
}

#[tokio::test]
async fn every_connect_failure_path_leaves_no_socket_behind() {
    // Dial error: nothing listens on the port.
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("ws://{}", unused.local_addr().unwrap());
    drop(unused);
    let mut h = harness(&dead_url, credentials("k1"), ME, true).await;
    h.start().await;
    h.turn_until("the dial error", |o| {
        connection(o) == ConnectionView::Disconnected
    })
    .await;
    assert!(!h.owner.connection.has_socket());

    // The upgrade is refused.
    for (status, expected) in [
        (
            "426 Upgrade Required",
            ConnectionView::Halted(HaltReason::UpdateRequired),
        ),
        ("403 Forbidden", ConnectionView::Disconnected),
    ] {
        let url = refusing_server(status).await;
        let mut h = harness(&url, credentials("k1"), ME, true).await;
        h.start().await;
        h.turn_until(status, |o| connection(o) == expected).await;
        assert!(!h.owner.connection.has_socket(), "{status}");
    }

    // Upgraded, then: dropped before the join; the join refused; the join never answered.
    for (mode, join_error) in [
        (Mode::DropAfterUpgrade, None),
        (Mode::Normal, Some("auth_invalid")),
        (Mode::SilentAfterUpgrade, None),
    ] {
        let server = ScriptedServer::start(ME).await;
        server.set_mode(mode);
        if let Some(code) = join_error {
            server.reject_join(code);
        }
        let mut h = harness(&server.url, credentials("k1"), ME, true).await;
        h.start().await;
        if mode == Mode::SilentAfterUpgrade {
            h.turn_until("the join is sent", |_| !server.frames().is_empty())
                .await;
            jump(Duration::from_secs(16)).await;
        }
        h.turn_until("the connect attempt ends", |o| {
            connection(o) != ConnectionView::Connecting
        })
        .await;
        assert_no_socket_left(&h, &server).await;
        assert_eq!(server.stats.upgrades(), 1, "{mode:?}");
    }
}

/// Four tasks each sending `Reconnect` 200 times, yielding between sends.
fn flood(h: &Harness) -> Vec<JoinHandle<()>> {
    (0..4)
        .map(|_| {
            let commands = h.controls.commands.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    let _ = commands.try_send(Command::Reconnect);
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect()
}

#[tokio::test]
async fn repeated_and_concurrent_reconnects_keep_one_socket_and_one_timer() {
    let server = ScriptedServer::start(ME).await;
    server.set_mode(Mode::SilentAfterUpgrade);
    let mut h = harness(&server.url, credentials("k1"), ME, true).await;
    h.start().await;
    h.turn_until("the join is sent", |_| !server.frames().is_empty())
        .await;

    // While connecting, a Reconnect is a no-op: still one socket.
    let flooders = flood(&h);
    h.turns(40).await;
    for flooder in flooders {
        flooder.await.unwrap();
    }
    assert_eq!(
        server.stats.upgrades(),
        1,
        "Reconnect while connecting opened a socket"
    );
    assert!(h.owner.connection.has_socket());

    // Disconnect the flooded engine, then hammer Reconnect against a server that drops every
    // upgrade: at most one dial per second (Decision 6).
    server.set_mode(Mode::DropAfterUpgrade);
    jump(Duration::from_secs(16)).await;
    h.turn_until("the connect timeout", |o| {
        connection(o) != ConnectionView::Connecting
    })
    .await;
    let dials_before = server.stats.upgrades();
    let started = tokio::time::Instant::now();
    for _ in 0..20 {
        let _ = h.controls.commands.try_send(Command::Reconnect);
        h.turns(1).await;
        h.turn_until("the connect attempt ends", |o| {
            connection(o) != ConnectionView::Connecting
        })
        .await;
        jump(Duration::from_millis(250)).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.turns(2).await;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let dials = (server.stats.upgrades() - dials_before) as f64;
    assert!(
        server.stats.peak_live() <= 1,
        "two sockets were open at once"
    );
    // Decision 6 (Task 13): a Reconnect within a second of a dial waits for the second to end.
    assert!(
        dials <= elapsed.floor() + 1.0,
        "{dials} dials in {elapsed:.1} s under a Reconnect flood"
    );
}
