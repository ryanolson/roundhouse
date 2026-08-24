// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M6 of `PLAN-agentic-control-plane.md`: the validate/steer loop, wired.
//!
//! The trigger, the brief, the verdict parse, the action map and the three arms
//! are proved one layer down, in `roundhouse-core`'s own suite, against a
//! `SessionState` built by hand. What *cannot* be proved there is everything
//! this file is about: that the seam sits where its contract says, that the
//! records an occupant returns are committed by the engine under the lease,
//! that a side call books under its own model row and reaches no cache ledger,
//! and that a validation leaves the conversation byte-identical.
//!
//! **Every test here runs a real [`Validator`] over a real [`Engine`].** The
//! only doubles are the judge — scripted, because what a hosted model would say
//! is not the subject — and the signal, which always fires, because *when* to
//! ask is `trigger.rs`'s subject and a fixture that had to arrange a
//! ping-pong to get a validation would be testing the signal in every one of
//! these assertions.
//!
//! **The control in almost every test is the same session run without the
//! loop.** A validation that changes nothing is indistinguishable from a
//! validation that did not happen unless the log says both, so each probe is
//! paired with the unvalidated run it must match, and with an assertion that
//! the validating run genuinely validated.

use std::sync::Arc;
use std::time::Duration;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{MemorySpendLedger, TargetFilter, TurnPolicy};
use roundhouse_core::event::{SessionEventKind, SideCallAbandonReason, Usage};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::interject::Interjector;
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::metrics::{MetricsConfig, Scope, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{AffinityPolicy, CacheLedger, DecisionRecord, RoutingError, Target};
use roundhouse_core::session::{Session, SessionState};
use roundhouse_core::store::{MemoryStore, SessionStore, StoreError};
use roundhouse_core::validate::{
    ActionPolicy, Arm, ArmShares, JudgeClient, JudgeFailure, SteerChannel, ValidationTerms,
    Validator, ValidatorConfig,
};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierClient, FrontierModelSpec, OpenAiResponsesClient,
    StaticFrontierCatalog,
};
use roundhouse_mcp::{ControlStore, IntentRecord};
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, EngineError, FleetJudge, JudgeConfig,
};

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

mod common;
use common::frontier_catalog;
use common::validate::{
    AlwaysFires, OFF_TRACK, ON_TRACK, ScriptedJudge, judge_spec, judge_target, judge_usage,
    open_trigger,
};

/// What the echo provider answers an ordinary turn with.
const ANSWER: &str = "frontier answer";

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    /// The one control store, shared with the engine as `main::serve` shares
    /// it — so a test can write what an agent would have written through the
    /// MCP surface without going back through the wire.
    control: Arc<ControlStore>,
}

impl Rig {
    /// The deployment's own accounting, folded from the log it just wrote.
    fn snapshot(&self) -> roundhouse_core::metrics::MetricsSnapshot {
        self.engine.metrics().snapshot(
            &MetricsConfig::new(ShadowPricing::new(Vec::new())),
            now_ms(),
        )
    }

    /// The `n`th brief this rig's judge was shown.
    fn judge_brief(&self, judge: &ScriptedJudge, n: usize) -> String {
        judge
            .briefs()
            .get(n)
            .cloned()
            .unwrap_or_else(|| panic!("the judge was consulted fewer than {} times", n + 1))
    }

    fn model_row(&self, provider: &str) -> Option<roundhouse_core::metrics::ModelMetrics> {
        self.snapshot()
            .models
            .into_iter()
            .find(|row| row.provider == provider)
    }
}

/// A catalog of one model per `(name, quality_prior)` pair.
///
/// Priced identically to the shared fixture's single model, so the only axis
/// that varies between two entries is the one an escalation moves. Provider and
/// model share the name, which is what lets [`frontier`] name a target in one
/// word at the assertion site.
fn catalog_of(models: &[(&str, f64)]) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(
        models
            .iter()
            .map(|(name, quality_prior)| FrontierModelSpec {
                provider: (*name).into(),
                model: (*name).into(),
                quality_prior: *quality_prior,
                ..frontier_catalog().models()[0].clone()
            })
            .collect(),
    )
}

/// The target [`catalog_of`] gives the model it named `name`.
fn frontier(name: &str) -> Target {
    Target::Frontier {
        provider: name.into(),
        model: name.into(),
    }
}

/// A deployment with the validator installed over `judge`.
fn rig(judge: Arc<ScriptedJudge>) -> Rig {
    rig_with(judge, Arc::new(EchoFrontierClient::new(ANSWER)))
}

fn rig_with(judge: Arc<ScriptedJudge>, frontier: Arc<dyn FrontierClient>) -> Rig {
    rig_with_catalog(judge, frontier, frontier_catalog())
}

/// The same deployment over a caller-chosen catalog.
///
/// The escalation tests are the reason it is parameterized: what an escalation
/// does depends entirely on what the pool it narrows can reach, so a fixture
/// that could only ever quote the shared 0.95 model could not tell "the floor
/// selected the strongest candidate" from "the floor happened to be met".
fn rig_with_catalog(
    judge: Arc<ScriptedJudge>,
    frontier: Arc<dyn FrontierClient>,
    catalog: StaticFrontierCatalog,
) -> Rig {
    rig_over_judge(
        Arc::clone(&judge) as Arc<dyn JudgeClient>,
        frontier,
        catalog,
    )
}

