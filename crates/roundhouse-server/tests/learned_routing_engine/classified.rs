// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The two learner claims that need a classification window: row 2 of draft
//! 7.8, where the classification sequence changes the route, and the Jev tier
//! answer reaching the store as a count on its source turn's keys.
//!
//! The classifier is configured and **saturated for the whole of each test**:
//! its one permit is held, so the engine records a window on every routed turn
//! and requests no classification of its own. The history is seeded through
//! the session's own writer, as `classification_window_engine.rs` does.

use std::sync::Arc;

use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
    ContextDependence, EvaluationSpend, Graded, ReservationRecord, SettlementAck, TAXONOMY_VERSION,
    TierChoice, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{BudgetWindow, MemorySpendLedger};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::learn_store::{LearnerStore, ReadRequest};
use roundhouse_core::routing::learn::{
    Band, JevCounts, LearnedChoice, LearnedInput, LearnerMode, PriorBand, Strategy,
};
use roundhouse_core::routing::{CacheLedger, ProviderPricing, Tier};
use roundhouse_core::session::Session;
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};

use crate::rig::{
    BELOW, PASS, Rig, RigConfig, admission, epoch, large, project, seed, small, terms,
};

fn env(name: &str) -> Option<String> {
    match name {
        "LEARNED_ROUTING_ENGINE_KEY" => Some("sk-learned-routing-engine".to_string()),
        _ => None,
    }
}

/// One permit, and a base URL nothing listens on.
fn classify_config() -> ClassifyConfig {
    roundhouse_server::test_support::classification::classify_config(
        "http://127.0.0.1:1",
        |value| {
            value["revision"] = serde_json::json!(7);
            value["auth"]["env"] = serde_json::json!("LEARNED_ROUTING_ENGINE_KEY");
            value["caps"]["max_prior_classifications"] = serde_json::json!(4);
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

fn intent(turn: u64, source: &ResponseId) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(format!("call_{}_{turn:06}", source.as_str())),
        source_turn_index: turn,
        source_response_id: source.clone(),
        requested_at_ms: 0,
        expires_at_ms: u64::MAX,
        identity: identity(),
        reservation: reservation(),
    }
}

fn result(
    turn: u64,
    source: &ResponseId,
    complexity: TurnComplexity,
    tier: TierChoice,
) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(format!("call_{}_{turn:06}", source.as_str())),
        source_turn_index: turn,
        source_response_id: source.clone(),
        completed_at_ms: turn,
        outcome: ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.8,
                },
                complexity: Graded {
                    value: complexity,
                    confidence: 0.6,
                },
                context_dependence: Graded {
                    value: ContextDependence::Recent,
                    confidence: 0.5,
                },
                tier: Some(Graded {
                    value: tier,
                    confidence: 0.7,
                }),
            },
            spend: EvaluationSpend::Unknown {
                granted_usd: 0.0,
                settled: SettlementAck::Committed,
                submitted_usd: 0.0,
            },
            reported_model: None,
        },
    }
}

/// Commit `(turn, source, complexity, tier)` classifications into the log the
/// engine serves from, through the session's own writer.
async fn seed_history(
    rig: &Rig,
    session_id: &SessionId,
    history: &[(u64, ResponseId, TurnComplexity, TierChoice)],
) {
    let mut session = Session::open(
        Arc::clone(&rig.sessions),
        session_id.clone(),
        "node-seed",
        30_000,
        CacheLedger::new(),
    )
    .await
    .expect("the engine released the session");
    for (turn, source, complexity, tier) in history {
        session
            .record_classification_intent(intent(*turn, source))
            .await
            .expect("an intent commits");
        session
            .record_background_classification(
                vec![result(*turn, source, *complexity, *tier)],
                vec![],
            )
            .await
            .expect("and its result");
    }
    session.release().await.expect("the lease goes back");
}

fn key(newest: Band, prior: PriorBand) -> roundhouse_core::routing::learn::LevelKey {
    LearnedInput {
        rules_pick: Tier::Capable,
        newest,
        prior,
        tool_turn: false,
    }
    .key(roundhouse_core::routing::learn::KeyLevel::L2)
}

