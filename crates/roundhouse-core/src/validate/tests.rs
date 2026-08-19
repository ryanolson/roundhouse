// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The occupant, end to end within core.
//!
//! Everything here drives a real [`Validator`] against a scripted judge. What
//! the engine does with the answer is stage-two's, so these assert on the two
//! things the occupant alone decides: what it *returns*, and what it asks the
//! engine to *record*.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::control::{Principal, TargetFilter, TurnPolicy};
use crate::event::{SessionEventKind, Usage};
use crate::ids::{ResponseId, SessionId};
use crate::item::{Item, ItemContent, Role};
use crate::session::SessionState;

fn call(call_id: &str, name: &str, arguments: &str) -> Item {
    Item::tool_call(call_id, name, arguments)
}

fn result(call_id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
        },
        response_id: None,
    }
}

/// A session whose gate is open and whose evidence fires the repeat signal.
fn stuck_in_arm(arm: Arm) -> SessionState {
    let mut state = SessionState::default();
    state.arm = Some(arm);
    state.turn_index = 9;
    state.tokens_since_last_validation = 200_000;
    state.last_event_at_ms = 5_000_000;
    state.items.push(Item::system_text("make the tests pass"));
    state
        .items
        .push(Item::user_text("the parser drops trailing commas"));
    for n in 0..4 {
        state
            .items
            .push(call(&format!("s{n}"), "pytest", r#"{"path":"tests/"}"#));
        state
            .items
            .push(result(&format!("s{n}"), "ImportError: no module named app"));
    }
    state
}

fn judge_target() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    }
}

/// A judge that answers from a script, and counts how often it was asked.
struct ScriptedJudge {
    answers: Mutex<Vec<Result<JudgeAnswer, JudgeFailure>>>,
    asked: AtomicUsize,
    /// Every `(system prompt, brief)` it was handed, so a test can assert on
    /// what the judge saw rather than only on what it said.
    seen: Mutex<Vec<(String, String)>>,
    /// The session id of every side call, which is the only input to the cache
    /// key the real implementation isolates on.
    keys: Mutex<Vec<String>>,
}

impl ScriptedJudge {
    fn new(answers: Vec<Result<JudgeAnswer, JudgeFailure>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().rev().collect()),
            asked: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            keys: Mutex::new(Vec::new()),
        })
    }

    fn answering(raw: &str) -> Arc<Self> {
        Self::new(vec![Ok(JudgeAnswer {
            raw: raw.to_string(),
            usage: Usage {
                input_tokens: 4_000,
                output_tokens: 40,
                ..Usage::default()
            },
            target: judge_target(),
        })])
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }

    /// The system prompt and brief of the `n`th consult.
    fn saw(&self, n: usize) -> (String, String) {
        self.seen.lock().unwrap()[n].clone()
    }
}

#[async_trait]
impl JudgeClient for ScriptedJudge {
    async fn consult(
        &self,
        side_call: &SideCall<'_>,
        system_prompt: &str,
        brief: &str,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        self.asked.fetch_add(1, Ordering::AcqRel);
        self.keys
            .lock()
            .unwrap()
            .push(side_call.session_id.to_string());
        self.seen
            .lock()
            .unwrap()
            .push((system_prompt.to_string(), brief.to_string()));
        self.answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(JudgeFailure::Abandoned {
                target: judge_target(),
                reason: SideCallAbandonReason::Unreachable,
            }))
    }
}

const OFF_TRACK: &str = r#"{"on_track":false,"confidence":0.8,
    "divergence":{"at_step":3,"description":"never opened the failing import"},
    "missing_context":null}"#;
const ON_TRACK: &str = r#"{"on_track":true,"confidence":0.9,"divergence":null,
    "missing_context":null}"#;

/// A validator and the membership terms its turns run under.
///
/// The two arrived apart in M6's wiring and the fixtures follow: what a *node*
/// sets (how often to ask, how long to wait) is [`ValidatorConfig`], and what a
/// *membership* permits (which arms, which channel, how strong an intervention)
/// is [`ValidationTerms`], resolved from the key. Bundling them here keeps a
/// test that only wants to move a channel from having to say both.
struct Enrolled {
    validator: Validator,
    terms: ValidationTerms,
}

