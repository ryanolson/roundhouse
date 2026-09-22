// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What classifier evaluation calls cost, as the serialized snapshot reports it.
//!
//! Asserted against `serde_json::to_value` of the document the API serves rather
//! than against the fold's own accumulators, because the accumulator is not the
//! contract: a figure folded correctly and published under a name nothing reads,
//! or published beside a serving figure it silently joined, is the same defect
//! from a dashboard's point of view. Every claim here is a claim about the
//! bytes.
//!
//! Two rules the fixtures below exist to hold apart, and both are money rules:
//!
//! - **An observed cost is not an invoice.** `evaluation.measured_usd` is the
//!   usage a service reported, priced by the rate card the *call itself*
//!   recorded. It is not a provider-reported dollar figure and it is not
//!   repriced from the live catalog, so moving the catalog must not move it.
//! - **A cost observation is not a settlement.** A call whose settle nobody
//!   acknowledged still billed; a repair that resolves the acknowledgement adds
//!   no second call and no second dollar.

use serde_json::Value;

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord,
    ClassificationSettlementRepair, ClassifierIdentity, ContextDependence, EvaluationSpend,
    EvaluationUsage, FundingRefusal, Graded, ReservationRecord, SettlementAck, TurnClassification,
    TurnComplexity, TurnIntent,
};
use roundhouse_core::control::{Billing, BudgetWindow, Principal, PrincipalKey, ProjectId};
use roundhouse_core::event::{Accounting, SessionEvent, SessionEventKind, SideCallPurpose, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, TurnId};
use roundhouse_core::metrics::{
    MetricsConfig, MetricsRecorder, MetricsSnapshot, ReferenceModel, ShadowPricing,
};
use roundhouse_core::routing::{DecisionRecord, ProviderPricing, Target};

// ---------------------------------------------------------------------------
// Rate cards
// ---------------------------------------------------------------------------

/// The serving rate card the snapshot prices *token counts* with.
///
/// Reporting configuration, so it moves when an operator corrects it — which is
/// the whole reason `historical_evaluation_amounts_do_not_reprice` exists.
const SERVING: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

/// The rate card a classifier call recorded on its own reservation.
///
/// Deliberately unlike [`SERVING`] and deliberately absent from
/// [`MetricsConfig`]: an evaluation amount comes out of the log, so no
/// configuration this test loads can reach it.
const CLASSIFIER: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 0.8,
    cached_input_per_mtok_usd: 0.08,
    cache_write_per_mtok_usd: 1.0,
    output_per_mtok_usd: 4.0,
};

fn config() -> MetricsConfig {
    MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: SERVING,
        quality_prior: 0.6,
    }]))
}

/// The same reporting configuration with every serving price doubled.
fn dearer_config() -> MetricsConfig {
    MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: ProviderPricing {
            input_per_mtok_usd: SERVING.input_per_mtok_usd * 2.0,
            cached_input_per_mtok_usd: SERVING.cached_input_per_mtok_usd * 2.0,
            cache_write_per_mtok_usd: SERVING.cache_write_per_mtok_usd * 2.0,
            output_per_mtok_usd: SERVING.output_per_mtok_usd * 2.0,
        },
        quality_prior: 0.6,
    }]))
}

// ---------------------------------------------------------------------------
// One session's log, built by hand
// ---------------------------------------------------------------------------

/// A log builder that appends at increasing sequence numbers, like the store.
///
/// Built here rather than driven through the engine because several of the
/// shapes under test are ones the engine will not produce on demand: a result
/// delivered twice, a result naming an intent it does not answer, an intent
/// whose worker died. The fold has to be right about all of them, because the
/// durable log is what a replay reads.
struct Log {
    session: SessionId,
    events: Vec<SessionEvent>,
    at_ms: u64,
}

impl Log {
    fn new(session: &str, principal: Option<Principal>) -> Self {
        let mut log = Self {
            session: SessionId::new(session),
            events: Vec::new(),
            at_ms: 1_000,
        };
        log.push(SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            principal,
            arm: None,
        });
        log
    }

    fn push(&mut self, kind: SessionEventKind) -> &mut Self {
        self.at_ms += 10;
        self.events.push(SessionEvent {
            seq: self.events.len() as u64 + 1,
            session_id: self.session.clone(),
            at_ms: self.at_ms,
            kind,
        });
        self
    }

    /// One frontier turn, served and billed. The serving control.
    ///
    /// Always `r1`, which is the response every classification fixture below
    /// names as the turn it is about: an intent that named anything else would
    /// be testing the attribution join rather than the accounting.
    fn turn(&mut self, usage: Usage) -> &mut Self {
        self.turn_on("anthropic", "claude", usage)
    }

    /// The same turn on a named hosted model, for the row the catalog has no
    /// rate for.
    fn turn_on(&mut self, provider: &str, model: &str, usage: Usage) -> &mut Self {
        let response_id = ResponseId::new("r1");
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new("turn-r1"),
            response_id: response_id.clone(),
        });
        self.push(SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                selection: None,
                local_quote_skipped: None,
                chosen: Target::Frontier {
                    provider: provider.into(),
                    model: model.into(),
                },
                rationale: "test".into(),
                policy: "test".into(),
                isl_tokens: usage.input_tokens,
                expected_prefill_tokens: 0.0,
                expected_cost_usd: 0.0,
                considered: Vec::new(),
                turn_policy_digest: String::new(),
                budget_state: Default::default(),
                rate_card: None,
                payer: Default::default(),
                billing: Default::default(),
                budget_draw: None,
                withheld_providers: Vec::new(),
                declared_baseline: None,
                attempts: Vec::new(),
            },
        });
        self.push(SessionEventKind::ResponseCompleted {
            response_id,
            usage,
            provider_reported_cost_usd: None,
            stop_reason: None,
        })
    }

    /// One hosted turn on a project that forwards its caller's own seat.
    ///
    /// Served and counted like any other, and billed to somebody else's
    /// subscription rather than to this deployment.
    fn turn_on_seat(&mut self, usage: Usage) -> &mut Self {
        self.turn(usage);
        // Rewrite the decision this just wrote rather than copying the whole
        // builder: the seat is the only difference, and a second copy of the
        // record is a second thing to keep in step with the first.
        for event in self.events.iter_mut().rev() {
            if let SessionEventKind::Routed { decision, .. } = &mut event.kind {
                decision.billing = Billing::AccountedNotBilled;
                break;
            }
        }
        self
    }

    /// One turn served by our own fleet. Bills nobody and costs GPU time.
    fn turn_local(&mut self, model: &str, usage: Usage) -> &mut Self {
        let response_id = ResponseId::new("r1");
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new("turn-r1"),
            response_id: response_id.clone(),
        });
        self.push(SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                selection: None,
                local_quote_skipped: None,
                chosen: Target::Local {
                    worker_id: 7,
                    dp_rank: 0,
                    model: model.into(),
                },
                rationale: "test".into(),
                policy: "test".into(),
                isl_tokens: usage.input_tokens,
                expected_prefill_tokens: 0.0,
                expected_cost_usd: 0.0,
                considered: Vec::new(),
                turn_policy_digest: String::new(),
                budget_state: Default::default(),
                rate_card: None,
                payer: Default::default(),
                billing: Default::default(),
                budget_draw: None,
                withheld_providers: Vec::new(),
                declared_baseline: None,
                attempts: Vec::new(),
            },
        });
        self.push(SessionEventKind::ResponseCompleted {
            response_id,
            usage,
            provider_reported_cost_usd: None,
            stop_reason: None,
        })
    }

    /// A judge side call: this deployment's own money, already counted by the
    /// serving fold under the model that billed it.
    fn judge(&mut self, usage: Usage) -> &mut Self {
        self.push(SessionEventKind::SideCallCompleted {
            side_call_id: SideCallId::new("sc-judge"),
            purpose: SideCallPurpose::Validate,
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            usage,
        })
    }

    fn intent(&mut self, call: &str, turn_index: u64, source: &str, model: &str) -> &mut Self {
        self.push(SessionEventKind::ClassificationRequested {
            record: intent(call, turn_index, source, model),
        })
    }

    fn result(&mut self, record: ClassificationRecord) -> &mut Self {
        self.push(SessionEventKind::ClassificationRecorded { record })
    }

    fn repair(&mut self, call: &str, applied: bool) -> &mut Self {
        self.push(SessionEventKind::ClassificationSettlementRepaired {
            record: ClassificationSettlementRepair {
                call_id: ResponseId::new(call),
                applied,
                repaired_at_ms: 9_000,
            },
        })
    }

    fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}

