// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The selection snapshot as the engine actually writes it.
//!
//! The unit tests one layer down prove that each policy fills in its own
//! evidence. What only an engine can prove is the three joins around it:
//!
//! - the **real extractor** reaches the record — the signals on a routed turn
//!   are the ones a session's own tool traffic produced, under the dialect its
//!   client writes, and not a default-constructed `TurnSignals` that would pass
//!   on a build where nothing ever computed one;
//! - a **failover** writes the same selection onto every `Routed` it produces,
//!   with the per-dispatch fields still differing, and the log position the
//!   features were read at does not creep forward onto the second attempt;
//! - a **later turn under a different recipe** does not rewrite the earlier
//!   one, through the store and back.
//!
//! Every claim has a control that varies exactly one thing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::{CacheReadSource, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{
    AffinityPolicy, AttemptClass, CacheLedger, DecisionRecord, DecisionSource, PickerMode,
    ProviderPricing, SelectionSnapshot, SelectorBranch, StageEvidence, StagePolicy, Target, Tier,
    TierRecipe, TurnSignals,
};
use roundhouse_core::session::SessionState;
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_core::validate::ControlCallDialect;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

// ---------------------------------------------------------------------------
// The fleet
// ---------------------------------------------------------------------------

const PRIMARY: &str = "alpha";
const SECONDARY: &str = "beta";
const THRIFTY: &str = "gamma";

fn target(provider: &str) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: "m".into(),
    }
}

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

/// Answers, or fails the way a provider that is not there fails.
struct Scripted {
    transport_fails: bool,
    text: String,
    calls: AtomicUsize,
}

impl Scripted {
    fn answering(text: &str) -> Arc<Self> {
        Arc::new(Self {
            transport_fails: false,
            text: text.to_string(),
            calls: AtomicUsize::new(0),
        })
    }

