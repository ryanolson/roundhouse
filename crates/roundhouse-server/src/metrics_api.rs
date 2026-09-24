// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The metrics surface: one JSON document and the page that renders it.
//!
//! Reads only, and takes no lease. The numbers come from
//! [`MetricsRecorder`](roundhouse_core::metrics::MetricsRecorder), which every
//! session has been feeding as it commits, so answering a request here is a
//! fold already done rather than a sweep over the log. A dashboard polling
//! every few seconds must not cost the store anything, or watching the fleet
//! becomes a load on the fleet.
//!
//! The page is served from this binary rather than shipped as a separate
//! frontend. It is a single self-contained file with no build step, no package
//! manifest, and no request to any host but this one — a deployment that can
//! reach the API can see the dashboard, with nothing else to install and no CDN
//! to be blocked by an air-gapped network.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use roundhouse_core::control::PrincipalKey;
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, MetricsSnapshot};
use roundhouse_core::now_ms;

use crate::control_config::{AuthError, ControlPlane, KeyScope, PlaneSource};

/// The dashboard, inlined at build time.
///
/// `include_str!` rather than a file read at startup: the page is part of the
/// binary's contract, and a deployment that copied the executable without the
/// asset directory would otherwise serve a 404 where its metrics used to be.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[derive(Clone)]
struct MetricsState {
    recorder: Arc<MetricsRecorder>,
    config: Arc<MetricsConfig>,
    /// Who may read this document, and how much of it.
    ///
    /// A [`PlaneSource`] rather than the compiled plane, for the reason the turn
    /// surfaces hold one: a revoked key must stop reading a tenant's spend at
    /// the same moment it stops being able to spend it.
    planes: Arc<dyn PlaneSource>,
}

/// Mount the metrics endpoints, gated by a control plane.
///
/// `config` carries the rate card and the declared correlaries. It is passed
/// here rather than read from the engine because pricing is a reporting
/// concern: repricing history under a corrected rate card must not require
/// touching the thing that serves traffic.
///
/// One constructor with a required plane, for the reason
/// [`http::router`](crate::http::router) gives.
///
/// Generic over the source and stored as `Arc<dyn PlaneSource>`, rather than
/// taking the trait object directly. `Arc<ControlPlane>` and
/// `Arc<ControlDirectory>` are both accepted and both unsize at the call site;
/// a parameter typed `Arc<dyn PlaneSource>` would not accept either through the
/// `Arc::clone(&plane)` a caller naturally writes, because the clone's own
/// return type is inferred before any coercion could apply.
pub fn metrics_router<P: PlaneSource>(
    planes: Arc<P>,
    recorder: Arc<MetricsRecorder>,
    config: Arc<MetricsConfig>,
) -> Router {
    let planes: Arc<dyn PlaneSource> = planes;
    Router::new()
        .route("/v1/metrics", get(snapshot))
        .route("/v1/metrics/dashboard", get(dashboard))
        .with_state(MetricsState {
            recorder,
            config,
            planes,
        })
}

/// `GET /v1/metrics`
///
/// The whole snapshot in one document. Deliberately not paginated or filtered:
/// it is bounded by the number of models a deployment serves, not by traffic,
/// and a dashboard that had to stitch several requests together could render a
/// state that never existed — provider totals from one instant beside a savings
/// figure from another.
async fn snapshot(
    State(state): State<MetricsState>,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let snapshot = scoped_snapshot(&state, &headers).await?;
    Ok(match serde_json::to_vec(&snapshot) {
        Ok(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                // A stale metrics page is a misleading one, and the numbers
                // change on every turn.
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "error": {
                    "code": "metrics_encode_failed",
                    "message": error.to_string(),
                }
            })
            .to_string(),
        )
            .into_response(),
    })
}