/// The same deployment over any judge, scripted or real.
///
/// Split out for the one test whose subject *is* the real judge: what a
/// [`FleetJudge`] does when its dispatch cannot authenticate is a fact about
/// the fleet path, and a scripted double would be asserting the answer the test
/// wrote down.
fn rig_over_judge(
    judge: Arc<dyn JudgeClient>,
    frontier: Arc<dyn FrontierClient>,
    catalog: StaticFrontierCatalog,
) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let control = Arc::new(ControlStore::new());
    let validator = Validator::new(
        judge,
        ValidatorConfig {
            trigger: open_trigger(),
            arm_salt: "m6-fixture".into(),
            ..ValidatorConfig::default()
        },
    )
    .with_signals(vec![Box::new(AlwaysFires)]);
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            catalog,
            frontier,
            Arc::new(AffinityPolicy::new()),
            EngineConfig {
                arm_salt: "m6-fixture".into(),
                ..EngineConfig::default()
            },
        )
        .with_spend_ledger(Arc::new(MemorySpendLedger::new()))
        .with_control_store(Arc::clone(&control))
        .with_interjector(Arc::new(validator) as Arc<dyn Interjector>),
    );
    Rig {
        engine,
        store,
        control,
    }
}

/// The identical deployment with the loop never installed.
///
/// The control every "unchanged" assertion below is made against. Built from
/// the same constructor path so the only difference between a probe and its
/// control is the occupant of one seam.
fn unvalidated_rig() -> Rig {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            Arc::new(EchoFrontierClient::new(ANSWER)),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        )
        .with_spend_ledger(Arc::new(MemorySpendLedger::new())),
    );
    Rig {
        engine,
        store,
        control: Arc::new(ControlStore::new()),
    }
}

/// An enrolled membership whose arm is `arm` for certain.
///
/// A share table with all its weight on one arm, because a hash over a session
/// id is deterministic but not *chosen*: a fixture that wanted the Shadow arm
/// and drew Live would fail for a reason that has nothing to do with its
/// subject. `arm::tests` is where the split itself is pinned.
fn enrolled(arm: Arm, action: ActionPolicy) -> Admission {
    let shares = match arm {
        Arm::Live => ArmShares::new(1, 0, 0),
        Arm::Shadow => ArmShares::new(0, 1, 0),
        Arm::Placebo => ArmShares::new(0, 0, 1),
    }
    .expect("one weight is a table");
    Admission {
        validation: Some(ValidationTerms {
            shares,
            action,
            placebo_rate: 1.0,
        }),
        ..Admission::open()
    }
}

