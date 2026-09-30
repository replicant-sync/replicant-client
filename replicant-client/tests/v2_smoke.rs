//! End-to-end smoke test of the v2 transport against a real replicant-server v2.
//!
//! Ignored by default. Needs a running v2 server and an enrolled account: set
//! `SYNC_SERVER_URL`, `REPLICANT_API_KEY`, `REPLICANT_API_SECRET` and `REPLICANT_TEST_USER_ID`,
//! then run `cargo test -p replicant-client --test v2_smoke -- --ignored`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use replicant_client::engine::machine::{Input, Request, Response};
use replicant_client::transport::connection::{Connection, Received};
use replicant_client::transport::wire::{socket_url, JoinAuth};
use tokio::time::timeout;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required for the v2 smoke test"))
}

fn server_socket_url() -> String {
    socket_url(&required_env("SYNC_SERVER_URL"), Uuid::new_v4()).unwrap()
}

fn seeded_auth() -> JoinAuth {
    JoinAuth {
        email: std::env::var("REPLICANT_TEST_EMAIL")
            .unwrap_or_else(|_| "integration-test@example.com".into()),
        api_key: required_env("REPLICANT_API_KEY"),
        api_secret: required_env("REPLICANT_API_SECRET"),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Next event that survives generation filtering.
async fn received(connection: &mut Connection) -> Received {
    timeout(WAIT, async {
        loop {
            let event = connection.next_event().await;
            if let Some(received) = connection.receive(event) {
                return received;
            }
        }
    })
    .await
    .expect("input within 10 s")
}

async fn opened(url: String, auth: JoinAuth) -> Connection {
    let mut connection = Connection::new(url, Some(auth), "replicant-client/smoke".into());
    connection.open(1);
    assert_eq!(
        received(&mut connection).await,
        Received::Input(Input::SocketOpened { gen: 1 })
    );
    connection
}

#[tokio::test]
#[ignore = "needs a v2 server: see the file header"]
async fn joins_requests_heartbeats_and_closes() {
    let mut connection = opened(server_socket_url(), seeded_auth()).await;

    assert_eq!(connection.send(1, &Request::Join, now_unix()), None);
    let joined = received(&mut connection).await;
    let Received::Joined { req: 1, user_id } = joined else {
        panic!("expected the join to succeed, got {joined:?}");
    };
    let seeded_user = Uuid::parse_str(&required_env("REPLICANT_TEST_USER_ID")).unwrap();
    assert_eq!(user_id, seeded_user);

    let changes = Request::GetChangesSince {
        scope: "own".into(),
        cursor: 0,
        limit: 500,
    };
    assert_eq!(connection.send(2, &changes, now_unix()), None);
    let reply = received(&mut connection).await;
    assert!(
        matches!(
            reply,
            Received::Input(Input::Reply {
                req: 2,
                result: Ok(Response::Changes { .. })
            })
        ),
        "{reply:?}"
    );

    assert_eq!(connection.send(3, &Request::Heartbeat, now_unix()), None);
    assert_eq!(
        received(&mut connection).await,
        Received::Input(Input::Reply {
            req: 3,
            result: Ok(Response::HeartbeatOk)
        })
    );

    connection.close(1);
    let after_close = timeout(Duration::from_millis(500), connection.next_event()).await;
    assert!(
        after_close.is_err(),
        "a closed socket reports nothing: {after_close:?}"
    );
}

#[tokio::test]
#[ignore = "needs a v2 server: see the file header"]
async fn unsupported_protocol_version_is_refused_with_update_required() {
    let url = server_socket_url().replace("protocol_version=2", "protocol_version=1");
    let mut connection = Connection::new(url, Some(seeded_auth()), "replicant-client/smoke".into());
    connection.open(1);
    let refused = received(&mut connection).await;
    let Received::Input(Input::ConnectRefused { gen: 1, error }) = refused else {
        panic!("expected HTTP 426 as ConnectRefused, got {refused:?}");
    };
    assert_eq!(error.code, "update_required");
    assert!(error.is_fatal);
}

#[tokio::test]
#[ignore = "needs a v2 server: see the file header"]
async fn wrong_secret_is_a_fatal_auth_invalid_join_error() {
    let wrong_secret = JoinAuth {
        api_secret: "rps_wrong".into(),
        ..seeded_auth()
    };
    let mut connection = opened(server_socket_url(), wrong_secret).await;
    assert_eq!(connection.send(1, &Request::Join, now_unix()), None);
    let rejected = received(&mut connection).await;
    let Received::Input(Input::Reply {
        req: 1,
        result: Err(error),
    }) = rejected
    else {
        panic!("expected a join error, got {rejected:?}");
    };
    assert_eq!(error.code, "auth_invalid");
    assert!(error.is_fatal);
}
