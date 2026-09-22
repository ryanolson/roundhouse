// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Background classification, through the engine that produces it.
//!
//! The unit suites one layer down prove the projection, the bounds and the
//! accounting. What only an engine can prove is the loop this whole rung exists
//! to close:
//!
//! - a turn that dispatched writes a **durable intent before anything is sent**,
//!   and a turn that never routed writes none;
//! - a **later turn's writer** delivers the result, and the classification is
//!   then a *named* input to that turn's routing decision;
//! - a decision **cannot name a result that landed after its cutoff**, however
//!   quickly the result arrives;
//! - a deployment that configured no classifier writes neither event, which is
//!   the shipped state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::classify::{ClassificationIntent, ClassificationRecord, TurnIntent};
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    MemorySpendLedger, Secret, TargetFilter, TurnCredential, TurnPolicy,
};
use roundhouse_core::event::{CacheReadSource, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, DecisionRecord, ProviderPricing, Target};
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_fleet::typesafe::{SystemOneClient, SystemOneLimits};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, LocalFleet, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

// Shared integration-test fixtures (keys, control-plane JSON, the hashing
// helper): reused rather than duplicated for the reporting suite below, which
// needs an authenticated `/v1/metrics` request and nothing else from it.
// `embedded_fleet` gives the restart test's successor a real local candidate,
// the same fixture `classification_settlement_recovery.rs` runs its whole
// suite over.
mod common;
use common::{admin_key, control_plane, embedded_fleet, sha256_hex};

// ---------------------------------------------------------------- classifier

const ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

#[derive(Clone)]
struct Classifier {
    calls: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

/// A loopback classifier that records every body it was sent.
async fn classifier_upstream() -> (String, Classifier) {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::post;

    async fn handle(State(state): State<Classifier>, body: String) -> Response {
        state.calls.fetch_add(1, Ordering::SeqCst);
        state.seen.lock().unwrap().push(body);
        Response::new(Body::from(ANSWER))
    }

    let state = Classifier {
        calls: Arc::new(AtomicUsize::new(0)),
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/systemone", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

impl Classifier {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn states(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|body| {
                let sent: serde_json::Value = serde_json::from_str(body).expect("JSON");
                sent["state"].as_str().expect("a state").to_string()
            })
            .collect()
    }
}

/// The configuration a deployment writes, pointed at the loopback classifier.
fn config(base_url: &str, enabled: bool) -> ClassifyConfig {
    let json = format!(
        r#"{{
          "enabled": {enabled},
          "revision": 4,
          "model": "jev-1.12",
          "base_url": "{base_url}",
          "auth": {{ "env": "CLASSIFY_TEST_KEY" }},
          "pricing": {{ "input_per_mtok_usd": 0.042, "output_per_mtok_usd": 0.084 }},
          "expected_output_tokens": 24,
          "caps": {{
            "max_prior_classifications": 4,
            "max_prompt_chars": 2000,
            "max_total_bytes": 8192
          }},
          "transport": {{
            "max_request_bytes": 65536,
            "max_response_bytes": 16384,
            "deadline_ms": 4000
          }},
          "executor": {{
            "max_in_flight": 8,
            "max_http_concurrency": 2,
            "call_ttl_ms": 60000,
            "result_retention_ms": 900000,
            "sweep_interval_ms": 50
          }},
          "budget": {{ "limit_usd": 25.0, "window": "total", "warn_at": 0.8 }}
        }}"#
    );
    ClassifyConfig::from_json(&json, "<test>").expect("a valid configuration")
}

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_TEST_KEY" => Some("sk-classification-runtime-test".to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------- fleet

const PROVIDER: &str = "alpha";

fn catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        quality_prior: 0.9,
        pricing: ProviderPricing::free(),
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec(PROVIDER, "m", WireProtocol::OpenAiResponses)
    }])
}

struct Answering;

#[async_trait]
impl FrontierClient for Answering {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(FrontierChunk::whole_response(
            "done".to_string(),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            4,
            0,
        ))
    }
}

// ------------------------------------------------------------------------ rig

struct Rig {
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
    runtime: Option<Arc<ClassificationRuntime<ByteTokenizer>>>,
}

fn rig(classify: Option<&ClassifyConfig>) -> Rig {
    let store = Arc::new(MemoryStore::new());
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let mut engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog(),
        Arc::new(registry),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    );
    // Through the library's own composition, not a hand-built runtime: the
    // decision of *whether there is one* is what a mutation of the wiring would
    // change, and a test that built its own would not run it.
    let runtime = classify.and_then(|config| {
        compose(
            "<test>",
            config,
            Arc::new(MemorySpendLedger::new()),
            ByteTokenizer,
            &env,
        )
        .expect("it composes")
    });
    if let Some(runtime) = &runtime {
        engine = engine.with_classifier(Arc::clone(runtime));
    }
    Rig {
        engine: Arc::new(engine),
        store,
        runtime,
    }
}

