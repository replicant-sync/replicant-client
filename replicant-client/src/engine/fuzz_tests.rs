use uuid::Uuid;

use super::backoff::Jitter;
use super::machine::*;

fn pick<T: Clone>(rng: &mut Jitter, items: &[T]) -> T {
    items[(rng.next_unit() * items.len() as f64) as usize].clone()
}

/// Generates a plausible next input, answering outstanding requests with random outcomes.
fn next_input(rng: &mut Jitter, outstanding: &mut Vec<(u64, Request)>) -> Input {
    let roll = rng.next_unit();
    if roll < 0.45 && !outstanding.is_empty() {
        let i = (rng.next_unit() * outstanding.len() as f64) as usize;
        let (req, request) = outstanding.remove(i);
        if rng.next_unit() < 0.15 {
            return Input::Timer(if request == Request::Heartbeat {
                TimerId::HeartbeatTimeout(req)
            } else {
                TimerId::Request(req)
            });
        }
        let ok = rng.next_unit() < 0.8;
        let result = match (request, ok) {
            (Request::Join, true) => Ok(Response::Joined),
            (Request::Heartbeat, true) => Ok(Response::HeartbeatOk),
            (Request::GetChangesSince { cursor, .. }, true) => Ok(Response::Changes {
                changes: vec![],
                next_cursor: cursor,
                has_more: false,
            }),
            (Request::GetSnapshot { .. }, true) => Ok(Response::SnapshotPage {
                docs: vec![],
                snapshot_seq: 10,
                next_page_token: None,
            }),
            (_, _) => {
                let code = pick(
                    rng,
                    &[
                        "internal",
                        "cursor_too_old",
                        "rate_limited",
                        "auth_invalid",
                        "update_required",
                        "subscription_forbidden",
                    ],
                );
                let mut err = super::types::ServerError::new(code);
                err.is_fatal = matches!(code, "auth_invalid" | "update_required");
                Err(err)
            }
        };
        return Input::Reply { req, result };
    }
    pick(
        rng,
        &[
            Input::SocketOpened,
            Input::SocketClosed,
            Input::Reconnect,
            Input::OutboxChanged,
            Input::Timer(TimerId::Reconnect),
            Input::Timer(TimerId::ConnectTimeout),
            Input::Timer(TimerId::Heartbeat),
            Input::Timer(TimerId::StableReset),
            Input::Timer(TimerId::Pump),
            Input::Timer(TimerId::PumpCap),
            Input::Timer(TimerId::HaltRetry),
            Input::Timer(TimerId::CatchUpRetry("own".into())),
            Input::Timer(TimerId::CatchUpRetry("collection:curated".into())),
            Input::Cursors(vec![("own".into(), 3), ("collection:curated".into(), 0)]),
            Input::Applied {
                scope: "own".into(),
                tag: ApplyTag::Push,
            },
            Input::Applied {
                scope: "own".into(),
                tag: ApplyTag::SnapshotFinish,
            },
            Input::Applied {
                scope: "collection:curated".into(),
                tag: ApplyTag::SnapshotFinish,
            },
        ],
    )
}

struct Harness {
    core: Core,
    seed: u64,
    outstanding: Vec<(u64, Request)>,
    socket_open: bool,
    events: Vec<Lifecycle>,
}

impl Harness {
    /// Feeds one input and answers every DB effect it (transitively) produces.
    fn feed(&mut self, input: Input) {
        let mut queue = std::collections::VecDeque::from([input]);
        while let Some(input) = queue.pop_front() {
            if matches!(input, Input::SocketClosed) {
                self.socket_open = false;
            }
            for e in self.core.step(input) {
                match e {
                    Effect::OpenSocket => {
                        assert!(
                            !self.socket_open,
                            "seed {}: second OpenSocket without close",
                            self.seed
                        );
                        self.socket_open = true;
                    }
                    Effect::CloseSocket => self.socket_open = false,
                    Effect::Send { req, request } => self.outstanding.push((req, request)),
                    Effect::Emit(l) => self.events.push(l),
                    Effect::ApplyChanges { scope, tag, .. }
                    | Effect::ApplySnapshotPage { scope, tag, .. } => {
                        queue.push_back(Input::Applied { scope, tag })
                    }
                    Effect::FinishSnapshot { scope, .. } => queue.push_back(Input::Applied {
                        scope,
                        tag: ApplyTag::SnapshotFinish,
                    }),
                    Effect::CheckCredentials => queue.push_back(Input::CredentialsChanged {
                        has_credentials: true,
                    }),
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn random_sequences_preserve_connection_and_ordering_invariants() {
    let mut fatal_errors = 0u64;
    let mut sync_completed = 0u64;
    let mut connection_lost = 0u64;
    for seed in 1..=2000u64 {
        let mut rng = Jitter::new(seed * 7919);
        let mut h = Harness {
            core: Core::new(
                Uuid::from_u128(0xA),
                vec!["own".into(), "collection:curated".into()],
                seed,
            ),
            seed,
            outstanding: Vec::new(),
            socket_open: false,
            events: Vec::new(),
        };
        h.feed(Input::Start {
            has_credentials: true,
        });
        for _ in 0..200 {
            let input = next_input(&mut rng, &mut h.outstanding);
            h.feed(input);
        }
        assert_ordering(seed, &h.events);
        for e in &h.events {
            match e {
                Lifecycle::SyncError { fatal: true, .. } => fatal_errors += 1,
                Lifecycle::SyncCompleted => sync_completed += 1,
                Lifecycle::ConnectionLost => connection_lost += 1,
                _ => {}
            }
        }
        h.feed(Input::Shutdown);
        for input in [
            Input::Reconnect,
            Input::SocketOpened,
            Input::Timer(TimerId::Reconnect),
            Input::Timer(TimerId::HaltRetry),
            Input::Timer(TimerId::Heartbeat),
        ] {
            assert!(
                h.core.step(input.clone()).is_empty(),
                "seed {seed}: {input:?} produced effects after shutdown"
            );
        }
    }
    assert!(
        fatal_errors >= 1,
        "harness never produced a fatal SyncError (auth_invalid/update_required never reached); count = {fatal_errors}"
    );
    assert!(
        sync_completed >= 100,
        "harness rarely reached SyncCompleted; count = {sync_completed}"
    );
    assert!(
        connection_lost >= 100,
        "harness rarely reached ConnectionLost; count = {connection_lost}"
    );
}

fn assert_ordering(seed: u64, events: &[Lifecycle]) {
    #[derive(PartialEq, Debug, Clone, Copy)]
    enum S {
        Out,
        Connected,
        Started,
        Completed,
    }
    let mut s = S::Out;
    for e in events {
        s = match (s, e) {
            // Neither changes the per-connection ordering state.
            (state, Lifecycle::ConnectionAttempted) | (state, Lifecycle::SyncError { .. }) => state,
            (S::Out, Lifecycle::ConnectionSucceeded) => S::Connected,
            (S::Connected, Lifecycle::SyncStarted) => S::Started,
            (S::Started, Lifecycle::SyncCompleted) => S::Completed,
            (S::Connected | S::Started | S::Completed, Lifecycle::ConnectionLost) => S::Out,
            (state, event) => {
                panic!("seed {seed}: {event:?} not allowed in {state:?}; events {events:?}")
            }
        };
    }
}