/// One enrolled membership, under the deployment defaults.
fn enrolled(judge: Arc<ScriptedJudge>, terms: ValidationTerms) -> Enrolled {
    Enrolled {
        validator: Validator::new(judge, live_config()),
        terms,
    }
}

fn live_config() -> ValidatorConfig {
    ValidatorConfig {
        arm_salt: "salt".into(),
        ..ValidatorConfig::default()
    }
}

/// A membership that permits the strongest action its client can take.
fn live_terms() -> ValidationTerms {
    ValidationTerms {
        action: ActionPolicy {
            channel: SteerChannel::Auto,
            ..ActionPolicy::default()
        },
        ..ValidationTerms::default()
    }
}

/// The conversation every fixture here runs in.
fn session() -> SessionId {
    SessionId::new("acme/ada/main")
}

/// Drive one turn through a validator.
async fn consider(
    enrolled: &Enrolled,
    state: &SessionState,
    policy: &TurnPolicy,
    capability: &SteerCapability,
) -> Interjection {
    let response_id = ResponseId::new("resp_01J");
    let session_id = session();
    let principal = Principal::new("acme", "ada");
    enrolled
        .validator
        .consider(&InterjectionContext {
            state,
            response_id: &response_id,
            turn_policy: policy,
            objective: Objective::from_items(&state.items),
            capability,
            // No budget: what a ledger does about a check is the
            // implementation's business, and every assertion here is about
            // what the occupant does with the answer.
            side_call: SideCall {
                session_id: &session_id,
                principal: &principal,
                budget: None,
            },
            validation: Some(&enrolled.terms),
        })
        .await
}

fn record_of(interjection: &Interjection) -> &ControlRecord {
    match interjection {
        Interjection::Proceed { record } | Interjection::Complete { record, .. } => record,
    }
}

#[tokio::test]
async fn a_session_with_no_arm_is_not_enrolled_and_is_never_asked_about() {
    let judge = ScriptedJudge::answering(OFF_TRACK);
    let validator = enrolled(judge.clone(), live_terms());

    // Every gate open, evidence blazing — and no arm stamp, because the log
    // predates the experiment. Guessing an arm here would work and would
    // silently re-assign every historical session the day the salt moved.
    let mut unenrolled = stuck_in_arm(Arm::Live);
    unenrolled.arm = None;
    let decided = consider(
        &validator,
        &unenrolled,
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    assert_eq!(decided, Interjection::proceed());
    assert_eq!(judge.asked(), 0);

    // The control: the identical session with an arm is asked about.
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Live),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    assert_eq!(judge.asked(), 1);
    assert!(!record_of(&decided).is_empty());
}

