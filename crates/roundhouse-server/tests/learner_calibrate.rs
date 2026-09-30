// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `learner-calibrate` binary over a fixture log (milestone M10).
//!
//! The fixture is a [`LogDump`] of crafted logs, so its timestamps are the
//! ones the script wrote and the report's latencies mean something. The
//! crafted-log helpers are the core suite's own, included by path so the two
//! crates build their fixtures one way.

#[path = "../../roundhouse-core/tests/learning_support/mod.rs"]
mod learning_support;

use std::path::{Path, PathBuf};
use std::process::Command;

use learning_support::*;
use roundhouse_core::classify::{TierChoice, TurnComplexity};
use roundhouse_core::control::ProjectId;
use roundhouse_core::routing::ProviderPricing;
use roundhouse_core::routing::learn::offline::{DumpedMark, DumpedSession, LogDump};
use roundhouse_core::routing::learn::{Artifact, Strategy};
use roundhouse_core::session::SessionState;
use roundhouse_server::learner_calibrate::run;

const CARD: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

/// One reviewed shadow session: turns that start, dispatch after `overhead`
/// ms, speak `to_output` ms after the start, and complete.
fn session(id: &str, turns: &[(u64, u64, bool)]) -> Script {
    let mut script = Script::named(id);
    let mut clock = 100_000;
    for (overhead, to_output, positive) in turns {
        let mut turn = script.begin_at(clock);
        script.route_at(
            &mut turn,
            clock + overhead,
            Spec::new().rate_card(CARD).decision(),
        );
        script.delta_at(&turn, clock + to_output, "answer");
        script.complete_at(&turn, clock + to_output + 400, measured(600));
        script.intent(&turn);
        script.answer(
            &turn,
            classification(TurnComplexity::Routine, Some(TierChoice::Efficient)),
        );
        script.review(&[&turn], if *positive { on_track() } else { off_track() });
        clock += 60_000;
    }
    script
}

/// The fixture dump: each session with the mark its newest entry-producing
/// event earns.
fn dump(scripts: &[Script]) -> LogDump {
    let mut sessions: Vec<DumpedSession> = scripts
        .iter()
        .map(|script| {
            let entries = SessionState::replay_learning(&script.events).entries;
            let seq = entries.last().expect("a learned session has entries").seq;
            DumpedSession {
                session: script.session.clone(),
                mark: DumpedMark {
                    project: ProjectId::new("acme"),
                    seq,
                    marked_at_ms: script.events[seq as usize - 1].at_ms,
                },
                events: script.events.clone(),
            }
        })
        .collect();
    sessions.sort_by(|a, b| a.session.cmp(&b.session));
    LogDump { sessions }
}

fn fixture(dir: &Path) -> PathBuf {
    let scripts = [
        session(
            "acme/ada/refactor#g0",
            &[(120, 900, true), (80, 1_400, true), (150, 700, false)],
        ),
        session("acme/ada/refactor#g1", &[(90, 1_100, true)]),
        session("acme/bob/tests#g0", &[(200, 2_300, true), (60, 800, true)]),
    ];
    std::fs::write(
        dir.join("dump.json"),
        serde_json::to_vec_pretty(&dump(&scripts)).unwrap(),
    )
    .unwrap();
    let manifest = dir.join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{
  "source": { "dump": { "path": "dump.json" } },
  "calibration": {
    "project": "acme",
    "strategies": ["rules", "efficient", "capable"],
    "prior": "credit",
    "quality": { "min_sessions": 20 },
    "latency_limit_ms": 10000,
    "bootstrap": { "seed": 20260929, "resamples": 1000 }
  }
}
"#,
    )
    .unwrap();
    manifest
}

fn calibrate_binary(manifest: &Path, out: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_learner-calibrate"))
        .arg(manifest)
        .arg(out)
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn the_binary_writes_the_artifact_sidecar_and_report_from_a_fixture_log() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path());
    let first = dir.path().join("one");
    let second = dir.path().join("two");
    let printed = calibrate_binary(&manifest, &first);
    calibrate_binary(&manifest, &second);

    for file in ["artifact.json", "report.md", "input-manifest.json"] {
        assert_eq!(
            std::fs::read(first.join(file)).unwrap(),
            std::fs::read(second.join(file)).unwrap(),
            "{file} is byte-identical for one manifest"
        );
    }
    let bytes = std::fs::read(first.join("artifact.json")).unwrap();
    let artifact = Artifact::parse(&bytes).expect("the server's loader accepts it");
    assert!(printed.contains(&format!("epoch {}", artifact.epoch())));
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(first.join("artifact.json.meta.json")).unwrap())
            .unwrap();
    assert_eq!(sidecar["artifact_sha256"], artifact.sha256());
    assert!(sidecar["created_at_ms"].as_u64().unwrap() > 0);

    let report = std::fs::read_to_string(first.join("report.md")).unwrap();
    assert!(report.contains(&format!("manifest digest: {}", artifact.manifest_digest())));
    assert!(report.contains("drift check not run"));
    assert!(report.contains("sessions (sequence key) with an eligible interval: 3"));
    assert!(report.contains("p50 first output from turn start, factual: 900 ms"));
    assert!(
        artifact
            .prior()
            .get(
                &input(
                    roundhouse_core::routing::Tier::Capable,
                    roundhouse_core::routing::learn::Band::None
                )
                .keys()[2],
                Strategy::Rules
            )
            .n
            > 0
    );
    println!("{report}");
}

#[tokio::test]
async fn a_manifest_with_an_unknown_field_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path());
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        "\"prior\": \"credit\",",
        "\"prior\": \"credit\", \"floor\": 0.8,",
    );
    std::fs::write(&manifest, text).unwrap();
    let error = run(&manifest, &dir.path().join("out"), 1, "host")
        .await
        .expect_err("an unknown field is refused");
    assert!(
        format!("{error:#}").contains("unknown field `floor`"),
        "{error:#}"
    );
    assert!(!dir.path().join("out").exists(), "nothing was written");
}

#[tokio::test]
async fn a_redis_source_names_its_url_variable_and_never_the_url() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{
  "source": { "redis": { "url_env": "ROUNDHOUSE_M10_UNSET_URL_VARIABLE", "namespace": "rh" } },
  "calibration": {
    "project": "acme", "strategies": ["rules", "capable"], "prior": "zero",
    "quality": { "min_sessions": 20 }, "latency_limit_ms": 10000,
    "bootstrap": { "seed": 1, "resamples": 40 }
  }
}"#,
    )
    .unwrap();
    let error = run(&manifest, &dir.path().join("out"), 1, "host")
        .await
        .expect_err("the variable is not set");
    assert!(
        format!("{error:#}").contains("ROUNDHOUSE_M10_UNSET_URL_VARIABLE"),
        "{error:#}"
    );
}
