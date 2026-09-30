// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `learner` block at the configuration boundary: each refusal the plan's
//! M8 settled points list, the two ruled defaults, and the resolved terms a
//! key's admission carries.

use roundhouse_core::routing::learn::{Artifact, LearnerMode, OnInfeasible, Strategy};

use crate::control_config::config::{ControlPlaneConfig, ControlPlaneError};
use crate::control_config::fixtures::TURN_HASH;

use super::{DEFAULT_EXPLORATION_RATE, LearnerConfigError};

/// A zero-prior artifact for `strategies`, written to a fresh file.
fn artifact(strategies: &[&str]) -> String {
    let list = strategies
        .iter()
        .map(|strategy| format!("\"{strategy}\""))
        .collect::<Vec<_>>()
        .join(",");
    let bytes = format!(
        r#"{{"schema_revision":1,"input_revision":1,"selector_revision":2,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":[{list}],"prior":[],"manifest_digest":"00","source_commit":"test"}}"#
    );
    let path = std::env::temp_dir().join(format!(
        "roundhouse-learner-config-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, bytes).expect("the temp dir is writable");
    path.to_string_lossy().into_owned()
}

/// A complete block in `mode`, with the ruled starting numbers, and every
/// field in `extra` merged over it.
fn block(mode: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut block = serde_json::json!({
        "mode": mode,
        "strategies": ["rules", "efficient", "capable"],
        "artifact": artifact(&["rules", "efficient", "capable"]),
        "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 5000, "min_sessions": 20 },
        "latency_limit_ms": 10000,
        "latency_min_samples": 20,
        "cache_min_samples": 20,
        "read_timeout_ms": 25,
        "apply_timeout_ms": 250
    });
    if let serde_json::Value::Object(extra) = extra {
        for (field, value) in extra {
            block[field] = value;
        }
    }
    block
}

fn document(learner: serde_json::Value, tiers: bool) -> String {
    let mut project = serde_json::json!({ "id": "acme", "learner": learner });
    if tiers {
        project["tiers"] = serde_json::json!({
            "capable": ["openai/large"],
            "efficient": ["openai/small"]
        });
    }
    // Every document writes the recovery block a `shadow` or `live` learner
    // requires (M9), so these tests stay about the `learner` block itself.
    serde_json::json!({
        "projects": [project],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }],
        "learner_recovery": crate::control_config::learner_recovery::tests::recovery()
    })
    .to_string()
}

fn refusal(learner: serde_json::Value, tiers: bool) -> LearnerConfigError {
    match ControlPlaneConfig::from_json(&document(learner, tiers), "test") {
        Err(ControlPlaneError::LearnerRejected { entry, source, .. }) => {
            assert_eq!(entry, "project `acme`");
            source
        }
        other => panic!("expected LearnerRejected, got {other:?}"),
    }
}

fn resolved(
    learner: serde_json::Value,
) -> Option<std::sync::Arc<roundhouse_core::routing::learn::LearnerTerms>> {
    let config = ControlPlaneConfig::from_json(&document(learner, true), "test")
        .expect("a well-formed learner block loads");
    config
        .turn_keys
        .get(TURN_HASH)
        .expect("the key resolved")
        .learner
        .clone()
}

/// The apply timeout the block wrote, as a key's admission carries it.
fn apply_timeout(learner: serde_json::Value) -> Option<u64> {
    let config = ControlPlaneConfig::from_json(&document(learner, true), "test")
        .expect("a well-formed learner block loads");
    config
        .turn_keys
        .get(TURN_HASH)
        .expect("the key resolved")
        .learner_apply_timeout_ms
}

#[test]
fn a_learner_without_tiers_is_refused() {
    assert_eq!(
        refusal(block("shadow", serde_json::json!({})), false),
        LearnerConfigError::NoTiers
    );
    // Even `off`: a block that could never be turned on is refused on the day
    // it is written.
    assert_eq!(
        refusal(serde_json::json!({ "mode": "off" }), false),
        LearnerConfigError::NoTiers
    );
    // Control: the same block with a recipe loads.
    assert!(resolved(block("shadow", serde_json::json!({}))).is_some());
}

#[test]
fn a_strategy_list_without_rules_is_refused() {
    let source = refusal(
        block(
            "shadow",
            serde_json::json!({ "strategies": ["efficient", "capable"] }),
        ),
        true,
    );
    assert!(
        matches!(&source, LearnerConfigError::Strategies(reason) if reason.contains("rules")),
        "{source}"
    );
    // A repeated strategy and a list of one are refused by the same check.
    assert!(matches!(
        refusal(
            block(
                "shadow",
                serde_json::json!({ "strategies": ["rules", "rules"] })
            ),
            true
        ),
        LearnerConfigError::Strategies(_)
    ));
    assert!(matches!(
        refusal(
            block("shadow", serde_json::json!({ "strategies": ["rules"] })),
            true
        ),
        LearnerConfigError::Strategies(_)
    ));
}