/// A membership that permits a synthetic call, whatever the client declared.
///
/// `ToolCall` rather than `Auto` because capability detection is §7's
/// milestone: under `Auto` the engine's `SteerCapability::Absent` would degrade
/// every correction to guidance, and no fixture here could reach outcome B.
fn tool_call_channel() -> ActionPolicy {
    ActionPolicy {
        channel: SteerChannel::ToolCall,
        steer_after_interventions: 1,
        ..ActionPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// Run `n` turns of one conversation, each a fresh user message.
///
/// Turn ids are content-free and distinct, which is what a client's growing
/// history produces; `an_identical_retry_...` is the one test that reuses one
/// deliberately.
async fn drive(
    rig: &Rig,
    session_id: &SessionId,
    admission: &Admission,
    n: usize,
) -> Vec<Result<roundhouse_server::TurnResult, EngineError>> {
    rig.engine
        .create_session(session_id)
        .await
        .expect("a fresh session");
    let mut results = Vec::new();
    for turn in 0..n {
        results.push(
            rig.engine
                .run_turn(
                    session_id,
                    TurnId::new(format!("t{turn}")),
                    vec![Item::user_text(format!("question {turn}"))],
                    admission,
                )
                .await,
        );
    }
    results
}

async fn events(store: &MemoryStore, session_id: &SessionId) -> Vec<SessionEventKind> {
    store
        .read_events(session_id, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

/// The conversation as the *prefix check* sees it: role and content, never the
/// response stamp.
///
/// Exactly the equality `Compat::same_item` applies, and for its reason: the
/// assistant history a client re-sends carries no id — it has no field to put
/// one in — so a comparison that included the stamp would fail on every turn
/// after the first and would say nothing about whether the conversation
/// changed. Two runs of the same conversation mint different response ids and
/// are the same conversation.
async fn conversation(
    store: &MemoryStore,
    session_id: &SessionId,
) -> Vec<(roundhouse_core::item::Role, ItemContent)> {
    stored_items(store, session_id)
        .await
        .into_iter()
        .map(|item| (item.role, item.content))
        .collect()
}

/// Every item the log holds, stamps and all.
///
/// The same filter `Compat::stored_items` runs, and deliberately the same one:
/// what the prefix check on the next turn is built from is exactly this list,
/// so a validation that added to it would fork the session.
async fn stored_items(store: &MemoryStore, session_id: &SessionId) -> Vec<Item> {
    store
        .read_events(session_id, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

fn validations(kinds: &[SessionEventKind]) -> usize {
    kinds
        .iter()
        .filter(|kind| matches!(kind, SessionEventKind::ValidationDecided { .. }))
        .count()
}

/// Every routing decision the log holds, in dispatch order.
///
/// The escalation probes read the decision rather than counting `Routed`
/// events, because what an escalation does is choose differently — the target,
/// the considered set and the policy digest are the three places that shows.
async fn decisions(store: &MemoryStore, session_id: &SessionId) -> Vec<DecisionRecord> {
    store
        .read_events(session_id, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect()
}

fn routings(kinds: &[SessionEventKind]) -> usize {
    kinds
        .iter()
        .filter(|kind| matches!(kind, SessionEventKind::Routed { .. }))
        .count()
}

/// Wait until `predicate` matches something in the log.
async fn await_event(
    store: &Arc<MemoryStore>,
    session_id: &SessionId,
    predicate: impl Fn(&SessionEventKind) -> bool,
) {
    for _ in 0..2_000 {
        if events(store, session_id).await.iter().any(&predicate) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the awaited event never arrived");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The sharpest assertion in the design: a validation leaves the conversation
/// alone.
///
/// Without it every later turn forks. The client re-sends its whole history on
/// every turn and this surface admits only the suffix, so one extra item in the
/// log — a verdict, a brief, a note about a side call — makes the next claim
/// disagree with the stored prefix, and the session is rebound to a fresh
/// generation with no warm prefix and no history. Silent from every side: each
/// turn still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_validator_verdict_never_becomes_a_conversation_item() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/validated");
    // `Off` on the channel so the action is `Continue` and the turn genuinely
    // dispatches: a steered turn's items *do* differ, and rightly, so the
    // comparison has to be made where nothing was supposed to change.
    let admission = enrolled(Arm::Live, ActionPolicy::default());
    for result in drive(&probe, &session_id, &admission, 3).await {
        result.expect("a validated turn still answers");
    }

    let control = unvalidated_rig();
    let control_id = SessionId::new("acme/ada/unvalidated");
    for result in drive(&control, &control_id, &Admission::open(), 3).await {
        result.expect("the control answers too");
    }

    // The probe must actually have validated, or this test compares two
    // identical unvalidated runs and passes for the wrong reason.
    let probe_events = events(&probe.store, &session_id).await;
    assert_eq!(judge.asked(), 2, "turns two and three are validated");
    assert_eq!(validations(&probe_events), 2);
    assert!(
        probe_events
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::SideCallCompleted { .. })),
        "and the money for those checks is in the log"
    );

    assert_eq!(
        conversation(&probe.store, &session_id).await,
        conversation(&control.store, &control_id).await,
        "a validation may cost money and change a policy; what it may never do \
         is put a byte into the conversation, because the conversation is what \
         the next turn's prefix check compares against"
    );

    // And nothing the judge said is anywhere in the items, under any encoding:
    // the equality above would also hold if both runs had somehow been given
    // the same extra item.
    let rendered: String = stored_items(&probe.store, &session_id)
        .await
        .iter()
        .map(Item::render)
        .collect();
    for forbidden in [
        "editing a file the task did not name",
        "on_track",
        "verdict",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "`{forbidden}` reached the conversation: {rendered}"
        );
    }
}

/// A membership that did not enrol is never validated, on a deployment that
/// installed the loop.
///
/// The shipped posture, and the one this milestone is most likely to get wrong
/// in the direction that matters: an operator upgrades, a project says nothing
/// about `validate`, and its traffic must be exactly what it was — no arm
/// stamp, no trigger, no judge, no side call, and not one extra store round
/// trip. The seam is still consulted on every turn; what changes is that it
/// decides nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_membership_that_did_not_enrol_is_never_validated() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/unenrolled");
    // `Admission::open()` is the unconfigured membership: no validate block, so
    // no terms, so nothing to enrol a session in.
    for result in drive(&probe, &session_id, &Admission::open(), 3).await {
        result.expect("answers");
    }

    assert_eq!(
        judge.asked(),
        0,
        "nobody asked for this and nobody paid for it"
    );
    let kinds = events(&probe.store, &session_id).await;
    assert_eq!(validations(&kinds), 0);
    assert_eq!(routings(&kinds), 3, "every turn dispatched, unchanged");
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::SessionCreated { arm: None, .. })),
        "and the log says so: an unstamped session is not enrolled, which is \
         what stops a later arm comparison being computed over a control group \
         that was never eligible"
    );

    // The control: the identical rig and the identical turns under an enrolled
    // membership does validate, so the silence above is the membership's and
    // not the fixture's.
    let enrolled_id = SessionId::new("acme/ada/enrolled");
    for result in drive(
        &probe,
        &enrolled_id,
        &enrolled(Arm::Live, ActionPolicy::default()),
        3,
    )
    .await
    {
        result.expect("answers");
    }
    assert_eq!(judge.asked(), 2);
}

/// Turning the loop off stops validating sessions that were already enrolled.
///
/// The arm stamp and the membership's terms are two independent gates, and this
/// is the case that separates them: the stamp says which arm a session was
/// *created* in and cannot be rewritten, so without the second gate an operator
/// who turned validation off would keep paying for every conversation that was
/// open when they did it. Off has to mean off from the next turn, which is what
/// an operator reaching for that switch believes they are buying.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_the_loop_off_stops_validating_a_session_already_stamped() {
    let judge = ScriptedJudge::always(ON_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/withdrawn");
    let on = enrolled(Arm::Live, ActionPolicy::default());
    probe
        .engine
        .create_session(&session_id)
        .await
        .expect("a fresh session");

    // Two turns while the project is enrolled: the session is stamped and the
    // second turn is checked.
    for turn in 0..2 {
        probe
            .engine
            .run_turn(
                &session_id,
                TurnId::new(format!("t{turn}")),
                vec![Item::user_text(format!("question {turn}"))],
                &on,
            )
            .await
            .expect("answers");
    }
    assert_eq!(judge.asked(), 1, "the stamp is in the log and the loop ran");

    // The operator turns it off. The stamp cannot be withdrawn — it is a fact
    // about a session that was created under it — so the terms are the only
    // thing that can stop the next turn.
    for turn in 2..4 {
        probe
            .engine
            .run_turn(
                &session_id,
                TurnId::new(format!("t{turn}")),
                vec![Item::user_text(format!("question {turn}"))],
                &Admission::open(),
            )
            .await
            .expect("answers");
    }
    assert_eq!(
        judge.asked(),
        1,
        "off has to mean off from the next turn, whatever the sessions already \
         in flight were stamped with"
    );
    let kinds = events(&probe.store, &session_id).await;
    assert_eq!(validations(&kinds), 1);
    assert_eq!(routings(&kinds), 4, "and every turn still answers");
}

/// The judge's money lands on the judge's row, and its prompt warms nothing.
///
/// Two isolations in one test because they are two halves of the same mistake.
/// A side call booked onto the conversation's row would make the dashboard
/// report the deployment's own overhead as the tenant's traffic; a side call
/// fed to the cache ledger would record a warm prefix on a target for a prompt
/// that is not a prefix of the conversation at all, and the router would then
/// price a hit nobody can serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_side_call_books_under_its_own_model_row_and_never_reaches_the_cache_ledger() {
    let judge = ScriptedJudge::always(ON_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/booking");
    let admission = enrolled(Arm::Live, ActionPolicy::default());
    for result in drive(&probe, &session_id, &admission, 2).await {
        result.expect("a validated turn still answers");
    }
    assert_eq!(judge.asked(), 1);
    assert_eq!(
        judge.keys(),
        ["acme/ada/booking#validate"],
        "the seam hands the judge the conversation it is checking, and the \
         implementation turns that into a key of its own -- `judge.rs` pins the \
         key's shape, and this pins that the right session reached it"
    );

    // --- the row ---------------------------------------------------------
    let judge_row = probe
        .model_row("judgeco")
        .expect("the judge's own model row exists");
    assert_eq!(judge_row.calls, 1, "one check, one call");
    assert_eq!(judge_row.tokens.output, judge_usage().output_tokens);
    let conversation_row = probe
        .model_row("anthropic")
        .expect("the conversation's row exists");
    assert_eq!(
        conversation_row.calls, 2,
        "two turns dispatched, and the check is not one of them: the dashboard \
         total equals the sum of its rows exactly once"
    );
    assert_ne!(
        conversation_row.tokens.output, judge_row.tokens.output,
        "and the two rows are genuinely separate numbers"
    );
    assert_eq!(
        probe
            .engine
            .metrics()
            .side_call_tally(Scope::Deployment)
            .completed,
        1,
    );

    // --- the ledger ------------------------------------------------------
    //
    // Projected through the same seeded ledger the engine uses, so what is
    // asserted is what the *next* turn's router would be told.
    let mut seeded = CacheLedger::new();
    frontier_catalog().apply_to_ledger(&mut seeded);
    seeded.register(
        &judge_target(),
        judge_spec().cache_model,
        judge_spec().pricing,
    );
    let state = SessionState::project(probe.store.as_ref(), &session_id, seeded, None)
        .await
        .expect("the session replays");
    assert_eq!(
        state
            .ledger
            .expected_cached_tokens(&judge_target(), now_ms(), 100_000),
        0.0,
        "a judge prompt is not a prefix of the conversation, and warming the \
         judge's target here would have the router price a hit nobody can serve"
    );
    // The control that makes the zero above mean something: the conversation's
    // own target *is* warm, so the ledger is live and this projection is
    // reading it.
    assert!(
        state.ledger.expected_cached_tokens(
            &Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            now_ms(),
            8,
        ) > 0.0,
        "the conversation's dispatches must have warmed their own target, or \
         the assertion above is about an empty ledger"
    );
}

/// A judge that does not answer costs the turn nothing and is marked anyway.
///
/// Both halves are load-bearing and they pull in opposite directions. The turn
/// must be *untouched* — the checker never breaks the checked — and the failure
/// must be *visible*, because an unaccounted call recorded as nothing is
/// indistinguishable from a free one, and a deployment whose judge has been
/// down for a week would read its validation spend as excellent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_validator_timeout_releases_the_turn_unchanged_and_is_marked_not_free() {
    let judge = ScriptedJudge::new(vec![Err(JudgeFailure::Abandoned {
        target: judge_target(),
        reason: SideCallAbandonReason::DeadlineExceeded,
    })]);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/timed-out");
    let admission = enrolled(Arm::Live, tool_call_channel());
    let results = drive(&probe, &session_id, &admission, 2).await;
    for result in &results {
        assert!(result.is_ok(), "a timed-out judge must not fail a turn");
    }

    // Unchanged: the turn dispatched and answered exactly as the control's did.
    let control = unvalidated_rig();
    let control_id = SessionId::new("acme/ada/control");
    for result in drive(&control, &control_id, &Admission::open(), 2).await {
        result.expect("the control answers");
    }
    assert_eq!(
        conversation(&probe.store, &session_id).await,
        conversation(&control.store, &control_id).await,
    );
    assert_eq!(
        results[1].as_ref().expect("answered").text,
        ANSWER,
        "the answer is the provider's, not the validator's"
    );

    // Not free: an abandoned side call in the log, a `JudgeFailed` decision
    // beside it, and an abandoned count on the fold.
    let kinds = events(&probe.store, &session_id).await;
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        SessionEventKind::SideCallAbandoned {
            reason: SideCallAbandonReason::DeadlineExceeded,
            ..
        }
    )));
    let tally = probe.engine.metrics().side_call_tally(Scope::Deployment);
    assert_eq!(
        (tally.completed, tally.abandoned),
        (0, 1),
        "a call that produced nothing is counted as a call that produced \
         nothing, never as one that billed zero"
    );

    // The control for the marking: an answering judge on the identical fixture
    // books a completion and no abandonment.
    let answered = rig(ScriptedJudge::always(ON_TRACK));
    let answered_id = SessionId::new("acme/ada/answered");
    for result in drive(&answered, &answered_id, &admission, 2).await {
        result.expect("answers");
    }
    let tally = answered.engine.metrics().side_call_tally(Scope::Deployment);
    assert_eq!((tally.completed, tally.abandoned), (1, 0));
}