/// An admission whose policy names no frontier target.
///
/// `prepare` refuses a call whose decision admitted no frontier target
/// (`NotRun::NoAdmittedFrontier`), so a turn served under this policy answers
/// from a local candidate and buys no classification of its own -- the
/// shipped posture of a project whose policy allows only local models, not a
/// contrivance. The identical construction anchors the whole of
/// `classification_settlement_recovery.rs`; kept local here rather than
/// shared because this file has exactly one caller for it.
fn local_only() -> Admission {
    Admission {
        policy: Arc::new(TurnPolicy {
            min_quality: 0.0,
            allow: TargetFilter::parse(["local/*"]).expect("a valid filter"),
            frontier_cadence: None,
        }),
        ..Admission::open()
    }
}

impl Rig {
    async fn turn(&self, session_id: &SessionId, turn: &str, text: &str) {
        self.turn_as(session_id, turn, text, &Admission::open())
            .await;
    }

    /// As [`Self::turn`], under a caller-chosen policy rather than the open
    /// one every other turn in this file runs under.
    async fn turn_as(&self, session_id: &SessionId, turn: &str, text: &str, admission: &Admission) {
        self.engine.create_session(session_id).await.unwrap();
        self.engine
            .run_turn(
                session_id,
                TurnId::new(turn),
                vec![Item::user_text(text)],
                admission,
            )
            .await
            .expect("this fleet always answers");
    }

    async fn events(&self, session_id: &SessionId) -> Vec<SessionEvent> {
        self.store
            .read_events(session_id, 0, 1_000)
            .await
            .expect("an in-memory log reads")
    }

    async fn intents(&self, session_id: &SessionId) -> Vec<ClassificationIntent> {
        self.events(session_id)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ClassificationRequested { record } => Some(record),
                _ => None,
            })
            .collect()
    }

    async fn results(&self, session_id: &SessionId) -> Vec<(u64, ClassificationRecord)> {
        self.events(session_id)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::ClassificationRecorded { record } => Some((event.seq, record)),
                _ => None,
            })
            .collect()
    }

    async fn decisions(&self, session_id: &SessionId) -> Vec<DecisionRecord> {
        self.events(session_id)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::Routed { decision, .. } => Some(decision),
                _ => None,
            })
            .collect()
    }

    /// Wait until the runtime has `count` results parked for this session.
    async fn await_parked(&self, session_id: &SessionId, count: usize) {
        let runtime = self.runtime.as_ref().expect("a runtime");
        for _ in 0..300 {
            if runtime.ready(session_id).await.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the classifier never answered");
    }
}

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

// ------------------------------------------------------------- store double

/// A store that refuses to append a classification result while it is armed.
///
/// **The one failure the drain has a recovery path for**, and the path is
/// otherwise unexecuted: a lost lease and a backend outage both land here, and
/// what must survive them is the *answer* — already bought, already billed —
/// rather than a second provider call.
struct RefusingStore {
    inner: MemoryStore,
    refusing: AtomicBool,
    refusing_intents: AtomicBool,
}

impl RefusingStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            refusing: AtomicBool::new(false),
            refusing_intents: AtomicBool::new(false),
        })
    }

    fn refuse_results(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::SeqCst);
    }

    /// The other half of the same outage: the turn's own writer cannot commit
    /// the intent, so nothing may be sent and whatever the turn took for that
    /// call has to come back.
    fn refuse_intents(&self, refusing: bool) {
        self.refusing_intents.store(refusing, Ordering::SeqCst);
    }
}

#[async_trait]
impl SessionStore for RefusingStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.inner.create_session(session_id, model_policy).await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.inner.acquire_lease(session_id, node_id, ttl_ms).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        self.inner.renew_lease(lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.inner.release_lease(lease).await
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let refuses = (self.refusing.load(Ordering::SeqCst)
            && kinds
                .iter()
                .any(|kind| matches!(kind, SessionEventKind::ClassificationRecorded { .. })))
            || (self.refusing_intents.load(Ordering::SeqCst)
                && kinds
                    .iter()
                    .any(|kind| matches!(kind, SessionEventKind::ClassificationRequested { .. })));
        match refuses {
            true => Err(StoreError::Backend(anyhow::anyhow!(
                "the store refused this append"
            ))),
            false => self.inner.append_events(lease, kinds).await,
        }
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.inner.read_events(session_id, after_seq, limit).await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.inner.last_seq(session_id).await
    }
}