#[test]
fn a_live_project_without_on_infeasible_serves_rules() {
    let terms = resolved(block("live", serde_json::json!({}))).expect("a live learner");
    assert_eq!(terms.mode, LearnerMode::Live);
    assert_eq!(terms.on_infeasible, OnInfeasible::ServeRules);
    // Control: `refuse` is an accepted value and is read.
    let refusing = resolved(block(
        "live",
        serde_json::json!({ "on_infeasible": "refuse" }),
    ))
    .expect("a live learner");
    assert_eq!(refusing.on_infeasible, OnInfeasible::Refuse);
}

#[test]
fn an_exploration_block_on_a_shadow_project_is_refused() {
    assert_eq!(
        refusal(
            block(
                "shadow",
                serde_json::json!({ "exploration": { "rate": 0.1 } })
            ),
            true
        ),
        LearnerConfigError::ExplorationNotLive
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "mode": "off", "exploration": {} }),
            true
        ),
        LearnerConfigError::ExplorationNotLive
    );
    // A rate outside (0, 1] is refused on a live project.
    for rate in [0.0, -0.1, 1.5] {
        assert_eq!(
            refusal(
                block(
                    "live",
                    serde_json::json!({ "exploration": { "rate": rate } })
                ),
                true
            ),
            LearnerConfigError::Rate(rate)
        );
    }
    // Control: a live project may explore, at the bound itself.
    let terms = resolved(block(
        "live",
        serde_json::json!({ "exploration": { "rate": 1.0 } }),
    ))
    .expect("a live learner");
    assert_eq!(terms.exploration.map(|terms| terms.rate), Some(1.0));
}

#[test]
fn an_exploration_rate_defaults_to_five_percent() {
    let terms =
        resolved(block("live", serde_json::json!({ "exploration": {} }))).expect("a live learner");
    assert_eq!(
        terms.exploration.map(|terms| terms.rate),
        Some(DEFAULT_EXPLORATION_RATE)
    );
    assert_eq!(DEFAULT_EXPLORATION_RATE, 0.05);
    // Control: no block is no exploration, not the default rate.
    let quiet = resolved(block("live", serde_json::json!({}))).expect("a live learner");
    assert_eq!(quiet.exploration, None);
}