#[tokio::test]
async fn the_shadow_arm_judges_and_releases_unchanged() {
    let judge = ScriptedJudge::answering(OFF_TRACK);
    let validator = enrolled(
        judge.clone(),
        ValidationTerms {
            action: ActionPolicy {
                channel: SteerChannel::Auto,
                steer_after_interventions: 1,
                ..ActionPolicy::default()
            },
            ..live_terms()
        },
    );
    // Arranged so the mapped action is a *steer*, not an escalation. That is
    // what makes this test discriminate: an escalation proceeds in every arm,
    // so a Shadow run that wrongly acted on one would be indistinguishable from
    // one that correctly did not. A steer completes the turn, so the arm's
    // discard is the only thing standing between this assertion and a
    // `Complete`.
    let mut state = stuck_in_arm(Arm::Shadow);
    state.consecutive_interventions = 1;
    let decided = consider(
        &validator,
        &state,
        &TurnPolicy::unrestricted(),
        &SteerCapability::Namespaced {
            namespace: "mcp__roundhouse".into(),
        },
    )
    .await;

    assert_eq!(
        judge.asked(),
        1,
        "the observe-only arm still pays for a judge"
    );
    // What the judge was handed, which is the other half of "the occupant owns
    // brief rendering": the injection defense arrives with every consult, and
    // roundhouse's measurement travels as a fact rather than being re-derived
    // by the judge from a transcript it can only partly see.
    let (system, brief) = judge.saw(0);
    assert!(system.contains(crate::validate::prompt::INJECTION_DEFENSE));
    assert!(brief.contains("produced identical output 4 times"));
    assert!(brief.contains("the parser drops trailing commas"));

    let Interjection::Proceed { record } = &decided else {
        panic!("shadow never completes a turn; got {decided:?}");
    };

    // Everything is logged: the money, the verdict, and the action that was
    // computed and thrown away. Without the last of those the arm comparison
    // has nothing to compare.
    let kinds = record.kinds();
    assert_eq!(kinds.len(), 2);
    assert!(matches!(
        kinds[0],
        SessionEventKind::SideCallCompleted { .. }
    ));
    let SessionEventKind::ValidationDecided { arm, outcome, .. } = &kinds[1] else {
        panic!("expected a decision; got {:?}", kinds[1]);
    };
    assert_eq!(*arm, Arm::Shadow);
    let ValidationOutcome::Judged { action, .. } = outcome else {
        panic!("the judge answered, so the outcome is judged: {outcome:?}");
    };
    assert!(
        matches!(action, SteerAction::Steer { .. }),
        "the action was computed in full -- discarding it is the arm's job, not \
         the map's. Got {action:?}"
    );
    assert_eq!(
        record.usage().total(),
        4_040,
        "and the check is not free just because nothing came of it"
    );
}

#[tokio::test]
async fn the_placebo_arm_intervenes_without_calling_the_judge() {
    // The rate is 1.0 so the hashed timing is pinned rather than sampled: what
    // this test is about is that no judge was consulted, and
    // `arm::tests` is where the timing's determinism is pinned.
    let judge = ScriptedJudge::answering(OFF_TRACK);
    let validator = enrolled(
        judge.clone(),
        ValidationTerms {
            placebo_rate: 1.0,
            ..live_terms()
        },
    );
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Placebo),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;

    assert_eq!(judge.asked(), 0, "the placebo consults nobody");
    let Interjection::Complete { record, usage, .. } = &decided else {
        panic!("a placebo at rate 1.0 intervenes; got {decided:?}");
    };
    assert_eq!(
        *usage,
        Usage::default(),
        "and it genuinely cost nothing, which is what makes it spend-matched \
         against a live arm only in the dashboard's arithmetic and not here"
    );
    let kinds = record.kinds();
    assert_eq!(kinds.len(), 1, "no side call happened, so none is booked");
    let SessionEventKind::ValidationDecided { arm, outcome, .. } = &kinds[0] else {
        panic!("expected a decision");
    };
    assert_eq!(*arm, Arm::Placebo);
    assert_eq!(
        *outcome,
        ValidationOutcome::NotRun {
            reason: NotRunReason::PlaceboArm { intervened: true },
        },
        "the sham is recorded as a sham: no verdict, and the timing said fire"
    );

    // The control: the same arm at rate 0.0 never intervenes, and still records
    // the decision — a control arm that logged nothing would be invisible.
    let quiet = enrolled(
        ScriptedJudge::answering(OFF_TRACK),
        ValidationTerms {
            placebo_rate: 0.0,
            ..live_terms()
        },
    );
    let decided = consider(
        &quiet,
        &stuck_in_arm(Arm::Placebo),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    let Interjection::Proceed { record } = &decided else {
        panic!("rate 0.0 never fires");
    };
    assert_eq!(
        record.kinds().len(),
        1,
        "a placebo that did not fire is still a validation that was decided"
    );
}

