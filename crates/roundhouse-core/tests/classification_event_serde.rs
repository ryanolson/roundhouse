// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The two durable classification kinds, through bytes.
//!
//! **A fold is not a format.** `MemoryStore` hands back the values it was
//! given, so every engine-level assertion about these records proves the
//! projection and says nothing about what a durable store would write and read.
//! These are internally tagged, nested enums carrying floats; the shapes worth
//! pinning are pinned here against literals rather than against an argument.
//!
//! The fold itself is asserted too, and deliberately through
//! `SessionState::project` over a *serialized* log: that is the path a successor
//! process takes, and the one place "the answer is unknown" has to survive as a
//! fact rather than as an absence.

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord,
    ClassificationSettlementRepair, ClassifierIdentity, ContextDependence, EvaluationSpend,
    EvaluationUsage, Graded, ReservationRecord, SettlementAck, TAXONOMY_VERSION,
    TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::control::BudgetWindow;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::{CacheLedger, ProviderPricing};
use roundhouse_core::session::SessionState;
use roundhouse_core::store::{MemoryStore, SessionStore};

fn intent() -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new("eval_1"),
        source_turn_index: 2,
        source_response_id: ResponseId::new("resp_2"),
        requested_at_ms: 1_000,
        expires_at_ms: 61_000,
        identity: ClassifierIdentity {
            model: "jev-1.12".into(),
            schema: "typesafe.systemone.choice.v1".into(),
            taxonomy_version: TAXONOMY_VERSION,
            projection_revision: 1,
            config_revision: 4,
        },
        reservation: ReservationRecord {
            rate_card: ProviderPricing {
                input_per_mtok_usd: 0.042,
                cached_input_per_mtok_usd: 0.0,
                cache_write_per_mtok_usd: 0.0,
                output_per_mtok_usd: 0.084,
            },
            estimated_input_tokens: 900,
            expected_output_tokens: 24,
            requested_usd: 0.000_039_8,
            hold_ttl_ms: 65_000,
            budget_limit_usd: 25.0,
            budget_window: BudgetWindow::Monthly,
            member_ceiling_usd: Some(6.25),
            warn_at: 0.8,
        },
    }
}

fn classification() -> TurnClassification {
    TurnClassification {
        taxonomy_version: TAXONOMY_VERSION,
        intent: Graded {
            value: TurnIntent::Diagnose,
            confidence: 0.82,
        },
        complexity: Graded {
            value: TurnComplexity::Deep,
            confidence: 0.61,
        },
        context_dependence: Graded {
            value: ContextDependence::Unknown,
            confidence: 0.4,
        },
    }
}

fn record(outcome: ClassificationOutcome) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new("eval_1"),
        source_turn_index: 2,
        source_response_id: ResponseId::new("resp_2"),
        completed_at_ms: 2_000,
        outcome,
    }
}

fn round<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    serde_json::from_str(&serde_json::to_string(value).expect("serializes")).expect("reads back")
}

/// **The intent's reservation survives as numbers.**
///
/// A digest would say two turns differed and never how, which is why these are
/// fields; a round trip that lost one would leave later accounting unable to
/// answer what a call was allowed to cost.
#[test]
fn an_intent_round_trips_with_every_reservation_term() {
    let original = SessionEventKind::ClassificationRequested { record: intent() };
    assert_eq!(round(&original), original);

    let encoded = serde_json::to_value(&original).expect("serializes");
    assert_eq!(encoded["type"], "classification_requested");
    let reservation = &encoded["record"]["reservation"];
    assert_eq!(reservation["budget_limit_usd"], 25.0);
    assert_eq!(reservation["budget_window"], "monthly");
    assert_eq!(reservation["member_ceiling_usd"], 6.25);
    assert_eq!(reservation["hold_ttl_ms"], 65_000);
    assert_eq!(reservation["requested_usd"], 0.000_039_8);
    assert!(
        reservation.get("granted_usd").is_none(),
        "an intent records a quote and never a grant: no ledger has been asked \
         anything when this is written"
    );
    assert_eq!(reservation["estimated_input_tokens"], 900);
    assert_eq!(encoded["record"]["identity"]["config_revision"], 4);
    assert_eq!(
        encoded["record"]["expires_at_ms"], 61_000,
        "the absolute expiry is what a later reader dates an unknown answer by"
    );
}

