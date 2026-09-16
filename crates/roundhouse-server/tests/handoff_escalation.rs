// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10.2 S6: the handoff note rides a *tier* escalation too, and only a real one.
//!
//! R2 shipped one trigger for the note — the validate loop's escalation — and
//! `validate_loop.rs` pins its rendering, its once-per-switch gate and the
//! property that the note never reaches the stored log. This file is about the
//! second trigger and nothing else: the stage router moving a session onto the
//! capable tier because a *signal* said the cheap one was in trouble.
//!
//! **Every assertion here is against the prompt a scripted transport actually
//! received.** The gate is a two-term `||` over a three-term predicate, and a
//! test that read the boolean would be asserting that `&&` works. What the
//! feature promises is that a particular model sees a particular sentence, so
//! that is what is checked — and its absence is checked the same way, because
//! the expensive failure of this surface is a note riding a turn that did not
//! earn one.
//!
//! Six ways a turn can *look* like a tier escalation without being one, one
//! test each:
//!
//! | Case | What blocks the note | The check doing the blocking |
//! |---|---|---|
//! | ambiguous fall-open onto capable | nothing said the cheap tier was in trouble | `DecisionSource::is_signal_driven` |
//! | signal-driven *de*-escalation | the capable model is not the one answering | capable-list membership |
//! | the second capable turn in a row | the switch already happened | the `last_decision` read |
//! | picked capable, key admits none of it | the capable model is not the one answering | capable-list membership |
//! | no note configured | this deployment did not opt in | the config read |
//! | no recipe on the project | there is no tier to have moved between | the `ctx.tiers` read |
//!
//! And one way it can look like *not* one while being one: a failover inside
//! the escalating turn, which is the case the gate's placement above the
//! dispatch loop exists for.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{TargetFilter, TurnPolicy};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::{
    AffinityPolicy, DecisionSource, ProviderPricing, StagePolicy, Target, TierRecipe,
};
use roundhouse_core::routing::{PickerMode, stage::DEFAULT_CONFIDENCE_THRESHOLD};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{EXAMPLE_HANDOFF_NOTE, HANDOFF_MARKER, ValidationTerms};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

mod common;

// ---------------------------------------------------------------------------
// Three providers: two in the capable tier, one in the efficient tier
// ---------------------------------------------------------------------------

/// Capable, first choice.
const ALPHA: &str = "alpha";
/// Capable, the fallback — which is what makes the failover case reachable.
const BETA: &str = "beta";
/// The efficient tier, alone in it.
const GAMMA: &str = "gamma";

fn target(provider: &str) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: "m".into(),
    }
}

/// [`frontier_spec`] (M15, H2): the same eight-field literal H2 named ten
/// other copies of, with `provider` and `quality_prior` the two arguments
/// this file's own escalation tests actually vary.
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
    StaticFrontierCatalog::new(vec![spec(ALPHA, 0.95), spec(BETA, 0.90), spec(GAMMA, 0.60)])
}

/// A transport that answers, and keeps every prompt it was handed.
///
/// The prompts are the whole point of the fixture: "the note fired" is a claim
/// about what a provider received, and nothing else in the process can be asked
/// about it — by design, since `validate::handoff` makes the note reach the
/// forwarded request and nowhere else.
struct Recording {
    /// `None` answers; `Some` fails the way a provider that is not there fails,
    /// which is a retryable class and therefore falls forward.
    dead: bool,
    prompts: Mutex<Vec<String>>,
}

