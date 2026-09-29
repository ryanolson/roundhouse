// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The calibration artifact and its sidecar, as bytes.
//!
//! **Written for [`Artifact::parse`], by the same field names.** The server's
//! loader parses what this writes; a calibrator whose output the server
//! refused would be caught only at the next boot.
//!
//! **No clock in the artifact.** The epoch hashes the artifact's bytes, so a
//! timestamp there would start a new epoch on every run of an unchanged
//! manifest and orphan the counters learned under the last one. The time and
//! the host go in the sidecar, which nothing hashes.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::routing::learn::artifact::{ARTIFACT_SCHEMA_REVISION, Artifact, GATE_NAME};
use crate::routing::learn::{
    LEARNING_CREDIT_REVISION, LEARNING_INPUT_REVISION, LevelKey, Strategy, StrategySet, Units,
};
use crate::routing::selection::{LEARNED_SELECTOR_REVISION, STAGE_SELECTOR_REVISION};

#[derive(Serialize)]
struct ArtifactOut<'a> {
    schema_revision: u32,
    input_revision: u32,
    selector_revision: u32,
    stage_revision: u32,
    credit_revision: u32,
    gate: &'a str,
    strategies: &'a StrategySet,
    prior: Vec<PriorOut>,
    manifest_digest: &'a str,
    source_commit: &'a str,
}

#[derive(Serialize)]
struct PriorOut {
    key: LevelKey,
    strategy: Strategy,
    pos: u64,
    n: u64,
}

/// The artifact bytes: this build's revisions, the strategy list in its
/// order, the prior in key and strategy order, the manifest digest and the
/// source commit, as pretty JSON with a trailing newline.
///
/// The prior's order is the map's, so the same units give the same bytes
/// whatever order the logs credited them in.
pub fn artifact_bytes(
    strategies: &StrategySet,
    prior: &BTreeMap<(LevelKey, Strategy), Units>,
    manifest_digest: &str,
    source_commit: &str,
) -> Vec<u8> {
    let out = ArtifactOut {
        schema_revision: ARTIFACT_SCHEMA_REVISION,
        input_revision: LEARNING_INPUT_REVISION,
        selector_revision: LEARNED_SELECTOR_REVISION,
        stage_revision: STAGE_SELECTOR_REVISION,
        credit_revision: LEARNING_CREDIT_REVISION,
        gate: GATE_NAME,
        strategies,
        prior: prior
            .iter()
            .map(|((key, strategy), units)| PriorOut {
                key: *key,
                strategy: *strategy,
                pos: units.pos,
                n: units.n,
            })
            .collect(),
        manifest_digest,
        source_commit,
    };
    let mut bytes = serde_json::to_vec_pretty(&out).expect("the artifact is plain data");
    bytes.push(b'\n');
    bytes
}

#[derive(Serialize)]
struct SidecarOut<'a> {
    artifact_sha256: &'a str,
    epoch: String,
    manifest_digest: &'a str,
    source_commit: &'a str,
    created_at_ms: u64,
    host: &'a str,
}

/// The sidecar, `<artifact>.meta.json`: what the artifact is, and when and
/// where it was written. The epoch never hashes it.
pub fn sidecar_bytes(artifact: &Artifact, created_at_ms: u64, host: &str) -> Vec<u8> {
    let out = SidecarOut {
        artifact_sha256: artifact.sha256(),
        epoch: artifact.epoch().to_string(),
        manifest_digest: artifact.manifest_digest(),
        source_commit: artifact.source_commit(),
        created_at_ms,
        host,
    };
    let mut bytes = serde_json::to_vec_pretty(&out).expect("the sidecar is plain data");
    bytes.push(b'\n');
    bytes
}