/// **A failed append keeps the answer and buys nothing further.**
///
/// The result stays with the runtime — still holding its admission permit, so
/// the bound still counts it — and the next turn whose writer works delivers it.
/// No second provider call: the answer is already bought.
#[tokio::test]
async fn a_failed_append_retains_the_result_for_a_later_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let store = RefusingStore::new();
    let runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Arc::new(
        Engine::with_provider_clients(
            Arc::clone(&store),
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
    );
    let session = SessionId::new("sess_append_fails");
    let turn = |name: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine.create_session(&session).await.unwrap();
            engine
                .run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers");
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let calls_after_first = upstream.count();
    assert_eq!(calls_after_first, 1);
    let held = runtime.available_capacity();

    // The second turn's writer refuses the result.
    store.refuse_results(true);
    turn("t2", "add a test").await;
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ClassificationRecorded { .. })),
        "the append really did fail"
    );
    assert!(
        !runtime.ready(&session).await.is_empty(),
        "and the result stayed with the runtime rather than being acknowledged"
    );
    assert!(
        runtime.available_capacity() <= held,
        "a retained result still occupies its admission permit"
    );

    // The third turn's writer works, and delivers it.
    store.refuse_results(false);
    turn("t3", "and another").await;
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    let delivered: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRecorded { record } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !delivered.is_empty(),
        "a later turn's writer delivers what the failed one could not"
    );
    assert_eq!(
        delivered[0]
            .outcome
            .classification()
            .expect("the answer survived the failed append")
            .intent
            .value,
        TurnIntent::Implement
    );
    // **No second answer was bought for turn one's question.** Asserted on the
    // durable intents rather than on the classifier's call count, which the
    // later turns' own background workers are still racing: one intent per
    // turn, three turns, and the first turn's call named exactly once.
    let intents: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRequested { record } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(intents.len(), 3, "one intent per turn and no more");
    let mut sources: Vec<_> = intents
        .iter()
        .map(|intent| intent.source_response_id.clone())
        .collect();
    sources.sort_by_key(|id| id.to_string());
    sources.dedup();
    assert_eq!(
        sources.len(),
        3,
        "a re-asked question would show up as two intents naming one turn"
    );
    assert_eq!(
        intents
            .iter()
            .filter(|intent| intent.call_id == delivered[0].call_id)
            .count(),
        1,
        "and the delivered answer's call was issued exactly once"
    );
    assert!(
        upstream.count() <= 3,
        "at most one provider call per turn, whatever the delivery did"
    );
}

/// The evaluation ledger is not the serving one, asserted through the boundary
/// that chooses both.
#[tokio::test]
async fn the_evaluation_ledger_is_a_different_ledger_from_the_serving_one() {
    use roundhouse_core::control::{
        Allocation, BalanceQuery, Budget, BudgetTerms, BudgetWindow, Exhaustion, GrantRequest,
        Principal,
    };
    use roundhouse_core::ids::ResponseId;
    use roundhouse_server::shared_backend;

    let namespace = shared_backend::resolve_namespace(None).expect("the default");
    let backends = shared_backend::open(None, &namespace)
        .await
        .expect("the per-process arm needs no Redis");

    let terms = BudgetTerms {
        budget: Budget {
            limit_usd: 100.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::Refuse,
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    };
    let principal = Principal::new("proj_split", "user_split");
    let serving = match &backends {
        roundhouse_server::Backends::PerProcess { spend, .. } => Arc::clone(spend),
        _ => panic!("the per-process arm was expected"),
    };
    let evaluation = Arc::clone(backends.evaluation_spend());

    evaluation
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: SessionId::new("sess_split"),
            response_id: ResponseId::new("eval_1"),
            requested_usd: 5.0,
            ttl_ms: 60_000,
            terms: terms.clone(),
            now_ms: 1_000,
        })
        .await
        .expect("the evaluation ledger grants");

    let serving_balance = serving
        .balance(BalanceQuery {
            principal: principal.clone(),
            terms: terms.clone(),
            now_ms: 1_000,
        })
        .await
        .expect("a balance");
    assert_eq!(
        serving_balance.held_usd, 0.0,
        "a classification must not be able to spend a project's serving budget"
    );
    let evaluation_balance = evaluation
        .balance(BalanceQuery {
            principal,
            terms,
            now_ms: 1_000,
        })
        .await
        .expect("a balance");
    assert_eq!(evaluation_balance.held_usd, 5.0);
}

// --------------------------------------------------- stalled ledger (H1)