/// Row 2. Two sessions whose newest classification is the same band: in A an
/// older one was `high`, and `efficient` is below the floor there; in B none
/// was, and `efficient` passes. The route follows the sequence.
#[tokio::test]
async fn row_2_the_classification_sequence_changes_the_route() {
    let classifier = classifier();
    let _held = classifier.capacity().expect("the one configured permit");
    let rig = Rig::new(RigConfig {
        classifier: Some(Arc::clone(&classifier)),
        ..RigConfig::default()
    });
    seed(
        &rig.learner,
        "seq",
        key(Band::Low, PriorBand::SomeHigh),
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, BELOW),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    seed(
        &rig.learner,
        "seq",
        key(Band::Low, PriorBand::NoHigh),
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let admission = admission("seq", Some(terms(LearnerMode::Live)));
    let mut routes = Vec::new();
    for (name, older) in [
        ("a", TurnComplexity::Involved),
        ("b", TurnComplexity::Routine),
    ] {
        let session = SessionId::new(format!("seq/ada/{name}"));
        let first = rig.turn(&session, "t1", &admission).await.expect("served");
        let source = first.response_id;
        seed_history(
            &rig,
            &session,
            &[
                (0, source.clone(), older, TierChoice::Capable),
                (1, source, TurnComplexity::Routine, TierChoice::Capable),
            ],
        )
        .await;
        rig.learner.inner.record_visits();
        let second = rig.turn(&session, "t2", &admission).await.expect("served");
        let record = rig.last_learned(&session).await;
        // The read and the decision name one key: the window the policy
        // encodes its input from is the window the read was built from.
        let l2 = record
            .input
            .key(roundhouse_core::routing::learn::KeyLevel::L2);
        let visited = rig.learner.inner.take_visited();
        assert!(
            visited
                .iter()
                .any(|name| name.ends_with(&format!(":q:l2:{}", l2.part()))),
            "the read visits the recorded L2 key {l2}: {visited:?}"
        );
        routes.push((
            record.input.newest,
            record.input.prior,
            record.choice.clone(),
            second.decision.expect("routed").target,
        ));
    }
    assert_eq!(
        routes,
        vec![
            (
                Band::Low,
                PriorBand::SomeHigh,
                LearnedChoice::Exploit {
                    strategy: Strategy::Rules
                },
                large()
            ),
            (
                Band::Low,
                PriorBand::NoHigh,
                LearnedChoice::Exploit {
                    strategy: Strategy::Efficient
                },
                small()
            ),
        ]
    );
}

/// A classification of a learned turn that carries a tier answer is one Jev
/// count on each of that turn's three keys, delivered by the next tail.
#[tokio::test]
async fn a_jev_tier_answer_reaches_the_store_as_a_count_on_the_source_turn_keys() {
    let classifier = classifier();
    let _held = classifier.capacity().expect("the one configured permit");
    let rig = Rig::new(RigConfig {
        classifier: Some(Arc::clone(&classifier)),
        ..RigConfig::default()
    });
    let admission = admission("jev", Some(terms(LearnerMode::Shadow)));
    let session = SessionId::new("jev/ada/s");
    let first = rig.turn(&session, "t1", &admission).await.expect("served");
    let source_input = rig.last_learned(&session).await.input;
    seed_history(
        &rig,
        &session,
        &[(
            0,
            first.response_id,
            TurnComplexity::Deep,
            TierChoice::Capable,
        )],
    )
    .await;
    rig.turn(&session, "t2", &admission).await.expect("served");

    let request = ReadRequest::new(
        project("jev"),
        epoch(),
        &source_input,
        &terms(LearnerMode::Shadow).strategies,
        [&large(), &small()],
    );
    let view = rig
        .learner
        .inner
        .read(&request)
        .await
        .expect("a memory read");
    let counts: Vec<JevCounts> = view.levels.iter().map(|level| level.jev).collect();
    assert_eq!(
        counts,
        vec![
            JevCounts {
                capable: 1,
                efficient: 0
            };
            3
        ],
        "one capable answer on each of the source turn's keys"
    );
}