/// The judge the boot check *cannot* ask about: one that resolves, and then
/// cannot authenticate.
///
/// [`judge_spec`] speaks Anthropic, which a Responses client refuses on the
/// dialect before it ever reaches the credential. This one speaks the dialect
/// the client serializes, so the refusal under test is the credential's.
fn openai_dialect_judge_spec() -> FrontierModelSpec {
    FrontierModelSpec {
        wire_protocol: roundhouse_fleet::WireProtocol::OpenAiResponses,
        ..judge_spec()
    }
}

/// What the boot promise check does **not** cover, and what happens instead.
///
/// `unkeepable_promises` asks one question about the judge — does
/// `ROUNDHOUSE_JUDGE_MODEL` name a model in this deployment's catalog — and
/// deliberately asks nothing about whether the side call can *authenticate*.
/// [`FleetJudge`] resolves no credential of its own (`judge.rs`: the deployment
/// tier is a second reader of the same keys, and which process holds it is a
/// design question M7 did not answer), so on a deployment composing a real
/// provider client every check is refused before a socket opens.
///
/// That is a **runtime fail-open**, not a boot failure, and it is the M6
/// interject contract holding: the checker never breaks the checked. This test
/// is what makes that sentence true rather than asserted, and it is the
/// evidence behind the comment at the boot check that used to claim the check
/// could see whether an enrolled project's validations would happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_judge_that_cannot_authenticate_abandons_its_check_and_the_turn_proceeds() {
    ensure_rustls_crypto_provider();
    // A real `FleetJudge` over a real provider client. No socket is opened and
    // none is needed: an absent credential is refused before one is.
    let judge = FleetJudge::new(
        Arc::new(OpenAiResponsesClient::new().expect("a client builds")) as Arc<dyn FrontierClient>,
        openai_dialect_judge_spec(),
        ByteTokenizer,
        EngineConfig::default().turn_deadline_ms,
        JudgeConfig::default(),
    );
    let probe = rig_over_judge(
        Arc::new(judge),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        frontier_catalog(),
    );
    let session_id = SessionId::new("acme/ada/unauthenticated-judge");
    let admission = enrolled(Arm::Live, tool_call_channel());

    // PROBE: the turns run. A judge that cannot authenticate must not be able
    // to fail the turn it was asked about — which is exactly why its
    // reachability is not a boot promise: there is no turn for it to break.
    for result in drive(&probe, &session_id, &admission, 2).await {
        assert!(
            result.is_ok(),
            "a judge that could not authenticate failed the turn it was checking: {result:?}"
        );
    }

    // And it is *loud*: the check is abandoned as unreachable, which is the
    // same answer an operator gets for a provider nobody could reach. Never a
    // silently skipped check, and never an unauthenticated request.
    let kinds = events(&probe.store, &session_id).await;
    assert!(
        kinds.iter().any(|kind| matches!(
            kind,
            SessionEventKind::SideCallAbandoned {
                reason: SideCallAbandonReason::Unreachable,
                ..
            }
        )),
        "the abandonment has to be in the log, or the fail-open is a silent one: {kinds:?}"
    );
    let tally = probe.engine.metrics().side_call_tally(Scope::Deployment);
    assert_eq!((tally.completed, tally.abandoned), (0, 1));

    // Unchanged: byte-for-byte the conversation an unvalidated deployment
    // produces. "The turn proceeds" is a claim about what the client got, not
    // just about a `Result` being `Ok`.
    let control = unvalidated_rig();
    let control_id = SessionId::new("acme/ada/unauthenticated-judge-control");
    for result in drive(&control, &control_id, &Admission::open(), 2).await {
        result.expect("the control answers");
    }
    assert_eq!(
        conversation(&probe.store, &session_id).await,
        conversation(&control.store, &control_id).await,
    );

    // CONTROL: the identical fixture over a judge that *can* answer completes
    // its check, so the abandonment above is the credential's doing and not the
    // enrolment's.
    let answered = rig(ScriptedJudge::always(ON_TRACK));
    let answered_id = SessionId::new("acme/ada/authenticated-judge");
    for result in drive(&answered, &answered_id, &admission, 2).await {
        result.expect("answers");
    }
    let tally = answered.engine.metrics().side_call_tally(Scope::Deployment);
    assert_eq!((tally.completed, tally.abandoned), (1, 0));
}