fn usage(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        cache_write_tokens: 0,
        output_tokens: output,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
        cache_read_source: Default::default(),
    }
}

fn intent(call: &str, turn_index: u64, source: &str, model: &str) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(call),
        source_turn_index: turn_index,
        source_response_id: ResponseId::new(source),
        requested_at_ms: 1_000,
        expires_at_ms: 61_000,
        identity: ClassifierIdentity {
            model: model.into(),
            schema: "json_schema".into(),
            taxonomy_version: 1,
            projection_revision: 1,
            config_revision: 7,
        },
        reservation: ReservationRecord {
            rate_card: CLASSIFIER,
            estimated_input_tokens: 900,
            expected_output_tokens: 64,
            requested_usd: 2.0,
            hold_ttl_ms: 60_000,
            budget_limit_usd: 100.0,
            budget_window: BudgetWindow::Monthly,
            member_ceiling_usd: None,
            warn_at: 0.8,
        },
    }
}

fn classification() -> TurnClassification {
    TurnClassification {
        taxonomy_version: 1,
        intent: Graded {
            value: TurnIntent::Implement,
            confidence: 0.9,
        },
        complexity: Graded {
            value: TurnComplexity::Routine,
            confidence: 0.8,
        },
        context_dependence: Graded {
            value: ContextDependence::Recent,
            confidence: 0.7,
        },
    }
}

/// What a classifier call billed, as the service reported it.
fn billed(input: u64, output: u64) -> EvaluationUsage {
    EvaluationUsage {
        input_tokens: input,
        output_tokens: output,
    }
}

/// A result carrying measured usage priced by the call's own rate card.
fn measured(
    call: &str,
    turn_index: u64,
    source: &str,
    usage: EvaluationUsage,
    usd: f64,
    settled: SettlementAck,
    reported_model: Option<&str>,
) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call),
        source_turn_index: turn_index,
        source_response_id: ResponseId::new(source),
        completed_at_ms: 5_000,
        outcome: ClassificationOutcome::Classified {
            classification: classification(),
            spend: EvaluationSpend::Measured {
                usage,
                usd,
                granted_usd: 2.0,
                settled,
            },
            reported_model: reported_model.map(str::to_string),
        },
    }
}

/// An envelope arrived, its answers were unusable, and nobody reported usage.
fn unknown_usage(call: &str, turn_index: u64, source: &str) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call),
        source_turn_index: turn_index,
        source_response_id: ResponseId::new(source),
        completed_at_ms: 5_000,
        outcome: ClassificationOutcome::Unusable {
            reason: "schema_violation".into(),
            spend: EvaluationSpend::Unknown {
                granted_usd: 2.0,
                settled: SettlementAck::Committed,
            },
            reported_model: None,
        },
    }
}

