// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The core delivery loop: a durable intent before anything is sent, a later
//! turn's writer delivering the result as a named input, a decision that
//! cannot name a result past its cutoff, the no-classifier posture, and the
//! metrics and restart-recall guarantees over the same path. Split from
//! classification_runtime.rs (server-7, PR 18 round 1) to keep each claim's
//! file under 1000 lines; the shared fixtures -- the loopback classifier, the
//! fleet, the rig, the instrumented store and the admission helpers -- stay
//! in the parent module, reached here through use super::*.

use super::*;

// --------------------------------------------------------------------- claims

/// **The shipped state writes neither event.**
///
/// A deployment that configured no classifier must not gain a log kind, a
/// background task, or a line of latency.
#[tokio::test]
async fn a_deployment_with_no_classifier_writes_no_classification_events() {
    let rig = rig(None);
    let session = SessionId::new("sess_off");
    rig.turn(&session, "t1", "fix the parser").await;
    rig.turn(&session, "t2", "now add a test").await;

    assert!(rig.intents(&session).await.is_empty());
    assert!(rig.results(&session).await.is_empty());
    for decision in rig.decisions(&session).await {
        let selection = decision.selection.expect("every routed turn has one");
        assert!(
            selection
                .classifications
                .is_none_or(|window| window.named.is_empty() && window.available == 0)
        );
    }
}

/// A configured-but-disabled deployment is the same as no deployment at all:
/// `compose` returns no runtime, so there is nothing to call.
#[tokio::test]
async fn a_disabled_configuration_writes_no_classification_events() {
    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, false)));
    let session = SessionId::new("sess_disabled");
    rig.turn(&session, "t1", "fix the parser").await;

    assert!(rig.runtime.is_none(), "a disabled file composes no runtime");
    assert!(rig.intents(&session).await.is_empty());
    assert_eq!(upstream.count(), 0);
}

/// **The whole loop, in one session.**
///
/// Turn one dispatches and records a durable intent; the worker answers; turn
/// two's writer delivers the result, and turn two's own routing decision then
/// *names* it. That last assertion is what makes this a feature producer rather
/// than a log of calls nobody reads.
#[tokio::test]
async fn a_classification_becomes_a_named_input_to_a_later_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_loop");

    rig.turn(&session, "t1", "fix the parser and prove it")
        .await;

    // The intent is durable, and it names the turn it is about.
    let intents = rig.intents(&session).await;
    assert_eq!(intents.len(), 1, "one turn, one intent");
    let intent = &intents[0];
    assert_eq!(intent.source_turn_index, 0, "the first turn");
    assert_eq!(intent.identity.model, "jev-1.12");
    assert_eq!(intent.identity.config_revision, 4);
    assert!(intent.reservation.requested_usd > 0.0);
    assert!(
        rig.results(&session).await.is_empty(),
        "and nothing has been delivered yet"
    );

    rig.await_parked(&session, 1).await;
    assert_eq!(upstream.count(), 1);
    // What went out is the projection, and it carries the prompt of the turn it
    // describes.
    assert!(upstream.states()[0].contains("fix the parser"));

    rig.turn(&session, "t2", "now add a regression test").await;

    let results = rig.results(&session).await;
    assert_eq!(results.len(), 1, "the second turn's writer delivered it");
    let (available_seq, record) = &results[0];
    assert_eq!(record.call_id, intent.call_id);
    assert_eq!(
        record
            .outcome
            .classification()
            .expect("a complete answer set")
            .intent
            .value,
        TurnIntent::Implement
    );

    let decisions = rig.decisions(&session).await;
    assert_eq!(decisions.len(), 2);
    let first = decisions[0].selection.clone().expect("a snapshot");
    assert!(
        first
            .classifications
            .as_ref()
            .is_none_or(|window| window.named.is_empty()),
        "the turn that bought it could not have seen it"
    );
    let second = decisions[1].selection.clone().expect("a snapshot");
    let window = second
        .classifications
        .clone()
        .expect("a configured classifier records a window");
    assert_eq!(window.named.len(), 1, "the later turn names it: {window:?}");
    assert_eq!(window.available, 1, "and nothing was left out of it");
    assert_eq!(window.named[0].call_id, intent.call_id);
    assert_eq!(window.named[0].source_turn_index, 0);
    assert_eq!(window.named[0].available_seq, *available_seq);
    assert_eq!(window.cutoff_seq, second.features.observed_through_seq);
    assert!(
        window.named[0].available_seq <= window.cutoff_seq,
        "a named classification must have landed at or before the cutoff the \
         features were taken at"
    );

    // And the second call's projection carries the first turn's labels as prior
    // metadata, which is the enrichment the whole loop is for.
    rig.await_parked(&session, 1).await;
    assert_eq!(upstream.count(), 2);
    assert!(
        upstream.states()[1].contains("turn 0: intent=implement"),
        "{}",
        upstream.states()[1]
    );
}