    fn dead() -> Arc<Self> {
        Arc::new(Self {
            transport_fails: true,
            text: String::new(),
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
                self.text.clone(),
                quote.prompt.len() as u64,
                0,
                CacheReadSource::Provider,
                self.text.len() as u64,
                0,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// capable = [alpha, beta], efficient = [gamma] — two capable entries, so a
/// within-tier failover is possible and the plan has a runner-up to record.
fn recipe(picker: PickerMode) -> TierRecipe {
    TierRecipe::new(
        vec![format!("{PRIMARY}/m"), format!("{SECONDARY}/m")],
        vec![format!("{THRIFTY}/m")],
        picker,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe at the shipped threshold")
}

/// The mirror image, at a different threshold and picker: every field the
/// evidence records differs from [`recipe`]'s.
fn inverted_recipe() -> TierRecipe {
    TierRecipe::new(
        vec![format!("{THRIFTY}/m")],
        vec![format!("{SECONDARY}/m"), format!("{PRIMARY}/m")],
        PickerMode::CapableFirst,
        0.95,
    )
    .expect("the inverse recipe")
}

fn admission_with(recipe: TierRecipe) -> Admission {
    Admission {
        tiers: Some(Arc::new(recipe)),
        ..Admission::open()
    }
}

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
}

/// An engine over the three-provider catalog under the stage router.
///
/// No local fleet: every candidate is hosted, so nothing below can be satisfied
/// by a local worker quietly taking a turn.
fn rig_of(clients: Vec<(&str, Arc<dyn FrontierClient>)>) -> Rig {
    rig_over(
        clients,
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
    )
}

/// The same fleet under a router of the caller's choosing.
///
/// One claim below needs an engine composed the way a process that booted
/// before any project had a recipe is composed — the plain policy, no stage
/// wrapper — because the claim is about what a recipe that reaches such a
/// router is recorded as.
fn rig_over(
    clients: Vec<(&str, Arc<dyn FrontierClient>)>,
    policy: Arc<dyn roundhouse_core::routing::RoutingPolicy>,
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
        catalog(),
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

impl Rig {
    async fn turn(
        &self,
        session_id: &SessionId,
        turn: &str,
        input: Vec<Item>,
        admission: &Admission,
    ) -> roundhouse_server::TurnResult {
        self.engine.create_session(session_id).await.unwrap();
        self.engine
            .run_turn(session_id, TurnId::new(turn), input, admission)
            .await
            .expect("this fleet always has somewhere to go")
    }

    async fn events(&self, session_id: &SessionId) -> Vec<SessionEvent> {
        self.store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads")
    }

    /// Every routing decision in the log, oldest first, **read back out of the
    /// store** rather than taken from the turn's return value.
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

/// A read-only [`SessionStore`] over a fixed log.
///
/// [`SessionState::project`] reads its events through the store rather than
/// taking a slice, so replaying a log that has been through the wire — or a
/// *prefix* of one, which is what a successor picking a session up mid-history
/// sees — needs a reader over exactly those events. The writing half is
/// unreachable by construction: nothing here ever acquires a lease.
struct ReplayLog {
    events: Vec<SessionEvent>,
}

impl ReplayLog {
    fn new(events: Vec<SessionEvent>) -> Self {
        Self { events }
    }
}

#[async_trait]
impl SessionStore for ReplayLog {
    async fn create_session(&self, _: &SessionId, _: &str) -> Result<bool, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn acquire_lease(
        &self,
        _: &SessionId,
        _: &str,
        _: u64,
    ) -> Result<Option<Lease>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn renew_lease(&self, _: &Lease, _: u64) -> Result<Option<Lease>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn release_lease(&self, _: &Lease) -> Result<(), StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn append_events(
        &self,
        _: &Lease,
        _: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn read_events(
        &self,
        _: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Ok(self
            .events
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn last_seq(&self, _: &SessionId) -> Result<u64, StoreError> {
        Ok(self.events.last().map_or(0, |event| event.seq))
    }
}

fn call(id: &str, name: &str, arguments: &str) -> Item {
    Item::tool_call(id, name, arguments)
}

fn result(id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: id.into(),
            output: output.into(),
        },
        response_id: None,
    }
}

fn shell(command: &str) -> String {
    serde_json::json!({ "command": command }).to_string()
}

/// Nine tool exchanges, producing nothing, investigating nothing, and finishing
/// on a traceback — the shape `tier_selection.rs` scores as a stall.
///
/// Driven through the engine as *items*, because the join under test is the
/// extractor: a hand-built `TurnSignals` would pass on a build where the engine
/// never computed one.
fn a_stalling_session() -> Vec<Item> {
    let mut items = vec![Item::user_text("make the failing test pass")];
    for index in 0..9 {
        let id = format!("c{index}");
        items.push(call(&id, "shell_command", &shell("make build")));
        items.push(result(
            &id,
            match index {
                8 => "Traceback (most recent call last):\n  File \"x.py\"\n",
                _ => "linking...\n",
            },
        ));
    }
    items
}

fn ask() -> Vec<Item> {
    vec![Item::user_text("please answer this")]
}

fn selection_of(decision: &DecisionRecord) -> SelectionSnapshot {
    decision
        .selection
        .clone()
        .expect("every routing event this engine writes carries one")
}

fn stage_evidence(selection: &SelectionSnapshot) -> StageEvidence {
    match &selection
        .selector
        .as_ref()
        .expect("a staged turn records its branch")
        .branch
    {
        SelectorBranch::Stage(evidence) => evidence.clone(),
        other => panic!("expected the stage branch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. The extractor, the dialect and the recipe reach the record
// ---------------------------------------------------------------------------

/// **The claim.** A real tier turn commits the exact signals the extractor
/// computed, the dialect they were computed under, the source that picked the
/// tier, the plan the policy named, and the recipe as configured.
#[tokio::test]
async fn a_tier_turn_commits_the_features_the_selector_actually_saw() {
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    // `efficient_first`, so a turn that lands capable did so because the
    // signals put it there.
    let session_id = SessionId::generate();
    let served = rig
        .turn(
            &session_id,
            "t1",
            a_stalling_session(),
            &admission_with(recipe(PickerMode::EfficientFirst)),
        )
        .await;
    assert_eq!(served.text, "alpha answered");

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 1, "one dispatch, one routing event");
    let selection = selection_of(&decisions[0]);

    // --- the features -----------------------------------------------------
    assert_ne!(
        selection.features.signals,
        TurnSignals::default(),
        "a default-constructed signal set would pass every assertion a \
         first-turn fixture could make; this session has nine tool exchanges"
    );
    assert_eq!(
        selection.features.signals.turn_depth, 9,
        "nine tool exchanges, after the control-call drop"
    );
    assert!(
        selection.features.signals.tools.severity > 0.0,
        "the session ends on a traceback: {:?}",
        selection.features.signals.tools
    );
    assert_eq!(
        selection.features.signals.tools.recent_write_count, 0,
        "and it produced nothing, which is the other half of the stall"
    );
    assert_eq!(
        selection.features.dialect,
        ControlCallDialect::CodexResponses,
        "the session key names no Messages surface"
    );
    assert_eq!(
        selection.features.extractor_revision, 1,
        "the shipped extractor, spelled here so a change to it has to be a \
         deliberate edit of this test"
    );
    assert_eq!(selection.features.turn_index, 0, "the session's first turn");

    // --- the selection ----------------------------------------------------
    assert_eq!(selection.source, Some(DecisionSource::Dimensions));
    assert_eq!(selection.selected, target(PRIMARY));
    assert_eq!(
        selection.fallbacks,
        vec![target(SECONDARY)],
        "the rest of the capable tier, in the recipe's order"
    );
    assert_eq!(
        selection.admitted.as_ref().map(Vec::len),
        Some(3),
        "all three hosted candidates were admissible on this turn"
    );

    // --- the recipe -------------------------------------------------------
    let evidence = stage_evidence(&selection);
    assert_eq!(
        evidence.capable,
        vec![format!("{PRIMARY}/m"), format!("{SECONDARY}/m")]
    );
    assert_eq!(evidence.efficient, vec![format!("{THRIFTY}/m")]);
    assert_eq!(evidence.picker, PickerMode::EfficientFirst);
    assert_eq!(evidence.confidence_threshold, DEFAULT_CONFIDENCE_THRESHOLD);
    assert_eq!(evidence.pick.tier, Tier::Capable);
    assert_eq!(evidence.pick.source, DecisionSource::Dimensions);
    assert!(
        evidence.pick.confidence.unwrap_or_default() >= DEFAULT_CONFIDENCE_THRESHOLD,
        "the scorer cleared its own threshold, which is what `Dimensions` means"
    );
}

/// **The control.** The identical rig over a session with no tool traffic
/// records empty signals and the fall-open source — so the assertions above are
/// about this session's exchanges and not about a value the engine always
/// writes.
#[tokio::test]
async fn a_quiet_session_records_empty_signals_rather_than_the_stalling_ones() {
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        ask(),
        &admission_with(recipe(PickerMode::EfficientFirst)),
    )
    .await;

    let selection = selection_of(&rig.decisions(&session_id).await[0]);
    assert_eq!(selection.features.signals, TurnSignals::default());
    assert_eq!(selection.source, Some(DecisionSource::Ambiguous));
    assert_eq!(selection.selected, target(THRIFTY));
    assert!(
        selection.fallbacks.is_empty(),
        "the efficient tier has one member, so there is no runner-up"
    );
}

/// **The claim.** The dialect is a recorded property of the extraction, not a
/// constant: a session keyed the way the Messages surface keys one records the
/// other value.
#[tokio::test]
async fn the_recorded_dialect_follows_the_session_key() {
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    // The key shape `messages_api::wire::session_key` writes.
    let messages = SessionId::new("proj/anthropic_messages/abc");
    rig.turn(
        &messages,
        "t1",
        ask(),
        &admission_with(recipe(PickerMode::EfficientFirst)),
    )
    .await;
    assert_eq!(
        selection_of(&rig.decisions(&messages).await[0])
            .features
            .dialect,
        ControlCallDialect::ClaudeMessages
    );

    // CONTROL: the same rig, the same input, an ordinary key.
    let codex = SessionId::new("proj/plain/abc");
    rig.turn(
        &codex,
        "t1",
        ask(),
        &admission_with(recipe(PickerMode::EfficientFirst)),
    )
    .await;
    assert_eq!(
        selection_of(&rig.decisions(&codex).await[0])
            .features
            .dialect,
        ControlCallDialect::CodexResponses
    );
}

/// **The control.** A project's recipe that reaches a router which does not read
/// one is recorded as the branch that *did* choose — the inner affinity policy —
/// and never as a tier decision a recipe was consumed by.
///
/// The engine warns about this composition (`tier_selection.rs` pins the
/// warning); what the log must not do is report the turn as staged.
#[tokio::test]
async fn a_recipe_a_router_cannot_read_is_not_recorded_as_a_tier_decision() {
    let rig = rig_over(
        vec![
            (
                PRIMARY,
                Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
            ),
            (
                SECONDARY,
                Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
            ),
            (
                THRIFTY,
                Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
            ),
        ],
        // The composition of a process that booted before any project had a
        // recipe: no stage wrapper.
        Arc::new(AffinityPolicy::new()),
    );

    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        a_stalling_session(),
        &admission_with(recipe(PickerMode::EfficientFirst)),
    )
    .await;

    let selection = selection_of(&rig.decisions(&session_id).await[0]);
    match &selection
        .selector
        .as_ref()
        .expect("the policy that chose names itself")
        .branch
    {
        SelectorBranch::Affinity(_) => {}
        other => panic!("an unread recipe must not read as a consumed one: {other:?}"),
    }
    assert_eq!(
        selection.source, None,
        "no tier was decided, so there is no source to state"
    );
    assert!(
        selection.fallbacks.is_empty(),
        "and no ordered plan, because nothing picked a tier to order"
    );
    assert_eq!(
        selection.features.signals.turn_depth, 9,
        "the features are still the engine's, on every composition"
    );
}

// ---------------------------------------------------------------------------
// 2. One selection, many dispatches
// ---------------------------------------------------------------------------

/// **The claim.** A failover writes one `Routed` per dispatch, each with its own
/// chosen target and attempt history, and all of them with the *same* selection
/// — the source describes the choice that was made once, not a fresh choice by
/// each fallback.
#[tokio::test]
async fn a_failover_repeats_one_selection_across_every_dispatch() {
    let dead = Scripted::dead();
    let alive = Scripted::answering("beta answered");
    let rig = rig_of(vec![
        (PRIMARY, Arc::clone(&dead) as Arc<dyn FrontierClient>),
        (SECONDARY, Arc::clone(&alive) as Arc<dyn FrontierClient>),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    let session_id = SessionId::generate();
    let served = rig
        .turn(
            &session_id,
            "t1",
            a_stalling_session(),
            &admission_with(recipe(PickerMode::EfficientFirst)),
        )
        .await;
    assert_eq!(served.text, "beta answered");
    assert_eq!(dead.calls(), 1, "alpha was tried");
    assert_eq!(alive.calls(), 1, "and beta answered");

    let decisions = rig.decisions(&session_id).await;
    assert_eq!(decisions.len(), 2, "one `Routed` per dispatch");

    // The per-dispatch half still differs, which is what makes the identity
    // below a claim about the selection rather than about two identical rows.
    assert_eq!(decisions[0].chosen, target(PRIMARY));
    assert_eq!(decisions[1].chosen, target(SECONDARY));
    assert!(decisions[0].attempts.is_empty());
    assert_eq!(decisions[1].attempts.len(), 1);
    assert_eq!(decisions[1].attempts[0].target, target(PRIMARY));
    assert_eq!(decisions[1].attempts[0].class, AttemptClass::Transport);

    let first = selection_of(&decisions[0]);
    let second = selection_of(&decisions[1]);
    assert_eq!(
        first, second,
        "the selection happened once; a second attempt did not re-run the \
         scorer, re-read the fold or re-resolve admission"
    );
    assert_eq!(
        second.selected,
        target(PRIMARY),
        "and it still names the target the policy chose, not the one that served"
    );
    assert_eq!(second.fallbacks, vec![target(SECONDARY)]);
    assert_eq!(second.source, Some(DecisionSource::Dimensions));

    // --- the log position the features were read at -----------------------
    let events = rig.events(&session_id).await;
    let routed_seqs: Vec<u64> = events
        .iter()
        .filter(|event| matches!(event.kind, SessionEventKind::Routed { .. }))
        .map(|event| event.seq)
        .collect();
    assert_eq!(routed_seqs.len(), 2);
    let last_input_seq = events
        .iter()
        .filter(|event| {
            event.seq < routed_seqs[0]
                && matches!(event.kind, SessionEventKind::ItemAppended { .. })
        })
        .map(|event| event.seq)
        .next_back()
        .expect("the turn's input is committed before its first dispatch");

    let observed = first.features.observed_through_seq;
    assert!(
        observed >= last_input_seq,
        "the extractor read through this turn's own input: observed {observed}, \
         last input at {last_input_seq}"
    );
    assert!(
        observed < routed_seqs[0],
        "and it predates the first `Routed`: observed {observed}, first routed \
         at {}",
        routed_seqs[0]
    );
    assert!(
        observed < routed_seqs[1],
        "which the second attempt cannot creep past: recomputed per dispatch it \
         would sit above {}, the first attempt's own record",
        routed_seqs[0]
    );
}

// ---------------------------------------------------------------------------
// 3. Through the store, past a later turn
// ---------------------------------------------------------------------------

/// **The claim.** A later turn under a different recipe and different input does
/// not rewrite the earlier turn's snapshot, and both survive the wire.
#[tokio::test]
async fn a_later_turn_under_a_changed_recipe_leaves_the_earlier_record_alone() {
    let rig = rig_of(vec![
        (
            PRIMARY,
            Scripted::answering("alpha answered") as Arc<dyn FrontierClient>,
        ),
        (
            SECONDARY,
            Scripted::answering("beta answered") as Arc<dyn FrontierClient>,
        ),
        (
            THRIFTY,
            Scripted::answering("gamma answered") as Arc<dyn FrontierClient>,
        ),
    ]);

    let session_id = SessionId::generate();
    rig.turn(
        &session_id,
        "t1",
        a_stalling_session(),
        &admission_with(recipe(PickerMode::EfficientFirst)),
    )
    .await;
    // A second turn on the same session: different input, and a recipe that
    // differs in every field the evidence records.
    rig.turn(&session_id, "t2", ask(), &admission_with(inverted_recipe()))
        .await;

    // Read back out of the store and through the wire, which is what a
    // successor process replaying this log does.
    let replayed: Vec<SessionEvent> = rig
        .events(&session_id)
        .await
        .into_iter()
        .map(|event| {
            let json = serde_json::to_string(&event).expect("an event serializes");
            serde_json::from_str::<SessionEvent>(&json).expect("and reads back")
        })
        .collect();
    let decisions: Vec<DecisionRecord> = replayed
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 2, "two turns, one dispatch each");

    let first = selection_of(&decisions[0]);
    let second = selection_of(&decisions[1]);

    // The first turn still carries the first turn's recipe.
    let first_evidence = stage_evidence(&first);
    assert_eq!(
        first_evidence.capable,
        vec![format!("{PRIMARY}/m"), format!("{SECONDARY}/m")],
        "the recipe in force when this turn was routed, not the one in force \
         when the log is read"
    );
    assert_eq!(first_evidence.picker, PickerMode::EfficientFirst);
    assert_eq!(
        first_evidence.confidence_threshold,
        DEFAULT_CONFIDENCE_THRESHOLD
    );

    // And the second carries the second's, so neither is a shared default.
    let second_evidence = stage_evidence(&second);
    assert_eq!(second_evidence.capable, vec![format!("{THRIFTY}/m")]);
    assert_eq!(
        second_evidence.efficient,
        vec![format!("{SECONDARY}/m"), format!("{PRIMARY}/m")]
    );
    assert_eq!(second_evidence.picker, PickerMode::CapableFirst);
    assert_eq!(second_evidence.confidence_threshold, 0.95);

    // The features moved with the turn, not with the reader.
    assert_eq!(first.features.turn_index, 0);
    assert_eq!(second.features.turn_index, 1);
    assert_eq!(first.features.signals.turn_depth, 9);
    assert_eq!(
        second.features.signals.turn_depth, 9,
        "the second turn adds no tool exchanges of its own -- the depth it sees \
         is the committed history"
    );
    assert!(
        second.features.observed_through_seq > first.features.observed_through_seq,
        "a later turn read further into the log: {} then {}",
        first.features.observed_through_seq,
        second.features.observed_through_seq
    );
    assert_ne!(
        first, second,
        "two turns of one session are two selections, and a snapshot shared \
         between them would make every assertion above pass on one value"
    );

    // --- and the same thing through the fold a successor actually uses ----
    //
    // The assertions above read the `Routed` events directly. What a recovering
    // process reads is `SessionState::project`, and `last_decision` is the field
    // `explain_last_route` and the engine's own escalation gate consult — so the
    // snapshot has to survive the *fold* and not only the wire.
    let whole = SessionState::project(
        &ReplayLog::new(replayed.clone()),
        &session_id,
        CacheLedger::new(),
        None,
    )
    .await
    .expect("a replayed log projects");
    assert_eq!(
        whole
            .last_decision()
            .expect("two turns were routed")
            .selection
            .as_ref(),
        Some(&second),
        "the fold's last decision carries the last turn's snapshot, not a \
         merge of the two"
    );

    // The same fold stopped one turn earlier answers the *earlier* snapshot,
    // which is what says the value above is the log's and not a constant.
    let second_turn_start = replayed
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.kind, SessionEventKind::TurnStarted { .. }))
        .map(|(index, _)| index)
        .nth(1)
        .expect("two turns start");
    let prefix = SessionState::project(
        &ReplayLog::new(replayed[..second_turn_start].to_vec()),
        &session_id,
        CacheLedger::new(),
        None,
    )
    .await
    .expect("a prefix projects");
    assert_eq!(
        prefix
            .last_decision()
            .expect("the first turn was routed")
            .selection
            .as_ref(),
        Some(&first),
        "a successor that picked this session up between the two turns reads \
         the first turn's recipe and features, unchanged by what came after"
    );
}