/// Every outcome shape survives, and each is distinguishable in the bytes.
///
/// `Classified`, `Unusable` and `Failed` are three different statements about
/// one call, and the whole vocabulary exists to stop them collapsing.
#[test]
fn all_three_outcomes_round_trip_and_read_distinctly() {
    let usage = EvaluationUsage {
        input_tokens: 312,
        output_tokens: 48,
    };
    let cases = [
        (
            "classified",
            ClassificationOutcome::Classified {
                classification: classification(),
                spend: EvaluationSpend::Measured {
                    granted_usd: 0.000_1,
                    usage,
                    usd: 0.000_017,
                    settled: SettlementAck::Committed,
                },
                // Deliberately not the id the intent was requested under: the
                // round trip has to carry the service's own answer, and a
                // fixture that reused the request alias could not tell the two
                // apart if one were written over the other.
                reported_model: Some("jev-1.12-canary-2026w38".into()),
            },
        ),
        (
            "unusable",
            ClassificationOutcome::Unusable {
                reason: "sum_is_not_one".into(),
                spend: EvaluationSpend::Measured {
                    granted_usd: 0.000_1,
                    usage,
                    usd: 0.000_017,
                    settled: SettlementAck::Unconfirmed,
                },
                reported_model: Some("jev-1.12-canary-2026w38".into()),
            },
        ),
        (
            "failed",
            ClassificationOutcome::Failed {
                reason: "transport_timeout".into(),
                spend: EvaluationSpend::Unknown {
                    granted_usd: 0.000_1,
                    settled: SettlementAck::Committed,
                },
            },
        ),
    ];
    for (tag, outcome) in cases {
        let original = SessionEventKind::ClassificationRecorded {
            record: record(outcome),
        };
        assert_eq!(round(&original), original, "{tag}");
        let encoded = serde_json::to_value(&original).expect("serializes");
        assert_eq!(encoded["type"], "classification_recorded");
        assert_eq!(encoded["record"]["outcome"]["kind"], tag);
    }
}

/// **An unconfirmed settle reads back as unconfirmed.**
///
/// The one field that separates "the service billed this" from "this
/// deployment holds the charge". A round trip that dropped it would turn every
/// ledger outage into a discount on replay — and, now that a repair exists, it
/// would also lose the only evidence that anything is owed.
#[test]
fn the_settlement_acknowledgement_survives_and_is_not_defaulted() {
    let unconfirmed = record(ClassificationOutcome::Classified {
        classification: classification(),
        spend: EvaluationSpend::Measured {
            granted_usd: 0.000_1,
            usage: EvaluationUsage {
                input_tokens: 312,
                output_tokens: 48,
            },
            usd: 0.000_017,
            settled: SettlementAck::Unconfirmed,
        },
        reported_model: None,
    });
    let decoded = round(&unconfirmed);
    assert_eq!(
        decoded.outcome.spend().expect("a call was made").settled(),
        SettlementAck::Unconfirmed
    );
    assert_eq!(decoded.outcome.committed_usd(), None);
    assert_eq!(
        decoded
            .outcome
            .spend()
            .expect("a call was made")
            .unconfirmed_settlement_usd(),
        Some(0.000_017),
        "and it reads back as something a repair can re-drive, at the price \
         the record carries rather than one derived after the fact"
    );

    let encoded = serde_json::to_value(&unconfirmed).expect("serializes");
    assert_eq!(encoded["outcome"]["spend"]["settled"], "unconfirmed");
    assert_eq!(
        encoded["outcome"]["spend"]["usage"]["input_tokens"], 312,
        "and the usage is still reported, because the service still billed it"
    );
}

