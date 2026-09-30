// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The wire shapes review coverage adds, and the history they must keep
//! reading.

use roundhouse_core::event::{SessionEventKind, ValidationOutcome};
use roundhouse_core::ids::ResponseId;
use roundhouse_core::routing::SelectionSnapshot;
use roundhouse_core::validate::{
    CoverageGap, IntervalLabel, IntervalReview, ObjectiveVersion, REVIEW_RULE_REVISION,
    ReviewedDecision, Verdict, label_for,
};

/// Byte for byte what a judged validation serialized to before coverage.
const HISTORICAL_JUDGED: &str = r#"{"type":"validation_decided","validation_id":"val_1","trigger":{"turn_index":3,"tokens_since_last_validation":100,"signals":[]},"arm":"shadow","outcome":{"outcome":"judged","side_call_id":"sc_1","verdict":{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null},"action":{"action":"continue"}}}"#;

fn review() -> IntervalReview {
    IntervalReview {
        rule_revision: REVIEW_RULE_REVISION,
        after_seq: 4,
        through_seq: 19,
        decisions: vec![ReviewedDecision {
            routed_seq: 9,
            turn_index: 1,
            response_id: ResponseId::new("resp_a"),
        }],
        gaps: vec![CoverageGap::Oversized],
        prompt_digest: "ab".repeat(32),
        label: IntervalLabel::Unknown,
    }
}

#[test]
fn a_judged_validation_written_before_coverage_still_reads_and_writes_the_same_bytes() {
    let kind: SessionEventKind = serde_json::from_str(HISTORICAL_JUDGED).expect("history reads");
    let SessionEventKind::ValidationDecided {
        outcome: ValidationOutcome::Judged { interval, .. },
        ..
    } = &kind
    else {
        panic!("{kind:?}");
    };
    assert!(interval.is_none());
    assert_eq!(serde_json::to_string(&kind).unwrap(), HISTORICAL_JUDGED);
}

#[test]
fn a_judged_validation_with_coverage_round_trips() {
    let mut kind: SessionEventKind = serde_json::from_str(HISTORICAL_JUDGED).unwrap();
    if let SessionEventKind::ValidationDecided {
        outcome: ValidationOutcome::Judged { interval, .. },
        ..
    } = &mut kind
    {
        *interval = Some(Box::new(review()));
    }
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(
        json["outcome"]["interval"],
        serde_json::json!({
            "rule_revision": REVIEW_RULE_REVISION,
            "after_seq": 4,
            "through_seq": 19,
            "decisions": [{"routed_seq": 9, "turn_index": 1, "response_id": "resp_a"}],
            "gaps": ["oversized"],
            "prompt_digest": "ab".repeat(32),
            "label": "unknown",
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionEventKind>(json).unwrap(),
        kind
    );
}

#[test]
fn a_selection_snapshot_names_its_objective_version_and_history_reads_as_none() {
    let historical = serde_json::json!({
        "features": {
            "extractor_revision": 1,
            "dialect": "claude_messages",
            "signals": serde_json::to_value(roundhouse_core::routing::TurnSignals::default()).unwrap(),
            "turn_index": 0,
            "observed_through_seq": 3,
        },
        "selected": {"kind": "frontier", "provider": "p", "model": "m"},
    });
    let snapshot: SelectionSnapshot =
        serde_json::from_value(historical.clone()).expect("history reads");
    assert_eq!(snapshot.objective, None);
    assert!(
        serde_json::to_value(&snapshot)
            .unwrap()
            .get("objective")
            .is_none()
    );

    for (version, wire) in [
        (
            ObjectiveVersion::Declared {
                digest: "cd".repeat(32),
            },
            serde_json::json!({"kind": "declared", "digest": "cd".repeat(32)}),
        ),
        (
            ObjectiveVersion::Undeclared,
            serde_json::json!({"kind": "undeclared"}),
        ),
    ] {
        let stamped = SelectionSnapshot {
            objective: Some(version.clone()),
            ..snapshot.clone()
        };
        let json = serde_json::to_value(&stamped).unwrap();
        assert_eq!(json["objective"], wire);
        assert_eq!(
            serde_json::from_value::<SelectionSnapshot>(json).unwrap(),
            stamped
        );
    }
}

#[test]
fn the_label_rule_reads_the_verdict_and_refuses_any_gap() {
    let parse = |raw: &str| Verdict::parse(raw).unwrap();
    let on =
        parse(r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#);
    let off =
        parse(r#"{"on_track":false,"confidence":0.9,"divergence":null,"missing_context":null}"#);
    let blind = parse(
        r#"{"on_track":false,"confidence":0.9,"divergence":null,"missing_context":"no logs"}"#,
    );
    assert_eq!(label_for(&[], &on), IntervalLabel::Positive);
    assert_eq!(label_for(&[], &off), IntervalLabel::Negative);
    assert_eq!(label_for(&[], &blind), IntervalLabel::Unknown);
    for gap in [
        CoverageGap::NoDecisions,
        CoverageGap::MetadataOverflow,
        CoverageGap::UnterminatedTurn,
        CoverageGap::VersionsUnavailable,
        CoverageGap::InstructionsChanged,
        CoverageGap::ObjectiveChanged,
        CoverageGap::UnrepresentableContent,
        CoverageGap::WithheldControlTraffic,
        CoverageGap::Oversized,
    ] {
        assert_eq!(label_for(&[gap], &on), IntervalLabel::Unknown, "{gap:?}");
        assert_eq!(label_for(&[gap], &off), IntervalLabel::Unknown, "{gap:?}");
    }
}
