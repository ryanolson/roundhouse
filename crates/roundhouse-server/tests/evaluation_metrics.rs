// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Who may read what this deployment spent on classifying turns.
//!
//! The fold's own suite establishes that evaluation spend is counted once and
//! priced by the call that incurred it. What it cannot establish is the surface:
//! that the document `/v1/metrics` actually serves carries those figures, and
//! that a turn key reading it sees its own membership's spend and nobody
//! else's. Evaluation spend is a new class of money on an existing surface, and
//! a new field that forgot to be scoped is exactly the kind of leak nobody
//! thinks to check — the money is scoped because the *document* is, and this is
//! where that is proven rather than assumed.
//!
//! Three scopes, because the fold has three and each answers a different
//! question: the deployment's is what an admin key gets, a principal's is what a
//! turn key gets, and a project's is what the reconciliation view measures. A
//! turn key is never widened to its project — two members of one project are
//! two payers, and one of them reading the other's spend is the disclosure this
//! suite exists to refuse.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
    EvaluationSpend, EvaluationUsage, SettlementAck,
};
use roundhouse_core::control::{BudgetWindow, Principal, PrincipalKey, ProjectId};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::metrics::{
    MetricsConfig, MetricsRecorder, MetricsSnapshot, ReferenceModel, ShadowPricing,
};
use roundhouse_core::routing::ProviderPricing;
use roundhouse_server::{ControlPlane, metrics_api};

mod common;
use common::{admin_key, control_plane, key, sha256_hex};

/// The rate card a classifier call recorded. Never in [`MetricsConfig`], which
/// is the point: an evaluation amount comes out of the log.
const CLASSIFIER: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 0.8,
    cached_input_per_mtok_usd: 0.08,
    cache_write_per_mtok_usd: 1.0,
    output_per_mtok_usd: 4.0,
};

fn metrics_config() -> Arc<MetricsConfig> {
    Arc::new(MetricsConfig::new(ShadowPricing::new(vec![
        ReferenceModel {
            provider: "anthropic".into(),
            model: "claude".into(),
            pricing: ProviderPricing {
                input_per_mtok_usd: 3.0,
                cached_input_per_mtok_usd: 0.3,
                cache_write_per_mtok_usd: 3.75,
                output_per_mtok_usd: 15.0,
            },
            quality_prior: 0.6,
        },
    ])))
}

/// Two projects, and two members inside one of them.
///
/// `bob` is what makes the principal scope mean anything: a document scoped to
/// his project rather than to him would carry `ada`'s spend, and the two would
/// be indistinguishable on a page that only ever showed one number.
fn plane() -> Arc<ControlPlane> {
    Arc::new(ControlPlane::configured(control_plane(
        serde_json::json!({
            "projects": [{ "id": "acme" }, { "id": "globex" }],
            "users": [{ "id": "ada" }, { "id": "bob" }, { "id": "zoe" }],
            "keys": [
                { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("ada")) },
                { "project": "acme", "user": "bob", "key_sha256": sha256_hex(&key("bob")) },
                { "project": "globex", "user": "zoe", "key_sha256": sha256_hex(&key("zoe")) },
            ],
            "admin_keys": [sha256_hex(&admin_key("root"))],
        }),
        "evaluation-metrics fixture",
    )))
}

fn principal(project: &str, user: &str) -> Principal {
    Principal::new(project, user)
}

/// One session that classified one turn, priced at `usd`.
fn classified(session: &str, who: Principal, call: &str, usd: f64) -> Vec<SessionEvent> {
    let session_id = SessionId::new(session);
    let call_id = ResponseId::new(call);
    let source = ResponseId::new("r1");
    let event = |seq: u64, kind: SessionEventKind| SessionEvent {
        seq,
        session_id: session_id.clone(),
        at_ms: 1_000 + seq,
        kind,
    };
    vec![
        event(
            1,
            SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(who),
                arm: None,
            },
        ),
        event(
            2,
            SessionEventKind::ClassificationRequested {
                record: ClassificationIntent {
                    call_id: call_id.clone(),
                    source_turn_index: 1,
                    source_response_id: source.clone(),
                    requested_at_ms: 1_000,
                    expires_at_ms: 61_000,
                    identity: ClassifierIdentity {
                        model: "haiku".into(),
                        schema: "json_schema".into(),
                        taxonomy_version: 1,
                        projection_revision: 1,
                        config_revision: 7,
                    },
                    reservation: roundhouse_core::classify::ReservationRecord {
                        rate_card: CLASSIFIER,
                        estimated_input_tokens: 900,
                        expected_output_tokens: 64,
                        requested_usd: 2.0,
                        hold_ttl_ms: 60_000,
                        budget_limit_usd: 100.0,
                        budget_window: BudgetWindow::Monthly,
                        member_ceiling_usd: None,
                        warn_at: 0.8,
                    },
                },
            },
        ),
        event(
            3,
            SessionEventKind::ClassificationRecorded {
                record: ClassificationRecord {
                    call_id,
                    source_turn_index: 1,
                    source_response_id: source,
                    completed_at_ms: 5_000,
                    // Unusable rather than classified: the accounting survives
                    // answers nobody can use, which is the whole reason usage
                    // sits outside the answers, and it keeps this fixture from
                    // needing a taxonomy it does not assert on.
                    outcome: ClassificationOutcome::Unusable {
                        reason: "schema_violation".into(),
                        spend: EvaluationSpend::Measured {
                            usage: EvaluationUsage {
                                input_tokens: 1_000,
                                output_tokens: 200,
                            },
                            usd,
                            granted_usd: 2.0,
                            settled: SettlementAck::Committed,
                        },
                        reported_model: Some("claude-haiku-4.5".into()),
                    },
                },
            },
        ),
    ]
}

