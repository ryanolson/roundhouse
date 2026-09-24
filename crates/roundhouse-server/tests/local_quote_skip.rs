// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! C3 of `PLAN-cache-affinity.md` — the Dynamo residency call becomes a
//! decision.
//!
//! `LocalFleet::price` is not a lookup: it is a realtime residency check that
//! sends this turn's block and sequence hashes to the selector and waits for
//! `effective_prefill_tokens` back. It is an HTTP call on the path to first
//! token. Until this rung it ran on every turn a fleet was configured for,
//! *before* the tool exclusion that removes every local candidate again — so a
//! coding agent, which declares a toolbox on nearly every turn, paid a
//! round-trip per turn for an answer the next twenty lines threw away, and a
//! selector that was down failed turns that were always going to a frontier
//! target.
//!
//! The claims are about the call, so the double counts it. Every test here
//! drives [`Engine::run_turn`] directly rather than the HTTP surface: the
//! question is what `plan` asks the fleet, and a dialect in the way would only
//! add ways for the count to be wrong for a reason other than the one under
//! test.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::json;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{Principal, TargetFilter, TurnPolicy};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, DecisionRecord, LocalQuoteSkip};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, EmbeddedFleet, FleetError, FleetQuery, FrontierClient, FrontierClients,
    LocalFleet, LocalQuote, Reservation,
};
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor, TurnInput,
};

mod common;

// ---------------------------------------------------------------------------
// The double: a fleet that counts, and can fail
// ---------------------------------------------------------------------------

/// A [`LocalFleet`] that counts residency checks and delegates everything else.
///
/// Wrapping the embedded fleet rather than replacing it, because
/// [`Reservation`]'s fields are private to `roundhouse-fleet` and a
/// hand-rolled mock therefore cannot satisfy `reserve` from out here — the
/// same constraint `tier_selection.rs` records. The count is the only thing
/// this adds; a turn that does route local still books and releases against a
/// real selector.
struct CountingFleet {
    inner: Arc<EmbeddedFleet>,
    calls: AtomicUsize,
    /// Whether `price` answers with an error instead of a quote, which is what
    /// a selector that is down or unreachable looks like from `plan`.
    fails: bool,
}

impl CountingFleet {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LocalFleet for CountingFleet {
    async fn price(&self, query: &FleetQuery) -> Result<Option<LocalQuote>, FleetError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.fails {
            true => Err(FleetError::Rejected("selector is down".to_string())),
            false => self.inner.price(query).await,
        }
    }

    async fn reserve(self: Arc<Self>, quote: &LocalQuote) -> Result<Reservation, FleetError> {
        Arc::clone(&self.inner).reserve(quote).await
    }

    async fn prefill_complete(&self, selection_id: &str) -> Result<(), FleetError> {
        self.inner.prefill_complete(selection_id).await
    }

    async fn output_block(&self, selection_id: &str) -> Result<(), FleetError> {
        self.inner.output_block(selection_id).await
    }

