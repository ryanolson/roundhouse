// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The exact figure behind classifier evaluation settlement, once every open
//! call has closed.
//!
//! A sibling file for the same reason `first_output_tests.rs` and
//! `turn_elapsed_tests.rs` are: the fixtures stay in `fold.rs`'s own test
//! module and are imported here, so one `LogBuilder` means one clock and one
//! session id namespace.

use super::tests::{LogBuilder, principal};
use super::*;
use crate::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord,
    ClassificationSettlementRepair, ClassifierIdentity, EvaluationSpend, EvaluationUsage,
    ReservationRecord, SettlementAck, TAXONOMY_VERSION,
};
use crate::control::{BudgetWindow, PrincipalKey};
use crate::event::SessionEventKind;
use crate::ids::ResponseId;
use crate::routing::ledger::ProviderPricing;

fn classify_identity() -> ClassifierIdentity {
    ClassifierIdentity {
        model: "jev-1.12".to_string(),
        schema: "typesafe.systemone.choice.v1".to_string(),
        taxonomy_version: TAXONOMY_VERSION,
        projection_revision: 1,
        config_revision: 1,
    }
}

fn classify_reservation() -> ReservationRecord {
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

fn classify_intent(
    call_id: &str,
    source_turn_index: u64,
    source_response_id: &str,
) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(call_id),
        source_turn_index,
        source_response_id: ResponseId::new(source_response_id),
        requested_at_ms: 0,
        expires_at_ms: 30_000,
        identity: classify_identity(),
        reservation: classify_reservation(),
    }
}

/// A result whose settle nobody has acknowledged yet -- the one state a
/// repair later resolves. `Unusable` rather than `Classified`, so the
/// fixture needs no `TurnClassification`: the accounting is what these tests
/// are about, not the answer.
fn unconfirmed_result(
    call_id: &str,
    source_turn_index: u64,
    source_response_id: &str,
    usd: f64,
) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call_id),
        source_turn_index,
        source_response_id: ResponseId::new(source_response_id),
        completed_at_ms: 1_000,
        outcome: ClassificationOutcome::Unusable {
            reason: "schema".to_string(),
            spend: EvaluationSpend::Measured {
                usage: EvaluationUsage {
                    input_tokens: 100,
                    output_tokens: 16,
                },
                usd,
                granted_usd: usd,
                settled: SettlementAck::Unconfirmed,
            },
            reported_model: None,
        },
    }
}

fn repair(call_id: &str) -> ClassificationSettlementRepair {
    ClassificationSettlementRepair {
        call_id: ResponseId::new(call_id),
        applied: true,
        repaired_at_ms: 2_000,
    }
}

/// Three unconfirmed results summed in result order, repaired in the
/// *reverse* order. Float addition is not associative, so
/// `awaiting_usd - repaired_usd` (a derivation this fold does not use) does
/// not land on exactly `0.0` here even though every call has closed --
/// summing the open set directly, rather than subtracting from a running
/// total, is what lands on it.
#[test]
fn unconfirmed_usd_is_exactly_zero_once_every_open_call_is_repaired_in_reverse_order() {
    let ada = principal("acme", "ada");
    let mut log = LogBuilder::new("s1");
    log.created(Some(ada.clone()));
    for (call, usd) in [("c1", 0.1), ("c2", 0.2), ("c3", 0.3)] {
        log.push(SessionEventKind::ClassificationRequested {
            record: classify_intent(call, 1, "r1"),
        });
        log.push(SessionEventKind::ClassificationRecorded {
            record: unconfirmed_result(call, 1, "r1", usd),
        });
    }
    for call in ["c3", "c2", "c1"] {
        log.push(SessionEventKind::ClassificationSettlementRepaired {
            record: repair(call),
        });
    }

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let counters = fold.evaluation(Scope::Deployment);
    assert_eq!(
        counters.counters.unmatched_repairs, 0,
        "every repair matched an open call"
    );
    assert_eq!(
        counters.unconfirmed_calls(),
        0,
        "all three calls were repaired"
    );
    assert_eq!(
        counters.unconfirmed_usd().to_bits(),
        0.0_f64.to_bits(),
        "bit-exact zero, not a signed residue that would format as -$0.00: got {}",
        counters.unconfirmed_usd()
    );
}