/// An evaluation ledger whose `open_grant` and `settle_grant` can each be held
/// open indefinitely, so a test can prove nothing on the serving path waits
/// for either. Grants everything asked and settles everything reported once
/// released; the accounting itself is not what this is about.
struct StallingLedger {
    open_grant_gate: tokio::sync::Notify,
    settle_gate: tokio::sync::Notify,
    hold_open_grant: std::sync::atomic::AtomicBool,
    hold_settle: std::sync::atomic::AtomicBool,
    open_grant_calls: AtomicUsize,
    settle_calls: AtomicUsize,
    /// Fired the instant each call is entered, before it waits on its own
    /// hold gate -- what a caller synchronizes on to know the worker is
    /// genuinely parked inside the ledger, rather than guessing with a sleep.
    /// `Notify::notify_one` buffers a permit for a `notified().await` that
    /// has not started yet, so this is race-free regardless of which side
    /// reaches its call first.
    open_grant_entered: tokio::sync::Notify,
    settle_entered: tokio::sync::Notify,
}

impl StallingLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open_grant_gate: tokio::sync::Notify::new(),
            settle_gate: tokio::sync::Notify::new(),
            hold_open_grant: std::sync::atomic::AtomicBool::new(true),
            hold_settle: std::sync::atomic::AtomicBool::new(true),
            open_grant_calls: AtomicUsize::new(0),
            settle_calls: AtomicUsize::new(0),
            open_grant_entered: tokio::sync::Notify::new(),
            settle_entered: tokio::sync::Notify::new(),
        })
    }

    /// Let every call to `open_grant` currently waiting, and every future one,
    /// proceed immediately.
    fn release_open_grant(&self) {
        self.hold_open_grant.store(false, Ordering::SeqCst);
        self.open_grant_gate.notify_waiters();
    }

    fn release_settle(&self) {
        self.hold_settle.store(false, Ordering::SeqCst);
        self.settle_gate.notify_waiters();
    }
}

#[async_trait]
impl roundhouse_core::control::SpendLedger for StallingLedger {
    async fn open_grant(
        &self,
        request: roundhouse_core::control::GrantRequest,
    ) -> Result<roundhouse_core::control::Grant, roundhouse_core::control::SpendError> {
        self.open_grant_calls.fetch_add(1, Ordering::SeqCst);
        self.open_grant_entered.notify_one();
        while self.hold_open_grant.load(Ordering::SeqCst) {
            self.open_grant_gate.notified().await;
        }
        Ok(roundhouse_core::control::Grant {
            granted_usd: request.requested_usd,
            state: roundhouse_core::control::LedgerState::Unconstrained,
        })
    }

    async fn settle_grant(
        &self,
        settlement: roundhouse_core::control::Settlement,
    ) -> Result<roundhouse_core::control::Settled, roundhouse_core::control::SpendError> {
        self.settle_calls.fetch_add(1, Ordering::SeqCst);
        self.settle_entered.notify_one();
        while self.hold_settle.load(Ordering::SeqCst) {
            self.settle_gate.notified().await;
        }
        Ok(roundhouse_core::control::Settled {
            applied: true,
            released_usd: 0.0,
            committed_usd: settlement.actual_usd,
        })
    }

    async fn balance(
        &self,
        _query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        unimplemented!("no test here reads a balance")
    }
}

/// **The claim.** `TypeSafeShadow::execute` is what awaits the evaluation
/// ledger, and it runs on the background worker `classifier.spawn` starts --
/// never on the turn that calls `request_classification`. Holding both
/// `open_grant` and, separately, `settle_grant` open for the whole test proves
/// neither is a routing dependency two ways: the dispatching turn itself
/// completes while its own call is parked in each, and — the claim a single
/// turn's own latency cannot make — a **second** and **third** turn run to
/// completion afterward, each while a different gate is still held, so
/// nothing about the session is blocked on the ledger either.
///
/// `max_in_flight: 1` keeps the proof attributable to turn one's call alone:
/// its worker holds the runtime's only admission permit for the whole stall,
/// so turns two and three find no capacity and skip classification (the
/// ordinary, already-tested fail-open path) rather than opening calls of
/// their own that would blur which call is being held.
///
/// Every wait below synchronizes on the ledger actually entering the call
/// (`StallingLedger`'s `*_entered` notifications) rather than sleeping a
/// guessed duration, with a bounded `tokio::time::timeout` as the outer
/// safety net against a real hang.
#[tokio::test]
async fn a_stalled_evaluation_grant_and_settlement_do_not_block_the_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let mut config = config(&base_url, true);
    config.executor.max_in_flight = 1;
    let ledger = StallingLedger::new();
    let runtime = compose(
        "<test>",
        &config,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");

    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::with_provider_clients(
            Arc::clone(&store),
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
    );
    let session = SessionId::new("sess_stalled_ledger");
    engine.create_session(&session).await.unwrap();

    let sync_bound = Duration::from_secs(5);
    let turn = |name: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                engine.run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("turn {name} must complete well within its deadline"))
            .expect("this fleet always answers")
        }
    };

    turn("t1", "fix the parser and prove it").await;

    // Synchronize on the worker genuinely being parked inside `open_grant`,
    // not on a guessed sleep.
    tokio::time::timeout(sync_bound, ledger.open_grant_entered.notified())
        .await
        .expect("open_grant must actually be entered for this test to be about anything");

    // The intent is durable -- the turn's own writer wrote it -- and the
    // ledger has been *asked*, but the worker is parked inside `open_grant`
    // and has sent nothing.
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ClassificationRequested { .. })),
        "the durable intent must exist before any grant is asked for"
    );
    assert_eq!(
        upstream.count(),
        0,
        "nothing was sent while the grant is held"
    );

    // The claim: a second turn, in the same session, runs to completion while
    // the first turn's classification is still stuck inside `open_grant`.
    turn("t2", "now add a test").await;

    // Release the grant. The worker proceeds to the HTTP call and then to
    // `settle_grant`, which this ledger is *also* holding -- a separate gate,
    // so this is a distinct proof from the one above rather than the same
    // stall observed twice.
    ledger.release_open_grant();
    tokio::time::timeout(sync_bound, ledger.settle_entered.notified())
        .await
        .expect("settle_grant must actually be entered once the grant releases");
    assert_eq!(
        upstream.count(),
        1,
        "the call must have reached the classifier by the time settlement is \
         entered, since settlement only follows a reply"
    );

    // The same claim again, for settlement: a third turn runs to completion
    // while the first turn's call is stuck inside `settle_grant`.
    turn("t3", "and one more").await;

    ledger.release_settle();
    let mut delivered = Vec::new();
    for _ in 0..200 {
        delivered = runtime.ready(&session).await;
        if !delivered.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(delivered.len(), 1, "the call settles and parks its result");
}

