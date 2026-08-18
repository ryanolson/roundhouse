// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the integration-test binaries.
//!
//! One canonical copy: the every-variant event list and its coverage guard
//! live here rather than one copy per test binary; the connection helpers
//! come from the crate's own `test_support` module, which the server's
//! durability test shares too.
//! Each binary compiles its own copy via `mod common;`, and none uses every
//! item, so the module opts out of dead-code analysis rather than sprinkling
//! `allow`s per item.
//!
//! Everything here presumes `--include-ignored` already opted into the real
//! backend, which is why a missing `ROUNDHOUSE_TEST_REDIS_URL` panics instead
//! of skipping: it is a runner error to report, not infrastructure to wait
//! for.
#![allow(dead_code)]

use roundhouse_core::control::Principal;
use roundhouse_core::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{Candidate, DecisionRecord, Target};
use roundhouse_core::store::SessionStore;
use roundhouse_store_redis::RedisSessionStore;
use roundhouse_store_redis::test_support;

/// The store under test, its key layout, and a raw connection for writing
/// what the store must then read (or for sabotaging it from outside).
pub struct Rig {
    pub store: RedisSessionStore,
    pub raw: redis::aio::MultiplexedConnection,
}

pub async fn rig() -> Rig {
    let url = url_from_env();
    let store = RedisSessionStore::connect(&url)
        .await
        .expect("Redis named by the env var must be reachable");
    let raw = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    Rig { store, raw }
}

pub async fn connect_from_env() -> RedisSessionStore {
    test_support::connect_from_env().await
}

pub fn url_from_env() -> String {
    test_support::url_from_env()
}

pub fn lease_key(session_id: &SessionId) -> String {
    test_support::lease_key(session_id)
}

pub fn log_key(session_id: &SessionId) -> String {
    test_support::log_key(session_id)
}

impl Rig {
    /// Append entries exactly as the fenced script does: explicit `<seq>-0`
    /// ids with `at_ms` and `kind` fields, bypassing the lease.
    pub async fn raw_append(
        &mut self,
        session_id: &SessionId,
        first_seq: u64,
        kinds: Vec<SessionEventKind>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::with_capacity(kinds.len());
        for (offset, kind) in kinds.into_iter().enumerate() {
            let seq = first_seq + offset as u64;
            // Deterministic timestamps so equality asserts the store read
            // back what was written, not something it re-stamped.
            let at_ms = 1_700_000_000_000 + seq;
            let _: String = redis::cmd("XADD")
                .arg(log_key(session_id))
                .arg(format!("{seq}-0"))
                .arg("at_ms")
                .arg(at_ms)
                .arg("kind")
                .arg(serde_json::to_string(&kind).unwrap())
                .query_async(&mut self.raw)
                .await
                .unwrap();
            events.push(SessionEvent {
                seq,
                session_id: session_id.clone(),
                at_ms,
                kind,
            });
        }
        events
    }

    /// A created session, ready to write to.
    pub async fn fresh_session(&self) -> SessionId {
        let sid = SessionId::generate();
        assert!(self.store.create_session(&sid, "affinity").await.unwrap());
        sid
    }
}

/// One of every event kind, so persistence proves it can take apart and
/// reassemble the whole vocabulary, not just the easy variants.
pub fn every_event_kind() -> Vec<SessionEventKind> {
    let response_id = ResponseId::generate();
    let local = Target::Local {
        worker_id: 7,
        dp_rank: 1,
        model: "llama".into(),
    };
    let frontier = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
    };
    vec![
        SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            // Populated rather than `None`: this list exists to prove the
            // backend reassembles what it took apart, and an empty principal
            // would let a codec that dropped attribution round-trip cleanly.
            principal: Some(Principal::new("acme", "ada")),
        },
        SessionEventKind::TurnStarted {
            turn_id: TurnId::generate(),
            response_id: response_id.clone(),
        },
        SessionEventKind::ItemAppended {
            item: Item::user_text("hello"),
        },
        SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                chosen: local.clone(),
                rationale: "warm prefix".into(),
                policy: "affinity".into(),
                isl_tokens: 128,
                expected_prefill_tokens: 16.5,
                expected_cost_usd: 0.0,
                considered: vec![
                    Candidate {
                        target: local,
                        expected_prefill_tokens: 16.5,
                        matched_prefix_tokens: 112,
                        expected_ttft_ms: 40.0,
                        expected_cost_usd: 0.0,
                        quality_prior: 0.6,
                        load: Some(2048.0),
                    },
                    Candidate {
                        target: frontier,
                        expected_prefill_tokens: 128.0,
                        matched_prefix_tokens: 0,
                        expected_ttft_ms: 900.0,
                        expected_cost_usd: 0.0021,
                        quality_prior: 0.9,
                        load: None,
                    },
                ],
            },
        },
        SessionEventKind::OutputTextDelta {
            response_id: response_id.clone(),
            text: "hel".into(),
        },
        SessionEventKind::ResponseCompleted {
            response_id: response_id.clone(),
            usage: Usage {
                input_tokens: 128,
                cached_input_tokens: 112,
                output_tokens: 5,
                reasoning_tokens: 2,
                accounting: Accounting::Reported,
            },
        },
        SessionEventKind::ResponseIncomplete {
            response_id: response_id.clone(),
            reason: IncompleteReason::OwnerLost,
            usage: Usage {
                accounting: Accounting::Estimated,
                ..Usage::default()
            },
        },
        SessionEventKind::TurnDeduplicated {
            turn_id: TurnId::generate(),
            response_id,
        },
        SessionEventKind::Error {
            message: "boom".into(),
        },
    ]
}

/// Assert the list lives up to its name. The exhaustive match is the point:
/// a new [`SessionEventKind`] refuses to compile here until
/// [`every_event_kind`] is taught about it, so "every" stays a checked claim
/// rather than a hopeful one.
pub fn assert_covers_every_variant(kinds: &[SessionEventKind]) {
    use SessionEventKind as K;
    let mut covered = [false; 9];
    for kind in kinds {
        covered[match kind {
            K::SessionCreated { .. } => 0,
            K::TurnStarted { .. } => 1,
            K::ItemAppended { .. } => 2,
            K::Routed { .. } => 3,
            K::OutputTextDelta { .. } => 4,
            K::ResponseCompleted { .. } => 5,
            K::ResponseIncomplete { .. } => 6,
            K::TurnDeduplicated { .. } => 7,
            K::Error { .. } => 8,
        }] = true;
    }
    assert!(
        covered.into_iter().all(|seen| seen),
        "every_event_kind() must cover the whole enum"
    );
}
