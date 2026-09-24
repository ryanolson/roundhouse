// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Frontier review interval coverage through the real engine.
//!
//! Every test runs the real [`Engine`] with the real [`Validator`] at its
//! interjection seam, then reads the durable log back through
//! [`SessionState::project`]. The judge is scripted except in the prompt-byte
//! and reservation tests, which use the real [`FleetJudge`] over a capturing
//! transport and a recording spend ledger.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use roundhouse_core::context::{ByteTokenizer, Tokenizer};
use roundhouse_core::control::{
    Allocation, Balance, BalanceQuery, Budget, BudgetTerms, BudgetWindow, DEFAULT_WARN_AT,
    Exhaustion, Grant, GrantRequest, MemorySpendLedger, Settled, Settlement, SpendError,
    SpendLedger,
};
use roundhouse_core::event::{
    Accounting, CacheReadSource, SessionEvent, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::interject::Interjector;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, PickerMode, ProviderPricing, StagePolicy, TierRecipe,
};
use roundhouse_core::session::{MAX_REVIEW_TURNS, SessionState};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{
    ActionPolicy, Arm, ArmShares, CoverageGap, DEFAULT_INTERVAL_SECTION_BYTES,
    INTERVAL_SECTION_HEADING, IntervalLabel, IntervalReview, JudgeClient, JudgeFailure, Objective,
    ObjectiveVersion, SteerAction, SteerChannel, ValidationTerms, Validator, ValidatorConfig,
};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierClients, FrontierError,
    FrontierModelSpec, FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_mcp::{ControlStore, IntentRecord};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, FleetJudge, JudgeConfig, LocalExecutor,
};

mod common;
use common::frontier_catalog;
use common::validate::{AlwaysFires, OFF_TRACK, ON_TRACK, ScriptedJudge, judge_spec, open_trigger};

const ANSWER: &str = "echoed answer";

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    control: Arc<ControlStore>,
}

fn validator_config(section_bytes: usize) -> ValidatorConfig {
    ValidatorConfig {
        trigger: roundhouse_core::validate::TriggerConfig {
            max_validations_per_session: 1_000,
            max_consecutive_interventions: 1_000,
            ..open_trigger()
        },
        interval_section_bytes: section_bytes,
        arm_salt: "review-engine".into(),
        ..ValidatorConfig::default()
    }
}

fn validator(judge: Arc<dyn JudgeClient>, section_bytes: usize) -> Arc<dyn Interjector> {
    Arc::new(
        Validator::new(judge, validator_config(section_bytes))
            .with_signals(vec![Box::new(AlwaysFires)]),
    )
}

/// An engine over the echo provider with `judge` at the seam.
fn rig(judge: Arc<dyn JudgeClient>, section_bytes: usize, spend: Arc<dyn SpendLedger>) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let control = Arc::new(ControlStore::new());
    let engine = Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            arm_salt: "review-engine".into(),
            ..EngineConfig::default()
        },
    )
    .with_spend_ledger(spend)
    .with_control_store(Arc::clone(&control))
    .with_interjector(validator(judge, section_bytes));
    Rig {
        engine: Arc::new(engine),
        store,
        control,
    }
}

fn scripted_rig(judge: Arc<ScriptedJudge>, section_bytes: usize) -> Rig {
    rig(
        judge as Arc<dyn JudgeClient>,
        section_bytes,
        Arc::new(MemorySpendLedger::new()),
    )
}

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
            handoff_note: None,
        }),
        ..Admission::open()
    }
}

fn shadow() -> Admission {
    enrolled(Arm::Shadow, ActionPolicy::default())
}

fn steering() -> ActionPolicy {
    ActionPolicy {
        channel: SteerChannel::Auto,
        steer_after_interventions: 1,
        ..ActionPolicy::default()
    }
}