/// The observe-only arm: everything computed, everything logged, nothing done.
///
/// The control the whole instrumentation is built on, and the reason it needs
/// its own engine-level test: the occupant's discard is proved in core, but
/// what a deployment actually needs to know is that a Shadow session's *turns*
/// are indistinguishable from an unvalidated deployment's — same items, same
/// dispatch, and, crucially, the same policy on the turn after an escalation
/// the arm decided not to take.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shadow_arm_judges_and_releases_unchanged() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/shadow");
    // A channel that permits action, so the arm is the only thing standing
    // between the verdict and an intervention.
    let admission = enrolled(Arm::Shadow, tool_call_channel());
    let results = drive(&probe, &session_id, &admission, 3).await;
    for result in &results {
        result.as_ref().expect("a shadow turn is an ordinary turn");
    }

    assert_eq!(judge.asked(), 2, "the shadow arm pays for its verdicts");
    let kinds = events(&probe.store, &session_id).await;
    assert_eq!(validations(&kinds), 2);
    assert_eq!(
        routings(&kinds),
        3,
        "every turn dispatched: a discarded action changes nothing about the turn"
    );

    let control = unvalidated_rig();
    let control_id = SessionId::new("acme/ada/shadow-control");
    for result in drive(&control, &control_id, &Admission::open(), 3).await {
        result.expect("the control answers");
    }
    assert_eq!(
        conversation(&probe.store, &session_id).await,
        conversation(&control.store, &control_id).await,
    );

    // The sharp half. The verdict mapped to an escalation; had the arm acted,
    // the turn after it would have routed under a narrowed floor and carried a
    // different policy digest. Every digest in this session is the same one.
    let digests: Vec<String> = probe
        .store
        .read_events(&session_id, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.turn_policy_digest),
            _ => None,
        })
        .collect();
    assert_eq!(digests.len(), 3);
    assert!(
        digests.iter().all(|digest| *digest == digests[0]),
        "a shadow escalation must not narrow the turns that follow it: {digests:?}"
    );

    // And the fold can tell a shadow run from a live one on the identical
    // verdict, which is the comparison the instrumentation exists for.
    let shadow = probe
        .engine
        .metrics()
        .validation_tally(Scope::Deployment, Arm::Shadow);
    assert_eq!((shadow.judged, shadow.intervened), (2, 0));

    let live = rig(ScriptedJudge::always(OFF_TRACK));
    let live_id = SessionId::new("acme/ada/live");
    for result in drive(
        &live,
        &live_id,
        &enrolled(Arm::Live, tool_call_channel()),
        3,
    )
    .await
    {
        result.expect("answers");
    }
    let acted = live
        .engine
        .metrics()
        .validation_tally(Scope::Deployment, Arm::Live);
    assert!(
        acted.judged >= 1 && acted.intervened >= 1,
        "the identical verdict in the live arm is recorded as acted on — the \
         two rows differing is the whole measurement: {acted:?}"
    );

    // And the escalation the live arm *did* take binds the turn after it. This
    // is what makes the digest assertion above discriminating rather than a
    // statement that policy digests are constant: the same verdict, the same
    // channel, and a different arm gives a different second digest.
    let live_digests: Vec<String> = live
        .store
        .read_events(&live_id, 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.turn_policy_digest),
            _ => None,
        })
        .collect();
    assert!(
        live_digests.len() >= 2 && live_digests[1] != live_digests[0],
        "an escalation the live arm took must narrow the turn that follows it, \
         and it must do so through the log rather than a side channel: \
         {live_digests:?}"
    );
}

