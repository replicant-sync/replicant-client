//! Drives an `Owner` turn by turn against a scripted server, for tests that need its insides.

use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{Controls, CredentialLoader, EngineEvent, Owner};
use crate::driver::test_support::{config, seeded_db, WAIT};
use crate::engine::machine::{ConnectionView, EngineState, Lifecycle, SyncView};

pub(super) struct Harness {
    pub owner: Owner,
    pub controls: Controls,
    pub events: mpsc::UnboundedReceiver<EngineEvent>,
    _dir: TempDir,
}

pub(super) async fn harness(
    server_url: &str,
    credentials: CredentialLoader,
    user_id: Uuid,
    adopted: bool,
) -> Harness {
    let (dir, path) = seeded_db(user_id, adopted).await;
    let (events_tx, events) = mpsc::unbounded_channel();
    let (owner, controls) = Owner::open(&path, config(server_url, credentials), events_tx)
        .await
        .unwrap();
    Harness {
        owner,
        controls,
        events,
        _dir: dir,
    }
}

impl Harness {
    pub async fn start(&mut self) {
        self.owner.start().await;
    }

    /// Turns until `done` holds; every idle turn ends at the next change-log tick.
    pub async fn turn_until(&mut self, what: &str, done: impl Fn(&Owner) -> bool) {
        let deadline = std::time::Instant::now() + WAIT;
        while !done(&self.owner) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            self.owner.turn().await;
        }
    }

    pub async fn turns(&mut self, count: usize) {
        for _ in 0..count {
            self.owner.turn().await;
        }
    }

    pub async fn live(&mut self) {
        self.start().await;
        self.turn_until("live", is_live).await;
    }

    /// Lifecycle events emitted so far.
    pub fn lifecycles(&mut self) -> Vec<Lifecycle> {
        let mut lifecycles = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let EngineEvent::Lifecycle(lifecycle) = event {
                lifecycles.push(lifecycle);
            }
        }
        lifecycles
    }
}

pub(super) fn is_live(owner: &Owner) -> bool {
    owner.core.state()
        == EngineState {
            connection: ConnectionView::Connected,
            sync: SyncView::Live,
        }
}

pub(super) fn connection(owner: &Owner) -> ConnectionView {
    owner.core.state().connection
}
