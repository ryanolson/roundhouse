// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `learner_recovery` block at the configuration boundary: required once
//! a project enables the learner, in the file and through the admin plane,
//! and no field may be zero.

use std::sync::Arc;

use roundhouse_core::control::MemoryDocumentStore;
use roundhouse_core::routing::{Candidate, Target};

use super::LearnerRecoveryError;
use crate::control_config::config::{ControlPlaneConfig, ControlPlaneError, ProjectEntry};
use crate::control_config::crosscheck::CrossChecks;
use crate::control_config::fixtures::{ADMIN_HASH, TURN_HASH};
use crate::control_config::{
    ControlDirectory, ControlPlane, DirectoryError, DirectoryMutation, DocumentDirectoryStore,
};

/// A zero-prior artifact for `rules, capable`, written to a fresh file.
pub(crate) fn artifact_file() -> String {
    let bytes = r#"{"schema_revision":1,"input_revision":1,"selector_revision":2,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":["rules","capable"],"prior":[],"manifest_digest":"00","source_commit":"test"}"#;
    let path = std::env::temp_dir().join(format!(
        "roundhouse-learner-recovery-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, bytes).expect("the temp dir is writable");
    path.to_string_lossy().into_owned()
}

/// A learner block in `mode` with the ruled starting numbers.
pub(crate) fn learner(mode: &str) -> serde_json::Value {
    serde_json::json!({
        "mode": mode,
        "strategies": ["rules", "capable"],
        "artifact": artifact_file(),
        "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 5000, "min_sessions": 20 },
        "latency_limit_ms": 10000,
        "latency_min_samples": 20,
        "cache_min_samples": 20,
        "read_timeout_ms": 25,
        "apply_timeout_ms": 250
    })
}

/// The section 4 starting values.
pub(crate) fn recovery() -> serde_json::Value {
    serde_json::json!({
        "sweep_interval_ms": 30000,
        "idle_after_ms": 60000,
        "max_sessions_per_sweep": 64,
        "pages_per_session_per_sweep": 4,
        "audit_sessions_per_sweep": 32,
        "read_timeout_ms": 25,
        "apply_timeout_ms": 250,
        "source_timeout_ms": 1000
    })
}

fn tiers() -> serde_json::Value {
    serde_json::json!({ "capable": ["echo/echo"], "efficient": [] })
}

/// One project, `acme`, with `learner` (or none), and the recovery block (or
/// none).
fn document(learner: Option<serde_json::Value>, recovery: Option<serde_json::Value>) -> String {
    let mut project = serde_json::json!({ "id": "acme", "tiers": tiers() });
    if let Some(learner) = learner {
        project["learner"] = learner;
    }
    let mut document = serde_json::json!({
        "projects": [project],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }],
        "admin_keys": [ADMIN_HASH]
    });
    if let Some(recovery) = recovery {
        document["learner_recovery"] = recovery;
    }
    document.to_string()
}

fn refusal(document: &str) -> LearnerRecoveryError {
    match ControlPlaneConfig::from_json(document, "test") {
        Err(ControlPlaneError::LearnerRecoveryRejected { source, .. }) => source,
        other => panic!("expected LearnerRecoveryRejected, got {other:?}"),
    }
}

#[test]
fn a_learner_without_a_recovery_block_is_refused() {
    for mode in ["shadow", "live"] {
        assert_eq!(
            refusal(&document(Some(learner(mode)), None)),
            LearnerRecoveryError::Missing {
                project: "acme".into()
            },
            "a `{mode}` project needs the block"
        );
    }
    // Controls: an `off` block, and no block at all, need none; and a block
    // with no learner enabled is accepted, ready for one the admin plane adds.
    for accepted in [
        document(Some(serde_json::json!({ "mode": "off" })), None),
        document(None, None),
        document(None, Some(recovery())),
        document(Some(learner("shadow")), Some(recovery())),
    ] {
        ControlPlaneConfig::from_json(&accepted, "test")
            .unwrap_or_else(|error| panic!("{accepted} loads: {error}"));
    }
}

