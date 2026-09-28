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
//!
//! **When the call is made and fails, the turn fails open within a bound**
//! (ruled 2026-09-28, ruling 5 in
//! `agent-docs/synergies/typesafe-selector-and-cache-affinity.md`). A fleet
//! error, or no answer inside `fleet_quote_deadline_ms`, drops the local
//! candidate and the turn routes among its hosted targets, recording why. A
//! turn with no admitted hosted target still fails, so a local-only session
//! never egresses.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::json;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{FrontierCadence, Principal, TargetFilter, TurnPolicy};
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
    Admission, EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecutor, TurnInput,
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
    behaviour: Behaviour,
}

/// What `price` does when it is asked.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Delegates to the real selector.
    Answers,
    /// Answers with an error, which is what a selector that is down or
    /// unreachable looks like from `plan`.
    Errors,
    /// Never answers, which is what a selector that accepted the request and
    /// went silent looks like. Pending forever rather than slow, so the bound
    /// under test is the only thing that can end the wait.
    Hangs,
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
        match self.behaviour {
            Behaviour::Answers => self.inner.price(query).await,
            Behaviour::Errors => Err(FleetError::Rejected("selector is down".to_string())),
            Behaviour::Hangs => std::future::pending().await,
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

async fn rig(behaviour: Behaviour) -> Rig {
    rig_with(behaviour, |config| config).await
}

async fn rig_with(
    behaviour: Behaviour,
    configure: impl FnOnce(EngineConfig) -> EngineConfig,
) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let fleet = Arc::new(CountingFleet {
        inner: common::embedded_fleet().await,
        calls: AtomicUsize::new(0),
        behaviour,
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
        configure(EngineConfig {
            turn_deadline_ms: 5_000,
            ..common::config()
        }),
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

/// An admission whose policy may name only this deployment's local model.
///
/// Built from the engine's own local identity rather than a hand-typed
/// pattern, so the test cannot pass by naming a model the engine does not
/// serve.
fn local_only() -> Admission {
    let local =
        roundhouse_core::routing::Target::local_policy_identity(&common::config().local_model);
    Admission {
        principal: Principal::new("proj", "user"),
        policy: Arc::new(TurnPolicy {
            allow: TargetFilter::parse([local.as_str()]).expect("the identity parses"),
            ..TurnPolicy::unrestricted()
        }),
        ..Admission::open()
    }
}

/// Run one turn and hand back what it answered, served or not.
async fn attempt(
    rig: &Rig,
    input: TurnInput,
    admission: &Admission,
) -> (
    SessionId,
    Result<roundhouse_server::TurnResult, EngineError>,
) {
    let session_id = SessionId::generate();
    rig.engine
        .create_session(&session_id)
        .await
        .expect("the session opens");
    let result = rig
        .engine
        .run_turn(&session_id, TurnId::new("t1"), input, admission)
        .await;
    (session_id, result)
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
    let rig = rig(Behaviour::Answers).await;
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
    let rig = rig(Behaviour::Answers).await;
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
    let rig = rig(Behaviour::Errors).await;
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
    let rig = rig(Behaviour::Answers).await;
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
    let rig = rig(Behaviour::Answers).await;
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
    let rig = rig(Behaviour::Answers).await;
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

// ---------------------------------------------------------------------------
// Fail open within a bound (ruled 2026-09-28)
// ---------------------------------------------------------------------------

/// **A fleet error on a turn that could go local does not fail it.** The
/// local candidate is dropped, the turn is served by a hosted target, and the
/// decision says why the local quote is missing.
#[tokio::test]
async fn a_fleet_error_drops_the_local_candidate_and_the_turn_serves_a_frontier_target() {
    let rig = rig(Behaviour::Errors).await;
    let (session_id, result) = attempt(&rig, turn_input(None), &Admission::open()).await;
    result.expect("a selector that is down must not fail a turn with a hosted target");

    let decision = decision(&rig.store, &session_id).await;
    assert!(!decision.chosen.is_local(), "{:?}", decision.chosen);
    assert_eq!(
        decision.local_quote_skipped,
        Some(LocalQuoteSkip::FleetError)
    );
    assert!(
        decision
            .considered
            .iter()
            .all(|candidate| !candidate.target.is_local()),
        "no local candidate was quoted, so none may be counted as considered: {:?}",
        decision.considered
    );
    assert_eq!(
        rig.fleet.calls(),
        1,
        "the call was made once and not retried"
    );
}

/// **A fleet that never answers is abandoned at its own bound, not the
/// turn's.** The turn deadline is five seconds and the residency bound 100 ms;
/// the turn must be served well inside the first.
#[tokio::test]
async fn a_fleet_that_hangs_past_its_bound_is_skipped_and_the_turn_serves_a_frontier_target() {
    let rig = rig_with(Behaviour::Hangs, |config| EngineConfig {
        fleet_quote_deadline_ms: 100,
        ..config
    })
    .await;
    let started = std::time::Instant::now();
    let (session_id, result) = attempt(&rig, turn_input(None), &Admission::open()).await;
    let elapsed = started.elapsed();
    result.expect("a silent selector must not fail a turn with a hosted target");

    assert!(
        elapsed < std::time::Duration::from_millis(2_500),
        "the residency call must be bounded by its own deadline, not the \
         five-second turn deadline: took {elapsed:?}"
    );
    let decision = decision(&rig.store, &session_id).await;
    assert!(!decision.chosen.is_local(), "{:?}", decision.chosen);
    assert_eq!(
        decision.local_quote_skipped,
        Some(LocalQuoteSkip::FleetTimeout)
    );
}

/// **A local-only session with an erroring fleet still fails, with the error
/// class it always had.** Failing open there would mean routing to a hosted
/// model the policy does not admit, which is the egress the policy exists to
/// prevent; with nothing else admitted, the fleet's failure is the turn's.
#[tokio::test]
async fn a_local_only_session_with_an_erroring_fleet_still_fails_the_turn() {
    let rig = rig(Behaviour::Errors).await;
    let (session_id, result) = attempt(&rig, turn_input(None), &local_only()).await;
    match result {
        Err(EngineError::Fleet(_)) => {}
        other => panic!("expected the fleet's own error, got {other:?}"),
    }
    assert_eq!(rig.fleet.calls(), 1);
    let routed = rig
        .store
        .read_events(&session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter(|event| matches!(event.kind, SessionEventKind::Routed { .. }))
        .count();
    assert_eq!(routed, 0, "nothing was dispatched anywhere");
}

/// **A local-only session is not held to the residency bound.** The bound
/// exists to fail open; with nothing to fail open to, cutting the call short
/// would only turn a slow turn into a failed one. So a local-only turn over a
/// silent fleet waits for the turn deadline and fails with it, exactly as it
/// did before the bound existed.
#[tokio::test]
async fn a_local_only_session_waits_for_the_turn_deadline_not_the_residency_bound() {
    let rig = rig_with(Behaviour::Hangs, |config| EngineConfig {
        fleet_quote_deadline_ms: 50,
        turn_deadline_ms: 400,
        ..config
    })
    .await;
    let started = std::time::Instant::now();
    let (_session_id, result) = attempt(&rig, turn_input(None), &local_only()).await;
    let elapsed = started.elapsed();
    match result {
        Err(EngineError::TurnDeadline(400)) => {}
        other => panic!("expected the turn deadline, got {other:?}"),
    }
    assert!(
        elapsed >= std::time::Duration::from_millis(400),
        "the call ran to the turn deadline rather than the 50 ms bound: {elapsed:?}"
    );
}

/// **A spent cadence leaves nothing to fail open to, so it keeps the old path
/// too.** The hosted targets are permitted by the policy but not admissible
/// this turn — the window is spent, which is exactly when the local answer
/// decides the turn — so cutting the residency call at the short bound would
/// drop the only candidate the turn could still take. `max_frontier: 0` spends
/// the window on the first turn.
#[tokio::test]
async fn a_spent_frontier_cadence_is_not_held_to_the_residency_bound() {
    let rig = rig_with(Behaviour::Hangs, |config| EngineConfig {
        fleet_quote_deadline_ms: 50,
        turn_deadline_ms: 400,
        ..config
    })
    .await;
    let rationed = Admission {
        principal: Principal::new("proj", "user"),
        policy: Arc::new(TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 0,
                per_turns: 1,
            }),
            ..TurnPolicy::unrestricted()
        }),
        ..Admission::open()
    };
    let started = std::time::Instant::now();
    let (_session_id, result) = attempt(&rig, turn_input(None), &rationed).await;
    let elapsed = started.elapsed();
    match result {
        Err(EngineError::TurnDeadline(400)) => {}
        other => panic!("expected the turn deadline, got {other:?}"),
    }
    assert!(
        elapsed >= std::time::Duration::from_millis(400),
        "the call ran to the turn deadline rather than the 50 ms bound: {elapsed:?}"
    );
}
