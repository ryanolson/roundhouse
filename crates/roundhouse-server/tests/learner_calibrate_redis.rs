// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The calibrator against a task-owned Redis gives what it gives against the
//! in-memory fixture of the same logs (milestone M10, Redis-gated).
//!
//! Both session stores stamp `at_ms` with their own clock at append, so one
//! fixture loaded into both would carry two sets of timestamps. The logs are
//! therefore seeded once into Redis through real marked appends, read back
//! into a `LogDump` exactly as Redis wrote them, and the two runs are compared
//! over the same bytes. Every test runs in a fresh namespace: enumeration
//! reads the whole namespace's learning index.
//!
//! Gated like the store's own integration tests: `#[ignore]`, opted into with
//! `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` then fails
//! loudly.

#[path = "../../roundhouse-core/tests/learning_support/mod.rs"]
mod learning_support;
#[path = "../../roundhouse-core/tests/offline_support/mod.rs"]
mod offline_support;

use learning_support::*;
use offline_support::seed;
use roundhouse_core::control::ProjectId;
use roundhouse_core::learn_store::{LearnerStore, LearningBatch, MemoryLearnerStore};
use roundhouse_core::routing::learn::offline::{
    ArtifactPrior, BootstrapPlan, CalibrationConfig, DriftCheck, DumpStore, LogDump,
    QualityMinimum, calibrate,
};
use roundhouse_core::routing::learn::{Strategy, StrategySet};
use roundhouse_core::session::SessionState;
use roundhouse_core::store::SessionStore;
use roundhouse_server::learner_calibrate::run;
use roundhouse_store_redis::test_support::{
    URL_VAR, connect_in, connect_learner_in, fresh_namespace,
};

fn config() -> CalibrationConfig {
    CalibrationConfig {
        project: ProjectId::new("acme"),
        strategies: StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap(),
        prior: ArtifactPrior::Credit,
        quality: QualityMinimum { min_sessions: 20 },
        latency_limit_ms: 10_000,
        bootstrap: BootstrapPlan {
            seed: 5,
            resamples: 200,
        },
        cutoff: None,
    }
}

fn learned(id: &str, labels: &[bool]) -> Script {
    let mut script = Script::named(id);
    for positive in labels {
        let turn = script.turn(Spec::new().decision());
        script.review(&[&turn], if *positive { on_track() } else { off_track() });
    }
    script
}

