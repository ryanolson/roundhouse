// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The engine every case runs on: the three-provider catalog, scripted
//! transports keyed by provider, and an in-memory store.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, RoutingPolicy, StagePolicy};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{FrontierClient, FrontierClients, StaticFrontierCatalog};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

use super::catalog;

pub(super) struct Rig {
    pub(super) engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
}

/// [`rig_of`]'s engine wiring, parameterized on the catalog so the cost-guard
/// case can price its providers for real without a second copy of it.
pub(super) fn rig_over(
    catalog: StaticFrontierCatalog,
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
) -> Rig {
    rig_routed_by(
        catalog,
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
        clients,
    )
}

/// [`rig_over`] with the routing policy as a parameter, for the one case that
/// routes a forced pick rather than the scorer's.
pub(super) fn rig_routed_by(
    catalog: StaticFrontierCatalog,
    policy: Arc<dyn RoutingPolicy>,
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let registry = FrontierClients::keyed(
        clients
            .into_iter()
            .map(|(provider, client)| (provider.to_string(), client))
            .collect(),
    );
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog,
        Arc::new(registry),
        policy,
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    );
    Rig {
        engine: Arc::new(engine),
        store,
    }
}

pub(super) fn rig_of(clients: Vec<(&str, Arc<dyn FrontierClient>)>) -> Rig {
    rig_over(catalog(), clients)
}

impl Rig {
    /// One turn on a fresh session.
    pub(super) async fn turn(
        &self,
        input: Vec<Item>,
        admission: &Admission,
    ) -> (SessionId, roundhouse_server::TurnResult) {
        let session_id = SessionId::generate();
        self.engine.create_session(&session_id).await.unwrap();
        let result = self
            .engine
            .run_turn(&session_id, TurnId::new("t1"), input, admission)
            .await
            .expect("a narration must never be the reason a turn fails");
        (session_id, result)
    }

    /// Two turns on *one* session, which is the only way to ask what the
    /// previous turn was served by.
    pub(super) async fn two_turns(
        &self,
        first: Vec<Item>,
        second: Vec<Item>,
        admission: &Admission,
    ) -> [roundhouse_server::TurnResult; 2] {
        let session_id = SessionId::generate();
        self.engine.create_session(&session_id).await.unwrap();
        let mut results = Vec::new();
        for (index, input) in [first, second].into_iter().enumerate() {
            results.push(
                self.engine
                    .run_turn(
                        &session_id,
                        TurnId::new(format!("t{index}")),
                        input,
                        admission,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("turn {index} came back {error}")),
            );
        }
        // `unwrap_or_else` over a `Vec` whose element is not `Debug`-bound the
        // way `expect` wants; the length is a loop invariant either way.
        match <[roundhouse_server::TurnResult; 2]>::try_from(results) {
            Ok(pair) => pair,
            Err(_) => unreachable!("a loop over two inputs pushes two results"),
        }
    }

    /// Everything the session log holds, rendered — the R2 property's other
    /// half.
    pub(super) async fn stored_text(&self, session_id: &SessionId) -> String {
        self.store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads")
            .iter()
            .map(|event| serde_json::to_string(&event.kind).expect("an event serializes"))
            .collect()
    }
}