/// The document this caller is entitled to.
///
/// Three answers, and the difference between them is the whole of decision 6.
/// An unconfigured deployment has one tenant and no keys, so it reports
/// everything to anyone — unchanged. A configured one answers an admin key
/// with the deployment-wide document and a turn key with its own membership's
/// rows and nothing else. There is no fourth case: a request with no key at
/// all is refused before this is reached, because "how much did the fleet
/// spend" is not a public question once there is more than one tenant to
/// answer it about.
///
/// A turn key gets a *scoped* document rather than a filtered copy of the
/// deployment's — see `MetricsSnapshot::build`, which scopes the session count,
/// the turn count and the event window too. Filtering only the money would
/// leave three fields quietly describing the neighbours.
async fn scoped_snapshot(
    state: &MetricsState,
    headers: &HeaderMap,
) -> Result<MetricsSnapshot, AuthError> {
    let at_ms = now_ms();
    // One snapshot, taken at the same instant the document is stamped with:
    // the mode branch below and the key resolution inside it have to be two
    // questions about one compiled plane.
    let plane = state.planes.plane(at_ms).await;
    match &*plane {
        // Short-circuited rather than resolved and scoped to the one principal
        // `Open` would hand back, and the difference matters on exactly one
        // deployment: an upgraded one. Every session logged before the control
        // plane existed folds under `PrincipalKey::Unattributed`, and
        // `default/default` is a different key — so scoping here would drop the
        // whole of an existing deployment's history from its own dashboard the
        // first time it ran this binary. Reporting everything to everyone is
        // also simply what `Open` means: one tenant, no keys, nothing to
        // withhold from whom.
        ControlPlane::Open => Ok(state.recorder.snapshot(&state.config, at_ms)),
        ControlPlane::Configured { .. } => match plane.scope(headers)? {
            KeyScope::Admin => Ok(state.recorder.snapshot(&state.config, at_ms)),
            // The admission's policy is not consulted here and is not meant to
            // be: what a key may *route to* has no bearing on what it may
            // *read about itself*.
            KeyScope::Turn(admission) => Ok(state.recorder.snapshot_for(
                &PrincipalKey::from(&admission.principal),
                &state.config,
                at_ms,
            )),
        },
    }
}