/// **The repair record round-trips, and `applied: false` survives as itself.**
///
/// The field a reader is most likely to mistake for a failure is the one a
/// codec is most likely to lose: a missing boolean defaulting to `false` and a
/// genuine "the ledger already had this call" are the same bytes to anyone not
/// looking. Pinned against a literal so the durable shape is a checked claim
/// rather than an argument about serde attributes.
#[test]
fn a_settlement_repair_round_trips_with_the_answer_the_ledger_gave() {
    for applied in [true, false] {
        let original = SessionEventKind::ClassificationSettlementRepaired {
            record: ClassificationSettlementRepair {
                call_id: ResponseId::new("eval_1"),
                applied,
                repaired_at_ms: 9_000,
            },
        };
        assert_eq!(round(&original), original);

        let encoded = serde_json::to_value(&original).expect("serializes");
        assert_eq!(encoded["type"], "classification_settlement_repaired");
        assert_eq!(encoded["record"]["call_id"], "eval_1");
        assert_eq!(encoded["record"]["applied"], applied);
        assert_eq!(encoded["record"]["repaired_at_ms"], 9_000);
        assert!(
            encoded["record"].get("usd").is_none()
                && encoded["record"].get("principal").is_none()
                && encoded["record"].get("window").is_none(),
            "a repair names a call and an answer, and carries no second copy \
             of the amount, the payer or the window: those live on the intent \
             and the result it resolves, and two copies of a price are two \
             numbers that can drift"
        );
    }
}

/// **A repair resolves its settlement in the fold, and changes nothing else.**
///
/// Through a serialized log and `SessionState::project`, which is the path a
/// successor process takes. The two negatives are the load-bearing half: a
/// repair that added a classification would give one answer extra weight in a
/// later decision's features, and one that moved an availability sequence
/// would let a late acknowledgement backdate evidence.
#[tokio::test]
async fn a_repair_resolves_its_settlement_and_creates_no_feature() {
    let store = MemoryStore::new();
    let session = SessionId::new("sess_repair_fold");
    store.create_session(&session, "affinity").await.unwrap();

    let unconfirmed = record(ClassificationOutcome::Classified {
        classification: classification(),
        spend: EvaluationSpend::Measured {
            granted_usd: 0.000_1,
            usage: EvaluationUsage {
                input_tokens: 312,
                output_tokens: 48,
            },
            usd: 0.000_017,
            settled: SettlementAck::Unconfirmed,
        },
        reported_model: None,
    });
    let lease = store
        .acquire_lease(&session, "node_repair_fold", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![
                SessionEventKind::SessionCreated {
                    model_policy: "affinity".into(),
                    principal: Some(roundhouse_core::control::Principal::new("acme", "ada")),
                    arm: None,
                },
                SessionEventKind::ClassificationRequested { record: intent() },
                SessionEventKind::ClassificationRecorded {
                    record: unconfirmed,
                },
            ],
            None,
        )
        .await
        .unwrap();

    let before = SessionState::project(&store, &session, CacheLedger::new(), None)
        .await
        .expect("the log replays");
    assert_eq!(
        before.unrepaired_settlements().len(),
        1,
        "an unconfirmed settle is something a successor can find and re-drive"
    );
    let unconfirmed = before
        .unrepaired_settlements()
        .next()
        .expect("the settle the fold held");
    assert_eq!(unconfirmed.usd, 0.000_017);
    assert_eq!(
        unconfirmed.window,
        BudgetWindow::Monthly,
        "the window comes off the intent, which is the only place it exists"
    );
    assert_eq!(
        before.principal(),
        Some(&roundhouse_core::control::Principal::new("acme", "ada")),
        "and the payer comes off the session's first event"
    );
    let classifications_before = before.classifications().len();

    store
        .append_events(
            &lease,
            vec![SessionEventKind::ClassificationSettlementRepaired {
                record: ClassificationSettlementRepair {
                    call_id: ResponseId::new("eval_1"),
                    applied: false,
                    repaired_at_ms: 9_000,
                },
            }],
            None,
        )
        .await
        .unwrap();

    let after = SessionState::project(&store, &session, CacheLedger::new(), None)
        .await
        .expect("the log replays");
    assert!(
        after.unrepaired_settlements().len() == 0,
        "`applied: false` is the ledger saying it already had this call, which \
         ends the question -- retaining it would re-drive the same settle on \
         every later turn forever"
    );
    assert_eq!(
        after.classifications().len(),
        classifications_before,
        "and a repair is not a classification: it adds no feature"
    );
    assert!(
        after.classification_settled(&ResponseId::new("eval_1")),
        "delivery acceptance is untouched by a repair that follows it -- the \
         fold arm that resolves `unrepaired_settlements` names no other field"
    );
}