fn developer(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

fn answers(raws: &[Result<&str, JudgeFailure>]) -> Arc<ScriptedJudge> {
    ScriptedJudge::new(
        raws.iter()
            .map(|raw| {
                raw.clone()
                    .map(|raw| roundhouse_core::validate::JudgeAnswer {
                        raw: raw.to_string(),
                        usage: common::validate::judge_usage(),
                        target: common::validate::judge_target(),
                    })
            })
            .collect(),
    )
}

impl Rig {
    async fn turn(&self, id: &SessionId, turn: &str, input: Vec<Item>, admission: &Admission) {
        self.engine.create_session(id).await.expect("a session");
        self.engine
            .run_turn(id, TurnId::new(turn), input, admission)
            .await
            .expect("the turn answers");
    }

    async fn events(&self, id: &SessionId) -> Vec<SessionEvent> {
        self.store
            .read_events(id, 0, 100_000)
            .await
            .expect("the log reads")
    }

    async fn replay(&self, id: &SessionId) -> SessionState {
        SessionState::project(self.store.as_ref(), id, CacheLedger::new(), None)
            .await
            .expect("the log replays")
    }
}

/// Every interval review in the log, with the sequence of the event carrying it.
fn reviews(events: &[SessionEvent]) -> Vec<(u64, IntervalReview)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ValidationDecided {
                outcome:
                    ValidationOutcome::Judged {
                        interval: Some(review),
                        ..
                    },
                ..
            } => Some((event.seq, (**review).clone())),
            _ => None,
        })
        .collect()
}

fn judged_actions(events: &[SessionEvent]) -> Vec<SteerAction> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ValidationDecided {
                outcome: ValidationOutcome::Judged { action, .. },
                ..
            } => Some(action.clone()),
            _ => None,
        })
        .collect()
}

fn routed(events: &[SessionEvent]) -> Vec<(u64, ResponseId)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::Routed { response_id, .. } => Some((event.seq, response_id.clone())),
            _ => None,
        })
        .collect()
}

fn covered(review: &IntervalReview) -> Vec<u64> {
    review
        .decisions
        .iter()
        .map(|decision| decision.routed_seq)
        .collect()
}

fn section(brief: &str) -> Option<&str> {
    brief.find(INTERVAL_SECTION_HEADING).map(|at| &brief[at..])
}

