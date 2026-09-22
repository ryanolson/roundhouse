// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The wire half of the selection snapshot: what a log written before it
//! existed reads as, and what a log written with it keeps.
//!
//! The record is persisted, replayed and folded, so the two directions matter
//! separately. A historical decision must come back as *unknown* rather than as
//! a turn whose signals happened to be empty; a current one must come back with
//! the recipe order, the thresholds and the weights it ran under, because a
//! reader that had to recompute any of them would be reading the configuration
//! of whatever build is doing the reading.

use roundhouse_core::control::{Billing, BudgetState, Payer};
use roundhouse_core::routing::{
    AffinityEvidence, Candidate, Decision, DecisionRecord, DecisionSource, LocalFeatures, Pick,
    PickerMode, SelectionSnapshot, SelectorBranch, SelectorSnapshot, StageEvidence, StageOutcome,
    Target, Tier, TurnSignals,
};
use roundhouse_core::validate::{ControlCallDialect, ToolSignals};

fn hosted(model: &str) -> Target {
    Target::Frontier {
        provider: "openai".into(),
        model: model.into(),
    }
}

fn features() -> LocalFeatures {
    LocalFeatures {
        extractor_revision: 1,
        dialect: ControlCallDialect::ClaudeMessages,
        signals: TurnSignals {
            tools: ToolSignals {
                severity: 0.7,
                recent_read_count: 3,
                tests_passed: true,
                ..ToolSignals::default()
            },
            turn_depth: 11,
        },
        turn_index: 4,
        observed_through_seq: 37,
    }
}

fn stage_snapshot() -> SelectorSnapshot {
    SelectorSnapshot::stage(StageEvidence {
        capable: vec!["openai/sol".into()],
        efficient: vec!["openai/luna".into(), "openai/terra".into()],
        picker: PickerMode::EfficientFirst,
        confidence_threshold: 0.5,
        pick: Pick {
            tier: Tier::Efficient,
            source: DecisionSource::Ambiguous,
            score: -0.125,
            confidence: Some(0.125),
        },
        outcome: StageOutcome::CostGuard {
            served: Tier::Capable,
            displaced: "openai/luna".into(),
        },
    })
}

fn snapshot() -> SelectionSnapshot {
    SelectionSnapshot {
        features: features(),
        selected: hosted("sol"),
        fallbacks: vec![hosted("terra")],
        source: Some(DecisionSource::CostGuard),
        admitted: Some(vec![hosted("sol"), hosted("luna"), hosted("terra")]),
        selector: Some(stage_snapshot()),
    }
}

/// A decision record with everything the engine stamps, so the round trip below
/// is over the shape that is actually persisted.
fn record(selection: Option<SelectionSnapshot>) -> DecisionRecord {
    DecisionRecord {
        chosen: hosted("sol"),
        rationale: "stage router: strong tier (openai/sol) by cost_guard".into(),
        policy: "stage".into(),
        isl_tokens: 10_000,
        expected_prefill_tokens: 1_000.0,
        expected_cost_usd: 0.05,
        considered: Vec::<Candidate>::new(),
        turn_policy_digest: "digest".into(),
        budget_state: BudgetState::Unconstrained,
        rate_card: None,
        payer: Payer::Deployment,
        billing: Billing::Billed,
        budget_draw: None,
        withheld_providers: Vec::new(),
        declared_baseline: None,
        attempts: Vec::new(),
        local_quote_skipped: None,
        selection,
    }
}

// ---------------------------------------------------------------------------
// The direction history has to survive
// ---------------------------------------------------------------------------

/// **The claim.** A decision written before this field existed reads back as
/// unknown, not as a turn with empty signals.
#[test]
fn a_record_without_a_selection_deserializes_as_unknown() {
    let historical = serde_json::json!({
        "chosen": { "kind": "frontier", "provider": "openai", "model": "sol" },
        "rationale": "score 0.0000 over 2 candidate(s)",
        "policy": "affinity",
        "isl_tokens": 1_000,
        "expected_prefill_tokens": 800.0,
        "expected_cost_usd": 0.02,
        "considered": [],
    });
    let decoded: DecisionRecord =
        serde_json::from_value(historical).expect("a pre-selection log still reads");
    assert_eq!(
        decoded.selection, None,
        "the serde default must invent no features: an absent snapshot is a \
         record older than the field, and a default-constructed one would be \
         indistinguishable from a first turn with no tool traffic"
    );
}

/// **The claim.** A record that carries no snapshot writes the bytes it wrote
/// before the field existed.
#[test]
fn an_absent_selection_is_skipped_on_the_wire() {
    let encoded = serde_json::to_value(record(None)).expect("a record serializes");
    assert!(
        encoded.get("selection").is_none(),
        "a deployment that has not written one must not start writing nulls: {encoded}"
    );

    // CONTROL: a record that has one writes it.
    let with = serde_json::to_value(record(Some(snapshot()))).expect("a record serializes");
    assert!(with.get("selection").is_some());
}

