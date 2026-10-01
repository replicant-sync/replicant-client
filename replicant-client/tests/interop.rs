//! Protocol v2 interop against a real replicant-server. Ignored by default: run it through
//! `test/run_phoenix_interop_local.sh`, which boots the server, seeds the account and sets the env.

mod support;

use std::path::Path;
use std::time::Duration;

use replicant_client::engine::machine::{ConnectionView, HaltReason, SyncView};
use replicant_client::host::{self, Handle, HostEvent, Origin};
use serde_json::{json, Value};
use support::*;
use uuid::Uuid;

/// Syncs `start` to both devices; then A writes `a_content` online while B is closed, and B
/// writes `b_content` offline and reconnects. Returns B's handle and its events since reconnecting.
fn edit_on_both_devices(
    a: &Handle,
    dir_b: &Path,
    seed: &Seed,
    start: Value,
    a_content: Value,
    b_content: Value,
) -> (Uuid, Handle, Vec<HostEvent>) {
    let doc_id = create(a, start.clone());
    wait_uploaded(a);
    let (b, _) = attach_synced(dir_b, seed);
    assert_eq!(content(&b, doc_id), Some(start));
    close(b);

    update(a, doc_id, a_content);
    wait_uploaded(a);

    let b = host::attach(config(dir_b, OFFLINE_URL)).unwrap();
    update(&b, doc_id, b_content);
    close(b);

    let (b, b_events) = attach_synced(dir_b, seed);
    (doc_id, b, b_events)
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn first_sync_reports_connection_then_sync_started_then_completed() {
    let seed = Seed::from_env();
    let dir = signed_in_dir(&seed);
    let (handle, events) = attach_synced(dir.path(), &seed);

    let position = |wanted: HostEvent| {
        events
            .iter()
            .position(|event| *event == wanted)
            .unwrap_or_else(|| panic!("no {wanted:?} in {events:?}"))
    };
    let connected = position(HostEvent::ConnectionSucceeded);
    let started = position(HostEvent::SyncStarted);
    let completed = position(HostEvent::SyncCompleted);
    assert!(connected < started && started < completed, "{events:?}");
    let state = handle.state();
    assert_eq!(state.connection, ConnectionView::Connected);
    assert_eq!(state.sync, SyncView::Live);
    close(handle);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn a_create_update_and_delete_reach_a_second_device() {
    let seed = Seed::from_env();
    let (dir_a, dir_b) = (signed_in_dir(&seed), signed_in_dir(&seed));
    let (a, _) = attach_synced(dir_a.path(), &seed);
    let (b, _) = attach_synced(dir_b.path(), &seed);

    let doc_id = create(&a, json!({"title": "t", "n": 1}));
    let changed_to = |n: i64| {
        move |event: &HostEvent| {
            matches!(event, HostEvent::DocumentChanged { document, origin: Origin::Server }
                if document.id == doc_id && document.content == json!({"title": "t", "n": n}))
        }
    };
    assert!(
        wait_event(&b, LIVE, changed_to(1)).is_some(),
        "B never received the create"
    );

    update(&a, doc_id, json!({"title": "t", "n": 2}));
    assert!(
        wait_event(&b, LIVE, changed_to(2)).is_some(),
        "B never received the update"
    );

    a.block_on(a.store().delete_document(doc_id)).unwrap();
    a.notify_outbox();
    let deleted = wait_event(
        &b,
        LIVE,
        |event| matches!(event, HostEvent::DocumentDeleted { doc_id: id, origin: Origin::Server } if *id == doc_id),
    );
    assert!(deleted.is_some(), "B never received the delete");
    assert_eq!(content(&b, doc_id), None);

    wait_uploaded(&a);
    assert_eq!(a.block_on(a.store().list_parked()).unwrap(), vec![]);
    close(a);
    close(b);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn edits_made_offline_upload_on_the_next_connection() {
    let seed = Seed::from_env();
    let (dir_a, dir_b) = (signed_in_dir(&seed), signed_in_dir(&seed));
    let (b, _) = attach_synced(dir_b.path(), &seed);

    let a = host::attach(config(dir_a.path(), OFFLINE_URL)).unwrap();
    let ids: Vec<Uuid> = (0..3)
        .map(|n| create(&a, json!({"title": format!("offline {n}"), "n": n})))
        .collect();
    update(&a, ids[0], json!({"title": "offline 0", "n": 10}));
    assert_eq!(pending(&a), 3);
    close(a);

    let (a, _) = attach_synced(dir_a.path(), &seed);
    let expected = [
        json!({"title": "offline 0", "n": 10}),
        json!({"title": "offline 1", "n": 1}),
        json!({"title": "offline 2", "n": 2}),
    ];
    let b_has_all = wait_until(LIVE, || {
        ids.iter()
            .zip(&expected)
            .all(|(doc_id, want)| content(&b, *doc_id).as_ref() == Some(want))
    });
    let seen: Vec<_> = ids.iter().map(|doc_id| content(&b, *doc_id)).collect();
    assert!(b_has_all, "B has {seen:?}");
    wait_uploaded(&a);
    close(a);
    close(b);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn different_fields_edited_on_two_devices_merge() {
    let seed = Seed::from_env();
    let (dir_a, dir_b) = (signed_in_dir(&seed), signed_in_dir(&seed));
    let (a, _) = attach_synced(dir_a.path(), &seed);
    let (doc_id, b, mut b_events) = edit_on_both_devices(
        &a,
        dir_b.path(),
        &seed,
        json!({"title": "m", "a": 0, "b": 0}),
        json!({"title": "m", "a": 1, "b": 0}),
        json!({"title": "m", "a": 0, "b": 2}),
    );

    let merged = json!({"title": "m", "a": 1, "b": 2});
    let converged = wait_until(LIVE, || {
        content(&a, doc_id).as_ref() == Some(&merged)
            && content(&b, doc_id).as_ref() == Some(&merged)
    });
    assert!(
        converged,
        "A {:?}, B {:?}",
        content(&a, doc_id),
        content(&b, doc_id)
    );
    wait_uploaded(&a);
    wait_uploaded(&b);

    b_events.extend(b.take_events());
    let a_events = a.take_events();
    let conflicts: Vec<_> = a_events
        .iter()
        .chain(&b_events)
        .filter(|event| matches!(event, HostEvent::Conflict { .. }))
        .collect();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(a.block_on(a.store().list_recovered()).unwrap(), vec![]);
    assert_eq!(b.block_on(b.store().list_recovered()).unwrap(), vec![]);
    close(a);
    close(b);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn the_same_field_edited_on_two_devices_keeps_the_losing_value() {
    let seed = Seed::from_env();
    let (dir_a, dir_b) = (signed_in_dir(&seed), signed_in_dir(&seed));
    let (a, _) = attach_synced(dir_a.path(), &seed);
    let (doc_id, b, b_events) = edit_on_both_devices(
        &a,
        dir_b.path(),
        &seed,
        json!({"title": "s", "a": 0, "b": 0}),
        json!({"title": "s", "a": 1, "b": 0}),
        json!({"title": "s", "a": 2, "b": 0}),
    );

    let converged = wait_until(LIVE, || {
        let on_a = content(&a, doc_id);
        on_a.is_some() && on_a == content(&b, doc_id) && pending(&a) == 0 && pending(&b) == 0
    });
    assert!(
        converged,
        "A {:?}, B {:?}",
        content(&a, doc_id),
        content(&b, doc_id)
    );
    let winning = content(&a, doc_id).unwrap()["a"].as_i64().unwrap();
    assert!(winning == 1 || winning == 2, "a = {winning}");
    assert_eq!(
        content(&a, doc_id),
        Some(json!({"title": "s", "a": winning, "b": 0}))
    );

    let losing = 3 - winning;
    let (loser, mut loser_events, winner) = if losing == 2 {
        (&b, b_events, &a)
    } else {
        (&a, a.take_events(), &b)
    };
    let mut recovered_id = None;
    let conflicted = wait_until(LIVE, || {
        loser_events.extend(loser.take_events());
        recovered_id = loser_events.iter().find_map(|event| match event {
            HostEvent::Conflict {
                doc_id: id,
                recovered_id: Some(recovered),
                ..
            } if *id == doc_id => Some(*recovered),
            _ => None,
        });
        recovered_id.is_some()
    });
    assert!(conflicted, "no Conflict with a kept copy: {loser_events:?}");

    let copies = loser.block_on(loser.store().list_recovered()).unwrap();
    let kept = copies
        .iter()
        .find(|copy| Some(copy.id) == recovered_id)
        .unwrap_or_else(|| panic!("kept copy {recovered_id:?} not in {copies:?}"));
    assert_eq!(kept.doc_id, doc_id);
    assert_eq!(kept.content["a"], json!(losing));
    assert_eq!(
        winner.block_on(winner.store().list_recovered()).unwrap(),
        vec![]
    );
    close(a);
    close(b);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn numbers_and_unicode_round_trip_without_an_extra_upload() {
    let seed = Seed::from_env();
    let (dir_a, dir_b) = (signed_in_dir(&seed), signed_in_dir(&seed));
    let (a, _) = attach_synced(dir_a.path(), &seed);
    let (b, _) = attach_synced(dir_b.path(), &seed);

    let doc_id = create(
        &a,
        json!({"title": "ünï ✓", "f": 1.0, "e": 1e21, "neg": -0.0, "s": "\u{0001}"}),
    );
    // Integral floats are stored as integers; 1e21 is beyond u64 and stays a float.
    let canonical = json!({"title": "ünï ✓", "f": 1, "e": 1e21, "neg": 0, "s": "\u{0001}"});
    assert_eq!(content(&a, doc_id), Some(canonical.clone()));

    let received = wait_event(
        &b,
        LIVE,
        |event| matches!(event, HostEvent::DocumentChanged { document, origin: Origin::Server } if document.id == doc_id),
    );
    let Some(HostEvent::DocumentChanged { document, .. }) = received else {
        panic!("B never received the document");
    };
    assert_eq!(document.content, canonical);
    assert_eq!(document.title.as_deref(), Some("ünï ✓"));
    assert_eq!(content(&b, doc_id), Some(canonical.clone()));
    wait_uploaded(&a);

    let mut a_events = a.take_events();
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(pending(&a), 0, "A queued another upload");
        a_events.extend(a.take_events());
    }
    let errors: Vec<_> = a_events
        .iter()
        .filter(|event| matches!(event, HostEvent::SyncError { .. }))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(content(&a, doc_id), Some(canonical));
    close(a);
    close(b);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn a_curated_publication_arrives_read_only() {
    let seed = Seed::from_env();
    let curated: Uuid = required_env("REPLICANT_TEST_CURATED_ID").parse().unwrap();
    let dir = signed_in_dir(&seed);
    let (handle, _) = attach_synced(dir.path(), &seed);

    let read = || {
        handle
            .block_on(handle.store().get_document(curated))
            .unwrap()
    };
    assert!(
        wait_until(LIVE, || read().is_some()),
        "the curated publication never arrived"
    );
    let document = read().unwrap();
    assert!(document.read_only);
    assert_eq!(document.visibility, "public");
    assert_eq!(document.title.as_deref(), Some("Curated seed"));

    let edit = handle.block_on(
        handle
            .store()
            .update_document(curated, json!({"title": "mine"})),
    );
    assert!(edit.is_err(), "a curated publication accepted an edit");
    assert_eq!(read().unwrap().content, document.content);
    close(handle);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn a_credential_with_no_user_halts_auth_invalid() {
    let legacy = Seed {
        api_key: required_env("REPLICANT_LEGACY_API_KEY"),
        secret: required_env("REPLICANT_LEGACY_API_SECRET"),
        ..Seed::from_env()
    };
    let dir = signed_in_dir(&legacy);
    let handle = host::attach(config(dir.path(), &legacy.server_url)).unwrap();

    let halted = wait_until(LIVE, || {
        handle.state().connection == ConnectionView::Halted(HaltReason::AuthInvalid)
    });
    assert!(halted, "state {:?}", handle.state());
    close(handle);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn a_fresh_database_adopts_the_server_identity() {
    let seed = Seed::from_env();
    let dir = signed_in_dir(&seed);
    let (handle, events) = attach_synced(dir.path(), &seed);

    assert!(
        events.contains(&HostEvent::IdentityAdopted {
            user_id: seed.user_id
        }),
        "{events:?}"
    );
    assert_eq!(
        handle.block_on(handle.store().user_id()).unwrap(),
        seed.user_id
    );
    close(handle);
}