/// Nothing was sent, so nothing was billed: a refusal established before HTTP.
fn refused(call: &str, turn_index: u64, source: &str) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call),
        source_turn_index: turn_index,
        source_response_id: ResponseId::new(source),
        completed_at_ms: 5_000,
        outcome: ClassificationOutcome::Unfunded {
            reason: FundingRefusal::BudgetRefused {
                requested_usd: 2.0,
                granted_usd: 0.4,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

fn ada() -> Principal {
    Principal::new("acme", "ada")
}

fn bob() -> Principal {
    Principal::new("acme", "bob")
}

fn zoe() -> Principal {
    Principal::new("globex", "zoe")
}

/// Everything `acme/ada` did: one billed turn, one judge call, and eight
/// classifier calls covering every state the vocabulary can be in.
///
/// The classifier results are appended in the *reverse* of the order their
/// intents were written, which is what completion order looks like in a durable
/// log: two calls finish out of order and the writer appends each as it lands,
/// at increasing sequence numbers.
fn ada_log() -> Log {
    let mut log = Log::new("acme/ada/main", Some(ada()));
    log.turn(usage(10_000, 8_000, 500));
    log.judge(usage(2_000, 0, 200));

    for (call, model) in [
        ("c-1", "haiku"),
        ("c-2", "haiku"),
        ("c-3", "haiku"),
        ("c-4", "haiku"),
        ("c-5", "haiku"),
        ("c-6", "haiku"),
        ("c-7", "haiku"),
        ("c-8", "big-classifier"),
    ] {
        log.intent(call, 1, "r1", model);
    }

    // Reverse completion: c-2 lands before c-1, both after both intents.
    log.result(measured(
        "c-2",
        1,
        "r1",
        billed(500, 100),
        0.125,
        SettlementAck::Unconfirmed,
        Some("claude-haiku-4.5"),
    ));
    log.result(measured(
        "c-1",
        1,
        "r1",
        billed(1_000, 200),
        0.25,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));
    // The same result delivered twice, at a later sequence. One answer, one
    // cost: the second delivery is refused by the call's durable identity.
    log.result(measured(
        "c-1",
        1,
        "r1",
        billed(1_000, 200),
        0.25,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));
    // c-3 has no result at all: its worker died. Pending, not free.
    log.result(refused("c-4", 1, "r1"));
    log.result(unknown_usage("c-5", 1, "r1"));
    // A result that names an intent it does not answer — the source response is
    // not the one the intent was taken about. Booked nowhere, and it must not
    // consume the intent the valid answer below still needs.
    log.result(measured(
        "c-6",
        1,
        "r-other",
        billed(2_000, 400),
        0.5,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));
    log.result(measured(
        "c-6",
        1,
        "r1",
        billed(2_000, 400),
        0.5,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));
    // A service that reported an empty model identity says no more than one
    // that reported none.
    log.result(measured(
        "c-7",
        1,
        "r1",
        billed(300, 60),
        0.0625,
        SettlementAck::Committed,
        Some("   "),
    ));
    log.result(measured(
        "c-8",
        1,
        "r1",
        billed(100, 20),
        0.031_25,
        SettlementAck::Unconfirmed,
        Some("claude-sonnet-5"),
    ));
    // A result nobody asked for: no intent of this session names it.
    log.result(measured(
        "c-orphan",
        1,
        "r1",
        billed(9_999, 9_999),
        99.0,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));

    // c-2's settle is acknowledged at last. The second acknowledgement resolves
    // nothing, and neither does one for a call that never produced a result.
    log.repair("c-2", true);
    log.repair("c-2", false);
    log.repair("c-3", true);
    log
}

/// `acme/bob` reuses `acme/ada`'s call id, in his own session.
///
/// A call id is fresh per external attempt, so one id in two sessions is two
/// calls and two costs — never one duplicate. The project view must count both.
fn bob_log() -> Log {
    let mut log = Log::new("acme/bob/main", Some(bob()));
    log.intent("c-1", 1, "rb", "haiku");
    log.result(measured(
        "c-1",
        1,
        "rb",
        billed(4_000, 800),
        1.5,
        SettlementAck::Committed,
        Some("claude-sonnet-5"),
    ));
    log
}

fn zoe_log() -> Log {
    let mut log = Log::new("globex/zoe/main", Some(zoe()));
    log.intent("c-z", 1, "rz", "haiku");
    log.result(measured(
        "c-z",
        1,
        "rz",
        billed(8_000, 1_600),
        4.0,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));
    log
}

fn recorder() -> MetricsRecorder {
    let recorder = MetricsRecorder::new();
    recorder.record(ada_log().events());
    recorder.record(bob_log().events());
    recorder.record(zoe_log().events());
    recorder
}

fn json(snapshot: &MetricsSnapshot) -> Value {
    serde_json::to_value(snapshot).expect("the snapshot serializes")
}

fn deployment() -> Value {
    json(&recorder().snapshot(&config(), 9_999))
}

fn principal(who: &Principal) -> Value {
    json(&recorder().snapshot_for(&PrincipalKey::from(who), &config(), 9_999))
}

fn project(id: &str) -> Value {
    json(&recorder().snapshot_for_project(&ProjectId::new(id), &config(), 9_999))
}

/// A `u64` field, named so a failure says which one was missing.
fn count(value: &Value, path: &[&str]) -> u64 {
    at(value, path)
        .as_u64()
        .unwrap_or_else(|| panic!("{} is not a count: {}", path.join("."), at(value, path)))
}

fn dollars(value: &Value, path: &[&str]) -> f64 {
    at(value, path)
        .as_f64()
        .unwrap_or_else(|| panic!("{} is not a number: {}", path.join("."), at(value, path)))
}

fn at<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    let mut cursor = value;
    for step in path {
        cursor = cursor
            .get(step)
            .unwrap_or_else(|| panic!("the snapshot has no `{}`", path.join(".")));
    }
    cursor
}

fn close(left: f64, right: f64, what: &str) {
    assert!(
        (left - right).abs() < 1e-9,
        "{what}: {left} differs from {right}"
    );
}

// ---------------------------------------------------------------------------
// Coverage: every state a classifier call can be in, told apart
// ---------------------------------------------------------------------------

/// The four states the requirement names, each countable and none inferred.
///
/// Measured usage, missing usage, a pending intent and a refusal established
/// before any HTTP are four different facts about four different calls, and the
/// document has to be able to say which is which. The two exact identities
/// below are what make that structural rather than asserted: every accepted
/// result is exactly one of the three outcome classes, and every intent has
/// either landed or has not.
#[test]
fn evaluation_coverage_separates_measured_unknown_pending_and_refused() {
    let ada = principal(&ada());
    let evaluation = ["evaluation"];

    assert_eq!(count(&ada, &["evaluation", "intents"]), 8);
    assert_eq!(count(&ada, &["evaluation", "results"]), 7);
    assert_eq!(count(&ada, &["evaluation", "pending"]), 1);
    assert_eq!(count(&ada, &["evaluation", "measured_calls"]), 5);
    assert_eq!(count(&ada, &["evaluation", "unknown_usage_calls"]), 1);
    assert_eq!(count(&ada, &["evaluation", "refused_calls"]), 1);

    let e = at(&ada, &evaluation);
    assert_eq!(
        count(e, &["results"]),
        count(e, &["measured_calls"])
            + count(e, &["unknown_usage_calls"])
            + count(e, &["refused_calls"]),
        "every accepted result is measured, unknown or refused, and never two of them"
    );
    assert_eq!(
        count(e, &["intents"]),
        count(e, &["results"]) + count(e, &["pending"]),
        "an intent has either landed or has not"
    );

    // A refusal costs nothing because nothing was sent; a pending intent and an
    // unknown usage cost something nobody here can name. Only the second kind
    // makes the total incomplete.
    assert_eq!(
        at(&ada, &["evaluation", "cost_incomplete"]),
        &Value::Bool(true)
    );
}

/// One served turn and no classifier call at all: every gap at zero.
///
/// The `false` half of every completeness flag, so an assertion that one of
/// them is `true` elsewhere is not an assertion about a constant.
fn deployment_without_classification() -> Value {
    let recorder = MetricsRecorder::new();
    let mut log = Log::new("acme/ada/main", Some(ada()));
    log.turn(usage(10_000, 8_000, 500));
    recorder.record(log.events());
    json(&recorder.snapshot(&config(), 9_999))
}

/// A deployment that has classified nothing says so without claiming a zero
/// cost it cannot know is zero.
#[test]
fn a_deployment_with_no_classifier_calls_reports_a_complete_zero() {
    let document = deployment_without_classification();

    assert_eq!(count(&document, &["evaluation", "intents"]), 0);
    assert_eq!(count(&document, &["evaluation", "results"]), 0);
    close(
        dollars(&document, &["evaluation", "measured_usd"]),
        0.0,
        "nothing classified, nothing spent",
    );
    assert_eq!(
        at(&document, &["evaluation", "cost_incomplete"]),
        &Value::Bool(false),
        "no call is not an unknown call"
    );
    assert_eq!(
        at(&document, &["observed_cost", "incomplete"]),
        &Value::Bool(false)
    );
}

// ---------------------------------------------------------------------------
// The money, and what it is not
// ---------------------------------------------------------------------------

/// The observed cost is the sum of the amounts the calls themselves recorded.
///
/// Published under a price basis that says where it came from, because the
/// number beside it on the same page came from somewhere else.
#[test]
fn evaluation_cost_is_the_recorded_amount_under_a_stated_basis() {
    let ada = principal(&ada());
    close(
        dollars(&ada, &["evaluation", "measured_usd"]),
        0.25 + 0.125 + 0.5 + 0.0625 + 0.031_25,
        "the five measured calls",
    );
    assert_eq!(
        at(&ada, &["evaluation", "price_basis"]),
        "rate_card_recorded_with_each_call"
    );
    assert_eq!(count(&ada, &["evaluation", "tokens", "input"]), 3_900);
    assert_eq!(count(&ada, &["evaluation", "tokens", "output"]), 780);
    assert_eq!(count(&ada, &["evaluation", "tokens", "total"]), 4_680);
}

/// Moving the catalog moves the serving figure and leaves evaluation alone.
///
/// The one property that makes an evaluation amount an *observation*: it was
/// priced once, by the rate card the call recorded, and no later edit to
/// reporting configuration can reach it. A figure that moved here would mean the
/// ledger and the dashboard could come to disagree about a finished call with
/// nothing able to say which was right.
#[test]
fn historical_evaluation_amounts_do_not_reprice() {
    let recorder = recorder();
    let cheap = json(&recorder.snapshot(&config(), 9_999));
    let dear = json(&recorder.snapshot(&dearer_config(), 9_999));

    close(
        dollars(&cheap, &["evaluation", "measured_usd"]),
        dollars(&dear, &["evaluation", "measured_usd"]),
        "an evaluation amount is not repriced by the reporting catalog",
    );
    assert!(
        dollars(&dear, &["savings", "frontier_spend_usd"])
            > dollars(&cheap, &["savings", "frontier_spend_usd"]),
        "the control: a serving figure *is* repriced, or this test proves nothing"
    );
    // And the combined view moves by exactly the serving half.
    close(
        dollars(&dear, &["observed_cost", "total_usd"])
            - dollars(&cheap, &["observed_cost", "total_usd"]),
        dollars(&dear, &["savings", "frontier_spend_usd"])
            - dollars(&cheap, &["savings", "frontier_spend_usd"]),
        "only the serving half of the combined view is catalog-priced",
    );
}

/// Classifier tokens and dollars stay off every serving figure.
///
/// The control is the same log without its classification events: if any
/// classifier token reached a serving counter, or any classifier dollar reached
/// the savings figures, these would differ. Compared as whole serialized
/// subtrees so a field added later is covered without anyone remembering.
#[test]
fn classifier_spend_never_enters_the_serving_counters() {
    let with_classification = MetricsRecorder::new();
    with_classification.record(ada_log().events());

    let without = MetricsRecorder::new();
    let serving_only: Vec<SessionEvent> = ada_log()
        .events()
        .iter()
        .filter(|event| {
            !matches!(
                event.kind,
                SessionEventKind::ClassificationRequested { .. }
                    | SessionEventKind::ClassificationRecorded { .. }
                    | SessionEventKind::ClassificationSettlementRepaired { .. }
            )
        })
        .cloned()
        .collect();
    without.record(&serving_only);

    let folded = json(&with_classification.snapshot(&config(), 9_999));
    let control = json(&without.snapshot(&config(), 9_999));

    for field in [
        "tokens",
        "coverage",
        "calls",
        "models",
        "providers",
        "serving_modes",
    ] {
        assert_eq!(
            folded.get(field),
            control.get(field),
            "`{field}` must not move when a classifier call is folded"
        );
    }
    assert_eq!(
        folded.get("savings"),
        control.get("savings"),
        "the savings figures are a serving claim; `total_usd` stays cache plus routing"
    );
    // Not vacuous: the classification events really are in the first fold.
    assert_eq!(count(&folded, &["evaluation", "results"]), 7);
    assert_eq!(count(&control, &["evaluation", "results"]), 0);
}

/// The judge's own call is serving money, counted once, and never a classifier
/// call.
///
/// A side call is this deployment's money on the model that billed it, and it
/// already sits in the serving counters. The combined view adds evaluation to
/// serving, so a judge call that also landed in the evaluation half would be
/// charged to the deployment twice.
#[test]
fn judge_side_calls_are_counted_once_and_never_as_classifier_calls() {
    let ada = principal(&ada());
    // One served turn plus one judge call, both on the hosted row.
    assert_eq!(count(&ada, &["calls"]), 2);
    assert_eq!(
        count(&ada, &["evaluation", "results"]),
        7,
        "the judge is not one of them"
    );

    let rows = at(&ada, &["evaluation", "models"])
        .as_array()
        .expect("the evaluation rows are an array");
    for row in rows {
        assert_ne!(
            row["requested_model"], "claude",
            "the judge's model must not appear as a classifier identity"
        );
    }
}

/// The combined view names both price bases and adds them once.
#[test]
fn the_combined_view_labels_its_two_price_bases() {
    let ada = principal(&ada());
    assert_eq!(
        at(&ada, &["observed_cost", "serving_basis"]),
        "current_catalog_rate_card"
    );
    assert_eq!(
        at(&ada, &["observed_cost", "evaluation_basis"]),
        "rate_card_recorded_with_each_call"
    );
    close(
        dollars(&ada, &["observed_cost", "serving_usd"]),
        dollars(&ada, &["savings", "frontier_spend_usd"]),
        "the serving half is the figure already published",
    );
    close(
        dollars(&ada, &["observed_cost", "evaluation_usd"]),
        dollars(&ada, &["evaluation", "measured_usd"]),
        "the evaluation half is the figure already published",
    );
    close(
        dollars(&ada, &["observed_cost", "total_usd"]),
        dollars(&ada, &["observed_cost", "serving_usd"])
            + dollars(&ada, &["observed_cost", "evaluation_usd"]),
        "the total is the two halves and nothing else",
    );
    // `Savings::total_usd` is savings and stays savings.
    close(
        dollars(&ada, &["savings", "total_usd"]),
        dollars(&ada, &["savings", "cache_savings_usd"])
            + dollars(&ada, &["savings", "routing_savings_usd"]),
        "the savings total is cache plus routing, never a spend",
    );
    // An incomplete evaluation half makes the whole view incomplete. The
    // converse does not hold — see the serving-gap test below — so the outer
    // flag is an implication rather than an equality.
    assert_eq!(
        at(&ada, &["evaluation", "cost_incomplete"]),
        &Value::Bool(true)
    );
    assert_eq!(
        at(&ada, &["observed_cost", "incomplete"]),
        &Value::Bool(true)
    );
    // The *inner* flag is the same fact under two names, and two names for one
    // fact drift unless something holds them together. Asserted in both
    // directions of the boolean, or the equality would hold on `true` alone.
    let quiet = deployment_without_classification();
    for document in [&ada, &quiet] {
        assert_eq!(
            at(document, &["observed_cost", "evaluation_incomplete"]),
            at(document, &["evaluation", "cost_incomplete"]),
            "the combined view republishes the evaluation half's own answer"
        );
    }
    assert_eq!(
        at(&quiet, &["observed_cost", "evaluation_incomplete"]),
        &Value::Bool(false)
    );
}

/// A complete classifier half does not make a combined total complete.
///
/// The serving half has its own two ways of being unknown, and both are here
/// rather than in the coverage figures alone: a provider that reported no usage
/// leaves the dollars priced off our own tokenizer, and a hosted model with no
/// catalog rate bills real money this document publishes as zero. A combined
/// figure that called itself complete on the strength of the classifier's
/// bookkeeping would be at its most confident exactly where the serving rate
/// card is missing.
#[test]
fn a_complete_evaluation_half_does_not_make_the_serving_half_complete() {
    let mut log = Log::new("acme/ada/main", Some(ada()));
    // A hosted model the reporting catalog holds no rate for: real tokens,
    // published as zero dollars.
    log.turn_on("unpriced", "mystery", usage(1_000, 0, 100));
    log.intent("c-1", 1, "r1", "haiku");
    log.result(measured(
        "c-1",
        1,
        "r1",
        billed(100, 20),
        0.5,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));

    let recorder = MetricsRecorder::new();
    recorder.record(log.events());
    let document = json(&recorder.snapshot(&config(), 9_999));

    assert_eq!(
        at(&document, &["evaluation", "cost_incomplete"]),
        &Value::Bool(false),
        "every classifier call here is measured and settled"
    );
    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "unpriced_models"]
        ),
        1,
        "one hosted row served tokens and published no price"
    );
    assert_eq!(
        at(&document, &["observed_cost", "incomplete"]),
        &Value::Bool(true),
        "a combined total cannot be complete while a serving row is unpriced"
    );
}