/// **One answer per turn, ever.** A second intent for a turn already classified
/// would buy a second answer to a question already paid for.
#[tokio::test]
async fn a_turn_is_classified_at_most_once() {
    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_once");

    rig.turn(&session, "t1", "fix the parser").await;
    rig.await_parked(&session, 1).await;
    // A client retry of a completed turn: deduplicated, and it must not buy a
    // second classification of the same response.
    rig.turn(&session, "t1", "fix the parser").await;

    assert_eq!(rig.intents(&session).await.len(), 1);
    assert_eq!(upstream.count(), 1);
    assert_eq!(
        rig.results(&session).await.len(),
        1,
        "and the retry still delivered the result it was holding"
    );
}

/// **A result is delivered exactly once**, however many drains see it.
///
/// The runtime keeps a completion until it is acknowledged, so that a failed
/// append does not lose it; the log's own record of a delivered call is what
/// stops the re-offer becoming a duplicate.
#[tokio::test]
async fn a_result_is_appended_once_however_many_turns_drain_it() {
    let (base_url, _upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_dedup");
    let runtime = rig.runtime.clone().expect("a runtime");

    rig.turn(&session, "t1", "fix the parser").await;
    rig.await_parked(&session, 1).await;
    let held = runtime.ready(&session).await;
    assert_eq!(held.len(), 1);

    rig.turn(&session, "t2", "add a test").await;
    assert_eq!(rig.results(&session).await.len(), 1);

    // Put the same completion back in front of a third turn, as an
    // acknowledgement lost to a crash would.
    runtime.acknowledge(&session, &[]).await;
    rig.turn(&session, "t3", "and another").await;
    assert_eq!(
        rig.results(&session).await.len(),
        1,
        "a re-offered completion is refused by the log's own record of it"
    );
}

/// A turn that never routed buys nothing: with no decision there is no admitted
/// pool, and admission evidence is what permits the egress.
#[tokio::test]
async fn a_turn_that_never_routed_records_no_intent() {
    let (base_url, upstream) = classifier_upstream().await;
    let config = config(&base_url, true);
    let rig = rig(Some(&config));
    let session = SessionId::new("sess_refused");

    // A policy that admits nothing this deployment can reach: every candidate is
    // filtered out, the turn terminates without a `Routed`, and `run_turn`
    // reports the refusal.
    let admission = Admission {
        policy: Arc::new(roundhouse_core::control::TurnPolicy {
            allow: roundhouse_core::control::TargetFilter::parse(["nowhere/*"]).expect("a filter"),
            ..roundhouse_core::control::TurnPolicy::unrestricted()
        }),
        ..Admission::open()
    };
    rig.engine.create_session(&session).await.unwrap();
    let refused = rig
        .engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &admission,
        )
        .await;

    assert!(
        refused.is_err(),
        "the fixture must actually refuse the turn"
    );
    assert!(
        rig.decisions(&session).await.is_empty(),
        "and it must have written no routing decision"
    );
    assert!(
        rig.intents(&session).await.is_empty(),
        "so there is no admission evidence to read as permission"
    );
    assert_eq!(upstream.count(), 0);
    let runtime = rig.runtime.as_ref().expect("a runtime");
    assert_eq!(
        runtime.available_capacity(),
        runtime.limits().max_in_flight,
        "and the slot the turn took on the way in is back, so a refusal costs \
         the next turn nothing"
    );
}