impl Recording {
    fn answering() -> Arc<Self> {
        Arc::new(Self {
            dead: false,
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn dead() -> Arc<Self> {
        Arc::new(Self {
            dead: true,
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .expect("no test panics holding it")
            .clone()
    }

    /// The one prompt this transport was handed, when the claim is that it was
    /// handed exactly one.
    fn only_prompt(&self, whose: &str) -> String {
        let prompts = self.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "{whose} was expected to serve exactly one dispatch, got {}",
            prompts.len()
        );
        prompts.into_iter().next().unwrap()
    }
}

#[async_trait]
impl FrontierClient for Recording {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.prompts
            .lock()
            .expect("no test panics holding it")
            .push(quote.prompt.clone());
        match self.dead {
            true => Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            }),
            false => Ok(FrontierChunk::whole_response(
                "answered".into(),
                quote.prompt.len() as u64,
                0,
                8,
                0,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// capable = [alpha, beta], efficient = [gamma].
///
/// `threshold` is a parameter because one case below needs the scorer to act on
/// a *negative* score, and the shipped operating point cannot: the most
/// de-escalating session reachable — pure production, nothing wrong, tests not
/// yet run — scores `tanh(-0.5) ≈ -0.4621`, which is deliberately under the
/// shipped `0.5`. A deployment that wants signal-driven de-escalation lowers the
/// threshold, and the case that proves the note does not narrate one has to be
/// run at a setting where the scorer can produce it at all.
fn recipe(picker: PickerMode, threshold: f64) -> TierRecipe {
    TierRecipe::new(
        vec![format!("{ALPHA}/m"), format!("{BETA}/m")],
        vec![format!("{GAMMA}/m")],
        picker,
        threshold,
    )
    .expect("a two-tier recipe")
}

/// A membership that would narrate an escalation if one happened.
///
/// `ValidationTerms::default()` for everything but the note: no interjector is
/// installed on the rig below, so the validate loop decides nothing and
/// `this_turn_opened_an_escalation` is `false` on every turn here. That is the
/// isolation the whole file rests on — a note that appears can only have come
/// from the tier half of the gate.
fn narrating(picker: PickerMode) -> Admission {
    Admission {
        tiers: Some(Arc::new(recipe(picker, DEFAULT_CONFIDENCE_THRESHOLD))),
        validation: Some(ValidationTerms {
            handoff_note: Some(EXAMPLE_HANDOFF_NOTE.to_string()),
            ..ValidationTerms::default()
        }),
        ..Admission::open()
    }
}

/// The same membership with the surface off, which is what every deployment
/// that has not opted in runs.
fn silent(picker: PickerMode) -> Admission {
    Admission {
        validation: Some(ValidationTerms::default()),
        ..narrating(picker)
    }
}

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
}

fn rig_of(clients: Vec<(&str, Arc<dyn FrontierClient>)>) -> Rig {
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
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
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
    /// One turn on a fresh session.
    async fn turn(
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
    async fn two_turns(
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
    async fn stored_text(&self, session_id: &SessionId) -> String {
        self.store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads")
            .iter()
            .map(|event| serde_json::to_string(&event.kind).expect("an event serializes"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Sessions with a shape the scorer can read
// ---------------------------------------------------------------------------

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

/// A session that ends on the one class of failure the scorer refuses to score:
/// `CRITICAL` severity, which is the hard escalate.
///
/// `Override` rather than `Dimensions` on purpose. Both are signal-driven and
/// both must narrate, but the override arm reaches the capable tier without
/// consulting the threshold at all — so a probe built on it stays true if a
/// deployment moves its `confidence_threshold`, and the case cannot start
/// passing for the wrong reason when someone retunes the scorer.
fn a_critical_session() -> Vec<Item> {
    let mut items = vec![Item::user_text("make the failing test pass")];
    for index in 0..3 {
        let id = format!("c{index}");
        items.push(call(&id, "shell_command", &shell("make build")));
        items.push(result(
            &id,
            match index {
                2 => "Traceback (most recent call last):\nMemoryError\n",
                _ => "linking...\n",
            },
        ));
    }
    items
}

/// The agent is producing work, nothing is broken, and the tests have not been
/// run — so `should_deescalate` does not fire and the *scorer* is what answers.
///
/// `production_intensity` is 1.0 (every recent op is a write), every other
/// dimension is zero, so the score is `tanh(5 * 0.1 * -1.0) = -0.4621` and the
/// pick is a `Dimensions` de-escalation at any threshold below that.
fn a_producing_session() -> Vec<Item> {
    let mut items = vec![Item::user_text("write the module")];
    for index in 0..3 {
        let id = format!("w{index}");
        items.push(call(&id, "write", r#"{"path":"src/lib.rs"}"#));
        items.push(result(&id, "wrote 12 lines"));
    }
    items
}

fn ask() -> Vec<Item> {
    vec![Item::user_text("please answer this")]
}

fn note_block() -> String {
    format!("\n\n{HANDOFF_MARKER} {EXAMPLE_HANDOFF_NOTE}")
}

// ---------------------------------------------------------------------------
// The claim
// ---------------------------------------------------------------------------

/// **S6's claim.** A signal that moves a session onto the capable tier decorates
/// the request the capable model receives, and decorates nothing else.
///
/// The probe and its control differ in exactly one field of the admission — the
/// note — so the difference in what alpha received is attributable to the gate
/// and not to the fixture. Equality against `plain + block` rather than
/// `contains`: a `contains` also passes on a request that carried the note
/// twice, or carried it in the middle of the conversation, or was rebuilt
/// around it.
#[tokio::test]
async fn a_signal_driven_move_to_the_capable_tier_narrates_it() {
    async fn run(admission: &Admission) -> (Rig, Arc<Recording>, SessionId, DecisionSource) {
        let alpha = Recording::answering();
        let rig = rig_of(vec![
            (ALPHA, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
            (BETA, Recording::answering() as Arc<dyn FrontierClient>),
            (GAMMA, Recording::answering() as Arc<dyn FrontierClient>),
        ]);
        let (session_id, result) = rig.turn(a_critical_session(), admission).await;
        let decision = result.decision.expect("a dispatched turn records one");
        assert_eq!(
            decision.target,
            target(ALPHA),
            "the fixture must reach the capable tier or it is testing nothing: {}",
            decision.rationale
        );
        let source = decision.source.expect("a staged decision names its source");
        (rig, alpha, session_id, source)
    }

    // `efficient_first`, so landing on the capable tier is the *signal's* doing
    // and not the picker's.
    let (probe, narrated, probe_session, source) =
        run(&narrating(PickerMode::EfficientFirst)).await;
    let (_control, plain, _, control_source) = run(&silent(PickerMode::EfficientFirst)).await;

    assert_eq!(
        source,
        DecisionSource::Override,
        "a critical result is the hard escalate, which is what makes this probe \
         independent of the confidence threshold"
    );
    assert_eq!(source, control_source, "the two runs decided identically");

    let narrated = narrated.only_prompt("alpha");
    let plain = plain.only_prompt("alpha");
    assert_eq!(
        narrated,
        format!("{plain}{}", note_block()),
        "the escalated request must be the undecorated one plus roundhouse's \
         line, and nothing else"
    );
    assert!(
        narrated.ends_with(EXAMPLE_HANDOFF_NOTE),
        "the note rides the trailing message — on this wire the whole render is \
         one user message, so the end of the prompt is the end of it"
    );
    assert!(
        !plain.contains(HANDOFF_MARKER),
        "the control must carry nothing: an unconfigured note decorates nothing"
    );

    // R2's safety argument, which S6 inherits whole: the note is not in the log,
    // so a client's resend and the prefix hash over it cannot notice that a tier
    // moved. Asserted over the serialized events rather than the items alone,
    // because a decision's *rationale* is republished into the calling model's
    // context by `explain_last_route` and would be a second way to leak it.
    assert!(
        !probe
            .stored_text(&probe_session)
            .await
            .contains(HANDOFF_MARKER),
        "nothing roundhouse decorated a request with may reach the durable log"
    );
}

/// **The source check's job.** A `capable_first` project serves the capable tier
/// on every unremarkable turn, and none of those is an escalation.
///
/// Under `efficient_first` this case proves nothing — an ambiguous turn lands on
/// the *efficient* tier there, so the membership check would block the note and
/// the source check would never be reached. `capable_first` is what puts an
/// `Ambiguous` decision on a capable target, which is the only configuration
/// where `is_signal_driven` is load-bearing.
#[tokio::test]
async fn an_ambiguous_turn_on_the_capable_tier_narrates_nothing() {
    let alpha = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
        (BETA, Recording::answering() as Arc<dyn FrontierClient>),
        (GAMMA, Recording::answering() as Arc<dyn FrontierClient>),
    ]);

    let (_, result) = rig.turn(ask(), &narrating(PickerMode::CapableFirst)).await;
    let decision = result.decision.unwrap();
    assert_eq!(decision.target, target(ALPHA));
    assert_eq!(
        decision.source,
        Some(DecisionSource::Ambiguous),
        "the fixture must fall open or the source check is not what is under \
         test: {}",
        decision.rationale
    );
    assert!(
        !alpha.only_prompt("alpha").contains(HANDOFF_MARKER),
        "a fall-open would tell the capable model the cheap tier had been \
         stalling on a turn where nothing said it was — upstream's \
         `only_on_wrong_signal_escalation`, and the reason the gate reads a \
         typed source rather than the rationale prose"
    );
}

/// **The membership check's job.** A signal can move a turn *down* a tier, and
/// the model that gets it is the cheap one — which has no preceding trouble to
/// be told about.
///
/// This is the only input where the tier half of the gate changes the answer:
/// `Override` is always capable, `TestsPassed` is never signal-driven, and
/// `Ambiguous` is never signal-driven. A gate that checked only the source would
/// pass every other case in this file and fail exactly here.
#[tokio::test]
async fn a_signal_driven_de_escalation_narrates_nothing() {
    let gamma = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Recording::answering() as Arc<dyn FrontierClient>),
        (BETA, Recording::answering() as Arc<dyn FrontierClient>),
        (GAMMA, Arc::clone(&gamma) as Arc<dyn FrontierClient>),
    ]);

    // Below `tanh(-0.5)`, so the scorer's negative answer is acted on. See
    // `recipe`.
    let admission = Admission {
        tiers: Some(Arc::new(recipe(PickerMode::CapableFirst, 0.4))),
        ..narrating(PickerMode::CapableFirst)
    };
    let (_, result) = rig.turn(a_producing_session(), &admission).await;
    let decision = result.decision.unwrap();
    assert_eq!(
        decision.source,
        Some(DecisionSource::Dimensions),
        "the scorer must be what decided, and by a margin the threshold accepts: \
         {}",
        decision.rationale
    );
    assert_eq!(
        decision.target,
        target(GAMMA),
        "and it must have decided *downwards*, or the case is the escalating one \
         again: {}",
        decision.rationale
    );
    assert!(
        !gamma.only_prompt("gamma").contains(HANDOFF_MARKER),
        "a de-escalation is signal-driven and must still narrate nothing: the \
         note claims the preceding steps are not to be trusted, and it is the \
         *efficient* model reading it"
    );
}

/// **The `last_decision` read's job.** The note rides the turn a session
/// switches on, and never the ones after it.
///
/// Both turns escalate for the same reason — the critical result stays inside
/// the scorer's trailing window, so turn two is an `Override` too — which is
/// what makes this a test about the switch rather than about the signal fading.
#[tokio::test]
async fn only_the_first_capable_turn_of_a_run_narrates() {
    let alpha = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
        (BETA, Recording::answering() as Arc<dyn FrontierClient>),
        (GAMMA, Recording::answering() as Arc<dyn FrontierClient>),
    ]);

    let results = rig
        .two_turns(
            a_critical_session(),
            ask(),
            &narrating(PickerMode::EfficientFirst),
        )
        .await;
    for (index, result) in results.iter().enumerate() {
        let decision = result.decision.as_ref().expect("both turns dispatched");
        assert_eq!(
            (index, &decision.target, decision.source),
            (index, &target(ALPHA), Some(DecisionSource::Override)),
            "both turns must reach the capable tier by override, or this is a \
             test about a signal fading: {}",
            decision.rationale
        );
    }

    let prompts = alpha.prompts();
    assert_eq!(prompts.len(), 2, "alpha served both turns");
    assert!(
        prompts[0].contains(HANDOFF_MARKER),
        "the turn that switched tiers narrates"
    );
    assert!(
        !prompts[1].contains(HANDOFF_MARKER),
        "and the next one does not: a note must ride once per switch and never \
         accumulate, which is the same promise the validate loop's half makes \
         through `this_turn_opened_an_escalation`"
    );
}

/// **The membership check's other job**, and the reason the gate asks about the
/// *target* rather than about a tier recorded on the decision.
///
/// The scorer picks capable, the key admits none of the capable tier, and
/// `StagePolicy` falls to the efficient tier — so the model answering is the one
/// that was already answering. A gate keyed on the tier the scorer *picked*
/// would narrate here, and the note would tell the cheap model that the
/// preceding steps came from something in trouble and that a change of hands had
/// happened. Neither is true.
#[tokio::test]
async fn a_capable_pick_the_key_cannot_reach_narrates_nothing() {
    let gamma = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Recording::answering() as Arc<dyn FrontierClient>),
        (BETA, Recording::answering() as Arc<dyn FrontierClient>),
        (GAMMA, Arc::clone(&gamma) as Arc<dyn FrontierClient>),
    ]);

    let admission = Admission {
        policy: Arc::new(TurnPolicy {
            allow: TargetFilter::parse([format!("{GAMMA}/*")]).expect("one glob parses"),
            ..TurnPolicy::unrestricted()
        }),
        ..narrating(PickerMode::EfficientFirst)
    };
    let (_, result) = rig.turn(a_critical_session(), &admission).await;
    let decision = result.decision.unwrap();
    assert_eq!(
        decision.source,
        Some(DecisionSource::Override),
        "the signal still fired — the pick is unchanged, only the pool is: {}",
        decision.rationale
    );
    assert_eq!(
        decision.target,
        target(GAMMA),
        "and the turn fell to the tier the key can reach: {}",
        decision.rationale
    );
    assert!(
        decision.rationale.contains("admits none of it"),
        "the rationale says why, which is the operator-facing half of the same \
         fact: {}",
        decision.rationale
    );
    assert!(
        !gamma.only_prompt("gamma").contains(HANDOFF_MARKER),
        "roundhouse cannot promise a stronger model answered when the pool held \
         none — `validate::handoff` refuses that claim in the wording, and this \
         is the same refusal in the gate"
    );
}

/// **The control for the whole file.** A project that configured no recipe
/// reaches none of this.
///
/// The gate's first read is `ctx.tiers`, which is `None` for every deployment
/// that has not opted into tier routing — the overwhelming majority, and the
/// ones for whom a note appearing out of nowhere would be a regression rather
/// than a feature. Without this claim, an implementation that decorated
/// unconditionally would still satisfy every test above, because every test
/// above configures a recipe.
///
/// The session driven here is the same stalling one that *does* narrate under a
/// recipe, and under the wrapper the turn even lands on the same target —
/// `AffinityPolicy` picks the highest-quality admissible candidate, which is
/// alpha. So the only thing that differs from the escalating claim is the
/// recipe.
#[tokio::test]
async fn a_project_with_no_recipe_narrates_nothing() {
    let alpha = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
        (BETA, Recording::answering() as Arc<dyn FrontierClient>),
        (GAMMA, Recording::answering() as Arc<dyn FrontierClient>),
    ]);
    let admission = Admission {
        tiers: None,
        ..narrating(PickerMode::EfficientFirst)
    };

    let (_, result) = rig.turn(a_critical_session(), &admission).await;
    let decision = result.decision.expect("a dispatched turn records one");
    assert_eq!(
        (decision.target, decision.source),
        (target(ALPHA), None),
        "a policy that picks a candidate rather than a tier reports no source, \
         which is the state the gate's `is_signal_driven` read must survive"
    );
    assert!(
        !alpha.only_prompt("alpha").contains(HANDOFF_MARKER),
        "the note is configured and the session is in trouble, and still nothing \
         rides: there is no tier for the session to have moved between"
    );
}

/// **The gate's placement.** A failover inside the escalating turn does not eat
/// the note.
///
/// The hazard is specific to asking `last_decision` about the previous turn:
/// there is one `Routed` per *dispatch*, so this turn's own first attempt
/// becomes `last_decision` before the second attempt is planned. A gate computed
/// inside the dispatch loop reads its own record on the second pass, sees a
/// capable target, concludes the session was already capable, and drops the note
/// — on exactly the turns where a provider died, which is when the answering
/// model most needs the context.
///
/// Mutation-verified twice, and the second one is why this test exists rather
/// than being covered by the others. Moving the `handoff_note` binding to below
/// `record_routing` reddens the three claims in this file that a note *does*
/// ride, because then every dispatch reads its own record and no turn is ever
/// narrated. Moving it to the *top* of the loop, above
/// `record_routing`, fails **this test alone**: the first attempt still reads the
/// previous turn's record and rides a note, and only the second attempt sees the
/// contamination. That is the placement a plausible refactor reaches for, and
/// this is the only guard standing in front of it.
#[tokio::test]
async fn a_note_survives_a_failover_inside_the_escalating_turn() {
    let alpha = Recording::dead();
    let beta = Recording::answering();
    let rig = rig_of(vec![
        (ALPHA, Arc::clone(&alpha) as Arc<dyn FrontierClient>),
        (BETA, Arc::clone(&beta) as Arc<dyn FrontierClient>),
        (GAMMA, Recording::answering() as Arc<dyn FrontierClient>),
    ]);

    let (_, result) = rig
        .turn(a_critical_session(), &narrating(PickerMode::EfficientFirst))
        .await;
    let decision = result.decision.unwrap();
    assert_eq!(
        (decision.target, decision.source),
        (target(ALPHA), Some(DecisionSource::Override)),
        "the *decision* is still alpha's — a failover changes which target
         served, not which one was chosen"
    );

    assert!(
        alpha.only_prompt("alpha").contains(HANDOFF_MARKER),
        "the dead provider was handed the decorated request too, which is what \
         says the note was resolved before the loop rather than per attempt"
    );
    assert!(
        beta.only_prompt("beta").contains(HANDOFF_MARKER),
        "and the target that actually answered got it: the note must survive the \
         fall-forward, or a provider outage silently strips the one sentence the \
         escalation exists to send"
    );
}
