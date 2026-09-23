// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The classification window as the engine actually writes it, over a session
//! with a long classification history.
//!
//! The unit suites one layer down prove what the window costs to build and what
//! it names. What only an engine can prove is that the thing it writes into
//! `Routed` is that window: the cutoff it was taken at is the one the features
//! were taken at, the count is of everything that had landed by then rather
//! than of everything the session holds, and neither survives a failover or a
//! trip through the store as something else.
//!
//! The history is seeded through the session's own commit path rather than by
//! driving four hundred classifier calls: what is under test is the snapshot
//! the engine takes over a long history, and buying that history from a
//! loopback service would make this a test of the executor with a slow fixture
//! attached. The engine's own classifier is configured and **saturated for the
//! whole of every test here** -- its one permit is held by the test -- so the
//! engine writes a window on every routed turn while adding no classification
//! events of its own that the assertions would then have to subtract.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassificationWindow,
    ClassifierIdentity, ContextDependence, EvaluationSpend, Graded, ReservationRecord,
    SettlementAck, TAXONOMY_VERSION, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{BudgetWindow, MemorySpendLedger};
use roundhouse_core::event::{CacheReadSource, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, DecisionRecord, PickerMode, ProviderPricing, SelectionSnapshot,
    StagePolicy, Target, TierRecipe,
};
use roundhouse_core::session::{Session, SessionState};
use roundhouse_core::store::doubles::ReplayLog;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

const PRIMARY: &str = "alpha";
const SECONDARY: &str = "beta";
const THRIFTY: &str = "gamma";
const TTL: u64 = 30_000;
/// The configured window, matching `max_prior_classifications` below.
const WINDOW: usize = 4;
/// Long enough that a window built by collecting the history would be visibly
/// different work from one that reaches for the newest few.
const HISTORY: u64 = 400;

// ------------------------------------------------------------------- fleet

fn spec(provider: &str, quality_prior: f64) -> FrontierModelSpec {
    FrontierModelSpec {
        quality_prior,
        pricing: ProviderPricing::free(),
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec(provider, "m", WireProtocol::OpenAiResponses)
    }
}

fn catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![
        spec(PRIMARY, 0.95),
        spec(SECONDARY, 0.90),
        spec(THRIFTY, 0.60),
    ])
}

fn target(provider: &str) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: "m".into(),
    }
}

/// Answers, or fails the way a provider that is not there fails.
struct Scripted {
    transport_fails: bool,
    calls: AtomicUsize,
}

impl Scripted {
    fn answering() -> Arc<Self> {
        Arc::new(Self {
            transport_fails: false,
            calls: AtomicUsize::new(0),
        })
    }

    fn dead() -> Arc<Self> {
        Arc::new(Self {
            transport_fails: true,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl FrontierClient for Scripted {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.transport_fails {
            true => Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            }),
            false => Ok(FrontierChunk::whole_response(
                "answered".to_string(),
                quote.prompt.len() as u64,
                0,
                CacheReadSource::Provider,
                8,
                0,
            )),
        }
    }
}

// -------------------------------------------------------------- classifier

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_WINDOW_ENGINE_KEY" => Some("sk-classification-window-engine".to_string()),
        _ => None,
    }
}

/// One permit, and a base URL nothing listens on: every test here holds that
/// permit for the whole run, so no call is ever admitted and no socket is ever
/// opened.
fn classify_config() -> ClassifyConfig {
    roundhouse_server::test_support::classification::classify_config(
        "http://127.0.0.1:1",
        |value| {
            value["revision"] = serde_json::json!(7);
            value["auth"]["env"] = serde_json::json!("CLASSIFY_WINDOW_ENGINE_KEY");
            value["caps"]["max_prior_classifications"] = serde_json::json!(WINDOW);
            value["executor"]["max_in_flight"] = serde_json::json!(1);
            value["executor"]["max_http_concurrency"] = serde_json::json!(1);
            value["executor"]["sweep_interval_ms"] = serde_json::json!(50000);
        },
    )
}

fn classifier() -> Arc<ClassificationRuntime<ByteTokenizer>> {
    compose(
        "<test>",
        &classify_config(),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present")
}

// --------------------------------------------------------------- the rig

/// capable = [alpha, beta], efficient = [gamma], capable first — so an ordinary
/// turn is served by alpha with beta behind it, and a dead alpha fails over
/// without needing a session shaped to force an escalation.
fn recipe() -> TierRecipe {
    TierRecipe::new(
        vec![format!("{PRIMARY}/m"), format!("{SECONDARY}/m")],
        vec![format!("{THRIFTY}/m")],
        PickerMode::CapableFirst,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe at the shipped threshold")
}

fn admission() -> Admission {
    Admission {
        tiers: Some(Arc::new(recipe())),
        ..Admission::open()
    }
}

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    classifier: Arc<ClassificationRuntime<ByteTokenizer>>,
}

fn rig(clients: Vec<(&str, Arc<dyn FrontierClient>)>) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let registry = FrontierClients::keyed(
        clients
            .into_iter()
            .map(|(provider, client)| (provider.to_string(), client))
            .collect(),
    );
    let classifier = classifier();
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog(),
        Arc::new(registry),
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    )
    .with_classifier(Arc::clone(&classifier));
    Rig {
        engine: Arc::new(engine),
        store,
        classifier,
    }
}