/// The admitted pool on the intent's own turn is the frontier target the policy
/// resolved, and the projection therefore went out. The control for the refusal
/// above.
#[tokio::test]
async fn an_admitted_frontier_target_is_what_permits_the_call() {
    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_admitted");
    rig.turn(&session, "t1", "fix the parser").await;
    rig.await_parked(&session, 1).await;

    let decision = rig.decisions(&session).await.remove(0);
    let admitted = decision
        .selection
        .expect("a snapshot")
        .admitted
        .expect("the policy resolved a pool");
    assert!(
        admitted
            .iter()
            .any(|target| !matches!(target, Target::Local { .. })),
        "{admitted:?}"
    );
    assert_eq!(upstream.count(), 1);
}

/// **A saturated queue fails open.** No capacity means no classification, no
/// event, and a turn that is otherwise unchanged.
#[tokio::test]
async fn a_saturated_queue_skips_the_classification_and_serves_the_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let mut config = config(&base_url, true);
    config.executor.max_in_flight = 1;
    let rig = rig(Some(&config));
    let runtime = rig.runtime.clone().expect("a runtime");
    let session = SessionId::new("sess_saturated");

    // Hold the only permit, the way a call in flight would.
    let held = runtime.capacity().expect("the one permit");
    rig.turn(&session, "t1", "fix the parser").await;

    assert!(rig.intents(&session).await.is_empty());
    assert_eq!(upstream.count(), 0);
    assert_eq!(
        rig.decisions(&session).await.len(),
        1,
        "the turn itself is unaffected"
    );
    drop(held);
}

/// **A replay never dispatches an intent again.**
///
/// The fold reads an intent with no result and learns that the answer is
/// unknown; buying it again would be a second charge for a question already
/// paid for.
#[tokio::test]
async fn a_replay_of_an_outstanding_intent_makes_no_second_call() {
    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_replay");
    rig.turn(&session, "t1", "fix the parser").await;
    rig.await_parked(&session, 1).await;
    assert_eq!(upstream.count(), 1);

    // A successor process: the same durable log, a fresh engine, and a fresh
    // runtime holding no results.
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let successor = Rig {
        engine: Arc::new(
            Engine::with_provider_clients(
                Arc::clone(&rig.store),
                ByteTokenizer,
                Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
                catalog(),
                Arc::new(registry),
                Arc::new(AffinityPolicy::new()),
                EngineConfig {
                    turn_deadline_ms: 5_000,
                    ..EngineConfig::default()
                },
            )
            .with_classifier(Arc::clone(&runtime)),
        ),
        store: Arc::clone(&rig.store),
        runtime: Some(runtime),
    };
    successor.turn(&session, "t2", "carry on").await;
    successor.await_parked(&session, 1).await;

    // Two calls: the first turn's, and the second turn's own. The replayed
    // intent is not re-dispatched, which is what keeps the count at two rather
    // than three.
    assert_eq!(upstream.count(), 2);
    assert_eq!(
        successor.intents(&session).await.len(),
        2,
        "one intent per turn, and neither reissued"
    );
    assert!(
        successor.results(&session).await.is_empty(),
        "the first turn's answer died with the process that bought it, and the \
         log records it as outstanding rather than buying another"
    );
}

// ------------------------------------------------------- metrics reporting