// ------------------------------------------------------ settlement repair

/// An evaluation ledger whose `settle_grant` fails the *first* attempt for a
/// given `response_id` and delegates normally to a real [`MemorySpendLedger`]
/// on every attempt after that.
///
/// So the original call's settle always fails and a repair's always succeeds,
/// which is what makes the test below about *whether anything retries* rather
/// than about the double.
struct SettleOnceFailingLedger {
    inner: MemorySpendLedger,
    settle_calls: AtomicUsize,
    failed_once: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Every `(call, amount)` this ledger actually applied.
    ///
    /// Per call rather than in total, because this rig has no local fleet: every
    /// turn routes to the frontier and so buys its own classification, and a
    /// project-wide committed figure here would be the sum of an unknown number
    /// of unrelated calls. The subject is one call's recovery, so the evidence
    /// is keyed by that call.
    applied: std::sync::Mutex<Vec<(String, f64)>>,
}

impl SettleOnceFailingLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            settle_calls: AtomicUsize::new(0),
            failed_once: std::sync::Mutex::new(std::collections::HashSet::new()),
            applied: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn settle_calls(&self) -> usize {
        self.settle_calls.load(Ordering::SeqCst)
    }

    /// What this ledger committed for one call, and `None` if it never did.
    fn applied_usd(&self, call_id: &str) -> Option<f64> {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, usd)| *usd)
    }

    fn applications(&self, call_id: &str) -> usize {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == call_id)
            .count()
    }
}

#[async_trait]
impl roundhouse_core::control::SpendLedger for SettleOnceFailingLedger {
    async fn open_grant(
        &self,
        request: roundhouse_core::control::GrantRequest,
    ) -> Result<roundhouse_core::control::Grant, roundhouse_core::control::SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(
        &self,
        settlement: roundhouse_core::control::Settlement,
    ) -> Result<roundhouse_core::control::Settled, roundhouse_core::control::SpendError> {
        self.settle_calls.fetch_add(1, Ordering::SeqCst);
        let first_attempt = self
            .failed_once
            .lock()
            .unwrap()
            .insert(settlement.response_id.to_string());
        if first_attempt {
            return Err(roundhouse_core::control::SpendError::Backend(
                anyhow::anyhow!("simulated transient settlement failure"),
            ));
        }
        let call_id = settlement.response_id.to_string();
        let actual_usd = settlement.actual_usd;
        let settled = self.inner.settle_grant(settlement).await?;
        if settled.applied {
            self.applied.lock().unwrap().push((call_id, actual_usd));
        }
        Ok(settled)
    }

    async fn balance(
        &self,
        query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        self.inner.balance(query).await
    }
}