#[tokio::test]
async fn a_verdict_that_does_not_parse_releases_the_turn_and_still_books_the_money() {
    for unusable in [
        "APPROVE",
        "I cannot approve this - REDO: run the tests",
        r#"{"on_track":false,"confidence":0.6,"divergence":null,
            "missing_context":null,"suggested_action":"use a stronger model"}"#,
    ] {
        let judge = ScriptedJudge::answering(unusable);
        let validator = enrolled(judge.clone(), live_terms());
        let decided = consider(
            &validator,
            &stuck_in_arm(Arm::Live),
            &TurnPolicy::unrestricted(),
            &SteerCapability::Absent,
        )
        .await;

        let Interjection::Proceed { record } = &decided else {
            panic!("an unusable answer releases the turn; got {decided:?} for `{unusable}`");
        };
        let kinds = record.kinds();
        assert!(
            matches!(kinds[0], SessionEventKind::SideCallCompleted { .. }),
            "the money was spent whatever the answer said, and a parse failure \
             that also lost the cost would make a broken judge look free"
        );
        assert_eq!(record.usage().total(), 4_040);
        let SessionEventKind::ValidationDecided { outcome, .. } = &kinds[1] else {
            panic!("expected a decision");
        };
        assert_eq!(
            *outcome,
            ValidationOutcome::NotRun {
                reason: NotRunReason::VerdictUnparseable,
            },
            "and it is not filed as a transport failure: an operator reading \
             this has a prompt or a schema to fix, not a network"
        );
    }
}

#[tokio::test]
async fn a_judge_that_cannot_be_reached_releases_the_turn_and_is_marked_not_free() {
    // A call that was attempted and abandoned: there is a target, so there is a
    // row for the unaccounted call.
    let judge = ScriptedJudge::new(vec![Err(JudgeFailure::Abandoned {
        target: judge_target(),
        reason: SideCallAbandonReason::DeadlineExceeded,
    })]);
    let validator = enrolled(judge.clone(), live_terms());
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Live),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    let Interjection::Proceed { record } = &decided else {
        panic!("the checker must never break the checked; got {decided:?}");
    };
    let kinds = record.kinds();
    assert!(matches!(
        kinds[0],
        SessionEventKind::SideCallAbandoned {
            reason: SideCallAbandonReason::DeadlineExceeded,
            ..
        }
    ));
    assert_eq!(
        record.usage(),
        Usage::default(),
        "what a timed-out call billed upstream is exactly what we do not know, \
         so it is marked rather than guessed at"
    );
    assert!(matches!(
        &kinds[1],
        SessionEventKind::ValidationDecided {
            outcome: ValidationOutcome::NotRun {
                reason: NotRunReason::JudgeFailed
            },
            ..
        }
    ));

    // A call that was never attempted: no target, so no row. An abandoned call
    // against a target nobody dialled would be a phantom on the dashboard.
    let absent = enrolled(
        ScriptedJudge::new(vec![Err(JudgeFailure::Unavailable)]),
        live_terms(),
    );
    let decided = consider(
        &absent,
        &stuck_in_arm(Arm::Live),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    let record = record_of(&decided);
    assert_eq!(record.kinds().len(), 1);
    assert!(matches!(
        &record.kinds()[0],
        SessionEventKind::ValidationDecided {
            outcome: ValidationOutcome::NotRun {
                reason: NotRunReason::JudgeUnavailable
            },
            ..
        }
    ));
}