/// A provider that reported no usage is a measurement gap, disclosed as one.
#[test]
fn an_unreported_serving_call_makes_the_combined_view_incomplete() {
    let mut log = Log::new("acme/ada/main", Some(ada()));
    let mut estimated = usage(1_000, 0, 100);
    estimated.accounting = Accounting::Estimated;
    log.turn(estimated);

    let recorder = MetricsRecorder::new();
    recorder.record(log.events());
    let document = json(&recorder.snapshot(&config(), 9_999));

    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "estimated_calls"]
        ),
        1
    );
    // One number and not two: the gap republishes the coverage figure this
    // document already carries rather than walking the rows a second time.
    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "estimated_calls"]
        ),
        count(&document, &["coverage", "estimated_calls"]),
    );
    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "unpriced_models"]
        ),
        0,
        "this row has a rate card; only its token counts are ours"
    );
    assert_eq!(
        at(&document, &["observed_cost", "incomplete"]),
        &Value::Bool(true)
    );
    // The serving dollars are still published: an estimated call is priced,
    // just not measured. Incomplete here means uncertain, not missing.
    assert!(dollars(&document, &["observed_cost", "serving_usd"]) > 0.0);
}

/// Serving locally costs GPU time, and the combined view says it cannot price
/// it rather than reporting the fleet as free.
///
/// The failure this refuses is the loudest one available: a deployment that
/// routes everything to its own workers bills nobody, so every dollar figure on
/// this page is legitimately near zero — and a combined total calling itself
/// complete would be publishing "we spent almost nothing" about a fleet whose
/// GPUs ran the whole time. Nothing here invents a price for that hardware. The
/// document states what it does not cover, which is the only honest thing a
/// catalog of per-token rates can say about a capital asset.
#[test]
fn local_serving_is_disclosed_because_its_hardware_cost_is_not_priced() {
    let mut log = Log::new("acme/ada/main", Some(ada()));
    log.turn_local("llama", usage(1_000, 0, 100));

    let recorder = MetricsRecorder::new();
    recorder.record(log.events());
    let document = json(&recorder.snapshot(&config(), 9_999));

    assert_eq!(
        at(&document, &["evaluation", "cost_incomplete"]),
        &Value::Bool(false),
        "nothing was classified, so the evaluation half knows everything it can"
    );
    assert_eq!(
        count(&document, &["observed_cost", "serving_gaps", "local_calls"]),
        1
    );
    assert_eq!(
        at(&document, &["observed_cost", "incomplete"]),
        &Value::Bool(true),
        "a total that excludes the hardware a turn ran on is not a complete cost"
    );
    // What it covers is named on the wire, not left to be inferred from a
    // field that happens to be zero.
    assert_eq!(
        at(&document, &["observed_cost", "covers"]),
        "hosted_serving_and_classifier_calls"
    );

    // And no price was invented for the fleet: the serving half is still
    // structurally zero for a local row, and the counterfactual saving is
    // untouched.
    close(
        dollars(&document, &["observed_cost", "serving_usd"]),
        0.0,
        "a local row bills nobody, and this stage did not change that",
    );
    close(
        dollars(&document, &["observed_cost", "total_usd"]),
        0.0,
        "no hardware cost was guessed at",
    );
    // Not vacuous: a local row really did serve this turn, so the zeroes above
    // are the deliberate absence of a bill rather than an empty fold.
    let local = at(&document, &["models"])
        .as_array()
        .expect("the rows are an array")
        .iter()
        .find(|row| row["mode"] == "local")
        .expect("the turn above routed to our own fleet");
    assert_eq!(local["calls"], 1);
    assert!(
        local["tokens"]["total"].as_u64().expect("a count") > 0,
        "the fleet served real tokens for the hardware cost nobody here prices"
    );
}