/// One real classification, from HTTP through the engine's own metrics
/// recorder to the authenticated `/v1/metrics` surface.
///
/// The reporting suites in `crates/roundhouse-core` and
/// `crates/roundhouse-server/tests/evaluation_metrics.rs` build their fixtures
/// by hand and cannot reach this rung: whether a classification a *live
/// engine* produced folds into `Engine::metrics()` and is served back byte for
/// byte. Reuses this file's own [`Rig`] rather than a second harness — the
/// delivery mechanics under test (a durable intent before any HTTP, a result
/// landed by a *later* turn's writer) are exactly what it already drives, as
/// [`a_classification_becomes_a_named_input_to_a_later_turn`] establishes.
#[tokio::test]
async fn a_classified_turns_result_reaches_the_recorder_and_the_api_exactly_once() {
    use axum::Router;
    use axum::body::Body;
    use axum::http::header::AUTHORIZATION;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use roundhouse_core::metrics::{MetricsConfig, ShadowPricing};
    use roundhouse_server::{ControlPlane, metrics_api};
    use tower::ServiceExt;

    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_reporting");

    rig.turn(&session, "t1", "fix the parser and prove it")
        .await;
    rig.await_parked(&session, 1).await;
    assert_eq!(upstream.count(), 1, "one call bought the one intent");

    // The later turn's writer delivers it. This turn also buys its own
    // classification — real collateral, not suppressed — but it is still
    // `pending` at the instant below, so it carries no measured dollars and
    // cannot be mistaken for a second delivery of the first.
    rig.turn(&session, "t2", "now add a regression test").await;

    let results = rig.results(&session).await;
    assert_eq!(
        results.len(),
        1,
        "the first turn's result, and only it, landed"
    );
    let historical_usd = results[0]
        .1
        .outcome
        .spend()
        .and_then(|spend| spend.committed_usd())
        .expect("the real classifier's usage settles as a committed, measured amount");

    let metrics_config = MetricsConfig::new(ShadowPricing::new(vec![]));
    let snapshot = rig.engine.metrics().snapshot(&metrics_config, 9_999_999);
    assert_eq!(
        snapshot.evaluation.results, 1,
        "the recorder holds exactly the one delivered result"
    );
    assert_eq!(
        snapshot.evaluation.pending, 1,
        "the second turn's own intent is outstanding, not a duplicate of the first"
    );
    assert!(
        (snapshot.evaluation.measured_usd - historical_usd).abs() < 1e-9,
        "the recorder's dollar figure must be the record's own: {} vs {historical_usd}",
        snapshot.evaluation.measured_usd
    );

    // Serving and evaluation are two economies, and this is the control that
    // a leak between them would fail: `calls` and `tokens` come from the two
    // real served turns and the fixture's own fixed reply shape (four output
    // tokens apiece), never from the classifier's two HTTP round trips or its
    // 312/48-token usage. `observed_cost.serving_usd` stays at the catalog's
    // free rate, so a mutation that folded the classifier's nonzero dollars
    // into serving would show up here first.
    assert_eq!(
        snapshot.calls, 2,
        "t1's and t2's own real dispatches; the classifier's two HTTP calls are a different economy"
    );
    assert_eq!(
        snapshot.tokens.output, 8,
        "two turns at the fixture's own four output tokens apiece, not the classifier's forty-eight"
    );
    assert_eq!(
        snapshot.observed_cost.serving_usd, 0.0,
        "the catalog prices this model at zero; a leaked classifier dollar would show up here first"
    );
    assert!(
        (snapshot.observed_cost.evaluation_usd - historical_usd).abs() < 1e-9,
        "observed_cost's evaluation half is the same figure the record carries"
    );

    // The same figure again, through the surface a turn key or an admin
    // actually reads rather than through the recorder directly.
    let plane = Arc::new(ControlPlane::configured(control_plane(
        serde_json::json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key("root"))],
        }),
        "classification-runtime metrics fixture",
    )));
    let app: Router =
        metrics_api::metrics_router(plane, rig.engine.metrics(), Arc::new(metrics_config));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/metrics")
                .header(AUTHORIZATION, format!("Bearer {}", admin_key("root")))
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body reads")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(document["evaluation"]["results"], 1);
    let api_usd = document["evaluation"]["measured_usd"]
        .as_f64()
        .expect("a number");
    assert!(
        (api_usd - historical_usd).abs() < 1e-9,
        "the API's dollar figure must match the record's own: {api_usd} vs {historical_usd}"
    );

    // The same serving/evaluation separation, through the authenticated
    // surface: an operator reading this document must see the same isolation
    // a direct read of the recorder does.
    assert_eq!(document["calls"], 2);
    assert_eq!(document["tokens"]["output"], 8);
    assert_eq!(document["observed_cost"]["serving_usd"], 0.0);
    let api_evaluation_usd = document["observed_cost"]["evaluation_usd"]
        .as_f64()
        .expect("a number");
    assert!(
        (api_evaluation_usd - historical_usd).abs() < 1e-9,
        "the API's observed_cost evaluation half must match the record's own too"
    );
}