impl Rig {
    async fn turn(&self, session_id: &SessionId, turn: &str) {
        self.engine.create_session(session_id).await.unwrap();
        self.engine
            .run_turn(
                session_id,
                TurnId::new(turn),
                vec![Item::user_text("please answer this")],
                &admission(),
            )
            .await
            .expect("this fleet always has somewhere to go");
    }

    async fn events(&self, session_id: &SessionId) -> Vec<SessionEvent> {
        self.store
            .read_events(session_id, 0, 8_000)
            .await
            .expect("an in-memory log reads")
    }

    async fn decisions(&self, session_id: &SessionId) -> Vec<DecisionRecord> {
        self.events(session_id)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::Routed { decision, .. } => Some(decision),
                _ => None,
            })
            .collect()
    }
}

// ------------------------------------------------------- seeded history

fn identity() -> ClassifierIdentity {
    ClassifierIdentity {
        model: "jev-1.12".to_string(),
        schema: "typesafe.systemone.choice.v1".to_string(),
        taxonomy_version: TAXONOMY_VERSION,
        projection_revision: PROJECTION_REVISION,
        config_revision: 7,
    }
}

fn reservation() -> ReservationRecord {
    ReservationRecord {
        rate_card: ProviderPricing::free(),
        estimated_input_tokens: 100,
        expected_output_tokens: 16,
        requested_usd: 0.0002,
        hold_ttl_ms: 30_000,
        budget_limit_usd: 100.0,
        budget_window: BudgetWindow::Total,
        member_ceiling_usd: None,
        warn_at: 0.8,
    }
}

fn intent(turn: u64) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(format!("call_{turn:06}")),
        source_turn_index: turn,
        source_response_id: ResponseId::new(format!("resp_{turn:06}")),
        requested_at_ms: 0,
        expires_at_ms: 30_000,
        identity: identity(),
        reservation: reservation(),
    }
}

fn result(turn: u64) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(format!("call_{turn:06}")),
        source_turn_index: turn,
        source_response_id: ResponseId::new(format!("resp_{turn:06}")),
        completed_at_ms: turn,
        outcome: ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.8,
                },
                complexity: Graded {
                    value: TurnComplexity::Involved,
                    confidence: 0.6,
                },
                context_dependence: Graded {
                    value: ContextDependence::Recent,
                    confidence: 0.5,
                },
            },
            spend: EvaluationSpend::Unknown {
                granted_usd: 0.0,
                settled: SettlementAck::Committed,
            },
            reported_model: None,
        },
    }
}

/// `count` landed, usable classifications, committed through the session's own
/// writer into the log the engine is serving from.
///
/// The lease is taken and given back here, so the engine's next turn owns the
/// session exactly as it would have if these had arrived from a background
/// worker over the preceding four hundred turns.
async fn seed_history(store: &Arc<MemoryStore>, session_id: &SessionId, count: u64) {
    let mut session = Session::open(
        Arc::clone(store),
        session_id.clone(),
        "node-seed",
        TTL,
        CacheLedger::new(),
    )
    .await
    .expect("the engine released the session");
    for turn in 0..count {
        session
            .record_classification_intent(intent(turn))
            .await
            .expect("an intent commits");
        session
            .record_classification(result(turn))
            .await
            .expect("and its result");
    }
    session.release().await.expect("the lease goes back");
}

fn window_of(decision: &DecisionRecord) -> ClassificationWindow {
    selection_of(decision)
        .classifications
        .expect("a deployment with a classifier records a window on every routed turn")
}

fn selection_of(decision: &DecisionRecord) -> SelectionSnapshot {
    decision
        .selection
        .as_deref()
        .cloned()
        .expect("every routing event this engine writes carries one")
}

/// The log as a successor reads it: every event encoded and decoded, then
/// folded by a fresh projection.
async fn replayed(events: &[SessionEvent], session_id: &SessionId) -> SessionState {
    let round_tripped: Vec<SessionEvent> = events
        .iter()
        .map(|event| {
            let encoded = serde_json::to_string(event).expect("an event encodes");
            serde_json::from_str(&encoded).expect("and decodes to the same event")
        })
        .collect();
    SessionState::project(
        &ReplayLog::new(round_tripped),
        session_id,
        CacheLedger::new(),
        None,
    )
    .await
    .expect("a successor folds the log it is given")
}

// --------------------------------------------------------------- claims