/// A forwarded seat leaves the combined total *complete*, and that is the
/// ruling rather than an oversight.
///
/// The contrast with local serving is the whole of it, and the two look alike
/// enough that pinning it matters. Both are traffic this deployment served and
/// priced nowhere. But GPU time is **this deployment's cost**, paid in hardware
/// instead of in invoices, so a total that omits it is short — while a seat's
/// tokens were charged to the caller's own subscription, so they are **not this
/// deployment's cost at all** and a total that omits them is exact. `covers`
/// carries that exclusion because it is a statement about scope; `serving_gaps`
/// does not, because there is no gap.
///
/// A control rather than a regression: this is what the code already does. It
/// is here so the next reader who notices the asymmetry finds the reason
/// attached to an assertion instead of deciding it was a miss.
#[test]
fn a_forwarded_seat_is_not_a_gap_because_it_is_not_this_deployments_cost() {
    let mut log = Log::new("acme/ada/main", Some(ada()));
    log.turn_on_seat(usage(1_000, 0, 100));

    let recorder = MetricsRecorder::new();
    recorder.record(log.events());
    let document = json(&recorder.snapshot(&config(), 9_999));

    // Real traffic, really counted.
    assert!(count(&document, &["seat_tokens", "total"]) > 0);
    assert_eq!(count(&document, &["calls"]), 1);

    // Priced nowhere, and complete anyway.
    close(
        dollars(&document, &["observed_cost", "total_usd"]),
        0.0,
        "roundhouse holds no rate card for a subscription and invents none",
    );
    for gap in ["estimated_calls", "unpriced_models", "local_calls"] {
        assert_eq!(
            count(&document, &["observed_cost", "serving_gaps", gap]),
            0,
            "a seat is not a serving gap: `{gap}`"
        );
    }
    assert_eq!(
        at(&document, &["observed_cost", "incomplete"]),
        &Value::Bool(false),
        "nothing this deployment paid for is missing from the total"
    );
    // The exclusion is still stated, on the scope string rather than as a gap.
    assert_eq!(
        at(&document, &["observed_cost", "covers"]),
        "hosted_serving_and_classifier_calls"
    );
}

/// A rate card priced at an explicit `$0` is a rate card, and is not counted as
/// a missing one.
///
/// The gap counter used to read "no price" off `billed_usd() == 0.0`, which is
/// also what a hosted row bills when its catalog entry prices every axis at
/// zero. The two are different facts — one row has no rate card to write, the
/// other already has one — and only the first is a gap anybody can close.
///
/// One document, two otherwise-identical rows, so the comparison is inside a
/// single snapshot rather than between two runs: `free-tier` has a full
/// `ReferenceModel` entry whose every rate is `0.0`, `mystery` has none at all.
/// Their dollars agree and their rate cards do not, which is why the wire has
/// to carry the second fact rather than leave it to be inferred from the first.
#[test]
fn an_explicit_zero_rate_card_is_not_counted_as_a_missing_one() {
    let config = MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "acme".into(),
        model: "free-tier".into(),
        pricing: ProviderPricing::free(),
        quality_prior: 0.6,
    }]));

    // A hosted row the catalog prices at an explicit `$0`: real tokens, a real
    // rate card, and a bill of zero dollars because the rate card says zero.
    let mut priced_zero = Log::new("acme/ada/main", Some(ada()));
    priced_zero.turn_on("acme", "free-tier", usage(1_000, 0, 100));
    priced_zero.intent("c-1", 1, "r1", "haiku");
    priced_zero.result(measured(
        "c-1",
        1,
        "r1",
        billed(100, 20),
        0.5,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));

    // The absent-price control: same shape of traffic, no catalog entry.
    let mut unpriced = Log::new("acme/bob/main", Some(bob()));
    unpriced.turn_on("provider-x", "mystery", usage(1_000, 0, 100));

    let recorder = MetricsRecorder::new();
    recorder.record(priced_zero.events());
    recorder.record(unpriced.events());
    let document = json(&recorder.snapshot(&config, 9_999));

    assert_eq!(
        at(&document, &["evaluation", "cost_incomplete"]),
        &Value::Bool(false),
        "the one classifier call here is measured and settled"
    );
    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "unpriced_models"]
        ),
        1,
        "only `mystery` has no rate card to write; `free-tier` already has one, \
         priced at zero on purpose"
    );

    // The same distinction on the wire, because the dashboard has to make it
    // too and a row's dollars cannot tell it: both rows below publish `$0`, and
    // an operator sent after a rate card for `free-tier` would be sent after a
    // file that already says zero.
    let row = |provider: &str, model: &str| {
        at(&document, &["models"])
            .as_array()
            .expect("the document publishes its model rows as an array")
            .iter()
            .find(|row| row["provider"] == provider && row["model"] == model)
            .unwrap_or_else(|| panic!("no `{provider}/{model}` row in the document"))
            .clone()
    };
    for (provider, model, priced) in [
        ("acme", "free-tier", true),
        ("provider-x", "mystery", false),
    ] {
        let row = row(provider, model);
        close(
            row["billed_usd"].as_f64().expect("a hosted row is billed"),
            0.0,
            "both rows bill nothing, which is what makes the dollars useless here",
        );
        assert!(
            row["tokens"]["total"].as_u64().unwrap_or(0) > 0,
            "`{provider}/{model}` served real tokens: {row}"
        );
        assert_eq!(
            row["priced_by_catalog"],
            Value::Bool(priced),
            "`{provider}/{model}` must say whether a rate card covered it, since \
             its money cannot: {row}"
        );
    }
}