/// Every session's entry chain, delivered to `store` as its learner did.
async fn deliver(store: &dyn LearnerStore, dump: &LogDump) {
    let project = ProjectId::new("acme");
    for session in &dump.sessions {
        let entries = SessionState::replay_learning(&session.events).entries;
        store
            .apply(&LearningBatch {
                project: &project,
                session: &session.session,
                entries: &entries,
            })
            .await
            .expect("the chain applies");
    }
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_report_against_redis_equals_the_report_against_the_memory_fixture() {
    let namespace = fresh_namespace();
    let sessions = connect_in(namespace.clone()).await;
    let scripts = [
        learned("acme/ada/one#g0", &[true, false, true]),
        learned("acme/ada/one#g1", &[true]),
        learned("acme/bob/two#g0", &[false, true]),
    ];
    for script in &scripts {
        seed(&sessions, script).await;
    }
    let mut other = Script::named_with(
        "globex/eve/three#g0",
        Some(roundhouse_core::control::Principal::new("globex", "eve")),
        Some(roundhouse_core::validate::Arm::Shadow),
    );
    let turn = other.turn(Spec::new().decision());
    other.review(&[&turn], on_track());
    seed(&sessions, &other).await;

    let dump = LogDump::capture(&sessions).await.unwrap();
    let fixture = DumpStore::new(dump.clone()).unwrap();
    let redis_copy = connect_learner_in(namespace.clone()).await;
    let memory_copy = MemoryLearnerStore::new();
    let acme: LogDump = LogDump {
        sessions: dump
            .sessions
            .iter()
            .filter(|session| session.mark.project == ProjectId::new("acme"))
            .cloned()
            .collect(),
    };
    deliver(&redis_copy, &acme).await;
    deliver(&memory_copy, &acme).await;

    let from_redis = calibrate(
        &config(),
        &sessions,
        Some(&redis_copy as &dyn LearnerStore),
        "c",
    )
    .await
    .unwrap();
    let from_fixture = calibrate(
        &config(),
        &fixture,
        Some(&memory_copy as &dyn LearnerStore),
        "c",
    )
    .await
    .unwrap();
    assert_eq!(from_redis.artifact, from_fixture.artifact);
    assert_eq!(from_redis.input, from_fixture.input);
    assert_eq!(from_redis.report.render(), from_fixture.report.render());
    assert_eq!(from_redis.report.census.project_sessions, 3);
    assert_eq!(from_redis.report.census.other_projects, 1);
    let DriftCheck::Ran(drift) = &from_redis.report.drift else {
        panic!("a copy was given");
    };
    assert!(
        drift.compared > 0 && drift.differences.is_empty(),
        "{drift:?}"
    );
    assert!(
        sessions
            .pending_learning(None, 0, std::num::NonZeroUsize::new(16).unwrap())
            .await
            .unwrap()
            .sessions
            .len()
            == 4,
        "the calibrator cleared no mark"
    );
}

/// The binary's own path: a Redis source and a dump source over the same logs
/// write the same artifact, report and input manifest.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_binary_writes_the_same_files_from_redis_and_from_its_dump() {
    let namespace = fresh_namespace();
    let sessions = connect_in(namespace.clone()).await;
    for script in [
        learned("acme/ada/one#g0", &[true, true]),
        learned("acme/bob/two#g0", &[false]),
    ] {
        seed(&sessions, &script).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let dump = LogDump::capture(&sessions).await.unwrap();
    std::fs::write(
        dir.path().join("dump.json"),
        serde_json::to_vec(&dump).unwrap(),
    )
    .unwrap();
    let calibration = r#""calibration": {
    "project": "acme", "strategies": ["rules", "efficient", "capable"], "prior": "credit",
    "quality": { "min_sessions": 20 }, "latency_limit_ms": 10000,
    "bootstrap": { "seed": 9, "resamples": 300 }
  }"#;
    let redis = dir.path().join("redis.json");
    std::fs::write(
        &redis,
        format!(
            r#"{{ "source": {{ "redis": {{ "url_env": "{URL_VAR}", "namespace": "{namespace}" }} }}, {calibration} }}"#
        ),
    )
    .unwrap();
    let fixture = dir.path().join("fixture.json");
    std::fs::write(
        &fixture,
        format!(r#"{{ "source": {{ "dump": {{ "path": "dump.json" }} }}, {calibration} }}"#),
    )
    .unwrap();
    // The executable itself for the Redis source: it reads the URL from the
    // variable the manifest names, which this process has set.
    let redis_out = dir.path().join("redis-out");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_learner-calibrate"))
        .arg(&redis)
        .arg(&redis_out)
        .status()
        .expect("the binary runs");
    assert!(status.success());
    let from_redis = run(&redis, &dir.path().join("redis-run"), 1, "a")
        .await
        .unwrap();
    for file in ["artifact.json", "report.md", "input-manifest.json"] {
        assert_eq!(
            std::fs::read(redis_out.join(file)).unwrap(),
            std::fs::read(dir.path().join("redis-run").join(file)).unwrap(),
            "the binary and the library write the same {file}"
        );
    }
    let from_fixture = run(&fixture, &dir.path().join("fixture-out"), 2, "b")
        .await
        .unwrap();
    assert_eq!(from_redis.epoch, from_fixture.epoch);
    for (one, two) in [
        (&from_redis.artifact, &from_fixture.artifact),
        (&from_redis.report, &from_fixture.report),
        (&from_redis.input_manifest, &from_fixture.input_manifest),
    ] {
        assert_eq!(std::fs::read(one).unwrap(), std::fs::read(two).unwrap());
    }
    assert_ne!(
        std::fs::read(&from_redis.sidecar).unwrap(),
        std::fs::read(&from_fixture.sidecar).unwrap(),
        "only the sidecar carries the time and host"
    );
}