#[test]
fn an_unknown_learner_field_is_refused() {
    let json = document(
        block("shadow", serde_json::json!({ "on_infeasable": "refuse" })),
        true,
    );
    let error = ControlPlaneConfig::from_json(&json, "test").unwrap_err();
    assert!(
        error.to_string().contains("on_infeasable"),
        "a misspelt field must be named, not dropped: {error}"
    );
    let json = document(
        block(
            "shadow",
            serde_json::json!({ "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 1, "min_sessions": 1, "max": 2 } }),
        ),
        true,
    );
    assert!(ControlPlaneConfig::from_json(&json, "test").is_err());
    // An unknown strategy name is refused by the same strictness.
    let json = document(
        block(
            "shadow",
            serde_json::json!({ "strategies": ["rules", "cheapest"] }),
        ),
        true,
    );
    assert!(ControlPlaneConfig::from_json(&json, "test").is_err());
}

#[test]
fn every_numeric_refusal_names_its_field() {
    for floor in [-0.1, 1.1] {
        assert_eq!(
            refusal(
                block(
                    "shadow",
                    serde_json::json!({ "quality": { "floor": floor, "z": 1.96, "min_evidence": 1, "min_sessions": 1 } })
                ),
                true
            ),
            LearnerConfigError::Floor(floor)
        );
    }
    for z in [0.0, -1.0] {
        assert_eq!(
            refusal(
                block(
                    "shadow",
                    serde_json::json!({ "quality": { "floor": 0.8, "z": z, "min_evidence": 1, "min_sessions": 1 } })
                ),
                true
            ),
            LearnerConfigError::Z(z)
        );
    }
    assert_eq!(
        refusal(
            block("shadow", serde_json::json!({ "read_timeout_ms": 0 })),
            true
        ),
        LearnerConfigError::ZeroTimeout("read_timeout_ms")
    );
    assert_eq!(
        refusal(
            block("shadow", serde_json::json!({ "apply_timeout_ms": 0 })),
            true
        ),
        LearnerConfigError::ZeroTimeout("apply_timeout_ms")
    );
    // A limit no first output can meet: every plan would fail latency.
    assert_eq!(
        refusal(
            block("shadow", serde_json::json!({ "latency_limit_ms": 0 })),
            true
        ),
        LearnerConfigError::ZeroLatencyLimit
    );
    // Checked in `off` too, like every present field.
    assert_eq!(
        refusal(
            serde_json::json!({ "mode": "off", "latency_limit_ms": 0 }),
            true
        ),
        LearnerConfigError::ZeroLatencyLimit
    );
}

/// An `off` block keeps the apply timeout it wrote: a session of a project
/// now `off` still delivers its pending entries (draft section 23), and it
/// delivers them under the number the operator wrote, not a built-in one.
#[test]
fn an_off_block_keeps_its_written_apply_timeout() {
    let off = serde_json::json!({ "mode": "off", "apply_timeout_ms": 1500 });
    assert!(
        resolved(off.clone()).is_none(),
        "an off block is still no learner"
    );
    assert_eq!(apply_timeout(off), Some(1500));
    assert_eq!(
        apply_timeout(block("off", serde_json::json!({ "apply_timeout_ms": 900 }))),
        Some(900)
    );
    // Control: a shadow block carries its value, and an off block that wrote
    // none carries none.
    assert_eq!(
        apply_timeout(block(
            "shadow",
            serde_json::json!({ "apply_timeout_ms": 400 })
        )),
        Some(400)
    );
    assert_eq!(apply_timeout(serde_json::json!({ "mode": "off" })), None);
}

#[test]
fn a_missing_field_is_refused_in_shadow_and_live_and_not_in_off() {
    for field in [
        "strategies",
        "artifact",
        "quality",
        "latency_limit_ms",
        "latency_min_samples",
        "cache_min_samples",
        "read_timeout_ms",
        "apply_timeout_ms",
    ] {
        for mode in ["shadow", "live"] {
            let mut learner = block(mode, serde_json::json!({}));
            learner.as_object_mut().unwrap().remove(field);
            assert_eq!(
                refusal(learner, true),
                LearnerConfigError::Missing { field, mode },
                "`{field}` in `{mode}`"
            );
        }
    }
    // `off` needs none of them, and resolves to no learner at all.
    assert!(resolved(serde_json::json!({ "mode": "off" })).is_none());
    assert!(
        resolved(serde_json::json!({})).is_none(),
        "mode defaults to off"
    );
    assert!(resolved(block("off", serde_json::json!({}))).is_none());
}

#[test]
fn an_artifact_whose_strategy_list_differs_is_refused() {
    let path = artifact(&["rules", "capable"]);
    let source = refusal(
        block("shadow", serde_json::json!({ "artifact": path })),
        true,
    );
    assert!(
        matches!(source, LearnerConfigError::ArtifactStrategies { .. }),
        "{source}"
    );
    // By label, the way every other refusal names a strategy.
    assert!(
        source
            .to_string()
            .ends_with("lists [rules, capable], and the block lists [rules, efficient, capable]"),
        "{source}"
    );
    // The order is part of the list: the epoch hashes it.
    let path = artifact(&["rules", "capable", "efficient"]);
    assert!(matches!(
        refusal(
            block("shadow", serde_json::json!({ "artifact": path })),
            true
        ),
        LearnerConfigError::ArtifactStrategies { .. }
    ));
    assert!(matches!(
        refusal(
            block(
                "shadow",
                serde_json::json!({ "artifact": "/nonexistent/roundhouse-artifact.json" })
            ),
            true
        ),
        LearnerConfigError::ArtifactUnreadable { .. }
    ));
}

#[test]
fn the_resolved_terms_carry_the_artifacts_epoch_and_every_configured_number() {
    let path = artifact(&["rules", "efficient", "capable"]);
    let terms = resolved(block(
        "shadow",
        serde_json::json!({ "artifact": path.clone(), "latency_limit_ms": 9000 }),
    ))
    .expect("a shadow learner");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(terms.epoch, Artifact::parse(&bytes).unwrap().epoch());
    assert_eq!(
        terms.strategies.as_slice(),
        &[Strategy::Rules, Strategy::Efficient, Strategy::Capable]
    );
    assert_eq!(terms.latency_limit_ms, 9000);
    assert_eq!(terms.quality.min_sessions, 20);
    assert_eq!(terms.read_timeout_ms, 25);
    assert_eq!(
        apply_timeout(block("shadow", serde_json::json!({}))),
        Some(250)
    );
}