/// A fully priced model stays priced when a call's own token mix happens to
/// bill nothing.
///
/// The same conflation as above on a rate card that is not trivially all-zero:
/// a model priced normally on every axis used to read as unpriced whenever one
/// call was entirely cache reads at a cache rate the catalog set to `$0`. The
/// catalog is unchanged by what the traffic did, so the gap counter must be
/// too.
///
/// Whether the existing rate-card contract permits a per-axis zero beside
/// non-zero siblings: yes — `ProviderPricing`'s four rates are independent
/// fields with no cross-field invariant, so this is a supported configuration,
/// not an invented one.
#[test]
fn a_wholly_cached_call_at_a_configured_zero_cache_rate_is_still_priced() {
    let config = MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.6,
    }]));

    let mut log = Log::new("acme/ada/main", Some(ada()));
    // Wholly cached input, no output: every billable token on this call prices
    // at the one rate this catalog set to zero.
    log.turn(usage(1_000, 1_000, 0));

    let recorder = MetricsRecorder::new();
    recorder.record(log.events());
    let document = json(&recorder.snapshot(&config, 9_999));

    close(
        dollars(&document, &["observed_cost", "serving_usd"]),
        0.0,
        "every token on this row billed at the configured $0 cache rate",
    );
    assert_eq!(
        count(
            &document,
            &["observed_cost", "serving_gaps", "unpriced_models"]
        ),
        0,
        "this model has a full rate card; the zero is this call's own token \
         mix, not a missing price"
    );
}

/// The combined flag is `serving gaps or evaluation gaps`, on every combination
/// of the two.
///
/// One fixture agreeing with one expectation is a control, not an invariant: a
/// flag hard-wired to the classifier's answer passes the classifier-incomplete
/// case and the both-incomplete case, and only the serving-only row catches it.
/// So all four corners are here, and each asserts the *relation* against the
/// gap fields the same document publishes rather than against a constant
/// restated in the test.
#[test]
fn the_combined_incomplete_flag_holds_on_every_combination_of_gaps() {
    /// A classifier call that is measured, settled and complete.
    fn complete_classification(log: &mut Log) {
        log.intent("c-1", 1, "r1", "haiku");
        log.result(measured(
            "c-1",
            1,
            "r1",
            billed(100, 20),
            0.5,
            SettlementAck::Committed,
            Some("claude-haiku-4.5"),
        ));
    }

    /// One corner of the truth table: a log, and which half it leaves short.
    struct Case {
        what: &'static str,
        serving_short: bool,
        evaluation_short: bool,
        build: fn(&mut Log),
    }

    let cases = [
        Case {
            what: "both complete",
            serving_short: false,
            evaluation_short: false,
            build: |log| {
                log.turn(usage(1_000, 0, 100));
                complete_classification(log);
            },
        },
        Case {
            what: "serving complete, evaluation pending",
            serving_short: false,
            evaluation_short: true,
            build: |log| {
                log.turn(usage(1_000, 0, 100));
                // An intent whose worker died: cost unknown, never zero.
                log.intent("c-1", 1, "r1", "haiku");
            },
        },
        Case {
            what: "serving unpriced, evaluation complete",
            serving_short: true,
            evaluation_short: false,
            build: |log| {
                log.turn_on("unpriced", "mystery", usage(1_000, 0, 100));
                complete_classification(log);
            },
        },
        Case {
            what: "serving local, evaluation complete",
            serving_short: true,
            evaluation_short: false,
            build: |log| {
                log.turn_local("llama", usage(1_000, 0, 100));
                complete_classification(log);
            },
        },
        Case {
            what: "both incomplete",
            serving_short: true,
            evaluation_short: true,
            build: |log| {
                log.turn_on("unpriced", "mystery", usage(1_000, 0, 100));
                log.intent("c-1", 1, "r1", "haiku");
            },
        },
    ];

    for Case {
        what,
        serving_short,
        evaluation_short,
        build,
    } in cases
    {
        let mut log = Log::new("acme/ada/main", Some(ada()));
        build(&mut log);
        let recorder = MetricsRecorder::new();
        recorder.record(log.events());
        let document = json(&recorder.snapshot(&config(), 9_999));

        let gaps = ["estimated_calls", "unpriced_models", "local_calls"]
            .iter()
            .map(|field| count(&document, &["observed_cost", "serving_gaps", field]))
            .sum::<u64>();
        let evaluation = at(&document, &["evaluation", "cost_incomplete"]) == &Value::Bool(true);

        assert_eq!(gaps > 0, serving_short, "{what}: serving gaps");
        assert_eq!(evaluation, evaluation_short, "{what}: evaluation gaps");
        assert_eq!(
            at(&document, &["observed_cost", "evaluation_incomplete"]),
            at(&document, &["evaluation", "cost_incomplete"]),
            "{what}: the republished evaluation answer"
        );
        assert_eq!(
            at(&document, &["observed_cost", "incomplete"]),
            &Value::Bool(gaps > 0 || evaluation),
            "{what}: the combined flag is either half, never one of them"
        );
    }
}

/// Classifier usage priced from a recorded rate card is not a provider invoice.
#[test]
fn recorded_classifier_amounts_never_become_provider_reported_dollars() {
    let ada = principal(&ada());
    assert_eq!(
        at(&ada, &["savings", "provider_reported_usd"]),
        &Value::Null,
        "no provider in this log reported a dollar figure, and a classifier's \
         own pricing is not one"
    );
}

// ---------------------------------------------------------------------------
// Settlement, which is a separate fact from the cost
// ---------------------------------------------------------------------------