struct Rig {
    app: Router,
    metrics: Arc<MetricsRecorder>,
    config: Arc<MetricsConfig>,
}

fn rig() -> Rig {
    let metrics = Arc::new(MetricsRecorder::new());
    metrics.record(&classified(
        "acme/ada/main",
        principal("acme", "ada"),
        "c-1",
        0.25,
    ));
    metrics.record(&classified(
        "acme/bob/main",
        principal("acme", "bob"),
        "c-1",
        1.5,
    ));
    metrics.record(&classified(
        "globex/zoe/main",
        principal("globex", "zoe"),
        "c-z",
        4.0,
    ));
    let config = metrics_config();
    Rig {
        app: metrics_api::metrics_router(plane(), Arc::clone(&metrics), Arc::clone(&config)),
        metrics,
        config,
    }
}

/// `GET /v1/metrics` as one key, parsed.
async fn metrics(app: &Router, secret: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/metrics")
                .header(AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body reads")
        .to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn usd(value: &Value, path: &[&str]) -> f64 {
    let mut cursor = value;
    for step in path {
        cursor = cursor
            .get(step)
            .unwrap_or_else(|| panic!("the document has no `{}`", path.join(".")));
    }
    cursor
        .as_f64()
        .unwrap_or_else(|| panic!("`{}` is not a number: {cursor}", path.join(".")))
}

fn close(left: f64, right: f64, what: &str) {
    assert!(
        (left - right).abs() < 1e-9,
        "{what}: {left} differs from {right}"
    );
}

/// An admin key reads the whole deployment's evaluation spend.
#[tokio::test]
async fn an_admin_key_reads_every_projects_evaluation_spend() {
    let rig = rig();
    let (status, document) = metrics(&rig.app, &admin_key("root")).await;
    assert_eq!(status, StatusCode::OK);

    close(
        usd(&document, &["evaluation", "measured_usd"]),
        0.25 + 1.5 + 4.0,
        "every project's classifier calls",
    );
    assert_eq!(document["evaluation"]["results"], 3);
    close(
        usd(&document, &["observed_cost", "evaluation_usd"]),
        usd(&document, &["evaluation", "measured_usd"]),
        "the combined view's evaluation half is the figure beside it",
    );
}

/// A turn key reads its own membership's spend, and is never widened to its
/// project.
///
/// `ada` and `bob` are two payers in one project. A document scoped to the
/// project would hand each of them the other's evaluation spend on a surface
/// they already have a key for, which is the failure the `Scope` seam exists to
/// make unrepresentable.
#[tokio::test]
async fn a_turn_key_reads_its_own_evaluation_spend_and_not_its_projects() {
    let rig = rig();
    let (status, ada) = metrics(&rig.app, &key("ada")).await;
    assert_eq!(status, StatusCode::OK);
    close(
        usd(&ada, &["evaluation", "measured_usd"]),
        0.25,
        "ada's own call",
    );
    assert_eq!(ada["evaluation"]["results"], 1);

    let (_, bob) = metrics(&rig.app, &key("bob")).await;
    close(
        usd(&bob, &["evaluation", "measured_usd"]),
        1.5,
        "bob's own call, on the same project",
    );

    // The project's own figure is the sum, and it is what neither key was
    // handed. Read through the recorder, which is where the reconciliation view
    // reads it: no HTTP surface offers a project scope to a turn key.
    let acme = rig
        .metrics
        .snapshot_for_project(&ProjectId::new("acme"), &rig.config, 9_999);
    close(
        acme.evaluation.measured_usd,
        1.75,
        "the project is its members added up",
    );
    assert!(
        usd(&ada, &["evaluation", "measured_usd"]) < acme.evaluation.measured_usd,
        "a turn key must see strictly less than its project, or the scope is \
         not doing anything"
    );
}

/// One tenant's document names no other tenant.
#[tokio::test]
async fn a_tenants_document_carries_no_other_tenants_evaluation_spend() {
    let rig = rig();
    let (_, zoe) = metrics(&rig.app, &key("zoe")).await;
    close(
        usd(&zoe, &["evaluation", "measured_usd"]),
        4.0,
        "globex's own",
    );

    let evaluation = serde_json::to_string(&zoe["evaluation"]).expect("encodes");
    for offender in ["acme", "ada", "bob", "c-1", "c-z", "main", "globex", "zoe"] {
        assert!(
            !evaluation.contains(offender),
            "the evaluation view must not carry `{offender}`: {evaluation}"
        );
    }
    // Not vacuous: it does carry what it is about.
    assert!(evaluation.contains("claude-haiku-4.5"));
}

/// An unkeyed request is refused before any of this is reached.
#[tokio::test]
async fn an_unkeyed_request_reads_no_evaluation_spend() {
    let rig = rig();
    let response = rig
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/metrics")
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The served document is the same one the recorder builds.
///
/// The surface adds no arithmetic of its own — it chooses a scope and encodes —
/// so a figure that differed here would mean the JSON had acquired a second
/// source. Compared as bytes, so a field added later is covered without anyone
/// remembering to add it to this list.
#[tokio::test]
async fn the_served_document_is_the_scoped_snapshot_verbatim() {
    let rig = rig();
    let (_, served) = metrics(&rig.app, &key("ada")).await;
    let built: MetricsSnapshot = rig.metrics.snapshot_for(
        &PrincipalKey::from(&principal("acme", "ada")),
        &rig.config,
        served["generated_at_ms"].as_u64().expect("a stamp"),
    );
    assert_eq!(
        served,
        serde_json::to_value(&built).expect("encodes"),
        "the surface encodes the scoped snapshot and computes nothing"
    );
}
