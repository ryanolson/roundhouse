// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner artifact as a compile input (milestone M9).
//!
//! An artifact is read from a node-local path each time a plane compiles, and
//! its bytes decide the project's epoch. Two nodes that read different bytes
//! at one path therefore run two epochs, and before M9 nothing in
//! [`CompiledUnder`] recorded the bytes, so neither node said so. The axis is
//! derived from the compile of each version, not fixed per handle, because an
//! artifact can arrive with an admin write as well as with the file.

use std::sync::Arc;

use roundhouse_core::control::{DocumentStore, MemoryDocumentStore};
use roundhouse_core::routing::{Candidate, Target};

use super::*;
use crate::control_config::config::{ControlPlaneConfig, ProjectEntry};
use crate::control_config::fixtures::{ADMIN_HASH, TURN_HASH};

const PATH: &str = "ROUNDHOUSE_CONTROL_PLANE";

fn artifact_bytes(manifest: &str) -> String {
    format!(
        r#"{{"schema_revision":1,"input_revision":1,"selector_revision":1,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":["rules","capable"],"prior":[],"manifest_digest":"{manifest}","source_commit":"test"}}"#
    )
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

/// The file every node boots with: one plain project, a recovery block so an
/// admin write may enable a learner, and a TTL of 0 so every `plane()` call
/// re-asks the store.
fn file() -> ControlPlaneConfig {
    let json = serde_json::json!({
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }],
        "admin_keys": [ADMIN_HASH],
        "admission_cache_ttl_ms": 0,
        "learner_recovery": {
            "sweep_interval_ms": 30000,
            "idle_after_ms": 60000,
            "max_sessions_per_sweep": 64,
            "pages_per_session_per_sweep": 4,
            "audit_sessions_per_sweep": 32,
            "read_timeout_ms": 25,
            "apply_timeout_ms": 250,
            "source_timeout_ms": 1000
        }
    });
    ControlPlaneConfig::from_json(&json.to_string(), PATH).expect("the file validates")
}

async fn node(documents: Arc<dyn DocumentStore>, now_ms: u64) -> ControlDirectory {
    ControlDirectory::new(
        file(),
        PATH,
        Arc::new(DocumentDirectoryStore::over(documents)),
        CrossChecks::new(vec![reachable()], None),
        now_ms,
    )
    .await
    .expect("the file and the stored records compile on this node")
}

fn learning_project(artifact: &str) -> ProjectEntry {
    serde_json::from_value(serde_json::json!({
        "id": "learns",
        "tiers": { "capable": ["echo/echo"], "efficient": [] },
        "learner": {
            "mode": "shadow",
            "strategies": ["rules", "capable"],
            "artifact": artifact,
            "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 5000, "min_sessions": 20 },
            "latency_limit_ms": 10000,
            "latency_min_samples": 20,
            "cache_min_samples": 20,
            "read_timeout_ms": 25,
            "apply_timeout_ms": 250
        }
    }))
    .expect("a project entry")
}

#[test]
fn an_artifact_axis_that_differs_is_named_on_its_own() {
    let stored = CompiledUnder {
        artifacts: vec!["learns=aa".into()],
        ..CompiledUnder::default()
    };
    let own = CompiledUnder {
        artifacts: vec!["learns=bb".into()],
        ..CompiledUnder::default()
    };
    assert_eq!(own.differs_from(&stored), vec![DivergentInput::Artifacts]);
    assert_eq!(stored.differs_from(&stored.clone()), Vec::new());
}

/// **The digest changes `CompiledUnder` when the bytes at one path change.**
///
/// One writer enables a learner through the admin plane. A reader that reads
/// the same bytes at the path agrees; a reader booted after the bytes changed
/// names the artifact axis, and only that axis.
#[tokio::test]
async fn the_artifact_digest_changes_compiled_under_when_the_bytes_at_one_path_change() {
    let path = std::env::temp_dir().join(format!(
        "roundhouse-artifact-axis-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, artifact_bytes("first")).expect("the temp dir is writable");
    let artifact = path.to_string_lossy().into_owned();

    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), 0).await;
    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: learning_project(&artifact),
            },
            1,
        )
        .await
        .expect("a file with a recovery block admits an admin-added learner");
    assert_eq!(
        writer.plane(2).await.learner_artifacts().len(),
        1,
        "the writer's plane resolved the artifact"
    );

    // Control: the same bytes, so the same fingerprint.
    let agreeing = node(Arc::clone(&documents), 3).await;
    agreeing.plane(4).await;
    assert_eq!(agreeing.status().divergence, None);
    assert_eq!(agreeing.status().divergences_named, 0);

    // One byte of intent more: another manifest, so another epoch.
    std::fs::write(&path, artifact_bytes("second")).expect("the temp file is writable");
    let diverging = node(Arc::clone(&documents), 5).await;
    diverging.plane(6).await;
    assert_eq!(
        diverging.status().divergence,
        Some(DirectoryDivergence {
            version: 1,
            differs: vec![DivergentInput::Artifacts],
        }),
        "a node that read other bytes at the same path names the artifact axis"
    );
}