/// An unacknowledged settle does not erase the usage or make the call free.
#[test]
fn an_unconfirmed_settlement_keeps_its_cost_and_is_marked() {
    let ada = principal(&ada());
    let settlement = ["evaluation", "settlement"];

    // c-8 alone is still open: c-2's was repaired.
    assert_eq!(
        count(&ada, &["evaluation", "settlement", "unconfirmed_calls"]),
        1
    );
    close(
        dollars(&ada, &["evaluation", "settlement", "unconfirmed_usd"]),
        0.031_25,
        "the amount c-8 billed and nobody acknowledged",
    );
    assert_eq!(
        count(&ada, &["evaluation", "settlement", "acknowledged_calls"]),
        5
    );
    close(
        dollars(&ada, &["evaluation", "settlement", "committed_usd"]),
        0.25 + 0.125 + 0.5 + 0.0625,
        "the four measured calls whose settle was acknowledged",
    );

    let s = at(&ada, &settlement);
    close(
        dollars(s, &["committed_usd"]) + dollars(s, &["unconfirmed_usd"]),
        dollars(&ada, &["evaluation", "measured_usd"]),
        "every measured dollar is either acknowledged or not, and never both",
    );
    assert_eq!(
        count(s, &["acknowledged_calls"]) + count(s, &["unconfirmed_calls"]),
        count(&ada, &["evaluation", "results"]) - count(&ada, &["evaluation", "refused_calls"]),
        "every result that reached the service has a settlement state; a refusal has none"
    );
}

/// A repair moves a settlement and adds neither a call nor a cost.
///
/// `applied: false` is a success — the ledger already held the settle — so both
/// answers end the question, and a second acknowledgement of a call already
/// resolved resolves nothing further.
#[test]
fn a_settlement_repair_acknowledges_without_adding_a_second_cost() {
    let unrepaired = MetricsRecorder::new();
    let without: Vec<SessionEvent> = ada_log()
        .events()
        .iter()
        .filter(|event| {
            !matches!(
                event.kind,
                SessionEventKind::ClassificationSettlementRepaired { .. }
            )
        })
        .cloned()
        .collect();
    unrepaired.record(&without);
    let before = json(&unrepaired.snapshot_for(&PrincipalKey::from(&ada()), &config(), 9_999));
    let after = principal(&ada());

    // The cost observation is untouched by the acknowledgement.
    close(
        dollars(&before, &["evaluation", "measured_usd"]),
        dollars(&after, &["evaluation", "measured_usd"]),
        "a repair prices nothing",
    );
    assert_eq!(
        count(&before, &["evaluation", "results"]),
        count(&after, &["evaluation", "results"]),
        "a repair is not a call"
    );
    // What it moves is the settlement, exactly once.
    assert_eq!(
        count(&before, &["evaluation", "settlement", "unconfirmed_calls"]),
        2
    );
    assert_eq!(
        count(&after, &["evaluation", "settlement", "unconfirmed_calls"]),
        1
    );
    assert_eq!(
        count(&after, &["evaluation", "settlement", "repaired_calls"]),
        1
    );
    close(
        dollars(&after, &["evaluation", "settlement", "committed_usd"])
            - dollars(&before, &["evaluation", "settlement", "committed_usd"]),
        0.125,
        "exactly c-2's recorded amount moved, and only once",
    );
}

/// A repair with no accepted result behind it creates no spend.
#[test]
fn a_repair_without_an_accepted_result_books_nothing() {
    let ada = principal(&ada());
    // c-3 has an intent and no result; c-2's second acknowledgement resolved a
    // settlement already closed. Neither is a call and neither is a dollar.
    assert_eq!(
        count(&ada, &["evaluation", "unbooked", "unmatched_repairs"]),
        2
    );
    assert_eq!(
        count(&ada, &["evaluation", "pending"]),
        1,
        "c-3 is still pending"
    );
    assert_eq!(count(&ada, &["evaluation", "results"]), 7);
}

// ---------------------------------------------------------------------------
// Identity, kept distinct
// ---------------------------------------------------------------------------

/// What was asked for and what answered are two names, never merged.
///
/// A row is keyed by both, so a service that answered under a different model
/// than the one configured is visible rather than absorbed — and a call with no
/// usable reported identity stays `null` rather than borrowing the requested
/// name.
#[test]
fn requested_and_reported_model_identities_stay_distinct() {
    let ada = principal(&ada());
    let rows = at(&ada, &["evaluation", "models"])
        .as_array()
        .expect("the rows are an array")
        .clone();

    let find = |requested: &str, reported: Option<&str>| {
        rows.iter()
            .find(|row| {
                row["requested_model"] == requested
                    && row["reported_model"]
                        == reported.map_or(Value::Null, |m| Value::String(m.into()))
            })
            .unwrap_or_else(|| panic!("no row for {requested}/{reported:?} in {rows:#?}"))
            .clone()
    };

    let named = find("haiku", Some("claude-haiku-4.5"));
    assert_eq!(named["calls"], 3, "c-1, c-2 and c-6");
    assert_eq!(named["measured_calls"], 3);
    close(
        named["measured_usd"].as_f64().expect("a number"),
        0.875,
        "the three measured calls that answered under this identity",
    );

    // Absent and empty reported identities are both "no usable identity", and a
    // refusal, an unknown usage and a measured call can share that row without
    // becoming indistinguishable.
    let unknown = find("haiku", None);
    assert_eq!(unknown["calls"], 3);
    assert_eq!(
        unknown["measured_calls"], 1,
        "c-7, whose identity was blank"
    );
    assert_eq!(unknown["unknown_usage_calls"], 1, "c-5");
    assert_eq!(unknown["refused_calls"], 1, "c-4, which reached nobody");

    // The requested identity is a key too: a second configured model is its own
    // row even when the service answers under a name another row also saw.
    let other = find("big-classifier", Some("claude-sonnet-5"));
    assert_eq!(other["calls"], 1);

    let calls: u64 = rows
        .iter()
        .map(|row| row["calls"].as_u64().expect("a count"))
        .sum();
    assert_eq!(
        calls,
        count(&ada, &["evaluation", "results"]),
        "every accepted result is on exactly one row"
    );
}

// ---------------------------------------------------------------------------
// Once only, whatever the log does
// ---------------------------------------------------------------------------

/// Delivered twice, answered out of order, replayed whole: one cost.
#[test]
fn duplicate_and_reordered_delivery_preserve_once_only_totals() {
    let ada = principal(&ada());
    assert_eq!(
        count(&ada, &["evaluation", "unbooked", "duplicate_results"]),
        1,
        "c-1's second delivery, at its own sequence"
    );
    // c-2 landed before c-1 and both are booked exactly once.
    assert_eq!(count(&ada, &["evaluation", "measured_calls"]), 5);
}

/// A result that answers no intent of this session books nothing, and does not
/// stand in the way of the answer that does.
#[test]
fn an_unattributed_result_neither_books_nor_blocks() {
    let ada = principal(&ada());
    assert_eq!(
        count(&ada, &["evaluation", "unbooked", "unattributed_results"]),
        2,
        "the orphan, and c-6's mismatched source response"
    );
    // The orphan's $99 reached nothing.
    close(
        dollars(&ada, &["evaluation", "measured_usd"]),
        0.968_75,
        "an unattributed result is not spend",
    );
    // And c-6's valid answer, which arrived after the mismatch, booked once.
    let rows = at(&ada, &["evaluation", "models"])
        .as_array()
        .expect("the rows are an array");
    let named = rows
        .iter()
        .find(|row| {
            row["reported_model"] == "claude-haiku-4.5" && row["requested_model"] == "haiku"
        })
        .expect("the named row exists");
    assert_eq!(named["calls"], 3, "c-6 contributed, and contributed once");
}

