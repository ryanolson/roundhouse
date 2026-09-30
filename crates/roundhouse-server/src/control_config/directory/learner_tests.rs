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
        r#"{{"schema_revision":1,"input_revision":1,"selector_revision":2,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":["rules","capable"],"prior":[],"manifest_digest":"{manifest}","source_commit":"test"}}"#
    )
}

/// An artifact file under `manifest`, removed when the test drops it.
fn artifact_file(manifest: &str) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("the temp dir is writable");
    std::fs::write(file.path(), artifact_bytes(manifest)).expect("the temp file is writable");
    file
}

fn path_of(file: &tempfile::NamedTempFile) -> String {
    file.path().to_string_lossy().into_owned()
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

fn axis(artifacts: Option<&[&str]>) -> CompiledUnder {
    CompiledUnder {
        artifacts: artifacts.map(|list| list.iter().map(|entry| entry.to_string()).collect()),
        ..CompiledUnder::default()
    }
}

#[test]
fn an_artifact_axis_that_differs_is_named_on_its_own() {
    let stored = axis(Some(&["learns=aa"]));
    let own = axis(Some(&["learns=bb"]));
    assert_eq!(own.differs_from(&stored), vec![DivergentInput::Artifacts]);
    assert_eq!(stored.differs_from(&stored.clone()), Vec::new());
    assert_eq!(
        axis(Some(&[])).differs_from(&stored),
        vec![DivergentInput::Artifacts],
        "a recorded empty list is compared: the writer resolved no artifact"
    );
}

/// **L3: `None` is "not recorded", and is never compared**, on either side.
#[test]
fn an_unrecorded_artifact_axis_is_never_compared() {
    let own = axis(Some(&["learns=aa"]));
    assert_eq!(own.differs_from(&axis(None)), Vec::new());
    assert_eq!(axis(None).differs_from(&own), Vec::new());
}

/// **L3: a document written before the artifact axis existed** records none,
/// and a node that compiles a learner from its records reports no
/// divergence. Before, the missing field read as "no artifacts", and every
/// M9 node that compiled a learner named a divergence at boot after an
/// upgrade.
#[tokio::test]
async fn a_pre_m9_document_compiled_by_a_node_with_a_learner_reports_no_divergence() {
    let artifact = artifact_file("first");
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), 0).await;
    writer
        .apply(
            DirectoryMutation::CreateProject {
                entry: learning_project(&path_of(&artifact)),
            },
            1,
        )
        .await
        .expect("a file with a recovery block admits an admin-added learner");

    // What a pre-M9 build wrote for these records: no artifact axis.
    let stored = documents.load().await.expect("the store answers");
    let mut document: serde_json::Value =
        serde_json::from_slice(&stored.document.expect("a document")).expect("JSON");
    assert!(
        document["compiled_under"]
            .as_object_mut()
            .expect("an object")
            .remove("artifacts")
            .is_some(),
        "control: the M9 writer stamped the axis"
    );
    documents
        .commit(stored.version, serde_json::to_vec(&document).expect("JSON"))
        .await
        .expect("the rewrite commits");

    let reader = node(Arc::clone(&documents), 3).await;
    assert_eq!(
        reader.plane(4).await.learner_artifacts().len(),
        1,
        "the reader compiled the learner"
    );
    assert_eq!(reader.status().divergence, None);
    assert_eq!(reader.status().divergences_named, 0);
}

/// **The digest changes `CompiledUnder` when the bytes at one path change.**
///
/// One writer enables a learner through the admin plane. A reader that reads
/// the same bytes at the path agrees; a reader booted after the bytes changed
/// names the artifact axis, and only that axis.
#[tokio::test]
async fn the_artifact_digest_changes_compiled_under_when_the_bytes_at_one_path_change() {
    let file = artifact_file("first");
    let artifact = path_of(&file);

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
    std::fs::write(file.path(), artifact_bytes("second")).expect("the temp file is writable");
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

/// **Mutation survivor S5: a write whose plane enables no learner stamps no
/// artifact axis.** `None` is what keeps a document with no learner
/// byte-identical to one written before the axis existed; without the
/// emptiness guard in `CompiledUnder::stamped_artifacts`, every write stamps
/// `Some([])`, and no byte-identity test notices, because they build their
/// stamp by hand.
#[tokio::test]
async fn a_write_with_no_learner_stamps_no_artifact_axis() {
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
    let writer = node(Arc::clone(&documents), 0).await;
    let plain: ProjectEntry =
        serde_json::from_value(serde_json::json!({ "id": "plain" })).expect("a project entry");
    writer
        .apply(DirectoryMutation::CreateProject { entry: plain }, 1)
        .await
        .expect("a plain project is admitted");
    assert!(
        writer.plane(2).await.learner_artifacts().is_empty(),
        "control: the writer's plane enables no learner"
    );

    let loaded = DocumentDirectoryStore::over(Arc::clone(&documents))
        .load()
        .await
        .expect("the store answers");
    assert_eq!(loaded.version, 1, "control: the write committed");
    assert_eq!(loaded.compiled_under.artifacts, None);
}