    async fn release(&self, selection_id: &str) -> Result<(), FleetError> {
        self.inner.release(selection_id).await
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    fleet: Arc<CountingFleet>,
}

async fn rig(fails: bool) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let fleet = Arc::new(CountingFleet {
        inner: common::embedded_fleet().await,
        calls: AtomicUsize::new(0),
        fails,
    });
    let clients = FrontierClients::keyed(
        [(
            "anthropic".to_string(),
            Arc::new(EchoFrontierClient::new("frontier answer")) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        common::frontier_catalog(),
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..common::config()
        },
    )
    .with_fleet(Arc::clone(&fleet) as Arc<dyn LocalFleet>);
    Rig {
        engine: Arc::new(engine),
        store,
        fleet,
    }
}

/// One tool, shaped like a client's, because only `tools.is_some()` is read.
fn tools() -> serde_json::Value {
    json!([{
        "name": "Read",
        "description": "read a file",
        "input_schema": { "type": "object" },
    }])
}

fn turn_input(tools: Option<serde_json::Value>) -> TurnInput {
    TurnInput {
        items: vec![Item::user_text("hi")],
        declared_baseline: None,
        output_token_cap: None,
        tools,
        tool_choice: None,
        tools_dialect: Some(roundhouse_fleet::WireProtocol::AnthropicMessages),
    }
}

/// An admission whose policy may name only hosted targets.
fn frontier_only() -> Admission {
    Admission {
        principal: Principal::new("proj", "user"),
        policy: Arc::new(TurnPolicy {
            allow: TargetFilter::parse(["anthropic/*"]).expect("the pattern parses"),
            ..TurnPolicy::unrestricted()
        }),
        ..Admission::open()
    }
}

async fn run(rig: &Rig, input: TurnInput, admission: &Admission) -> SessionId {
    let session_id = SessionId::generate();
    rig.engine
        .create_session(&session_id)
        .await
        .expect("the session opens");
    rig.engine
        .run_turn(&session_id, TurnId::new("t1"), input, admission)
        .await
        .expect("the turn is served");
    session_id
}

async fn decision(store: &MemoryStore, session_id: &SessionId) -> DecisionRecord {
    let mut all: Vec<DecisionRecord> = store
        .read_events(session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    assert_eq!(all.len(), 1, "the session routed exactly one turn");
    all.remove(0)
}

// ---------------------------------------------------------------------------
// The claims
// ---------------------------------------------------------------------------

/// **CONTROL.** A turn with no toolbox still asks the fleet.
///
/// Without it every assertion below is satisfiable by never calling the fleet
/// at all, which would be a router that has stopped routing rather than one
/// that has stopped wasting a round-trip.
#[tokio::test]
async fn a_turn_without_tools_still_asks_the_fleet() {
    let rig = rig(false).await;
    run(&rig, turn_input(None), &Admission::open()).await;
    assert_eq!(
        rig.fleet.calls(),
        1,
        "a turn a local worker could serve must be priced against one"
    );
}

/// The residency check is not made on a turn whose answer it cannot change.
#[tokio::test]
async fn a_tool_declaring_turn_makes_no_fleet_call() {
    let rig = rig(false).await;
    run(&rig, turn_input(Some(tools())), &Admission::open()).await;
    assert_eq!(
        rig.fleet.calls(),
        0,
        "every local candidate is excluded twenty lines later; the call buys nothing"
    );
}

/// A selector that is down does not fail a turn that was never going to it.
#[tokio::test]
async fn a_fleet_error_on_a_tool_turn_does_not_fail_the_turn() {
    let rig = rig(true).await;
    let session_id = run(&rig, turn_input(Some(tools())), &Admission::open()).await;
    assert!(
        !decision(&rig.store, &session_id).await.chosen.is_local(),
        "a tool turn is served by a hosted model"
    );
    assert_eq!(rig.fleet.calls(), 0, "the failing call is never made");
}

/// A policy that names no local target skips the check for its own reason.
#[tokio::test]
async fn a_turn_whose_policy_admits_no_local_target_makes_no_fleet_call() {
    let rig = rig(false).await;
    run(&rig, turn_input(None), &frontier_only()).await;
    assert_eq!(
        rig.fleet.calls(),
        0,
        "a quote for a target this principal may never reach changes nothing"
    );
}

/// The skip is a fact about the turn, so the log carries why.
///
/// "Not quoted" and "quoted and rejected" are different answers, and a
/// dashboard that cannot tell them apart reports a local fleet nobody wanted
/// when the truth is a local fleet nobody asked.
#[tokio::test]
async fn a_skipped_local_quote_is_named_in_the_decision_record() {
    let rig = rig(false).await;
    let tooled = run(&rig, turn_input(Some(tools())), &Admission::open()).await;
    assert_eq!(
        decision(&rig.store, &tooled).await.local_quote_skipped,
        Some(LocalQuoteSkip::ToolsDeclared),
    );

    let refused = run(&rig, turn_input(None), &frontier_only()).await;
    assert_eq!(
        decision(&rig.store, &refused).await.local_quote_skipped,
        Some(LocalQuoteSkip::PolicyAdmitsNoLocal),
    );

    let quoted = run(&rig, turn_input(None), &Admission::open()).await;
    assert_eq!(
        decision(&rig.store, &quoted).await.local_quote_skipped,
        None,
        "a turn that was quoted records no skip, whatever the quote said"
    );
}

/// **ORDERING.** Tools must win over policy.
///
/// A turn that declares tools under a policy that also excludes every local
/// target is a shape where the two refusals in `local_quote_can_matter`
/// could both apply, and the tool one must be checked first: it is what
/// `plan` reads to refuse a starved tool turn with `NoToolCapableTarget` and
/// to annotate a served one with `TOOL_TURN_EXCLUDES_LOCAL`, and a turn
/// this restrictive is exactly the shape that would otherwise surface
/// `PolicyAdmitsNoLocal` instead and lose that note.
#[tokio::test]
async fn a_tool_declaring_turn_under_a_local_excluding_policy_skips_for_tools_not_policy() {
    let rig = rig(false).await;
    let session_id = run(&rig, turn_input(Some(tools())), &frontier_only()).await;
    assert_eq!(
        decision(&rig.store, &session_id).await.local_quote_skipped,
        Some(LocalQuoteSkip::ToolsDeclared),
        "the tool exclusion must be named even under a policy that would also refuse local"
    );
    assert_eq!(
        rig.fleet.calls(),
        0,
        "neither reason for skipping the quote asks the fleet"
    );
}