// ---------------------------------------------------------------------------
// The direction a learner has to survive
// ---------------------------------------------------------------------------

/// **The claim.** Everything the snapshot carries comes back: the features, the
/// selection, the admitted pool, and the branch with its configuration.
#[test]
fn a_selection_snapshot_survives_a_round_trip_whole() {
    let original = record(Some(snapshot()));
    let json = serde_json::to_string(&original).expect("a record serializes");
    let decoded: DecisionRecord = serde_json::from_str(&json).expect("and reads back");
    assert_eq!(decoded, original);

    // And the parts a reader actually consults, named rather than left to the
    // whole-struct comparison above: a `PartialEq` that passed on two `None`s
    // would say nothing about any of them.
    let selection = decoded.selection.expect("the round trip kept it");
    assert_eq!(selection.features.signals.turn_depth, 11);
    assert_eq!(selection.features.signals.tools.severity, 0.7);
    assert!(selection.features.signals.tools.tests_passed);
    assert_eq!(
        selection.features.dialect,
        ControlCallDialect::ClaudeMessages
    );
    assert_eq!(selection.features.observed_through_seq, 37);
    assert_eq!(selection.features.turn_index, 4);
    assert_eq!(selection.source, Some(DecisionSource::CostGuard));
    assert_eq!(selection.fallbacks, vec![hosted("terra")]);
    assert_eq!(selection.admitted.as_ref().map(Vec::len), Some(3));
}

/// **The claim.** The recipe's order is preserved, not its membership: two
/// recipes over the same names are different evidence.
#[test]
fn the_recipe_order_and_the_thresholds_survive_distinctly() {
    let reordered = SelectorSnapshot::stage(StageEvidence {
        efficient: vec!["openai/terra".into(), "openai/luna".into()],
        confidence_threshold: 0.75,
        picker: PickerMode::CapableFirst,
        ..match stage_snapshot().branch {
            SelectorBranch::Stage(evidence) => evidence,
            other => panic!("the fixture is a stage snapshot, got {other:?}"),
        }
    });
    let shipped = stage_snapshot();

    let round = |snapshot: &SelectorSnapshot| -> SelectorSnapshot {
        serde_json::from_str(&serde_json::to_string(snapshot).expect("serializes"))
            .expect("reads back")
    };
    assert_eq!(round(&reordered), reordered);
    assert_eq!(round(&shipped), shipped);
    assert_ne!(
        round(&reordered),
        shipped,
        "a round trip that collapsed the order, the picker or the threshold \
         would make two different configurations read as one"
    );
}

/// **The claim.** Two differently tuned affinity policies stay distinguishable
/// across the wire.
#[test]
fn affinity_weights_survive_distinctly() {
    let tuned = SelectorSnapshot::affinity(AffinityEvidence {
        prefill_weight: 0.25,
        cost_weight: 2.0,
        ttft_weight: 0.75,
        max_load: Some(7_000.0),
    });
    let shipped = SelectorSnapshot::affinity(AffinityEvidence {
        prefill_weight: 1.0,
        cost_weight: 0.5,
        ttft_weight: 0.25,
        max_load: None,
    });

    let round = |snapshot: &SelectorSnapshot| -> SelectorSnapshot {
        serde_json::from_str(&serde_json::to_string(snapshot).expect("serializes"))
            .expect("reads back")
    };
    assert_eq!(round(&tuned), tuned);
    assert_eq!(round(&shipped), shipped);
    assert_ne!(round(&tuned), shipped);
}

// ---------------------------------------------------------------------------
// What a policy this module cannot name records
// ---------------------------------------------------------------------------

/// **The claim.** A decision assembled by a policy outside this module records
/// its branch and its pool as unknown, rather than as a nearest-fit builtin.
#[test]
fn a_hand_built_decision_records_unknown_evidence() {
    let custom = Decision {
        target: hosted("sol"),
        rationale: "a fourth policy chose this".into(),
        budget_state: BudgetState::Unconstrained,
        fallbacks: Vec::new(),
        source: None,
        admitted: None,
        selector: None,
    };

    let selection = SelectionSnapshot::of(&custom, features());
    assert_eq!(
        selection.admitted, None,
        "unknown, because no `Admitted` resolution produced this decision"
    );
    assert_eq!(selection.selector, None);
    assert_eq!(
        selection.selected,
        hosted("sol"),
        "what it chose is still recorded -- only the evidence about *how* is \
         absent"
    );
    assert_eq!(
        selection.features.signals.turn_depth, 11,
        "and the features are the engine's, which no policy can decline to \
         supply"
    );

    // CONTROL: a decision that came through `Admitted` carries both, so the
    // `None`s above are this policy's silence and not a field nothing fills.
    let builtin = Decision {
        admitted: Some(vec![hosted("sol")]),
        selector: Some(stage_snapshot()),
        ..custom
    };
    let selection = SelectionSnapshot::of(&builtin, features());
    assert!(selection.admitted.is_some());
    assert!(selection.selector.is_some());
}