/// **The checker must never break the checked, and this is the sharp case.**
///
/// An escalation raises a quality floor. Nothing about the verdict that
/// produced it knows what this membership's pool can reach, so a shipped floor
/// of 0.8 over a deployment whose only model priors at 0.6 asks for something
/// that does not exist — and a narrowing that empties the candidate set fails
/// the turn with `PolicyRefused` for as many turns as the escalation lasts.
/// That is the validator breaking the conversation it was installed to protect,
/// on the deployment least able to absorb it.
///
/// So an escalation is *best-effort narrowing*: it selects the strongest
/// candidate the membership already admits, and the floor it asked for is
/// clamped to what the pool can reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escalation_above_the_whole_pool_selects_its_best_rather_than_refusing() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    // Every candidate below the shipped escalation floor of 0.8.
    let probe = rig_with_catalog(
        Arc::clone(&judge),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        catalog_of(&[("modest", 0.6)]),
    );
    let session_id = SessionId::new("acme/ada/floor-out-of-reach");
    let results = drive(
        &probe,
        &session_id,
        &enrolled(Arm::Live, tool_call_channel()),
        2,
    )
    .await;
    for (turn, result) in results.iter().enumerate() {
        result.as_ref().unwrap_or_else(|error| {
            panic!(
                "an escalation must never be the reason a turn fails: turn \
                 {turn} came back {error}"
            )
        });
    }

    // Turn 0 is unvalidated; turn 1 is the one the judge escalated on, and
    // `ActiveEscalation` counts from the turn it was decided on.
    assert_eq!(judge.asked(), 1, "turn 1 is the validated one");
    let decided = decisions(&probe.store, &session_id).await;
    assert_eq!(
        decided.len(),
        2,
        "both turns reached a provider: {:?}",
        decided.iter().map(|d| &d.chosen).collect::<Vec<_>>()
    );
    assert_eq!(decided[1].chosen, frontier("modest"));

    // The other half, and what keeps the clamp from being "drop the
    // escalation": the narrowing still reached routing. A dropped escalation
    // would leave the second turn's policy byte-identical to the first's.
    assert_ne!(
        decided[1].turn_policy_digest, decided[0].turn_policy_digest,
        "the escalation was clamped into reach, not discarded"
    );
    assert!(
        decided[1].rationale.contains("quality floor"),
        "an operator reading the audit trail has to be able to see that the \
         floor served was not the floor asked for: {:?}",
        decided[1].rationale
    );
}

/// The control that makes the clamp a *clamp* rather than a widening.
///
/// The identical verdict, the identical floor, over a pool that can meet it:
/// the escalation must select the strong candidate and drop the weak one from
/// the considered set, exactly as an unclamped narrowing would — and must say
/// nothing about having been clamped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escalation_the_pool_can_meet_narrows_to_the_floor_it_asked_for() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig_with_catalog(
        Arc::clone(&judge),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        catalog_of(&[("modest", 0.6), ("flagship", 0.95)]),
    );
    let session_id = SessionId::new("acme/ada/floor-in-reach");
    for result in drive(
        &probe,
        &session_id,
        &enrolled(Arm::Live, tool_call_channel()),
        2,
    )
    .await
    {
        result.expect("a reachable floor changes where a turn goes, not whether");
    }

    let decided = decisions(&probe.store, &session_id).await;
    assert_eq!(decided.len(), 2);
    assert_eq!(
        decided[1].chosen,
        frontier("flagship"),
        "0.8 admits only the flagship, and that is the whole point of escalating"
    );
    assert_eq!(
        decided[1]
            .considered
            .iter()
            .map(|candidate| candidate.target.clone())
            .collect::<Vec<_>>(),
        vec![frontier("flagship")],
        "the modest model is unreachable this turn, so the counterfactual must \
         not be priced against it"
    );
    assert!(
        !decided[1].rationale.contains("quality floor"),
        "nothing was clamped, so nothing may claim it was: {:?}",
        decided[1].rationale
    );
}

/// And the refusal that is still a refusal.
///
/// The clamp exists so that a floor *this deployment invented* cannot fail a
/// turn. A floor an operator wrote is the opposite: an empty candidate set
/// under the membership's own policy is the configured intent, and rescuing it
/// would route traffic to a model the key says it may never reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_floor_the_membership_itself_wrote_still_refuses() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig_with_catalog(
        Arc::clone(&judge),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        catalog_of(&[("modest", 0.6)]),
    );
    let session_id = SessionId::new("acme/ada/operator-refusal");
    let admission = Admission {
        policy: Arc::new(TurnPolicy {
            min_quality: 0.9,
            allow: TargetFilter::allow_all(),
            frontier_cadence: None,
        }),
        ..enrolled(Arm::Live, tool_call_channel())
    };
    for (turn, result) in drive(&probe, &session_id, &admission, 2)
        .await
        .into_iter()
        .enumerate()
    {
        assert!(
            matches!(
                result,
                Err(EngineError::Routing(RoutingError::PolicyRefused))
            ),
            "turn {turn} must be refused by the membership's own floor: {result:?}"
        );
    }
}

