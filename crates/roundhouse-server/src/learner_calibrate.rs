// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The store half of the `learner-calibrate` binary: the manifest file, the
//! stores it names, and the four files a run writes (milestone M10).
//!
//! The calibration itself is `roundhouse_core::routing::learn::offline`. This
//! module only opens what the manifest names and writes what that returns, so
//! a test can drive the same run the binary makes without spawning it.
//!
//! **No secret in the manifest.** A Redis source names the environment
//! variable that holds its URL, never the URL, because a URL can carry a
//! password and a manifest is a file people pass around and approve by digest.
//!
//! What a run writes into its output directory:
//!
//! | File | What it is |
//! |---|---|
//! | `artifact.json` | The calibration artifact the server's `learner.artifact` names. No clock; its bytes are the epoch. |
//! | `artifact.json.meta.json` | The sidecar: creation time, host, digests. Nothing hashes it. |
//! | `report.md` | The promotion report. Deterministic for one manifest. |
//! | `input-manifest.json` | The cutoff the run read. Paste its `sessions` into the manifest's `calibration.cutoff` to reproduce the run byte for byte. |

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use roundhouse_core::learn_store::LearnerStore;
use roundhouse_core::routing::learn::EpochId;
use roundhouse_core::routing::learn::offline::{
    Calibrated, CalibrationConfig, DumpStore, LogDump, calibrate, sidecar_bytes,
};
use roundhouse_store_redis::{KeyNamespace, RedisLearnerStore, RedisSessionStore};

/// The commit the calibrator was built from, as the artifact records it.
///
/// Set `ROUNDHOUSE_SOURCE_COMMIT` at build time to record a commit; without
/// it the crate version stands in. Never the clock: the artifact's bytes are
/// its epoch.
pub const SOURCE_COMMIT: &str = match option_env!("ROUNDHOUSE_SOURCE_COMMIT") {
    Some(commit) => commit,
    None => concat!("roundhouse-server ", env!("CARGO_PKG_VERSION")),
};

/// The file a run is given.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Where the session logs and their marks are read from.
    pub source: SourceSpec,
    /// A point-in-time copy of the learner store to compare against. Without
    /// one the report says `drift check not run`.
    #[serde(default)]
    pub drift_check: Option<DriftSpec>,
    pub calibration: CalibrationConfig,
}

/// A session source.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSpec {
    /// A Redis session store.
    Redis(RedisSpec),
    /// A dump file of marked logs ([`LogDump`]), resolved against the
    /// manifest's directory when relative.
    Dump(DumpSpec),
}

/// A Redis store under one namespace, its URL in an environment variable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisSpec {
    pub url_env: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DumpSpec {
    pub path: PathBuf,
}

/// The learner store the drift check reads. It must be a point-in-time copy,
/// for example a snapshot loaded into a disposable Redis: the manifest's
/// author is asserting that, and the check is only meaningful if it holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftSpec {
    pub point_in_time_copy: RedisSpec,
}

/// Where a run wrote its files.
#[derive(Debug, Clone)]
pub struct Written {
    pub artifact: PathBuf,
    pub sidecar: PathBuf,
    pub report: PathBuf,
    pub input_manifest: PathBuf,
    pub epoch: EpochId,
}

/// Read the manifest at `manifest_path`, run the calibration, and write the
/// four files into `out_dir`.
pub async fn run(
    manifest_path: &Path,
    out_dir: &Path,
    created_at_ms: u64,
    host: &str,
) -> anyhow::Result<Written> {
    let text = std::fs::read(manifest_path)
        .with_context(|| format!("reading the manifest {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&text)
        .with_context(|| format!("parsing the manifest {}", manifest_path.display()))?;
    let base = manifest_path.parent().unwrap_or(Path::new("."));
    let copy = match &manifest.drift_check {
        Some(drift) => Some(
            RedisLearnerStore::connect_namespaced(
                redis_url(&drift.point_in_time_copy)?,
                namespace(&drift.point_in_time_copy)?,
            )
            .await
            .context("connecting to the learner store copy")?,
        ),
        None => None,
    };
    let copy = copy.as_ref().map(|copy| copy as &dyn LearnerStore);
    let config = &manifest.calibration;
    let calibrated = match &manifest.source {
        SourceSpec::Redis(spec) => {
            let store = RedisSessionStore::connect_namespaced(redis_url(spec)?, namespace(spec)?)
                .await
                .context("connecting to the session store")?;
            calibrate(config, &store, copy, SOURCE_COMMIT).await?
        }
        SourceSpec::Dump(spec) => {
            let path = base.join(&spec.path);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading the dump {}", path.display()))?;
            let dump: LogDump = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing the dump {}", path.display()))?;
            let store = DumpStore::new(dump)?;
            calibrate(config, &store, copy, SOURCE_COMMIT).await?
        }
    };
    write(&calibrated, out_dir, created_at_ms, host)
}

fn write(
    calibrated: &Calibrated,
    out_dir: &Path,
    created_at_ms: u64,
    host: &str,
) -> anyhow::Result<Written> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let written = Written {
        artifact: out_dir.join("artifact.json"),
        sidecar: out_dir.join("artifact.json.meta.json"),
        report: out_dir.join("report.md"),
        input_manifest: out_dir.join("input-manifest.json"),
        epoch: calibrated.parsed.epoch(),
    };
    for (path, bytes) in [
        (&written.artifact, calibrated.artifact.clone()),
        (
            &written.sidecar,
            sidecar_bytes(&calibrated.parsed, created_at_ms, host),
        ),
        (&written.report, calibrated.report.render().into_bytes()),
        (&written.input_manifest, calibrated.input.bytes()),
    ] {
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(written)
}

fn redis_url(spec: &RedisSpec) -> anyhow::Result<String> {
    std::env::var(&spec.url_env).with_context(|| {
        format!(
            "the manifest names `{}` for the Redis URL, and it is not set",
            spec.url_env
        )
    })
}

fn namespace(spec: &RedisSpec) -> anyhow::Result<KeyNamespace> {
    KeyNamespace::new(spec.namespace.clone())
        .map_err(|error| anyhow::anyhow!("namespace `{}`: {error}", spec.namespace))
}

/// The host name the sidecar records: `HOSTNAME`, else `/etc/hostname`, else
/// `unknown`.
pub fn host() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}