/// `GET /v1/metrics/dashboard`
///
/// Deliberately ungated, in both modes. The page carries no numbers — it is a
/// static asset that fetches [`snapshot`] from the browser — so gating it would
/// buy nothing and cost the only way a human reaches this surface: a browser
/// cannot be told to send a bearer header on a navigation. The data it renders
/// is gated where the data is, one request later.
///
/// What that means today, stated plainly because the page cannot say it for
/// itself: in `Configured` mode a browser navigating here sends no key, so the
/// fetch is refused and the page renders its own error — "cannot reach
/// /v1/metrics -- HTTP 401" — rather than an empty or a partial dashboard. That
/// is honest but not usable; giving the page somewhere to put a key is a later
/// milestone's work, not an oversight in this one.
async fn dashboard() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use roundhouse_core::classify::{
        ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
        EvaluationSpend, EvaluationUsage, ReservationRecord, SettlementAck,
    };
    use roundhouse_core::control::{BudgetState, BudgetWindow};
    use roundhouse_core::event::{SessionEvent, SessionEventKind, Usage};
    use roundhouse_core::ids::{ResponseId, SessionId};
    use roundhouse_core::metrics::{ReferenceModel, ShadowPricing};
    use roundhouse_core::routing::{DecisionRecord, ProviderPricing, Target};
    use tower::ServiceExt;

    fn config() -> Arc<MetricsConfig> {
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

    fn recorder_with_one_call() -> Arc<MetricsRecorder> {
        let recorder = Arc::new(MetricsRecorder::new());
        let session_id = SessionId::new("s1");
        let response_id = ResponseId::new("r1");
        recorder.record(&[
            SessionEvent {
                seq: 1,
                session_id: session_id.clone(),
                at_ms: 1_000,
                kind: SessionEventKind::Routed {
                    response_id: response_id.clone(),
                    decision: DecisionRecord {
                        selection: None,
                        local_quote_skipped: None,
                        chosen: Target::Frontier {
                            provider: "anthropic".into(),
                            model: "claude".into(),
                        },
                        rationale: "test".into(),
                        policy: "test".into(),
                        isl_tokens: 10_000,
                        expected_prefill_tokens: 10_000.0,
                        expected_cost_usd: 0.03,
                        considered: vec![],
                        turn_policy_digest: String::new(),
                        budget_state: BudgetState::Unconstrained,
                        rate_card: None,
                        payer: Default::default(),
                        billing: Default::default(),
                        budget_draw: None,
                        withheld_providers: Vec::new(),
                        declared_baseline: None,
                        attempts: Vec::new(),
                    },
                },
            },
            SessionEvent {
                seq: 2,
                session_id,
                at_ms: 1_100,
                kind: SessionEventKind::ResponseCompleted {
                    response_id,
                    usage: Usage {
                        input_tokens: 10_000,
                        cached_input_tokens: 8_000,
                        output_tokens: 500,
                        reasoning_tokens: 100,
                        ..Default::default()
                    },
                    provider_reported_cost_usd: None,
                    stop_reason: None,
                },
            },
        ]);
        recorder
    }

    /// The same recorder, having also classified that turn.
    ///
    /// A classifier call in the fixture is what keeps the correspondence test
    /// below from comparing a page against an empty document: a `models` array
    /// with nothing in it would let a row field the dashboard reads go missing
    /// unnoticed.
    fn recorder_with_a_classifier_call() -> Arc<MetricsRecorder> {
        let recorder = recorder_with_one_call();
        let session_id = SessionId::new("s1");
        let call_id = ResponseId::new("cls_1");
        let source = ResponseId::new("r1");
        recorder.record(&[
            SessionEvent {
                seq: 3,
                session_id: session_id.clone(),
                at_ms: 1_200,
                kind: SessionEventKind::ClassificationRequested {
                    record: ClassificationIntent {
                        call_id: call_id.clone(),
                        source_turn_index: 1,
                        source_response_id: source.clone(),
                        requested_at_ms: 1_200,
                        expires_at_ms: 61_200,
                        identity: ClassifierIdentity {
                            model: "haiku".into(),
                            schema: "json_schema".into(),
                            taxonomy_version: 1,
                            projection_revision: 1,
                            config_revision: 1,
                        },
                        reservation: ReservationRecord {
                            rate_card: ProviderPricing {
                                input_per_mtok_usd: 0.8,
                                cached_input_per_mtok_usd: 0.08,
                                cache_write_per_mtok_usd: 1.0,
                                output_per_mtok_usd: 4.0,
                            },
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
            },
            SessionEvent {
                seq: 4,
                session_id,
                at_ms: 1_300,
                kind: SessionEventKind::ClassificationRecorded {
                    record: ClassificationRecord {
                        call_id,
                        source_turn_index: 1,
                        source_response_id: source,
                        completed_at_ms: 1_300,
                        outcome: ClassificationOutcome::Unusable {
                            reason: "schema_violation".into(),
                            spend: EvaluationSpend::Measured {
                                usage: EvaluationUsage {
                                    input_tokens: 900,
                                    output_tokens: 60,
                                },
                                usd: 0.25,
                                granted_usd: 2.0,
                                settled: SettlementAck::Unconfirmed,
                            },
                            reported_model: Some("claude-haiku-4.5".into()),
                        },
                    },
                },
            },
        ]);
        recorder
    }

    async fn get(path: &str) -> (StatusCode, String, String) {
        let app = metrics_router(ControlPlane::open(), recorder_with_one_call(), config());
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    #[tokio::test]
    async fn the_snapshot_carries_the_full_breakdown() {
        let (status, content_type, body) = get("/v1/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["calls"], 1);
        assert_eq!(json["tokens"]["input"], 10_000);
        assert_eq!(json["tokens"]["cached_input"], 8_000);
        assert_eq!(json["tokens"]["reasoning"], 100);
        assert!(json["savings"]["cache_savings_usd"].as_f64().unwrap() > 0.0);
        assert!(json["savings"]["frontier_spend_usd"].as_f64().unwrap() > 0.0);

        // Both rollup axes are present, and both serving modes always appear.
        assert_eq!(json["providers"][0]["provider"], "anthropic");
        assert_eq!(json["serving_modes"].as_array().unwrap().len(), 2);

        // The aggregates carry their money and volume at the top level of the
        // row, not nested. They are a flattened `Rollup` internally, and the
        // whole point of flattening was that consumers could not tell — this
        // pins that, because a `flatten` silently dropped or renamed is a
        // dashboard column that reads `undefined` rather than a build error.
        let provider = &json["providers"][0];
        for key in [
            "calls",
            "tokens",
            "coverage",
            "seat_tokens",
            "billed_usd",
            "shadow_usd",
            "models",
        ] {
            assert!(
                provider.get(key).is_some(),
                "provider rollup lost `{key}`: {provider}"
            );
        }
        assert_eq!(provider["calls"], 1);
        let mode = &json["serving_modes"][0];
        for key in ["mode", "calls", "tokens", "billed_usd"] {
            assert!(
                mode.get(key).is_some(),
                "serving-mode rollup lost `{key}`: {mode}"
            );
        }

        // The seat count is published beside the money and never as money —
        // zero here, because this fixture forwards nobody's subscription, and
        // present rather than absent so a dashboard can always read it.
        assert_eq!(json["seat_tokens"]["total"], 0);

        // And a model row still flattens its accounting: `mode` beside only
        // the money that applies to it.
        let row = &json["models"][0];
        assert_eq!(row["mode"], "frontier");
        assert!(row.get("billed_usd").is_some());
        assert_eq!(
            row["seat_tokens"]["total"], 0,
            "a hosted row carries its unpriceable share, even when it has none"
        );
        assert!(
            row.get("shadow_usd").is_none(),
            "a hosted row must not carry a shadow price, not even a zero one"
        );
    }

    /// Every `data.…` field path the page reads, in source order.
    ///
    /// Hand-scanned rather than matched with a regular expression because the
    /// shape is trivial and the dependency is not: a path is `data.` followed by
    /// lower-case identifiers and dots. The page is written so that every such
    /// occurrence is a pure field access — a method call on one is bound to a
    /// local first — which is what lets the whole captured path be resolved
    /// against the document rather than a prefix of it.
    fn field_paths(page: &str, root: &str) -> Vec<String> {
        let needle = format!("data.{root}.");
        let mut found = Vec::new();
        let mut rest = page;
        while let Some(at) = rest.find(&needle) {
            rest = &rest[at + needle.len()..];
            let end = rest
                .find(|c: char| {
                    !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
                })
                .unwrap_or(rest.len());
            let path = rest[..end].trim_end_matches('.').to_string();
            if !path.is_empty() && !found.contains(&path) {
                found.push(path);
            }
        }
        found
    }

    /// The page reads only fields the document publishes, under those names.
    ///
    /// The one failure this catches is silent in both directions: a renamed JSON
    /// field leaves the dashboard printing `undefined` where a dollar figure
    /// used to be, and a column added to the page against a field nobody
    /// publishes reads as a permanent zero. Neither is a build error and neither
    /// looks wrong — a metrics page is most convincing exactly when it is empty.
    #[tokio::test]
    async fn the_dashboard_reads_the_fields_the_document_publishes() {
        let app = metrics_router(
            ControlPlane::open(),
            recorder_with_a_classifier_call(),
            config(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let mut checked = 0;
        for root in ["evaluation", "observed_cost"] {
            let paths = field_paths(DASHBOARD_HTML, root);
            assert!(
                paths.len() >= 5,
                "the page reads only {} `data.{root}` fields, which is too few for \
                 this test to be checking anything: {paths:?}",
                paths.len()
            );
            for path in &paths {
                let mut cursor = &json[root];
                for step in path.split('.') {
                    cursor = cursor.get(step).unwrap_or_else(|| {
                        panic!("the dashboard reads `data.{root}.{path}`, which the document does not publish")
                    });
                }
                checked += 1;
            }
        }
        assert!(checked >= 12, "only {checked} field paths were checked");

        // The model rows are read off a bound local rather than through
        // `data.`, so their fields are named here instead of scanned.
        let row = &json["evaluation"]["models"][0];
        assert!(
            row.is_object(),
            "the fixture classified a turn, so there is a row to check against"
        );
        for field in [
            "requested_model",
            "reported_model",
            "calls",
            "measured_calls",
            "measured_usd",
            "unknown_usage_calls",
            "refused_calls",
            "tokens",
        ] {
            assert!(
                DASHBOARD_HTML.contains(&format!("m.{field}")),
                "the evaluation table publishes `{field}` and does not render it"
            );
            assert!(row.get(field).is_some(), "the row lost `{field}`: {row}");
        }

        // A serving row states whether the catalog priced it, and the page
        // reads that rather than inferring it from a zero. Both halves are
        // needed: a renamed field leaves `m.priced_by_catalog === false`
        // reading `undefined === false`, which is never true, so the rate-card
        // alert would stop firing without a single thing going red — and an
        // alert that reads the dollars again would call an explicit `$0` rate
        // card a missing one.
        let serving = &json["models"][0];
        assert_eq!(
            serving["mode"], "frontier",
            "the fixture serves one hosted turn, so there is a hosted row here: {serving}"
        );
        assert!(
            serving["priced_by_catalog"].is_boolean(),
            "a hosted row must say whether a rate card covered it: {serving}"
        );
        assert!(
            DASHBOARD_HTML.contains("m.priced_by_catalog"),
            "the page must read pricing availability off the document"
        );
        assert!(
            !DASHBOARD_HTML.contains("m.billed_usd === 0"),
            "a hosted row that billed zero is not the same fact as one the \
             catalog holds no rate for"
        );

        // Every basis and scope string the page prints is the document's own,
        // not a second copy on the page that could come to disagree with it.
        for served in [
            "rate_card_recorded_with_each_call",
            "current_catalog_rate_card",
            "hosted_serving_and_classifier_calls",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(served),
                "the page must render `{served}` as it was served, never as a literal of its own"
            );
        }
        assert_eq!(
            json["evaluation"]["price_basis"], "rate_card_recorded_with_each_call",
            "and the served basis is the one the core publishes"
        );
        assert_eq!(
            json["observed_cost"]["covers"], "hosted_serving_and_classifier_calls",
            "the combined total names what it is a total of"
        );
    }

    #[tokio::test]
    async fn the_dashboard_is_self_contained() {
        let (status, content_type, body) = get("/v1/metrics/dashboard").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));

        // A page that reached for a CDN would render blank on an air-gapped
        // deployment, which is where a fleet dashboard most needs to work.
        for offender in [
            "http://",
            "https://",
            "//cdn",
            "<script src",
            "<link rel=\"stylesheet\"",
        ] {
            assert!(
                !body.contains(offender),
                "the dashboard must not reference `{offender}`"
            );
        }
        assert!(body.contains("/v1/metrics"), "the page must poll the API");
    }
}