#[tokio::test]
async fn the_action_recorded_for_an_escalation_is_the_one_the_ceiling_permits() {
    let judge = ScriptedJudge::answering(OFF_TRACK);
    let validator = enrolled(
        judge,
        ValidationTerms {
            action: ActionPolicy {
                channel: SteerChannel::Auto,
                escalation_floor: 0.99,
                ..ActionPolicy::default()
            },
            ..live_terms()
        },
    );
    // A ceiling that admits nothing hosted. The escalation is an ask, and what
    // the log must record is what will actually be in force.
    let ceiling = TurnPolicy {
        min_quality: 0.5,
        allow: TargetFilter::parse(["local/*"]).expect("a well-formed pattern"),
        frontier_cadence: None,
    };
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Live),
        &ceiling,
        &SteerCapability::Absent,
    )
    .await;
    let Interjection::Proceed { record } = &decided else {
        panic!("an escalation dispatches; the client never sees it. Got {decided:?}");
    };
    let SessionEventKind::ValidationDecided {
        outcome: ValidationOutcome::Judged { action, .. },
        ..
    } = &record.kinds()[1]
    else {
        panic!("expected a judged decision");
    };
    let SteerAction::Escalate { overrides, .. } = action else {
        panic!("a real divergence escalates by default; got {action:?}");
    };
    assert_eq!(
        overrides.min_quality, 0.99,
        "the floor narrows, so the ask stands"
    );
    assert_eq!(
        action.applied_to(&ceiling).allow,
        ceiling.allow,
        "and it reaches nothing the ceiling excluded"
    );

    // The control on the other side: a ceiling whose floor is already higher
    // than the escalation's records the ceiling's, not the ask's — because that
    // is what the turn will be served under.
    let strict = TurnPolicy {
        min_quality: 0.995,
        ..ceiling
    };
    let decided = consider(
        &enrolled(
            ScriptedJudge::answering(OFF_TRACK),
            ValidationTerms {
                action: ActionPolicy {
                    channel: SteerChannel::Auto,
                    escalation_floor: 0.99,
                    ..ActionPolicy::default()
                },
                ..live_terms()
            },
        ),
        &stuck_in_arm(Arm::Live),
        &strict,
        &SteerCapability::Absent,
    )
    .await;
    let SessionEventKind::ValidationDecided {
        outcome:
            ValidationOutcome::Judged {
                action: SteerAction::Escalate { overrides, .. },
                ..
            },
        ..
    } = &record_of(&decided).kinds()[1]
    else {
        panic!("expected an escalation");
    };
    assert_eq!(overrides.min_quality, 0.995);
}

#[tokio::test]
async fn an_on_track_verdict_costs_money_and_changes_nothing() {
    let judge = ScriptedJudge::answering(ON_TRACK);
    let validator = enrolled(judge.clone(), live_terms());
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Live),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Namespaced {
            namespace: "mcp__roundhouse".into(),
        },
    )
    .await;
    let Interjection::Proceed { record } = &decided else {
        panic!("a healthy trajectory is dispatched unchanged");
    };
    assert_eq!(record.usage().total(), 4_040, "asking is never free");
    let SessionEventKind::ValidationDecided {
        outcome: ValidationOutcome::Judged { action, .. },
        ..
    } = &record.kinds()[1]
    else {
        panic!("expected a judged decision");
    };
    assert_eq!(*action, SteerAction::Continue);
}

/// The node-local half of the review budget, on its own.
#[test]
fn a_reservation_is_taken_before_the_await_and_released_on_every_path_out() {
    let budget = ReviewBudget::new(ReviewLimits {
        max_in_flight: 2,
        max_consecutive_failures: 2,
    });
    let first = budget.reserve().expect("capacity");
    let second = budget.reserve().expect("capacity");
    assert_eq!(budget.in_flight(), 2);
    assert!(
        budget.reserve().is_none(),
        "concurrent consults cannot overdraw, which is the whole reason the \
         reservation happens before the await"
    );

    first.succeeded();
    assert_eq!(budget.in_flight(), 1, "the guard releases on drop");
    second.failed();
    assert_eq!(budget.in_flight(), 0, "including on the failure path");
    assert_eq!(
        budget.consecutive_failures(),
        1,
        "a failed consult refunds its reservation and counts against the \
         separate cap -- one counter would show a node with all its capacity \
         free cheerfully timing out every turn"
    );

    // The circuit breaker: consecutive failures stop the asking, and one
    // success clears the streak.
    budget.reserve().expect("still under the cap").failed();
    assert!(budget.reserve().is_none(), "the node has stopped asking");
    budget.consecutive_failures.store(1, Ordering::Release);
    budget.reserve().expect("under the cap again").succeeded();
    assert_eq!(budget.consecutive_failures(), 0);
    assert!(budget.reserve().is_some());
}