/// The judge is shown what the agent said it was doing, not what we guessed.
///
/// The declared objective is the whole reason `declare_intent` has a write
/// half: a stated goal turns the judge's question from "infer the goal, then
/// judge drift against your inference" into "here is the goal, name the
/// divergence". It lives in a node-local store the log cannot hold, so the
/// engine reads it at the seam — and if that read is dropped the brief silently
/// degrades to the last user message and nothing else changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_brief_carries_the_goal_the_agent_declared() {
    let judge = ScriptedJudge::always(ON_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/declared");
    probe.control.set_intent(
        &session_id,
        IntentRecord {
            goal: "make the trailing-comma parser test pass".into(),
            plan_steps: vec!["read the failing test".into(), "fix the tokenizer".into()],
            done_when: "pytest tests/parser is green".into(),
            declared_at_ms: now_ms(),
        },
    );
    for result in drive(
        &probe,
        &session_id,
        &enrolled(Arm::Live, ActionPolicy::default()),
        2,
    )
    .await
    {
        result.expect("answers");
    }

    let brief = probe.judge_brief(&judge, 0);
    assert!(
        brief.contains("make the trailing-comma parser test pass")
            && brief.contains("pytest tests/parser is green"),
        "the declared goal has to reach the judge, or the write half of \
         `declare_intent` buys nothing: {brief}"
    );

    // The control: the identical session with nothing declared falls back to
    // the last user message, which every session has — so the assertion above
    // is about the control store and not about briefs carrying text at all.
    let bare_judge = ScriptedJudge::always(ON_TRACK);
    let bare = rig(Arc::clone(&bare_judge));
    let bare_id = SessionId::new("acme/ada/undeclared");
    for result in drive(
        &bare,
        &bare_id,
        &enrolled(Arm::Live, ActionPolicy::default()),
        2,
    )
    .await
    {
        result.expect("answers");
    }
    let fallback = bare.judge_brief(&bare_judge, 0);
    assert!(
        !fallback.contains("pytest tests/parser is green") && fallback.contains("question 1"),
        "an undeclared session's brief is the log's own fallback: {fallback}"
    );
}

/// The sham arm: an interruption with no correction and no judge behind it.
///
/// The control for the Intervention Paradox. Without it, "tokens fell after we
/// steered" is consistent with the steer having said anything at all, because
/// being interrupted changes a trajectory on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_placebo_arm_intervenes_without_calling_the_judge() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/placebo");
    let admission = enrolled(Arm::Placebo, tool_call_channel());
    let results = drive(&probe, &session_id, &admission, 2).await;

    assert_eq!(
        judge.asked(),
        0,
        "the placebo arm consults nobody: that is what makes it a control for \
         the judge rather than a worse judge"
    );
    let intervened = results[1].as_ref().expect("an intervened turn completes");
    assert!(
        intervened.decision.is_none(),
        "nothing was dispatched, so nothing was decided"
    );
    assert!(
        intervened.text.contains("Re-read the task"),
        "the sham's interruption reaches the client as plain guidance, which is \
         what ends the agent's loop: got {:?}",
        intervened.text
    );
    assert_eq!(
        intervened.usage,
        Usage::default(),
        "and it genuinely cost nothing — the placebo is spend-matched in the \
         dashboard's arithmetic, never by pretending to have bought something"
    );

    let kinds = events(&probe.store, &session_id).await;
    assert_eq!(
        routings(&kinds),
        1,
        "the first turn routed and the intervened turn did not"
    );
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::SideCallCompleted { .. })),
        "no judge, no side call, no money"
    );
    let tally = probe
        .engine
        .metrics()
        .validation_tally(Scope::Deployment, Arm::Placebo);
    assert_eq!(
        (tally.decided, tally.judged, tally.not_run, tally.intervened),
        (1, 0, 1, 1),
        "a sham that logged nothing would be a control nobody could subtract"
    );

    // The control: the identical arm whose hashed timing says "not this turn"
    // proceeds, dispatches, and still records the decision.
    let quiet_judge = ScriptedJudge::always(OFF_TRACK);
    let quiet = rig(Arc::clone(&quiet_judge));
    let quiet_id = SessionId::new("acme/ada/placebo-quiet");
    let mut never = enrolled(Arm::Placebo, tool_call_channel());
    never.validation = never.validation.map(|terms| ValidationTerms {
        placebo_rate: 0.0,
        ..terms
    });
    for result in drive(&quiet, &quiet_id, &never, 2).await {
        result.expect("answers");
    }
    assert_eq!(quiet_judge.asked(), 0);
    let kinds = events(&quiet.store, &quiet_id).await;
    assert_eq!(
        routings(&kinds),
        2,
        "a quiet placebo turn is an ordinary turn"
    );
    assert_eq!(validations(&kinds), 1, "and it is still on the record");
}