/// **A settlement nobody acknowledged is recovered by a later turn, under its
/// original identity and its original measured amount, with no second
/// purchase.**
///
/// The classifier call itself succeeds (usage is reported, `spend` is
/// `Measured`); only the settle fails, which is the one condition
/// `SettlementAck::Unconfirmed` exists to record. `engine/spend.rs` names a
/// `repair_settle` that runs on every session open for the *serving* ledger;
/// this is the evaluation ledger's counterpart, and until it existed the
/// question stayed open forever — the record said the charge was unconfirmed
/// and nothing ever asked again, so whether the ledger had applied it was
/// unknowable rather than known to be lost.
///
/// The durable record keeps reading `Unconfirmed`, and that is deliberate
/// rather than an omission: it is what the *call* established, and it is still
/// true of the call. What the repair adds is a separate event saying the
/// question was later resolved. Rewriting the first record would destroy the
/// evidence that a settle had failed at all.
///
/// `classification_settlement_recovery.rs` is where the full matrix lives —
/// restart, reprice, monthly reset, missing usage, payer attribution. This one
/// keeps the claim in the suite that used to assert its opposite.
#[tokio::test]
async fn an_unconfirmed_settlement_is_repaired_by_a_later_turn_without_a_second_purchase() {
    let (base_url, upstream) = classifier_upstream().await;
    let classify = config(&base_url, true);
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::with_provider_clients(
            Arc::clone(&store),
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
    );
    let session = SessionId::new("sess_settle_repair");
    engine.create_session(&session).await.unwrap();
    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers");
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked = runtime.ready(&session).await;
    assert_eq!(parked.len(), 1, "the classification completed and parked");
    let spend = parked[0]
        .record
        .outcome
        .spend()
        .expect("a call was actually attempted");
    assert_eq!(
        spend.settled(),
        roundhouse_core::classify::SettlementAck::Unconfirmed,
        "the first settlement genuinely went unacknowledged"
    );
    let measured_usd = match spend {
        roundhouse_core::classify::EvaluationSpend::Measured { usd, .. } => *usd,
        other => panic!("the classifier answered, so usage must be measured: {other:?}"),
    };
    // The identity everything below is keyed by. This rig has no local fleet,
    // so later turns route to the frontier and buy classifications of their
    // own; only this call's settlement is the subject.
    let call_id = parked[0].record.call_id.to_string();
    drop(parked);
    assert_eq!(ledger.settle_calls(), 1);
    assert_eq!(
        ledger.applied_usd(&call_id),
        None,
        "and nothing is committed for it yet"
    );

    // `t2` drains the parked result into the log as `ClassificationRecorded`
    // and, at its tail, starts the repair. `t3` is what commits the repair's
    // acknowledgement. Bounded polling rather than a sleep, and on the
    // *committed total* rather than on a call count: the charge lands when the
    // background settle returns, which is not synchronous with either turn.
    turn("t2", "add a test").await;
    turn("t3", "one more").await;
    let mut recovered = None;
    for _ in 0..300 {
        recovered = ledger.applied_usd(&call_id);
        if recovered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        recovered,
        Some(measured_usd),
        "the original measured amount, recovered under the original call \
         identity -- not a re-derived price and not a later turn's"
    );
    assert_eq!(
        ledger.applications(&call_id),
        1,
        "and applied exactly once, however many turns replayed the log after it"
    );

    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    let recorded = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ClassificationRecorded { record }
                if record.call_id.to_string() == call_id =>
            {
                Some(record.clone())
            }
            _ => None,
        })
        .expect("the result is durable");
    assert_eq!(
        recorded
            .outcome
            .spend()
            .expect("a spend was attempted")
            .settled(),
        roundhouse_core::classify::SettlementAck::Unconfirmed,
        "and it still reads what the call established: the repair is a \
         separate event, not a rewrite of the evidence"
    );

    // **A repair settles; it never sends.** Stated as a relation rather than as
    // a hard-coded count, because this rig's later turns legitimately classify:
    // every purchase must be accounted for by a durable intent, so a repair
    // that issued a request would show up here as a call nobody committed to.
    let intents = intents_in(&*store, &session).await;
    assert_eq!(
        upstream.count(),
        intents.len(),
        "every classifier request is accounted for by a durable intent, so \
         recovering a settlement bought nothing"
    );
    assert!(
        intents
            .iter()
            .any(|intent| intent.call_id.to_string() == call_id),
        "including the one whose settlement this test recovered"
    );
}

// ------------------------------------------------- production lifetime (H6)

/// The production lifetime guard (`Supervisor`, returned by
/// `runtime.supervise()` and held for the life of `serve` in `main.rs`) must
/// stop admission when it ends. `Supervisor::drop` today only aborts the
/// sweep task; it does not call `shutdown`, so this is exercised through the
/// real composition seam (`compose` + `.supervise()`) rather than a hand-built
/// `shutdown()` call, which `shutdown_cancels_in_flight_work_and_releases_its_capacity`
/// (`classify_runtime/tests.rs`) already proves works on its own.
#[tokio::test]
async fn the_production_lifetime_guard_stops_admission_on_drop() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");

    // The production seam, exactly as `main.rs` calls it, held for a moment
    // and then dropped -- standing in for `serve` returning.
    let supervisor = runtime.supervise();
    drop(supervisor);

    assert!(
        runtime.capacity().is_none(),
        "dropping the production lifetime guard must stop admission"
    );
}