#[tokio::test]
async fn a_spent_review_budget_releases_the_turn_and_records_why() {
    let judge = ScriptedJudge::answering(OFF_TRACK);
    // A node with no review capacity, which is the one knob in this file that
    // is genuinely a deployment setting rather than a membership one.
    let validator = Enrolled {
        validator: Validator::new(
            judge.clone(),
            ValidatorConfig {
                review: ReviewLimits {
                    max_in_flight: 0,
                    max_consecutive_failures: 1,
                },
                ..live_config()
            },
        ),
        terms: live_terms(),
    };
    let decided = consider(
        &validator,
        &stuck_in_arm(Arm::Live),
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    assert_eq!(judge.asked(), 0, "no capacity, no consult");
    let record = record_of(&decided);
    assert!(matches!(
        &record.kinds()[0],
        SessionEventKind::ValidationDecided {
            outcome: ValidationOutcome::NotRun {
                reason: NotRunReason::ReviewBudgetSpent
            },
            ..
        }
    ));
    assert!(matches!(decided, Interjection::Proceed { .. }));
}

#[tokio::test]
async fn a_steer_names_one_id_the_client_can_fetch_and_resend() {
    // Past the first intervention, so escalation is spent and the protocol-heavy
    // path is what is left — which is the order the plan puts them in.
    let mut state = stuck_in_arm(Arm::Live);
    state.consecutive_interventions = 1;
    let validator = enrolled(
        ScriptedJudge::answering(OFF_TRACK),
        ValidationTerms {
            action: ActionPolicy {
                channel: SteerChannel::Auto,
                steer_after_interventions: 1,
                ..ActionPolicy::default()
            },
            ..live_terms()
        },
    );
    let decided = consider(
        &validator,
        &state,
        &TurnPolicy::unrestricted(),
        &SteerCapability::Namespaced {
            namespace: "mcp__roundhouse".into(),
        },
    )
    .await;
    let Interjection::Complete {
        item,
        guidance,
        usage,
        ..
    } = &decided
    else {
        panic!("expected a steered turn; got {decided:?}");
    };
    let ItemContent::ToolCall {
        call_id,
        name,
        arguments,
    } = &item.content
    else {
        panic!("a steer is a tool call");
    };
    assert_eq!(name, STEER_TOOL, "the bare name; a namespace is a dialect");
    assert_eq!(call_id, "rhsteer_resp_01J");
    assert_eq!(
        arguments, r#"{"steer_id":"rhsteer_resp_01J"}"#,
        "one id written once: the call an agent fetches by and the call its \
         client resends are the same string"
    );
    assert_eq!(
        item.response_id, None,
        "the item is built without provenance; only the commit stamps it"
    );
    assert!(
        guidance.contains("never opened the failing import"),
        "the judge's finding is quoted as an observation"
    );
    assert!(
        guidance.contains("identical output 4 times"),
        "and roundhouse's own measurement travels as a fact"
    );
    assert_eq!(usage.total(), 4_040);
}

#[tokio::test]
async fn a_halt_completes_with_text_and_leaves_nothing_to_fetch() {
    let mut state = stuck_in_arm(Arm::Live);
    state.consecutive_interventions = 1;
    let validator = enrolled(
        ScriptedJudge::answering(OFF_TRACK),
        ValidationTerms {
            action: ActionPolicy {
                channel: SteerChannel::Auto,
                steer_after_interventions: 1,
                ..ActionPolicy::default()
            },
            ..live_terms()
        },
    );
    // No capability under `Auto`: the degrade path, which hands control back to
    // the human rather than emitting a call the client cannot dispatch.
    let decided = consider(
        &validator,
        &state,
        &TurnPolicy::unrestricted(),
        &SteerCapability::Absent,
    )
    .await;
    let Interjection::Complete { item, .. } = &decided else {
        panic!("expected a halted turn; got {decided:?}");
    };
    assert!(
        matches!(item.content, ItemContent::Text { .. }),
        "plain text ends the client's loop; there is no call, so nothing is \
         deposited for an agent to fetch"
    );
    assert_eq!(item.role, Role::Assistant);
}