/// A restart replays the durable log; it must not re-buy the classification
/// that already landed, and cold-folding the same log again must not double
/// its cost.
///
/// The successor engine attaches a **fresh, live classifier runtime** — the
/// eligible-turn control at the end of this test proves it live — rather than
/// no classifier at all. What suppresses a new call for the replay turn is
/// the same per-turn policy the whole of
/// `classification_settlement_recovery.rs` runs under: an admission whose
/// policy names no frontier target, so `prepare` refuses with
/// `NotRun::NoAdmittedFrontier` before any HTTP call is even considered. That
/// is a stronger claim than "no classifier was attached" — it is the shape a
/// real restarted node actually has, and it rules out the runtime being
/// merely inert rather than deliberately withheld by policy.
/// `a_replay_of_an_outstanding_intent_makes_no_second_call` proves the
/// adjacent claim for an intent with **no** result yet; this one starts from
/// a result already delivered durably, which is the case that question 2
/// asks about.
#[tokio::test]
async fn a_restart_over_the_same_store_neither_recalls_the_classifier_nor_doubles_its_cost() {
    use axum::Router;
    use axum::body::Body;
    use axum::http::header::AUTHORIZATION;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ShadowPricing};
    use roundhouse_server::{ControlPlane, metrics_api};
    use tower::ServiceExt;

    let (base_url, upstream) = classifier_upstream().await;
    let rig = rig(Some(&config(&base_url, true)));
    let session = SessionId::new("sess_restart_reporting");

    rig.turn(&session, "t1", "fix the parser and prove it")
        .await;
    rig.await_parked(&session, 1).await;
    assert_eq!(upstream.count(), 1);

    // Delivers t1's result durably before any restart. Its own classification
    // (collateral, as above) is still pending and contributes no dollars.
    rig.turn(&session, "t2", "now add a regression test").await;
    // t2's own call is still in flight in the background; wait for it to
    // land before reading the call count below, or its HTTP arrival can race
    // the restart assertion and make the comparison flaky.
    rig.await_parked(&session, 1).await;

    let results_before = rig.results(&session).await;
    assert_eq!(
        results_before.len(),
        1,
        "t1's result landed before any restart"
    );
    let historical_usd = results_before[0]
        .1
        .outcome
        .spend()
        .and_then(|spend| spend.committed_usd())
        .expect("a committed, measured amount");
    let calls_before_restart = upstream.count();
    assert_eq!(
        calls_before_restart, 2,
        "t1's call and t2's own, both landed and synchronized before the restart"
    );

    // A successor process: the same durable store, a fresh engine, a fresh
    // classifier runtime that can make calls, and a real local candidate --
    // without one a local-only policy refuses a turn outright rather than
    // routing it, which would prove nothing about classification.
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let successor_runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let successor = Rig {
        engine: Arc::new(
            Engine::with_provider_clients(
                Arc::clone(&rig.store),
                ByteTokenizer,
                Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
                catalog(),
                Arc::new(registry),
                Arc::new(AffinityPolicy::new()),
                EngineConfig {
                    turn_deadline_ms: 5_000,
                    ..EngineConfig::default()
                },
            )
            .with_fleet(embedded_fleet().await as Arc<dyn LocalFleet>)
            .with_classifier(Arc::clone(&successor_runtime)),
        ),
        store: Arc::clone(&rig.store),
        runtime: Some(successor_runtime),
    };

    // The replay turn: local-only, so its own decision admits no frontier
    // target and `prepare` refuses before any call is made -- suppression by
    // policy, not by an absent classifier.
    successor
        .turn_as(&session, "t3", "carry on after a restart", &local_only())
        .await;

    assert_eq!(
        upstream.count(),
        calls_before_restart,
        "no HTTP call for either original intent, or for t3's own, on restart"
    );
    assert_eq!(
        successor.results(&session).await.len(),
        1,
        "still exactly the one delivered result"
    );
    assert_eq!(
        successor.intents(&session).await.len(),
        2,
        "t1's and t2's own; the restart turn bought no third"
    );

    // The successor's own live recorder, fed by the replay
    // `Session::open_observed` performed when t3 opened this session -- not a
    // manually rebuilt one.
    let metrics_config = MetricsConfig::new(ShadowPricing::new(vec![]));
    let snapshot = successor
        .engine
        .metrics()
        .snapshot(&metrics_config, 9_999_999);
    assert_eq!(
        snapshot.evaluation.results, 1,
        "the successor's own recorder recovered the one result exactly once"
    );
    assert!(
        (snapshot.evaluation.measured_usd - historical_usd).abs() < 1e-9,
        "at the same figure the original process recorded: {} vs {historical_usd}",
        snapshot.evaluation.measured_usd
    );

    // The same figure again, through the authenticated API surface.
    let plane = Arc::new(ControlPlane::configured(control_plane(
        serde_json::json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key("root"))],
        }),
        "classification-runtime restart metrics fixture",
    )));
    let app: Router =
        metrics_api::metrics_router(plane, successor.engine.metrics(), Arc::new(metrics_config));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/metrics")
                .header(AUTHORIZATION, format!("Bearer {}", admin_key("root")))
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body reads")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(document["evaluation"]["results"], 1);
    let api_usd = document["evaluation"]["measured_usd"]
        .as_f64()
        .expect("a number");
    assert!(
        (api_usd - historical_usd).abs() < 1e-9,
        "the API's dollar figure must match the record's own: {api_usd} vs {historical_usd}"
    );

    // Supplemental: a cold rebuild of the recorder from the full durable log
    // reproduces the same figure -- corroborating the live checks above
    // rather than substituting for them.
    let events = successor.events(&session).await;
    let cold = MetricsRecorder::new();
    cold.record(&events);
    let cold_config = MetricsConfig::new(ShadowPricing::new(vec![]));
    let cold_snapshot = cold.snapshot(&cold_config, 9_999_999);
    assert_eq!(cold_snapshot.evaluation.results, 1);
    assert!(
        (cold_snapshot.evaluation.measured_usd - historical_usd).abs() < 1e-9,
        "replaying the log once must reproduce the same dollar figure, not \
         double it: {} vs {historical_usd}",
        cold_snapshot.evaluation.measured_usd
    );

    // Positive control: the successor's classifier is live, not merely
    // attached and dormant. A turn free to reach the frontier buys its own
    // classification exactly as it would on any other process, which is what
    // proves t3's silence above was the local-only policy and not an inert
    // runtime.
    successor
        .turn_as(
            &session,
            "t4",
            "one more, freely routed",
            &Admission::open(),
        )
        .await;
    successor.await_parked(&session, 1).await;
    assert_eq!(
        upstream.count(),
        calls_before_restart + 1,
        "the successor's runtime can make a fresh call"
    );
    assert_eq!(
        successor.intents(&session).await.len(),
        3,
        "t4's own new intent; t3's policy bought none"
    );
}