/// The same guarantee for a worker already spawned and waiting on the wire:
/// ending the production lifetime must cancel it and release its permit.
///
/// Synchronizes on the classifier double's handler actually being invoked --
/// not merely on the runtime having admitted and spawned the call -- so
/// dropping the guard genuinely races a request already on the wire, which is
/// the claim this test's name makes. Admission alone would still leave a
/// window between the worker being spawned and its request reaching the
/// upstream at all.
#[tokio::test]
async fn the_production_lifetime_guard_cancels_a_worker_in_flight_on_drop() {
    use axum::Router;
    use axum::routing::post;
    use std::future::pending;

    // A classifier that never answers -- signals `arrived` the instant its
    // handler is invoked, then hangs forever, so the worker is genuinely on
    // the wire until something cancels it.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let handler_arrived = Arc::clone(&arrived);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let arrived = Arc::clone(&handler_arrived);
            async move {
                arrived.notify_one();
                pending::<()>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base_url = format!("http://{addr}");

    let rig = rig(Some(&config(&base_url, true)));
    let runtime = rig.runtime.clone().expect("a runtime");
    let session = SessionId::new("sess_guard_inflight");
    rig.turn(&session, "t1", "fix the parser").await;

    // Synchronize on the request actually reaching the upstream handler, not
    // on a guessed sleep and not merely on the runtime's own admission count.
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .expect(
            "the worker's request must actually reach the upstream for this \
             test to be about on-wire cancellation",
        );
    assert!(
        runtime.available_capacity() < runtime.limits().max_in_flight,
        "the call must actually be admitted for this test to be about anything"
    );

    let supervisor = runtime.supervise();
    drop(supervisor);

    // Bounded wait, standing in for "long enough that a real cancellation would
    // have taken effect". The upstream never answers, so the only way capacity
    // comes back is a cancellation the drop above was supposed to cause.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        runtime.available_capacity(),
        runtime.limits().max_in_flight,
        "dropping the production lifetime guard must cancel a worker still \
         waiting to dispatch and release its permit"
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

// ------------------------------------------- admission before the payload

/// A fleet client that samples the classifier's free capacity at the moment
/// this turn is on the wire.
///
/// The one place a test can look at the runtime from *inside* a turn: what it
/// answers is whether the permit that will carry this turn's classification
/// was already taken before the turn was dispatched, or only after it came
/// back.
struct Watchful {
    runtime: Arc<ClassificationRuntime<ByteTokenizer>>,
    observed: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait]
impl FrontierClient for Watchful {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.observed
            .lock()
            .unwrap()
            .push(self.runtime.available_capacity());
        Ok(FrontierChunk::whole_response(
            "done".to_string(),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            4,
            0,
        ))
    }
}

/// An engine over `store` and `client`, with `runtime` as its classifier.
fn engine_over<S: SessionStore + 'static>(
    store: Arc<S>,
    client: Arc<dyn FrontierClient>,
    runtime: Arc<ClassificationRuntime<ByteTokenizer>>,
) -> Arc<Engine<S, ByteTokenizer>> {
    let registry = FrontierClients::keyed([(PROVIDER.to_string(), client)].into_iter().collect());
    Arc::new(
        Engine::with_provider_clients(
            store,
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
        .with_classifier(runtime),
    )
}

fn runtime_with(config: &ClassifyConfig) -> Arc<ClassificationRuntime<ByteTokenizer>> {
    compose(
        "<test>",
        config,
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present")
}

fn one_slot(base_url: &str) -> ClassifyConfig {
    let mut config = config(base_url, true);
    config.executor.max_in_flight = 1;
    config
}

async fn intents_in(store: &impl SessionStore, session: &SessionId) -> Vec<ClassificationIntent> {
    store
        .read_events(session, 0, 1_000)
        .await
        .expect("a log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ClassificationRequested { record } => Some(record),
            _ => None,
        })
        .collect()
}

/// **The permit is taken before the payload, and held for it.**
///
/// A classification's admission is decided while the turn still holds its own
/// items -- which is the only moment the bounded prompt copy can be taken -- so
/// by the time the turn is on the wire the permit that will carry its
/// classification is already spent. A permit taken after the answer came back
/// would leave this turn's capture built on a queue that may have no room for
/// it, and the whole capture paid for nothing.
#[tokio::test]
async fn the_capacity_for_a_classification_is_taken_before_the_turn_is_dispatched() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&config(&base_url, true));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Watchful {
            runtime: Arc::clone(&runtime),
            observed: Arc::clone(&observed),
        }) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );

    let free = runtime.limits().max_in_flight;
    assert_eq!(runtime.available_capacity(), free, "nothing held yet");

    let session = SessionId::new("sess_permit_first");
    engine.create_session(&session).await.unwrap();
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[free - 1],
        "the classification's permit must already be held while the turn it \
         describes is on the wire, because the payload it admits was taken \
         before the turn was even committed"
    );
}