/// Failed, skipped and unparseable reviews leave the interval open; the first
/// parsed review covers every text turn since the session began and nothing
/// after its capture.
#[tokio::test]
async fn the_engine_covers_every_text_turn_once_and_leaves_the_next_for_later() {
    let judge = answers(&[
        Err(JudgeFailure::Unaffordable),
        Err(JudgeFailure::Abandoned {
            target: common::validate::judge_target(),
            reason: roundhouse_core::event::SideCallAbandonReason::Refused,
        }),
        Ok("this is not a verdict"),
        Ok(ON_TRACK),
    ]);
    let rig = scripted_rig(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
    let id = SessionId::new("acme/ada/text");
    for n in 0..5 {
        rig.turn(
            &id,
            &format!("t{n}"),
            vec![Item::user_text(format!("question {n}"))],
            &shadow(),
        )
        .await;
        if (1..4).contains(&n) {
            assert!(
                reviews(&rig.events(&id).await).is_empty(),
                "turn {n} is not a checkpoint"
            );
            assert_eq!(rig.replay(&id).await.review_checkpoint(), 0);
        }
    }
    let events = rig.events(&id).await;
    let all = reviews(&events);
    assert_eq!(all.len(), 1, "one parsed review");
    let (event_seq, review) = &all[0];
    let decisions = routed(&events);
    assert_eq!(
        covered(review),
        decisions[..4]
            .iter()
            .map(|(seq, _)| *seq)
            .collect::<Vec<_>>()
    );
    assert!(
        review.through_seq < decisions[4].0,
        "turn 4's own decision follows the capture"
    );
    assert!(review.through_seq < *event_seq);
    assert_eq!(review.label, IntervalLabel::Positive);
    let brief = judge.briefs().last().cloned().expect("briefs");
    let shown = section(&brief).unwrap_or_else(|| panic!("no section:\n{brief}"));
    for n in 0..5 {
        assert!(
            shown.contains(&format!("question {n}")),
            "turn {n}:\n{shown}"
        );
    }
    assert_eq!(
        shown.matches(ANSWER).count(),
        4,
        "four answered turns:\n{shown}"
    );

    let replayed = rig.replay(&id).await;
    assert_eq!(replayed.review_checkpoint(), review.through_seq);
    assert_eq!(
        replayed.pending_review_decisions().collect::<Vec<_>>(),
        vec![decisions[4].0]
    );
    for event in &events {
        if let SessionEventKind::Routed { decision, .. } = &event.kind {
            assert_eq!(
                decision
                    .selection
                    .as_ref()
                    .and_then(|s| s.objective.clone()),
                Some(ObjectiveVersion::Undeclared),
                "every decision records the objective it ran under"
            );
        }
    }
}

/// The reviewed turn's input precedes the checkpoint; its decision follows.
/// The next review shows that input and covers that decision.
#[tokio::test]
async fn the_open_turn_input_crosses_the_checkpoint_through_the_engine() {
    let judge = answers(&[Ok(ON_TRACK), Ok(ON_TRACK)]);
    let rig = scripted_rig(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
    let id = SessionId::new("acme/ada/open");
    rig.turn(&id, "t0", vec![Item::user_text("first")], &shadow())
        .await;
    rig.turn(
        &id,
        "t1",
        vec![Item::user_text("OPEN-TURN-REQUEST")],
        &shadow(),
    )
    .await;
    rig.turn(&id, "t2", vec![Item::user_text("third")], &shadow())
        .await;
    let events = rig.events(&id).await;
    let decisions = routed(&events);
    let all = reviews(&events);
    assert_eq!(all.len(), 2);
    assert_eq!(covered(&all[0].1), vec![decisions[0].0]);
    assert_eq!(
        covered(&all[1].1),
        vec![decisions[1].0],
        "exactly once, in the next interval"
    );
    assert_eq!(all[1].1.after_seq, all[0].1.through_seq);
    let second = section(&judge.briefs()[1])
        .map(str::to_string)
        .unwrap_or_default();
    assert!(second.contains("OPEN-TURN-REQUEST"), "{second}");
    assert!(!second.contains("first"), "{second}");
}

/// A scripted provider that fails on the calls its plan names.
struct Flaky {
    fail_on: Vec<usize>,
    calls: AtomicUsize,
}

impl Flaky {
    fn new(fail_on: Vec<usize>) -> Arc<Self> {
        Arc::new(Self {
            fail_on,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FrontierClient for Flaky {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_on.contains(&call) {
            return Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            });
        }
        Ok(FrontierChunk::whole_response(
            format!("served on call {call}"),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            4,
            0,
        ))
    }
}

/// Every dispatch is its own decision: a failover writes two, a failed turn
/// keeps its own, and its retry is another.
#[tokio::test]
async fn failover_dispatches_and_a_failed_then_retried_turn_are_all_covered() {
    let free = |provider: &str| FrontierModelSpec {
        pricing: ProviderPricing::free(),
        ..frontier_spec(provider, "m", WireProtocol::OpenAiResponses)
    };
    let catalog = StaticFrontierCatalog::new(vec![
        FrontierModelSpec {
            quality_prior: 0.95,
            ..free("alpha")
        },
        FrontierModelSpec {
            quality_prior: 0.90,
            ..free("beta")
        },
        FrontierModelSpec {
            quality_prior: 0.60,
            ..free("gamma")
        },
    ]);
    let dead: Vec<usize> = (0..100).collect();
    // Beta's second call fails, so the second turn exhausts its tier.
    let registry = FrontierClients::keyed(
        [
            (
                "alpha".to_string(),
                Flaky::new(dead) as Arc<dyn FrontierClient>,
            ),
            (
                "beta".to_string(),
                Flaky::new(vec![1]) as Arc<dyn FrontierClient>,
            ),
            (
                "gamma".to_string(),
                Flaky::new(vec![]) as Arc<dyn FrontierClient>,
            ),
        ]
        .into_iter()
        .collect(),
    );
    let store = Arc::new(MemoryStore::new());
    let judge = answers(&[
        Err(JudgeFailure::Unaffordable),
        Err(JudgeFailure::Unaffordable),
        Ok(OFF_TRACK),
    ]);
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog,
        Arc::new(registry),
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
        EngineConfig {
            turn_deadline_ms: 5_000,
            arm_salt: "review-engine".into(),
            ..EngineConfig::default()
        },
    )
    .with_interjector(validator(
        Arc::clone(&judge) as Arc<dyn JudgeClient>,
        DEFAULT_INTERVAL_SECTION_BYTES,
    ));
    let recipe = TierRecipe::new(
        vec!["alpha/m".into(), "beta/m".into()],
        vec!["gamma/m".into()],
        PickerMode::CapableFirst,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a recipe");
    let admission = Admission {
        tiers: Some(Arc::new(recipe)),
        ..shadow()
    };
    let id = SessionId::new("acme/ada/failover");
    engine.create_session(&id).await.unwrap();
    engine
        .run_turn(
            &id,
            TurnId::new("t0"),
            vec![Item::user_text("q0")],
            &admission,
        )
        .await
        .expect("beta serves after alpha fails");
    engine
        .run_turn(
            &id,
            TurnId::new("t1"),
            vec![Item::user_text("q1")],
            &admission,
        )
        .await
        .expect_err("both capable targets fail");
    engine
        .run_turn(
            &id,
            TurnId::new("t1"),
            vec![Item::user_text("q1 retried")],
            &admission,
        )
        .await
        .expect("the retry is served");
    engine
        .run_turn(
            &id,
            TurnId::new("t2"),
            vec![Item::user_text("q2")],
            &admission,
        )
        .await
        .expect("served");

    let events = store.read_events(&id, 0, 10_000).await.unwrap();
    let decisions = routed(&events);
    assert_eq!(decisions.len(), 8, "two dispatches per turn: {decisions:?}");
    let all = reviews(&events);
    assert_eq!(all.len(), 1);
    let review = &all[0].1;
    assert_eq!(
        covered(review),
        decisions[..6]
            .iter()
            .map(|(seq, _)| *seq)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        review.decisions[0].response_id,
        review.decisions[1].response_id
    );
    assert_ne!(
        review.decisions[3].response_id, review.decisions[4].response_id,
        "the retry is a new response"
    );
    assert_eq!(
        review
            .decisions
            .iter()
            .map(|d| d.turn_index)
            .collect::<Vec<_>>(),
        vec![0, 0, 1, 1, 2, 2]
    );
    assert!(review.gaps.is_empty(), "{:?}", review.gaps);
    assert_eq!(review.label, IntervalLabel::Negative);
    let replayed = SessionState::project(store.as_ref(), &id, CacheLedger::new(), None)
        .await
        .unwrap();
    assert_eq!(replayed.review_outcomes()[0].decisions, covered(review));
}

fn declare(control: &ControlStore, id: &SessionId, goal: &str) {
    control.set_intent(
        id,
        IntentRecord {
            goal: goal.into(),
            plan_steps: vec![format!("{goal} step")],
            done_when: format!("{goal} done"),
            declared_at_ms: roundhouse_core::now_ms(),
        },
    );
}

fn declared(goal: &str) -> Objective {
    Objective::Declared {
        goal: goal.into(),
        plan_steps: vec![format!("{goal} step")],
        done_when: format!("{goal} done"),
    }
}

/// Configuration and objective changes between covered turns, A-B-A included,
/// are unknown. The stable control renders both in full and is labelled.
#[tokio::test]
async fn configuration_and_objective_changes_between_covered_turns_are_unknown() {
    let config_a = format!("CONFIG-A {}", "a".repeat(2_000));
    let config_b = "CONFIG-B".to_string();
    // (why, configuration per turn, declared goal per turn, expected gap)
    type Case<'a> = (&'a str, [&'a str; 3], [&'a str; 3], Option<CoverageGap>);
    let cases: Vec<Case> = vec![
        (
            "stable",
            [&config_a, &config_a, &config_a],
            ["GOAL-A", "GOAL-A", "GOAL-A"],
            None,
        ),
        (
            "configuration A-B-A",
            [&config_a, &config_b, &config_a],
            ["GOAL-A", "GOAL-A", "GOAL-A"],
            Some(CoverageGap::InstructionsChanged),
        ),
        (
            "objective A-B-A",
            [&config_a, &config_a, &config_a],
            ["GOAL-A", "GOAL-B", "GOAL-A"],
            Some(CoverageGap::ObjectiveChanged),
        ),
    ];
    for (why, configs, goals, gap) in cases {
        let judge = answers(&[
            Err(JudgeFailure::Unaffordable),
            Err(JudgeFailure::Unaffordable),
            Ok(ON_TRACK),
        ]);
        let rig = scripted_rig(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
        let id = SessionId::new(format!("acme/ada/{}", why.replace(' ', "-")));
        for n in 0..3 {
            declare(&rig.control, &id, goals[n]);
            rig.turn(
                &id,
                &format!("t{n}"),
                vec![developer(configs[n]), Item::user_text(format!("q{n}"))],
                &shadow(),
            )
            .await;
        }
        declare(&rig.control, &id, "GOAL-A");
        rig.turn(
            &id,
            "t3",
            vec![developer(&config_a), Item::user_text("q3")],
            &shadow(),
        )
        .await;

        let events = rig.events(&id).await;
        let stamps: Vec<Option<ObjectiveVersion>> = events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::Routed { decision, .. } => Some(
                    decision
                        .selection
                        .as_ref()
                        .and_then(|s| s.objective.clone()),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            stamps[..3],
            goals.map(|goal| Some(ObjectiveVersion::of(&declared(goal)))),
            "{why}: the engine stamps the declared objective on each decision"
        );
        let review = reviews(&events).pop().expect("one review").1;
        match gap {
            None => {
                assert!(review.gaps.is_empty(), "{why}: {:?}", review.gaps);
                assert_eq!(review.label, IntervalLabel::Positive, "{why}");
                let shown = section(&judge.briefs()[2])
                    .map(str::to_string)
                    .unwrap_or_default();
                assert!(shown.contains(&config_a), "{why}: the whole configuration");
                assert!(shown.contains("GOAL-A done"), "{why}: the whole objective");
            }
            Some(gap) => {
                assert!(review.gaps.contains(&gap), "{why}: {:?}", review.gaps);
                assert_eq!(review.label, IntervalLabel::Unknown, "{why}");
                assert!(section(&judge.briefs()[2]).is_none(), "{why}");
            }
        }
    }
}

/// An interval over the bound is unknown whole. After the checkpoint, a small
/// interval is labelled and covers only later decisions.
#[tokio::test]
async fn an_oversized_interval_is_unknown_and_the_next_interval_recovers() {
    let judge = answers(&[
        Err(JudgeFailure::Unaffordable),
        Err(JudgeFailure::Unaffordable),
        Ok(ON_TRACK),
        Ok(ON_TRACK),
    ]);
    let rig = scripted_rig(Arc::clone(&judge), 1_200);
    let id = SessionId::new("acme/ada/oversized");
    for n in 0..3 {
        rig.turn(
            &id,
            &format!("t{n}"),
            vec![Item::user_text(format!("LONG-{n} {}", "x".repeat(600)))],
            &shadow(),
        )
        .await;
    }
    rig.turn(&id, "t3", vec![Item::user_text("short")], &shadow())
        .await;
    rig.turn(&id, "t4", vec![Item::user_text("short again")], &shadow())
        .await;
    let events = rig.events(&id).await;
    let decisions = routed(&events);
    let all = reviews(&events);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].1.gaps, vec![CoverageGap::Oversized]);
    assert_eq!(all[0].1.label, IntervalLabel::Unknown);
    assert!(section(&judge.briefs()[2]).is_none(), "omitted whole");
    assert_eq!(all[1].1.after_seq, all[0].1.through_seq);
    assert_eq!(covered(&all[1].1), vec![decisions[3].0]);
    assert!(all[1].1.gaps.is_empty(), "{:?}", all[1].1.gaps);
    assert_eq!(all[1].1.label, IntervalLabel::Positive);
    let labels: Vec<IntervalLabel> = rig
        .replay(&id)
        .await
        .review_outcomes()
        .iter()
        .map(|o| o.label)
        .collect();
    assert_eq!(
        labels,
        vec![IntervalLabel::Unknown, IntervalLabel::Positive]
    );
}

/// With no review for longer than the tracking bound, the next review is
/// unknown; the checkpoint clears the overflow.
#[tokio::test]
async fn a_metadata_overflow_is_unknown_until_a_checkpoint_clears_it() {
    let turns = MAX_REVIEW_TURNS + 2;
    let mut script: Vec<Result<&str, JudgeFailure>> =
        vec![Err(JudgeFailure::Unaffordable); turns - 1];
    script.extend([Ok(ON_TRACK), Ok(ON_TRACK)]);
    let judge = answers(&script);
    let rig = scripted_rig(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
    let id = SessionId::new("acme/ada/overflow");
    for n in 0..=turns + 1 {
        rig.turn(
            &id,
            &format!("t{n}"),
            vec![Item::user_text(format!("q{n}"))],
            &shadow(),
        )
        .await;
    }
    let events = rig.events(&id).await;
    let decisions = routed(&events);
    let all = reviews(&events);
    assert_eq!(all.len(), 2);
    assert!(
        all[0].1.gaps.contains(&CoverageGap::MetadataOverflow),
        "{:?}",
        all[0].1.gaps
    );
    assert_eq!(all[0].1.label, IntervalLabel::Unknown);
    assert_eq!(covered(&all[1].1), vec![decisions[turns].0]);
    assert_eq!(all[1].1.label, IntervalLabel::Positive);
}

/// The label follows the verdict whatever the action map and the arm did.
#[tokio::test]
async fn the_label_is_the_verdicts_whatever_action_was_delivered() {
    for (arm, action, verdict, label, delivered) in [
        (
            Arm::Live,
            ActionPolicy::default(),
            OFF_TRACK,
            IntervalLabel::Negative,
            "continue",
        ),
        (
            Arm::Shadow,
            steering(),
            OFF_TRACK,
            IntervalLabel::Negative,
            "escalate",
        ),
        (
            Arm::Live,
            steering(),
            ON_TRACK,
            IntervalLabel::Positive,
            "continue",
        ),
    ] {
        let judge = answers(&[Ok(verdict)]);
        let rig = scripted_rig(judge, DEFAULT_INTERVAL_SECTION_BYTES);
        let id = SessionId::new(format!("acme/ada/{arm:?}-{delivered}"));
        let admission = enrolled(arm, action);
        rig.turn(&id, "t0", vec![Item::user_text("q0")], &admission)
            .await;
        rig.turn(&id, "t1", vec![Item::user_text("q1")], &admission)
            .await;
        let events = rig.events(&id).await;
        let action = judged_actions(&events).pop().expect("judged");
        let wire = serde_json::to_value(&action).unwrap();
        assert_eq!(wire["action"], delivered, "{arm:?}");
        let review = reviews(&events).pop().expect("a review").1;
        assert_eq!(review.label, label, "{arm:?} {verdict} with {action:?}");
    }
}

/// Content the section cannot render makes the interval unknown.
#[tokio::test]
async fn an_unrenderable_turn_is_unknown_through_the_engine() {
    let judge = answers(&[Ok(ON_TRACK)]);
    let rig = scripted_rig(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
    let id = SessionId::new("acme/ada/opaque");
    let image = Item {
        role: Role::User,
        content: ItemContent::Opaque {
            block_type: "image".into(),
            block: serde_json::json!({"type": "image", "source": {"type": "base64", "data": "AAAA"}}),
        },
        response_id: None,
    };
    rig.turn(
        &id,
        "t0",
        vec![image, Item::user_text("what is this")],
        &shadow(),
    )
    .await;
    rig.turn(&id, "t1", vec![Item::user_text("q1")], &shadow())
        .await;
    let review = reviews(&rig.events(&id).await).pop().expect("a review").1;
    assert_eq!(review.gaps, vec![CoverageGap::UnrepresentableContent]);
    assert_eq!(review.label, IntervalLabel::Unknown);
    assert!(section(&judge.briefs()[0]).is_none());
}

// Path-declared, like `classification_settlement_recovery.rs`'s own split
// submodules: a `tests/*.rs` file is its own crate root, so a plain `mod x;`
// here would look for `tests/x.rs` (and, unqualified, Cargo would auto-discover
// that as a second top-level test binary) rather than the file actually beside
// this one's own directory.
#[path = "review_interval_engine/fleet_reservation.rs"]
mod fleet_reservation;