/// `unknown` on an axis is a label the classifier chose, and it survives as one
/// rather than as a missing field.
#[test]
fn an_unknown_axis_round_trips_as_a_chosen_label() {
    let encoded = serde_json::to_value(classification()).expect("serializes");
    assert_eq!(encoded["context_dependence"]["value"], "unknown");
    assert_eq!(encoded["context_dependence"]["confidence"], 0.4);
    assert_eq!(encoded["intent"]["value"], "diagnose");
    assert_eq!(round(&classification()), classification());
}

/// **A successor reads an outstanding intent as an unknown answer**, through a
/// real store and the fold a replay runs.
///
/// This is the durability claim stated as a test: the process that bought the
/// answer died, and what survives is the knowledge that it was bought — not a
/// reason to buy it again.
#[tokio::test]
async fn a_replayed_log_reports_an_answered_call_and_an_outstanding_one() {
    let store = MemoryStore::new();
    let session = SessionId::new("sess_replay_fold");
    store.create_session(&session, "affinity").await.unwrap();
    let answered = ClassificationIntent {
        call_id: ResponseId::new("eval_answered"),
        source_response_id: ResponseId::new("resp_1"),
        source_turn_index: 1,
        ..intent()
    };
    let lease = store
        .acquire_lease(&session, "node_replay_fold", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![
                SessionEventKind::ClassificationRequested {
                    record: answered.clone(),
                },
                SessionEventKind::ClassificationRequested { record: intent() },
                SessionEventKind::ClassificationRecorded {
                    record: ClassificationRecord {
                        call_id: ResponseId::new("eval_answered"),
                        source_turn_index: 1,
                        source_response_id: ResponseId::new("resp_1"),
                        completed_at_ms: 2_000,
                        outcome: ClassificationOutcome::Classified {
                            classification: classification(),
                            spend: EvaluationSpend::Unknown {
                                granted_usd: 0.000_1,
                                settled: SettlementAck::Committed,
                            },
                            reported_model: None,
                        },
                    },
                },
            ],
            None,
        )
        .await
        .expect("an in-memory log appends");

    let state = SessionState::project(&store, &session, CacheLedger::default(), None)
        .await
        .expect("a replay");

    let outstanding: Vec<_> = state.outstanding_classifications().collect();
    assert_eq!(
        outstanding.len(),
        1,
        "the answered call left the outstanding set and the unanswered one did \
         not"
    );
    assert_eq!(outstanding[0].call_id, ResponseId::new("eval_1"));
    assert_eq!(
        outstanding[0].reservation.requested_usd, 0.000_039_8,
        "and what it was quoted at is readable from the log alone"
    );

    let available = state.classifications();
    assert_eq!(available.len(), 1);
    assert_eq!(
        available[0].classification.intent.value,
        TurnIntent::Diagnose
    );
    assert!(
        state.classification_settled(&ResponseId::new("eval_answered")),
        "a delivered call is refused a second delivery by the log's own record"
    );
    assert!(!state.classification_settled(&ResponseId::new("eval_1")));

    // And the availability cutoff is what a decision taken earlier would have
    // been filtered by.
    let seq = available[0].reference.available_seq;
    assert_eq!(state.classifications_through(seq).count(), 1);
    assert_eq!(
        state.classifications_through(seq - 1).count(),
        0,
        "a decision whose cutoff predates the result cannot name it"
    );
}