#[test]
fn a_zero_learner_recovery_interval_page_or_timeout_is_refused() {
    for field in [
        "sweep_interval_ms",
        "idle_after_ms",
        "max_sessions_per_sweep",
        "pages_per_session_per_sweep",
        "audit_sessions_per_sweep",
        "read_timeout_ms",
        "apply_timeout_ms",
        "source_timeout_ms",
    ] {
        let mut block = recovery();
        block[field] = serde_json::json!(0);
        assert_eq!(
            refusal(&document(Some(learner("shadow")), Some(block.clone()))),
            LearnerRecoveryError::Zero(field),
            "a zero `{field}` is refused with a learner"
        );
        // Checked whatever the projects say, like every present learner
        // field: the day a project flips `shadow` is the worst day to learn
        // the cadence was never valid.
        assert_eq!(
            refusal(&document(None, Some(block))),
            LearnerRecoveryError::Zero(field),
            "a zero `{field}` is refused without one"
        );
    }
}

#[test]
fn an_unknown_learner_recovery_field_is_refused() {
    let mut block = recovery();
    block["sweep_intervall_ms"] = serde_json::json!(1);
    assert!(matches!(
        ControlPlaneConfig::from_json(&document(None, Some(block)), "test"),
        Err(ControlPlaneError::Parse { .. })
    ));
}

#[test]
fn a_resolved_plane_carries_the_cadence_the_file_wrote() {
    let config =
        ControlPlaneConfig::from_json(&document(Some(learner("shadow")), Some(recovery())), "test")
            .expect("a complete document loads");
    let cadence = ControlPlane::configured(config)
        .learner_recovery()
        .expect("the block resolves onto the plane");
    assert_eq!(
        cadence.sweep_interval,
        std::time::Duration::from_millis(30000)
    );
    assert_eq!(cadence.idle_after_ms, 60000);
    assert_eq!(cadence.max_sessions_per_sweep.get(), 64);
    assert_eq!(cadence.pages_per_session_per_sweep.get(), 4);
    assert_eq!(cadence.audit_sessions_per_sweep.get(), 32);
    assert_eq!(cadence.read_timeout, std::time::Duration::from_millis(25));
    assert_eq!(cadence.apply_timeout, std::time::Duration::from_millis(250));
    assert_eq!(
        cadence.source_timeout,
        std::time::Duration::from_millis(1000)
    );
}

fn reachable() -> Candidate {
    Candidate {
        target: Target::Frontier {
            provider: "echo".into(),
            model: "echo".into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 1.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.5,
        load: None,
    }
}

async fn directory_over(file: &str) -> ControlDirectory {
    ControlDirectory::new(
        ControlPlaneConfig::from_json(file, "test").expect("the file loads"),
        "test",
        Arc::new(DocumentDirectoryStore::over(Arc::new(
            MemoryDocumentStore::new(),
        ))),
        CrossChecks::new(vec![reachable()], None),
        0,
    )
    .await
    .expect("the file compiles")
}

fn learning_project(id: &str) -> ProjectEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "tiers": tiers(),
        "learner": learner("shadow"),
    }))
    .expect("a project entry")
}

/// The admin plane compiles the file merged with its records through the same
/// `validate`, so a write that turns a learner on under a file with no block
/// is refused the way the boot would be.
#[tokio::test]
async fn an_admin_write_that_enables_a_learner_without_a_recovery_block_is_refused() {
    let without = directory_over(&document(None, None)).await;
    let refused = without
        .apply(
            DirectoryMutation::CreateProject {
                entry: learning_project("learns"),
            },
            1,
        )
        .await;
    match refused {
        Err(DirectoryError::Invalid(ControlPlaneError::LearnerRecoveryRejected {
            source: LearnerRecoveryError::Missing { project },
            ..
        })) => assert_eq!(project, "learns"),
        other => panic!("expected a missing-block refusal, got {other:?}"),
    }

    // Control: the same write under a file that prepared the block.
    let prepared = directory_over(&document(None, Some(recovery()))).await;
    prepared
        .apply(
            DirectoryMutation::CreateProject {
                entry: learning_project("learns"),
            },
            1,
        )
        .await
        .expect("a file with the block admits an admin-added learner");
}