/// A retry of a steered turn replays and never re-runs the judge.
///
/// The seam sits after the dedup short-circuit, and this is what that buys: a
/// client reconnecting through a flaky link re-POSTs the identical
/// conversation, the log answers from what it already holds, and the judge —
/// the one expensive thing on this path — is paid for once per turn id rather
/// than once per attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_retry_of_a_steered_turn_never_revalidates() {
    let judge = ScriptedJudge::always(OFF_TRACK);
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/steered");
    let admission = enrolled(Arm::Live, tool_call_channel());
    probe
        .engine
        .create_session(&session_id)
        .await
        .expect("a fresh session");

    // Turn 1 is unvalidated (turn 0 has no trajectory). Turn 2 escalates, which
    // is the default for a located divergence. Turn 3 therefore has one
    // intervention behind it and maps to the steer this test is about.
    let mut results = Vec::new();
    for turn in 0..3 {
        results.push(
            probe
                .engine
                .run_turn(
                    &session_id,
                    TurnId::new(format!("t{turn}")),
                    vec![Item::user_text(format!("question {turn}"))],
                    &admission,
                )
                .await
                .expect("each turn answers"),
        );
    }
    assert_eq!(judge.asked(), 2);
    let steered = stored_items(&probe.store, &session_id)
        .await
        .into_iter()
        .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .count();
    assert_eq!(
        steered, 1,
        "the fixture must actually have steered, or the retry below is a retry \
         of an ordinary turn"
    );
    let before = stored_items(&probe.store, &session_id).await;

    // The retry: the same turn id, which is what an identical resent
    // conversation hashes to.
    let replay = probe
        .engine
        .run_turn(
            &session_id,
            TurnId::new("t2"),
            vec![Item::user_text("question 2")],
            &admission,
        )
        .await
        .expect("a retry of a completed turn replays");

    // The steered turn is priced, and priced once. It emitted no `Routed`, so
    // the fold books it against no model row at all; what the client was told
    // it cost is what the check cost, which is the only figure that keeps this
    // deployment's own dashboard from exceeding it.
    let steered_turn = &results[2];
    assert_eq!(
        steered_turn.usage,
        judge_usage(),
        "a turn the judge caused not to run as asked is visible in the fold and \
         on the wire, never silently unpriced"
    );
    assert_eq!(
        routings(&events(&probe.store, &session_id).await),
        2,
        "three turns, two dispatches: the steered one booked nothing to any \
         model row for itself"
    );
    assert_eq!(
        probe
            .engine
            .metrics()
            .side_call_tally(Scope::Deployment)
            .completed,
        2,
        "while the two checks that produced those decisions booked once each, \
         under the judge's own row"
    );

    assert!(replay.deduplicated);
    assert_eq!(
        judge.asked(),
        2,
        "the judge is paid for once per turn id, not once per attempt: the \
         seam is consulted after the dedup short-circuit, and this is the \
         assertion that says so"
    );
    assert_eq!(
        stored_items(&probe.store, &session_id).await,
        before,
        "and a replay appends nothing, so the client's next prefix claim still \
         matches"
    );
}

/// A lease lost inside the side call settles the turn exactly like a lease lost
/// inside a dispatch.
///
/// The new exposure M6 introduces: the seam turns a zero-latency window into a
/// network round trip, so there is now a second place a takeover can land. The
/// claim is not that nothing goes wrong — something did — but that it goes
/// wrong in the *existing* vocabulary: the displaced owner cannot commit, the
/// turn never completes, and the successor finds it re-admittable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_lost_mid_side_call_settles_the_turn_like_any_mid_dispatch_failure() {
    let release = Arc::new(tokio::sync::Notify::new());
    let judge = ScriptedJudge::blocking(ON_TRACK, Arc::clone(&release));
    let probe = rig(Arc::clone(&judge));
    let session_id = SessionId::new("acme/ada/fenced");
    let admission = enrolled(Arm::Live, ActionPolicy::default());
    probe
        .engine
        .create_session(&session_id)
        .await
        .expect("a fresh session");
    // One completed turn, so the gate is open on the next.
    probe
        .engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("question 0")],
            &admission,
        )
        .await
        .expect("the first turn answers");

    let running = tokio::spawn({
        let engine = Arc::clone(&probe.engine);
        let session_id = session_id.clone();
        let admission = admission.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t1"),
                    vec![Item::user_text("question 1")],
                    &admission,
                )
                .await
        }
    });

    // Sequenced on the log rather than timed: the turn is admitted before the
    // seam is consulted, so a `TurnStarted` for `t1` proves the owner has
    // reached the side call the judge is holding shut.
    await_event(&probe.store, &session_id, |kind| {
        matches!(kind, SessionEventKind::TurnStarted { turn_id, .. } if turn_id.as_str() == "t1")
    })
    .await;
    probe.store.expire_lease_now(&session_id).await;
    let successor = Session::open(
        Arc::clone(&probe.store),
        session_id.clone(),
        "node-b",
        30_000,
        CacheLedger::new(),
    )
    .await
    .expect("an expired lease is claimable");
    release.notify_waiters();

    let error = running
        .await
        .expect("the task itself does not panic")
        .expect_err("a displaced owner must not be able to commit its decision");
    assert!(
        matches!(
            error,
            EngineError::Session(roundhouse_core::session::SessionError::Store(
                StoreError::LeaseLost { .. }
            ))
        ),
        "the fence has to be the failure, in the same vocabulary a fenced \
         dispatch fails in: got {error}"
    );
    assert_eq!(
        judge.asked(),
        1,
        "the side call did happen — this test proves nothing unless the \
         takeover landed while it was in flight"
    );

    // Settled like any mid-dispatch failure: the turn is not in the completed
    // set, so the client's retry re-admits it rather than replaying a decision
    // that was never written.
    assert!(
        successor
            .state()
            .completed_response_for(&TurnId::new("t1"))
            .is_none(),
        "a turn whose commit was fenced must stay retryable"
    );
    let kinds = events(&probe.store, &session_id).await;
    assert_eq!(
        validations(&kinds),
        0,
        "and nothing the fenced owner decided reached the log, because the log \
         has one writer and it is the holder of the lease"
    );
    // Same writer, same batch, same reason: the side call's own cost is part
    // of what the fenced owner decided, and it is committed in the same
    // append as `ValidationDecided` (`Session::record_control` /
    // `Session::complete_with_item` push the whole `ControlRecord` in one
    // `commit`). A fence therefore loses it too — see
    // `PLAN-agentic-control-plane.md`'s "The side-call" section, which
    // documents this as the actual behavior rather than a narrower promise
    // that record_control cannot keep without a second, non-atomic commit.
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::SideCallCompleted { .. })),
        "the side call's cost shares the fenced owner's single atomic commit \
         with the validation decision, so it is lost right along with it — \
         a second write here would itself be a second, unfenced writer on \
         the log"
    );
}