/// **A deduplicated turn gives back what it took.**
///
/// The client's retry of a completed turn returns before anything is classified
/// -- and the permit it took on the way in has to come back, or a retry would
/// cost a deployment a classification slot per retry. Asserted on the next
/// turn's own classification rather than only on the gauge: with one slot
/// configured, a leaked permit is a classifier that never runs again.
#[tokio::test]
async fn a_deduplicated_turn_releases_the_capacity_it_took() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_dedup_capacity");
    engine.create_session(&session).await.unwrap();
    let turn = |name: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text("fix the parser")],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    let first = turn("t1").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.available_capacity(),
        0,
        "the one slot is occupied by the undelivered result"
    );

    // The same turn id again: the client's retry, replayed from the log. Its
    // drain frees the first call's slot, and whatever the retry itself took has
    // to come back too.
    let replayed = turn("t1").await;
    assert!(
        replayed.deduplicated,
        "the fixture must actually deduplicate"
    );
    assert_eq!(replayed.response_id, first.response_id);
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "a retry buys no second answer"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the retry released the slot it took on the way in"
    );

    turn("t2").await;
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        2,
        "so the next real turn can still be classified"
    );
}

/// **A turn that failed gives back what it took.**
///
/// Nothing is classified without a decision to classify under, so a dispatch
/// that never completed writes no intent -- and the slot it took while it was
/// trying has to be free for the turn that follows it.
#[tokio::test]
async fn a_failed_turn_releases_the_capacity_it_took() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = Arc::new(MemoryStore::new());
    let failing = Arc::new(FailsOnce::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::clone(&failing) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_failed_capacity");
    engine.create_session(&session).await.unwrap();

    let failed = engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await;
    assert!(failed.is_err(), "the fixture must actually fail the turn");
    assert!(
        intents_in(store.as_ref(), &session).await.is_empty(),
        "a turn that never completed is not classified"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the slot it took before the dispatch is back"
    );

    engine
        .run_turn(
            &session,
            TurnId::new("t2"),
            vec![Item::user_text("try again")],
            &Admission::open(),
        )
        .await
        .expect("the second attempt answers");
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "so the next turn can still be classified"
    );
}

/// **An intent the log refused gives back what it took.**
///
/// The durable intent is written before anything may be sent, so an append that
/// fails means no call -- and the slot reserved for that call must not be lost
/// with it. A store outage that cost a deployment its classification capacity
/// permanently would be an outage that outlived itself.
#[tokio::test]
async fn an_intent_the_log_refused_releases_the_capacity_it_took() {
    let (base_url, upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = RefusingStore::new();
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_refused_intent");
    engine.create_session(&session).await.unwrap();

    store.refuse_intents(true);
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("the refusal is the classifier's, and does not fail the turn");
    assert!(
        intents_in(store.as_ref(), &session).await.is_empty(),
        "the append really did fail"
    );
    assert_eq!(upstream.count(), 0, "so nothing was sent");
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the slot that call would have used is back"
    );

    store.refuse_intents(false);
    engine
        .run_turn(
            &session,
            TurnId::new("t2"),
            vec![Item::user_text("now add a test")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "so the next turn can still be classified"
    );
}

/// Fails the first dispatch the way a provider that is not there fails, and
/// answers every one after it.
struct FailsOnce {
    calls: AtomicUsize,
}

impl FailsOnce {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl FrontierClient for FailsOnce {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            }),
            _ => Ok(FrontierChunk::whole_response(
                "done".to_string(),
                quote.prompt.len() as u64,
                0,
                CacheReadSource::Provider,
                4,
                0,
            )),
        }
    }
}

/// The transport limits a configuration names reach the client that uses them.
#[test]
fn the_configured_transport_limits_reach_the_client() {
    let config = config("https://classifier.test/v1", true);
    let limits: SystemOneLimits = config.transport_limits();
    assert_eq!(limits.max_request_bytes, 65_536);
    assert_eq!(limits.deadline_ms, 4_000);
    // And a client builds over them, which is the only thing that proves the
    // base URL is usable at all.
    SystemOneClient::new(config.base_url.clone(), limits).expect("a client");
    assert!(matches!(
        config
            .credential("<test>", &env)
            .expect("the key is present"),
        TurnCredential::Stored(_)
    ));
    assert!(Secret::api_key("sk-classification-runtime-test").is_ok());
}