/// The same three calls, repaired in result order this time -- the
/// direction that happened to cancel exactly under the old subtraction.
/// Kept as a control: it must still hold once the derivation changes, so a
/// fix that only special-cases the reversed order is caught here.
#[test]
fn unconfirmed_usd_is_exactly_zero_once_every_open_call_is_repaired_in_result_order() {
    let ada = principal("acme", "ada");
    let mut log = LogBuilder::new("s1");
    log.created(Some(ada.clone()));
    for (call, usd) in [("c1", 0.1), ("c2", 0.2), ("c3", 0.3)] {
        log.push(SessionEventKind::ClassificationRequested {
            record: classify_intent(call, 1, "r1"),
        });
        log.push(SessionEventKind::ClassificationRecorded {
            record: unconfirmed_result(call, 1, "r1", usd),
        });
    }
    for call in ["c1", "c2", "c3"] {
        log.push(SessionEventKind::ClassificationSettlementRepaired {
            record: repair(call),
        });
    }

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let counters = fold.evaluation(Scope::Deployment);
    assert_eq!(counters.unconfirmed_calls(), 0);
    assert_eq!(counters.unconfirmed_usd().to_bits(), 0.0_f64.to_bits());
}

/// With nothing repaired at all, every call is genuinely still open, and
/// `committed_usd` -- the figure `unconfirmed_usd` was carved out of --
/// must itself land on exactly `0.0` rather than inherit a residue from
/// whatever order the open sum happens to run in.
#[test]
fn committed_usd_is_exactly_zero_while_nothing_has_been_acknowledged() {
    let ada = principal("acme", "ada");
    let mut log = LogBuilder::new("s1");
    log.created(Some(ada.clone()));
    // Call ids deliberately out of numeric order, so a sum keyed by
    // `(SessionId, ResponseId)` does not coincide with result order.
    for (call, usd) in [("c3", 0.1), ("c1", 0.2), ("c2", 0.3)] {
        log.push(SessionEventKind::ClassificationRequested {
            record: classify_intent(call, 1, "r1"),
        });
        log.push(SessionEventKind::ClassificationRecorded {
            record: unconfirmed_result(call, 1, "r1", usd),
        });
    }

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let counters = fold.evaluation(Scope::Deployment);
    assert_eq!(counters.unconfirmed_calls(), 3);
    assert_eq!(counters.acknowledged_calls(), 0);
    assert_eq!(
        counters.counters.committed_usd().to_bits(),
        0.0_f64.to_bits(),
        "got {}",
        counters.counters.committed_usd()
    );
}

/// A row nobody has tallied yet must not count an open call as acknowledged.
///
/// `EvaluationFold::principal_row` reads exactly what `by_principal` holds,
/// with no call through `tally`. The open amount lives on the row's own
/// `open` set, so `acknowledged_calls()` -- which every per-principal row can
/// answer for itself -- excludes it without needing a second scan or a
/// resolver back to `tally`.
#[test]
fn a_row_nobody_has_tallied_must_not_count_an_open_call_as_acknowledged() {
    let ada = principal("acme", "ada");
    let mut log = LogBuilder::new("s1");
    log.created(Some(ada.clone()));
    log.push(SessionEventKind::ClassificationRequested {
        record: classify_intent("c1", 1, "r1"),
    });
    log.push(SessionEventKind::ClassificationRecorded {
        record: unconfirmed_result("c1", 1, "r1", 0.5),
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let key = PrincipalKey::from(&ada);
    let row = fold
        .evaluation
        .principal_row(&key)
        .expect("ada booked one call");
    assert_eq!(row.counters.all.calls, 1, "the result was booked");
    assert_eq!(
        row.acknowledged_calls(),
        0,
        "the one call this row booked is still open, so a raw read of the \
         row must not report it as acknowledged"
    );
}