/// A live fold, a batch fold and a replay produce the same document, byte for
/// byte.
///
/// The property the whole projection rests on: metrics are a function of the
/// log. Duplicate delivery, reverse completion order and the sequence watermark
/// all meet here, and a single byte of difference means one of the three paths
/// has kept state the log does not justify.
#[test]
fn a_replayed_classifier_log_folds_exactly_once() {
    let log = ada_log();

    let live = MetricsRecorder::new();
    for event in log.events() {
        live.record(std::slice::from_ref(event));
    }
    let batched = MetricsRecorder::new();
    batched.record(log.events());
    let replayed = MetricsRecorder::new();
    replayed.record(log.events());
    replayed.record(log.events());

    let document = |recorder: &MetricsRecorder| {
        serde_json::to_string(&recorder.snapshot(&config(), 9_999)).expect("encodes")
    };
    assert_eq!(document(&live), document(&batched));
    assert_eq!(
        document(&live),
        document(&replayed),
        "a log folded twice must report what it reported once"
    );
    // Not a comparison of three empty documents.
    assert_eq!(
        count(
            &json(&live.snapshot(&config(), 9_999)),
            &["evaluation", "results"]
        ),
        7
    );
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// One call id in two sessions is two calls, and each principal owns its own.
///
/// A call id is fresh per external attempt, so the identity that deduplicates a
/// delivery is `(session, call)` and not the call alone. Deduplicating across
/// sessions would silently drop a second tenant's spend — and pricing it twice
/// inside one session is the failure the dedup exists to prevent, so both
/// directions are pinned here.
#[test]
fn one_call_id_in_two_sessions_is_two_calls() {
    let ada_view = principal(&ada());
    let bob_view = principal(&bob());
    let acme = project("acme");

    assert_eq!(count(&ada_view, &["evaluation", "measured_calls"]), 5);
    assert_eq!(count(&bob_view, &["evaluation", "measured_calls"]), 1);
    assert_eq!(
        count(&acme, &["evaluation", "measured_calls"]),
        6,
        "the shared call id is two real calls, one per session"
    );
    close(
        dollars(&acme, &["evaluation", "measured_usd"]),
        0.968_75 + 1.5,
        "and two real costs",
    );
    assert_eq!(
        count(&acme, &["evaluation", "unbooked", "duplicate_results"]),
        1
    );
}

/// Every scope reports its own, and the deployment is the sum of them.
#[test]
fn the_three_scopes_report_their_own_evaluation_spend() {
    let everything = deployment();
    let acme = project("acme");
    let globex = project("globex");
    let ada_view = principal(&ada());

    close(
        dollars(&everything, &["evaluation", "measured_usd"]),
        dollars(&acme, &["evaluation", "measured_usd"])
            + dollars(&globex, &["evaluation", "measured_usd"]),
        "the deployment is its projects added up",
    );
    close(
        dollars(&globex, &["evaluation", "measured_usd"]),
        4.0,
        "globex's own call",
    );
    close(
        dollars(&ada_view, &["evaluation", "measured_usd"]),
        0.968_75,
        "ada sees her own and nobody else's",
    );
    assert_eq!(
        count(&ada_view, &["evaluation", "intents"]),
        8,
        "a principal view must not absorb its project's other members"
    );
    assert_eq!(count(&acme, &["evaluation", "intents"]), 9);
    assert_eq!(count(&everything, &["evaluation", "intents"]), 10);
}

/// A session with no principal at all still cost real classifier money, and
/// that money has to live somewhere: visible at the deployment, held on its
/// own [`PrincipalKey::Unattributed`] key, and never swept into a principal or
/// a project that did not incur it.
///
/// **Distinct from [`an_unattributed_result_neither_books_nor_blocks`].** That
/// test is about a *result* naming no outstanding intent of its own session —
/// an orphan delivery inside an otherwise ordinary, attributed session. This is
/// about a *session* whose `SessionCreated` carries no principal at all; the
/// classifier call here is fully matched and settled, and the only unknown is
/// who pays for it.
#[test]
fn unattributed_evaluation_spend_is_visible_and_never_leaks_into_a_principal_or_project() {
    // No principal on `SessionCreated`: pre-control-plane or otherwise
    // unresolved traffic. The call itself is ordinary and fully settled.
    let mut unattributed = Log::new("acme/legacy/main", None);
    unattributed.intent("c-legacy", 1, "r1", "haiku");
    unattributed.result(measured(
        "c-legacy",
        1,
        "r1",
        billed(100, 20),
        0.75,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));

    // An ordinary session in the same project, so "never leaks into" has a real
    // principal and a real project to fail to leak into.
    let mut owned = Log::new("acme/ada/main", Some(ada()));
    owned.intent("c-ada", 1, "r1", "haiku");
    owned.result(measured(
        "c-ada",
        1,
        "r1",
        billed(50, 10),
        0.10,
        SettlementAck::Committed,
        Some("claude-haiku-4.5"),
    ));

    let recorder = MetricsRecorder::new();
    recorder.record(unattributed.events());
    recorder.record(owned.events());
    let cfg = config();

    // Visible at the deployment: not silently dropped.
    let deployment = json(&recorder.snapshot(&cfg, 9_999));
    close(
        dollars(&deployment, &["evaluation", "measured_usd"]),
        0.75 + 0.10,
        "the deployment sees every classifier dollar, attributed or not",
    );
    assert_eq!(count(&deployment, &["evaluation", "results"]), 2);

    // Held on its own key, not merged into anything else.
    let marked = json(&recorder.snapshot_for(&PrincipalKey::Unattributed, &cfg, 9_999));
    close(
        dollars(&marked, &["evaluation", "measured_usd"]),
        0.75,
        "the unattributed key carries exactly the call nobody could be charged for",
    );
    assert_eq!(count(&marked, &["evaluation", "results"]), 1);

    // Never leaks into an unrelated principal in the same project.
    let ada_doc = json(&recorder.snapshot_for(&PrincipalKey::from(&ada()), &cfg, 9_999));
    close(
        dollars(&ada_doc, &["evaluation", "measured_usd"]),
        0.10,
        "ada's own document must not gain the unattributed call's dollars",
    );
    assert_eq!(count(&ada_doc, &["evaluation", "results"]), 1);

    // Never leaks into a project either: `PrincipalKey::Unattributed` belongs
    // to no project, so the project's own members are all it sees.
    let acme = json(&recorder.snapshot_for_project(&ProjectId::new("acme"), &cfg, 9_999));
    close(
        dollars(&acme, &["evaluation", "measured_usd"]),
        0.10,
        "the project view is its members' spend, and the unattributed call has no member",
    );
    assert_eq!(count(&acme, &["evaluation", "results"]), 1);
}

/// A tenant's document names no other tenant, and no durable call identity.
///
/// The evaluation view is counts and model names. A call id, a session id or a
/// neighbour's project would each be a new cross-tenant identifier on a surface
/// a turn key can read.
#[test]
fn the_evaluation_view_exposes_no_identifiers() {
    let ada = principal(&ada());
    let evaluation = serde_json::to_string(at(&ada, &["evaluation"])).expect("encodes");
    for secret in [
        "c-1",
        "c-8",
        "c-orphan",
        "acme",
        "ada",
        "bob",
        "globex",
        "zoe",
        "main",
        "r1",
        "schema_violation",
    ] {
        assert!(
            !evaluation.contains(secret),
            "the evaluation view must not carry `{secret}`: {evaluation}"
        );
    }
    // Not vacuous: the view does carry the model identities it is about.
    assert!(evaluation.contains("claude-haiku-4.5"));
}