/// **The claim.** Over four hundred landed classifications, the engine's own
/// snapshot names the newest four in order, counts all four hundred, and takes
/// its cutoff from the same position the features were read at -- and all of
/// that survives the wire and a successor's fold.
#[tokio::test]
async fn the_engine_records_a_bounded_window_over_a_long_classification_history() {
    let alpha = Scripted::answering();
    let rig = rig(vec![
        (PRIMARY, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
        (SECONDARY, Scripted::answering() as Arc<dyn FrontierClient>),
        (THRIFTY, Scripted::answering() as Arc<dyn FrontierClient>),
    ]);
    // Held for the whole test: the engine writes a window on every routed turn
    // and buys no classification of its own.
    let _held = rig
        .classifier
        .capacity()
        .expect("the one configured permit");

    let session_id = SessionId::new("sess_long_history");
    // A real first turn, so the log the history is seeded into is the log a
    // served session actually has: identity at the bottom, this turn's items
    // above it.
    rig.turn(&session_id, "t1").await;
    seed_history(&rig.store, &session_id, HISTORY).await;
    rig.turn(&session_id, "t2").await;

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 2, "one dispatch each, both answered");

    let before = window_of(&decisions[0]);
    assert_eq!(
        before.available, 0,
        "the first turn ran before any of the history existed"
    );
    assert!(before.named.is_empty());

    let window = window_of(&decisions[1]);
    assert_eq!(window.revision, PROJECTION_REVISION);
    assert_eq!(window.window, WINDOW, "the configured cap, as configured");
    assert_eq!(
        window.available, HISTORY as usize,
        "everything that had landed by the cutoff is counted, named or not"
    );
    assert_eq!(
        window
            .named
            .iter()
            .map(|reference| reference.source_turn_index)
            .collect::<Vec<_>>(),
        vec![HISTORY - 4, HISTORY - 3, HISTORY - 2, HISTORY - 1],
        "the newest {WINDOW}, oldest first"
    );
    assert_eq!(
        window.named[0].call_id,
        ResponseId::new(format!("call_{:06}", HISTORY - 4))
    );

    let features = selection_of(&decisions[1]).features;
    assert_eq!(
        window.cutoff_seq, features.observed_through_seq,
        "the window is taken at the position the features were read at"
    );
    assert!(
        window
            .named
            .iter()
            .all(|reference| reference.available_seq <= window.cutoff_seq),
        "and nothing above that position is nameable"
    );

    // Through the wire and a successor's fold: the same window, on the same
    // decision, with the same cutoff.
    let events = rig.events(&session_id).await;
    let state = replayed(&events, &session_id).await;
    let folded = state
        .last_decision()
        .expect("a served session has a last decision");
    assert_eq!(
        window_of(folded),
        window,
        "the window a successor reads is the window this engine wrote"
    );
    assert_eq!(alpha.calls(), 2, "two turns, two dispatches");
}

/// **The claim.** A failover writes one `Routed` per dispatch, and every one of
/// them carries the same window at the same cutoff -- the snapshot describes
/// the decision that was taken once, so a second attempt cannot re-read the
/// fold and quietly name a classification the first attempt could not see.
#[tokio::test]
async fn a_failover_repeats_one_window_and_one_cutoff_across_every_dispatch() {
    let dead = Scripted::dead();
    let alive = Scripted::answering();
    let rig = rig(vec![
        (PRIMARY, Arc::clone(&dead) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&alive) as Arc<dyn FrontierClient>),
        (THRIFTY, Scripted::answering() as Arc<dyn FrontierClient>),
    ]);
    let _held = rig
        .classifier
        .capacity()
        .expect("the one configured permit");

    let session_id = SessionId::new("sess_failover_window");
    rig.turn(&session_id, "t1").await;
    seed_history(&rig.store, &session_id, HISTORY).await;
    rig.turn(&session_id, "t2").await;

    assert_eq!(dead.calls(), 2, "alpha was tried on both turns");
    assert_eq!(alive.calls(), 2, "and beta answered both");

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 4, "two dispatches on each of two turns");
    assert_eq!(decisions[2].chosen, target(PRIMARY));
    assert_eq!(decisions[3].chosen, target(SECONDARY));

    let first = window_of(&decisions[2]);
    let second = window_of(&decisions[3]);
    assert_eq!(
        first, second,
        "a failover re-dispatches; it does not re-take the snapshot"
    );
    assert_eq!(first.available, HISTORY as usize);
    assert_eq!(first.named.len(), WINDOW);

    // The cutoff in particular: recomputed on the second attempt it would have
    // crept above the first attempt's own `Routed`.
    let events = rig.events(&session_id).await;
    let routed_seqs: Vec<u64> = events
        .iter()
        .filter(|event| matches!(event.kind, SessionEventKind::Routed { .. }))
        .map(|event| event.seq)
        .collect();
    assert_eq!(routed_seqs.len(), 4);
    assert!(
        first.cutoff_seq < routed_seqs[2],
        "the cutoff predates the turn's first dispatch: {} against {}",
        first.cutoff_seq,
        routed_seqs[2]
    );
    assert_eq!(
        second.cutoff_seq, first.cutoff_seq,
        "and the second dispatch did not move it forward"
    );

    let state = replayed(&events, &session_id).await;
    let folded = state
        .last_decision()
        .expect("a served session has a last decision");
    assert_eq!(
        window_of(folded),
        second,
        "and the fold of the encoded log agrees"
    );
}
